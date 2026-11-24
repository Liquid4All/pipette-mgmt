CREATE INDEX plan_job_spans_members_by_job
    ON plan_job_spans (job_id, lineage, from_plan) INCLUDE (to_plan)
    WHERE ordinal IS NOT NULL;

CREATE INDEX plans_cancelled_by_lineage
    ON plans ((coalesce(campaign_id, plan_id)), plan_id)
    WHERE status = 'cancelled';
