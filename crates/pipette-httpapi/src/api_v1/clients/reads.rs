//! `GET /api/v1/clients`, `GET /api/v1/clients/{client_id}` and
//! `GET /api/v1/clients/capabilities`: the fleet, read by a user. `read_clients`
//! shows the whole fleet too, until per-submitter restrictions (PIP-526) land.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::num::NonZeroUsize;

use axum::Json;
use axum::extract::State;
use axum::response::IntoResponse;
use futures::stream;
use futures::stream::{StreamExt, TryStreamExt};
use jiff::Timestamp;

use pipette_core::campaign::Campaign;
use pipette_core::client::{Client, ClientStatus, normalized_flags};
use pipette_core::client_permission;
use pipette_core::client_permission::ClientPermissionSet;
use pipette_pipeline::client_cli;
use pipette_pipeline::client_cli::{CapabilityMatcher, CapabilityVocabulary};
use pipette_pipeline::org::OrgRef;
use pipette_plan_types::{
    Backlog, BacklogByCampaign, BacklogCampaign, CampaignId, CapabilityVocabularyQuery,
    ClientDetail, ClientId, ClientListQuery, ClientListRow, ClientOwner, ClientView,
    OwnerLookupQuery, Tag,
};
use pipette_store::{CampaignBacklog, ClientStore, Page, STORAGE_CONCURRENCY};

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
    // The other owned listings refuse `owner=mine` too; read alone, it would be the whole fleet.
    if pairs.iter().any(|(key, _)| key == "owner") {
        return Err(AppError::BadRequest(
            "owner is not a filter here: an organization owns every device; filter by org_id"
                .into(),
        ));
    }
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
    if query.owned.is_some() && query.org_id.is_some() {
        return Err(AppError::BadRequest(
            "`owned` cannot be combined with `org_id`".into(),
        ));
    }
    // One listing of the ownerships, not a read per record. Before any shortcut
    // below, so `org_id` is refused outside its roster.
    let owned = match (query.owned, &query.org_id) {
        (Some(wanted), _) => Some((wanted, owned_ids(state).await?)),
        // The organization's roster read, 404 outside it, as `GET /api/v1/orgs/{org_id}/clients`.
        (None, Some(org_id)) => {
            let (_, owners) =
                crate::api_v1::orgs::device_owners(state, caller, &OrgRef::Id(org_id.clone()))
                    .await?;
            Some((true, owners.into_keys().collect()))
        }
        (None, None) => None,
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

async fn fleet_views(
    state: &AppState,
    caller: Caller<'_>,
    rows: Vec<Row>,
) -> Result<Vec<ClientListRow>, AppError> {
    let reindexing: HashSet<ClientId> = state
        .todo_store
        .list_pending_reindex()
        .await
        .map_err(|e| AppError::Internal(format!("failed to list the reindex flags: {e:#}")))?
        .into_iter()
        .map(|request| request.client_id)
        .collect();
    let seen: HashMap<ClientId, Timestamp> = state
        .todo_store
        .list_liveness()
        .await
        .map_err(|e| AppError::Internal(format!("failed to list the liveness markers: {e:#}")))?
        .into_iter()
        .collect();
    let counts = backlog_counts(state).await;
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
    Ok(rows
        .into_iter()
        .zip(owners)
        .map(|((client, permissions), owner)| {
            let administered = crate::api_v1::clients::administers(reach.as_ref(), owner.as_ref());
            let derived_capabilities = normalized_flags(&client.device_profile);
            ClientListRow {
                reindex_pending: reindexing.contains(&client.client_id),
                last_seen_at: seen.get(&client.client_id).copied(),
                backlog: backlog(counts.as_ref(), &client.client_id),
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

/// The count of claimable jobs for each device, and the time of the count.
/// `None` when the store cannot count: the count serves the console only, so
/// a failed count must not fail the request.
async fn backlog_counts(state: &AppState) -> Option<BacklogCounts> {
    let counted_at = Timestamp::now();
    state
        .todo_store
        .backlog()
        .await
        .map(|jobs| (counted_at, jobs))
        .inspect_err(|e| {
            tracing::warn!(error = %format!("{e:#}"), "backlog query failed; showing no backlog");
        })
        .ok()
}

type BacklogCounts = (Timestamp, BTreeMap<ClientId, u64>);

/// A device's backlog from the counts: `None` if the count failed, and zero
/// for a device with no claimable job.
fn backlog(counts: Option<&BacklogCounts>, client_id: &ClientId) -> Option<Backlog> {
    counts.map(|(counted_at, jobs)| Backlog {
        job_count: jobs.get(client_id).copied().unwrap_or(0),
        counted_at: *counted_at,
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
    // One request time for both counts, so the console pairs them.
    let counted_at = row
        .backlog
        .as_ref()
        .map_or_else(Timestamp::now, |backlog| backlog.counted_at);
    let (reach, backlog_by_campaign) =
        futures::try_join!(crate::api_v1::clients::device_reach(state, caller), async {
            Ok(campaign_backlog(state, caller, client_id, counted_at).await)
        },)?;
    let may_administer = reach
        .as_ref()
        .is_some_and(|scope| scope.may_administer(row.owner.as_ref().map(ClientOwner::org_id)));
    Ok(ClientDetail {
        view: row.view(may_administer),
        may_administer,
        backlog_by_campaign,
    })
}

/// The device's backlog by campaign. The value is `None` if the count fails.
/// The breakdown serves the console only, so a failed count must not fail the
/// request.
async fn campaign_backlog(
    state: &AppState,
    caller: Caller<'_>,
    client_id: &ClientId,
    counted_at: Timestamp,
) -> Option<BacklogByCampaign> {
    read_campaign_backlog(state, caller, client_id, counted_at)
        .await
        .inspect_err(|e| {
            tracing::warn!(error = %e, "campaign backlog query failed; showing no breakdown");
        })
        .ok()
}

/// Count the device's backlog by campaign, and read the campaign reach of the
/// reader and the campaign records that the count names.
async fn read_campaign_backlog(
    state: &AppState,
    caller: Caller<'_>,
    client_id: &ClientId,
    counted_at: Timestamp,
) -> Result<BacklogByCampaign, AppError> {
    let creating_max_age = jiff::SignedDuration::try_from(std::time::Duration::from_secs(
        state.config.plan_creating_max_age_secs.get(),
    ))
    .map_err(|error| AppError::internal(error.into()))?;
    let entry = state
        .todo_store
        .campaign_backlog(client_id, counted_at - creating_max_age)
        .await
        .map_err(|e| AppError::internal(e.context("failed to count the campaign backlog")))?;
    let named: Vec<CampaignId> = entry.by_campaign.keys().cloned().collect();
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
    Ok(by_campaign(entry, counted_at, &campaigns))
}

/// Split the device's count into the campaigns in `campaigns` and a sum of the
/// rest. `campaigns` holds only the records that the reader may read, so a
/// campaign with no record goes into the sum, the same as a campaign outside
/// the reach.
fn by_campaign(
    entry: CampaignBacklog,
    counted_at: Timestamp,
    campaigns: &HashMap<CampaignId, Campaign>,
) -> BacklogByCampaign {
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
        counted_at,
    }
}

/// One client's row, scoped by the same fleet grant the listing takes. Its reads
/// are independent, so they run concurrently.
pub(crate) async fn one(state: &AppState, client_id: &ClientId) -> Result<FleetRow, AppError> {
    let (client, permissions, reindex_pending, last_seen_at, counts, tags, owner) = futures::try_join!(
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
        async { Ok::<_, AppError>(backlog_counts(state).await) },
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
        last_seen_at,
        backlog: backlog(counts.as_ref(), client_id),
    })
}

/// A device's row as [`one`] reads it, its record still whole.
pub(crate) struct FleetRow {
    client: Client,
    owner: Option<ClientOwner>,
    tags: BTreeSet<Tag>,
    permissions: pipette_plan_types::ClientPermissionSet,
    reindex_pending: bool,
    last_seen_at: Option<Timestamp>,
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
