-- Folders sent with `pyo deploy` (source_uploads.rs): the CLI sends a
-- gzipped tar in parts, Pyo keeps them here until a builder in the
-- service's cluster has fetched the archive, then deletes them. In Pyo's
-- own database so every web replica (and Pyo Cloud) sees them. Never kept
-- longer than a few hours.
CREATE TABLE pyo.source_uploads (
  id            text PRIMARY KEY,
  team_id       text NOT NULL REFERENCES pyo.teams (id) ON DELETE CASCADE,
  -- The archive's size in bytes and hex SHA-256, as the CLI said.
  size          bigint NOT NULL CHECK (size > 0),
  sha256        text NOT NULL,
  -- What it holds and where it came from, for the deployment's page.
  files         integer NOT NULL DEFAULT 0,
  folder        text NOT NULL DEFAULT '',
  machine       text NOT NULL DEFAULT '',
  created_by    text NOT NULL,
  -- uploading: parts arriving. ready: every part here and checked.
  -- used: a deployment builds it (deployment_id).
  state         text NOT NULL DEFAULT 'uploading' CHECK (state IN ('uploading', 'ready', 'used')),
  deployment_id text,
  created_at    timestamptz NOT NULL DEFAULT now(),
  expires_at    timestamptz NOT NULL
);
CREATE INDEX source_uploads_team_idx ON pyo.source_uploads (team_id);
CREATE INDEX source_uploads_deployment_idx ON pyo.source_uploads (deployment_id) WHERE deployment_id IS NOT NULL;

-- The archive, in order (n from 0), each part the same size but the last.
CREATE TABLE pyo.source_upload_parts (
  upload_id text NOT NULL REFERENCES pyo.source_uploads (id) ON DELETE CASCADE,
  n         integer NOT NULL CHECK (n >= 0),
  data      bytea NOT NULL,
  PRIMARY KEY (upload_id, n)
);
-- Large and short-lived: no compression attempt on the gzip inside.
ALTER TABLE pyo.source_upload_parts ALTER COLUMN data SET STORAGE EXTERNAL;

-- Row security (rls.rs), as for the other team tables.
ALTER TABLE pyo.source_uploads ENABLE ROW LEVEL SECURITY;
CREATE POLICY team_rows ON pyo.source_uploads TO pyo_team
  USING (team_id = ANY (pyo.team_scope()))
  WITH CHECK (team_id = ANY (pyo.team_scope()));
