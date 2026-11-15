-- pyo:contract since=v0.3.2 the unique indexes are on tables this migration creates, which no release writes, and api_tokens only loses its access check, so every row a release writes still fits
-- Pyo Sandbox (sandbox/): isolated machines made on demand on dedicated
-- sandbox servers. Pyo keeps the records, the keys, the limits and the
-- usage. The servers run the sandboxes.

-- API keys may also have the "sandboxes" access, checked in code
-- (auth.rs KeyAccess). A release before this one reads it as read-only.
ALTER TABLE pyo.api_tokens DROP CONSTRAINT IF EXISTS api_tokens_access_check;

-- The installation's Ed25519 key that signs sandbox tokens
-- (pyo_proto::sandbox::token). Made on first use. Its seed is sealed with
-- PYO_SECRET_KEY like Pyo's other credentials (secrets.rs). Sandbox servers
-- hold only the public key, so a server that runs untrusted code can't make
-- tokens. A retired key still checks tokens until they run out.
CREATE TABLE pyo.sandbox_keys (
  kid          text PRIMARY KEY,
  seed_sealed  text NOT NULL,
  -- 32 bytes, base64url, as daemons have it in their config.
  public_key   text NOT NULL,
  created_at   timestamptz NOT NULL DEFAULT now(),
  retired_at   timestamptz
);

-- Servers that run sandboxes, outside clusters. A team's own, or the
-- installation's (team_id NULL), which every team may use.
CREATE TABLE pyo.sandbox_servers (
  id              text PRIMARY KEY,
  team_id         text REFERENCES pyo.teams (id) ON DELETE CASCADE,
  name            text NOT NULL,
  -- Its tailnet IP, once it joined. The daemon answers on :7070 there.
  address         text,
  -- joining: waiting for sandbox.sh to redeem its code. ready: reports
  -- come in. unreachable: no report for a while. refused: it can run
  -- neither Firecracker nor gVisor (error says why).
  state           text NOT NULL CHECK (state IN ('joining', 'ready', 'unreachable', 'refused')),
  -- An admin turned sandboxes off on it: nothing new is placed there.
  enabled         boolean NOT NULL DEFAULT true,
  -- The daemon's CapabilityReport, CapacityReport and templates, from
  -- its latest report.
  capability      jsonb,
  capacity        jsonb,
  templates       jsonb NOT NULL DEFAULT '[]',
  daemon_version  text,
  last_report_at  timestamptz,
  error           text,
  created_by      text NOT NULL,
  created_at      timestamptz NOT NULL DEFAULT now(),
  updated_at      timestamptz NOT NULL DEFAULT now(),
  joined_at       timestamptz
);
CREATE UNIQUE INDEX sandbox_servers_name ON pyo.sandbox_servers (coalesce(team_id, ''), name);

-- A sandbox server's one-time join code (sandbox/join.rs), like a cloud
-- server's bootstrap code: kept as its hash, bound to the server, valid 15
-- minutes and burnt on first use. sandbox.sh redeems it over the tailnet
-- for the daemon's config.
CREATE TABLE pyo.sandbox_join_codes (
  code_hash   text PRIMARY KEY,
  server_id   text NOT NULL REFERENCES pyo.sandbox_servers (id) ON DELETE CASCADE,
  expires_at  timestamptz NOT NULL,
  used_at     timestamptz,
  created_at  timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX sandbox_join_codes_server ON pyo.sandbox_join_codes (server_id);

-- Sandbox templates: what a sandbox starts from. A team's own, or the
-- installation's (team_id NULL). Servers keep them under their id.
CREATE TABLE pyo.sandbox_templates (
  id          text PRIMARY KEY,
  team_id     text REFERENCES pyo.teams (id) ON DELETE CASCADE,
  name        text NOT NULL,
  -- {"kind": "image", "image": "node:22"}
  source      jsonb NOT NULL,
  -- pending: waiting for a build. ready: on the servers. failed: error says why.
  state       text NOT NULL CHECK (state IN ('pending', 'building', 'ready', 'failed')),
  -- Per runtime: {"firecracker": "ready", "gvisor": "pending"}.
  snapshots   jsonb NOT NULL DEFAULT '{}',
  error       text,
  created_by  text NOT NULL,
  created_at  timestamptz NOT NULL DEFAULT now(),
  updated_at  timestamptz NOT NULL DEFAULT now()
);
CREATE UNIQUE INDEX sandbox_templates_name ON pyo.sandbox_templates (coalesce(team_id, ''), name);

-- Sandboxes. They belong to the team. project_id is only a label.
CREATE TABLE pyo.sandboxes (
  id           text PRIMARY KEY,
  team_id      text NOT NULL REFERENCES pyo.teams (id) ON DELETE CASCADE,
  project_id   text REFERENCES pyo.projects (id) ON DELETE SET NULL,
  server_id    text REFERENCES pyo.sandbox_servers (id) ON DELETE SET NULL,
  template_id  text REFERENCES pyo.sandbox_templates (id) ON DELETE SET NULL,
  -- The SandboxSpec sent to the server.
  spec         jsonb NOT NULL,
  state        text NOT NULL CHECK (state IN ('creating', 'running', 'pausing', 'paused', 'resuming', 'stopping', 'stopped', 'failed')),
  -- firecracker or gvisor, once it runs.
  runtime      text,
  error        text,
  created_by   text NOT NULL,
  created_at   timestamptz NOT NULL DEFAULT now(),
  started_at   timestamptz,
  expires_at   timestamptz NOT NULL,
  paused_at    timestamptz,
  ended_at     timestamptz
);
CREATE INDEX sandboxes_team ON pyo.sandboxes (team_id, created_at DESC);
CREATE INDEX sandboxes_live ON pyo.sandboxes (server_id) WHERE state NOT IN ('stopped', 'failed');
CREATE INDEX sandboxes_expiry ON pyo.sandboxes (expires_at) WHERE state NOT IN ('stopped', 'failed');

-- A team's sandbox limits, set by the installation's admin. No row: the
-- defaults in sandbox/limits.rs.
CREATE TABLE pyo.sandbox_limits (
  team_id         text PRIMARY KEY REFERENCES pyo.teams (id) ON DELETE CASCADE,
  max_running     integer NOT NULL,
  max_cpus        integer NOT NULL,
  max_memory_mib  integer NOT NULL,
  max_disk_mib    integer NOT NULL,
  max_ttl_secs    integer NOT NULL,
  updated_at      timestamptz NOT NULL DEFAULT now()
);

-- Usage per sandbox and minute, from the servers' reports: seconds it ran
-- in that minute, at its size. Later to ClickHouse and Pyo Cloud billing.
CREATE TABLE pyo.sandbox_usage (
  sandbox_id  text NOT NULL,
  minute      timestamptz NOT NULL,
  team_id     text NOT NULL REFERENCES pyo.teams (id) ON DELETE CASCADE,
  server_id   text,
  cpus        integer NOT NULL,
  memory_mib  integer NOT NULL,
  seconds     integer NOT NULL CHECK (seconds BETWEEN 0 AND 60),
  PRIMARY KEY (sandbox_id, minute)
);
CREATE INDEX sandbox_usage_team ON pyo.sandbox_usage (team_id, minute);
