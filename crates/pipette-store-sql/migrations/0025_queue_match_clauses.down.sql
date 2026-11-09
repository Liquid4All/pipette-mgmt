ALTER TABLE queue_jobs DROP COLUMN match_clauses;
DROP FUNCTION queue_match_clauses(text);
