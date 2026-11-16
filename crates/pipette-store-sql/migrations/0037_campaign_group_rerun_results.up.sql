-- Each re-run a campaign group's policies filed, one row per policy that re-ran
-- it: the check's value on the run it replaces, and on the re-run once its rows
-- land. `after`, `after_code`, `outcome` and `judged_at` are NULL while it waits.
CREATE TABLE campaign_group_rerun_results (
    group_id      text NOT NULL REFERENCES campaign_groups (group_id) ON DELETE CASCADE,
    rerun_job_id  text NOT NULL,
    kind          text NOT NULL,
    campaign_id   text NOT NULL,
    rerun_of      text NOT NULL,
    plan_id       text NOT NULL,
    filed_at      timestamptz NOT NULL,
    attempt       integer NOT NULL,
    benchmark_id  text NOT NULL,
    model_key     text,
    before        double precision NOT NULL,
    threshold     double precision,
    before_code   text,
    after         double precision,
    after_code    text,
    outcome       text CHECK (outcome IN ('cleared', 'flagged', 'failed', 'dropped')),
    judged_at     timestamptz,
    PRIMARY KEY (group_id, rerun_job_id, kind),
    CHECK ((outcome IS NULL) = (judged_at IS NULL))
);

-- A member's rows that wait, which the policy pass reads every run.
CREATE INDEX campaign_group_rerun_results_waiting
    ON campaign_group_rerun_results (group_id, campaign_id) WHERE judged_at IS NULL;
