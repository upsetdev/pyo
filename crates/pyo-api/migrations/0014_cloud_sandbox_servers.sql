-- pyo:contract since=v0.4.0 the role check only widens and cluster_id only stops being required, so every row a release writes still fits
-- Sandbox servers Pyo makes at a cloud provider (cloud/mod.rs): the cloud
-- server points at the sandbox server it becomes, and so does its
-- bootstrap code, which then opens sandbox.sh's details instead of a
-- cluster's. Kept as plain text, so a removed sandbox server's cloud server
-- still knows which label to check before Pyo deletes it at the provider.
ALTER TABLE pyo.cloud_servers ADD COLUMN sandbox_server_id text;
CREATE INDEX cloud_servers_sandbox ON pyo.cloud_servers (sandbox_server_id) WHERE sandbox_server_id IS NOT NULL;

ALTER TABLE pyo.cloud_bootstrap ADD COLUMN sandbox_server_id text;
ALTER TABLE pyo.cloud_bootstrap ALTER COLUMN cluster_id DROP NOT NULL;
ALTER TABLE pyo.cloud_bootstrap DROP CONSTRAINT cloud_bootstrap_role_check;
ALTER TABLE pyo.cloud_bootstrap ADD CONSTRAINT cloud_bootstrap_role_check CHECK (role IN ('join', 'connect', 'sandbox'));
