-- The logo of a model family in one scope. An empty scope is global. An org
-- entry with no logo of its own for its family shows the global logo.
-- The logo_uri check is the grammar of `LogoUri` in pipette-plan-types, so the
-- table holds no URI that a reader refuses.
CREATE TABLE descriptor_family_logos (
    scope text NOT NULL,
    family_key text NOT NULL,
    family text NOT NULL,
    logo_uri text NOT NULL CONSTRAINT descriptor_family_logos_logo_uri_check
        CHECK (length(logo_uri) <= 2048 AND logo_uri ~
            '^https://pipette\.liquid\.ai/logos/[A-Za-z0-9_-][A-Za-z0-9._-]*(/[A-Za-z0-9_-][A-Za-z0-9._-]*)*$'),
    PRIMARY KEY (scope, family_key),
    CONSTRAINT descriptor_family_logos_family_key_check
        CHECK (family_key = lower(family COLLATE "C"))
);
