-- A dump can also go up in parts (database_imports.rs `upload_part`), so
-- no request is bigger than a proxy in front takes (Cloudflare's 100 MB).
-- Each part goes on to the store's multipart upload as it arrives, in
-- order. What came so far is kept here, so any web replica takes the next.
ALTER TABLE pyo.database_import_uploads
  -- Parts received so far: the next one expected is this.
  ADD COLUMN parts_received integer NOT NULL DEFAULT 0,
  -- The store's ETag of each part, in order.
  ADD COLUMN etags jsonb NOT NULL DEFAULT '[]',
  -- The SHA-256 of the parts so far, as a hasher's saved state.
  ADD COLUMN hash_state bytea,
  -- The last part's SHA-256 (hex), to tell a part sent again from another.
  ADD COLUMN last_part_sha256 text NOT NULL DEFAULT '';
