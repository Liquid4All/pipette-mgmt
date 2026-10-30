-- The audit log (`pipette_core::audit`). The body is the event; the columns
-- are what a listing filters on, copied onto the tables that drive one.
CREATE TABLE audit_events (
    event_id    text PRIMARY KEY,
    at          timestamptz NOT NULL,
    outcome     text NOT NULL,
    actor_type  text NOT NULL,
    body        text NOT NULL
);

-- For the retention prune.
CREATE INDEX audit_events_at ON audit_events (at);
-- A listing walks one of these per outcome, or per actor kind, asked for.
CREATE INDEX audit_events_outcome ON audit_events (outcome, event_id);
CREATE INDEX audit_events_actor ON audit_events (actor_type, event_id);

-- The organizations whose log lists each event, keyed for paging one log.
CREATE TABLE audit_event_orgs (
    org_id      text NOT NULL,
    event_id    text NOT NULL REFERENCES audit_events (event_id) ON DELETE CASCADE,
    outcome     text NOT NULL,
    actor_type  text NOT NULL,
    PRIMARY KEY (org_id, event_id)
);

CREATE INDEX audit_event_orgs_event_id ON audit_event_orgs (event_id);
CREATE INDEX audit_event_orgs_outcome ON audit_event_orgs (org_id, outcome, event_id);
CREATE INDEX audit_event_orgs_actor ON audit_event_orgs (org_id, actor_type, event_id);

-- Every record an event names: its targets, and the account or device that
-- acted, keyed for paging one record's history.
CREATE TABLE audit_event_records (
    record_type  text NOT NULL,
    record_id    text NOT NULL,
    event_id     text NOT NULL REFERENCES audit_events (event_id) ON DELETE CASCADE,
    PRIMARY KEY (record_type, record_id, event_id)
);

CREATE INDEX audit_event_records_event_id ON audit_event_records (event_id);
