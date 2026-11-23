-- The earlier grammar has no data URIs and allows at most 2,048 characters, so
-- the rollback drops every logo outside it.
DELETE FROM descriptor_family_logos WHERE logo_uri LIKE 'data:%' OR length(logo_uri) > 2048;

DROP INDEX descriptor_family_logos_logo_sha256;

ALTER TABLE descriptor_family_logos
    DROP COLUMN logo_sha256,
    DROP CONSTRAINT descriptor_family_logos_logo_uri_check,
    ADD CONSTRAINT descriptor_family_logos_logo_uri_check
        CHECK (length(logo_uri) <= 2048 AND logo_uri ~
            '^https://pipette\.liquid\.ai/logos/[A-Za-z0-9_-][A-Za-z0-9._-]*(/[A-Za-z0-9_-][A-Za-z0-9._-]*)*$');
