use std::path::Path;
use std::process::{Command, Output};
use std::sync::Arc;

use anyhow::Context;
use serde_json::{Value, json};
use sqlx::ConnectOptions;
use sqlx::postgres::PgConnectOptions;

use pipette_core::config::Config;
use pipette_core::organization::Organization;
use pipette_core::vault::blob_key;
use pipette_plan_types::{
    CatalogName, CatalogScope, DescriptorEntry, DescriptorEntryCreator, DescriptorEntryKind,
    DescriptorEntryState, NonEmptyTrimmedString, NonEmptyVec, OrgSlug, PrincipalOwner, UserId,
    descriptor_digest,
};
use pipette_store::CreateEntryOutcome;
use pipette_store_sql::{SqlTodoStore, scratch::TestDb};
use pipette_store_testkit::{
    VAULT_BLOB, a_descriptor_entry, a_huggingface_origin, a_member, a_model_member, blob_body,
    vault_blob_sha256, vault_resource_for,
};

/// Run `pipette-mgmt catalog <args>` on the fixture's schema.
fn catalog(
    config_path: &Path,
    options: &PgConnectOptions,
    args: &[&str],
) -> anyhow::Result<Output> {
    let mut command = Command::new(env!("CARGO_BIN_EXE_pipette-mgmt"));
    command
        .arg("--config")
        .arg(config_path)
        .arg("catalog")
        .args(args)
        .env_remove("PIPETTE_MGMT_CONFIG")
        .env("RUST_LOG", "off")
        // The child uses the same isolated, migrated schema as the fixture.
        .env(
            "PGOPTIONS",
            options.get_options().context("test schema options")?,
        );
    let connection_url = options.to_url_lossy();
    if let Some(password) = connection_url.password() {
        command.env(
            "PGPASSWORD",
            percent_encoding::percent_decode_str(password)
                .decode_utf8()
                .context("fixture database password")?
                .as_ref(),
        );
    }
    for (key, value) in connection_url.query_pairs() {
        let variable = match key.as_ref() {
            "sslmode" => "PGSSLMODE",
            "sslrootcert" => "PGSSLROOTCERT",
            "sslcert" => "PGSSLCERT",
            "sslkey" => "PGSSLKEY",
            _ => continue,
        };
        // sqlx adds this display prefix to certificate file paths in its URL.
        command.env(variable, value.strip_prefix("file: ").unwrap_or(&value));
    }
    let output = command.output().context("run pipette-mgmt catalog")?;
    anyhow::ensure!(
        output.status.success(),
        "catalog {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(output)
}

#[tokio::test]
async fn cli_finds_live_entries_by_digest_and_lists_json() -> anyhow::Result<()> {
    let test_db = TestDb::new()
        .await?
        .context("set TEST_DATABASE_URL to run the CLI catalog test")?;
    let dir = tempfile::tempdir()?;
    let options = test_db.connect_options();
    let config_path = dir.path().join("config.toml");
    let quoted = |value: &str| serde_json::to_string(value);
    let data_dir = quoted(dir.path().to_str().context("temporary directory path")?)?;
    std::fs::write(
        &config_path,
        format!(
            "evals_server_url = \"http://localhost:8000\"\n\
             [storage]\nbackend = \"local_fs\"\ndata_dir = {data_dir}\n\
             [auth_storage]\nbackend = \"local_fs\"\ndata_dir = {data_dir}\n\
             [database]\nhost = {}\nport = {}\nuser = {}\ndatabase = {}\n",
            quoted(options.get_host())?,
            options.get_port(),
            quoted(options.get_username())?,
            quoted(options.get_database().context("test database name")?)?,
        ),
    )?;
    let config = Config::load(&config_path)?;
    let stores = test_db.stores(pipette_store_fs::build_local_fs_stores(
        &config,
        Arc::new(SqlTodoStore::new(test_db.db().clone())),
    )?);
    let org = Organization::new(
        OrgSlug::try_new("acme")?,
        NonEmptyTrimmedString::try_new("Acme")?,
        jiff::Timestamp::now(),
    );
    stores
        .orgs
        .claim_org_slug(org.slug.as_str(), &org.org_id)
        .await?;
    stores.orgs.put_org(&org).await?;
    let catalog_store = stores.descriptor_catalog.as_ref().context("SQL catalog")?;

    let runtime = a_member("b1", "linux-x64-rocm")?;
    let model = a_model_member("gguf_text", "AcmeLM-GGUF", "q4_0")?;
    let at = jiff::Timestamp::from_second(300).context("valid time")?;
    let org_fork = DescriptorEntry::new(
        CatalogScope::Org(org.org_id.clone()),
        CatalogName::try_new("fork")?,
        DescriptorEntryKind::HandMade,
        DescriptorEntryState::Active,
        at,
        at,
        DescriptorEntryCreator::Account(UserId::try_new(
            "user_0195c3e2-a4f3-7b8c-9d1e-2f3a4b5c6d7e",
        )?),
        NonEmptyVec::try_new(vec![runtime.clone()])?,
    )?;
    for entry in [
        a_descriptor_entry(
            "llamacpp-b1",
            DescriptorEntryKind::Stable,
            1,
            runtime.clone(),
        )?,
        a_descriptor_entry("dead", DescriptorEntryKind::HandMade, 2, runtime.clone())?,
        a_descriptor_entry("AcmeLM", DescriptorEntryKind::HandMade, 3, model.clone())?,
        org_fork,
    ] {
        assert_eq!(
            catalog_store.create_entry(&entry).await?,
            CreateEntryOutcome::Created
        );
    }
    catalog(&config_path, &options, &["tombstone", "dead", "--yes"])?;

    // A digest in two scopes, in scope order; the tombstoned entry is left out.
    let found = catalog(
        &config_path,
        &options,
        &["find", "--digest", runtime.digest(), "--json"],
    )?;
    let hits = String::from_utf8(found.stdout)?
        .lines()
        .map(serde_json::from_str::<Value>)
        .collect::<Result<Vec<_>, _>>()?;
    let expected = [
        ("global", "llamacpp-b1", "stable"),
        (org.org_id.as_str(), "fork", "hand-made"),
    ]
    .map(|(scope, name, kind)| {
        json!({
            "digest": runtime.digest(), "scope": scope, "name": name, "kind": kind,
            "state": "active", "member_key": "linux-x64-rocm",
        })
    });
    assert_eq!(hits, expected);

    // `--model` computes the member digest offline; the label does not change it.
    let model_json = serde_json::to_string(model.model().context("a model member")?)?;
    let found = catalog(
        &config_path,
        &options,
        &["find", "--model", &format!("q8_0={model_json}")],
    )?;
    assert_eq!(
        String::from_utf8(found.stdout)?,
        format!(
            "{}\tglobal\tAcmeLM\thand-made\tactive\tgguf::q4_0\n",
            model.digest()
        )
    );

    let missing = catalog(
        &config_path,
        &options,
        &["find", "--digest", &"0".repeat(64)],
    )?;
    assert!(missing.stdout.is_empty());
    assert!(String::from_utf8(missing.stderr)?.contains("no live entry holds"));

    // `list --json` prints entries on stdout and the cursor on stderr.
    let listed = catalog(&config_path, &options, &["list", "--json", "--limit", "1"])?;
    let stdout = String::from_utf8(listed.stdout)?;
    let lines = stdout.lines().collect::<Vec<_>>();
    assert_eq!(lines.len(), 1, "{stdout}");
    let entry: DescriptorEntry = serde_json::from_str(lines[0])?;
    assert_eq!(entry.name.to_string(), "AcmeLM");
    assert_eq!(entry.members.head(), &model);
    let stderr = String::from_utf8(listed.stderr)?;
    assert!(stderr.contains("next_cursor: acmelm"), "{stderr}");
    let cursor = stderr
        .lines()
        .find_map(|line| line.strip_prefix("next_cursor: "))
        .context("first page cursor")?;
    let resumed = catalog(
        &config_path,
        &options,
        &["list", "--json", "--limit", "1", "--cursor", cursor],
    )?;
    let stdout = String::from_utf8(resumed.stdout)?;
    let lines = stdout.lines().collect::<Vec<_>>();
    assert_eq!(lines.len(), 1, "{stdout}");
    let entry: DescriptorEntry = serde_json::from_str(lines[0])?;
    assert_eq!(entry.name.to_string(), "llamacpp-b1");
    assert_eq!(entry.members.head(), &runtime);
    let stderr = String::from_utf8(resumed.stderr)?;
    assert!(!stderr.contains("next_cursor:"), "{stderr}");

    let owner = PrincipalOwner::Org {
        org_id: org.org_id.clone(),
    };
    let sha256 = vault_blob_sha256()?;
    let key = blob_key(&owner, &sha256)?;
    stores
        .vault
        .put_blob(&key, &sha256, blob_body(VAULT_BLOB))
        .await?;
    let mut resource = vault_resource_for(owner, sha256.clone(), VAULT_BLOB.len() as u64)?;
    resource.origin = Some(a_huggingface_origin()?);
    stores.vault.put_resource(&resource).await?;
    let vault_model = json!({
        "type": "gguf_text", "source": "vault_url",
        "url": format!("/vault/resources/{}/content", resource.resource_id),
        "principal_id": org.org_id, "sha256": sha256,
    });
    catalog(
        &config_path,
        &options,
        &[
            "create",
            "--org",
            org.org_id.as_str(),
            "--name",
            "VaultLM",
            "--model",
            &format!("q4_0={vault_model}"),
        ],
    )?;
    let alias = json!({
        "type": "gguf_text", "source": "huggingface", "org": "LiquidAI",
        "repo_name": "LFM2-700M-GGUF", "revision": "a".repeat(40),
        "path": "LFM2-700M-Q4_0.gguf",
    });
    let alias_digest = descriptor_digest(&alias)?;
    assert_ne!(alias_digest, descriptor_digest(&vault_model)?);
    let model_selector = format!("q8_0={alias}");
    for selector in [
        ["--digest", alias_digest.as_str()],
        ["--model", model_selector.as_str()],
    ] {
        let found = catalog(
            &config_path,
            &options,
            &["find", selector[0], selector[1], "--json"],
        )?;
        let hits = String::from_utf8(found.stdout)?
            .lines()
            .map(serde_json::from_str::<Value>)
            .collect::<Result<Vec<_>, _>>()?;
        assert_eq!(
            hits,
            [json!({
                "digest": alias_digest, "scope": org.org_id, "name": "VaultLM",
                "kind": "hand-made", "state": "active", "member_key": "gguf::q4_0",
            })]
        );
    }
    Ok(())
}
