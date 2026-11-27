-- A Cloudflare Tunnel rotation under way (docs/cloudflare-tunnel.md §6.3
-- step 6): the connectors that ran the old secret, cut one by one once
-- the new ones are connected, or 5 minutes after the secret changed.
-- Null when none is under way.
ALTER TABLE pyo.cluster_tunnels ADD COLUMN rotation jsonb;
