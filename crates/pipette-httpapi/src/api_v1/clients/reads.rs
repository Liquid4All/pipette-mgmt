//! `GET /api/v1/clients`, `GET /api/v1/clients/{client_id}` and
//! `GET /api/v1/clients/capabilities`: the fleet, read by a user. `read_clients`
//! shows the whole fleet too, until per-submitter restrictions (PIP-526) land.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::num::NonZeroUsize;

use axum::Json;
use axum::extract::State;
use axum::response::IntoResponse;
use chrono::{DateTime, Utc};
use futures::stream;
use futures::stream::{StreamExt, TryStreamExt};

use pipette_core::campaign::Campaign;
use pipette_core::client::{Client, ClientStatus, normalized_flags};
use pipette_core::client_permission;
use pipette_core::client_permission::ClientPermissionSet;
use pipette_pipeline::client_cli;
use pipette_pipeline::client_cli::{CapabilityMatcher, CapabilityVocabulary};
use pipette_pipeline::org;
use pipette_pipeline::org::{DeviceScope, OrgRef};
use pipette_plan_types::{
    Backlog, BacklogByCampaign, BacklogCampaign, CampaignId, CapabilityVocabularyQuery,
    ClientDetail, ClientId, ClientListQuery, ClientListRow, ClientOwner, ClientView, FleetOwner,
    OwnerLookupQuery, Tag, UserId,
};
use pipette_store::{
    BacklogSnapshot, CampaignBacklogSnapshot, ClientStore, Page, STORAGE_CONCURRENCY,
};

use crate::api_v1::clients::filters::FleetApiFilters;
use crate::auth::{ActiveUser, Caller, HumanUser};
use crate::campaign_access::CampaignReach;
use crate::error::AppError;
use crate::extract::{ApiPath, ApiQuery, Parsed};
use crate::owner_choices::device_choices;
use crate::state::AppState;

// GET /api/v1/clients
pub async fn list_clients(
    State(state): State<AppState>,
    active: ActiveUser,
    ApiQuery(query): ApiQuery<ClientListQuery>,
    ApiQuery(pairs): ApiQuery<Vec<(String, String)>>,
) -> Result<impl IntoResponse, AppError> {
    let caller = active.caller();
    let filters = FleetApiFilters::from_pairs(&pairs)?;
    Ok(Json(api_listing(&state, caller, query, filters).await?))
}

/// `GET /api/v1/clients/capabilities` returns capability flags that a plan author
/// can use in a job's `requires` set.
pub async fn get_capability_vocabulary(
    State(state): State<AppState>,
    _: ActiveUser,
    ApiQuery(query): ApiQuery<CapabilityVocabularyQuery>,
) -> Result<impl IntoResponse, AppError> {
    let vocabulary: CapabilityVocabulary = client_cli::capability_vocabulary(
        state.client_store.as_ref(),
        state.tag_store.as_ref(),
        query
            .status
            .map(|status| ClientStatus::try_from(status).map_err(AppError::BadRequest))
            .transpose()?,
    )
    .await
    .map_err(|e| AppError::Internal(format!("failed to read the fleet: {e:#}")))?;
    Ok(Json(pipette_plan_types::CapabilityVocabulary::from(
        vocabulary,
    )))
}

async fn api_listing(
    state: &AppState,
    caller: Caller<'_>,
    query: ClientListQuery,
    filters: FleetApiFilters,
) -> Result<pipette_plan_types::ListPage<ClientListRow>, AppError> {
    let user = caller.user;
    let user_id = &user.user_id;
    let status = query
        .status
        .map(|status| ClientStatus::try_from(status).map_err(AppError::BadRequest))
        .transpose()?;
    let limit = crate::list::list_limit(query.limit, &state.config)?;
    let after = crate::list::list_cursor(query.cursor, ClientId::try_new)?;
    if query.owned.is_some() && (query.owner.is_some() || query.org_id.is_some()) {
        return Err(AppError::BadRequest(
            "`owned` cannot be combined with `owner` or `org_id`".into(),
        ));
    }
    if query.owner.is_some() && query.org_id.is_some() {
        return Err(AppError::BadRequest(
            "`owner` and `org_id` cannot be combined".into(),
        ));
    }
    // One listing of the ownerships, not a read per record. Before any shortcut
    // below, so `org_id` is refused outside its roster.
    let owned = match (query.owned, query.owner, &query.org_id) {
        (Some(wanted), _, _) => Some((wanted, owned_ids(state).await?)),
        (None, Some(FleetOwner::Mine), _) => Some((true, mine(state, user_id).await?)),
        // The organization's roster read, 404 outside it, as `GET /api/v1/orgs/{org_id}/clients`.
        (None, None, Some(org_id)) => {
            let (_, owners) =
                crate::api_v1::orgs::device_owners(state, caller, &OrgRef::Id(org_id.clone()))
                    .await?;
            Some((true, owners.into_keys().collect()))
        }
        (None, None, None) => None,
    };
    let tagged = if filters.tags.is_empty() {
        None
    } else {
        Some(
            client_cli::clients_with_all_tags(state.tag_store.as_ref(), &filters.tags)
                .await
                .map_err(|e| AppError::Internal(format!("failed to list clients by tag: {e:#}")))?,
        )
    };
    if filters.capabilities.matches_nobody() {
        tracing::debug!(
            user_id = %user_id,
            "the fleet capability query has no `requires` clause, so it matches no client"
        );
    }
    // An owner with no devices needs no scan either.
    if filters.capabilities.matches_nobody()
        || tagged.as_ref().is_some_and(|ids| ids.is_empty())
        || owned
            .as_ref()
            .is_some_and(|(wanted, ids)| *wanted && ids.is_empty())
    {
        return Ok(pipette_plan_types::ListPage::whole(Vec::new(), 0));
    }
    let matcher = CapabilityMatcher::load(&filters.capabilities, state.tag_store.as_ref())
        .await
        .map_err(|e| AppError::Internal(format!("failed to list client tags: {e:#}")))?;
    // The status, tag, owner and capability filters are applied per record, not
    // in the store, so a store page comes back short of the limit.
    let client_store = state.client_store.as_ref();
    let admits = |client: &Client| {
        status.is_none_or(|status| client.status == status)
            && tagged
                .as_ref()
                .is_none_or(|ids| ids.contains(&client.client_id))
            && owned
                .as_ref()
                .is_none_or(|(wanted, ids)| ids.contains(&client.client_id) == *wanted)
            && matcher.matches(client)
    };
    let page = match &owned {
        // An owner's devices page over their own ids, so the reads track the
        // owner's count rather than the fleet's.
        Some((true, ids)) => {
            let mut ids: Vec<ClientId> = ids.iter().cloned().collect();
            ids.sort_unstable();
            let ids = ids.as_slice();
            crate::list::page_filtered(
                limit,
                after,
                "clients",
                admits,
                |after, remaining| async move {
                    owned_page(client_store, ids, after.as_ref(), remaining).await
                },
            )
            .await?
        }
        _ => {
            crate::list::page_filtered(
                limit,
                after,
                "clients",
                admits,
                |after, remaining| async move {
                    client_store.list_clients(after.as_ref(), remaining).await
                },
            )
            .await?
        }
    };

    let rows = add_permissions(state, page.items).await?;
    Ok(pipette_plan_types::ListPage {
        items: fleet_views(state, caller, rows).await?,
        next_cursor: page.next_cursor,
        unreadable: page.unreadable,
    })
}

/// Every device an ownership names.
async fn owned_ids(state: &AppState) -> Result<HashSet<ClientId>, AppError> {
    Ok(state
        .org_store
        .list_ownerships()
        .await
        .map_err(|e| AppError::Internal(format!("failed to list the ownerships: {e:#}")))?
        .into_iter()
        .map(|ownership| ownership.client_id)
        .collect())
}

/// One store-shaped page of `ids` (ascending) past `after`: at most `remaining`
/// reads, resumed from the last id examined, as a store page is.
async fn owned_page(
    store: &dyn ClientStore,
    ids: &[ClientId],
    after: Option<&ClientId>,
    remaining: NonZeroUsize,
) -> anyhow::Result<Page<Client, ClientId>> {
    let start = after.map_or(0, |after| ids.partition_point(|id| id <= after));
    // Owned ids: a stream of borrowed ones breaks the handler's `Send` bound.
    let examined: Vec<ClientId> = ids
        .iter()
        .skip(start)
        .take(remaining.get())
        .cloned()
        .collect();
    let more = start.saturating_add(examined.len()) < ids.len();
    let resume_after = more.then(|| examined.last().cloned()).flatten();
    // A device deleted since its ownership was listed is skipped, as a store page skips it.
    let items: Vec<Option<Client>> = stream::iter(
        examined
            .into_iter()
            .map(|id| async move { store.get_client(&id).await }),
    )
    .buffered(STORAGE_CONCURRENCY)
    .try_collect()
    .await?;
    Ok(Page {
        items: items.into_iter().flatten().collect(),
        unreadable: Vec::new(),
        resume_after,
    })
}

/// Every device `user_id` owns as a person.
async fn mine(state: &AppState, user_id: &UserId) -> Result<HashSet<ClientId>, AppError> {
    Ok(org::devices_owned_by(&*state.org_store, user_id)
        .await
        .map_err(|e| AppError::Internal(format!("failed to list the ownerships: {e:#}")))?
        .into_iter()
        .collect())
}

async fn fleet_views(
    state: &AppState,
    caller: Caller<'_>,
    rows: Vec<Row>,
) -> Result<Vec<ClientListRow>, AppError> {
    let suspended: HashSet<ClientId> = state
        .todo_store
        .list_suspended_ids()
        .await
        .map_err(|e| AppError::Internal(format!("failed to list the suspensions: {e:#}")))?
        .into_iter()
        .collect();
    let reindexing: HashSet<ClientId> = state
        .todo_store
        .list_pending_reindex()
        .await
        .map_err(|e| AppError::Internal(format!("failed to list the reindex flags: {e:#}")))?
        .into_iter()
        .map(|(client_id, _)| client_id)
        .collect();
    let seen: HashMap<ClientId, DateTime<Utc>> = state
        .todo_store
        .list_liveness()
        .await
        .map_err(|e| AppError::Internal(format!("failed to list the liveness markers: {e:#}")))?
        .into_iter()
        .collect();
    let snapshot = backlog_snapshot(state).await;
    // The forward tree, the authoritative one, so the listing agrees with
    // `GET /api/v1/clients/{client_id}`.
    let mut tags: HashMap<ClientId, BTreeSet<Tag>> = HashMap::new();
    for (client_id, tag) in state
        .tag_store
        .list_forward_tag_markers()
        .await
        .map_err(|e| AppError::Internal(format!("failed to list the tags: {e:#}")))?
    {
        tags.entry(client_id).or_default().insert(tag);
    }
    // One read per row, so the page's cost does not grow with the fleet. Owned
    // ids, or the handler future fails axum's `Send` bound.
    let ids: Vec<ClientId> = rows
        .iter()
        .map(|(client, _)| client.client_id.clone())
        .collect();
    let owners: Vec<Option<ClientOwner>> = stream::iter(
        ids.into_iter()
            .map(|client_id| async move { state.org_store.get_ownership(&client_id).await }),
    )
    .buffered(STORAGE_CONCURRENCY)
    .try_collect()
    .await
    .map_err(|e| AppError::Internal(format!("failed to read the owners: {e:#}")))?;
    let reach = crate::api_v1::clients::device_reach(state, caller).await?;
    let administered: Vec<bool> =
        stream::iter(owners.iter().cloned().map(|owner| {
            let reach = reach.clone();
            async move {
                crate::api_v1::clients::administers(state, reach.as_ref(), owner.as_ref()).await
            }
        }))
        .buffered(STORAGE_CONCURRENCY)
        .try_collect()
        .await?;

    Ok(rows
        .into_iter()
        .zip(owners)
        .zip(administered)
        .map(|(((client, permissions), owner), administered)| {
            let derived_capabilities = normalized_flags(&client.device_profile);
            ClientListRow {
                suspended: suspended.contains(&client.client_id),
                reindex_pending: reindexing.contains(&client.client_id),
                last_seen_at: seen.get(&client.client_id).copied(),
                backlog: backlog(snapshot.as_ref(), &client.client_id),
                permissions: pipette_plan_types::ClientPermissionSet::from(
                    &client_permission::effective(permissions),
                ),
                // Absent is an untagged client: the empty set, as the single read answers.
                tags: tags.remove(&client.client_id).unwrap_or_default(),
                view: ClientView {
                    client: client.view(administered),
                    owner,
                },
                derived_capabilities,
            }
        })
        .collect())
}

/// The backlog snapshot, or `None` when the store cannot read it: it serves the
/// console only, so a failed read must not fail the request.
async fn backlog_snapshot(state: &AppState) -> Option<BacklogSnapshot> {
    state.todo_store.read_backlog().await.unwrap_or_else(|e| {
        tracing::warn!(error = %format!("{e:#}"), "backlog snapshot read failed; showing no backlog");
        None
    })
}

/// A device's backlog from the snapshot: `None` before the first count, and
/// zero for a device that the count did not name.
fn backlog(snapshot: Option<&BacklogSnapshot>, client_id: &ClientId) -> Option<Backlog> {
    snapshot.map(|snapshot| Backlog {
        job_count: snapshot.jobs.get(client_id).copied().unwrap_or(0),
        counted_at: snapshot.counted_at,
    })
}

/// A client record and its permission object: what a row is read from.
type Row = (Client, Option<ClientPermissionSet>);

async fn add_permissions(state: &AppState, clients: Vec<Client>) -> Result<Vec<Row>, AppError> {
    stream::iter(clients.into_iter().map(|client| async move {
        let permissions = state
            .client_store
            .get_client_permissions(&client.client_id)
            .await
            .map_err(|e| AppError::Internal(format!("failed to read the permissions: {e:#}")))?;
        Ok((client, permissions))
    }))
    .buffered(STORAGE_CONCURRENCY)
    .try_collect()
    .await
}

// GET /api/v1/clients/{client_id}
pub async fn get_client(
    State(state): State<AppState>,
    active: ActiveUser,
    ApiPath(Parsed(client_id)): ApiPath<Parsed<ClientId>>,
) -> Result<impl IntoResponse, AppError> {
    let caller = active.caller();
    Ok(Json(record(&state, caller, &client_id).await?))
}

/// [`one`], with what the reader may do to the device. Drawn, not enforced:
/// each write's route decides again.
async fn record(
    state: &AppState,
    caller: Caller<'_>,
    client_id: &ClientId,
) -> Result<ClientDetail, AppError> {
    let row = one(state, client_id).await?;
    let (owning, reach, read, backlog_by_campaign) = futures::try_join!(
        async {
            org::owning_org(&*state.org_store, client_id)
                .await
                .map_err(|e| AppError::internal(e.context("failed to resolve the owner")))
        },
        crate::api_v1::clients::device_reach(state, caller),
        crate::api_v1::users::read_reach(state, caller),
        async { Ok(campaign_backlog(state, caller, client_id).await) },
    )?;
    let may_administer = reach
        .as_ref()
        .is_some_and(|scope| scope.may_administer(owning.as_ref()));
    // The rollup is roster data the fleet grant does not read: it goes only to a
    // reader of that roster or the administrator scoped to it.
    let owning_org_id = owning.filter(|org_id| {
        read.covers(Some(org_id)) || reach.as_ref() == Some(&DeviceScope::Org(org_id.clone()))
    });
    Ok(ClientDetail {
        view: row.view(may_administer),
        may_administer,
        owning_org_id,
        backlog_by_campaign,
    })
}

/// The device's backlog by campaign. The value is `None` if no run counted
/// the backlog, or if the read fails. The breakdown serves the console only,
/// so a failed read must not fail the request.
async fn campaign_backlog(
    state: &AppState,
    caller: Caller<'_>,
    client_id: &ClientId,
) -> Option<BacklogByCampaign> {
    read_campaign_backlog(state, caller, client_id)
        .await
        .unwrap_or_else(|e| {
            tracing::warn!(
                error = %e,
                "campaign backlog read failed; showing no breakdown"
            );
            None
        })
}

/// Read the snapshot, the campaign reach of the reader, and the campaign
/// records that the entry of the device names.
async fn read_campaign_backlog(
    state: &AppState,
    caller: Caller<'_>,
    client_id: &ClientId,
) -> Result<Option<BacklogByCampaign>, AppError> {
    let snapshot = state
        .todo_store
        .read_campaign_backlog()
        .await
        .map_err(|e| AppError::internal(e.context("failed to read the campaign backlog")))?;
    let Some(snapshot) = snapshot else {
        return Ok(None);
    };
    let named: Vec<CampaignId> = snapshot
        .devices
        .get(client_id)
        .map(|backlog| backlog.by_campaign.keys().cloned().collect())
        .unwrap_or_default();
    // A device whose backlog names no campaign needs no reach.
    let reach = if named.is_empty() {
        None
    } else {
        CampaignReach::over_records_if_granted(state, caller).await?
    };
    // A reader with no campaign grant reads no campaign record.
    let named = if reach.is_some() { named } else { Vec::new() };
    let reach = &reach;
    let campaigns: HashMap<CampaignId, Campaign> = stream::iter(named)
        .map(|campaign_id| async move {
            state
                .campaign_store
                .get_campaign(&campaign_id)
                .await
                .map_err(|e| AppError::internal(e.context("failed to read a backlog campaign")))
        })
        .buffer_unordered(STORAGE_CONCURRENCY)
        // The map holds only the records that the reader may read.
        .try_filter_map(|campaign| async move {
            Ok(campaign
                .filter(|campaign| reach.as_ref().is_some_and(|reach| reach.admits(campaign)))
                .map(|campaign| (campaign.campaign_id.clone(), campaign)))
        })
        .try_collect()
        .await?;
    Ok(Some(by_campaign(&snapshot, client_id, &campaigns)))
}

/// Split the device's entry into the campaigns in `campaigns` and a sum of the
/// rest. `campaigns` holds only the records that the reader may read, so a
/// campaign with no record goes into the sum, the same as a campaign outside
/// the reach.
fn by_campaign(
    snapshot: &CampaignBacklogSnapshot,
    client_id: &ClientId,
    campaigns: &HashMap<CampaignId, Campaign>,
) -> BacklogByCampaign {
    let entry = snapshot.devices.get(client_id).cloned().unwrap_or_default();
    let (mut readable, unreadable) = entry.by_campaign.into_iter().fold(
        (Vec::new(), 0),
        |(mut readable, unreadable), (campaign_id, jobs)| match campaigns.get(&campaign_id) {
            Some(campaign) => {
                readable.push(BacklogCampaign {
                    campaign_id,
                    name: campaign.name.clone(),
                    job_count: jobs,
                });
                (readable, unreadable)
            }
            None => (readable, unreadable + jobs),
        },
    );
    readable.sort_by(|a, b| {
        b.job_count
            .cmp(&a.job_count)
            .then_with(|| a.name.as_str().cmp(b.name.as_str()))
            .then_with(|| a.campaign_id.cmp(&b.campaign_id))
    });
    BacklogByCampaign {
        campaigns: readable,
        outside_campaign_job_count: entry.outside_campaigns,
        unreadable_job_count: unreadable,
        counted_at: snapshot.counted_at,
    }
}

/// One client's row, scoped by the same fleet grant the listing takes. Its reads
/// are independent, so they run concurrently.
pub(crate) async fn one(state: &AppState, client_id: &ClientId) -> Result<FleetRow, AppError> {
    let (client, permissions, suspended, reindex_pending, last_seen_at, snapshot, tags, owner) = futures::try_join!(
        async {
            state
                .client_store
                .get_client(client_id)
                .await
                .map_err(|e| AppError::Internal(format!("failed to read the client: {e:#}")))
        },
        async {
            state
                .client_store
                .get_client_permissions(client_id)
                .await
                .map_err(|e| AppError::Internal(format!("failed to read the permissions: {e:#}")))
        },
        async {
            Ok(state
                .todo_store
                .read_suspension(client_id)
                .await
                .map_err(|e| AppError::Internal(format!("failed to read the suspension: {e:#}")))?
                .is_some())
        },
        async {
            state
                .todo_store
                .has_pending_reindex(client_id)
                .await
                .map_err(|e| AppError::Internal(format!("failed to read the reindex flag: {e:#}")))
        },
        async {
            state
                .todo_store
                .read_liveness(client_id)
                .await
                .map_err(|e| {
                    AppError::Internal(format!("failed to read the liveness marker: {e:#}"))
                })
        },
        async { Ok::<_, AppError>(backlog_snapshot(state).await) },
        async {
            state
                .tag_store
                .get_client_tags(client_id)
                .await
                .map_err(|e| AppError::Internal(format!("failed to read the tags: {e:#}")))
        },
        async {
            state
                .org_store
                .get_ownership(client_id)
                .await
                .map_err(|e| AppError::Internal(format!("failed to read the owner: {e:#}")))
        },
    )?;

    // Checked after the join: an unknown client is rare. "Out of reach" and
    // "does not exist" are one answer, as a narrowed reach will need.
    let client = client.ok_or_else(|| AppError::NotFound("client not found".into()))?;

    Ok(FleetRow {
        client,
        owner,
        tags,
        permissions: pipette_plan_types::ClientPermissionSet::from(&client_permission::effective(
            permissions,
        )),
        reindex_pending,
        suspended,
        last_seen_at,
        backlog: backlog(snapshot.as_ref(), client_id),
    })
}

/// A device's row as [`one`] reads it, its record still whole.
pub(crate) struct FleetRow {
    client: Client,
    owner: Option<ClientOwner>,
    tags: BTreeSet<Tag>,
    permissions: pipette_plan_types::ClientPermissionSet,
    reindex_pending: bool,
    suspended: bool,
    last_seen_at: Option<DateTime<Utc>>,
    backlog: Option<Backlog>,
}

impl FleetRow {
    /// The row a reader sees: the address only for one who administers the device.
    fn view(self, administered: bool) -> ClientListRow {
        ClientListRow {
            derived_capabilities: normalized_flags(&self.client.device_profile),
            view: ClientView {
                client: self.client.view(administered),
                owner: self.owner,
            },
            tags: self.tags,
            permissions: self.permissions,
            reindex_pending: self.reindex_pending,
            suspended: self.suspended,
            last_seen_at: self.last_seen_at,
            backlog: self.backlog,
        }
    }
}

// GET /api/v1/clients/{client_id}/owners
pub async fn list_client_owners(
    State(state): State<AppState>,
    active: HumanUser,
    ApiPath(Parsed(client_id)): ApiPath<Parsed<ClientId>>,
    ApiQuery(query): ApiQuery<OwnerLookupQuery>,
) -> Result<impl IntoResponse, AppError> {
    let caller = active.caller();
    let choices = device_choices(&state, caller, &client_id).await?;
    Ok(Json(choices.matching(&query.q, query.owner_type)))
}
