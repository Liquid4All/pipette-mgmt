-- Empty scope is global; every other value is an organization id.
ALTER TABLE descriptor_entries ADD COLUMN scope text NOT NULL DEFAULT '';
ALTER TABLE descriptor_members ADD COLUMN scope text NOT NULL DEFAULT '';

ALTER TABLE descriptor_members DROP CONSTRAINT descriptor_members_name_key_fkey;
ALTER TABLE descriptor_members DROP CONSTRAINT descriptor_members_pkey;
ALTER TABLE descriptor_members DROP CONSTRAINT descriptor_members_name_key_flavor_key;
ALTER TABLE descriptor_members DROP CONSTRAINT descriptor_members_name_key_digest_key;
ALTER TABLE descriptor_entries DROP CONSTRAINT descriptor_entries_pkey;

ALTER TABLE descriptor_entries ADD PRIMARY KEY (scope, name_key);
ALTER TABLE descriptor_entries ADD CONSTRAINT descriptor_entries_org_kind_check
    CHECK (scope = '' OR kind = 'hand-made');
ALTER TABLE descriptor_entries ADD CONSTRAINT descriptor_entries_creator_scope_check
    CHECK ((scope = '' AND created_by NOT LIKE 'account:%')
        OR (scope <> '' AND created_by NOT LIKE 'importer:%'));
ALTER TABLE descriptor_members ADD PRIMARY KEY (scope, name_key, position);
ALTER TABLE descriptor_members ADD UNIQUE (scope, name_key, flavor);
ALTER TABLE descriptor_members ADD UNIQUE (scope, name_key, digest);
ALTER TABLE descriptor_members ADD CONSTRAINT descriptor_members_scope_name_key_fkey
    FOREIGN KEY (scope, name_key) REFERENCES descriptor_entries (scope, name_key) ON DELETE CASCADE;

DROP INDEX descriptor_entries_release_time_idx;
CREATE INDEX descriptor_entries_scope_release_time_idx
    ON descriptor_entries (scope, release_time DESC, name_key);
CREATE INDEX descriptor_entries_release_time_scope_idx
    ON descriptor_entries (release_time DESC, scope, name_key);
