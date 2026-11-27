# Sandbox servers on Pyo Cloud

Design for 0.51.1. Status: approved 2026-11-24. Stages 1 to 3 (the API, the daemon, sandbox.sh and the dashboard) are built. Stage 4 (Pyo Cloud's signer, then end to end on staging) is next. The chosen approach is (b): the sandbox server calls out to Pyo Cloud.

Self-hosted Pyo moves to the same channel in 0.51.1 too (decided 2026-11-24). See [Self-hosted](#self-hosted) at the end. Pyo's direct path to sandbox servers is gone in 0.52.0 (decided 2026-11-27). See [Removing the direct path](#removing-the-direct-path-0520).

## Goal

- A Pyo Cloud team adds a sandbox server with one command, the same as a self-hosted team.
- It joins the team's tailnet with a key from the team's Tailscale (Integrations › Tailscale).
- It registers with Pyo Cloud over HTTPS with a one-time code, and gets a credential of its own.
- After that it keeps an outbound connection to Pyo Cloud for its work. Pyo Cloud never reaches into the tailnet.
- Commands are signed and scoped like agent commands.
- The desktop and private ports are reachable from the team's tailnet devices.
- Self-hosted keeps today's direct flow. (Changed in 0.51.1: see [Self-hosted](#self-hosted).)

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
 │  sandbox.sh ─────────────┼──────────────────▶│  …/join                │
 │  pyo-sandbox daemon      │  2 long poll      │  …/commands            │
 │   (its own key) ─────────┼──────────────────▶│  …/report              │
 │                          │◀── signed cmds ───│  signer (KMS) signs    │
 │  :7443 HTTPS on its      │                   └────────────────────────┘
 │  tailnet name            │
 └─────▲────────────────────┘
       │ terminal, files, desktop, SDK (token from Pyo Cloud)
   team's devices on the tailnet
```

1. **Add.** A team owner or admin adds a sandbox server. Pyo Cloud makes its row (joining), a one-time join code and a single-use Tailscale auth key from the team's holder (`join_key`, as today). The dashboard shows one command with both, plus Pyo Cloud's command key (below).
2. **Join the tailnet.** `sandbox.sh` joins with the key, gets a certificate for the server's tailnet name (`tailscale cert`) and installs the daemon.
3. **Register.** The daemon makes an Ed25519 key on the server and posts its public half with the code to `POST /api/sandbox-agent/v1/join`, over HTTPS. Pyo Cloud checks the code, keeps the public key and answers with the daemon's config: its server id, the key id, Pyo's sandbox token keys and the command key.
4. **Work.** The daemon long-polls `GET /api/sandbox-agent/v1/commands` for its commands, and posts its report every 15 seconds and each command's result. Every request is signed with its key (pyo-proto `agentauth`, as agents do on Cloud).
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
- **Lifetime.** Good for a day. The daemon swaps in a new key at half that (`/api/sandbox-agent/v1/renew-key`). The old one stays good for a few minutes, as for agents.
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

## Decisions (2026-11-24)

1. The signer signs sandbox commands under a new prefix (`pyosbx1.`), for one server (`sbs_…`) and its team, with a sequence number per server and an expiry.
2. A sandbox server holds its own short-lived, revocable credential: an Ed25519 key it makes and registers at join, good for about a day, renewed by itself, revoked by **Reset access** or when the server is removed. It opens only that server's command channel.
3. The command key comes in the join command (trust on first use), as for agents.
4. The desktop works only from devices on the team's tailnet for now.
5. Hetzner sandbox servers on Cloud are out of scope (0.52.0).

Self-hosted keeps today's flow. This is only for a multi-tenant Pyo (`hosted()`), and only for teams with their own Tailscale.

## Stages

1. **API** (built): join codes, the join, the server's keys, the command queue and long poll, results, reports, Reset access, tests.
2. **Daemon** (built): the outbound client and its HTTPS listener. In the API, a `Daemon` that queues instead of calling for outbound servers, so placement, templates, edge certificates and exec work over the channel.
3. **sandbox.sh and the dashboard** (built), with the docs.
4. **Pyo Cloud's signer**, then end to end on staging.

### What stage 1 settled

- The channel lives at `/api/sandbox-agent/v1`, beside `/api/agent/v1`, not under `/api/v1` (that's for people and API keys). Pyo mounts it only when it's hosted.
- The Tailscale key is made with the join code, as in step 1 above. The server is on the tailnet before it joins Pyo, so the join can check it's there.
- The `tag:pyo` check: the join's tailnet name must end in the team's tailnet suffix, and its hostname must be one of the `tag:pyo` machines in the holder's last sync. A server not listed yet gets 409 and the code keeps working. Pyo asks the holder to read the tailnet again with its next pass (about 20 seconds). Every other refusal burns the code.
- Commands are kept sealed (they carry env values and certificate keys) and dropped once answered. A command waits at most 10 minutes for the server, then 10 more for its answer. Answered ones go after a day.
- A daemon that restarts (a new `x-pyo-sandbox-instance`) gets nothing again: what went to the process before fails with "The sandbox server restarted before it answered." Running a create twice is worse than asking again.
- Until stage 2, an outbound server takes no new sandboxes (placement skips it), and the runner doesn't call it. Its report comes in over the channel.

### What stage 2 settled

- **The join is its own step.** `pyo-sandbox join` joins and writes the config, then `pyo-sandbox serve` works. sandbox.sh runs the join, so it can show Pyo's answer to the person who ran the command.
- **The server's files.** Its key, the key's id and when it ends are in `/var/lib/pyo-sandbox/key`. Its server, team, command keys and last sequence are in `/var/lib/pyo-sandbox/outbound.json`. Both are root's, mode 0600, and replaced whole by a rename. The sequence is kept before a command runs, so a crash never runs it twice. A new join of the same server keeps its last sequence.
- **Commands run side by side.** The daemon checks them one at a time, in order, then runs each in its own task. A long exec doesn't hold up a delete.
- **A refused command** is answered with an `error` result, code `forbidden`, and "The sandbox server refused the command: <why>." The daemon logs it too.
- **New command kinds** for Pyo's own calls that aren't commands on a self-hosted server: `exec` (`{id, request}`, answered `{type: exec, result}`), `readFile` (`{id, path}`, answered `{type: file, data}` in base64) and `writeFile` (`{id, path, data}` in base64, answered `{type: written, bytes}`). A self-hosted daemon takes them on `POST /v1/commands` too, with the token's `exec` or `files` scope.
- **Limits over the channel.** An exec runs at most 5 minutes and keeps 1 MiB of each output (`truncated` past that). Files go up to 2 MiB. A command's JSON goes up to 3 MiB. Bigger files go from a device on the tailnet, and the error says so.
- **Where clients reach it.** For an outbound server, `connection.url` and `connection.webUrl` are `https://<tailnetName>:7443`, and so is a sandbox's `url` in lists.
- **Templates.** After each report over the channel, Pyo asks the server for the templates it lacks and to delete the ones Pyo dropped, as the runner does for the servers it calls. A server pulls template images from their registries itself. Images in Pyo's own registry don't reach it, as that registry is on Pyo's tailnet. Public images do, `base` among them.
- **Revocation.** On the 401 with "This server's key is unknown, expired or revoked…" (pyo-proto `outbound::KEY_GONE`), or once its key's time is over, the daemon stops polling, reporting and renewing, and logs "Pyo no longer takes this server's key…". It keeps serving its sandboxes on the tailnet.
- **Removing an outbound server** doesn't delete its sandboxes, as in stage 1: its key stops working at once. They end at their lifetime, or with `sandbox.sh --uninstall`.

## Interface for stages 2 to 4

### The API (stage 1, for the dashboard)

| Operation | Request | Answer |
|---|---|---|
| `createSandboxJoinCode` | `POST /api/v1/sandbox-servers/join-codes` `{teamId, name, edgeDomain?}` | `{server, command, expiresAt, pyoUrl}` |
| `resetSandboxServerAccess` | `POST /api/v1/sandbox-servers/{id}/reset-access` | the same, for the same server |

- Owners and admins, full API keys. Not MCP tools: the command carries a Tailscale key.
- Self-hosted answers 409 ("This Pyo adds sandbox servers with addSandboxServer"). A team without Tailscale gets 400. At most 20 codes per team an hour (429).
- `server` (as `listSandboxServers` shows it) has `outbound` and `tailnetName`.
- The command is, exactly:

  ```text
  curl -fsSL <scripts>/sandbox.sh | bash -s -- --pyo-url '<url>' --server '<sbs_…>' --name '<name>' --code '<code>' --tailscale-key '<key>' --command-key '<keys>' --version '<version>' [--edge-domain '<domain>']
  ```

  `<url>` is Pyo's `PYO_AGENT_API_URL`. `<keys>` are the signer's public keys, base64url and comma separated.

### sandbox.sh (stage 3)

- Takes the flags above. `--pyo-url` instead of `--pyo` means the outbound mode.
- Joins the tailnet with the key (`tag:pyo`, hostname = the name), gets the certificate for its tailnet name with `tailscale cert` and sets a renewal timer, installs the daemon.
- Hands the code and the command key to the daemon in a root-only file, never in its arguments.
- The daemon's side of this handoff is below, under "What stage 3 settled". Beyond that, the daemon:
  - Prints "Waiting for your tailnet to list this server…" on stdout while Pyo answers 409, and "Joined Pyo as <id>." once it's done.
  - Keeps its key in the pool `local.json` names (`/var/lib/pyo-sandbox` by default).
  - Reads `tailscale status --json` for the address and names when the join file has none. It takes `commandKey` for `commandKeys` too.
  - Takes `tailnetHttps` (`{listen, cert, key}`) in `local.json` in place of the defaults: `<tailnet IP>:7443` and the config's folder plus `tailnet/`.
  - Keeps listening on `<tailnet IP>:7070` as before.

What stage 3 settled, for the daemon's side (stage 2):

- **The certificate.** `tailscale cert` writes `/etc/pyo-sandbox/tailnet/cert.pem` and `key.pem` (the folder 0700, the key 0600). A timer (`pyo-sandbox-cert.timer`, every 12 hours) runs it again, and `tailscale cert` gets a new one only near the end. The daemon serves :7443 on its tailnet address with them, and reloads them when they change. The folder is the config's folder plus `tailnet/`.
- **The join.** `sandbox.sh` writes `/etc/pyo-sandbox/join.json` (root, 0600), then runs `pyo-sandbox join --config /etc/pyo-sandbox/config.json --join /etc/pyo-sandbox/join.json` with `RUST_LOG=off`. The file is the `JoinRequest` without `publicKey`, plus `pyoUrl` and `commandKeys`:

  ```json
  {"pyoUrl": "https://agent.pyo.example", "serverId": "sbs_…", "code": "…", "commandKeys": "<base64url>,<base64url>",
   "address": "100.64.0.9", "hostname": "sbx-1", "tailnetName": "sbx-1.tail1234.ts.net",
   "jailUid": 998, "jailGid": 997, "capability": { …pyo-sandbox detect's report… }}
  ```

  The daemon makes its key, posts the join (409: again every 5 seconds until the code runs out), checks `commandKeys`, and writes its key, `outbound.json` and `config.json` (the answer's `config`, 0600). It exits 0 once they're written. Otherwise it exits non-zero, and its last line on stderr is one plain sentence for people (a leading `Error: ` is dropped), such as Pyo's refusal. `sandbox.sh` deletes the join file either way, and takes a written `config.json` as joined.
- **Outbound from then on.** `config.json` has `outbound`, so a run without a code updates the daemon, the certificate timer and the firewall (7070 and 7443 on `tailscale0`), as on self-hosted.
- **Already on a tailnet.** A machine that's on a tailnet without `tag:pyo` isn't moved: `sandbox.sh` stops and says to log out of Tailscale first.

### The daemon (stage 2)

Wire types are pyo-proto `sandbox::outbound`. Paths are under `<url>/api/sandbox-agent/v1`.

1. **Join.** Make an Ed25519 key (`/var/lib/pyo-sandbox/key`, root, 0600). `POST /join` with `JoinRequest`: `serverId`, `code`, `publicKey`, `address` (tailnet IP), `hostname` (`tailscale status` Self.HostName), `tailnetName` (Self.DNSName), `jailUid`, `jailGid`, `capability`.
   - 200: `JoinAnswer` with `serverId`, `teamId`, `keyId`, `notAfter`, `commandKeys` and `config` (the self-hosted config, plus `outbound: {pyoUrl, teamId, tailnetName}`). Refuse to go on when `commandKeys` differ from `--command-key`.
   - 409: not listed on the tailnet yet. Ask again every 5 seconds until the code runs out.
   - 403, 422, 429: stop and print the message.
2. **Sign every other request** as agents do (pyo-proto `agentauth`): `pyo-agent-key: <keyId>` and `pyo-agent-signature: t=…,n=…,s=…` over the method, the path with its query, the time, a new nonce and the body's SHA-256. The clock must be within a minute of Pyo's.
3. **Renew** at half the key's life: `POST /renew-key {publicKey}` with the current key, answer `{keyId, notAfter}`. The old key works 15 more minutes.
4. **Poll** `GET /commands?wait=25` with `x-pyo-sandbox-instance: <random per process>`. Each `CommandEnvelope` is `{id, kind, signed}`. Check it with pyo-proto `TrustedKeys::verify_sandbox(signed, id, serverId, teamId, lastSeq, now)` (the `command-signing` feature, new for pyo-sandbox), keep `lastSeq` on disk, run the `DaemonCommand` in `signed.command`. Refused: answer an `error` result with code `forbidden` and the reason.
5. **Answer** `POST /commands/{id}` with the `DaemonResult`. A 404 means it isn't wanted any more: drop it.
6. **Report** `POST /report` with the `ServerReport` every 15 seconds.
7. **401** with "This server's key is unknown, expired or revoked…": stop polling and reporting, keep the sandboxes, and log that it needs a new join command. Other failures: back off from 1 to 30 seconds.
8. **HTTPS** on `<tailnetName>:7443` with the `tailscale cert` certificate (reloaded when it changes), the same API and pages as :7070, CORS and Private Network Access for the dashboard's origin.

### Pyo Cloud (stage 4)

- `RemoteSigner` implements `CommandSigner::sign_sandbox(server_id, team_id, items)`. Until then the default refuses, so polls answer 503 and commands wait.
- The signer gets an endpoint like `/v1/sign-sandbox {serverId, teamId, items}` → `{signatures}`. It checks `serverId` starts with `sbs_` and `teamId` is set, keeps a sequence per server that only grows, signs `pyosbx1.` + base64url(`SandboxClaims`) for 10 minutes, rate-limits per server, and keeps its deny list by kind. Its session key's delegation adds the purpose `sandboxCommands`.
- The kinds it signs are the `type`s of pyo-proto's `DaemonCommand`: `create`, `delete`, `pause`, `resume`, `setTtl`, `get`, `list`, `report`, `snapshotTemplate`, `openPort`, `closePort`, `setEdgeCert`, `prepareTemplate`, `deleteTemplate`, `template`, and from stage 2 `exec`, `readFile` and `writeFile`.
- The agent host's Cloudflare rule allows `/api/sandbox-agent/` next to `/api/agent/`, `/api/builder/` and `/api/static/`.
- `PYO_AGENT_API_URL` is the public agent URL: it's what `--pyo-url` carries.

## Considered and not chosen

- **(a) Relay through the team's clusters.** Pyo calls the daemon through a team cluster's agent (`SandboxCall`, which exists), and the server joins over HTTPS without a credential of its own. About 7 days, no signer change, and no credential on a machine that runs untrusted code. Not chosen: the relay puts a team cluster in the sandbox path, and each call costs a signed command.
- **A sandbox server as a server of a team cluster.** Firecracker and gVisor run on the host, outside k3s, and a cluster server would run the cluster's workloads and secrets next to untrusted code.

## Self-hosted

Status: built 2026-11-24, with unit, Postgres and end-to-end tests (KVM, in CI). The staging test on throwaway servers is next.

Decided 2026-11-24: in 0.51.1 a self-hosted Pyo's sandbox servers use the same channel as Pyo Cloud's. New servers join and get their work over `/api/sandbox-agent/v1`. Pyo stops calling daemons for them. Servers that joined before keep the direct path for one release.

### Signing

- **A local signer.** `sandbox::outbound::signer::Local` implements `CommandSigner::sign_sandbox` and nothing else. Its `sign` and `sign_reply` refuse. It is never the build's command signer (`command_signer()`), so agents keep their token auth and get no `PYO_COMMAND_KEY`. Nothing changes for clusters.
- **Its own key.** An Ed25519 key (pyo-proto `KeySigner`), made on first use under an advisory lock. Its PKCS#8 is kept in `pyo.sandbox_command_keys`, sealed with the installation's key (`PYO_SECRET_KEY`), as the sandbox token key is. It's the installation's key, not a team's, so a team's data key doesn't fit. Without `PYO_SECRET_KEY` there are no sandbox servers, as today.
- **Never the token key.** Sandbox tokens and sandbox commands both sign `pyosbx1.<claims>`. With one key for both, a token's signature could be read as a command's, or the other way. Their claims don't parse as each other's today, but that is luck, not design.
- **Sequence and expiry.** Each command gets the server's next number from `sandbox_servers.command_seq`, raised in the statement that reads it. It's never reset, so a server that joins again keeps working with the last number it kept. Commands expire after 10 minutes, as on Cloud.
- **How the daemon trusts it.** As on Cloud. The join command carries `--command-key` with the local signer's public key, and the daemon refuses a join answer with other keys. Trust on first use, from the person who ran the command as root.
- **Rotation.** None in 0.51.1. The key lives as long as the installation, like the token key. Whoever can read it can already sign tokens for every daemon. A rotation would make a new key and have each server run its command again (**Update**, below, gives the current keys). A later release can let the old key sign the new one over, so servers learn it without a new command.

### Where the server reaches Pyo

The join command's `--pyo-url` is, first of:

1. **Pyo's tailnet address**, when the server joins Pyo's own tailnet: the installation's servers, and teams with no Tailscale of their own or whose tailnet is Pyo's. That's `https://<Pyo's name>.<tailnet>.ts.net`, or `http://<Pyo's tailnet IP>:8000` before Pyo has its HTTPS name. Clusters' agents reach Pyo there too.
2. **Pyo's public HTTPS address**, for a team on a tailnet of its own: the public dashboard while it's open, or `PYO_PUBLIC_API_URL`.
3. **Neither:** the add is refused with "Pyo isn't reachable from your team's tailnet. Turn on Public dashboard on the Tailscale card, then add the server again." Such a server can't reach Pyo's door today either, so nothing that works now stops working.

Why the tailnet first: a self-hosted Pyo may have no public address. Sandbox servers need a tailnet anyway. Traffic stays inside WireGuard. And an admin can close the public dashboard at any time.

- The chosen URL is kept on the server's row (`pyo_url`), and the join answer names the same one.
- The daemon and `sandbox.sh` take plain `http://` to a tailnet address (100.64.0.0/10, fd7a:115c:a1e0::/48) as well as to this machine. WireGuard encrypts it. It's the rule `secure_transport` already has for agents.

### The routes

- `/api/sandbox-agent/v1` is mounted on every Pyo. Hosted: with the build's signer. Self-hosted: with the local signer.
- On a self-hosted Pyo they answer wherever the API does: the tailnet name, the public dashboard while it's open, and port 8000.
- The join needs a live code (15 minutes, one use, rate-limited) and a `tag:pyo` machine the tailnet lists. Every other route needs a server's key. That's the same exposure as on Cloud.
- The door's `/sandbox/join` stays for one release, for servers that got a code before the upgrade.

### Teams

- **A team's server.** As on Cloud. The claims, its keys and its commands carry the team. Its requests run lowered to the team (rls.rs). Its commands are sealed with the team's data key.
- **The installation's server** (no team, self-hosted only, every team may use it, `shared()`):
  - Its claims carry the team `installation`. Team ids start with `team_`, so it's never a team's.
  - Its keys and commands have no team. The migration lets their `team_id` be empty. Row security shows such rows to no team.
  - Its requests aren't lowered, since there's no team to lower to. Every query is bound to the server's id.
  - Its commands are sealed with the installation's key, like Pyo's other secrets.
  - Only the installation's admin adds it, updates it or resets its access, as today.
  - A hosted Pyo never has one. The channel refuses a key whose server has no team there.
- **The tailnet the join checks** is the one its Tailscale key came from: the tailnet of `via_cluster`'s team. That's the team's own holder, else the installation's (`team_key_cluster`). On Cloud it's the team's, as before.
- **Sharing doesn't change.** Every team may place sandboxes on the installation's servers. Sandbox tokens still name the sandbox's team. A daemon refuses a command for another server, whatever its team.

### Migration

- **New servers.** In 0.51.1, `addSandboxServer` and a Hetzner sandbox server made by a self-hosted Pyo both make outbound servers. `createSandboxJoinCode` stays Cloud's: self-hosted keeps its one add operation, with the same answer.
- **Servers that joined before** were to keep the direct path for one release. There was no 0.51.1 release, so from 0.52.0 they take no commands until **Update** (see [Removing the direct path](#removing-the-direct-path-0520)).
- **Update** (`resetSandboxServerAccess`, now for these servers too, self-hosted only) gives a fresh command for the same server. It doesn't stop the server. Run on the server, it joins over the channel. At the join the row turns outbound and Pyo stops calling it. Its sandboxes keep their records and keep running: the daemon restarts with its new config, as on any update. The API marks such a server `needsUpdate`.
- **Removal of the direct path:** 0.52.0. See [Removing the direct path](#removing-the-direct-path-0520).
- **Removing an outbound server** stops its key at once but doesn't delete its sandboxes, as on Cloud. They end at their lifetime, or with `sandbox.sh --uninstall` there. The docs say so.

### The four contexts

| | Self-hosted | Pyo Cloud |
|---|---|---|
| **Team** | New servers join outbound. The URL is Pyo's tailnet address when the team uses Pyo's tailnet, else Pyo's public address. Commands signed by the local signer, for that server and team. Older servers keep the direct path until **Update** | Unchanged from stage 3: the build's signer, `PYO_AGENT_API_URL`, `createSandboxJoinCode` |
| **Global** | The installation's servers join outbound on Pyo's tailnet, with the team `installation` in their claims and no team on their rows. Every team may use them, as today | None, as before |

### What building it settled

- **`Mode`.** `resetSandboxServerAccess` and the join take hosted or self-hosted as an input (`outbound::Mode`, `hosted` on `join`), and self-hosted's addresses as `Where`, so tests cover both on one Pyo.
- **The CLI** shows "needs update" next to the state of such a server in `pyo sandbox servers ls`.
- **The door** reads an outbound server's code as gone, so a code for the channel never makes a door config.
- **The installation's servers** in the e2e: they're covered against Postgres. The e2e moves a team's server.

### Work

| Part | Days |
|---|---|
| Local signer: key table, sealed key, sequence per server | 0.5 |
| Channel for servers with no team, the migration | 0.75 |
| `addSandboxServer`, Hetzner and **Update** on self-hosted, the URL choice, the join's tailnet | 1 |
| Daemon and `sandbox.sh`: plain HTTP to a tailnet address | 0.25 |
| Dashboard: add flow, the notice, **Update**, Reset access on self-hosted | 0.5 |
| Tests: unit, throwaway Postgres, the e2e extended (KVM, in CI) | 1 |
| Docs (pyo-docs) | 0.5 |
| CI fixes | 0.5 |
| **Total** | **about 5 days** |

The staging test on throwaway servers comes after this and isn't counted.

## Removing the direct path (0.52.0)

Decided 2026-11-27: 0.52.0 removes the direct path. There was no 0.51.1 release, so 0.52.0 is also the first release with the outbound channel. Self-hosted servers that joined at Pyo's tailnet door take no commands after the update until an admin presses **Update** on each one and runs its command there.

### What's gone

- **Pyo's calls to daemons.** `daemon.rs` only queues commands on a server's channel. No more plain HTTP to `<tailnet IP>:7070`, and no more relay through a cluster's agent (`SandboxCall`). Agents answer a `SandboxCall` from an older Pyo with an error. The proto type stays one release, so an agent still parses a command an older Pyo queued.
- **The runner's report reads.** Every server posts its report over its channel. The runner sweeps channels, ends sandboxes past their lifetime and retries deletes.
- **The door's join.** `POST /sandbox/join` on the tailnet door (:5006) is gone. The door keeps the registry and Hetzner servers' bootstrap.
- **Pyo's `sandboxes` name as a proxy.** It passed web pages, the terminal, files and exec on to a door server's daemon. It now answers only the update notice, so nothing else of Pyo answers on that host. The name itself stays in tailscale.rs for now: removing it touches the Gateway and the Tailscale card, which is separate work.
- **Admin and create tokens.** Pyo signs no tokens for its own calls. The daemon refuses tokens with the admin scope.
- **The daemon's routes for Pyo:** `GET /v1/report`, `GET` and `POST /v1/sandboxes`, `POST /v1/commands`, `DELETE /v1/sandboxes/{id}`, `.../pause`, `.../resume` and `.../ttl`. Those come over the channel only.
- **The daemon's plain HTTP listener** on `<tailnet IP>:7070`. The daemon serves HTTPS on its tailnet name at :7443 only. `serve` stops on a config without `outbound` and says to press **Update**.
- **`sandbox.sh --pyo`** and its door join. A `--pyo` command, or an update run on a server from the door, stops with "This server joined Pyo the old way and needs an update. Press Update on it in Pyo, then run the command it shows here." Its health check is HTTPS on the tailnet name. ufw opens 7443 and closes 7070.

### What stays

- The browser and SDK API on the daemon, with client tokens: get, exec, files, the terminal, ports and web pages. They're what the dashboard and SDKs call on :7443.
- The one-time join codes (`sandbox_join_codes`, `join.rs`): the outbound join, **Update** and Hetzner servers use them.
- The sandbox token key: client tokens and registry tokens.
- `address`, `via_cluster` and `outbound` on `sandbox_servers`. The channel uses the first two. `outbound = false` is how Pyo knows a server joined the old way.
- The edge proxy on 80 and 443. It never went through Pyo.

### Servers from the door

- `listSandboxServers` shows `needsUpdate` (self-hosted). The dashboard shows "Needs update" as the server's state and "This server needs an update. Press Update to reconnect it." with **Update**.
- `listSandboxes` and `getSandbox` show `serverNeedsUpdate`, and `url` is null. The sandbox's page shows the same notice with **Update**. `getSandbox` has no `connection`, and `connectionNote` is the notice.
- Every command against such a server or its sandboxes answers 409 with "This server needs an update. Press Update to reconnect it." before anything is sent: create on it, delete, pause, resume, extend, ports, tokens, MCP exec and files. Placement skips it. When every server a team may use needs an update, a create says "Your sandbox servers need an update. Press Update on each one to reconnect it."
- The runner asks such a server nothing. Its sandboxes past their lifetime end in Pyo's records, and the daemon ends them on its own.
- **Update** (`resetSandboxServerAccess`) gives a command for the same server. At its join the row turns outbound and the server takes commands again. Its sandboxes keep running through it.

### Migration

None. No column or table is used only by the direct path, so nothing is dropped, and the release before keeps working against the schema during an update.

