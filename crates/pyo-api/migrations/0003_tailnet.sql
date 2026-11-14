-- Tailscale Services Pyo made ("svc:<name>", tailscale.rs): Pyo's own names
-- on the tailnet and services' tailnet domains. Each is marked as this
-- installation's on the tailnet too (its comment), so Pyo never touches
-- one it didn't make.
CREATE TABLE pyo.tailnet_services (
  name        text PRIMARY KEY,
  -- 'dashboard', 'registry' or 'domain'
  kind        text NOT NULL CHECK (kind IN ('dashboard', 'registry', 'domain')),
  -- The cluster whose servers host it.
  cluster_id  text REFERENCES pyo.clusters (id) ON DELETE SET NULL,
  -- Its VIPs, IPv4 then IPv6.
  addrs       text[] NOT NULL DEFAULT '{}',
  -- Why it isn't working, as people read it ("Taken on your tailnet").
  error       text,
  -- Deleted on the tailnet: its servers take it off.
  retired_at  timestamptz,
  created_at  timestamptz NOT NULL DEFAULT now(),
  updated_at  timestamptz NOT NULL DEFAULT now()
);
