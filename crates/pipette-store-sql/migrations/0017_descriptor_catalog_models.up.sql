-- An entry holds runtime members or model members. The entry's descriptor
-- type is its runtime type, or the model type of its first member.
ALTER TABLE descriptor_entries RENAME COLUMN runtime_type TO descriptor_type;
ALTER TABLE descriptor_entries ADD COLUMN descriptor_kind text NOT NULL DEFAULT 'runtime'
    CONSTRAINT descriptor_entries_descriptor_kind_check
    CHECK (descriptor_kind IN ('runtime', 'model'));
ALTER TABLE descriptor_entries ADD CONSTRAINT descriptor_entries_model_kind_check
    CHECK (descriptor_kind = 'runtime' OR kind = 'hand-made');

-- Each member keeps its own type, so a type filter finds every format of a
-- model entry. A model member has a format and a quantization; a runtime
-- member has neither.
ALTER TABLE descriptor_members ADD COLUMN descriptor_type text;
UPDATE descriptor_members m SET descriptor_type = e.descriptor_type
    FROM descriptor_entries e
    WHERE (e.scope, e.name_key) = (m.scope, m.name_key);
ALTER TABLE descriptor_members ALTER COLUMN descriptor_type SET NOT NULL;
ALTER TABLE descriptor_members ADD COLUMN format text;
ALTER TABLE descriptor_members ADD COLUMN quantization text;
ALTER TABLE descriptor_members ADD CONSTRAINT descriptor_members_model_key_check
    CHECK ((format IS NULL) = (quantization IS NULL));
ALTER TABLE descriptor_members ADD CONSTRAINT descriptor_members_scope_name_key_format_quantization_key
    UNIQUE (scope, name_key, format, quantization);
