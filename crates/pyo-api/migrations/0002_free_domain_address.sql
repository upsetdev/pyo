-- Where a cluster's free domains point: 'public' (its public IP) or
-- 'tailscale' (its Tailscale IP). NULL: public when it takes public
-- traffic and the internet reaches its public IP directly, else
-- Tailscale (freedomain.rs).
ALTER TABLE pyo.clusters ADD COLUMN free_domain_address text
  CHECK (free_domain_address IN ('public', 'tailscale'));
