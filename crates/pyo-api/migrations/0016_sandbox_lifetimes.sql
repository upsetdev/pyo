-- Sandboxes may live up to a year, or run until they're deleted. A team's
-- longest lifetime NULL: no limit. A sandbox's expires_at NULL: it never
-- ends on its own.
ALTER TABLE pyo.sandbox_limits ALTER COLUMN max_ttl_secs DROP NOT NULL;
ALTER TABLE pyo.sandboxes ALTER COLUMN expires_at DROP NOT NULL;
