use std::process::Command;

use anyhow::Context;
use serde_json::json;
use sqlx::ConnectOptions;

use pipette_core::organization::Organization;
use pipette_plan_types::{NonEmptyTrimmedString, OrgSlug};
use pipette_store_sql::{audit_stores, scratch::TestDb};

#[tokio::test]
async fn orgs_list_warns_on_stderr_for_a_stored_reserved_slug() -> anyhow::Result<()> {
    let db = TestDb::new()
        .await?
        .context("set TEST_DATABASE_URL to run the CLI organization test")?;
    let dir = tempfile::tempdir()?;
    let options = db.connect_options();
    let config = dir.path().join("config.toml");
    std::fs::write(
        &config,
        format!(
            "evals_server_url = \"http://localhost:8000\"\n\
             [storage]\nbackend = \"local_fs\"\ndata_dir = {root}\n\
             [auth_storage]\nbackend = \"local_fs\"\ndata_dir = {root}\n\
             [database]\nhost = {}\nport = {}\nuser = {}\ndatabase = {}\n",
            json!(options.get_host()),
            options.get_port(),
            json!(options.get_username()),
            json!(options.get_database().context("test database name")?),
            root = json!(dir.path().to_str().context("temporary directory path")?),
        ),
    )?;
    let store = audit_stores(db.db()).orgs;
    let legacy = Organization::new(
        OrgSlug::try_new("global")?,
        NonEmptyTrimmedString::try_new("Legacy")?,
        jiff::Timestamp::now(),
    );
    store
        .claim_org_slug(legacy.slug.as_str(), &legacy.org_id)
        .await?;
    store.put_org(&legacy).await?;
    let other = Organization::new(
        OrgSlug::try_new("globals")?,
        NonEmptyTrimmedString::try_new("Other")?,
        jiff::Timestamp::now(),
    );
    store.put_org(&other).await?;

    let mut command = Command::new(env!("CARGO_BIN_EXE_pipette-mgmt"));
    command
        .arg("--config")
        .arg(&config)
        .args(["orgs", "list"])
        .env_remove("PIPETTE_MGMT_CONFIG")
        .env("RUST_LOG", "off")
        .env(
            "PGOPTIONS",
            options.get_options().context("test schema options")?,
        );
    let connection_url = options.to_url_lossy();
    if let Some(password) = connection_url.password() {
        command.env(
            "PGPASSWORD",
            percent_encoding::percent_decode_str(password)
                .decode_utf8()?
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
        command.env(variable, value.strip_prefix("file: ").unwrap_or(&value));
    }
    let output = command.output().context("run pipette-mgmt orgs list")?;
    let stderr = String::from_utf8(output.stderr)?;
    let stdout = String::from_utf8(output.stdout)?;
    assert!(output.status.success(), "{stderr}");
    assert_eq!(stderr.lines().count(), 1, "{stderr}");
    assert!(stderr.contains(legacy.org_id.as_str()), "{stderr}");
    assert!(
        stderr.contains("organization slug \"global\" is reserved"),
        "{stderr}"
    );
    assert!(!stdout.contains("Warning:"), "{stdout}");
    assert!(stdout.contains(legacy.org_id.as_str()), "{stdout}");
    assert!(stdout.contains(other.org_id.as_str()), "{stdout}");
    assert_eq!(
        store.lookup_org_by_slug("global").await?,
        Some(legacy.org_id.clone())
    );
    assert_eq!(store.get_org(&legacy.org_id).await?, Some(legacy));
    Ok(())
}
