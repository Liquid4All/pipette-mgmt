//! An organization's private benchmark catalog over HTTP: the CRUD under
//! `/api/v1/orgs/{org}/benchmarks`, and that what it writes is what
//! `GET /benchmarks` serves the organization's own principals on the next request.

use axum::http::StatusCode;
use futures::{StreamExt, TryStreamExt};
use rstest::rstest;
use serde_json::{Value, json};

use pipette_core::organization::Organization;
use pipette_httpapi::state::AppState;
use pipette_plan_types::{OrgId, UserPermission, UserStatus};
use pipette_web_testkit::*;

const PRIVATE: &str = "eval_acme_internal_v3";

fn a_definition() -> Value {
    json!({
        "benchmark_type": "eval",
        "parameter_eval_id": "acme_internal",
        "parameter_dataset_name": "v3",
        "parameter_max_tokens": 4096
    })
}

const CORPUS_DIGEST: &str = "1111111111111111111111111111111111111111111111111111111111111111";

/// A `vl_spec_decode` definition pinning the corpus whose artifact is `digest`.
fn a_corpus_pin(digest: &str) -> Value {
    json!({
        "benchmark_type": "vl_spec_decode",
        "parameter_draft_length": 8,
        "parameter_max_tokens": 2048,
        "dataset": {
            "format": "vl-spec-decode-corpus/1",
            "adapter": "mmspec@1",
            "digest": digest,
            "bytes": 1024,
            "subset": "GQA",
            "upstream": {
                "source": "git",
                "url": "github.com/acme/corpus",
                "revision": "0123456789abcdef0123456789abcdef01234567",
                "subpath": "dataset/test",
                "identity": "2923603292c7dd463ad01a119f57965817778fc3",
                "digest": "2222222222222222222222222222222222222222222222222222222222222222",
                "license": "Apache-2.0"
            }
        }
    })
}

/// The public catalog, an organization, and the URI its catalog lives at.
async fn setup() -> anyhow::Result<(tempfile::TempDir, AppState, Organization)> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let acme = seed_org(&state, "acme").await?;
    Ok((dir, state, acme))
}

/// An account holding `manage_orgs` and belonging to `org`: the organization's
/// own administrator.
async fn org_admin(state: &AppState, org: &Organization) -> anyhow::Result<UserSigner> {
    let signer = registered_user(state, &[UserPermission::ManageOrgs]).await?;
    set_user_org(state, &signer.1, Some(org.org_id.clone())).await?;
    Ok(signer)
}

fn entry_uri(org: &impl std::fmt::Display, benchmark_id: &str) -> String {
    format!("/api/v1/orgs/{org}/benchmarks/{benchmark_id}")
}

/// Write the fixture entry as `signer` and return the response.
async fn put_entry(
    state: &AppState,
    signer: &UserSigner,
    org: &OrgId,
) -> anyhow::Result<axum::http::Response<axum::body::Body>> {
    signed_json(
        state,
        signer,
        "PUT",
        &entry_uri(org, PRIVATE),
        &a_definition(),
    )
    .await
}

/// A member reads its organization's catalog by belonging, and a fleet
/// reader reads every one; only its administrators write to it, in good
/// standing. An administrator of another organization, and an unaffiliated
/// holder of the grant, find it absent on every verb; a pending account is
/// refused everything.
#[rstest]
#[case::fleet_reader(&[UserPermission::ReadAnyOrgs], None, UserStatus::Active, StatusCode::OK, StatusCode::FORBIDDEN)]
#[case::member_without_the_grant(&[], Some("acme"), UserStatus::Active, StatusCode::OK, StatusCode::FORBIDDEN)]
#[case::scoped_reader_of_another_org(&[UserPermission::ReadOrgs], Some("labs"), UserStatus::Active, StatusCode::NOT_FOUND, StatusCode::FORBIDDEN)]
#[case::admin_of_another_org(&[UserPermission::ManageOrgs], Some("labs"), UserStatus::Active, StatusCode::NOT_FOUND, StatusCode::NOT_FOUND)]
#[case::unaffiliated_admin(&[UserPermission::ManageOrgs], None, UserStatus::Active, StatusCode::NOT_FOUND, StatusCode::NOT_FOUND)]
#[case::pending_admin(&[UserPermission::ManageOrgs], Some("acme"), UserStatus::Pending, StatusCode::FORBIDDEN, StatusCode::FORBIDDEN)]
#[tokio::test]
async fn members_read_and_administrators_write(
    #[case] permissions: &[UserPermission],
    #[case] member_of: Option<&str>,
    #[case] status_of_signer: UserStatus,
    #[case] read: StatusCode,
    #[case] write: StatusCode,
) -> anyhow::Result<()> {
    let (_dir, state, acme) = setup().await?;
    let signer = registered_user(&state, permissions).await?;
    set_user_status(&state, &signer.1, status_of_signer).await?;
    if let Some(slug) = member_of {
        let org = if slug == "acme" {
            acme.clone()
        } else {
            seed_org(&state, slug).await?
        };
        set_user_org(&state, &signer.1, Some(org.org_id)).await?;
    }

    // Written by the organization's own administrator, so a read that is
    // admitted has something to find.
    let admin = org_admin(&state, &acme).await?;
    assert_eq!(
        put_entry(&state, &admin, &acme.org_id).await?.status(),
        StatusCode::OK
    );

    let list = signed_empty(&state, &signer, "GET", "/api/v1/orgs/acme/benchmarks").await?;
    assert_eq!(list.status(), read);
    let get = signed_empty(&state, &signer, "GET", &entry_uri(&"acme", PRIVATE)).await?;
    assert_eq!(get.status(), read);
    let put = put_entry(&state, &signer, &acme.org_id).await?;
    assert_eq!(put.status(), write);
    let delete = signed_empty(&state, &signer, "DELETE", &entry_uri(&acme.org_id, PRIVATE)).await?;
    assert_eq!(delete.status(), write);
    Ok(())
}

/// The catalog routes are an account's, so even the organization's own device
/// is refused by its id's prefix on every verb; were a route to take a device
/// signature, the device could read or rewrite the catalog.
#[rstest]
#[case::list("GET", false)]
#[case::get("GET", true)]
#[case::put("PUT", true)]
#[case::delete("DELETE", true)]
#[tokio::test]
async fn a_device_signature_reaches_no_catalog_route(
    #[case] method: &str,
    #[case] entry: bool,
) -> anyhow::Result<()> {
    let (_dir, state, acme) = setup().await?;
    let admin = org_admin(&state, &acme).await?;
    assert_eq!(
        put_entry(&state, &admin, &acme.org_id).await?.status(),
        StatusCode::OK
    );
    let (device_key, device) = register_and_approve(&state).await?;
    own_client_by_org(&state, &device, &acme.org_id).await?;

    let uri = if entry {
        entry_uri(&acme.org_id, PRIVATE)
    } else {
        "/api/v1/orgs/acme/benchmarks".to_string()
    };
    let headers = auth_headers(&device_key, &device, method, &uri)?;
    let resp = if method == "PUT" {
        let mut revised = a_definition();
        revised["parameter_max_tokens"] = json!(2048);
        json_request(&state, method, &uri, &headers, &revised).await?
    } else {
        request_with_headers(&state, method, &uri, &headers).await?
    };
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(
        json_str(&body_json(resp).await?, "error")?,
        "invalid X-Client-Id header: user id must start with `user_`"
    );

    let shown =
        body_json(signed_empty(&state, &admin, "GET", &entry_uri(&"acme", PRIVATE)).await?).await?;
    assert_eq!(
        shown["parameter_max_tokens"], 4096,
        "neither replaced nor deleted"
    );
    Ok(())
}

/// The organization's own administrator, and a server admin, each drive the
/// whole lifecycle, reading by handle or by id and writing by id. What comes
/// back carries the organization, and the listing reflects each write.
#[rstest]
#[case::org_admin_by_handle(false, true)]
#[case::server_admin_by_id(true, false)]
#[tokio::test]
async fn an_administrator_drives_the_lifecycle(
    #[case] server_admin: bool,
    #[case] by_handle: bool,
) -> anyhow::Result<()> {
    let (_dir, state, acme) = setup().await?;
    let signer = if server_admin {
        registered_user(&state, &[UserPermission::ManageAnyOrgs]).await?
    } else {
        org_admin(&state, &acme).await?
    };
    let org = if by_handle {
        "acme".to_string()
    } else {
        acme.org_id.to_string()
    };

    let empty = body_json(
        signed_empty(
            &state,
            &signer,
            "GET",
            &format!("/api/v1/orgs/{org}/benchmarks"),
        )
        .await?,
    )
    .await?;
    assert_eq!(empty["org_id"], acme.org_id.as_str());
    assert_eq!(empty["benchmarks"], json!([]));
    assert_eq!(empty["unreadable_benchmark_ids"], json!([]));

    let resp = put_entry(&state, &signer, &acme.org_id).await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let written = body_json(resp).await?;
    assert_eq!(written["benchmark_id"], PRIVATE);
    assert_eq!(written["org_id"], acme.org_id.as_str());
    assert_eq!(written["parameter_max_tokens"], 4096);

    let shown =
        body_json(signed_empty(&state, &signer, "GET", &entry_uri(&org, PRIVATE)).await?).await?;
    assert_eq!(shown, written);
    let listed = body_json(
        signed_empty(
            &state,
            &signer,
            "GET",
            &format!("/api/v1/orgs/{org}/benchmarks"),
        )
        .await?,
    )
    .await?;
    assert_eq!(listed["benchmarks"], json!([written]));

    // A second put replaces.
    let mut revised = a_definition();
    revised["parameter_max_tokens"] = json!(2048);
    let resp = signed_json(
        &state,
        &signer,
        "PUT",
        &entry_uri(&acme.org_id, PRIVATE),
        &revised,
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let shown =
        body_json(signed_empty(&state, &signer, "GET", &entry_uri(&org, PRIVATE)).await?).await?;
    assert_eq!(shown["parameter_max_tokens"], 2048);

    let resp = signed_empty(&state, &signer, "DELETE", &entry_uri(&acme.org_id, PRIVATE)).await?;
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    let resp = signed_empty(&state, &signer, "GET", &entry_uri(&org, PRIVATE)).await?;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    let resp = signed_empty(&state, &signer, "DELETE", &entry_uri(&acme.org_id, PRIVATE)).await?;
    assert_eq!(
        resp.status(),
        StatusCode::NOT_FOUND,
        "a delete names what is missing"
    );
    Ok(())
}

/// A body that a catalog file could not carry is refused before anything is
/// written: an unknown key, a missing parameter, an unknown type, and an id
/// in the body (the path carries it).
#[rstest]
#[case::unknown_key(json!({"benchmark_type": "prefill_throughput", "parameter_prefill_tokens": 256, "nope": 1}))]
#[case::missing_parameter(json!({"benchmark_type": "prefill_throughput"}))]
#[case::id_in_the_body(json!({"benchmark_id": "x", "benchmark_type": "prefill_throughput", "parameter_prefill_tokens": 256}))]
#[case::unknown_type(json!({"benchmark_type": "nope"}))]
#[tokio::test]
async fn a_definition_a_file_could_not_carry_is_refused(#[case] body: Value) -> anyhow::Result<()> {
    let (_dir, state, acme) = setup().await?;
    let signer = org_admin(&state, &acme).await?;

    let resp = signed_json(
        &state,
        &signer,
        "PUT",
        &entry_uri(&acme.org_id, PRIVATE),
        &body,
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let listed =
        body_json(signed_empty(&state, &signer, "GET", "/api/v1/orgs/acme/benchmarks").await?)
            .await?;
    assert_eq!(listed["benchmarks"], json!([]), "nothing was written");
    Ok(())
}

/// The server fetches a pin's upstream on every probe and build, so an
/// administrator cannot aim it at a host off the public allowlist.
#[rstest]
#[case::internal_host("gitlab.internal/acme/corpus")]
#[case::metadata_ip("169.254.169.254/latest/meta-data")]
#[case::port("github.com:8443/acme/corpus")]
#[case::userinfo("github.com@10.0.0.1/acme/corpus")]
#[tokio::test]
async fn a_corpus_pin_off_the_host_allowlist_is_refused(#[case] url: &str) -> anyhow::Result<()> {
    let (_dir, state, acme) = setup().await?;
    let signer = org_admin(&state, &acme).await?;
    let mut definition = a_corpus_pin(CORPUS_DIGEST);
    definition["dataset"]["upstream"]["url"] = json!(url);

    let uri = entry_uri(&acme.org_id, "vl_spec_decode_acme_k8");
    let resp = signed_json(&state, &signer, "PUT", &uri, &definition).await?;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let listed =
        body_json(signed_empty(&state, &signer, "GET", "/api/v1/orgs/acme/benchmarks").await?)
            .await?;
    assert_eq!(listed["benchmarks"], json!([]), "nothing was written");
    Ok(())
}

/// A count the client could not run, or one past the Int32 warehouse column,
/// is refused at the write rather than failing `POST /benchmarks` with a `500`.
/// The full range matrix is `pipette-core`'s; these prove the route applies it.
#[rstest]
#[case::zero(
    json!({"benchmark_type": "eval", "parameter_eval_id": "acme_internal", "parameter_dataset_name": "v3", "parameter_max_tokens": 0}),
    "PositiveCount is too small"
)]
#[case::zero_batch_size(
    json!({"benchmark_type": "prefill_throughput", "parameter_prefill_tokens": 256, "parameter_batch_size": 0}),
    "BatchSize is too small"
)]
#[case::past_the_warehouse_column(
    json!({"benchmark_type": "prefill_throughput", "parameter_prefill_tokens": 2_147_483_648_u32}),
    "integer `2147483648`, expected i32"
)]
#[tokio::test]
async fn a_count_out_of_range_is_refused(
    #[case] body: Value,
    #[case] reason: &str,
) -> anyhow::Result<()> {
    let (_dir, state, acme) = setup().await?;
    let signer = org_admin(&state, &acme).await?;

    let resp = signed_json(
        &state,
        &signer,
        "PUT",
        &entry_uri(&acme.org_id, PRIVATE),
        &body,
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let err = body_json(resp).await?;
    let err = json_str(&err, "error")?;
    assert!(err.contains(reason), "refused as {err:?}");
    let listed =
        body_json(signed_empty(&state, &signer, "GET", "/api/v1/orgs/acme/benchmarks").await?)
            .await?;
    assert_eq!(listed["benchmarks"], json!([]), "nothing was written");
    Ok(())
}

/// Entries an older server wrote before the count bounds held are not fatal:
/// a private one is listed as unreadable, a public one is skipped from startup
/// on, and the rest of each catalog is still served.
#[tokio::test]
async fn a_count_stored_before_the_bounds_is_unreadable_not_fatal() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    std::fs::write(
        dir.path().join("benchmarks").join("eval_public_zero.toml"),
        ACME_EVAL_TOML.replace("4096", "0"),
    )?;
    let state = make_state(dir.path()).await?;
    let acme = seed_org(&state, "acme").await?;
    let signer = org_admin(&state, &acme).await?;
    assert_eq!(
        put_entry(&state, &signer, &acme.org_id).await?.status(),
        StatusCode::OK
    );
    let stored = [
        ("eval_acme_zero", ACME_EVAL_TOML.replace("4096", "0")),
        ("eval_acme_negative", ACME_EVAL_TOML.replace("4096", "-1")),
        (
            "eval_acme_huge",
            ACME_EVAL_TOML.replace("4096", "2147483648"),
        ),
    ];
    stored
        .iter()
        .try_for_each(|(id, toml)| write_private_entry(dir.path(), &acme.org_id, id, toml))?;

    let listed =
        body_json(signed_empty(&state, &signer, "GET", "/api/v1/orgs/acme/benchmarks").await?)
            .await?;
    assert_eq!(benchmark_ids(&listed["benchmarks"]), vec![PRIVATE]);
    assert_eq!(
        listed["unreadable_benchmark_ids"],
        json!(["eval_acme_huge", "eval_acme_negative", "eval_acme_zero"])
    );

    let (key, user_id, key_id) = &signer;
    let resp = authed_user_get(&state, key, user_id, key_id, "/benchmarks").await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let served = benchmark_ids(&body_json(resp).await?);
    assert!(served.iter().any(|id| id == PRIVATE), "{served:?}");
    assert!(
        served.iter().any(|id| id == "prefill_throughput_256"),
        "{served:?}"
    );
    assert!(
        !served
            .iter()
            .any(|id| id == "eval_public_zero" || stored.iter().any(|(bad, _)| id == bad)),
        "{served:?}"
    );
    Ok(())
}

/// A signed `PUT` of `body` under `content_type`, or under none.
async fn put_raw(
    state: &AppState,
    signer: &UserSigner,
    uri: &str,
    content_type: Option<&str>,
    body: &str,
) -> anyhow::Result<axum::http::Response<axum::body::Body>> {
    let (key, user_id, key_id) = signer;
    let headers = user_auth_headers(key, user_id, key_id, "PUT", uri)?;
    raw_request(state, "PUT", uri, &headers, content_type, body.to_string()).await
}

/// The catalog file is a body as it stands: an entry written from its TOML
/// reads back exactly as the same definition written as JSON.
#[tokio::test]
async fn the_catalog_file_is_a_body() -> anyhow::Result<()> {
    let (_dir, state, acme) = setup().await?;
    let signer = org_admin(&state, &acme).await?;
    let uri = entry_uri(&acme.org_id, PRIVATE);

    let resp = put_raw(
        &state,
        &signer,
        &uri,
        Some("application/toml; charset=utf-8"),
        ACME_EVAL_TOML,
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let from_toml = body_json(resp).await?;
    assert_eq!(from_toml["parameter_max_tokens"], 4096);
    let shown = body_json(signed_empty(&state, &signer, "GET", &uri).await?).await?;
    assert_eq!(shown, from_toml);

    let resp = put_entry(&state, &signer, &acme.org_id).await?;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        body_json(resp).await?,
        from_toml,
        "one entry, whichever form wrote it"
    );
    Ok(())
}

/// An `application/*+json` type is a JSON definition: if our media-type check
/// and the JSON extractor's drifted apart, this write would be refused by its
/// type.
#[tokio::test]
async fn a_json_suffix_type_is_a_json_definition() -> anyhow::Result<()> {
    let (_dir, state, acme) = setup().await?;
    let signer = org_admin(&state, &acme).await?;
    let uri = entry_uri(&acme.org_id, PRIVATE);

    let resp = put_raw(
        &state,
        &signer,
        &uri,
        Some("application/vnd.pipette+json"),
        &a_definition().to_string(),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let from_suffix = body_json(resp).await?;
    assert_eq!(from_suffix["benchmark_id"], PRIVATE);
    assert_eq!(from_suffix["parameter_max_tokens"], 4096);
    let shown = body_json(signed_empty(&state, &signer, "GET", &uri).await?).await?;
    assert_eq!(shown, from_suffix);

    let resp = put_entry(&state, &signer, &acme.org_id).await?;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(body_json(resp).await?, from_suffix);
    Ok(())
}

/// A TOML body is held to the checks a JSON one is, and a body in neither form
/// is refused by its type. Nothing is written either way.
#[rstest]
#[case::unknown_key(
    Some("application/toml"),
    "benchmark_type = \"prefill_throughput\"\nparameter_prefill_tokens = 256\nnope = 1\n",
    StatusCode::BAD_REQUEST
)]
#[case::id_in_the_body(
    Some("application/toml"),
    "benchmark_id = \"x\"\nbenchmark_type = \"prefill_throughput\"\nparameter_prefill_tokens = 256\n",
    StatusCode::BAD_REQUEST
)]
#[case::not_toml(Some("application/toml"), "benchmark_type = ", StatusCode::BAD_REQUEST)]
#[case::plain_text(Some("text/plain"), ACME_EVAL_TOML, StatusCode::UNSUPPORTED_MEDIA_TYPE)]
#[case::untyped(None, ACME_EVAL_TOML, StatusCode::UNSUPPORTED_MEDIA_TYPE)]
#[tokio::test]
async fn a_body_a_catalog_file_could_not_be_is_refused(
    #[case] content_type: Option<&str>,
    #[case] body: &str,
    #[case] status: StatusCode,
) -> anyhow::Result<()> {
    let (_dir, state, acme) = setup().await?;
    let signer = org_admin(&state, &acme).await?;

    let resp = put_raw(
        &state,
        &signer,
        &entry_uri(&acme.org_id, PRIVATE),
        content_type,
        body,
    )
    .await?;
    assert_eq!(resp.status(), status);
    let listed =
        body_json(signed_empty(&state, &signer, "GET", "/api/v1/orgs/acme/benchmarks").await?)
            .await?;
    assert_eq!(listed["benchmarks"], json!([]), "nothing was written");
    Ok(())
}

/// A repeated key is refused in either form, rather than one copy silently
/// winning, and nothing is written.
#[rstest]
#[case::json(
    "application/json",
    r#"{"benchmark_type": "eval", "parameter_eval_id": "acme_internal", "parameter_dataset_name": "v3", "parameter_max_tokens": 4096, "parameter_max_tokens": 8192}"#
)]
#[case::toml("application/toml", &format!("{ACME_EVAL_TOML}parameter_max_tokens = 8192\n"))]
#[tokio::test]
async fn a_repeated_key_is_refused(
    #[case] content_type: &str,
    #[case] body: &str,
) -> anyhow::Result<()> {
    let (_dir, state, acme) = setup().await?;
    let signer = org_admin(&state, &acme).await?;

    let uri = entry_uri(&acme.org_id, PRIVATE);
    let resp = put_raw(&state, &signer, &uri, Some(content_type), body).await?;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let err = body_json(resp).await?;
    let err = json_str(&err, "error")?;
    assert!(
        err.contains("duplicate") && err.contains("parameter_max_tokens"),
        "refused as {err:?}"
    );
    let listed =
        body_json(signed_empty(&state, &signer, "GET", "/api/v1/orgs/acme/benchmarks").await?)
            .await?;
    assert_eq!(listed["benchmarks"], json!([]), "nothing was written");
    Ok(())
}

/// A TOML body that is not UTF-8 is the caller's `400` and nothing is written,
/// though its only bad bytes sit in a comment: read lossily, it would be a
/// sound definition and be stored.
#[tokio::test]
async fn a_toml_body_that_is_not_utf8_is_refused() -> anyhow::Result<()> {
    let (_dir, state, acme) = setup().await?;
    let signer = org_admin(&state, &acme).await?;
    let uri = entry_uri(&acme.org_id, PRIVATE);

    let (key, user_id, key_id) = &signer;
    let headers = user_auth_headers(key, user_id, key_id, "PUT", &uri)?;
    let resp = raw_request(
        &state,
        "PUT",
        &uri,
        &headers,
        Some("application/toml"),
        [ACME_EVAL_TOML.as_bytes(), b"# \xff\xfe\n"].concat(),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let error = body_json(resp).await?;
    let message = json_str(&error, "error")?;
    assert!(message.to_ascii_lowercase().contains("utf-8"), "{message}");
    let listed =
        body_json(signed_empty(&state, &signer, "GET", "/api/v1/orgs/acme/benchmarks").await?)
            .await?;
    assert_eq!(listed["benchmarks"], json!([]), "nothing was written");
    Ok(())
}

/// A body is read only for a caller the write admits, in either form: another
/// organization's administrator gets the `404` its catalog answers, not a
/// `400` about a body nobody would have written.
#[rstest]
#[case::toml("application/toml", "not toml =")]
#[case::json("application/json", "{")]
#[tokio::test]
async fn a_refused_caller_gets_the_refusal_not_the_parse(
    #[case] content_type: &str,
    #[case] body: &str,
) -> anyhow::Result<()> {
    let (_dir, state, acme) = setup().await?;
    let labs = seed_org(&state, "labs").await?;
    let outsider = org_admin(&state, &labs).await?;

    let uri = entry_uri(&acme.org_id, PRIVATE);
    let resp = put_raw(&state, &outsider, &uri, Some(content_type), body).await?;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    Ok(())
}

/// The one definition with nested tables survives the JSON body, the TOML
/// the store holds, and the JSON served back, field for field: a corpus pin
/// is exactly the shape a client verifies a download against, so a field
/// lost on the way through would be a benchmark that cannot run.
#[tokio::test]
async fn a_corpus_pin_round_trips_through_the_store() -> anyhow::Result<()> {
    let (_dir, state, acme) = setup().await?;
    let signer = org_admin(&state, &acme).await?;
    let definition = a_corpus_pin(CORPUS_DIGEST);
    let uri = entry_uri(&acme.org_id, "vl_spec_decode_acme_k8");
    let resp = signed_json(&state, &signer, "PUT", &uri, &definition).await?;
    assert_eq!(resp.status(), StatusCode::OK);

    let mut expected = definition.clone();
    expected["benchmark_id"] = json!("vl_spec_decode_acme_k8");
    expected["org_id"] = json!(acme.org_id.as_str());
    // The defaults the catalog fills in, served as a public entry's are.
    expected["parameter_batch_size"] = json!(1);
    expected["parameter_temperature"] = json!(0.0);
    let shown = body_json(signed_empty(&state, &signer, "GET", &uri).await?).await?;
    assert_eq!(shown, expected);

    // Served to the organization's device with the corpus URL attached, as a
    // public one would be.
    let (key, device) = register_and_approve(&state).await?;
    own_client_by_org(&state, &device, &acme.org_id).await?;
    let served =
        body_json(authed_get(&state, &key, &device, "/benchmarks/vl_spec_decode_acme_k8").await?)
            .await?;
    expected["dataset"]["url"] = json!("/datasets/vl_spec_decode_acme_k8");
    assert_eq!(served, expected);
    Ok(())
}

/// `dataset.url` is the server's field: one in the body is not stored, so
/// neither the organization's reads nor the listing serve a caller's corpus
/// URL, and the detail route still stamps its own.
#[tokio::test]
async fn a_corpus_url_in_the_body_is_not_stored() -> anyhow::Result<()> {
    const ID: &str = "vl_spec_decode_acme_k8";
    let (dir, state, acme) = setup().await?;
    let signer = org_admin(&state, &acme).await?;
    let mut definition = a_corpus_pin(CORPUS_DIGEST);
    definition["dataset"]["url"] = json!("https://evil.example/x.tar");
    let uri = entry_uri(&acme.org_id, ID);
    let resp = signed_json(&state, &signer, "PUT", &uri, &definition).await?;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(body_json(resp).await?["dataset"].get("url"), None);
    let stored = std::fs::read_to_string(
        dir.path()
            .join("org-benchmarks")
            .join(acme.org_id.as_str())
            .join(format!("{ID}.toml")),
    )?;
    assert!(!stored.contains("evil.example"), "{stored}");
    let resp = signed_empty(&state, &signer, "GET", &uri).await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let shown = body_json(resp).await?;
    assert_eq!(shown["dataset"]["digest"], CORPUS_DIGEST);
    assert_eq!(shown["dataset"].get("url"), None);

    let (key, user_id, key_id) = &signer;
    let listed =
        body_json(authed_user_get(&state, key, user_id, key_id, "/benchmarks").await?).await?;
    let row = listed
        .as_array()
        .and_then(|rows| rows.iter().find(|row| row["benchmark_id"] == ID))
        .ok_or_else(|| anyhow::anyhow!("the member is served its entry"))?;
    assert_eq!(row["dataset"].get("url"), None);
    let served = body_json(
        authed_user_get(&state, key, user_id, key_id, &format!("/benchmarks/{ID}")).await?,
    )
    .await?;
    assert_eq!(served["dataset"]["url"], format!("/datasets/{ID}"));
    Ok(())
}

/// An entry the store cannot read is not among a listing's `benchmarks` but is
/// named in its `unreadable_benchmark_ids`, and is a `500` by id rather than a `404`: it is
/// there, and the fault is the server's. What `GET /benchmarks` serves the
/// organization skips it. It still holds its id, so `DELETE` removes it.
#[tokio::test]
async fn an_unreadable_entry_is_reported_in_a_listing_and_named_by_id() -> anyhow::Result<()> {
    let (dir, state, acme) = setup().await?;
    let signer = org_admin(&state, &acme).await?;
    assert_eq!(
        put_entry(&state, &signer, &acme.org_id).await?.status(),
        StatusCode::OK
    );
    write_private_entry(
        dir.path(),
        &acme.org_id,
        "eval_broken",
        "benchmark_type = \"nope\"",
    )?;

    let listed =
        body_json(signed_empty(&state, &signer, "GET", "/api/v1/orgs/acme/benchmarks").await?)
            .await?;
    assert_eq!(benchmark_ids(&listed["benchmarks"]), vec![PRIVATE]);
    assert_eq!(listed["unreadable_benchmark_ids"], json!(["eval_broken"]));
    let resp = signed_empty(&state, &signer, "GET", &entry_uri(&"acme", "eval_broken")).await?;
    assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);

    let (key, user_id, key_id) = &signer;
    let served = benchmark_ids(
        &body_json(authed_user_get(&state, key, user_id, key_id, "/benchmarks").await?).await?,
    );
    assert!(served.iter().any(|id| id == PRIVATE));
    assert!(!served.iter().any(|id| id == "eval_broken"));

    let uri = entry_uri(&acme.org_id, "eval_broken");
    let statuses: Vec<StatusCode> = futures::stream::iter(0..2)
        .then(|_| signed_empty(&state, &signer, "DELETE", &uri))
        .map_ok(|resp| resp.status())
        .try_collect()
        .await?;
    assert_eq!(statuses, [StatusCode::NO_CONTENT, StatusCode::NOT_FOUND]);
    let listed =
        body_json(signed_empty(&state, &signer, "GET", "/api/v1/orgs/acme/benchmarks").await?)
            .await?;
    assert_eq!(listed["unreadable_benchmark_ids"], json!([]));
    Ok(())
}

/// `benchmarks` and `unreadable_benchmark_ids` are each served sorted by id, whatever order
/// the entries were written in or the store lists them; without the sort a
/// listing could reorder between reads.
#[tokio::test]
async fn a_listing_serves_each_array_sorted_by_id() -> anyhow::Result<()> {
    let (dir, state, acme) = setup().await?;
    let signer = org_admin(&state, &acme).await?;
    // Neither in id order nor its reverse, so a creation-ordered store lists
    // them unsorted too.
    for id in ["eval_acme_c", "eval_acme_a", "eval_acme_d", "eval_acme_b"] {
        let resp = signed_json(
            &state,
            &signer,
            "PUT",
            &entry_uri(&acme.org_id, id),
            &a_definition(),
        )
        .await?;
        assert_eq!(resp.status(), StatusCode::OK, "{id}");
    }
    for id in [
        "eval_acme_z_broken",
        "eval_acme_bb_broken",
        "eval_acme_m_broken",
    ] {
        write_private_entry(dir.path(), &acme.org_id, id, "benchmark_type = \"nope\"")?;
    }

    let listed =
        body_json(signed_empty(&state, &signer, "GET", "/api/v1/orgs/acme/benchmarks").await?)
            .await?;
    let served: Vec<&str> = listed["benchmarks"]
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("benchmarks is an array"))?
        .iter()
        .filter_map(|row| row["benchmark_id"].as_str())
        .collect();
    assert_eq!(
        served,
        ["eval_acme_a", "eval_acme_b", "eval_acme_c", "eval_acme_d"]
    );
    assert_eq!(
        listed["unreadable_benchmark_ids"],
        json!([
            "eval_acme_bb_broken",
            "eval_acme_m_broken",
            "eval_acme_z_broken"
        ])
    );
    Ok(())
}

/// A read that fails, as opposed to an entry that does not parse, fails the
/// listing with a `500` rather than naming the entry unreadable: the fault is
/// the server's and the entry may be sound.
#[tokio::test]
async fn a_failed_read_fails_the_listing() -> anyhow::Result<()> {
    let (dir, state, acme) = setup().await?;
    let signer = org_admin(&state, &acme).await?;
    assert_eq!(
        put_entry(&state, &signer, &acme.org_id).await?.status(),
        StatusCode::OK
    );
    // A directory under an entry's key lists as an id but cannot be read.
    std::fs::create_dir_all(
        dir.path()
            .join("org-benchmarks")
            .join(acme.org_id.as_str())
            .join("eval_unreachable.toml"),
    )?;

    let resp = signed_empty(&state, &signer, "GET", "/api/v1/orgs/acme/benchmarks").await?;
    assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(
        json_str(&body_json(resp).await?, "error")?,
        "internal server error"
    );
    Ok(())
}

/// At `GET /benchmarks/{id}` an entry the organization keeps but the store
/// cannot read is a `500` for the organization's account and device, as it is
/// on the organization's own route; anyone else gets the `404` an unknown id draws.
#[tokio::test]
async fn an_unreadable_entry_is_a_fault_to_its_organization_and_absent_to_others()
-> anyhow::Result<()> {
    const BROKEN: &str = "eval_broken";
    let (dir, state, acme) = setup().await?;
    // Membership alone reaches the entry; no grant is involved.
    let member = registered_user(&state, &[]).await?;
    set_user_org(&state, &member.1, Some(acme.org_id.clone())).await?;
    let (device_key, device) = register_and_approve(&state).await?;
    own_client_by_org(&state, &device, &acme.org_id).await?;
    let (stranger_key, stranger) = register_and_approve(&state).await?;
    write_private_entry(
        dir.path(),
        &acme.org_id,
        BROKEN,
        "benchmark_type = \"nope\"",
    )?;
    let uri = format!("/benchmarks/{BROKEN}");

    let (key, user_id, key_id) = &member;
    let by_member = authed_user_get(&state, key, user_id, key_id, &uri).await?;
    let by_device = authed_get(&state, &device_key, &device, &uri).await?;
    let corpus_by_device =
        authed_get(&state, &device_key, &device, &format!("/datasets/{BROKEN}")).await?;
    for resp in [by_member, by_device, corpus_by_device] {
        assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(
            json_str(&body_json(resp).await?, "error")?,
            "internal server error"
        );
    }

    let by_stranger = authed_get(&state, &stranger_key, &stranger, &uri).await?;
    let unsigned = unauthed_get(&state, &uri).await?;
    for resp in [by_stranger, unsigned] {
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        assert_eq!(
            json_str(&body_json(resp).await?, "error")?,
            "benchmark not found"
        );
    }
    Ok(())
}

/// The public catalog wins at serve time, so an entry under one of its ids is
/// refused rather than written and never served.
#[tokio::test]
async fn a_private_entry_cannot_take_a_public_id() -> anyhow::Result<()> {
    let (_dir, state, acme) = setup().await?;
    let signer = org_admin(&state, &acme).await?;

    let resp = signed_json(
        &state,
        &signer,
        "PUT",
        &entry_uri(&acme.org_id, "prefill_throughput_256"),
        &json!({"benchmark_type": "prefill_throughput", "parameter_prefill_tokens": 4096}),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::CONFLICT);
    assert!(json_str(&body_json(resp).await?, "error")?.contains("prefill_throughput_256"));
    Ok(())
}

/// Benchmark ids are one namespace across organizations, so the server's own
/// passes can resolve every id: a second organization is refused the id the
/// first keeps, which may still replace its own entry.
#[tokio::test]
async fn a_private_entry_cannot_take_another_organizations_id() -> anyhow::Result<()> {
    let (_dir, state, acme) = setup().await?;
    let labs = seed_org(&state, "labs").await?;
    let acme_admin = org_admin(&state, &acme).await?;
    let labs_admin = org_admin(&state, &labs).await?;
    assert_eq!(
        put_entry(&state, &acme_admin, &acme.org_id).await?.status(),
        StatusCode::OK
    );

    let resp = put_entry(&state, &labs_admin, &labs.org_id).await?;
    assert_eq!(resp.status(), StatusCode::CONFLICT);
    assert!(json_str(&body_json(resp).await?, "error")?.contains(PRIVATE));
    assert_eq!(
        put_entry(&state, &acme_admin, &acme.org_id).await?.status(),
        StatusCode::OK
    );
    Ok(())
}

/// Ownership is judged by key, so an entry another organization keeps that
/// does not parse still holds its id: were it judged by parsed entries, two
/// organizations would hold one id and the server's passes could resolve neither.
#[tokio::test]
async fn an_unreadable_entry_still_holds_its_id_against_another_organization() -> anyhow::Result<()>
{
    let (dir, state, acme) = setup().await?;
    let labs = seed_org(&state, "labs").await?;
    write_private_entry(
        dir.path(),
        &labs.org_id,
        PRIVATE,
        "benchmark_type = \"nope\"",
    )?;
    let acme_admin = org_admin(&state, &acme).await?;

    let resp = put_entry(&state, &acme_admin, &acme.org_id).await?;
    assert_eq!(resp.status(), StatusCode::CONFLICT);
    assert_eq!(
        json_str(&body_json(resp).await?, "error")?,
        format!("another organization already keeps a benchmark {PRIVATE}")
    );
    let listed =
        body_json(signed_empty(&state, &acme_admin, "GET", "/api/v1/orgs/acme/benchmarks").await?)
            .await?;
    assert_eq!(listed["benchmarks"], json!([]), "nothing was written");
    Ok(())
}

/// `GET /datasets/{id}` serves an artifact by the digest its entry pins, so a
/// second organization may not pin a corpus the first keeps: it would be
/// served the first organization's copy.
#[tokio::test]
async fn a_private_entry_cannot_pin_another_organizations_corpus() -> anyhow::Result<()> {
    let (_dir, state, acme) = setup().await?;
    let labs = seed_org(&state, "labs").await?;
    let acme_admin = org_admin(&state, &acme).await?;
    let labs_admin = org_admin(&state, &labs).await?;
    let pin = a_corpus_pin(CORPUS_DIGEST);
    let resp = signed_json(
        &state,
        &acme_admin,
        "PUT",
        &entry_uri(&acme.org_id, "vl_spec_decode_acme_k8"),
        &pin,
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::OK);

    let uri = entry_uri(&labs.org_id, "vl_spec_decode_labs_k8");
    let resp = signed_json(&state, &labs_admin, "PUT", &uri, &pin).await?;
    assert_eq!(resp.status(), StatusCode::CONFLICT);
    assert!(json_str(&body_json(resp).await?, "error")?.contains(CORPUS_DIGEST));
    let other = a_corpus_pin(&"3".repeat(64));
    let resp = signed_json(&state, &labs_admin, "PUT", &uri, &other).await?;
    assert_eq!(resp.status(), StatusCode::OK);
    Ok(())
}

/// A corpus a public entry pins is everyone's, so a second organization may pin
/// it after a first has: without the exemption the second write would be a `409`.
/// Only as the public entry pins it, though.
#[tokio::test]
async fn a_publicly_pinned_corpus_may_be_pinned_by_several_organizations() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    std::fs::write(
        dir.path()
            .join("benchmarks")
            .join("vl_spec_decode_public_k8.toml"),
        format!(
            r#"benchmark_type = "vl_spec_decode"
parameter_draft_length = 8
parameter_max_tokens = 2048

[dataset]
format  = "vl-spec-decode-corpus/1"
adapter = "mmspec@1"
digest  = "{CORPUS_DIGEST}"
bytes   = 1024
subset  = "GQA"

[dataset.upstream]
source   = "git"
url      = "github.com/acme/corpus"
revision = "0123456789abcdef0123456789abcdef01234567"
subpath  = "dataset/test"
identity = "2923603292c7dd463ad01a119f57965817778fc3"
digest   = "2222222222222222222222222222222222222222222222222222222222222222"
license  = "Apache-2.0"
"#
        ),
    )?;
    let state = make_state(dir.path()).await?;
    let public = unauthed_get(&state, "/benchmarks/vl_spec_decode_public_k8").await?;
    assert_eq!(public.status(), StatusCode::OK, "the public entry loads");
    let acme = seed_org(&state, "acme").await?;
    let labs = seed_org(&state, "labs").await?;
    let acme_admin = org_admin(&state, &acme).await?;
    let labs_admin = org_admin(&state, &labs).await?;
    let pin = a_corpus_pin(CORPUS_DIGEST);

    // Acme pins first, so labs' write meets another organization's pin.
    let resp = signed_json(
        &state,
        &acme_admin,
        "PUT",
        &entry_uri(&acme.org_id, "vl_spec_decode_acme_k8"),
        &pin,
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let uri = entry_uri(&labs.org_id, "vl_spec_decode_labs_k8");
    let resp = signed_json(&state, &labs_admin, "PUT", &uri, &pin).await?;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(body_json(resp).await?["dataset"]["digest"], CORPUS_DIGEST);

    // Pinned differently, the corpus would contest the public entry's probe.
    let mut rival = pin.clone();
    rival["dataset"]["upstream"]["url"] = json!("github.com/rival/corpus");
    let uri = entry_uri(&labs.org_id, "vl_spec_decode_a_k8");
    let resp = signed_json(&state, &labs_admin, "PUT", &uri, &rival).await?;
    assert_eq!(resp.status(), StatusCode::CONFLICT);
    assert_eq!(
        json_str(&body_json(resp).await?, "error")?,
        format!("a public benchmark pins dataset {CORPUS_DIGEST} differently")
    );
    Ok(())
}

/// The round trip the catalog exists for: an entry an administrator writes is
/// served to the organization's device and member on their very next
/// `GET /benchmarks`, is still absent for everyone else, and is gone again the
/// request after it is deleted. No restart, no cache to wait out.
#[tokio::test]
async fn a_written_entry_is_served_to_the_organization_on_the_next_request() -> anyhow::Result<()> {
    async fn lists(
        state: &AppState,
        key: &ed25519_dalek::SigningKey,
        client_id: &pipette_plan_types::ClientId,
    ) -> anyhow::Result<bool> {
        let body = body_json(authed_get(state, key, client_id, "/benchmarks").await?).await?;
        Ok(benchmark_ids(&body).iter().any(|id| id == PRIVATE))
    }

    let (_dir, state, acme) = setup().await?;
    let admin = org_admin(&state, &acme).await?;
    let (device_key, device) = register_and_approve(&state).await?;
    own_client_by_org(&state, &device, &acme.org_id).await?;
    let (stranger_key, stranger) = register_and_approve(&state).await?;

    assert!(!lists(&state, &device_key, &device).await?);
    assert_eq!(
        put_entry(&state, &admin, &acme.org_id).await?.status(),
        StatusCode::OK
    );

    assert!(lists(&state, &device_key, &device).await?);
    assert!(!lists(&state, &stranger_key, &stranger).await?);
    // The member account, through its membership; an eval, so the served
    // detail reaches the samples proxy and the unreachable evals server.
    let (key, user_id, key_id) = &admin;
    let resp = authed_user_get(
        &state,
        key,
        user_id,
        key_id,
        &format!("/benchmarks/{PRIVATE}"),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
    let resp = authed_get(
        &state,
        &stranger_key,
        &stranger,
        &format!("/benchmarks/{PRIVATE}"),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    let resp = signed_empty(&state, &admin, "DELETE", &entry_uri(&acme.org_id, PRIVATE)).await?;
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    assert!(!lists(&state, &device_key, &device).await?);
    Ok(())
}

/// What the app reads conforms to its schema: the listing with a readable and
/// an unreadable entry, and an entry with nested tables as `PUT` and `GET`
/// answer it.
#[tokio::test]
async fn the_catalog_bodies_conform_to_their_schemas() -> anyhow::Result<()> {
    let (dir, state, acme) = setup().await?;
    let signer = org_admin(&state, &acme).await?;
    let uri = entry_uri(&acme.org_id, "vl_spec_decode_acme_k8");
    let resp = signed_json(&state, &signer, "PUT", &uri, &a_corpus_pin(CORPUS_DIGEST)).await?;
    assert_eq!(resp.status(), StatusCode::OK);
    crate::api_schema::assert_conforms("OrgBenchmark", &body_json(resp).await?)?;
    let shown = body_json(signed_empty(&state, &signer, "GET", &uri).await?).await?;
    crate::api_schema::assert_conforms("OrgBenchmark", &shown)?;
    write_private_entry(
        dir.path(),
        &acme.org_id,
        "eval_broken",
        "benchmark_type = \"nope\"",
    )?;

    let listed =
        body_json(signed_empty(&state, &signer, "GET", "/api/v1/orgs/acme/benchmarks").await?)
            .await?;
    assert_eq!(listed["unreadable_benchmark_ids"], json!(["eval_broken"]));
    assert_eq!(listed["benchmarks"], json!([shown]));
    crate::api_schema::assert_conforms("OrgBenchmarks", &listed)?;
    Ok(())
}

/// A signed `GET` of the fixture entry with `accept` as its `Accept`, or none.
async fn get_accepting(
    state: &AppState,
    signer: &UserSigner,
    uri: &str,
    accept: Option<&str>,
) -> anyhow::Result<axum::http::Response<axum::body::Body>> {
    let (key, user_id, key_id) = signer;
    let mut headers = user_auth_headers(key, user_id, key_id, "GET", uri)?;
    headers.extend(accept.map(|accept| ("Accept", accept.to_string())));
    request_with_headers(state, "GET", uri, &headers).await
}

fn header<'a>(resp: &'a axum::http::Response<axum::body::Body>, name: &str) -> Option<&'a str> {
    resp.headers()
        .get(name)
        .and_then(|value| value.to_str().ok())
}

/// The single entry is the catalog file only for an `Accept` that ranks
/// `application/toml` above JSON; the CLI's `application/json`, a wildcard,
/// no header and a tie keep JSON. Either answer varies on `Accept`, and the
/// file reads back as the entry it was written from.
#[rstest]
#[case::toml(Some("application/toml"), true)]
#[case::toml_first(Some("application/toml, application/json;q=0.9"), true)]
#[case::toml_any_case(Some("Application/TOML; charset=utf-8"), true)]
#[case::no_header(None, false)]
#[case::wildcard(Some("*/*"), false)]
#[case::json(Some("application/json"), false)]
#[case::json_first(Some("application/json, application/toml;q=0.5"), false)]
#[case::tie(Some("application/json, application/toml"), false)]
#[case::refused(Some("application/toml;q=0"), false)]
#[case::toml_below_a_wildcard(Some("application/toml;q=0.5, */*"), false)]
#[case::toml_above_a_wildcard(Some("application/toml, */*;q=0.5"), true)]
#[case::json_below_its_wildcard(Some("application/toml;q=0.5, application/*"), false)]
#[case::upper_case_q(Some("application/json;Q=0.1, application/toml;q=0.5"), true)]
#[tokio::test]
async fn the_entry_is_the_catalog_file_when_asked_for(
    #[case] accept: Option<&str>,
    #[case] toml: bool,
) -> anyhow::Result<()> {
    let (_dir, state, acme) = setup().await?;
    let signer = org_admin(&state, &acme).await?;
    let uri = entry_uri(&acme.org_id, PRIVATE);
    assert_eq!(
        put_raw(
            &state,
            &signer,
            &uri,
            Some("application/toml"),
            ACME_EVAL_TOML
        )
        .await?
        .status(),
        StatusCode::OK
    );

    let resp = get_accepting(&state, &signer, &uri, accept).await?;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(header(&resp, "vary"), Some("accept"));
    if !toml {
        assert_eq!(header(&resp, "content-type"), Some("application/json"));
        assert_eq!(body_json(resp).await?["benchmark_id"], PRIVATE);
        return Ok(());
    }
    assert_eq!(header(&resp, "content-type"), Some("application/toml"));
    let file = String::from_utf8(
        axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await?
            .to_vec(),
    )?;
    assert!(file.contains("parameter_max_tokens = 4096"), "{file}");
    assert!(
        !file.contains("benchmark_id") && !file.contains("org_id"),
        "the path carries both: {file}"
    );
    let before = body_json(signed_empty(&state, &signer, "GET", &uri).await?).await?;
    let resp = put_raw(&state, &signer, &uri, Some("application/toml"), &file).await?;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        body_json(resp).await?,
        before,
        "the file writes back the entry"
    );
    Ok(())
}

/// Asked for the file, a refusal and a fault still answer the JSON envelope:
/// an id the organization does not keep, another organization's catalog, and
/// an entry that does not read.
#[tokio::test]
async fn a_refusal_of_the_file_is_still_json() -> anyhow::Result<()> {
    let (dir, state, acme) = setup().await?;
    let signer = org_admin(&state, &acme).await?;
    seed_org(&state, "labs").await?;
    write_private_entry(
        dir.path(),
        &acme.org_id,
        "eval_broken",
        "benchmark_type = \"nope\"",
    )?;
    for (uri, status) in [
        (entry_uri(&"acme", PRIVATE), StatusCode::NOT_FOUND),
        (entry_uri(&"labs", PRIVATE), StatusCode::NOT_FOUND),
        (
            entry_uri(&"acme", "eval_broken"),
            StatusCode::INTERNAL_SERVER_ERROR,
        ),
    ] {
        let resp = get_accepting(&state, &signer, &uri, Some("application/toml")).await?;
        assert_eq!(resp.status(), status, "{uri}");
        assert_eq!(header(&resp, "content-type"), Some("application/json"));
        let error = body_json(resp).await?;
        assert!(error["error"].is_string(), "{uri}: {error}");
    }
    Ok(())
}

/// A signed `PUT` of the fixture TOML carrying `headers` beside the signature.
async fn put_conditional(
    state: &AppState,
    signer: &UserSigner,
    uri: &str,
    headers: &[(&'static str, &str)],
    body: &str,
) -> anyhow::Result<axum::http::Response<axum::body::Body>> {
    let (key, user_id, key_id) = signer;
    let mut signed = user_auth_headers(key, user_id, key_id, "PUT", uri)?;
    signed.extend(
        headers
            .iter()
            .map(|(name, value)| (*name, (*value).to_string())),
    );
    raw_request(
        state,
        "PUT",
        uri,
        &signed,
        Some("application/toml"),
        body.to_string(),
    )
    .await
}

/// `If-None-Match: *` creates only and `If-Match: *` replaces only, each a
/// `412` naming what stands in the way and writing nothing. An entry that does
/// not read is kept, so it may be replaced and may not be created over.
#[tokio::test]
async fn a_precondition_narrows_the_put_to_a_create_or_a_replace() -> anyhow::Result<()> {
    let (dir, state, acme) = setup().await?;
    let signer = org_admin(&state, &acme).await?;
    let uri = entry_uri(&acme.org_id, PRIVATE);
    let create = [("If-None-Match", "*")];
    let replace = [("If-Match", "*")];
    let revised = ACME_EVAL_TOML.replace("4096", "2048");

    let resp = put_conditional(&state, &signer, &uri, &replace, ACME_EVAL_TOML).await?;
    assert_eq!(resp.status(), StatusCode::PRECONDITION_FAILED);
    assert_eq!(
        json_str(&body_json(resp).await?, "error")?,
        format!("organization {} keeps no benchmark {PRIVATE}", acme.org_id)
    );
    let resp = signed_empty(&state, &signer, "GET", &uri).await?;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND, "nothing was written");

    let resp = put_conditional(&state, &signer, &uri, &create, ACME_EVAL_TOML).await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let resp = put_conditional(&state, &signer, &uri, &create, &revised).await?;
    assert_eq!(resp.status(), StatusCode::PRECONDITION_FAILED);
    assert_eq!(
        json_str(&body_json(resp).await?, "error")?,
        format!("acme already keeps a benchmark {PRIVATE}")
    );
    let shown = body_json(signed_empty(&state, &signer, "GET", &uri).await?).await?;
    assert_eq!(shown["parameter_max_tokens"], 4096, "not replaced");

    let resp = put_conditional(&state, &signer, &uri, &replace, &revised).await?;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(body_json(resp).await?["parameter_max_tokens"], 2048);

    write_private_entry(
        dir.path(),
        &acme.org_id,
        "eval_broken",
        "benchmark_type = \"nope\"",
    )?;
    let broken = entry_uri(&acme.org_id, "eval_broken");
    let resp = put_conditional(&state, &signer, &broken, &create, ACME_EVAL_TOML).await?;
    assert_eq!(resp.status(), StatusCode::PRECONDITION_FAILED);
    let resp = put_conditional(&state, &signer, &broken, &replace, ACME_EVAL_TOML).await?;
    assert_eq!(resp.status(), StatusCode::OK, "repaired in place");
    Ok(())
}

/// The route serves no `ETag`, so a precondition other than `*`, or both at
/// once, is a `400`. A caller refused the write is refused before either is
/// judged, as before its body is.
#[rstest]
#[case::an_etag(&[("If-Match", "\"abc\"")], false, StatusCode::BAD_REQUEST)]
#[case::both(&[("If-Match", "*"), ("If-None-Match", "*")], false, StatusCode::BAD_REQUEST)]
#[case::a_member(&[("If-Match", "\"abc\"")], true, StatusCode::FORBIDDEN)]
#[tokio::test]
async fn a_precondition_is_star_or_nothing(
    #[case] headers: &[(&'static str, &str)],
    #[case] member: bool,
    #[case] status: StatusCode,
) -> anyhow::Result<()> {
    let (_dir, state, acme) = setup().await?;
    let signer = if member {
        let signer = registered_user(&state, &[]).await?;
        set_user_org(&state, &signer.1, Some(acme.org_id.clone())).await?;
        signer
    } else {
        org_admin(&state, &acme).await?
    };
    let uri = entry_uri(&acme.org_id, PRIVATE);
    let resp = put_conditional(&state, &signer, &uri, headers, ACME_EVAL_TOML).await?;
    assert_eq!(resp.status(), status);
    Ok(())
}

/// A writer outside the organization's reach gets the `404` an unconditional
/// write gets, with either precondition: a `412` would tell it the id is kept.
#[tokio::test]
async fn a_precondition_does_not_probe_another_catalog() -> anyhow::Result<()> {
    let (_dir, state, acme) = setup().await?;
    let labs = seed_org(&state, "labs").await?;
    let labs_admin = org_admin(&state, &labs).await?;
    assert_eq!(
        put_entry(&state, &labs_admin, &labs.org_id).await?.status(),
        StatusCode::OK
    );
    let outsider = org_admin(&state, &acme).await?;
    let uri = entry_uri(&labs.org_id, PRIVATE);

    let resp = put_conditional(&state, &outsider, &uri, &[], ACME_EVAL_TOML).await?;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    let refusal = body_json(resp).await?;
    for precondition in [("If-None-Match", "*"), ("If-Match", "*")] {
        let resp =
            put_conditional(&state, &outsider, &uri, &[precondition], ACME_EVAL_TOML).await?;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND, "{precondition:?}");
        assert_eq!(body_json(resp).await?, refusal, "{precondition:?}");
    }
    Ok(())
}
