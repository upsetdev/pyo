-- pyo:contract since=v0.2.5 the kind check only widens, so every row a release writes still fits, and the unique index is on a table this migration creates, which no release writes
-- A team's Hetzner Cloud token (Integrations › Hetzner), sealed with the
-- team's data key like its other credentials.
ALTER TABLE pyo.integrations DROP CONSTRAINT integrations_kind_check;
ALTER TABLE pyo.integrations ADD CONSTRAINT integrations_kind_check CHECK (kind IN ('tailscale', 'hetzner'));

-- Servers Pyo made at a cloud provider (cloud/mod.rs): followed while they
-- join their cluster, and deleted there when they're removed. Each carries
-- this installation's label at the provider too, so Pyo never deletes one
-- it didn't make.
CREATE TABLE pyo.cloud_servers (
  id            text PRIMARY KEY,
  team_id       text NOT NULL REFERENCES pyo.teams (id) ON DELETE CASCADE,
  provider      text NOT NULL CHECK (provider IN ('hetzner')),
  -- The cluster it joins, or the new cluster it is the first server of.
  cluster_id    text REFERENCES pyo.clusters (id) ON DELETE SET NULL,
  -- Its name in the cluster, on the tailnet and at the provider.
  name          text NOT NULL,
  -- It runs connect.sh: the cluster's first server (its control plane).
  first_server  boolean NOT NULL DEFAULT false,
  -- A build-only server.
  builder       boolean NOT NULL DEFAULT false,
  location      text NOT NULL,
  server_type   text NOT NULL,
  -- A month, before VAT, as the provider priced it when it was made.
  monthly       double precision NOT NULL,
  currency      text NOT NULL DEFAULT 'EUR',
  -- The provider's ID for it.
  remote_id     text NOT NULL,
  ipv4          text,
  state         text NOT NULL CHECK (state IN ('creating', 'booting', 'joining', 'ready', 'failed', 'removing', 'deleted')),
  -- Why it's stuck, as people read it.
  error         text,
  -- Tailnet Lock waits for its key: the command to run on a signing device.
  sign_command  text,
  created_by    text NOT NULL,
  created_at    timestamptz NOT NULL DEFAULT now(),
  updated_at    timestamptz NOT NULL DEFAULT now(),
  ready_at      timestamptz,
  deleted_at    timestamptz
);
CREATE INDEX cloud_servers_team ON pyo.cloud_servers (team_id);
CREATE UNIQUE INDEX cloud_servers_name ON pyo.cloud_servers (cluster_id, name) WHERE state <> 'deleted';

-- A server's single-use code (cloud/bootstrap.rs), the only secret in its
-- user data besides a single-use Tailscale key. Kept as its hash, bound to
-- the server, its cluster and its role. On its first boot the server trades
-- it over the tailnet for its join details, sealed here until then. The
-- first use burns it. Expired details go at once, the row a day later.
CREATE TABLE pyo.cloud_bootstrap (
  code_hash        text PRIMARY KEY,
  cloud_server_id  text NOT NULL REFERENCES pyo.cloud_servers (id) ON DELETE CASCADE,
  cluster_id       text NOT NULL REFERENCES pyo.clusters (id) ON DELETE CASCADE,
  -- join.sh, or connect.sh for a new cluster's first server.
  role             text NOT NULL CHECK (role IN ('join', 'connect')),
  material_sealed  text,
  expires_at       timestamptz NOT NULL,
  used_at          timestamptz,
  created_at       timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX cloud_bootstrap_server ON pyo.cloud_bootstrap (cloud_server_id);
