CREATE INDEX plans_open ON plans (plan_id) WHERE status NOT IN ('complete', 'cancelled');
