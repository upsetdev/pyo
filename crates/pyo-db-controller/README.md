# pyo-db-controller

Runs Pyo's built-in databases on a cluster. `pyo-db-controller` turns
`Database` resources (`pyo.dev/v1alpha1`) into Postgres, run through
CloudNativePG, Valkey with Sentinel, or MySQL 8.4, run through MOCO. The Pyo
agent writes the `Database` objects. `pyo-kv` is the address in front of
Valkey: the Redis protocol and the REST API.

- `crates/pyo-db-controller`: the controller. `extensions.json` is the
  Postgres extension catalog the API and the dashboard read too.
- `crates/pyo-kv`, `crates/pyo-resp`: pyo-kv and its RESP client.
- `crates/pyo-proto/system/databases/`: what a cluster gets with its first
  database (`databases_bundle` in `crates/pyo-proto/src/bundles.rs`), all in
  the `pyo-databases` namespace:
  - `crd.yaml`: the `Database` resource, with its validation rules.
  - `controller.yaml`: the controller and its roles. Its image, pyo-kv's and
    Pyo's Postgres images are filled in for the `__..._IMAGE__`
    placeholders.
  - `cnpg.yaml`: CloudNativePG, vendored.
  - `moco.yaml`: MOCO, the MySQL operator, vendored (it needs cert-manager).
    Its operator and backup images are Pyo's build with a few patches
    (`deploy/images/moco/`), `PYO_MYSQL_OPERATOR_IMAGE` and
    `PYO_MYSQL_BACKUP_IMAGE`.
- `crates/pyo-db-controller/e2e/`: `e2e.yaml`, Databases for a test cluster,
  and `sdk.test.mjs`, which drives pyo-kv's REST API with a Redis REST SDK.

## Images

| Engine | Image |
|---|---|
| Postgres | Pyo's own per major version, `PYO_DB_POSTGRES_IMAGE_16`, `_17`, `_18` (`deploy/images/postgres.Dockerfile`). A version without one gets `PYO_DB_POSTGRES_IMAGE:<version>` (CloudNativePG's). |
| Valkey | `PYO_DB_VALKEY_IMAGE:<version>`; while it loads modules, `PYO_DB_VALKEY_BUNDLE_IMAGE` (valkey-bundle 8.1.11, pinned; Valkey 8.1 only) |
| MySQL | `PYO_DB_MYSQL_IMAGE`, pinned |
| KV address | `PYO_DB_PYO_KV_IMAGE` |

The other settings are listed at the top of `src/main.rs`.

## Extensions

`spec.postgres.extensions` lists the extensions Pyo turns on, the "managed"
ones in `extensions.json` (`src/extensions.rs`):

- Libraries that must load at start go in the Cluster's
  `shared_preload_libraries`, with their settings. CloudNativePG restarts the
  instances. With replicas it restarts one at a time and ends with a
  switchover (`primaryUpdateMethod: switchover`), so there is always a
  primary.
- The extension is created through CloudNativePG's declarative `Database`
  `<name>-db` (`databaseReclaimPolicy: retain`), only once its libraries are
  loaded on every instance. A custom monitoring query,
  `cnpg_pyo_preload_loaded`, reports what each instance has loaded.
- Turning one off sets it `absent`. CloudNativePG drops it without CASCADE,
  so Postgres refuses while other objects depend on it, and that refusal
  shows in the status. Its libraries stay loaded until it is dropped.
- With high availability, CloudNativePG refuses a new image and new settings
  at once. The controller applies the image first and the settings on the
  next pass.
- Read replicas load the same libraries, with the same settings.

`status.extensions[]` has each one's `name`, `state` (`enabled`, `pending`,
`restarting`, `disabling` or `failed`) and `message`. The database owner
creates the "additional" (trusted) extensions itself, so the controller
doesn't handle them.

## KV modules

`spec.valkey.modules` lists the Valkey modules a KV database loads, the
catalog's `kv` entries in `extensions.json` (`src/modules.rs`): `valkey-json`,
`valkey-bloom` and `valkey-search` (BSD-3-Clause), from valkey-bundle 8.1.11
(`json` 1.0.3, `bf` 1.0.1, `search` 1.0.3 by `MODULE LIST`). The bundle also
has valkey-ldap, which isn't offered.

- Every instance (primary, replicas, read replicas) runs the bundle image
  and gets a `loadmodule` line per module (`MODULES` in `valkey.sh`).
  Sentinels load none and keep the plain image.
- Changing them restarts the instances. The StatefulSets are `OnDelete`
  and the controller restarts each stale instance itself (`roll`), one at
  a time once every instance is ready and streaming: read replicas,
  replicas, then the primary. With sentinels the primary first hands over:
  it pauses writes (`CLIENT PAUSE WRITE`), a replica catches up,
  `SENTINEL FAILOVER`, and the controller makes the old primary a replica
  before the pause ends, so paused writes fail with `READONLY` instead of
  being lost. Version and size changes restart the same way.
- Valkey won't start from data holding a module's keys or indexes without
  the module. One being turned off stays loaded while the primary's INFO
  counts any (`json_num_documents`, `bf_bloom_num_objects`,
  `search_number_of_indexes`); its status says how many. Then each instance
  rewrites its append-only file before it restarts.
- `status.extensions[]` is what every instance's `MODULE LIST` says, in the
  Postgres shape (`enabled`, `restarting`, `disabling`, `failed`).
- pyo-kv asks the primary for `COMMAND` every 10s, so module reads Valkey
  flags `readonly` (`JSON.GET`, `BF.EXISTS`, ...) go to read replicas.
  Search's commands aren't flagged `readonly`, so `FT.*` stays on the
  primary. A replica answering `ERR unknown command` (mid-change) is
  retried on the primary.

## Read replicas

`spec.readReplicas.instances` adds instances that only serve reads and are
never promoted to primary. They are separate from `spec.instances`, whose
replicas exist for failover. Postgres can have 1 to 5 read replicas and
Valkey 1 to 4. They use the same size, storage and settings as the
database. A missing field or `instances: 0` turns the feature off and
removes every read-replica resource.

```yaml
spec: {engine: postgres, instances: 3, storage: 10Gi, readReplicas: {instances: 2}}
```

Apps read from `status.endpoints.read`. Writes there are refused.

| Engine | `endpoints.read` | What runs |
|---|---|---|
| Postgres | `<name>-read-pooler.<ns>.svc:5432` | CloudNativePG replica Cluster `<name>-read` and PgBouncer Pooler `<name>-read-pooler` (transaction mode, `max_prepared_statements` 200, type `r`) |
| Valkey | `<name>-read.<ns>.svc:6379` | StatefulSet `<name>-read` (`replica-priority 0`), per-pod Services `<name>-read-<i>`, and the Service `<name>-read` |

Read replicas log in with the database's own login: `username` and
`password` from the credentials secret. For Postgres, Pyo turns the endpoint
into the `DATABASE_URL_READ` variable. Valkey apps don't need a separate
address: pyo-kv sends their reads to the read replicas (see below).

### How they follow the primary

- **Postgres.** The replica Cluster streams from the main cluster's `-rw`
  Service, so after a failover it reconnects to the new primary. It logs in
  as `streaming_replica` with the main cluster's replication certificate.
  CloudNativePG never promotes a replica Cluster on its own. Inside it, one
  instance streams from the main cluster and the others stream from that
  instance. When backups are on, it can also fetch WAL from the archive,
  which lets it catch up after falling behind further than the primary
  keeps WAL.
- **Valkey.** Each read pod starts as a replica of the primary that the
  Sentinels name, addressed by its per-pod Service IP like the other
  instances. Because its priority is 0, the Sentinels never promote it, but
  after a failover they move it to the new primary like any other replica.

Read replicas are spread over different servers where possible.

### Status

```yaml
status:
  endpoints: {read: main-read-pooler.app.svc.cluster.local:5432}
  readReplicas:
    ready: 2
    members:
      - {name: main-read-1, node: server-a, ready: true, lagSeconds: 0}
      - {name: main-read-2, node: server-b, ready: true, lagSeconds: 0.3}
```

`lagSeconds` is how far behind the primary each member is. For Postgres it
comes from a metric the controller adds to the read cluster,
`cnpg_pyo_read_replica_lag` (ConfigMap `<name>-read-monitoring`). It is 0
when the instance has replayed everything it has received, and otherwise
the time since the last transaction it replayed. CloudNativePG's own
`cnpg_pg_replication_lag` isn't used: on an idle database, a replica that
has just reconnected looks minutes behind even when it is caught up. For
Valkey it comes from the primary's `INFO replication`. It is 0 when the
replica has the primary's offset, and otherwise the seconds since its last
acknowledgement. The field is left out when the member isn't replicating.

## The Valkey address: pyo-kv

A KV database's address, `<name>.<ns>.svc:6379` (`status.endpoints.primary`),
is pyo-kv. It runs as Deployment `<name>-kv`: two pods on different servers,
with a PodDisruptionBudget. It speaks the Redis protocol (RESP2 and RESP3)
on 6379 and, when `spec.rest` is on, the REST API on 8080 (Service
`<name>-rest`). Clients log in with the credentials secret's password, as
`default` or with no username, through AUTH or HELLO. The primary itself is
`<name>-primary`. pyo-kv's settings are listed at the top of
`crates/pyo-kv/src/main.rs`.

pyo-kv follows the Sentinels four times a second, so a new primary takes
traffic within about a quarter of a second after the Sentinels switch. A
request that was waiting on the old primary gets
`ERR pyo-kv lost the connection to the database, try again`.

| Kind of command | Where it runs | After a failover |
|---|---|---|
| Stateless commands | A small shared pool of connections. Pipelines keep their order. | The next command goes to the new primary. |
| MULTI/EXEC, WATCH | A primary connection held until EXEC, DISCARD or UNWATCH | EXEC answers `EXECABORT ... the primary changed`. Retry the transaction. |
| SUBSCRIBE, PSUBSCRIBE, SSUBSCRIBE | A primary connection of the client's own; the client connection becomes a tunnel to it | The connection closes. Client libraries reconnect and subscribe again. Messages published during the switch are lost. |
| Blocking commands (BLPOP, BRPOP, BLMOVE, BLMPOP, BZPOP*, BRPOPLPUSH, XREAD/XREADGROUP with BLOCK) | A primary connection of the client's own. If the client leaves, the connection closes, so nothing is popped for nobody. | The command gets the error above. |
| SELECT | Remembered per client and applied to whichever connection runs its commands | Unaffected |
| HELLO, AUTH, CLIENT SETNAME/GETNAME/ID/SETINFO, RESET, QUIT | Answered by pyo-kv | Unaffected |
| WAIT | Answered by pyo-kv from the replicas' offsets. It takes about 100–200ms, because the write went over a shared connection. | Unaffected |
| Refused with an error | MONITOR, CLIENT TRACKING and the other CLIENT subcommands, WAITAOF, CONFIG, DEBUG, SHUTDOWN, REPLICAOF, SLAVEOF, FAILOVER, ACL, CLUSTER, READONLY, READWRITE, MODULE, SAVE, BGSAVE, BGREWRITEAOF, MIGRATE, SENTINEL, SYNC, PSYNC, LATENCY, SLOWLOG | |

### Reads on read replicas

With read replicas, these requests go to a read replica:

- Over the Redis protocol, read-only commands.
- Over REST, single commands and pipelines that are entirely read-only.

A command counts as read-only when it is on the read-only token's list and
Valkey flags it `readonly` and not `blocking`. SCAN and its variants, and
EVALSHA_RO, are left out: a cursor from one instance means nothing on
another, and scripts loaded on the primary aren't on its replicas. Anything
else goes to the primary, including transactions and anything unknown.

- **Health.** Ten times a second, pyo-kv runs `INFO replication` on the
  primary and on each read replica. A replica gets reads only when its link
  is up and it is at most `PYO_KV_READ_MAX_LAG_MS` (1000) behind.
- **Fallback.** A replica that fails a request, or takes longer than
  `PYO_KV_READ_TIMEOUT_MS` (1000), is taken out of rotation, and the
  primary answers the same request.
- **Read-your-writes, per Redis connection.** After a connection writes,
  its reads go to the primary until a replica has the primary's offset from
  a check that started after the write. That takes about 50–100ms, because
  the checks run every 100ms. A pipeline's reads after a write in the same
  pipeline also go to the primary.
- **After any write, for everyone.** `PYO_KV_READ_AFTER_WRITE_MS` (default
  0, off) sends all reads to the primary for that long after any write.
- **REST.** The header `Pyo-Read-From: primary` sends a request to the
  primary. Responses carry `Pyo-Served-By: primary|replica`.

## MySQL

```yaml
spec: {engine: mysql, version: "8.4", instances: 3, storage: 10Gi, cpu: 500m, memory: 1Gi}
```

MOCO runs a MySQLCluster `<name>`: pods `<name>-0..N`, a primary and
semi-synchronous replicas with GTIDs, and MOCO's own failover. The CRD and
the controller hold it to what MOCO does:

- `instances` is 1, 3 or 5, and it only grows: 1 to 3 or 5, 3 to 5.
- `version` is `8.4` (or empty).
- `memory` is at least `1Gi`. It is the mysqld container's request and
  limit, and the settings come from it (ConfigMap `<name>-mysql`):

  | Setting | Under 2Gi | 2Gi to 4Gi | 4Gi and more |
  |---|---|---|---|
  | `innodb_buffer_pool_size` | 50% | 55% | 60% (whole 128Mi) |
  | `max_connections` | memory / 8Mi, 100 to 4000 (128 at 1Gi) | | |
  | `performance_schema` | OFF | ON | ON |
  | per-connection buffers, temporary tables | small (256K sorts, 16M tables) | small | MySQL's defaults |

  The redo log is a sixteenth of `storage` (64Mi to 2Gi), and binlogs are
  kept 2 days. MOCO's own defaults (70% buffer pool, 100000 connections,
  65536 open tables, the performance schema on, 1.6Gi of redo) run out of
  memory at 512Mi.
- No `readReplicas`, pooler or `rest`: the replicas serve reads at
  `endpoints.replicas`.

The app logs in with the credentials secret's `username` (default `app`)
and `password`, and has every privilege on the database `app` and nothing
else. The controller makes them, and puts them right when the password
changes. MOCO's own users (`moco-admin` and the rest) stay in its secret
`<name>`.

### Names

Pyo's MOCO has a name prefix (`deploy/images/moco`, patch 0002), and Pyo
sets it empty: a MySQLCluster made by this controller carries the
annotation `moco.cybozu.com/name-prefix: ""`, so everything MOCO makes for
it is named after the database: StatefulSet and headless Service
`<name>`, pods `<name>-<i>`, claims `mysql-data-<name>-<i>`, Services
`<name>-primary` and `<name>-replica`, Secrets `<name>`, `<name>-grpc` and
`my-cnf-<name>`, backup CronJob `backup-<name>` (Jobs
`backup-<name>-...`) and restore Job `restore-<name>`. Status, events,
logs and metrics use these names as they are.

| Status | Value |
|---|---|
| `endpoints.primary` | `<name>-primary.<ns>.svc.<domain>:3306` |
| `endpoints.replicas` | `<name>-replica.<ns>.svc.<domain>:3306`, with more than one instance |
| `members[]` | pod name, `node`, `role` (MOCO's `moco.cybozu.com/role`), `ready`, and `lagSeconds` for replicas (`SHOW REPLICA STATUS`) |

`<name>-primary` and `<name>-replica` are MOCO's Services.

Failover is MOCO's. Replication is lossless semi-sync (AFTER_SYNC, one
replica's acknowledgement, no fallback to asynchronous), so an
acknowledged commit is never lost. Two additions from the controller:

- **Errant replicas.** An old primary that crashed with a transaction
  written but not yet acknowledged has a transaction the new primary
  doesn't, and MOCO leaves it out of the cluster. The controller deletes
  that replica's claim and pod (event `ReplicaRebuilt`), and MOCO clones
  the primary into a fresh disk.
- **Grace period.** The pods' `terminationGracePeriodSeconds` is 60
  instead of MOCO's 300. A frozen mysqld can't act on SIGTERM, so it held
  its pod, and the failover, for 5 minutes after its liveness probe
  failed.

Logs: mysqld writes its error log to stdout as
`<RFC3339 UTC, microseconds> <thread> [System|Error|Warning|Note] [MY-<code>] [<subsystem>] <message>`,
with `log_error_verbosity` 3, so notes are included. MOCO's `agent` container
logs `<time> <pod> moco-agent <level>: "<message>" key="value" ...`,
including an access line for each probe. Backup and restore Jobs log JSON:
`{"level","ts","logger","msg"}`.

Backups and restoring take the same `spec.backup` and `spec.restore` as
Postgres, with these differences:

- A backup is a full dump plus the binlogs since the previous one, on
  `baseBackupSchedule` (its seconds field dropped), and the first one runs
  right away. It runs in a Job as ServiceAccount `<name>-backup`.
- A restore reaches only as far as the last backup's binlogs. There is no
  continuous archiving.
- `targetTime` empty restores up to when the restoring Database was made.
- Backups are stored at `<path>/<namespace>/<name>/<YYYYMMDD-hhmmss>/` in
  the bucket, as Postgres's (`bucketConfig.prefix`, patch 0003): the API
  passes the team's folder as `destination.path`. A restore reads them from
  its source's `path`. MOCO can't put them at the top of a bucket: an empty
  `path` is `moco/`.
- `retentionDays` (patch 0004): after each backup, its Job deletes the
  backups older than the newest one taken `retentionDays` or more before
  it, which a restore in that window still needs.
  `status.backup.firstRecoverabilityPoint` follows: the first backup seen,
  or the last one's time less `retentionDays` once that's later.
- `status.backup.lastArchivedWal` is the last backup's time, the same as
  `lastSuccessfulBackup`.

## Imports

`spec.import` copies data into a Postgres or MySQL database (`src/import.rs`);
a new `id` runs it again, and `status.import` has its `phase` (`Pending`,
`Running`, `Succeeded`, `Failed`), step or error, and times. The source's
password, or a dump's URL and token, are in the Secret `spec.import.secret`,
which the agent deletes once Pyo sees the import end.

- `bootstrap` (Postgres from a URL, as the database is made): CloudNativePG's
  `initdb.import` of type `microservice`, from the external cluster
  `import-source`. Two failed tries of its Job count as failed; a new `id`
  then deletes the Cluster, waits for its disks to go, and makes it again.
- Otherwise a Job `<name>-import-<id>`, once the database serves (and its
  extensions are on). It refuses a database that isn't empty, then streams
  the source in as the app's login: `pg_dump` or the file through
  `psql --single-transaction` (the database's own Postgres image), or
  `mysqldump` or the file through `mysql` (the MySQL image) with `DEFINER`
  clauses and SUPER-only statements removed, emptying the database again on
  a failure. A Postgres file is fetched first (curl, from the MySQL image) to
  a scratch volume the size of the file.

## Metrics

Each engine serves `/metrics` (Prometheus text) on its pods, found by
`pyo.dev/database=<name>` and `pyo.dev/component`. The agent reads them
(`crates/pyo-agent/src/dbmetrics.rs`).

| Engine | `pyo.dev/component` | Port (name) | What serves it |
|---|---|---|---|
| Postgres instances | `postgres` (read replicas: `read`) | 9187 (`metrics`) | CloudNativePG's exporter, with its default queries and ours from ConfigMap `<name>-monitoring` (`cnpg_pyo_replica_lag`, `cnpg_pyo_preload_loaded`) and `<name>-read-monitoring` (`cnpg_pyo_read_replica_lag`) |
| PgBouncer | `pooler`, `read-pooler` | 9127 (`metrics`) | CloudNativePG's pooler exporter |
| MySQL | `mysql` | 9104 (`mysqld-metrics`, MOCO's name) | MOCO's mysqld_exporter sidecar (container `mysqld-exporter`), collectors global_status, global_variables and slave_status |
| KV | `kv` (pyo-kv) | 9121 (`metrics`) | pyo-kv: its own counters, and every Valkey instance's INFO labelled `member=<pod>` |

pyo-kv reports the `valkey_*` series for every instance, and both pyo-kv pods
report the same ones. Keep one pod's. Its `pyo_kv_*` counters are per pod, so
sum them.

## Events and labels

The controller records events (events.k8s.io) on each Database:

| Reason | When |
|---|---|
| `Failover` (Warning) | The primary moved: "The primary moved from X to Y" |
| `Scaled` | `spec.instances` changed |
| `ReadReplicasAdded`, `ReadReplicasScaled`, `ReadReplicasRemoved` | Read replicas turned on, resized or turned off |
| `BackupStarted`, `BackupFinished`, `BackupFailed` (Warning) | A base backup (MySQL: a backup Job) started or ended |
| `RestoreStarted`, `RestoreFinished` | A database made from backups started restoring, or has a primary |
| `ReplicaRebuilt` (Warning) | MySQL: an errant replica's data was removed so that it copies the primary again |

Pods carry `pyo.dev/database=<name>`. MySQL pods carry
`pyo.dev/component=mysql`, and the controller labels MOCO's backup and
restore Job pods `pyo.dev/component=backup` or `restore`. Pooler pods also
carry `pyo.dev/component=pooler`, read-pooler pods
`pyo.dev/component=read-pooler`, and read-replica pods
`pyo.dev/component=read`. The names in `status.members` and
`status.readReplicas.members` are pod names.

## Build

From the repository root (`scripts/pyo-up.sh` builds both with the rest of
Pyo's images):

```sh
docker build -f deploy/images/pyo.Dockerfile --target db-controller -t <registry>/db-controller:<tag> .
docker build -f deploy/images/pyo.Dockerfile --target kv -t <registry>/kv:<tag> .
```
