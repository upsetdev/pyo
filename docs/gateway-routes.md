# Routes in Pyo Gateway

Internal design for path routes, redirects and header rules in Pyo Gateway.
Written 2026-11-25 against `dev` at 0f3e0f8. File references are to that
commit.

Status: **being built** for 0.51.1. Redirects and header rules are on
`dev` (2026-11-25). Path routes follow.

## Decisions (2026-11-25)

The user decided:

1. **Scope B.** Everything now: path routes with per-path WAF and
   analytics, redirects and header rules.
2. **Model:** a primary entry per hostname plus path entries.
3. **Redirects always keep the query string.**
4. **Release:** all of it goes in 0.51.1.
5. **Naming:** the sections are **Routes** (which path goes to which
   service), **Redirects** and **Headers**. Where one word is needed for
   all three (a tab, a heading), **Rules**. Never "Page Rules".
6. **UI:** set in the service's Domains section, where each domain row gets
   an optional path. The team's **Domains** page has a row per hostname
   (where it goes, tunnel or public, status). Opening one shows its routes
   (read only, linking to each service), redirects and headers, with DNS,
   certificate and tunnel.

One change from the first draft, made while building: redirects are a list
on a domain entry (`redirects: [{path, toHost, toPath, status, keepPath}]`)
instead of entries of their own. A path redirect belongs to the hostname,
so it sits on the entry that holds the hostname, and needs none of the
sharing rules. A domain that only redirects (the `www` case) is an entry
without a port whose list has a redirect of `/`.

## Contents

1. Goals
2. How domains work today
3. The model
4. Data model and API
5. Gateway objects
6. Conflict rules
7. How it fits the rest
8. Migration
9. Edge cases
10. Interfaces (API, MCP, CLI, dashboard)
11. Tests
12. Estimate
13. Decisions needed

---

## 1. Goals

- **Path routes.** Several services of one team share a hostname, split by
  path prefix. The longest prefix wins. Each service keeps its own firewall,
  rate limits and WAF.
- **Redirects.** Host redirects (`www` to apex and back) and path redirects
  (`/old` to `/new`), 301 or 302, keeping the path or not. A `www` redirect
  needs no service of its own.
- **Header rules.** Add, set or remove response headers per hostname or
  path. Request headers to the service too. Pyo's own and hop-by-hop
  headers can't be changed.
- Existing domains behave exactly as today. Nothing to migrate.
- The same on self-hosted and Pyo Cloud, and on public, tunnel and tailnet
  domains.

Non-goals: regex or header matches, weighted splits, redirects to another
scheme or port, sharing a hostname across teams or clusters.

## 2. How domains work today

- A domain is an entry in a service's `spec.domains` (JSONB on
  `pyo.services`). It holds the hostname, the port, the TLS mode, the
  uploaded certificate, the HTTPS redirect and the tunnel state. Its id
  keys its DNS record (`pyo.dns_records (service_id, domain_id)`).
- `routes.rs` turns entries into `DomainSpec`s (`pyo-proto/src/lib.rs:2428`)
  and sends `SyncRoutes` to the agent on every change (`routes_revision`).
- The agent builds everything per service (`gateway.rs:181 route_objects`):
  - an HTTPS Gateway of the service's own (`<ns>-<svc>`) with one listener
    per hostname, and a cert-manager Certificate per hostname
    (`pyo-tls-<ns>-<svc>-<hash>`)
  - HTTPRoutes per port: `-https-<port>`, tunnel routes, `-http-<port>`
    and `-redirect` on the shared :80, tailnet routes
  - SecurityPolicy and BackendTrafficPolicy per route (firewall, rate
    limits), EnvoyExtensionPolicy per route (WAF)
  - ClientTrafficPolicy per listener section for a CDN or the tunnel
  - stale objects are removed by the service label (`remove_stale`)
- A hostname belongs to one service in the whole installation
  (`ops.rs:752 domains_unique`). On Cloud, a new hostname needs proof
  (`domainproof.rs:365 check_claims`).
- The WAF processor picks settings **by host** (`system.rs:540 waf_config`).
- Analytics attribute a request to a service **by host**
  (`totals.rs:290`, `pyo-agent/src/traffic/clean.rs:167`).

Two facts shape the design:

1. Gateways are merged onto one proxy fleet. Two Gateways can't both have
   an HTTPS listener for the same hostname on 443. Envoy Gateway marks the
   second one conflicted. So a shared hostname needs **one listener and one
   certificate**, and other services' routes attach to it.
2. Hostname state (DNS record, certificate, tunnel switch, CDN trust) is
   per hostname, not per path.

## 3. The model

**Pick: a primary entry per hostname, plus path entries.** Each entry in
`spec.domains` gets an optional `path`. For each hostname in a team there is
one *primary* entry. It owns everything that is per hostname. Other entries
for the same hostname (on any service of the team, same cluster) only add a
path.

```
example.com  primary   web    path /       TLS, DNS record, tunnel, headers
example.com  path      api    path /api    strip prefix on
example.com  path      docs   path /docs   (a static site)
www.example.com  redirect on web   -> https://example.com, 301, keep path
```

- Today every entry is a primary with path `/`. Nothing changes for them.
- The first entry added for a hostname is its primary. Removing it hands
  the role to the oldest remaining entry (section 7, "Hand-off").
- An entry routes to a port, **or** redirects (no port). A `www` redirect is
  a redirect entry on any service of the team, usually the one it points
  to. It gets the normal domain pipeline: proof, DNS record, certificate,
  tunnel. No dummy service.

Why not team-level domain objects (a `pyo.domains` table with services
attached by path)? It is the cleaner model on paper. Redirect-only hosts
would need no service at all. But it moves every existing domain, its DNS
record and tunnel state into a new table, rewrites `domains_unique`,
`check_claims`, the tunnel loop, previews, compose sync, imports and every
dashboard view of domains, and adds a migration for live installs. That is
2 to 3 weeks on its own, and against "nothing to migrate". The primary
model reuses the whole per-entry pipeline and only adds rules for sharing.

Its downside: per-hostname settings sit on one service, and handing them
over when that service goes needs care (a certificate copy, a DNS record
row moving). Section 7 covers it.

## 4. Data model and API

A domain entry in `spec.domains` (all new fields optional):

| Field | Type | Default | Meaning |
|---|---|---|---|
| `path` | string | `/` | Path prefix. Starts with `/`, no `?`, `#`, `*`, `//` or `..`, at most 200 chars. Stored without a trailing `/` except `/` itself. |
| `stripPrefix` | bool | false | Send `/api/x` to the service as `/x`. Not on `/`. |
| `redirect` | object | none | Redirect instead of routing. Then `portName` is empty. |
| `redirect.host` | string | same host | Target hostname. |
| `redirect.path` | string | keep | Target path prefix (path redirects). |
| `redirect.status` | 301 or 302 | 301 | |
| `redirect.keepPath` | bool | true | Keep the rest of the path. Off sends everything to the target path. |
| `headers` | object | none | Header rules (below). |

Header rules, both lists optional:

```json
"headers": {
  "response": { "set": [{"name": "Strict-Transport-Security", "value": "max-age=31536000"}], "remove": ["X-Powered-By"] },
  "request":  { "set": [{"name": "X-Region", "value": "eu"}], "remove": ["X-Debug"] }
}
```

- At most 20 rules per entry. Names are HTTP tokens, values at most 1 KiB,
  no CR or LF.
- Can't be changed: hop-by-hop headers (`Connection`, `Keep-Alive`,
  `Proxy-*`, `TE`, `Trailer`, `Transfer-Encoding`, `Upgrade`), `Host`,
  `Content-Length`, `X-Forwarded-*`, `X-Real-Ip`, `Forwarded`, `X-Envoy-*`,
  `Cf-*`, `X-Pyo-*`, and pseudo headers. Plain message: "X-Forwarded-For is
  set by Pyo Gateway and can't be changed."
- Primary entry headers apply to every path of the hostname. A path entry's
  own rules come after them (its `set` wins on the same name).

`pyo-proto` `DomainSpec` gains `path`, `strip_prefix`, `redirect`,
`headers`, all `skip_serializing_if` empty, so old agents and the
`routes_version` fingerprint see the same JSON for existing domains. It also
gains two fields the API fills in for path entries:

- `primary_of`: the base name (`<ns>-<svc>`) of the service whose Gateway
  holds the listener. Empty for a primary.
- the primary's `tls`, `redirect_https`, `tunnel` and headers, copied in by
  `routes::domain_specs` so the agent can build the path entry's tunnel and
  :80 routes alone.

The dashboard's `Domain` type gains `path`, `stripPrefix`, `redirect`,
`headers` and a read-only `primary` flag. `saveService` keeps working as
today. The `removeDomain` op takes a domain id (it already does) since a
hostname alone is no longer unique on a service.

No new tables. One index helps the conflict checks:
`CREATE INDEX ON pyo.services USING gin ((spec->'domains') jsonb_path_ops)`.
Optional, the checks already scan by team.

## 5. Gateway objects

All in `pyo-gateway`, built by `route_objects` as today.

**Primary entry.** Exactly as today: listener `h-<hash>` on the service's
Gateway, Certificate, ClientTrafficPolicy section, `-redirect` on :80. Its
HTTPRoute rule gets a `PathPrefix` match when its path isn't `/`.

**Path entry.** No listener, no Certificate, no client policy. Its own
HTTPRoutes, named as today plus a path hash
(`<ns>-<svc>-https-<port>-p<hash>`), with:

```yaml
parentRefs: [{ name: <primary_of>, sectionName: h-<hosthash> }]
hostnames: [example.com]
rules:
- matches: [{ path: { type: PathPrefix, value: /api } }]
  filters:
  - type: URLRewrite            # only with stripPrefix
    urlRewrite: { path: { type: ReplacePrefixMatch, replacePrefixMatch: / } }
  - type: ResponseHeaderModifier  # header rules, when any
  - type: RequestHeaderModifier   # merged with Pyo's own (static sites, tunnel)
  backendRefs: [<its own Backend>]
```

The Gateway API sends each request to the longest matching prefix across
all routes on the listener. `PathPrefix /api` matches `/api` and `/api/x`,
never `/apix`. Its firewall, rate limits and WAF policies target its own
routes, so they stay per service. The :80 routes, tunnel routes and tailnet
routes get the same match on the shared :80 listener, the tunnel group and
the tailnet Gateway.

**Redirect entry.** An HTTPRoute with no backend:

```yaml
rules:
- matches: [{ path: { type: PathPrefix, value: /old } }]
  filters:
  - type: RequestRedirect
    requestRedirect:
      hostname: example.com          # host redirects
      path: { type: ReplacePrefixMatch, replacePrefixMatch: /new }   # keepPath
      # or { type: ReplaceFullPath, replaceFullPath: /new } without keepPath
      statusCode: 301
      scheme: https
```

A redirect entry that is the primary of its hostname (the `www` case) still
gets its listener and certificate, so `https://www.example.com` answers.
No firewall or WAF policy on redirect routes, as today for `-redirect`.

**Header rules.** `ResponseHeaderModifier` and `RequestHeaderModifier`
filters on each rule of the entry. Gateway API allows one filter of each
type per rule, so Pyo's own request headers (`x-pyo-site` for static sites,
the tunnel's `X-Forwarded-For`) are merged into the same filter, Pyo's
last so they always win.

**Names stay put.** Objects for existing domains keep today's names and
contents, so the fingerprint test (`gateway.rs:1006`) proves nothing moves
on upgrade. New sample targets with a path entry and a redirect join the
fingerprint, which resends routes once after the agent update. That resend
writes the same objects for existing services.

## 6. Conflict rules

All checked in `domains_unique` (`ops.rs:765`) inside the save, with plain
messages:

| Case | Result |
|---|---|
| Same hostname and path twice (any services of the team) | "example.com/api is already used by api." |
| Same hostname in another team | "example.com is already used by another service." (as today, takeover by proof unchanged) |
| Same hostname on a service in another cluster | "example.com is served from cluster eu. Paths of one domain must be on one cluster." |
| Path entry and primary disagree on a CDN | "example.com is behind Cloudflare on web. It can't be behind Bunny on api." |
| Strip prefix on `/` | "Strip prefix needs a path other than /." |
| A redirect to itself (same host and path) | "This redirect points to itself." |
| A redirect entry with a port | refused, the form never sends it |
| A tailnet hostname with a path entry from another cluster | same as the cluster rule |

Two services behind the same CDN, or one behind it and one not, are fine.
The listener trusts the CDN when any sharer is behind it. That is safe: the
client policy only trusts forwarded addresses on connections from the CDN's
ranges.

## 7. How it fits the rest

- **Domain proof (Cloud).** Proof is per team and hostname already
  (`domainproof::token`). A path entry for a hostname the team already
  holds is not a new claim, so `check_claims` skips it. A takeover by a
  team that proves the hostname removes every entry of the old team for it,
  primary and paths (the query already matches by hostname).
- **DNS records.** Only primaries get one. Path entries show the primary's
  record. Add domain on a path entry never creates a record.
- **Certificates.** One per hostname, on the primary's Gateway. Path entries
  show the primary's status.
- **Tunnel.** The tunnel's ingress is per hostname already
  (`tunnel_loop.rs:309`), so only primaries are read there. Switching a
  hostname to the tunnel is a primary action. Path entries follow it by the
  copied `tunnel` field, and their routes join the matching tunnel group.
- **Tailnet domains.** Tailscale Service names are deduped by hostname
  already (`tailscale.rs:1743`). Path routes on the tailnet Gateway get the
  same matches.
- **Free nip.io domains.** Same as any hostname.
- **WAF.** The processor picks settings by host today
  (`system.rs:540`). `WafRoute` gains `paths`, and the processor (Go,
  `waf/`) picks the longest host and path prefix. Without this, two
  services on one hostname would share one WAF. Hosts without paths keep
  today's keys.
- **Firewall and rate limits.** Per route, unchanged.
- **Access logs and analytics.** The traffic context maps host to service
  (`totals.rs:290`). It becomes host to a list of (path, service), and the
  agent picks the longest prefix on each request (`clean.rs:167`).
  `RequestRow` and `HostTotals` gain `serviceId`, totals are keyed by
  (minute, host, service). ClickHouse already has `service_id` on
  `traffic_1m` and `traffic_1h`, so no schema change there. Service pages
  filter by service id instead of host list. The cluster's raw request
  store gets the field with a default, so older rows read as before.
- **Previews.** A preview's copy gets one preview hostname of its own, at
  `/`, as before. Its original's paths, redirects and header rules aren't
  copied.
- **Compose sync.** `x-pyo.domains` accepts `example.com/api`. A path
  without a primary in the team is refused with "Add example.com to a
  service first." Strip prefix and redirects stay dashboard, CLI and API
  settings.
- **Changing a primary.** Its tunnel switch, TLS mode, CDN or headers change
  every sharer's routes. The save sends `SyncRoutes` to each service with a
  path entry for that hostname, in the same transaction (`routes::changed`).
- **Hand-off.** Removing the primary entry, or deleting its service, makes
  the oldest path entry the new primary in the same transaction:
  - its DNS record row moves (`UPDATE pyo.dns_records SET service_id, domain_id`)
  - its tunnel state, TLS mode and uploaded certificate id move
  - the agent of the new primary writes the listener. Before it writes the
    new Certificate, it copies the old certificate Secret's data under the
    new name, so HTTPS keeps answering while cert-manager renews.
  - the old service's apply removes its listener after the new one exists
    (both services are synced, the new primary first)
- **Removing the last entry** of a hostname removes its DNS record and
  certificate, as today.
- **Static sites.** Work as path entries. Strip prefix is usually on, so
  `/docs/index.html` is read as `/index.html`.

## 8. Migration

None. Entries without the new fields are primaries at `/`, and their
objects keep today's names and contents. Old agents ignore the new fields.
Since a path entry would be routed wrong by an old agent, the API refuses
paths, redirects and header rules on a cluster whose agent is older than
the release ("Update the cluster's agent to use paths.").

## 9. Edge cases

- **Primary at a path.** A hostname can have no `/` entry (only `/api` and
  `/app`). Requests elsewhere get the gateway's 404.
- **WebSockets** pass through path routes unchanged.
- **Strip prefix and redirects from the app.** An app that redirects to
  `/login` behind a stripped `/app` leaves its prefix. Docs say so.
- **Trailing slash.** `/api/` is stored as `/api`.
- **Case.** Paths are case sensitive, hostnames are not.
- **Query string on redirects.** The Gateway API keeps the query on a
  redirect and has no way to drop it. So "keep path and query" is one
  switch, and the query is always kept (see decisions).
- **HSTS on a hostname with plain HTTP.** Allowed. Docs say it only
  matters over HTTPS.
- **Service moved to another cluster** while it shares a hostname: refused
  by the cluster rule until its path entries are removed.
- **Primary service stopped.** Its Gateway and listener stay (routes stay
  when a service stops today), so path entries keep working.
- **Listener limit.** Unchanged: listeners are per primary service.
- **Two entries of one service on one hostname** (`/` to port web, `/api`
  to port api): allowed. Same rules as across services.

## 10. Interfaces

**API (OpenAPI).** `Domain` gets the new fields. No new endpoints:
`saveService` carries them, `removeDomain` takes the domain id.

**MCP.** `add_domain` gains `path`, `strip_prefix`. New tools
`add_redirect` (service_id, hostname, path, to_host, to_path, status,
keep_path) and `set_domain_headers` (service_id, domain_id, response,
request). `remove_domain` takes a domain id.

**CLI.**

```
pyo domain add example.com/api --service api --strip-prefix
pyo domain redirect www.example.com --to example.com [--status 302] [--no-keep-path]
pyo domain redirect example.com/old --to /new --service web
pyo domain header example.com --set 'Strict-Transport-Security: max-age=31536000' --remove X-Powered-By
pyo domain header example.com/api --request --set 'X-Region: eu'
pyo domain list        # shows paths as example.com/api
```

**Dashboard.** Minimal copy, no new pages.

- Service, Domains: each row shows `example.com/api` when it has a path. A
  small "Shared" tag when other services use the hostname, with a tooltip
  naming them. Path entries show the primary's DNS and certificate lines
  read only.
- Add domain dialog: a "Path" field, empty means `/`. "Strip prefix"
  switch, shown once a path is typed. Labels: "Path", "Strip prefix",
  "Send /api/x as /x".
- Row menu: "Redirect", "Headers". Headers opens a small list editor
  ("Response headers", "Request headers", "Set", "Remove") with presets
  for HSTS, frame options and nosniff.
- Team, Domains: rows grouped by hostname, one line per path. An "Add
  redirect" button next to "Add domain", picking a service for it (the
  target's service by default).
- No flashing: the shared tag and primary lines come from the store's
  service list, which is loaded before the page shows. Nothing is fetched
  per row.

**Docs (pyo-docs).** "Domains" gets sections "Paths", "Redirects" and
"Headers". CLI and MCP references.

## 11. Tests

- **Gateway object tests** (`gateway.rs` tests): path entry attaches to the
  primary's listener, strip prefix rewrite, redirect entries (host, path,
  keep path, 302), header filters merged with static and tunnel filters,
  :80 and tunnel and tailnet routes with matches, existing samples
  byte-for-byte unchanged (fingerprint).
- **Unit tests** (API): path and header validation, protected headers,
  conflict messages, `domain_specs` copying primary fields, hand-off
  ordering. WAF config with paths (`system.rs`), Go processor longest
  match (`waf/`). Traffic clean longest prefix (`clean.rs`).
- **Postgres tests**: sharing within a team, refusal across teams and
  clusters, proof skipped for a held hostname (Cloud mode), takeover
  removes all entries, hand-off on removing the primary and on deleting
  its service (DNS row moves), last entry removes the record, compose sync
  with a path, preview of a path entry.
- **Real k3s e2e.** The repo has none for routes in CI
  (`scripts/tunnel-gate-e2e.sh` is a hand-run script). Add
  `scripts/gateway-routes-e2e.sh` in the same style: two services on one
  hostname, longest prefix, strip prefix, a `www` redirect, a header rule,
  removing the primary. Run by hand on a throwaway machine before the
  release.

## 12. Estimate

Honest, in working days, including tests and CI fixes.

| Part | Days |
|---|---|
| Proto fields, gateway objects for paths, redirects, headers, tests | 2.0 |
| API: validation, conflicts, proof, primary fields, sibling sync | 2.0 |
| API: hand-off, DNS rows, tunnel loop, previews, compose sync, Postgres tests | 1.5 |
| WAF processor by host and path (Rust config and Go) | 0.75 |
| Analytics by path (agent clean and totals, API queries) | 1.5 |
| OpenAPI, MCP, CLI | 1.0 |
| Dashboard: service Domains, add dialog, headers editor, team Domains | 2.0 |
| pyo-docs | 0.5 |
| k3s e2e script, review fixes, CI | 1.0 |
| **Total** | **12.25** (range 11 to 15) |

Split by feature:

- **Redirects and header rules alone**: about 4 days. They fit the current
  one-service-per-hostname model. A `www` redirect is a redirect entry on
  a service. No hand-off, WAF or analytics work.
- **Path routes**: about 9 days on top. Most of the cost is sharing a
  hostname: hand-off, WAF by path, analytics by path.

## 13. Decisions needed

1. **Over budget.** 12 days against 6. Options:
   - A. Ship redirects and header rules first (about 4 days), path routes
     in the next release (about 9 days). **Pick.** Low risk, useful now,
     and path routes get their own release candidate.
   - B. Build all of it now as one release (about 12 days).
   - C. Path routes without per-path WAF and analytics (refuse sharing when
     the WAF differs, analytics per hostname). About 9.5 days for all. It
     cuts what makes paths safe, so not advised.
2. **Model.** Primary entry plus path entries (**pick**, no migration) or
   team-level domain objects (cleaner, redirect hosts need no service, but
   2 to 3 more weeks and a migration of live domains).
3. **Query string on redirects.** The Gateway API can't drop it. Keep it
   always (**pick**) or add Envoy Gateway's own filter for it (more Envoy
   surface, about half a day).
4. **Version.** A feature in a patch release goes against the versioning
   rule (features bump the minor: 0.52.0). Ship as 0.52.0 (**pick**) or
   keep 0.51.1.
