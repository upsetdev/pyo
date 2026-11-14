-- Git providers a team connects with an access token (git/mod.rs): GitLab,
-- on gitlab.com or self-managed. Services built from one of their
-- repositories name it in their source (`gitConnectionId`).
CREATE TABLE pyo.git_connections (
  id             text PRIMARY KEY,
  team_id        text NOT NULL REFERENCES pyo.teams (id) ON DELETE CASCADE,
  provider       text NOT NULL CHECK (provider IN ('gitlab')),
  -- The provider's address, like https://gitlab.com (no trailing slash).
  base_url       text NOT NULL,
  -- What the token belongs to: a group or project path, or a username.
  account        text NOT NULL DEFAULT '',
  -- The access token, sealed with the team's key.
  token          text NOT NULL,
  -- The secret the provider sends with each webhook, sealed the same way.
  webhook_secret text NOT NULL,
  -- Where Pyo is reached: webhooks go to {public_url}/api/git/{id}/webhook.
  public_url     text NOT NULL DEFAULT '',
  -- What the last check found: the token's user, scopes, expiry, version.
  info           jsonb NOT NULL DEFAULT '{}',
  created_at     timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX git_connections_team_idx ON pyo.git_connections (team_id);

-- The webhook Pyo made on each repository its services follow. No hook
-- (it couldn't be made, or Pyo has no address the provider reaches): the
-- branch is checked every minute instead.
CREATE TABLE pyo.git_webhooks (
  connection_id text NOT NULL REFERENCES pyo.git_connections (id) ON DELETE CASCADE,
  -- The repository's path, lowercased ("group/sub/project").
  repo          text NOT NULL,
  project_id    bigint,
  hook_id       bigint,
  url           text NOT NULL DEFAULT '',
  error         text,
  updated_at    timestamptz NOT NULL DEFAULT now(),
  PRIMARY KEY (connection_id, repo)
);
