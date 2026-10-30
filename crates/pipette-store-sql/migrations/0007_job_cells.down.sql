-- A binary before 0007 reads no cell and no run flags, so the down script
-- drops both.
DROP INDEX plan_jobs_rerun_of;
ALTER TABLE submission_outcomes DROP COLUMN run_flags;
DROP TABLE job_cells;
