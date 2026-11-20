-- A hosted team whose plan paused or ended has commands held back
-- (gate.rs). When it was first held back since its last sync: once its
-- plan allows commands again, its clusters get their syncs sent once
-- more, and this goes back to NULL.
ALTER TABLE pyo.teams ADD COLUMN commands_held_at timestamptz;
