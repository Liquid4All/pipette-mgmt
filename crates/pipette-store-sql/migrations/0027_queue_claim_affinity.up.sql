-- The key of the JSON value at `path` in `doc`: the md5 of its jsonb text.
-- jsonb sorts object keys, so the key order in `doc` does not change the key.
-- A document that is not an object, that has no value at `path`, or that
-- jsonb cannot represent gives NULL.
CREATE FUNCTION queue_json_key(doc text, path text[]) RETURNS text
LANGUAGE plpgsql IMMUTABLE STRICT PARALLEL SAFE AS $$
BEGIN
    IF json_typeof(doc::json) <> 'object' THEN
        RETURN NULL;
    END IF;
    RETURN md5((doc::jsonb #> path)::text);
EXCEPTION WHEN invalid_text_representation OR untranslatable_character OR numeric_value_out_of_range THEN
    -- The job stays claimable, with no claim affinity.
    RETURN NULL;
END;
$$;

-- The downloads that a job needs, as keys.
ALTER TABLE queue_jobs
    ADD COLUMN model_key text GENERATED ALWAYS AS (queue_json_key(body, '{spec,model}')) STORED,
    ADD COLUMN runtime_key text GENERATED ALWAYS AS (queue_json_key(body, '{spec,runtime}')) STORED;

-- One row per client: the keys of its last grant. A failure report or a lost
-- lease on that job deletes the row.
CREATE TABLE queue_client_last_grant (
    client_id   text PRIMARY KEY,
    job_id      text NOT NULL,
    model_key   text,
    runtime_key text,
    granted_at  timestamptz NOT NULL
);
