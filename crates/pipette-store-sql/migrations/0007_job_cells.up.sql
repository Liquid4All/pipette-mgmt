-- The cell each minted job measures: its body's cell digest, which leaves out
-- the eligibility clauses. `find_cell_results` reads it to find a finished run
-- of a cell.
CREATE TABLE job_cells (
    job_id       text PRIMARY KEY,
    cell_digest  text NOT NULL
);

CREATE INDEX job_cells_cell_digest ON job_cells (cell_digest);

-- The flags a finished run held, from its submission record. NULL for a
-- failure, and for a row that the backfill has not reached.
ALTER TABLE submission_outcomes ADD COLUMN run_flags text[];

-- A lookup of the jobs that a re-run replaced.
CREATE INDEX plan_jobs_rerun_of ON plan_jobs (rerun_of) WHERE rerun_of IS NOT NULL;
