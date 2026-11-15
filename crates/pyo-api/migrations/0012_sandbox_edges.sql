-- Sandbox servers' edge proxies (sandbox/edgecert.rs): the domain their
-- sandbox ports are served on, and the wildcard certificate Pyo gets for
-- it from Let's Encrypt through DNS-01 with the team's Cloudflare
-- connection, then puts on the server.

-- `<port>-<sandbox>.<edge_domain>`. Given when the server is added, and
-- kept in step with what the server reports.
ALTER TABLE pyo.sandbox_servers ADD COLUMN edge_domain text;

CREATE TABLE pyo.sandbox_edges (
  server_id    text PRIMARY KEY REFERENCES pyo.sandbox_servers (id) ON DELETE CASCADE,
  -- The domain the certificate is for: `*.<domain>`.
  domain       text NOT NULL,
  -- The team whose Cloudflare connection answers the DNS-01 challenge and
  -- makes the wildcard record. The server's own team, or for the
  -- installation's servers the team its admin picked.
  dns_team_id  text REFERENCES pyo.teams (id) ON DELETE SET NULL,
  -- none: no certificate asked for yet. issuing: Pyo is getting one (a
  -- renewal keeps serving the old one meanwhile). ready: issued. failed:
  -- error says why, and a certificate from before stays served.
  state        text NOT NULL DEFAULT 'none' CHECK (state IN ('none', 'issuing', 'ready', 'failed')),
  cert_pem     text,
  -- Sealed with PYO_SECRET_KEY like Pyo's other credentials (secrets.rs).
  key_sealed   text,
  -- pyo_proto::sandbox::cert_fingerprint of the leaf. The server reports
  -- the one it serves, and gets this one again when they differ.
  fingerprint  text,
  not_before   timestamptz,
  not_after    timestamptz,
  issued_at    timestamptz,
  error        text,
  -- When the runner may try a failed renewal again.
  retry_at     timestamptz,
  -- The wildcard record Pyo made at Cloudflare, when asked:
  -- {"zoneId", "status", "records": [{"id", "type", "content"}], "existing", "error"}.
  dns          jsonb,
  updated_at   timestamptz NOT NULL DEFAULT now()
);
