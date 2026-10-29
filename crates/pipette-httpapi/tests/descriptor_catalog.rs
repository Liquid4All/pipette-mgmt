//! Read routes for the SQL descriptor catalog.

use std::collections::HashMap;
use std::num::NonZeroUsize;

use anyhow::Context;
use axum::http::StatusCode;
use rstest::rstest;
use serde_json::{Value, json};

use pipette_core::client::ClientStatus;
use pipette_core::config::OfferRule;
use pipette_core::vault::blob_key;
use pipette_httpapi::state::AppState;
use pipette_hub_testkit::{MockFile, MockGate, MockHub, MockRepo};
use pipette_plan_types::{
    CatalogName, CatalogScope, DescriptorEntryKind as Kind, DescriptorEntryState as EntryState,
    DeviceProfile, NonEmptyTrimmedString, NonEmptyVec, OrgId, PrincipalOwner, Tag, UserPermission,
    descriptor_digest,
};
use pipette_store::{AuditFilter, DescriptorCatalogStore};
use pipette_store_testkit::{
    VAULT_BLOB, a_descriptor_entry, a_huggingface_origin, a_member, a_model_member, blob_body,
    vault_blob_sha256, vault_resource_for,
};
use pipette_web_testkit::*;

const BASE: &str = "/api/v1/descriptor-catalog";

macro_rules! sql_state_or_skip {
    ($dir:expr) => {{
        let Some(sql) = seeded_sql_state($dir).await? else {
            return Ok(());
        };
        sql
    }};
}

fn runtime(version: &str, flavor: &str) -> Value {
    json!({
        "type": "llamacpp_cli_stock_tools", "source": "github_release",
        "repository_url": "github.com/ggml-org/llama.cpp",
        "repository_version": version, "flavor": flavor,
    })
}

fn create_body(name: &str, version: &str) -> Value {
    json!({"name": name, "runtimes": [runtime(version, "macos-arm64")]})
}

#[tokio::test]
async fn name_lookup_counts_approved_offers_and_supplies_plan_toml() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let mut sql = sql_state_or_skip!(dir.path());
    let mut config = (*sql.state.config).clone();
    config.descriptor_catalog.offer_rules.extend([
        OfferRule {
            runtime_type: pipette_plan_types::RuntimeType::LlamacppCliStockTools,
            flavor: "macos-arm64".into(),
            requires: ["os:macos".to_owned(), "tag:ready".to_owned()].into(),
            any_of: Vec::new(),
        },
        OfferRule {
            runtime_type: pipette_plan_types::RuntimeType::LlamacppCliStockTools,
            flavor: "macos-x64".into(),
            requires: ["os:macos".to_owned()].into(),
            any_of: Vec::new(),
        },
    ]);
    sql.state.config = std::sync::Arc::new(config);
    let state = &sql.state;
    let store = store_of(state)?;
    store
        .create_entry(&a_descriptor_entry(
            "Offer-1",
            Kind::HandMade,
            100,
            a_member("b1", "macos-arm64")?,
        )?)
        .await?;
    store
        .append_members(
            &CatalogScope::Global,
            &CatalogName::try_new("Offer-1")?,
            NonEmptyVec::try_new(vec![
                a_member("b1", "macos-x64")?,
                a_member("b1", "linux-x64-rocm")?,
            ])?,
        )
        .await?;

    for (approved, tagged) in [(true, true), (true, false), (false, true)] {
        let (_, id) = register_pending(state).await?;
        let mut client = state
            .client_store
            .get_client(&id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("registered client missing"))?;
        client.device_profile.device_os_name = Some(NonEmptyTrimmedString::try_new("macOS")?);
        if approved {
            client.status = ClientStatus::Approved;
        }
        state.client_store.put_client(&client).await?;
        if tagged {
            state
                .tag_store
                .add_client_tag(&id, &Tag::try_new("ready")?)
                .await?;
        }
    }
    let without_fleet = registered_user(state, &[]).await?;
    let reader = registered_user(state, &[UserPermission::ReadClients]).await?;
    let uri = format!("{BASE}/names?names=global/Offer-1");
    let (status, ordinary) = read(state, &without_fleet, &uri).await?;
    assert_eq!(status, StatusCode::OK, "{ordinary}");
    assert!(
        ordinary["entries"][0]["members"][0]
            .get("offered_device_count")
            .is_none()
    );

    assert_eq!(
        read(state, &without_fleet, &format!("{uri}&offer_counts=true"))
            .await?
            .0,
        StatusCode::FORBIDDEN
    );
    let (status, counted) = read(state, &reader, &format!("{uri}&offer_counts=true")).await?;
    assert_eq!(status, StatusCode::OK, "{counted}");
    let members = counted["entries"][0]["members"]
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("members missing"))?;
    assert_eq!(members[0]["offered_device_count"], 1);
    assert_eq!(members[1]["offered_device_count"], 2);
    assert_eq!(members[2]["offered_device_count"], 0);
    let plan_toml = members[0]["plan_toml"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("plan TOML missing"))?;
    let parsed: toml::Value = toml::from_str(&format!("runtime = {plan_toml}"))?;
    assert_eq!(parsed["runtime"]["flavor"].as_str(), Some("macos-arm64"));
    assert_eq!(parsed["runtime"]["repository_version"].as_str(), Some("b1"));
    assert_eq!(
        serde_json::to_value(parsed["runtime"].clone())?,
        members[0]["descriptor"]
    );
    crate::api_schema::assert_conforms("DescriptorEntryLookup", &counted)?;

    let ids = state
        .tag_store
        .list_client_ids_by_tag(&Tag::try_new("ready")?)
        .await?;
    for id in ids {
        state
            .tag_store
            .remove_client_tag(&id, &Tag::try_new("ready")?)
            .await?;
    }
    let (_, without_tag) = read(state, &reader, &format!("{uri}&offer_counts=true")).await?;
    assert_eq!(
        without_tag["entries"][0]["members"][0]["offered_device_count"],
        0
    );
    assert_eq!(
        without_tag["entries"][0]["members"][1]["offered_device_count"],
        2
    );
    Ok(())
}

#[tokio::test]
async fn org_catalog_writes_respect_scope_and_share_cli_checks() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let sql = sql_state_or_skip!(dir.path());
    let state = &sql.state;
    let acme = seed_org(state, "acme").await?.org_id;
    let beta = seed_org(state, "beta").await?.org_id;
    let member = registered_user(state, &[UserPermission::EditCatalog]).await?;
    set_user_org(state, &member.1, Some(acme.clone())).await?;
    let no_grant = registered_user(state, &[]).await?;
    set_user_org(state, &no_grant.1, Some(acme.clone())).await?;
    let server = registered_user(state, &[UserPermission::EditAnyCatalog]).await?;
    store_of(state)?
        .create_entry(&a_descriptor_entry(
            "Fork",
            Kind::Stable,
            100,
            a_member("b0", "macos-arm64")?,
        )?)
        .await?;
    let acme_uri = format!("/api/v1/orgs/{acme}/descriptor-catalog");
    let beta_uri = format!("/api/v1/orgs/{beta}/descriptor-catalog");
    super::api_schema::assert_conforms("CreateDescriptorEntryRequest", &create_body("Fork", "b1"))?;

    assert_eq!(
        signed_json(
            state,
            &no_grant,
            "POST",
            &acme_uri,
            &create_body("Fork", "b1")
        )
        .await?
        .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        signed_json(
            state,
            &member,
            "POST",
            &beta_uri,
            &create_body("Fork", "b1")
        )
        .await?
        .status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        signed_json(
            state,
            &member,
            "POST",
            "/api/v1/orgs/acme/descriptor-catalog",
            &create_body("Fork", "b1")
        )
        .await?
        .status(),
        StatusCode::BAD_REQUEST
    );

    let response = signed_json(
        state,
        &member,
        "POST",
        &acme_uri,
        &create_body("Fork", "b1"),
    )
    .await?;
    assert_eq!(response.status(), StatusCode::CREATED);
    let created = body_json(response).await?;
    super::api_schema::assert_conforms("DescriptorEntryListRow", &created)?;
    assert_eq!(created["name"], "Fork");
    assert_eq!(created["scope"], acme.as_str());
    assert_eq!(created["display_name"], "Fork");
    assert_eq!(created["created_by"], format!("account:{}", member.1));
    assert_eq!(created["members"].as_array().map(Vec::len), Some(1));
    assert_eq!(
        signed_json(
            state,
            &member,
            "POST",
            &acme_uri,
            &create_body("fOrK", "b1")
        )
        .await?
        .status(),
        StatusCode::CONFLICT
    );

    // The same name in another scope is independent; an org may also use
    // a global importer pattern.
    let response = signed_json(
        state,
        &server,
        "POST",
        &beta_uri,
        &create_body("Fork", "b2"),
    )
    .await?;
    assert_eq!(response.status(), StatusCode::CREATED);
    assert_eq!(body_json(response).await?["display_name"], "beta/Fork");
    let beta_audit = state
        .audit_store
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("audit store"))?;
    let beta_events = beta_audit
        .list_events(
            &AuditFilter {
                org_id: Some(beta.clone()),
                ..Default::default()
            },
            None,
            NonZeroUsize::new(100).unwrap_or(NonZeroUsize::MIN),
        )
        .await?;
    assert!(
        beta_events.items.iter().any(|event| matches!(
            &event.action,
            pipette_plan_types::AuditAction::Http { route, status: 201, .. }
                if route == "/api/v1/orgs/{org_id}/descriptor-catalog"
        )),
        "an outside editor's write reaches the target organization's audit log"
    );
    let response = signed_json(
        state,
        &member,
        "POST",
        &acme_uri,
        &create_body("llamacpp-v0.7.0", "b3"),
    )
    .await?;
    assert_eq!(response.status(), StatusCode::CREATED);

    let member_uri = format!("{acme_uri}/fOrK/members");
    super::api_schema::assert_conforms(
        "AddDescriptorMemberRequest",
        &json!({"runtime": runtime("b1", "macos-x64")}),
    )?;
    let appended = signed_json(
        state,
        &member,
        "POST",
        &member_uri,
        &json!({"runtime": runtime("b1", "macos-x64")}),
    )
    .await?;
    assert_eq!(appended.status(), StatusCode::OK);
    let appended = body_json(appended).await?;
    assert_eq!(appended["name"], "Fork");
    assert_eq!(appended["display_name"], "Fork");
    assert_eq!(appended["members"].as_array().map(Vec::len), Some(2));
    assert_eq!(
        signed_json(
            state,
            &member,
            "POST",
            &member_uri,
            &json!({"runtime": runtime("b2", "linux-x64")})
        )
        .await?
        .status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        signed_json(
            state,
            &member,
            "POST",
            &member_uri,
            &json!({"runtime": runtime("b1", "macos-x64")})
        )
        .await?
        .status(),
        StatusCode::CONFLICT
    );
    let vault = json!({
        "type": "llamacpp_cli_stock_tools", "source": "vault_remote_archive",
        "url": "/vault/resources/vault_1/content", "principal_id": acme.as_str(),
        "flavor": "macos-arm64",
    });
    assert_eq!(
        signed_json(
            state,
            &member,
            "POST",
            &acme_uri,
            &json!({"name":"Vault", "runtimes":[vault.clone()]})
        )
        .await?
        .status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        signed_json(
            state,
            &member,
            "POST",
            &member_uri,
            &json!({"runtime":vault})
        )
        .await?
        .status(),
        StatusCode::BAD_REQUEST
    );
    let mut model = a_descriptor_entry(
        "Model-1",
        Kind::HandMade,
        100,
        a_model_member("gguf_text", "AcmeLM-3B-GGUF", "q4_0")?,
    )?;
    model.scope = CatalogScope::Org(acme.clone());
    store_of(state)?.create_entry(&model).await?;
    let model_state_uri = format!("{acme_uri}/Model-1/state");
    assert_eq!(
        signed_json(
            state,
            &member,
            "POST",
            &model_state_uri,
            &json!({"action":"pin"})
        )
        .await?
        .status(),
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        store_of(state)?
            .get_entry(&CatalogScope::Org(acme.clone()), &model.name)
            .await?
            .map(|entry| entry.state),
        Some(EntryState::Pinned)
    );
    assert_eq!(
        signed_json(
            state,
            &member,
            "POST",
            &model_state_uri,
            &json!({"action":"tombstone", "confirmation":"Model-1"})
        )
        .await?
        .status(),
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        store_of(state)?
            .get_entry(&CatalogScope::Org(acme.clone()), &model.name)
            .await?
            .map(|entry| entry.state),
        Some(EntryState::Tombstoned)
    );
    Ok(())
}

#[tokio::test]
async fn org_catalog_state_and_audit() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let sql = sql_state_or_skip!(dir.path());
    let state = &sql.state;
    let acme = seed_org(state, "acme").await?.org_id;
    let member = registered_user(state, &[UserPermission::EditCatalog]).await?;
    set_user_org(state, &member.1, Some(acme.clone())).await?;
    let base = format!("/api/v1/orgs/{acme}/descriptor-catalog");
    assert_eq!(
        signed_json(state, &member, "POST", &base, &create_body("Fork", "b1"))
            .await?
            .status(),
        StatusCode::CREATED
    );
    let members = format!("{base}/Fork/members");
    assert_eq!(
        signed_json(
            state,
            &member,
            "POST",
            &members,
            &json!({"runtime": runtime("b1", "macos-x64")})
        )
        .await?
        .status(),
        StatusCode::OK
    );
    let state_uri = format!("{base}/Fork/state");
    super::api_schema::assert_conforms(
        "ChangeDescriptorEntryStateRequest",
        &json!({"action":"pin"}),
    )?;
    for action in ["pin", "unpin", "hide", "unhide"] {
        assert_eq!(
            signed_json(
                state,
                &member,
                "POST",
                &state_uri,
                &json!({"action": action})
            )
            .await?
            .status(),
            StatusCode::NO_CONTENT,
            "{action}"
        );
    }
    assert_eq!(
        signed_json(
            state,
            &member,
            "POST",
            &state_uri,
            &json!({"action":"unhide"})
        )
        .await?
        .status(),
        StatusCode::CONFLICT
    );
    assert_eq!(
        signed_json(
            state,
            &member,
            "POST",
            &state_uri,
            &json!({"action":"tombstone"})
        )
        .await?
        .status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        signed_json(
            state,
            &member,
            "POST",
            &state_uri,
            &json!({"action":"tombstone", "confirmation":"fork"})
        )
        .await?
        .status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        signed_json(
            state,
            &member,
            "POST",
            &state_uri,
            &json!({"action":"tombstone", "confirmation":"Fork"})
        )
        .await?
        .status(),
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        signed_json(state, &member, "POST", &base, &create_body("Fork", "b1"))
            .await?
            .status(),
        StatusCode::CONFLICT
    );

    let audit = state
        .audit_store
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("audit store"))?;
    let page = audit
        .list_events(
            &AuditFilter {
                org_id: Some(acme.clone()),
                ..Default::default()
            },
            None,
            NonZeroUsize::new(100).unwrap_or(NonZeroUsize::MIN),
        )
        .await?;
    let writes = page
        .items
        .iter()
        .filter(|event| {
            matches!(&event.action, pipette_plan_types::AuditAction::Http { route, status, .. }
            if route.contains("/descriptor-catalog") && matches!(status, 200 | 201 | 204))
        })
        .count();
    assert_eq!(
        writes, 7,
        "each successful write is in the organization's audit log"
    );
    Ok(())
}

/// A name with a space travels percent-encoded in a signed path and
/// form-encoded in a signed query, as the console sends it.
#[tokio::test]
async fn a_spaced_name_round_trips_through_signed_paths_and_queries() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let sql = sql_state_or_skip!(dir.path());
    let state = &sql.state;
    let acme = seed_org(state, "acme").await?.org_id;
    let member = registered_user(state, &[UserPermission::EditCatalog]).await?;
    set_user_org(state, &member.1, Some(acme.clone())).await?;
    let base = format!("/api/v1/orgs/{acme}/descriptor-catalog");
    let created = signed_json(state, &member, "POST", &base, &create_body("My Fork", "b1")).await?;
    assert_eq!(created.status(), StatusCode::CREATED);
    let added = signed_json(
        state,
        &member,
        "POST",
        &format!("{base}/my%20fork/members"),
        &json!({"runtime": runtime("b1", "macos-x64")}),
    )
    .await?;
    assert_eq!(added.status(), StatusCode::OK);

    let (status, named) = read(
        state,
        &member,
        &format!("{BASE}/names?names=acme/My+Fork,my%20fork"),
    )
    .await?;
    assert_eq!(status, StatusCode::OK, "{named}");
    assert_eq!(named["entries"][0]["name"], "My Fork", "{named}");
    assert_eq!(
        named["entries"][0]["members"].as_array().map(Vec::len),
        Some(2),
        "{named}"
    );
    assert_eq!(named["unknown"], json!([]), "{named}");
    Ok(())
}

#[tokio::test]
async fn short_list_selects_and_reports_every_withheld_member() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let mut sql = sql_state_or_skip!(dir.path());
    let mut config = (*sql.state.config).clone();
    config.descriptor_catalog.offer_rules.extend([
        OfferRule {
            runtime_type: pipette_plan_types::RuntimeType::LlamacppCliStockTools,
            flavor: "macos-arm64".into(),
            requires: [
                "os:macos".to_owned(),
                "chip:applem4".to_owned(),
                "runtime:llama_cpp".to_owned(),
                "tag:ready".to_owned(),
            ]
            .into(),
            any_of: Vec::new(),
        },
        OfferRule {
            runtime_type: pipette_plan_types::RuntimeType::LlamacppCliStockTools,
            flavor: "macos-x64".into(),
            requires: ["os:macos".to_owned()].into(),
            any_of: Vec::new(),
        },
    ]);
    sql.state.config = std::sync::Arc::new(config);
    let state = &sql.state;
    let store = store_of(state)?;
    for (name, second, flavor, status) in [
        ("Pinned-1", 500, "macos-arm64", EntryState::Pinned),
        ("Mixed-1", 400, "macos-arm64", EntryState::Active),
        ("No-Offer-1", 300, "linux-x64-rocm", EntryState::Active),
        ("Hidden-1", 200, "linux-x64-rocm", EntryState::Hidden),
        ("Gone-1", 100, "linux-x64-rocm", EntryState::Tombstoned),
    ] {
        store
            .create_entry(&a_descriptor_entry(
                name,
                Kind::Stable,
                second,
                a_member("b1", flavor)?,
            )?)
            .await?;
        if status != EntryState::Active {
            store
                .set_state(
                    &pipette_plan_types::CatalogScope::Global,
                    &CatalogName::try_new(name)?,
                    status,
                )
                .await?;
        }
    }
    let mut org_only = a_descriptor_entry(
        "Org-Only-1",
        Kind::HandMade,
        600,
        a_member("b1", "macos-arm64")?,
    )?;
    org_only.scope = CatalogScope::Org(pipette_plan_types::OrgId::try_new("org_acme")?);
    store.create_entry(&org_only).await?;
    store
        .append_members(
            &pipette_plan_types::CatalogScope::Global,
            &CatalogName::try_new("Mixed-1")?,
            NonEmptyVec::try_new(vec![
                a_member("b1", "linux-x64-rocm")?,
                a_member("b1", "macos-x64")?,
            ])?,
        )
        .await?;
    let (_, client_id) = register_pending(state).await?;
    let mut client = state
        .client_store
        .get_client(&client_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("registered client missing"))?;
    client.device_profile = DeviceProfile {
        device_os_name: Some(NonEmptyTrimmedString::try_new("macOS")?),
        device_chip_model: Some(NonEmptyTrimmedString::try_new("Apple M4")?),
        ..Default::default()
    };
    client.capabilities.insert("runtime:llama_cpp".into());
    state.client_store.put_client(&client).await?;
    let ready = Tag::try_new("ready")?;
    state.tag_store.add_client_tag(&client_id, &ready).await?;
    let signer = registered_user(state, &[UserPermission::ReadClients]).await?;
    let uri = format!("{BASE}/devices/{client_id}");
    let (status, body) = read(state, &signer, &uri).await?;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["next_cursor"], Value::Null);
    assert_eq!(body["unreadable"], 0);
    assert_eq!(body["items"].as_array().map(Vec::len), Some(2));
    assert_eq!(body["items"][0]["name"], "Pinned-1");
    assert_eq!(body["items"][1]["name"], "Mixed-1");
    assert_eq!(body["items"][1]["descriptor_kind"], "runtime");
    assert_eq!(
        body["items"][1]["members"].as_array().map(Vec::len),
        Some(1)
    );
    assert_eq!(
        body["items"][1]["members"][0]["member_key"],
        json!({"kind":"runtime","flavor":"macos-arm64"})
    );
    assert_eq!(
        body["items"][1]["members"][0]["descriptor"]["flavor"],
        "macos-arm64"
    );
    assert_eq!(body["withheld"]["member_count"], 2);
    assert_eq!(body["withheld"]["entry_count"], 1);
    assert_eq!(body["withheld"]["members"][0]["name"], "Mixed-1");
    assert_eq!(body["withheld"]["members"][0]["reason_code"], "no_rule");
    assert!(
        body["withheld"]["members"][0]["reason"]
            .as_str()
            .unwrap_or_default()
            .contains("linux-x64-rocm")
    );
    assert_eq!(body["withheld"]["members"][1]["name"], "No-Offer-1");
    crate::api_schema::assert_conforms("DescriptorCatalogShortList", &body)?;

    state
        .tag_store
        .remove_client_tag(&client_id, &ready)
        .await?;
    let (status, without_tag) = read(state, &signer, &uri).await?;
    assert_eq!(status, StatusCode::OK, "{without_tag}");
    assert_eq!(without_tag["items"].as_array().map(Vec::len), Some(1));
    assert_eq!(without_tag["items"][0]["name"], "Mixed-1");
    assert_eq!(
        without_tag["items"][0]["members"][0]["member_key"]["flavor"],
        "macos-x64"
    );
    assert_eq!(without_tag["withheld"]["member_count"], 4);
    assert_eq!(without_tag["withheld"]["entry_count"], 2);
    assert_eq!(
        without_tag["withheld"]["members"][0]["reason_code"],
        "not_met"
    );
    Ok(())
}

#[tokio::test]
async fn short_list_uses_fleet_read_gate_and_unknown_device_is_404() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let sql = sql_state_or_skip!(dir.path());
    let state = &sql.state;
    let (_, client_id) = register_pending(state).await?;
    let uri = format!("{BASE}/devices/{client_id}");
    let without_fleet = registered_user(state, &[]).await?;
    assert_eq!(
        read(state, &without_fleet, &uri).await?.0,
        StatusCode::FORBIDDEN
    );
    let with_fleet = registered_user(state, &[UserPermission::ReadClients]).await?;
    let unknown = format!("{BASE}/devices/ev1_unknown");
    assert_eq!(
        read(state, &with_fleet, &unknown).await?.0,
        StatusCode::NOT_FOUND
    );
    Ok(())
}

#[tokio::test]
async fn short_list_drains_every_store_page() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let mut sql = sql_state_or_skip!(dir.path());
    let mut config = (*sql.state.config).clone();
    config.descriptor_catalog.offer_rules.push(OfferRule {
        runtime_type: pipette_plan_types::RuntimeType::LlamacppCliStockTools,
        flavor: "macos-arm64".into(),
        requires: ["os:macos".to_owned()].into(),
        any_of: Vec::new(),
    });
    sql.state.config = std::sync::Arc::new(config);
    let state = &sql.state;
    let store = store_of(state)?;
    for index in 0..201 {
        store
            .create_entry(&a_descriptor_entry(
                &format!("Entry-{index:03}"),
                Kind::HandMade,
                index + 1000,
                a_member("b1", "macos-arm64")?,
            )?)
            .await?;
    }
    let (_, client_id) = register_pending(state).await?;
    let mut client = state
        .client_store
        .get_client(&client_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("registered client missing"))?;
    client.device_profile.device_os_name = Some(NonEmptyTrimmedString::try_new("macOS")?);
    state.client_store.put_client(&client).await?;
    let signer = registered_user(state, &[UserPermission::ReadClients]).await?;
    let (status, body) = read(state, &signer, &format!("{BASE}/devices/{client_id}")).await?;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["items"].as_array().map(Vec::len), Some(201));
    assert_eq!(body["items"][0]["name"], "Entry-200");
    assert_eq!(body["items"][200]["name"], "Entry-000");
    assert_eq!(body["next_cursor"], Value::Null);
    assert_eq!(body["unreadable"], 0);
    assert_eq!(body["withheld"]["member_count"], 0);
    Ok(())
}

/// A Windows device, and a reader who may read it. The offer rules offer the
/// device llama.cpp `windows-x64-cpu` and `ovms` `windows-x64`. Returns the
/// short list's URI and the reader's signer.
async fn windows_device(
    sql: &mut SqlState,
) -> anyhow::Result<(
    String,
    (
        ed25519_dalek::SigningKey,
        pipette_plan_types::UserId,
        pipette_plan_types::UserKeyId,
    ),
)> {
    let mut config = (*sql.state.config).clone();
    config.descriptor_catalog.offer_rules.extend([
        OfferRule {
            runtime_type: pipette_plan_types::RuntimeType::LlamacppCliStockTools,
            flavor: "windows-x64-cpu".into(),
            requires: ["os:windows".to_owned()].into(),
            any_of: Vec::new(),
        },
        OfferRule {
            runtime_type: pipette_plan_types::RuntimeType::Ovms,
            flavor: "windows-x64".into(),
            requires: ["os:windows".to_owned()].into(),
            any_of: Vec::new(),
        },
    ]);
    sql.state.config = std::sync::Arc::new(config);
    let state = &sql.state;
    let (_, client_id) = register_pending(state).await?;
    let mut client = state
        .client_store
        .get_client(&client_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("registered client missing"))?;
    client.device_profile.device_os_name = Some(NonEmptyTrimmedString::try_new("Windows")?);
    state.client_store.put_client(&client).await?;
    let signer = registered_user(state, &[UserPermission::ReadClients]).await?;
    Ok((format!("{BASE}/devices/{client_id}"), signer))
}

/// A runtime member of `runtime_type`, `llamacpp_cli_stock_tools` or `ovms`,
/// with `flavor`.
fn a_runtime_member(
    runtime_type: &str,
    flavor: &str,
) -> anyhow::Result<pipette_plan_types::DescriptorMember> {
    if runtime_type != "ovms" {
        return a_member("b1", flavor);
    }
    Ok(pipette_plan_types::DescriptorMember::new(
        serde_json::from_value(json!({
            "type": "ovms", "source": "github_release",
            "repository_url": "github.com/openvinotoolkit/model_server",
            "repository_version": "v2026.4.0", "flavor": flavor,
        }))?,
        chrono::Utc::now(),
    )?)
}

/// The `key` field of each object in the array `array`.
fn field_of(array: &Value, key: &str) -> Option<Vec<Value>> {
    array
        .as_array()
        .map(|items| items.iter().map(|item| item[key].clone()).collect())
}

/// A model entry with one member for each of `members`: a model type, its
/// repository and its quantization.
fn a_model_entry_of(
    name: &str,
    release_second: i64,
    members: &[(&str, &str, &str)],
) -> anyhow::Result<pipette_plan_types::DescriptorEntry> {
    let members = members
        .iter()
        .map(|(model_type, repo, quantization)| a_model_member(model_type, repo, quantization))
        .collect::<anyhow::Result<Vec<_>>>()?;
    let mut entry = a_descriptor_entry(
        name,
        Kind::HandMade,
        release_second,
        members.first().cloned().context("a member")?,
    )?;
    entry.members = NonEmptyVec::try_new(members)?;
    Ok(entry)
}

/// The model entry `AcmeLM-3B` has release second 150: between the two
/// runtime entries, so only grouping by kind puts it last.
#[rstest]
#[case::loadable_members_offered(
    &[
        ("llamacpp-v0.7.0", 200, "llamacpp_cli_stock_tools", "windows-x64-cpu"),
        ("ovms-v2026.4.0", 100, "ovms", "windows-x64"),
    ],
    &[
        ("gguf_text", "AcmeLM-3B-GGUF", "q4_0"),
        ("openvino", "AcmeLM-3B-int4-ov", "int4"),
        ("onnx_genai", "AcmeLM-3B-onnx", "int4"),
        ("mlx", "AcmeLM-3B-MLX-4bit", "4bit"),
    ],
    &["llamacpp-v0.7.0", "ovms-v2026.4.0", "AcmeLM-3B"],
    &["AcmeLM-3B::gguf::q4_0", "AcmeLM-3B::openvino::int4"],
    0,
    &[
        ("AcmeLM-3B::onnx::int4", "no offered runtime loads `onnx_genai`"),
        ("AcmeLM-3B::mlx::4bit", "no offered runtime loads `mlx`"),
    ],
)]
#[case::no_runtime_offered(
    &[],
    &[
        ("gguf_text", "AcmeLM-3B-GGUF", "q4_0"),
        ("onnx_genai", "AcmeLM-3B-onnx", "int4"),
    ],
    &[],
    &[],
    1,
    &[
        ("AcmeLM-3B::gguf::q4_0", "no offered runtime loads `gguf_text`"),
        ("AcmeLM-3B::onnx::int4", "no offered runtime loads `onnx_genai`"),
    ],
)]
#[tokio::test]
async fn short_list_offers_model_members_that_an_offered_runtime_loads(
    #[case] runtime_entries: &[(&str, i64, &str, &str)],
    #[case] model_members: &[(&str, &str, &str)],
    #[case] expected_items: &[&str],
    #[case] expected_offered: &[&str],
    #[case] expected_withheld_entries: usize,
    #[case] expected_withheld: &[(&str, &str)],
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let mut sql = sql_state_or_skip!(dir.path());
    let (uri, signer) = windows_device(&mut sql).await?;
    let state = &sql.state;
    let store = store_of(state)?;
    for (name, second, runtime_type, flavor) in runtime_entries {
        store
            .create_entry(&a_descriptor_entry(
                name,
                Kind::Stable,
                *second,
                a_runtime_member(runtime_type, flavor)?,
            )?)
            .await?;
    }
    store
        .create_entry(&a_model_entry_of("AcmeLM-3B", 150, model_members)?)
        .await?;
    let (status, body) = read(state, &signer, &uri).await?;
    assert_eq!(status, StatusCode::OK, "{body}");
    let expected_items = expected_items.iter().map(|name| json!(name)).collect();
    assert_eq!(
        field_of(&body["items"], "name"),
        Some(expected_items),
        "{body}"
    );
    let model = body["items"]
        .as_array()
        .and_then(|items| items.iter().find(|item| item["name"] == "AcmeLM-3B"));
    if let Some(model) = model {
        assert_eq!(model["descriptor_kind"], "model");
    }
    let offered = model
        .and_then(|model| field_of(&model["members"], "selector"))
        .unwrap_or_default();
    let expected_offered: Vec<Value> = expected_offered.iter().map(|s| json!(s)).collect();
    assert_eq!(offered, expected_offered);
    assert_eq!(body["withheld"]["entry_count"], expected_withheld_entries);
    assert_eq!(body["withheld"]["member_count"], expected_withheld.len());
    let withheld = body["withheld"]["members"]
        .as_array()
        .map(|members| {
            members
                .iter()
                .map(|member| {
                    (
                        member["name"].clone(),
                        member["selector"].clone(),
                        member["reason_code"].clone(),
                        member["reason"].clone(),
                    )
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let expected_withheld: Vec<_> = expected_withheld
        .iter()
        .map(|(selector, reason)| {
            (
                json!("AcmeLM-3B"),
                json!(selector),
                json!("no_runtime"),
                json!(reason),
            )
        })
        .collect();
    assert_eq!(withheld, expected_withheld);
    crate::api_schema::assert_conforms("DescriptorCatalogShortList", &body)?;
    Ok(())
}

#[tokio::test]
async fn model_members_carry_their_parameter_counts() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let sql = sql_state_or_skip!(dir.path());
    let state = &sql.state;
    store_of(state)?
        .create_entry(&a_model_entry_of(
            "AcmeLM-3B",
            50,
            &[
                ("gguf_text", "AcmeLM-3B-GGUF", "q4_0"),
                ("openvino", "AcmeLM-3B-ov", "int4"),
            ],
        )?)
        .await?;
    state
        .model_params_store
        .put_model_params(&pipette_store_testkit::a_model_params("acmelm-3b", 3100)?)
        .await?;
    let signer = registered_user(state, &[]).await?;
    let (status, lookup) = read(state, &signer, &format!("{BASE}/names?names=AcmeLM-3B")).await?;
    assert_eq!(status, StatusCode::OK, "{lookup}");
    let (status, full) = read(
        state,
        &signer,
        &format!("{BASE}?descriptor_kind=model&members=true"),
    )
    .await?;
    assert_eq!(status, StatusCode::OK, "{full}");
    for members in [
        &lookup["entries"][0]["members"],
        &full["items"][0]["members"],
    ] {
        assert_eq!(members[0]["model_key"], "acmelm-3b", "{members}");
        assert_eq!(
            members[0]["parameters"],
            json!({"total_millions": 3100, "active_millions": 3100})
        );
        assert_eq!(members[1]["model_key"], "acmelm-3b-ov", "{members}");
        assert!(members[1].get("parameters").is_none(), "{members}");
    }
    let (_, compact) = read(state, &signer, &format!("{BASE}?descriptor_kind=model")).await?;
    assert!(compact["items"][0].get("members").is_none(), "{compact}");
    Ok(())
}

fn store_of(state: &AppState) -> anyhow::Result<&dyn DescriptorCatalogStore> {
    state
        .descriptor_catalog_store
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("SQL catalog"))
}

/// Two organizations, their members, and a server admin in neither. Global
/// `x` and acme's `x` share digest `shared`, with a global stable and a global
/// daily name. acme's `y` is tombstoned, so it does not shadow global `y`.
struct Scoped {
    acme: pipette_plan_types::OrgId,
    beta: pipette_plan_types::OrgId,
    acme_member: UserSigner,
    beta_member: UserSigner,
    admin: UserSigner,
    shared: String,
}

impl Scoped {
    fn reader(&self, who: &str) -> anyhow::Result<&UserSigner> {
        match who {
            "acme" => Ok(&self.acme_member),
            "beta" => Ok(&self.beta_member),
            "admin" => Ok(&self.admin),
            other => anyhow::bail!("unknown reader {other}"),
        }
    }

    /// `scope` as the table names it: `global`, `acme`, `beta`, or the raw
    /// value.
    fn scope_label(&self, scope: &Value) -> String {
        match scope.as_str() {
            Some("global") => "global".into(),
            Some(id) if id == self.acme.as_str() => "acme".into(),
            Some(id) if id == self.beta.as_str() => "beta".into(),
            other => format!("{other:?}"),
        }
    }
}

async fn scoped_catalog(state: &AppState) -> anyhow::Result<Scoped> {
    let store = store_of(state)?;
    let acme = seed_org(state, "acme").await?.org_id;
    let beta = seed_org(state, "beta").await?.org_id;
    let acme_member = registered_user(state, &[]).await?;
    set_user_org(state, &acme_member.1, Some(acme.clone())).await?;
    let beta_member = registered_user(state, &[]).await?;
    set_user_org(state, &beta_member.1, Some(beta.clone())).await?;
    let admin = registered_user(state, &[UserPermission::ManageAnyOrgs]).await?;
    let shared = a_member("b1", "macos-arm64")?;
    let other = a_member("b2", "macos-arm64")?;
    let acme_scope = CatalogScope::Org(acme.clone());
    for (scope, name, kind, second, member) in [
        (&acme_scope, "Org-Only-1", Kind::HandMade, 300, &other),
        (&acme_scope, "x", Kind::HandMade, 200, &shared),
        (&CatalogScope::Global, "x", Kind::HandMade, 100, &shared),
        (&acme_scope, "y", Kind::HandMade, 70, &other),
        (&CatalogScope::Global, "y", Kind::HandMade, 60, &other),
        (&CatalogScope::Global, "Stable-1", Kind::Stable, 50, &shared),
        (&CatalogScope::Global, "Daily-1", Kind::Daily, 40, &shared),
    ] {
        let mut entry = a_descriptor_entry(name, kind, second, member.clone())?;
        entry.scope = scope.clone();
        store.create_entry(&entry).await?;
    }
    store
        .set_state(
            &acme_scope,
            &CatalogName::try_new("y")?,
            EntryState::Tombstoned,
        )
        .await?;
    Ok(Scoped {
        acme,
        beta,
        acme_member,
        beta_member,
        admin,
        shared: shared.digest().to_owned(),
    })
}

fn displayed(fixture: &Scoped, rows: &Value) -> Vec<(String, String)> {
    rows.as_array()
        .into_iter()
        .flatten()
        .map(|row| {
            (
                row["display_name"].as_str().unwrap_or_default().to_owned(),
                fixture.scope_label(&row["scope"]),
            )
        })
        .collect()
}

/// The first member selector of each row.
fn first_selectors(rows: &Value) -> Vec<String> {
    rows.as_array()
        .into_iter()
        .flatten()
        .map(|row| {
            row.pointer("/member_summaries/0/selector")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned()
        })
        .collect()
}

/// The selector of each expected row: its display name and the one flavor of
/// the fixture's members.
fn selectors(expected: &[(&str, &str)]) -> Vec<String> {
    expected
        .iter()
        .map(|(name, _)| format!("{name}::macos-arm64"))
        .collect()
}

fn pairs(expected: &[(&str, &str)]) -> Vec<(String, String)> {
    expected
        .iter()
        .map(|(name, scope)| ((*name).to_owned(), (*scope).to_owned()))
        .collect()
}

const OUTSIDER_LIST: &[(&str, &str)] = &[
    ("x", "global"),
    ("y", "global"),
    ("Stable-1", "global"),
    ("Daily-1", "global"),
];
const OUTSIDER_NAMES: &[(&str, &str)] = &[("x", "global"), ("y", "global")];
const OUTSIDER_UNKNOWN: &[&str] = &["ACME/x", "Org-Only-1", "NoSuch/x", "GLOBAL/Missing"];
const OUTSIDER_DIGEST: &[(&str, &str)] = &[
    ("x", "global"),
    ("Stable-1", "global"),
    ("Daily-1", "global"),
];

/// Each reader sees the entries of its scopes, with the names that it reads.
/// A server admin without `edit_any_catalog` reads as an outsider.
#[rstest]
#[case::acme_member(
    "acme",
    &[
        ("Org-Only-1", "acme"),
        ("x", "acme"),
        ("global/x", "global"),
        ("y", "global"),
        ("Stable-1", "global"),
        ("Daily-1", "global"),
    ],
    &[("x", "acme"), ("global/x", "global"), ("Org-Only-1", "acme"), ("y", "global")],
    &["NoSuch/x", "GLOBAL/Missing"],
    &[("x", "acme"), ("global/x", "global"), ("Stable-1", "global"), ("Daily-1", "global")],
)]
#[case::beta_member(
    "beta",
    OUTSIDER_LIST,
    OUTSIDER_NAMES,
    OUTSIDER_UNKNOWN,
    OUTSIDER_DIGEST
)]
#[case::server_admin(
    "admin",
    OUTSIDER_LIST,
    OUTSIDER_NAMES,
    OUTSIDER_UNKNOWN,
    OUTSIDER_DIGEST
)]
#[tokio::test]
async fn reads_follow_the_reader_scope(
    #[case] who: &str,
    #[case] list: &[(&str, &str)],
    #[case] names: &[(&str, &str)],
    #[case] unknown: &[&str],
    #[case] digest: &[(&str, &str)],
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let sql = sql_state_or_skip!(dir.path());
    let state = &sql.state;
    let fixture = scoped_catalog(state).await?;
    let reader = fixture.reader(who)?;

    let (status, listed) = read(state, reader, BASE).await?;
    assert_eq!(status, StatusCode::OK, "{listed}");
    assert_eq!(displayed(&fixture, &listed["items"]), pairs(list));
    assert_eq!(first_selectors(&listed["items"]), selectors(list));

    // `unknown` gives back each name as the caller wrote it.
    let query = "x,global/x,ACME/x,Org-Only-1,y,NoSuch/x,GLOBAL/Missing";
    let (status, named) = read(state, reader, &format!("{BASE}/names?names={query}")).await?;
    assert_eq!(status, StatusCode::OK, "{named}");
    assert_eq!(displayed(&fixture, &named["entries"]), pairs(names));
    assert_eq!(first_selectors(&named["entries"]), selectors(names));
    assert_eq!(named["unknown"], json!(unknown));

    let shared = &fixture.shared;
    let (status, hits) = read(state, reader, &format!("{BASE}/by-digest?digests={shared}")).await?;
    assert_eq!(status, StatusCode::OK, "{hits}");
    let hits = &hits["names"][shared];
    assert_eq!(displayed(&fixture, hits), pairs(digest));
    assert_eq!(
        hits.as_array()
            .into_iter()
            .flatten()
            .map(|hit| hit["selector"].as_str().unwrap_or_default())
            .collect::<Vec<_>>(),
        selectors(digest)
    );
    Ok(())
}

/// A holder of `edit_any_catalog` reads every organization. An affiliated
/// holder sees the entries of its own organization with bare names, and these
/// names shadow global names.
#[rstest]
#[case::unaffiliated(
    "admin",
    &[UserPermission::ManageAnyOrgs, UserPermission::EditAnyCatalog],
    &[
        ("acme/Org-Only-1", "acme"),
        ("beta/Foreign-1", "beta"),
        ("acme/x", "acme"),
        ("x", "global"),
        ("y", "global"),
        ("Stable-1", "global"),
        ("Daily-1", "global"),
    ],
    &[("x", "global"), ("beta/Foreign-1", "beta")],
    &[
        ("x", "global"),
        ("Stable-1", "global"),
        ("Daily-1", "global"),
        ("acme/x", "acme"),
        ("beta/Foreign-1", "beta"),
    ],
)]
#[case::affiliated(
    "acme",
    &[UserPermission::EditAnyCatalog],
    &[
        ("Org-Only-1", "acme"),
        ("beta/Foreign-1", "beta"),
        ("x", "acme"),
        ("global/x", "global"),
        ("y", "global"),
        ("Stable-1", "global"),
        ("Daily-1", "global"),
    ],
    &[("x", "acme"), ("beta/Foreign-1", "beta")],
    &[
        ("x", "acme"),
        ("global/x", "global"),
        ("Stable-1", "global"),
        ("Daily-1", "global"),
        ("beta/Foreign-1", "beta"),
    ],
)]
#[tokio::test]
async fn edit_any_catalog_reads_every_org(
    #[case] who: &str,
    #[case] grants: &[UserPermission],
    #[case] list: &[(&str, &str)],
    #[case] names: &[(&str, &str)],
    #[case] digest: &[(&str, &str)],
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let sql = sql_state_or_skip!(dir.path());
    let state = &sql.state;
    let fixture = scoped_catalog(state).await?;
    let mut foreign = a_descriptor_entry(
        "Foreign-1",
        Kind::HandMade,
        250,
        a_member("b1", "macos-arm64")?,
    )?;
    foreign.scope = CatalogScope::Org(fixture.beta.clone());
    store_of(state)?.create_entry(&foreign).await?;
    let reader = fixture.reader(who)?;
    grant_user(state, &reader.1, grants).await?;

    let (status, listed) = read(state, reader, BASE).await?;
    assert_eq!(status, StatusCode::OK, "{listed}");
    assert_eq!(displayed(&fixture, &listed["items"]), pairs(list));

    let (status, beta_only) = read(state, reader, &format!("{BASE}?scope=beta")).await?;
    assert_eq!(status, StatusCode::OK, "{beta_only}");
    assert_eq!(
        displayed(&fixture, &beta_only["items"]),
        pairs(&[("beta/Foreign-1", "beta")])
    );

    // A bare `x` alone shows which scope the reader resolves it in.
    let (status, named) = read(
        state,
        reader,
        &format!("{BASE}/names?names=x,beta/Foreign-1"),
    )
    .await?;
    assert_eq!(status, StatusCode::OK, "{named}");
    assert_eq!(displayed(&fixture, &named["entries"]), pairs(names));
    assert_eq!(named["unknown"], json!([]));

    let shared = &fixture.shared;
    let (status, hits) = read(state, reader, &format!("{BASE}/by-digest?digests={shared}")).await?;
    assert_eq!(status, StatusCode::OK, "{hits}");
    assert_eq!(displayed(&fixture, &hits["names"][shared]), pairs(digest));
    Ok(())
}

/// A service account never holds `edit_any_catalog`. With the name in its
/// stored set, it reads only its own organization.
#[tokio::test]
async fn a_service_account_never_reads_every_org() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let sql = sql_state_or_skip!(dir.path());
    let state = &sql.state;
    let fixture = scoped_catalog(state).await?;
    let mut foreign = a_descriptor_entry(
        "Foreign-1",
        Kind::HandMade,
        250,
        a_member("b1", "macos-arm64")?,
    )?;
    foreign.scope = CatalogScope::Org(fixture.beta.clone());
    store_of(state)?.create_entry(&foreign).await?;
    let service = registered_user(state, &[UserPermission::EditAnyCatalog]).await?;
    make_service_account(state, &service.1, Some(fixture.acme.clone())).await?;

    let (status, listed) = read(state, &service, BASE).await?;
    assert_eq!(status, StatusCode::OK, "{listed}");
    assert_eq!(
        displayed(&fixture, &listed["items"]),
        pairs(&[
            ("Org-Only-1", "acme"),
            ("x", "acme"),
            ("global/x", "global"),
            ("y", "global"),
            ("Stable-1", "global"),
            ("Daily-1", "global"),
        ])
    );

    let (status, body) = read(state, &service, &format!("{BASE}?scope=beta")).await?;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    Ok(())
}

/// The `scope` filter keeps one readable scope.
#[rstest]
#[case::global("acme", "scope=global", &["global/x", "y", "Stable-1", "Daily-1"])]
#[case::own_slug("acme", "scope=acme", &["Org-Only-1", "x"])]
#[case::own_slug_ignores_case("acme", "scope=ACME", &["Org-Only-1", "x"])]
#[case::outsider_global("beta", "scope=global", &["x", "y", "Stable-1", "Daily-1"])]
#[tokio::test]
async fn the_scope_filter_keeps_one_scope(
    #[case] who: &str,
    #[case] query: &str,
    #[case] expected: &[&str],
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let sql = sql_state_or_skip!(dir.path());
    let state = &sql.state;
    let fixture = scoped_catalog(state).await?;
    let (status, listed) = read(state, fixture.reader(who)?, &format!("{BASE}?{query}")).await?;
    assert_eq!(status, StatusCode::OK, "{listed}");
    let shown: Vec<String> = displayed(&fixture, &listed["items"])
        .into_iter()
        .map(|(name, _)| name)
        .collect();
    assert_eq!(shown, expected);
    Ok(())
}

/// The org id selects the same scope as the slug, and a page that ends on an
/// org entry resumes after it.
#[tokio::test]
async fn the_list_resumes_across_scopes() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let sql = sql_state_or_skip!(dir.path());
    let state = &sql.state;
    let fixture = scoped_catalog(state).await?;
    let reader = &fixture.acme_member;
    let acme = &fixture.acme;
    let (status, by_id) = read(state, reader, &format!("{BASE}?scope={acme}")).await?;
    assert_eq!(status, StatusCode::OK, "{by_id}");
    assert_eq!(by_id["items"].as_array().map(Vec::len), Some(2));

    let (status, first) = read(state, reader, &format!("{BASE}?limit=2")).await?;
    assert_eq!(status, StatusCode::OK, "{first}");
    assert_eq!(first["next_cursor"], json!(format!("{acme}/x")));
    let cursor = first["next_cursor"].as_str().unwrap_or_default();
    let (status, rest) = read(state, reader, &format!("{BASE}?cursor={cursor}")).await?;
    assert_eq!(status, StatusCode::OK, "{rest}");
    let shown: Vec<String> = displayed(&fixture, &rest["items"])
        .into_iter()
        .map(|(name, _)| name)
        .collect();
    assert_eq!(shown, ["global/x", "y", "Stable-1", "Daily-1"]);

    // An outsider cannot resume from a cursor in a scope that it cannot read.
    let (status, _) = read(
        state,
        &fixture.beta_member,
        &format!("{BASE}?cursor={cursor}"),
    )
    .await?;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    Ok(())
}

#[rstest]
#[case::other_org("beta", "?scope=acme", StatusCode::NOT_FOUND)]
#[case::server_admin("admin", "?scope=acme", StatusCode::NOT_FOUND)]
#[case::unknown_org("acme", "?scope=nosuch", StatusCode::NOT_FOUND)]
#[case::malformed_scope("acme", "?scope=not%20a%20slug", StatusCode::BAD_REQUEST)]
#[case::malformed_name("acme", "/names?names=acme/team/x", StatusCode::BAD_REQUEST)]
#[case::malformed_qualifier("acme", "/names?names=not%20a%20slug/x", StatusCode::BAD_REQUEST)]
#[tokio::test]
async fn scoped_reads_refuse(
    #[case] who: &str,
    #[case] path: &str,
    #[case] expected: StatusCode,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let sql = sql_state_or_skip!(dir.path());
    let state = &sql.state;
    let fixture = scoped_catalog(state).await?;
    let (status, body) = read(state, fixture.reader(who)?, &format!("{BASE}{path}")).await?;
    assert_eq!(status, expected, "{body}");
    Ok(())
}

async fn read(
    state: &AppState,
    signer: &UserSigner,
    uri: &str,
) -> anyhow::Result<(StatusCode, Value)> {
    let response = signed_empty(state, signer, "GET", uri).await?;
    let status = response.status();
    Ok((status, body_json(response).await?))
}

#[rstest]
#[case::kind("kind=daily", "Daily-1")]
#[case::state("state=hidden", "Hidden-1")]
#[case::runtime_type("descriptor_kind=runtime&type=apple_foundation", "Apple-1")]
#[case::prefix_ignores_case("prefix=dAi", "Daily-1")]
#[case::model_kind("descriptor_kind=model", "AcmeLM-3B")]
#[case::second_model_type("type=mlx", "AcmeLM-3B")]
#[tokio::test]
async fn filters(#[case] query: &str, #[case] expected: &str) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let sql = sql_state_or_skip!(dir.path());
    let state = &sql.state;
    let store = store_of(state)?;
    for (name, kind, version) in [
        ("Daily-1", Kind::Daily, "b1"),
        ("Hidden-1", Kind::Stable, "b2"),
    ] {
        let entry = a_descriptor_entry(name, kind, 200, a_member(version, "macos-arm64")?)?;
        store.create_entry(&entry).await?;
    }
    store
        .set_state(
            &pipette_plan_types::CatalogScope::Global,
            &CatalogName::try_new("Hidden-1")?,
            EntryState::Hidden,
        )
        .await?;
    let apple = pipette_plan_types::DescriptorMember::new(
        serde_json::from_value(json!({"type":"apple_foundation"}))?,
        chrono::Utc::now(),
    )?;
    store
        .create_entry(&a_descriptor_entry("Apple-1", Kind::HandMade, 100, apple)?)
        .await?;
    store.create_entry(&a_model_entry()?).await?;
    let signer = registered_user(state, &[]).await?;
    let (status, page) = read(state, &signer, &format!("{BASE}?{query}")).await?;
    assert_eq!(status, StatusCode::OK, "{page}");
    assert_eq!(page["items"].as_array().map(Vec::len), Some(1), "{page}");
    assert_eq!(page["items"][0]["name"], expected);
    Ok(())
}

fn a_model_entry() -> anyhow::Result<pipette_plan_types::DescriptorEntry> {
    a_model_entry_of(
        "AcmeLM-3B",
        50,
        &[
            ("gguf_text", "AcmeLM-3B-GGUF", "q4_0"),
            ("mlx", "AcmeLM-3B-MLX-4bit", "4bit"),
        ],
    )
}

#[tokio::test]
async fn a_model_entry_reads_with_its_member_keys() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let sql = sql_state_or_skip!(dir.path());
    let state = &sql.state;
    let store = store_of(state)?;
    let entry = a_model_entry()?;
    store.create_entry(&entry).await?;
    let signer = registered_user(state, &[]).await?;
    let (status, full) = read(state, &signer, &format!("{BASE}?members=true")).await?;
    assert_eq!(status, StatusCode::OK, "{full}");
    let row = &full["items"][0];
    assert_eq!(row["descriptor_kind"], "model");
    assert_eq!(row["descriptor_types"], json!(["gguf_text", "mlx"]));
    assert_eq!(
        row["member_summaries"],
        json!([
            {
                "selector": "AcmeLM-3B::gguf::q4_0",
                "member_key": {"kind": "model", "format": "gguf", "quantization": "q4_0"}
            },
            {
                "selector": "AcmeLM-3B::mlx::4bit",
                "member_key": {"kind": "model", "format": "mlx", "quantization": "4bit"}
            }
        ])
    );
    assert!(row["members"][1].get("coverage").is_none(), "{row}");
    assert_eq!(row["members"][1]["descriptor"]["type"], "mlx");

    let digest = entry.members.last().digest().to_owned();
    let (_, by_digest) = read(
        state,
        &signer,
        &format!("{BASE}/by-digest?digests={digest}"),
    )
    .await?;
    let hit = &by_digest["names"][&digest][0];
    assert_eq!(hit["selector"], "AcmeLM-3B::mlx::4bit", "{by_digest}");
    assert_eq!(hit["descriptor_kind"], "model");
    Ok(())
}

#[tokio::test]
async fn list_paging_members_and_states() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let mut sql = sql_state_or_skip!(dir.path());
    let mut config = (*sql.state.config).clone();
    config.descriptor_catalog.offer_rules.push(OfferRule {
        runtime_type: pipette_plan_types::RuntimeType::LlamacppCliStockTools,
        flavor: "macos-arm64".into(),
        requires: ["os:macos".to_owned()].into(),
        any_of: Vec::new(),
    });
    sql.state.config = std::sync::Arc::new(config);
    let state = &sql.state;
    let store = store_of(state)?;
    for (name, second, version, flavor) in [
        ("Alpha-1", 300, "b1", "macos-arm64"),
        ("Beta-1", 300, "b2", "linux-x64-rocm"),
        ("Hidden-1", 200, "b3", "macos-arm64"),
        ("Gone-1", 100, "b4", "macos-arm64"),
    ] {
        store
            .create_entry(&a_descriptor_entry(
                name,
                Kind::Stable,
                second,
                a_member(version, flavor)?,
            )?)
            .await?;
    }
    store
        .set_state(
            &pipette_plan_types::CatalogScope::Global,
            &CatalogName::try_new("Hidden-1")?,
            EntryState::Hidden,
        )
        .await?;
    store
        .set_state(
            &pipette_plan_types::CatalogScope::Global,
            &CatalogName::try_new("Gone-1")?,
            EntryState::Tombstoned,
        )
        .await?;
    let signer = registered_user(state, &[]).await?;
    let (status, list) = read(state, &signer, BASE).await?;
    assert_eq!(status, StatusCode::OK, "{list}");
    let items = list["items"]
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("items"))?;
    assert_eq!(
        items
            .iter()
            .map(|row| row["name"].as_str().unwrap_or_default())
            .collect::<Vec<_>>(),
        ["Alpha-1", "Beta-1", "Hidden-1"]
    );
    assert_eq!(items[0]["member_count"], 1);
    assert_eq!(items[0]["descriptor_kind"], "runtime");
    assert_eq!(
        items[0]["descriptor_types"],
        json!(["llamacpp_cli_stock_tools"])
    );
    assert_eq!(items[0]["member_summaries"][0]["coverage"], "covered");
    assert_eq!(items[1]["member_summaries"][0]["coverage"], "no_rule");
    assert_eq!(
        items[0]["member_summaries"][0]["member_key"],
        json!({"kind":"runtime","flavor":"macos-arm64"})
    );
    assert_eq!(
        items[0]["member_summaries"][0]["selector"],
        "Alpha-1::macos-arm64"
    );
    assert!(items[0].get("members").is_none());
    let (_, full) = read(state, &signer, &format!("{BASE}?members=true")).await?;
    assert_eq!(full["items"][0]["members"][0]["coverage"], "covered");
    assert!(full["items"][0]["members"][0]["descriptor"].is_object());
    assert_eq!(
        full["items"][0]["members"][0]["member_key"],
        json!({"kind":"runtime","flavor":"macos-arm64"})
    );

    let mut cursor: Option<String> = None;
    let mut names = Vec::new();
    loop {
        let uri = cursor.as_deref().map_or_else(
            || format!("{BASE}?limit=1"),
            |cursor: &str| format!("{BASE}?limit=1&cursor={cursor}"),
        );
        let (_, page) = read(state, &signer, &uri).await?;
        names.push(
            page["items"][0]["name"]
                .as_str()
                .unwrap_or_default()
                .to_owned(),
        );
        cursor = page["next_cursor"].as_str().map(str::to_owned);
        if cursor.is_none() {
            break;
        }
    }
    assert_eq!(names, ["Alpha-1", "Beta-1", "Hidden-1"]);
    let (_, models) = read(
        state,
        &signer,
        &format!("{BASE}?descriptor_kind=model&type=gguf_text"),
    )
    .await?;
    assert_eq!(models["items"], json!([]));
    Ok(())
}

#[tokio::test]
async fn name_and_digest_lookups() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let sql = sql_state_or_skip!(dir.path());
    let state = &sql.state;
    let store = store_of(state)?;
    let member = a_member("b1", "macos-arm64")?;
    let digest = member.digest().to_owned();
    for (name, kind) in [
        ("Stable-1", Kind::Stable),
        ("Daily-1", Kind::Daily),
        ("Made-1", Kind::HandMade),
        ("Gone-1", Kind::Stable),
    ] {
        store
            .create_entry(&a_descriptor_entry(name, kind, 100, member.clone())?)
            .await?;
    }
    store
        .set_state(
            &pipette_plan_types::CatalogScope::Global,
            &CatalogName::try_new("Stable-1")?,
            EntryState::Hidden,
        )
        .await?;
    store
        .set_state(
            &pipette_plan_types::CatalogScope::Global,
            &CatalogName::try_new("Gone-1")?,
            EntryState::Tombstoned,
        )
        .await?;
    let signer = registered_user(state, &[]).await?;
    let (_, by_name) = read(
        state,
        &signer,
        &format!("{BASE}/names?names=stable-1,STABLE-1,Gone-1,Missing-1"),
    )
    .await?;
    assert_eq!(
        by_name["entries"].as_array().map(Vec::len),
        Some(1),
        "{by_name}"
    );
    assert_eq!(by_name["entries"][0]["name"], "Stable-1");
    assert!(by_name["entries"][0]["members"].is_array());
    assert_eq!(by_name["unknown"], json!(["Gone-1", "Missing-1"]));
    let unknown_digest = "0".repeat(64);
    let (_, by_digest) = read(
        state,
        &signer,
        &format!("{BASE}/by-digest?digests={digest},{unknown_digest}"),
    )
    .await?;
    let hits = by_digest["names"][&digest]
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("digest hits: {by_digest}"))?;
    assert_eq!(
        hits.iter()
            .map(|hit| hit["name"].as_str().unwrap_or_default())
            .collect::<Vec<_>>(),
        ["Made-1", "Stable-1", "Daily-1"]
    );
    assert_eq!(hits[0]["selector"], "Made-1::macos-arm64");
    assert_eq!(
        hits[0]["member_key"],
        json!({"kind":"runtime","flavor":"macos-arm64"})
    );
    assert_eq!(hits[0]["descriptor_kind"], "runtime");
    assert!(by_digest["names"].get(&unknown_digest).is_none());
    Ok(())
}

#[rstest]
#[case::unknown_cursor(format!("{BASE}?cursor=no-such-entry"))]
#[case::tombstoned_state(format!("{BASE}?state=tombstoned"))]
#[case::model_type_as_runtime(format!("{BASE}?descriptor_kind=runtime&type=gguf_text"))]
#[case::names_missing(format!("{BASE}/names"))]
#[case::names_over_cap(format!("{BASE}/names?names={}", vec!["a"; 101].join(",")))]
#[case::digests_missing(format!("{BASE}/by-digest"))]
#[case::digest_invalid(format!("{BASE}/by-digest?digests=invalid"))]
#[case::digests_over_cap(format!("{BASE}/by-digest?digests={}", vec!["0".repeat(64); 101].join(",")))]
#[tokio::test]
async fn rejects_bad_query(#[case] uri: String) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let sql = sql_state_or_skip!(dir.path());
    let signer = registered_user(&sql.state, &[]).await?;
    let (status, body) = read(&sql.state, &signer, &uri).await?;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    Ok(())
}

#[rstest]
#[case::list(BASE.to_owned())]
#[case::for_device(format!("{BASE}/devices/ev1_unknown"))]
#[case::names(format!("{BASE}/names?names=foo"))]
#[case::by_digest(format!("{BASE}/by-digest?digests={}", "0".repeat(64)))]
#[tokio::test]
async fn no_database_returns_404(#[case] uri: String) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let signer = registered_user(&state, &[UserPermission::ReadClients]).await?;
    let (status, body) = read(&state, &signer, &uri).await?;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    Ok(())
}

#[tokio::test]
async fn org_write_routes_gate_before_the_missing_catalog() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let org = seed_org(&state, "acme").await?.org_id;
    let member = registered_user(&state, &[UserPermission::EditCatalog]).await?;
    set_user_org(&state, &member.1, Some(org.clone())).await?;
    let ungated = registered_user(&state, &[]).await?;
    set_user_org(&state, &ungated.1, Some(org.clone())).await?;
    let base = format!("/api/v1/orgs/{org}/descriptor-catalog");
    assert_eq!(
        signed_json(&state, &ungated, "POST", &base, &create_body("Fork", "b1"))
            .await?
            .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        signed_json(&state, &member, "POST", &base, &create_body("Fork", "b1"))
            .await?
            .status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        signed_json(
            &state,
            &member,
            "POST",
            &format!("{base}/Fork/members"),
            &json!({"runtime": runtime("b1", "macos-x64")})
        )
        .await?
        .status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        signed_json(
            &state,
            &member,
            "POST",
            &format!("{base}/Fork/state"),
            &json!({"action":"pin"})
        )
        .await?
        .status(),
        StatusCode::NOT_FOUND
    );
    Ok(())
}

/// The commit of every repository on [`model_hub`].
const COMMIT: &str = "cccccccccccccccccccccccccccccccccccccccc";

/// Public, gated, private and rate-limited model repositories.
async fn model_hub() -> anyhow::Result<MockHub> {
    let gguf = || vec![MockFile::lfs("m.gguf", &[1; 16])];
    let repo = |public, gate, files| MockRepo {
        commit: COMMIT.to_owned(),
        files,
        public,
        rate_limited: false,
        gate,
    };
    MockHub::start(HashMap::from([
        (
            "acme/Public-GGUF".to_owned(),
            repo(true, MockGate::Open, gguf()),
        ),
        (
            "acme/Other-GGUF".to_owned(),
            repo(true, MockGate::Open, gguf()),
        ),
        (
            "acme/Public-MLX".to_owned(),
            repo(
                true,
                MockGate::Open,
                vec![
                    MockFile::git("config.json", b"{}"),
                    MockFile::lfs("model.safetensors", &[2; 16]),
                ],
            ),
        ),
        (
            "acme/Gated-GGUF".to_owned(),
            repo(true, MockGate::Gated("manual"), gguf()),
        ),
        (
            "acme/Private-GGUF".to_owned(),
            repo(false, MockGate::Open, gguf()),
        ),
        (
            "acme/Limited-GGUF".to_owned(),
            MockRepo {
                rate_limited: true,
                ..repo(true, MockGate::Open, gguf())
            },
        ),
        (
            "acme/Sizeless-GGUF".to_owned(),
            repo(
                true,
                MockGate::Open,
                vec![MockFile::lfs("m.gguf", &[1; 16]).sizeless()],
            ),
        ),
    ]))
    .await
}

fn gguf_model(repo_name: &str) -> Value {
    json!({
        "type": "gguf_text", "source": "huggingface", "org": "acme",
        "repo_name": repo_name, "path": "m.gguf",
    })
}

fn model_body(name: &str, quantization: &str, model: Value) -> Value {
    json!({"name": name, "models": [{"quantization": quantization, "model": model}]})
}

async fn vault_model_resource(
    state: &AppState,
    org_id: &OrgId,
    origin: bool,
) -> anyhow::Result<Value> {
    let owner = PrincipalOwner::Org {
        org_id: org_id.clone(),
    };
    let sha256 = vault_blob_sha256()?;
    let key = blob_key(&owner, &sha256)?;
    state
        .vault_store
        .put_blob(&key, &sha256, blob_body(VAULT_BLOB))
        .await?;
    let mut resource = vault_resource_for(owner, sha256.clone(), VAULT_BLOB.len() as u64)?;
    if origin {
        resource.origin = Some(a_huggingface_origin()?);
    }
    state.vault_store.put_resource(&resource).await?;
    Ok(json!({
        "type": "gguf_text", "source": "vault_url",
        "url": format!("/vault/resources/{}/content", resource.resource_id),
        "principal_id": org_id, "sha256": sha256,
    }))
}

/// A SQL state that pins against `hub`, the `acme` org's uri, and a member of
/// `acme` that holds `edit_catalog`.
async fn model_writer(
    dir: &std::path::Path,
    hub: &MockHub,
) -> anyhow::Result<Option<(SqlState, String, UserSigner)>> {
    let Some(sql) = seeded_sql_state_with_hf_endpoint(dir, hub.endpoint.as_str()).await? else {
        return Ok(None);
    };
    let acme = seed_org(&sql.state, "acme").await?.org_id;
    let member = registered_user(&sql.state, &[UserPermission::EditCatalog]).await?;
    set_user_org(&sql.state, &member.1, Some(acme.clone())).await?;
    Ok(Some((
        sql,
        format!("/api/v1/orgs/{acme}/descriptor-catalog"),
        member,
    )))
}

#[tokio::test]
async fn an_org_vault_model_names_its_plan_and_verified_result_alias() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let hub = model_hub().await?;
    let Some((sql, uri, member)) = model_writer(dir.path(), &hub).await? else {
        return Ok(());
    };
    let state = &sql.state;
    let org_id: OrgId = uri
        .split('/')
        .nth(4)
        .context("the org catalog path carries its id")?
        .parse()?;
    let mut vault_model = vault_model_resource(state, &org_id, true).await?;
    let planned_digest = descriptor_digest(&vault_model)?;
    vault_model["custom_label"] = json!("private checkpoint");
    let alias = json!({
        "type": "gguf_text", "source": "huggingface", "org": "LiquidAI",
        "repo_name": "LFM2-700M-GGUF", "revision": "a".repeat(40),
        "path": "LFM2-700M-Q4_0.gguf",
    });
    let result_digest = descriptor_digest(&alias)?;
    let response = signed_json(
        state,
        &member,
        "POST",
        &uri,
        &model_body("VaultLM", "q4_0", vault_model),
    )
    .await?;
    assert_eq!(response.status(), StatusCode::CREATED);
    let entry = body_json(response).await?;
    assert_eq!(entry["members"][0]["digest"], planned_digest);
    assert_eq!(hub.revision_requests(), 0);

    let (status, hits) = read(
        state,
        &member,
        &format!("{BASE}/by-digest?digests={planned_digest},{result_digest}"),
    )
    .await?;
    assert_eq!(status, StatusCode::OK);
    for digest in [&planned_digest, &result_digest] {
        assert_eq!(
            hits["names"][digest][0]["selector"], "VaultLM::gguf::q4_0",
            "{hits}"
        );
    }

    let other_member = vault_model_resource(state, &org_id, false).await?;
    let other_digest = descriptor_digest(&other_member)?;
    let response = signed_json(
        state,
        &member,
        "POST",
        &format!("{uri}/VaultLM/members"),
        &json!({"model": other_member, "quantization": "q8_0"}),
    )
    .await?;
    assert_eq!(response.status(), StatusCode::OK);
    let (status, hits) = read(
        state,
        &member,
        &format!("{BASE}/by-digest?digests={other_digest}"),
    )
    .await?;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        hits["names"][&other_digest][0]["selector"],
        "VaultLM::gguf::q8_0"
    );

    let outsider = registered_user(state, &[]).await?;
    let (status, hidden) = read(
        state,
        &outsider,
        &format!("{BASE}/by-digest?digests={planned_digest},{result_digest}"),
    )
    .await?;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(hidden["names"], json!({}));

    let other = seed_org(state, "other").await?.org_id;
    let foreign = vault_model_resource(state, &other, false).await?;
    let response = signed_json(
        state,
        &member,
        "POST",
        &uri,
        &model_body("ForeignLM", "q4_0", foreign),
    )
    .await?;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    Ok(())
}

#[tokio::test]
async fn invalid_model_write_targets_do_not_call_hugging_face() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let hub = model_hub().await?;
    let Some((sql, uri, member)) = model_writer(dir.path(), &hub).await? else {
        return Ok(());
    };
    let state = &sql.state;
    let response = signed_json(state, &member, "POST", &uri, &create_body("Taken", "b1")).await?;
    assert_eq!(response.status(), StatusCode::CREATED);

    let model = gguf_model("Public-GGUF");
    let response = signed_json(
        state,
        &member,
        "POST",
        &uri,
        &model_body("Taken", "q4_0", model.clone()),
    )
    .await?;
    assert_eq!(response.status(), StatusCode::CONFLICT);

    let member_body = json!({"model": model, "quantization": "q4_0"});
    let response = signed_json(
        state,
        &member,
        "POST",
        &format!("{uri}/Absent/members"),
        &member_body,
    )
    .await?;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let response = signed_json(
        state,
        &member,
        "POST",
        &format!("{uri}/Taken/members"),
        &member_body,
    )
    .await?;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);

    let org_id: OrgId = uri
        .split('/')
        .nth(4)
        .context("the org catalog path carries its id")?
        .parse()?;
    store_of(state)?
        .set_state(
            &CatalogScope::Org(org_id),
            &CatalogName::try_new("Taken")?,
            EntryState::Tombstoned,
        )
        .await?;
    let response = signed_json(
        state,
        &member,
        "POST",
        &format!("{uri}/Taken/members"),
        &member_body,
    )
    .await?;
    assert_eq!(response.status(), StatusCode::CONFLICT);
    assert_eq!(hub.revision_requests(), 0);
    Ok(())
}

#[rstest]
#[case::public_at_main("Public-GGUF", None, StatusCode::CREATED, "")]
#[case::public_at_commit("Public-GGUF", Some(COMMIT), StatusCode::CREATED, "")]
#[case::gated("Gated-GGUF", None, StatusCode::BAD_REQUEST, "is a gated repository")]
#[case::private("Private-GGUF", None, StatusCode::BAD_REQUEST, "without a token")]
#[case::rate_limited("Limited-GGUF", None, StatusCode::BAD_GATEWAY, "")]
#[case::missing_size("Sizeless-GGUF", None, StatusCode::BAD_GATEWAY, "")]
#[tokio::test]
async fn an_org_model_entry_pins_a_public_model(
    #[case] repo_name: &str,
    #[case] revision: Option<&str>,
    #[case] status: StatusCode,
    #[case] refusal: &str,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let hub = model_hub().await?;
    let Some((sql, uri, member)) = model_writer(dir.path(), &hub).await? else {
        return Ok(());
    };
    let mut model = gguf_model(repo_name);
    if let (Some(revision), Some(object)) = (revision, model.as_object_mut()) {
        object.insert("revision".to_owned(), json!(revision));
    }
    let body = model_body("AcmeLM", "q4_0", model);
    super::api_schema::assert_conforms("CreateDescriptorEntryRequest", &body)?;
    let response = signed_json(&sql.state, &member, "POST", &uri, &body).await?;
    assert_eq!(response.status(), status);
    let answer = body_json(response).await?;
    if status == StatusCode::CREATED {
        // A branch resolves to its commit, which the member holds.
        assert_eq!(
            answer["members"][0]["descriptor"]["revision"], COMMIT,
            "{answer}"
        );
        assert_eq!(
            answer["members"][0]["selector"], "AcmeLM::gguf::q4_0",
            "{answer}"
        );
    } else {
        let error = answer["error"].as_str().unwrap_or_default();
        assert!(error.contains(refusal), "{answer}");
    }
    Ok(())
}

#[rstest]
#[case::another_format(json!({"model": {"type": "mlx", "source": "huggingface", "org": "acme", "repo_name": "Public-MLX"}, "quantization": "4bit"}), StatusCode::OK)]
#[case::same_format_and_quantization(json!({"model": gguf_model("Other-GGUF"), "quantization": "q4_0"}), StatusCode::CONFLICT)]
#[case::a_runtime(json!({"runtime": runtime("b1", "macos-arm64")}), StatusCode::BAD_REQUEST)]
#[case::gated(json!({"model": gguf_model("Gated-GGUF"), "quantization": "q8_0"}), StatusCode::BAD_REQUEST)]
#[case::no_quantization(json!({"model": gguf_model("Public-GGUF")}), StatusCode::BAD_REQUEST)]
#[tokio::test]
async fn an_org_model_entry_appends_a_model_member(
    #[case] member_body: Value,
    #[case] status: StatusCode,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let hub = model_hub().await?;
    let Some((sql, uri, member)) = model_writer(dir.path(), &hub).await? else {
        return Ok(());
    };
    let body = model_body("AcmeLM", "q4_0", gguf_model("Public-GGUF"));
    let response = signed_json(&sql.state, &member, "POST", &uri, &body).await?;
    assert_eq!(response.status(), StatusCode::CREATED);
    super::api_schema::assert_conforms("AddDescriptorMemberRequest", &member_body)?;
    let response = signed_json(
        &sql.state,
        &member,
        "POST",
        &format!("{uri}/AcmeLM/members"),
        &member_body,
    )
    .await?;
    assert_eq!(response.status(), status);
    if status == StatusCode::OK {
        let entry = body_json(response).await?;
        assert_eq!(
            entry["members"][1]["selector"], "AcmeLM::mlx::4bit",
            "{entry}"
        );
        assert_eq!(
            entry["members"][1]["descriptor"]["revision"], COMMIT,
            "{entry}"
        );
    }
    Ok(())
}

#[rstest]
#[case::both(json!({"name": "Fork", "runtimes": [runtime("b1", "macos-arm64")], "models": [{"quantization": "q4_0", "model": gguf_model("Public-GGUF")}]}))]
#[case::neither(json!({"name": "Fork"}))]
#[tokio::test]
async fn an_org_entry_has_one_kind_of_member(#[case] body: Value) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let hub = model_hub().await?;
    let Some((sql, uri, member)) = model_writer(dir.path(), &hub).await? else {
        return Ok(());
    };
    let response = signed_json(&sql.state, &member, "POST", &uri, &body).await?;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    Ok(())
}

#[tokio::test]
async fn a_model_member_joins_no_runtime_entry() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let hub = model_hub().await?;
    let Some((sql, uri, member)) = model_writer(dir.path(), &hub).await? else {
        return Ok(());
    };
    let response = signed_json(
        &sql.state,
        &member,
        "POST",
        &uri,
        &create_body("Fork", "b1"),
    )
    .await?;
    assert_eq!(response.status(), StatusCode::CREATED);
    let response = signed_json(
        &sql.state,
        &member,
        "POST",
        &format!("{uri}/Fork/members"),
        &json!({"model": gguf_model("Public-GGUF"), "quantization": "q4_0"}),
    )
    .await?;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    Ok(())
}
