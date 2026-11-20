-- `pyo login` (cli.rs): the CLI asks Pyo for a sign-in and polls for the
-- answer, so the browser never has to reach the CLI on localhost. The
-- terminal shows a short code that the person types on the dashboard to
-- approve it, then the CLI trades its verifier for an API key. Requests
-- last 10 minutes and work once. Only hashes of the code and verifier
-- are stored. Replaces pyo.cli_logins, which the release before this one
-- still uses: it goes in a later release.
CREATE TABLE pyo.cli_login_requests (
  -- In the approval page's link. Not a secret: the code and the verifier are.
  id          text PRIMARY KEY,
  -- sha256 of the verifier only the CLI knows.
  challenge   text NOT NULL,
  -- sha256 of the code the terminal shows.
  code_hash   text NOT NULL,
  -- The computer's name and the CLI's version and OS, as the CLI sent them.
  name        text NOT NULL,
  client      text NOT NULL,
  -- Days the key lasts (NULL: it never expires).
  key_days    integer,
  status      text NOT NULL DEFAULT 'pending' CHECK (status IN ('pending', 'approved', 'denied')),
  -- Who approved it.
  user_id     text REFERENCES pyo.users (id) ON DELETE CASCADE,
  -- Wrong codes typed. Five end the request.
  tries       integer NOT NULL DEFAULT 0,
  expires_at  timestamptz NOT NULL
);
