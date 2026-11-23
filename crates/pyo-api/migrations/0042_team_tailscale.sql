-- Tailscale per team (tailscale.rs, docs/tailscale-all-clusters.md).

-- A team's Tailscale. Its OAuth client lives in one of its clusters, the
-- holder, whose agent does the Tailscale work for every cluster of the
-- team. Pyo keeps only what isn't secret.
CREATE TABLE pyo.team_tailscale (
  team_id           text PRIMARY KEY REFERENCES pyo.teams (id) ON DELETE CASCADE,
  holder_cluster_id text REFERENCES pyo.clusters (id) ON DELETE SET NULL,
  client_id         text NOT NULL,
  hint              text NOT NULL DEFAULT '',
  -- MagicDNS suffix, as last seen ("tail1234.ts.net").
  suffix            text,
  -- tag:pyo machines and Tailnet Lock waits, from the holder's last sync.
  machines          jsonb NOT NULL DEFAULT '[]',
  lock_waiting      jsonb NOT NULL DEFAULT '[]',
  -- Why Tailscale isn't working: 'holder_gone' or 'client_refused'. NULL when fine.
  problem           text CHECK (problem IN ('holder_gone', 'client_refused')),
  -- OAuth clients the team's clusters kept before, forgotten once the
  -- holder took over: to delete in the Tailscale console.
  -- [{clientId, hint, clusterId, cluster}]
  old_clients       jsonb NOT NULL DEFAULT '[]',
  created_at        timestamptz NOT NULL DEFAULT now(),
  updated_at        timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX team_tailscale_holder_idx ON pyo.team_tailscale (holder_cluster_id);
ALTER TABLE pyo.team_tailscale ENABLE ROW LEVEL SECURITY;
CREATE POLICY team_rows ON pyo.team_tailscale TO pyo_team
  USING (team_id = ANY (pyo.team_scope()))
  WITH CHECK (team_id = ANY (pyo.team_scope()));

-- A command one cluster's agent runs for another of the same team (the
-- holder, for a cluster of its team). NULL: for the cluster itself.
ALTER TABLE pyo.commands ADD COLUMN for_cluster text REFERENCES pyo.clusters (id) ON DELETE CASCADE;
CREATE INDEX commands_for_cluster_idx ON pyo.commands (for_cluster) WHERE for_cluster IS NOT NULL;

-- A removed cluster's machines and names on its team's tailnet, for the
-- holder to take off. Gone once it did, or after a week.
CREATE TABLE pyo.tailnet_leftovers (
  id           bigserial PRIMARY KEY,
  team_id      text NOT NULL REFERENCES pyo.teams (id) ON DELETE CASCADE,
  -- The removed cluster (its row is gone): its names' mark is made from it.
  cluster_id   text NOT NULL,
  cluster_name text NOT NULL DEFAULT '',
  -- Older marks that were its own too.
  adopt        text[] NOT NULL DEFAULT '{}',
  names        text[] NOT NULL DEFAULT '{}',
  -- Its servers' node IDs.
  nodes        text[] NOT NULL DEFAULT '{}',
  -- The holder's sync taking them off, while one is out, and the last try.
  command_id   text,
  tried_at     timestamptz,
  created_at   timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX tailnet_leftovers_team_idx ON pyo.tailnet_leftovers (team_id);
ALTER TABLE pyo.tailnet_leftovers ENABLE ROW LEVEL SECURITY;
CREATE POLICY team_rows ON pyo.tailnet_leftovers TO pyo_team
  USING (team_id = ANY (pyo.team_scope()))
  WITH CHECK (team_id = ANY (pyo.team_scope()));

-- Kept when a cluster on its team's tailnet is removed (not Pyo's own
-- cluster, and not one with its own client on another tailnet), while its
-- team is still there.
CREATE FUNCTION pyo.keep_tailnet_leftovers() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE
  v_nodes text[];
  v_names text[];
BEGIN
  IF OLD.tailscale IS NULL OR OLD.same_cluster
     OR NOT EXISTS (SELECT 1 FROM pyo.teams WHERE id = OLD.team_id) THEN
    RETURN OLD;
  END IF;
  IF NOT EXISTS (SELECT 1 FROM pyo.team_tailscale tt
                  WHERE tt.team_id = OLD.team_id
                    AND (coalesce(OLD.tailscale->>'clientId', '') = ''
                         OR tt.holder_cluster_id = OLD.id
                         OR OLD.tailscale->>'suffix' = tt.suffix)) THEN
    RETURN OLD;
  END IF;
  SELECT coalesce(array_agg(DISTINCT t->>'nodeId'), '{}') INTO v_nodes
    FROM jsonb_array_elements(coalesce(OLD.capabilities->'tailnet', '[]')) t
   WHERE coalesce(t->>'nodeId', '') <> '';
  SELECT coalesce(array_agg(name), '{}') INTO v_names
    FROM pyo.cluster_tailnet_names WHERE cluster_id = OLD.id;
  IF cardinality(v_nodes) > 0 OR cardinality(v_names) > 0 THEN
    INSERT INTO pyo.tailnet_leftovers (team_id, cluster_id, cluster_name, adopt, names, nodes)
    VALUES (OLD.team_id, OLD.id, OLD.name,
            ARRAY(SELECT jsonb_array_elements_text(coalesce(OLD.tailscale->'adopt', '[]'))),
            v_names, v_nodes);
  END IF;
  RETURN OLD;
END
$$;
CREATE TRIGGER keep_tailnet_leftovers BEFORE DELETE ON pyo.clusters
  FOR EACH ROW EXECUTE FUNCTION pyo.keep_tailnet_leftovers();
