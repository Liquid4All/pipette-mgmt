use std::path::Path;
use std::process::{Command, Output};

use anyhow::Context;
use rstest::rstest;

use pipette_core::config::Config;

const MINIMAL_CONFIG: &str =
    "evals_server_url = \"http://localhost:8000\"\n\n[auth_storage]\nbackend = \"local_fs\"\n";

/// The binary with `args`, no config path in its environment, and no AWS
/// credentials, so a check that reaches for storage fails.
fn command(args: &[&str]) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_pipette-mgmt"));
    command
        .args(args)
        .env("RUST_LOG", "info")
        .env_remove("PIPETTE_MGMT_CONFIG")
        .env_remove("AWS_ACCESS_KEY_ID")
        .env_remove("AWS_SECRET_ACCESS_KEY")
        .env_remove("AWS_SESSION_TOKEN")
        .env_remove("AWS_PROFILE");
    command
}

fn run(args: &[&str]) -> anyhow::Result<Output> {
    command(args).output().context("run pipette-mgmt")
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn write_config(dir: &Path, content: &str) -> anyhow::Result<std::path::PathBuf> {
    let path = dir.join("config.toml");
    std::fs::write(&path, content)?;
    Ok(path)
}

enum ConfigSource {
    BeforeSubcommand,
    AfterSubcommand,
    Env,
}

#[rstest]
#[case::before_subcommand(ConfigSource::BeforeSubcommand)]
#[case::after_subcommand(ConfigSource::AfterSubcommand)]
#[case::env(ConfigSource::Env)]
fn finds_the_config(#[case] source: ConfigSource) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let path = write_config(dir.path(), MINIMAL_CONFIG)?;
    let path = path.to_string_lossy();

    let output = match source {
        ConfigSource::BeforeSubcommand => run(&["--config", &path, "config", "check"])?,
        ConfigSource::AfterSubcommand => run(&["config", "check", "--config", &path])?,
        ConfigSource::Env => command(&["config", "check"])
            .env("PIPETTE_MGMT_CONFIG", &*path)
            .output()
            .context("run pipette-mgmt")?,
    };
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(stderr(&output).contains("config check passed"));
    Ok(())
}

#[test]
fn reports_every_missing_rule_with_an_action_and_strict_fails() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let path = write_config(dir.path(), MINIMAL_CONFIG)?;
    let config = Config::load(&path)?;
    let path_arg = path.to_string_lossy();

    let normal = run(&["config", "check", "--config", &path_arg])?;
    assert!(normal.status.success(), "{}", stderr(&normal));
    let warnings = stderr(&normal);
    for (runtime_type, flavor) in config.descriptor_catalog.unruled_flavors() {
        assert!(
            warnings.contains(&format!("runtime_type={runtime_type} flavor={flavor}")),
            "missing warning for {runtime_type} {flavor}: {warnings}"
        );
    }
    let count = config.descriptor_catalog.unruled_flavors().count();
    assert_eq!(
        warnings
            .matches("offer rule missing: flavor never offered; add a [[descriptor_catalog.offer_rules]] entry to offer it")
            .count(),
        count
    );

    let strict = run(&["config", "check", "--config", &path_arg, "--strict"])?;
    assert!(!strict.status.success());
    let strict_output = stderr(&strict);
    assert_eq!(strict_output.matches("offer rule missing").count(), count);
    assert!(strict_output.contains(&format!("{count} known flavors have no offer rule")));
    assert!(strict_output.contains("run without --strict"));
    Ok(())
}

#[test]
fn strict_passes_when_every_known_flavor_has_a_rule() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let path = write_config(dir.path(), MINIMAL_CONFIG)?;
    let config = Config::load(&path)?;
    let rules = config
        .descriptor_catalog
        .unruled_flavors()
        .map(|(runtime_type, flavor)| {
            format!(
                "\n[[descriptor_catalog.offer_rules]]\nruntime_type = \"{runtime_type}\"\nflavor = \"{flavor}\"\nrequires = [\"os:macos\"]\n"
            )
        })
        .collect::<String>();
    std::fs::write(&path, format!("{MINIMAL_CONFIG}{rules}"))?;

    let output = run(&[
        "config",
        "check",
        "--config",
        &path.to_string_lossy(),
        "--strict",
    ])?;
    let message = stderr(&output);
    assert!(output.status.success(), "{message}");
    assert!(message.contains("config check passed"));
    assert!(!message.contains("offer rule missing"));
    Ok(())
}

#[rstest]
#[case::top_level_key(
    format!("evals_server_ur = \"http://localhost:8000\"\n{MINIMAL_CONFIG}"),
    "evals_server_ur"
)]
#[case::section(format!("{MINIMAL_CONFIG}\n[storag]\nbackend = \"s3\"\n"), "storag")]
fn warns_on_an_unknown_key_and_strict_fails(
    #[case] content: String,
    #[case] key: &str,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let path = write_config(dir.path(), &content)?;
    let path = path.to_string_lossy();

    let normal = run(&["config", "check", "--config", &path])?;
    let warnings = stderr(&normal);
    assert!(normal.status.success(), "{warnings}");
    assert!(
        warnings.contains("config key unknown") && warnings.contains(&format!("key={key}")),
        "{warnings}"
    );

    let strict = run(&["config", "check", "--config", &path, "--strict"])?;
    let message = stderr(&strict);
    assert!(!strict.status.success(), "{message}");
    assert!(message.contains("1 keys are unknown"), "{message}");
    Ok(())
}

const UNKNOWN_FLAVOR_RULE: &str = "\n[[descriptor_catalog.offer_rules]]\nruntime_type = \"llamacpp_cli_stock_tools\"\nflavor = \"macos-unknown\"\nrequires = [\"os:macos\"]\n";
const NON_CANONICAL_FLAG_RULE: &str = "\n[[descriptor_catalog.offer_rules]]\nruntime_type = \"llamacpp_cli_stock_tools\"\nflavor = \"macos-x64\"\nrequires = [\"OS:macOS\"]\n";

/// `content` is the whole file, a rule after `MINIMAL_CONFIG`, or no file.
enum BadConfig {
    Whole(&'static str),
    AfterMinimal(&'static str),
    Absent,
}

#[rstest]
#[case::invalid_toml(
    BadConfig::Whole("evals_server_url = [\n"),
    "failed to parse config file"
)]
#[case::unknown_flavor(
    BadConfig::AfterMinimal(UNKNOWN_FLAVOR_RULE),
    "offer rule llamacpp_cli_stock_tools macos-unknown: unknown flavor"
)]
#[case::non_canonical_flag(
    BadConfig::AfterMinimal(NON_CANONICAL_FLAG_RULE),
    "non-canonical flag `OS:macOS`"
)]
#[case::missing_file(BadConfig::Absent, "failed to read config file")]
fn rejects_bad_config_with_the_path_and_specific_error(
    #[case] content: BadConfig,
    #[case] expected: &str,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let path = match content {
        BadConfig::Whole(content) => write_config(dir.path(), content)?,
        BadConfig::AfterMinimal(rule) => {
            write_config(dir.path(), &format!("{MINIMAL_CONFIG}{rule}"))?
        }
        BadConfig::Absent => dir.path().join("absent.toml"),
    };
    let output = run(&["config", "check", "--config", &path.to_string_lossy()])?;
    let message = stderr(&output);
    assert!(!output.status.success(), "{message}");
    assert!(message.contains(&path.display().to_string()), "{message}");
    assert!(message.contains(expected), "{message}");
    Ok(())
}

#[test]
fn s3_config_checks_without_runtime_credentials() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let path = write_config(
        dir.path(),
        &format!(
            "{MINIMAL_CONFIG}\n[storage]\nbackend = \"s3\"\nbucket = \"unused-test-bucket\"\nregion = \"us-east-1\"\n"
        ),
    )?;
    let output = run(&["config", "check", "--config", &path.to_string_lossy()])?;
    let message = stderr(&output);
    assert!(output.status.success(), "{message}");
    assert!(message.contains("config check passed"));
    assert!(!message.contains("config key unknown"), "{message}");
    Ok(())
}
