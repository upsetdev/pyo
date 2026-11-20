-- Keys of a cluster's other components (agent_auth.rs): its builders and
-- its pyo-static sign their requests too, with keys the agent makes in the
-- cluster and registers. 'agent' is the agent's own. A builder's key goes
-- with the builder.
ALTER TABLE pyo.agent_keys ADD COLUMN component text NOT NULL DEFAULT 'agent';
ALTER TABLE pyo.agent_keys ADD COLUMN builder_id text REFERENCES pyo.builders (id) ON DELETE CASCADE;
CREATE INDEX agent_keys_builder_idx ON pyo.agent_keys (builder_id) WHERE builder_id IS NOT NULL;
