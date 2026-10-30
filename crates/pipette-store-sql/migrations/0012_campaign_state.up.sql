-- A campaign's state is a column, outside `body`, so a rolled-back binary that
-- rewrites only the body keeps an archived campaign archived.
ALTER TABLE campaigns ADD COLUMN state text NOT NULL DEFAULT 'active';
