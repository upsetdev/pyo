//! Backups of a volume or a database, as a Job in the service's namespace.
//!
//! A database is dumped first by its own tool (pg_dump, mysqldump,
//! mariadb-dump, mongodump, redis-cli/valkey-cli --rdb), from the service's own image and env, into a
//! scratch volume; then restic backs that up. A volume is mounted read-only
//! and backed up as it is (Kubernetes puts the Job on the volume's server).
//! restic encrypts everything before it leaves; the repository's password
//! and storage keys reach the Job in a Secret that's deleted afterwards.

use std::{
    collections::HashMap,
    sync::{Arc, LazyLock, Mutex},
    time::Duration,
};

use k8s_openapi::api::{
    apps::v1::{Deployment, StatefulSet},
    batch::v1::Job,
    core::v1::{Pod, PodSpec, Secret},
};
use kube::{
    Api,
    api::{DeleteParams, ListParams, LogParams, Patch, PatchParams, PostParams},
};
use pyo_proto::{BackupSpec, CommandResult};
use serde_json::{Value, json};

use crate::{Agent, MANAGER, failed};

/// One backup at a time per repository: two `restic init`s on a new one
/// would leave mismatched keys, and a prune would fight a running backup.
static REPOS: LazyLock<Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>> =
    LazyLock::new(Default::default);

pub async fn run(agent: &Agent, spec: BackupSpec) -> CommandResult {
    let lock = REPOS
        .lock()
        .unwrap()
        .entry(spec.repository.clone())
        .or_default()
        .clone();
    let _turn = lock.lock().await;
    match backup(&agent.kube, &spec).await {
        Ok(summary) => CommandResult {
            ok: true,
            message: Some("Backed up".into()),
            logs: vec![],
            data: Some(summary),
        },
        Err(e) => failed(format!("{e:#}")),
    }
}

fn job_name(spec: &BackupSpec) -> String {
    format!(
        "pyo-backup-{}",
        spec.backup_id.replace('_', "-").to_lowercase()
    )
}

/// The running service's image and env Secret, for the dump step.
async fn service_pod(client: &kube::Client, spec: &BackupSpec) -> anyhow::Result<PodSpec> {
    let ns = &spec.service.namespace;
    let template = if spec.service.workload_kind == "StatefulSet" {
        Api::<StatefulSet>::namespaced(client.clone(), ns)
            .get(&spec.service.name)
            .await?
            .spec
            .map(|s| s.template)
    } else {
        Api::<Deployment>::namespaced(client.clone(), ns)
            .get(&spec.service.name)
            .await?
            .spec
            .map(|s| s.template)
    };
    template
        .and_then(|t| t.spec)
        .ok_or_else(|| anyhow::anyhow!("{} isn't deployed", spec.service.name))
}

fn dump_script(engine: &str, host: &str) -> anyhow::Result<String> {
    Ok(match engine {
        "postgres" => format!(
            r#"PGPASSWORD="$POSTGRES_PASSWORD" pg_dump -h {host} -U "${{POSTGRES_USER:-postgres}}" -d "${{POSTGRES_DB:-${{POSTGRES_USER:-postgres}}}}" -Fc -f /dump/postgres.dump"#
        ),
        "mysql" => format!(
            r#"mysqldump -h {host} -uroot -p"$MYSQL_ROOT_PASSWORD" --all-databases --single-transaction --routines --events > /dump/mysql.sql"#
        ),
        "mariadb" => format!(
            r#"mariadb-dump -h {host} -uroot -p"$MARIADB_ROOT_PASSWORD" --all-databases --single-transaction --routines --events > /dump/mariadb.sql"#
        ),
        "mongo" => format!(
            r#"mongodump --uri "mongodb://$MONGO_INITDB_ROOT_USERNAME:$MONGO_INITDB_ROOT_PASSWORD@{host}:27017/?authSource=admin" --archive=/dump/mongo.archive --gzip"#
        ),
        // Redis and Valkey: an RDB snapshot streamed from the live server.
        "redis" => format!(
            r#"CLI=$(command -v valkey-cli || command -v redis-cli); PASS="${{VALKEY_PASSWORD:-${{REDIS_PASSWORD:-}}}}"; if [ -n "$PASS" ]; then export REDISCLI_AUTH="$PASS"; fi; "$CLI" -h {host} --rdb /dump/dump.rdb"#
        ),
        other => anyhow::bail!("can't dump a {other:?} database"),
    })
}

/// restic: make the repository if it's new, back up, then keep the newest
/// `keep` snapshots of this target. The summary line is what we report.
fn restic_script(spec: &BackupSpec, path: &str) -> String {
    let tags: String = spec
        .tags
        .iter()
        .map(|t| format!(" --tag '{}'", t.replace('\'', "")))
        .collect();
    // A volume's snapshots by its id alone: it keeps them when it's
    // attached to another service.
    let filter = match spec.tags.iter().find(|t| t.starts_with("volume=")) {
        Some(v) => v.replace('\'', ""),
        None => spec.tags.join(",").replace('\'', ""),
    };
    format!(
        "set -e\n\
         restic cat config >/dev/null 2>&1 || restic init || true\n\
         restic backup {path} --host pyo{tags} --json | grep '\"message_type\":\"summary\"'\n\
         restic forget --host pyo --tag '{filter}' --keep-last {keep} --prune --quiet || echo 'prune skipped: repository busy'\n",
        keep = spec.keep.max(1),
    )
}

pub(crate) async fn backup(client: &kube::Client, spec: &BackupSpec) -> anyhow::Result<Value> {
    let ns = spec.service.namespace.as_str();
    let name = job_name(spec);
    let secrets = Api::<Secret>::namespaced(client.clone(), ns);
    let jobs = Api::<Job>::namespaced(client.clone(), ns);
    let labels =
        json!({ "app.kubernetes.io/managed-by": MANAGER, "pyo/backup-id": spec.backup_id });

    secrets
        .patch(
            &name,
            &PatchParams::apply(MANAGER).force(),
            &Patch::Apply(json!({
                "apiVersion": "v1", "kind": "Secret",
                "metadata": { "name": name, "labels": labels },
                "type": "Opaque",
                "stringData": {
                    "RESTIC_REPOSITORY": spec.repository,
                    "RESTIC_PASSWORD": spec.password,
                    "AWS_ACCESS_KEY_ID": spec.access_key,
                    "AWS_SECRET_ACCESS_KEY": spec.secret_key,
                    "AWS_DEFAULT_REGION": spec.region,
                    "DB_HOST": spec.connection.as_ref().map(|c| c.host.clone()).unwrap_or_default(),
                    "DB_PORT": spec.connection.as_ref().map(|c| c.port.to_string()).unwrap_or_default(),
                    "DB_NAME": spec.connection.as_ref().map(|c| c.database.clone()).unwrap_or_default(),
                    "DB_USER": spec.connection.as_ref().map(|c| c.user.clone()).unwrap_or_default(),
                    "DB_PASSWORD": spec.connection.as_ref().map(|c| c.password.clone()).unwrap_or_default(),
                },
            })),
        )
        .await?;

    let restic_env = json!([{ "secretRef": { "name": name } }]);
    let (volumes, init, path) = if let Some(conn) =
        spec.connection.as_ref().filter(|_| spec.kind == "database")
    {
        // A database set up for backups: its read-only user, the engine's tools.
        (
            json!([{ "name": "dump", "emptyDir": {} }]),
            json!([{
                "name": "dump",
                "image": pyo_proto::db_tools_image(&conn.engine),
                "command": ["sh", "-c", connection_dump_script(&conn.engine)?],
                "envFrom": [{ "secretRef": { "name": name } }],
                "volumeMounts": [{ "name": "dump", "mountPath": "/dump" }],
            }]),
            "/dump",
        )
    } else if spec.kind == "database" {
        let pod = service_pod(client, spec).await?;
        let svc = pod
            .containers
            .first()
            .ok_or_else(|| anyhow::anyhow!("no container"))?;
        let script = dump_script(&spec.engine, &spec.service.name)?;
        (
            json!([{ "name": "dump", "emptyDir": {} }]),
            json!([{
                "name": "dump",
                "image": svc.image,
                "command": ["sh", "-c", script],
                "envFrom": svc.env_from,
                "volumeMounts": [{ "name": "dump", "mountPath": "/dump" }],
            }]),
            "/dump",
        )
    } else {
        (
            json!([{ "name": "data", "persistentVolumeClaim": { "claimName": spec.claim, "readOnly": true } }]),
            json!([]),
            "/data",
        )
    };
    let mount = if spec.kind == "database" {
        json!({ "name": "dump", "mountPath": "/dump" })
    } else {
        json!({ "name": "data", "mountPath": "/data", "readOnly": true })
    };

    let job = json!({
        "apiVersion": "batch/v1", "kind": "Job",
        "metadata": { "name": name, "labels": labels },
        "spec": {
            "backoffLimit": 0,
            "activeDeadlineSeconds": 3600,
            "ttlSecondsAfterFinished": 600,
            "template": {
                "metadata": { "labels": labels },
                "spec": {
                    "restartPolicy": "Never",
                    // A volume on a GPU server kept for GPU work.
                    "tolerations": [pyo_proto::gpu_toleration()],
                    "volumes": volumes,
                    "initContainers": init,
                    "containers": [{
                        "name": "restic",
                        "image": pyo_proto::RESTIC_IMAGE,
                        "command": ["sh", "-c", restic_script(spec, path)],
                        "envFrom": restic_env,
                        "volumeMounts": [mount],
                        "resources": { "requests": { "cpu": "100m", "memory": "128Mi" }, "limits": { "memory": "1Gi" } },
                    }],
                },
            },
        },
    });
    let _ = jobs.delete(&name, &DeleteParams::background()).await;
    jobs.create(&PostParams::default(), &serde_json::from_value(job)?)
        .await?;

    let outcome = wait(&jobs, &name).await;
    let logs = pod_logs(client, ns, &spec.backup_id).await;
    let _ = jobs.delete(&name, &DeleteParams::background()).await;
    let _ = secrets.delete(&name, &DeleteParams::default()).await;
    outcome.map_err(|e| {
        anyhow::anyhow!(
            "{e}{}",
            if logs.is_empty() {
                String::new()
            } else {
                format!(": {}", last_lines(&logs, 3))
            }
        )
    })?;

    let summary = logs
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .find(|v| v["message_type"] == json!("summary"))
        .ok_or_else(|| anyhow::anyhow!("restic didn't report a summary"))?;
    Ok(json!({
        "snapshotId": summary["snapshot_id"],
        "bytes": summary["total_bytes_processed"],
        "added": summary["data_added"],
        "files": summary["total_files_processed"],
    }))
}

// ── Snapshots and restores of a volume ───────────────────────────────────

/// Only ids restic could have made (hex), so they're safe in a script.
fn hex_id(s: &str) -> bool {
    (8..=64).contains(&s.len()) && s.chars().all(|c| c.is_ascii_hexdigit())
}

/// Only volume ids Pyo makes (`vol_` and [a-z0-9_-]).
fn safe_tag(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// One restic Job in `ns` with the repository's keys in a Secret (deleted
/// afterwards), optionally mounting claim `claim` at /data. Its output.
async fn restic_job(
    client: &kube::Client,
    ns: &str,
    id: &str,
    repo: &pyo_proto::ResticRepo,
    script: &str,
    claim: Option<&str>,
    secs: u64,
) -> anyhow::Result<String> {
    let name = format!("pyo-backup-{}", id.replace('_', "-").to_lowercase());
    let secrets = Api::<Secret>::namespaced(client.clone(), ns);
    let jobs = Api::<Job>::namespaced(client.clone(), ns);
    let labels = json!({ "app.kubernetes.io/managed-by": MANAGER, "pyo/backup-id": id });
    secrets
        .patch(
            &name,
            &PatchParams::apply(MANAGER).force(),
            &Patch::Apply(json!({
                "apiVersion": "v1", "kind": "Secret",
                "metadata": { "name": name, "labels": labels },
                "type": "Opaque",
                "stringData": {
                    "RESTIC_REPOSITORY": repo.repository,
                    "RESTIC_PASSWORD": repo.password,
                    "AWS_ACCESS_KEY_ID": repo.access_key,
                    "AWS_SECRET_ACCESS_KEY": repo.secret_key,
                    "AWS_DEFAULT_REGION": repo.region,
                },
            })),
        )
        .await?;
    let (volumes, mounts) = match claim {
        Some(c) => (
            json!([{ "name": "data", "persistentVolumeClaim": { "claimName": c } }]),
            json!([{ "name": "data", "mountPath": "/data" }]),
        ),
        None => (json!([]), json!([])),
    };
    let job = json!({
        "apiVersion": "batch/v1", "kind": "Job",
        "metadata": { "name": name, "labels": labels },
        "spec": {
            "backoffLimit": 0,
            "activeDeadlineSeconds": secs,
            "ttlSecondsAfterFinished": 600,
            "template": {
                "metadata": { "labels": labels },
                "spec": {
                    "restartPolicy": "Never",
                    "tolerations": [pyo_proto::gpu_toleration()],
                    "volumes": volumes,
                    "containers": [{
                        "name": "restic",
                        "image": pyo_proto::RESTIC_IMAGE,
                        "command": ["sh", "-c", script],
                        "envFrom": [{ "secretRef": { "name": name } }],
                        "volumeMounts": mounts,
                        "resources": { "requests": { "cpu": "100m", "memory": "128Mi" }, "limits": { "memory": "1Gi" } },
                    }],
                },
            },
        },
    });
    let _ = jobs.delete(&name, &DeleteParams::background()).await;
    let made = jobs
        .create(&PostParams::default(), &serde_json::from_value(job)?)
        .await;
    let outcome = match made {
        Ok(_) => wait_for(&jobs, &name, secs + 60).await,
        Err(e) => Err(format!("couldn't start the job: {e}")),
    };
    let logs = pod_logs(client, ns, id).await;
    let _ = jobs.delete(&name, &DeleteParams::background()).await;
    let _ = secrets.delete(&name, &DeleteParams::default()).await;
    outcome.map_err(|e| {
        anyhow::anyhow!(
            "{e}{}",
            if logs.trim().is_empty() {
                String::new()
            } else {
                format!(": {}", last_lines(&logs, 3))
            }
        )
    })?;
    Ok(logs)
}

/// `restic snapshots --json` of one volume, as the API wants them: newest
/// first, each with `id`, `shortId`, `time` (ms) and `sizeBytes`.
fn parse_snapshots(logs: &str) -> anyhow::Result<Vec<Value>> {
    let line = logs
        .lines()
        .find(|l| l.trim_start().starts_with('['))
        .ok_or_else(|| anyhow::anyhow!("restic didn't list any snapshots"))?;
    let list: Vec<Value> = serde_json::from_str(line.trim())?;
    let mut out: Vec<Value> = list
        .iter()
        .map(|s| {
            let time = s["time"]
                .as_str()
                .and_then(|t| t.parse::<k8s_openapi::jiff::Timestamp>().ok())
                .map(|t| t.as_millisecond())
                .unwrap_or(0);
            json!({
                "id": s["id"],
                "shortId": s["short_id"],
                "time": time,
                "sizeBytes": s["summary"]["total_bytes_processed"],
            })
        })
        .collect();
    out.sort_by_key(|s| std::cmp::Reverse(s["time"].as_i64().unwrap_or(0)));
    Ok(out)
}

/// ListSnapshots: the volume's snapshots (none while the repository doesn't
/// exist yet). Without a lock: a prune's would hide them all.
pub async fn snapshots(client: &kube::Client, spec: pyo_proto::SnapshotsSpec) -> CommandResult {
    if !safe_tag(&spec.volume_id) || !safe_tag(&spec.request_id) {
        return failed("Not a volume id.");
    }
    let script = format!(
        "restic cat config --no-lock >/dev/null 2>&1 || {{ echo '[]'; exit 0; }}
         restic snapshots --no-lock --host pyo --tag 'volume={}' --json
",
        spec.volume_id
    );
    let run = async {
        parse_snapshots(
            &restic_job(
                client,
                &spec.namespace,
                &spec.request_id,
                &spec.repo,
                &script,
                None,
                300,
            )
            .await?,
        )
    };
    match run.await {
        Ok(list) => CommandResult {
            ok: true,
            message: Some(format!("{} snapshot(s)", list.len())),
            logs: vec![],
            data: Some(json!({ "snapshots": list })),
        },
        Err(e) => failed(format!("{e:#}")),
    }
}

/// The script deleting one of a volume's snapshots: only one of its own;
/// one not there (deleted already) is done.
fn forget_script(volume_id: &str, snapshot_id: &str) -> String {
    format!(
        r#"set -e
restic cat config >/dev/null 2>&1 || {{ echo 'PYO gone'; exit 0; }}
restic snapshots --no-lock --host pyo --tag 'volume={volume_id}' --json | grep -q '"id":"{snapshot_id}' || {{ echo 'PYO gone'; exit 0; }}
restic forget '{snapshot_id}'
echo 'PYO forgot'
"#
    )
}

/// ForgetSnapshot: delete one of a volume's snapshots, taking turns with
/// the repository's backups and restores. Its data is pruned afterwards in
/// the background (in turn too), so the answer doesn't wait for a prune of
/// the whole repository; a prune that doesn't happen (the agent restarted)
/// is done by the next backup, which prunes as it goes.
pub async fn forget(client: &kube::Client, spec: pyo_proto::ForgetSpec) -> CommandResult {
    if !safe_tag(&spec.volume_id) || !safe_tag(&spec.request_id) || !hex_id(&spec.snapshot_id) {
        return failed("Not a volume or snapshot id.");
    }
    let lock = REPOS
        .lock()
        .unwrap()
        .entry(spec.repo.repository.clone())
        .or_default()
        .clone();
    let turn = lock.clone().lock_owned().await;
    let out = restic_job(
        client,
        &spec.namespace,
        &spec.request_id,
        &spec.repo,
        &forget_script(&spec.volume_id, &spec.snapshot_id),
        None,
        600,
    )
    .await;
    drop(turn);
    let short: String = spec.snapshot_id.chars().take(8).collect();
    match out {
        Ok(logs) if logs.lines().any(|l| l.trim() == "PYO gone") => {
            crate::ok(&format!("Snapshot {short} is already gone"))
        }
        Ok(_) => {
            let client = client.clone();
            tokio::spawn(async move {
                let _turn = lock.lock_owned().await;
                let id = format!("{}-prune", spec.request_id);
                match restic_job(
                    &client,
                    &spec.namespace,
                    &id,
                    &spec.repo,
                    "restic prune --quiet",
                    None,
                    3600,
                )
                .await
                {
                    Ok(_) => {
                        tracing::info!(volume = %spec.volume_id, "pruned the repository after deleting a snapshot")
                    }
                    Err(e) => {
                        tracing::warn!(volume = %spec.volume_id, error = %format!("{e:#}"), "couldn't prune the repository; the next backup does")
                    }
                }
            });
            crate::ok(&format!("Deleted snapshot {short}"))
        }
        Err(e) => failed(format!("{e:#}")),
    }
}

/// The restore Job's script: the snapshot (only one of this volume's) is
/// restored into a folder beside the data, on the same disk; only once
/// that's done is the data swapped for it, by renames, and the old data
/// removed. A failed restore leaves the data as it was.
fn restore_script(volume_id: &str, snapshot_id: &str, run: &str) -> String {
    format!(
        r#"set -e
D=/data; S="$D/.pyo-restore-{run}"; O="$D/.pyo-previous-{run}"
restic snapshots --no-lock --host pyo --tag 'volume={volume_id}' --json | grep -q '"id":"{snapshot_id}' || {{ echo "That snapshot isn't one of this volume's." >&2; exit 3; }}
rm -rf "$S" "$O"; mkdir -p "$S" "$O"
restic restore '{snapshot_id}:/data' --target "$S" --no-lock || {{ rm -rf "$S" "$O"; exit 4; }}
for f in "$D"/* "$D"/.[!.]* "$D"/..?*; do
  [ -e "$f" ] || [ -L "$f" ] || continue
  case "${{f##*/}}" in lost+found|.pyo-restore-{run}|.pyo-previous-{run}) continue;; esac
  mv "$f" "$O"/
done
for f in "$S"/* "$S"/.[!.]* "$S"/..?*; do
  [ -e "$f" ] || [ -L "$f" ] || continue
  case "${{f##*/}}" in lost+found) continue;; esac
  mv "$f" "$D"/
done
rm -rf "$S" "$O"
echo restored
"#
    )
}

/// The restore Job alone (the caller has the volume to itself).
pub async fn restore_job(
    client: &kube::Client,
    spec: &pyo_proto::RestoreSpec,
) -> anyhow::Result<()> {
    if !safe_tag(&spec.volume_id) || !safe_tag(&spec.restore_id) || !hex_id(&spec.snapshot_id) {
        anyhow::bail!("Not a volume or snapshot id.");
    }
    let lock = REPOS
        .lock()
        .unwrap()
        .entry(spec.repo.repository.clone())
        .or_default()
        .clone();
    let _turn = lock.lock().await;
    let run = spec.restore_id.replace('_', "-").to_lowercase();
    restic_job(
        client,
        &spec.namespace,
        &spec.restore_id,
        &spec.repo,
        &restore_script(&spec.volume_id, &spec.snapshot_id, &run),
        Some(&spec.claim),
        3600,
    )
    .await?;
    Ok(())
}

/// A dump as the read-only backup user, from DB_* in the Job's env.
fn connection_dump_script(engine: &str) -> anyhow::Result<String> {
    Ok(match engine {
        "postgres" => r#"PGPASSWORD="$DB_PASSWORD" pg_dump -h "$DB_HOST" -p "$DB_PORT" -U "$DB_USER" -d "$DB_NAME" -Fc -f /dump/postgres.dump"#.into(),
        "mysql" | "mariadb" => {
            let tool = if engine == "mariadb" { "mariadb-dump" } else { "mysqldump" };
            format!(
                r#"if [ -n "$DB_NAME" ]; then WHAT="--databases $DB_NAME"; else WHAT=--all-databases; fi; {tool} -h "$DB_HOST" -P "$DB_PORT" -u "$DB_USER" -p"$DB_PASSWORD" --single-transaction --routines --events --no-tablespaces $WHAT > /dump/{engine}.sql"#
            )
        }
        "mongo" => r#"mongodump --uri "mongodb://$DB_USER:$DB_PASSWORD@$DB_HOST:$DB_PORT/?authSource=admin" ${DB_NAME:+--db "$DB_NAME"} --archive=/dump/mongo.archive --gzip"#.into(),
        "redis" => r#"REDISCLI_AUTH="$DB_PASSWORD" valkey-cli --user "$DB_USER" -h "$DB_HOST" -p "$DB_PORT" --rdb /dump/dump.rdb"#.into(),
        other => anyhow::bail!("can't dump a {other:?} database"),
    })
}

/// Makes (or resets) the read-only backup user, as the admin login. Each
/// engine gets only what its dump needs.
fn setup_script(engine: &str) -> anyhow::Result<String> {
    let u = pyo_proto::BACKUP_DB_USER;
    Ok(match engine {
        "postgres" => format!(
            r#"set -e
export PGPASSWORD="$ADMIN_PASSWORD"
psql -h "$DB_HOST" -p "$DB_PORT" -U "$ADMIN_USER" -d "$DB_NAME" -v ON_ERROR_STOP=1 -v ro="$RO_PASSWORD" <<'SQL'
SELECT 'CREATE ROLE {u} LOGIN' WHERE NOT EXISTS (SELECT FROM pg_roles WHERE rolname = '{u}')\gexec
ALTER ROLE {u} WITH LOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE PASSWORD :'ro';
SELECT format('GRANT CONNECT ON DATABASE %I TO {u}', current_database())\gexec
SELECT CASE WHEN current_setting('server_version_num')::int >= 140000
  THEN 'GRANT pg_read_all_data TO {u}'
  ELSE 'GRANT USAGE ON SCHEMA public TO {u}; GRANT SELECT ON ALL TABLES IN SCHEMA public TO {u}; GRANT SELECT ON ALL SEQUENCES IN SCHEMA public TO {u}' END\gexec
SQL
echo ok"#
        ),
        "mysql" | "mariadb" => {
            let cli = if engine == "mariadb" {
                "mariadb"
            } else {
                "mysql"
            };
            format!(
                r#"set -e
{cli} -h "$DB_HOST" -P "$DB_PORT" -u "$ADMIN_USER" -p"$ADMIN_PASSWORD" -e "CREATE USER IF NOT EXISTS '{u}'@'%' IDENTIFIED BY '$RO_PASSWORD'; ALTER USER '{u}'@'%' IDENTIFIED BY '$RO_PASSWORD'; GRANT SELECT, SHOW VIEW, TRIGGER, LOCK TABLES, EVENT ON *.* TO '{u}'@'%'; FLUSH PRIVILEGES;"
echo ok"#
            )
        }
        "mongo" => format!(
            r#"set -e
mongosh --quiet "mongodb://$ADMIN_USER:$ADMIN_PASSWORD@$DB_HOST:$DB_PORT/?authSource=admin" --eval '
const a = db.getSiblingDB("admin"), pw = process.env.RO_PASSWORD;
if (a.getUser("{u}")) a.updateUser("{u}", {{ pwd: pw, roles: [{{ role: "backup", db: "admin" }}] }});
else a.createUser({{ user: "{u}", pwd: pw, roles: [{{ role: "backup", db: "admin" }}] }});'
echo ok"#
        ),
        "redis" => format!(
            r#"set -e
export REDISCLI_AUTH="$ADMIN_PASSWORD"
if [ -n "$ADMIN_USER" ] && [ "$ADMIN_USER" != default ]; then U="--user $ADMIN_USER"; else U=""; fi
valkey-cli $U -h "$DB_HOST" -p "$DB_PORT" ACL SETUSER {u} reset on ">$RO_PASSWORD" resetkeys resetchannels -@all +ping +info +sync +psync +replconf | grep -q OK
echo ok"#
        ),
        other => anyhow::bail!("can't set up a {other:?} database"),
    })
}

/// DbSetup: run the setup script in a short Job and report how it went.
pub async fn setup(
    agent: &Agent,
    namespace: &str,
    admin: &pyo_proto::DbConnection,
    password: &str,
) -> CommandResult {
    match run_setup(agent, namespace, admin, password).await {
        Ok(()) => crate::ok("Backup user ready"),
        Err(e) => failed(format!("{e:#}")),
    }
}

async fn run_setup(
    agent: &Agent,
    ns: &str,
    admin: &pyo_proto::DbConnection,
    password: &str,
) -> anyhow::Result<()> {
    let client = agent.kube.clone();
    // External databases' Jobs run in a namespace of their own.
    let _ = Api::<k8s_openapi::api::core::v1::Namespace>::all(client.clone())
        .patch(
            ns,
            &PatchParams::apply(MANAGER),
            &Patch::Apply(json!({ "apiVersion": "v1", "kind": "Namespace", "metadata": { "name": ns, "labels": { "app.kubernetes.io/managed-by": MANAGER } } })),
        )
        .await;
    crate::scope::ensure_managed(&client, ns).await?;
    let id = format!("setup-{:x}", rand_suffix());
    let name = format!("pyo-backup-{id}");
    let labels = json!({ "app.kubernetes.io/managed-by": MANAGER, "pyo/backup-id": id });
    let secrets = Api::<Secret>::namespaced(client.clone(), ns);
    let jobs = Api::<Job>::namespaced(client.clone(), ns);
    secrets
        .patch(&name, &PatchParams::apply(MANAGER).force(), &Patch::Apply(json!({
            "apiVersion": "v1", "kind": "Secret", "metadata": { "name": name, "labels": labels }, "type": "Opaque",
            "stringData": {
                "DB_HOST": admin.host, "DB_PORT": admin.port.to_string(), "DB_NAME": admin.database,
                "ADMIN_USER": admin.user, "ADMIN_PASSWORD": admin.password, "RO_PASSWORD": password,
            },
        })))
        .await?;
    let job = json!({
        "apiVersion": "batch/v1", "kind": "Job",
        "metadata": { "name": name, "labels": labels },
        "spec": { "backoffLimit": 0, "activeDeadlineSeconds": 120, "ttlSecondsAfterFinished": 300, "template": {
            "metadata": { "labels": labels },
            "spec": { "restartPolicy": "Never", "containers": [{
                "name": "setup",
                "image": pyo_proto::db_tools_image(&admin.engine),
                "command": ["sh", "-c", setup_script(&admin.engine)?],
                "envFrom": [{ "secretRef": { "name": name } }],
            }] },
        } },
    });
    jobs.create(&PostParams::default(), &serde_json::from_value(job)?)
        .await?;
    let outcome = wait_for(&jobs, &name, 130).await;
    let logs = pod_logs(&agent.kube, ns, &id).await;
    let _ = jobs.delete(&name, &DeleteParams::background()).await;
    let _ = secrets.delete(&name, &DeleteParams::default()).await;
    outcome.map_err(|e| {
        anyhow::anyhow!(
            "{}",
            if logs.trim().is_empty() {
                e
            } else {
                last_lines(&logs, 2)
            }
        )
    })
}

fn rand_suffix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

async fn wait(jobs: &Api<Job>, name: &str) -> Result<(), String> {
    wait_for(jobs, name, 3700).await
}

async fn wait_for(jobs: &Api<Job>, name: &str, secs: u64) -> Result<(), String> {
    for _ in 0..(secs / 5) {
        tokio::time::sleep(Duration::from_secs(5)).await;
        let Ok(job) = jobs.get(name).await else {
            continue;
        };
        let st = job.status.unwrap_or_default();
        if st.succeeded.unwrap_or(0) > 0 {
            return Ok(());
        }
        if st.failed.unwrap_or(0) > 0 {
            return Err("the job failed".into());
        }
    }
    Err("the job didn't finish in time".into())
}

/// Every container's log of the Job's pod, init containers first.
async fn pod_logs(client: &kube::Client, ns: &str, backup_id: &str) -> String {
    let pods = Api::<Pod>::namespaced(client.clone(), ns);
    let Ok(list) = pods
        .list(&ListParams::default().labels(&format!("pyo/backup-id={backup_id}")))
        .await
    else {
        return String::new();
    };
    let mut out = String::new();
    for pod in list.items {
        let name = pod.metadata.name.clone().unwrap_or_default();
        let spec = pod.spec.unwrap_or_default();
        for c in spec
            .init_containers
            .unwrap_or_default()
            .iter()
            .chain(spec.containers.iter())
        {
            if let Ok(l) = pods
                .logs(
                    &name,
                    &LogParams {
                        container: Some(c.name.clone()),
                        tail_lines: Some(200),
                        ..Default::default()
                    },
                )
                .await
            {
                out.push_str(&l);
            }
        }
    }
    out
}

fn last_lines(s: &str, n: usize) -> String {
    let lines: Vec<&str> = s.lines().filter(|l| !l.trim().is_empty()).collect();
    lines[lines.len().saturating_sub(n)..].join(" | ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshots_newest_first() {
        let logs = "noise\n[{\"time\":\"2026-01-01T10:00:00.5+01:00\",\"id\":\"aa\",\"short_id\":\"a\",\"summary\":{\"total_bytes_processed\":10}},{\"time\":\"2026-02-01T10:00:00Z\",\"id\":\"bb\",\"short_id\":\"b\"}]\n";
        let list = parse_snapshots(logs).unwrap();
        assert_eq!(list[0]["id"], "bb");
        assert_eq!(list[1]["sizeBytes"], 10);
        assert_eq!(list[1]["time"], 1_767_258_000_500i64);
        assert!(list[0]["sizeBytes"].is_null());
        assert!(parse_snapshots("[]").unwrap().is_empty());
    }

    #[test]
    fn restores_only_safe_ids() {
        assert!(hex_id("0123abcd") && !hex_id("0123abc") && !hex_id("abcdefgh; rm -rf /"));
        assert!(safe_tag("vol_ab-1") && !safe_tag("vol'x") && !safe_tag(""));
        let s = restore_script("vol_x", "0123abcd", "r1");
        assert!(s.contains("--tag 'volume=vol_x'") && s.contains("'0123abcd:/data'"));
    }
}
