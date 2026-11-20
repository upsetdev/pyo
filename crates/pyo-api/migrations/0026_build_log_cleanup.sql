-- Build and deploy logs are kept 90 days (jobs.rs `prune`): the oldest
-- lines, found by their time.
CREATE INDEX deployment_logs_at_idx ON pyo.deployment_logs (at);
