//! Read-only descriptor catalog routes.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::num::NonZeroUsize;

use axum::{Json, extract::State, http::StatusCode};
use chrono::Utc;

use pipette_core::{
    client::{Client, ClientStatus},
    matching::FleetTags,
};
use pipette_pipeline::descriptor_catalog_admin::{self, CatalogTransition, CatalogWriteError};
use pipette_pipeline::descriptor_names::{self, CatalogReadScope, NameView};
use pipette_pipeline::descriptor_offer::{self, OfferError, Resolution};
use pipette_pipeline::model_params;
use pipette_pipeline::org::{OrgRef, RosterScope};
use pipette_plan_types::{
    AddDescriptorMemberRequest, CatalogName, CatalogScope, ChangeDescriptorEntryStateRequest,
    ClientId, CreateDescriptorEntryRequest, DescriptorCatalogQuery, DescriptorCatalogShortList,
    DescriptorCatalogShortListItem, DescriptorDigestNames, DescriptorDigestQuery, DescriptorEntry,
    DescriptorEntryAction, DescriptorEntryCreator, DescriptorEntryListRow, DescriptorEntryLookup,
    DescriptorEntryState, DescriptorKind, DescriptorMember, DescriptorMemberCoverage,
    DescriptorMemberDescriptor, DescriptorMemberKey, DescriptorMemberParameters,
    DescriptorMemberSummary, DescriptorMemberView, DescriptorNameHit, DescriptorNameQuery,
    DescriptorWithheldMember, DescriptorWithheldReasonCode, DescriptorWithheldReport, ListPage,
    ModelKey, ModelType, OrgId, QualifiedCatalogName, Runtime, RuntimeType, UserPermission,
    VaultEntry,
};
use pipette_store::{
    CatalogEntryKey, ClientListing, DescriptorCatalogStore, DescriptorEntryFilter, drain_clients,
    drain_model_params, is_content_digest, warn_unreadable_clients, warn_unreadable_model_params,
};

use crate::{
    api_v1::orgs::{OrgPath, require_in_scope, roster_reach},
    auth::{ActiveUser, Caller},
    error::AppError,
    extract::{ApiJson, ApiPath, ApiQuery, Parsed},
    list::{list_cursor, list_limit, page_filtered},
    names::catalog_read_scope,
    state::AppState,
};

const NEEDS_DATABASE: &str = "the descriptor catalog is kept only on a [database]";
const BATCH_MAX: usize = 100;

fn catalog(state: &AppState) -> Result<&dyn DescriptorCatalogStore, AppError> {
    state
        .descriptor_catalog_store
        .as_deref()
        .ok_or_else(|| AppError::NotFound(NEEDS_DATABASE.into()))
}

/// The organizations whose org entries the caller changes: every organization
/// on `edit_any_catalog`, its own on `edit_catalog`, and none without either.
pub async fn edit_reach(state: &AppState, caller: Caller<'_>) -> Result<RosterScope, AppError> {
    roster_reach(
        state,
        caller,
        UserPermission::EditCatalog,
        UserPermission::EditAnyCatalog,
    )
    .await
}

/// Convert the read scope to a roster scope. The handler uses it to refuse an
/// organization that the caller cannot read.
fn roster_scope(read: &CatalogReadScope) -> RosterScope {
    match read {
        CatalogReadScope::Unaffiliated => RosterScope::Unaffiliated,
        CatalogReadScope::Org(org_id) => RosterScope::Org(org_id.clone()),
        CatalogReadScope::EveryOrg { .. } => RosterScope::Fleet,
    }
}

async fn name_view<'a>(
    state: &AppState,
    store: &dyn DescriptorCatalogStore,
    read: CatalogReadScope,
    scopes: impl IntoIterator<Item = &'a CatalogScope>,
) -> Result<NameView, AppError> {
    NameView::load(store, state.org_store.as_ref(), read, scopes)
        .await
        .map_err(AppError::internal)
}

fn coverage(
    state: &AppState,
    member: &pipette_plan_types::DescriptorMember,
) -> DescriptorMemberCoverage {
    match descriptor_offer::coverage(&state.config.descriptor_catalog, member) {
        Ok(_) => DescriptorMemberCoverage::Covered,
        Err(OfferError::NoRule { .. }) => DescriptorMemberCoverage::NoRule,
        Err(
            OfferError::Unsupported { .. }
            | OfferError::ModelMember
            | OfferError::NoLoadingRuntime { .. },
        ) => DescriptorMemberCoverage::Unsupported,
        Err(OfferError::NotMet) => DescriptorMemberCoverage::Covered,
    }
}

/// The offer-rule coverage of a runtime member. A model member has none.
fn runtime_coverage(
    state: &AppState,
    member: &DescriptorMember,
) -> Option<DescriptorMemberCoverage> {
    member.runtime().map(|_| coverage(state, member))
}

/// One entry as this caller sees it. `display_name` comes from [`NameView`].
fn row(
    state: &AppState,
    entry: DescriptorEntry,
    display_name: QualifiedCatalogName,
    include_members: bool,
    offer_counts: Option<&HashMap<String, usize>>,
    parameters: &ParameterRows,
) -> Result<DescriptorEntryListRow, AppError> {
    let descriptor_kind = entry.descriptor_kind();
    let descriptor_types = entry.descriptor_types();
    let member_summaries = entry
        .members
        .iter()
        .map(|member| {
            let member_key = member.key();
            DescriptorMemberSummary {
                selector: descriptor_names::member_selector(&display_name, &member_key),
                member_key,
                coverage: runtime_coverage(state, member),
            }
        })
        .collect();
    let members = include_members
        .then(|| {
            entry
                .members
                .iter()
                .map(|member| member_view(state, &display_name, member, offer_counts, parameters))
                .collect::<Result<Vec<_>, _>>()
        })
        .transpose()?;
    Ok(DescriptorEntryListRow {
        name: entry.name,
        scope: entry.scope,
        display_name: display_name.to_string(),
        kind: entry.kind,
        state: entry.state,
        release_time: entry.release_time,
        created_at: entry.created_at,
        created_by: entry.created_by,
        descriptor_kind,
        descriptor_types,
        member_count: entry.members.len(),
        member_summaries,
        members,
    })
}

/// The model parameter table's counts by key, read once for one answer.
type ParameterRows = HashMap<ModelKey, DescriptorMemberParameters>;

/// Read the model parameter table when `entries` hold a model entry. The
/// counts are for display only, so a failed read leaves every member without
/// counts and does not fail the answer.
async fn parameter_rows<'a>(
    state: &AppState,
    mut entries: impl Iterator<Item = &'a DescriptorEntry>,
) -> ParameterRows {
    if !entries.any(|entry| entry.descriptor_kind() == DescriptorKind::Model) {
        return ParameterRows::new();
    }
    match drain_model_params(state.model_params_store.as_ref()).await {
        Ok(listing) => {
            warn_unreadable_model_params(&listing.unreadable);
            listing
                .items
                .into_iter()
                .map(|row| {
                    let counts = DescriptorMemberParameters {
                        total_millions: row.total_millions,
                        active_millions: row.active_millions,
                    };
                    (row.key, counts)
                })
                .collect()
        }
        Err(error) => {
            tracing::warn!(%error, "model parameter table unreadable for catalog members");
            ParameterRows::new()
        }
    }
}

/// An unreadable record could be approved and offered, so a partial fleet
/// cannot produce a trustworthy count for any member.
fn approved_fleet(fleet: ClientListing) -> Option<Vec<Client>> {
    if !fleet.unreadable.is_empty() {
        warn_unreadable_clients(&fleet.unreadable);
        tracing::debug!(
            unreadable = fleet.unreadable.len(),
            "catalog offer counts unavailable"
        );
        return None;
    }
    Some(
        fleet
            .items
            .into_iter()
            .filter(|client| client.status == ClientStatus::Approved)
            .collect(),
    )
}

/// Count devices that pass each member's rule, independently of entry
/// precedence. An unreadable device makes every count incomplete.
async fn offered_device_counts(
    state: &AppState,
    entries: &[DescriptorEntry],
) -> Option<HashMap<String, usize>> {
    let fleet = match drain_clients(state.client_store.as_ref()).await {
        Ok(fleet) => fleet,
        Err(error) => {
            tracing::warn!(%error, "catalog offer counts unavailable: fleet read failed");
            return None;
        }
    };
    let approved = approved_fleet(fleet)?;
    let markers = match state.tag_store.list_forward_tag_markers().await {
        Ok(markers) => markers,
        Err(error) => {
            tracing::warn!(%error, "catalog offer counts unavailable: tag read failed");
            return None;
        }
    };
    let tags = FleetTags::new(markers);
    let counts = entries
        .iter()
        .flat_map(|entry| entry.members.iter())
        .filter(|member| member.runtime().is_some())
        .fold(HashMap::new(), |mut counts, member| {
            counts.entry(member.digest().to_owned()).or_insert_with(|| {
                approved
                    .iter()
                    .filter(|client| {
                        descriptor_offer::offer(
                            &state.config.descriptor_catalog,
                            member,
                            client,
                            &tags,
                        )
                        .is_ok()
                    })
                    .count()
            });
            counts
        });
    tracing::debug!(
        approved_devices = approved.len(),
        members = counts.len(),
        "counted catalog offers"
    );
    Some(counts)
}

#[cfg(test)]
#[test]
fn an_unreadable_device_makes_offer_counts_unavailable() {
    let fleet = ClientListing {
        items: Vec::new(),
        unreadable: vec![pipette_store::UnreadableObjectError {
            key: "clients/ev1_bad.json".into(),
            reason: "invalid record".into(),
        }],
    };
    assert!(approved_fleet(fleet).is_none());
}

fn write_error(error: CatalogWriteError) -> AppError {
    match error {
        CatalogWriteError::Invalid(error) => AppError::BadRequest(format!("{error:#}")),
        CatalogWriteError::Conflict(message) => AppError::Conflict(message),
        CatalogWriteError::NotFound(message) => AppError::NotFound(message),
        CatalogWriteError::Storage(error) => AppError::internal(error),
        CatalogWriteError::Upstream(error) => AppError::BadGateway(format!("{error:#}")),
    }
}

async fn write_scope(
    state: &AppState,
    caller: Caller<'_>,
    org_id: OrgId,
) -> Result<CatalogScope, AppError> {
    let org = require_in_scope(
        state,
        &edit_reach(state, caller).await?,
        &OrgRef::Id(org_id),
    )
    .await?;
    Ok(CatalogScope::Org(org.org_id))
}

async fn write_display_name(
    state: &AppState,
    store: &dyn DescriptorCatalogStore,
    caller: Caller<'_>,
    scope: &CatalogScope,
    name: &CatalogName,
) -> Result<QualifiedCatalogName, AppError> {
    let read = catalog_read_scope(state, caller).await?;
    let view = name_view(state, store, read, std::iter::once(scope)).await?;
    view.display_name(scope, name).ok_or_else(|| {
        AppError::Internal("the creator cannot read the catalog entry it writes".into())
    })
}

/// The pinned Hugging Face references of `members`, joined with commas. Empty
/// for runtime members.
fn model_refs<'a>(members: impl Iterator<Item = &'a DescriptorMember>) -> String {
    members
        .map(descriptor_catalog_admin::member_refs)
        .filter(|refs| !refs.is_empty())
        .collect::<Vec<_>>()
        .join(",")
}

async fn written_row(
    state: &AppState,
    entry: DescriptorEntry,
    mut shown: QualifiedCatalogName,
) -> Result<DescriptorEntryListRow, AppError> {
    // The path can use different letter case from the name stored in the entry.
    shown.name = entry.name.clone();
    let parameters = parameter_rows(state, std::iter::once(&entry)).await;
    row(state, entry, shown, true, None, &parameters)
}

// POST /api/v1/orgs/{org_id}/descriptor-catalog
pub async fn create_org_descriptor_entry(
    State(state): State<AppState>,
    active: ActiveUser,
    ApiPath(Parsed(org_id)): ApiPath<Parsed<OrgId>>,
    ApiJson(body): ApiJson<CreateDescriptorEntryRequest>,
) -> Result<(StatusCode, Json<DescriptorEntryListRow>), AppError> {
    let caller = active.caller();
    let scope = write_scope(&state, caller, org_id).await?;
    let store = catalog(&state)?;
    let shown = write_display_name(&state, store, caller, &scope, &body.name).await?;
    let entry = descriptor_catalog_admin::create_entry_with_vault(
        store,
        &state.hf_imports.hub,
        state.vault_store.as_ref(),
        &scope,
        body,
        DescriptorEntryCreator::Account(caller.user.user_id.clone()),
        Utc::now(),
    )
    .await
    .map_err(write_error)?;
    tracing::info!(
        catalog_name = %entry.name,
        org_id = %scope,
        user_id = %caller.user.user_id,
        member_keys = %descriptor_catalog_admin::member_keys(&entry.members),
        hf_refs = %model_refs(entry.members.iter()),
        "catalog entry created"
    );
    let view = written_row(&state, entry, shown).await?;
    Ok((StatusCode::CREATED, Json(view)))
}

// POST /api/v1/orgs/{org_id}/descriptor-catalog/{name}/members
pub async fn add_org_descriptor_member(
    State(state): State<AppState>,
    active: ActiveUser,
    ApiPath((Parsed(org_id), Parsed(name))): ApiPath<(Parsed<OrgId>, Parsed<CatalogName>)>,
    ApiJson(body): ApiJson<AddDescriptorMemberRequest>,
) -> Result<Json<DescriptorEntryListRow>, AppError> {
    let caller = active.caller();
    let scope = write_scope(&state, caller, org_id).await?;
    let store = catalog(&state)?;
    let shown = write_display_name(&state, store, caller, &scope, &name).await?;
    let entry = descriptor_catalog_admin::add_entry_member_with_vault(
        store,
        &state.hf_imports.hub,
        state.vault_store.as_ref(),
        &scope,
        &name,
        body,
        Utc::now(),
    )
    .await
    .map_err(write_error)?;
    let added = entry.members.last();
    tracing::info!(
        catalog_name = %entry.name,
        org_id = %scope,
        user_id = %caller.user.user_id,
        member_key = %descriptor_catalog_admin::member_key(added),
        hf_refs = %model_refs(std::iter::once(added)),
        "catalog member appended"
    );
    Ok(Json(written_row(&state, entry, shown).await?))
}

// POST /api/v1/orgs/{org_id}/descriptor-catalog/{name}/state
pub async fn change_org_descriptor_entry_state(
    State(state): State<AppState>,
    active: ActiveUser,
    ApiPath((Parsed(org_id), Parsed(name))): ApiPath<(Parsed<OrgId>, Parsed<CatalogName>)>,
    ApiJson(body): ApiJson<ChangeDescriptorEntryStateRequest>,
) -> Result<StatusCode, AppError> {
    let caller = active.caller();
    let scope = write_scope(&state, caller, org_id).await?;
    let store = catalog(&state)?;
    let action = body.action;
    let entry = store
        .get_entry(&scope, &name)
        .await
        .map_err(AppError::internal)?
        .ok_or_else(|| AppError::NotFound(format!("no catalog entry {name}")))?;
    if entry.state == DescriptorEntryState::Tombstoned {
        return Err(AppError::Conflict(format!(
            "catalog entry {name} is tombstoned"
        )));
    }
    let transition = match action {
        DescriptorEntryAction::Pin => Some(CatalogTransition::Pin),
        DescriptorEntryAction::Unpin => Some(CatalogTransition::Unpin),
        DescriptorEntryAction::Hide => Some(CatalogTransition::Hide),
        DescriptorEntryAction::Unhide => Some(CatalogTransition::Unhide),
        DescriptorEntryAction::Tombstone => None,
    };
    match transition {
        None => {
            let confirmation = body.confirmation.ok_or_else(|| {
                AppError::BadRequest("confirmation is required for tombstone".into())
            })?;
            if confirmation != entry.name.as_str() {
                return Err(AppError::BadRequest(format!(
                    "confirmation did not match {}; nothing changed",
                    entry.name
                )));
            }
            descriptor_catalog_admin::tombstone_entry(store, &scope, &name)
                .await
                .map_err(write_error)?;
        }
        Some(transition) => {
            if body.confirmation.is_some() {
                return Err(AppError::BadRequest(
                    "confirmation is only accepted for tombstone".into(),
                ));
            }
            descriptor_catalog_admin::transition_entry(store, &scope, &name, transition)
                .await
                .map_err(write_error)?;
        }
    }
    tracing::info!(catalog_name = %name, org_id = %scope, user_id = %caller.user.user_id, ?action, "catalog entry state changed");
    Ok(StatusCode::NO_CONTENT)
}

fn member_view(
    state: &AppState,
    name: &QualifiedCatalogName,
    member: &DescriptorMember,
    offer_counts: Option<&HashMap<String, usize>>,
    parameters: &ParameterRows,
) -> Result<DescriptorMemberView, AppError> {
    let member_key = member.key();
    let descriptor = member.descriptor();
    let plan_entry = match &descriptor {
        DescriptorMemberDescriptor::Runtime(runtime) => VaultEntry::Runtime(runtime.clone()),
        DescriptorMemberDescriptor::Model(model) => VaultEntry::Model(model.clone()),
    };
    let plan_toml = plan_entry
        .to_toml()
        .map_err(|e| AppError::internal(e.into()))?;
    let selector = descriptor_names::member_selector(name, &member_key);
    let model_key = member
        .model()
        .and_then(model_params::model_key)
        .and_then(|key| match ModelKey::try_new(key.clone()) {
            Ok(model_key) => Some(model_key),
            Err(error) => {
                tracing::warn!(model_key = %key, selector = %selector, %error, "catalog member's model key is not a valid table key");
                None
            }
        });
    let counts = model_key
        .as_ref()
        .and_then(|key| parameters.get(key).copied());
    Ok(DescriptorMemberView {
        selector,
        member_key,
        coverage: runtime_coverage(state, member),
        offered_device_count: offer_counts.and_then(|counts| counts.get(member.digest()).copied()),
        digest: member.digest().to_owned(),
        added_at: member.added_at(),
        descriptor,
        plan_toml,
        model_key,
        parameters: counts,
    })
}

fn withheld_reason_code(reason: &OfferError) -> DescriptorWithheldReasonCode {
    match reason {
        OfferError::NoRule { .. } => DescriptorWithheldReasonCode::NoRule,
        OfferError::Unsupported { .. } | OfferError::ModelMember => {
            DescriptorWithheldReasonCode::Unsupported
        }
        OfferError::NotMet => DescriptorWithheldReasonCode::NotMet,
        OfferError::NoLoadingRuntime { .. } => DescriptorWithheldReasonCode::NoRuntime,
    }
}

/// Every pinned and active global entry, newest release first, and the number
/// of unreadable entries.
async fn short_list_entries(
    store: &dyn DescriptorCatalogStore,
) -> Result<(Vec<DescriptorEntry>, usize), AppError> {
    let filter = DescriptorEntryFilter {
        scopes: [CatalogScope::Global].into(),
        states: BTreeSet::from([DescriptorEntryState::Pinned, DescriptorEntryState::Active]),
        ..Default::default()
    };
    let mut after = None;
    let mut entries = Vec::new();
    let mut unreadable = 0usize;
    loop {
        let page = store
            .list_entries(
                &filter,
                after.as_ref(),
                NonZeroUsize::new(200).unwrap_or(NonZeroUsize::MIN),
            )
            .await
            .map_err(AppError::internal)?;
        for bad in &page.unreadable {
            tracing::warn!(path = %bad.key, error = %bad.reason, "unreadable descriptor catalog entry");
        }
        unreadable += page.unreadable.len();
        entries.extend(page.items);
        match page.resume_after {
            Some(cursor) => after = Some(cursor),
            None => break,
        }
    }
    Ok((entries, unreadable))
}

/// The short list names entries with the bare name: it holds global entries
/// only.
fn bare_name(entry: &DescriptorEntry) -> QualifiedCatalogName {
    QualifiedCatalogName {
        qualifier: None,
        name: entry.name.clone(),
    }
}

/// Split the offer verdicts of `entry`'s members into the offered members and
/// the withheld records.
fn split_verdicts<'a>(
    entry: &DescriptorEntry,
    verdicts: impl Iterator<Item = (&'a DescriptorMember, Result<(), OfferError>)>,
) -> (Vec<&'a DescriptorMember>, Vec<DescriptorWithheldMember>) {
    let (offered, refused): (Vec<_>, Vec<_>) = verdicts.partition(|(_, verdict)| verdict.is_ok());
    (
        offered.into_iter().map(|(member, _)| member).collect(),
        refused
            .into_iter()
            .filter_map(|(member, verdict)| {
                verdict
                    .err()
                    .map(|reason| withheld_member(entry, member, &reason))
            })
            .collect(),
    )
}

fn withheld_member(
    entry: &DescriptorEntry,
    member: &DescriptorMember,
    reason: &OfferError,
) -> DescriptorWithheldMember {
    let member_key = member.key();
    DescriptorWithheldMember {
        name: entry.name.clone(),
        selector: descriptor_names::member_selector(&bare_name(entry), &member_key),
        member_key,
        reason_code: withheld_reason_code(reason),
        reason: reason.to_string(),
    }
}

/// The pinned and active catalog entries that offer a member to one device.
/// A runtime entry gives the member that the device resolves. A model entry
/// gives every member that a runtime member offered to the device can load.
pub async fn list_descriptor_catalog_for_device(
    State(state): State<AppState>,
    _user: ActiveUser,
    ApiPath(Parsed(client_id)): ApiPath<Parsed<ClientId>>,
) -> Result<Json<DescriptorCatalogShortList>, AppError> {
    tracing::Span::current().record("target_client_id", tracing::field::display(&client_id));
    let store = catalog(&state)?;
    let client = state
        .client_store
        .get_client(&client_id)
        .await
        .map_err(AppError::internal)?
        .ok_or_else(|| AppError::NotFound("client not found".into()))?;
    let tags = state
        .tag_store
        .get_client_tags(&client_id)
        .await
        .map_err(AppError::internal)?;
    let tags = FleetTags::new(tags.into_iter().map(|tag| (client_id.clone(), tag)));
    let config = &state.config.descriptor_catalog;
    let (entries, unreadable) = short_list_entries(store).await?;
    let (runtime_entries, model_entries): (Vec<_>, Vec<_>) = entries
        .into_iter()
        .partition(|entry| entry.descriptor_kind() == DescriptorKind::Runtime);
    let parameters = parameter_rows(&state, model_entries.iter()).await;
    let mut items = Vec::new();
    let mut withheld = Vec::new();
    let mut withheld_entries = 0usize;
    // Every runtime member offered to the device, not only the resolved
    // ones: a model member is offered if any of them can load it.
    let mut offered: Vec<Runtime> = Vec::new();
    for entry in &runtime_entries {
        let (accepted, refused) = split_verdicts(
            entry,
            entry.members.iter().map(|member| {
                let verdict = descriptor_offer::offer(config, member, &client, &tags);
                (member, verdict)
            }),
        );
        offered.extend(
            accepted
                .iter()
                .filter_map(|member| member.runtime().cloned()),
        );
        withheld.extend(refused);
        match descriptor_offer::resolve(entry, config, &client, &tags) {
            Resolution::Resolved(member) => items.push(DescriptorCatalogShortListItem {
                name: entry.name.clone(),
                release_time: entry.release_time,
                descriptor_kind: DescriptorKind::Runtime,
                members: vec![member_view(
                    &state,
                    &bare_name(entry),
                    member,
                    None,
                    &parameters,
                )?],
            }),
            Resolution::Withheld(_) => withheld_entries += 1,
        }
    }
    let runtime_items = items.len();
    for entry in &model_entries {
        let (loadable, refused) = split_verdicts(
            entry,
            entry.members.iter().filter_map(|member| {
                member
                    .model()
                    .map(|model| (member, descriptor_offer::offer_model(model, &offered)))
            }),
        );
        withheld.extend(refused);
        if loadable.is_empty() {
            withheld_entries += 1;
            continue;
        }
        let members = loadable
            .into_iter()
            .map(|member| member_view(&state, &bare_name(entry), member, None, &parameters))
            .collect::<Result<Vec<_>, _>>()?;
        items.push(DescriptorCatalogShortListItem {
            name: entry.name.clone(),
            release_time: entry.release_time,
            descriptor_kind: DescriptorKind::Model,
            members,
        });
    }
    let withheld = DescriptorWithheldReport {
        member_count: withheld.len(),
        entry_count: withheld_entries,
        members: withheld,
    };
    tracing::debug!(
        runtime_items,
        model_items = items.len() - runtime_items,
        offered_runtime_members = offered.len(),
        withheld_members = withheld.member_count,
        withheld_entries = withheld.entry_count,
        unreadable,
        "listed descriptor catalog for device"
    );
    Ok(Json(DescriptorCatalogShortList {
        page: ListPage::whole(items, unreadable),
        withheld,
    }))
}

/// Parse each `type` value as a runtime type or a model type. A type must
/// belong to `kind` when the caller gives one.
fn type_filter(
    kind: Option<DescriptorKind>,
    types: Option<Vec<String>>,
) -> Result<(HashSet<RuntimeType>, HashSet<ModelType>), AppError> {
    types.unwrap_or_default().into_iter().try_fold(
        (HashSet::new(), HashSet::new()),
        |(mut runtimes, mut models), name| {
            match (name.parse::<RuntimeType>(), name.parse::<ModelType>()) {
                (Ok(_), _) if kind == Some(DescriptorKind::Model) => {
                    return Err(AppError::BadRequest(format!(
                        "type: `{name}` is not a model type"
                    )));
                }
                (Ok(runtime_type), _) => {
                    runtimes.insert(runtime_type);
                }
                (Err(_), Ok(_)) if kind == Some(DescriptorKind::Runtime) => {
                    return Err(AppError::BadRequest(format!(
                        "type: `{name}` is not a runtime type"
                    )));
                }
                (Err(_), Ok(model_type)) => {
                    models.insert(model_type);
                }
                (Err(_), Err(_)) => {
                    return Err(AppError::BadRequest(format!(
                        "type: unknown descriptor type `{name}`"
                    )));
                }
            }
            Ok((runtimes, models))
        },
    )
}

/// List live entries in descending release time and ascending name order.
pub async fn list_descriptor_catalog(
    State(state): State<AppState>,
    user: ActiveUser,
    ApiQuery(query): ApiQuery<DescriptorCatalogQuery>,
) -> Result<Json<ListPage<DescriptorEntryListRow>>, AppError> {
    let store = catalog(&state)?;
    let limit = list_limit(query.limit, &state.config)?;
    let read = catalog_read_scope(&state, user.caller()).await?;
    let after = list_cursor(query.cursor, |cursor| cursor.parse::<CatalogEntryKey>())?;
    if let Some(cursor) = &after {
        let exists = read.can_read(&cursor.scope)
            && store
                .get_entry(&cursor.scope, &cursor.name)
                .await
                .map_err(AppError::internal)?
                .is_some();
        if !exists {
            return Err(AppError::BadRequest(
                "`cursor` is not a `next_cursor` this listing issued".into(),
            ));
        }
    }
    let (runtime_types, model_types) = type_filter(query.descriptor_kind, query.descriptor_type)?;
    let scopes = match query.scope.as_deref() {
        None => read
            .readable_scopes(state.org_store.as_ref())
            .await
            .map_err(AppError::internal)?,
        Some(word) if word.eq_ignore_ascii_case(QualifiedCatalogName::GLOBAL) => {
            BTreeSet::from([CatalogScope::Global])
        }
        Some(reference) => {
            let OrgPath(reference) = reference
                .parse()
                .map_err(|e| AppError::BadRequest(format!("scope: {e}")))?;
            let org = require_in_scope(&state, &roster_scope(&read), &reference).await?;
            BTreeSet::from([CatalogScope::Org(org.org_id)])
        }
    };
    let mut filter = DescriptorEntryFilter {
        scopes,
        kinds: query.kind.unwrap_or_default().into_iter().collect(),
        states: query.state.unwrap_or_default().into_iter().collect(),
        descriptor_kind: query.descriptor_kind,
        runtime_types,
        model_types,
        name_prefix: query.prefix,
    };
    if filter.states.contains(&DescriptorEntryState::Tombstoned) {
        return Err(AppError::BadRequest(
            "state: tombstoned entries cannot be listed".into(),
        ));
    }
    if filter.states.is_empty() {
        filter.states = BTreeSet::from([
            DescriptorEntryState::Pinned,
            DescriptorEntryState::Active,
            DescriptorEntryState::Hidden,
        ]);
    }
    let filter = &filter;
    let page = page_filtered(
        limit,
        after,
        "descriptor catalog entries",
        |entry: &DescriptorEntry| entry.state != DescriptorEntryState::Tombstoned,
        |after, limit| async move { store.list_entries(filter, after.as_ref(), limit).await },
    )
    .await?;
    let view = name_view(&state, store, read, &filter.scopes).await?;
    let include_members = query.members.unwrap_or(false);
    let parameters = if include_members {
        parameter_rows(&state, page.items.iter()).await
    } else {
        ParameterRows::new()
    };
    let items = page
        .items
        .into_iter()
        .filter_map(|entry| {
            let shown = view.display_name(&entry.scope, &entry.name)?;
            Some(row(
                &state,
                entry,
                shown,
                include_members,
                None,
                &parameters,
            ))
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Json(ListPage {
        items,
        next_cursor: page.next_cursor,
        unreadable: page.unreadable,
    }))
}

/// Look up names with their complete member lists.
pub async fn lookup_descriptor_names(
    State(state): State<AppState>,
    user: ActiveUser,
    ApiQuery(query): ApiQuery<DescriptorNameQuery>,
) -> Result<Json<DescriptorEntryLookup>, AppError> {
    let store = catalog(&state)?;
    let names = required_batch(query.names, "names")?;
    if query.offer_counts.unwrap_or(false) {
        user.rights.require(UserPermission::ReadClients)?;
    }
    let read = catalog_read_scope(&state, user.caller()).await?;
    let mut asked = HashSet::new();
    let mut found = HashSet::new();
    let mut entries = Vec::new();
    let mut unknown = Vec::new();
    for requested in names {
        let name = &requested.name;
        if !asked.insert(name.to_string().to_ascii_lowercase()) {
            continue;
        }
        match descriptor_names::resolve(store, state.org_store.as_ref(), &read, name)
            .await
            .map_err(AppError::internal)?
        {
            Some(entry) => {
                if found.insert((entry.scope.clone(), entry.name.key())) {
                    entries.push(entry);
                }
            }
            None => unknown.push(requested.text),
        }
    }
    let counts = if query.offer_counts.unwrap_or(false) && !entries.is_empty() {
        offered_device_counts(&state, &entries).await
    } else {
        None
    };
    let parameters = parameter_rows(&state, entries.iter()).await;
    let view = name_view(&state, store, read, entries.iter().map(|e| &e.scope)).await?;
    let entries = entries
        .into_iter()
        .filter_map(|entry| {
            let shown = view.display_name(&entry.scope, &entry.name)?;
            Some(row(
                &state,
                entry,
                shown,
                true,
                counts.as_ref(),
                &parameters,
            ))
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Json(DescriptorEntryLookup { entries, unknown }))
}

/// Map exact member digests to ranked live names.
pub async fn lookup_descriptor_digests(
    State(state): State<AppState>,
    user: ActiveUser,
    ApiQuery(query): ApiQuery<DescriptorDigestQuery>,
) -> Result<Json<DescriptorDigestNames>, AppError> {
    let store = catalog(&state)?;
    let digests = required_batch(query.digests, "digests")?;
    if !digests.iter().all(|digest| is_content_digest(digest)) {
        return Err(AppError::BadRequest(
            "digests must be 64 lowercase hex characters".into(),
        ));
    }
    let unique: Vec<String> = digests
        .into_iter()
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    let found = store
        .find_by_digests(&unique)
        .await
        .map_err(AppError::internal)?;
    let read = catalog_read_scope(&state, user.caller()).await?;
    let scopes = found.values().flatten().map(|hit| &hit.scope);
    let view = name_view(&state, store, read, scopes).await?;
    let names = found
        .into_iter()
        .filter_map(|(digest, hits)| {
            let ranked = view.ranked(hits);
            (!ranked.is_empty()).then(|| {
                (
                    digest,
                    ranked
                        .into_iter()
                        .map(|(hit, shown)| {
                            let member_key = hit.member_key;
                            let descriptor_kind = match &member_key {
                                DescriptorMemberKey::Runtime { .. } => DescriptorKind::Runtime,
                                DescriptorMemberKey::Model { .. } => DescriptorKind::Model,
                            };
                            DescriptorNameHit {
                                selector: descriptor_names::member_selector(&shown, &member_key),
                                member_key,
                                name: hit.name,
                                scope: hit.scope,
                                display_name: shown.to_string(),
                                descriptor_kind,
                                kind: hit.kind,
                                state: hit.state,
                            }
                        })
                        .collect(),
                )
            })
        })
        .collect::<BTreeMap<_, _>>();
    Ok(Json(DescriptorDigestNames { names }))
}

fn required_batch<T>(values: Option<Vec<T>>, field: &str) -> Result<Vec<T>, AppError> {
    let values = values
        .filter(|values| !values.is_empty())
        .ok_or_else(|| AppError::BadRequest(format!("{field} must not be empty")))?;
    if values.len() > BATCH_MAX {
        return Err(AppError::BadRequest(format!(
            "{field} must not exceed {BATCH_MAX} values"
        )));
    }
    Ok(values)
}
