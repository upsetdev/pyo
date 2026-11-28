# Images

Every image Pyo runs. `scripts/images-md.ts` writes this file from
`deploy/pyo-images.json` and `deploy/mirrors.txt`. Don't edit it by hand:
change those and run `bun run scripts/images-md.ts`. CI fails when it's
out of date.

## Pyo's own

Each one has the same name on `ghcr.io/upsetdev` and `docker.io/upsetdev`. Pyo pulls from
ghcr.io. A release pins each image by digest in its `release.json`.

| Image | What it is | ghcr.io | Docker Hub |
|---|---|---|---|
| `upsetdev/pyo` | Pyo's API, dashboard and workers | [ghcr.io](https://github.com/orgs/upsetdev/packages/container/package/pyo) | [Docker Hub](https://hub.docker.com/r/upsetdev/pyo) |
| `upsetdev/pyo-agent` | The agent on each cluster. Also runs updates | [ghcr.io](https://github.com/orgs/upsetdev/packages/container/package/pyo-agent) | [Docker Hub](https://hub.docker.com/r/upsetdev/pyo-agent) |
| `upsetdev/pyo-builder` | Builds services from Git and folders into images | [ghcr.io](https://github.com/orgs/upsetdev/packages/container/package/pyo-builder) | [Docker Hub](https://hub.docker.com/r/upsetdev/pyo-builder) |
| `upsetdev/pyo-flows` | Connection analytics collector, one per server | [ghcr.io](https://github.com/orgs/upsetdev/packages/container/package/pyo-flows) | [Docker Hub](https://hub.docker.com/r/upsetdev/pyo-flows) |
| `upsetdev/pyo-waf` | Web application firewall for Pyo Gateway (Coraza) | [ghcr.io](https://github.com/orgs/upsetdev/packages/container/package/pyo-waf) | [Docker Hub](https://hub.docker.com/r/upsetdev/pyo-waf) |
| `upsetdev/pyo-geoip` | GeoLite2 Country database for country rules | [ghcr.io](https://github.com/orgs/upsetdev/packages/container/package/pyo-geoip) | [Docker Hub](https://hub.docker.com/r/upsetdev/pyo-geoip) |
| `upsetdev/pyo-db-controller` | Runs Pyo's built-in databases | [ghcr.io](https://github.com/orgs/upsetdev/packages/container/package/pyo-db-controller) | [Docker Hub](https://hub.docker.com/r/upsetdev/pyo-db-controller) |
| `upsetdev/pyo-kv` | REST API in front of a KV database (Valkey) | [ghcr.io](https://github.com/orgs/upsetdev/packages/container/package/pyo-kv) | [Docker Hub](https://hub.docker.com/r/upsetdev/pyo-kv) |
| `upsetdev/pyo-static` | Serves static sites | [ghcr.io](https://github.com/orgs/upsetdev/packages/container/package/pyo-static) | [Docker Hub](https://hub.docker.com/r/upsetdev/pyo-static) |
| `upsetdev/pyo-postgres` | PostgreSQL 16, 17 and 18 for databases, tagged `<major>-<version>` | [ghcr.io](https://github.com/orgs/upsetdev/packages/container/package/pyo-postgres) | [Docker Hub](https://hub.docker.com/r/upsetdev/pyo-postgres) |
| `upsetdev/pyo-mysql-operator` | MySQL operator (MOCO, patched) | [ghcr.io](https://github.com/orgs/upsetdev/packages/container/package/pyo-mysql-operator) | [Docker Hub](https://hub.docker.com/r/upsetdev/pyo-mysql-operator) |
| `upsetdev/pyo-mysql-backup` | MySQL backups (MOCO, patched) | [ghcr.io](https://github.com/orgs/upsetdev/packages/container/package/pyo-mysql-backup) | [Docker Hub](https://hub.docker.com/r/upsetdev/pyo-mysql-backup) |

The sandbox templates' images are built in [upsetdev/pyo-sandbox-images](https://github.com/upsetdev/pyo-sandbox-images).
They keep their current names until that repository publishes the flat
ones.

| Image | What it is | Published as | ghcr.io |
|---|---|---|---|
| `upsetdev/pyo-sandbox-base` | Sandbox template base (Ubuntu 24.04) | `ghcr.io/upsetdev/pyo-sandbox-images/base` | [ghcr.io](https://github.com/orgs/upsetdev/packages/container/package/pyo-sandbox-images%2Fbase) |
| `upsetdev/pyo-sandbox-desktop` | Sandbox template desktop (Ubuntu 24.04 with a desktop and a browser) | `ghcr.io/upsetdev/pyo-sandbox-images/desktop` | [ghcr.io](https://github.com/orgs/upsetdev/packages/container/package/pyo-sandbox-images%2Fdesktop) |

## Third-party images

Pulled straight from Docker Hub, pinned by tag and digest.

| Image | Version | Digest | Used for |
|---|---|---|---|
| [`docker.io/library/registry`](https://hub.docker.com/_/registry) | `3.1.2` | `sha256:82f6ff4a7f98…` | Pyo's own registry |
| [`docker.io/alpine/socat`](https://hub.docker.com/r/alpine/socat) | `1.8.1.3` | `sha256:5aa280c6fc5b…` | The port forward to Pyo's registry |
| [`docker.io/moby/buildkit`](https://hub.docker.com/r/moby/buildkit) | `v0.35.1` | `sha256:baf6138eb8c3…` | Builds |
| [`docker.io/library/postgres`](https://hub.docker.com/_/postgres) | `18.6-alpine` | `sha256:9ba53c7d41ea…` | Pyo's database, and pg_dump for backups |
| [`docker.io/clickhouse/clickhouse-server`](https://hub.docker.com/r/clickhouse/clickhouse-server) | `26.8.36.3` | `sha256:d426d2f612e1…` | Analytics |
| [`docker.io/dxflrs/garage`](https://hub.docker.com/r/dxflrs/garage) | `v2.5.0` | `sha256:677e5b758e44…` | The backup store |
| [`docker.io/timberio/vector`](https://hub.docker.com/r/timberio/vector) | `0.60.0-debian` | `sha256:3ccb4bbd36e3…` | Logs and figures |
| [`docker.io/curlimages/curl`](https://hub.docker.com/r/curlimages/curl) | `8.11.1` | `sha256:c1fe1679c34d…` | A server's public address check |
| [`docker.io/cloudflare/cloudflared`](https://hub.docker.com/r/cloudflare/cloudflared) | `2026.10.7` | `sha256:4955524aeaa3…` | The Cloudflare Tunnel connector |
| [`docker.io/restic/restic`](https://hub.docker.com/r/restic/restic) | `0.17.3` | `sha256:8f5a62b422a2…` | Volume backups |
| [`docker.io/library/mysql`](https://hub.docker.com/_/mysql) | `8.4` | `sha256:8c233bad33d5…` | Client tools to back up services' own MySQL, MariaDB and MongoDB |
| [`docker.io/library/mariadb`](https://hub.docker.com/_/mariadb) | `11.8` | `sha256:e1d5495f4456…` | Client tools to back up services' own MySQL, MariaDB and MongoDB |
| [`docker.io/library/mongo`](https://hub.docker.com/_/mongo) | `8.0` | `sha256:479e85ced54f…` | Client tools to back up services' own MySQL, MariaDB and MongoDB |
| [`docker.io/valkey/valkey`](https://hub.docker.com/r/valkey/valkey) | `8.0` | `sha256:fcb69dfef815…` | KV databases, by Valkey version |
| [`docker.io/valkey/valkey`](https://hub.docker.com/r/valkey/valkey) | `8.1` | `sha256:5032c4dd7abe…` | KV databases, by Valkey version |
| [`docker.io/valkey/valkey-bundle`](https://hub.docker.com/r/valkey/valkey-bundle) | `8.1.12` | `sha256:ca9d1c30a6b3…` | KV databases with modules: Valkey 8.1 |
| [`docker.io/envoyproxy/gateway-helm`](https://hub.docker.com/r/envoyproxy/gateway-helm) | `v1.9.3` | `sha256:be1b8f163a33…` | Pyo Gateway: Envoy Gateway's Helm chart |
| [`docker.io/envoyproxy/gateway`](https://hub.docker.com/r/envoyproxy/gateway) | `v1.9.3` | `sha256:192fc9bdadb6…` | Pyo Gateway: Envoy Gateway's controller |
| [`docker.io/envoyproxy/envoy`](https://hub.docker.com/r/envoyproxy/envoy) | `distroless-v1.39.5` | `sha256:9d84a586119a…` | Pyo Gateway: the proxies |

## Old names

Up to 0.52.0, Pyo's images were `ghcr.io/upsetdev/pyo/<name>`. Each release copies its
images there too until the date below. Releases after it aren't copied. Tags
published before it stay.

| Old name | Now | Stops |
|---|---|---|
| `ghcr.io/upsetdev/pyo/pyo` | `ghcr.io/upsetdev/pyo` | 2027-03-01 |
| `ghcr.io/upsetdev/pyo/agent` | `ghcr.io/upsetdev/pyo-agent` | 2027-03-01 |
| `ghcr.io/upsetdev/pyo/builder` | `ghcr.io/upsetdev/pyo-builder` | 2027-03-01 |
| `ghcr.io/upsetdev/pyo/flows` | `ghcr.io/upsetdev/pyo-flows` | 2027-03-01 |
| `ghcr.io/upsetdev/pyo/waf` | `ghcr.io/upsetdev/pyo-waf` | 2027-03-01 |
| `ghcr.io/upsetdev/pyo/geoip` | `ghcr.io/upsetdev/pyo-geoip` | 2027-03-01 |
| `ghcr.io/upsetdev/pyo/db-controller` | `ghcr.io/upsetdev/pyo-db-controller` | 2027-03-01 |
| `ghcr.io/upsetdev/pyo/kv` | `ghcr.io/upsetdev/pyo-kv` | 2027-03-01 |
| `ghcr.io/upsetdev/pyo/static` | `ghcr.io/upsetdev/pyo-static` | 2027-03-01 |
| `ghcr.io/upsetdev/pyo/postgres` | `ghcr.io/upsetdev/pyo-postgres` | 2027-03-01 |
| `ghcr.io/upsetdev/pyo/mysql-operator` | `ghcr.io/upsetdev/pyo-mysql-operator` | 2027-03-01 |
| `ghcr.io/upsetdev/pyo/mysql-backup` | `ghcr.io/upsetdev/pyo-mysql-backup` | 2027-03-01 |
| `ghcr.io/upsetdev/pyo-sandbox-images/base` | `ghcr.io/upsetdev/pyo-sandbox-base` | When upsetdev/pyo-sandbox-images moves |
| `ghcr.io/upsetdev/pyo-sandbox-images/desktop` | `ghcr.io/upsetdev/pyo-sandbox-desktop` | When upsetdev/pyo-sandbox-images moves |
| `ghcr.io/upsetdev/pyo-mirror/<path>` | the image on Docker Hub | Stopped with 0.52.1. The packages stay for clusters on 0.52.0 |
