-- queue-maintenance rebuilds the eligibility rows of each available job that
-- carries the flag, then clears it. An insert sets it, and so do a recycle, a
-- re-date and a client reindex that skips a leased job.
ALTER TABLE queue_jobs ADD COLUMN needs_reindex boolean NOT NULL DEFAULT true;

-- The workers that the last index pass matched against.
CREATE TABLE queue_workers (client_id text PRIMARY KEY);
-- The former reader treated a corrupt snapshot as an empty set.
DO $$
DECLARE
    workers jsonb;
BEGIN
    BEGIN
        SELECT body::jsonb INTO workers FROM queue_maintenance_state
        WHERE name = 'eligible-workers';
    EXCEPTION WHEN data_exception THEN
        workers := NULL;
    END;

    IF jsonb_typeof(workers) = 'array' THEN
        IF NOT EXISTS (
            SELECT 1 FROM jsonb_array_elements(workers) AS entry
            WHERE jsonb_typeof(entry) <> 'string'
        ) THEN
            INSERT INTO queue_workers (client_id)
            SELECT jsonb_array_elements_text(workers)
            ON CONFLICT DO NOTHING;
        END IF;
    END IF;
END;
$$;

-- A listing joins queue_jobs for the live deadline.
ALTER TABLE queue_eligibility DROP COLUMN expires_at;

DROP TABLE queue_reindex_jobs;
DELETE FROM queue_maintenance_state WHERE name IN ('eligible-indexed', 'eligible-workers');
