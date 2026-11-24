use std::num::NonZeroUsize;
use std::process::Command;
use std::sync::Arc;

use anyhow::Context;
use rstest::rstest;
use serde_json::Value;
use sqlx::ConnectOptions;

use pipette_core::config::Config;
use pipette_plan_types::{AuditAction, CatalogName, DescriptorEntryKind};
use pipette_store::{AuditFilter, CreateEntryOutcome};
use pipette_store_sql::{SqlTodoStore, scratch::TestDb};
use pipette_store_testkit::{a_descriptor_entry, a_model_member};

#[rstest]
#[case::set(&["LFM"], Some("LFM"), false, "Set the family of LFM2.5-1.2B to LFM")]
#[case::default(&["--default"], None, false, "Set the family of LFM2.5-1.2B to LFM2.5")]
#[case::dry_run(&["LFM", "--dry-run"], Some("LFM"), true, "Would set the family of LFM2.5-1.2B to LFM")]
#[tokio::test]
async fn cli_family_change_audits_and_logs_only_a_write(
    #[case] args: &[&str],
    #[case] family: Option<&str>,
    #[case] dry_run: bool,
    #[case] printed: &str,
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
    // The entry starts with an override, so `--default` changes it.
    let entry = a_descriptor_entry(
        "LFM2.5-1.2B",
        DescriptorEntryKind::HandMade,
        300,
        a_model_member("mlx", "LFM2.5-1.2B-MLX", "4bit")?,
    )?
    .with_family(Some(CatalogName::try_new("Liquid")?))?;
    assert_eq!(
        catalog.create_entry(&entry).await?,
        CreateEntryOutcome::Created
    );

    let command = || -> anyhow::Result<Command> {
        let mut command = Command::new(env!("CARGO_BIN_EXE_pipette-mgmt"));
        command
            .args(["--log-format", "json", "--config"])
            .arg(&config_path)
            .arg("catalog")
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
        Ok(command)
    };
    let output = command()?
        .args(["set-family", "LFM2.5-1.2B"])
        .args(args)
        .output()
        .context("run catalog set-family")?;
    let stderr = String::from_utf8(output.stderr)?;
    assert!(output.status.success(), "{stderr}");
    let stdout = String::from_utf8(output.stdout)?;
    assert!(stdout.contains(printed), "{stdout}");

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
    let changes = page
        .items
        .iter()
        .filter(|event| matches!(event.action, AuditAction::CatalogSetFamily { .. }))
        .map(|event| event.action.clone())
        .collect::<Vec<_>>();
    let logs = stderr
        .lines()
        .map(serde_json::from_str::<Value>)
        .collect::<Result<Vec<_>, _>>()?;
    let family_logs = logs
        .iter()
        .filter(|line| line["message"] == "catalog entry family set")
        .count();
    let stored = catalog
        .get_entry(&entry.scope, &entry.name)
        .await?
        .context("the entry")?;
    if dry_run {
        assert!(page.items.is_empty(), "{page:?}");
        assert_eq!(family_logs, 0, "{stderr}");
        assert_eq!(stored, entry);
        return Ok(());
    }
    let family = family.map(CatalogName::try_new).transpose()?;
    assert_eq!(
        changes,
        [AuditAction::CatalogSetFamily {
            scope: entry.scope.clone(),
            name: entry.name.clone(),
            family: family.clone(),
        }]
    );
    assert_eq!(family_logs, 1, "{stderr}");
    assert_eq!(stored, entry.with_family(family)?);
    for args in [["show", "LFM2.5-1.2B"], ["list", "--json"]] {
        let output = command()?.args(args).output()?;
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let item: Value = serde_json::from_slice(&output.stdout)?;
        assert_eq!(item["family"].as_str(), stored.family(), "{args:?}: {item}");
    }
    Ok(())
}

#[rstest]
#[case::neither(&[])]
#[case::both(&["LFM", "--default"])]
fn set_family_needs_a_family_or_default(#[case] args: &[&str]) -> anyhow::Result<()> {
    let output = Command::new(env!("CARGO_BIN_EXE_pipette-mgmt"))
        .args(["catalog", "set-family", "LFM2.5-1.2B"])
        .args(args)
        .env_remove("PIPETTE_MGMT_CONFIG")
        .output()?;
    // Exit status 2 is a usage error, which clap gives before the command runs.
    assert_eq!(output.status.code(), Some(2), "{args:?}");
    assert!(
        String::from_utf8(output.stderr)?.contains("--default"),
        "{args:?}"
    );
    Ok(())
}
