//! The benchmark catalog: `GET /benchmarks` and
//! `GET /benchmarks/{benchmark_id}`, and their `/api/v2` twins. They answer
//! anyone, and an organization's private entry reads as absent to all but its
//! principals. An eval benchmark whose upstream is unreachable or answers
//! nonsense is a `502`, not a `500`.

use axum::http::{StatusCode, header};
use ed25519_dalek::SigningKey;
use http_body_util::BodyExt;
use httpmock::Method::GET;
use httpmock::MockServer;
use rstest::rstest;
use serde_json::json;

use pipette_core::client::ClientStatus;
use pipette_core::organization::Organization;
use pipette_httpapi::state::AppState;
use pipette_plan_types::{BenchmarkType, ClientId, ClientOwner, UserId, UserKeyId, UserStatus};
use pipette_web_testkit::{
    auth_headers, authed_get, authed_get_with_headers, authed_post, authed_user_get, benchmark_ids,
    body_json, make_state, make_state_with_evals_url, own_client, own_client_by_org,
    register_and_approve, register_pending, registered_user, seed_org, set_user_org,
    set_user_status, setup_benchmarks, setup_private_benchmark, setup_scheduler_benchmarks,
    unauthed_get, unauthed_get_with_headers, user_auth_headers, valid_submission,
};

/// The private entry every visibility test seeds, beside the four public ones.
const PRIVATE: &str = "eval_acme_private";

/// The four public entries and one private to a freshly seeded `acme`, which
/// is returned so a test can hang a device or an account off it.
async fn state_with_a_private_entry(
    dir: &std::path::Path,
) -> anyhow::Result<(AppState, Organization)> {
    setup_benchmarks(dir)?;
    let state = make_state(dir).await?;
    let acme = seed_org(&state, "acme").await?;
    setup_private_benchmark(dir, PRIVATE, &acme.org_id)?;
    Ok((state, acme))
}

/// An organization's entry under an id the public catalog holds is not served
/// twice, and not served at all: the public one wins, on the listing and by
/// id. The write side refuses such an entry; this covers one planted on disk.
#[rstest]
#[tokio::test]
async fn a_public_id_wins_over_a_private_entry(
    #[values("/benchmarks", "/api/v2/benchmarks")] route: &str,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, acme) = state_with_a_private_entry(dir.path()).await?;
    setup_private_benchmark(dir.path(), "prefill_throughput_256", &acme.org_id)?;
    let (key, client_id) = register_and_approve(&state).await?;
    own_client_by_org(&state, &client_id, &acme.org_id).await?;

    let ids = listed(authed_get(&state, &key, &client_id, route).await?).await?;
    assert_eq!(ids.len(), 5, "no duplicate row for the shadowed id");
    let body = body_json(
        authed_get(
            &state,
            &key,
            &client_id,
            &format!("{route}/prefill_throughput_256"),
        )
        .await?,
    )
    .await?;
    assert_eq!(
        body["benchmark_type"], "prefill_throughput",
        "the public one"
    );
    assert!(body.get("org_id").is_none());
    Ok(())
}

async fn listed(resp: axum::http::Response<axum::body::Body>) -> anyhow::Result<Vec<String>> {
    anyhow::ensure!(resp.status() == StatusCode::OK, "listing failed");
    Ok(benchmark_ids(&body_json(resp).await?))
}

/// Whether a client's listing carries the private entry, and whether its
/// detail route answers — the two surfaces, and their `/api/v2` twins, have to agree.
async fn client_sees_private(
    state: &AppState,
    key: &SigningKey,
    client_id: &ClientId,
) -> anyhow::Result<bool> {
    let ids = listed(authed_get(state, key, client_id, "/benchmarks").await?).await?;
    let listed = ids.iter().any(|id| id == PRIVATE);
    let resp = authed_get(state, key, client_id, &format!("/benchmarks/{PRIVATE}")).await?;
    // The evals server the tests point at is unreachable, so an entry that is
    // served is a `502` from the samples proxy; one that is not is a `404`
    // decided before that call. Either way the listing and the detail agree.
    let detailed = match resp.status() {
        StatusCode::BAD_GATEWAY => true,
        StatusCode::NOT_FOUND => false,
        other => anyhow::bail!("unexpected status {other} for the private entry"),
    };
    anyhow::ensure!(
        listed == detailed,
        "the listing ({listed}) and the detail route ({detailed}) disagree"
    );
    // `/api/v2` calls no upstream, so an entry it serves is a `200`.
    let v2_ids =
        self::listed(authed_get(state, key, client_id, "/api/v2/benchmarks").await?).await?;
    let v2_listed = v2_ids.iter().any(|id| id == PRIVATE);
    let resp = authed_get(
        state,
        key,
        client_id,
        &format!("/api/v2/benchmarks/{PRIVATE}"),
    )
    .await?;
    let v2_detailed = match resp.status() {
        StatusCode::OK => true,
        StatusCode::NOT_FOUND => false,
        other => anyhow::bail!("unexpected status {other} for the private entry on /api/v2"),
    };
    anyhow::ensure!(
        v2_listed == listed && v2_detailed == listed,
        "/api/v2 (listed {v2_listed}, detailed {v2_detailed}) disagrees with v1 ({listed})"
    );
    Ok(listed)
}

/// Nobody in particular is served the public catalog, and a private entry is
/// not in it: absent from the listing, and `404` by id like anything unknown.
#[rstest]
#[tokio::test]
async fn a_private_entry_is_absent_from_the_public_catalog(
    #[values("/benchmarks", "/api/v2/benchmarks")] route: &str,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, _) = state_with_a_private_entry(dir.path()).await?;

    let ids = listed(unauthed_get(&state, route).await?).await?;
    assert_eq!(ids.len(), 4);
    assert!(!ids.iter().any(|id| id == PRIVATE));

    let resp = unauthed_get(&state, &format!("{route}/{PRIVATE}")).await?;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    assert_eq!(body_json(resp).await?["error"], "benchmark not found");
    Ok(())
}

/// A device the organization holds, directly or through a member who owns
/// it, is served the organization's private entries beside the public ones,
/// and each private entry names the organization it is private to.
#[rstest]
#[case::org_owned(true)]
#[case::user_owned(false)]
#[tokio::test]
async fn an_owned_device_reads_its_organizations_private_entries(
    #[case] org_owned: bool,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, acme) = state_with_a_private_entry(dir.path()).await?;
    let (key, client_id) = register_and_approve(&state).await?;
    let owner = if org_owned {
        ClientOwner::Org {
            org_id: acme.org_id.clone(),
        }
    } else {
        let (_, member, _) = registered_user(&state, &[]).await?;
        set_user_org(&state, &member, Some(acme.org_id.clone())).await?;
        ClientOwner::User { user_id: member }
    };
    own_client(&state, &client_id, owner).await?;

    let ids = listed(authed_get(&state, &key, &client_id, "/benchmarks").await?).await?;
    assert_eq!(ids.len(), 5);
    assert!(client_sees_private(&state, &key, &client_id).await?);

    let body = body_json(authed_get(&state, &key, &client_id, "/benchmarks").await?).await?;
    let private = body
        .as_array()
        .and_then(|items| items.iter().find(|b| b["benchmark_id"] == PRIVATE))
        .ok_or_else(|| anyhow::anyhow!("the private entry is listed"))?;
    assert_eq!(private["org_id"], acme.org_id.as_str());
    assert!(
        body.as_array().is_some_and(|items| items
            .iter()
            .filter(|b| b["benchmark_id"] != PRIVATE)
            .all(|b| b.get("org_id").is_none())),
        "a public entry carries no org_id"
    );
    Ok(())
}

/// A signature alone opens nothing: a device nobody owns, one another
/// organization holds, and one still pending all read the public catalog.
#[rstest]
#[case::unowned(None, ClientStatus::Approved)]
#[case::another_org(Some("labs"), ClientStatus::Approved)]
#[case::pending(Some("acme"), ClientStatus::Pending)]
#[tokio::test]
async fn a_device_outside_the_organization_reads_the_public_catalog(
    #[case] owned_by: Option<&str>,
    #[case] status: ClientStatus,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, acme) = state_with_a_private_entry(dir.path()).await?;
    let (key, client_id) = match status {
        ClientStatus::Approved => register_and_approve(&state).await?,
        ClientStatus::Pending => register_pending(&state).await?,
    };
    let owner = match owned_by {
        None => None,
        Some("acme") => Some(acme.org_id),
        Some(other) => Some(seed_org(&state, other).await?.org_id),
    };
    if let Some(org_id) = owner {
        own_client_by_org(&state, &client_id, &org_id).await?;
    }

    let ids = listed(authed_get(&state, &key, &client_id, "/benchmarks").await?).await?;
    assert_eq!(ids.len(), 4);
    assert!(!client_sees_private(&state, &key, &client_id).await?);
    Ok(())
}

/// An account is served its organization's private entries through its
/// membership, once active; one belonging to no organization, and one still
/// pending, read the public catalog.
#[rstest]
#[case::member(true, UserStatus::Active, true)]
#[case::no_org(false, UserStatus::Active, false)]
#[case::pending_member(true, UserStatus::Pending, false)]
#[tokio::test]
async fn an_account_reads_through_its_membership(
    #[case] member: bool,
    #[case] status: UserStatus,
    #[case] sees_private: bool,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, acme) = state_with_a_private_entry(dir.path()).await?;
    let (key, user_id, key_id) = registered_user(&state, &[]).await?;
    if member {
        set_user_org(&state, &user_id, Some(acme.org_id.clone())).await?;
    }
    set_user_status(&state, &user_id, status).await?;

    let ids =
        listed(authed_user_get(&state, &key, &user_id, &key_id, "/benchmarks").await?).await?;
    assert_eq!(ids.iter().any(|id| id == PRIVATE), sees_private);
    assert_eq!(ids.len(), if sees_private { 5 } else { 4 });

    let resp = authed_user_get(
        &state,
        &key,
        &user_id,
        &key_id,
        &format!("/benchmarks/{PRIVATE}"),
    )
    .await?;
    let expected = if sees_private {
        StatusCode::BAD_GATEWAY
    } else {
        StatusCode::NOT_FOUND
    };
    assert_eq!(resp.status(), expected);

    let ids = listed(authed_user_get(&state, &key, &user_id, &key_id, "/api/v2/benchmarks").await?)
        .await?;
    assert_eq!(ids.iter().any(|id| id == PRIVATE), sees_private);
    let resp = authed_user_get(
        &state,
        &key,
        &user_id,
        &key_id,
        &format!("/api/v2/benchmarks/{PRIVATE}"),
    )
    .await?;
    let expected = if sees_private {
        StatusCode::OK
    } else {
        StatusCode::NOT_FOUND
    };
    assert_eq!(resp.status(), expected);
    Ok(())
}

/// A disabled account authenticates and is still refused with a `403` on
/// every catalog route, rather than served the public view.
#[rstest]
#[tokio::test]
async fn a_disabled_account_is_refused_a_catalog_read(
    #[values(
        "/benchmarks",
        "/benchmarks/prefill_throughput_256",
        "/api/v2/benchmarks",
        "/api/v2/benchmarks/prefill_throughput_256"
    )]
    route: &str,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, acme) = state_with_a_private_entry(dir.path()).await?;
    let (key, user_id, key_id) = registered_user(&state, &[]).await?;
    set_user_org(&state, &user_id, Some(acme.org_id)).await?;
    set_user_status(&state, &user_id, UserStatus::Disabled).await?;

    let resp = authed_user_get(&state, &key, &user_id, &key_id, route).await?;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    Ok(())
}

/// An identity a catalog read claims and cannot hold to.
#[derive(Clone, Copy, Debug)]
enum Claim {
    /// An `acme` device signing with a key that is not its own.
    WrongKey,
    /// An `acme` device an operator deleted, still signing with its key.
    DeletedDevice,
    /// A registered device's id, with no signing headers.
    DeviceIdAlone,
    /// A registered account's id, with no signing headers.
    AccountIdAlone,
    /// An `acme` member whose account is disabled, signing correctly.
    DisabledAccount,
}

/// What a [`Claim`] sends: a device or an account signing each request, or an
/// `X-Client-Id` and nothing else.
enum Claimant {
    Device(SigningKey, ClientId),
    Account(SigningKey, UserId, UserKeyId),
    IdAlone(String),
}

impl Claimant {
    /// The credential headers for a `GET` of `uri`.
    fn headers(&self, uri: &str) -> anyhow::Result<Vec<(&'static str, String)>> {
        Ok(match self {
            Self::Device(key, client_id) => auth_headers(key, client_id, "GET", uri)?,
            Self::Account(key, user_id, key_id) => {
                user_auth_headers(key, user_id, key_id, "GET", uri)?
            }
            Self::IdAlone(id) => vec![("X-Client-Id", id.clone())],
        })
    }
}

/// Set up the principal behind `claim`, with `acme` as the organization a
/// device or account belongs to where the claim has one.
async fn claimant(state: &AppState, acme: &Organization, claim: Claim) -> anyhow::Result<Claimant> {
    Ok(match claim {
        Claim::WrongKey => {
            let (_, client_id) = register_and_approve(state).await?;
            own_client_by_org(state, &client_id, &acme.org_id).await?;
            Claimant::Device(pipette_web_testkit::signing_key()?, client_id)
        }
        Claim::DeletedDevice => {
            let (key, client_id) = register_and_approve(state).await?;
            own_client_by_org(state, &client_id, &acme.org_id).await?;
            let ids = listed(authed_get(state, &key, &client_id, "/benchmarks").await?).await?;
            anyhow::ensure!(
                ids.iter().any(|id| id == PRIVATE),
                "the device reads its organization's entries until its record goes"
            );
            state.client_store.delete_client(&client_id).await?;
            Claimant::Device(key, client_id)
        }
        Claim::DeviceIdAlone => {
            let (_, client_id) = register_and_approve(state).await?;
            Claimant::IdAlone(client_id.to_string())
        }
        Claim::AccountIdAlone => {
            let (_, user_id, _) = registered_user(state, &[]).await?;
            Claimant::IdAlone(user_id.to_string())
        }
        Claim::DisabledAccount => {
            let (key, user_id, key_id) = registered_user(state, &[]).await?;
            set_user_org(state, &user_id, Some(acme.org_id.clone())).await?;
            set_user_status(state, &user_id, UserStatus::Disabled).await?;
            Claimant::Account(key, user_id, key_id)
        }
    })
}

/// A request that claims an identity is held to it on every catalog route: a
/// claim that does not hold is refused by name, not a quiet fall back to the
/// public catalog, so a misconfigured or retired caller learns it is one.
#[rstest]
#[case::wrong_key(Claim::WrongKey, StatusCode::UNAUTHORIZED, "invalid signature")]
#[case::deleted_device(Claim::DeletedDevice, StatusCode::UNAUTHORIZED, "client not found")]
#[case::device_id_alone(
    Claim::DeviceIdAlone,
    StatusCode::UNAUTHORIZED,
    "missing X-Timestamp header"
)]
#[case::account_id_alone(
    Claim::AccountIdAlone,
    StatusCode::UNAUTHORIZED,
    "missing X-User-Key-Id header"
)]
#[case::disabled_account(Claim::DisabledAccount, StatusCode::FORBIDDEN, "user is disabled")]
#[tokio::test]
async fn a_catalog_read_whose_claimed_identity_does_not_hold_is_refused(
    #[case] claim: Claim,
    #[case] status: StatusCode,
    #[case] error: &str,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, acme) = state_with_a_private_entry(dir.path()).await?;
    let claimant = claimant(&state, &acme, claim).await?;

    // A public id, so a fall back to the public view would be a `200`.
    for uri in [
        "/benchmarks",
        "/benchmarks/prefill_throughput_256",
        "/api/v2/benchmarks",
        "/api/v2/benchmarks/prefill_throughput_256",
    ] {
        let headers = claimant.headers(uri)?;
        let resp = authed_get_with_headers(&state, uri, &headers).await?;
        assert_eq!(resp.status(), status, "{claim:?} {uri}");
        assert_eq!(body_json(resp).await?["error"], error, "{claim:?} {uri}");
    }
    Ok(())
}

/// The `ETag` is over what the reader is served, so an anonymous listing's
/// tag does not `304` a member's request: a cache that revalidates on behalf
/// of a different principal gets the right body, not a stale one.
#[rstest]
#[tokio::test]
async fn the_etag_tells_the_two_views_apart(
    #[values("/benchmarks", "/api/v2/benchmarks")] route: &str,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, acme) = state_with_a_private_entry(dir.path()).await?;
    let (key, client_id) = register_and_approve(&state).await?;
    own_client_by_org(&state, &client_id, &acme.org_id).await?;

    let public = unauthed_get(&state, route).await?;
    let public_etag = public
        .headers()
        .get(header::ETAG)
        .ok_or_else(|| anyhow::anyhow!("missing etag"))?
        .to_str()?
        .to_string();
    let member = authed_get(&state, &key, &client_id, route).await?;
    let member_etag = member
        .headers()
        .get(header::ETAG)
        .ok_or_else(|| anyhow::anyhow!("missing etag"))?
        .to_str()?
        .to_string();
    assert_ne!(public_etag, member_etag);

    let mut headers = auth_headers(&key, &client_id, "GET", route)?;
    headers.push((header::IF_NONE_MATCH.as_str(), public_etag));
    let resp = authed_get_with_headers(&state, route, &headers).await?;
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "the public tag does not match the member's view"
    );
    Ok(())
}

fn etag(resp: &axum::http::Response<axum::body::Body>) -> anyhow::Result<String> {
    Ok(resp
        .headers()
        .get(header::ETAG)
        .ok_or_else(|| anyhow::anyhow!("missing etag header"))?
        .to_str()?
        .to_string())
}

/// A member's listing is the same body under the same `ETag` on every read, so
/// a cache revalidating it gets a `304`; served in map order, the private
/// entries each request reads into a fresh map would shuffle and defeat that.
#[tokio::test]
async fn a_members_listing_revalidates_under_a_stable_etag() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, acme) = state_with_a_private_entry(dir.path()).await?;
    setup_private_benchmark(dir.path(), "a_acme_first", &acme.org_id)?;
    setup_private_benchmark(dir.path(), "zz_acme_last", &acme.org_id)?;
    let (key, client_id) = register_and_approve(&state).await?;
    own_client_by_org(&state, &client_id, &acme.org_id).await?;

    let resp = authed_get(&state, &key, &client_id, "/benchmarks").await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let tag = etag(&resp)?;
    let body = body_json(resp).await?;
    assert_eq!(
        benchmark_ids(&body),
        [
            "a_acme_first",
            PRIVATE,
            "eval_test",
            "max_memory_usage_256",
            "prefill_throughput_256",
            "vl_throughput_384x384_32_64",
            "zz_acme_last",
        ],
        "three private entries among the public ones"
    );

    // Several rounds, since one fresh map can land in the first one's order.
    for round in 0..4 {
        let resp = authed_get(&state, &key, &client_id, "/benchmarks").await?;
        assert_eq!(resp.status(), StatusCode::OK, "round {round}");
        assert_eq!(etag(&resp)?, tag, "round {round}");
        assert_eq!(body_json(resp).await?, body, "round {round}");
    }
    let mut headers = auth_headers(&key, &client_id, "GET", "/benchmarks")?;
    headers.push((header::IF_NONE_MATCH.as_str(), tag.clone()));
    let resp = authed_get_with_headers(&state, "/benchmarks", &headers).await?;
    assert_eq!(resp.status(), StatusCode::NOT_MODIFIED);
    assert_eq!(etag(&resp)?, tag);
    Ok(())
}

/// The `type` filter applies to what the reader is served, so a member
/// filtering on the private entry's type gets it and nobody else does.
#[rstest]
#[tokio::test]
async fn the_type_filter_applies_after_visibility(
    #[values("/benchmarks", "/api/v2/benchmarks")] route: &str,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, acme) = state_with_a_private_entry(dir.path()).await?;
    let (key, client_id) = register_and_approve(&state).await?;
    own_client_by_org(&state, &client_id, &acme.org_id).await?;

    let path = format!("{route}?type=eval");
    let ids = listed(unauthed_get(&state, &path).await?).await?;
    assert_eq!(ids, vec!["eval_test".to_string()]);
    let ids = listed(authed_get(&state, &key, &client_id, &path).await?).await?;
    assert_eq!(ids, vec![PRIVATE.to_string(), "eval_test".to_string()]);
    Ok(())
}

/// `type` takes either spelling of a benchmark type, an empty value is no
/// filter, and a known type no entry has is an empty list rather than a `400`.
#[rstest]
#[case::snake_case("?type=vl_throughput", &["vl_throughput_384x384_32_64"])]
#[case::kebab_case("?type=vl-throughput", &["vl_throughput_384x384_32_64"])]
#[case::empty("?type=", &["eval_test", "max_memory_usage_256", "prefill_throughput_256", "vl_throughput_384x384_32_64"])]
#[case::none_of_that_type("?type=decode_throughput", &[])]
#[tokio::test]
async fn the_type_filter_reads_a_benchmark_type(
    #[case] query: &str,
    #[case] expected: &[&str],
    #[values("/benchmarks", "/api/v2/benchmarks")] route: &str,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;

    let resp = unauthed_get(&state, &format!("{route}{query}")).await?;
    assert_eq!(resp.status(), StatusCode::OK, "{query}");
    let body = body_json(resp).await?;
    assert!(body.is_array(), "{query}: {body}");
    assert_eq!(benchmark_ids(&body), expected, "{query}");
    Ok(())
}

/// A type no benchmark can have is a `400` that lists the ones there are, not an
/// empty catalog a caller would read as "none of that type".
#[rstest]
#[tokio::test]
async fn an_unknown_type_is_refused_with_the_known_types(
    #[values("/benchmarks", "/api/v2/benchmarks")] route: &str,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;

    let resp = unauthed_get(&state, &format!("{route}?type=prefill")).await?;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body = body_json(resp).await?;
    let error = body["error"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("a refusal carries an `error` string"))?;
    assert!(error.contains("'prefill'"), "{error}");
    for known in BenchmarkType::ALL {
        assert!(error.contains(&known.to_string()), "{known} in {error}");
    }
    Ok(())
}

/// `/api/v2` serves each entry as its id and the pair a submission declares, and
/// nothing else: no `parameter_*` fields, and no eval samples, so the
/// unreachable evals server the tests point at is never asked.
#[tokio::test]
async fn v2_serves_each_entry_as_its_declared_pair() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let eval = json!({
        "benchmark_id": "eval_test",
        "benchmark_type": "eval",
        "benchmark_params": {"eval_id": "test_eval", "dataset_name": "test_ds", "max_tokens": 100, "temperature": 0.0},
    });

    let resp = unauthed_get(&state, "/api/v2/benchmarks").await?;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        body_json(resp).await?,
        json!([
            eval,
            {
                "benchmark_id": "max_memory_usage_256",
                "benchmark_type": "max_memory_usage",
                "benchmark_params": {"prefill_tokens": 256},
            },
            {
                "benchmark_id": "prefill_throughput_256",
                "benchmark_type": "prefill_throughput",
                "benchmark_params": {"prefill_tokens": 256, "batch_size": 1},
            },
            {
                "benchmark_id": "vl_throughput_384x384_32_64",
                "benchmark_type": "vl_throughput",
                "benchmark_params": {"image_width": 384, "image_height": 384, "text_tokens": 32, "decode_tokens": 64, "num_images": 1},
            },
        ])
    );

    let resp = unauthed_get(&state, "/api/v2/benchmarks/eval_test").await?;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(body_json(resp).await?, eval);
    let resp = unauthed_get(&state, "/api/v2/benchmarks/does_not_exist").await?;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    Ok(())
}

/// `/api/v2` serves the entries `/benchmarks` does, by the same rule: a member
/// reads the private entry with its `org_id`, and to an outsider it is absent
/// from the listing and a `404` by id.
#[rstest]
#[case::member(true)]
#[case::outsider(false)]
#[tokio::test]
async fn v2_serves_private_entries_by_the_same_rule(#[case] member: bool) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, acme) = state_with_a_private_entry(dir.path()).await?;
    let (key, client_id) = register_and_approve(&state).await?;
    if member {
        own_client_by_org(&state, &client_id, &acme.org_id).await?;
    }

    let v1 = listed(authed_get(&state, &key, &client_id, "/benchmarks").await?).await?;
    let v2 = listed(authed_get(&state, &key, &client_id, "/api/v2/benchmarks").await?).await?;
    assert_eq!(v2, v1);
    assert_eq!(v2.iter().any(|id| id == PRIVATE), member);

    let resp = authed_get(
        &state,
        &key,
        &client_id,
        &format!("/api/v2/benchmarks/{PRIVATE}"),
    )
    .await?;
    let served = match resp.status() {
        StatusCode::OK => Some(body_json(resp).await?),
        StatusCode::NOT_FOUND => None,
        status => anyhow::bail!("unexpected {status}"),
    };
    let expected = member.then(|| {
        json!({
            "benchmark_id": PRIVATE,
            "benchmark_type": "eval",
            "benchmark_params": {"eval_id": "private_eval", "dataset_name": "private_ds", "max_tokens": 100, "temperature": 0.0},
            "org_id": acme.org_id.as_str(),
        })
    });
    assert_eq!(served, expected);
    Ok(())
}

/// Each entry carries every parameter its definition sets, sampling and image
/// count included, and only the corpus `dataset` is left out.
#[rstest]
#[case::decode(
    "decode_throughput_512_100",
    "decode_throughput",
    json!({"prefill_tokens": 512, "decode_tokens": 100, "batch_size": 1})
)]
#[case::end_to_end(
    "end_to_end_latency_512_256",
    "end_to_end_latency",
    json!({"prefill_tokens": 512, "decode_tokens": 256, "batch_size": 1})
)]
#[case::eval_sampling(
    "eval_test_temp1.0_topp0.95_topk64",
    "eval",
    json!({"eval_id": "test_eval", "dataset_name": "test_ds", "max_tokens": 100, "temperature": 1.0, "top_p": 0.95, "top_k": 64})
)]
#[case::vlm_eval_sampling(
    "vlm_eval_test_temp0.7_topp0.8_topk20",
    "vlm_eval",
    json!({"lmms_task": "test_task", "dataset_version": "2026.10.1", "sample_count": 3, "max_tokens": 100, "temperature": 0.7, "top_p": 0.8, "top_k": 20, "presence_penalty": 1.5, "repeat_penalty": 1.05})
)]
#[case::vl_max_memory(
    "vl_max_memory_512x384_2img",
    "vl_max_memory",
    json!({"image_width": 512, "image_height": 384, "text_tokens": 16, "num_images": 2})
)]
#[case::vl_spec_decode(
    "vl_spec_decode_corpus_k8_bs4",
    "vl_spec_decode",
    json!({"draft_length": 8, "max_tokens": 2048, "batch_size": 4, "temperature": 0.7})
)]
#[tokio::test]
async fn v2_serves_every_parameter_for_every_type(
    #[case] benchmark_id: &str,
    #[case] benchmark_type: &str,
    #[case] benchmark_params: serde_json::Value,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_scheduler_benchmarks(dir.path())?;
    let catalog = dir.path().join("benchmarks");
    std::fs::write(
        catalog.join("eval_test_temp1.0_topp0.95_topk64.toml"),
        r#"benchmark_type = "eval"
parameter_eval_id = "test_eval"
parameter_dataset_name = "test_ds"
parameter_max_tokens = 100
parameter_temperature = 1.0
parameter_top_p = 0.95
parameter_top_k = 64"#,
    )?;
    std::fs::write(
        catalog.join("vlm_eval_test_temp0.7_topp0.8_topk20.toml"),
        r#"benchmark_type = "vlm_eval"
parameter_lmms_task = "test_task"
parameter_dataset_version = "2026.10.1"
parameter_sample_count = 3
parameter_max_tokens = 100
parameter_temperature = 0.7
parameter_top_p = 0.8
parameter_top_k = 20
parameter_presence_penalty = 1.5
parameter_repeat_penalty = 1.05

[dataset]
format  = "vlm-eval-corpus/1"
adapter = "evals-server@1"
digest  = "1111111111111111111111111111111111111111111111111111111111111111"
bytes   = 2048

[dataset.upstream]
source          = "evals_server"
eval_id         = "test_task"
dataset_version = "2026.10.1"
identity        = "2222222222222222222222222222222222222222222222222222222222222222"

[recipe]
family  = "Test family"
models  = [{ repo = "org/repo" }]
sources = ["https://huggingface.co/org/repo"]"#,
    )?;
    std::fs::write(
        catalog.join("vl_max_memory_512x384_2img.toml"),
        r#"benchmark_type = "vl_max_memory"
parameter_image_width = 512
parameter_image_height = 384
parameter_text_tokens = 16
parameter_num_images = 2"#,
    )?;
    std::fs::write(
        catalog.join("vl_spec_decode_corpus_k8_bs4.toml"),
        r#"benchmark_type = "vl_spec_decode"
parameter_draft_length = 8
parameter_max_tokens = 2048
parameter_batch_size = 4
parameter_temperature = 0.7

[dataset]
format  = "vl-spec-decode-corpus/1"
adapter = "mmspec@1"
digest  = "1111111111111111111111111111111111111111111111111111111111111111"
bytes   = 1024

[dataset.upstream]
source   = "git"
url      = "github.com/acme/corpus"
revision = "0123456789abcdef0123456789abcdef01234567"
identity = "2923603292c7dd463ad01a119f57965817778fc3"
digest   = "2222222222222222222222222222222222222222222222222222222222222222""#,
    )?;
    let state = make_state(dir.path()).await?;
    let expected = json!({
        "benchmark_id": benchmark_id,
        "benchmark_type": benchmark_type,
        "benchmark_params": benchmark_params,
    });

    let resp = unauthed_get(&state, &format!("/api/v2/benchmarks/{benchmark_id}")).await?;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(body_json(resp).await?, expected);
    let listing = body_json(unauthed_get(&state, "/api/v2/benchmarks").await?).await?;
    assert!(
        listing
            .as_array()
            .is_some_and(|entries| entries.contains(&expected)),
        "{listing}"
    );
    Ok(())
}

/// The pair an entry carries is one `POST /benchmarks` accepts, copied as-is.
#[tokio::test]
async fn a_v2_pair_is_accepted_on_a_submission() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (key, client_id) = register_and_approve(&state).await?;

    let entry = body_json(
        authed_get(
            &state,
            &key,
            &client_id,
            "/api/v2/benchmarks/prefill_throughput_256",
        )
        .await?,
    )
    .await?;
    let mut body = valid_submission("prefill_throughput_256");
    body["benchmark_type"] = entry["benchmark_type"].clone();
    body["benchmark_params"] = entry["benchmark_params"].clone();

    let resp = authed_post(&state, &key, &client_id, "/benchmarks", &body).await?;
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    Ok(())
}

#[tokio::test]
async fn test_unauthed_client_can_list_benchmarks() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;

    let resp = unauthed_get(&state, "/benchmarks").await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await?;
    let benchmarks = body
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("expected benchmarks array"))?;
    assert_eq!(benchmarks.len(), 4);

    for bm in benchmarks {
        assert!(bm.get("benchmark_type").is_some());
        assert!(bm.get("type").is_none());
        assert!(bm.get("benchmark_id").is_some());
        assert!(bm.get("visibility").is_none());
    }
    Ok(())
}

#[rstest]
#[tokio::test]
async fn test_benchmark_list_honors_if_none_match(
    #[values("/benchmarks", "/api/v2/benchmarks")] route: &str,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;

    let resp = unauthed_get(&state, route).await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let etag = resp
        .headers()
        .get(header::ETAG)
        .ok_or_else(|| anyhow::anyhow!("missing etag header"))?
        .to_str()?
        .to_string();

    let resp =
        unauthed_get_with_headers(&state, route, &[(header::IF_NONE_MATCH.as_str(), etag)]).await?;
    assert_eq!(resp.status(), StatusCode::NOT_MODIFIED);
    assert!(resp.headers().get(header::ETAG).is_some());
    Ok(())
}

#[tokio::test]
async fn test_unauthed_client_can_get_benchmark() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;

    let resp = unauthed_get(&state, "/benchmarks/prefill_throughput_256").await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await?;
    assert_eq!(
        body["benchmark_id"].as_str(),
        Some("prefill_throughput_256")
    );
    assert_eq!(body["benchmark_type"].as_str(), Some("prefill_throughput"));
    assert!(body.get("visibility").is_none());
    Ok(())
}

#[rstest]
#[tokio::test]
async fn test_get_benchmark_honors_if_none_match(
    #[values(
        "/benchmarks/prefill_throughput_256",
        "/api/v2/benchmarks/prefill_throughput_256"
    )]
    route: &str,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;

    let resp = unauthed_get(&state, route).await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let etag = resp
        .headers()
        .get(header::ETAG)
        .ok_or_else(|| anyhow::anyhow!("missing etag header"))?
        .to_str()?
        .to_string();

    let resp =
        unauthed_get_with_headers(&state, route, &[(header::IF_NONE_MATCH.as_str(), etag)]).await?;
    assert_eq!(resp.status(), StatusCode::NOT_MODIFIED);
    assert!(resp.headers().get(header::ETAG).is_some());
    Ok(())
}

/// `If-None-Match` matches in each form a cache sends (`*`, a weak tag, a list
/// holding the tag) on both catalog routes; a tag that does not match, or a
/// value with non-ASCII bytes, still gets the body.
#[rstest]
#[case::any("*", true)]
#[case::weak("W/{etag}", true)]
#[case::in_a_list("\"x\", {etag}", true)]
#[case::another_tag("\"nope\"", false)]
#[case::not_ascii("\"\u{e9}\"", false)]
#[tokio::test]
async fn if_none_match_is_read_in_each_form_a_cache_sends(
    #[case] if_none_match: &str,
    #[case] matches: bool,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;

    for uri in ["/benchmarks", "/benchmarks/prefill_throughput_256"] {
        let resp = unauthed_get(&state, uri).await?;
        let tag = etag(&resp)?;
        let body = body_json(resp).await?;

        let value = if_none_match.replace("{etag}", &tag);
        let resp =
            unauthed_get_with_headers(&state, uri, &[(header::IF_NONE_MATCH.as_str(), value)])
                .await?;
        assert_eq!(etag(&resp)?, tag, "{uri}");
        if matches {
            assert_eq!(resp.status(), StatusCode::NOT_MODIFIED, "{uri}");
            let bytes = resp.into_body().collect().await?.to_bytes();
            assert!(bytes.is_empty(), "{uri}: a 304 carries no body");
        } else {
            assert_eq!(resp.status(), StatusCode::OK, "{uri}");
            assert_eq!(body_json(resp).await?, body, "{uri}");
        }
    }
    Ok(())
}

#[tokio::test]
async fn test_get_benchmark_404_when_missing() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;

    let resp = unauthed_get(&state, "/benchmarks/does_not_exist").await?;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    Ok(())
}

#[tokio::test]
async fn test_eval_benchmark_returns_502_when_upstream_unreachable() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;

    let resp = unauthed_get(&state, "/benchmarks/eval_test").await?;
    assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
    Ok(())
}

#[tokio::test]
async fn test_eval_benchmark_returns_502_on_malformed_upstream_response() -> anyhow::Result<()> {
    // `200` bodies the contract deserializer must reject: no `samples` field,
    // and a content part no device could send. A loop, as httpmock's `Rc`-based
    // mocks make an rstest body's future `!Send`.
    for upstream in [
        json!({"not_samples": []}),
        json!({"samples": [{"id": "s1", "messages": [
            {"role": "user", "content": [{"type": "input_video", "input_video": {"data": ""}}]},
        ]}]}),
    ] {
        let evals = MockServer::start_async().await;
        let samples = evals
            .mock_async(|when, then| {
                when.method(GET)
                    .path("/evals/test_eval/datasets/test_ds/samples");
                then.status(200).json_body(upstream);
            })
            .await;
        let dir = tempfile::tempdir()?;
        setup_benchmarks(dir.path())?;
        let state = make_state_with_evals_url(dir.path(), &evals.base_url()).await?;

        let resp = unauthed_get(&state, "/benchmarks/eval_test").await?;
        assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
        // A missed matcher answers 404, which would also map to a 502.
        samples.assert_async().await;
    }
    Ok(())
}

/// Without the status check, an error whose body parses would be a `200` with
/// no samples.
#[tokio::test]
async fn an_upstream_error_status_is_a_502_whatever_its_body() -> anyhow::Result<()> {
    // A loop, not an rstest table: httpmock's `Rc`-based mocks make an rstest
    // body's future `!Send`, which clippy denies.
    for upstream_status in [500, 404] {
        let evals = MockServer::start_async().await;
        let samples = evals
            .mock_async(|when, then| {
                when.method(GET)
                    .path("/evals/test_eval/datasets/test_ds/samples");
                then.status(upstream_status)
                    .json_body(json!({"samples": []}));
            })
            .await;
        let dir = tempfile::tempdir()?;
        setup_benchmarks(dir.path())?;
        let state = make_state_with_evals_url(dir.path(), &evals.base_url()).await?;

        let resp = unauthed_get(&state, "/benchmarks/eval_test").await?;
        assert_eq!(
            resp.status(),
            StatusCode::BAD_GATEWAY,
            "upstream {upstream_status}"
        );
        assert_eq!(
            body_json(resp).await?,
            json!({"error": "upstream request failed"}),
            "upstream {upstream_status}"
        );
        samples.assert_async().await;
    }
    Ok(())
}

#[tokio::test]
async fn test_eval_benchmark_embeds_the_upstream_samples() -> anyhow::Result<()> {
    // Two samples pin order and count; a vision message's parts pass through.
    let upstream = json!([
        {"id": "s1", "messages": [{"role": "user", "content": "What is 2+2?"}]},
        {"id": "s2", "messages": [
            {"role": "system", "content": "Answer briefly."},
            {"role": "user", "content": [
                {"type": "image_url", "image_url": {"url": "data:image/png;base64,iVBO"}},
                {"type": "text", "text": "Capital of France?"},
            ]},
        ]},
    ]);
    let evals = MockServer::start_async().await;
    let samples = evals
        .mock_async(|when, then| {
            when.method(GET)
                .path("/evals/test_eval/datasets/test_ds/samples");
            then.status(200).json_body(json!({"samples": upstream}));
        })
        .await;
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state_with_evals_url(dir.path(), &evals.base_url()).await?;

    let resp = unauthed_get(&state, "/benchmarks/eval_test").await?;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(body_json(resp).await?.get("samples"), Some(&upstream));
    samples.assert_async().await;
    Ok(())
}

/// A vlm_eval's samples are its corpus, not an inline proxy: the entry
/// carries the pin and the URL to download it from, and the evals server is
/// never asked, whatever profile the entry names.
#[tokio::test]
async fn a_vlm_eval_serves_its_corpus_pin_not_its_samples() -> anyhow::Result<()> {
    let evals = MockServer::start_async().await;
    let samples = evals
        .mock_async(|when, then| {
            when.method(GET).path_includes("/samples");
            then.status(200).json_body(json!({"samples": []}));
        })
        .await;
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let entries = [
        ("vlm_eval_mmstar", None),
        ("vlm_eval_mmstar_internvl", Some("internvl")),
    ];
    for (benchmark_id, profile) in entries {
        let (entry_line, upstream_line) = profile.map_or((String::new(), String::new()), |p| {
            (
                format!("parameter_prompt_profile = \"{p}\"\n"),
                format!("prompt_profile  = \"{p}\"\n"),
            )
        });
        std::fs::write(
            dir.path().join(format!("benchmarks/{benchmark_id}.toml")),
            format!(
                "benchmark_type = \"vlm_eval\"\nparameter_lmms_task = \"mmstar\"\n\
                 parameter_dataset_version = \"2026.10.1\"\nparameter_sample_count = 1\n\
                 parameter_max_tokens = 100\n{entry_line}\n\
                 [dataset]\nformat = \"vlm-eval-corpus/1\"\nadapter = \"evals-server@1\"\n\
                 digest = \"{}\"\nbytes = 2048\n\n\
                 [dataset.upstream]\nsource = \"evals_server\"\neval_id = \"mmstar\"\n\
                 dataset_version = \"2026.10.1\"\n{upstream_line}identity = \"{}\"\n\n\
                 [recipe]\nfamily = \"Test family\"\nmodels = [{{ repo = \"org/repo\" }}]\n\
                 sources = [\"https://huggingface.co/org/repo\"]\n",
                "1".repeat(64),
                "2".repeat(64),
            ),
        )?;
    }
    let state = make_state_with_evals_url(dir.path(), &evals.base_url()).await?;

    for (benchmark_id, profile) in entries {
        let resp = unauthed_get(&state, &format!("/benchmarks/{benchmark_id}")).await?;
        assert_eq!(resp.status(), StatusCode::OK, "{benchmark_id}");
        let body = body_json(resp).await?;
        assert_eq!(body.get("samples"), None, "{benchmark_id}");
        assert_eq!(
            body["dataset"]["url"],
            json!(format!("/datasets/{benchmark_id}")),
            "{benchmark_id}"
        );
        assert_eq!(body["dataset"]["format"], json!("vlm-eval-corpus/1"));
        assert_eq!(
            body["dataset"]["upstream"].get("prompt_profile"),
            profile.map(|p| json!(p)).as_ref(),
            "{benchmark_id}"
        );
    }
    samples.assert_calls_async(0).await;
    Ok(())
}

/// An asr_eval entry, org-private as every one is: Voxtral on FLEURS Dutch.
const ASR: &str = "asr_eval_fleurs_nl_nl_voxtral";

fn asr_toml() -> String {
    format!(
        "benchmark_type = \"asr_eval\"\nparameter_asr_task = \"asr_fleurs_nl_nl\"\n\
         parameter_dataset_version = \"2026.10.1\"\nparameter_sample_count = 364\n\
         parameter_prompt_profile = \"voxtral\"\nparameter_max_tokens = 500\n\
         parameter_temperature = 0.0\n{}",
        pipette_core::dataset::asr_eval_catalog_tables("asr_fleurs_nl_nl", "2026.10.1", "voxtral")
    )
}

/// An asr_eval's samples are its corpus, as a vlm_eval's are: its
/// organization's device reads the pin, the URL to download it from and the
/// recipe; the evals server is never asked, on a fetch or a revalidation; a
/// device outside the organization reads nothing; and the samples route the
/// clips were once paged from is gone.
#[rstest]
#[case::member(Some("acme"), StatusCode::OK)]
#[case::another_org(Some("labs"), StatusCode::NOT_FOUND)]
#[case::unowned(None, StatusCode::NOT_FOUND)]
fn an_asr_eval_serves_its_corpus_pin_not_its_samples(
    #[case] holder: Option<&'static str>,
    #[case] status: StatusCode,
) -> anyhow::Result<()> {
    pipette_web_testkit::run_mocked(asr_corpus_pin(holder, status))
}

async fn asr_corpus_pin(holder: Option<&str>, status: StatusCode) -> anyhow::Result<()> {
    let evals = MockServer::start_async().await;
    let samples = evals
        .mock_async(|when, then| {
            when.method(GET).path_includes("/samples");
            then.status(200).json_body(json!({"samples": []}));
        })
        .await;
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state_with_evals_url(dir.path(), &evals.base_url()).await?;
    let acme = seed_org(&state, "acme").await?;
    let org_dir = dir.path().join("org-benchmarks").join(acme.org_id.as_str());
    std::fs::create_dir_all(&org_dir)?;
    std::fs::write(org_dir.join(format!("{ASR}.toml")), asr_toml())?;
    let (key, client_id) = register_and_approve(&state).await?;
    if let Some(slug) = holder {
        let org_id = match slug {
            "acme" => acme.org_id.clone(),
            other => seed_org(&state, other).await?.org_id,
        };
        own_client_by_org(&state, &client_id, &org_id).await?;
    }

    let route = format!("/benchmarks/{ASR}");
    let resp = authed_get(&state, &key, &client_id, &route).await?;
    assert_eq!(resp.status(), status);
    if status == StatusCode::OK {
        let etag = resp
            .headers()
            .get(header::ETAG)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string)
            .ok_or_else(|| anyhow::anyhow!("no etag"))?;
        let body = body_json(resp).await?;
        assert_eq!(body.get("samples"), None);
        assert_eq!(body.pointer("/parameter_sample_count"), Some(&json!(364)));
        assert_eq!(
            body.pointer("/dataset/url"),
            Some(&json!(format!("/datasets/{ASR}")))
        );
        assert_eq!(
            body.pointer("/dataset/format"),
            Some(&json!("asr-eval-corpus/1"))
        );
        assert_eq!(
            body.pointer("/dataset/upstream/prompt_profile"),
            Some(&json!("voxtral"))
        );
        assert_eq!(body.pointer("/recipe/enable_thinking"), Some(&json!(false)));
        let mut headers = auth_headers(&key, &client_id, "GET", &route)?;
        headers.push((header::IF_NONE_MATCH.as_str(), etag));
        let resp = authed_get_with_headers(&state, &route, &headers).await?;
        assert_eq!(resp.status(), StatusCode::NOT_MODIFIED);
    }
    let resp = authed_get(
        &state,
        &key,
        &client_id,
        &format!("/benchmarks/{ASR}/samples?offset=0&limit=8"),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    samples.assert_calls_async(0).await;
    Ok(())
}
