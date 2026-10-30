CREATE TABLE descriptor_entries (
    name_key text PRIMARY KEY,
    name text NOT NULL,
    kind text NOT NULL CHECK (kind IN ('stable', 'daily', 'hand-made')),
    state text NOT NULL CHECK (state IN ('pinned', 'active', 'hidden', 'tombstoned')),
    runtime_type text NOT NULL,
    release_time timestamptz NOT NULL,
    created_at timestamptz NOT NULL,
    created_by text NOT NULL,
    CONSTRAINT descriptor_entries_name_key_check CHECK (name_key = lower(name COLLATE "C"))
);
CREATE INDEX descriptor_entries_release_time_idx ON descriptor_entries (release_time DESC, name_key);

CREATE TABLE descriptor_members (
    name_key text NOT NULL REFERENCES descriptor_entries (name_key) ON DELETE CASCADE,
    position integer NOT NULL CHECK (position >= 0),
    flavor text,
    digest text NOT NULL,
    descriptor text NOT NULL,
    added_at timestamptz NOT NULL,
    PRIMARY KEY (name_key, position),
    UNIQUE (name_key, flavor),
    UNIQUE (name_key, digest)
);
CREATE INDEX descriptor_members_digest_idx ON descriptor_members (digest);
