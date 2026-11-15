# syntax=docker/dockerfile:1.7
# Pyo's Rust images, one target each, from one build of every binary:
#
#   pyo            the control plane: pyo-api plus the dashboard it serves
#                  (the default target)
#   agent          pyo-agent: runs in each cluster and dials out to the API
#   builder        pyo-builder, with buildctl, git and railpack
#   flows          pyo-flows, the connection collector
#   db-controller  pyo-db-controller: runs a cluster's databases
#   kv             pyo-kv: the Redis address and REST API of each KV database
#   static         pyo-static: serves a cluster's static sites
#
# The `rust` stage builds them all with one `cargo build` per target (glibc
# and static musl), so building several images compiles the workspace once:
# BuildKit reuses that stage for every target. CARGO_PROFILE picks the Cargo
# profile: `release` (the default; releases) or `dev-release`
# (scripts/pyo-up.sh, quicker to compile).
#
# The `cli` target is the CLI alone, for the GitHub release
# (pyo-linux-x86_64, installed with `curl -fsSL https://pyo.dev/cli.sh | sh`).

FROM rust:1.99.1-trixie AS rust
RUN apt-get update && apt-get install -y --no-install-recommends musl-tools && rm -rf /var/lib/apt/lists/* \
 && rustup target add x86_64-unknown-linux-musl
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
ARG CARGO_PROFILE=release
# Static (musl): pyo-builder (it also runs on any Linux VM), pyo-flows and
# pyo-static (scratch images) and the CLI. The two builds run side by side, each in its
# own target directory. PYO_CLI_RELEASE marks the CLI as a release build:
# `pyo upgrade` replaces only those (a CLI built with plain cargo is left to
# its builder).
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    set -e; \
    cargo build --profile "$CARGO_PROFILE" --locked --target-dir target/gnu \
      -p pyo-api -p pyo-agent -p pyo-db-controller -p pyo-kv & gnu=$!; \
    PYO_CLI_RELEASE=1 cargo build --profile "$CARGO_PROFILE" --locked --target-dir target/musl --target x86_64-unknown-linux-musl \
      -p pyo-builder -p pyo-flows -p pyo-static -p pyo-cli & musl=$!; \
    wait "$gnu"; wait "$musl"; \
    mkdir /out; \
    for b in pyo-api pyo-agent pyo-db-controller pyo-kv; do cp "target/gnu/$CARGO_PROFILE/$b" /out/; done; \
    for b in pyo-builder pyo-flows pyo-static pyo; do cp "target/musl/x86_64-unknown-linux-musl/$CARGO_PROFILE/$b" /out/; done

# Just the binaries (scripts/build-images.sh hashes them to tell which
# images changed).
FROM scratch AS bins
COPY --from=rust /out/ /

# The CLI, for the release's pyo-linux-x86_64.
FROM scratch AS cli
COPY --from=rust /out/pyo /pyo

FROM debian:trixie-20261020-slim AS runtime
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates && rm -rf /var/lib/apt/lists/*

FROM runtime AS agent
COPY --from=rust /out/pyo-agent /usr/local/bin/pyo-agent
USER 65532:65532
ENTRYPOINT ["/usr/local/bin/pyo-agent"]

# Runs Pyo's databases on a cluster (Postgres through CloudNativePG, MySQL
# through MOCO, Valkey with Sentinel), from the Database objects the agent
# writes. Installed with a cluster's first database (pyo-proto's
# databases_bundle).
FROM runtime AS db-controller
COPY --from=rust /out/pyo-db-controller /usr/local/bin/pyo-db-controller
USER 65532:65532
ENTRYPOINT ["/usr/local/bin/pyo-db-controller"]

# pyo-db-controller runs it beside each KV database (Valkey).
FROM runtime AS kv
COPY --from=rust /out/pyo-kv /usr/local/bin/pyo-kv
USER 65532:65532
ENTRYPOINT ["/usr/local/bin/pyo-kv"]

# The connection collector (connection analytics, off by default): a static
# binary, nothing else. It runs as root in the server's network with
# NET_ADMIN only, to hear conntrack's events; no volumes, read-only root.
FROM scratch AS flows
COPY --from=rust /etc/ssl/certs/ca-certificates.crt /etc/ssl/certs/ca-certificates.crt
COPY --from=rust /out/pyo-flows /pyo-flows
ENTRYPOINT ["/pyo-flows"]

# Pyo Static: serves every static site of a cluster from its cache on the
# pod's disk, reading folders through Pyo. A static binary and the CA
# certificates, nothing else; non-root, read-only root.
FROM scratch AS static
COPY --from=rust /etc/ssl/certs/ca-certificates.crt /etc/ssl/certs/ca-certificates.crt
COPY --from=rust /out/pyo-static /pyo-static
USER 65532:65532
ENTRYPOINT ["/pyo-static"]

# The builder: pyo-builder plus buildctl, git (it checks out each build's
# commit) and railpack (zero-config builds of repositories without a
# Dockerfile). BuildKit itself runs beside it (a second container in the
# pod).
FROM moby/buildkit:v0.34.1 AS buildkit

# Railpack's CLI; its BuildKit frontend image must be the same version
# (RAILPACK_FRONTEND in pyo-builder).
FROM debian:trixie-20261020-slim AS railpack
ADD --checksum=sha256:2842de93e68713af9037e0bc0a398d7da78f3b96aa4804303a638db2bc69bd30 \
    https://github.com/railwayapp/railpack/releases/download/v0.40.1/railpack-v0.40.1-x86_64-unknown-linux-musl.tar.gz /tmp/railpack.tgz
RUN mkdir /tmp/rp && tar -xzf /tmp/railpack.tgz -C /tmp/rp && install -m 0755 "$(find /tmp/rp -type f -name railpack | head -n1)" /railpack

FROM debian:trixie-20261020-slim AS builder
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates git && rm -rf /var/lib/apt/lists/*
COPY --from=buildkit /usr/bin/buildctl /usr/local/bin/buildctl
COPY --from=railpack /railpack /usr/local/bin/railpack
COPY --from=rust /out/pyo-builder /usr/local/bin/pyo-builder
# Runs as root: it talks to BuildKit's socket, which BuildKit (root) creates.
ENTRYPOINT ["/usr/local/bin/pyo-builder"]

# The dashboard.
FROM oven/bun:1.4.2 AS web
WORKDIR /src
COPY package.json bun.lock ./
COPY dash/package.json ./dash/
RUN bun install --frozen-lockfile
COPY dash/tsconfig.json dash/vite.config.ts ./dash/
COPY dash/public ./dash/public
COPY dash/src ./dash/src
COPY dash/scripts/fix-shell-css.mjs ./dash/scripts/
RUN bun run build

# GeoLite2 City, bundled so visitor locations work without a MaxMind key
# (as Umami does); with MAXMIND_LICENSE_KEY set, the API also keeps a fresh
# copy (crates/pyo-api/src/geoip.rs).
FROM alpine:3.24.3 AS geo
RUN wget -qO- https://raw.githubusercontent.com/GitSquared/node-geolite2-redist/master/redist/GeoLite2-City.tar.gz | tar xz -C /tmp \
 && mkdir -p /geo && mv /tmp/GeoLite2-City_*/GeoLite2-City.mmdb /geo/

# The control plane: the API and the dashboard it serves.
FROM runtime AS pyo
COPY --from=rust /out/pyo-api /usr/local/bin/pyo-api
COPY --from=web /src/dash/dist/client /app/web
COPY --from=geo /geo/GeoLite2-City.mmdb /app/geoip/GeoLite2-City.mmdb
ENV PYO_WEB_DIR=/app/web LISTEN_ADDR=0.0.0.0:8000
USER 65532:65532
EXPOSE 8000
ENTRYPOINT ["/usr/local/bin/pyo-api"]
