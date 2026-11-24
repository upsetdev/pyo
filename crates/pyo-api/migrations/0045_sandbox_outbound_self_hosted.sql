-- A self-hosted Pyo's sandbox servers work over the outbound channel too
-- (sandbox/outbound, docs/cloud-sandbox-servers.md "Self-hosted").

-- The key a self-hosted Pyo signs sandbox servers' commands with
-- (sandbox/outbound/signer.rs). Its own, never the token key in
-- sandbox_keys. Made on first start. Its PKCS#8 is sealed with
-- PYO_SECRET_KEY like Pyo's other credentials (secrets.rs). No team may
-- read it: row security with no policy hides it from pyo_team.
CREATE TABLE pyo.sandbox_command_keys (
  kid           text PRIMARY KEY,
  pkcs8_sealed  text NOT NULL,
  -- 32 bytes, base64url: what the join command's --command-key carries.
  public_key    text NOT NULL,
  created_at    timestamptz NOT NULL DEFAULT now(),
  retired_at    timestamptz
);
ALTER TABLE pyo.sandbox_command_keys ENABLE ROW LEVEL SECURITY;

-- The last command number the local signer gave the server. It only grows.
ALTER TABLE pyo.sandbox_servers ADD COLUMN command_seq bigint NOT NULL DEFAULT 0;
-- Where an outbound server reaches Pyo, as its join command said.
ALTER TABLE pyo.sandbox_servers ADD COLUMN pyo_url text;

-- The installation's servers (no team) have keys and commands too. Row
-- security shows those rows to no team.
ALTER TABLE pyo.sandbox_server_keys ALTER COLUMN team_id DROP NOT NULL;
ALTER TABLE pyo.sandbox_commands ALTER COLUMN team_id DROP NOT NULL;
