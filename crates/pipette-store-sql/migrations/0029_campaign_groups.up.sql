-- Campaign groups, kept as campaigns are: the record's JSON in `body`.
CREATE TABLE campaign_groups (
    group_id  text PRIMARY KEY,
    body      text NOT NULL
);

-- The key puts a campaign in at most one group.
CREATE TABLE campaign_group_members (
    campaign_id  text PRIMARY KEY REFERENCES campaigns (campaign_id) ON DELETE CASCADE,
    group_id     text NOT NULL REFERENCES campaign_groups (group_id) ON DELETE CASCADE
);

CREATE INDEX campaign_group_members_group_id ON campaign_group_members (group_id);
