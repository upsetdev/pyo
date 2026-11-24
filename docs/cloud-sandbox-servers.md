# Sandbox servers on Pyo Cloud

Design for 0.51.1. Status: proposed, for review before any code. The chosen approach is (b): the sandbox server calls out to Pyo Cloud.

## Goal

- A Pyo Cloud team adds a sandbox server with one command, the same as a self-hosted team.
- It joins the team's tailnet with a key from the team's Tailscale (Integrations › Tailscale).
- It registers with Pyo Cloud over HTTPS with a one-time code, and gets a credential of its own.
- After that it keeps an outbound connection to Pyo Cloud for its work. Pyo Cloud never reaches into the tailnet.
- Commands are signed and scoped like agent commands.
- The desktop and private ports are reachable from the team's tailnet devices.
- Self-hosted keeps today's direct flow.

## Why it doesn't work today

| Step | Today | On Cloud |
|---|---|---|
| Join | `sandbox.sh` posts its one-time code to Pyo's tailnet door (`http://<door>:5006/sandbox/join`), which admits tailnet peers only | Fails. Pyo has no door on the team's tailnet |
| Pyo → daemon (reports, create, exec, templates, edge certificates) | Pyo calls `http://<tailnet IP>:7070`, or relays through a cluster's agent (`SandboxCall`) | Pyo can't reach it directly |
| Browser and SDK → daemon (terminal, files, exec from code) | Straight to `http://<tailnet IP>:7070` with a sandbox token | The dashboard is on HTTPS, so a plain HTTP call is blocked as mixed content |
| Desktop and web pages | `https://sandboxes.<tailnet>.ts.net/…`, Pyo's own name on the tailnet | Pyo has no name on the team's tailnet |

## How it works

```text
 team's tailnet                                 Pyo Cloud (public HTTPS)
 ┌──────────────────────────┐                   ┌────────────────────────┐
 │ sandbox server           │  1 join (code)    │ cloud-api              │
 │  sandbox.sh ─────────────┼──────────────────▶│  /sandbox-agent/join   │
 │  pyo-sandbox daemon      │  2 long poll      │  /sandbox-agent/poll   │
 │   (its own key) ─────────┼──────────────────▶│  /sandbox-agent/report │
 │                          │◀── signed cmds ───│  signer (KMS) signs    │
 │  :7443 HTTPS on its      │                   └────────────────────────┘
 │  tailnet name            │
 └─────▲────────────────────┘
       │ terminal, files, desktop, SDK (token from Pyo Cloud)
   team's devices on the tailnet
```

1. **Add.** A team owner or admin adds a sandbox server. Pyo Cloud makes its row (joining), a one-time join code and a single-use Tailscale auth key from the team's holder (`join_key`, as today). The dashboard shows one command with both, plus Pyo Cloud's command key (below).
2. **Join the tailnet.** `sandbox.sh` joins with the key, gets a certificate for the server's tailnet name (`tailscale cert`) and installs the daemon.
3. **Register.** The daemon makes an Ed25519 key on the server and posts its public half with the code to `POST /api/v1/sandbox-agent/join`, over HTTPS. Pyo Cloud checks the code, keeps the public key and answers with the daemon's config: its server id, the key id, Pyo's sandbox token keys and the command key.
4. **Work.** The daemon long-polls `POST /api/v1/sandbox-agent/poll` for its commands, and posts its report every 15 seconds and each command's result. Every request is signed with its key (pyo-proto `agentauth`, as agents do on Cloud).
5. **Use.** Browsers and SDKs on the tailnet call the daemon at `https://<server>.<tailnet>.ts.net:7443` with sandbox tokens from Pyo Cloud, as they call `http://<ip>:7070` today.

Self-hosted keeps today's flow: the door join, and Pyo calling the daemon. The outbound mode is on when `sandbox.sh` gets `--pyo-url` instead of `--pyo`. Pyo gives `--pyo-url` only on a hosted Pyo.

## Security model

### Commands: signed and scoped

- Every command to a sandbox server is signed by Pyo Cloud's signer, like an agent command, with its own prefix (`pyosbx1.`) so a sandbox command can never pass for an agent command, or the other way.
- The claims name the command id and kind, the **server id** (`sbs_…`) as the subject, the **team id**, a sequence number that only grows per server, when it was signed and when it expires, and the SHA-256 of the command's exact JSON. The signer never sees the command, so sandbox env values in it never reach the signer.
- The daemon runs a command only when it's signed for its own server id and team, its sequence is new and it hasn't expired. Anything else is refused and reported.
- The command key: the daemon trusts the keys given in the join command (`--command-key`), as an agent trusts `PYO_COMMAND_KEY` from its connect command. Session-key delegation works as for agents, so the signer signs a few times an hour, not every command.
- Results and reports are signed by the server's own key. Pyo Cloud takes a result only for a command it sent to that server.

### The server's credential

- **What it is.** An Ed25519 key made on the server (`/var/lib/pyo-sandbox/key`, root, mode 0600, outside every sandbox). Pyo Cloud keeps only its public half.
- **What it can do.** Fetch that server's own commands, post that server's report and results, renew its own key. Nothing else: no team API, no other server, no agent route.
- **Lifetime.** Good for a day. The daemon swaps in a new key at half that (`/sandbox-agent/renew-key`). The old one stays good for a few minutes, as for agents.
- **Revocation.** Removing the server deletes its keys at once. **Reset access** on the server's menu revokes its keys and gives a new join command for the same server. A server that stays offline past its key's day has to join again with a new code.

### The join code

- 15 minutes. Single use: the first ask burns it, whatever comes of it.
- Kept only as its hash. Bound to one server and one team.
- Wrong codes are rate-limited per server and overall, as today.
- The answer has no secret. A stolen code lets someone register their own key for that server before the real one does. The real server's join then fails, the team sees it, and **Reset access** makes a new code. To make this harder the join also needs the server's tailnet address to be a `tag:pyo` device on the team's tailnet, as seen in the holder's last sync.

### What each breach could do

| Breached | What it could do | What it can't |
|---|---|---|
| **cloud-api** | Queue commands for any team's sandbox servers. The signer signs what cloud-api asks (it's a deny list), so it could create sandboxes or run commands in existing ones. That matches what it can do to clusters today. It could hand a new server a command key of its own at join, as with a new agent. It holds Pyo's sandbox token key, but tokens are useless without a path into the tailnet | Reach into a team's tailnet. Use a server's credential. Run a command the signer didn't sign |
| **The signer** | Sign any command for any server. Same as for agents | Deliver it. It still needs cloud-api |
| **A sandbox server** (an escape from a sandbox to the host) | Use that server's credential: read that server's commands (sandbox specs and env values for sandboxes on it), post false reports (capacity, usage), take that server's edge certificate. All of that is on the host already. It's a `tag:pyo` device on the team's tailnet, as on self-hosted | Act as another server, team or agent. Reach any other route of Pyo Cloud |
| **A team cluster** | The holder mints the join key, and sees what it sees today. It's not in the sandbox path | Read or send sandbox commands |
| **A team member's device on the tailnet** | Reach the daemon's port. The daemon still needs a sandbox token for every call | Act without a token |

The new risk compared with today's agents: a machine that runs untrusted code holds a credential for Pyo Cloud. It's scoped to that one server's channel, short-lived and revocable, and everything it opens is already on that host.

## The desktop and private ports

- **On the tailnet.** The daemon answers HTTPS on its tailnet name at :7443, with the certificate its `tailscaled` gets. The same API and web pages as :7070: terminal, files, exec, the desktop and private ports (`/v1/sandboxes/{id}/web/{port}/`). Connections from Pyo Cloud give that URL. It answers CORS and Chrome's Private Network Access preflight for the dashboard's origin. Needs HTTPS certificates on the tailnet, which Pyo's Tailscale setup already turns on.
- **Off the tailnet.** The terminal, files and desktop say "Opens on your tailnet." Pyo Cloud's API and MCP exec still work, through the channel. Ports opened to the web keep working on the edge domain, as today.
- **Not in this design:** opening the desktop on the public edge. That would put a full desktop on the internet behind a token, so it's a separate decision.

## The four contexts

| | Self-hosted | Pyo Cloud |
|---|---|---|
| **Team** | Unchanged. A team's sandbox servers join its tailnet (or Pyo's when the team has none), at Pyo's door. Pyo calls them directly | New flow. Key from the team's holder, join and work over HTTPS, outbound only. No team Tailscale means "Set up Tailscale on Integrations" |
| **Global** | The installation's own sandbox servers (no team) are unchanged, and every team may use them | None. Each team has only its own. Pyo Cloud staff see nothing new, and the signer needs one change (below) |

## Work per component

| Component | Work | Days |
|---|---|---|
| **pyo-proto** | Sandbox command envelope and claims (`pyosbx1.`, server and team ids, sequence), poll, report and result types, tests | 1 |
| **API** | Join route (code, key enrollment, `tag:pyo` check). Signed-request auth for sandbox servers (reuse `agentauth`). A command queue per server with sequences, long poll, results and timeouts. Report intake. A `Daemon` that queues instead of calling, for outbound servers, so the runner, templates, edge certificates and exec work unchanged above it. Renew and revoke keys. Migration. Tests on a throwaway Postgres | 4 |
| **Daemon** | Outbound client: enroll, renew, long poll, verify signatures, run the command, post results, report every 15 seconds, back off when offline. HTTPS listener on the tailnet name with certificate reload, CORS, Private Network Access. Tests | 4 |
| **sandbox.sh** | `--pyo-url` and `--command-key`, `tailscale cert` with a renewal timer, the outbound config | 1 |
| **Cloud signer** (pyo-cloud) | Sign the `pyosbx1.` prefix for `sbs_` subjects, its own rate limit per server, deny-list check by kind | 1 |
| **Dashboard** | Add server on Cloud, Reset access, "Opens on your tailnet", terminal, files and desktop on the HTTPS name | 1 |
| **Docs** | Sandbox page, Cloud differences, the security notes | 0.5 |
| **End to end** | Staging Cloud with the staging signer, a real tailnet, throwaway VMs: join, work, renew, revoke, offline and back, desktop in a real browser | 3 |
| **Total** | | **about 15.5 days** |

Hetzner sandbox servers on Cloud are not in this. Their first boot uses Pyo's door too (`cloud/bootstrap.rs`). Add about 1.5 days once this works.

## Risks

- **Bigger daemon.** The daemon becomes a client of Pyo Cloud with its own retries and state. Bugs there cost more than in the relay design.
- **Private Network Access.** Chrome checks a public page calling a tailnet address. The daemon must answer the preflight. To check early in a real browser.
- **Signer change.** The signer gets a new prefix and subject type. It needs its own review and a staged rollout.
- **Exec through the channel.** API and MCP exec wait for the daemon's next poll. Long polls keep this under a second, but long-running execs need a cap and a clear timeout message.
- **Metering trust.** Usage comes from the server's own report. A breached server could under-report. It matters only if sandboxes are billed by use.

## Decisions needed

1. The signer signs sandbox commands with a new prefix and `sbs_` subjects. Yes?
2. A machine that runs untrusted code holds a short-lived, revocable credential for Pyo Cloud, scoped to its own channel. Accept?
3. The command key comes in the join command, as for agents (trust on first use). Or pin Pyo Cloud's signer key in the release, which is stronger but needs a release to rotate it?
4. Desktop off the tailnet stays unavailable for now. Yes?
5. Hetzner sandbox servers on Cloud: in 0.51.1 or after?

## Considered and not chosen

- **(a) Relay through the team's clusters.** Pyo calls the daemon through a team cluster's agent (`SandboxCall`, which exists), and the server joins over HTTPS without a credential of its own. About 7 days, no signer change, and no credential on a machine that runs untrusted code. Not chosen: the relay puts a team cluster in the sandbox path, and each call costs a signed command.
- **A sandbox server as a server of a team cluster.** Firecracker and gVisor run on the host, outside k3s, and a cluster server would run the cluster's workloads and secrets next to untrusted code.
