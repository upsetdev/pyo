-- The images a cluster's databases run, as its databases system was last
-- applied (pyo-proto's `database_images`): compared with a newer one's, so
-- Pyo knows which databases restart for an update. NULL until the next one
-- is applied.
ALTER TABLE pyo.cluster_bundles ADD COLUMN images jsonb;
