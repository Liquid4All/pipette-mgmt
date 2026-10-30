//! The organization reads behind the overview, through [`crate::api_v1::orgs`]
//! as the JSON routes read them.

use std::collections::{BTreeMap, HashSet};

use axum::Json;
use axum::extract::State;
use axum::response::IntoResponse;
use strum::IntoEnumIterator;

use pipette_core::client::{Client, ClientStatus};
use pipette_pipeline::org::OrgRef;
use pipette_plan_types::{
    ClientId, ClientPreauthKeyView, Highlights, OrgId, OrgOverview, UserPermission, UserStatus,
};

use crate::api_v1::orgs::{OrgPath, roster_reach};
use crate::auth::{ActiveUser, Caller};
use crate::error::AppError;
use crate::extract::{ApiPath, Parsed};
use crate::state::AppState;

/// One organization's overview: its counts, what waits on a decision, and its
/// newest campaigns. The device highlights take `read_clients`.
async fn overview(
    state: &AppState,
    caller: Caller<'_>,
    reference: &OrgRef,
) -> Result<OrgOverview, AppError> {
    let org = crate::api_v1::orgs::record(state, caller, reference).await?;
    let counts = crate::api_v1::orgs::counts_of(state, &org).await?;
    let (_, members) = crate::api_v1::orgs::members(state, caller, reference).await?;
    let rights = caller.rights;
    let org_id = &org.org_id;
    let reads_devices = rights.holds(UserPermission::ReadClients);
    let (client_status_counts, pending_clients, suspended_clients) = if reads_devices {
        let (by_status, pending, suspended) = device_highlights(state, caller, reference).await?;
        (Some(by_status), Some(pending), Some(suspended))
    } else {
        (None, None, None)
    };

    // Each mirrors its listing route's filter, so a count matches the leaf it links to.
    let campaigns = unless_refused(
        crate::api_v1::campaigns::reads::org_campaigns(state, caller, org_id, HIGHLIGHTS).await,
    )?;
    let vault_resource_count =
        Some(crate::api_v1::vault::reads::count_org_resources(state, caller, org_id).await?);
    let benchmark_count = Some(
        crate::api_v1::orgs::catalog::reads::count_org_benchmarks(state, caller, org_id).await?,
    );
    let client_preauth_key_count = client_preauth_keys(state, caller, org_id)
        .await?
        .map(|keys| keys.iter().filter(|key| !key.expired).count());

    // Advisory, not enforced: the writes still decide.
    let may_administer = roster_reach(
        state,
        caller,
        UserPermission::ManageOrgs,
        UserPermission::ManageAnyOrgs,
    )
    .await?
    .covers(Some(org_id));
    let member_status_counts = [
        UserStatus::Active,
        UserStatus::Pending,
        UserStatus::Disabled,
    ]
    .into_iter()
    .map(|status| {
        let count = members.iter().filter(|m| m.status == status).count();
        (status, count)
    })
    .filter(|(_, count)| *count > 0)
    .collect();
    let pending_members = Highlights::of(
        members
            .into_iter()
            .filter(|member| member.status == UserStatus::Pending)
            .map(Into::into),
        HIGHLIGHTS,
    );
    Ok(OrgOverview {
        org: org.into(),
        member_count: counts.members,
        client_count: counts.clients,
        member_status_counts,
        pending_members,
        client_status_counts,
        pending_clients,
        suspended_clients,
        campaigns,
        vault_resource_count,
        benchmark_count,
        client_preauth_key_count,
        may_administer,
        may_delete: rights.holds(UserPermission::ManageAnyOrgs),
    })
}

/// How many rows of a kind an overview draws before it links to the listing.
const HIGHLIGHTS: usize = 5;

// GET /api/v1/orgs/{org_id}/overview
pub async fn get_org_overview(
    State(state): State<AppState>,
    active: ActiveUser,
    ApiPath(Parsed(OrgPath(reference))): ApiPath<Parsed<OrgPath>>,
) -> Result<impl IntoResponse, AppError> {
    let caller = active.caller();
    Ok(Json(overview(&state, caller, &reference).await?))
}

/// The device pre-auth keys naming the organization, for a reader who may mint
/// one; `None` for any other.
pub async fn client_preauth_keys(
    state: &AppState,
    caller: Caller<'_>,
    org_id: &OrgId,
) -> Result<Option<Vec<ClientPreauthKeyView>>, AppError> {
    if !crate::api_v1::orgs::client_preauth::may_mint_for(state, caller, org_id).await? {
        return Ok(None);
    }
    Ok(Some(
        crate::api_v1::orgs::client_preauth::keys(state, org_id).await?,
    ))
}

/// A listing's answer, or `None` where it refuses the reader: the overview
/// then leaves that highlight out, and the leaf's own page says why.
fn unless_refused<T>(listed: Result<T, AppError>) -> Result<Option<T>, AppError> {
    match listed {
        Ok(listed) => Ok(Some(listed)),
        Err(AppError::Forbidden(_)) => Ok(None),
        Err(e) => Err(e),
    }
}

/// The organization's device statuses by count, then its pending and its
/// suspended devices.
type DeviceHighlights = (
    BTreeMap<pipette_plan_types::ClientStatus, usize>,
    Highlights<pipette_plan_types::Client>,
    Highlights<pipette_plan_types::Client>,
);

async fn device_highlights(
    state: &AppState,
    caller: Caller<'_>,
    reference: &OrgRef,
) -> Result<DeviceHighlights, AppError> {
    let (org, owned) = crate::api_v1::orgs::devices(state, caller, reference).await?;
    let administered = crate::api_v1::orgs::administers_org(state, caller, &org.org_id).await?;
    let view = |client: &Client| client.clone().view(administered);
    let suspended: HashSet<ClientId> = state
        .todo_store
        .list_suspended_ids()
        .await
        .map_err(|e| AppError::internal(e.context("failed to list the suspensions")))?
        .into_iter()
        .collect();
    let status_counts = ClientStatus::iter()
        .map(|status| {
            let count = owned.iter().filter(|(c, _)| c.status == status).count();
            (status.into(), count)
        })
        .filter(|(_, count)| *count > 0)
        .collect();
    let clients = || owned.iter().map(|(client, _)| client);
    Ok((
        status_counts,
        Highlights::of(
            clients()
                .filter(|client| client.status == ClientStatus::Pending)
                .map(view),
            HIGHLIGHTS,
        ),
        Highlights::of(
            clients()
                .filter(|client| suspended.contains(&client.client_id))
                .map(view),
            HIGHLIGHTS,
        ),
    ))
}
