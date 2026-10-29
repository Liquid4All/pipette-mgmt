-- A campaign reader's scope is a column, outside `body`, so a rolled-back
-- binary that rewrites only the body keeps it. `NULL` is every account that
-- is no campaign reader, which every account before this one was.
ALTER TABLE users ADD COLUMN campaign_scope jsonb;
