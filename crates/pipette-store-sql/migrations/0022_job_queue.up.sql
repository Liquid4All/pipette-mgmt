-- One row per queued job. `leased_by IS NULL` means available.
CREATE TABLE queue_jobs (
  job_id        text PRIMARY KEY,
  body          text NOT NULL,                 -- the job JSON that ingestion promoted
  expires_at    timestamptz,                   -- the deadline; NULL = never
  enqueued_at   timestamptz NOT NULL DEFAULT now(),
  leased_by     text,                          -- the client that holds the lease
  lease_expiry  timestamptz,
  revision      bigint NOT NULL,               -- CAS: every write of the row bumps it
  CHECK ((leased_by IS NULL) = (lease_expiry IS NULL))
);
CREATE INDEX queue_jobs_avail  ON queue_jobs (expires_at) WHERE leased_by IS NULL;
CREATE INDEX queue_jobs_leased ON queue_jobs (leased_by, lease_expiry) WHERE leased_by IS NOT NULL;

-- Staged bodies between write_tmp and promote_avail.
CREATE TABLE queue_tmp (job_id text PRIMARY KEY, body text NOT NULL, written_at timestamptz NOT NULL DEFAULT now());

-- The eligibility index that queue-maintenance writes and claim reads. The
-- expires_at column stays until the simplification stage drops it.
CREATE TABLE queue_eligibility (
  client_id text NOT NULL, job_id text NOT NULL, expires_at timestamptz,
  PRIMARY KEY (client_id, job_id)
);
CREATE INDEX queue_eligibility_job ON queue_eligibility (job_id);

-- One row per (job, client) retriable failure. Every field is nullable. A row
-- with every field NULL reads as DenialRecord::default.
CREATE TABLE queue_denials (
  job_id text NOT NULL, client_id text NOT NULL,
  denied_at timestamptz, failure_reason text, client_version text, runtime_version text,
  PRIMARY KEY (job_id, client_id)
);

-- One row per lease that ended without a result.
CREATE TABLE queue_lost_leases (
  job_id text NOT NULL, client_id text NOT NULL, lease_expiry timestamptz NOT NULL,
  PRIMARY KEY (job_id, client_id, lease_expiry)
);

-- One row per reindex request. The pass deletes only the rows it captured.
CREATE TABLE queue_reindex_requests (
  request_id text PRIMARY KEY, client_id text NOT NULL, requested_at timestamptz NOT NULL
);
CREATE INDEX queue_reindex_requests_client ON queue_reindex_requests (client_id);

CREATE TABLE queue_reindex_jobs (job_id text PRIMARY KEY);

CREATE TABLE queue_suspensions (
  client_id text PRIMARY KEY, suspended_at timestamptz NOT NULL, conflicting_job_id text NOT NULL
);

-- The last signed request of each device. SQL sets last_seen_at with now(),
-- so the replicas agree, as the S3 server clock does.
CREATE TABLE client_liveness (client_id text PRIMARY KEY, last_seen_at timestamptz NOT NULL);

-- Transitional: the five run-state blobs (.eligible-indexed, .eligible-workers,
-- .gc-candidates, .backlog, .backlog-campaigns) as text, keyed by name.
CREATE TABLE queue_maintenance_state (name text PRIMARY KEY, body text NOT NULL);
