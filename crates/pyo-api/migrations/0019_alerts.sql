-- Alert channels (alerts.rs): where a team's alerts go, a Telegram chat,
-- a Discord or Slack webhook, or a webhook of its own. Where it sends (a
-- bot token, a webhook URL with its token in it, a signing secret) is
-- sealed with the team's key; `hint` is what the dashboard shows of it.
CREATE TABLE pyo.alert_channels (
  id            text PRIMARY KEY,
  team_id       text NOT NULL REFERENCES pyo.teams (id) ON DELETE CASCADE,
  kind          text NOT NULL CHECK (kind IN ('telegram', 'discord', 'slack', 'webhook')),
  name          text NOT NULL,
  target_sealed text NOT NULL,
  hint          text NOT NULL DEFAULT '',
  -- Events it doesn't get. Everything else it does, events added later too.
  muted         text[] NOT NULL DEFAULT '{}',
  -- How the last send went.
  last_sent_at  timestamptz,
  last_error    text,
  last_error_at timestamptz,
  created_at    timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX alert_channels_team_idx ON pyo.alert_channels (team_id);

-- The last alert sent about each subject ("server:<cluster>/<name>"), so
-- one goes per subject per window. `held`: one kept back in the window,
-- sent once it ends unless something ended it first.
CREATE TABLE pyo.alert_state (
  team_id text NOT NULL REFERENCES pyo.teams (id) ON DELETE CASCADE,
  key     text NOT NULL,
  sent_at timestamptz NOT NULL DEFAULT now(),
  event   text NOT NULL,
  held    jsonb,
  PRIMARY KEY (team_id, key)
);
CREATE INDEX alert_state_held_idx ON pyo.alert_state (sent_at) WHERE held IS NOT NULL;
