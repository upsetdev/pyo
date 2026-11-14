-- Where an imported database came from: {platform, host, id, project}, as
-- services keep in spec.source.importedFrom (platform_import). A second
-- import finds it by this, not by its name. NULL: not imported.
ALTER TABLE pyo.databases ADD COLUMN imported_from jsonb;
