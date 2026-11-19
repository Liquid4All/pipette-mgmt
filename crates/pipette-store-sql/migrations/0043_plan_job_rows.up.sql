-- Compatibility rows for readers outside mgmt. Membership is the half-open
-- range of plan ids in a campaign, or the plan's own lineage without a campaign.
-- Each plan expands its lineage with the two branches of the manifest read, so
-- each branch is bounded by `plan_job_spans_open`.
CREATE VIEW plan_job_rows AS
SELECT p.plan_id, s.job_id, s.ordinal, s.content_key, s.rerun_of, s.expired, s.outcome
FROM plans p
CROSS JOIN LATERAL (
    SELECT job_id, ordinal, content_key, rerun_of, expired, outcome
    FROM plan_job_spans
    WHERE lineage = coalesce(p.campaign_id, p.plan_id)
      AND to_plan IS NULL AND from_plan <= p.plan_id
    UNION ALL
    SELECT job_id, ordinal, content_key, rerun_of, expired, outcome
    FROM plan_job_spans
    WHERE lineage = coalesce(p.campaign_id, p.plan_id)
      AND to_plan > p.plan_id AND from_plan <= p.plan_id
) s;
