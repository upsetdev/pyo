-- Tailscale goes through each cluster's agent (tailscale.rs). The cluster
-- keeps its own OAuth client, as a Secret in its pyo-system. Pyo keeps
-- only what isn't secret: the client's ID and hint, the tailnet's suffix,
-- Pyo's own names there and what the last sync said.
ALTER TABLE pyo.clusters ADD COLUMN tailscale jsonb;

-- The cluster's names on its tailnet (Tailscale Services). Per cluster:
-- each cluster's tailnet is its own, so two clusters can both have a
-- "blog". Replaces pyo.tailnet_services, whose name was unique across the
-- installation. That table stays until the release before this one stops
-- reading it.
CREATE TABLE pyo.cluster_tailnet_names (
  cluster_id  text NOT NULL REFERENCES pyo.clusters (id) ON DELETE CASCADE,
  name        text NOT NULL,
  kind        text NOT NULL CHECK (kind IN ('dashboard', 'registry', 'sandboxes', 'domain')),
  -- Its VIPs, IPv4 then IPv6.
  addrs       text[] NOT NULL DEFAULT '{}',
  -- Why it isn't working, as people read it ("Taken on your tailnet").
  error       text,
  -- Deleted on the tailnet: its servers take it off.
  retired_at  timestamptz,
  created_at  timestamptz NOT NULL DEFAULT now(),
  updated_at  timestamptz NOT NULL DEFAULT now(),
  PRIMARY KEY (cluster_id, name)
);

INSERT INTO pyo.cluster_tailnet_names (cluster_id, name, kind, addrs, error, retired_at, created_at, updated_at)
SELECT cluster_id, name, kind, addrs, error, retired_at, created_at, updated_at
  FROM pyo.tailnet_services WHERE cluster_id IS NOT NULL;

-- A sandbox server is reached through the agent of the cluster whose
-- tailnet it joined: Pyo itself may be on no tailnet (Pyo Cloud).
ALTER TABLE pyo.sandbox_servers ADD COLUMN via_cluster text REFERENCES pyo.clusters (id) ON DELETE SET NULL;
-- The installation's servers so far joined Pyo's own tailnet, the one its
-- own cluster is on.
UPDATE pyo.sandbox_servers SET via_cluster = (SELECT id FROM pyo.clusters WHERE same_cluster ORDER BY created_at LIMIT 1)
 WHERE via_cluster IS NULL;
