-- A campaign's labels are a column, outside `body`, so a rolled-back binary
-- that rewrites only the body keeps them, and the database can select on them.
ALTER TABLE campaigns ADD COLUMN labels jsonb NOT NULL DEFAULT '{}';
