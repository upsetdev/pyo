# MOCO, patched

Pyo's MySQL runs on [MOCO](https://github.com/cybozu-go/moco)
(cybozu-go/moco, Apache-2.0) with a few patches. This directory builds the
two images whose code they change:

| Image | Target | What it is |
|---|---|---|
| `pyo/mysql-operator` | `operator` | `moco-controller`, the operator |
| `pyo/mysql-backup` | `backup` | `moco-backup`, the backup and restore Jobs |

`moco-agent` (the sidecar in each MySQL pod) isn't affected and stays
upstream's image, pinned in `crates/pyo-proto/system/databases/moco.yaml`.

- Upstream: tag `v0.38.0`, commit
  `acb2716d1ef13a8953dd18d55cf230c0edf90f04` (`MOCO_TAG` and
  `MOCO_COMMIT` in `moco.Dockerfile`; the build fails if the tag doesn't
  point at that commit).
- `moco.Dockerfile` clones upstream at the tag, checks the commit, applies
  `patches/*.patch` in order and builds with upstream's own stages.
- Built with Pyo's other images: `scripts/pyo-up.sh` (local, into
  `127.0.0.1:5005/pyo/mysql-operator` and `mysql-backup`),
  `.github/workflows/release.yml` and `scripts/release-manifest.ts`
  (`mysqlOperator`, `mysqlBackup`). The installation passes them on as
  `PYO_MYSQL_OPERATOR_IMAGE` and `PYO_MYSQL_BACKUP_IMAGE`, and the
  databases bundle puts them in for `__MYSQL_OPERATOR_IMAGE__` and
  `__MYSQL_BACKUP_IMAGE__` in `moco.yaml`.

```sh
docker build -f deploy/images/moco/moco.Dockerfile --target operator -t <registry>/mysql-operator:<tag> deploy/images/moco
docker build -f deploy/images/moco/moco.Dockerfile --target backup -t <registry>/mysql-backup:<tag> deploy/images/moco
```

## The patches

Each one keeps upstream's behaviour unless it's turned on, and is written
to go upstream as its own pull request (with tests, the regenerated CRDs
and the docs). None has been sent yet.

1. **`0001` Delete only the cluster's own old ConfigMaps.** A bug fix:
   old my.cnf and slow-log ConfigMaps were found by name prefix alone, so a
   cluster could delete another's whose name starts with its name
   (`moco-slow-log-agent-config-a` matches cluster `ab`). It now lists only
   ConfigMaps with the cluster's labels. Needed before the next patch, which
   makes such names shorter.
2. **`0002` A configurable name prefix.** `moco-controller --name-prefix`
   (default `moco-`) replaces `moco-` in the names MOCO gives a cluster's
   parts: StatefulSet, pods, Services, the users' and my.cnf Secrets,
   ConfigMaps, the gRPC Secret, PodDisruptionBudget, ServiceAccount, backup
   CronJob, restore Job and their Roles. The prefix is recorded in the
   cluster's `moco.cybozu.com/name-prefix` annotation when it's created and
   can't change after (a StatefulSet can't be renamed); a cluster without
   it, any made before, keeps `moco-`. The Certificate and Secret in the
   operator's namespace keep their names. The restore Job reads the target
   cluster to find its pod instead of assuming the default name. Pyo sets
   `--name-prefix=`, so database `shop` has pods `shop-0`, ... and Services
   `shop-primary` and `shop-replica`.
3. **`0003` A configurable backup key prefix.** `bucketConfig.prefix`
   (`moco-backup --prefix`, default `moco`) puts backups under
   `<prefix>/<namespace>/<name>/` instead of `moco/<namespace>/<name>/`. A
   restore reads them from its own `jobConfig.bucketConfig.prefix`. Pyo
   passes the database's `destination.path`, so MySQL backups sit in the
   team's folder beside Postgres's.
4. **`0004` Backup retention.** BackupPolicy `spec.retentionDays`
   (`moco-backup backup --retention-days`, default 0: keep everything).
   After each successful backup, the Job deletes every backup older than
   the newest one taken at or before `retentionDays` ago, which a restore
   to any moment in that window still needs. A failure to delete is logged
   and doesn't fail the backup. `Bucket` gains `Delete` (S3, GCS, Azure).

## Moving to a new upstream tag

1. Clone upstream and check out the new tag on a branch.
2. `git am deploy/images/moco/patches/*.patch` (from a clone at the old
   tag, `git format-patch` reproduces them). Resolve conflicts, then
   regenerate what upstream generates: `make manifests generate apidoc`.
3. Run the unit tests and the envtest suites: `go test ./api/... ./backup/
   ./controllers/...` with `ENVTEST_KUBERNETES_VERSION` and
   `ENVTEST_ASSETS_DIR` set as upstream's Makefile does.
4. `git format-patch --no-signature --zero-commit <tag>..` into `patches/`
   (replacing the old files), and update `MOCO_TAG` and `MOCO_COMMIT` here
   and in `moco.Dockerfile`, with the base images' digests from upstream's
   Dockerfile at that tag.
5. Update `moco.yaml` from the new release manifest as its header says,
   keeping the patches' CRD fields (`bucketConfig.prefix`,
   `retentionDays`), `--name-prefix=` and the image placeholders, and the
   pinned `moco-agent`, fluent-bit, mysqld_exporter and MySQL images.
6. A patch that upstream has merged is dropped.
