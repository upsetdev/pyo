-- Agents with a client certificate (agent_tls.rs): the serial of the
-- cluster's current certificate and the one before it (through a renewal),
-- and the connect token already used to enroll (a token enrolls once).
ALTER TABLE pyo.clusters ADD COLUMN agent_cert_serial text;
ALTER TABLE pyo.clusters ADD COLUMN agent_cert_prev_serial text;
ALTER TABLE pyo.clusters ADD COLUMN agent_token_used_hash text;
