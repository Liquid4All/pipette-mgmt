-- A runtime-only schema cannot keep model entries. Their members go with them.
DELETE FROM descriptor_entries WHERE descriptor_kind = 'model';

ALTER TABLE descriptor_members DROP CONSTRAINT descriptor_members_scope_name_key_format_quantization_key;
ALTER TABLE descriptor_members DROP CONSTRAINT descriptor_members_model_key_check;
ALTER TABLE descriptor_members DROP COLUMN quantization;
ALTER TABLE descriptor_members DROP COLUMN format;
ALTER TABLE descriptor_members DROP COLUMN descriptor_type;

ALTER TABLE descriptor_entries DROP CONSTRAINT descriptor_entries_model_kind_check;
ALTER TABLE descriptor_entries DROP COLUMN descriptor_kind;
ALTER TABLE descriptor_entries RENAME COLUMN descriptor_type TO runtime_type;
