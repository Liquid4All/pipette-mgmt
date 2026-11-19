-- What queue-maintenance's policy pass found of each job of a member's plan, as
-- JSON: the checks that flag it and whether the pass is done with it. The rows
-- go with the membership. Empty at first, so the pass reads every job once.
CREATE TABLE campaign_group_job_states (
    campaign_id  text NOT NULL REFERENCES campaign_group_members (campaign_id) ON DELETE CASCADE,
    job_id       text NOT NULL,
    body         text NOT NULL,
    PRIMARY KEY (campaign_id, job_id)
);
