ALTER TABLE descriptor_members DROP CONSTRAINT descriptor_members_scope_name_key_fkey;
ALTER TABLE descriptor_members ADD CONSTRAINT descriptor_members_scope_name_key_fkey
    FOREIGN KEY (scope, name_key) REFERENCES descriptor_entries (scope, name_key) ON DELETE CASCADE;
