-- A plan's note and its revision change are columns, outside `body`. A reader
-- edits the note while queue-maintenance rewrites the body, and that rewrite
-- leaves both columns as they are. Each note starts as the body's `plan_name`,
-- which is left where it is. The note is trimmed, and a blank `plan_name` gives
-- no note, as the API stores a note.
ALTER TABLE plans ADD COLUMN note text, ADD COLUMN change jsonb;
UPDATE plans
SET note = NULLIF(regexp_replace(body::jsonb ->> 'plan_name', '^[[:space:]]+|[[:space:]]+$', '', 'g'), '')
-- A CASE, not AND: only it keeps the cast off a body that is not JSON.
WHERE CASE WHEN body IS JSON OBJECT THEN body::jsonb ? 'plan_name' ELSE false END;
