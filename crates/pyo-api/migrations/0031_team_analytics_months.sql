-- A team may keep its analytics shorter than the installation does
-- (retention.rs): 3, 6 or 12 months. Null keeps them as long as the
-- installation (Settings › Observability). Once a day the rows older than
-- a team's months are removed from the analytics tables.
ALTER TABLE pyo.teams ADD COLUMN analytics_months smallint CHECK (analytics_months IN (3, 6, 12));
