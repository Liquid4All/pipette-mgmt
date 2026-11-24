-- The bytes that a client downloads for a Hugging Face model member, read at
-- its pinned commit. NULL for a runtime member, a vault model member, and a
-- model member that no write or backfill has sized.
ALTER TABLE descriptor_members ADD COLUMN size_bytes bigint
    CHECK (size_bytes IS NULL OR size_bytes >= 0);
