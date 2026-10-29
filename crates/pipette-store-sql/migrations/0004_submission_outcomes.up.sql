-- The outcome of each submission record, `finished` or `failed`, from the
-- record's `message_type` (`docs/storage.md` §4.3).
CREATE TABLE submission_outcomes (
    job_id text PRIMARY KEY,
    outcome text NOT NULL CHECK (outcome IN ('finished', 'failed'))
);
