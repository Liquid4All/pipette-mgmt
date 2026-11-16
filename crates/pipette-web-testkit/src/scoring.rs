//! The scoring pipeline, run end to end.

use pipette_httpapi::state::AppState;

use crate::stores_of;

/// Run the full pipeline an eval job traverses: route, slow eval scoring, then
/// finalize. A non-eval job is done after the first pass.
pub async fn run_full_score(state: &AppState) -> anyhow::Result<()> {
    pipette_pipeline::score::run_process_submissions(&state.config, stores_of(state)).await?;
    pipette_pipeline::score::run_score_eval(&state.config, stores_of(state)).await?;
    pipette_pipeline::score::run_process_submissions(&state.config, stores_of(state)).await?;
    Ok(())
}
