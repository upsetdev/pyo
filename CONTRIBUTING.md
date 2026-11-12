# Contributing to Pyo

Thanks for helping with Pyo. This guide covers running it from source, the
checks a pull request has to pass, and a few conventions.

What's in the repository:

| Path | What it is |
|---|---|
| `crates/` | The Rust workspace: `pyo-api` (the API, which also serves the dashboard), `pyo-agent`, `pyo-builder`, `pyo-cli` (the `pyo` binary), `pyo-flows`, `pyo-proto`, and the database crates `pyo-db-controller`, `pyo-kv` and `pyo-resp` |
| `dash/` | The dashboard: React, Vite and TanStack Router |
| `waf/` | The WAF's processor, in Go (see [`waf/README.md`](waf/README.md)) |
| `deploy/images/` | Dockerfiles for every image Pyo ships |
| `deploy/k8s/` | The manifests Pyo is deployed with |
| `scripts/` | Development and release scripts |

## Requirements

- **Linux on x86-64** with `sudo`. Pyo runs in [k3s](https://k3s.io), which
  `scripts/setup-host.sh` installs. The setup script uses `apt-get`, so a
  Debian or Ubuntu machine is easiest.
- **Rust 1.99.1**, the version CI and the images build with.
- **Bun 1.4.2** for the dashboard and the scripts in `scripts/` and `dash/scripts/`.
- **Docker** for the WAF. `waf/build.sh` runs Go 1.26 in a container, so you
  don't need Go installed.
- **Tailscale** (optional) to reach your development Pyo from other devices
  over HTTPS.

## Run Pyo from source

Everything Pyo needs (Postgres, ClickHouse, the API, the dashboard, BuildKit
and an image registry) runs inside k3s. Images are built by BuildKit in the
cluster, pushed to the cluster's registry on `127.0.0.1:5005`, and deployed
from there.

**1. One-time host setup (as root):**

```bash
sudo bash scripts/setup-host.sh
```

Read the script before you run it. It changes the machine:

- It installs `build-essential` and `pkg-config` with `apt-get`.
- It writes `/etc/rancher/k3s/config.yaml` and `/etc/rancher/k3s/registries.yaml`,
  then installs k3s with Traefik turned off. If k3s is already installed, it
  replaces those two files and restarts k3s.
- Ports **80, 443, 8000 and 5005** must be free. The script stops if they
  aren't. k3s also uses **6443**.

To undo it, run `/usr/local/bin/k3s-uninstall.sh`.

**2. Build and deploy (as your normal user):**

```bash
scripts/pyo-up.sh                   # build the images that changed, then deploy
scripts/pyo-up.sh --only pyo,agent  # build just these images, then deploy
scripts/pyo-up.sh --all             # build every image, then deploy
scripts/pyo-up.sh --no-build TAG    # redeploy images already built with TAG
scripts/pyo-up.sh --dry-run         # print what would be built, and stop
scripts/pyo-up.sh --verbose         # stream the builds' output as they run
scripts/pyo-up.sh --release 0.1.0   # install a published release instead
```

When it's done, the dashboard is on <http://localhost:8000>. On a fresh
install, the script prints a one-time login for `admin@pyo.local`.

Run `scripts/pyo-up.sh` again after each change you want to try in the cluster.
It builds with `scripts/build-images.sh`, which you can also run on its own:

- **Images:** `pyo`, `agent`, `builder`, `flows`, `db-controller` and `kv`
  (all from `deploy/images/pyo.Dockerfile`, one target each), `waf`,
  `geoip`, `postgres-16`, `postgres-17`, `postgres-18`, `mysql-operator`
  and `mysql-backup`. These are the names `--only` takes.
- **Only what changed:** each build gets a new timestamp tag, but an image
  whose inputs (its Dockerfile and the files it copies, tracked or not) are
  the same as at its last build keeps that build's tag, and isn't built,
  pushed or rolled out again. The Rust binaries are compiled once for all
  six Rust images and then hashed, so a change in `pyo-api` rebuilds only
  the `pyo` image. The last tags are in `~/.cache/pyo-up/images.tsv`.
  Delete it, or use `--all`, to build everything.
- **In parallel:** up to four builds run at once (`PYO_BUILD_JOBS`). Each
  one's output goes to a log. If one fails, the script stops and shows the
  end of that log.
- **Progress:** in a terminal, a board shows each image's state, time and
  current build step, redrawn every second (for the Rust binaries, also the
  crates compiled so far). Piped or in CI, it prints a line when a build
  starts or ends and every 15 seconds while one runs. `--verbose` streams
  every build's output instead, each line prefixed with its image. While
  `pyo-up.sh` waits for a rollout it shows the ready replicas and the time,
  and a pod that can't start (image pull, crash) right away, with its last
  log lines. `NO_COLOR` turns colors off.
- **Cargo profile:** the images are compiled with the `dev-release` profile
  (`Cargo.toml`), which compiles faster than `release`. Releases use
  `release`. Set `PYO_CARGO_PROFILE=release` to build like a release.

What `pyo-up.sh` does besides building and deploying:

- It downloads `buildctl` to `~/.local/bin` if you don't have it.
- It uses `sudo` to install the systemd timers `pyo-connect` and
  `pyo-tailnet-cert`, the helper scripts in `/usr/local/lib/pyo`, and a key
  in `/etc/pyo`. These connect this machine as a server once you've signed up.
- On Ubuntu with kernel 6.17 or later, it applies the AppArmor workaround in
  `deploy/host/apparmor-workaround.sh`. Without it, Postgres never becomes
  ready.
- Set `MAXMIND_LICENSE_KEY` when you run it to get visitor locations. Without
  a key, locations show as unknown.

**HTTPS on your tailnet:** if this machine is on Tailscale with MagicDNS and
HTTPS certificates turned on, Pyo is also served on `https://<machine>:8443`,
with its registry on `:5443`. Tailscale's certificates are public, so the
machine's name and your tailnet's name end up in Certificate Transparency
logs. If you don't want that, use a randomized tailnet name.

## Work on the dashboard

The dashboard lives in `dash/`, a Bun workspace: run `bun install` and the
`bun run` scripts (`dev`, `build`, `typecheck`, `test`) from the repository
root. The Vite dev server (<http://localhost:5173>) proxies `/api`,
`/install` and the OAuth discovery paths to a running Pyo API.
`PYO_API_URL` sets which API it uses, read from the repository's `.env`.

```bash
cp .env.example .env
bun install
```

**Against the Pyo in k3s:** set `PYO_API_URL=http://localhost:8000` in
`.env`, then run `bun run dev`.

**Against an API you run with Cargo:** keep the default
`PYO_API_URL=http://127.0.0.1:8071`. The API needs a Postgres database. It
doesn't read `.env`, so export its settings in the shell (the others are
listed in `.env.example`):

```bash
DATABASE_URL=postgres://pyo:PASSWORD@127.0.0.1:5432/pyo \
LISTEN_ADDR=127.0.0.1:8071 \
cargo run -p pyo-api
```

Then run `bun run dev`.

The template catalog in `dash/src/lib/pyo/catalog.json` comes from
[pyo-templates](https://github.com/upsetdev/pyo-templates). Don't edit it by
hand. Run `bun scripts/sync-templates.ts` in `dash/` with pyo-templates built
next to this repository.

## Run the checks

CI (`.github/workflows/ci.yml`) runs these on every pull request. Run them
before you open one:

```bash
# Rust
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --locked --workspace

# Dashboard and scripts, and their tests
bun install --frozen-lockfile
bun run typecheck
bun run test
bun run build

# The migration check against main
bun run scripts/check-migrations.ts --base origin/main

# WAF (go vet, go test and a build, in docker)
waf/build.sh
```

If you change the WAF's custom rules, also run the end-to-end harness. It
runs the WAF behind a real Envoy in docker. See [`waf/README.md`](waf/README.md):

```bash
PYO_WAF_CONFIG=/tmp/waf.json cargo test -p pyo-api waf_harness_config -- --ignored
waf/test/rules.sh /tmp/waf.json <GeoLite2-Country.mmdb>
```

## Database migrations

Migrations run while the previous release is still serving, so the previous
release has to keep working on the new schema.

- Add a new file with the next number in `crates/pyo-api/migrations/`, for
  example `0002_what_it_does.sql`.
- Additive changes are always fine: new tables, nullable columns, and columns
  with a default.
- Removing or renaming something takes several releases.
- Never edit a migration that has been merged.

`scripts/check-migrations.ts` enforces these rules in CI. The full rules are
in [RELEASING.md](RELEASING.md#change-the-database).

## Docs and API reference

The docs live in [pyo-docs](https://github.com/upsetdev/pyo-docs). The website
that renders them is [pyo-web](https://github.com/upsetdev/pyo-web). Templates
live in [pyo-templates](https://github.com/upsetdev/pyo-templates). Keep
`pyo-docs` checked out next to this repository when you change the API, the
CLI or the MCP tools.

Some pages are generated from the code, so don't edit them by hand:

- **API reference:** the OpenAPI file comes from the `#[utoipa::path]`
  attributes and the operations in `crates/pyo-api/src/ops.rs`. Regenerate it
  with `cargo run -p pyo-api --bin openapi`, which writes it to
  `../pyo-docs/openapi/pyo.json`. When `pyo-docs` is checked out next to this
  repository, `cargo test` fails if that copy is out of date.
- **MCP tools and CLI commands:** these come from `crates/pyo-api/src/mcp.rs`
  and the CLI's help. Run `cargo build -p pyo-cli` here, then
  `bun run generate` in `pyo-docs`.

See [RELEASING.md](RELEASING.md#api-reference) for details.

### Wording in the dashboard and docs

- Machines are **servers**, never "nodes".
- Keep dashboard copy minimal: short labels, no explanations that the UI
  already makes clear.
- No Kubernetes jargon in the dashboard. People use Pyo without knowing it
  runs on k3s.
- The traffic layer is the **Gateway** in the dashboard and **Pyo Gateway**
  in the docs.

## Commit messages

Use [Conventional Commits](https://www.conventionalcommits.org):

```
<type>(<optional scope>): <short summary>

<optional body: what and why, wrapped at 72 characters>

<optional footer: BREAKING CHANGE: ..., Refs #123>
```

- Types: `feat`, `fix`, `docs`, `style`, `refactor`, `perf`, `test`, `build`,
  `ci`, `chore`, `revert`.
- Write the summary in lowercase and the imperative mood ("add", not
  "added"), with no trailing period, in about 72 characters or fewer.
- Make each commit one logical change. Put unrelated changes in separate
  commits.
- Mark breaking changes with `!` after the type or scope, as in
  `feat(api)!: ...`, and add a `BREAKING CHANGE:` footer.

## Pull requests

- The pull request template asks for what a review needs. Fill it in.
- Keep a pull request to one change, and explain what it changes and why.
- Make sure the [checks](#run-the-checks) pass.
- Add or update tests for what you change.
- Include screenshots for dashboard changes.
- If the API, CLI or MCP tools change, open a matching pull request in
  pyo-docs with the regenerated pages.

## Security issues

Don't report vulnerabilities in public issues. Report them privately through
[GitHub's security advisories](https://github.com/upsetdev/pyo/security/advisories/new).

## Licence

Pyo is licensed under the [Apache License 2.0](LICENSE). By contributing,
you agree that your contributions are licensed under it too.
