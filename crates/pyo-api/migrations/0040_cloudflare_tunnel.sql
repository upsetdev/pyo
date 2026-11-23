-- pyo:contract since=v0.5.5 dns_records_status_check only widens (adds switching and reverting), so every status v0.5.5 writes (created, exists, conflict, failed) still fits
-- Cloudflare Tunnel (cloudflare/tunnel.rs, docs/cloudflare-tunnel.md).

-- The cluster's traffic: 'public' (ports 80 and 443 on its servers),
-- 'tailscale' (only the tailnet, or inside the cluster without one) or
-- 'tunnel' (websites through Cloudflare Tunnel, no ports 80 and 443).
ALTER TABLE pyo.clusters ADD COLUMN traffic text NOT NULL DEFAULT 'public'
  CHECK (traffic IN ('public', 'tailscale', 'tunnel'));
UPDATE pyo.clusters SET traffic = CASE WHEN public_traffic THEN 'public' ELSE 'tailscale' END;
-- public_traffic stays a plain column while v0.5.5 may still run: it reads
-- it and writes it (setClusterPublicTraffic, install::web_ports_busy). This
-- trigger keeps the two in step, so every reader of public_traffic follows
-- traffic, and v0.5.5 writing public_traffic moves traffic with it. A later
-- release makes it a generated column (RELEASING.md, Pending contracts).
CREATE FUNCTION pyo.clusters_traffic_sync() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
  IF TG_OP = 'INSERT' THEN
    IF NEW.traffic = 'public' AND NOT NEW.public_traffic THEN
      NEW.traffic := 'tailscale';
    END IF;
  ELSIF NEW.traffic IS DISTINCT FROM OLD.traffic THEN
    NULL;
  ELSIF NEW.public_traffic IS DISTINCT FROM OLD.public_traffic THEN
    NEW.traffic := CASE WHEN NEW.public_traffic THEN 'public' ELSE 'tailscale' END;
  END IF;
  NEW.public_traffic := NEW.traffic = 'public';
  RETURN NEW;
END
$$;
CREATE TRIGGER clusters_traffic_sync BEFORE INSERT OR UPDATE OF traffic, public_traffic
  ON pyo.clusters FOR EACH ROW EXECUTE FUNCTION pyo.clusters_traffic_sync();

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

-- What's left at Cloudflare once its row is gone: a tunnel record of a
-- deleted service, or the tunnel of a deleted cluster. The tunnel loop
-- deletes them (only Pyo's own records) with the team's token.
CREATE TABLE pyo.tunnel_leftovers (
  id              bigserial PRIMARY KEY,
  team_id         text NOT NULL REFERENCES pyo.teams (id) ON DELETE CASCADE,
  -- 'record' (zone_id, record_id, hostname) or 'tunnel' (account_id,
  -- tunnel_id, cf_certificates, and its wildcard record as a record).
  kind            text NOT NULL CHECK (kind IN ('record', 'tunnel')),
  zone_id         text,
  record_id       text,
  hostname        text,
  account_id      text,
  tunnel_id       text,
  cf_certificates text[] NOT NULL DEFAULT '{}',
  error           text,
  created_at      timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX tunnel_leftovers_team_idx ON pyo.tunnel_leftovers (team_id);
ALTER TABLE pyo.tunnel_leftovers ENABLE ROW LEVEL SECURITY;
CREATE POLICY team_rows ON pyo.tunnel_leftovers TO pyo_team
  USING (team_id = ANY (pyo.team_scope()))
  WITH CHECK (team_id = ANY (pyo.team_scope()));

-- Kept when its team is still there (a team deleted takes its token too).
CREATE FUNCTION pyo.keep_tunnel_record() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
  IF OLD.via = 'tunnel' AND OLD.record_id IS NOT NULL
     AND EXISTS (SELECT 1 FROM pyo.teams WHERE id = OLD.team_id) THEN
    INSERT INTO pyo.tunnel_leftovers (team_id, kind, zone_id, record_id, hostname)
    VALUES (OLD.team_id, 'record', OLD.zone_id, OLD.record_id, OLD.hostname);
  END IF;
  RETURN OLD;
END
$$;
CREATE TRIGGER dns_records_tunnel_leftover BEFORE DELETE ON pyo.dns_records
  FOR EACH ROW EXECUTE FUNCTION pyo.keep_tunnel_record();

CREATE FUNCTION pyo.keep_tunnel() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
  IF (OLD.tunnel_id IS NOT NULL OR OLD.cf_certificate_id IS NOT NULL OR OLD.default_record_id IS NOT NULL)
     AND EXISTS (SELECT 1 FROM pyo.teams WHERE id = OLD.team_id) THEN
    INSERT INTO pyo.tunnel_leftovers (team_id, kind, zone_id, record_id, hostname, account_id, tunnel_id, cf_certificates)
    VALUES (OLD.team_id, 'tunnel', OLD.default_zone_id, OLD.default_record_id, '*.' || OLD.default_domain,
            OLD.account_id, OLD.tunnel_id, array_remove(ARRAY[OLD.cf_certificate_id, OLD.cf_certificate_old], NULL));
  END IF;
  RETURN OLD;
END
$$;
CREATE TRIGGER cluster_tunnels_leftover BEFORE DELETE ON pyo.cluster_tunnels
  FOR EACH ROW EXECUTE FUNCTION pyo.keep_tunnel();
