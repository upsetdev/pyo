-- Team observability (teamobs.rs, Team › Settings › Observability). Each is
-- the team's own choice, null while it follows the installation's default
-- (pyo.settings 'observability'). What applies is never more than the
-- installation's limit: it's worked out when used, so a lower limit hides
-- more at once and the team's choice comes back when it's raised again.

-- Whether the team's servers collect analytics, request logs and usage
-- history. Null: on. Off keeps what was collected.
ALTER TABLE pyo.teams ADD COLUMN observability_on boolean;
-- Whether the team's servers keep its requests and connections. Null: on.
ALTER TABLE pyo.teams ADD COLUMN request_logs_on boolean;
-- What's kept of visitors' addresses. Null: the installation's default.
ALTER TABLE pyo.teams ADD COLUMN ip_mode text
  CHECK (ip_mode IN ('none', 'anonymized', 'full'));
-- What's kept of query strings: values and names, names only, or nothing.
-- Null: the installation's default. Sensitive values are never kept.
ALTER TABLE pyo.teams ADD COLUMN query_strings text
  CHECK (query_strings IN ('keep', 'names', 'none'));
