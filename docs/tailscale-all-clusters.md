# Tailscale for all clusters

Design for 0.51.0. Status: proposed, for review before implementation.

## Goal

- Tailscale is set up once, per team, on **Integrations**. There's no other Tailscale setting.
- Every cluster of the team, existing and future, is on that tailnet.
- Pyo never stores a usable Tailscale credential, and Pyo Cloud least of all.
- Self-hosted and Pyo Cloud use the same flow.

## Decision: one holder cluster per team (option c)

The team's OAuth client lives in **one** cluster, the *holder*, as the Secret `pyo-system/pyo-tailscale` (as today). Every other cluster of the team keeps **no** client. Pyo sends the Tailscale work for any cluster of the team to the holder's agent, as the same signed commands as today: names (`tailscale_sync`), join keys (`tailscale_key`) and name checks (`tailscale_check_name`). The credential never leaves the holder, so nothing has to be sealed or moved.

This works today without agent changes. The agent's Tailscale code doesn't depend on the cluster it runs in. `sync()` only uses the client and the `TailnetSync` it is given (the subject cluster's mark, names, servers and peers). `auth_key()` and `check_name()` take nothing cluster-local either. Pyo already sends join keys across clusters (`key_cluster` falls back to the cluster Pyo runs on, and `sandbox_servers.via_cluster`).

### Why not (a) or (b)

| | (a) new client per cluster, sealed | (b) shared client, sealed | **(c) holder acts** |
|---|---|---|---|
| Credentials in clusters | one per cluster | one, copied to every cluster | **one, in one cluster** |
| Blast radius of a breached cluster | its client (can mint `tag:pyo` keys for the whole tailnet anyway) | the team's only client | **the holder only. Other clusters hold nothing.** |
| New Tailscale scope | `oauth_keys` on the holder: a client that mints clients. Revoking it doesn't revoke its children (1) | none | **none** |
| Revocation | N clients to find and delete | one, but in N places | **one client in one place** |
| Exfiltration path on Cloud | a "seal it to key K" command. A compromised cloud-api can swap K (see Sealing) | same | **none: no command ever exports a credential** |
| Holder gone | others keep working | others keep working | joining servers and name changes pause until a new token is pasted. Running servers, existing names and certificates keep working |
| Agent changes | new commands, key enrollment | same | **none needed** |

The one cost of (c) is the holder going away. That's management only. Servers stay on the tailnet, Services stay, and `tailscaled` renews certificates by itself. Recovery is one pasted token. Self-hosted, the holder of the installation's team is the cluster Pyo runs on, so if it's gone Pyo is gone too.

## Research answers

**Tailscale API** (Trust credentials (1), OAuth clients (2), Services (3)):

- Creating an OAuth client is `POST /api/v2/tailnet/{tailnet}/keys` with `"keyType": "client"` and needs the **`oauth_keys`** scope (1). Pyo's client has `devices:core`, `auth_keys` and `services` (`crates/pyo-agent/src/tailscale/setup.rs`, `SCOPES`). So **it can't create another OAuth client**. The docs don't say a client can't mint a child with wider scopes, and "revoking a trust credential does not revoke auth keys or child trust credentials that it created" (1). That rules (a) out.
- **Auth keys for any cluster: yes.** `auth_keys` covers `POST /api/v2/tailnet/{tailnet}/keys` for auth keys (1). Keys "must have those exact tags, or tags owned by the credential's tags" (1), and that's `tag:pyo`, which every Pyo server has.
- **Services for any cluster: yes.** Services are tailnet-wide: `PUT`/`DELETE /api/v2/tailnet/{tailnet}/services/{serviceName}`, and host approval is `POST .../services/{serviceName}/device/{deviceId}/approved`, all with `services`. The device endpoints also need `devices:core` (1). Hosts must be tagged devices (3), which Pyo's servers are. Tags are set with `POST /api/v2/device/{deviceId}/tags` (`devices:core`).
- Names are unique per tailnet (the path is `/services/{serviceName}`), so two clusters of a team can't both have `blog` any more.
- **Plan device count:** no API returns a tailnet's plan or allowance. Pricing says plans include 50 tagged resources, with more at $1/month (4). Pyo sees only `tag:pyo` devices (the client's tag).

**Cloud signer:** it signs `tailscale_*` kinds already. `update_agent` is root-key only (`ROOT_ONLY_KINDS`, pyo-proto `signing.rs`).

**Sealing:** agents have no encryption key today. The `signed` mode (Cloud) has a daily Ed25519 key enrolled at `/enroll-key` (`agent_auth.rs`, `pyo.agent_keys`), and the `mtls` mode has a client certificate key (`trust.rs`, rcgen). Both only sign. Self-hosted `token` mode has no key. If sealing is ever needed (see Later), the smallest sound choice is **HPKE (RFC 9180) base mode, DHKEM(X25519, HKDF-SHA256) with ChaCha20-Poly1305**, through `rustls::crypto::aws_lc_rs::hpke` (5). rustls 0.23.46 with `aws_lc_rs` is already a direct dependency of pyo-agent, so no new crate is needed. The agent would keep the private key in a pyo-system Secret and report the public key in its heartbeat.

The limit, and the main reason for (c): on Cloud the recipient's public key reaches the sender through cloud-api. Nothing the holder trusts (only `PYO_COMMAND_KEY`) can tell a real key from one a compromised cloud-api put in. The signer can't either, because it signs what cloud-api asks. Sealing protects a credential at rest in Pyo's database but not against an active cloud-api breach. With (c) there's nothing to redirect.

**Roles and refresh:** (c) adds **no agent role, no admission policy change and no new command kind**. The holder writes the same Secret as today. Other clusters do nothing new. So no cluster needs its connect command run again for this. The once-per-0.51.0 refresh stays the only one, and this feature costs **zero** extra refreshes on Cloud and self-hosted. A later sealed handover wouldn't need one either: the agent already writes Secrets in pyo-system (`rbac.rs`, write role), so it would only need a new agent image (`update_agent`, which Cloud signs).

## The four contexts

| | Self-hosted | Pyo Cloud |
|---|---|---|
| **Team** | Integrations › Tailscale per team. The holder is the cluster Pyo runs on for its team, else the team's oldest connected cluster | same, oldest connected cluster |
| **Global** | Pyo's own names (`pyo`, `registry`, `sandboxes`) live on the cluster Pyo runs on, so they follow that team's tailnet. A team with no Tailscale of its own uses that tailnet, as today (`key_cluster` fallback) | none. Pyo Cloud is never on a team's tailnet. Every team brings its own |
| Agent rules | self-update on | `PYO_AGENT_SELF_UPDATE=false`. Not affected: no new roles |
| Signer | none | signs `tailscale_*` already. No change |

## Data model

New migration (next free number after `0041_bundle_images.sql`):

```sql
-- A team's Tailscale (tailscale.rs). The OAuth client lives in one of its
-- clusters, the holder. Pyo keeps only what isn't secret.
CREATE TABLE pyo.team_tailscale (
  team_id           text PRIMARY KEY REFERENCES pyo.teams (id) ON DELETE CASCADE,
  holder_cluster_id text REFERENCES pyo.clusters (id) ON DELETE SET NULL,
  client_id         text NOT NULL,
  hint              text NOT NULL DEFAULT '',
  -- MagicDNS suffix, as last seen ("tail1234.ts.net").
  suffix            text,
  -- tag:pyo machines and Tailnet Lock waits, from the last sync (once per team now).
  machines          jsonb NOT NULL DEFAULT '[]',
  lock_waiting      jsonb NOT NULL DEFAULT '[]',
  -- Why Tailscale isn't working: 'holder_gone' or 'client_refused'. NULL when fine.
  problem           text,
  created_at        timestamptz NOT NULL DEFAULT now(),
  updated_at        timestamptz NOT NULL DEFAULT now()
);
-- Row security like the other team tables (0022_row_security.sql).

-- A command run by one cluster's agent for another (the holder for a team's cluster).
ALTER TABLE pyo.commands ADD COLUMN for_cluster text REFERENCES pyo.clusters (id) ON DELETE CASCADE;
```

`pyo.clusters.tailscale` stays, per cluster, for its sync state (names, `sent`, `forget`, `adopt`, `syncFailed`). Its `clientId` and `hint` stay only on a cluster that still keeps its own client on another tailnet. The column stays readable by 0.5.5 during the upgrade window. `pyo.cluster_tailnet_names` is unchanged. Marks stay per cluster (`marker(install, cluster)`), so existing Services keep their owners. Name checks also look at the other clusters of the same tailnet in the database, for names not made yet.

**Migrating existing setups** (runtime, in the Tailscale runner, like `hand_over`, idempotent):

1. Per team, group clusters with `tailscale` by suffix. The team's tailnet is the group with the cluster Pyo runs on, else the biggest (ties go to the oldest cluster).
2. Holder: the cluster Pyo runs on when it's in that group, else the group's oldest connected cluster whose client passes `tailscale_test`. Write `team_tailscale`.
3. For each other cluster of the group: once the holder's first sync for it succeeds, send `tailscale_remove {names: [], marks: []}` to its agent. It forgets its client and deletes nothing. Clear its `clientId`. Pyo lists the forgotten client IDs on the card as a todo: "Delete these OAuth clients in the Tailscale console".
4. Clusters on another suffix keep their client and show as **On another tailnet** (below). Nothing changes for them silently.
5. Clusters without Tailscale become members. Pyo-level: the holder syncs their names, and their join keys come from the holder.

A cluster offline during migration is handled on the next pass.

## Commands, signing and sealing

No new command kinds. Routing changes in `tailscale.rs`:

- `holder(team)`: the cluster whose agent acts. Always a cluster of the **same team**, enforced in one function with a test. Self-hosted, a team without Tailscale falls back to the installation team's holder.
- `tick` builds `sync_for(cluster)` for every member as today and enqueues it on the holder, with `for_cluster` set. "One sync in flight" is checked per `for_cluster`. `synced()` writes results to `for_cluster`, and `machines`/`lock_waiting` go to the team row. At most 5 Tailscale commands are in flight per holder (signer rate limit, big teams).
- `join_key`, `check_name`, `check_domain` and `host_report` ask the holder.
- `TailnetSync` gets an optional `cluster` field, for the agent's logs only. Older agents ignore it.

Signing is as today: each command is signed for the holder's cluster id (`Claims.cluster`). Secrets: the pasted API token and a hand-pasted client go to the holder only, sealed while queued and wiped after (`teambackup::wipe_command`), as today. Join keys come back as today (single-use, an hour, `tag:pyo`). **Nothing new is sealed**, because no credential moves between clusters.

## API, MCP and CLI

Ops change from `clusterId` to `teamId`. Taking a token, they stay **dashboard only** (`key_access_for` → None), so no API keys, no MCP tools and no CLI commands, as today.

- `planTailscaleSetup {teamId, token}` and `setUpTailscale {teamId, token, version}`: run on the holder (chosen by Pyo). A new setup on a new holder also deletes the old holder's client when the token can (it's an admin token).
- `saveTailscale {teamId, clientId, secret}`, `testTailscale {teamId}`
- `removeTailscale {teamId}`: disconnect for the whole team (below).
- `moveClusterToTailnet {clusterId}`: for a cluster on another tailnet. Returns a command per server.
- `renameTailscaleName` is unchanged.

Read: `getState` gets `teams[].tailscale` with `{suffix, holder, problem, devices, clusters: [{id, state}]}`. MCP `get_state` and `get_tailnet_lock` show it with no new tools. The CLI gets no Tailscale commands.

## Dashboard

**Integrations › Tailscale** (copy is final wording, minimal):

- Not set up, with clusters: today's token flow (Token, Review, Done). Summary line: "Every cluster of this team joins your tailnet."
- **No clusters yet:** the paste is held **in the open dashboard tab only**, never sent to or kept by Pyo. The dialog says "Connect a cluster. Tailscale is set up on it as soon as it connects." and links to the connect flow. When the first agent connects, the tab runs the review, and the admin confirms. A closed tab drops it. The token expires within a day anyway. *Why not the connect command:* the token would sit in the server's shell history and process list for up to a day, and the policy changes would skip review.
- Set up: rows "Tailnet" (suffix), "Kept on" (holder cluster), "Servers" ("12 Pyo servers"). At 40 or more: "Tailscale plans include 50 tagged devices." Then a cluster list with a badge each: "On your tailnet", "Not on it yet" (no server joined), "On another tailnet" with **Move**, "Waiting for {holder}". Then Names on your tailnet (self-hosted, unchanged), Test, Disconnect.
- Problems on the card: holder gone → "Set up again. The cluster that kept it is gone." Client refused → "Tailscale doesn't take the client. Set up again."
- **Disconnect** confirm, title "Disconnect Tailscale?", body (lines as they apply):
  - "Every cluster of this team leaves your tailnet in Pyo."
  - "New servers can't join."
  - "Tailnet domains stop working."
  - Self-hosted, the installation's team: "Pyo's tailnet address stops working. The public dashboard opens again."
  - "Servers stay on your tailnet. Delete the OAuth client {id} in the Tailscale console."

  Servers are never removed from the tailnet: multi-server clusters run k3s over `tailscale0`.
- **Move** (other tailnet), confirm: "Move {cluster} to your tailnet?" Body: "Its servers join {suffix}. Its tailnet domains move to {suffix}." Then one command per server, as with "First, run this on {server}". In 0.51.0 this is offered for single-server clusters only. Multi-server clusters show "Move its servers by hand. Steps in the docs." (Open question 2.)

**Cluster page:** no Tailscale setup there. One row in the cluster's networking settings: "Tailscale" with the badge above, linking to Integrations. `SetUpTailscaleLink` and `NeedsTailscale` link to the team card without a cluster. **Removing the holder cluster** warns: "This cluster keeps your team's Tailscale. After it's removed, set Tailscale up again."

## Failure handling

- **Holder offline:** background syncs wait. `tick` retries every pass (20 s), resends at least every 5 minutes, and signed commands expire after 10 minutes and are queued again. So it's retried until done. Asks that need an answer now (join key, name check) say "{holder} keeps your Tailscale and isn't connected."
- **Other clusters offline:** nothing waits on them. The holder makes their names anyway, and their servers advertise them when back. This answers the fan-out case: there is no fan-out of credentials.
- **Holder removed or gone:** `holder_cluster_id` becomes NULL and `problem = 'holder_gone'`. Running servers, names and certificates keep working. A new token makes a new holder (oldest connected cluster) and deletes the old client.
- **Client revoked in Tailscale:** syncs fail with 401, then `problem = 'client_refused'`. Tailnet domains show `SYNC_FAILED` as today.
- **Disconnect with the holder offline:** Pyo forgets anyway (as `remove()` does today). Names are retired, so servers stop advertising them. The card lists what's left to delete in the console.
- **Two admins at once:** one Tailscale op per team at a time (advisory lock on the team id).
- **0.5.5 still running during upgrade:** its per-cluster commands to member clusters fail with "Tailscale isn't set up on this cluster" until it stops. Nothing is deleted.

## Test plan

- **Unit:** holder choice (self-hosted, Cloud, ties, failed client), same-team routing (never another team's holder), migration grouping by suffix, `for_cluster` result routing, per-subject in-flight check, name clash across clusters of one tailnet, disconnect copy per context, copy lint (no semicolons, no dash-joined clauses).
- **Fake Tailscale API** (`PYO_TAILSCALE_API_URL`, existing e2e fake): 3 clusters in one team, one holder. Syncs for all three run on the holder only. Join keys for a non-holder cluster come from the holder. Holder offline, then back. Holder deleted, new token, old client deleted. Migration from 2 per-cluster clients on one tailnet plus 1 on another. Cloud mode with the staging signer (`pyo-cloud/scripts/e2e-multipass.sh`): every `tailscale_*` command is signed for the holder's id.
- **Real tailnet, once:** a **throwaway tailnet** and 2 throwaway VMs, never the user's tailnet or servers. Two clusters in one team, one token on Integrations. Check that cluster 2 needs no paste, a server joins cluster 2, a tailnet domain on cluster 2 goes live with a certificate, and the Tailscale console shows one OAuth client. Then disconnect and check the names are gone. Then delete the VMs and the tailnet.

## Implementation plan

| Phase | What | Size |
|---|---|---|
| 1 | Migration (`team_tailscale`, `commands.for_cluster`), `holder()`, routing of sync, keys and checks, `synced()` by `for_cluster`, runtime migration of existing setups | M, 2 to 3 days |
| 2 | Ops by team, `getState`, Integrations dialog (cluster list, problems, device count, held paste), cluster page row, holder removal warning, docs page | M, 2 to 3 days |
| 3 | Disconnect for the team, "On another tailnet" list, Move for single-server clusters with the domain suffix rewrite | S, 1 to 2 days |
| 4 | Fake-API e2e, Cloud signer e2e, real 2-VM tailnet test | S, 1 to 2 days |
| Later | Standby holder, sealed handover (HPKE), multi-server Move | only if wanted |

No agent image or role change is needed for phases 1 to 4.

## Open questions

1. **Standby holder.** When a token is pasted and a second cluster is connected, Pyo could set up a second client there from the same paste, so losing one cluster needs no new token. It's one more credential in one more place. Default: no.
2. **Multi-server clusters on another tailnet.** Moving them changes every server's tailnet address, and k3s runs over it. Is "move by hand, steps in the docs" fine for 0.51.0?
3. **Device count.** Tailscale's API doesn't say the plan's allowance. Is the "50 tagged devices" hint enough, or should admins enter their own allowance?
4. **Self-hosted teams without Tailscale** keep using the installation's tailnet, as today. Keep that, or require each team to connect its own?

## Sources

1. Tailscale, Trust credentials (scopes and endpoints): https://tailscale.com/docs/reference/trust-credentials (was kb/1623)
2. Tailscale, OAuth clients: https://tailscale.com/docs/features/oauth-clients
3. Tailscale, Services: https://tailscale.com/kb/1552/tailscale-services
4. Tailscale, Pricing (tagged resources): https://tailscale.com/pricing
5. RFC 9180, HPKE: https://www.rfc-editor.org/rfc/rfc9180, and rustls 0.23.46 `src/crypto/aws_lc_rs/hpke.rs` (`DH_KEM_X25519_HKDF_SHA256_CHACHA20_POLY1305`)
6. Code: `crates/pyo-api/src/tailscale.rs`, `crates/pyo-agent/src/tailscale/{mod,setup}.rs`, `crates/pyo-proto/src/{tailscale,signing,rbac}.rs`, `crates/pyo-api/src/{agent_auth,install,ops}.rs`, `pyo-cloud/crates/cloud-signer/src/main.rs`
