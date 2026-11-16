-- What queue-maintenance's policy pass keeps for a member, as JSON; NULL until
-- the pass first writes it.
ALTER TABLE campaign_group_members ADD COLUMN policy_state text;
