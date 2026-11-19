-- The member states an older binary kept have no `triggered`, which the pass
-- reads: an empty one reads as cleared until the pass next reads the member.
UPDATE campaign_group_members
SET policy_state = jsonb_set(policy_state::jsonb, '{triggered}', '[]')::text
WHERE CASE WHEN policy_state IS JSON OBJECT
    THEN NOT (policy_state::jsonb ? 'triggered') ELSE false END;
