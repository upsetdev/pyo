-- An agent's socket (a terminal's or a log follow's) that lands on another
-- web replica than the one holding the other end is forwarded there
-- (terminal.rs `hand_over`). The replica that checked the agent leaves a
-- ticket here, good once for a minute, and the other one takes it. Only
-- Pyo itself reads them: no team policy.
CREATE TABLE pyo.relay_tickets (
  -- SHA-256 of the ticket, never the ticket.
  hash       text PRIMARY KEY,
  session_id text NOT NULL,
  created_at timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX relay_tickets_created_idx ON pyo.relay_tickets (created_at);
ALTER TABLE pyo.relay_tickets ENABLE ROW LEVEL SECURITY;
