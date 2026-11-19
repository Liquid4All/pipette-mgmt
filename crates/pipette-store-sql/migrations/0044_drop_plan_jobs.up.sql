-- `plan_job_spans` (0042) records plan membership. No build from PIP-904 on
-- reads or writes `plan_jobs`, so a rollback to such a build needs no table.
DROP TABLE plan_jobs;
