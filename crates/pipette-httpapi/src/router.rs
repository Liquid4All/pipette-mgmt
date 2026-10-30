use axum::body::Body;
use axum::extract::DefaultBodyLimit;
use axum::http::{Method, Request, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post, put};
use axum::{Json, Router};
use tracing::Instrument;

use pipette_plan_types::ErrorBody;
use pipette_plan_types::UserPermission::{
    CancelPlans, EditAnyCatalog, EditCatalog, ManageAnyOrgs, ManageClients, ManageOrgs,
    ReadAnyPlans, ReadClients, ReadOrgs, ReadPlans, SubmitPlans,
};

use crate::error::AppError;
use crate::gate::Gate::{self, Active, AllOf, AnyOf, Needs};
use crate::gate::gate;
use crate::state::AppState;
use crate::{api_schema, api_v1, client, health, ui};

pub(crate) const READ_CAMPAIGN: Gate = AnyOf(
    &[ReadPlans, ManageOrgs],
    "reading campaigns needs read_plans, or manage_orgs for your organization's",
);
/// The routes that file into a campaign take [`FILE_PLANS`] instead.
pub(crate) const ADMINISTER_CAMPAIGN: Gate = AnyOf(
    &[SubmitPlans, ManageOrgs],
    "campaigns need submit_plans to own one, or manage_orgs to administer your organization's",
);
/// An organization's devices are its roster and the fleet at once.
pub(crate) const READ_ORG_DEVICES: Gate = AllOf(&[ReadOrgs, ReadClients]);
/// The fleet reads. `GET /api/v1/users/me` answers these three gates in
/// `may`, so they are named once.
pub(crate) const READ_FLEET: Gate = Needs(ReadClients);
pub(crate) const FILE_PLANS: Gate = Needs(SubmitPlans);
pub(crate) const CREATE_ORGS: Gate = Needs(ManageAnyOrgs);
pub(crate) const EDIT_ORG_CATALOG: Gate = AnyOf(
    &[EditCatalog, EditAnyCatalog],
    "changing an organization's descriptor catalog needs edit_catalog or edit_any_catalog",
);

/// Gzip for a caller that asks: a plan's jobs run to megabytes of JSON. The
/// encoder flushes on each wait, so a progress line still arrives at once.
pub(crate) fn compression() -> tower_http::compression::CompressionLayer {
    tower_http::compression::CompressionLayer::new()
}

/// The served route table, built by `serve` and the integration tests alike.
pub fn app(state: AppState) -> Router {
    // Read before `.with_state(state)` moves the state into the router.
    let default_body_limit = state.config.default_body_limit_bytes.get();
    let plan_body_limit = state.config.plan_body_limit_bytes.get();
    let submission_body_limit = state.config.submission_body_limit_bytes.get();
    let router = Router::new().route("/health", get(health::health));
    let router = client_protocol(app_pages(router), submission_body_limit);
    operator_api(router, plan_body_limit)
        // A route layer, so only a matched route is audited, under its pattern.
        .route_layer(axum::middleware::from_fn_with_state(
            state.clone(),
            crate::audit::record,
        ))
        // After every route, because the 405 reaches only the routes added
        // before it, and before the layers, so the request span covers both.
        .fallback(no_route)
        .method_not_allowed_fallback(method_not_allowed)
        .layer(DefaultBodyLimit::max(default_body_limit))
        .layer(compression())
        // One span per request; handlers record the fields below onto it. The
        // failure is logged by `AppError::into_response`, not here. `INFO`: a
        // span below the filter's level discards every field recorded on it.
        .layer(axum::middleware::from_fn(
            |req: Request<Body>, next: Next| {
                let span = tracing::info_span!(
                    "request",
                    method = %req.method(),
                    path = %req.uri().path(),
                    // `client_id` is the caller. A route acting on a device names
                    // it in `target_client_id`, as JSON keeps one value per field.
                    client_id = tracing::field::Empty,
                    target_client_id = tracing::field::Empty,
                    job_id = tracing::field::Empty,
                    plan_id = tracing::field::Empty,
                    campaign_id = tracing::field::Empty,
                    benchmark_id = tracing::field::Empty,
                    resource_id = tracing::field::Empty,
                );
                next.run(req).instrument(span)
            },
        ))
        .with_state(state)
}

/// `/`, which lands on the app, the app's static files (`crate::ui`), and the
/// API reference page.
fn app_pages(router: Router<AppState>) -> Router<AppState> {
    router
        .route("/docs", get(api_schema::openapi::site_root))
        .route("/docs/", get(api_schema::openapi::site_page))
        .route("/docs/style.css", get(api_schema::openapi::site_style))
        .route("/", get(ui::root))
        .route("/app", get(ui::root))
        .route("/app/", get(ui::index))
        .route("/app/assets/{file}", get(ui::asset))
        // Every other path is a page the app routes itself; the static
        // `assets` segment wins over the catch-all.
        .route("/app/{*page}", get(ui::index))
}

/// What a device calls: its own record, the catalog, the queue, results and the
/// artifacts a plan names. Everything an operator calls is in [`operator_api`].
fn client_protocol(router: Router<AppState>, submission_body_limit: usize) -> Router<AppState> {
    router
        .route("/clients/register", post(client::clients::register_client))
        .route(
            "/clients/me",
            get(client::clients::get_own_client).patch(client::clients::update_own_client),
        )
        .route(
            "/benchmarks",
            get(client::benchmarks::list_benchmarks)
                .post(client::benchmarks::submissions::submit_benchmark)
                .layer(DefaultBodyLimit::max(submission_body_limit)),
        )
        .route(
            "/benchmarks/batch",
            post(client::benchmarks::submissions::submit_benchmark_batch)
                .layer(DefaultBodyLimit::max(submission_body_limit)),
        )
        .route(
            "/benchmarks/{benchmark_id}",
            get(client::benchmarks::get_benchmark),
        )
        // The same catalog, each entry reduced to the `benchmark_type` +
        // `benchmark_params` pair a submission declares.
        .route(
            "/api/v2/benchmarks",
            get(client::benchmarks::list_benchmarks_v2),
        )
        .route(
            "/api/v2/benchmarks/{benchmark_id}",
            get(client::benchmarks::get_benchmark_v2),
        )
        // A benchmark's corpus, served from the cache while its upstream probe
        // backs it (`docs/design/vl-spec-decode-dataset-delivery.md`).
        .route(
            "/datasets/{benchmark_id}",
            get(client::datasets::get_dataset),
        )
        .route("/jobs/{job_id}", get(client::jobs::get_job))
        .route(
            "/jobs/{job_id}/eval-sample-results",
            get(client::jobs::get_eval_sample_results),
        )
        // The static `claim` wins over `{job_id}` at the same position.
        .route("/plans/claim", post(client::plans::claim))
        .route("/plans/{job_id}/heartbeat", put(client::plans::heartbeat))
        .route("/plans/{job_id}/reclaim", post(client::plans::reclaim))
        // A vault record and its bytes, at the address a plan's artifact URL
        // names, for a device or an account ([`crate::vault_access`]).
        .route(
            "/vault/resources/{resource_id}",
            get(gate(Active, client::vault::get_resource)),
        )
        .route(
            "/vault/resources/{resource_id}/content",
            get(gate(Active, client::vault::get_resource_content)),
        )
}

/// What the app and `pipette-ctl` call on behalf of a *user*, under `/api/v1`.
fn operator_api(router: Router<AppState>, plan_body_limit: usize) -> Router<AppState> {
    router
        // These routes as an OpenAPI document, open to any caller: it
        // describes routes and no data (`crate::api_schema::openapi`).
        .route(
            "/api/v1/openapi.json",
            get(api_schema::openapi::openapi_document),
        )
        .route(
            "/api/v1/clients",
            get(gate(READ_FLEET, api_v1::clients::reads::list_clients)),
        )
        // The router selects this static path before `{client_id}`.
        .route(
            "/api/v1/clients/capabilities",
            get(gate(
                READ_FLEET,
                api_v1::clients::reads::get_capability_vocabulary,
            )),
        )
        // Approving a device decides whether it may authenticate, so it is
        // `manage_clients`, not a fleet grant.
        .route(
            "/api/v1/clients/{client_id}",
            get(gate(READ_FLEET, api_v1::clients::reads::get_client))
                .patch(gate(Needs(ManageClients), api_v1::clients::update_client))
                .delete(gate(Needs(ManageClients), api_v1::clients::delete_client)),
        )
        .route(
            "/api/v1/clients/{client_id}/approve",
            post(gate(Needs(ManageClients), api_v1::clients::approve_client)),
        )
        .route(
            "/api/v1/clients/{client_id}/reject",
            post(gate(Needs(ManageClients), api_v1::clients::reject_client)),
        )
        .route(
            "/api/v1/clients/{client_id}/unsuspend",
            post(gate(
                Needs(ManageClients),
                api_v1::clients::unsuspend_client,
            )),
        )
        // Whom the device may be handed to, for the app's owner field: the
        // console's boundary, which `PUT …/owner` itself does not draw.
        .route(
            "/api/v1/clients/{client_id}/owners",
            get(gate(READ_FLEET, api_v1::clients::reads::list_client_owners)),
        )
        .route(
            "/api/v1/clients/{client_id}/permissions",
            put(gate(
                Needs(ManageClients),
                api_v1::clients::set_client_permissions,
            ))
            .patch(gate(
                Needs(ManageClients),
                api_v1::clients::amend_client_permissions,
            )),
        )
        // Filed under clients: it writes the client record under
        // `manage_clients`.
        .route(
            "/api/v1/clients/{client_id}/owner",
            put(gate(
                Needs(ManageClients),
                api_v1::clients::set_client_owner,
            )),
        )
        .route(
            "/api/v1/users/register",
            post(api_v1::users::own::register_user_key),
        )
        .route(
            "/api/v1/users/register/clerk",
            post(api_v1::users::clerk::register_clerk_key),
        )
        .route("/api/v1/users/me", get(api_v1::users::own::get_own_user))
        .route(
            "/api/v1/users/me/keys",
            post(gate(Active, api_v1::users::own::add_own_key)),
        )
        .route(
            "/api/v1/users/me/keys/{key_id}",
            delete(gate(Active, api_v1::users::own::revoke_own_key)),
        )
        // `matchit` selects the static `me` and `register` before `{user_id}`.
        .route(
            "/api/v1/users/{user_id}/org",
            put(gate(Needs(ManageOrgs), api_v1::users::set_user_org)),
        )
        .route(
            "/api/v1/users",
            get(gate(Needs(ReadOrgs), api_v1::users::list_users))
                .post(gate(Needs(ManageOrgs), api_v1::users::create_user)),
        )
        .route(
            "/api/v1/users/{user_id}",
            get(gate(Needs(ReadOrgs), api_v1::users::get_user))
                .patch(gate(Needs(ManageOrgs), api_v1::users::update_user)),
        )
        .route(
            "/api/v1/users/{user_id}/permissions",
            put(gate(Needs(ManageOrgs), api_v1::users::set_permissions))
                .patch(gate(Needs(ManageOrgs), api_v1::users::amend_permissions)),
        )
        // The static `me/keys` paths win over `{user_id}` here.
        .route(
            "/api/v1/users/{user_id}/keys",
            get(gate(Needs(ManageOrgs), api_v1::users::list_user_keys)),
        )
        .route(
            "/api/v1/users/{user_id}/keys/{key_id}",
            delete(gate(Needs(ManageOrgs), api_v1::users::revoke_user_key)),
        )
        .route(
            "/api/v1/users/{user_id}/preauth-keys",
            get(gate(Needs(ManageOrgs), api_v1::users::list_preauth_keys))
                .post(gate(Needs(ManageOrgs), api_v1::users::mint_preauth_key)),
        )
        .route(
            "/api/v1/users/{user_id}/preauth-keys/{key_id}",
            delete(gate(Needs(ManageOrgs), api_v1::users::revoke_preauth_key)),
        )
        // ── the organization roster ─────────────────────────────────────────
        //
        // A read takes `{org_id}` as an id or a handle
        // (`pipette_pipeline::org::OrgRef`); a mutation takes the id only.
        .route(
            "/api/v1/orgs",
            get(gate(Needs(ReadOrgs), api_v1::orgs::list_orgs))
                .post(gate(CREATE_ORGS, api_v1::orgs::create_org)),
        )
        .route(
            "/api/v1/orgs/{org_id}",
            get(gate(Needs(ReadOrgs), api_v1::orgs::get_org))
                .patch(gate(Needs(ManageOrgs), api_v1::orgs::update_org)),
        )
        .route(
            "/api/v1/orgs/{org_id}/users",
            get(gate(Needs(ReadOrgs), api_v1::orgs::list_org_users)),
        )
        .route(
            "/api/v1/orgs/{org_id}/overview",
            get(gate(Needs(ReadOrgs), api_v1::orgs::reads::get_org_overview)),
        )
        // What changed in the organization, and who changed it.
        .route(
            "/api/v1/orgs/{org_id}/audit",
            get(gate(Needs(ManageOrgs), api_v1::audit::list_org_audit)),
        )
        // The benchmarks an organization keeps to itself, read per request by
        // `GET /benchmarks` for its principals.
        .route(
            "/api/v1/orgs/{org_id}/benchmarks",
            get(gate(Active, api_v1::orgs::catalog::list_org_benchmarks)),
        )
        .route(
            "/api/v1/orgs/{org_id}/benchmarks/{benchmark_id}",
            get(gate(Active, api_v1::orgs::catalog::get_org_benchmark))
                .put(gate(
                    Needs(ManageOrgs),
                    api_v1::orgs::catalog::put_org_benchmark,
                ))
                .delete(gate(
                    Needs(ManageOrgs),
                    api_v1::orgs::catalog::delete_org_benchmark,
                )),
        )
        // One call hands a set of devices over, as onboarding a rack wants.
        .route(
            "/api/v1/orgs/{org_id}/clients",
            get(gate(READ_ORG_DEVICES, api_v1::orgs::list_org_clients)).put(gate(
                Needs(ManageClients),
                api_v1::orgs::assign_clients_to_org,
            )),
        )
        // The pre-auth keys that register a device straight into the
        // organization, minted by its own administrator.
        .route(
            "/api/v1/orgs/{org_id}/client-preauth-keys",
            get(gate(
                Needs(ManageClients),
                api_v1::orgs::client_preauth::list_client_preauth_keys,
            ))
            .post(gate(
                Needs(ManageClients),
                api_v1::orgs::client_preauth::mint_client_preauth_key,
            )),
        )
        .route(
            "/api/v1/orgs/{org_id}/client-preauth-keys/{key_id}",
            delete(gate(
                Needs(ManageClients),
                api_v1::orgs::client_preauth::revoke_client_preauth_key,
            )),
        )
        // Every event, the host's commands and the refusals no account claims
        // included.
        .route(
            "/api/v1/audit",
            get(gate(Needs(ManageAnyOrgs), api_v1::audit::list_audit)),
        )
        .route(
            "/api/v1/descriptor-catalog",
            get(gate(
                Active,
                api_v1::descriptor_catalog::list_descriptor_catalog,
            )),
        )
        .route(
            "/api/v1/descriptor-catalog/import-runs",
            get(gate(
                READ_FLEET,
                api_v1::catalog_import_runs::list_catalog_import_runs,
            )),
        )
        .route(
            "/api/v1/descriptor-catalog/devices/{client_id}",
            get(gate(
                READ_FLEET,
                api_v1::descriptor_catalog::list_descriptor_catalog_for_device,
            )),
        )
        .route(
            "/api/v1/descriptor-catalog/names",
            get(gate(
                Active,
                api_v1::descriptor_catalog::lookup_descriptor_names,
            )),
        )
        .route(
            "/api/v1/descriptor-catalog/by-digest",
            get(gate(
                Active,
                api_v1::descriptor_catalog::lookup_descriptor_digests,
            )),
        )
        .route(
            "/api/v1/orgs/{org_id}/descriptor-catalog",
            post(gate(
                EDIT_ORG_CATALOG,
                api_v1::descriptor_catalog::create_org_descriptor_entry,
            )),
        )
        .route(
            "/api/v1/orgs/{org_id}/descriptor-catalog/{name}/members",
            post(gate(
                EDIT_ORG_CATALOG,
                api_v1::descriptor_catalog::add_org_descriptor_member,
            )),
        )
        .route(
            "/api/v1/orgs/{org_id}/descriptor-catalog/{name}/state",
            post(gate(
                EDIT_ORG_CATALOG,
                api_v1::descriptor_catalog::change_org_descriptor_entry_state,
            )),
        )
        // ── model params ────────────────────────────────────────────────────
        //
        // An organization's writes are held to its own vault files' rows.
        // Reading is `read_any_plans`, whose holder already sees every model.
        .route(
            "/api/v1/model-params",
            get(gate(
                Needs(ReadAnyPlans),
                api_v1::model_params::list_model_params,
            ))
            .post(gate(
                Needs(ManageOrgs),
                api_v1::model_params::create_model_params,
            )),
        )
        .route(
            "/api/v1/model-params/{model_key}",
            put(gate(
                Needs(ManageOrgs),
                api_v1::model_params::update_model_params,
            ))
            .delete(gate(
                Needs(ManageOrgs),
                api_v1::model_params::delete_model_params,
            )),
        )
        // ── campaigns ───────────────────────────────────────────────────────
        //
        // Beside `/api/v1/plans`, not under it: a campaign outlives any plan.
        // A create files the first plan, so it takes the plan body ceiling.
        .route(
            "/api/v1/campaigns",
            get(gate(
                READ_CAMPAIGN,
                api_v1::campaigns::reads::list_campaigns,
            ))
            .post(gate(FILE_PLANS, api_v1::campaigns::create_campaign))
            .layer(DefaultBodyLimit::max(plan_body_limit)),
        )
        .route(
            "/api/v1/campaigns/{campaign_id}",
            get(gate(READ_CAMPAIGN, api_v1::campaigns::reads::get_campaign))
                .patch(gate(
                    ADMINISTER_CAMPAIGN,
                    api_v1::campaigns::update_campaign,
                ))
                .delete(gate(
                    ADMINISTER_CAMPAIGN,
                    api_v1::campaigns::delete_campaign,
                )),
        )
        // The campaign's page in one read, for the app: what the reader may do
        // to it and the plans it has run.
        .route(
            "/api/v1/campaigns/{campaign_id}/overview",
            get(gate(
                READ_CAMPAIGN,
                api_v1::campaigns::reads::get_campaign_overview,
            )),
        )
        // Whom the campaign may be handed to, for the app's owner field.
        .route(
            "/api/v1/campaigns/{campaign_id}/owners",
            get(gate(
                ADMINISTER_CAMPAIGN,
                api_v1::campaigns::reads::list_campaign_owners,
            )),
        )
        // Read-only: the pointer moves by filing work, never by being pointed.
        .route(
            "/api/v1/campaigns/{campaign_id}/plan",
            get(gate(
                READ_CAMPAIGN,
                api_v1::campaigns::reads::get_campaign_plan,
            )),
        )
        // Filing a revision: the one write that moves the pointer.
        .route(
            "/api/v1/campaigns/{campaign_id}/plans",
            get(gate(
                READ_CAMPAIGN,
                api_v1::campaigns::reads::list_campaign_plans,
            ))
            .post(gate(FILE_PLANS, api_v1::campaigns::file_plan))
            .layer(DefaultBodyLimit::max(plan_body_limit)),
        )
        // A variant edit files a revision of the head and cancels the head.
        .route(
            "/api/v1/campaigns/{campaign_id}/variants",
            get(gate(FILE_PLANS, api_v1::campaigns::list_variants))
                .post(gate(FILE_PLANS, api_v1::campaigns::edit_variants))
                .layer(DefaultBodyLimit::max(plan_body_limit)),
        )
        // A document revision files a whole document in place of the head's.
        .route(
            "/api/v1/campaigns/{campaign_id}/document",
            get(gate(FILE_PLANS, api_v1::campaigns::read_document))
                .put(gate(FILE_PLANS, api_v1::campaigns::revise_document))
                .layer(DefaultBodyLimit::max(plan_body_limit)),
        )
        // A re-run names jobs only, so it takes the default ceiling.
        .route(
            "/api/v1/campaigns/{campaign_id}/rerun",
            post(gate(FILE_PLANS, api_v1::campaigns::rerun_jobs)),
        )
        // A revert names a plan only, so it takes the default ceiling.
        .route(
            "/api/v1/campaigns/{campaign_id}/revert",
            post(gate(FILE_PLANS, api_v1::campaigns::revert_campaign)),
        )
        // A recalculation rescores the named evals under new jobs no device
        // runs.
        .route(
            "/api/v1/campaigns/{campaign_id}/recalculate",
            post(gate(FILE_PLANS, api_v1::campaigns::recalculate_jobs)),
        )
        // Its own route, not a `PATCH` field, so both ends of the move are
        // checked.
        .route(
            "/api/v1/campaigns/{campaign_id}/owner",
            put(gate(
                ADMINISTER_CAMPAIGN,
                api_v1::campaigns::set_campaign_owner,
            )),
        )
        // ── the vault ───────────────────────────────────────────────────────
        //
        // A blob is streamed into the store, never collected, so the handler
        // holds it to `vault_blob_limit_bytes` instead.
        .route(
            "/api/v1/vault/blobs/{principal_id}/{sha256}",
            put(gate(Active, api_v1::vault::put_blob))
                .head(gate(Active, api_v1::vault::head_blob))
                .layer(DefaultBodyLimit::disable()),
        )
        .route(
            "/api/v1/vault/types",
            get(gate(Active, api_v1::vault::reads::types)),
        )
        .route(
            "/api/v1/vault/resources",
            get(gate(Active, api_v1::vault::list_resources))
                .post(gate(Active, api_v1::vault::upload_resources)),
        )
        // No `PATCH` and no `/owner`: a record is immutable, so the only
        // writes are upload and delete (`pipette_core::vault`).
        .route(
            "/api/v1/vault/resources/{resource_id}",
            get(gate(Active, client::vault::get_resource))
                .delete(gate(Active, api_v1::vault::delete_resource)),
        )
        // A plan is filed only into a campaign; the fallback tells an old
        // pipette-plan's `POST` where to file instead.
        .route(
            "/api/v1/plans",
            get(gate(Needs(ReadPlans), api_v1::plans::reads::list_plans))
                .fallback(plans_method_not_allowed),
        )
        // Reads a whole plan document, so it takes the filing routes' ceiling.
        .route(
            "/api/v1/plans/validate",
            post(gate(FILE_PLANS, api_v1::plans::validate_plan))
                .layer(DefaultBodyLimit::max(plan_body_limit)),
        )
        // The static `validate` wins over `{plan_id}` at the same position.
        .route(
            "/api/v1/plans/{plan_id}",
            get(gate(Needs(ReadPlans), api_v1::plans::reads::get_plan))
                .patch(gate(FILE_PLANS, api_v1::plans::reads::update_plan)),
        )
        .route(
            "/api/v1/plans/{plan_id}/document",
            get(gate(
                Needs(ReadPlans),
                api_v1::plans::reads::get_plan_document,
            )),
        )
        .route(
            "/api/v1/plans/{plan_id}/variants",
            get(gate(
                Needs(ReadPlans),
                api_v1::plans::reads::get_plan_variants,
            )),
        )
        .route(
            "/api/v1/plans/{plan_id}/jobs",
            get(gate(Needs(ReadPlans), api_v1::plans::reads::list_plan_jobs)),
        )
        // Read from pipette-duckdb, not the store (`pipette_pipeline::plan_results`).
        .route(
            "/api/v1/plans/{plan_id}/results.csv",
            get(gate(
                Needs(ReadPlans),
                api_v1::plans::reads::get_plan_results_csv,
            )),
        )
        .route(
            "/api/v1/plans/{plan_id}/explain",
            get(gate(Needs(ReadPlans), api_v1::plans::reads::explain_plan)),
        )
        .route(
            "/api/v1/plans/{plan_id}/cancel",
            post(gate(Needs(CancelPlans), api_v1::plans::reads::cancel_plan)),
        )
        // The plans in the caller's scope that asked for a job.
        .route(
            "/api/v1/jobs/{job_id}/plans",
            get(gate(Needs(ReadPlans), api_v1::plans::reads::list_job_plans)),
        )
}

/// A path no route serves, in the envelope every other error carries.
async fn no_route() -> AppError {
    AppError::NotFound("no such route".into())
}

/// A method the path does not serve. `AppError` has no `405`, so the body is
/// built here; axum adds the `Allow` header after.
async fn method_not_allowed() -> Response {
    let body = ErrorBody {
        error: "method not allowed".to_string(),
    };
    (StatusCode::METHOD_NOT_ALLOWED, Json(body)).into_response()
}

/// A `POST` here is a filing from a pipette-plan that predates the campaign
/// verbs, so its answer names the routes to use.
async fn plans_method_not_allowed(method: Method) -> Response {
    if method != Method::POST {
        return method_not_allowed().await;
    }
    tracing::warn!("plan filing refused: the plan listing files no plan");
    let body = ErrorBody {
        error: "This server files a plan only into a campaign. File the plan document with \
                `POST /api/v1/campaigns` or `POST /api/v1/campaigns/{campaign_id}/plans`."
            .to_string(),
    };
    (StatusCode::METHOD_NOT_ALLOWED, Json(body)).into_response()
}
