# Releasing Pyo

A version is one number for everything: `version` in the root `Cargo.toml`.

## Cut a release

1. Set `version` in `Cargo.toml` and run `cargo check` to update `Cargo.lock`.
2. Write what installations should know in `deploy/release-notices.json`
   (see [Notices](#notices)), with `"version"` set to the new version.
3. Write the API reference for the docs (see [API reference](#api-reference)):
   `cargo run -p pyo-api --bin openapi`.
4. Commit that to `main`, and wait for CI to pass.
5. Tag the commit and push the tag:

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
- **Pyo Cloud:** whoever runs it updates the shared Pyo. Each team's agents
  then follow its version: within a minute for teams with automatic agent
  updates (the default), or when the team updates a cluster from its page.

How databases pick up new images: the databases system on a cluster
updates with Pyo, at the cluster's next heartbeat once its agent is the new
one. pyo-db-controller restarts with the new images and rolls each database
onto them. A Postgres or MySQL database with replicas restarts a replica
first and switches over to it, so the primary stops only for the
switchover. A single instance restarts, which is a short outage. New
third-party database images (MySQL, valkey-bundle, the Postgres bases) come
the same way, since the release's code pins them.

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
- `agentAccess`: a hash of the roles the release's agent is installed with
  (see [Notices](#notices))
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

A change to the agent's roles needs no notice: `release.json` carries the
release agent's `agentAccess` (`pyo-agent access-hash`, which
`check-release` verifies), and an installation whose agent has other roles
adds "After updating, run the connect command shown in each cluster's
settings." to the update dialog by itself. An agent brings its admission
policy and image pins up to date on its own (with the self-update role), but
never its roles.

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
