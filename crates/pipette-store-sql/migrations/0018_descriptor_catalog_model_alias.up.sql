-- A vault model member can also name result rows recorded under its verified
-- Hugging Face alias. The member's own digest still names planned cells.
ALTER TABLE descriptor_members ADD COLUMN alias_digest text;
CREATE INDEX descriptor_members_alias_digest_idx ON descriptor_members (alias_digest)
    WHERE alias_digest IS NOT NULL;
