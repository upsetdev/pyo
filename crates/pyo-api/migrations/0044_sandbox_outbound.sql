-- Sandbox servers that work over HTTPS (sandbox/outbound, Pyo Cloud,
-- docs/cloud-sandbox-servers.md): they join with a one-time code, hold a
-- key of their own and long-poll Pyo for their commands. Pyo never calls
-- them.

-- outbound: it joins and works over HTTPS. Pyo reaches it only through
-- its commands. tailnet_name: its full name on the team's tailnet.
ALTER TABLE pyo.sandbox_servers ADD COLUMN outbound boolean NOT NULL DEFAULT false;
ALTER TABLE pyo.sandbox_servers ADD COLUMN tailnet_name text;

-- An outbound server's keys: Ed25519, made on the server. Pyo keeps only
-- the public half. Good for a day, renewed at half that. Revoked by Reset
-- access, and gone with the server.
CREATE TABLE pyo.sandbox_server_keys (
  id          text PRIMARY KEY,
  server_id   text NOT NULL REFERENCES pyo.sandbox_servers (id) ON DELETE CASCADE,
  team_id     text NOT NULL REFERENCES pyo.teams (id) ON DELETE CASCADE,
  -- 32 bytes, base64url.
  public_key  text NOT NULL,
  expires_at  timestamptz NOT NULL,
  revoked_at  timestamptz,
  created_at  timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX sandbox_server_keys_server ON pyo.sandbox_server_keys (server_id);

-- An outbound server's commands, in the order they were queued. The
-- command is sealed (it may carry env values or a certificate's key) and
-- dropped once answered. The signer signs it when the server takes it.
CREATE TABLE pyo.sandbox_commands (
  id               text PRIMARY KEY,
  server_id        text NOT NULL REFERENCES pyo.sandbox_servers (id) ON DELETE CASCADE,
  team_id          text NOT NULL REFERENCES pyo.teams (id) ON DELETE CASCADE,
  -- The DaemonCommand's type.
  kind             text NOT NULL,
  command_sealed   text,
  status           text NOT NULL DEFAULT 'pending' CHECK (status IN ('pending', 'sent', 'done', 'failed')),
  -- The DaemonResult.
  result           jsonb,
  -- The daemon process it went to (its instance header).
  sent_to          text,
  created_at       timestamptz NOT NULL DEFAULT now(),
  sent_at          timestamptz,
  done_at          timestamptz
);
CREATE INDEX sandbox_commands_queue ON pyo.sandbox_commands (server_id, created_at) WHERE status IN ('pending', 'sent');
CREATE INDEX sandbox_commands_done ON pyo.sandbox_commands (done_at) WHERE done_at IS NOT NULL;

-- Row security like the other team tables (0022, 0032).
DO $$
DECLARE
  t text;
BEGIN
  FOREACH t IN ARRAY ARRAY['sandbox_server_keys', 'sandbox_commands'] LOOP
    EXECUTE format('ALTER TABLE pyo.%I ENABLE ROW LEVEL SECURITY', t);
    EXECUTE format(
      'CREATE POLICY team_rows ON pyo.%I TO pyo_team
         USING (team_id = ANY (pyo.team_scope()))
         WITH CHECK (team_id = ANY (pyo.team_scope()))', t);
  END LOOP;
END
$$;
