-- Bodies are text parsed in Rust, so a corrupt one is reported, not refused.
-- A claim or a marker may outlive, or precede, its record, so no foreign key
-- joins them. `plan_jobs` is the one exception: `put_plan` writes it with its
-- plan in one transaction.

-- `revision`, with the body, conditions the self-heal write-back, never
-- `put_client`.
CREATE TABLE clients (
    client_id  text PRIMARY KEY,
    body       text NOT NULL,
    revision   bigint NOT NULL
);

CREATE TABLE signature_migration (
    client_id  text PRIMARY KEY,
    body       text NOT NULL
);

CREATE TABLE client_permissions (
    client_id  text PRIMARY KEY,
    body       text NOT NULL,
    revision   bigint NOT NULL
);

CREATE TABLE client_preauth (
    key_id  text PRIMARY KEY,
    body    text NOT NULL
);

-- Its own table so a spend outlives the record it deleted.
CREATE TABLE client_preauth_spent (
    key_id    text PRIMARY KEY,
    spent_at  timestamptz NOT NULL
);

-- One claim per keypair, across both principal kinds.
CREATE TABLE public_key_index (
    digest  text PRIMARY KEY,
    holder  text NOT NULL
);

CREATE TABLE users (
    user_id  text PRIMARY KEY,
    body     text NOT NULL
);

CREATE TABLE user_email_index (
    email    text PRIMARY KEY,
    user_id  text NOT NULL
);

CREATE TABLE user_keys (
    user_id  text NOT NULL,
    key_id   text NOT NULL,
    body     text NOT NULL,
    PRIMARY KEY (user_id, key_id)
);

CREATE TABLE user_permissions (
    user_id   text PRIMARY KEY,
    revision  bigint NOT NULL,
    body      text NOT NULL
);

CREATE TABLE user_preauth (
    key_id  text PRIMARY KEY,
    body    text NOT NULL
);

-- Its own table: the marker outlives the record, and only the prune removes it.
CREATE TABLE user_preauth_spent (
    key_id    text PRIMARY KEY,
    spent_at  timestamptz NOT NULL
);

CREATE TABLE user_awaiting_sign_in (
    user_id    text PRIMARY KEY,
    marked_at  timestamptz NOT NULL
);

CREATE TABLE orgs (
    org_id  text PRIMARY KEY,
    body    text NOT NULL
);

-- The slug claim holds the bare org id, as the object stores do.
CREATE TABLE org_slug_index (
    slug    text PRIMARY KEY,
    org_id  text NOT NULL
);

-- The organization each account belongs to, if any.
CREATE TABLE user_orgs (
    user_id  text PRIMARY KEY,
    org_id   text NOT NULL
);

-- The owner of each owned device: an organization or a user account.
CREATE TABLE client_owners (
    client_id   text PRIMARY KEY,
    owner_type  text NOT NULL CHECK (owner_type IN ('org', 'user')),
    owner_id    text NOT NULL
);

-- The UNIQUE constraint is the tag-to-clients index.
CREATE TABLE client_tags (
    client_id  text NOT NULL,
    tag        text NOT NULL,
    PRIMARY KEY (client_id, tag),
    UNIQUE (tag, client_id)
);

-- One benchmark id may sit under several orgs: the writer above the store
-- polices that, not this key.
CREATE TABLE org_benchmarks (
    org_id        text NOT NULL,
    benchmark_id  text NOT NULL,
    body          text NOT NULL,
    PRIMARY KEY (org_id, benchmark_id)
);

-- The fields that a listing filters on are columns. `body` is the rest of the
-- manifest as JSON, without these columns and without the per-job fields that
-- `plan_jobs` holds.
CREATE TABLE plans (
    plan_id       text PRIMARY KEY,
    status        text NOT NULL,
    campaign_id   text,
    submitted_by  text,
    body          text NOT NULL
);

CREATE INDEX plans_status ON plans (status);
CREATE INDEX plans_campaign_id ON plans (campaign_id);
CREATE INDEX plans_submitted_by ON plans (submitted_by);

-- One row for each job that a manifest names. `ordinal` is the position of the
-- job in the manifest's `job_ids`. It is NULL for a job that only one of the
-- per-job maps (`content_keys`, `rerun_of`, `expired_jobs`) names.
CREATE TABLE plan_jobs (
    plan_id      text NOT NULL REFERENCES plans (plan_id) ON DELETE CASCADE,
    job_id       text NOT NULL,
    ordinal      integer,
    content_key  text,
    rerun_of     text,
    expired      jsonb,
    PRIMARY KEY (plan_id, job_id),
    UNIQUE (plan_id, ordinal)
);

CREATE TABLE cancelled_plans (
    plan_id  text PRIMARY KEY
);

CREATE TABLE content_keys (
    digest    text PRIMARY KEY,
    record    text NOT NULL,
    revision  bigint NOT NULL
);

CREATE TABLE job_plans (
    job_id      text NOT NULL,
    plan_id     text NOT NULL,
    expires_at  text NOT NULL,
    written_at  timestamptz NOT NULL,
    PRIMARY KEY (job_id, plan_id, expires_at)
);

CREATE TABLE plan_sources (
    plan_id  text PRIMARY KEY,
    source   text NOT NULL
);

CREATE TABLE plan_sources_pending (
    plan_id     text PRIMARY KEY,
    written_at  timestamptz NOT NULL
);

CREATE TABLE job_sources (
    job_id  text PRIMARY KEY,
    body    text NOT NULL
);

CREATE TABLE job_plan_links (
    job_id   text NOT NULL,
    plan_id  text NOT NULL,
    PRIMARY KEY (job_id, plan_id)
);

CREATE TABLE campaigns (
    campaign_id  text PRIMARY KEY,
    body         text NOT NULL
);

CREATE TABLE vault (
    resource_id  text PRIMARY KEY,
    body         text NOT NULL
);

CREATE TABLE vault_imports (
    owner_id       text NOT NULL,
    import_sha256  text NOT NULL,
    body           text NOT NULL,
    PRIMARY KEY (owner_id, import_sha256)
);
