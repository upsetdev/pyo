-- A database's live logs are followed too (logfollow.rs): a follow is a
-- service's or a database's.
ALTER TABLE pyo.log_follows ALTER COLUMN service_id DROP NOT NULL;
ALTER TABLE pyo.log_follows ADD COLUMN database_id text REFERENCES pyo.databases (id) ON DELETE CASCADE;
