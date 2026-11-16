DROP FUNCTION queue_sizes_below(bigint[], bigint[]);
DROP INDEX queue_jobs_family;
ALTER TABLE queue_jobs DROP CONSTRAINT queue_jobs_sizes_compared,
  DROP CONSTRAINT queue_jobs_family_both,
  DROP COLUMN family_key, DROP COLUMN sizes;
