-- Traffic paths (docs/traffic-and-gateway-design.md §7): the cluster's
-- Traffic card is three switches. Public ports on is traffic = 'public' and
-- Cloudflare Tunnel on is a cluster_tunnels row that isn't 'removing', as
-- before. These columns hold the rest.

-- Services' websites answer on the tailnet (ts.net names, the servers'
-- Tailscale addresses). Off: only Pyo's own names are served there.
ALTER TABLE pyo.clusters ADD COLUMN tailnet_web boolean NOT NULL DEFAULT true;

-- TCP and UDP ports on the servers while public ports are off.
-- NULL: as before (published in tunnel mode by the tunnel's ports_public).
ALTER TABLE pyo.clusters ADD COLUMN public_ports boolean;
UPDATE pyo.clusters c SET public_ports = t.ports_public
  FROM pyo.cluster_tunnels t WHERE t.cluster_id = c.id AND c.traffic = 'tunnel';

-- Public ports close once the domains asked to move to the tunnel are on
-- it (cloudflare/tunnel_loop.rs `close_public`). Set while that waits.
ALTER TABLE pyo.clusters ADD COLUMN close_public_after timestamptz;

-- Turning the tunnel off while public ports are on: its domains switch
-- back first (both paths answer meanwhile), then the tunnel is removed.
-- Set while that waits.
ALTER TABLE pyo.cluster_tunnels ADD COLUMN remove_when_back timestamptz;

-- cluster_tunnels.ports_public stays for one release, in step with
-- clusters.public_ports, so the previous release reads the right value
-- (it sends the agent publicPorts from it in tunnel mode). Both ways: the
-- previous release's setClusterTraffic writes ports_public. Each side only
-- writes when the value differs, so the two triggers stop after one round.
CREATE FUNCTION pyo.clusters_public_ports_sync() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
  IF NEW.public_ports IS NOT NULL AND NEW.public_ports IS DISTINCT FROM OLD.public_ports THEN
    UPDATE pyo.cluster_tunnels SET ports_public = NEW.public_ports
     WHERE cluster_id = NEW.id AND ports_public IS DISTINCT FROM NEW.public_ports;
  END IF;
  RETURN NEW;
END
$$;
CREATE TRIGGER clusters_public_ports_sync AFTER UPDATE OF public_ports
  ON pyo.clusters FOR EACH ROW EXECUTE FUNCTION pyo.clusters_public_ports_sync();

CREATE FUNCTION pyo.cluster_tunnels_ports_sync() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
  -- Whoever makes a tunnel says what its ports are: the cluster follows.
  IF TG_OP = 'INSERT' OR NEW.ports_public IS DISTINCT FROM OLD.ports_public THEN
    UPDATE pyo.clusters SET public_ports = NEW.ports_public
     WHERE id = NEW.cluster_id AND public_ports IS DISTINCT FROM NEW.ports_public;
  END IF;
  RETURN NEW;
END
$$;
CREATE TRIGGER cluster_tunnels_ports_sync AFTER INSERT OR UPDATE OF ports_public
  ON pyo.cluster_tunnels FOR EACH ROW EXECUTE FUNCTION pyo.cluster_tunnels_ports_sync();
