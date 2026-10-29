//! The local-dispatch examples in `examples/plans/*.toml`, copies of the ones
//! `pipette-ctl` ships, parse and expand, so a breaking change fails here
//! before a client bumps its pin. `pipette-planner` covers the scheduler ones.

use std::path::PathBuf;

use anyhow::Context;
use rstest::rstest;

use pipette_plan_types::Plan;

/// Expansion yields at least one runnable cell; a flag entry naming a cell
/// outside the plan fails it.
#[rstest]
fn local_example_parses_and_expands(
    #[base_dir = "../../examples/plans"]
    #[files("*.toml")]
    path: PathBuf,
) -> anyhow::Result<()> {
    let cells = Plan::load(&path)?
        .runnable_cells()
        .with_context(|| format!("expanding {}", path.display()))?;
    anyhow::ensure!(!cells.is_empty(), "{} expands to no cells", path.display());
    Ok(())
}
