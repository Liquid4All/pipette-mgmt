-- An account's type is a column, outside `body`, so a rolled-back binary that
-- rewrites only the body keeps it. Every account before this one was a person's.
ALTER TABLE users
    ADD COLUMN account_type text NOT NULL DEFAULT 'human'
    CHECK (account_type IN ('human', 'service'));
