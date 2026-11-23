use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;

use anyhow::Context;
use rstest::rstest;
use serde_json::Value;
use sqlx::ConnectOptions;
use sqlx::postgres::PgConnectOptions;

use pipette_core::config::Config;
use pipette_plan_types::{AuditAction, CatalogName, CatalogScope, DescriptorEntryKind, LogoUri};
use pipette_store::{AuditFilter, CreateEntryOutcome, Stores};
use pipette_store_sql::{SqlTodoStore, scratch::TestDb};
use pipette_store_testkit::{a_descriptor_entry, a_model_member};

const LOGO: &str = "https://pipette.liquid.ai/logos/lfm.svg";
const DATA_LOGO: &str = "data:image/svg+xml;base64,PHN2Zy8+";

/// A migrated test schema, the stores on it, and the CLI configured for it.
struct Fixture {
    _test_db: TestDb,
    _dir: tempfile::TempDir,
    config_path: PathBuf,
    options: Arc<PgConnectOptions>,
    stores: Stores,
}

impl Fixture {
    async fn new() -> anyhow::Result<Self> {
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
        Ok(Self {
            _test_db: test_db,
            _dir: dir,
            config_path,
            options,
            stores,
        })
    }

    fn command(&self) -> anyhow::Result<Command> {
        let mut command = Command::new(env!("CARGO_BIN_EXE_pipette-mgmt"));
        command
            .args(["--log-format", "json", "--config"])
            .arg(&self.config_path)
            .arg("catalog")
            .env_remove("PIPETTE_MGMT_CONFIG")
            .env("RUST_LOG", "info")
            // The child uses the same isolated, migrated schema as the fixture.
            .env(
                "PGOPTIONS",
                self.options.get_options().context("test schema options")?,
            );
        let connection_url = self.options.to_url_lossy();
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
    }
}

#[rstest]
#[case::set(&[LOGO], "Set the logo of family LFM2.5 to", "catalog family logo set")]
#[case::set_data(&[DATA_LOGO], "Set the logo of family LFM2.5 to an embedded image with SHA-256 ", "catalog family logo set")]
#[case::dry_run(&[LOGO, "--dry-run"], "Would set the logo of family LFM2.5 to", "")]
#[case::clear(&["--clear"], "Removed the logo of family LFM2.5", "catalog family logo cleared")]
#[case::dry_clear(&["--clear", "--dry-run"], "Would remove the logo of family LFM2.5", "")]
#[tokio::test]
async fn cli_family_logo_change_audits_and_logs_only_a_write(
    #[case] args: &[&str],
    #[case] printed: &str,
    #[case] log_message: &str,
) -> anyhow::Result<()> {
    let dry_run = args.contains(&"--dry-run");
    // A clear starts from a stored logo. The logo is there after the command
    // exactly when the command is a dry run of a clear or a real set.
    let preset = args.contains(&"--clear");
    let logo = args
        .iter()
        .copied()
        .find(|arg| !arg.starts_with("--"))
        .unwrap_or(LOGO);
    let stored_logo = (preset == dry_run)
        .then(|| LogoUri::try_new(logo))
        .transpose()?;
    let fixture = Fixture::new().await?;
    let catalog = fixture
        .stores
        .descriptor_catalog
        .as_ref()
        .context("SQL catalog")?;
    let entry = a_descriptor_entry(
        "LFM2.5-1.2B",
        DescriptorEntryKind::HandMade,
        300,
        a_model_member("mlx", "LFM2.5-1.2B-MLX", "4bit")?,
    )?;
    assert_eq!(
        catalog.create_entry(&entry).await?,
        CreateEntryOutcome::Created
    );
    let family = CatalogName::try_new("LFM2.5")?;
    let global = CatalogScope::Global;
    if preset {
        let logo_uri = LogoUri::try_new(logo)?;
        catalog
            .set_family_logo(&global, &family, Some(&logo_uri), None)
            .await?;
    }

    let output = fixture
        .command()?
        .args(["set-family-logo", "LFM2.5"])
        .args(args)
        .output()
        .context("run catalog set-family-logo")?;
    let stderr = String::from_utf8(output.stderr)?;
    assert!(output.status.success(), "{stderr}");
    let stdout = String::from_utf8(output.stdout)?;
    assert!(stdout.contains(printed), "{stdout}");

    let page = fixture
        .stores
        .audit
        .as_ref()
        .context("SQL audit log")?
        .list_events(
            &AuditFilter::default(),
            None,
            NonZeroUsize::new(100).context("limit")?,
        )
        .await?;
    let logs = stderr
        .lines()
        .map(serde_json::from_str::<Value>)
        .collect::<Result<Vec<_>, _>>()?;
    let logo_logs = logs
        .iter()
        .filter(|line| {
            line["message"]
                .as_str()
                .is_some_and(|message| message.starts_with("catalog family logo"))
        })
        .map(|line| line["message"].clone())
        .collect::<Vec<_>>();
    let stored = catalog
        .family_logos(std::slice::from_ref(&global))
        .await?
        .into_iter()
        .map(|logo| logo.sha256.to_string())
        .collect::<Vec<_>>();
    assert_eq!(
        stored,
        Vec::from_iter(stored_logo.as_ref().map(LogoUri::sha256))
    );
    if dry_run {
        assert!(page.items.is_empty(), "{page:?}");
        assert!(logo_logs.is_empty(), "{stderr}");
        return Ok(());
    }
    let changes = page
        .items
        .iter()
        .filter(|event| matches!(event.action, AuditAction::CatalogSetFamilyLogo { .. }))
        .map(|event| event.action.clone())
        .collect::<Vec<_>>();
    assert_eq!(
        changes,
        [AuditAction::CatalogSetFamilyLogo {
            scope: global,
            family,
            logo_uri: stored_logo.clone(),
        }]
    );
    assert_eq!(logo_logs, [log_message], "{stderr}");
    for args in [["show", "LFM2.5-1.2B"], ["list", "--json"]] {
        let output = fixture.command()?.args(args).output()?;
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let item: Value = serde_json::from_slice(&output.stdout)?;
        assert_eq!(
            item["family_logo_sha256"].as_str(),
            stored_logo.as_ref().map(LogoUri::sha256).as_deref(),
            "{args:?}: {item}"
        );
    }
    Ok(())
}

#[tokio::test]
async fn cli_family_logo_needs_an_entry_with_the_family() -> anyhow::Result<()> {
    let fixture = Fixture::new().await?;
    let output = fixture
        .command()?
        .args(["set-family-logo", "Qwen3", LOGO])
        .output()
        .context("run catalog set-family-logo")?;
    let stderr = String::from_utf8(output.stderr)?;
    assert!(!output.status.success(), "{stderr}");
    assert!(
        stderr.contains("no model entry has family Qwen3"),
        "{stderr}"
    );
    Ok(())
}

#[rstest]
#[case::hosted(LOGO)]
#[case::embedded(DATA_LOGO)]
#[tokio::test]
async fn cli_shows_a_family_logo_by_digest(#[case] uri: &str) -> anyhow::Result<()> {
    let fixture = Fixture::new().await?;
    let catalog = fixture
        .stores
        .descriptor_catalog
        .as_ref()
        .context("SQL catalog")?;
    catalog
        .create_entry(&a_descriptor_entry(
            "LFM2.5-1.2B",
            DescriptorEntryKind::HandMade,
            300,
            a_model_member("mlx", "LFM2.5-1.2B-MLX", "4bit")?,
        )?)
        .await?;
    let logo = LogoUri::try_new(uri)?;
    catalog
        .set_family_logo(
            &CatalogScope::Global,
            &CatalogName::try_new("LFM2.5")?,
            Some(&logo),
            None,
        )
        .await?;

    let output = fixture.command()?.args(["show", "LFM2.5-1.2B"]).output()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let entry: Value = serde_json::from_slice(&output.stdout)?;
    let digest = entry["family_logo_sha256"]
        .as_str()
        .context("catalog logo digest")?;
    let output = fixture
        .command()?
        .args(["show-family-logo", digest])
        .output()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(String::from_utf8(output.stdout)?, format!("{uri}\n"));
    Ok(())
}

#[tokio::test]
async fn cli_reports_a_missing_family_logo() -> anyhow::Result<()> {
    let fixture = Fixture::new().await?;
    let digest = "0".repeat(64);
    let output = fixture
        .command()?
        .args(["show-family-logo", &digest])
        .output()?;
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8(output.stderr)?;
    assert!(
        stderr.contains(&format!("no catalog family logo with SHA-256 {digest}")),
        "{stderr}"
    );
    Ok(())
}

#[rstest]
#[case::neither(&[], "--clear")]
#[case::both(&[LOGO, "--clear"], "--clear")]
#[case::other_host(&["https://example.com/logos/lfm.svg"], "https://pipette.liquid.ai/logos/")]
fn set_family_logo_refuses_bad_arguments(
    #[case] args: &[&str],
    #[case] mentioned: &str,
) -> anyhow::Result<()> {
    let output = Command::new(env!("CARGO_BIN_EXE_pipette-mgmt"))
        .args(["catalog", "set-family-logo", "LFM2.5"])
        .args(args)
        .env_remove("PIPETTE_MGMT_CONFIG")
        .output()?;
    // Exit status 2 is a usage error, which clap gives before the command runs.
    assert_eq!(output.status.code(), Some(2), "{args:?}");
    assert!(
        String::from_utf8(output.stderr)?.contains(mentioned),
        "{args:?}"
    );
    Ok(())
}
