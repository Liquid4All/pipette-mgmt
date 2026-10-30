//! Request and response bodies checked against the schemas the console app
//! generates its TypeScript from (`pipette_httpapi::api_schema`), so a body that
//! drifts from its declared type fails here rather than in the browser.

use serde_json::Value;

/// Fails unless `body` is a valid `def` from the bundle, its formats checked
/// too, so a `date-time` must be RFC 3339.
pub(crate) fn assert_conforms(def: &str, body: &Value) -> anyhow::Result<()> {
    let mut schema = pipette_httpapi::api_schema::bundle()?;
    schema
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("the bundle is not an object"))?
        .insert("$ref".into(), format!("#/$defs/{def}").into());
    let validator = jsonschema::options()
        .should_validate_formats(true)
        .build(&schema)?;
    let errors: Vec<String> = validator
        .iter_errors(body)
        .map(|e| format!("{e} at {}", e.instance_path()))
        .collect();
    anyhow::ensure!(
        errors.is_empty(),
        "{def} does not match its schema: {}\n{body}",
        errors.join("; ")
    );
    Ok(())
}

/// A body missing a required field, or carrying the wrong type, is refused.
#[test]
fn test_a_body_that_drifts_from_its_schema_is_refused() {
    let missing = serde_json::json!({ "key_id": "userkey_x", "status": "active" });
    let wrong_type = serde_json::json!({ "user_id": "u", "key_id": "k", "status": 1 });
    for body in [missing, wrong_type] {
        let refused = assert_conforms("UserKeyRegistered", &body);
        assert!(refused.is_err(), "{body} was accepted");
    }
}

/// A deadline is RFC 3339, `null` for never, not the compact `avail/` form.
#[test]
fn test_a_compact_deadline_is_refused() -> anyhow::Result<()> {
    let rfc_3339 = serde_json::json!({
        "earliest_expires_at": "2026-04-09T12:00:00Z",
        "latest_expires_at": null,
    });
    let compact = serde_json::json!({
        "earliest_expires_at": "20260409T120000Z",
        "latest_expires_at": "never",
    });
    assert_conforms("JobExpiry", &rfc_3339)?;
    assert!(assert_conforms("JobExpiry", &compact).is_err());
    Ok(())
}

/// A registration the route accepts conforms to its request schema, and one
/// missing a required field is refused by both.
#[tokio::test]
async fn test_the_registration_schema_agrees_with_its_route() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    pipette_web_testkit::setup_benchmarks(dir.path())?;
    let state = pipette_web_testkit::make_state(dir.path()).await?;
    let public_key = hex::encode(
        pipette_web_testkit::signing_key()?
            .verifying_key()
            .as_bytes(),
    );
    let accepted = serde_json::json!({
        "public_key": public_key,
        "contact_email": "asker@example.test",
        "key_details": "work laptop",
    });
    let missing = serde_json::json!({
        "public_key": public_key,
        "contact_email": "asker@example.test",
    });

    assert!(assert_conforms("RegisterUserKeyRequest", &missing).is_err());
    let resp =
        pipette_web_testkit::unauthed_post(&state, "/api/v1/users/register", &missing).await?;
    assert_eq!(resp.status(), axum::http::StatusCode::BAD_REQUEST);

    assert_conforms("RegisterUserKeyRequest", &accepted)?;
    let resp =
        pipette_web_testkit::unauthed_post(&state, "/api/v1/users/register", &accepted).await?;
    assert_eq!(resp.status(), axum::http::StatusCode::CREATED);
    Ok(())
}
