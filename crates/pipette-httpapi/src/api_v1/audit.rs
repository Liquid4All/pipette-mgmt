//! The audit log's reads, newest first: an organization's under `manage_orgs`,
//! and the whole log, host commands and unattributed refusals included, under
//! `manage_any_orgs`. Kept on a `[database]` alone; without one both answer
//! `404`.

use std::collections::{BTreeMap, HashSet};

use axum::Json;
use axum::extract::State;
use axum::response::IntoResponse;
use futures::stream::{self, StreamExt};

use pipette_pipeline::audit::{NEEDS_DATABASE, audit_filter};
use pipette_pipeline::org::RosterScope;
use pipette_plan_types::{
    AuditAction, AuditEvent, AuditEventId, AuditQuery, AuditTarget, ClientId, FleetDetail,
    ListPage, Named, OrgId, PrincipalOwner, UserId, UserPermission,
};
use pipette_store::{AuditFilter, STORAGE_CONCURRENCY};

use crate::api_v1::orgs::{OrgPath, require_in_scope, roster_reach};
use crate::auth::{Caller, HumanUser};
use crate::campaign_access::CampaignReach;
use crate::error::AppError;
use crate::extract::{ApiPath, ApiQuery, Parsed};
use crate::list::{list_cursor, list_limit, page_filtered};
use crate::names::{campaign_names, device_names, owner_labels, submitter_addresses, vault_names};
use crate::state::AppState;
use crate::vault_access::VaultReach;

/// The device protocol's write routes, which the route table does not
/// describe, in words.
const DEVICE_WRITES: &[(&str, &str, &str)] = &[
    ("POST", "/clients/register", "Register a device"),
    ("PATCH", "/clients/me", "Update the device's own record"),
    ("POST", "/benchmarks", "Submit a benchmark result"),
    (
        "POST",
        "/benchmarks/batch",
        "Submit benchmark results in a batch",
    ),
    ("POST", "/plans/claim", "Claim the next job"),
    ("POST", "/plans/{job_id}/reclaim", "Take back a job's lease"),
];

/// Route patterns renamed since events were stored under them, with the
/// current pattern each now reads as.
const FORMER_ROUTES: &[(&str, &str)] = &[(
    "/api/v1/model-params/{key}",
    "/api/v1/model-params/{model_key}",
)];

type AuditAnswer = Named<ListPage<AuditEvent>>;

// GET /api/v1/orgs/{org_id}/audit
pub async fn list_org_audit(
    State(state): State<AppState>,
    active: HumanUser,
    ApiPath(Parsed(OrgPath(reference))): ApiPath<Parsed<OrgPath>>,
    ApiQuery(query): ApiQuery<AuditQuery>,
) -> Result<impl IntoResponse, AppError> {
    let caller = active.caller();
    let scope = roster_reach(
        &state,
        caller,
        UserPermission::ManageOrgs,
        UserPermission::ManageAnyOrgs,
    )
    .await?;
    let org = require_in_scope(&state, &scope, &reference).await?;
    Ok(Json(
        answer(&state, caller, &scope, Some(org.org_id), query).await?,
    ))
}

// GET /api/v1/audit
pub async fn list_audit(
    State(state): State<AppState>,
    admin: HumanUser,
    ApiQuery(query): ApiQuery<AuditQuery>,
) -> Result<impl IntoResponse, AppError> {
    let caller = admin.caller();
    Ok(Json(
        answer(&state, caller, &RosterScope::Fleet, None, query).await?,
    ))
}

async fn answer(
    state: &AppState,
    caller: Caller<'_>,
    scope: &RosterScope,
    org_id: Option<OrgId>,
    query: AuditQuery,
) -> Result<AuditAnswer, AppError> {
    let store = state
        .audit_store
        .as_deref()
        .ok_or_else(|| AppError::NotFound(NEEDS_DATABASE.into()))?;
    let filter = audit_filter(
        org_id.clone(),
        query.outcome.as_deref(),
        query.actor.as_deref(),
        query.target.as_deref(),
    )?;
    // Another organization's events would answer what its redaction hides.
    if let (
        RosterScope::Org(_) | RosterScope::Unaffiliated,
        Some(AuditTarget::Org { org_id: other }),
    ) = (scope, &filter.record)
        && org_id.as_ref() != Some(other)
    {
        return Err(AppError::BadRequest(
            "target: an organization's log names no other organization".into(),
        ));
    }
    let limit = list_limit(query.limit, &state.config)?;
    let before = list_cursor(query.cursor, AuditEventId::try_new)?;
    let filter: &AuditFilter = &filter;
    let page = page_filtered(
        limit,
        before,
        "audit events",
        |_| true,
        |before, limit| async move { store.list_events(filter, before.as_ref(), limit).await },
    )
    .await?;
    let mut items = page.items;
    if let (RosterScope::Org(_) | RosterScope::Unaffiliated, Some(org_id)) = (scope, &org_id) {
        items
            .iter_mut()
            .for_each(|event| keep_only_org(event, org_id));
    }
    items
        .iter_mut()
        .for_each(|event| event.summary = summary(state, &event.action));
    let names = if query.names.unwrap_or(false) {
        Some(names(state, caller, scope, &items).await?)
    } else {
        None
    };
    Ok(Named {
        body: ListPage {
            items,
            next_cursor: page.next_cursor,
            unreadable: page.unreadable,
        },
        names,
    })
}

/// `event` naming no organization but `org_id`: another's id or handle would
/// answer what a `404` hides, that a handle probed from this one exists. The
/// path and a command's arguments can carry one, so the route and the command
/// stand in.
fn keep_only_org(event: &mut AuditEvent, org_id: &OrgId) {
    event.org_ids.retain(|listed| listed == org_id);
    event
        .targets
        .retain(|target| !matches!(target, AuditTarget::Org { org_id: other } if other != org_id));
    match &mut event.action {
        AuditAction::Http { route, path, .. } => path.clone_from(route),
        AuditAction::Cli { args, .. } => args.clear(),
        // The keys are the account's own, which no other organization's id names.
        AuditAction::FirstClerkSignIn { .. } => {}
    }
}

/// What an event's route does, in words: the route table's summary, or the
/// device protocol's.
fn summary(state: &AppState, action: &AuditAction) -> Option<String> {
    let AuditAction::Http { method, route, .. } = action else {
        return None;
    };
    let route = FORMER_ROUTES
        .iter()
        .find(|(former, _)| former == route)
        .map_or(route.as_str(), |(_, current)| *current);
    let described = state.openapi.summary(method, route).or_else(|| {
        DEVICE_WRITES
            .iter()
            .find(|(m, r, _)| m == method && *r == route)
            .map(|(_, _, label)| *label)
    });
    described.map(str::to_owned)
}

/// The names of the ids on the page that the reader may see by other routes:
///
/// - an acting account, by its address, as a plan names its submitter;
/// - a target account, only inside the roster of the reader;
/// - a device, with `read_clients`;
/// - a campaign or a vault resource, within the reach of the reader.
///
/// A claimed id is never named, because the caller chose it.
async fn names(
    state: &AppState,
    caller: Caller<'_>,
    scope: &RosterScope,
    events: &[AuditEvent],
) -> Result<BTreeMap<String, String>, AppError> {
    let (mut users, mut target_users, mut clients) =
        (HashSet::new(), HashSet::new(), HashSet::new());
    let (mut orgs, mut campaigns, mut resources) = (HashSet::new(), HashSet::new(), HashSet::new());
    for event in events {
        match event.actor.record() {
            Some(AuditTarget::User { user_id }) => {
                users.insert(user_id);
            }
            Some(AuditTarget::Client { client_id }) => {
                clients.insert(client_id);
            }
            _ => {}
        }
        orgs.extend(event.org_ids.iter().cloned());
        for target in &event.targets {
            match target {
                AuditTarget::Org { org_id } => {
                    orgs.insert(org_id.clone());
                }
                AuditTarget::User { user_id } => {
                    target_users.insert(user_id.clone());
                }
                AuditTarget::Client { client_id } => {
                    clients.insert(client_id.clone());
                }
                AuditTarget::Campaign { campaign_id } => {
                    campaigns.insert(campaign_id.clone());
                }
                AuditTarget::VaultResource { resource_id } => {
                    resources.insert(resource_id.clone());
                }
                AuditTarget::Plan { .. } | AuditTarget::Job { .. } => {}
            }
        }
    }
    users.extend(in_roster(state, scope, target_users).await);
    let owners: Vec<PrincipalOwner> = orgs
        .into_iter()
        .map(|org_id| PrincipalOwner::Org { org_id })
        .collect();
    let campaign_reach = CampaignReach::over_records(state, caller).await?;
    let vault_reach = VaultReach::resolve(state, caller).await?;
    let fleet = caller.rights.fleet_detail() == FleetDetail::Full;
    let clients: Vec<ClientId> = if fleet {
        clients.into_iter().collect()
    } else {
        Vec::new()
    };
    let (addresses, handles, devices, campaigns, resources) = futures::join!(
        submitter_addresses(state, users.into_iter()),
        owner_labels(state, owners.iter()),
        device_names(state, clients.into_iter()),
        campaign_names(state, &campaign_reach, campaigns),
        vault_names(state, &vault_reach, resources),
    );
    Ok([addresses, handles, devices, campaigns, resources]
        .into_iter()
        .flatten()
        .collect())
}

/// The accounts of `user_ids` inside `scope`'s roster, the rule `users::in_scope`
/// reads an account by. A membership that fails to read drops the account.
async fn in_roster(
    state: &AppState,
    scope: &RosterScope,
    user_ids: HashSet<UserId>,
) -> Vec<UserId> {
    if matches!(scope, RosterScope::Fleet) {
        return user_ids.into_iter().collect();
    }
    stream::iter(user_ids)
        .map(|user_id| async move {
            let membership = state.org_store.get_membership(&user_id).await;
            membership
                .inspect_err(|e| tracing::warn!(error = %format!("{e:#}"), "a membership did not read; the account stays unnamed"))
                .ok()
                .filter(|org_id| scope.covers(org_id.as_ref()))
                .map(|_| user_id)
        })
        .buffer_unordered(STORAGE_CONCURRENCY)
        .filter_map(std::future::ready)
        .collect()
        .await
}

#[cfg(test)]
mod tests {
    use pipette_plan_types::{AuditActor, AuditOutcome, OrgId};
    use pipette_web_testkit::registered_routes;

    use super::*;

    /// A command's arguments can name another organization, so an
    /// organization's reader gets the command alone.
    #[test]
    fn an_org_reader_gets_no_command_arguments() -> anyhow::Result<()> {
        let (acme, rival) = (OrgId::try_new("org_acme")?, OrgId::try_new("org_rival")?);
        let mut event = pipette_core::audit::new_event(
            AuditActor::Host {
                os_user: "ops".into(),
                hostname: "mgmt".into(),
            },
            AuditAction::Cli {
                command: "users set-org".into(),
                args: vec!["users".into(), "set-org".into(), "rival".into()],
            },
            AuditOutcome::Succeeded,
        );
        event.org_ids = [acme.clone(), rival.clone()].into();
        event.targets = [AuditTarget::Org { org_id: rival }].into();
        keep_only_org(&mut event, &acme);
        assert_eq!(event.org_ids, [acme].into());
        assert!(event.targets.is_empty());
        assert!(matches!(&event.action, AuditAction::Cli { args, .. } if args.is_empty()));
        Ok(())
    }

    /// A stored event under a renamed route still reads a summary.
    #[test]
    fn every_former_route_reads_as_a_described_one() -> anyhow::Result<()> {
        let openapi = crate::api_schema::openapi::OpenApi::build()?;
        let paths = openapi.document().get("paths");
        let missing: Vec<&str> = FORMER_ROUTES
            .iter()
            .filter(|(_, current)| paths.and_then(|paths| paths.get(*current)).is_none())
            .map(|(former, _)| *former)
            .collect();
        anyhow::ensure!(missing.is_empty(), "undescribed: {missing:?}");
        Ok(())
    }

    /// Every audited device write the router serves has words for an audit row.
    #[test]
    fn every_device_write_is_described() -> anyhow::Result<()> {
        let missing: Vec<String> = registered_routes(include_str!("../router.rs"))?
            .into_iter()
            .filter(|route| !matches!(route.method.as_str(), "GET" | "HEAD"))
            .filter(|route| !route.path.starts_with("/api/"))
            .filter(|route| !crate::audit::UNAUDITED.contains(&route.path.as_str()))
            .filter(|route| {
                !DEVICE_WRITES
                    .iter()
                    .any(|(method, path, _)| *method == route.method && *path == route.path)
            })
            .map(|route| format!("{} {}", route.method, route.path))
            .collect();
        anyhow::ensure!(missing.is_empty(), "undescribed: {missing:?}");
        Ok(())
    }
}
