-- Every member's policy state is lost; the memberships stay.
ALTER TABLE campaign_group_members DROP COLUMN policy_state;
