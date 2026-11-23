# Releasing Pyo

A version is one number for everything: `version` in the root `Cargo.toml`.

## Cut a release

1. Set `version` in `Cargo.toml` and run `cargo check` to update `Cargo.lock`.
2. Write what installations should know in `deploy/release-notices.json`
   (see [Notices](#notices)), with `"version"` set to the new version.
3. Check whether `agentAccess` changed since the last release: compare
   `cargo run -q -p pyo-agent -- access-hash` with `agentAccess` in the last
   release's `release.json`. When it changed, the release notes must say
   which clusters need the connect command under **Refresh this cluster**
   once, and that databases restart once when it runs (see
   [Notices](#notices)). For 0.51.0 that's every cluster. After it, a
   self-hosted cluster with the self-update role needs it only when the
   release adds a role object or changes a chart installed with the agent,
   and a cluster without that role (Pyo Cloud's) whenever it changed. The
   cluster Pyo itself runs on never needs it from its admin: its server
   runs the command by itself (see
   [The server Pyo runs on](#the-server-pyo-runs-on)). Say so in the notes,
   so an installation with only that cluster knows there's nothing to run.
4. Write the API reference for the docs (see [API reference](#api-reference)):
   `cargo run -p pyo-api --bin openapi`.
5. Commit that to `main`, and wait for CI to pass.
6. Tag the commit and push the tag:

   ```bash
   git tag v0.1.0
   git push origin v0.1.0
   ```

`.github/workflows/release.yml` then checks that the tag matches `Cargo.toml`,
runs the tests (the WAF's Go tests in `waf/` too), publishes signed images
to `ghcr.io/upsetdev/pyo/{pyo,agent,builder,flows,waf,geoip,db-controller,kv,postgres,mysql-operator,mysql-backup}`
(all from `deploy/images/`, at the same version. The Rust binaries are
compiled once, by the `binaries` job, and the Rust images' jobs take them
from that run's build cache),
and creates a GitHub release with the CLI, the install scripts and
`release.json`. The CLI is
`pyo-linux-x86_64`, `pyo-linux-aarch64`, `pyo-macos-aarch64`,
`pyo-macos-x86_64` and `pyo-windows-x86_64.exe`, each with its `.sha256`,
plus its installer `deploy/cli.sh`. Linux x86-64's comes from the Rust
binaries job. The others are built on macOS, Windows and Linux Arm runners
by `.github/workflows/cli.yml`, which you can also run by hand (Actions ›
CLI › Run workflow) to try those builds on a branch. That run publishes
nothing. It keeps the binaries as the run's artifacts.

The release also carries a sandbox server's files for each architecture,
`pyo-sandbox-linux-x86_64.tar.gz` and `pyo-sandbox-linux-aarch64.tar.gz`,
each with its `.sha256`, beside `deploy/sandbox.sh`, which installs them.
`.github/workflows/sandbox.yml` builds them (by hand too, like the CLI's):
the daemon and `pyo-envd` (static, musl), Firecracker and its jailer at the
release pinned in `crates/pyo-sandbox/scripts/firecracker.env`, and the guest
kernel from `crates/pyo-sandbox/scripts/guest-kernel.sh` (Linux 6.1 with
Firecracker's microVM config, cached until either file changes).
`crates/pyo-sandbox/scripts/bundle.sh` packs them, locally as well:

```bash
crates/pyo-sandbox/scripts/guest-kernel.sh out    # on the architecture it's for
crates/pyo-sandbox/scripts/bundle.sh --arch x86_64 \
  --sandbox target/x86_64-unknown-linux-musl/release/pyo-sandbox \
  --envd target/x86_64-unknown-linux-musl/release/pyo-envd \
  --kernel out/vmlinux --out dist
```

`sandbox.sh --bundle dist/pyo-sandbox-linux-x86_64.tar.gz` installs such a
build, with `--bundle-sha256 <its sum>` or, unchecked, `--dev`.

Installations and sandbox servers pull every image without logging in,
so the release checks that each can be pulled anonymously. The first
time a new image is published, ghcr.io may make its package private:
make it public in the package's settings, then run the failed job again.

Pyo Sandbox's built-in templates `base` and `desktop` aren't built here.
They come from [upsetdev/pyo-sandbox-images](https://github.com/upsetdev/pyo-sandbox-images),
whose signed `images.json` Pyo reads itself every hour. Pyo ships the
`images.json` of when it was built, in
`crates/pyo-api/assets/sandbox-images/` with its signature (a test checks
it), for installs that can't reach GitHub. `release.json` names those
images as `images.sandboxBase` and `images.sandboxDesktop`, as Pyo 0.4
reads them there. Before a release, take the latest:

```sh
cd crates/pyo-api/assets/sandbox-images
curl -fsSLO https://github.com/upsetdev/pyo-sandbox-images/releases/latest/download/images.json
curl -fsSLO https://github.com/upsetdev/pyo-sandbox-images/releases/latest/download/images.json.sigstore.json
cargo test -p pyo-api sandbox::images
```

`release.json` is signed with
cosign keyless as `release.json.sigstore.json` (the job needs `id-token:
write`, and `.github/release-signing-config.json` pins Sigstore's public
instance with Rekor v1).

The release's text lists the changes since the previous release (see
[Release notes](#release-notes)).

Each image is scanned with Trivy before it's pushed or signed. A CRITICAL
CVE that has a fix stops the release. HIGH ones are listed in the job's
summary and in code scanning. A CVE checked and found not to apply goes in
`.trivyignore`, with the reason.

Don't move or reuse a tag. If a release is bad, publish the next version.

The repo has GitHub's immutable releases on: once a release is published,
its assets and its tag can't change. So the workflow uploads the release
as a draft with every asset, checks the install scripts in it, and only
then publishes it, as a stable release marked latest (installations only
see GitHub's "latest" release). If the job fails before that, run it again:
it replaces the draft's assets. A release that's published already is left
as it is. The release's text can still be edited.

Then the `site` job rebuilds pyo.dev (see
[Install scripts on pyo.dev](#install-scripts-on-pyodev)). If it fails, run
that job again.

## Security patch release

When a fix can't wait for the next release, cut a patch release of the
latest one. Only the latest release gets fixes (see `SECURITY.md`).

What asks for one:

- the daily **Security scan** (`security-scan.yml`): an issue labelled
  `security` for each image, Pyo's or a third-party database image, with
  HIGH or CRITICAL CVEs that have a fix
- the weekly **Postgres rebuild check** (`postgres-rebuild.yml`): an issue
  when a fresh build of Pyo's Postgres images would have fewer of them
- Renovate's security pull requests, which it opens right away
- CI's **Lockfile CVEs** and **cargo-deny** checks

1. Fix it on `dev`: merge Renovate's pull request, or bump the dependency
   or the image's digest. Name the commit `fix(security): ...` or give it
   a `Security: GHSA-...` footer, so the release notes list it under
   Security Fixes. A CVE that doesn't apply goes in `.trivyignore`
   (images and lockfiles) or in `deny.toml` and `.cargo/audit.toml` (Rust
   advisories), with the reason.
2. Make the branch from the last release tag:

   ```bash
   scripts/patch-release.sh <commit>...           # a dry run: says what it would do
   scripts/patch-release.sh --apply <commit>...   # does it, locally
   ```

   It branches `release/vX.Y.Z` from the tag, cherry-picks the commits
   and bumps the version as a release commit does (`Cargo.toml`, each
   crate's `pyo-proto` dependency, `Cargo.lock` and
   `deploy/release-notices.json`). Keep the notices' `minFrom`, and clear
   `beforeYouUpdate` and `manualOnly` unless the patch needs them. Without
   commits it's a rebuild of the tag's code, which picks up what the
   Dockerfiles install as it is today. That's enough when the weekly check
   says so and the base image's digest didn't change.
3. Push the branch, open a pull request into `main` and wait for CI. The
   release checks the template catalog against pyo-templates' main, so if
   that check fails, cherry-pick the catalog's update too.
4. Merge it, then tag the merge `vX.Y.Z` (`v0.5.1` after `v0.5.0`) and push
   the tag. The release workflow scans, signs and publishes it like any
   release.

How it reaches installations:

- **Self-hosted:** an installation looks for a new release every 6 hours.
  Its admins see it in the update notice in the sidebar and under
  **Settings › Updates**, and apply it there. With automatic updates on,
  it's applied in the maintenance window. `install.sh` updates too. Pyo
  updates first, then every cluster's agent moves to the new agent image.
  A cluster with the self-update role then brings the rest up to date by
  itself within a minute, its roles included, and its databases restart
  then. 0.51.0 is the last release where every cluster needs the command
  under **Refresh this cluster** in the cluster's settings once. After it,
  only a release that adds a role object or changes a chart installed with
  the agent does. On the cluster Pyo runs on, the server runs that command
  by itself within a few minutes, 0.51.0 included. So an installation with
  only that cluster has nothing to run after any release.
- **Pyo Cloud:** whoever runs it updates the shared Pyo. Each team's agents
  then follow its version: within a minute for teams with automatic agent
  updates (the default), or when the team updates a cluster from its page.
  A team's databases don't move with either. They restart once the team
  runs the command under **Refresh this cluster** in the cluster's settings,
  which says "Restarts your databases once" while that's so. Say in the
  release's announcement to Cloud teams that this command restarts their
  databases once. From 0.51.0 that command also sets up automatic updates
  on the cluster's first server. In 0.51.0 they apply nothing yet, so the
  command is still needed after each release (see
  [Automatic updates on Pyo Cloud](#automatic-updates-on-pyo-cloud)).

How databases pick up new images: the databases system on a cluster
updates with Pyo, at the cluster's next heartbeat once the cluster's
install (its admission policy, which pins the images) is the new Pyo's.
The agent's own image doesn't matter. An agent with the self-update role
brings the install up to date by itself within a minute of Pyo's update,
its roles included (see [Notices](#notices) for what it can't bring).
0.51.0 is the exception: 0.5.5's self-update role can't write roles, so
every cluster stays outdated until the connect command under **Refresh this
cluster** runs once. The server Pyo runs on runs it by itself. Clusters
without that role (Pyo Cloud's, or `PYO_AGENT_SELF_UPDATE=false`) always
wait for the command.
pyo-db-controller then restarts with the new images and rolls each database
onto them. A Postgres or MySQL database with replicas restarts a replica
first and switches over to it, so the primary stops only for the
switchover. A single instance restarts, which is a short outage. New
third-party database images (MySQL, valkey-bundle, the Postgres bases) come
the same way, since the release's code pins them.

Every release restarts Postgres and KV databases once: their images are
built with the release and tagged with its version, so they're never the
ones before. MySQL databases restart when the release pins another MySQL
image. They restart when the cluster's install becomes the new Pyo's:
within a minute of Pyo's update on a self-hosted cluster with the
self-update role. On every other cluster they restart when the command
under **Refresh this cluster** runs, which on the cluster Pyo runs on is
within a few minutes of the update. Pyo says so by itself, nothing to write in the notices: the update
dialog and the cluster's refresh command show "Restarts your databases
once" when a database runs an image the release changes (compared from
`release.json`'s images and `databasePins`), and each database's restart
is written to its team's activity ("Restarting the database main for an
update").

## Release notes

`scripts/release-notes.ts` writes each release's text from the Conventional
Commits since the previous release, and the release workflow publishes it.
Try it before tagging:

```bash
bun run scripts/release-notes.ts --to v0.2.2             # from the version tag before it
bun run scripts/release-notes.ts --from v0.2.1 --to v0.2.2 --offline
```

- The changes are the commits the tag reaches and no earlier version tag
  does. Merge commits are left out, the commits they bring in are listed.
  A commit whose subject was already released, or that appears twice (a
  cherry-pick), is listed once.
- Sections, in this order and only when they have something: Features
  (`feat`), Bug Fixes (`fix`), Security Fixes, Performance (`perf`), Docs
  (`docs`) and Other (`refactor`, `build`, `revert`). `chore`, `ci`, `test`
  and `style` aren't listed.
- **Security fixes:** a commit with the scope `security`, or with a
  `Security:` footer line. Put the advisory's id in the footer
  (`Security: GHSA-xxxx-xxxx-xxxx`) and the release links the advisory.
  The word "security" in a message isn't enough.
- Each change links its commit, or its pull request when the subject ends
  with `(#123)`. An author who isn't a maintainer gets "(Thanks @login)".
  The maintainers are `MAINTAINERS` in the script (`--maintainers a,b`
  replaces them for one run).
- The notices in `deploy/release-notices.json` go first, under "Before you
  update".

To rewrite a published release's text:

```bash
bun run scripts/release-notes.ts --to v0.2.2 > notes.md
gh release edit v0.2.2 -R upsetdev/pyo --notes-file notes.md
```

## API reference

The docs' API reference is an OpenAPI 3.1 file made from the code: the
`#[utoipa::path]` attribute on each public handler, the request and response
types, and the operations of `/api/v1/ops` (the `Op` enum in
`crates/pyo-api/src/ops.rs`: each variant's doc comment is its description,
`key_access_for` its API key access, `op_doc` its sidebar folder). Write it
with:

```bash
cargo run -p pyo-api --bin openapi            # to ../pyo-docs/openapi/pyo.json
cargo run -p pyo-api --bin openapi -- -       # or print it
```

Then run `bun run sync-docs` in pyo-web. When `pyo-docs` sits beside this
repository, `cargo test` fails while its copy is out of date
(`openapi::tests::docs_copy_is_current`), so a change to the API doesn't go
out without its reference. Only what an API key can call is in it.

## release.json: what installations update to

`scripts/release-manifest.ts` writes it in the release workflow, and the
release carries it as an asset. Installations find it in **Settings › Updates**
(they ask GitHub every 6 hours) and apply it from there. It holds:

- `version`, and Pyo's images pinned by the digests the workflow just
  signed (`ghcr.io/upsetdev/pyo/pyo:0.1.0@sha256:…`). Each one is a field
  of `ReleaseImages` (`crates/pyo-proto/src/lib.rs`), and the release's
  manifests may only use those of Pyo's images: an image added to the
  script is added there too (`cargo test` compares them), and the workflow
  checks `release.json` with the release's own agent
  (`pyo-agent check-release`) before signing it
- `manifests`: `deploy/k8s/platform.yaml` and `deploy/k8s/pyo.yaml` as JSON
  objects with those images
- `migrations`: the migration files, so an installation can say how many it
  hasn't run yet
- the notices from `deploy/release-notices.json`
- `agentAccess`: a hash of the roles and the admission policy the release's
  agent is installed with (see [Notices](#notices))
- `databasePins`: the database images the release's code pins itself
  (`pyo-agent database-pins`: MySQL's and Valkey's with its modules, which
  `check-release` verifies). With `images.postgres16` to `postgres18` and
  `images.kv` they're what the release's databases run. The workflow writes
  it, nothing to set by hand
- `k3s`: the k3s a fresh install gets, from `deploy/k3s.env` (see
  [Bump k3s](#bump-k3s)).
- `sandbox`: the SHA-256 of each architecture's sandbox server files.
  `deploy/sandbox.sh` checks release.json's signature with cosign, as
  `install.sh` does, then the files against these.
- `scripts`: the SHA-256 of each install script the release publishes
  (`install.sh`, `cli.sh`, `connect.sh`, `join.sh`, `sandbox.sh`). The
  script writes them to `--scripts <dir>`, with this version on the
  `pyo:release-version` line of `connect.sh`, `join.sh` and `sandbox.sh`,
  and the release uploads them from there. pyo.dev serves only scripts
  that match these sums. The release notes list them too.

To try it before tagging:

```bash
bun run scripts/release-manifest.ts --version 0.1.0 --images ghcr.io/upsetdev/pyo \
  --pyo sha256:<64 hex> --agent sha256:<64 hex> --builder sha256:<64 hex> --flows sha256:<64 hex> \
  --waf sha256:<64 hex> --geoip sha256:<64 hex> --db-controller sha256:<64 hex> --kv sha256:<64 hex> \
  --mysql-operator sha256:<64 hex> --mysql-backup sha256:<64 hex> \
  --postgres-16 sha256:<64 hex> --postgres-17 sha256:<64 hex> --postgres-18 sha256:<64 hex> \
  --agent-access "$(cargo run -q -p pyo-agent -- access-hash)" \
  --database-pins "$(cargo run -q -p pyo-agent -- database-pins)" \
  --sandbox-x86_64 <64 hex> --sandbox-aarch64 <64 hex> \
  --sandbox-gvisor-x86_64 <64 hex> --sandbox-gvisor-aarch64 <64 hex> \
  --scripts /tmp/release-scripts
```

### Notices

`deploy/release-notices.json` is read only when its `"version"` is the
release's (old notices never carry over):

```json
{
  "version": "0.1.1",
  "minFrom": "0.1.0",
  "manualOnly": true,
  "beforeYouUpdate": ["Postgres moves to 19: the database restarts once and is down for about a minute."]
}
```

- `beforeYouUpdate`: shown at the top of the update dialog. Use it for
  anything an admin has to know or do first: breaking changes, manual steps,
  a restart of Postgres or ClickHouse (changing their image in
  `platform.yaml` restarts them, which means a short outage).
- `manualOnly`: automatic updates won't apply it, so an admin has to.
  Set it whenever there's something in `beforeYouUpdate`.
- `minFrom`: the oldest version that can update straight to this one, when an
  intermediate release has to run first (e.g. a migration in two steps).
  Older installations are told to update to `minFrom` first.

A change to the agent's roles or to its admission policy's rules needs no
notice for the update dialog: `release.json` carries the release agent's `agentAccess`
(`pyo-agent access-hash`, which `check-release` verifies), and an
installation whose agent has other ones and whose clusters don't have the
self-update role (Pyo Cloud, or `PYO_AGENT_SELF_UPDATE=false`) adds "After
updating, run the connect command shown in each cluster's settings." to the
update dialog by itself. The images in the policy don't count. A
self-hosted installation with the self-update role gets no such line from
0.51.0 on, since its agents bring the roles themselves. So when a release
adds a role object or changes a chart installed with the agent (the two
things a refresh can't bring, see below), write that line yourself as a
`beforeYouUpdate` notice. An installation whose only cluster is the one
Pyo runs on doesn't show that exact line: its server runs the command by
itself. The release notes never get the line by
themselves, so write it there when it applies (step 3 of
[Cut a release](#cut-a-release)): as a `beforeYouUpdate` notice, which the
release notes put first, or in the release's text (see
[Release notes](#release-notes)).

From 0.51.0 on, an agent with the self-update role writes its own roles.
Its refresh reads each role and binding of the install and updates the ones
that differ, then the admission policy, then the image pins. So a release
that changes rules in roles a cluster already has needs no command there.
0.51.0 itself still does, once: 0.5.5's self-update role may only read the
roles, so every cluster stays outdated until its connect command runs.

What a refresh can't do, and still needs the connect command for:

- **A new role object.** The agent updates roles and bindings by name and
  never creates one (Kubernetes can't limit a create to a name). Put a new
  permission into a role the install already has. A cluster-wide role with
  `resourceNames` covers a new namespace too.
- **A chart installed with the agent** (cert-manager, Envoy Gateway). The
  agent only reads them. Envoy Gateway's CRDs come from `connect.sh` anyway.
- **A binding to another role.** Kubernetes never changes a binding's role.
- **A cluster without the self-update role** (Pyo Cloud's, one installed
  with `PYO_AGENT_SELF_UPDATE=false`, one whose owner deleted the
  `pyo-agent-self-update` ClusterRoleBinding) **or read only.** Pyo sends it
  no refresh.

When a refresh can't bring a role, it still brings the rest: the admission
policy (it only narrows what the roles allow, so the new policy is safe
with the roles as they are), the image pins, and with them the databases
system, unless the roles left behind are the bundles' own
(`pyo-agent-bundles`, `pyo-agent-bundle-write`) or a chart. The install's
stamp then names the roles left behind, the cluster's settings show
"Update needed" with "This cluster's agent needs an update for some
features.", and only the features that need those roles wait for the
command (Cloudflare Tunnel is one). This is also what 0.51.0's agent does
on a 0.5.5 install before its owner runs the command. A feature that
needs a new permission must check for it and point to **Refresh this
cluster**, as `install_ready` in
`crates/pyo-api/src/cloudflare/tunnel_ops.rs` does.

The self-update role holds `update`, `escalate` and `bind` on the agent's
own roles, by name. With it, whoever controls Pyo can give the agent any
permission on the cluster, up to cluster-admin. That was already so while
the role only rewrote the admission policy: the agent takes any policy that
fails closed (`check` in `crates/pyo-agent/src/install.rs` can't pin a later
release's rules), and without the policy's limits the agent's roles run
privileged pods in `pyo-system` and write any rule into the bundles'
ClusterRoles (`cluster_rules` in `crates/pyo-proto/src/bundles.rs`). The
role now says so directly. Pyo Cloud installs without it. Don't widen it:
`self_update_is_narrow` in `crates/pyo-proto/src/rbac.rs` pins its exact
shape.

The roles are partly
built from the cluster-wide object names in the system bundles
(`cluster_rules` in `crates/pyo-proto/src/bundles.rs`), so a release that
adds or renames such an object changes rules in a role the install
already has, which a refresh brings. A new Service,
network policy or ConfigMap of the gateway in `pyo-gateway` changes no role:
the role there names none of them, and the admission policy
(`gatewayAllowed` in `crates/pyo-proto/src/rbac.rs`) holds each to Pyo's own
by name and content. Add the new object's clause there. A cluster on
Kubernetes older than 1.30 has no admission policy, so nothing but the
agent's own code keeps it to those names in that namespace. A cluster
without the self-update role or read only gets new roles and a new policy
only from its connect command. Until then, the agent writes the gateway's client policies
in the shape the cluster's older policy accepts, and the cluster's settings
say to run the command.

### The server Pyo runs on

The cluster Pyo itself runs on needs no command from its admin. Most
self-hosted installations have only this cluster.

- The server's `pyo-tailnet` timer runs as root every minute
  (`host_units` in `deploy/install.sh`). It first brings the running Pyo's
  host scripts (`/install/host-units.sh`, when its checksum changes), then
  runs `connect-this-machine.sh auto`.
- That asks `GET /install/this-machine` (with the bootstrap key). The
  answer has `"refresh": true` while the cluster's install is "outdated"
  or "roles", this installation has the self-update role
  (`PYO_AGENT_SELF_UPDATE` isn't off), the cluster's own self-update
  binding is there and the cluster isn't read only (`host_refreshes` in
  `crates/pyo-api/src/install.rs`).
- The server then runs `connect.sh` from Pyo on `127.0.0.1:8000`, the same
  command as under **Refresh this cluster**. It brings new role objects,
  charts and host settings, and may restart k3s once.
- It runs once per Pyo version and install (`/etc/pyo/refresh-tried`). One
  that failed (`/etc/pyo/refresh-failed`) is tried again after an hour. No
  more than 4 run in a day (`/etc/pyo/refresh-runs`). A run by hand and the
  installer take turns with it (`/run/pyo-connect.lock`).
- Meanwhile the cluster's settings show "Updating". They show "Update
  needed" with the command once the run failed, or when the server said
  nothing for 10 minutes.
- `install.sh` runs the same refresh at the end of an update.
- Its log: `journalctl -u pyo-tailnet`.

From 0.5.5 this needs no command either: 0.5.5's timer already brings the
new host scripts within a minute of the update, and the next run refreshes
the cluster. 0.5.5's own update dialog still says "After updating, run the
connect command shown in each cluster's settings." for 0.51.0, since it
writes that line itself. Say in 0.51.0's release notes that the server Pyo
runs on needs nothing, and every other cluster needs the command once.

A server set up with Pyo 0.4.1 or earlier, and never by `install.sh` since,
has no such timer script. Its cluster shows "Updating" for 10 minutes,
then "Update needed" with the command. Running the command, or
`install.sh` again, brings the timer script.

Pyo Cloud's clusters are never refreshed this way: Pyo Cloud runs without
the self-update role, and a team's servers have no such timer. They have
a verifier of their own instead.

### Automatic updates on Pyo Cloud

Pyo Cloud must never be able to push roles, an admission policy or image
pins to a team's cluster. So a cluster there gets them only from something
the team runs as root on its own server: the connect command, or the
verifier that command sets up.

- On a Pyo without the self-update role (`PYO_AGENT_SELF_UPDATE=false`),
  the connect command and the command under **Refresh this cluster** carry
  `--auto-update` (`auto_update_flag` in `crates/pyo-api/src/join.rs`). The
  dashboard shows a checkbox with them, "Keep this cluster up to date
  automatically", ticked. Unticked, the command shown carries
  `--no-auto-update`. Pyo keeps no setting for it: the command the team
  runs decides. A self-hosted Pyo with the self-update role shows no
  checkbox and adds no flag.
- With `--auto-update`, `connect.sh` installs the verifier (`pyo-verify`)
  on the cluster's first server, run as root by a systemd timer.
- The verifier asks `GET /install/version` on Pyo's public address. The
  answer is `{"version":"X.Y.Z","install":"<hash>"}`: the version this Pyo
  runs and the hash of the install a cluster without the self-update role
  gets (`verifier_install` in `crates/pyo-api/src/install.rs`). It needs no
  key and may be cached for a minute. Naming a version is all Pyo does.
- The verifier checks that version's signed release on GitHub and takes
  only what that release defines. It never moves to an earlier version.
- It writes its status to the ConfigMap `pyo-system/pyo-verifier`
  (`version`, `live`, `checked`, `error`, `hold`, `host`). The agent may
  read that ConfigMap by name and the admission policy refuses it any
  write. The agent passes the status on with its heartbeat.
- The team's switches are on their side only: `updates: "hold"` in the
  ConfigMap `pyo-system/pyo-policy`, and `/etc/pyo/verify.conf` on the
  server. Pyo has no switch that turns the verifier on or off or makes it
  run. Don't add one.

**What 0.51.0 ships is the bootstrap only.** Its verifier checks the
signed release, never moves backwards, replaces itself with that release's
verifier and writes its status. It applies nothing to the cluster. So
after 0.51.0 and after the release that follows it, a Pyo Cloud cluster
still shows "Update needed" and still needs the command under **Refresh
this cluster**, as before. Applying a release's roles, admission policy
and image pins is 0.52.0's work. Don't tell Cloud teams their clusters
update by themselves before that ships.

What the cluster's settings show under **Refresh this cluster**, from the
status (`auto_update` in `crates/pyo-api/src/install.rs`):

- "Automatic updates are set up on this cluster (version 0.51.0)." With
  "This update still needs the command." while the install is outdated.
- "Automatic updates are on hold. They were turned off on the server." Or
  "…in this cluster's policy." for `updates: "hold"`.
- "Automatic updates reported a problem: …" with the verifier's own words,
  and "Run the command to update this cluster." while the install is
  outdated.
- Nothing when there's no verifier, or its last check is more than 48
  hours old.

The status comes through the agent, so an agent that was broken into can
make it say anything. It only adds `autoUpdate` to what the settings show.
The install's state, the "Update needed" badge, the command and every
feature that waits for a refresh follow from the install's own stamp, never
from the status (`a_lying_verifier_status_only_changes_what_is_shown`).
Keep it that way: never read `AgentScope::verifier` to decide anything.

This release changes the agent's roles once more: the role
`pyo-agent-policy` also reads `pyo-system/pyo-verifier`. It's a rule in a
role every install has, so a self-hosted cluster gets it with a refresh. A
Pyo Cloud cluster gets it with the command, and until then its agent says
once in its log that it can't read the status.

### How an installation updates

On the cluster Pyo runs on, the agent runs the new release's upgrader: a Job
in `pyo-system` from the new agent image, with a ServiceAccount that can only
change `pyo-system`. It does what `scripts/pyo-up.sh --release` does: makes the
secrets a newer release needs, applies `platform.yaml` then `pyo.yaml`
(server-side, field manager `pyo-install`), and waits
for each rollout. If something doesn't become ready, it puts back the pod
templates it found. Then every cluster's agent moves to the new agent image,
after checking it can pull and start it. Builders on clusters follow the new
builder image after their current build.

Images are checked against the digests in `release.json` (Kubernetes pulls
them by digest). `release.json` itself is used only once its signature
checks out: the dashboard's updates check the bundle in Rust
(`crates/pyo-api/src/sigstore.rs`, with Fulcio's and Rekor's keys from
Sigstore's trusted root in `crates/pyo-api/assets/sigstore/`, and Fulcio's
certificates are valid to 2031), `install.sh` with cosign (the machine's, or
a pinned download checked by SHA-256). The images' own signatures aren't
verified in the cluster. `pyo-up.sh --release` checks them.

## Install scripts on pyo.dev

pyo.dev serves the scripts each release publishes, as they are:

- `https://pyo.dev/<script>`: the latest release's (`install.sh`,
  `cli.sh`, `connect.sh`, `join.sh`, `sandbox.sh`), never cached for long.
- `https://pyo.dev/v/<version>/<script>`: each release's, which never
  change. Pyo's commands for other machines use these when they fetch
  scripts from pyo.dev (`crates/pyo-api/src/join.rs`).

pyo-web's build (`scripts/release-scripts/` there) takes them from the
GitHub releases. For each release, it checks `release.json`'s signature
(this workflow at that tag, in this repository), then each script against
its SHA-256 in `release.json`, and fails on any mismatch. Nothing in the
pyo-web repo can replace them. Releases up to 0.4.2 have no sums in their
release.json: pyo-web holds a reviewed list of theirs.

The release workflow's `site` job starts that build through a Workers Builds
deploy hook, so pyo.dev serves a new release within minutes. The hook's
URL is this repo's secret `PYO_WEB_DEPLOY_HOOK`.

Pyo 0.4.2 and earlier give the latest script `--version <their version>`.
The scripts' `run_release` (`pyo:release` blocks) then runs that release's
own script from `pyo.dev/v/<version>/`.

## Install or update a release

The release workflow attaches `deploy/install.sh` as `install.sh`, and
`https://pyo.dev/install.sh` is the one from the latest stable release:

```bash
curl -fsSL https://pyo.dev/install.sh | bash                          # install, or update to the latest
curl -fsSL https://pyo.dev/install.sh | bash -s -- --version 0.1.0    # a particular release
```

It installs k3s if it's missing, reads the release's `release.json`, and runs
its upgrader as a Job, the same way updates from the dashboard do: images
pinned by digest, `--server-side --field-manager=pyo-install`, rollouts waited
for and rolled back on failure. On an update it honours `minFrom`, shows
`beforeYouUpdate` and asks first (`--yes` skips the question, and without a
terminal it refuses until you pass it). It never goes back a version and never
removes data. `PYO_UPDATE_SOURCE` points it at a mirror, as for Pyo itself.

From a repo copy, `scripts/pyo-up.sh --release 0.1.0` checks the signature of
the release's `release.json` and of every image in it with `cosign`, and then
runs `deploy/install.sh`. Without cosign it stops, unless you add
`--insecure-skip-verify` (`install.sh` still checks `release.json`).

When cosign or Sigstore's keys change, update `COSIGN_VERSION` and
`COSIGN_SHA256_AMD64` in `deploy/install.sh` (from the cosign release's
checksums) and the files in `crates/pyo-api/assets/sigstore/` (from
Sigstore's `trusted_root.json`).

`install.sh` reads `release.json` with `sed`, so keep
`scripts/release-manifest.ts` writing it pretty-printed with two spaces.

## Bump k3s

Each release installs one k3s version, the one it was tested with:
`deploy/k3s.env`. It changes only when a release changes it on purpose.

1. Pick the k3s release and get its checksums:

   ```bash
   V=v1.37.2+k3s1; T=${V//+/%2B}
   curl -fsSL "https://raw.githubusercontent.com/k3s-io/k3s/$T/install.sh" | sha256sum   # PYO_K3S_INSTALLER_SHA256
   curl -fsSL "https://github.com/k3s-io/k3s/releases/download/$T/sha256sum-amd64.txt" | grep ' k3s$'        # PYO_K3S_SHA256_AMD64
   curl -fsSL "https://github.com/k3s-io/k3s/releases/download/$T/sha256sum-arm64.txt" | grep ' k3s-arm64$'  # PYO_K3S_SHA256_ARM64
   ```

2. Write them in `deploy/k3s.env`, and the same four lines in the
   `pyo:k3s-pin` blocks of `crates/pyo-api/assets/connect.sh`,
   `crates/pyo-api/assets/join.sh` and `scripts/join-node.sh`.
   `cargo test -p pyo-api --lib join::` fails while a copy differs.
   `release-manifest.ts` puts it in `release.json`, where `install.sh`
   reads it.
3. Update the version in the docs (`system-reference.mdx`, section k3s).
4. Test it: a fresh install on a throwaway server (`install.sh`), a
   cluster connected with `connect.sh`, a server joined with `join.sh`,
   and Pyo's own checks (databases, builds, the gateway) on it. Note it in
   `deploy/release-notices.json` when it matters to existing installations
   (they keep their k3s).

The scripts check k3s's installer against `PYO_K3S_INSTALLER_SHA256`, and
the binary against the pinned checksum (or, for a joining server on another
version, against that k3s release's own checksum list), and stop before
installing anything when one doesn't match.

## Change the database

Migrations run while the previous release is still serving, and it has to keep
working on the new schema. That's what makes updates seamless and rollbacks
possible. CI enforces this with `scripts/check-migrations.ts`.

Adding is always fine: new tables, nullable columns, columns with a default.

To remove or rename something, split it across releases:

1. **First release:** add the new thing, and write to both. Read the new one, falling back to the old.
2. **Next release:** stop using the old thing completely.
3. **A later release:** remove it, naming the release from step 2 in the migration:

   ```sql
   -- pyo:contract since=v0.1.1 nothing reads users.nickname since v0.1.1
   ALTER TABLE pyo.users DROP COLUMN nickname;
   ```

CI rejects a contract whose tag doesn't exist yet, because that release has to
have shipped first.

A migration that an installation already ran can't get that line, because
sqlx checksums the file. Its contract goes in `scripts/migration-contracts.json`
instead, keyed by the file name, with the same `since` and reason and the
file's SHA-256. It covers only that exact file. Use the line in the migration
whenever you can.

Never edit a migration that has been merged. sqlx checksums applied migrations,
and Pyo refuses to start if one has changed.

A new migration is the next number in `crates/pyo-api/migrations/`, e.g.
`0002_what_it_does.sql`. `0001_init.sql` is the initial schema. Until the
first release is tagged, check-migrations.ts still lets the migrations be
edited and merged. From v0.1.0 on, they're frozen.
