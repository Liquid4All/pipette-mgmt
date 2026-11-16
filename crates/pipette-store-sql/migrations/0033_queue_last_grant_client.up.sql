-- Keep deletion and grant writes out while removing old orphan rows.
LOCK TABLE clients, queue_client_last_grant IN SHARE ROW EXCLUSIVE MODE;

DELETE FROM queue_client_last_grant g
WHERE NOT EXISTS (SELECT 1 FROM clients c WHERE c.client_id = g.client_id);

ALTER TABLE queue_client_last_grant
  ADD CONSTRAINT queue_client_last_grant_client_id_fkey
  FOREIGN KEY (client_id) REFERENCES clients (client_id) ON DELETE CASCADE;
