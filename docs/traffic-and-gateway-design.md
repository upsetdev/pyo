# Traffic paths and the Gateway section

Design for two cluster features, target release **0.52.0**:

1. **Traffic paths.** The cluster's Traffic card becomes three switches
   (Public ports, Cloudflare Tunnel, Tailnet) instead of one radio
   choice. Each domain takes one of the paths that are on.
2. **Gateway section.** A panel next to the Traffic card with the
   gateway's status per server and a few cluster-wide settings: HTTP/3,
   compression, timeouts and the TLS minimum version.

Written 2026-11-27 against `dev` at ae4f613. File and line references are
to that commit. It assumes the change that makes every website route
always carry a SecurityPolicy and a BackendTrafficPolicy, so changing a
firewall rule never changes a filter chain.

## Contents

1. Scope
2. How traffic gets in today
3. What cuts open connections
4. Traffic paths: behaviour
5. Traffic paths: UI
6. Traffic paths: API
7. Traffic paths: data and migration
8. Traffic paths: control plane and agent
9. Traffic paths: connection cuts
10. Gateway section: status
11. Gateway section: settings
12. Gateway section: UI
13. Gateway section: API
14. Gateway section: agent and admission policy
15. Gateway section: connection cuts
16. Tests
17. Docs
18. Open questions
19. Estimate
20. How the Envoy Gateway facts were checked

---

## 1. Scope

- **Per cluster.** Both features are settings of one cluster.
- **Roles.** Team owners and admins change them. Developers and viewers
  see them read-only: switches are disabled and no Save button shows.
  This is the role split the cluster settings already use
  (`ops.rs`, `require_role` with `Role::Admin`).
- **Where.** Self-hosted and Pyo Cloud customer clusters work the same.
  Everything goes through the heartbeat reply and signed commands, as the
  tunnel does (`docs/cloudflare-tunnel.md` §14).
- **Shared clusters.** Every cluster belongs to exactly one team
  (`pyo.clusters.team_id`, `cloud/mod.rs`: "clusters belong to a team").
  No code path lets two teams' services share a cluster, on Pyo Cloud
  either. So both settings only ever touch the owning team's websites.
- **Older installs.** Both features need new admission policy rules
  (`crates/pyo-proto/src/rbac.rs`). A cluster whose install is older
  refuses the change with the existing message: "Run the command under
  Refresh this cluster in the cluster's settings first." Same gate as
  tunnel mode (`install::install_state`).

## 2. How traffic gets in today

One Envoy fleet serves every Gateway (`mergeGateways`, `system.rs:682`).
The relevant pieces:

| Entry | Kubernetes object | Envoy listener |
|---|---|---|
| Public 80 and 443 on each server | `EnvoyProxy.envoyService`: LoadBalancer with public traffic, ClusterIP without (`system.rs:750`). ServiceLB runs an `svclb-*` pod per server | 10080 (plain HTTP, one shared filter chain) and 10443 (one filter chain per HTTPS hostname, matched by SNI) |
| Cloudflare Tunnel | `pyo-tunnel-origin` ClusterIP Service → 10443 (`system.rs:1826`) | 10443, the domain's own filter chain with the tunnel client policy |
| Websites on the servers' Tailscale addresses | `pyo-tailnet-web` Service with externalIPs (`system.rs:1233`), only while free domains point at the Tailscale IP | 10080 and 10443 |
| ts.net names (Tailscale Services) | `pyo-tailnet-vip` Service on the VIPs | 7080 and 7443 on the `pyo-tailnet` Gateway |
| Public TCP and UDP ports | On the Envoy Service with public traffic. In tunnel mode, `pyo-public-ports` (`system.rs:1842`) | one listener per port |

The Traffic card (`TrafficMode.tsx:106`) sets `clusters.traffic`
(`public`, `tailscale` or `tunnel`). The tunnel connector runs whenever
`pyo.cluster_tunnels` has a row that isn't `removing` (`agent.rs:777`),
whatever the traffic value. So "public ports and some domains through the
tunnel" already works today: it's `traffic = 'public'` with a tunnel row,
the state a cluster is in while it switches back (cloudflare-tunnel.md
§12).

## 3. What cuts open connections

Envoy's rules (Envoy LDS docs, "filter chain only update"), with Pyo's
drain settings (`shutdown.drainTimeout: 6s`, which Envoy Gateway passes
as `--drain-time-s 6` with `--drain-strategy immediate`,
`infrastructure/common/proxy_args.go` in v1.9.3):

| Change | Effect on open connections |
|---|---|
| Routes only (RDS) | None |
| A new listener | None |
| One filter chain changes (its HTTP filters, its TLS settings, its client policy) | Only that chain's connections drain. One HTTPS chain is one hostname |
| A listener-wide field changes (listener filters such as PROXY protocol) | Every connection on that listener drains |
| ServiceLB's `svclb-*` pod on a server is replaced | Expected: every connection through it ends, and new connections to that server fail until the new pod is up. Not measured yet (§16) |

A drain means: Envoy asks clients to close at once (HTTP/2 GOAWAY,
`Connection: close` on HTTP/1.1) and closes what's left after 6 s.
Browsers and HTTP clients open a new connection for their next request.
WebSockets, long polls, gRPC streams and uploads still running at 6 s are
cut.

Rule for both features: a change that drains a listener or filter chain
says so in its confirm dialog before it's saved, and names what drains.

---

## 4. Traffic paths: behaviour

### 4.1 The three paths

| Switch | On means | Off means |
|---|---|---|
| **Public ports** | The proxies are published on ports 80 and 443 of every server except build-only ones (Envoy's Service is a LoadBalancer). Today's `traffic = 'public'` | Envoy's Service is ClusterIP. Nothing listens on the servers' 80 and 443 |
| **Cloudflare Tunnel** | The cluster has a tunnel and runs its connector (`cluster_tunnels` row) | No tunnel. Turning it off removes it, like today's Remove tunnel |
| **Tailnet** | Services' websites answer on the tailnet: their ts.net names, and the servers' Tailscale addresses when free domains point there | Services' ts.net names and the Tailscale addresses don't answer websites |

**Tailnet is not joining.** Once a team connects Tailscale, every cluster
of the team is on the tailnet, with no per-cluster opt-out. The Tailnet
switch only decides whether the gateway serves websites there. Turning it
off leaves the servers on the tailnet, k3s's traffic between servers, the
registry and Pyo's own names (`pyo.<tailnet>.ts.net`, the registry,
sandboxes) as they are. The switch is disabled with "Connect Tailscale on
Integrations first." while the team has no Tailscale.

Label: **Tailnet**. Hint: "Websites on ts.net names, for devices on your
tailnet. Servers stay on the tailnet when this is off." The word
"Tailscale" stays for the integration and the domain kind, so the switch
doesn't read as "join Tailscale".

### 4.2 Each domain's path

A domain has exactly one path. It follows from what it is today:

| Domain | Path |
|---|---|
| A ts.net name | Tailnet, always |
| A free nip.io name on the Tailscale IP | Tailnet |
| A free nip.io name on the public IP | Public ports |
| A name under the tunnel's default domain | Cloudflare Tunnel, always (cloudflare-tunnel.md, Decisions) |
| Your own domain with `tunnel: "on"` or `"switching"` | Cloudflare Tunnel |
| Your own domain otherwise | Public ports |

A domain whose path is off is **not served**. It stays in the service
with its settings and shows "Not served. Public ports are off on eu." (or
Cloudflare Tunnel, or Tailnet). Turning the path back on serves it again.
A switch never deletes a domain.

Your own domain on the tailnet path (DNS pointing at Tailscale addresses)
is out of scope (open question 5).

### 4.3 States

| Public | Tunnel | Tailnet | Example |
|---|---|---|---|
| on | off | on | Today's default |
| on | on | on | Some domains direct, some through the tunnel |
| off | on | on | Tunnel only: no public web ports |
| off | off | on | Tailnet only: private |
| on | on | off | Public and tunnel, nothing on the tailnet |
| on | off | off | Public only |
| off | on | off | Tunnel only, nothing on the tailnet |

**At least one path stays on.** Turning off the last one is refused:
"Turn on another path first. A cluster needs one." Tailnet counts as on
even before the team connects Tailscale, which keeps today's "Tailscale
only" with no Tailscale ("Only inside the cluster until you set up
Tailscale.").

### 4.4 Turning a path off while domains use it

The op answers `status: "confirm"` with the domains on that path, and
changes nothing until it's sent again with `confirm: true`. The dialog
lists them (service, hostname), at most 10 with "and 4 more".

- **Public ports off, Tunnel on.** Domains in a tunnel-ready zone of the
  tunnel's account can move first. The dialog has a switch "Move the 6
  domains in your Cloudflare account to the tunnel first", on by default.
  Pyo switches them (each goes `switching`, then `on`) and closes the
  public ports only once every moved domain is `on`. Domains waiting for
  "Replace and switch" (someone else's record) hold the close and are
  listed. So no visitor of a moved domain meets a closed port. The rest
  stop answering from the internet, as today.
  This is a change from today's switch to tunnel mode, which closes the
  ports at once while domains are still `switching`.
- **Public ports off, Tunnel off.** The listed domains stop answering
  from the internet.
- **Tunnel off, Public ports on.** Same order as today's rollback
  (cloudflare-tunnel.md §12): each tunnel domain switches back (records
  restored when kept, else an A record to the public IP), then the tunnel
  is removed. The dialog keeps "Put back the records it replaced" (on by
  default). Names under the default domain have no direct record: they
  stop answering and are listed so.
- **Tunnel off, Public ports off.** Tunnel domains stop answering. The
  tunnel is removed.
- **Tailnet off.** ts.net domains, and free domains on the Tailscale IP,
  stop answering.

The public dashboard on the cluster Pyo runs on closes with Public ports
off, as today. The dialog keeps that line.

### 4.5 TCP and UDP ports and PROXY protocol

These two move into the Traffic card and attach to Public ports, since
both are about direct connections to the servers.

- **TCP and UDP ports** (today's "Public TCP and UDP ports" in tunnel
  mode, `Tunnel.tsx:244`). Shown only when the cluster has a service with
  public TCP or UDP ports.
  - With Public ports on, they're always published (as today). The row
    reads "Published with the websites." and has no switch.
  - With Public ports off, a switch: "Publish TCP and UDP ports", "On
    every server except build-only servers." It's a cluster setting now,
    not the tunnel's, so it works in the Tailnet-only state too.
  - When Public ports goes off, its confirm dialog shows this switch, on
    by default when the cluster has such ports. This keeps today's
    default for tunnel mode.
- **PROXY protocol** (today's switch in Traffic routing,
  `TrafficMode.tsx:86`). An indented switch under Public ports: "Accept
  PROXY protocol", "Only if every connection comes through a load
  balancer that sends it." It drains every web connection once (§9).

### 4.6 New domains

In the add domain dialog the path is picked from the paths that are on:

- Your own domain: **Direct** or **Cloudflare Tunnel**, when both are on
  and the hostname's zone is tunnel-ready. Default: Cloudflare Tunnel when
  Public ports is off, Direct otherwise.
- Free domain: on the public IP, or under the tunnel's default domain,
  by the same rule.
- Tailscale: needs Tailnet on.

## 5. Traffic paths: UI

### 5.1 Traffic card (`TrafficMode.tsx`, `ClusterTraffic`)

```
Traffic                                                   Learn more
─────────────────────────────────────────────────────────────────────
Public ports                                                  [on ]
Ports 80 and 443 on every server except build-only servers.
   Accept PROXY protocol                                      [off]
   Only if every connection comes through a load balancer that sends it.
   TCP and UDP ports            Published with the websites.
─────────────────────────────────────────────────────────────────────
Cloudflare Tunnel                                             [on ]
Websites through Cloudflare. No open ports needed.
   Cloudflare account   Acme            (only with several accounts)
   Default domain       example.com
   ● Tunnel connected · 2 connectors · FRA, AMS
                                               [Rotate]  [Save]
─────────────────────────────────────────────────────────────────────
Tailnet                                                       [on ]
Websites on ts.net names, for devices on your tailnet.
Servers stay on the tailnet when this is off.     tail1234.ts.net
```

- One switch per path, in a list. No radio group.
- Each switch saves on its own. A switch that needs a confirm shows the
  dialog first, and stays as it was if the dialog is cancelled.
- Turning Cloudflare Tunnel on opens its settings below the switch (as
  picking it does today) with **Turn on** and **Cancel**. The switch
  shows on only after Turn on.
- While a change is under way the path's status line says so: "Moving 6
  domains to the tunnel…", "Closing public ports after the last domain
  moves…", "Removing the tunnel…". The switch shows the saved state, not
  the wanted one, until the cluster reports it.
- Disabled switches say why on the hint line: "Connect Cloudflare
  first.", "Give the token Cloudflare Tunnel:Edit.", "Not reported yet.",
  "Connect Tailscale on Integrations first."
- Viewers and developers: switches disabled, no buttons, no hint about
  roles beyond the panel's usual note.
- No flash or jump: the card renders from the cluster snapshot and the
  team's integrations, which the store has before the page shows. The
  tunnel's status line keeps its place while it loads ("Starting the
  tunnel…" as today).
- The Danger zone's Remove tunnel row (`Tunnel.tsx:325`) goes. The
  switch does that now.
- `TunnelBadge` (`Tunnel.tsx:103`) shows while the tunnel is on, not
  only in tunnel mode.

### 5.2 Confirm dialogs

Turning Public ports off with domains on it:

- Title: "Close public ports on eu?"
- Body, only the lines that apply:
  - "6 domains are in your Cloudflare account." + switch "Move them to
    the tunnel first" (on).
  - "These stop answering from the internet: shop.example.com, … and 2
    more."
  - "The public dashboard closes. Open Pyo through Tailscale or your local
    network."
  - Switch "Publish TCP and UDP ports" when the cluster has some.
- Buttons: "Cancel", "Close public ports".

Turning Cloudflare Tunnel off:

- Title: "Turn off Cloudflare Tunnel on eu?"
- Body: "4 domains switch back to public ports." / "These stop
  answering: web.example.com, …" / switch "Put back the records it
  replaced".
- Buttons: "Cancel", "Turn off".

Turning Tailnet off:

- Title: "Stop serving websites on the tailnet on eu?"
- Body: "These stop answering: blog.tail1234.ts.net, …" and "Servers stay
  on the tailnet."
- Buttons: "Cancel", "Stop serving".

Turning PROXY protocol on or off:

- Title: "Accept PROXY protocol on eu?"
- Body: "Saving drops open connections on every website of eu once.
  Visitors reconnect. WebSockets and uploads in progress are cut."

### 5.3 Domain dialog (`AddDomainDialog.tsx`)

- The **Through Cloudflare Tunnel** switch (`CloudflareFields` with
  `tunnel`, `AddDomainDialog.tsx:563`) becomes a **Path** choice, the same
  `Choices` control as the Domain kind (`AddDomainDialog.tsx:411`), shown
  for Your domain and Free domain when both Public ports and Cloudflare
  Tunnel are on:
  - "Direct", "Ports 80 and 443 of the servers"
  - "Cloudflare Tunnel", "CNAME to the tunnel in example.com"
- With one of the two on, no choice. The hostname hint says the path, as
  today ("Through Cloudflare Tunnel. No DNS needed.").
- "Direct" keeps today's "Create the DNS record in Cloudflare" switch.
- The Tailscale kind is disabled with "Tailnet is off on eu." when Tailnet
  is off.
- Editing a domain: the Path choice is shown with the current path.
  Changing it runs the existing Switch to Tunnel or Switch back
  (`setDomainTunnel`), including the "Replace and switch" step. A name
  under the default domain shows "Cloudflare Tunnel" with no choice.
- The service's Domains list shows "Not served" with the reason for a
  domain whose path is off.

## 6. Traffic paths: API

### 6.1 New op

```rust
/// Turn a cluster's traffic paths on or off: public ports 80 and 443 (`public`), Cloudflare Tunnel
/// (`tunnel`, with its settings as for setClusterTraffic) and websites on the tailnet (`tailnet`).
/// Unset fields stay as they are. At least one path stays on. When a path turned off still has
/// domains, `status` is "confirm" and `domains` lists them: send it again with `confirm` to go
/// ahead. With `moveToTunnel`, domains in the tunnel's account move to the tunnel before public
/// ports close. Team owners and admins.
SetClusterPaths {
    cluster_id: String,
    #[serde(default)] public: Option<bool>,
    #[serde(default)] tunnel: Option<bool>,
    #[serde(default)] tailnet: Option<bool>,
    /// TCP and UDP ports on the servers while public ports are off.
    #[serde(default)] public_ports: Option<bool>,
    #[serde(default)] proxy_protocol: Option<bool>,
    #[serde(default)] default_domain: Option<String>,
    #[serde(default)] account_id: Option<String>,
    #[serde(default)] move_to_tunnel: Option<bool>,
    /// With turning the tunnel off: put back the records its domains replaced.
    #[serde(default)] restore_records: Option<bool>,
    #[serde(default)] confirm: bool,
},
```

Answer: `{ ok, status: "done" | "confirm" | "moving", domains: [{
serviceId, domainId, hostname, path, outcome: "moves" | "stops" }],
moving: 6 }`.

Errors, plain:

- "Turn on another path first. A cluster needs one."
- "Connect Cloudflare first." / "Give the token Cloudflare Tunnel:Edit."
- "Connect Tailscale on Integrations first." (Tailnet on with no
  Tailscale is allowed: this is only for the hint. The op accepts it.)
- "Run the command under Refresh this cluster in the cluster's settings
  first."

### 6.2 Old ops keep working

| Op | Becomes |
|---|---|
| `setClusterTraffic` `public` | `public: true`. Tunnel and tailnet as they are (today: a tunnel row keeps running in public mode) |
| `setClusterTraffic` `tailscale` | `public: false`. Tunnel as it is (today: a tunnel row keeps running), tailnet as it is |
| `setClusterTraffic` `tunnel` | `public: false, tunnel: true`, with `defaultDomain`, `accountId`, `portsPublic` as `publicPorts` |
| `setClusterPublicTraffic` | as `setClusterTraffic` `public` or `tailscale` |
| `setClusterWebProxyProtocol` | `proxy_protocol` |
| `removeClusterTunnel` | `tunnel: false` with `restoreRecords`, without the confirm step. Keeps today's rule: refused while `traffic` is `tunnel` |

The old ops never stop at "confirm". They do what they do today, so
scripts and MCP clients see no change. Their doc comments gain "Prefer
setClusterPaths."

### 6.3 Reads

- Snapshot per cluster gains `paths: { public, tunnel, tailnet }`,
  `publicPorts` and `proxyProtocol`. `traffic` stays.
- Each domain gains `trafficPath` (`"public" | "tunnel" | "tailnet"`) and
  `served` (false while its path is off). Not `path`, which is the
  domain's URL path.
- `dash/src/lib/pyo/types.ts`: `Cluster` and `Domain` follow.

### 6.4 CLI and MCP

- `pyo cluster paths eu [--public on|off] [--tunnel on|off] [--tailnet
  on|off] [--ports on|off] [--move-to-tunnel] [--yes]`. Without `--yes`,
  a "confirm" answer prints the domains and "Run again with --yes to go
  ahead." (exit 1).
- `pyo cloudflare tunnel on|off|remove` keep working through the old ops.
- MCP gets `set_cluster_paths` from the op's doc comment.

## 7. Traffic paths: data and migration

### 7.1 Columns

Migration `crates/pyo-api/migrations/0047_traffic_paths.sql` (it also adds
`close_public_after` and `remove_when_back`, see "Traffic paths as
built"):

```sql
-- Services' websites answer on the tailnet (ts.net names, the servers'
-- Tailscale addresses). Off: only Pyo's own names are served there.
ALTER TABLE pyo.clusters ADD COLUMN tailnet_web boolean NOT NULL DEFAULT true;
-- TCP and UDP ports on the servers while public ports are off.
-- NULL: as before (published in tunnel mode by the tunnel's ports_public).
ALTER TABLE pyo.clusters ADD COLUMN public_ports boolean;
UPDATE pyo.clusters c SET public_ports = t.ports_public
  FROM pyo.cluster_tunnels t WHERE t.cluster_id = c.id AND c.traffic = 'tunnel';
```

- **Public ports on** is `traffic = 'public'`, as today.
- **Cloudflare Tunnel on** is a `cluster_tunnels` row that isn't
  `removing`, as today.
- **Tailnet on** is `tailnet_web`.
- `traffic` keeps its three values and keeps deciding where free domains
  point (`freedomain.rs`). New writes set it from the switches: Public on
  → `public`, else Tunnel on → `tunnel`, else `tailscale`.
- `cluster_tunnels.ports_public` stays for one release, kept in step by a
  trigger, so a 0.51 replica still reads the right value (the pattern of
  `clusters_traffic_sync`).

### 7.2 Existing clusters

Nothing changes for any cluster: each maps to the switches that describe
what it does now, and the heartbeat reply carries the same values.

| Today | Public | Tunnel | Tailnet | TCP and UDP ports |
|---|---|---|---|---|
| `public`, no tunnel | on | off | on | with the websites |
| `public`, with a tunnel | on | on | on | with the websites |
| `tailscale`, no tunnel | off | off | on | off |
| `tailscale`, with a tunnel | off | on | on | off |
| `tunnel` | off | on | on | the tunnel's `ports_public` |

The `tailscale`-with-a-tunnel row keeps `traffic = 'tailscale'`, so its
free domains stay on the Tailscale IP. Tailnet starts on everywhere
because ts.net names are served today whatever the traffic setting.

## 8. Traffic paths: control plane and agent

Most of this is control plane work. The agent already does each path on
its own.

- **Public ports**: the reply's `publicTraffic` (unchanged).
- **TCP and UDP ports**: the reply's `publicPorts` becomes
  `traffic = 'public' OR coalesce(c.public_ports, traffic = 'tunnel' AND
  coalesce(t.ports_public, true))` (`agent.rs:762`). With Public ports off
  and the ports on, the agent writes `pyo-public-ports` as in tunnel mode
  (`main.rs:302`, `ports && !on`), which already works for any value of
  `traffic`.
- **Tunnel**: `TunnelWanted` from the row (unchanged).
- **Tailnet off**:
  - `routes::domain_specs` leaves out domains on the tailnet path, with a
    routes sync. Their HTTPRoutes go. Their certificate Secrets stay, so
    the tailnet listener's certificate list doesn't change (§9).
  - `tailscale::wanted_for` leaves services' names out of what the
    servers host. Pyo's own names stay.
  - New reply field `tailnetWeb: false`: the agent removes
    `pyo-tailnet-web` (today it follows only `freeDomainAddress`,
    `main.rs:305`). `freeDomainAddress` itself stays as it is: changing
    it would move free domains to another address (`freedomain.rs`,
    `rehome`).
  - Older agents ignore `tailnetWeb` and keep `pyo-tailnet-web`. The
    tailnet-path domains still stop, since their routes are gone.
- **Public off after moves** (§4.4): a new step in the tunnel loop
  (`cloudflare/tunnel_loop.rs`). The cluster gets `close_public_after`
  (timestamp). The loop sets `traffic` once every domain it asked to move
  is `on`, or the user cancels. The card shows the waiting domains.
- **Tunnel off**: the existing removal (`removeClusterTunnel`) after the
  domain switches back.
- **Activity log**: one line per switch change, naming the domains moved
  or stopped.

## 9. Traffic paths: connection cuts

| Change | What drains or ends | Why |
|---|---|---|
| Public ports on | Nothing | ServiceLB adds its pods. No listener changes |
| Public ports off | Connections that came in on public ports end, after the confirm. Tunnel and tailnet connections stay | ServiceLB removes its pods. No listener changes |
| Cloudflare Tunnel on | Nothing | The connector and `pyo-tunnel-origin` are new. No domain has moved yet |
| A domain moves to the tunnel (`switching`) | That domain's open connections drain once | Its filter chain gets the tunnel client policy (visitor from X-Forwarded-For, trusted from the connectors). That's per filter chain in Envoy, so it can't be route-only |
| `switching` → `on` | Nothing | The gate goes into its SecurityPolicy, and its :80 route goes. With every route always carrying its policies, both are route changes |
| A domain moves back to public ports | That domain's open connections drain once | Its filter chain drops the tunnel client policy |
| Cloudflare Tunnel off | Domains still on the tunnel stop (confirmed). Moved-back domains drained once each when they moved | |
| A connector rollout | Tunnel domains drain about 3 times (unchanged, cloudflare-tunnel.md §7.3) | The trusted address list is in the tunnel client policies |
| Tailnet off | ts.net domains stop (confirmed). Other tailnet names, Pyo's dashboard included, stay connected | Only routes and hosting change. Certificates stay, so the `pyo-tailnet` HTTPS listener is untouched |
| Tailnet on | Nothing | Routes and hosting come back |
| TCP and UDP ports on or off | Port connections end when off (confirmed). Web connections stay | `pyo-public-ports` is its own Service with its own ServiceLB pods |
| PROXY protocol on or off | Every connection on ports 80 and 443 drains once: public, tunnel and the Tailscale addresses. ts.net names stay | `proxyProtocol` is a listener filter (listener-wide in Envoy). Checked with egctl (§20). Unchanged from today |

The dialog of each change that drains says so. Moving a domain
(Switch to Tunnel, Switch back) gets the line "Open connections to
shop.example.com drop once." in its confirm.

---

## 10. Gateway section: status

Read-only, from each heartbeat. New `Heartbeat.gateway: Option<GatewayReport>`
(`pyo-proto/src/lib.rs:88`), filled by the agent from pods it already
reads cluster-wide:

```rust
pub struct GatewayReport {
    /// Envoy Gateway's controller (pyo-gateway Deployment).
    pub controller: Part,
    /// The proxies (Envoy DaemonSet), one per server.
    pub proxies: Vec<ServerPart>,
    /// The firewall's processor (WAF DaemonSet), one per server.
    pub firewalls: Vec<ServerPart>,
}
pub struct Part { pub ready: bool, pub version: String, pub reason: Option<String> }
pub struct ServerPart {
    pub server: String,
    pub ready: bool,
    /// From the image tag: "1.39.5".
    pub version: String,
    pub restarts: i32,
    /// When its container last started (ms).
    pub started_at: Option<i64>,
    /// Why it isn't ready: one of WORKLOAD_PROBLEMS, or the last error line, 200 chars at most.
    pub reason: Option<String>,
}
```

- A server that should run a proxy and has none is listed with
  `ready: false` and "No proxy on this server." Build-only servers are
  left out (`gateway_affinity`).
- `capabilities.gateway_error` (`lib.rs:544`) still drives the cluster's
  error banner. The section shows the same message on the controller
  line.
- Shown in the snapshot per cluster as `gateway.status`. Older agents
  send none: the status part says "Not reported yet."

## 11. Gateway section: settings

Envoy Gateway v1.9.3 (`ENVOY_GATEWAY_IMAGE`, `system.rs:40`), Envoy
1.39.5. Each line was checked against the v1.9.3 source and with
`egctl x translate` (§20).

| Setting | Envoy Gateway object and field | Per route | Effect on open connections |
|---|---|---|---|
| HTTP/2 | Default ALPN `h2, http/1.1` on HTTPS listeners | n/a | Shown as on. No setting |
| HTTP/3 | `ClientTrafficPolicy.spec.http3: {}` on `pyo-https` | No | A new UDP listener, plus an `alt-svc` header on every route on 443 (routes only). TCP filter chains don't change. But Envoy Gateway also adds UDP 443 (`<port>-h3`) to Envoy's Service, which replaces ServiceLB's pod on each server: expected to end public connections once per server |
| Compression | `BackendTrafficPolicy.spec.compressor` | Yes | Route-only, given one always-compressing route per Gateway (below). Without it, the first compressing route of a domain drains that domain once |
| Request timeout default | HTTPRoute rule `timeouts.request` (Gateway API) | Yes | None |
| Idle timeout default | `BackendTrafficPolicy.spec.timeout.http.streamIdleTimeout` | Yes | None |
| Maximum request size | `BackendTrafficPolicy.spec.requestBuffer.limit` | Yes | Drains a domain the first time. Breaks WebSockets. Left out (below) |
| TLS minimum version | `ClientTrafficPolicy.spec.tls.minVersion` | No | Every HTTPS filter chain changes: every HTTPS connection drains once |

### 11.1 HTTP/3

- **Off by default.** Needs Public ports on: tunnel visitors get HTTP/3
  from Cloudflare's edge, and the tailnet entries don't publish UDP.
- **What Pyo checks first**, before it saves:
  - No other LoadBalancer Service in the cluster holds UDP 443 (the
    agent's `port_free`, `gateway.rs:1276`, with UDP).
  - Servers Pyo made at Hetzner: Pyo's firewall gets "udp 443, Web
    (HTTP/3)" (`cloud/hetzner.rs:170`, `firewall_rules`). Today it opens
    only TCP 80 and 443. Updating an existing firewall is a call to the
    Hetzner API with the team's own token, so on Pyo Cloud it only ever
    touches the team's own project.
  - Other servers: Pyo can't see a provider's firewall. The dialog says
    "Open UDP port 443 to every server in your provider's firewall."
- **After it's on**, Pyo checks each server's public IPv4 address from
  the control plane with one UDP packet to 443: a QUIC Initial with a
  reserved version (`0x?a?a?a?a`), which a QUIC server answers with a
  Version Negotiation packet. No QUIC library needed. The section shows
  "UDP 443 reaches server-1" or "UDP 443 doesn't reach server-2. Visitors
  there use HTTP/2." Browsers fall back to HTTP/2 by themselves when UDP
  is blocked, so a failed check costs speed, not visits. On the cluster
  Pyo runs on, a check to its own public IP may not come back through
  the NAT: then "Pyo can't check this server from here."
- **Why UDP isn't on a Service of its own.** Envoy Gateway names the UDP
  port `<name>-h3` on the same port number 443
  (`infrastructure/kubernetes/proxy/resource_provider.go`). Pyo removes
  ports from that Service with a strategic merge patch
  (`system.rs:750`), whose merge key for Service ports is `port`, so it
  can't remove UDP 443 and keep TCP 443. Moving the public web Service out
  of Envoy Gateway's hands would itself replace ServiceLB's pods. So
  HTTP/3 on or off is one cut per server, said in the dialog (open
  question 3).
- `alt-svc: h3=":443"; ma=86400` goes on every route that listens on
  10443, tunnel domains and the Tailscale addresses included, because
  Envoy Gateway enables HTTP/3 per address and port, not per Gateway.
  Clients there try UDP 443, find nothing and stay on HTTP/2. To test
  with Cloudflare (§16).

### 11.2 Compression

- **Off by default.** On means Zstd, Brotli and Gzip, preferred in that
  order when the client accepts several. There's no per-algorithm choice:
  every route lists the same three in the same order, because Envoy
  Gateway builds the filter chain's compressor settings from the first
  compressing route it finds, and differing lists would change the chain.
- Each entry needs its type's object: `{ type: Zstd, zstd: {} }`. Without
  it Envoy Gateway v1.9.3 drops the entry silently
  (`buildCompression`, `internal/gatewayapi/backendtrafficpolicy.go`).
  `minContentLength` is never set (Envoy's default, 30 bytes).
- **Content that's already compressed is skipped.** Envoy's compressor
  leaves a response alone when it has `Content-Encoding`, and only
  compresses its default text types (HTML, CSS, JavaScript, JSON, plain
  text, XML, SVG). Images, video and archives pass as they are.
- Envoy Gateway removes `Accept-Encoding` from the request on a
  compressing route (`compressor.go`, `patchRoute`), so the service never
  compresses too. It also means a service's own precompressed files
  (`.br`, `.gz`) aren't used on those routes. Said in the hint: "Pyo
  compresses responses itself. Your service's own compression is turned
  off while this is on."
- **No cut on and off: the anchor route.** The compressor filters exist
  on a filter chain only while at least one route on it compresses. So
  every HTTPS Gateway (a service's, `pyo-tailnet`, `pyo-public`) gets one
  more HTTPRoute that always compresses:
  - name `<base>-anchor`, parent the Gateway without a section (all its
    listeners), no hostnames,
  - one rule: exact path `/.well-known/pyo/anchor`, an `ExtensionRef` to
    a shared `HTTPRouteFilter` `pyo-not-found` with `directResponse: {
    statusCode: 404 }`. No backend, so it's no way around the firewall,
  - its BackendTrafficPolicy with the three compressors.
  The filters are then always on every chain, off by default, and a route
  turns them on for itself. Checked: with the anchor in place, turning
  compression on or off changes routes only (§20).
- The anchor's first rollout changes every HTTPS filter chain once. It
  ships in the release that makes every route carry its policies, which
  changes the same chains once anyway, so the cut happens once.
- Plain HTTP on :80 isn't compressed. All plain-HTTP hostnames share one
  filter chain, and those routes are almost all redirects.
- Per service: not in 0.52.0. Every service follows the cluster. A
  per-service off switch is route-only, so it can come later without a
  cut (open question 7).

### 11.3 Defaults for every service

| Default | Today | Range | A service's own |
|---|---|---|---|
| Request timeout | 300 s (`REQUEST_TIMEOUT_SECS`, `system.rs:1298`) | 1 to 3600 s, or no limit | `requestTimeout` (`lib.rs:2043`) overrides it. Exists today |
| Idle timeout: how long a WebSocket or stream may stay quiet | 1 h, on the client policies (`STREAM_IDLE_TIMEOUT`, `system.rs:1314`) | 1 min to 24 h | None today. Not added in 0.52.0 |

- The request timeout default replaces the constant. The API resolves
  each service's value (its own, else the cluster's) into
  `DeploySpec.request_timeout` and `FirewallTarget.request_timeout`, as
  now. Changing the default resends the routes of every service without
  its own.
- The idle timeout goes per route, through the BackendTrafficPolicy every
  website route carries (`timeout.http.streamIdleTimeout`, which becomes
  Envoy's route `idle_timeout`, `route.go` `idleTimeout`). The client
  policies keep their fixed 1 h and never change, so no chain drains.
  Envoy Gateway's own rule keeps the idle timeout at least the request
  timeout when only the latter is set. Pyo sets both, and the API refuses
  an idle timeout shorter than the request timeout default: "The idle
  timeout must be at least the request timeout."
- **Maximum request size is left out of 0.52.0.** The only Envoy Gateway
  v1.9.3 way is `requestBuffer`. It holds every request whole in Envoy's
  memory, its CRD refuses it next to `httpUpgrade`, and its docs say
  WebSocket and streaming requests hang with it. A cluster-wide default
  would break those services. Through the tunnel, Cloudflare's own limit
  already applies (100 MB on Free and Pro). Open question 2.

### 11.4 TLS minimum version

- 1.2 (the default, Envoy Gateway's own) or 1.3.
- Written into every client policy that has TLS: `pyo-https` and all its
  variants, each service's CDN and tunnel policies (they replace
  `pyo-https` on their listeners, so they need it too), `pyo-tailnet`.
- The tunnel's connectors (cloudflared, Go's TLS client) support TLS 1.3
  to the origin. To confirm in §16.4 with the rest. Services behind
  Cloudflare's proxy: Cloudflare to origin over 1.3 is to be checked with
  a real zone before the release (§16).
- Every HTTPS connection drains once on save, ts.net names included.

## 12. Gateway section: UI

A panel **Gateway** on the cluster settings page, right after Traffic
(`settings.tsx:139`), with a Learn more link to the gateway page.

```
Gateway                                                   Learn more
─────────────────────────────────────────────────────────────────────
Controller   ● Ready · v1.9.3
─────────────────────────────────────────────────────────────────────
Server      Proxy                         Firewall
server-1    ● Ready · 1.39.5 · 3 days     ● Ready · 0.52.0
server-2    ● Ready · 1.39.5 · 2 hours    ● Not ready · ImagePullBackOff
─────────────────────────────────────────────────────────────────────
HTTP/2       On
HTTP/3                                                         [off]
             Needs UDP port 443 open to every server.
Compression                                                    [off]
             Zstd, Brotli and Gzip. Files that are already compressed are skipped.
TLS minimum  [1.2 ▾]
─────────────────────────────────────────────────────────────────────
Defaults for every service
Request timeout   [300] seconds      A service's own setting wins.
Idle timeout      [60] minutes       How long a WebSocket may stay quiet.
                                                               [Save]
```

- "3 days" is the time since the proxy's container last started.
- Status rows render from the snapshot with the page. A cluster with no
  report yet shows one line "Not reported yet." in the status part and
  the settings below, so nothing moves when the first report arrives.
- Settings save with one **Save**, shown only when something changed.
- A save that drains says so first:
  - HTTP/3 on or off: "Saving drops open connections on every server's
    public ports once, one server at a time. Visitors reconnect.
    WebSockets and uploads in progress are cut."
  - TLS minimum: "Saving drops open HTTPS connections on every website of
    eu once. Visitors reconnect. WebSockets and uploads in progress are
    cut."
  Compression and the defaults save without a dialog: they change routes
  only.
- HTTP/3 disabled with "Turn on public ports first." while Public ports
  is off.
- After HTTP/3 is on, the UDP check lines show under its row (§11.1).
- Errors, plain: "UDP port 443 is in use on server-2 by shop/game.
  Free it first.", "The request timeout is 1 to 3600 seconds, or 0 for no
  limit."
- Viewers and developers: the same panel, controls disabled, no Save.

## 13. Gateway section: API

```rust
/// A cluster's gateway settings. Unset fields stay as they are. `http3` needs public ports and
/// UDP port 443 free on every server. `requestTimeout` (seconds, 0 for no limit) and
/// `idleTimeout` (seconds) are the defaults for services without their own. `tlsMin` is "1.2"
/// or "1.3". `drops` in the answer says whether open connections were dropped once.
/// Team owners and admins.
SetClusterGateway {
    cluster_id: String,
    #[serde(default)] http3: Option<bool>,
    #[serde(default)] compression: Option<bool>,
    #[serde(default)] tls_min: Option<String>,
    #[serde(default)] request_timeout: Option<u32>,
    #[serde(default)] idle_timeout: Option<u32>,
},
```

- Answer: `{ ok, drops: bool, checks: [{ server, udp443: "open" |
  "closed" | "unknown" }] }` (checks fill in on the next read).
- Snapshot per cluster: `gateway: { settings: {...}, status: {...},
  udpChecks: [...] }`.
- Storage: columns on `pyo.clusters`: `http3 boolean NOT NULL DEFAULT
  false`, `compression boolean NOT NULL DEFAULT false`, `tls_min text NOT
  NULL DEFAULT '1.2'`, `request_timeout integer NOT NULL DEFAULT 300`,
  `idle_timeout integer NOT NULL DEFAULT 3600`. Existing clusters get
  today's values, so nothing changes until someone saves.
- CLI: `pyo cluster gateway eu [--http3 on|off] [--compression on|off]
  [--tls-min 1.3] [--request-timeout 300] [--idle-timeout 3600]`. MCP:
  `set_cluster_gateway`.

## 14. Gateway section: agent and admission policy

- **Listener-level settings** (HTTP/3, TLS minimum) join PROXY protocol
  and origin pull in `system::WebSettings` (`system.rs:1318`), which the
  agent reads back from `pyo-https` (`gateway.rs:2284`) and writes into
  every client policy (`write_web`, `gateway.rs:2298`). A new command
  `SetWebSettings { proxy_protocol, http3, tls_min }` replaces
  `SetWebProxyProtocol` for new agents. The old command stays for N-1.
- **Route-level settings** (compression, request timeout, idle timeout)
  travel with each service: `DeploySpec` and `FirewallTarget` gain
  `compression: bool` and `idle_timeout: Option<u32>`, and
  `request_timeout` is resolved by the API. `route_objects`
  (`gateway.rs:181`) writes them on the BackendTrafficPolicy and HTTPRoute
  rules of HTTPS routes, and writes the anchor route. A change of a
  cluster default bumps `routes_revision` for its services.
- **Status**: `GatewayReport` from the pods of the Envoy DaemonSet, the
  WAF DaemonSet and the controller Deployment in pyo-gateway (the agent
  already reads pods cluster-wide).
- **HTTP/3 check in the cluster**: `port_free("UDP", 443)` answered in a
  command reply before the control plane saves.
- **Admission policy** (`rbac.rs`):
  - `clienttrafficpolicies`: `client_policy_variants` gains `http3` and
    `tls.minVersion`. `pyo-https` goes from 4 to 16 exact variants. The
    CDN, tunnel and tailnet policy rules accept the two fields with those
    values only.
  - `backendtrafficpolicies` on web routes (`rbac.rs:866`): allow
    `compressor` equal to the one list, and `timeout` with only
    `http.streamIdleTimeout` between 60 s and 24 h.
  - HTTPRoute: the anchor's exact shape. `httproutefilters`: only
    `pyo-not-found` with exactly its spec.
  - The new rules change `agentAccess`, so the release shows the "run the
    connect command" notice by itself. The settings are refused with the
    refresh message until then. The status needs no new permission.

## 15. Gateway section: connection cuts

| Save | Cut |
|---|---|
| HTTP/3 on or off | Public connections end once per server as ServiceLB replaces its pod, one server at a time (expected, measured in §16). HTTP/3 connections end when it goes off. Tunnel and tailnet connections stay |
| Compression on or off | None (with the anchor). Each domain drains once when the anchor first rolls out |
| Request timeout default | None |
| Idle timeout default | None |
| TLS minimum | Every HTTPS connection drains once |

---

## 16. Tests

### 16.1 Unit

- `SetClusterPaths`: each of the 7 states, the refusal of no path, the
  confirm answer listing domains, old ops mapped as in §6.2 with the same
  stored values as today.
- Migration: each row of §7.2 gives the same heartbeat reply fields
  (`publicTraffic`, `publicPorts`, `tunnel`, `freeDomainAddress`) before
  and after.
- `domain_specs` with Tailnet off leaves tailnet-path domains out and
  keeps their certificate Secrets.
- Public off with moves: `traffic` changes only once every moved domain
  is `on`. A "confirm" domain holds it.
- `route_objects`: the anchor on every HTTPS Gateway, compressor list
  identical on every route, idle timeout on the policy, no compression on
  :80 routes.
- `client_policy_variants` covers every combination, and each written
  policy is one of them (the admission policy test that already does this
  for origin pull and PROXY protocol).
- rbac tests: anchor route, filter and the new policy fields pass, and
  anything else on those objects is refused.
- The QUIC probe packet against a captured Version Negotiation answer.

### 16.2 egctl checks in CI

A test that runs `egctl x translate` v1.9.3 (checksum pinned) on Pyo's
own objects for a sample service and compares listener and filter chain
sets before and after each route-only setting: compression on with the
anchor, both timeouts. It fails if one of them touches a listener. This
keeps the "route only" promise when Envoy Gateway is updated.

### 16.3 e2e (`scripts/update-traffic-e2e.sh`)

A new phase `toggles` on the throwaway VMs, counting lost requests per
kind (fresh, keepalive, long, ws) while it:

- turns Public ports off and on with Tunnel off (Tailnet only and back),
- turns compression on and off (expect 0 lost),
- changes both timeout defaults (expect 0 lost),
- turns Tailnet off and on while a ts.net WebSocket to the dashboard is
  open (expect it to stay),
- turns HTTP/3 on and off, and records what ServiceLB's pod replacement
  costs (this is the measurement §3 and §11.1 depend on),
- changes the TLS minimum (expect one drain).

### 16.4 Real services, throwaway only

- A throwaway Cloudflare zone and tunnel: move a domain with Public ports
  closing after the move, measure the gap (expect none). Check whether
  Envoy's `alt-svc` reaches visitors through the tunnel. Check TLS 1.3 to
  origin for a service behind Cloudflare's proxy.
- A throwaway Hetzner server: the firewall gains UDP 443 and the probe
  says "open". Without the rule it says "closed".
- A throwaway tailnet: Tailnet off and on with two clusters in one team.

## 17. Docs

pyo-docs, in the same piece of work:

- `gateway.mdx`: the Gateway section, each setting, which saves drop
  connections once.
- `clusters.mdx`: the Traffic card's three paths.
- `cloudflare-tunnel.mdx`: Turn it on and Turn it off follow the switch.
  Public ports closing after domains move.
- `tailscale.mdx`: the Tailnet switch, and that servers stay on the
  tailnet.
- `domains.mdx`: the Path choice and "Not served".
- `hetzner.mdx`: UDP 443 in Pyo's firewall.
- CLI `commands.mdx`. The API reference and MCP tools come from the op
  comments.

## 18. Open questions

Each with a pick.

1. **Tunnel off: remove the tunnel at Cloudflare, or keep it idle?**
   Pick: remove, as Remove tunnel does today. An idle tunnel keeps a
   token in the cluster and records at Cloudflare for nothing. Turning it
   on again makes a new one in a minute.
2. **Maximum request size.** Pick: not in 0.52.0 (§11.3). Revisit if
   Envoy Gateway gets a body limit that works with WebSockets.
3. **HTTP/3 and ServiceLB.** Pick: accept one cut per server on HTTP/3
   on or off, said in the dialog. If the e2e shows new connections fail
   for more than a few seconds per server, HTTP/3 waits for a design that
   keeps UDP 443 on a Service of its own.
4. **Label for the tailnet switch.** Pick: "Tailnet", with the hint that
   servers stay on the tailnet.
5. **Your own domains on the tailnet path** (DNS to the servers' Tailscale
   addresses). Pick: not in 0.52.0. ts.net names cover private sites.
6. **When the anchor routes ship.** Pick: in the same release as
   always-present route policies, so the filter chains change once.
7. **Per-service compression and idle timeout.** Pick: later. Both are
   route-only, so adding them won't cut.
8. **Default path for a new domain with Public and Tunnel on.** Pick:
   Direct, except names under the default domain. Tunnel when Public is
   off.

## Traffic paths as built (2026-11-27)

Built on `feat/traffic-paths`. Where the code differs from the sections
above, this list wins. Each change keeps traffic flowing.

- **Domain field name.** A domain's path is `trafficPath`, not `path`
  (§6.3). `path` is already the domain's URL path ("/api"), so writing the
  traffic path there would have changed its routes.
- **Tailnet off keeps the hosting.** `tailscale::wanted_for` is unchanged:
  services' ts.net names stay hosted on the servers, and only their routes
  go (§8). Envoy answers them with 404. Taking the names off at Tailscale
  and back could take minutes and give them new addresses. This way
  turning Tailnet on is one routes sync. Leaving the routes out happens
  when a command is handed to the agent (`routes::current`, `served`), so
  deploys, routes syncs and firewall syncs all follow the switch.
- **Free domains on a Tailscale address.** They sit on their service's own
  HTTPS Gateway, not on the tailnet listener. With Tailnet off their
  listener and certificate go with their routes. Only that domain's filter
  chain changes, and that domain is the one that stops. Their certificate
  is self-signed, so it is made again at once when Tailnet goes back on.
- **Public off after moves.** `clusters.close_public_after` is set while
  the ports wait. The tunnel loop (`tunnel_loop::close_public`) sets
  `traffic` to `tunnel` once no domain of the cluster has
  `tunnel: "switching"`. That covers domains still moving and domains
  waiting for "Replace and switch". Turning Public ports on again cancels
  it. While it waits, the switch shows Public ports on, with "Moving 6
  domains to the tunnel… Public ports close after the last one."
- **Tunnel off with Public ports on.** Each tunnel domain outside the
  default domain runs the existing Switch back (records restored when
  kept, both paths answer meanwhile). `cluster_tunnels.remove_when_back`
  is set, and the loop marks the tunnel `removing` once no record is
  still `reverting` (`tunnel_loop::remove_when_back`). Then the existing
  removal runs. Names under the default domain stop and are listed so.
  With Public ports off the tunnel is removed at once, as before.
- **Migration** `0047_traffic_paths.sql` also adds
  `clusters.close_public_after` and `cluster_tunnels.remove_when_back`.
  The `ports_public` and `public_ports` triggers run both ways: a tunnel
  row written by the previous release (its setClusterTraffic) moves the
  cluster's setting too. The old ops `setClusterTraffic` with `public` or
  `tailscale` and `setClusterPublicTraffic` clear `public_ports`, so they
  send the agent the same `publicPorts` as before.
- **Mixed versions.** While the previous release still serves during an
  update, a cluster in a state it can't express (Tailnet only with TCP
  and UDP ports on) gets `publicPorts: false` from its replicas. The ports
  come back with the next heartbeat the new release answers. The same
  applies to a tunnel-mode cluster switched to Tailscale only through the
  previous release's dashboard during the update: its ports stay on until
  someone saves the switch again.
- **Snapshot.** Each cluster has `paths`, `publicPorts`, `proxyProtocol`,
  `tailnetWeb`, `closingPublic` and `publicPortsSet` (the stored value,
  null before anyone set it). Its `tunnel` has `removeWhenBack`.
- **Old op mapping.** The old ops keep their own code (§6.2 asked for the
  same stored values, and running them through the new op would have
  moved domains or waited where they never did). They gained "Prefer
  setClusterPaths." and clear a waiting close.
- **CLI.** `pyo cluster paths <cluster>` takes `--move-to-tunnel on|off`
  and `--restore-records on|off` (both on by default), and prints the
  paths when given no flag.
- **Domain dialog.** The Path choice is built for Your domain. A Free
  domain stays on the public IP (or under the default domain in tunnel
  mode, as before). A name under the default domain can be added as Your
  domain with Path Cloudflare Tunnel. Editing a domain keeps Switch to
  Tunnel and Switch back as menu actions. Both confirms now say "Open
  connections to shop.example.com drop once."
- **TCP and UDP ports off** has a confirm of its own: "Open connections
  to them end."
- **Last path.** The dashboard refuses it before asking the API, with the
  same words.

## 19. Estimate

In working days, for one person, including the dashboard, tests and docs.

| Part | Days |
|---|---|
| **Traffic paths** | |
| Migration, `SetClusterPaths`, confirm lists, old ops mapped | 2 to 3 |
| Public off after moves (tunnel loop step) | 1 to 1.5 |
| Tailnet off (routes, hosting, certificates kept) | 1 to 1.5 |
| Traffic card, confirm dialogs, domain dialog Path, "Not served" | 2.5 to 3.5 |
| CLI, MCP comments | 0.5 |
| Unit tests and the e2e toggles phase | 1.5 to 2 |
| Docs | 0.5 |
| Subtotal | 9 to 12.5 |
| **Gateway section** | |
| Gateway report and status rows | 1.5 to 2 |
| HTTP/3 and TLS minimum in the client policies, admission variants | 1.5 to 2 |
| HTTP/3 checks: UDP port, Hetzner firewall, QUIC probe | 1.5 to 2.5 |
| Compression with anchor routes, admission rules | 1 to 1.5 |
| Timeout defaults through routes | 1 |
| Gateway panel and save dialogs | 1.5 to 2 |
| egctl CI check, e2e measurements, real-service checks | 2 to 3 |
| Docs | 0.5 |
| Subtotal | 10.5 to 14.5 |
| **Total** | **19.5 to 27** |

The range's top end assumes the ServiceLB measurement needs a second
pass. If HTTP/3 has to wait (open question 3), take 1.5 to 2.5 days off.

## 20. How the Envoy Gateway facts were checked

Source: the v1.9.3 tag of envoyproxy/gateway. Tool: `egctl` v1.9.3
(linux amd64), checksum matched against the release asset's SHA-256.

- CRD fields read in `api/v1alpha1`: `compression_types.go` (Gzip, Brotli,
  Zstd), `backendtrafficpolicy_types.go` (`compressor`, `requestBuffer`
  and its CEL rule against `httpUpgrade`), `clienttrafficpolicy_types.go`
  (`http3`), `tls_types.go` (`minVersion`), `timeout_types.go`
  (`streamIdleTimeout` on BackendTrafficPolicy).
- Translator code read in `internal/xds/translator`: `compressor.go`
  (filter added to a chain when any route on it compresses, enabled per
  route, `Accept-Encoding` removed), `translator.go` (HTTP/3 per address
  and port, a separate QUIC listener, `alt-svc` per route),
  `route.go` (route `idle_timeout` from the policy),
  `internal/gatewayapi/clienttrafficpolicy.go` (TLS minimum defaults to
  1.2), `infrastructure/kubernetes/proxy/resource_provider.go` (the
  `-h3` UDP Service port), `infrastructure/common/proxy_args.go`
  (`--drain-time-s` from the shutdown drain timeout).
- `egctl x translate` on two HTTPS Gateways and the shared :80 Gateway
  shaped like Pyo's, each route with its BackendTrafficPolicy, compared
  listener by listener, chain by chain and route table by route table:

| Change | Listener-level | Filter chain | Routes |
|---|---|---|---|
| Compression on one route, no anchor | | that domain's | yes |
| Compression on :80 routes | | the shared :80 chain | yes |
| Compression on a second route of a domain already compressing | | | yes |
| Compression on, anchor in place | | | yes |
| HTTP/3 | new QUIC listener | none on TCP | yes (`alt-svc`) |
| TLS minimum 1.3 | | every HTTPS chain | |
| Request timeout and stream idle timeout per route | | | yes |
| `requestBuffer` on one route | | that domain's | yes |
| PROXY protocol | yes | | |
| Connection idle timeout (client policy) | | every chain it targets | |

  `egctl x translate` doesn't merge Gateways (only Envoy Gateway's
  runner sets `MergeGateways`), and it drops labels, so label selectors
  don't match there: the client policies were given as `targetRefs`. With
  merged Gateways each HTTPS Gateway listener becomes one filter chain of
  the shared listener, built by the same code (`addHCMToXDSListener`), so
  the chain results carry over.
- Envoy's listener update rules: the LDS page of Envoy's docs.
- Not checked:
  - Whether replacing ServiceLB's pod ends the connections through it.
    Measured in §16.3 before HTTP/3 ships.
  - That Envoy answers the reserved-version QUIC packet with Version
    Negotiation (RFC 9000 says a server does, for a packet of at least
    1200 bytes). Checked in §16.1 and §16.4.
  - Envoy's compressor rules for `Content-Encoding` and its default
    content types are from Envoy's compressor docs, not from a test.
  - That an EnvoyProxy change of only the Service type doesn't restart
    the proxies. §16.3 counts proxy restarts while Public ports goes off
    and on.
