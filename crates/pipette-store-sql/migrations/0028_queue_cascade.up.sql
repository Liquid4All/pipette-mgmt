-- A job's eligibility, denial and lost-lease rows go with its queue row.
DELETE FROM queue_eligibility e
WHERE NOT EXISTS (SELECT 1 FROM queue_jobs j WHERE j.job_id = e.job_id);
DELETE FROM queue_denials d
WHERE NOT EXISTS (SELECT 1 FROM queue_jobs j WHERE j.job_id = d.job_id);
DELETE FROM queue_lost_leases l
WHERE NOT EXISTS (SELECT 1 FROM queue_jobs j WHERE j.job_id = l.job_id);

ALTER TABLE queue_eligibility ADD CONSTRAINT queue_eligibility_job_id_fkey
  FOREIGN KEY (job_id) REFERENCES queue_jobs (job_id) ON DELETE CASCADE;
ALTER TABLE queue_denials ADD CONSTRAINT queue_denials_job_id_fkey
  FOREIGN KEY (job_id) REFERENCES queue_jobs (job_id) ON DELETE CASCADE;
ALTER TABLE queue_lost_leases ADD CONSTRAINT queue_lost_leases_job_id_fkey
  FOREIGN KEY (job_id) REFERENCES queue_jobs (job_id) ON DELETE CASCADE;

-- A deleted client's last-seen time cannot outlive its record.
DELETE FROM client_liveness l
WHERE NOT EXISTS (SELECT 1 FROM clients c WHERE c.client_id = l.client_id);
ALTER TABLE client_liveness ADD CONSTRAINT client_liveness_client_id_fkey
  FOREIGN KEY (client_id) REFERENCES clients (client_id) ON DELETE CASCADE;

-- The plan-index keys that queue-maintenance found orphaned. A key that a run
-- finds orphaned again is deleted.
CREATE TABLE plan_index_gc_candidates (
  key           text PRIMARY KEY,
  first_seen_at timestamptz NOT NULL DEFAULT now()
);

-- The cascade removes the queue orphans, so the queue keeps no candidates.
DELETE FROM queue_maintenance_state WHERE name = 'gc-candidates';
