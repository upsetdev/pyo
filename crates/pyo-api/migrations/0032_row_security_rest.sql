-- Row security (rls.rs) on the team tables 0022 left out, and on the
-- tables that belong to a cluster, service, database or upload. As in
-- 0022, Pyo's own login (the owner) sees every row: only a request's
-- transaction lowered to pyo_team is held to its teams.

-- The teams a person (or an API key limited to some teams) may see, read
-- as the owner: members has row security now, and a transaction already
-- lowered would see only the teams it had.
CREATE FUNCTION pyo.member_team_ids(p_user text, p_key_teams text[]) RETURNS text
  LANGUAGE sql STABLE SECURITY DEFINER SET search_path = pg_catalog, pyo AS $$
  SELECT coalesce(string_agg(team_id, ','), '') FROM pyo.members
   WHERE user_id = p_user AND status = 'active' AND (p_key_teams IS NULL OR team_id = ANY (p_key_teams))
$$;
REVOKE ALL ON FUNCTION pyo.member_team_ids(text, text[]) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION pyo.member_team_ids(text, text[]) TO pyo_team;

-- Tables with a team_id.
DO $$
DECLARE
  t text;
BEGIN
  FOREACH t IN ARRAY ARRAY[
    'activity', 'alert_state', 'backups', 'builders', 'cloud_servers',
    'clusters', 'database_import_uploads', 'database_imports',
    'firewall_rules', 'members', 'platform_imports', 'projects',
    'registry_repos', 'sandbox_limits', 'sandbox_usage', 'sandboxes',
    'static_sites', 'terminal_sessions', 'volumes', 'waf_rules'
  ] LOOP
    EXECUTE format('ALTER TABLE pyo.%I ENABLE ROW LEVEL SECURITY', t);
    EXECUTE format(
      'CREATE POLICY team_rows ON pyo.%I TO pyo_team
         USING (team_id = ANY (pyo.team_scope()))
         WITH CHECK (team_id = ANY (pyo.team_scope()))', t);
  END LOOP;
END
$$;

-- Sandbox servers and templates of the installation (team_id NULL) are
-- every team's to use, and only its admin's to change (outside a team's
-- scope).
DO $$
DECLARE
  t text;
BEGIN
  FOREACH t IN ARRAY ARRAY['sandbox_servers', 'sandbox_templates'] LOOP
    EXECUTE format('ALTER TABLE pyo.%I ENABLE ROW LEVEL SECURITY', t);
    EXECUTE format(
      'CREATE POLICY team_rows ON pyo.%I TO pyo_team
         USING (team_id IS NULL OR team_id = ANY (pyo.team_scope()))
         WITH CHECK (team_id = ANY (pyo.team_scope()))', t);
  END LOOP;
END
$$;

-- What a cluster's agent reports and keeps: its cluster's team.
DO $$
DECLARE
  t text;
BEGIN
  FOREACH t IN ARRAY ARRAY[
    'servers', 'server_tags', 'cluster_bundles', 'cluster_tailnet_names',
    'tailnet_services', 'deleted_databases', 'deleted_volumes'
  ] LOOP
    EXECUTE format('ALTER TABLE pyo.%I ENABLE ROW LEVEL SECURITY', t);
    EXECUTE format(
      'CREATE POLICY team_rows ON pyo.%I TO pyo_team
         USING (cluster_id IN (SELECT id FROM pyo.clusters WHERE team_id = ANY (pyo.team_scope())))
         WITH CHECK (cluster_id IN (SELECT id FROM pyo.clusters WHERE team_id = ANY (pyo.team_scope())))', t);
  END LOOP;
END
$$;

-- A service's cron runs, with their output.
ALTER TABLE pyo.cron_runs ENABLE ROW LEVEL SECURITY;
CREATE POLICY team_rows ON pyo.cron_runs TO pyo_team
  USING (service_id IN (SELECT id FROM pyo.services WHERE team_id = ANY (pyo.team_scope())))
  WITH CHECK (service_id IN (SELECT id FROM pyo.services WHERE team_id = ANY (pyo.team_scope())));

-- A database's events and the services it's linked to.
DO $$
DECLARE
  t text;
BEGIN
  FOREACH t IN ARRAY ARRAY['database_events', 'database_links'] LOOP
    EXECUTE format('ALTER TABLE pyo.%I ENABLE ROW LEVEL SECURITY', t);
    EXECUTE format(
      'CREATE POLICY team_rows ON pyo.%I TO pyo_team
         USING (database_id IN (SELECT id FROM pyo.databases WHERE team_id = ANY (pyo.team_scope())))
         WITH CHECK (database_id IN (SELECT id FROM pyo.databases WHERE team_id = ANY (pyo.team_scope())))', t);
  END LOOP;
END
$$;

-- A folder sent with `pyo deploy`.
ALTER TABLE pyo.source_upload_parts ENABLE ROW LEVEL SECURITY;
CREATE POLICY team_rows ON pyo.source_upload_parts TO pyo_team
  USING (upload_id IN (SELECT id FROM pyo.source_uploads WHERE team_id = ANY (pyo.team_scope())))
  WITH CHECK (upload_id IN (SELECT id FROM pyo.source_uploads WHERE team_id = ANY (pyo.team_scope())));

-- A sandbox server's edge certificate: its key is changed only for the
-- team's own servers. The installation's servers' (shared by every team)
-- can be read, like the servers.
ALTER TABLE pyo.sandbox_edges ENABLE ROW LEVEL SECURITY;
CREATE POLICY team_rows ON pyo.sandbox_edges TO pyo_team
  USING (server_id IN (SELECT id FROM pyo.sandbox_servers))
  WITH CHECK (server_id IN (SELECT id FROM pyo.sandbox_servers WHERE team_id = ANY (pyo.team_scope())));
