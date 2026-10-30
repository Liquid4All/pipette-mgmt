//! HTTP handlers for an organization's private benchmark catalog: the four routes
//! under `/api/v1/orgs/{org_id}/benchmarks`, and the reach the organization
//! overview's count (`crate::api_v1::orgs::catalog::reads`) shares with them.

pub mod reads;

use axum::Json;
use axum::extract::{FromRequest, Request, State};
use axum::http::header::{ACCEPT, CONTENT_TYPE, IF_MATCH, IF_NONE_MATCH, VARY};
use axum::http::{HeaderMap, HeaderName, StatusCode};
use axum::response::{IntoResponse, Response};

use pipette_core::benchmark::{Benchmark, DefinitionObject};
use pipette_core::organization::Organization;
use pipette_core::user::User;
use pipette_pipeline::org::{OrgRef, RosterScope};
use pipette_pipeline::org_catalog;
use pipette_pipeline::org_catalog::OrgCatalogError;
use pipette_plan_types::{BenchmarkId, CatalogEntry, OrgBenchmarks, OrgId, UserId, UserPermission};

use crate::api_v1::orgs::{OrgPath, require_in_scope, roster_reach};
use crate::auth::{ActiveUser, Caller, HumanUser, user_org};
use crate::error::AppError;
use crate::extract::{ApiPath, DefinitionBody, Parsed};
use crate::state::AppState;

/// Whose catalogs `user` may read: every organization's on `read_any_orgs`,
/// else its own. Membership alone is the reach, so this never refuses.
pub async fn read_reach(state: &AppState, caller: Caller<'_>) -> Result<RosterScope, AppError> {
    let user = caller.user;
    let rights = caller.rights;
    if rights.holds(UserPermission::ReadAnyOrgs) {
        return Ok(RosterScope::Fleet);
    }
    own_org(state, user).await
}

/// Whose catalogs `user` may write: every organization's on
/// `manage_any_orgs`, else its own on `manage_orgs`, and nobody's without.
pub async fn write_reach(state: &AppState, caller: Caller<'_>) -> Result<RosterScope, AppError> {
    roster_reach(
        state,
        caller,
        UserPermission::ManageOrgs,
        UserPermission::ManageAnyOrgs,
    )
    .await
}

async fn own_org(state: &AppState, user: &User) -> Result<RosterScope, AppError> {
    Ok(user_org(state, &user.user_id)
        .await?
        .map_or(RosterScope::Unaffiliated, RosterScope::Org))
}

/// The organization `reference` names, once the caller is found to belong to
/// it, or to read every one. Never a `403`: membership alone is the reach.
pub async fn readable(
    state: &AppState,
    caller: Caller<'_>,
    reference: &OrgRef,
) -> Result<Organization, AppError> {
    require_in_scope(state, &read_reach(state, caller).await?, reference).await
}

/// An organization's catalog, once the caller is found to administer it. Every
/// write goes through one.
struct CatalogAdmin {
    org: Organization,
    user_id: UserId,
}

impl CatalogAdmin {
    /// The organization `reference` names, inside [`write_reach`]. The route's
    /// gate refuses a caller without the grant before the lookup.
    async fn authorize(
        state: &AppState,
        caller: Caller<'_>,
        reference: &OrgRef,
    ) -> Result<Self, AppError> {
        let user = caller.user;
        let org = require_in_scope(state, &write_reach(state, caller).await?, reference).await?;
        Ok(Self {
            org,
            user_id: user.user_id.clone(),
        })
    }

    /// Whether the organization keeps `benchmark_id`, judged by key, so an
    /// entry that does not parse still counts.
    async fn keeps(&self, state: &AppState, benchmark_id: &BenchmarkId) -> Result<bool, AppError> {
        let kept = state
            .catalog_store
            .list_benchmark_ids_of(&self.org.org_id)
            .await
            .map_err(|e| {
                AppError::Internal(format!(
                    "failed to list the organization's benchmarks: {e:#}"
                ))
            })?;
        Ok(kept.contains(benchmark_id))
    }

    /// Create or replace `benchmark`, as the store will serve it. The public-id
    /// check reads the cached catalog, so may miss a fresh public entry, which
    /// still wins at serve time.
    async fn put(&self, state: &AppState, benchmark: Benchmark) -> Result<Benchmark, AppError> {
        let public = state.catalog_cache.get().await.map_err(|e| {
            AppError::Internal(format!(
                "failed to load the public catalog to check the id: {e:#}"
            ))
        })?;
        let org_id = &self.org.org_id;
        let written = org_catalog::put(&*state.catalog_store, &public, org_id, benchmark).await?;
        tracing::info!(
            %org_id, benchmark_id = %written.benchmark_id(), user_id = %self.user_id,
            "organization benchmark written"
        );
        Ok(written)
    }

    /// Refuse with `412` unless the entry stands as `condition` needs it:
    /// absent to create, kept (by key) to replace. Checked before the write,
    /// not with it, so two writes racing on one id can both pass.
    async fn require(
        &self,
        state: &AppState,
        benchmark_id: &BenchmarkId,
        condition: Precondition,
    ) -> Result<(), AppError> {
        let kept = self.keeps(state, benchmark_id).await?;
        match condition {
            Precondition::Absent if kept => Err(AppError::PreconditionFailed(format!(
                "{} already keeps a benchmark {benchmark_id}",
                self.org.slug
            ))),
            Precondition::Kept if !kept => Err(AppError::PreconditionFailed(
                OrgCatalogError::NotFound {
                    org_id: self.org.org_id.clone(),
                    benchmark_id: benchmark_id.clone(),
                }
                .to_string(),
            )),
            Precondition::Absent | Precondition::Kept => Ok(()),
        }
    }

    /// Delete the entry `benchmark_id` names, found by its key.
    async fn delete(&self, state: &AppState, benchmark_id: &BenchmarkId) -> Result<(), AppError> {
        let org_id = &self.org.org_id;
        org_catalog::delete(&*state.catalog_store, org_id, benchmark_id).await?;
        tracing::info!(
            %org_id, %benchmark_id, user_id = %self.user_id,
            "organization benchmark deleted"
        );
        Ok(())
    }
}

/// What a `PUT` asks of the entry under its id: `If-None-Match: *` creates
/// only, `If-Match: *` replaces only.
#[derive(Clone, Copy, Debug)]
pub enum Precondition {
    Absent,
    Kept,
}

impl Precondition {
    /// The one `headers` name, if any. This route serves no `ETag`, so a value
    /// other than `*` could match nothing and is refused.
    pub fn from_headers(headers: &HeaderMap) -> Result<Option<Self>, AppError> {
        let star = |name: HeaderName| match headers.get(&name) {
            None => Ok(false),
            Some(value) if value.as_bytes().trim_ascii() == b"*" => Ok(true),
            Some(_) => Err(AppError::BadRequest(format!(
                "{name}: only `*` is supported"
            ))),
        };
        match (star(IF_NONE_MATCH)?, star(IF_MATCH)?) {
            (true, true) => Err(AppError::BadRequest(
                "send if-none-match or if-match, not both".into(),
            )),
            (true, false) => Ok(Some(Self::Absent)),
            (false, true) => Ok(Some(Self::Kept)),
            (false, false) => Ok(None),
        }
    }
}

/// Whether `headers` ask for the catalog file: `Accept` ranks
/// `application/toml` above `application/json`. No header, a wildcard, or a
/// tie keeps JSON, which every existing client reads.
fn wants_toml(headers: &HeaderMap) -> bool {
    let ranges: Vec<(&str, f32)> = headers
        .get_all(ACCEPT)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .map(|range| {
            let (media, params) = range.split_once(';').unwrap_or((range, ""));
            let q = params
                .split(';')
                .filter_map(|param| param.split_once('='))
                .find(|(name, _)| name.trim().eq_ignore_ascii_case("q"))
                .and_then(|(_, q)| q.trim().parse::<f32>().ok())
                .unwrap_or(1.0);
            (media.trim(), q)
        })
        .collect();
    // A type takes the q of the most specific range naming it (RFC 9110 §12.5.1).
    let quality = |media: &str| {
        [media, "application/*", "*/*"]
            .into_iter()
            .find_map(|wanted| {
                ranges
                    .iter()
                    .filter(|(range, _)| range.eq_ignore_ascii_case(wanted))
                    .map(|&(_, q)| q)
                    .reduce(f32::max)
            })
            .unwrap_or(0.0)
    };
    quality("application/toml") > quality("application/json")
}

/// The entry `body` defines under `benchmark_id`, held to the checks a
/// catalog file is at load, so an entry that could not be served cannot be
/// written.
pub fn definition(
    benchmark_id: BenchmarkId,
    body: DefinitionBody<DefinitionObject>,
) -> Result<Benchmark, AppError> {
    match body {
        DefinitionBody::Json(body) => Benchmark::from_json(benchmark_id, body),
        DefinitionBody::Toml(text) => Benchmark::from_toml(benchmark_id.as_str(), &text),
    }
    .map_err(|e| AppError::BadRequest(format!("{e:#}")))
}

// GET /api/v1/orgs/{org_id}/benchmarks
pub async fn list_org_benchmarks(
    State(state): State<AppState>,
    active: ActiveUser,
    ApiPath(Parsed(OrgPath(reference))): ApiPath<Parsed<OrgPath>>,
) -> Result<impl IntoResponse, AppError> {
    let caller = active.caller();
    let org = readable(&state, caller, &reference).await?;
    let listed = org_catalog::list(&*state.catalog_store, &org.org_id)
        .await
        .map_err(|e| {
            AppError::Internal(format!(
                "failed to list the organization's benchmarks: {e:#}"
            ))
        })?;
    let (mut benchmarks, mut unreadable_benchmark_ids) = (Vec::new(), Vec::new());
    listed.into_iter().for_each(|kept| match kept.entry {
        Ok(benchmark) => benchmarks.push(CatalogEntry::from(benchmark)),
        Err(_) => unreadable_benchmark_ids.push(kept.benchmark_id),
    });
    Ok(Json(OrgBenchmarks {
        org_id: org.org_id,
        benchmarks,
        unreadable_benchmark_ids,
    }))
}

// GET /api/v1/orgs/{org_id}/benchmarks/{benchmark_id}
//
// JSON, or the catalog file for an `Accept` that asks for it; errors stay JSON.
pub async fn get_org_benchmark(
    State(state): State<AppState>,
    active: ActiveUser,
    ApiPath((Parsed(OrgPath(reference)), Parsed(benchmark_id))): ApiPath<(
        Parsed<OrgPath>,
        Parsed<BenchmarkId>,
    )>,
    headers: HeaderMap,
) -> Result<Response, AppError> {
    let caller = active.caller();
    let org = readable(&state, caller, &reference).await?;
    let benchmark = org_catalog::require(&*state.catalog_store, &org.org_id, &benchmark_id).await?;
    if !wants_toml(&headers) {
        return Ok(([(VARY, "accept")], Json(CatalogEntry::from(benchmark))).into_response());
    }
    let toml = benchmark
        .to_toml()
        .map_err(|e| AppError::internal(e.context("failed to render the definition")))?;
    Ok(([(CONTENT_TYPE, "application/toml"), (VARY, "accept")], toml).into_response())
}

// PUT /api/v1/orgs/{org_id}/benchmarks/{benchmark_id}
//
// The body is read only after the grant check, so a refused caller gets the
// refusal rather than a verdict on its body.
pub async fn put_org_benchmark(
    State(state): State<AppState>,
    active: HumanUser,
    ApiPath((Parsed(org_id), Parsed(benchmark_id))): ApiPath<(Parsed<OrgId>, Parsed<BenchmarkId>)>,
    request: Request,
) -> Result<impl IntoResponse, AppError> {
    let reference = OrgRef::Id(org_id);
    let caller = active.caller();
    let admin = CatalogAdmin::authorize(&state, caller, &reference).await?;
    if let Some(condition) = Precondition::from_headers(request.headers())? {
        admin.require(&state, &benchmark_id, condition).await?;
    }
    let body = DefinitionBody::<DefinitionObject>::from_request(request, &state).await?;
    let benchmark = definition(benchmark_id, body)?;
    let stored = admin.put(&state, benchmark).await?;
    Ok(Json(CatalogEntry::from(stored)))
}

// DELETE /api/v1/orgs/{org_id}/benchmarks/{benchmark_id}
pub async fn delete_org_benchmark(
    State(state): State<AppState>,
    active: HumanUser,
    ApiPath((Parsed(org_id), Parsed(benchmark_id))): ApiPath<(Parsed<OrgId>, Parsed<BenchmarkId>)>,
) -> Result<impl IntoResponse, AppError> {
    let reference = OrgRef::Id(org_id);
    let caller = active.caller();
    let admin = CatalogAdmin::authorize(&state, caller, &reference).await?;
    admin.delete(&state, &benchmark_id).await?;
    Ok(StatusCode::NO_CONTENT)
}
