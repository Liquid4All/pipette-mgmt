-- The older binary takes one max_reruns for every code of a failed policy and
-- refuses settings.codes, so the largest limit becomes max_reruns.
ALTER TABLE campaign_group_rerun_results DROP COLUMN codes;
UPDATE campaign_group_policies
SET body = (jsonb_set(body::jsonb, '{max_reruns}', (
    SELECT to_jsonb(max(n::integer)) FROM jsonb_each_text(body::jsonb #> '{settings,codes}') AS limits (code, n)
)) #- '{settings,codes}')::text
WHERE kind = 'failed'
    AND CASE WHEN body IS JSON OBJECT
        THEN jsonb_typeof(body::jsonb #> '{settings,codes}') = 'object' ELSE false END;
UPDATE campaign_group_members
SET policy_state = jsonb_set(policy_state::jsonb, '{policies}', (
    SELECT jsonb_agg(CASE WHEN policy ->> 'kind' = 'failed'
            AND jsonb_typeof(policy #> '{settings,codes}') = 'object'
        THEN jsonb_set(policy, '{max_reruns}', (
            SELECT to_jsonb(max(n::integer)) FROM jsonb_each_text(policy #> '{settings,codes}') AS limits (code, n)
        )) #- '{settings,codes}'
        ELSE policy END ORDER BY at)
    FROM jsonb_array_elements(policy_state::jsonb -> 'policies') WITH ORDINALITY AS listed (policy, at)
))::text
WHERE CASE WHEN policy_state IS JSON OBJECT
    THEN policy_state::jsonb @? 'strict $.policies[*] ? (@.kind == "failed")' ELSE false END;
