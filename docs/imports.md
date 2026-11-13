# Imports from other platforms

Internal plan and notes for moving a team's services from another self-hosted
platform into Pyo. v1 ships Coolify and Dokploy. The last section is a draft
for a pyo-docs page.

Code: `crates/pyo-api/src/platform_import/` (mod.rs, coolify.rs, dokploy.rs),
fixtures in `crates/pyo-api/tests/fixtures/platforms/`, the dashboard page
`dash/src/routes/_app/$team/import_.platform.tsx`, the CLI `pyo import`.

## How it works

1. **Preview** `POST /api/v1/platform-imports/preview` with
   `{teamId, platform, url, token}`. Pyo walks the platform's REST API
   read only, server side, and returns a plan: per project there, its
   services, Compose stacks and databases, each with what it becomes and a
   warning for each thing that doesn't carry over. Variables come back as
   names only. Items an earlier import made are marked.
2. **Import** `POST /api/v1/platform-imports` with the same source plus
   `clusterId`, `items` (keys from the preview) and `deploy`. Pyo reads the
   platform again (the token is never stored, so nothing to seal in the
   `pyo1:` vault) and creates the picked items. Each item gets a result:
   created, importing (database data being copied), exists, skipped or
   failed. One failure doesn't stop the others (a savepoint per item).

Rules:

- **Who:** owners and admins, API keys with full access. Rate limit 30 per
  person per 15 minutes (`platform-import:user`), since each one reads the
  whole platform.
- **SSRF:** the address goes through `outbound.rs`: `parse` (http(s), no
  login in it) and `check` before use, and the client's resolver checks
  every connection again (DNS rebinding). Loopback, link-local and metadata
  addresses and the cluster's pod and service networks are refused always,
  private networks only when `PYO_ALLOW_PRIVATE_NETWORKS` allows (on for self
  hosted, off for Pyo Cloud), plain http only to private addresses.
  Redirects aren't followed. TLS is verified (rustls).
- **Limits:** 20 s per request, 16 MiB per answer, 600 requests per read,
  4 at a time.
- **Errors** say what went wrong (refused token, not found, not JSON)
  without the platform's answer and without the token.
- **Idempotent:** each service stores `source.importedFrom`
  `{platform, host, id, project}`. A second import skips what's there and
  adds new services to the project the first one made. Databases match by
  name in the team.
- **Not deployed** unless `deploy`: services are saved with status stopped
  and no deployment. Domains are stored and go live with the first deploy.

## Coolify

Sources: API reference https://coolify.io/docs/api-reference/authorization
and the OpenAPI document and controllers at tag v4.7.5:
https://github.com/coollabsio/coolify/blob/v4.7.5/openapi.json,
`app/Http/Controllers/Api/{Project,Applications,Services,Databases}Controller.php`,
`app/Models/{Application,LocalPersistentVolume,LocalFileVolume,ServiceApplication}.php`,
`routes/api.php`, `app/Http/Middleware/ApiAllowed.php`.

- **Auth:** `Authorization: Bearer <token>`, base `<url>/api/v1`. Tokens are
  made in Keys & Tokens, API tokens. Values (variables, Compose files,
  database URLs) only reach tokens with `read:sensitive` or `root`. The API
  can be off for the instance (`is_api_enabled`, 403 "API is disabled").
- **Walk:** `GET /projects` → `GET /projects/{uuid}/environments` →
  `GET /projects/{uuid}/{env_uuid}` (the environment with `applications`,
  `services`, `postgresqls`, `mysqls`, `mariadbs`, `mongodbs`, `redis`) →
  per application `GET /applications/{uuid}/envs` and `/storages`, per
  service `GET /services/{uuid}/envs` and `/applications`, and
  `GET /databases` once (KeyDB, Dragonfly and ClickHouse aren't in the
  environment's answer, so they're placed by `environment_id`).
- **Variables:** `key`, `value`/`real_value` (shared variables resolved),
  `is_shown_once` (locked: never sent, so it comes over secret and empty
  with a warning), `is_runtime`/`is_buildtime` (build only → build
  argument), `is_preview` (skipped).
- **Quirk:** applications from an image or a pasted Dockerfile keep the
  placeholder repository `coollabsio/coolify`. `git_repository` is
  `owner/name` for github.com, else a URL.

## Dokploy

Sources: https://docs.dokploy.com/docs/api, the generated OpenAPI document
https://github.com/Dokploy/website/blob/main/apps/docs/public/openapi.json
(its responses are untyped), so field names come from the source at v0.31.2:
`packages/server/src/db/schema/{application,compose,domain,mount,port,postgres,mysql,mariadb,mongo,redis,server,gitlab,gitea,environment,project}.ts`,
`apps/dokploy/server/api/routers/{project,application,compose,postgres}.ts`,
`packages/server/src/services/{application,compose,postgres}.ts`,
`packages/server/src/utils/docker/utils.ts` (variable references).

- **Auth:** `x-api-key: <key>`, base `<url>/api`. Keys are made in the
  profile's API/CLI section (admins can let members make them).
- **Walk:** `GET /project.all` (projects → environments → ids of
  applications, compose, postgres, mysql, mariadb, mongo, redis, libsql) →
  `GET /environment.one?environmentId=` (shared `env`) →
  `GET /application.one?applicationId=` (with `domains`, `mounts`, `ports`,
  `gitlab`, `gitea`, `server`), `GET /compose.one?composeId=`,
  `GET /<engine>.one?<engine>Id=`.
- **Variables:** `KEY=value` text. `${{project.X}}`, `${{environment.X}}` and
  `${{X}}` are resolved as Dokploy does. An unresolvable one stays as written,
  with a warning. No secret flag, so Pyo's secret name heuristic decides.
- **Source:** `sourceType` github / gitlab (`gitlab.gitlabUrl` +
  `gitlabPathNamespace`) / gitea (`gitea.giteaUrl`) / bitbucket / git
  (`customGitUrl`) / docker (`dockerImage`) / drop (uploaded, not importable).
- **Databases:** reachable from outside when `externalPort` is set, on
  `server.ipAddress` or else the Dokploy address. The URL is built from
  `databaseUser`, `databasePassword`, `databaseName`.

## What maps to what

| There | In Pyo |
| --- | --- |
| Coolify project + environment, Dokploy project + environment | A project. The production or default environment keeps the project's name, others are "Name (env)". Made only when it gets a service. |
| Application from Git | Service with a Git source and branch. Dockerfile build packs → Dockerfile (its path and base directory). Nixpacks, Railpack, static, buildpacks → zero config (Railpack), with a note for static and buildpacks. A GitHub repository whose owner has the team's GitHub App installed builds through it (private works). |
| Application from an image | Service from that image and tag. A registry login is a note (add it in Images). |
| Compose (Coolify services and one-click services, Dokploy raw compose) | A project of its own through Pyo's Compose plan, the platform's variables as its `.env` (so Coolify's `SERVICE_PASSWORD_*` and `SERVICE_FQDN_*` fill in). Each container's domains go to its HTTP port. |
| Compose from a repository (Coolify `dockercompose` build pack, Dokploy git compose) | Not imported: use Pyo's Compose import from the repository. |
| Variables | Variables. Secret when locked there (Coolify `is_shown_once`) or when the name looks secret (`compose::secret_key`, a superset of the dashboard's `SECRET_HINT`). Build only ones → build arguments. |
| Domains | Service domains (TLS auto), live after the first deploy. A port in the domain picks the HTTP port. Domains the platform generated (sslip.io, nip.io, traefik.me) are dropped with a note. Paths are a note (Pyo serves the whole service). A domain seen twice goes to the first service. |
| Exposed ports | HTTP ports, public when a domain uses them. |
| Published host ports (Coolify `ports_mappings`, Dokploy `ports`) | Public TCP or UDP ports through the gateway, when Pyo allows that port. |
| Volumes | Volumes of 10 GiB, empty. A host directory becomes an empty volume too, with a note. File mounts are a note. |
| Health check (Coolify) | HTTP on its path when on, else TCP on the first port. |
| Limits (Coolify) | Memory and CPU limits. |
| Replicas (Dokploy) | Replicas. |
| Postgres, MySQL reachable from outside | A new database of the same engine, its data copied by the database import (checked on the cluster first). |
| Postgres, MySQL not reachable | A new empty database, with how to copy the data (make it public there, or a dump). |
| Redis, KeyDB, Dragonfly | A new empty KV database. |
| MariaDB, MongoDB, ClickHouse, libSQL | Not imported, with what to do instead. |

Not carried over (each one a note on its item, never dropped silently):
start, build and install commands, build secrets, file mounts, deploy keys,
preview environments, labels and custom Docker options, and volume data.
A variable that still points at one of the platform's databases by its
internal name gets a note to connect the new database instead.

## Tests

- `platform_import::tests`: the walk asks for exactly the recorded answers,
  the mapping of every case above, previews hold no values, Git URL forms,
  the address guard (loopback, metadata, cluster network, login in the URL,
  plain http, query).
- `importing_twice_makes_nothing_twice` (ignored, needs Postgres): creates
  from the Coolify fixture, checks services are stopped with no deployment,
  secret values sealed, `importedFrom` stored. A second run creates nothing.
  A new service joins the project the first run made. Run with
  `DATABASE_URL=… PYO_SECRET_KEY=<64 hex> cargo test -p pyo-api -- --ignored platform_import`.

## Next

- **OpenShip** (https://github.com/oblien/openship, https://openship.io/docs/api).
  It has a REST API with personal access tokens (`openship token create`,
  bearer) and documents its routes (`GET /api/projects`,
  `GET /api/projects/:id`, `/info`, `/env`, `/cluster/databases`, domains,
  services), but not the response fields, and `GET /api/projects/:id/env`
  masks secret values. Next: read its source for the response shapes and a
  way to read secret values, then add `openship.rs`.
- **Dokku**: no HTTP API. Over SSH: `dokku apps:list`, `dokku config:export
  <app>`, `dokku domains:report <app>`, `dokku ports:report <app>`,
  `dokku storage:report <app>`, `dokku git:report <app>`, and the
  postgres/mysql/redis plugins' `:info`. Needs an SSH key, so a CLI side
  import (`pyo import dokku --ssh host`) fits better than the API.
  https://dokku.com/docs/
- **CapRover**: no documented public API. Its CLI and dashboard use
  `/api/v2/login` (password, returns a token) and
  `/api/v2/user/apps/appDefinitions`, with env vars, ports, volumes and
  custom domains per app. Undocumented, so pin a version.
  https://caprover.com/docs/
- **Render**: REST API with an API key. Services, env vars, custom domains,
  disks, Postgres and Key Value. https://api-docs.render.com/
- **Railway**: GraphQL public API with a token. Projects, environments,
  services, variables, domains, volumes.
  https://docs.railway.com/reference/public-api
- **Heroku**: Platform API with a token. Apps, config vars, domains, formation,
  attached Heroku Postgres (data through a backup URL).
  https://devcenter.heroku.com/articles/platform-api-reference
- Volume data (phase 2): copy a volume's files from the source server.
- Pick names and target project per item in the preview, and a per item
  cluster.
- Coolify and Dokploy Compose from a repository: run the Compose import from
  the repository when the team's GitHub App reaches it.
- pyo-docs: add the page below and regenerate `openapi/pyo.json`
  (`cargo run -p pyo-api --bin openapi`): the two new endpoints change it.

## Docs page draft (pyo-docs)

> Title: Import from another platform

Move your services from Coolify or Dokploy into Pyo. Pyo reads them through
the platform's API, shows you what each one becomes, and imports the ones you
pick. Owners and admins can import.

**Before you start**

- Coolify: in Keys & Tokens, make an API token with read and read:sensitive.
  Turn the API on in Settings if it's off.
- Dokploy: in your profile, API/CLI, generate an API key.

Pyo uses the token only while it reads, and never stores it.

**Import**

1. Open New, then Import from a platform.
2. Pick the platform, give its address and the token, then Preview.
3. Check what each project, service and database becomes. Notes say what
   doesn't carry over.
4. Pick what to bring, and choose whether to deploy the services now.
5. Import.

From the command line:

```
pyo import coolify --from https://coolify.example.com --source-token <token>
```

**What comes over**

- Each project becomes a Pyo project. Each application becomes a service,
  built from its repository (with its Dockerfile, or zero config) or run
  from its image.
- Variables, with secret ones sealed. Values locked in Coolify can't be read:
  set those in Pyo.
- Domains. They go live once the service is deployed. Point their DNS at
  Pyo when you're ready to switch.
- Ports and volumes. Volumes start empty: copy their data yourself.
- Compose stacks become projects of their own.
- Postgres and MySQL databases the platform makes reachable from outside are
  copied into new Pyo databases. Others are made empty or listed for you to
  move. Redis becomes a KV database.

Run it again any time: what was imported before is left alone, and new
services are added.
