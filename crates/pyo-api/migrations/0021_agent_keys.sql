-- Signed agent requests (agent_auth.rs): each agent's public keys (a day
-- each, the newest and the one it replaced), and the nonces of requests
-- seen in the last minutes, so none is accepted twice.
CREATE TABLE pyo.agent_keys (
  id          text PRIMARY KEY,
  cluster_id  text NOT NULL REFERENCES pyo.clusters (id) ON DELETE CASCADE,
  -- Ed25519, base64url.
  public_key  text NOT NULL,
  created_at  timestamptz NOT NULL DEFAULT now(),
  expires_at  timestamptz NOT NULL,
  revoked_at  timestamptz
);
CREATE INDEX agent_keys_cluster_idx ON pyo.agent_keys (cluster_id);

CREATE TABLE pyo.agent_nonces (
  nonce    text PRIMARY KEY,
  seen_at  timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX agent_nonces_seen_idx ON pyo.agent_nonces (seen_at);
