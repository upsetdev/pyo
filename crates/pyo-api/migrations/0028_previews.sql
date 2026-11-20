-- Preview environments (previews/): each open pull request (GitHub) or
-- merge request (GitLab) gets a copy of the services that turned previews
-- on, built from its head and removed when it closes.

-- A preview's copy is a service of its own, next to the one it previews
-- (same project and cluster, its own name): deploys, logs and routes work
-- as for any service. Gone with it.
ALTER TABLE pyo.services ADD COLUMN preview_of text REFERENCES pyo.services (id) ON DELETE CASCADE;
CREATE INDEX services_preview_of_idx ON pyo.services (preview_of) WHERE preview_of IS NOT NULL;

-- A cluster's preview domain: {"domain", "certificateId", "certificate":
-- {"state", "error", "at"}, "record": {"id", "content", "state"}}.
-- Previews there get "<name>.<domain>" (a wildcard record points at it).
ALTER TABLE pyo.clusters ADD COLUMN preview_domain jsonb;

-- What an installation of the GitHub App may do, as GitHub last said:
-- previews need pull requests, deployments and checks.
ALTER TABLE pyo.github_installations ADD COLUMN permissions jsonb;
ALTER TABLE pyo.github_installations ADD COLUMN events text[];
ALTER TABLE pyo.github_installations ADD COLUMN checked_at timestamptz;

-- A GitLab webhook that sends merge request events too (made before
-- previews, it only sent pushes, and is made again).
ALTER TABLE pyo.git_webhooks ADD COLUMN merge_requests boolean NOT NULL DEFAULT false;

-- Pull requests and merge requests of repositories whose services have
-- previews on, as Pyo last saw them, with what the bot left on them.
CREATE TABLE pyo.pull_requests (
  id           text PRIMARY KEY,
  team_id      text NOT NULL REFERENCES pyo.teams (id) ON DELETE CASCADE,
  provider     text NOT NULL CHECK (provider IN ('github', 'gitlab')),
  -- Whose: the GitHub installation, or the team's GitLab connection.
  installation_id bigint,
  connection_id   text REFERENCES pyo.git_connections (id) ON DELETE CASCADE,
  -- The installation's id or the connection's: one row per PR of each.
  source       text NOT NULL,
  -- "owner/repo" or "group/sub/project", lowercased.
  repo         text NOT NULL,
  number       bigint NOT NULL,
  title        text NOT NULL DEFAULT '',
  url          text NOT NULL DEFAULT '',
  author       text NOT NULL DEFAULT '',
  head_ref     text NOT NULL DEFAULT '',
  head_sha     text NOT NULL DEFAULT '',
  base_ref     text NOT NULL DEFAULT '',
  -- From another repository (a fork).
  fork         boolean NOT NULL DEFAULT false,
  labels       text[] NOT NULL DEFAULT '{}',
  state        text NOT NULL DEFAULT 'open' CHECK (state IN ('open', 'closed', 'merged')),
  -- The last push (a new head): previews idle since are removed.
  pushed_at    timestamptz NOT NULL DEFAULT now(),
  -- The bot's comment (GitHub) or note (GitLab), and a hash of its text.
  comment_id   bigint,
  comment_hash text,
  -- The check run (GitHub) or commit status (GitLab) on the head.
  check_id     bigint,
  check_sha    text,
  check_hash   text,
  created_at   timestamptz NOT NULL DEFAULT now(),
  updated_at   timestamptz NOT NULL DEFAULT now(),
  closed_at    timestamptz,
  UNIQUE (team_id, provider, source, repo, number)
);
CREATE INDEX pull_requests_open_idx ON pyo.pull_requests (state) WHERE state = 'open';

-- A service's preview for a pull request.
CREATE TABLE pyo.previews (
  id                 text PRIMARY KEY,
  team_id            text NOT NULL REFERENCES pyo.teams (id) ON DELETE CASCADE,
  pull_id            text NOT NULL REFERENCES pyo.pull_requests (id) ON DELETE CASCADE,
  -- The service previewed, and its copy while there is one.
  service_id         text NOT NULL REFERENCES pyo.services (id) ON DELETE CASCADE,
  preview_service_id text REFERENCES pyo.services (id) ON DELETE SET NULL,
  -- The copy's name ("web-pr-42") and URL host: kept for the PR's life.
  name               text NOT NULL,
  hostname           text,
  -- waiting (over the limit) | active (has a copy) | failed (couldn't
  -- start one) | removed
  state              text NOT NULL CHECK (state IN ('waiting', 'active', 'failed', 'removed')),
  -- Why it waits, failed or was removed, as people read it.
  message            text,
  -- closed | merged | idle | label | settings | user | fork | limit
  removed_reason     text,
  -- The commit last deployed (or tried), and its deployment.
  sha                text NOT NULL DEFAULT '',
  deployment_id      text,
  -- GitHub's deployment for `sha` ("View deployment"), and what was last
  -- reported on it; GitLab's environment and deployment.
  remote_deployment_id  bigint,
  remote_deployment_sha text,
  remote_environment_id bigint,
  reported           text,
  created_at         timestamptz NOT NULL DEFAULT now(),
  updated_at         timestamptz NOT NULL DEFAULT now(),
  UNIQUE (pull_id, service_id)
);
CREATE INDEX previews_service_idx ON pyo.previews (service_id);
CREATE INDEX previews_copy_idx ON pyo.previews (preview_service_id) WHERE preview_service_id IS NOT NULL;

-- Row security (rls.rs), as for the other team tables.
ALTER TABLE pyo.pull_requests ENABLE ROW LEVEL SECURITY;
CREATE POLICY team_rows ON pyo.pull_requests TO pyo_team
  USING (team_id = ANY (pyo.team_scope()))
  WITH CHECK (team_id = ANY (pyo.team_scope()));
ALTER TABLE pyo.previews ENABLE ROW LEVEL SECURITY;
CREATE POLICY team_rows ON pyo.previews TO pyo_team
  USING (team_id = ANY (pyo.team_scope()))
  WITH CHECK (team_id = ANY (pyo.team_scope()));
