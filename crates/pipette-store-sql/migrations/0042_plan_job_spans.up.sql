-- Plan membership as spans. One row says that the plans of a lineage from
-- `from_plan` up to `to_plan` (exclusive) list a job with these field values.
-- `lineage` is the plan's `campaign_id`, or the plan's own `plan_id` for a plan
-- with no campaign. `to_plan` is NULL while the newest plan of the lineage lists
-- the job. The five other columns have the meaning of the `plan_jobs` columns.
-- The bounds are plan ids, which sort in filing order. Two spans of one job in
-- one lineage do not overlap.
CREATE TABLE plan_job_spans (
    lineage      text NOT NULL,
    job_id       text NOT NULL,
    from_plan    text NOT NULL,
    to_plan      text,
    ordinal      integer,
    content_key  text,
    rerun_of     text,
    expired      jsonb,
    outcome      text,
    PRIMARY KEY (lineage, job_id, from_plan)
);

CREATE INDEX plan_job_spans_open ON plan_job_spans (lineage, to_plan, from_plan);
CREATE INDEX plan_job_spans_rerun_of ON plan_job_spans (rerun_of) WHERE rerun_of IS NOT NULL;

-- One row for each lineage that has span writes. Each transaction that writes
-- the spans of a lineage first increments `writes`. At REPEATABLE READ, two
-- such transactions with overlapping views thus conflict, and the store runs
-- the loser again.
CREATE TABLE plan_lineages (
    lineage  text PRIMARY KEY,
    writes   bigint NOT NULL
);
