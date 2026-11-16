-- Spread's short repetition rule at a re-run's filing, so the re-run is judged
-- by it: NULL when the policy had none, and for the other kinds.
ALTER TABLE campaign_group_rerun_results
    ADD COLUMN short_repetition_ms double precision,
    ADD COLUMN short_repetition_percent double precision;
