//! The scoring pipeline, run end to end.

use pipette_core::config::Config;
use pipette_store_fs::build_local_fs_stores;

/// Run the full pipeline an eval job traverses: route, slow eval scoring, then
/// finalize. A non-eval job is done after the first pass.
pub async fn run_full_score(config: &Config) -> anyhow::Result<()> {
    pipette_pipeline::score::run_process_submissions(config, build_local_fs_stores(config)?)
        .await?;
    pipette_pipeline::score::run_score_eval(config, build_local_fs_stores(config)?).await?;
    pipette_pipeline::score::run_process_submissions(config, build_local_fs_stores(config)?)
        .await?;
    Ok(())
}
