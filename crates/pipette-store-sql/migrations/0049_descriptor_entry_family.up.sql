-- The family that an operator set for a model entry. NULL means that readers
-- see the text of the entry name before its first hyphen.
ALTER TABLE descriptor_entries ADD COLUMN family text
    CHECK (family IS NULL OR descriptor_kind = 'model');
