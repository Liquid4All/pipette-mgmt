//! Parity with `testdata/golden/{plan}/cell-NNN.json`, the bodies that
//! `pipette-plan generate` wrote for `examples/plans/scheduler/{plan}.toml`.
//!
//! The client no longer expands plans (PIP-633): edit a golden body by hand, in
//! the same change as its example.

use std::path::{Path, PathBuf};

use anyhow::Context;
use serde_json::Value;

use pipette_pipeline::content_key::content_digest;

fn manifest_dir() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

fn examples_dir() -> PathBuf {
    manifest_dir().join("../../examples/plans/scheduler")
}

fn golden_dir() -> PathBuf {
    manifest_dir().join("testdata/golden")
}

fn names(dir: &Path, keep: impl Fn(&Path) -> bool) -> anyhow::Result<Vec<String>> {
    let mut names = std::fs::read_dir(dir)
        .with_context(|| format!("reading {}", dir.display()))?
        .map(|entry| entry.map(|entry| entry.path()))
        .collect::<std::io::Result<Vec<_>>>()?
        .into_iter()
        .filter(|path| keep(path))
        .filter_map(|path| Some(path.file_stem()?.to_str()?.to_owned()))
        .collect::<Vec<_>>();
    names.sort();
    Ok(names)
}

fn is_toml(path: &Path) -> bool {
    path.extension().is_some_and(|ext| ext == "toml")
}

fn golden_bodies(plan: &str) -> anyhow::Result<Vec<Value>> {
    let dir = golden_dir().join(plan);
    names(&dir, |path| {
        path.extension().is_some_and(|ext| ext == "json")
    })?
    .iter()
    .map(|cell| {
        let path = dir.join(format!("{cell}.json"));
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))
    })
    .collect()
}

/// An example without golden output would go unchecked.
#[test]
fn every_example_has_golden_output() -> anyhow::Result<()> {
    let examples = names(&examples_dir(), is_toml)?;
    let golden = names(&golden_dir(), Path::is_dir)?;
    anyhow::ensure!(!examples.is_empty(), "no scheduler examples found");
    assert_eq!(examples, golden);
    Ok(())
}

/// Reads the examples directory at run time, so a new example needs no edit
/// here.
#[test]
fn expansion_matches_the_client() -> anyhow::Result<()> {
    let plans = names(&examples_dir(), is_toml)?;
    anyhow::ensure!(!plans.is_empty(), "no scheduler examples found");
    plans.iter().try_for_each(|plan| matches_the_client(plan))
}

fn matches_the_client(plan: &str) -> anyhow::Result<()> {
    let path = examples_dir().join(format!("{plan}.toml"));
    let document =
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let expansion = pipette_planner::expand(&pipette_planner::parse(
        &document,
        &pipette_planner::prefix_types(&document),
    )?)?;
    let golden = golden_bodies(plan)?;

    anyhow::ensure!(
        expansion.warnings.is_empty(),
        "{plan}: {:?}",
        expansion.warnings
    );
    anyhow::ensure!(
        expansion.bodies.len() == golden.len(),
        "{plan}: {} cells, the client wrote {}",
        expansion.bodies.len(),
        golden.len()
    );
    expansion
        .bodies
        .iter()
        .zip(&golden)
        .enumerate()
        .try_for_each(|(index, (ours, theirs))| {
            anyhow::ensure!(ours == theirs, "{plan} cell {index}: body differs");
            anyhow::ensure!(
                content_digest(ours) == content_digest(theirs),
                "{plan} cell {index}: content key differs"
            );
            Ok(())
        })
}
