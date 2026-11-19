DROP TABLE campaign_group_job_states;
-- The older binary needs the member state's list of the jobs the pass is done
-- with; an empty one makes it read every job again.
UPDATE campaign_group_members
SET policy_state = jsonb_set(policy_state::jsonb, '{decided}', '[]')::text
WHERE CASE WHEN policy_state IS JSON OBJECT
    THEN NOT (policy_state::jsonb ? 'decided') ELSE false END;
