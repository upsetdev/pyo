-- Pyo's initial schema. From here on: one new migration per change, never
-- edited once released.
--
-- Everything lives in the pyo schema. Ids are prefixed strings ("prj_…"),
-- times are timestamptz. What the dashboard edits as a whole (a service's
-- configuration, a deployment's stages and events) is jsonb in the
-- dashboard's own shape; columns hold what the API filters, joins or
-- enforces on.

CREATE SCHEMA IF NOT EXISTS pyo;

-- The dashboard's timestamps: epoch milliseconds.
CREATE FUNCTION pyo.ms(t timestamptz) RETURNS bigint
  LANGUAGE sql IMMUTABLE AS $$ SELECT (extract(epoch FROM t) * 1000)::bigint $$;

-- ---------------------------------------------------------------------------
-- Accounts and sign-in
-- ---------------------------------------------------------------------------

CREATE TABLE pyo.users (
  id            text PRIMARY KEY,
  email         text NOT NULL UNIQUE,     -- stored lowercased
  name          text NOT NULL DEFAULT '',
  password_hash text NOT NULL,            -- argon2id PHC string
  timezone      text NOT NULL DEFAULT 'UTC',
  created_at    timestamptz NOT NULL DEFAULT now(),
  -- Who runs this installation (the first account): installation-wide settings.
  is_admin      boolean NOT NULL DEFAULT false,
  -- The installer prints the first sign-in; its owner must replace it.
  must_set_up   boolean NOT NULL DEFAULT false
);

-- Browser sessions. The cookie holds a random token; only its sha256 is kept.
CREATE TABLE pyo.sessions (
  token_hash   text PRIMARY KEY,
  user_id      text NOT NULL REFERENCES pyo.users (id) ON DELETE CASCADE,
  created_at   timestamptz NOT NULL DEFAULT now(),
  expires_at   timestamptz NOT NULL,
  user_agent   text NOT NULL DEFAULT ''
);
CREATE INDEX sessions_user_idx ON pyo.sessions (user_id);

-- Sign-in attempts per client address or account (ratelimit.rs), shared
-- by every replica. One fixed window per (scope, key); the key is a sha256
-- of the address or email, never the value itself. Rows older than a day
-- are pruned.
CREATE TABLE pyo.rate_limits (
  scope         text NOT NULL,
  key           text NOT NULL,
  window_start  timestamptz NOT NULL,
  count         integer NOT NULL,
  PRIMARY KEY (scope, key)
);
CREATE INDEX rate_limits_window_idx ON pyo.rate_limits (window_start);

-- Passkeys (WebAuthn, passkeys.rs). A passkey belongs to the host name it
-- was added at (rp_id).
CREATE TABLE pyo.passkeys (
  id             text PRIMARY KEY,
  user_id        text NOT NULL REFERENCES pyo.users (id) ON DELETE CASCADE,
  -- base64url, as the browser reports it.
  credential_id  text NOT NULL UNIQUE,
  -- The account's WebAuthn user handle (a UUID), the same on all its passkeys.
  user_handle    text NOT NULL,
  rp_id          text NOT NULL,
  name           text NOT NULL,
  -- webauthn-rs's Passkey: public key, counter and flags.
  passkey        jsonb NOT NULL,
  created_at     timestamptz NOT NULL DEFAULT now(),
  last_used_at   timestamptz
);
CREATE INDEX passkeys_user_idx ON pyo.passkeys (user_id);

-- Ceremonies in flight: the challenge state stays here, never with the
-- browser. Single use, a few minutes long.
CREATE TABLE pyo.passkey_challenges (
  id          text PRIMARY KEY,
  -- 'register' (user_id set) or 'login'.
  kind        text NOT NULL,
  user_id     text REFERENCES pyo.users (id) ON DELETE CASCADE,
  rp_id       text NOT NULL,
  origin      text NOT NULL,
  state       jsonb NOT NULL,
  expires_at  timestamptz NOT NULL
);
CREATE INDEX passkey_challenges_expires_idx ON pyo.passkey_challenges (expires_at);

-- Personal API keys, for scripts and assistants (MCP) acting as a user.
-- Shown once; only the hash is kept, plus a short prefix to tell them
-- apart. `access`: what it may do; `team_ids`: where (NULL: all of the
-- owner's teams, including future ones). `expires_at` NULL: never.
CREATE TABLE pyo.api_tokens (
  id           text PRIMARY KEY,
  user_id      text NOT NULL REFERENCES pyo.users (id) ON DELETE CASCADE,
  name         text NOT NULL,
  token_hash   text NOT NULL UNIQUE,
  prefix       text NOT NULL,
  created_at   timestamptz NOT NULL DEFAULT now(),
  last_used_at timestamptz,
  expires_at   timestamptz,
  access       text NOT NULL DEFAULT 'full' CHECK (access IN ('read', 'deploy', 'full')),
  team_ids     text[]
);
CREATE INDEX api_tokens_user_idx ON pyo.api_tokens (user_id);

-- `pyo login`: the signed-in user approves the CLI on the dashboard, which
-- hands it a one-time code over localhost; the CLI trades the code (with
-- the verifier only it knows) for an API key. Codes last 5 minutes.
CREATE TABLE pyo.cli_logins (
  code_hash  text PRIMARY KEY,
  user_id    text NOT NULL REFERENCES pyo.users (id) ON DELETE CASCADE,
  challenge  text NOT NULL,
  name       text NOT NULL,
  expires_at timestamptz NOT NULL
);

-- OAuth 2.1 for MCP clients (oauth.rs). Clients register themselves
-- (RFC 7591), a signed-in user approves them, and they get short-lived
-- access tokens plus rotating refresh tokens. Only hashes are stored.
CREATE TABLE pyo.oauth_clients (
  id            text PRIMARY KEY,           -- client_id
  name          text NOT NULL,
  redirect_uris jsonb NOT NULL,
  created_at    timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE pyo.oauth_codes (
  code_hash      text PRIMARY KEY,
  client_id      text NOT NULL REFERENCES pyo.oauth_clients (id) ON DELETE CASCADE,
  user_id        text NOT NULL REFERENCES pyo.users (id) ON DELETE CASCADE,
  redirect_uri   text NOT NULL,
  code_challenge text NOT NULL,             -- PKCE, S256
  scope          text NOT NULL,
  resource       text NOT NULL,
  expires_at     timestamptz NOT NULL
);

-- One row per approval ("grant"); its tokens rotate in place.
CREATE TABLE pyo.oauth_grants (
  id                 text PRIMARY KEY,
  client_id          text NOT NULL REFERENCES pyo.oauth_clients (id) ON DELETE CASCADE,
  user_id            text NOT NULL REFERENCES pyo.users (id) ON DELETE CASCADE,
  scope              text NOT NULL,
  resource           text NOT NULL,
  access_hash        text UNIQUE,
  access_expires_at  timestamptz,
  refresh_hash       text UNIQUE,
  refresh_expires_at timestamptz,
  -- The refresh token this one replaced: presented again, it was copied,
  -- and the grant is revoked.
  previous_refresh_hash text UNIQUE,
  created_at         timestamptz NOT NULL DEFAULT now(),
  last_used_at       timestamptz
);
CREATE INDEX oauth_grants_user_idx ON pyo.oauth_grants (user_id);

-- What assistants did over MCP: one row per tool call, kept 30 days.
CREATE TABLE pyo.mcp_calls (
  id          bigserial PRIMARY KEY,
  user_id     text NOT NULL REFERENCES pyo.users (id) ON DELETE CASCADE,
  -- The assistant, by name: kept after it's disconnected.
  client      text NOT NULL,
  tool        text NOT NULL,
  args        jsonb NOT NULL DEFAULT '{}',
  ok          boolean NOT NULL,
  error       text,
  duration_ms integer NOT NULL,
  created_at  timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX mcp_calls_user_idx ON pyo.mcp_calls (user_id, created_at DESC);

-- ---------------------------------------------------------------------------
-- Installation
-- ---------------------------------------------------------------------------

-- Installation-wide settings, one JSON value per key ("email", "github",
-- "backup", "observability", "updates", ...).
CREATE TABLE pyo.settings (
  key        text PRIMARY KEY,
  value      jsonb NOT NULL,
  updated_at timestamptz NOT NULL DEFAULT now()
);

-- Durable background jobs (jobs.rs): claimed with FOR UPDATE SKIP LOCKED,
-- retried with backoff, recovered when the worker that held one vanished.
CREATE TABLE pyo.jobs (
  id          bigserial PRIMARY KEY,
  kind        text NOT NULL,
  payload     jsonb NOT NULL DEFAULT '{}',
  status      text NOT NULL DEFAULT 'queued' CHECK (status IN ('queued', 'running', 'done', 'failed')),
  run_at      timestamptz NOT NULL DEFAULT now(),
  attempts    int NOT NULL DEFAULT 0,
  last_error  text,
  locked_at   timestamptz,
  finished_at timestamptz,
  created_at  timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX jobs_due_idx ON pyo.jobs (run_at) WHERE status IN ('queued', 'running');

-- Backups of Pyo's own data (backup.rs); settings are in pyo.settings
-- ('backup'), this is each run.
CREATE TABLE pyo.control_backups (
  id          text PRIMARY KEY,
  -- queued | running | succeeded | failed
  status      text NOT NULL DEFAULT 'queued',
  -- schedule | manual
  trigger     text NOT NULL,
  object_key  text,
  size_bytes  bigint,
  tables      int,
  error       text,
  created_at  timestamptz NOT NULL DEFAULT now(),
  started_at  timestamptz,
  finished_at timestamptz
);
CREATE INDEX control_backups_created_idx ON pyo.control_backups (created_at DESC);

-- Updates of Pyo from the dashboard (updates.rs): one row per attempt. The
-- worker moves it along: Pyo itself, then the agent on every cluster.
CREATE TABLE pyo.updates (
  id          text PRIMARY KEY,
  -- The release it moves to, and what was running when it started.
  version     text NOT NULL,
  from_version text NOT NULL,
  -- 'manual' (an admin) or 'auto' (the maintenance window).
  trigger     text NOT NULL,
  started_by  text REFERENCES pyo.users (id) ON DELETE SET NULL,
  -- 'pyo' (updating Pyo), 'agents', then 'succeeded', 'partial' or 'failed'.
  status      text NOT NULL,
  -- The release.json it applies.
  release     jsonb NOT NULL,
  -- The UpgradePyo command, while Pyo is updating.
  command_id  text,
  -- Per cluster: {clusterId, name, from, state, commandId, message, at}.
  agents      jsonb NOT NULL DEFAULT '[]',
  message     text,
  created_at  timestamptz NOT NULL DEFAULT now(),
  updated_at  timestamptz NOT NULL DEFAULT now(),
  finished_at timestamptz
);
CREATE INDEX updates_created_idx ON pyo.updates (created_at DESC);

-- One random salt per day for hashing visitors (IP + user agent + host).
-- Yesterday's is kept so late log lines still count; older ones are
-- deleted, after which those hashes can't be recomputed by anyone.
CREATE TABLE pyo.observability_salts (
  day  date PRIMARY KEY,
  salt text NOT NULL
);

-- ---------------------------------------------------------------------------
-- Teams
-- ---------------------------------------------------------------------------

CREATE TABLE pyo.teams (
  id                 text PRIMARY KEY,
  name               text NOT NULL,
  slug               text NOT NULL UNIQUE,
  description        text NOT NULL DEFAULT '',
  default_cluster_id text,
  preferences        jsonb NOT NULL DEFAULT '{"requireDeployApproval":false,"autoDeployOnPush":true,"retainDeployments":20}',
  created_at         timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE pyo.members (
  id        text PRIMARY KEY,
  team_id   text NOT NULL REFERENCES pyo.teams (id) ON DELETE CASCADE,
  user_id   text REFERENCES pyo.users (id) ON DELETE CASCADE,   -- null while invited
  email     text NOT NULL,
  role      text NOT NULL CHECK (role IN ('owner', 'admin', 'developer', 'viewer')),
  status    text NOT NULL CHECK (status IN ('active', 'invited')),
  joined_at timestamptz NOT NULL DEFAULT now(),
  -- An invite is accepted with its link's secret (sha256 here), not by
  -- signing up with the address.
  invite_token_hash text UNIQUE,
  invite_expires_at timestamptz,
  UNIQUE (team_id, email)
);
CREATE INDEX members_user_idx ON pyo.members (user_id);

-- Envelope encryption (vault.rs): each team's data key, wrapped by the
-- master key. Values sealed with a key are unreadable once it's deleted,
-- so deleting a team shreds its secrets.
CREATE TABLE pyo.data_keys (
  id          text PRIMARY KEY,
  team_id     text NOT NULL REFERENCES pyo.teams (id) ON DELETE CASCADE,
  -- Which key-encryption key wrapped it: local:<fingerprint> or transit:<mount>/<key>.
  kek_id      text NOT NULL,
  wrapped     text NOT NULL,
  created_at  timestamptz NOT NULL DEFAULT now(),
  -- Last rewrapped under a new master key.
  rotated_at  timestamptz,
  -- Set when a newer key takes over; old values still open with it.
  retired_at  timestamptz
);
CREATE UNIQUE INDEX data_keys_team_active ON pyo.data_keys (team_id) WHERE retired_at IS NULL;

-- Third-party credentials a team saves (a Tailscale auth key), sealed;
-- `hint` is shown in the dashboard (e.g. the last 4 characters).
CREATE TABLE pyo.integrations (
  team_id    text NOT NULL REFERENCES pyo.teams (id) ON DELETE CASCADE,
  kind       text NOT NULL CHECK (kind IN ('tailscale')),
  secret     text NOT NULL,
  hint       text NOT NULL DEFAULT '',
  created_at timestamptz NOT NULL DEFAULT now(),
  PRIMARY KEY (team_id, kind)
);

-- External secret stores a team connects. The credential is sealed with
-- the team's data key, like its environment variables.
CREATE TABLE pyo.secret_stores (
  id         text PRIMARY KEY,
  team_id    text NOT NULL REFERENCES pyo.teams (id) ON DELETE CASCADE,
  kind       text NOT NULL,
  name       text NOT NULL,
  addr       text NOT NULL,
  options    jsonb NOT NULL DEFAULT '{}',
  credential text NOT NULL,
  created_at timestamptz NOT NULL DEFAULT now(),
  UNIQUE (team_id, name)
);

-- GitHub App installations: a GitHub account or organization that
-- installed this Pyo's GitHub App, linked to the team that connected it.
-- The app itself is an installation-wide setting (pyo.settings 'github').
CREATE TABLE pyo.github_installations (
  id                   bigint PRIMARY KEY,   -- GitHub's installation id
  team_id              text NOT NULL REFERENCES pyo.teams (id) ON DELETE CASCADE,
  account              text NOT NULL,        -- login of the user or organization
  account_type         text NOT NULL,        -- "User" or "Organization"
  repository_selection text NOT NULL DEFAULT 'selected',
  created_at           timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX github_installations_team_idx ON pyo.github_installations (team_id);

-- Logins for private container registries, one per registry host per
-- team. Services whose image is on that host pull with it.
CREATE TABLE pyo.registry_logins (
  id            text PRIMARY KEY,
  team_id       text NOT NULL REFERENCES pyo.teams (id) ON DELETE CASCADE,
  host          text NOT NULL,
  username      text NOT NULL,
  secret_sealed text NOT NULL,
  -- The registry accepted it when saved (false: it couldn't be reached to check).
  verified      boolean NOT NULL DEFAULT false,
  created_at    timestamptz NOT NULL DEFAULT now(),
  last_used_at  timestamptz,
  UNIQUE (team_id, host)
);

-- Tokens for pushing a team's own images to Pyo's registry
-- (registrydoor.rs), under teams/<team slug>/ only. Stored hashed; shown once.
CREATE TABLE pyo.registry_tokens (
  id           text PRIMARY KEY,
  team_id      text NOT NULL REFERENCES pyo.teams (id) ON DELETE CASCADE,
  name         text NOT NULL,
  token_hash   text NOT NULL UNIQUE,
  -- "push" (push and pull) or "pull".
  scope        text NOT NULL CHECK (scope IN ('push', 'pull')),
  created_by   text NOT NULL DEFAULT '',
  created_at   timestamptz NOT NULL DEFAULT now(),
  -- NULL: never expires.
  expires_at   timestamptz,
  last_used_at timestamptz
);
CREATE INDEX registry_tokens_team ON pyo.registry_tokens (team_id);

-- Certificates a team uploads for its domains (HTTPS mode "custom"). The
-- private key is sealed and only ever sent to agents.
CREATE TABLE pyo.certificates (
  id          text PRIMARY KEY,
  team_id     text NOT NULL REFERENCES pyo.teams (id) ON DELETE CASCADE,
  name        text NOT NULL,
  -- DNS names it's valid for (wildcards included), from the leaf certificate.
  hostnames   text[] NOT NULL,
  issuer      text NOT NULL,
  not_before  timestamptz NOT NULL,
  not_after   timestamptz NOT NULL,
  fingerprint text NOT NULL,
  -- Leaf first, then intermediates.
  chain_pem   text NOT NULL,
  key_sealed  text NOT NULL,
  -- Expiry warnings already sent: 30, then 7 (days before).
  warned_days integer,
  created_at  timestamptz NOT NULL DEFAULT now(),
  updated_at  timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX certificates_team_idx ON pyo.certificates (team_id);

-- Certificates a team generates for its CDN to prove itself to Pyo's
-- servers (Authenticated Origin Pulls). Only public material is kept: the
-- client certificate and its key are shown once, the CA's key is thrown away.
CREATE TABLE pyo.origin_pull_certificates (
  id          text PRIMARY KEY,
  team_id     text NOT NULL REFERENCES pyo.teams (id) ON DELETE CASCADE,
  name        text NOT NULL,
  common_name text NOT NULL,
  ca_pem      text NOT NULL,
  fingerprint text NOT NULL,
  not_after   timestamptz NOT NULL,
  warned_days int,
  created_at  timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX origin_pull_certificates_team_idx ON pyo.origin_pull_certificates (team_id);

CREATE TABLE pyo.activity (
  id      text PRIMARY KEY,
  team_id text NOT NULL REFERENCES pyo.teams (id) ON DELETE CASCADE,
  at      timestamptz NOT NULL DEFAULT now(),
  data    jsonb NOT NULL                  -- the dashboard's ActivityEvent
);
CREATE INDEX activity_team_idx ON pyo.activity (team_id, at DESC);

-- ---------------------------------------------------------------------------
-- Clusters and servers
-- ---------------------------------------------------------------------------

CREATE TABLE pyo.clusters (
  id                 text PRIMARY KEY,
  team_id            text NOT NULL REFERENCES pyo.teams (id) ON DELETE CASCADE,
  name               text NOT NULL,
  description        text NOT NULL DEFAULT '',
  location           text NOT NULL DEFAULT '',
  -- pending until the agent's first heartbeat; disconnected when removed by a user.
  status             text NOT NULL DEFAULT 'pending',
  version            text NOT NULL DEFAULT '',
  agent_version      text,
  -- sha256 of the agent's bearer token; the token itself is shown once.
  agent_token_hash   text NOT NULL UNIQUE,
  capabilities       jsonb NOT NULL DEFAULT '{}',
  workloads          jsonb NOT NULL DEFAULT '[]',
  created_at         timestamptz NOT NULL DEFAULT now(),
  last_seen_at       timestamptz,
  -- Tags people give it, to place services by.
  tags               text[] NOT NULL DEFAULT '{}',
  -- The cluster Pyo runs on: its agent proved it (agent.rs, with the host's
  -- bootstrap key), never only said so.
  same_cluster       boolean NOT NULL DEFAULT false,
  -- Added without a name: takes its server's hostname when the agent reports in.
  name_from_host     boolean NOT NULL DEFAULT false,
  -- Origin pull certificates required on HTTPS, and how applying that went:
  -- {certificateIds, revision, state, error}.
  origin_pull        jsonb NOT NULL DEFAULT '{}',
  -- 'region' (keep traffic in its region) or 'spread'; NULL: 'region'.
  traffic_mode       text,
  -- Ports 80 and 443 read a PROXY protocol header (behind a load balancer).
  web_proxy_protocol boolean NOT NULL DEFAULT false,
  -- It takes public traffic: the gateway is published on its servers (all
  -- but build-only ones). Off: it answers only inside the cluster.
  public_traffic     boolean NOT NULL DEFAULT true,
  -- Made by the host's own setup for the machine Pyo runs on (install.rs),
  -- before its agent reports in as same_cluster.
  from_host_setup    boolean NOT NULL DEFAULT false,
  -- Claims labelled as Pyo volumes that Pyo has no volume for, as the last
  -- heartbeat found them: shown on the cluster, never deleted unasked.
  stray_volumes      jsonb NOT NULL DEFAULT '[]',
  -- Its servers' login at Pyo's registry door, to pull (registrydoor.rs):
  -- sha256, and sealed (for the next server's join command).
  registry_token_hash   text UNIQUE,
  registry_token_sealed text,
  UNIQUE (team_id, name)
);

-- Servers as their cluster reports them.
CREATE TABLE pyo.servers (
  id          text PRIMARY KEY,
  cluster_id  text NOT NULL REFERENCES pyo.clusters (id) ON DELETE CASCADE,
  name        text NOT NULL,
  report      jsonb NOT NULL,             -- pyo_proto::ServerReport
  updated_at  timestamptz NOT NULL DEFAULT now(),
  -- Where it is ("City, CC"): 'manual' or 'geoip' as location_source; NULL
  -- uses the cluster's.
  location        text,
  location_source text,
  -- Being taken out of its cluster: {state, force, by, startedAt, message}.
  -- The row goes once the cluster stops reporting it.
  removal     jsonb,
  UNIQUE (cluster_id, name)
);

-- A server's settings, kept by name (not on its pyo.servers row, which follows
-- what the cluster reports) so they can be set before it joins.
CREATE TABLE pyo.server_tags (
  cluster_id text NOT NULL REFERENCES pyo.clusters (id) ON DELETE CASCADE,
  name       text NOT NULL,
  tags       text[] NOT NULL DEFAULT '{}',
  -- A GPU server runs regular services too; NULL: Pyo's default.
  regular_services boolean,
  PRIMARY KEY (cluster_id, name)
);

-- Work for an agent. Long-polled; one row per action.
CREATE TABLE pyo.commands (
  id          text PRIMARY KEY,
  cluster_id  text NOT NULL REFERENCES pyo.clusters (id) ON DELETE CASCADE,
  -- pending -> sent -> done | failed
  status      text NOT NULL DEFAULT 'pending',
  command     jsonb NOT NULL,             -- pyo_proto::Command
  result      jsonb,                      -- pyo_proto::CommandResult
  created_at  timestamptz NOT NULL DEFAULT now(),
  sent_at     timestamptz,
  done_at     timestamptz,
  -- Which agent process it was handed to: when another process of the same
  -- agent polls (it restarted), rollouts it was watching are handed out again.
  sent_to     text
);
CREATE INDEX commands_pending_idx ON pyo.commands (cluster_id, created_at) WHERE status = 'pending';

-- Bundles installed on a cluster (bundles.rs), one row each: the version
-- last sent, where that stands, and the objects it was (so an update prunes
-- the ones it drops and removal deletes them all).
CREATE TABLE pyo.cluster_bundles (
  cluster_id  text NOT NULL REFERENCES pyo.clusters (id) ON DELETE CASCADE,
  bundle      text NOT NULL,
  version     text NOT NULL,
  -- "installing" (sent), "applied" (the agent applied it; ready once its
  -- workloads are), "failed" or "removing".
  state       text NOT NULL CHECK (state IN ('installing', 'applied', 'failed', 'removing')),
  message     text NOT NULL DEFAULT '',
  -- The bundle's objects and ready checks as sent.
  objects     jsonb NOT NULL DEFAULT '[]',
  ready       jsonb NOT NULL DEFAULT '[]',
  command_id  text,
  -- What the agent last reported: the version it finds, and how many
  -- objects of its in-use kinds exist.
  reported_version text,
  in_use      integer NOT NULL DEFAULT 0,
  created_at  timestamptz NOT NULL DEFAULT now(),
  updated_at  timestamptz NOT NULL DEFAULT now(),
  PRIMARY KEY (cluster_id, bundle)
);

-- ---------------------------------------------------------------------------
-- Projects, services and deployments
-- ---------------------------------------------------------------------------

CREATE TABLE pyo.projects (
  id          text PRIMARY KEY,
  team_id     text NOT NULL REFERENCES pyo.teams (id) ON DELETE CASCADE,
  name        text NOT NULL,
  namespace   text NOT NULL,
  description text NOT NULL DEFAULT '',
  created_at  timestamptz NOT NULL DEFAULT now(),
  updated_at  timestamptz NOT NULL DEFAULT now(),
  -- Default-deny networking between projects (agent rollout.rs set_isolation).
  network_isolation boolean NOT NULL DEFAULT true,
  UNIQUE (team_id, namespace)
);

-- Services.
CREATE TABLE pyo.services (
  id                    text PRIMARY KEY,
  team_id               text NOT NULL REFERENCES pyo.teams (id) ON DELETE CASCADE,
  project_id            text NOT NULL REFERENCES pyo.projects (id) ON DELETE CASCADE,
  cluster_id            text NOT NULL REFERENCES pyo.clusters (id) ON DELETE RESTRICT,
  name                  text NOT NULL,
  -- running | deploying | degraded | failed | stopped
  status                text NOT NULL DEFAULT 'deploying',
  -- The dashboard's service minus status, runtime and the bookkeeping columns.
  spec                  jsonb NOT NULL,
  runtime               jsonb NOT NULL DEFAULT '{"readyReplicas":0,"cpuMillis":0,"memoryMi":0,"restarts":0}',
  current_deployment_id text,
  pending_changes       boolean NOT NULL DEFAULT false,
  -- Set while a delete is being carried out by the agent.
  deleting              boolean NOT NULL DEFAULT false,
  created_at            timestamptz NOT NULL DEFAULT now(),
  updated_at            timestamptz NOT NULL DEFAULT now(),
  -- The web application firewall: {"mode": "off" | "log" | "block",
  -- "level": 1-4, "exceptions": [942100], "threshold": 0 | 10 | 20 | 50};
  -- '{}' is off. While any service on a cluster has it on, the WAF's
  -- bundle runs there (bundles.rs).
  waf                   jsonb NOT NULL DEFAULT '{}',
  -- Grows with each change of its domains (spec.domains), which go live in
  -- place: the agent applies the newest it has seen (routes.rs).
  routes_revision       bigint NOT NULL DEFAULT 0,
  UNIQUE (project_id, name)
);
CREATE INDEX services_cluster_idx ON pyo.services (cluster_id);

CREATE TABLE pyo.deployments (
  id          text PRIMARY KEY,
  team_id     text NOT NULL REFERENCES pyo.teams (id) ON DELETE CASCADE,
  project_id  text NOT NULL,
  service_id  text NOT NULL REFERENCES pyo.services (id) ON DELETE CASCADE,
  cluster_id  text NOT NULL,
  status      text NOT NULL,
  -- source, revision, revisionLabel, trigger, actor, stages, stageStartedAt,
  -- events, failure, rollbackOf, retryOf, config, builderId: the
  -- dashboard's Deployment.
  data        jsonb NOT NULL,
  -- The DeploySpec sent to the agent; reused by rollbacks (with the
  -- service's current domains: routes.rs).
  spec        jsonb NOT NULL,
  created_at  timestamptz NOT NULL DEFAULT now(),
  finished_at timestamptz
);
CREATE INDEX deployments_service_idx ON pyo.deployments (service_id, created_at DESC);
CREATE INDEX deployments_team_idx ON pyo.deployments (team_id, created_at DESC);

CREATE TABLE pyo.deployment_logs (
  id            bigserial PRIMARY KEY,
  deployment_id text NOT NULL REFERENCES pyo.deployments (id) ON DELETE CASCADE,
  at            timestamptz NOT NULL,
  level         text NOT NULL,
  source        text NOT NULL,
  message       text NOT NULL
);
CREATE INDEX deployment_logs_dep_idx ON pyo.deployment_logs (deployment_id, id);

-- A deleted volume keeps its row (`deleting`) until the cluster says its
-- claim is gone. `namespace` is where its claim is, kept after its project
-- goes.
CREATE TABLE pyo.volumes (
  id         text PRIMARY KEY,
  team_id    text NOT NULL REFERENCES pyo.teams (id) ON DELETE CASCADE,
  cluster_id text NOT NULL REFERENCES pyo.clusters (id) ON DELETE CASCADE,
  project_id text,
  service_id text REFERENCES pyo.services (id) ON DELETE SET NULL,
  data       jsonb NOT NULL,              -- the dashboard's Volume
  created_at timestamptz NOT NULL DEFAULT now(),
  deleting   boolean NOT NULL DEFAULT false,
  namespace  text
);

-- The ids of volumes Pyo deleted, so a new volume can't take one.
CREATE TABLE pyo.deleted_volumes (
  id          text PRIMARY KEY,
  cluster_id  text NOT NULL REFERENCES pyo.clusters (id) ON DELETE CASCADE,
  namespace   text NOT NULL,
  name        text NOT NULL,
  deleted_at  timestamptz NOT NULL DEFAULT now()
);

-- Deploy hooks: secret URLs that deploy a service, or every service of a
-- project, when anything POSTs to them. The token is looked up by its hash
-- and kept sealed so the dashboard can show the URL again.
CREATE TABLE pyo.deploy_hooks (
  id           text PRIMARY KEY,
  team_id      text NOT NULL REFERENCES pyo.teams (id) ON DELETE CASCADE,
  project_id   text NOT NULL REFERENCES pyo.projects (id) ON DELETE CASCADE,
  service_id   text REFERENCES pyo.services (id) ON DELETE CASCADE,   -- null: the whole project
  name         text NOT NULL,
  token_hash   text NOT NULL UNIQUE,
  token_sealed text NOT NULL,
  created_by   text NOT NULL,
  created_at   timestamptz NOT NULL DEFAULT now(),
  last_used_at timestamptz
);
CREATE INDEX deploy_hooks_project_idx ON pyo.deploy_hooks (project_id);

-- Shells opened in service replicas from the dashboard: who, where and when
-- (an audit trail), plus which API replica holds the browser's socket so
-- the agent's socket can be relayed to it.
CREATE TABLE pyo.terminal_sessions (
  id         text PRIMARY KEY,
  team_id    text NOT NULL REFERENCES pyo.teams (id) ON DELETE CASCADE,
  service_id text REFERENCES pyo.services (id) ON DELETE SET NULL,
  user_id    text REFERENCES pyo.users (id) ON DELETE SET NULL,
  -- host:port of the API replica the browser is connected to.
  relay      text,
  pod        text,
  created_at timestamptz NOT NULL DEFAULT now(),
  ended_at   timestamptz
);
CREATE INDEX terminal_sessions_service_idx ON pyo.terminal_sessions (service_id, created_at DESC);

-- Every cron job run the clusters report, with the end of its output.
-- Kubernetes keeps only the last few, so Pyo keeps the history.
CREATE TABLE pyo.cron_runs (
  service_id  text NOT NULL REFERENCES pyo.services (id) ON DELETE CASCADE,
  job         text NOT NULL,
  -- Which of the service's cron jobs; '' for a service that is one.
  cron_id     text NOT NULL DEFAULT '',
  started_at  timestamptz NOT NULL,
  finished_at timestamptz,
  status      text NOT NULL,
  exit_code   int,
  reason      text,
  output      text,
  PRIMARY KEY (service_id, job)
);
CREATE INDEX cron_runs_recent ON pyo.cron_runs (service_id, cron_id, started_at DESC);

-- Repositories teams pushed to Pyo's registry (registrydoor.rs): which team
-- and service each belongs to, recorded on its first push.
CREATE TABLE pyo.registry_repos (
  repo         text PRIMARY KEY,
  team_id      text NOT NULL REFERENCES pyo.teams (id) ON DELETE CASCADE,
  service_id   text REFERENCES pyo.services (id) ON DELETE SET NULL,
  created_at   timestamptz NOT NULL DEFAULT now(),
  last_push_at timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX registry_repos_team ON pyo.registry_repos (team_id);

-- ---------------------------------------------------------------------------
-- Builders
-- ---------------------------------------------------------------------------

-- Builders turn Git sources into images: a pod pinned to a server of a
-- cluster, or a build-only server.
CREATE TABLE pyo.builders (
  id           text PRIMARY KEY,
  team_id      text NOT NULL REFERENCES pyo.teams (id) ON DELETE CASCADE,
  name         text NOT NULL,
  kind         text NOT NULL CHECK (kind IN ('cluster', 'server')),
  cluster_id   text REFERENCES pyo.clusters (id) ON DELETE CASCADE,
  server       text,
  -- provisioning | needs_signature | ready | offline | error | removing
  status       text NOT NULL DEFAULT 'provisioning',
  status_message text,
  -- Everything else shown in the dashboard: last heartbeat, Tailscale node
  -- key while it waits for a Tailnet Lock signature.
  info         jsonb NOT NULL DEFAULT '{}',
  token_hash   text NOT NULL UNIQUE,
  created_at   timestamptz NOT NULL DEFAULT now(),
  last_seen_at timestamptz,
  UNIQUE (team_id, name)
);

-- A build of one deployment's Git source. Any ready builder of the team
-- claims it.
CREATE TABLE pyo.build_jobs (
  id            text PRIMARY KEY,
  team_id       text NOT NULL REFERENCES pyo.teams (id) ON DELETE CASCADE,
  deployment_id text NOT NULL REFERENCES pyo.deployments (id) ON DELETE CASCADE,
  builder_id    text REFERENCES pyo.builders (id) ON DELETE SET NULL,
  -- queued -> running -> succeeded | failed | cancelled
  status        text NOT NULL DEFAULT 'queued',
  job           jsonb NOT NULL,       -- pyo_proto::BuildJob
  result        jsonb,                -- pyo_proto::BuildResult
  created_at    timestamptz NOT NULL DEFAULT now(),
  started_at    timestamptz,
  finished_at   timestamptz
);
CREATE INDEX build_jobs_queued_idx ON pyo.build_jobs (team_id, created_at) WHERE status = 'queued';

-- ---------------------------------------------------------------------------
-- Firewall and WAF
-- ---------------------------------------------------------------------------

-- Who may reach a service, in `position` order: allow and block rules
-- (the first that matches decides) and rate limits (websites; each one that
-- matches counts). A rule matches clients whose address or country is in
-- (`operator` 'in') or not in ('notIn') its `sources`: address ranges
-- ("203.0.113.0/24") or country codes ("DE"), not both; none is any client.
CREATE TABLE pyo.firewall_rules (
  id          text PRIMARY KEY,
  team_id     text NOT NULL REFERENCES pyo.teams (id) ON DELETE CASCADE,
  service_id  text NOT NULL REFERENCES pyo.services (id) ON DELETE CASCADE,
  position    integer NOT NULL DEFAULT 0,
  action      text NOT NULL CHECK (action IN ('allow', 'block', 'rateLimit')),
  operator    text NOT NULL DEFAULT 'in' CHECK (operator IN ('in', 'notIn')),
  sources     text[] NOT NULL DEFAULT '{}',
  -- A rate limit's: requests per second or minute from each address.
  requests    integer,
  per         text CHECK (per IN ('second', 'minute')),
  note        text NOT NULL DEFAULT '',
  created_by  text NOT NULL DEFAULT '',
  created_at  timestamptz NOT NULL DEFAULT now(),
  CHECK ((action = 'rateLimit') = (requests IS NOT NULL AND per IS NOT NULL))
);
CREATE INDEX firewall_rules_service ON pyo.firewall_rules (service_id, position);
CREATE INDEX firewall_rules_team ON pyo.firewall_rules (team_id);

-- A service's custom WAF rules, in `position` order, before the OWASP
-- rules. `number` is the rule's id in findings, from a range kept apart
-- from the Core Rule Set's.
CREATE SEQUENCE pyo.waf_rule_number MINVALUE 1000000 MAXVALUE 1999999 START 1000000 NO CYCLE;
CREATE TABLE pyo.waf_rules (
  id          text PRIMARY KEY,
  team_id     text NOT NULL REFERENCES pyo.teams (id) ON DELETE CASCADE,
  service_id  text NOT NULL REFERENCES pyo.services (id) ON DELETE CASCADE,
  number      integer NOT NULL UNIQUE DEFAULT nextval('pyo.waf_rule_number'),
  name        text NOT NULL,
  enabled     boolean NOT NULL DEFAULT true,
  position    integer NOT NULL,
  -- "all" or "any" of the conditions.
  matching    text NOT NULL CHECK (matching IN ('all', 'any')),
  -- [{"field", "name", "op", "values", "negate"}], checked by the API.
  conditions  jsonb NOT NULL,
  action      text NOT NULL CHECK (action IN ('block', 'log', 'allow', 'skipOwasp')),
  created_by  text NOT NULL DEFAULT '',
  created_at  timestamptz NOT NULL DEFAULT now(),
  updated_at  timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX waf_rules_service ON pyo.waf_rules (service_id, position);
CREATE INDEX waf_rules_team ON pyo.waf_rules (team_id);

-- ---------------------------------------------------------------------------
-- Databases
-- ---------------------------------------------------------------------------

-- Postgres, MySQL and KV databases a team runs on one of its clusters
-- (databases.rs). They belong to the team; any of its services on that
-- cluster can use one.
CREATE TABLE pyo.databases (
  id           text PRIMARY KEY,
  team_id      text NOT NULL REFERENCES pyo.teams (id) ON DELETE CASCADE,
  cluster_id   text NOT NULL REFERENCES pyo.clusters (id) ON DELETE CASCADE,
  name         text NOT NULL,
  engine       text NOT NULL CHECK (engine IN ('postgres', 'valkey', 'mysql')),
  version      text NOT NULL,
  -- "small", "medium", "large", "xlarge" or "custom": CPU and memory per instance.
  size         text NOT NULL,
  storage_gi   integer NOT NULL,
  -- The API checks each engine's counts; this only bounds them.
  instances    integer NOT NULL CHECK (instances BETWEEN 1 AND 9),
  pooler       boolean NOT NULL DEFAULT false,
  rest         boolean NOT NULL DEFAULT false,
  deletion_protection boolean NOT NULL DEFAULT true,
  -- Where its Database object is: the team's database namespace there.
  namespace    text NOT NULL,
  -- Its logins, sealed with the team's key (vault.rs).
  password     text NOT NULL,
  token        text NOT NULL DEFAULT '',
  readonly_token text NOT NULL DEFAULT '',
  -- "active" or "deleting"; how it's doing is `status`, as the cluster reports.
  state        text NOT NULL DEFAULT 'active',
  status       jsonb NOT NULL DEFAULT '{}',
  created_by   text NOT NULL DEFAULT '',
  created_at   timestamptz NOT NULL DEFAULT now(),
  updated_at   timestamptz NOT NULL DEFAULT now(),
  -- Point-in-time recovery: {enabled, retentionDays, hour}.
  backup       jsonb NOT NULL DEFAULT '{}',
  -- For one made from another's backups: the Database object's restore
  -- block (set at creation, never changed), and {sourceId, sourceName,
  -- targetTime} to show where it came from.
  restore      jsonb,
  restored_from jsonb,
  -- Dedicated read replicas (0: none), the database's size each.
  read_replicas int NOT NULL DEFAULT 0,
  -- Size 'custom': CPU in millicores and memory in MiB per instance.
  cpu_millis   int,
  memory_mi    int,
  -- Postgres extensions Pyo turns on (the Extensions tab).
  extensions   jsonb NOT NULL DEFAULT '[]',
  UNIQUE (team_id, name)
);
CREATE INDEX databases_cluster ON pyo.databases (cluster_id);

-- Services connected to one: the variables it added to each, and the
-- connection's own names for them ({"<service's key>": "<database's
-- variable>"}; null: the database's names).
CREATE TABLE pyo.database_links (
  database_id  text NOT NULL REFERENCES pyo.databases (id) ON DELETE CASCADE,
  service_id   text NOT NULL REFERENCES pyo.services (id) ON DELETE CASCADE,
  keys         text[] NOT NULL,
  created_at   timestamptz NOT NULL DEFAULT now(),
  env          jsonb,
  PRIMARY KEY (database_id, service_id)
);

-- What happened to a database, as Pyo saw its status change: failovers,
-- members ready or not, backups finishing. Kept 30 days.
CREATE TABLE pyo.database_events (
  id          bigserial PRIMARY KEY,
  database_id text NOT NULL REFERENCES pyo.databases (id) ON DELETE CASCADE,
  at          timestamptz NOT NULL DEFAULT now(),
  type        text NOT NULL,            -- "Normal" or "Warning"
  reason      text NOT NULL,
  message     text NOT NULL
);
CREATE INDEX database_events_idx ON pyo.database_events (database_id, at DESC);

-- Databases whose deletion the agent confirmed: a Database object on the
-- cluster labelled with one of these ids (left behind, or brought back by
-- a late change) is deleted again when a heartbeat reports it.
CREATE TABLE pyo.deleted_databases (
  id          text PRIMARY KEY,
  cluster_id  text NOT NULL REFERENCES pyo.clusters (id) ON DELETE CASCADE,
  namespace   text NOT NULL,
  name        text NOT NULL,
  deleted_at  timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX deleted_databases_cluster ON pyo.deleted_databases (cluster_id);

-- Imports into built-in databases (database_imports.rs): from another
-- database's URL or an uploaded dump. The latest of a database is its
-- Database object's `spec.import`.
CREATE TABLE pyo.database_imports (
  id           text PRIMARY KEY,
  team_id      text NOT NULL REFERENCES pyo.teams (id) ON DELETE CASCADE,
  -- Checked at commit: an import made with its database is recorded first.
  database_id  text NOT NULL REFERENCES pyo.databases (id) ON DELETE CASCADE DEFERRABLE INITIALLY DEFERRED,
  -- "url" or "file".
  source       text NOT NULL CHECK (source IN ('url', 'file')),
  -- Where from, for people, no password: "shop on db.example.com:5432", or the file's name.
  source_label text NOT NULL,
  -- The source's password (url), sealed with the team's key; dropped once it succeeded.
  password     text NOT NULL DEFAULT '',
  -- The Database's spec.import: connection (no password), format, size.
  spec         jsonb NOT NULL,
  -- The uploaded file (file), and the token its Job fetches it with:
  -- sha256 to check it, sealed to hand it to the cluster.
  upload_id    text,
  token_hash   text NOT NULL DEFAULT '',
  token        text NOT NULL DEFAULT '',
  -- Made together with its database: a failure leaves that one to retry or delete.
  new_database boolean NOT NULL DEFAULT false,
  -- "pending", "running", "succeeded" or "failed"; the controller's step, and why it failed.
  state        text NOT NULL DEFAULT 'pending' CHECK (state IN ('pending', 'running', 'succeeded', 'failed')),
  phase        text NOT NULL DEFAULT '',
  message      text NOT NULL DEFAULT '',
  created_by   text NOT NULL DEFAULT '',
  created_at   timestamptz NOT NULL DEFAULT clock_timestamp(),
  started_at   timestamptz,
  finished_at  timestamptz
);
CREATE INDEX database_imports_database ON pyo.database_imports (database_id, created_at DESC);

-- Dumps uploaded to import, in the team's bucket of Pyo's own store
-- (`object_key`). Two steps: made "pending" with an upload token (its hash,
-- until `token_expires_at`), then the file is PUT with it ("uploading",
-- the store's multipart upload in `multipart_id`) and it's "uploaded". Gone
-- once their import succeeded, or unused (or never finished) after a while.
CREATE TABLE pyo.database_import_uploads (
  id           text PRIMARY KEY,
  team_id      text NOT NULL REFERENCES pyo.teams (id) ON DELETE CASCADE,
  -- "postgres" or "mysql" when given up front; an import says which otherwise.
  engine       text CHECK (engine IN ('postgres', 'mysql')),
  file_name    text NOT NULL,
  -- The size declared up front; the file must be exactly that.
  size         bigint NOT NULL,
  content_type text NOT NULL DEFAULT '',
  -- "sql", "sql.gz" or "dump" (Postgres's custom format).
  format       text NOT NULL,
  state        text NOT NULL DEFAULT 'pending' CHECK (state IN ('pending', 'uploading', 'uploaded')),
  token_hash   text,
  token_expires_at timestamptz NOT NULL,
  multipart_id text NOT NULL DEFAULT '',
  -- The file's SHA-256 (hex), once uploaded.
  sha256       text NOT NULL DEFAULT '',
  -- What its start says: the extensions it makes (Postgres), the version it was dumped from.
  extensions   jsonb NOT NULL DEFAULT '[]',
  source_version text NOT NULL DEFAULT '',
  object_key   text NOT NULL,
  created_by   text NOT NULL DEFAULT '',
  created_at   timestamptz NOT NULL DEFAULT now(),
  uploaded_at  timestamptz
);
CREATE UNIQUE INDEX database_import_uploads_token ON pyo.database_import_uploads (token_hash) WHERE token_hash IS NOT NULL;

-- ---------------------------------------------------------------------------
-- Backups
-- ---------------------------------------------------------------------------

-- Where a team's backups go (teambackup.rs).
CREATE TABLE pyo.team_backup_settings (
  team_id    text PRIMARY KEY REFERENCES pyo.teams (id) ON DELETE CASCADE,
  value      jsonb NOT NULL,
  updated_at timestamptz NOT NULL DEFAULT now()
);

-- A team's own login to the backup store on Pyo's cluster (backup.rs
-- `team_login`): it reaches only the team's bucket. Secret sealed with the
-- team's key.
CREATE TABLE pyo.backup_store_logins (
  team_id       text PRIMARY KEY REFERENCES pyo.teams (id) ON DELETE CASCADE,
  access_key    text NOT NULL,
  secret_sealed text NOT NULL,
  created_at    timestamptz NOT NULL DEFAULT now()
);

-- Databases a team backs up (database_backups.rs): engine, where it is,
-- its backup login (password sealed with the team's key) and when.
CREATE TABLE pyo.backup_databases (
  id           text PRIMARY KEY,
  team_id      text NOT NULL REFERENCES pyo.teams (id) ON DELETE CASCADE,
  -- Where its backups run (the cluster must reach the host).
  cluster_id   text NOT NULL,
  -- The service it is, if it's one.
  service_id   text,
  name         text NOT NULL,
  engine       text NOT NULL,
  host         text NOT NULL,
  port         int NOT NULL,
  database     text NOT NULL DEFAULT '',
  password_sealed text NOT NULL,
  enabled      boolean NOT NULL DEFAULT true,
  schedule     text NOT NULL DEFAULT 'daily',
  hour         int NOT NULL DEFAULT 2,
  keep         int NOT NULL DEFAULT 14,
  created_at   timestamptz NOT NULL DEFAULT now(),
  -- A KV database's registration here, made and removed with it.
  database_id  text UNIQUE REFERENCES pyo.databases (id) ON DELETE CASCADE,
  -- The login dumps use (null: the read-only pyo_backup user a setup made).
  login        text,
  UNIQUE (team_id, name)
);

-- Each backup run of a volume or database.
CREATE TABLE pyo.backups (
  id          text PRIMARY KEY,
  team_id     text NOT NULL REFERENCES pyo.teams (id) ON DELETE CASCADE,
  cluster_id  text NOT NULL,
  -- Null for a volume that wasn't attached.
  service_id  text,
  -- The volume backed up (kind volume); a database is its service.
  volume_id   text,
  -- volume | database
  kind        text NOT NULL,
  -- queued | running | succeeded | failed
  status      text NOT NULL DEFAULT 'queued',
  -- schedule | manual
  trigger     text NOT NULL,
  label       text NOT NULL,
  snapshot_id text,
  size_bytes  bigint,
  added_bytes bigint,
  error       text,
  command_id  text,
  created_at  timestamptz NOT NULL DEFAULT now(),
  started_at  timestamptz,
  finished_at timestamptz,
  database_id text
);
CREATE INDEX backups_team_idx ON pyo.backups (team_id, created_at DESC);
CREATE INDEX backups_database_idx ON pyo.backups (database_id, created_at DESC);
