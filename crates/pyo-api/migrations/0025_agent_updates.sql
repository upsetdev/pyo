-- A team's agent updates on a hosted Pyo (updates.rs `hosted_agents`):
-- 'auto' moves its clusters' agents to Pyo's version on their own,
-- 'manual' leaves it to the team, from each cluster's page.
ALTER TABLE pyo.teams ADD COLUMN agent_updates text NOT NULL DEFAULT 'auto' CHECK (agent_updates IN ('auto', 'manual'));
-- Each cluster's agent updates, newest first (the cluster's page, and
-- which clusters are due).
CREATE INDEX commands_update_agent_idx ON pyo.commands (cluster_id, created_at DESC) WHERE command->>'kind' = 'update_agent';
