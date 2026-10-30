//! Request body ceilings: 64 KB by default, raised for the routes whose body
//! scales with the workload. What matters is the ordering: a body is refused
//! for its size before it is parsed, and a caller the route would refuse never
//! has a plan-sized body read on its behalf.

use std::num::NonZeroUsize;
use std::time::Duration;

use anyhow::Context;
use axum::body::Body;
use axum::http::StatusCode;
use rstest::rstest;
use serde_json::{Value, json};

use pipette_httpapi::state::AppState;
use pipette_plan_types::{UserId, UserPermission, UserStatus};
use pipette_web_testkit::{
    MLX_700M_VARIANT, ONE_CELL_PLAN, UserSigner, assert_nothing_staged, auth_headers, authed_post,
    body_json, created_campaign, json_str, make_state, make_state_from_config,
    make_state_with_default_body_limit, make_state_with_plan_body_limit,
    make_state_with_submission_body_limit, own_campaign_plans_uri, post_plan_document, raw_request,
    register_and_approve, register_pending, registered_user, set_user_status, setup_benchmarks,
    signed_json, test_config, unauthed_post, user_auth_headers,
};

/// Past the default `default_body_limit_bytes` (64 KiB). Raising that default
/// past this fails `test_oversized_body_is_rejected_before_parsing`.
const OVER_DEFAULT_LIMIT: usize = 65 * 1024;

/// On `POST /clients/register`, the route that serves unauthenticated callers.
/// The configured case's body is under the 64 KiB default, so only its ceiling refuses it.
#[rstest]
#[case::default(None, OVER_DEFAULT_LIMIT)]
#[case::configured(Some(1024), 2 * 1024)]
#[tokio::test]
async fn test_oversized_body_is_rejected_before_parsing(
    #[case] limit: Option<usize>,
    #[case] details_len: usize,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = match limit {
        None => make_state(dir.path()).await?,
        Some(limit) => {
            let limit = NonZeroUsize::new(limit).context("limit is non-zero")?;
            make_state_with_default_body_limit(dir.path(), limit).await?
        }
    };

    // Valid in every respect except size, so a rejection can only be the limit.
    let body = json!({
        "generate_key": true,
        "organization": "test-org",
        "client_details": "x".repeat(details_len),
        "contact_email": "t@t.com"
    });
    let resp = unauthed_post(&state, "/clients/register", &body).await?;

    assert_eq!(resp.status(), StatusCode::PAYLOAD_TOO_LARGE);
    let body = body_json(resp).await?;
    assert_eq!(json_str(&body, "error")?, "request body is too large");
    Ok(())
}

#[tokio::test]
async fn test_an_oversized_malformed_body_reports_only_its_size() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;

    // Broken a few bytes in, so a parser would fail long before the ceiling.
    let body = format!("{{\"organization\": {}", "x".repeat(OVER_DEFAULT_LIMIT));
    let resp = raw_request(
        &state,
        "POST",
        "/clients/register",
        &[],
        Some("application/json"),
        body,
    )
    .await?;

    assert_eq!(resp.status(), StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(
        json_str(&body_json(resp).await?, "error")?,
        "request body is too large"
    );
    Ok(())
}

fn padding() -> String {
    "x".repeat(OVER_DEFAULT_LIMIT)
}

/// The submission routes raise the ceiling, so a body far past the router-wide
/// limit reaches the handler and is judged on its contents.
#[rstest]
#[case::single(
    "/benchmarks",
    json!({ "padding": padding() }),
    StatusCode::BAD_REQUEST,
    "/error",
    "benchmark_id"
)]
#[case::batch_envelope(
    "/benchmarks/batch",
    json!({ "padding": padding() }),
    StatusCode::BAD_REQUEST,
    "/error",
    "submissions"
)]
#[case::batch_item(
    "/benchmarks/batch",
    json!({ "submissions": [{ "padding": padding() }] }),
    StatusCode::OK,
    "/results/0/error",
    "benchmark_id"
)]
#[tokio::test]
async fn test_submission_routes_take_bodies_over_the_default_limit(
    #[case] uri: &str,
    #[case] body: Value,
    #[case] expected: StatusCode,
    #[case] pointer: &str,
    #[case] field: &str,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (sk, client_id) = register_and_approve(&state).await?;

    let resp = authed_post(&state, &sk, &client_id, uri, &body).await?;

    // Each error names a field only a handler that read the whole body sees.
    assert_eq!(resp.status(), expected, "{uri}");
    let body = body_json(resp).await?;
    assert!(
        body.pointer(pointer)
            .and_then(Value::as_str)
            .is_some_and(|e| e.contains(field)),
        "{body}"
    );
    Ok(())
}

/// Small enough that exceeding it is cheap, yet well over
/// [`OVER_DEFAULT_LIMIT`].
const TEST_SUBMISSION_BODY_LIMIT: usize = 256 * 1024;

const OVER_SUBMISSION_LIMIT: usize = TEST_SUBMISSION_BODY_LIMIT + 1;

async fn submission_limited_state(dir: &std::path::Path) -> anyhow::Result<AppState> {
    let limit = NonZeroUsize::new(TEST_SUBMISSION_BODY_LIMIT).context("limit is non-zero")?;
    make_state_with_submission_body_limit(dir, limit).await
}

/// Without a ceiling any signed device could make the server buffer any size.
#[rstest]
#[case::single("/benchmarks")]
#[case::batch("/benchmarks/batch")]
#[tokio::test]
async fn test_submission_routes_refuse_bodies_over_their_ceiling(
    #[case] uri: &str,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = submission_limited_state(dir.path()).await?;
    let (sk, client_id) = register_and_approve(&state).await?;

    // The signature covers no body, so these bytes need not be JSON either.
    let headers = auth_headers(&sk, &client_id, "POST", uri)?;
    let body = vec![b'x'; OVER_SUBMISSION_LIMIT];
    let resp = raw_request(
        &state,
        "POST",
        uri,
        &headers,
        Some("application/json"),
        body,
    )
    .await?;

    assert_eq!(resp.status(), StatusCode::PAYLOAD_TOO_LARGE, "{uri}");
    assert_eq!(
        json_str(&body_json(resp).await?, "error")?,
        "request body is too large"
    );
    Ok(())
}

fn scalars() -> Value {
    json!(vec![0; TEST_SUBMISSION_BODY_LIMIT / 8])
}

fn objects() -> Value {
    json!(vec![json!({ "a": 0 }); 1000])
}

/// Bodies far under the ceiling that parse past it: a zero takes a 32-byte
/// `Value`, and a one-key object a whole map node.
#[rstest]
#[case::single_scalars("/benchmarks", "padding", scalars())]
#[case::batch_scalars("/benchmarks/batch", "submissions", scalars())]
#[case::single_objects("/benchmarks", "padding", objects())]
#[case::batch_objects("/benchmarks/batch", "submissions", objects())]
#[tokio::test]
async fn test_submission_routes_refuse_bodies_dense_in_values(
    #[case] uri: &str,
    #[case] field: &str,
    #[case] values: Value,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = submission_limited_state(dir.path()).await?;
    let (sk, client_id) = register_and_approve(&state).await?;

    let body = json!({ field: values });
    let resp = authed_post(&state, &sk, &client_id, uri, &body).await?;

    assert_eq!(resp.status(), StatusCode::PAYLOAD_TOO_LARGE, "{uri}");
    assert_eq!(
        json_str(&body_json(resp).await?, "error")?,
        format!("request body would take more than {TEST_SUBMISSION_BODY_LIMIT} bytes once parsed")
    );
    Ok(())
}

/// A body that stops arriving gives up its share of the ceiling.
#[tokio::test]
async fn test_a_submission_body_that_stalls_times_out() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = submission_limited_state(dir.path()).await?;
    let (sk, client_id) = register_and_approve(&state).await?;
    let headers = auth_headers(&sk, &client_id, "POST", "/benchmarks")?;

    // Paused, the clock jumps to the idle timeout once the request only waits on it.
    tokio::time::pause();
    let body = Body::from_stream(futures::stream::pending::<Result<Vec<u8>, std::io::Error>>());
    let resp = raw_request(
        &state,
        "POST",
        "/benchmarks",
        &headers,
        Some("application/json"),
        body,
    )
    .await?;

    assert_eq!(resp.status(), StatusCode::REQUEST_TIMEOUT);
    assert_eq!(
        json_str(&body_json(resp).await?, "error")?,
        "the request body sent nothing for 60 seconds"
    );
    Ok(())
}

/// A device reads one submission body at a time, so a stalled upload holds up
/// only that device.
#[tokio::test]
async fn test_a_stalled_submission_holds_up_only_its_device() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = submission_limited_state(dir.path()).await?;
    let (sk, client_id) = register_and_approve(&state).await?;
    let (other_sk, other_id) = register_and_approve(&state).await?;
    let headers = auth_headers(&sk, &client_id, "POST", "/benchmarks")?;
    let body = pipette_web_testkit::valid_submission("prefill_throughput_256");

    tokio::time::pause();
    let pending = Body::from_stream(futures::stream::pending::<Result<Vec<u8>, std::io::Error>>());
    // Boxed, not pinned in place, so dropping it ends the request.
    let mut stalled = Box::pin(raw_request(
        &state,
        "POST",
        "/benchmarks",
        &headers,
        Some("application/json"),
        pending,
    ));
    let next = authed_post(&state, &sk, &client_id, "/benchmarks", &body);
    tokio::pin!(next);
    let wait = Duration::from_secs(1);

    assert!(tokio::time::timeout(wait, &mut stalled).await.is_err());
    assert!(tokio::time::timeout(wait, &mut next).await.is_err());
    let other = authed_post(&state, &other_sk, &other_id, "/benchmarks", &body).await?;
    assert_eq!(other.status(), StatusCode::ACCEPTED);
    drop(stalled);
    assert_eq!(next.await?.status(), StatusCode::ACCEPTED);
    Ok(())
}

/// Four pending devices stalling uploads of no declared length, enough to fill
/// all four ceilings, delay no approved device: held bodies share one.
#[tokio::test]
async fn test_stalled_held_submissions_leave_approved_devices_room() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let mut config = test_config(dir.path());
    config.submission_body_limit_bytes =
        NonZeroUsize::new(TEST_SUBMISSION_BODY_LIMIT).context("limit is non-zero")?;
    config.unverified_submissions.enabled = true;
    let state = make_state_from_config(config).await?;
    let (sk, client_id) = register_and_approve(&state).await?;
    let pending = futures::future::try_join_all((0..4).map(|_| register_pending(&state))).await?;
    let headers = pending
        .iter()
        .map(|(key, id)| auth_headers(key, id, "POST", "/benchmarks"))
        .collect::<Result<Vec<_>, _>>()?;

    tokio::time::pause();
    let mut stalled = headers
        .iter()
        .map(|headers| {
            let body =
                Body::from_stream(futures::stream::pending::<Result<Vec<u8>, std::io::Error>>());
            Box::pin(raw_request(
                &state,
                "POST",
                "/benchmarks",
                headers,
                Some("application/json"),
                body,
            ))
        })
        .collect::<Vec<_>>();
    let wait = Duration::from_secs(1);
    let all = futures::future::join_all(stalled.iter_mut());
    assert!(tokio::time::timeout(wait, all).await.is_err());

    let body = pipette_web_testkit::valid_submission("prefill_throughput_256");
    let approved = authed_post(&state, &sk, &client_id, "/benchmarks", &body);
    let resp = tokio::time::timeout(wait, approved)
        .await
        .context("the held uploads held up an approved device")??;
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    Ok(())
}

/// A parsed submission waits for its share of the trees in flight.
#[tokio::test]
async fn test_a_submission_waits_for_the_trees_in_flight() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = submission_limited_state(dir.path()).await?;
    let (sk, client_id) = register_and_approve(&state).await?;
    let all = u32::try_from(state.submissions_in_flight.available_permits())?;
    let bodies = state.submission_bodies.available_permits();
    let busy = state.submissions_in_flight.try_acquire_many(all)?;

    // Paused, the clock jumps to the timeout only once the request waits on a permit.
    tokio::time::pause();
    let body = pipette_web_testkit::valid_submission("prefill_throughput_256");
    let request = authed_post(&state, &sk, &client_id, "/benchmarks", &body);
    tokio::pin!(request);
    let waited = tokio::time::timeout(Duration::from_secs(1), &mut request).await;

    assert!(waited.is_err(), "answered while every tree share was taken");
    assert!(
        state.submission_bodies.available_permits() < bodies,
        "waited before its body was read"
    );
    drop(busy);
    assert_eq!(request.await?.status(), StatusCode::ACCEPTED);
    assert_eq!(state.submission_bodies.available_permits(), bodies);
    Ok(())
}

enum RefusedBody {
    /// Not JSON, which reading the body would answer with `400`.
    Malformed,
    /// Past the ceiling, which reading it would answer with `413`.
    Oversized,
}

impl RefusedBody {
    fn into_body(self) -> Body {
        match self {
            Self::Malformed => Body::from("{"),
            Self::Oversized => Body::from(vec![b'{'; OVER_SUBMISSION_LIMIT]),
        }
    }
}

/// With the unverified archive off: `403`, not the `400` or `413` the body
/// would earn.
#[rstest]
#[case::single_malformed("/benchmarks", RefusedBody::Malformed)]
#[case::single_oversized("/benchmarks", RefusedBody::Oversized)]
#[case::batch_malformed("/benchmarks/batch", RefusedBody::Malformed)]
#[case::batch_oversized("/benchmarks/batch", RefusedBody::Oversized)]
#[tokio::test]
async fn test_a_pending_client_is_refused_before_its_submission_is_read(
    #[case] uri: &str,
    #[case] body: RefusedBody,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = submission_limited_state(dir.path()).await?;
    let (sk, client_id) = register_pending(&state).await?;

    let headers = auth_headers(&sk, &client_id, "POST", uri)?;
    let resp = raw_request(
        &state,
        "POST",
        uri,
        &headers,
        Some("application/json"),
        body.into_body(),
    )
    .await?;

    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    assert_eq!(
        json_str(&body_json(resp).await?, "error")?,
        "client is not approved"
    );
    Ok(())
}

// ── the plan routes, whose ceiling is configured rather than fixed ──────────────

/// Small enough that exceeding it is cheap, yet well over
/// [`OVER_DEFAULT_LIMIT`].
const TEST_PLAN_BODY_LIMIT: usize = 256 * 1024;

/// A plan document padded to `padding` bytes by a comment, followed by `rest`.
fn padded_document(padding: usize, rest: &str) -> String {
    format!("# {}\n{rest}", "x".repeat(padding))
}

/// The `TempDir` comes back too, since dropping it deletes the store.
async fn plan_limit_state() -> anyhow::Result<(tempfile::TempDir, AppState)> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let limit = NonZeroUsize::new(TEST_PLAN_BODY_LIMIT).context("limit is non-zero")?;
    let state = make_state_with_plan_body_limit(dir.path(), limit).await?;
    Ok((dir, state))
}

enum Caller {
    Approved,
    Pending,
    Anonymous,
    /// A user account holding `submit_plans`: the ordinary plan filer.
    Submitter,
    /// An account that holds `submit_plans` but still waits for an operator.
    PendingUser,
    /// An active account that an operator has granted nothing.
    UngrantedUser,
}

/// Every gate runs before the plan body is read, so only a caller the route
/// admits can make the server read one. Registration and asking for an account
/// are both self-service, so without this ordering either could exhaust memory
/// through the raised ceiling. The pending user holds `submit_plans`, so its
/// `403` can only come from the status gate.
#[rstest]
#[case::approved_client_is_refused_first(
    Caller::Approved,
    StatusCode::UNAUTHORIZED,
    DEVICE_REFUSAL
)]
#[case::submitter_hits_the_ceiling(
    Caller::Submitter,
    StatusCode::PAYLOAD_TOO_LARGE,
    "request body is too large"
)]
#[case::pending_is_refused_first(Caller::Pending, StatusCode::UNAUTHORIZED, DEVICE_REFUSAL)]
#[case::pending_user_is_refused_first(
    Caller::PendingUser,
    StatusCode::FORBIDDEN,
    "user is pending an operator's approval"
)]
#[case::ungranted_user_is_refused_first(
    Caller::UngrantedUser,
    StatusCode::FORBIDDEN,
    "user lacks the submit_plans permission"
)]
#[case::anonymous_is_refused_first(
    Caller::Anonymous,
    StatusCode::UNAUTHORIZED,
    "missing X-Client-Id header"
)]
#[tokio::test]
async fn test_only_an_admitted_caller_can_send_a_plan_sized_body(
    #[case] caller: Caller,
    #[case] expected: StatusCode,
    #[case] expected_error: &str,
) -> anyhow::Result<()> {
    let (_dir, state) = plan_limit_state().await?;
    let body = padded_document(TEST_PLAN_BODY_LIMIT + 1, ONE_CELL_PLAN);
    let owner = registered_user(&state, &[UserPermission::SubmitPlans]).await?;

    let (uri, headers) = match caller {
        Caller::Anonymous => (own_campaign_plans_uri(&state, &owner.1).await?, Vec::new()),
        Caller::Approved => {
            device_request(&state, &owner.1, register_and_approve(&state).await?).await?
        }
        Caller::Pending => {
            device_request(&state, &owner.1, register_pending(&state).await?).await?
        }
        Caller::Submitter => user_request(&state, &[UserPermission::SubmitPlans], false).await?,
        Caller::PendingUser => user_request(&state, &[UserPermission::SubmitPlans], true).await?,
        Caller::UngrantedUser => user_request(&state, &[], false).await?,
    };
    let resp = raw_request(
        &state,
        "POST",
        &uri,
        &headers,
        Some("application/toml"),
        body,
    )
    .await?;

    let status = resp.status();
    assert_eq!(
        (status, json_str(&body_json(resp).await?, "error")?),
        (expected, expected_error)
    );
    assert_nothing_staged(&state).await?;
    Ok(())
}

/// The plans route of a campaign that `owner` owns, and the headers of
/// `device` signing a `POST` to it.
async fn device_request(
    state: &AppState,
    owner: &UserId,
    (key, client_id): (ed25519_dalek::SigningKey, pipette_plan_types::ClientId),
) -> anyhow::Result<(String, Vec<(&'static str, String)>)> {
    let uri = own_campaign_plans_uri(state, owner).await?;
    let headers = auth_headers(&key, &client_id, "POST", &uri)?;
    Ok((uri, headers))
}

/// A new account's own campaign's plans route, and that account's signed `POST`
/// headers. `pending` resets it after the grant, so a refusal is the status
/// gate's.
async fn user_request(
    state: &AppState,
    grants: &[UserPermission],
    pending: bool,
) -> anyhow::Result<(String, Vec<(&'static str, String)>)> {
    let (key, user_id, key_id) = registered_user(state, grants).await?;
    if pending {
        set_user_status(state, &user_id, UserStatus::Pending).await?;
    }
    let uri = own_campaign_plans_uri(state, &user_id).await?;
    let headers = user_auth_headers(&key, &user_id, &key_id, "POST", &uri)?;
    Ok((uri, headers))
}

/// What a device that signs a request to a user route gets.
const DEVICE_REFUSAL: &str = "invalid X-Client-Id header: user id must start with `user_`";

/// A plan document obeys the configured ceiling, to file or to check. The
/// body is a comment, so size is the only thing that can be wrong with it.
#[rstest]
#[case::file(true)]
#[case::validate(false)]
#[tokio::test]
async fn test_an_oversized_plan_document_is_refused(#[case] file: bool) -> anyhow::Result<()> {
    let (_dir, state) = plan_limit_state().await?;
    let signer = registered_user(&state, &[UserPermission::SubmitPlans]).await?;
    let uri = if file {
        own_campaign_plans_uri(&state, &signer.1).await?
    } else {
        "/api/v1/plans/validate".to_string()
    };

    let document = padded_document(TEST_PLAN_BODY_LIMIT, "");
    let resp = post_plan_document(&state, &signer, &uri, &document).await?;

    assert_eq!(resp.status(), StatusCode::PAYLOAD_TOO_LARGE, "{uri}");
    assert_eq!(
        json_str(&body_json(resp).await?, "error")?,
        "request body is too large"
    );
    assert_nothing_staged(&state).await?;
    Ok(())
}

/// A plan document grows with each variant it lists, so the route needs the
/// raise.
#[tokio::test]
async fn test_plan_route_takes_bodies_over_the_default_limit() -> anyhow::Result<()> {
    let (_dir, state) = plan_limit_state().await?;
    let signer = registered_user(&state, &[UserPermission::SubmitPlans]).await?;
    let uri = own_campaign_plans_uri(&state, &signer.1).await?;

    // Well over the router-wide ceiling, well under the configured one. The
    // document does not parse, so the answer is a `400` — which is only
    // reachable once the body has been read whole.
    let document = padded_document(OVER_DEFAULT_LIMIT, "benchmarks = [");
    let resp = post_plan_document(&state, &signer, &uri, &document).await?;

    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let error = json_str(&body_json(resp).await?, "error")?.to_string();
    assert!(error.contains("not a valid scheduler plan"), "{error}");
    assert_nothing_staged(&state).await?;
    Ok(())
}

// ── POST /api/v1/plans/validate, which reads a document under the same ceiling ─────

/// A valid document past the router-wide default reaches the check, so any
/// document filing takes can be checked first.
#[tokio::test]
async fn test_validation_takes_documents_over_the_default_limit() -> anyhow::Result<()> {
    let (_dir, state) = plan_limit_state().await?;
    let signer = registered_user(&state, &[UserPermission::SubmitPlans]).await?;

    let document = format!("# {}\n{ONE_CELL_PLAN}", "x".repeat(OVER_DEFAULT_LIMIT));
    let resp = post_plan_document(&state, &signer, "/api/v1/plans/validate", &document).await?;

    assert_eq!(resp.status(), StatusCode::OK);
    let verdict = body_json(resp).await?;
    assert_eq!(verdict.pointer("/issues"), Some(&json!([])), "{verdict}");
    Ok(())
}

// ── the campaign routes, which file a plan under the same ceiling ────────────

#[derive(Clone, Copy, Debug)]
enum CampaignRoute {
    /// `POST /api/v1/campaigns`, which files the first plan.
    Create,
    /// `POST /api/v1/campaigns/{campaign_id}/plans`, which files a revision.
    Revision,
    /// `POST /api/v1/campaigns/{campaign_id}/variants`, which files a variant edit.
    Variants,
}

impl CampaignRoute {
    /// The URI to post a document to, creating the campaign a revision needs.
    async fn uri(self, state: &AppState, signer: &UserSigner) -> anyhow::Result<String> {
        let campaign = || created_campaign(state, signer, &[("name", "Nightly")]);
        Ok(match self {
            Self::Create => "/api/v1/campaigns?name=Nightly".to_string(),
            Self::Revision => format!("/api/v1/campaigns/{}/plans", campaign().await?.0),
            Self::Variants => {
                let (campaign_id, head) = campaign().await?;
                format!("/api/v1/campaigns/{campaign_id}/variants?base_plan_id={head}")
            }
        })
    }

    /// A body the route files: a plan document, or a variant edit.
    fn document(self) -> &'static str {
        match self {
            Self::Create | Self::Revision => ONE_CELL_PLAN,
            Self::Variants => MLX_700M_VARIANT,
        }
    }
}

/// A caller that every campaign route admits, so that its body reaches the size
/// limit ([`pipette_web_testkit::FULL_EDITOR`]).
async fn campaign_filer(state: &AppState) -> anyhow::Result<UserSigner> {
    registered_user(state, pipette_web_testkit::FULL_EDITOR).await
}

/// The plans and campaigns in the store, which a refused request must leave as
/// it found them.
async fn stored(state: &AppState) -> anyhow::Result<(usize, usize)> {
    let plans = pipette_store::drain_plans(state.plan_store.as_ref(), None)
        .await?
        .items
        .len();
    let campaigns = pipette_store::drain_campaigns(state.campaign_store.as_ref())
        .await?
        .items
        .len();
    Ok((plans, campaigns))
}

/// A valid plan document padded past the router-wide default reaches the
/// handler on every campaign route, so a real plan can be filed through them.
#[rstest]
#[case::create(CampaignRoute::Create)]
#[case::revision(CampaignRoute::Revision)]
#[case::variants(CampaignRoute::Variants)]
#[tokio::test]
async fn test_campaign_routes_take_documents_over_the_default_limit(
    #[case] route: CampaignRoute,
) -> anyhow::Result<()> {
    let (_dir, state) = plan_limit_state().await?;
    let signer = campaign_filer(&state).await?;
    let uri = route.uri(&state, &signer).await?;

    let document = format!("# {}\n{}", "x".repeat(OVER_DEFAULT_LIMIT), route.document());
    let resp = post_plan_document(&state, &signer, &uri, &document).await?;

    assert_eq!(resp.status(), StatusCode::CREATED, "{route:?}");
    Ok(())
}

/// A document past the configured ceiling is refused on every campaign route
/// before it is read, and neither a plan nor a campaign is written.
#[rstest]
#[case::create(CampaignRoute::Create)]
#[case::revision(CampaignRoute::Revision)]
#[case::variants(CampaignRoute::Variants)]
#[tokio::test]
async fn test_an_oversized_campaign_document_is_refused(
    #[case] route: CampaignRoute,
) -> anyhow::Result<()> {
    let (_dir, state) = plan_limit_state().await?;
    let signer = campaign_filer(&state).await?;
    let uri = route.uri(&state, &signer).await?;
    let before = stored(&state).await?;

    let document = format!("# {}\n", "x".repeat(TEST_PLAN_BODY_LIMIT));
    let resp = post_plan_document(&state, &signer, &uri, &document).await?;

    assert_eq!(resp.status(), StatusCode::PAYLOAD_TOO_LARGE, "{route:?}");
    assert_eq!(
        json_str(&body_json(resp).await?, "error")?,
        "request body is too large"
    );
    assert_eq!(stored(&state).await?, before, "the refused document wrote");
    Ok(())
}

/// A caller holding no campaign grant is refused before its body is read: an
/// oversized document answers `403`, not the `413` that reading it would reach.
#[rstest]
#[case::create(CampaignRoute::Create)]
#[case::revision(CampaignRoute::Revision)]
#[case::variants(CampaignRoute::Variants)]
#[tokio::test]
async fn test_a_refused_caller_sends_no_campaign_sized_body(
    #[case] route: CampaignRoute,
) -> anyhow::Result<()> {
    let (_dir, state) = plan_limit_state().await?;
    let owner = registered_user(&state, &[UserPermission::SubmitPlans]).await?;
    let uri = route.uri(&state, &owner).await?;
    let ungranted = registered_user(&state, &[]).await?;
    let before = stored(&state).await?;

    let document = format!("# {}\n", "x".repeat(TEST_PLAN_BODY_LIMIT));
    let resp = post_plan_document(&state, &ungranted, &uri, &document).await?;

    assert_eq!(resp.status(), StatusCode::FORBIDDEN, "{route:?}");
    let error = json_str(&body_json(resp).await?, "error")?.to_string();
    assert!(error.contains("submit_plans"), "{error}");
    assert_eq!(stored(&state).await?, before, "the refused caller wrote");
    Ok(())
}

/// A caller that administers the campaign but may not cancel the plan an edit
/// replaces is refused before its body is read too: an oversized edit answers
/// `403`, not `413`.
#[tokio::test]
async fn test_an_edit_refused_on_its_plan_sends_no_body() -> anyhow::Result<()> {
    let (_dir, state) = plan_limit_state().await?;
    let owner = registered_user(&state, &[UserPermission::SubmitPlans]).await?;
    let uri = CampaignRoute::Variants.uri(&state, &owner).await?;
    let before = stored(&state).await?;

    let document = format!("# {}\n", "x".repeat(TEST_PLAN_BODY_LIMIT));
    let resp = post_plan_document(&state, &owner, &uri, &document).await?;

    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    let error = json_str(&body_json(resp).await?, "error")?.to_string();
    assert!(error.contains("cancel_plans"), "{error}");
    assert_eq!(stored(&state).await?, before, "the refused caller wrote");
    Ok(())
}

/// A malformed campaign id answers `400`, not the `413` its oversized body would reach.
#[tokio::test]
async fn test_a_malformed_path_id_is_refused_before_the_body_is_read() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let signer = campaign_filer(&state).await?;

    let body = json!({ "name": "x".repeat(OVER_DEFAULT_LIMIT) });
    let resp = signed_json(&state, &signer, "PATCH", "/api/v1/campaigns/nightly", &body).await?;

    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        json_str(&body_json(resp).await?, "error")?,
        "campaign_id must start with \"campaign_\""
    );
    Ok(())
}
