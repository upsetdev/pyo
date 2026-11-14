# Imports from other platforms

Internal plan and notes for moving a team's services from another platform
into Pyo. v1 ships Coolify and Dokploy (self hosted) and Railway and Vercel
(hosted). The last section is a draft for a pyo-docs page.

Code: `crates/pyo-api/src/platform_import/` (mod.rs, coolify.rs, dokploy.rs,
railway.rs, vercel.rs),
fixtures in `crates/pyo-api/tests/fixtures/platforms/`, the dashboard page
`dash/src/routes/_app/$team/import_.platform.tsx`, the CLI `pyo import`.

## How it works

1. **Preview** `POST /api/v1/platform-imports/preview` with
   `{teamId, platform, url, token}` (`url` empty for Railway and Vercel,
   and an optional `team` for Vercel). Pyo walks the platform's REST API
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
3. **Runs on the server.** The import answers 202 at once with the run
   (`pyo.platform_imports`) and goes on in a task of its own, so a closed
   tab or a dropped connection can't stop it half done (services made,
   databases not). The token stays in that task's memory only. The run
   records its items once the platform is read, then each result as it
   finishes (services after their transaction commits, each database as
   it's made). `GET /api/v1/platform-imports/{id}` follows one and
   `GET /api/v1/platform-imports?teamId=` lists a team's (30 days). The
   dashboard keeps the run's id in the address and shows the team's last
   import when there's none, so a reload or a reopened tab picks it back
   up. The CLI follows it until it ends. A running import says it's alive
   every 20 s: one whose process stopped shows as interrupted after 2
   minutes. Importing again is safe: services and databases are found by
   `importedFrom`, and a database's record is saved in the same
   transaction that makes it.

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
- **Hosted platforms:** Railway and Vercel have no address to give. Pyo
  sends their token only to their official API,
  `https://backboard.railway.com/graphql/v2` and `https://api.vercel.com`.
  An address given anyway must be that host over https, else it's refused.
  The connection still goes through the guarded client. Vercel's team is
  checked (letters, digits, `-`, `_`) and sent as `teamId` or `slug`.
- **Limits:** 20 s per request, 16 MiB per answer, 600 requests per read,
  4 at a time.
- **Errors** say what went wrong (refused token, not found, not JSON)
  without the platform's answer and without the token.
- **Idempotent:** each service stores `source.importedFrom`
  `{platform, host, id, project}`. For Railway and Vercel the host is
  `railway.com` or `vercel.com` (their ids are unique across accounts). A second import skips what's there and
  adds new services to the project the first one made. Databases keep the
  same record in `pyo.databases.imported_from` and match by it, never by
  name. A new database whose name is taken in the team, or whose objects
  would meet another database's (`shop-2` is one of Postgres `shop`'s
  pods), gets the platform in front: `railway-shop`, then `railway2-shop`.
- **Not deployed** unless `deploy`: services are saved with status stopped
  and no deployment. Domains are stored and go live with the first deploy.
- **Databases a service uses** are connected to it before its first
  deploy, and a service Pyo can't connect is held (next section).

## Databases a service uses

Code: `platform_import/links.rs` (detection), `connect_and_deploy` in
mod.rs (apply).

When a service's variable points at a database the same import brings,
Pyo connects the new database to the service in place of that variable,
as the database's Connect action does, before the first deploy. So the
first deploy runs against the new database with nothing to fix by hand.
When a variable points at a database Pyo can't match, the service is
held: made, not deployed, with the reason.

**Detection** (while planning, conservative). Each platform first says
what it can, then every value is read as a database is reached:

- **Railway:** a value that is one reference to a database service,
  `${{Postgres.DATABASE_URL}}`, links by that service and the variable's
  name: `DATABASE_URL`, `DATABASE_PRIVATE_URL`, `DATABASE_PUBLIC_URL`,
  `MYSQL_URL`, `REDIS_URL`, `MONGO_URL` (and their public and private
  ones) are its URL, `PGHOST`, `MYSQLHOST`, `REDISHOST`,
  `RAILWAY_PRIVATE_DOMAIN` its host, then the `*PORT`, `*USER`,
  `*PASSWORD`, `*DATABASE` ones. A value built from several references
  (`postgresql://${{Postgres.PGUSER}}:…@${{Postgres.PGHOST}}:5432/…`) is
  read with them resolved to the database's values there.
- **Vercel:** a variable's `contentHint` names its store (`storeId`) and
  what it is: `postgres-url`, `postgres-prisma-url` (URL),
  `postgres-url-non-pooling` (direct URL), `postgres-host`, `-user`,
  `-password`, `-database`, `redis-url`, `redis-rest-api-url`, `-token`,
  `-read-only-token`. That works for secret values too. A KV store whose
  REST variables a project uses is made with its REST API on.
- **Coolify:** `real_value` comes with `{{environment.X}}` and the others
  resolved, so it's read by value. A database is reached by its uuid inside
  Coolify (`internal_db_url`) and by `external_db_url` from outside.
- **Dokploy:** `${{project.X}}` and `${{environment.X}}` are resolved as
  Dokploy does, so they're read by value. A database is reached by its
  `appName` on its engine's port, and by `externalPort` on its server's
  `ipAddress` (or Dokploy's address).
- **Every platform**, by value: a URL with scheme `postgres`,
  `postgresql`, `mysql`, `redis`, `rediss`, `mongodb` or `mongodb+srv`
  whose host and port (the scheme's default when none) are one database's
  and whose database name is that database's (not checked for Redis). A
  value that is a database's host with its port (`host:port`), or its
  host alone in a variable named `…HOST` or `…HOSTNAME`. Next to such a
  host, the variables with the same prefix (`PG` of `PGHOST`, `DB_` of
  `DB_HOST`) ending in `PORT`, `USER`/`USERNAME`, `PASSWORD`/`PASS`,
  `DATABASE`/`DB`/`NAME`/`DBNAME` whose value is that database's. A name
  with `DIRECT`, `NON_POOLING` or `UNPOOLED` in it is the direct URL.

Each link is in the plan as the service's `connects`:
`{database: <key>, env: {<its variable>: <the database's variable>}}`, and
the preview shows it ("DATABASE_URL: Pyo connects it to postgres, the
database it imports"). A link to a database Pyo doesn't import (MariaDB,
MongoDB and the others) is a hold instead.

**What Connect sets.** Each variable becomes the database's own:

| It held | Postgres | MySQL | KV |
| --- | --- | --- | --- |
| URL | `DATABASE_URL` | `DATABASE_URL` | `REDIS_URL` |
| Direct URL | `DATABASE_URL_DIRECT` | `DATABASE_URL` | `REDIS_URL` |
| Host | `POSTGRES_HOST` | `MYSQL_HOST` | `REDIS_HOST` |
| host:port | `POSTGRES_HOST:POSTGRES_PORT` | `MYSQL_HOST:MYSQL_PORT` | `REDIS_HOST:REDIS_PORT` |
| Port, user, password, database | `POSTGRES_PORT`, `_USER`, `_PASSWORD`, `_DATABASE` | `MYSQL_PORT`, `_USER`, `_PASSWORD`, `_DATABASE` | `REDIS_PORT`, `REDIS_PASSWORD` (no user or database: left as they are) |
| REST URL, token, read-only token | | | `KV_REST_API_URL`, `KV_REST_API_TOKEN`, `KV_REST_API_READ_ONLY_TOKEN` |

When the service used the database's own names only (`DATABASE_URL` for
Postgres), Pyo connects it as the Connect button does, without names of
its own: Postgres gives `DATABASE_URL` and `DATABASE_URL_DIRECT`, MySQL
`DATABASE_URL` and the `MYSQL_*` set, KV `REDIS_URL`. Otherwise it
connects with the service's names (`connect`'s `env` map, as for a
template), plus the database's own names the service doesn't use. So
`DATABASE_PUBLIC_URL` and the `PGHOST`, `PGUSER`… set aren't dropped:
they're replaced, under their names, with the new database's values
(the public URL becomes the internal one). The old variables are
removed in the same transaction, so Connect never refuses because
`DATABASE_URL` exists. The values are sealed, secret variables, and the
link is a `pyo.database_links` row like any connection.

**Apply** (the background run). Services with links, or a hold, are made
without a deployment. Then the databases are made. Then each such
service is connected to each of its databases that this import made, or
an earlier import made. Then, with `deploy`, Pyo waits (polling, up to
45 minutes) for each of those databases to be ready (`status.phase`
"Ready") and, when it's imported, for its data copy to succeed. Only then
does it start the first deploy. The result goes from "created" with
"Deploys once postgres is ready." to "created" with `connected`
(`[{database, databaseId, variables}]`), or "held" with why.

**Holds** (the service is made, not deployed, `state: "held"`. Without
`deploy` the reason is the result's message):

- "DATABASE_URL points at a database Pyo couldn't match. Connect one,
  then deploy." A database URL that matches none of the import's
  databases, or several, and whose host only the platform knows: no dot,
  `.internal`, `.local`, `.lan`, a private, loopback or 100.64/10
  address, the platform's own host, or one of its databases' hosts. Also
  a value that is a database's host (or `host:port`) shared by several.
  A URL to a public host outside the platform (a hosted database) is
  left alone: the service reaches it from Pyo the same.
- "MONGO_URL points at events, which Pyo doesn't import. Connect a
  database, then deploy."
- "DATABASE_URL points at shop-db, which this import doesn't bring.
  Import it, or connect a database, then deploy." The database wasn't
  picked.
- "shop-db couldn't be made, so it isn't connected. Connect a database,
  then deploy."
- "Copying shop-db's data failed. Retry it on the database's page, then
  deploy." / "shop-db wasn't ready in time. Deploy once it is."
- "Pyo couldn't connect shop-db: …" / "Its first deploy couldn't start: …"

Other services still deploy.

**Idempotent.** A service already linked to the database (a
`pyo.database_links` row) isn't connected again. A service an earlier
import made is connected on a later run when it isn't linked yet and
still has every variable the link replaces (a re-run after a crash, or a
database picked only the second time). It isn't deployed by the re-run.

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
  profile's API/CLI section (admins can let members make them). A key with
  Rate Limiting on gets better-auth's default, 10 requests a day, so an
  import answers 429: Pyo says to make a key with it off.
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

## Railway

Sources: https://docs.railway.com/integrations/api (endpoint, tokens, rate
limits), its guides `/integrations/api/manage-projects`, `manage-services`,
`manage-variables`, `manage-domains` and `manage-volumes`, the schema read by
introspection of the endpoint (types Project, Environment, ServiceInstance,
ServiceSource, VolumeInstance, TCPProxy, CustomDomain, ServiceDomain,
DeploymentTrigger, Variable, ApiTokenContext, ProjectToken),
https://docs.railway.com/variables and `/variables/reference` (references,
sealed variables, Railway's own variables), `/config-as-code/reference`,
`/databases/postgresql`, `/databases/mysql`, `/databases/redis`,
`/databases/mongodb`, `/networking/tcp-proxy`, `/volumes/reference`,
`/deployments/healthchecks` (PORT).

- **Auth:** GraphQL at `https://backboard.railway.com/graphql/v2`. Account
  and workspace tokens go as `Authorization: Bearer`, a project token as
  `Project-Access-Token` (one environment of one project). Pyo tries the
  first and falls back to the second. Railway refuses with "Not Authorized"
  in a 200 answer's `errors`.
- **Rate limit:** 100 requests an hour on the free plan, 1000 on Hobby. So
  the walk is a few large questions: `apiToken { workspaces }`, then
  `projects(workspaceId:)` per workspace (or `projectToken` and
  `project(id:)`), each project with its environments (not pull request
  ones: `isEphemeral: false`), service instances, volume instances,
  deployment triggers and variable names. Then one question per project with
  every service's variables and TCP proxies as aliases (IDs passed as
  GraphQL variables, never in the query text). Connections are read without
  paging.
- **Variables:** read with `unrendered: true` and resolved here:
  `${{shared.KEY}}`, `${{Service.KEY}}`, `${{KEY}}`. Railway's own
  `RAILWAY_PRIVATE_DOMAIN` becomes the service's name in Pyo,
  `RAILWAY_PUBLIC_DOMAIN` its first custom domain. A reference to a database
  stays as written: Pyo connects the new database in its place. Others that can't be
  resolved stay as written, with a note. Sealed variables are never sent by
  the API: they come over secret and empty, with a note. A database's
  variables are read rendered, for its public URL.
- **Source:** `source.repo` (GitHub `owner/name`) with the branch of its
  deployment trigger, or `source.image`. Neither means it was uploaded with
  `railway up`: not importable. `railway.json`/`railway.toml` settings are
  read from `resolvedFileConfig.fileManifest` (`build.builder`,
  `build.dockerfilePath`, `deploy.startCommand`, `deploy.numReplicas` and
  the others) before the service's own.
- **Databases:** services from an image. Postgres
  (`ghcr.io/railwayapp-templates/postgres-ssl`, also PostGIS, pgvector,
  TimescaleDB images), MySQL, Redis (also Valkey, KeyDB, Dragonfly), MongoDB,
  MariaDB, ClickHouse are told apart by the image name. Public access fills
  `DATABASE_PUBLIC_URL` or `MYSQL_PUBLIC_URL`. Without it, a TCP proxy with
  the database's `PGUSER`/`PGPASSWORD`/`PGDATABASE` (or `MYSQLUSER`...) is
  used.

## Vercel

Sources: https://vercel.com/docs/rest-api and the OpenAPI document
https://openapi.vercel.sh/ (`servers`, `securitySchemes`, the 200 responses
of the endpoints below),
https://vercel.com/docs/environment-variables/sensitive-environment-variables,
https://vercel.com/docs/cron-jobs, https://railpack.com/config/environment-variables
and https://railpack.com/languages/node (what Railpack reads).

- **Auth:** `Authorization: Bearer <token>` to `https://api.vercel.com`.
  Tokens are made in Account Settings, Tokens. A team is read with `teamId`
  (an id, `team_...`) or `slug` on every request. Without one, the token's
  own scope. When that has no projects, `GET /v2/teams` names the teams the
  token reaches.
- **Walk:** `GET /v10/projects?limit=100` (pages through `from` =
  `pagination.next`, at most 5) → per project `GET /v9/projects/{id}`,
  `GET /v10/projects/{id}/env`, `GET /v9/projects/{id}/domains?limit=100` →
  `GET /v1/projects/{id}/env/{envId}` for each production variable of type
  `encrypted` (its decrypted value). The list's `decrypt` parameter is
  deprecated, so it isn't used.
- **Variables:** production ones only (`target` includes `production`, no
  `gitBranch`). `plain` values come in the list. `sensitive`, `secret` and
  `visibility: secret` are write only: by name, secret and empty, listed in a
  note to set again. `system` ones (`VERCEL_URL` and others) are left out
  with a note. Public ones (`NEXT_PUBLIC_`, `VITE_`, `PUBLIC_`, `REACT_APP_`,
  `NUXT_PUBLIC_`, `GATSBY_`, `VUE_APP_`) go to the build too.
- **Source:** `link.type` github, github-limited (`org`/`repo`),
  github-custom-host (`host`), gitlab (`projectUrl`), bitbucket
  (`owner`/`slug`), branch `productionBranch`, build context
  `rootDirectory`. No link means it was deployed with Vercel's CLI: not
  importable.
- **Build:** zero config (Railpack) with `installCommand`, `buildCommand`
  and `nodeVersion` as `RAILPACK_INSTALL_CMD`, `RAILPACK_BUILD_CMD`,
  `RAILPACK_NODE_VERSION` build arguments, and `outputDirectory` as
  `RAILPACK_SPA_OUTPUT_DIR` for static frameworks. HTTP port 3000 (Next.js's
  default, and Pyo sets `PORT` to it).
- **Storage:** told by each variable's `contentHint` (`type`, `storeId`). A
  Postgres store (`postgres-url-non-pooling`, else `postgres-url`) is copied
  by the database import from its URL. A key-value store (`redis-url`,
  `redis-rest-api-*`) becomes an empty KV database. Blob stores, Edge Config
  and integration secrets are notes. Each store is listed once, under the
  first project that uses it.

## What maps to what

| There | In Pyo |
| --- | --- |
| Coolify project + environment, Dokploy project + environment | A project. The production or default environment keeps the project's name, others are "Name (env)". Made only when it gets a service. |
| Application from Git | Service with a Git source and branch. Dockerfile build packs → Dockerfile (its path and base directory). Nixpacks, Railpack, static, buildpacks → zero config (Railpack), with a note for static and buildpacks. A GitHub repository whose owner has the team's GitHub App installed builds through it (private works). |
| Application from an image | Service from that image and tag. A registry login is a note (add it in Images). |
| Compose (Coolify services and one-click services, Dokploy raw compose) | A project of its own through Pyo's Compose plan, the platform's variables as its `.env` (so Coolify's `SERVICE_PASSWORD_*` and `SERVICE_FQDN_*` fill in). Each container's domains go to its HTTP port. |
| Compose from a repository (Coolify `dockercompose` build pack, Dokploy git compose) | Not imported: use Pyo's Compose import from the repository. |
| Variables | Variables. Secret when locked there (Coolify `is_shown_once`) or when the name looks secret (`compose::secret_key`, a superset of the dashboard's `SECRET_HINT`). Build only ones → build arguments. |
| Domains | Service domains (TLS auto), live after the first deploy. A port in the domain picks the HTTP port. Domains the platform generated (sslip.io, nip.io, traefik.me) are dropped with a note, and their port stays as a private HTTP port for a free Pyo domain. Paths are a note (Pyo serves the whole service). A domain seen twice goes to the first service. |
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

Railway:

| There | In Pyo |
| --- | --- |
| Project + environment | A project, as above (the primary or production environment keeps the name). |
| Service from GitHub | Service with that repository and its trigger's branch, root directory as the build context. A Dockerfile build (config file or Dockerfile path) → Dockerfile. Others → zero config, with the build and start commands as `RAILPACK_BUILD_CMD` and `RAILPACK_START_CMD`. Buildpacks get a note. |
| Service from an image | Service from that image. A start command is a note. |
| Variables | Variables, references resolved, sealed ones by name. |
| Custom domains | Service domains on their target port, else the `PORT` the service sets, else 8080. `*.up.railway.app` dropped with a note. |
| No domain | HTTP port 8080 (a new service's default, Pyo sets `PORT` to it), private, with no health check unless Railway has a path, since it may be a worker. |
| TCP proxy | A public TCP port on its application port, with a note of the old address. |
| Volumes | Empty volumes of the same size (at least 1 GiB). |
| Replicas, health check path | Replicas, an HTTP health check. |
| Postgres, MySQL with public access | A new database, its data copied. |
| Postgres, MySQL without | A new empty database, with how to copy the data. |
| Redis | A new empty KV database. |
| MongoDB, MariaDB, ClickHouse | Not imported, with what to do instead. |
| Cron schedule, pre-deploy command, sleeping when idle, uploads from `railway up` | Notes. |

Vercel:

| There | In Pyo |
| --- | --- |
| Project | A project with one service. |
| Git link | Service built from the repository and production branch, zero config (Railpack), port 3000. |
| Build, install, output, Node.js version | Railpack build arguments. |
| Production variables | Variables. Secret ones by name only, to set again. |
| Custom domains | Service domains. `*.vercel.app`, redirects, branch and custom environment domains are left out with a note. |
| Postgres store | A new database, its data copied from its URL. |
| Key-value store | A new empty KV database. |
| Next.js | Runs as one server (`next start`). Middleware, ISR and image optimization run in it, a note says so. |
| SvelteKit, Nuxt, Remix, Astro, SolidStart, React Router | A note to switch from the Vercel adapter to the Node one. |
| Static frameworks and Other | A note that Vercel Functions and Edge Functions don't run. |
| Cron jobs | A note with each path and schedule, to add as Pyo cron jobs. |
| Preview deployments, Blob, Edge Config, password protection, files outside the root directory | Notes. |

Not carried over (each one a note on its item, never dropped silently):
start, build and install commands, build secrets, file mounts, deploy keys,
preview environments, labels and custom Docker options, and volume data.
A variable that points at one of the platform's databases is replaced by
the new database's, or holds the service's deploy (above).

## Tests

- `platform_import::tests`: the walk asks for exactly the recorded answers,
  the mapping of every case above, previews hold no values, Git URL forms,
  the address guard (loopback, metadata, cluster network, login in the URL,
  plain http, query). Railway: reference resolution (shared, another
  service's, Railway's own, a database's, missing), sealed variables, a
  project token's single environment, the database URL from public access
  or the TCP proxy, the per project question (IDs as variables). Vercel:
  both pages, production values only, write-only variables by name, skipped
  domains, storage, the empty answer naming teams. Both: only the official
  API host is used, Vercel's team checked.
- Databases a service uses, per platform (`coolify_database_links`,
  `dokploy_database_links`, `railway_database_links`,
  `vercel_database_links`): a reference or hint, a matching host (inside
  and outside the platform, `host:port`, a `PGHOST` set), an unmatched URL
  and a database Pyo doesn't import hold, a database outside the platform
  is left alone. `a_database_not_brought_holds`.
- `imports_connect_databases_before_the_first_deploy` (ignored, needs
  Postgres): deploy on, the service's `DATABASE_URL` is replaced by the
  connection (`DATABASE_URL`, `DATABASE_URL_DIRECT`, secret) once the
  database is ready, one deployment, a `database_links` row. An unmatched
  URL and a database not picked hold, with no deployment. A second run
  connects and deploys nothing again.
- `hosted_imports_make_nothing_twice` (ignored, needs Postgres): imports the
  Railway and Vercel fixtures twice, the second run creates nothing, and
  `importedFrom.host` is `railway.com` or `vercel.com`.
- `importing_twice_makes_nothing_twice` (ignored, needs Postgres): creates
  from the Coolify fixture, checks services are stopped with no deployment,
  secret values sealed, `importedFrom` stored. A second run creates nothing.
  A new service joins the project the first run made. Run with
  `DATABASE_URL=… PYO_SECRET_KEY=<64 hex> cargo test -p pyo-api -- --ignored platform_import --test-threads=1`.
  The database's user must not be called `pyo` (it would see the `pyo`
  schema first).

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
- **Heroku**: Platform API with a token. Apps, config vars, domains, formation,
  attached Heroku Postgres (data through a backup URL).
  https://devcenter.heroku.com/articles/platform-api-reference
- Volume data (phase 2): copy a volume's files from the source server.
- Pick names and target project per item in the preview, and a per item
  cluster.
- Coolify and Dokploy Compose from a repository: run the Compose import from
  the repository when the team's GitHub App reaches it.
- Vercel cron jobs as Pyo cron jobs (each one calls its path on the
  service's domain on its schedule).
- Railway volume data through a volume backup, Railway MongoDB and Vercel
  Blob files.
- pyo-docs: add the page below and regenerate `openapi/pyo.json`
  (`cargo run -p pyo-api --bin openapi`): the two new endpoints change it,
  and the bodies' new `team` field and optional `url`.

## Docs page draft (pyo-docs)

> Title: Import from another platform

Move your services from Coolify, Dokploy, Railway or Vercel into Pyo. Pyo
reads them through the platform's API, shows you what each one becomes, and
imports the ones you pick. Owners and admins can import.

**Before you start**

- Coolify: in Keys & Tokens, make an API token with read and read:sensitive.
  Turn the API on in Settings if it's off.
- Dokploy: in your profile, API/CLI, generate an API key with Rate Limiting
  off.
- Railway: in Account Settings, Tokens, make a token. A workspace token or a
  project token works too. A project token reads one environment.
- Vercel: in Account Settings, Tokens, make a token with access to the team
  you're moving. Give the team's slug when you import.

Pyo uses the token only while it reads, and never stores it. For Railway
and Vercel, Pyo talks only to their own API.

**Import**

1. Open New, then Import from a platform.
2. Pick the platform, give its address (not for Railway or Vercel) and the
   token, then Preview.
3. Check what each project, service and database becomes. Notes say what
   doesn't carry over.
4. Pick what to bring, and choose whether to deploy the services now.
5. Import.

From the command line:

```
pyo import coolify --from https://coolify.example.com --source-token <token>
pyo import railway --source-token <token>
pyo import vercel --source-token <token> --source-team acme
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
- A service whose variables point at a database you import is connected to
  the new one before its first deploy, in place of those variables. When
  Pyo can't tell which database a variable points at, it doesn't deploy
  that service and says why.
- From Railway: each environment becomes a project. Build and start commands,
  references between services, TCP proxies and volume sizes come over.
  Turn on public access for a Postgres or MySQL database there to have its
  data copied. Sealed variables can't be read: set those in Pyo.
- From Vercel: each project becomes a service built with zero config, with
  its production variables and custom domains. Secret variables can't be
  read: Pyo lists them for you to set. A Vercel Postgres database is copied,
  and a key-value store becomes an empty KV database. Next.js runs as a
  server, so middleware, ISR and image optimization keep working. Vercel
  Functions in other frameworks, cron jobs and Blob files are listed for you
  to move.

Vercel and the Vercel logo are trademarks of Vercel, Inc. Pyo isn't
affiliated with or endorsed by Railway or Vercel.

Run it again any time: what was imported before is left alone, and new
services are added.
