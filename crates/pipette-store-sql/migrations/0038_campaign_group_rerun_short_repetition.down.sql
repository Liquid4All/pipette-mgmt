-- Re-runs filed under a short repetition rule are then judged by percent alone.
ALTER TABLE campaign_group_rerun_results
    DROP COLUMN short_repetition_ms,
    DROP COLUMN short_repetition_percent;
