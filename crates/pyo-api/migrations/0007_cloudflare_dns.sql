-- A team's Cloudflare account (cloudflare.rs): an API token with Zone:Read
-- and DNS:Edit, sealed like other integration keys, and the zones it
-- covered at its last test ([{id, name, canEdit, missing}]).
CREATE TABLE pyo.cloudflare_accounts (
  team_id      text PRIMARY KEY REFERENCES pyo.teams (id) ON DELETE CASCADE,
  token_sealed text NOT NULL,
  hint         text NOT NULL DEFAULT '',
  zones        jsonb NOT NULL DEFAULT '[]',
  created_by   text NOT NULL DEFAULT '',
  created_at   timestamptz NOT NULL DEFAULT now(),
  tested_at    timestamptz NOT NULL DEFAULT now()
);

-- The DNS record of a service's domain in a connected Cloudflare zone.
-- `record_id` is set only for a record Pyo made: its comment names this
-- installation, and only such a record is ever deleted.
CREATE TABLE pyo.dns_records (
  service_id text NOT NULL REFERENCES pyo.services (id) ON DELETE CASCADE,
  domain_id  text NOT NULL,
  team_id    text NOT NULL REFERENCES pyo.teams (id) ON DELETE CASCADE,
  hostname   text NOT NULL,
  zone_id    text NOT NULL,
  zone_name  text NOT NULL,
  record_id  text,
  type       text NOT NULL DEFAULT 'A',
  content    text NOT NULL DEFAULT '',
  -- As it is at Cloudflare.
  proxied    boolean NOT NULL DEFAULT false,
  -- Proxied by Cloudflare was asked for: turned on once the certificate
  -- is issued, so the first HTTP-01 check reaches the cluster directly.
  proxy      boolean NOT NULL DEFAULT false,
  -- 'created' (Pyo made it), 'exists' (it already pointed there),
  -- 'conflict' (another record has the name: `existing`), 'failed' (`error`).
  status     text NOT NULL CHECK (status IN ('created', 'exists', 'conflict', 'failed')),
  error      text,
  existing   jsonb NOT NULL DEFAULT '[]',
  updated_at timestamptz NOT NULL DEFAULT now(),
  PRIMARY KEY (service_id, domain_id)
);
CREATE INDEX dns_records_proxy_idx ON pyo.dns_records (service_id) WHERE proxy AND NOT proxied;
