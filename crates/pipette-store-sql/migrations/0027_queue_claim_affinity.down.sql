DROP TABLE queue_client_last_grant;
ALTER TABLE queue_jobs DROP COLUMN runtime_key, DROP COLUMN model_key;
DROP FUNCTION queue_json_key(text, text[]);
