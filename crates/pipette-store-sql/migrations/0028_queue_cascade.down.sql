DROP TABLE plan_index_gc_candidates;

ALTER TABLE client_liveness DROP CONSTRAINT client_liveness_client_id_fkey;

ALTER TABLE queue_lost_leases DROP CONSTRAINT queue_lost_leases_job_id_fkey;
ALTER TABLE queue_denials DROP CONSTRAINT queue_denials_job_id_fkey;
ALTER TABLE queue_eligibility DROP CONSTRAINT queue_eligibility_job_id_fkey;
