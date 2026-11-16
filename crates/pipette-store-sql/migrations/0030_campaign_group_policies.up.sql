-- Campaign group policies, kept as groups are: the record's JSON in `body`.
-- The key gives a group one policy per check kind.
CREATE TABLE campaign_group_policies (
    group_id  text NOT NULL REFERENCES campaign_groups (group_id) ON DELETE CASCADE,
    kind      text NOT NULL,
    body      text NOT NULL,
    PRIMARY KEY (group_id, kind)
);
