-- The table from 0022_job_queue, empty. A build that reads it finds no
-- suspended client.
CREATE TABLE queue_suspensions (
  client_id text PRIMARY KEY, suspended_at timestamptz NOT NULL, conflicting_job_id text NOT NULL
);
