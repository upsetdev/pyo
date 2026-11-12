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
and creates a GitHub release with the CLI and `release.json`, signed with
cosign keyless as `release.json.sigstore.json` (the job needs `id-token:
write`, and `.github/release-signing-config.json` pins Sigstore's public
instance with Rekor v1).

Don't move or reuse a tag. If a release is bad, publish the next version.

Publish releases as stable releases (not drafts or pre-releases): installations
only see GitHub's "latest" release.

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
  signed (`ghcr.io/upsetdev/pyo/pyo:0.1.0@sha256:…`)
- `manifests`: `deploy/k8s/platform.yaml` and `deploy/k8s/pyo.yaml` as JSON
  objects with those images. `__TAILNET_IP__`, `__REGISTRY_PUSH__`,
  `__TAILNET_API__` and `__TAILNET_NAME__` stay as they are, and the upgrader reads each installation's
  own values from what it's running
- `migrations`: the migration files, so an installation can say how many it
  hasn't run yet
- the notices from `deploy/release-notices.json`
- `k3s`: the k3s a fresh install gets, from `deploy/k3s.env` (see
  [Bump k3s](#bump-k3s)).

To try it before tagging:

```bash
bun run scripts/release-manifest.ts --version 0.1.0 --images ghcr.io/upsetdev/pyo \
  --pyo sha256:<64 hex> --agent sha256:<64 hex> --builder sha256:<64 hex> --flows sha256:<64 hex> \
  --waf sha256:<64 hex> --geoip sha256:<64 hex> --db-controller sha256:<64 hex> --kv sha256:<64 hex> \
  --mysql-operator sha256:<64 hex> --mysql-backup sha256:<64 hex> \
  --postgres-16 sha256:<64 hex> --postgres-17 sha256:<64 hex> --postgres-18 sha256:<64 hex>
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

## Install or update a release

The release workflow attaches `deploy/install.sh` as `install.sh`, and
`https://pyo.dev/install.sh` runs the one from the latest stable release:

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

Never edit a migration that has been merged. sqlx checksums applied migrations,
and Pyo refuses to start if one has changed.

A new migration is the next number in `crates/pyo-api/migrations/`, e.g.
`0002_what_it_does.sql`. `0001_init.sql` is the initial schema. Until the
first release is tagged, check-migrations.ts still lets the migrations be
edited and merged. From v0.1.0 on, they're frozen.
