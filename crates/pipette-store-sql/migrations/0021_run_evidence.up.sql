-- The run evidence index: for each descriptor digest and device, the last
-- success and the last failure, each with the client version of that run. The
-- scorer keeps it current; `db backfill-run-evidence` makes it again from the
-- warehouse. `digest` is a runtime or a model descriptor digest.
CREATE TABLE run_evidence (
    digest                       text NOT NULL,
    client_id                    text NOT NULL,
    last_success_at              timestamptz,
    last_success_client_version  text,
    last_failure_at              timestamptz,
    last_failure_client_version  text,
    PRIMARY KEY (digest, client_id),
    CHECK (last_success_at IS NOT NULL OR last_failure_at IS NOT NULL)
);
