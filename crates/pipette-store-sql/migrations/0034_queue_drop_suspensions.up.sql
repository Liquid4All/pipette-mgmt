-- Client suspension is retired (PIP-836): no build from release
-- 2026.11.06-119339c on reads this table.
DROP TABLE queue_suspensions;
