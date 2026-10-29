-- Dropping the column would widen every campaign reader into a member of its
-- organization, so the revert waits until none is left.
DO $$
BEGIN
    IF EXISTS (SELECT 1 FROM users WHERE campaign_scope IS NOT NULL) THEN
        RAISE EXCEPTION 'campaign readers exist: disable and retire them before reverting';
    END IF;
END $$;
ALTER TABLE users DROP COLUMN campaign_scope;
