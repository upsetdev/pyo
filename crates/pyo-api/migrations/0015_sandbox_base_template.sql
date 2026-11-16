-- Pyo's own sandbox templates (the built-in `base`, sandbox/base.rs): Pyo
-- keeps them in step with its release, and nobody deletes them. A
-- revision names the image a template is made from now: servers keep it
-- as `<id>.<revision>`, so a new image is made beside the old one, which
-- running and paused sandboxes keep using. NULL: the template's id alone.
ALTER TABLE pyo.sandbox_templates ADD COLUMN builtin boolean NOT NULL DEFAULT false;
ALTER TABLE pyo.sandbox_templates ADD COLUMN revision text;
