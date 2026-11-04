CREATE TABLE submission_outcome_repairs (
    job_id text PRIMARY KEY,
    generation bigint NOT NULL,
    pending boolean NOT NULL,
    updated_at timestamptz NOT NULL DEFAULT now()
);

CREATE INDEX submission_outcome_repairs_pending_age
    ON submission_outcome_repairs (updated_at) WHERE pending;
