-- A global-only schema cannot retain entries in multiple namespaces.
DELETE FROM descriptor_entries WHERE scope <> '';

ALTER TABLE descriptor_members DROP CONSTRAINT descriptor_members_scope_name_key_fkey;
ALTER TABLE descriptor_members DROP CONSTRAINT descriptor_members_pkey;
ALTER TABLE descriptor_members DROP CONSTRAINT descriptor_members_scope_name_key_flavor_key;
ALTER TABLE descriptor_members DROP CONSTRAINT descriptor_members_scope_name_key_digest_key;
ALTER TABLE descriptor_entries DROP CONSTRAINT descriptor_entries_pkey;

DROP INDEX descriptor_entries_scope_release_time_idx;
DROP INDEX descriptor_entries_release_time_scope_idx;

ALTER TABLE descriptor_members DROP COLUMN scope;
ALTER TABLE descriptor_entries DROP COLUMN scope;

ALTER TABLE descriptor_entries ADD PRIMARY KEY (name_key);
ALTER TABLE descriptor_members ADD PRIMARY KEY (name_key, position);
ALTER TABLE descriptor_members ADD UNIQUE (name_key, flavor);
ALTER TABLE descriptor_members ADD UNIQUE (name_key, digest);
ALTER TABLE descriptor_members ADD CONSTRAINT descriptor_members_name_key_fkey
    FOREIGN KEY (name_key) REFERENCES descriptor_entries (name_key) ON DELETE CASCADE;

CREATE INDEX descriptor_entries_release_time_idx
    ON descriptor_entries (release_time DESC, name_key);
