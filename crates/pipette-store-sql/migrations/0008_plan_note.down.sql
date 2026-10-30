-- A body the up script read still holds its `plan_name` until a rewrite; an
-- edited note goes with the column.
ALTER TABLE plans DROP COLUMN note, DROP COLUMN change;
