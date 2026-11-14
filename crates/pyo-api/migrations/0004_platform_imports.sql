-- Imports from other platforms (platform_import), each run on the server
-- whatever the browser or CLI does, with what became of each item. The
-- platform's token is never kept here.
CREATE TABLE pyo.platform_imports (
  id          text PRIMARY KEY,
  team_id     text NOT NULL REFERENCES pyo.teams (id) ON DELETE CASCADE,
  platform    text NOT NULL,
  -- 'reading' (the platform), 'importing', 'done' or 'failed'. One still
  -- 'reading' or 'importing' whose updated_at stops moving was cut off
  -- by a restart: it's shown as 'interrupted'.
  status      text NOT NULL DEFAULT 'reading',
  deploy      boolean NOT NULL DEFAULT false,
  -- What's being imported, once the platform is read: [{key, name, kind}].
  items       jsonb NOT NULL DEFAULT '[]',
  -- Each item's result as it finishes: [{key, name, state, message, id, projectId}].
  results     jsonb NOT NULL DEFAULT '[]',
  error       text NOT NULL DEFAULT '',
  created_by  text NOT NULL DEFAULT '',
  created_at  timestamptz NOT NULL DEFAULT now(),
  updated_at  timestamptz NOT NULL DEFAULT now(),
  finished_at timestamptz
);
CREATE INDEX platform_imports_team ON pyo.platform_imports (team_id, created_at DESC);
