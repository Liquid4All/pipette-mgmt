CREATE TABLE queue_reindex_jobs (job_id text PRIMARY KEY);
-- An available job is re-indexed when the indexed set is missing; a leased one needs its flag.
INSERT INTO queue_reindex_jobs (job_id)
SELECT job_id FROM queue_jobs WHERE needs_reindex AND leased_by IS NOT NULL;

ALTER TABLE queue_eligibility ADD COLUMN expires_at timestamptz;
UPDATE queue_eligibility e SET expires_at = j.expires_at FROM queue_jobs j WHERE j.job_id = e.job_id;

INSERT INTO queue_maintenance_state (name, body)
SELECT 'eligible-workers', coalesce(json_agg(client_id ORDER BY client_id), '[]'::json)::text
FROM queue_workers
ON CONFLICT (name) DO UPDATE SET body = EXCLUDED.body;
DROP TABLE queue_workers;

ALTER TABLE queue_jobs DROP COLUMN needs_reindex;
