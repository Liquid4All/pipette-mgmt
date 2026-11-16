-- A job's family: the cells of one campaign that differ only in their sizes,
-- the benchmark params that scale a run's work. `sizes` lists them in the one
-- order of the family's benchmark type, at most six. The claim hands out a job
-- later the more smaller rows of its family are queued. Ingestion sets both
-- values or neither; older rows have neither.
ALTER TABLE queue_jobs
  ADD COLUMN family_key text,
  ADD COLUMN sizes bigint[],
  ADD CONSTRAINT queue_jobs_family_both CHECK ((family_key IS NULL) = (sizes IS NULL)),
  ADD CONSTRAINT queue_jobs_sizes_compared CHECK (cardinality(sizes) <= 6);
CREATE INDEX queue_jobs_family ON queue_jobs (family_key) WHERE family_key IS NOT NULL;

-- Whether a run of `smaller` sizes is smaller than a run of `larger` ones of
-- its family: no size is larger and the two differ. Twins, equal in every
-- size, rank each other in neither direction. One plain expression over
-- the six positions, so the planner inlines it: the claim runs it for every
-- pair of a candidate and a row of its family.
CREATE FUNCTION queue_sizes_below(smaller bigint[], larger bigint[]) RETURNS boolean
LANGUAGE sql IMMUTABLE PARALLEL SAFE
RETURN smaller <> larger
  AND coalesce(smaller[1], 0) <= coalesce(larger[1], 0)
  AND coalesce(smaller[2], 0) <= coalesce(larger[2], 0)
  AND coalesce(smaller[3], 0) <= coalesce(larger[3], 0)
  AND coalesce(smaller[4], 0) <= coalesce(larger[4], 0)
  AND coalesce(smaller[5], 0) <= coalesce(larger[5], 0)
  AND coalesce(smaller[6], 0) <= coalesce(larger[6], 0);
