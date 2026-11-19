use std::num::NonZeroUsize;
use std::process::Command;
use std::sync::Arc;

use anyhow::Context;
use rstest::rstest;
use serde_json::Value;
use sqlx::ConnectOptions;

use pipette_core::config::Config;
use pipette_plan_types::{AuditAction, AuditOutcome, DescriptorEntryKind, NonEmptyVec};
use pipette_store::{AuditFilter, CreateEntryOutcome};
use pipette_store_sql::{SqlTodoStore, scratch::TestDb};
use pipette_store_testkit::{a_descriptor_entry, a_member, a_model_member};

#[rstest]
#[case::runtime_write(false, false)]
#[case::runtime_dry_run(false, true)]
#[case::model_write(true, false)]
#[case::model_dry_run(true, true)]
#[tokio::test]
async fn cli_removal_audits_and_logs_only_a_write(
    #[case] model: bool,
    #[case] dry_run: bool,
) -> anyhow::Result<()> {
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
    let catalog = stores.descriptor_catalog.as_ref().context("SQL catalog")?;
    let (removed, kept, key) = if model {
        (
            a_model_member("gguf_text", "AcmeLM-Q4-GGUF", "q4_0")?,
            a_model_member("gguf_text", "AcmeLM-Q8-GGUF", "q8_0")?,
            "gguf::q4_0",
        )
    } else {
        (
            a_member("b1", "linux-x64-rocm")?,
            a_member("b1", "linux-x64-vulkan")?,
            "linux-x64-rocm",
        )
    };
    let mut entry = a_descriptor_entry(
        "AcmeLM",
        DescriptorEntryKind::HandMade,
        300,
        removed.clone(),
    )?;
    entry.members = NonEmptyVec::try_new(vec![removed.clone(), kept.clone()])?;
    assert_eq!(
        catalog.create_entry(&entry).await?,
        CreateEntryOutcome::Created
    );

    let mut command = Command::new(env!("CARGO_BIN_EXE_pipette-mgmt"));
    command
        .args(["--log-format", "json", "--config"])
        .arg(&config_path)
        .args(["catalog", "remove-member", "AcmeLM", "--member", key])
        .env_remove("PIPETTE_MGMT_CONFIG")
        .env("RUST_LOG", "info")
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
    if dry_run {
        command.arg("--dry-run");
    }
    let output = command.output().context("run catalog remove-member")?;
    let stderr = String::from_utf8(output.stderr)?;
    assert!(output.status.success(), "{stderr}");
    let stdout = String::from_utf8(output.stdout)?;
    let prefix = if dry_run { "Would remove" } else { "Removed" };
    assert!(
        stdout.contains(&format!("{prefix} AcmeLM::{key}")),
        "{stdout}"
    );

    let page = stores
        .audit
        .as_ref()
        .context("SQL audit log")?
        .list_events(
            &AuditFilter::default(),
            None,
            NonZeroUsize::new(100).context("limit")?,
        )
        .await?;
    assert!(page.unreadable.is_empty());
    let removals = page
        .items
        .iter()
        .filter(|event| matches!(event.action, AuditAction::CatalogRemoveMember { .. }))
        .collect::<Vec<_>>();
    let logs = stderr
        .lines()
        .map(serde_json::from_str::<Value>)
        .collect::<Result<Vec<_>, _>>()?;
    let removal_logs = logs
        .iter()
        .filter(|line| line["message"] == "catalog member removed")
        .collect::<Vec<_>>();
    if dry_run {
        assert!(page.items.is_empty(), "{page:?}");
        assert!(removal_logs.is_empty(), "{stderr}");
    } else {
        assert_eq!(removals.len(), 1, "{page:?}");
        assert_eq!(removals[0].outcome, AuditOutcome::Succeeded);
        assert_eq!(
            removals[0].action,
            AuditAction::CatalogRemoveMember {
                scope: entry.scope.clone(),
                name: entry.name.clone(),
                member: key.to_owned(),
                digest: removed.digest().to_owned(),
            }
        );
        assert_eq!(removal_logs.len(), 1, "{stderr}");
        assert_eq!(removal_logs[0]["level"], "INFO");
        assert_eq!(removal_logs[0]["catalog_name"], "AcmeLM");
        if model {
            assert_eq!(removal_logs[0]["format"], "gguf");
            assert_eq!(removal_logs[0]["quantization"], "q4_0");
        } else {
            assert_eq!(removal_logs[0]["flavor"], key);
        }
        entry.members = NonEmptyVec::try_new(vec![kept])?;
    }
    assert_eq!(
        catalog.get_entry(&entry.scope, &entry.name).await?,
        Some(entry)
    );
    Ok(())
}
