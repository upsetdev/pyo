-- Sandboxes' names (sandbox/names.rs), for people to tell them apart and
-- find them. Not unique: the id addresses a sandbox, so a rename breaks
-- nothing. Pyo names every sandbox it makes, with a short one like
-- quiet-river when none is given. NULL on sandboxes from before this:
-- they show the name made from their id, the same every time.
ALTER TABLE pyo.sandboxes ADD COLUMN name text;
