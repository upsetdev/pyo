-- Team security (twofactor.rs, audit.rs): two-factor authentication with
-- an authenticator app, teams that require it, and the audit log.

-- An account's authenticator app. The secret is sealed with the
-- installation's key: it belongs to the account, not to a team. While
-- `enabled_at` is NULL it's being set up and isn't asked for yet.
CREATE TABLE pyo.user_totp (
  user_id       text PRIMARY KEY REFERENCES pyo.users (id) ON DELETE CASCADE,
  secret_sealed text NOT NULL,
  enabled_at    timestamptz,
  -- The last time step a code was accepted for: each code works once.
  last_step     bigint NOT NULL DEFAULT 0,
  created_at    timestamptz NOT NULL DEFAULT now()
);

-- One-time recovery codes, for when the app is lost. Only their sha256 is
-- kept, and a used one stays marked so it can't work again.
CREATE TABLE pyo.recovery_codes (
  user_id    text NOT NULL REFERENCES pyo.users (id) ON DELETE CASCADE,
  code_hash  text NOT NULL,
  used_at    timestamptz,
  created_at timestamptz NOT NULL DEFAULT now(),
  PRIMARY KEY (user_id, code_hash)
);

-- A sign-in that got the password (or a sign-in link) right and waits for
-- the code. Single use, a few minutes long, a few tries.
CREATE TABLE pyo.sign_in_challenges (
  token_hash text PRIMARY KEY,
  user_id    text NOT NULL REFERENCES pyo.users (id) ON DELETE CASCADE,
  -- 'password' or 'link': how the first step was made.
  method     text NOT NULL,
  attempts   integer NOT NULL DEFAULT 0,
  expires_at timestamptz NOT NULL
);
CREATE INDEX sign_in_challenges_user_idx ON pyo.sign_in_challenges (user_id);

-- A sign-in link made on the server Pyo runs on (pyo-reset-login) also
-- turns off two-factor: whoever has the server has everything anyway.
ALTER TABLE pyo.sign_in_links ADD COLUMN clears_two_factor boolean NOT NULL DEFAULT false;

-- A team can require two-factor for its members, and chooses what its
-- audit log keeps of addresses: 'full', 'anonymized' or 'none'.
ALTER TABLE pyo.teams ADD COLUMN require_two_factor boolean NOT NULL DEFAULT false;
ALTER TABLE pyo.teams ADD COLUMN audit_ips text NOT NULL DEFAULT 'full'
  CHECK (audit_ips IN ('full', 'anonymized', 'none'));

-- Who did what in a team, when and from where. Written with the team's
-- activity and for security events (sign-ins, two-factor, API keys).
-- Kept a year on Pyo Cloud, as the installation's setting says when
-- self-hosted (pyo.settings 'security').
CREATE TABLE pyo.audit_events (
  id          text PRIMARY KEY,
  team_id     text NOT NULL REFERENCES pyo.teams (id) ON DELETE CASCADE,
  at          timestamptz NOT NULL DEFAULT now(),
  -- 'auth.sign_in', 'api_key.created', 'service.deployed', ...
  action      text NOT NULL,
  title       text NOT NULL,
  -- The account, kept as it was when the account is gone.
  actor_id    text,
  actor_name  text NOT NULL,
  actor_email text NOT NULL DEFAULT '',
  -- How: 'dashboard', 'api' (an API key), 'mcp' (an assistant) or 'system'.
  via         text NOT NULL CHECK (via IN ('dashboard', 'api', 'mcp', 'system')),
  -- The API key's or assistant's name.
  key_name    text,
  -- As the team's audit_ips allowed when it was written.
  ip          text,
  user_agent  text NOT NULL DEFAULT '',
  target      jsonb NOT NULL DEFAULT '{}',
  -- 'ok', or 'failed' (a wrong password or code).
  outcome     text NOT NULL DEFAULT 'ok' CHECK (outcome IN ('ok', 'failed'))
);
CREATE INDEX audit_events_team_idx ON pyo.audit_events (team_id, at DESC);
CREATE INDEX audit_events_actor_idx ON pyo.audit_events (team_id, actor_id, at DESC);
CREATE INDEX audit_events_at_idx ON pyo.audit_events (at);

-- Row security (rls.rs): a team's request sees and adds its own team's
-- entries, and never changes or removes one. Pruning runs as the owner.
ALTER TABLE pyo.audit_events ENABLE ROW LEVEL SECURITY;
CREATE POLICY team_read ON pyo.audit_events FOR SELECT TO pyo_team
  USING (team_id = ANY (pyo.team_scope()));
CREATE POLICY team_write ON pyo.audit_events FOR INSERT TO pyo_team
  WITH CHECK (team_id = ANY (pyo.team_scope()));

-- Whether an account still has to set up two-factor before it may use
-- anything but its own profile: the installation requires it of everyone
-- (pyo.settings 'security'), or of Pyo Cloud's staff (its admins, when
-- `p_hosted`).
CREATE FUNCTION pyo.needs_two_factor(p_user text, p_hosted boolean) RETURNS boolean
  LANGUAGE sql STABLE SECURITY DEFINER SET search_path = pg_catalog, pyo AS $$
  SELECT NOT EXISTS (SELECT 1 FROM pyo.user_totp t WHERE t.user_id = p_user AND t.enabled_at IS NOT NULL)
     AND ((p_hosted AND coalesce((SELECT u.is_admin FROM pyo.users u WHERE u.id = p_user), false))
          OR coalesce((SELECT (s.value->>'requireTwoFactor')::boolean FROM pyo.settings s WHERE s.key = 'security'), false))
$$;
REVOKE ALL ON FUNCTION pyo.needs_two_factor(text, boolean) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION pyo.needs_two_factor(text, boolean) TO pyo_team;

-- The teams a person (or an API key limited to some teams) may see: as
-- before, without the teams that require two-factor while the account
-- has none, and without any while the account must set it up first.
CREATE FUNCTION pyo.member_team_ids(p_user text, p_key_teams text[], p_hosted boolean) RETURNS text
  LANGUAGE sql STABLE SECURITY DEFINER SET search_path = pg_catalog, pyo AS $$
  SELECT coalesce(string_agg(m.team_id, ','), '')
    FROM pyo.members m JOIN pyo.teams t ON t.id = m.team_id
   WHERE m.user_id = p_user AND m.status = 'active'
     AND (p_key_teams IS NULL OR m.team_id = ANY (p_key_teams))
     AND NOT pyo.needs_two_factor(p_user, p_hosted)
     AND (NOT t.require_two_factor
          OR EXISTS (SELECT 1 FROM pyo.user_totp u WHERE u.user_id = p_user AND u.enabled_at IS NOT NULL))
$$;
REVOKE ALL ON FUNCTION pyo.member_team_ids(text, text[], boolean) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION pyo.member_team_ids(text, text[], boolean) TO pyo_team;

-- The release before this one asks with two arguments while it still
-- serves: it gets the same answer (staff are a Pyo Cloud matter).
CREATE OR REPLACE FUNCTION pyo.member_team_ids(p_user text, p_key_teams text[]) RETURNS text
  LANGUAGE sql STABLE SECURITY DEFINER SET search_path = pg_catalog, pyo AS $$
  SELECT pyo.member_team_ids(p_user, p_key_teams, false)
$$;
