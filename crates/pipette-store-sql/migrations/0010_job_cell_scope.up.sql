-- The scope that may reuse the run of a job: `org:<org_id>` for a campaign
-- owner in an organization, `user:<user_id>` for a user in none. A lookup
-- matches the scope of the plan exactly, so a row with no scope answers no
-- plan. Ingestion writes the scope when it mints a job. The rows written
-- before this column existed, and the rows of the backfill, have none.
ALTER TABLE job_cells ADD COLUMN scope text;
