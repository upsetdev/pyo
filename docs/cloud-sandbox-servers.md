# Sandbox servers on Pyo Cloud

Design. Status: proposed, for review before any code.

## Goal

- A Pyo Cloud team adds a sandbox server the same way a self-hosted team does: one command, or later a Hetzner server.
- It uses the team's Tailscale (Integrations › Tailscale). Nothing else to set up.
- Pyo Cloud never joins a team's tailnet and never holds a usable Tailscale credential.
- A sandbox server runs untrusted code, so it gets no credential for Pyo Cloud.
- Self-hosted keeps working as it does now.

## Why it doesn't work today

A sandbox server talks to Pyo in four ways. Only one of them works without Pyo on the tailnet.

| Step | Today | On Cloud |
|---|---|---|
| Join | `sandbox.sh` posts its one-time code to Pyo's tailnet door (`http://<door>:5006/sandbox/join`). The door admits tailnet peers only (`join.rs`, `tailnet_peer`) | **Fails.** Pyo has no door on the team's tailnet. `add_server` refuses first because Pyo has no tailnet address |
| Pyo calls the daemon (reports, create, exec, templates, edge certificates) | `Daemon` calls `http://<tailnet IP>:7070`, or through the agent of `sandbox_servers.via_cluster` with the `SandboxCall` command | **Works already.** The agent relay was built for this (`pyo-agent/src/tailscale/sandbox.rs`) |
| Browser and SDK call the daemon (terminal, files, exec from code) | Straight to `http://<tailnet IP>:7070` with a sandbox token | **Fails in the browser.** The dashboard is on HTTPS, so a plain HTTP call to the daemon is blocked as mixed content. The SDK works when its machine is on the team's tailnet |
| Desktop and other web pages | `https://sandboxes.<tailnet>.ts.net/…`, Pyo's own name on the tailnet, which Pyo's API passes on to the daemon (`pages.rs`) | **Fails.** Pyo has no name on the team's tailnet |

Template images from public registries and Pyo's built-in images work as they are. Scripts and the daemon bundle come from the public release.

## Options

### (a) Relay through the team's clusters, join over HTTPS. Recommended

- The server joins the team's tailnet with a key from the team's holder (as today, `join_key`).
- It joins Pyo over Pyo's public HTTPS API with its one-time code, instead of the tailnet door. The answer has nothing secret: the server id, Pyo's public verifying keys and where the daemon listens.
- Pyo calls the daemon only through a connected cluster of the same team, with the signed `SandboxCall` command that exists today.
- The daemon also answers over HTTPS on its own tailnet name (`<server>.<tailnet>.ts.net`), with a certificate from its own `tailscaled`. Browsers and SDKs use that name for the terminal, files, exec and the desktop.

### (b) The sandbox server calls out to Pyo Cloud

- The daemon gets a credential at join (its own key, registered with the code). It long-polls Pyo Cloud for work and posts its reports, the way an agent does.
- Pyo Cloud never reaches in.
- The terminal, files and desktop still need a path from the browser to the daemon. Either the browser goes over the tailnet (the same HTTPS work as in (a)) or Pyo Cloud carries every stream (Pyo in the hot path, which the sandbox design avoids).

### (c) Others considered

- **A sandbox server as a server of a team cluster** (a k3s node with a taint, run by the agent). It reuses the agent's channel. But Firecracker and gVisor run on the host, outside k3s, and a node runs the cluster's workloads and secrets next to untrusted code. Rejected.
- **A `sandboxes` Tailscale Service on the holder's Gateway** that proxies the browser to each daemon. It keeps one name per team. But the Gateway would need a table of every sandbox server, and the holder's Envoy would carry every desktop stream. Kept as a fallback if per-server names run into a limit.

## Security on Pyo Cloud

| | (a) relay | (b) call out |
|---|---|---|
| **Compromised cloud-api** | Can sign sandbox tokens (Pyo's sandbox key is in its database, sealed) and ask the signer for `sandbox_call` commands. So it can run code in a team's sandboxes. That's the same reach it has today through deploys. The agent calls only a tailnet address, port 7070 and paths under `/v1/`. Hardening below narrows that to the team's sandbox servers | Same, through the daemon's own channel. The signer isn't in that path unless the daemon also checks `PYO_COMMAND_KEY` signatures, which is new code |
| **Compromised sandbox server** | Holds no Pyo Cloud credential. It can send bad answers back through the relay. Pyo already checks the report's server id and caps body sizes. It's a `tag:pyo` device on the team's tailnet, as on self-hosted | Holds a credential for Pyo Cloud's API, on a machine that runs untrusted code. An escape from a sandbox gets an authenticated channel into Pyo Cloud |
| **Compromised team cluster** (the relay) | Sees the calls for its own team's sandbox servers: short-lived tokens bound to one server, exec input and output, files. Same team, same trust as today. It can't reach another team's servers: the relay is always a cluster of the server's team | Not in the path |
| **Stolen join code** | 15 minutes, single use, rate-limited, stored as a hash. The answer has no secret. A fake address is caught: it must be a `tag:pyo` device on the team's tailnet, and the first report must carry the server's id | Same code, but it now buys a long-lived credential |
| **Per-team scoping** | Each `sandbox_call` is signed for the relay's cluster id. Sandbox tokens name one server. The relay is picked in one function that only returns a cluster of the same team, with a test | A credential per server |

Hardening for (a), in the same work:
- The agent calls a sandbox address only when it's a `tag:pyo` device it saw in its last tailnet sync. A compromised cloud-api can't use the relay to probe other devices on port 7070.
- On a hosted Pyo, the relay never falls back to another team's cluster or to Pyo's own. No team Tailscale means no sandbox servers, with a plain message.

## What changes

| | (a) relay | (b) call out |
|---|---|---|
| **API** | A public join route (`POST /api/v1/sandbox-servers/join`) that runs today's `answer()` without the tailnet-peer check, plus the `tag:pyo` device check. `add_server` on a hosted Pyo needs no door and gives `--pyo-url`. The relay is any connected cluster of the team on its tailnet, holder first. A migration adds `sandbox_servers.tailnet_name`. Connections give the daemon's HTTPS name when Pyo has no `sandboxes` name | A command queue and report intake for daemons, a credential per server, revocation |
| **Agent** | Only the hardening: check the address against the last sync. No new command, role or refresh | None |
| **sandbox.sh** | `--pyo-url` joins over HTTPS. Gets a certificate for the server's tailnet name (`tailscale cert`) and renews it on a timer. Sends that name with the join | Registers the daemon's key at join |
| **Daemon** | An HTTPS listener on its tailnet address (port 7443) with that certificate, for the same API and web pages. CORS and the Private Network Access preflight for the dashboard's origin | A client for Pyo Cloud: long poll, reports, retries. Plus the same HTTPS listener for the browser |
| **Dashboard** | Add server on Cloud once the team has Tailscale, else "Set up Tailscale on Integrations". Terminal, files and the desktop use the HTTPS name. The desktop check looks at the team's Tailscale | Same |
| **Docs** | Sandbox page and Cloud differences: sandbox servers on Cloud, what needs the tailnet | Same |
| **Cloud signer** | Nothing. `sandbox_call` isn't on the deny list | New kinds to sign, or none if the daemon trusts cloud-api directly |

Self-hosted stays as it is in (a). Servers join at Pyo's door, Pyo calls them straight over its tailnet, and the desktop stays on Pyo's `sandboxes` name. The daemon's HTTPS name is used there only when Pyo has no `sandboxes` name.

## The four contexts (option a)

| | Self-hosted | Pyo Cloud |
|---|---|---|
| **Team** | A team's sandbox servers join the team's tailnet with a key from its holder, or Pyo's tailnet when the team has no Tailscale of its own. They join at Pyo's door. Team owners and admins add them | Same key from the team's holder. They join over HTTPS. Pyo reaches them only through the team's own clusters. No team Tailscale means "Set up Tailscale on Integrations" |
| **Global** | The installation's own sandbox servers (no team) are unchanged and every team may use them | None. Each team has only its own (`shared()` is off on a hosted Pyo). Pyo Cloud staff see nothing new |

## Estimate

| | (a) | (b) |
|---|---|---|
| API: join route, relay choice, device check, migration, tests | 1.5 days | 3 days |
| sandbox.sh and daemon: HTTPS on the tailnet name, certificate renewal, CORS | 2 days | 5 days (outbound client, reports, retries, plus the same HTTPS work) |
| Agent hardening | 0.5 day | none |
| Dashboard and docs | 1 day | 1 day |
| End to end: Cloud staging with the signer, a real tailnet, throwaway VMs | 2 days | 3 days |
| **Total** | **about 7 days** | **about 12 days** |

Hetzner sandbox servers on Cloud are not in these numbers. Their first boot fetches join details from Pyo's door too (`cloud/bootstrap.rs`), so they need the same HTTPS join. Add about 1.5 days once (a) works.

## Risks

- **Signer rate.** Every relayed call is a signed command. The runner reads each server every 15 seconds, so 4 calls a minute per server, plus creates and execs. The signer allows 120 a minute per cluster. Spreading calls over the team's connected clusters keeps a team with 10 servers well under. Many execs through Pyo's API (MCP, the API's exec) are the risk. SDKs and the dashboard go straight to the daemon and don't count.
- **Relay latency.** A relayed call waits for the agent's long poll. Fine for reports and control. Interactive use goes straight to the daemon.
- **Private Network Access.** Chrome checks a public page calling a tailnet address. The daemon must answer the preflight. To check early with a real browser.
- **Certificates.** `tailscale cert` needs HTTPS certificates on in the tailnet. Pyo's Tailscale setup already turns them on. Each server's name shows in the public Certificate Transparency logs, which Tailscale already documents for every HTTPS name.
- **Holder gone.** Calls go through any connected cluster of the team, so a missing holder pauses only new servers (they need a key from it), as for clusters today.
- **Metering trust.** Usage comes from the daemon's report, as on self-hosted. On Cloud it would matter if sandboxes are ever billed by use.

## Open questions

1. Pick (a)?
2. Hetzner sandbox servers on Cloud in the same release, or after?
3. Should the browser use the daemon's HTTPS name on self-hosted too? It would likely also fix the terminal when the dashboard is opened over HTTPS there (not checked). It changes nothing else.
