CREATE TABLE catalog_import_runs (
    id text PRIMARY KEY,
    started_at timestamptz NOT NULL,
    finished_at timestamptz,
    command text NOT NULL CHECK (command IN ('import', 'refresh')),
    source text NOT NULL DEFAULT 'llamacpp',
    target_name text,
    result text NOT NULL CHECK (result IN ('running', 'success', 'error')),
    entries_created integer NOT NULL DEFAULT 0,
    members_added integer NOT NULL DEFAULT 0,
    unclassified_assets jsonb NOT NULL DEFAULT '[]'::jsonb,
    error_summary text
);
CREATE INDEX catalog_import_runs_started_at ON catalog_import_runs (started_at DESC);
