# Cloudflare Tunnel

Internal spec for a built-in Cloudflare Tunnel in Pyo, target release
**0.51.0**. Websites come in through Cloudflare with no open ports on the
servers. Pyo makes the tunnel, keeps its routes and DNS records in step with
the cluster's domains, issues the certificate Cloudflare checks, and runs the
connector (cloudflared) in the cluster like Envoy and Vector. The last
section is a draft for the pyo-docs page.

Written 2026-11-22 against `dev` at b2f92aa. File and line references are to
that commit. Pyo Cloud references are to pyo-cloud at 94956c8.

## Contents

1. Goals and non-goals
2. How it fits together
3. User flows (dashboard, CLI, API, MCP)
4. Data model and migration
5. API
6. Control plane: the reconcile loop and Cloudflare calls
7. Agent: the connector and its manifests
8. Pyo Gateway changes
9. Certificates
10. Health and dashboard states
11. Edge cases and limits
12. Failure modes and rollback
13. Security
14. Pyo Cloud
15. connect.sh
16. Docs pages
17. Test plan
18. Build plan (phases, file:line)
19. Open questions (answered)
20. Draft docs page
21. Build status

---

## Decisions (2026-11-22)

The user accepted these answers to the open questions (§19). The sections
below are updated to match.

1. **Origin certificate key**: made by the control plane (rcgen) and kept
   sealed with the team's certificates.
2. **Origin CA validity**: 15 years (5475 days, Cloudflare's longest). No
   renewal loop. Pyo still revokes and issues a new one on demand: when a
   new name isn't covered, on **Rotate** (new tunnel token and new
   certificate), and on removal.
3. **TCP and UDP ports in tunnel mode**: published on the servers by
   default, since the user asked for them per service. The Traffic panel
   says they don't go through the tunnel.
4. **Default domain**: optional. Only a zone apex, as `*.example.com`. A
   cluster can be in tunnel mode with only domains switched by hand (Pyo
   Ops needs that). Without a default domain there are no free domains in
   tunnel mode.
5. **Removing a domain** that replaced a record: the old record isn't put
   back by default. The remove dialog offers "Put back the old record"
   (off by default).
6. **One Cloudflare account per tunnel**: yes.
7. **Pyo's own dashboard through the tunnel**: not in 0.51.0.
8. **Pyo Ops**: cloud.pyo.dev and agent.pyo.dev move onto it right after
   0.51.0, with no default domain. Planned in §14, not done as part of
   this work.
9. **HTTPS redirect**: at the Gateway, from `cf-visitor`.
10. **Who may switch a domain**: developers when nothing is in the way,
    owners and admins when it replaces an existing record.
11. **CF-Connecting-IP fix**: shipped in 0.5.5 (§8.4). Done.
12. **Release number**: 0.51.0.
13. **Large uploads**: done in 0.5.x (database dumps upload in parts,
    static site files up to 90 MiB). Done.

Decisions made while building (not in the first draft):

- **What moves when a cluster switches to tunnel mode.** Pyo asks for a
  switch of every domain of the cluster in a tunnel-ready zone of the
  tunnel's account. A domain with nothing in the way (no record, or only
  Pyo's own) switches. A domain with someone else's record stops at
  "confirm" and waits for **Replace and switch**. So records made by hand
  or by Terraform (Pyo Ops) are never replaced without a yes. Pyo's own
  direct record isn't kept in `previous`: Switch back makes a new one.
- **Both paths only while a domain switches** (user, 2026-11-22). A
  domain's `tunnel` is `"switching"` or `"on"`:
  - `"switching"`: the Gateway has the tunnel listener and route, the
    tunnel route has no 403 gate, and the domain keeps its plain-HTTP
    :80 route. Direct visitors and the tunnel both work, so there's no
    gap on a cluster with public ports while the record moves.
  - `"on"` (the record points at the tunnel, switch complete): the
    firewall's "behind" gate (`source.port != 0`) goes ahead of the
    tunnel route's rules, so a direct, non-tunnel connection with that
    hostname gets 403, and the :80 route is removed.
  - Switch back goes through `"switching"` again before the record
    moves back, so direct visitors are let in before DNS sends them.
  - A new connector pod whose address isn't trusted yet is refused by
    the gate (403) for those few seconds, never let through with the
    wrong visitor.
- **A behind-Cloudflare service with tunnel domains**: its CDN client
  policy covers only its direct HTTPS listeners. The tunnel policy covers
  the tunnel ones (Envoy Gateway allows one section policy per listener).
  The behind gate stays on its direct routes only.
- **Clusters on an older install.** Tunnel mode changes the agent's
  admission policy (the connector's Deployment, Secret and PDB in
  pyo-system, the `pyo-tunnel-origin` and `pyo-public-ports` Services, the
  `tunnel` client policy label). Since 1cebb7a the policy counts toward
  `agentAccess`, so the 0.51.0 release shows the "run the connect command"
  notice by itself. On top of that:
  - `setClusterTraffic` to `tunnel` is refused while the cluster's install
    is outdated (`install::install_state` says "outdated", or the agent
    hasn't reported the `tunnel` capability): "Run the command under
    Refresh this cluster in the cluster's settings first." So a cluster
    never loses its public ports for a tunnel it can't run.
  - When the install later turns out older than the agent (a refused
    write of a tunnel object), the agent keeps what's running, writes
    nothing else for the tunnel, reports `TunnelReport.refused`, and tries
    again once the install changes (`refused::install_stamp`). The status
    line says "Run the command under Refresh this cluster in the cluster's
    settings." The Gateway itself is never broken by that refusal: the
    tunnel client policy is written through `write_client_policy`, and a
    refused one is skipped and reported, never stopping the service's
    routes.
- **Rotate** gives the tunnel a new token and a new certificate in one
  action. There's no separate certificate action.
- **Certificates while switching**: a domain keeps a certificate already
  issued for it while it's `"switching"` (cloudflared trusts public CAs
  too, and direct visitors still see a valid one). It takes the tunnel's
  Origin CA certificate once `"on"`, or at once when it has no issued one
  (a new domain). Its old TLS mode is kept as `tlsModeBefore` (and
  `certificateIdBefore`) and comes back with Switch back.
- **Public TCP and UDP ports** get their own heartbeat reply field,
  `publicPorts` (public traffic, or tunnel mode with `ports_public`), so
  they don't depend on the tunnel id being known yet.
- **connect.sh keeps its line** ("Public traffic stays off, so your sites
  keep working."). The seamless-scripts rule keeps hints out of scripts.
  The dashboard says Cloudflare Tunnel can take the websites.
- **Tunnel removal while a domain is removed**: the dashboard's "Put back
  the old record" sends `setDomainTunnel` with `restore` before the domain
  goes. A removed tunnel domain's record is otherwise reverted without
  putting back what it replaced.

Decisions after the real-Cloudflare test (2026-11-22):

- **The permission probe is this installation's own.** The probe tunnel
  is `pyo-check-<install id>-<6 hex>`. A check deletes only probes with
  that prefix made more than 5 minutes ago, so it never touches another
  installation's probe, a tunnel someone named `pyo-check-…`, or the
  probe of a check running at the same time. The DNS probe's text is
  `"pyo-check-<install id>"`: Cloudflare refuses identical records, so
  with one shared text a probe another installation left in a shared
  zone would fail every later check there.
- **No wildcard route.** The default domain keeps its wildcard record and
  its wildcard name on the certificate, but the tunnel's routes list only
  names a service has (free names under the default domain included).
  Any other name stops at the routes' last rule, `http_status:404`, in the
  connector. Before, the wildcard route sent those names to the Gateway,
  which has no listener for them and reset the connection (502 at
  Cloudflare). Nothing was added to the Gateway, so the gate is as it was.
  A new name still needs the loop's next pass (up to 30 s) and the
  Gateway's listener before it answers. Until then it gets 404, then for
  a few seconds 502 while the Gateway takes the listener.
- **A name under the default domain always goes through the tunnel.**
  Without a record of its own the loop puts it through the wildcard, so
  Switch back would last one pass. `setDomainTunnel` with `on: false`
  answers "<name> is under the tunnel's default domain, so it always goes
  through the tunnel." and writes no activity line. Two cases still go
  ahead. A record the switch replaced comes back (`restore` with a kept
  record: it answers ahead of the wildcard, and removing a domain with
  "Put back the old record" needs it). And a switch that waits for a yes
  can be cancelled: the name stays `"on"`. The dashboard shows such a name
  as "Via Tunnel · *.example.com" and offers Switch back only with a kept
  record.
- **A new name under the default domain gets no record of its own**
  (user, 2026-11-23). `setDomainTunnel` with `on: true` for a name one
  level under the default domain that isn't on the tunnel yet reads the
  records on that exact name at Cloudflare. With no A, AAAA or CNAME
  there, the domain takes `tunnel: "on"` at once and the answer is
  `status: "active"`: the wildcard record has it, and Pyo makes no
  record. With one there (it would answer ahead of the wildcard), or when
  Cloudflare can't be asked, the usual switch runs: Pyo's own record is
  replaced and someone else's stops at "conflict". This is in the op, so
  the dashboard, the CLI, MCP and the API behave the same. The CLI now
  sends the op for such names too. Asking again for a name already
  `"on"` still makes its own record (next point).
- **A name already `"on"` never goes back to `"switching"`.** Asking to
  switch a name the wildcard already serves makes its own record and route
  while it stays gated. Only a name moving from public ports gets both
  paths.
- **X-Forwarded-For for the service is the visitor alone.** Envoy reads
  the visitor from the header on the connectors' connections, then appends
  the connection's address, so the service saw `<visitor>, <connector's
  pod>`. Tunnel routes (`-tunnel-` and `-switching-`) now set
  `X-Forwarded-For: %DOWNSTREAM_REMOTE_ADDRESS_WITHOUT_PORT%` with a
  `RequestHeaderModifier` on the rule that goes to the service
  (`system::tunnel_rule`). That is Envoy's own address for the client
  after the client policy ran: the visitor through the tunnel, and a
  direct client's own address while a domain is switching, whatever
  either sent. Envoy Gateway passes the value to Envoy as it is
  (`buildXdsAddedHeaders`, v1.9.3). Its client policy option
  `disableXForwardedForAppend` wasn't used: it would pass a direct
  client's made-up header to the service while a domain is switching.
  The admission policy's HTTPRoute rule allows that one header and value
  next to a static site's, so the access hash moves (the roles don't).

---

## 1. Goals and non-goals

**Goals**

- A cluster can take web traffic through Cloudflare Tunnel with no inbound
  ports. The cluster's traffic setting becomes one choice of three:
  **Public ports**, **Tailscale only**, **Cloudflare Tunnel**.
- The team's existing Cloudflare token (Integrations › Cloudflare, today
  Zone:Read and DNS:Edit) also gets **Cloudflare Tunnel:Edit** and
  **SSL and Certificates:Edit**. Pyo checks them and shows which zones can
  use the tunnel.
- The control plane holds the token (sealed, as today), makes one tunnel per
  cluster through Cloudflare's API, keeps its routes (remotely managed
  ingress) and proxied DNS records in step with the cluster's domains, and
  issues a 15 year Origin CA certificate for Pyo Gateway.
- The agent gets only the tunnel token. It runs cloudflared as a system part
  (2 replicas spread across servers, HTTP/2 to Cloudflare), pointed at Pyo
  Gateway inside the cluster over HTTPS, and reports its health.
- In tunnel mode Pyo Gateway takes no host ports 80 and 443, so Pyo runs next
  to Traefik or nginx on the same server.
- Firewall, rate limits, WAF, country rules, analytics and the audit log see
  the real visitor, trusted only from cloudflared.
- Each domain shows whether it goes **via Tunnel**, with **Switch to Tunnel**
  and **Switch back**. An existing DNS record is shown, replaced only after
  the user says yes, and kept so Switch back restores it. This is the
  zero-downtime path off Traefik (real case: misc-ten-sin4, k3s Traefik on
  80/443 with 8 sites).
- Works the same on Pyo Cloud: the token stays on the control plane, the
  agent only gets the tunnel token.

**Non-goals (0.51.0)**

- TCP and UDP ports through the tunnel. Cloudflare only proxies them through
  Spectrum or WARP clients. They stay on public ports or Tailscale.
- Cloudflare Access, Zero Trust policies, private networks (WARP routing),
  load balancers, Workers. Users can add those in Cloudflare themselves.
- Locally managed tunnels (a config file next to cloudflared). Pyo uses
  remotely managed config only.
- More than one Cloudflare account per cluster tunnel (see §11).
- Managing zone settings (Always Use HTTPS, SSL mode, caching). Pyo never
  changes zone-wide settings.
- Pyo's own dashboard through the tunnel (decision 7).
- Uptime checks or status pages (never in Pyo).

## 2. How it fits together

```
visitor ── HTTPS ──► Cloudflare edge (proxied CNAME <tunnel>.cfargotunnel.com)
                          │  tunnel connections (HTTP/2 over TCP 7844, outbound from the cluster)
                          ▼
         pyo-system/pyo-tunnel (cloudflared ×2, spread across servers)
                          │  https://pyo-tunnel-origin.pyo-gateway.svc:443, SNI = Host
                          ▼
         Pyo Gateway (Envoy, listener per hostname, Origin CA certificate)
                          │  visitor from X-Forwarded-For, trusted only from cloudflared's pods
                          ▼
                     the service
```

Who does what:

| Part | Holds | Does |
|---|---|---|
| Control plane (pyo-api, worker) | The team's Cloudflare API token, sealed | Creates the tunnel, PUTs its ingress, makes and removes DNS records, issues Origin CA certificates, reads tunnel status, sends the tunnel token to the agent |
| Agent | The tunnel token (a Secret in pyo-system) | Runs cloudflared, reports its pods, writes Gateway objects for tunnel domains, keeps the trusted cloudflared addresses current |
| cloudflared | The tunnel token | Dials out to Cloudflare, forwards requests to Pyo Gateway |
| Cloudflare | The tunnel and its config | Proxies the hostnames, holds the routes |

Traffic keeps flowing when Pyo's control plane is down: the routes live at
Cloudflare and the token lives in the cluster.

## 3. User flows

### 3.1 Dashboard

**A. Add permissions to the token** (Integrations › Cloudflare,
`dash/src/components/pyo/Cloudflare.tsx`)

- Token field hint (today `Cloudflare.tsx:141`):
  "Zone:Read and DNS:Edit. For Cloudflare Tunnel, also Cloudflare Tunnel:Edit and SSL and Certificates:Edit."
- Zone badges gain the tunnel state:
  - `example.com` (green): DNS and tunnel ready.
  - `example.com · DNS only` (neutral): token has no tunnel permissions here.
  - `example.com · needs SSL and Certificates:Edit` (warning).
- Problems list (one line each, from `cloudflare::problems`):
  "example.com: give the token Cloudflare Tunnel:Edit."

**B. Choose Cloudflare Tunnel for a cluster** (Cluster › Settings, the
"Public traffic" panel `dash/src/components/pyo/TrafficMode.tsx:77-114`
becomes "Traffic", rendered at
`dash/src/routes/_app/$team/clusters/$clusterId/settings.tsx:120`)

Radio group, same look as "Traffic routing" (`TrafficMode.tsx:24-53`):

| Choice | Hint |
|---|---|
| Public ports | Ports 80 and 443 on every server except build-only servers. |
| Tailscale only | Only from your tailnet. *(No Tailscale: "Only inside the cluster until you set up Tailscale.")* |
| Cloudflare Tunnel | No open ports. Websites come in through Cloudflare. *(Disabled: "Connect Cloudflare first." / "Give the token Cloudflare Tunnel:Edit." / "Not reported yet.")* |

When Cloudflare Tunnel is picked, the panel shows its settings below the
radio group:

- **Default domain** (select of tunnel-ready zones, optional, "None"
  first): hint "Services get names like web.example.com."
- **Cloudflare account** (select, only when the token's zones span more than
  one account).
- **Public TCP and UDP ports** (switch, on by default, shown only when the
  cluster has any): "On every server except build-only servers. They don't
  go through the tunnel."
- Status line (see §10), with a **Remove tunnel** button under Danger zone
  when the cluster is not in tunnel mode and still has one.

Confirm dialog before switching to Cloudflare Tunnel:

- Title: "Switch eu to Cloudflare Tunnel?"
- Body lines, only those that apply:
  - "Websites on example.com zones move to the tunnel. Records Pyo didn't make wait for your OK."
  - "3 domains aren't in your Cloudflare account. They stop answering from the internet."
  - "The public dashboard closes. Open Pyo through Tailscale or your local network." (cluster Pyo runs on)
- Buttons: "Cancel", "Switch".

**C. Per domain** (service › Domains,
`dash/src/routes/_app/$team/$projectId_/$serviceId/domains.tsx:263`,
`DnsRecordLine` in `Cloudflare.tsx:298-408`)

The record line gets a tunnel variant:

- Via Tunnel: `[cf] Via Tunnel · CNAME 1f2e….cfargotunnel.com` and a
  **Switch back** button.
- Not via Tunnel, cluster has a tunnel, zone is tunnel-ready: a
  **Switch to Tunnel** button.
- Existing record found (needs OK), warning box, like today's conflict box:
  - "Cloudflare has a record for blog.example.com:"
  - rows: `A blog.example.com 203.0.113.7 proxied`
  - "Pyo keeps a copy. Switch back puts it back."
  - Buttons: "Replace and switch" (owners and admins), "Cancel".
    Developers see "An owner or admin can replace it."
- Switching: "Switching to the tunnel…"
- Switching back: "Switching back…"
- Failed: "Cloudflare: <message>" with "Try again".
- Not in the account, cluster in tunnel mode:
  "Not in your Cloudflare account. It doesn't answer from the internet."
- Not in the account, cluster on public ports: nothing new (as today).

Switch back confirm: "Put back A 203.0.113.7 for blog.example.com?"
Buttons "Cancel", "Switch back". Without a kept record and with public
ports: "Point blog.example.com at 203.0.113.7?". Without either: "Remove
Pyo's record for blog.example.com?".

**D. Add domain** (`dash/src/components/pyo/AddDomainDialog.tsx:86-183`,
`CloudflareFields` at `Cloudflare.tsx:242-286`)

On a cluster with a tunnel and a tunnel-ready zone, the Cloudflare fields
become one switch, on by default in tunnel mode:

- "Through Cloudflare Tunnel" with description "CNAME to the tunnel in example.com".
- Off falls back to today's "Create the DNS record in Cloudflare".

**E. Remove domain**: the existing remove dialog's `DeleteRecordSwitch`
(`Cloudflare.tsx:410`) gets a second switch when a record was kept:
"Put back the old record" (off by default, decision 5), description
"A blog.example.com 203.0.113.7".

### 3.2 CLI (`crates/pyo-cli`)

New subcommands under the existing `pyo cloudflare` and `pyo domain`
(`main.rs:267-508`):

```
pyo cloudflare tunnel status [--cluster eu]
pyo cloudflare tunnel on  --cluster eu [--default-domain example.com] [--account <id>] [--no-public-ports]
pyo cloudflare tunnel off --cluster eu [--to public|tailscale]
pyo cloudflare tunnel remove --cluster eu [--keep-records]
pyo cloudflare tunnel rotate --cluster eu
pyo domain tunnel <hostname> [--service web] [--replace]
pyo domain tunnel <hostname> --back
```

- `status` prints the tunnel, its connectors (server, colo, since) and each
  domain's state.
- `domain tunnel` without `--replace` stops at a found record, prints it and
  says "Run again with --replace to replace it. Pyo keeps a copy." (exit 1).
- `pyo cloudflare test` output adds the tunnel columns per zone.

### 3.3 API

All changes go through `POST /api/v1/ops` (Op enum in
`crates/pyo-api/src/ops.rs`), so they land in the OpenAPI reference and as
MCP tools without extra code (`mcp.rs:467-500` builds op tools from the
spec). Plus one read endpoint. Details in §5.

### 3.4 MCP

- New op tools come for free: `set_cluster_traffic`,
  `switch_domain_tunnel`, `remove_cluster_tunnel`, `rotate_tunnel_token`.
  Their doc comments are the tool descriptions, so they follow the copy
  rules.
- `add_domain` (`mcp.rs:158`) gains `"tunnel": { "type": "boolean",
  "description": "Through the cluster's Cloudflare Tunnel when a tunnel-ready zone covers the hostname (default: on in tunnel mode)." }`
  and the code path at `mcp.rs:1289-1310` calls the same function as the op.
- `getCloudflare` already exists. `getClusterTunnel` comes from the new
  endpoint.

## 4. Data model and migration

New migration `crates/pyo-api/migrations/0040_cloudflare_tunnel.sql` (0039
went to `0039_database_import_parts.sql` in 0.5.5).

```sql
-- The cluster's traffic: 'public' (ports 80 and 443 on its servers),
-- 'tailscale' (only the tailnet, or inside the cluster without one) or
-- 'tunnel' (websites through Cloudflare Tunnel, no ports 80 and 443).
ALTER TABLE pyo.clusters ADD COLUMN traffic text NOT NULL DEFAULT 'public'
  CHECK (traffic IN ('public', 'tailscale', 'tunnel'));
UPDATE pyo.clusters SET traffic = CASE WHEN public_traffic THEN 'public' ELSE 'tailscale' END;
-- Every reader of public_traffic keeps working (freedomain.rs, traffic.rs,
-- agent.rs, snapshot.rs): it follows the choice.
ALTER TABLE pyo.clusters DROP COLUMN public_traffic;
ALTER TABLE pyo.clusters ADD COLUMN public_traffic boolean
  GENERATED ALWAYS AS (traffic = 'public') STORED;

-- A cluster's Cloudflare Tunnel (cloudflare/tunnel.rs). Nothing secret:
-- the tunnel token is read from Cloudflare when the agent needs it, and
-- the Origin CA key sits sealed with the team's certificates.
CREATE TABLE pyo.cluster_tunnels (
  cluster_id      text PRIMARY KEY REFERENCES pyo.clusters (id) ON DELETE CASCADE,
  team_id         text NOT NULL REFERENCES pyo.teams (id) ON DELETE CASCADE,
  account_id      text NOT NULL,
  account_name    text NOT NULL DEFAULT '',
  -- Cloudflare's id, once made. "pyo-<cluster>-<installation id>".
  tunnel_id       text,
  tunnel_name     text NOT NULL,
  -- 'creating', 'ready', 'failed', 'removing'.
  state           text NOT NULL DEFAULT 'creating'
                  CHECK (state IN ('creating', 'ready', 'failed', 'removing')),
  error           text,
  -- '*.example.com' gets a wildcard record and route. The zone's own name.
  default_domain  text,
  default_zone_id text,
  -- Pyo's wildcard record, when it made it.
  default_record_id text,
  -- Public TCP and UDP ports while the cluster is in tunnel mode.
  ports_public    boolean NOT NULL DEFAULT true,
  -- The ingress Pyo last PUT, as a hash, and Cloudflare's version of it.
  config_hash     text NOT NULL DEFAULT '',
  config_version  integer,
  -- The Origin CA certificate the tunnel's domains use.
  certificate_id  text REFERENCES pyo.certificates (id) ON DELETE SET NULL,
  -- Cloudflare's id for it, to revoke it, and the one it replaced
  -- (revoked a day after the new one was issued).
  cf_certificate_id  text,
  cf_certificate_old text,
  cf_certificate_old_at timestamptz,
  -- Asked for by Rotate: a new tunnel secret and a new certificate.
  rotate          boolean NOT NULL DEFAULT false,
  -- Last reads: Cloudflare's view ({status, connections: [{colo, clientVersion,
  -- openedAt, originIp}], at}) and the agent's ({tunnelId, pods: [...], at}).
  cf_status       jsonb NOT NULL DEFAULT '{}',
  agent_status    jsonb NOT NULL DEFAULT '{}',
  -- When the token last went to the agent (not more than every 5 minutes).
  token_sent_at   timestamptz,
  created_by      text NOT NULL DEFAULT '',
  created_at      timestamptz NOT NULL DEFAULT now(),
  updated_at      timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX cluster_tunnels_team_idx ON pyo.cluster_tunnels (team_id);

-- A domain through the tunnel: its record is a proxied CNAME to the
-- tunnel. `previous` keeps the records it replaced, for Switch back.
ALTER TABLE pyo.dns_records
  ADD COLUMN via text NOT NULL DEFAULT 'direct' CHECK (via IN ('direct', 'tunnel')),
  ADD COLUMN previous jsonb NOT NULL DEFAULT '[]';
ALTER TABLE pyo.dns_records DROP CONSTRAINT dns_records_status_check;
ALTER TABLE pyo.dns_records ADD CONSTRAINT dns_records_status_check
  CHECK (status IN ('created', 'exists', 'conflict', 'failed', 'switching', 'reverting'));
```

Plus row security for `cluster_tunnels` on `team_id`, like
`0032_row_security_rest.sql`.

Domain spec (`services.spec.domains[]`, JSON) gains `"tunnel"`:
`"switching"` (both paths, see Decisions) or `"on"` (only through the
tunnel). Absent: direct. It's the agent's switch (routes.rs) and changes
routes, so it bumps `routes_revision` like any domain change.
`dns_records.via` mirrors it for the record's side. The domain's `tlsMode`
becomes `"custom"` with `certificateId` = the tunnel's certificate while via
Tunnel, and goes back to what it was (`"auto"` by default) on Switch back.
The old mode is kept as `"tlsModeBefore"` on the domain.

`previous` element: `{ "type": "A", "name": "blog.example.com", "content":
"203.0.113.7", "proxied": true, "ttl": 1, "comment": "…" }`. Only A, AAAA
and CNAME records on the exact name, as `cloudflare::plan` finds them
(`cloudflare.rs:178-196`).

Snapshot (`crates/pyo-api/src/snapshot.rs:140-170`) per cluster:

```jsonc
"traffic": "tunnel",
"tunnel": {
  "state": "ready", "error": null,
  "accountId": "…", "accountName": "Acme",
  "tunnelId": "1f2e…", "defaultDomain": "example.com", "portsPublic": true,
  "connectors": [{ "server": "fal1", "ready": true, "colo": "FRA", "since": 1763800000000 }],
  "health": "connected",          // connected | degraded | down | starting | unknown
  "certificate": { "id": "crt_…", "notAfter": 1795300000000 },
  "checkedAt": 1763800000000
}
```

Per domain, `dnsRecord` (`cloudflare::attach`, `cloudflare.rs:591-621`)
gains `via` and `previous`, and the domain itself carries `tunnel`.
`dash/src/lib/pyo/types.ts` (`Cluster` at `:391`, `Domain.dnsRecord` at
`:907`) follows.

## 5. API

### 5.1 Ops (ops.rs)

Add next to the Cloudflare ops (`ops.rs:2937-2958`), folder "Integrations"
for the token, "Clusters" for traffic, "Domains" for the switch
(`ops.rs:3438`, `:3482`):

```rust
/// How a cluster takes web traffic: "public" (ports 80 and 443 on every server but build-only ones),
/// "tailscale" (only from the tailnet, or only inside the cluster without Tailscale) or "tunnel"
/// (through Cloudflare Tunnel, with no ports 80 and 443 open). For "tunnel", the team's Cloudflare
/// token needs Cloudflare Tunnel:Edit and SSL and Certificates:Edit. `defaultDomain` (optional) names one of
/// its zones: services get names under it. Domains in the account move to the tunnel, and those with a
/// record Pyo didn't make wait for `setDomainTunnel` with `replace`. Refused while the cluster's install is
/// older than Pyo's. Team owners and admins.
SetClusterTraffic {
    cluster_id: String,
    mode: String,
    #[serde(default)] default_domain: Option<String>,
    #[serde(default)] account_id: Option<String>,
    /// Public TCP and UDP ports in tunnel mode. On by default.
    #[serde(default)] ports_public: Option<bool>,
},
/// Serve a domain through the cluster's Cloudflare Tunnel (`on`), or switch it back. Pyo adds the tunnel's
/// route first, then points the name at the tunnel with a proxied CNAME. When other A, AAAA or CNAME records
/// have the name, `status` is "confirm" and `existing` lists them. Send it again with `replace` to replace
/// them (team owners and admins): Pyo keeps them, and switching back puts them back.
SetDomainTunnel { service_id: String, domain_id: String, on: bool, #[serde(default)] replace: bool },
/// Remove a cluster's Cloudflare Tunnel: its routes, the DNS records Pyo made (with `restoreRecords`, the
/// ones they replaced go back), the connector in the cluster and the tunnel itself. The cluster must not
/// be in tunnel mode. Team owners and admins.
RemoveClusterTunnel { cluster_id: String, #[serde(default = "yes")] restore_records: bool },
/// Give the cluster's tunnel a new token and drop the old connections. Team owners and admins.
RotateTunnelToken { cluster_id: String },
```

- `SetClusterPublicTraffic` (`ops.rs:2308`, handler `:4736-4764`) stays for
  API users and maps `on` to "public" and off to "tailscale". Its handler
  writes `traffic` (the generated `public_traffic` can't be written).
- `SetDomainTunnel` with `on` needs Role::Developer, with `replace` or with
  `on: false` restoring a kept record needs Role::Admin (same split as
  `cloudflare::role_for`, `cloudflare.rs:536-542`).
- Rate limit: the existing `ops:team` limit is enough. The reconcile loop
  does the Cloudflare work, so ops return fast (202-like `{ ok, status }`).

### 5.2 Read endpoint

`GET /api/v1/clusters/{id}/tunnel` (`getClusterTunnel`, tag "Clusters",
x-pyo-access read, any team member): the snapshot's `tunnel` plus a fresh
read of Cloudflare's connections (cached 30 s per cluster). Used by the
dashboard's status line on open and by `pyo cloudflare tunnel status`.
Registered in `ops::routes()` (`ops.rs:71-109`).

### 5.3 Changed

- `connectCloudflare` / `testCloudflare` (`cloudflare.rs:657-748`): each
  zone gains `accountId`, `accountName` and
  `tunnel: { ready: bool, missing: ["Cloudflare Tunnel:Edit", …] }`.
  Connecting still needs DNS:Edit in at least one zone. Tunnel permissions
  are optional.
- `removeCloudflare` (`cloudflare.rs:751-776`): refused while a cluster of
  the team has a tunnel: "Remove the Cloudflare Tunnel of eu first. Its
  websites keep working until then." Forgetting the token would strand the
  tunnel (Pyo could no longer change or remove it).
- Agent endpoints: heartbeat body and reply (§7.4).

## 6. Control plane

New module `crates/pyo-api/src/cloudflare/tunnel.rs` (the `cloudflare/`
folder exists for `tests.rs`). The `Cloudflare` trait
(`cloudflare.rs:119-128`) grows, so the fake account in
`cloudflare/tests.rs` covers it too.

### 6.1 Cloudflare calls

Base `https://api.cloudflare.com/client/v4` (`cloudflare::API`), bearer
token, through the existing `Http::call` (`cloudflare.rs:363-385`: host
pinned, no redirects, outbound client with `Reach { private: false }`).

| Purpose | Call | Permission |
|---|---|---|
| Zones and their account | `GET /zones?per_page=50&page=n` (already called, now also reads `result[].account.{id,name}`) | Zone:Read |
| Records (exists) | `GET/POST/PATCH/DELETE /zones/{zone}/dns_records[/{id}]` | DNS:Edit |
| Tunnel permission check | `POST /accounts/{acct}/cfd_tunnel` `{ "name": "pyo-check-<install id>-<6 hex>", "config_src": "cloudflare" }`, then `DELETE /accounts/{acct}/cfd_tunnel/{id}`. Leftovers are listed with `GET /accounts/{acct}/cfd_tunnel?name=pyo-check-<install id>-&is_deleted=false`, and only this installation's own (that prefix and six hex characters) made more than 5 minutes ago (`created_at`) are deleted. Another installation's probes and anyone's own `pyo-check-…` tunnel are never touched. Once per account. | Cloudflare Tunnel:Edit |
| Origin CA permission check | `GET /certificates?zone_id={zone}` | SSL and Certificates:Read (Edit is required, see below) |
| Create tunnel | `POST /accounts/{acct}/cfd_tunnel` `{ "name": "pyo-<cluster>-<install id>", "config_src": "cloudflare" }` | Cloudflare Tunnel:Edit |
| Read tunnel | `GET /accounts/{acct}/cfd_tunnel/{tunnel}` (status: inactive, degraded, healthy, down; `deleted_at`) | Cloudflare Tunnel:Read |
| Connectors | `GET /accounts/{acct}/cfd_tunnel/{tunnel}/connections` (colo_name, client_version, opened_at, origin_ip per connection) | Cloudflare Tunnel:Read |
| Token for the agent | `GET /accounts/{acct}/cfd_tunnel/{tunnel}/token` | Cloudflare Tunnel:Edit |
| Routes | `PUT /accounts/{acct}/cfd_tunnel/{tunnel}/configurations` `{ "config": { "ingress": [...] } }` | Cloudflare Tunnel:Edit |
| Rotate token | `PATCH /accounts/{acct}/cfd_tunnel/{tunnel}` `{ "tunnel_secret": "<base64 32 bytes>" }`, then drop connections | Cloudflare Tunnel:Edit |
| Drop connections | `DELETE /accounts/{acct}/cfd_tunnel/{tunnel}/connections` | Cloudflare Tunnel:Edit |
| Delete tunnel | `DELETE /accounts/{acct}/cfd_tunnel/{tunnel}` (needs no active connections) | Cloudflare Tunnel:Edit |
| Origin certificate | `POST /certificates` `{ "csr": "<PEM>", "hostnames": [...], "request_type": "origin-ecc", "requested_validity": 5475 }` | SSL and Certificates:Edit |
| Revoke it | `DELETE /certificates/{id}` | SSL and Certificates:Edit |

Checked against Cloudflare's docs on 2026-11-22: "Create a tunnel (API)"
lists Account › Cloudflare Tunnel › Edit and Zone › DNS › Edit, and the
calls above. The Origin CA page says an API token with Zone › SSL and
Certificates › Edit works (no Origin CA Key needed), up to 200 names per
certificate, one-level wildcards, validity 7, 30, 90, 365, 730, 1095 or
5475 days. The tunnel token endpoint path and the PATCH rotation shape come
from the API reference and must be checked again in the real-zone test
(§17.4).

There's no cheap probe for SSL and Certificates **Edit** short of issuing a
certificate. The check reads (proves Read), and the first issue reports a
missing Edit plainly: "Give the token SSL and Certificates:Edit on
example.com." The dashboard shows the zone as tunnel-ready only when both
checks pass.

The account id comes from each zone, so the token needs no Account
Settings permission. Token checks are once per account (tunnel) and once
per zone (certificates), capped by `MAX_ZONES` (200).

Ids from Cloudflare go into paths only after `id_ok` (`cloudflare.rs:322`).
Tunnel ids are UUIDs: `id_ok` gains `-`, or a separate `uuid_ok`.

### 6.2 The ingress Pyo sends

```json
{ "config": { "ingress": [
  { "hostname": "blog.example.com",
    "service": "https://pyo-tunnel-origin.pyo-gateway.svc.cluster.local:443",
    "originRequest": { "matchSNItoHost": true, "connectTimeout": 10, "keepAliveTimeout": 90 } },
  { "hostname": "web-shop.example.com",
    "service": "https://pyo-tunnel-origin.pyo-gateway.svc.cluster.local:443",
    "originRequest": { "matchSNItoHost": true, "connectTimeout": 10, "keepAliveTimeout": 90 } },
  { "service": "http_status:404" }
] } }
```

- Sorted: longest name first, then the catch-all. cloudflared matches top
  down.
- No wildcard route for the default domain (see Decisions). Each free name
  under it has its own rule, and any other name gets the catch-all's 404.
- `matchSNItoHost` sends the visitor's host as SNI. Go's TLS then checks
  the Origin CA certificate against that host. Pyo Cloud's Terraform uses
  `origin_server_name = h` per name (`pyo-cloud deploy/terraform/cloudflare/tunnel.tf:22-27`),
  which can't express a wildcard. Verify in e2e (§17.2).
- cloudflared trusts Cloudflare's Origin CA itself (Pyo Cloud runs that way
  today without `caPool`, `main.tf:7-9`), so no CA file in the pod.
- The origin is a Service Pyo owns (§8.2), not Envoy Gateway's generated
  `envoy-pyo-<hash>` (Pyo Cloud's Terraform hardcodes
  `envoy-pyo-695a31a2`, `variables.tf:44-48`).
- Hash: sha256 of the canonical JSON. PUT only when it changes or
  Cloudflare's `config_version` moved under Pyo (someone edited it in
  Cloudflare). Pyo's config wins.

### 6.3 The loop

`tunnel::reconcile`, every 30 s on the leader
(`crate::bg::as_leader(&db, LOCK_TUNNEL, …)`, new lock `7_012` in
`bg.rs:69-85`, added to the list at `bg.rs:260`). Started with the worker
tasks next to `deploy::dns_watcher` (`deploy.rs:2531`). Per cluster with a
`cluster_tunnels` row, serially, each step only when needed:

1. **Token.** `cloudflare::account()` (`cloudflare.rs:493`). None: state
   stays, `error = "Connect Cloudflare again to change the tunnel. Its websites keep working."`
   and skip.
2. **Tunnel.** No `tunnel_id`: create it, store the id. `GET` returns 404
   or `deleted_at`: create a new one (same name plus `-2`), mark every
   Pyo-made tunnel record to repoint, send the new token.
3. **Wanted routes** = every domain with `tunnel` set on a non-deleting
   service of the cluster (by `services.cluster_id`), the free names
   under the default domain among them. PUT when the hash differs.
4. **Certificate.** Needed names (§9). Missing a name, or `rotate` set:
   issue a new one, store it, point the domains at it (§9). No renewal by
   age (15 years, decision 2).
5. **Records**, only after step 3 succeeded in this or an earlier pass:
   - Rows `status = 'switching'`, once the service's routes with the
     domain's `tunnel: "switching"` reached the agent (its applied routes
     revision is at least the switch's): `cloudflare::ensure`-like with
     `kind = CNAME`, `content = <tunnel_id>.cfargotunnel.com`,
     `proxied = true`, comment `marker(install_id)`. Replaced records go to
     `previous` (only when the op said `replace`). Then `status = 'created'`,
     the domain's `tunnel = "on"` (gate on, :80 route gone) and its
     `status = 'active'`.
   - Rows `status = 'reverting'`: the domain goes back to
     `tunnel: "switching"` first. Once that reached the agent: delete
     Pyo's CNAME (`delete_ours`, `cloudflare.rs:1124`), create each
     `previous` record as it was (or an A record to the public IP, per the
     op), then drop `tunnel` from the domain and the route on the next pass
     (records first, route after: no gap).
   - The default domain's wildcard: `edgecert::ensure_wildcard`
     (`sandbox/edgecert.rs:775`) with a CNAME.
6. **Agent token.** The agent's last report has no tunnel or another
   tunnel id, and `token_sent_at` is older than 5 minutes: `GET …/token`,
   enqueue `Command::TunnelSet { tunnel_id, token }` (§7.1).
7. **Status.** Every 2 minutes: `GET` tunnel and `/connections`, store
   `cf_status`. Health is computed (§10). Down for 5 minutes: alert event
   `cluster.tunnel_down` (alerts.rs, like server offline), and
   `cluster.tunnel_up` when back.
8. **Old certificate.** `cf_certificate_old` and `cf_certificate_old_at`
   passed (a day after the new one was issued): revoke and clear.
9. **Removing.** `state = 'removing'`: PUT ingress with only the catch-all,
   restore or delete records (only Pyo's, `ours()`), enqueue
   `TunnelRemove`, wait for its answer (or 10 minutes), drop connections,
   delete the tunnel, revoke the certificate, delete the row and the team
   certificate.

Cloudflare's 429s back off per team (Retry-After, else 60 s). A pass
touches Cloudflare only when something differs, plus the 2-minute status
read: well under 1,200 requests per 5 minutes per token.

Errors are stored on the row (`error`) or the record (`dns_records.error`)
in Cloudflare's words via `CfError` (`cloudflare.rs:88-107`), never with the
token.

### 6.4 Domain DNS checks

`deploy::check_pending_dns` (`deploy.rs:2547`) skips domains with
`tunnel` set (their address is Cloudflare's, not the cluster's), like it
skips `.ts.net`. The tunnel loop sets their status: `pending_dns` while
switching, `active` once the CNAME is in place and the route is live.

### 6.5 Free domains in tunnel mode

`freedomain::Mode` (`freedomain.rs:69-89`) gains `Tunnel`. In tunnel mode
with a default domain, the cluster's free domain is
`<service>-<namespace>.<default domain>`
(covered by the wildcard record, with a route of its own) instead of nip.io, and the
dashboard's "free domain" button makes that. Existing nip.io domains on the
cluster stop answering from the internet in tunnel mode (no public 80/443).
The confirm dialog counts them with the other unreachable domains. On
Switch back to public, they answer again (nothing to move). Without a
default domain, the cluster has no free domain in tunnel mode and the
dashboard's button says "Pick a default domain first."

## 7. Agent

New module `crates/pyo-agent/src/tunnel.rs`, objects in
`crates/pyo-proto/src/system.rs` (so the admission policy and tests share
them).

### 7.1 Commands (`pyo-proto/src/lib.rs:959-1276`)

```rust
/// Run the cluster's Cloudflare Tunnel connector with this token: kept as the Secret pyo-system/pyo-tunnel,
/// labelled with the tunnel's id. The connector is (re)applied. `data` is a [`TunnelReport`].
TunnelSet { tunnel_id: String, token: String },
/// Stop the connector and delete its token.
TunnelRemove {},
```

- `teambackup::secret_fields` (`teambackup.rs:465-492`) gets
  `("tunnel_set", "token")`, so the queued command keeps it sealed, and
  `wipe_command` (`:510-519`) adds `"tunnel_set"`, so the database keeps no
  tunnel token after the agent answers.
- Handled in `pyo-agent/src/main.rs` next to `TailscaleSetup` (`:583`) and
  listed in `actions.rs:160`.

### 7.2 Objects (pyo-system)

`system::tunnel_objects(image, replicas)`:

```yaml
apiVersion: v1
kind: Secret
metadata: { name: pyo-tunnel, namespace: pyo-system,
            labels: { app.kubernetes.io/managed-by: pyo, pyo.dev/tunnel-id: "<id>" } }
type: Opaque
stringData: { token: "<tunnel token>" }
---
apiVersion: apps/v1
kind: Deployment
metadata: { name: pyo-tunnel, namespace: pyo-system, labels: { app.kubernetes.io/name: pyo-tunnel, app.kubernetes.io/managed-by: pyo } }
spec:
  replicas: 2
  selector: { matchLabels: { app.kubernetes.io/name: pyo-tunnel } }
  strategy: { type: RollingUpdate, rollingUpdate: { maxSurge: 1, maxUnavailable: 0 } }
  template:
    metadata: { labels: { app.kubernetes.io/name: pyo-tunnel } }
    spec:
      automountServiceAccountToken: false
      affinity: <system::gateway_affinity()>   # not on build-only servers
      topologySpreadConstraints:
        - { maxSkew: 1, topologyKey: kubernetes.io/hostname, whenUnsatisfiable: ScheduleAnyway,
            labelSelector: { matchLabels: { app.kubernetes.io/name: pyo-tunnel } } }
      securityContext: { runAsNonRoot: true, runAsUser: 65532, seccompProfile: { type: RuntimeDefault } }
      containers:
        - name: cloudflared
          image: cloudflare/cloudflared@sha256:4955524aeaa379e154023d5a027c250f7b35041a2e7e950cf13f144bc9b9c5ba # 2026.10.7
          args: [tunnel, --no-autoupdate, --protocol, http2, --metrics, 0.0.0.0:2000, --loglevel, info, run]
          env: [{ name: TUNNEL_TOKEN, valueFrom: { secretKeyRef: { name: pyo-tunnel, key: token } } }]
          readinessProbe: { httpGet: { path: /ready, port: 2000 }, periodSeconds: 10 }
          livenessProbe:  { httpGet: { path: /ready, port: 2000 }, initialDelaySeconds: 60, periodSeconds: 30, failureThreshold: 6 }
          resources: { requests: { cpu: 10m, memory: 32Mi }, limits: { memory: 128Mi } }
          securityContext: { allowPrivilegeEscalation: false, readOnlyRootFilesystem: true, capabilities: { drop: [ALL] } }
---
apiVersion: policy/v1
kind: PodDisruptionBudget
metadata: { name: pyo-tunnel, namespace: pyo-system }
spec: { minAvailable: 1, selector: { matchLabels: { app.kubernetes.io/name: pyo-tunnel } } }
```

- Image constant `CLOUDFLARED_IMAGE` in `system.rs`, the same digest
  Pyo Cloud runs today (`deploy/pyo-ops/compose.yaml:137`), kept current by
  Renovate. Same treatment as Vector's image
  (`pyo-agent/src/observability.rs:32`).
- `--protocol http2`: QUIC (UDP 7844) is often blocked, and cloudflared's
  fallback is slow enough that the liveness probe restarts it first (seen
  on Pyo Ops, `compose.yaml:147-149`). HTTP/2 needs outbound TCP 7844 to
  Cloudflare's edge.
- Replicas: 2 always. On a one-server cluster both run there, which still
  covers a crash and a rollout.
- No Service, no ports: nothing connects to cloudflared. The pyo-system
  default-deny ingress policy (`deploy/k8s/platform.yaml:27-32`) stays as is.
  Kubelet probes are allowed by k3s's network policy controller.
- The Deployment is reapplied on each heartbeat reply that wants a tunnel
  and the Secret exists with that tunnel id, so an agent update moves the
  image. Without the Secret, the agent reports `tunnelId: null` and the
  control plane sends `TunnelSet` (§6.3 step 6).
- Admission: a Deployment in pyo-system with no host paths, no host
  network, no privilege passes the generic pod rules (`rbac.rs:718-723`).
  Check this in the rbac tests. The PDB name is added to the agent's
  write role if PDB writes in pyo-system aren't already allowed.

### 7.3 Trusted addresses

The agent watches pods with `app.kubernetes.io/name=pyo-tunnel` (all their
`podIPs`, ready or not) and keeps `TUNNEL_IPS` (sorted, deduped, `/32` and
`/128`). A change rewrites the client policies of services with tunnel
domains (§8.3), the same way the websites' settings rewrite CDN policies
today (`gateway.rs:1914`). A new pod's first requests can arrive before its
address is trusted. For those few seconds the gate refuses them (403) on
switched domains (see Decisions). The readiness probe keeps Cloudflare from
sending it traffic until cloudflared is connected, which usually takes
longer than the policy update, and the agent watches pods as soon as
they have an address, ready or not.

### 7.4 Heartbeat

`Heartbeat` (`pyo-proto/src/lib.rs:87`) gains:

```rust
/// The Cloudflare Tunnel connector, when the cluster has one.
#[serde(default, skip_serializing_if = "Option::is_none")]
pub tunnel: Option<TunnelReport>,
```

```rust
pub struct TunnelReport {
    /// From the Secret's label. None: no token here.
    pub tunnel_id: Option<String>,
    pub replicas: i32,
    pub ready_replicas: i32,
    /// Each pod: its server, ready, restarts, and why it isn't ready
    /// ("ImagePullBackOff", the last log line containing "ERR", capped at 200 chars).
    pub pods: Vec<TunnelPod>,
    /// The origin Service and the tunnel certificate Secret are in place.
    pub origin_ready: bool,
    /// The cluster's install refused a tunnel object: its admission policy
    /// is older than the agent's (see Decisions).
    pub refused: bool,
}
```

`Capabilities` (`lib.rs:483`) gains `tunnel: bool`. The dashboard greys the
choice until it's reported ("Not reported yet.").

`HeartbeatReply` (`lib.rs:2610-2650`) gains:

```rust
/// The Cloudflare Tunnel connector: run it (with the token the agent has for this id) or not.
/// None (a reply without it): left as it is.
#[serde(default, skip_serializing_if = "Option::is_none")]
pub tunnel: Option<TunnelWanted>,
```

```rust
pub struct TunnelWanted {
    /// The tunnel to run. None: remove the connector and its token.
    pub tunnel_id: Option<String>,
    /// In tunnel mode: TCP and UDP ports published on the servers (§8.5).
    pub ports_public: bool,
}
```

Filled in `agent.rs:699-733` from `cluster_tunnels`. `publicTraffic`
follows `traffic = 'public'` through the generated column, so older agents
(N-1) do the right thing for Envoy's Service.

## 8. Pyo Gateway changes

All in `crates/pyo-agent/src/gateway.rs` (route building at `:311-460`) and
`crates/pyo-proto/src/system.rs`.

### 8.1 No host ports in tunnel mode

Nothing new: `traffic != 'public'` gives `publicTraffic: false`, and the
EnvoyProxy's Service becomes ClusterIP (`system.rs:711-722`,
`gateway.rs:1573-1611`). ServiceLB then runs no `svclb-envoy` pods, so
Traefik or nginx keep 80 and 443.

### 8.2 The origin Service

`system::TUNNEL_ORIGIN_SERVICE = "pyo-tunnel-origin"` in pyo-gateway,
written by the agent while the cluster has a tunnel, shaped like
`tailnet_web_service` (`system.rs:1169-1186`) without externalIPs:

```json
{ "apiVersion": "v1", "kind": "Service",
  "metadata": { "name": "pyo-tunnel-origin", "namespace": "pyo-gateway", "labels": { "app.kubernetes.io/managed-by": "pyo" } },
  "spec": { "type": "ClusterIP", "selector": <proxy_pods().matchLabels>,
            "ports": [{ "name": "https", "port": 443, "targetPort": 10443, "protocol": "TCP" }] } }
```

Admission: add it to the pinned Services (`rbac.rs:268`, `:1332` and the
CEL pins near `:649-660`) with exactly this spec.

### 8.3 Tunnel domains on the service's Gateway

`DomainSpec` (`lib.rs:2250-2264`) gains
`#[serde(default, skip_serializing_if = "String::is_empty")] pub tunnel: String`
(`""`, `"switching"` or `"on"`), set by `routes::domain_specs`
(`routes.rs:19-43`) from the domain's `tunnel`. For a domain with `tunnel`:

- **Listener**: as today, one HTTPS listener `h-<hash>` on 443 per
  hostname (`gateway.rs:320-335`), `tls = "custom"`, the tunnel's
  certificate (fetched like any uploaded one, `gateway.rs:741-758`).
- **Routes**: its own route per port, `{base}-tunnel-{port}`, parent the
  service's Gateway, so the redirect below applies to tunnel names only.
  A service can mix tunnel and direct domains.
- **:80 routes** stay while `"switching"` and go once `"on"` (see
  Decisions). cloudflared always speaks HTTPS to the Gateway.
- **HTTPS redirect**: with `redirect_https`, the tunnel route's first rule
  matches header `cf-visitor` exactly `{"scheme":"http"}` and redirects to
  https (301). Cloudflare sets that header on every proxied request and
  cloudflared passes it on. Without it, a plain-HTTP visitor would get the
  page over HTTP at Cloudflare's edge.
- **Visitor address**: a ClientTrafficPolicy per service for its tunnel
  listeners, from `cdn_client_policy_spec` (`system.rs:1324-1343`) with
  `trusted = TUNNEL_IPS`, labelled `CDN_LABEL: "tunnel"`. Envoy then reads
  the visitor from X-Forwarded-For (the address Cloudflare appended, the
  same one it puts in CF-Connecting-IP) only on connections from
  cloudflared's pods. Everyone else is their own address.
- **Gate** (see Decisions): with `"on"`, the firewall's "behind" gate
  (`firewall::behind_gate_rule`, `BEHIND_GATE_CEL = "source.port != 0"`)
  goes ahead of the service's own rules on the tunnel route. A direct
  connection with a tunnel hostname (another pod, or a visitor reaching
  443 on a public-ports cluster) gets 403. With `"switching"`, no gate.
  Tunnel routes with both kinds of domains are split: `{base}-tunnel-{port}`
  for `"on"` names, `{base}-switching-{port}` for the others.
- **Firewall, rate limits, WAF, countries**: unchanged. They read the
  visitor Envoy worked out, so they see the real one. GeoIP country follows
  the visitor address. The access log's `remote` field
  (`system.rs:68`) is the visitor, so analytics and the request explorer
  are right.
- The service's own "Behind Cloudflare" switch (`firewall.behind`) doesn't
  apply to tunnel listeners. The tunnel policy wins on those sections. The
  dashboard hides the "Turn on Behind Cloudflare" hint for tunnel records
  (`Cloudflare.tsx:390-398`).

Admission (`rbac.rs:547-552`): the CDN client policy rule allows labels in
`firewall::BEHIND`. Add `"tunnel"` to that list (not to `BEHIND` itself,
which is user-facing) and keep the `/32` and `/128` entries within
`TRUSTED_V4_RE` / `TRUSTED_V6_RE` (both already allow them). The CDN
policy's `headers` are pinned per CDN name (`cdn_headers` in rbac.rs, from
`system::cdn_own_headers`): give `"tunnel"` Cloudflare's list there too,
so CF-Connecting-IP keeps passing on tunnel listeners (§8.4).

Why not Envoy's `customHeader: CF-Connecting-IP`: Envoy would trust it from
any connection (`system.rs:1320-1323`). X-Forwarded-For with trusted peers
gives the same address and only from cloudflared.

### 8.4 The API behind the Gateway (found while writing this)

pyo-api reads the visitor from `CF-Connecting-IP` when the connection comes
from a trusted proxy (`ratelimit.rs:355-366`), and the default trusted
proxies are the pod network (`ratelimit.rs:280`), which includes Envoy.
Envoy passes on a `CF-Connecting-IP` a direct client made up. On a cluster
with the public dashboard, a client can pick its own address for Pyo's rate
limits and audit log. Fix (0.5.x, before this feature): the Gateway removes
`CF-Connecting-IP` from requests on every listener except tunnel listeners
and listeners behind Cloudflare (ClientTrafficPolicy
`headers.earlyRequestHeaders.remove`, Envoy Gateway 1.2+), with the
admission policy's client policy variants updated. Through the tunnel the
header is Cloudflare's own, so audit and rate limits see the real visitor.
This also closes the Pyo Cloud item "checking real client IPs in the audit
log".

**Done** in 0.5.5 (branch `fix/client-ip-headers`, decision 11):

- Every client policy sets `headers.earlyRequestHeaders.remove`
  (`system::visitor_headers`): CF-Connecting-IP, CF-Connecting-IPv6,
  True-Client-IP, Fastly-Client-IP, X-Real-IP, X-Client-IP and Forwarded.
  `pyo-https` with origin pull keeps the four CDN headers (only the CDN
  connects). A service behind Cloudflare keeps Cloudflare's two. The
  tailnet Gateway gets a policy of its own (`pyo-tailnet`) with the full
  list. X-Forwarded-For is never removed.
- pyo-api (`ratelimit::visitor_ip`) never takes CF-Connecting-IP from k3s's
  pod network, whatever `PYO_TRUSTED_PROXIES` lists. Behind the Gateway the
  visitor comes from X-Forwarded-For, walked right to left.
- The sandbox edge removes the same headers before the guest sees them.

### 8.5 TCP and UDP ports in tunnel mode

Envoy's own Service is ClusterIP in tunnel mode, so services' public TCP
and UDP ports would vanish. With `ports_public` (default on), the agent
writes `system::PUBLIC_PORTS_SERVICE = "pyo-public-ports"`: a LoadBalancer
Service in pyo-gateway, `externalTrafficPolicy: Local`, selector the proxy
pods, ports = the L4 listeners' ports (`proxy_listen_port`,
`lib.rs:2240`). ServiceLB publishes only those. The admission policy pins
the name, type, selector and refuses 80, 443, 10080, 10443 and the tailnet
ports (the `tcp_reserved` list at `rbac.rs:701`). Off: TCP and UDP ports are
reachable only inside the cluster and over Tailscale, as in Tailscale only
mode.

## 9. Certificates

- **One Origin CA certificate per cluster tunnel**, ECDSA P-256, 5475
  days (15 years), never renewed by age (decision 2). Names: for each tunnel hostname, the
  hostname itself if it's a zone apex, else `*.<parent>`
  (`blog.example.com` → `*.example.com`, `a.b.example.com` →
  `*.b.example.com`), plus `*.<default domain>`. Deduped, at most 200.
  A new name not covered triggers a reissue. Over 200: refuse the switch
  with "This tunnel has too many names for one certificate."
- **Key and CSR made by the control plane** (rcgen, already used for origin
  pull certificates, `certs.rs:314`), stored with the team's certificates
  (`pyo.certificates`, key sealed with `seal_credential`), named
  "Cloudflare Tunnel (eu)", issuer "Cloudflare Origin CA". This is the
  pattern preview wildcards already use (`previews/domain.rs:460-520`).
  Decision 1.
- **Tunnel domains use it as an uploaded certificate**: `tlsMode =
  "custom"`, `certificateId = cluster_tunnels.certificate_id`. The agent
  already fetches custom certificates from the API and writes them as the
  listener's Secret (`gateway.rs:741-758`). Nothing new on the agent's side.
- **Reissue** (a name not covered, or Rotate): the new certificate
  replaces the row's chain and key in place (same id), and the services
  using it get a routes sync (`certs::used_by`, `certs.rs:208`, as
  `ReplaceCertificate` does). The old Cloudflare certificate is revoked a
  day later (§6.3 step 8).
- **ACME skipped**: tunnel domains never get a cert-manager `Certificate`,
  so no HTTP-01 to fail. `cloudflare::proxy_ready`
  (`cloudflare.rs:1199-1245`) only handles `via = 'direct'` rows.
- **Expiry alerts**: none needed in practice (15 years). The existing
  warnings (`certs::warn_expiring`) still apply if one ever gets close.
- Visitors see Cloudflare's edge certificate (Universal SSL). The Origin CA
  certificate is only between cloudflared and the Gateway.

## 10. Health and dashboard states

Health is computed by the control plane from both views:

| Health | When | Status line |
|---|---|---|
| connected | Cloudflare `healthy`, and the agent reports every replica ready | "Tunnel connected · 2 connectors · FRA, AMS" |
| degraded | Cloudflare `degraded`, or fewer ready replicas than wanted | "Tunnel degraded · 1 of 2 connectors" |
| down | Cloudflare `down` or `inactive` for over a minute while wanted | "Tunnel down. Its websites don't answer." |
| starting | Created in the last 5 minutes and not yet healthy | "Starting the tunnel…" |
| unknown | No token, or Cloudflare unreachable | "Pyo can't reach Cloudflare. Websites keep working." |

Extra lines under the status, only when true:

- "pyo-tunnel can't start on fal2: ImagePullBackOff" (from the agent's pod reason).
- "Connect Cloudflare again to change the tunnel." (token gone or refused).
- "Certificate: <reason>" (the last issue failed).
- "Run the command under Refresh this cluster in the cluster's settings."
  (the agent reports `refused`, see Decisions).

Colours follow the existing Badge tones (success, warning, destructive).
The cluster list shows a small "Tunnel" badge next to the cluster's status
when in tunnel mode, warning tone when degraded or down.

## 11. Edge cases and limits

- **Domains outside the account**: in public mode they stay on public
  ports. In tunnel mode they don't answer from the internet (tailnet
  domains keep working). The dashboard says so per domain and in the
  switch dialog.
- **One account per tunnel**: Cloudflare refuses a CNAME to a tunnel in
  another account. The tunnel lives in the account of the default domain's
  zone. Zones in other accounts show "Another Cloudflare account" and stay
  direct.
- **Default domain**: optional (decision 4). Without it, only domains
  switched by hand use the tunnel, and there are no free domains (nip.io
  names can't be proxied). It must be a zone the token covers (`example.com`, not
  `apps.example.com`): Universal SSL only covers one level below the zone,
  so `web.apps.example.com` would fail TLS at Cloudflare without Advanced
  Certificate Manager. One wildcard record `*.example.com` (proxied CNAME)
  and a route per name (no wildcard route). Explicit records on the zone win over the
  wildcard, so existing sites aren't touched. A wildcard record already
  there is a conflict and asks first, like any record.
- **Deeper user names** (`a.b.example.com`) can be switched, with a warning
  in the dialog: "Cloudflare's free certificate doesn't cover a.b.example.com."
- **Removal**: deletes the tunnel, its routes, the connector, the origin
  certificate and only records Pyo made (comment `Made by Pyo (<install id>)`
  and the stored id, `cloudflare::ours`). Kept records go back when asked.
  Tunnels Pyo didn't make are never touched.
- **TCP and UDP ports**: never through the tunnel (§8.5).
- **ACME**: skipped for tunnel domains (§9). Domains switched back get
  their old TLS mode, and Let's Encrypt issues again once DNS points at the
  cluster (direct records start DNS-only, as today).
- **Cloudflare limits** (docs only, not dashboard):
  - Request body 100 MB on Free and Pro, 200 MB Business, 500 MB
    Enterprise. Bigger uploads get 413 at Cloudflare.
  - Cloudflare waits 100 s for the response to start (524). A service's
    longer request timeout doesn't help through the tunnel.
  - WebSockets work. gRPC needs the zone's gRPC setting.
  - Cloudflare caches some file types by default. Pyo sets no cache rules.
- **Plain HTTP visitors**: redirected by the Gateway via `cf-visitor`
  (§8.3), whatever the zone's Always Use HTTPS.
- **Pyo's own dashboard** on the cluster Pyo runs on: its nip.io address
  closes in tunnel mode (no public 80/443). The confirm dialog says so.
  Reach Pyo over Tailscale or LAN HTTP (tailscale-first decision).
- **Preview environments** on a tunnel cluster: previews use the default
  domain (`web-pr-42.example.com`), covered by the wildcard. The preview
  domain setting and its DNS-01 wildcard certificate aren't used in tunnel
  mode.
- **Hostname moved between clusters**: switching a domain whose name is a
  route on another cluster's tunnel removes it there first (one name, one
  tunnel).
- **Domain hostname edited**: `cloudflare::forget_removed`
  (`cloudflare.rs:625-652`) is extended: a removed tunnel domain's row goes
  to `reverting` (with the user's restore choice) instead of being
  forgotten, so its CNAME and route are cleaned up.
- **Cluster deleted**: rows cascade, so the loop can't clean up. The
  delete-cluster op first sets the tunnel to `removing` and waits up to
  2 minutes for the loop. After that, the leftovers are listed in the
  activity entry ("Remove the tunnel pyo-eu-1a2b3c in Cloudflare by hand").

## 12. Failure modes and rollback

| Failure | What happens | What Pyo does |
|---|---|---|
| Control plane down (Pyo Cloud outage, self-hosted restart) | Traffic keeps flowing: routes at Cloudflare, token in the cluster | Nothing to do |
| Token removed or revoked in Cloudflare | Traffic keeps flowing | "Connect Cloudflare again to change the tunnel." Switches are refused |
| Both cloudflared pods down | Cloudflare shows error 1033 | Health "down", alert after 5 minutes, pod reason shown |
| QUIC/7844 blocked outbound | cloudflared can't connect | Pod not ready. Docs: allow outbound TCP 7844 |
| Tunnel deleted in Cloudflare | 1033 until recreated | Loop recreates it, repoints Pyo's records, sends the new token |
| Routes edited in Cloudflare | Pyo's config is PUT back on the next pass | Docs: don't edit Pyo's tunnel in Cloudflare |
| Origin certificate revoked by hand | 502 from cloudflared | Rotate issues a new one. Issue failures shown on the status line |
| Agent too old (no `tunnel` capability) | Choice greyed out | "Not reported yet." |
| Cluster install older than the agent | Tunnel mode refused, or a running connector kept as it is | "Run the command under Refresh this cluster…" (see Decisions) |
| New cloudflared pod not yet trusted | Its requests get 403 for a few seconds | Pod watch updates the policy |
| Record conflict | Switch stops at "confirm" | User replaces or cancels |
| Cloudflare 429 or 5xx | Pass retried later | Backoff per team |

**Rollback paths**

- Per domain: **Switch back**. Records first (kept ones restored, or an A
  record to the public IP on a public-ports cluster), then the route goes.
  Proxied-to-proxied changes take effect at Cloudflare at once. A
  DNS-only kept record follows its TTL, while the tunnel route still
  answers until the next pass, so there's no gap.
- Per cluster: **Traffic › Public ports**. Envoy's Service goes back to
  LoadBalancer on the next heartbeat. Tunnel domains keep using the tunnel
  until switched back (both paths work at once), so the order is: public
  ports on, check, then Switch back per domain (or "Switch all back" in the
  confirm dialog), then Remove tunnel.
- Whole feature: an agent that ignores `tunnel` (N-1) keeps the connector
  it has. Removing the tunnel through the op is the way out. A release
  rollback leaves `cluster_tunnels` and `dns_records.via` unused (the
  `traffic` column with its generated `public_traffic` stays compatible).

## 13. Security

- **Token scope**: Zone:Read, DNS:Edit, Cloudflare Tunnel:Edit (account),
  SSL and Certificates:Edit (zone). Docs show how to limit it to one
  account and the zones Pyo may use. No Account Settings, no Zero Trust,
  no Workers.
- **Storage**: the API token stays sealed per team
  (`vault::seal_credential`, `cloudflare.rs:694`), opened only in the
  control plane, sent only to `api.cloudflare.com` through the pinned
  client. Never to the agent, never logged.
- **Tunnel token**: never stored by Pyo. Read from Cloudflare when needed,
  sealed in the queued command, wiped once answered (§7.1). In the cluster
  it's the Secret `pyo-system/pyo-tunnel` (k3s secrets encryption,
  secretbox, as connect.sh sets up). Whoever has it can serve the tunnel's
  hostnames, so it's treated like a certificate key. **Rotate** issues a
  new one and drops old connections. Rotating the cluster's agent token
  (Danger zone) also rotates it.
- **What the agent sees**: the tunnel token and the Origin CA certificate
  with its key (as for any uploaded certificate). Not the API token, not
  other clusters' tunnels, not zone data.
- **cloudflared pod**: no service account token, non-root, read-only root
  filesystem, all capabilities dropped, no host network or paths, nothing
  listens except metrics on 2000 (unreachable: pyo-system denies ingress).
- **Visitor trust**: only cloudflared's pod addresses may set the visitor
  on tunnel listeners, and anything else with a tunnel hostname gets 403
  (§8.3). Plus the `CF-Connecting-IP` fix (§8.4).
- **SSRF**: the only outbound host is `api.cloudflare.com`
  (`Http::call` refuses others). No user-given URLs.
- **Roles**: viewers see tunnel state. Developers switch a domain when
  nothing is in the way. Owners and admins replace records, change the
  cluster's traffic, remove or rotate the tunnel, and change the token.
- **Activity log** entries (`log_activity`) for: tunnel created, removed,
  rotated, traffic changed, each switch and switch back with the replaced
  records, certificate issued.

## 14. Pyo Cloud

**Works on Cloud** (checklist from works-on-cloud-too):

- No reliance on the control plane being in the customer's cluster: all
  cluster work goes through `TunnelSet`/`TunnelRemove` (signed commands)
  and the heartbeat reply (signed reply). The signer signs hashes only and
  never sees the tunnel token.
- Everything per team and per cluster: the token per team,
  `cluster_tunnels` per cluster with row security, records marked with the
  installation id and checked against stored ids.
- Secrets per team: the token sealed with the team's data key, the origin
  key sealed with the team's data key.
- No SSRF into Cloud's network: only `api.cloudflare.com`.
- Domain ownership (`Extensions::verify_domains`, `domainproof.rs`): when
  the team's token can edit DNS in the zone, Switch to Tunnel writes the
  `_pyo-verify.<host>` TXT record itself before claiming the name, so Cloud
  teams don't add it by hand.
- Held plans (`gate::check_queue`, called in `deploy.rs:1205`): `TunnelSet`
  is held like other changes. `TunnelRemove` and removing a tunnel are
  always allowed, so a team can always take its records back.
- Load on Cloudflare per team: each team's own token and rate limit.
- Cloud e2e: `pyo-cloud/scripts/e2e-multipass.sh` with the fake Cloudflare
  API (§17.3).

**Pyo Cloud's own tunnel** (cloud.pyo.dev and agent.pyo.dev on Pyo Ops)
could move to this once 0.51.0 runs on Pyo Ops:

1. In Pyo Ops (team frans), replace the Cloudflare token with one that also
   has Cloudflare Tunnel:Edit and SSL and Certificates:Edit on pyo.dev.
2. Cluster `cl_eavb3paccm12` › Traffic › Cloudflare Tunnel, no default
   domain (decision 8: a wildcard on pyo.dev would catch unknown names).
   The default domain is optional (decision 4).
3. Switch cloud.pyo.dev and agent.pyo.dev to the tunnel. Pyo shows the
   Terraform CNAMEs to the old tunnel and asks. After the switch, traffic
   goes through Pyo's tunnel. The old one stays up until removed, so this
   is zero downtime.
4. In pyo-cloud:
   - `deploy/terraform/cloudflare/tunnel.tf`: delete the tunnel, its
     config, its token data source and the DNS records
     (`terraform state rm` the records first so Terraform doesn't delete
     Pyo's).
   - `deploy/terraform/cloudflare/main.tf:37-66`: delete the origin key,
     CSR and `cloudflare_origin_ca_certificate`. Keep zone settings, cache
     and WAF rulesets, R2 buckets.
   - `variables.tf`: drop `tunnel_name`, `gateway_service`.
     `outputs.tf`: drop the token and certificate outputs.
   - `deploy/pyo-ops/compose.yaml:133-149`: remove the `cloudflared`
     service (and `TUNNEL_TOKEN` from setup).
   - `scripts/ops/setup.sh`: remove the `certificate` step (`:174-182`) and
     the origin certificate on cloud-api's domains (`:284-291`).
   - README: Cloudflare section points at the dashboard.
5. Two open ops items close: "spreading cloudflared across both servers"
   (2 replicas, spread) and "checking real client IPs in the audit log"
   (§8.3, §8.4).

Cloud customers get the feature with no Cloud-only code. The pyo-cloud
repo needs no change for them.

## 15. connect.sh

Done in 0.5.5 (`8545c8f`, `378fa72`): connect.sh finds Traefik or another
program on 80 or 443, sends `&webPorts=busy` with the manifest request,
leaves the firewall as it is, and the API turns public traffic off before
the agent's first heartbeat (`install::web_ports_busy`). In 0.51.0:

- `web_ports_busy` writes `traffic = 'tailscale'` (the generated
  `public_traffic` can't be written).
- connect.sh's line (`web_ports_line`) becomes
  `Ports 80 and 443 are in use by Traefik. Public traffic stays off, so your sites keep working. Cloudflare Tunnel can take your websites instead.`

The dashboard then shows on the new cluster: "Ports 80 and 443 are taken
on this server. Choose Cloudflare Tunnel or Tailscale only." with a link to
the Traffic panel.

## 16. Docs pages (pyo-docs)

New:

- `content/(core)/cloudflare-tunnel.mdx` (`/docs/cloudflare-tunnel`):
  draft in §20.

Changed:

- `cloudflare.mdx`: "Create the token" adds the two permissions and a
  scoped-token example. New short section "Cloudflare Tunnel" linking to
  the new page.
- `clusters.mdx` "## Public traffic" (`:51`) becomes "## Traffic" with the
  three choices. Keep a `#public-traffic` anchor (gateway.mdx links it).
- `gateway.mdx`: "How a request travels" (`:8`) adds the tunnel path.
  "Compared with Traefik" (`:75`) adds "runs next to Traefik in tunnel
  mode". Visitor address through the tunnel. Mentions cloudflared by name
  only here and in the system reference.
- `domains.mdx`: "Custom domains" adds via Tunnel, Switch to Tunnel and
  Switch back. "Free domains" adds the default domain in tunnel mode.
  "Public TCP and UDP ports" adds the tunnel-mode switch.
- `firewall.mdx` "Behind Cloudflare" (`:58`): tunnel domains don't need it.
  Rules see the real visitor.
- `certificates.mdx`: the Cloudflare Tunnel certificate Pyo makes (15
  years, a new one on Rotate).
- `system-reference.mdx` "What runs on your server" (`:143`): `pyo-tunnel`
  (cloudflared, 2 replicas), `pyo-tunnel-origin`, `pyo-public-ports`.
  "Connections" (`:41`): outbound TCP 7844 to Cloudflare in tunnel mode.
- `hardening-your-server.mdx`: in tunnel mode, close 80 and 443 in the
  cloud firewall.
- `tailscale.mdx`: Tailscale only as one of the three choices.
- `cli/commands.mdx`: `pyo cloudflare tunnel …`, `pyo domain tunnel …`.
- `mcp/tools.mdx`: the new tools (generated from the API if that page is
  generated).
- New how-to section on the tunnel page: "Move sites from Traefik with no
  downtime" (the misc-ten-sin4 path).

Copy rules for all of it: "Pyo Gateway", "servers", "services", plain
sentences, no semicolons, no dash-joined clauses.

## 17. Test plan

### 17.1 Unit

- **pyo-api `cloudflare/tests.rs`** (fake `Cloudflare`):
  - zones read their account, tunnel check per account, only this
    installation's old `pyo-check-<install id>-` tunnels deleted, Origin
    CA read check per zone,
    `problems()` lines.
  - `tunnel::ingress()`: order, no wildcard, catch-all, `matchSNItoHost`,
    hash stable across runs.
  - certificate names: apex, one level, two levels, default domain, dedupe,
    200 cap.
  - switch: create, conflict → confirm, replace keeps `previous`, switch
    back restores exactly (type, content, proxied, ttl, comment), switch
    back without kept record (public IP / delete only).
  - only records Pyo made are deleted (`ours`), tunnels Pyo didn't make
    are never deleted.
  - recreate after `deleted_at`: records repointed, token resent.
  - removal order: catch-all only, records, agent, connections, tunnel,
    certificate.
  - `removeCloudflare` refused with a tunnel.
  - secret handling: `tunnel_set` token sealed in `pyo.commands`, wiped
    after the answer (extend the existing teambackup tests).
- **pyo-api `ops.rs`**: `setClusterTraffic` roles, validation messages,
  `setClusterPublicTraffic` mapping, generated `public_traffic`.
- **pyo-proto `system.rs` / `rbac.rs`**: tunnel objects pass the admission
  policy, origin and public-ports Services pinned (wrong selector, port 80,
  extra externalIPs refused), CDN client policy with label `tunnel` and
  `/32` entries allowed, more than 128 refused.
- **pyo-agent `gateway.rs`** (pattern of `behind_cloudflare_routes`,
  `:2802`): tunnel domain gets its own route, the gate rule, the client
  policy with pod IPs, no :80 route, the `cf-visitor` redirect, the custom
  certificate. Mixed service (one tunnel, one direct domain). Pod IP change
  rewrites only the client policies.
- **pyo-agent `tunnel.rs`**: Secret label → report, Deployment applied
  only with matching Secret, `TunnelRemove` deletes all three objects.
- **Heartbeat N/N-1**: a reply without `tunnel` leaves the connector, an
  old heartbeat without `tunnel` parses.

### 17.2 e2e (no Cloudflare account)

- `scripts/tunnel-gate-e2e.sh`, like `scripts/cdn-gate-e2e.sh`: a real
  Envoy Gateway in k3s, a pod labelled as the connector sending requests
  with X-Forwarded-For and `cf-visitor`, a second pod doing the same
  without the label. Checks: visitor address in the access log, country
  rule, rate limit per visitor, 403 for the unlabelled pod, redirect on
  `{"scheme":"http"}`, WAF block by visitor. Run after every Envoy Gateway
  upgrade.
- A local cloudflared against `pyo-tunnel-origin` with a
  `--config` ingress using `matchSNItoHost` and an Origin-CA-shaped test
  certificate (test CA in `caPool`): proves the wildcard SNI and
  verification path without Cloudflare.
- Coexistence: k3s with Traefik on 80/443 (default k3s), connect Pyo,
  tunnel mode, Envoy ClusterIP, no pending svclb pods, Traefik still
  serving.

### 17.3 Fake Cloudflare API

A debug-only override `PYO_DEV_CLOUDFLARE_API` (like `PYO_DEV_TXT_FILE`),
refused in release builds, pointing `Http` at a small fake in
`scripts/fake-cloudflare/` that keeps zones, records, tunnels and
certificates in memory. Used by the dashboard e2e and the Cloud e2e
(`pyo-cloud/scripts/e2e-multipass.sh`): a Cloud team enables a tunnel and
switches a domain, the agent runs a stub connector image, everything
through signed commands.

### 17.4 Real Cloudflare (only with the user's approval)

- Only on a throwaway test zone the user names and approves, with a
  token made for the test. Never pyo.dev, never the user's existing
  records or tunnels, never misc-ten-sin4 (tests never touch user
  resources).
- Test machine: dev-ten-sin0, only after asking (the user tests their own
  install there).
- Checks: permission checks report the right missing permission for each
  of four tokens (each missing one permission), tunnel up with 2
  connectors in two colos, Switch to Tunnel and Switch back on a record
  the test made first, real visitor IP in access log and audit log,
  removal leaves the zone as before (diff the record list), token rotate,
  tunnel deleted by hand gets recreated, the 100 MB and 100 s limits as
  documented.
- Confirm the exact paths and shapes marked in §6.1 (token endpoint,
  PATCH rotation, `deleted_at`).

## 18. Build plan

Built on one branch, `feat/cloudflare-tunnel` (worktree `pyo-tunnel`),
merged to `dev` only when the feature is complete and tested. Rust builds
one at a time under the laptop guardrails. Each phase ends green and
shippable behind the capability flag.

**Phase 0 (0.5.5, separate fixes, done)**

- connect.sh 80/443 preflight (`connect.sh:1237-1243`).
- Remove client-sent `CF-Connecting-IP` at the Gateway (§8.4):
  `system.rs:1276-1300` (client policy), `rbac.rs` client policy variants,
  tests in `system.rs:2149-2207`.

**Phase 1: token and zones**

- `crates/pyo-api/src/cloudflare.rs`: `Zone` (`:50-60`) gains account and
  `tunnel`. `Cloudflare` trait (`:119-128`) gains tunnel and certificate
  calls. `zones()` (`:404-424`) reads `account`. `inspect` (`:262-287`) and
  `problems` (`:290-304`) check tunnel and Origin CA. `id_ok` (`:322`)
  accepts UUIDs. `remove` (`:751`) refuses with a tunnel.
- `crates/pyo-api/src/cloudflare/tests.rs`: fake grows.
- `crates/pyo-api/src/ops.rs:2937-2942`: op doc comments name the new
  permissions.
- `dash/src/components/pyo/Cloudflare.tsx:37-240`: hint and badges.
  `dash/src/lib/pyo/types.ts`: zone type.
- `crates/pyo-cli/src/cloudflare.rs`: token message and `test` columns.

**Phase 2: agent and Gateway**

- `crates/pyo-proto/src/lib.rs`: `Command::TunnelSet`/`TunnelRemove`
  (after `:1275`), `Heartbeat.tunnel` (`:87`), `Capabilities.tunnel`
  (`:483`), `HeartbeatReply.tunnel` (`:2610-2650`), `DomainSpec.tunnel`
  (`:2250-2264`).
- `crates/pyo-proto/src/system.rs`: `CLOUDFLARED_IMAGE`,
  `tunnel_objects`, `TUNNEL_ORIGIN_SERVICE` (near `:1150-1216`),
  `PUBLIC_PORTS_SERVICE`.
- `crates/pyo-proto/src/rbac.rs`: Service pins (`:268`, `:649-660`,
  `:1332`), `"tunnel"` label for CDN client policies (`:547-552`, `:680`),
  public ports rule (`:701`), tests.
- `crates/pyo-agent/src/tunnel.rs` (new), `main.rs:268-269` (reply
  handling next to public traffic), `main.rs:583` (commands),
  `actions.rs:160`.
- `crates/pyo-agent/src/gateway.rs`: tunnel listeners and routes
  (`:311-380`), gate and policies (`:411-484`), pod watch and policy
  rewrite (`:1914`), origin Service in `ensure_once` (`:1835`), tests
  near `:2802` and `:3432`.
- `crates/pyo-api/src/teambackup.rs:465-519`: seal and wipe `tunnel_set`.
- `crates/pyo-api/src/routes.rs:19-43`: `tunnel` into `DomainSpec`.

**Phase 3: control plane**

- `crates/pyo-api/migrations/0039_cloudflare_tunnel.sql` (§4), row
  security.
- `crates/pyo-api/src/cloudflare/tunnel.rs` (new): reconcile, ingress,
  certificates, status, removal. Registered from `cloudflare.rs` (`mod
  tunnel;`).
- `crates/pyo-api/src/bg.rs:69-85`, `:260`: `LOCK_TUNNEL`. Task started
  with the worker tasks (next to `deploy::dns_watcher`, `deploy.rs:2531`).
- `crates/pyo-api/src/agent.rs:699-733`: reply `tunnel`. Heartbeat
  handling stores `agent_status`.
- `crates/pyo-api/src/snapshot.rs:140-170`: `traffic`, `tunnel`.
  `cloudflare::attach` (`cloudflare.rs:591-621`): `via`, `previous`.
- `crates/pyo-api/src/deploy.rs:2547`: skip tunnel domains in DNS checks.
- `crates/pyo-api/src/alerts.rs`: `cluster.tunnel_down`/`_up` events.

**Phase 4: traffic choice, switching, default domain**

- `crates/pyo-api/src/ops.rs`: `SetClusterTraffic`, `SetDomainTunnel`,
  `RemoveClusterTunnel`, `RotateTunnelToken` (enum near `:2937`, folders
  `:3438`/`:3482`, handlers near `:4736` and `:8202`).
  `SetClusterPublicTraffic` handler writes `traffic` (`:4736-4764`).
- `GET /clusters/{id}/tunnel` in `ops::routes()` (`ops.rs:71-109`).
- `crates/pyo-api/src/freedomain.rs:69-89`, `:156-197`: `Mode::Tunnel`.
- `crates/pyo-api/src/cloudflare.rs:625-652`: `forget_removed` → reverting.
- `crates/pyo-api/src/mcp.rs:158`, `:1289-1310`: `add_domain` `tunnel`.
- Dashboard:
  - `dash/src/components/pyo/TrafficMode.tsx:77-114` → Traffic panel.
  - `dash/src/routes/_app/$team/clusters/$clusterId/settings.tsx:120`.
  - `dash/src/components/pyo/Cloudflare.tsx:242-408`: fields and record
    line variants, `DeleteRecordSwitch` (`:410`).
  - `dash/src/components/pyo/AddDomainDialog.tsx:86-183`, `:332`.
  - `dash/src/lib/pyo/store.ts:1501-1506` (new `setClusterTraffic`,
    `setDomainTunnel`, `removeClusterTunnel`, `rotateTunnelToken`),
    `:2575` area.
  - `dash/src/lib/pyo/types.ts:391-430`, `:907`.
  - Merge to `dev` in the main checkout so the user sees it on :5173
    (type-check with `dash/node_modules/.bin/tsc`, no pnpm while :5173
    runs).
- CLI: `crates/pyo-cli/src/main.rs:267-508` (enums),
  `crates/pyo-cli/src/cloudflare.rs`, `crates/pyo-cli/src/domain.rs`.

**Phase 5: ports, connect.sh, alerts**

- `pyo-public-ports` Service (§8.5) in `gateway.rs` with its rbac pin.
- `crates/pyo-api/assets/connect.sh` `web_ports_line`: mention the tunnel.
- `crates/pyo-api/src/install.rs` `web_ports_busy`: set `traffic`.

**Phase 6: docs, tests, Pyo Ops**

- pyo-docs pages (§16).
- `scripts/tunnel-gate-e2e.sh`, `scripts/fake-cloudflare/`, Cloud e2e.
- Real-zone test with the user's approval (§17.4).
- After 0.51.0 is on Pyo Ops: move cloud.pyo.dev and agent.pyo.dev
  (§14), then the pyo-cloud cleanup commit.
- Release notes (Features: Cloudflare Tunnel). The CF-Connecting-IP fix
  shipped in 0.5.5.

## 19. Open questions (answered)

All answered on 2026-11-22. See Decisions at the top. Kept for the
record.

1. **Origin certificate key**: made by the control plane and kept sealed
   with the team's certificates (like preview wildcards, less code), or
   made in the cluster with only the CSR sent up (key never leaves the
   cluster, two more commands)? Spec picks the control plane.
2. **Origin certificate validity**: 1 year renewed at 30 days (spec), or
   15 years like Pyo Cloud's Terraform with no renewals?
3. **TCP and UDP ports in tunnel mode**: published on the servers by
   default (spec), or off by default for a "no open ports" promise?
4. **Default domain**: only a zone apex with `*.zone` (spec, works with
   free Universal SSL), or also a subdomain with a warning about Advanced
   Certificate Manager? Should it be optional when every domain is
   switched by hand (needed for Pyo Ops)?
5. **Removing a domain** that replaced a record: put the old record back
   by default (spec), or leave the name empty?
6. **One Cloudflare account per tunnel**: fine for 0.51.0? Teams with zones
   in two accounts would need two tokens or a second cluster.
7. **Pyo's own dashboard through the tunnel** (e.g. pyo.example.com on the
   cluster Pyo runs on): in 0.51.0 or later?
8. **Pyo Ops**: move cloud.pyo.dev and agent.pyo.dev to this right after
   0.51.0, and with or without a default domain on pyo.dev?
9. **HTTPS redirect** through `cf-visitor` at the Gateway (spec), or ask
   users to turn on Always Use HTTPS in Cloudflare?
10. **Who may switch a domain**: developers when nothing is in the way,
    admins to replace (spec, same as DNS records today)?
11. **CF-Connecting-IP fix** (§8.4): ship in 0.5.5 as a security fix, or
    with 0.51.0?
12. **Release number**: 0.51.0 as the first feature release after 0.5.x
    (versioning memory notes this wasn't confirmed)?
13. **`pyo deploy` uploads** to cloud.pyo.dev already pass Cloudflare's
    100 MB limit on Free. Is the zone on a plan that allows the ~500 MB
    folder upload, or should uploads be chunked?

## 20. Draft docs page: `/docs/cloudflare-tunnel`

```mdx
---
title: Cloudflare Tunnel
description: Take websites in through Cloudflare with no open ports on your servers.
---

With Cloudflare Tunnel, your servers open no ports for websites. A small
connector in your cluster dials out to Cloudflare, and Cloudflare sends
your visitors through it to Pyo Gateway. Pyo sets up the tunnel, its
routes, the DNS records and the certificates.

It also lets Pyo run on a server where another program already uses ports
80 and 443, like Traefik or nginx.

## What you need

- Your domains in a Cloudflare account.
- A [Cloudflare token](/docs/cloudflare) with these permissions:
  - Zone › Zone › Read
  - Zone › DNS › Edit
  - Zone › SSL and Certificates › Edit
  - Account › Cloudflare Tunnel › Edit
- Your servers can reach Cloudflare on TCP port 7844.

## Turn it on

1. Open your cluster's **Settings** and find **Traffic**.
2. Pick **Cloudflare Tunnel**.
3. Pick a **default domain** if you want one. Services get names under
   it, like `web.example.com`.
4. Select **Switch**.

Pyo creates the tunnel and starts two connectors on different servers.
Domains in your Cloudflare account move to the tunnel. If a name has a
record Pyo didn't make, Pyo asks first. Domains in other accounts stop
answering from the internet.

## Move one domain

On a service's **Domains** tab, select **Switch to Tunnel**. Pyo adds the
route first, then points the name at the tunnel. If Cloudflare already has
a record for the name, Pyo shows it and asks before replacing it. Pyo keeps
a copy, so **Switch back** puts it back.

## Move sites from Traefik with no downtime

1. Connect the server to Pyo. Pick **Cloudflare Tunnel**.
2. Deploy each site as a service in Pyo and add its domain.
3. Select **Switch to Tunnel** on the domain. Traefik keeps serving until
   Cloudflare points at the tunnel.
4. Check the site. If something's wrong, select **Switch back**.
5. When every site has moved, stop Traefik.

## Visitors

Pyo trusts the visitor's address only when it comes from the connectors.
Firewall rules, rate limits, the WAF, country rules, analytics and the
audit log all see the real visitor.

## TCP and UDP ports

Cloudflare Tunnel carries websites only. Public TCP and UDP ports stay on
your servers' public addresses, or on Tailscale. They don't go through
the tunnel.

## Cloudflare's limits

- Uploads over 100 MB fail on the Free and Pro plans.
- Cloudflare waits 100 seconds for a response to start.
- Plain HTTP visitors are sent to HTTPS.

## Turn it off

Pick **Public ports** or **Tailscale only**. Then select **Switch back** on
each domain, and **Remove tunnel**. Pyo deletes the tunnel and only the
records it made.
```

## 21. Build status

Kept current as each phase lands, so the work can be picked up from here.
Branch `feat/cloudflare-tunnel`, worktree `/home/frx/workspace/upsetdev/pyo-tunnel`.
Not merged into `dev` until the feature is complete and tested.

| Phase | State |
|---|---|
| Decisions applied to this spec | Done (b145440, 5284a70, bf1cc78) |
| 1. Token and zones | Done, tested (e3d91dd api, adc8e62 cli, 12cf81f dash) |
| 2. Agent and Gateway | Done, tested (eadce05 proto, 4db8b6f agent) |
| 3. Control plane | Done (3b9ae43): migration 0040, tunnel_loop.rs, heartbeat and snapshot. Unit tested, and end to end against the fake Cloudflare and a throwaway Postgres in CI |
| 4. Traffic choice, switching | API, MCP and CLI written (3b9ae43, a47f92e). Dashboard merged (06dfbba, `tsc` passes): Traffic panel, tunnel badge, record line, add and remove domain |
| 5. Ports, connect.sh, alerts | Public ports Service and tunnel alerts written. connect.sh keeps its line (see Decisions) |
| 6. Docs, e2e, gaps, free domains | Done except the real-zone test and Pyo Ops (both need the user): fake Cloudflare and the loop test (0394f36), gaps and free domains (e36141e, d135ff0), gate e2e written, not run (d8e1a16), pyo-docs branch `docs/cloudflare-tunnel` (011a38f, ec36fd2) |

Where things are:

- `crates/pyo-api/src/cloudflare/tunnel.rs`: the Cloudflare tunnel client
  (`TunnelApi`), token checks, routes, certificate names.
- `cloudflare/tunnel_loop.rs` (+ `tunnel_loop_tests.rs`): the loop,
  switching and reverting records, certificates, health, removal, the
  snapshot's `tunnel`.
- `cloudflare/tunnel_ops.rs`: the ops and `GET /clusters/{id}/tunnel`.
- `crates/pyo-agent/src/tunnel.rs`: the connector, its report, trusted
  pod addresses. `gateway.rs`: tunnel routes, policies, public ports
  Service.
- `crates/pyo-proto/src/system.rs` (Cloudflare Tunnel section), `rbac.rs`
  (origin and public ports Services, `tunnel` client policies).

Tests (2026-11-22): the full pyo-proto (167), pyo-agent (216), pyo-api
(611) and pyo-cli (80) suites pass, except `openapi::docs_copy_is_current`,
which compares with pyo-docs' `openapi/pyo.json` (regenerate it with the
docs in Phase 6). `cargo clippy -p pyo-proto -p pyo-agent -p pyo-api
-p pyo-cli --tests -- -D warnings` is clean on 928ac34, and the API and
CLI tunnel tests pass on it.

Phase 6 checks (2026-11-22): `cargo clippy -p pyo-api -p pyo-cli --tests
-- -D warnings` clean, pyo-api 617 passed (only `docs_copy_is_current`
red), pyo-cli 80 passed. `scripts/tunnel-gate-e2e.sh` passes all 11
checks against Envoy 1.39.5. Previews in tunnel mode go under the
default domain (3cb5c75).

Fixes after the real-Cloudflare test (2026-11-22, branch
`worktree-agent-a1e523da2ad53df3d`, see "Decisions after the
real-Cloudflare test"):

| Bug | Fix | Tested by |
|---|---|---|
| The probe deleted every `pyo-check-*` tunnel | Probe named for the installation, only its own old ones deleted (`tunnel::tunnel_missing`), DNS probe text per installation | `http_tests::a_check_deletes_only_this_installations_old_probes` (fake Cloudflare API), `tests::only_this_installations_old_probe_tunnels_are_deleted`, `tests::another_installations_probe_record_is_left_alone` |
| Switch back undone by the wildcard | Refused under the default domain (`tunnel_ops::back_plan`), dashboard and CLI follow | `tunnel_ops::tests::switch_back_is_refused_under_the_default_domain`, `tsc` |
| 502 for names no service has | No wildcard route (`tunnel::ingress`) | `tunnel::tests::a_name_no_service_has_stops_at_the_404_before_the_cluster`, `routes_are_exact_names_then_404`, the loop's end-to-end test (CI) |
| X-Forwarded-For ended with the connector's pod | Tunnel routes set it to the visitor (`system::tunnel_rule`) | `gateway::tests::tunnel_domains_switch_then_close_the_direct_path`, `rbac::tests::a_static_sites_routes_may_only_name_its_folder`, `scripts/tunnel-gate-e2e.sh` (17 checks, Envoy 1.39.5) |
| The gate opened on a redundant switch | A name already `"on"` stays `"on"` (`tunnel_ops::switch_state`) | `tunnel_ops::tests::a_name_already_on_the_tunnel_stays_gated_while_its_record_is_made` |

The admission policy changed (HTTPRoute header filter), so the access
hash moves and a cluster connected before this needs **Refresh this
cluster** before its tunnel routes are accepted. The roles hash
(`the_roles_are_as_0_51_0s`) is unchanged.

Checks (2026-11-23): `cargo clippy -p pyo-proto -p pyo-agent -p pyo-api
-p pyo-cli --tests -- -D warnings` clean. Suites pass: pyo-proto 171,
pyo-agent 221, pyo-api 655, pyo-cli 81. `scripts/tunnel-gate-e2e.sh`
passes all 17 checks, and its six new ones fail without the header fix.
The loop's end-to-end test still needs CI's Postgres.

Not checked against a real Envoy Gateway or Cloudflare: that Envoy
Gateway 1.9.3 accepts the route's header value and hands it to Envoy as
it is (read from its source, not run), and Cloudflare's 404 for a name
with no route.

Next:

1. Merge into `dev` when asked. Pyo writes Cloud's domain proof when a
   domain is added (the user may change that).
2. With the user's OK: the real-zone test (§17.4) on a throwaway zone.
3. After 0.51.0 merges: regenerate pyo-docs' generated pages, merge
   pyo-docs' `docs/cloudflare-tunnel`, then the Pyo Ops move (§14).
4. Dashboard: no warning for deeper names (a.b.example.com) when
   switching, and "Certificate: <reason>" shows only through `tunnel.error`.

Known gaps:

- Nothing tested against a real Cloudflare account (needs the user's
  approval and a throwaway zone, §17.4).
- The loop's end-to-end test (`cloudflare::http_tests::the_tunnel_loop_end_to_end`)
  needs a throwaway Postgres: CI runs it (ci.yml's Postgres job), it
  wasn't run on the laptop.
- pyo-docs' generated pages (`openapi/pyo.json`, `content/cli/commands.mdx`,
  `content/mcp/tools.mdx`) are regenerated once all 0.51.0 branches are
  merged. `openapi::tests::docs_copy_is_current` stays red until then.
- `dash/node_modules` in the worktrees is a symlink to the main
  checkout's, for `tsc` only (ignored by git).
