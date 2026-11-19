-- Annotated tags on a campaign's plans: the tag's JSON in `body`, its name
-- unique within the campaign. A tag is never updated.
CREATE TABLE plan_tags (
    campaign_id  text NOT NULL REFERENCES campaigns (campaign_id) ON DELETE CASCADE,
    name         text NOT NULL,
    plan_id      text NOT NULL,
    body         text NOT NULL,
    PRIMARY KEY (campaign_id, name)
);
