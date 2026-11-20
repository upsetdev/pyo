-- Connect commands carry a connect token of their own (install.rs), not
-- the agent's credential: it installs the agent once, within an hour, for
-- the team and cluster it was made for. Fetching the agent's manifest with
-- it makes the agent's own token there, which no command shows. It's used
-- once that agent reports in.
ALTER TABLE pyo.clusters ADD COLUMN connect_token_hash text;
ALTER TABLE pyo.clusters ADD COLUMN connect_token_team text;
ALTER TABLE pyo.clusters ADD COLUMN connect_token_expires_at timestamptz;
ALTER TABLE pyo.clusters ADD COLUMN connect_token_used_at timestamptz;
CREATE INDEX clusters_connect_token_idx ON pyo.clusters (connect_token_hash) WHERE connect_token_hash IS NOT NULL;
