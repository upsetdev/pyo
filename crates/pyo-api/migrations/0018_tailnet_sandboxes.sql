-- Pyo's own tailnet name for sandboxes' web pages, like the desktop
-- ("sandboxes.<tailnet>.ts.net", sandbox/pages.rs), next to the
-- dashboard's and the registry's.
ALTER TABLE pyo.tailnet_services DROP CONSTRAINT tailnet_services_kind_check;
ALTER TABLE pyo.tailnet_services ADD CONSTRAINT tailnet_services_kind_check
  CHECK (kind IN ('dashboard', 'registry', 'sandboxes', 'domain'));
