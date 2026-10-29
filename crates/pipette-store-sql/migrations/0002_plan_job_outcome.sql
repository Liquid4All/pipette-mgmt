-- The outcome of a plan job that left the queue with a submission record:
-- `finished` or `failed`, from the manifest's `settled_jobs`. NULL for a job
-- whose record `queue-maintenance` has not read. A build that does not know the
-- column writes the other columns of a row and leaves this one as it is.
ALTER TABLE plan_jobs ADD COLUMN outcome text;
