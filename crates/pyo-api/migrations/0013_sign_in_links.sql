-- One-time sign-in links (recovery.rs): a lost password is replaced
-- through one. They come from the host's `pyo-reset-login`, a "Forgot
-- password?" email, or a team owner's "Reset sign-in". Only the token's
-- sha256 is kept. A link works once, until it expires, and a new one for
-- the same account replaces it.
CREATE TABLE pyo.sign_in_links (
  token_hash  text PRIMARY KEY,
  user_id     text NOT NULL REFERENCES pyo.users (id) ON DELETE CASCADE,
  created_at  timestamptz NOT NULL DEFAULT now(),
  expires_at  timestamptz NOT NULL
);
CREATE INDEX sign_in_links_user ON pyo.sign_in_links (user_id);
