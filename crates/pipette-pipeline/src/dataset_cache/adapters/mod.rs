//! One adapter per upstream layout: the only benchmark-specific code in the
//! dataset cache. Versioned in the catalog (`mmspec@1`), since what it emits
//! feeds the packed artifact's digest.

mod mmspec;

use std::path::Path;

use pipette_plan_types::DatasetAdapter;

use super::format::Corpus;

/// Read a checkout of the pinned upstream into a corpus.
pub(super) fn adapt(adapter: DatasetAdapter, root: &Path) -> anyhow::Result<Corpus> {
    match adapter {
        DatasetAdapter::MmspecV1 => mmspec::adapt(root),
        DatasetAdapter::EvalsServerV1 => {
            anyhow::bail!("evals-server@1 reads the evals server, not a git checkout")
        }
    }
}
