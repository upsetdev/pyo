-- Following a service's logs (logfollow.rs): one signed command opens a
-- follow, and the cluster's agent dials back to send new lines. A row
-- says which API replica holds the reader's socket. Rows go after a day.
CREATE TABLE pyo.log_follows (
  id         text PRIMARY KEY,
  team_id    text NOT NULL REFERENCES pyo.teams (id) ON DELETE CASCADE,
  service_id text NOT NULL REFERENCES pyo.services (id) ON DELETE CASCADE,
  -- host:port of the API replica the reader is connected to.
  relay      text,
  created_at timestamptz NOT NULL DEFAULT now(),
  ended_at   timestamptz
);
CREATE INDEX log_follows_created_idx ON pyo.log_follows (created_at);

-- Row security (rls.rs): a team's request sees and adds its own rows.
ALTER TABLE pyo.log_follows ENABLE ROW LEVEL SECURITY;
CREATE POLICY team_rows ON pyo.log_follows TO pyo_team
  USING (team_id = ANY (pyo.team_scope()))
  WITH CHECK (team_id = ANY (pyo.team_scope()));
