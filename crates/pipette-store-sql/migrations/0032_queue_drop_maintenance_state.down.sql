-- The table holds only derived counts; the next queue-maintenance run of the
-- earlier server fills it.
CREATE TABLE queue_maintenance_state (name text PRIMARY KEY, body text NOT NULL);
