-- The table of 0001, 0002 and 0007, filled from the spans through the
-- `plan_job_rows` view (0043). The build of 0043 reads spans and writes this
-- table, so it starts from rows that match the spans.
CREATE TABLE plan_jobs (
    plan_id      text NOT NULL REFERENCES plans (plan_id) ON DELETE CASCADE,
    job_id       text NOT NULL,
    ordinal      integer,
    content_key  text,
    rerun_of     text,
    expired      jsonb,
    outcome      text,
    PRIMARY KEY (plan_id, job_id),
    UNIQUE (plan_id, ordinal)
);

CREATE INDEX plan_jobs_rerun_of ON plan_jobs (rerun_of) WHERE rerun_of IS NOT NULL;

INSERT INTO plan_jobs (plan_id, job_id, ordinal, content_key, rerun_of, expired, outcome)
SELECT plan_id, job_id, ordinal, content_key, rerun_of, expired, outcome
FROM plan_job_rows;
