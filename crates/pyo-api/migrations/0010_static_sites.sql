-- Pyo Static (sites.rs): the folders static builds uploaded to the team's
-- bucket, `sites/<service_id>/<deployment_id>/`, one per deployment. A
-- deployment serves its folder while this row is here and not removed;
-- pyo-static reads only these. Kept for the newest deployments of each
-- service (rollbacks), removed from the bucket when pruned or when the
-- service is gone.
CREATE TABLE pyo.static_sites (
  deployment_id  text PRIMARY KEY,
  team_id        text NOT NULL REFERENCES pyo.teams (id) ON DELETE CASCADE,
  service_id     text NOT NULL,
  cluster_id     text NOT NULL,
  -- {files: [{path, size, sha256}], indexFallback}
  manifest       jsonb NOT NULL,
  files          integer NOT NULL,
  bytes          bigint NOT NULL,
  -- Set once its files are uploaded; a deploy serves only a complete folder.
  complete       boolean NOT NULL DEFAULT false,
  created_at     timestamptz NOT NULL DEFAULT now(),
  -- Pruned (or its service deleted): its files are being removed.
  removed_at     timestamptz
);
CREATE INDEX static_sites_service_idx ON pyo.static_sites (service_id, created_at DESC);
