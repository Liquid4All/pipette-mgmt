use std::num::NonZeroUsize;
use std::process::Command;
use std::sync::Arc;

use anyhow::Context;
use rstest::rstest;
use serde_json::Value;

use pipette_core::config::Config;
use pipette_core::organization::Organization;
use pipette_plan_types::{
    AuditAction, AuditOutcome, CatalogName, CatalogScope, DescriptorEntry, DescriptorEntryCreator,
    DescriptorEntryKind, DescriptorEntryState, NonEmptyTrimmedString, NonEmptyVec, OrgSlug, UserId,
};
use pipette_store::{AuditFilter, CreateEntryOutcome};
use pipette_store_sql::{SqlTodoStore, scratch::TestDb};
use pipette_store_testkit::{a_descriptor_entry, a_member, a_model_member};

#[rstest]
#[tokio::test]
async fn cli_move_to_global_audits_and_logs_only_a_write(
    #[values(false, true)] dry_run: bool,
    #[values(false, true)] taken: bool,
    #[values(false, true)] fail_audit: bool,
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
             [database]\nhost = {}\nport = {}\nuser = {}\ndatabase = {}\nssl_mode = \"disable\"\n",
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
    let catalog = stores.descriptor_catalog.as_ref().context("SQL catalog")?;
    let account = DescriptorEntryCreator::Account(UserId::try_new(
        "user_0195c3e2-a4f3-7b8c-9d1e-2f3a4b5c6d7e",
    )?);
    let at = jiff::Timestamp::from_second(300).context("valid time")?;
    let entry = DescriptorEntry::new(
        CatalogScope::Org(org.org_id.clone()),
        CatalogName::try_new("AcmeLM")?,
        DescriptorEntryKind::HandMade,
        DescriptorEntryState::Active,
        at,
        at,
        account.clone(),
        NonEmptyVec::try_new(vec![
            a_model_member("gguf_text", "AcmeLM-Q4-GGUF", "q4_0")?,
            a_model_member("gguf_text", "AcmeLM-Q8-GGUF", "q8_0")?,
        ])?,
    )?;
    assert_eq!(
        catalog.create_entry(&entry).await?,
        CreateEntryOutcome::Created
    );
    let holder = a_descriptor_entry(
        "AcmeLM",
        DescriptorEntryKind::HandMade,
        1,
        a_member("b1", "linux-x64-rocm")?,
    )?;
    if taken {
        assert_eq!(
            catalog.create_entry(&holder).await?,
            CreateEntryOutcome::Created
        );
    }

    if fail_audit {
        // Fail after the audit event and its org lookup row have been inserted.
        test_db
            .execute(
                "CREATE FUNCTION refuse_audit_record() RETURNS trigger LANGUAGE plpgsql
                 AS $$ BEGIN RAISE EXCEPTION 'injected audit write failure'; END $$;
             CREATE TRIGGER refuse_audit_record BEFORE INSERT ON audit_event_records
                 FOR EACH ROW EXECUTE FUNCTION refuse_audit_record();",
            )
            .await?;
    }

    let mut command = Command::new(env!("CARGO_BIN_EXE_pipette-mgmt"));
    command
        .args(["--log-format", "json", "--config"])
        .arg(&config_path)
        .args(["catalog", "move-to-global", "AcmeLM", "--org", "acme"])
        .env_remove("PIPETTE_MGMT_CONFIG")
        .env("RUST_LOG", "info")
        // The child uses the same isolated, migrated schema as the fixture.
        .env(
            "PGOPTIONS",
            options.get_options().context("test schema options")?,
        );
    if dry_run {
        command.arg("--dry-run");
    }
    let output = command.output().context("run catalog move-to-global")?;
    let stderr = String::from_utf8(output.stderr)?;
    let stdout = String::from_utf8(output.stdout)?;
    let audit_failed = fail_audit && !dry_run && !taken;
    assert_eq!(
        output.status.success(),
        !taken && !audit_failed,
        "{stdout}\n{stderr}"
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
    let moves = page
        .items
        .iter()
        .filter(|event| matches!(event.action, AuditAction::CatalogMoveToGlobal { .. }))
        .collect::<Vec<_>>();
    let move_logs = stderr
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|line| line["message"] == "catalog entry moved to global scope")
        .collect::<Vec<_>>();
    let global = catalog
        .get_entry(&CatalogScope::Global, &entry.name)
        .await?;
    let in_org = catalog.get_entry(&entry.scope, &entry.name).await?;
    let wrote = !dry_run && !taken && !fail_audit;
    if taken {
        assert!(
            stderr.contains("already holds the catalog name AcmeLM"),
            "{stderr}"
        );
    } else if audit_failed {
        assert!(stderr.contains("injected audit write failure"), "{stderr}");
        assert!(!stdout.contains("Moved "), "{stdout}");
        assert!(page.items.is_empty(), "{page:?}");
    } else {
        let prefix = if dry_run { "Would move" } else { "Moved" };
        let line = format!("{prefix} {}/AcmeLM to global scope", org.org_id);
        assert!(stdout.contains(&line), "{stdout}");
        assert!(stdout.contains(&format!("Creator: operator replaces {account}")));
    }
    if wrote {
        assert_eq!(moves.len(), 1, "{page:?}");
        assert_eq!(moves[0].outcome, AuditOutcome::Succeeded);
        assert!(moves[0].org_ids.contains(&org.org_id));
        assert_eq!(
            moves[0].action,
            AuditAction::CatalogMoveToGlobal {
                org_id: org.org_id.clone(),
                name: entry.name.clone(),
                original_created_by: account,
            }
        );
        assert_eq!(move_logs.len(), 1, "{stderr}");
        assert_eq!(move_logs[0]["level"], "INFO");
        assert_eq!(move_logs[0]["catalog_name"], "AcmeLM");
        assert_eq!(move_logs[0]["org_id"], org.org_id.as_str());
        assert_eq!(move_logs[0]["member_count"], 2);
        let moved = DescriptorEntry {
            scope: CatalogScope::Global,
            created_by: DescriptorEntryCreator::Operator,
            ..entry
        };
        assert_eq!((in_org, global), (None, Some(moved)));
    } else {
        assert!(moves.is_empty(), "{page:?}");
        assert!(move_logs.is_empty(), "{stderr}");
        assert_eq!(in_org, Some(entry));
        assert_eq!(global, taken.then_some(holder));
    }
    Ok(())
}
