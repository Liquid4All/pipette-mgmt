-- Failed's codes at a re-run's filing, a JSON list, so the re-run is judged by
-- them: NULL for the other kinds. Failed re-ran runtime_error, device_error and
-- unspecified until now, so its re-runs take those.
ALTER TABLE campaign_group_rerun_results ADD COLUMN codes text;
UPDATE campaign_group_rerun_results
SET codes = '["runtime_error", "device_error", "unspecified"]'
WHERE kind = 'failed';
-- A failed policy's limits move into settings.codes, a map from each code it
-- re-runs (those three when it names none) to its max_reruns, which becomes 0.
UPDATE campaign_group_policies
SET body = jsonb_set(jsonb_set(body::jsonb, '{max_reruns}', '0'), '{settings,codes}', (
    SELECT jsonb_object_agg(code, body::jsonb -> 'max_reruns')
    FROM jsonb_array_elements_text(COALESCE(body::jsonb #> '{settings,codes}',
        '["runtime_error", "device_error", "unspecified"]')) AS code
))::text
WHERE kind = 'failed'
    -- A CASE, not AND: only it keeps the cast off a body that is not JSON.
    AND CASE WHEN body IS JSON OBJECT THEN jsonb_typeof(body::jsonb -> 'settings') = 'object'
        AND jsonb_typeof(body::jsonb -> 'max_reruns') = 'number' ELSE false END;
-- The pass compares member states' copies of the policies with the stored ones.
UPDATE campaign_group_members
SET policy_state = jsonb_set(policy_state::jsonb, '{policies}', (
    SELECT jsonb_agg(CASE WHEN policy ->> 'kind' = 'failed' AND jsonb_typeof(policy -> 'settings') = 'object'
            AND jsonb_typeof(policy -> 'max_reruns') = 'number'
        THEN jsonb_set(jsonb_set(policy, '{max_reruns}', '0'), '{settings,codes}', (
            SELECT jsonb_object_agg(code, policy -> 'max_reruns')
            FROM jsonb_array_elements_text(COALESCE(policy #> '{settings,codes}',
                '["runtime_error", "device_error", "unspecified"]')) AS code
        ))
        ELSE policy END ORDER BY at)
    FROM jsonb_array_elements(policy_state::jsonb -> 'policies') WITH ORDINALITY AS listed (policy, at)
))::text
-- Strict: lax mode would match a policies object, which jsonb_array_elements refuses.
WHERE CASE WHEN policy_state IS JSON OBJECT
    THEN policy_state::jsonb @? 'strict $.policies[*] ? (@.kind == "failed")' ELSE false END;
