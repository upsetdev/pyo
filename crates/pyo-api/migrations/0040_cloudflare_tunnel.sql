-- Cloudflare Tunnel (cloudflare/tunnel.rs, docs/cloudflare-tunnel.md).

-- The cluster's traffic: 'public' (ports 80 and 443 on its servers),
-- 'tailscale' (only the tailnet, or inside the cluster without one) or
-- 'tunnel' (websites through Cloudflare Tunnel, no ports 80 and 443).
ALTER TABLE pyo.clusters ADD COLUMN traffic text NOT NULL DEFAULT 'public'
  CHECK (traffic IN ('public', 'tailscale', 'tunnel'));
UPDATE pyo.clusters SET traffic = CASE WHEN public_traffic THEN 'public' ELSE 'tailscale' END;
-- Every reader of public_traffic keeps working: it follows the choice.
ALTER TABLE pyo.clusters DROP COLUMN public_traffic;
ALTER TABLE pyo.clusters ADD COLUMN public_traffic boolean
  GENERATED ALWAYS AS (traffic = 'public') STORED;

-- A cluster's Cloudflare Tunnel. Nothing secret: the tunnel token is read
-- from Cloudflare when the agent needs it, and the Origin CA key sits
-- sealed with the team's certificates.
CREATE TABLE pyo.cluster_tunnels (
  cluster_id      text PRIMARY KEY REFERENCES pyo.clusters (id) ON DELETE CASCADE,
  team_id         text NOT NULL REFERENCES pyo.teams (id) ON DELETE CASCADE,
  account_id      text NOT NULL,
  account_name    text NOT NULL DEFAULT '',
  -- Cloudflare's id, once made, and its name there.
  tunnel_id       text,
  tunnel_name     text NOT NULL,
  state           text NOT NULL DEFAULT 'creating'
                  CHECK (state IN ('creating', 'ready', 'failed', 'removing')),
  error           text,
  -- Its zone apex: '*.<default_domain>' gets a wildcard record and route.
  default_domain  text,
  default_zone_id text,
  -- Pyo's wildcard record, when it made it.
  default_record_id text,
  -- Public TCP and UDP ports while the cluster is in tunnel mode.
  ports_public    boolean NOT NULL DEFAULT true,
  -- The routes Pyo last PUT (a hash), and Cloudflare's version of them.
  config_hash     text NOT NULL DEFAULT '',
  config_version  integer,
  -- The Origin CA certificate the tunnel's domains use.
  certificate_id  text REFERENCES pyo.certificates (id) ON DELETE SET NULL,
  -- Cloudflare's id for it, and the one it replaced, revoked once
  -- cf_certificate_old_at has passed.
  cf_certificate_id  text,
  cf_certificate_old text,
  cf_certificate_old_at timestamptz,
  -- Rotate was asked for: a new tunnel secret and a new certificate.
  rotate          boolean NOT NULL DEFAULT false,
  -- Restore the records the tunnel's domains replaced while it's removed.
  restore_records boolean NOT NULL DEFAULT true,
  -- Last reads: Cloudflare's view and the agent's.
  cf_status       jsonb NOT NULL DEFAULT '{}',
  agent_status    jsonb NOT NULL DEFAULT '{}',
  -- Since when Cloudflare says it's down (for the alert after 5 minutes),
  -- and whether that alert went out.
  down_since      timestamptz,
  down_alerted    boolean NOT NULL DEFAULT false,
  -- When the token last went to the agent (not more than every 5 minutes).
  token_sent_at   timestamptz,
  -- When Cloudflare's view was last read.
  checked_at      timestamptz,
  -- Cloudflare asked to wait (429) until then.
  backoff_until   timestamptz,
  created_by      text NOT NULL DEFAULT '',
  created_at      timestamptz NOT NULL DEFAULT now(),
  updated_at      timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX cluster_tunnels_team_idx ON pyo.cluster_tunnels (team_id);
ALTER TABLE pyo.cluster_tunnels ENABLE ROW LEVEL SECURITY;
CREATE POLICY team_rows ON pyo.cluster_tunnels TO pyo_team
  USING (team_id = ANY (pyo.team_scope()))
  WITH CHECK (team_id = ANY (pyo.team_scope()));

-- A domain through the tunnel: its record is a proxied CNAME to the
-- tunnel. `previous` keeps the records it replaced, for Switch back.
-- 'switching' and 'reverting' are the tunnel loop's to finish.
ALTER TABLE pyo.dns_records
  ADD COLUMN via text NOT NULL DEFAULT 'direct' CHECK (via IN ('direct', 'tunnel')),
  ADD COLUMN previous jsonb NOT NULL DEFAULT '[]',
  -- The switch may replace records in the way (an owner or admin said so).
  ADD COLUMN replace boolean NOT NULL DEFAULT false,
  -- Switching back: put the kept records back (else an A record to the
  -- cluster's public IP on a cluster with public ports, else nothing).
  ADD COLUMN restore boolean NOT NULL DEFAULT true,
  -- The routes sync the switch waits for on the agent (pyo.commands).
  ADD COLUMN routes_command text;
ALTER TABLE pyo.dns_records DROP CONSTRAINT dns_records_status_check;
ALTER TABLE pyo.dns_records ADD CONSTRAINT dns_records_status_check
  CHECK (status IN ('created', 'exists', 'conflict', 'failed', 'switching', 'reverting'));
CREATE INDEX dns_records_tunnel_idx ON pyo.dns_records (service_id)
  WHERE status IN ('switching', 'reverting');
