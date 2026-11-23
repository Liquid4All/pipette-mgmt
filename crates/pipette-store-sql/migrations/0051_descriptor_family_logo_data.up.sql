-- A family logo is a URI under https://pipette.liquid.ai/logos/ or a base64
-- data URI of a PNG, SVG or WebP image. The logo_uri check is the grammar of
-- `LogoUri` in pipette-plan-types, so the table holds no URI that a reader
-- refuses. The grammar admits only ASCII and excludes the escape characters
-- of the bytea input format, so this cast hashes the same bytes as LogoUri.
ALTER TABLE descriptor_family_logos
    DROP CONSTRAINT descriptor_family_logos_logo_uri_check,
    ADD CONSTRAINT descriptor_family_logos_logo_uri_check
        CHECK (length(logo_uri) <= 65536 AND logo_uri ~
            '^(https://pipette\.liquid\.ai/logos/[A-Za-z0-9_-][A-Za-z0-9._-]*(/[A-Za-z0-9_-][A-Za-z0-9._-]*)*|data:image/(png|svg\+xml|webp);base64,([A-Za-z0-9+/]{4})*([A-Za-z0-9+/]{2}==|[A-Za-z0-9+/]{3}=|[A-Za-z0-9+/]{4}))$'),
    ADD COLUMN logo_sha256 text
        GENERATED ALWAYS AS (encode(sha256(logo_uri::bytea), 'hex')) STORED NOT NULL;

CREATE INDEX descriptor_family_logos_logo_sha256 ON descriptor_family_logos (logo_sha256);
