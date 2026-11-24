-- Why the agent couldn't delete a service, as the team reads it (ops.rs
-- `service_delete_finished`). Shown on the service until it's deleted
-- again or deployed. NULL when there's nothing to say.
ALTER TABLE pyo.services ADD COLUMN delete_error text;
