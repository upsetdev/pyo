-- Row security on team_id (rls.rs), behind the API's own team checks.
--
-- Pyo connects as the owner of these tables, which row security doesn't
-- hold back: migrations, agents, builders, webhooks and background jobs
-- see every row as before, and so does the release before this one. A
-- request made for a person or an API key lowers its transaction to the
-- pyo_team role, with the teams it may see in pyo.team_ids. There the
-- policies below show and accept only those teams' rows.

DO $$
BEGIN
  IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'pyo_team') THEN
    CREATE ROLE pyo_team NOLOGIN;
  END IF;
  -- Pyo's own login switches to it (SET ROLE) in a request's transaction.
  -- A superuser already may.
  IF NOT pg_has_role(current_user, 'pyo_team',
       CASE WHEN current_setting('server_version_num')::int >= 160000 THEN 'SET' ELSE 'MEMBER' END) THEN
    EXECUTE format('GRANT pyo_team TO %I', current_user);
  END IF;
END
$$;

GRANT USAGE ON SCHEMA pyo TO pyo_team;
GRANT SELECT, INSERT, UPDATE, DELETE ON ALL TABLES IN SCHEMA pyo TO pyo_team;
GRANT USAGE, SELECT, UPDATE ON ALL SEQUENCES IN SCHEMA pyo TO pyo_team;
-- Tables later migrations add, as these are.
ALTER DEFAULT PRIVILEGES IN SCHEMA pyo GRANT SELECT, INSERT, UPDATE, DELETE ON TABLES TO pyo_team;
ALTER DEFAULT PRIVILEGES IN SCHEMA pyo GRANT USAGE, SELECT, UPDATE ON SEQUENCES TO pyo_team;

-- The teams the transaction may see. NULL (none) when it set none.
CREATE FUNCTION pyo.team_scope() RETURNS text[]
  LANGUAGE sql STABLE AS $$ SELECT string_to_array(nullif(current_setting('pyo.team_ids', true), ''), ',') $$;

-- Tables with a team_id: secrets, credentials and what services run with.
DO $$
DECLARE
  t text;
BEGIN
  FOREACH t IN ARRAY ARRAY[
    'alert_channels', 'backup_databases', 'backup_store_logins', 'build_jobs',
    'certificates', 'cloudflare_accounts', 'data_keys', 'databases',
    'deploy_hooks', 'deployments', 'dns_records', 'git_connections',
    'github_installations', 'integrations', 'origin_pull_certificates',
    'registry_logins', 'registry_tokens', 'secret_stores', 'services',
    'team_backup_settings'
  ] LOOP
    EXECUTE format('ALTER TABLE pyo.%I ENABLE ROW LEVEL SECURITY', t);
    EXECUTE format(
      'CREATE POLICY team_rows ON pyo.%I TO pyo_team
         USING (team_id = ANY (pyo.team_scope()))
         WITH CHECK (team_id = ANY (pyo.team_scope()))', t);
  END LOOP;
END
$$;

-- Tables without one go by what they belong to.
ALTER TABLE pyo.deployment_logs ENABLE ROW LEVEL SECURITY;
CREATE POLICY team_rows ON pyo.deployment_logs TO pyo_team
  USING (deployment_id IN (SELECT id FROM pyo.deployments WHERE team_id = ANY (pyo.team_scope())))
  WITH CHECK (deployment_id IN (SELECT id FROM pyo.deployments WHERE team_id = ANY (pyo.team_scope())));

ALTER TABLE pyo.git_webhooks ENABLE ROW LEVEL SECURITY;
CREATE POLICY team_rows ON pyo.git_webhooks TO pyo_team
  USING (connection_id IN (SELECT id FROM pyo.git_connections WHERE team_id = ANY (pyo.team_scope())))
  WITH CHECK (connection_id IN (SELECT id FROM pyo.git_connections WHERE team_id = ANY (pyo.team_scope())));

-- An agent's keys, and the commands it gets (with what services run
-- with): its cluster's team.
ALTER TABLE pyo.agent_keys ENABLE ROW LEVEL SECURITY;
CREATE POLICY team_rows ON pyo.agent_keys TO pyo_team
  USING (cluster_id IN (SELECT id FROM pyo.clusters WHERE team_id = ANY (pyo.team_scope())))
  WITH CHECK (cluster_id IN (SELECT id FROM pyo.clusters WHERE team_id = ANY (pyo.team_scope())));

ALTER TABLE pyo.commands ENABLE ROW LEVEL SECURITY;
CREATE POLICY team_rows ON pyo.commands TO pyo_team
  USING (cluster_id IN (SELECT id FROM pyo.clusters WHERE team_id = ANY (pyo.team_scope())))
  WITH CHECK (cluster_id IN (SELECT id FROM pyo.clusters WHERE team_id = ANY (pyo.team_scope())));
