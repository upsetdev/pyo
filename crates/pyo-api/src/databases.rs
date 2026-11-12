//! Databases: Postgres and MySQL databases and Valkey
//! (Redis-compatible) KV databases a team runs on one of its clusters,
//! through Pyo's databases system (pyo-db-controller runs them:
//! CloudNativePG for Postgres, MOCO for MySQL, pyo-kv for Valkey). It's set
//! up on a cluster with the first database there (`bundles::sync_databases`):
//! until it's ready, databases wait ("setting up") and are sent once it is.
//! Not the external databases Pyo backs up (databases.rs).
//!
//! - **Owner:** the team. Any of its services on the same cluster can use
//!   one (`connectDatabase` adds the connection variables).
//! - **Where:** the team's own managed namespace on that cluster,
//!   `pyo-db-<team id>`: one per team, so a database isn't tied to a
//!   project, and the controller (bound in every managed namespace) runs it.
//! - **Logins:** made and kept by Pyo, sealed with the team's key; the agent
//!   writes them into the database's Secret. Shown only through
//!   `revealDatabasePassword` (owners and admins).
//! - **Status:** the controller's, as each heartbeat reports it.

use axum::{
    Json,
    extract::{Path, Query, State},
};
use pyo_proto::{Command, DatabaseReport, DatabaseSpec, EnvVar, NamedSecret};
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::{PgPool, Postgres, Transaction};

use crate::{
    AppState,
    auth::{SessionUser, User},
    deploy,
    error::{ApiError, ApiResult, ErrorBody},
    ops::{Role, log_activity, require_role, team_of},
};

/// (name, CPU, memory, storage in GiB) per instance.
pub const SIZES: [(&str, &str, &str, i32); 4] = [
    ("small", "250m", "512Mi", 5),
    ("medium", "500m", "1Gi", 10),
    ("large", "1", "2Gi", 20),
    ("xlarge", "2", "4Gi", 50),
];
pub const POSTGRES_VERSIONS: [&str; 3] = ["16", "17", "18"];
pub const VALKEY_VERSIONS: [&str; 2] = ["8.0", "8.1"];
pub const MYSQL_VERSIONS: [&str; 1] = ["8.4"];
/// MySQL's instance counts: 1 (single server), or a primary and 2 or 4
/// replicas (high availability). As pyo-db-controller takes them.
pub const MYSQL_INSTANCES: [i32; 3] = [1, 3, 5];
/// The least memory a MySQL instance gets, in MiB: no "small" preset.
pub const MYSQL_MIN_MEMORY_MI: i32 = 1024;
const MAX_STORAGE_GI: i32 = 1000;

fn bad(m: impl Into<String>) -> ApiError {
    ApiError::bad_request(m)
}

/// The team's managed namespace for its databases on a cluster.
pub fn namespace_for(team_id: &str) -> String {
    pyo_proto::databases_namespace(team_id)
}

fn size_of(size: &str) -> ApiResult<(&'static str, &'static str, &'static str, i32)> {
    SIZES
        .iter()
        .copied()
        .find(|s| s.0 == size)
        .ok_or_else(|| bad("The size is small, medium, large or xlarge."))
}

/// "A Postgres database", for messages.
fn what(engine: &str) -> &'static str {
    match engine {
        "valkey" => "A KV database",
        "mysql" => "A MySQL database",
        _ => "A Postgres database",
    }
}

/// The most replicas a high-availability one has: Postgres 8, KV and MySQL 4.
fn max_replicas(engine: &str) -> i32 {
    if engine == "postgres" { 8 } else { 4 }
}

/// 1 (single server), or a primary and 2 to `max_replicas` replicas
/// (high availability, failing over across servers). A primary and one
/// replica can't fail over safely on their own. MySQL: only `MYSQL_INSTANCES`.
fn check_instances(engine: &str, n: i32) -> ApiResult<()> {
    if engine == "mysql" {
        if MYSQL_INSTANCES.contains(&n) {
            return Ok(());
        }
        let ha: Vec<String> = MYSQL_INSTANCES
            .iter()
            .filter(|i| **i > 1)
            .map(|i| i.to_string())
            .collect();
        let replicas: Vec<String> = MYSQL_INSTANCES
            .iter()
            .filter(|i| **i > 1)
            .map(|i| (i - 1).to_string())
            .collect();
        return Err(bad(format!(
            "A MySQL database runs on a single server (instances 1), or with {} replicas (instances {}).",
            replicas.join(" or "),
            ha.join(" or ")
        )));
    }
    let max = max_replicas(engine);
    if n == 1 || (3..=max + 1).contains(&n) {
        return Ok(());
    }
    Err(bad(format!(
        "{} can have 2 to {max} replicas (instances {}), or run on a single server (instances 1).",
        what(engine),
        if max + 1 > 3 {
            format!("3 to {}", max + 1)
        } else {
            "3".into()
        }
    )))
}

/// MySQL only grows: 1 to 3 or 5, 3 to 5 (MOCO can't remove instances).
fn check_mysql_grows(engine: &str, from: i32, to: i32) -> ApiResult<()> {
    if engine == "mysql" && to < from {
        return Err(bad(format!(
            "A MySQL database's instances only grow: it has {from}, so {to} isn't possible. Restore its backups into a new, smaller database instead."
        )));
    }
    Ok(())
}

/// The engines and what each is called in messages.
fn check_engine(engine: &str) -> ApiResult<()> {
    if ["postgres", "valkey", "mysql"].contains(&engine) {
        return Ok(());
    }
    Err(bad(
        "The engine is \"postgres\", \"mysql\" or \"valkey\" (KV: Redis-compatible key-value).",
    ))
}

/// A preset the engine can run on: MySQL needs 1 GiB, so not "small".
fn check_preset(engine: &str, size: &str) -> ApiResult<()> {
    let (_, _, mem, _) = size_of(size)?;
    if engine == "mysql" && parse_mi(mem) < MYSQL_MIN_MEMORY_MI {
        return Err(bad(
            "A MySQL database needs at least 1 GiB of memory: medium, large, xlarge, or custom from 1024 MiB.",
        ));
    }
    Ok(())
}

/// Backed up continuously, restorable to a moment (point-in-time recovery).
fn point_in_time(engine: &str) -> bool {
    engine == "postgres" || engine == "mysql"
}

/// "500m" or "2" as millicores.
fn parse_millis(cpu: &str) -> i32 {
    cpu.strip_suffix('m').map_or_else(
        || cpu.parse::<f64>().map_or(0, |c| (c * 1000.0) as i32),
        |m| m.parse().unwrap_or(0),
    )
}

/// "512Mi" or "4Gi" as MiB.
fn parse_mi(mem: &str) -> i32 {
    mem.strip_suffix("Gi").map_or_else(
        || {
            mem.strip_suffix("Mi")
                .and_then(|m| m.parse().ok())
                .unwrap_or(0)
        },
        |g| g.parse::<i32>().unwrap_or(0) * 1024,
    )
}

/// The most dedicated read replicas: Postgres 5, KV 4, MySQL none.
fn max_read_replicas(engine: &str) -> i32 {
    match engine {
        "postgres" => 5,
        "mysql" => 0,
        _ => 4,
    }
}

/// 0 (none), or 1 up to the engine's most.
fn check_read_replicas(engine: &str, n: i32) -> ApiResult<()> {
    let max = max_read_replicas(engine);
    if (0..=max).contains(&n) {
        return Ok(());
    }
    if max == 0 {
        return Err(bad(format!("{} has no read replicas.", what(engine))));
    }
    Err(bad(format!(
        "{} can have 1 to {max} read replicas, or 0 for none.",
        what(engine)
    )))
}

/// A custom size's bounds: CPU 0.125 to 8 in steps of 0.125, memory in
/// steps of 256 MiB from 512 MiB for Postgres (256 for KV, 1024 for MySQL)
/// to 32 GiB.
pub(crate) fn check_custom(engine: &str, cpu_millis: i32, memory_mi: i32) -> ApiResult<()> {
    if !(125..=8000).contains(&cpu_millis) || cpu_millis % 125 != 0 {
        return Err(bad(
            "CPU is 0.125 to 8, in steps of 0.125 (cpuMillis 125 to 8000, a multiple of 125).",
        ));
    }
    let min = match engine {
        "postgres" => 512,
        "mysql" => MYSQL_MIN_MEMORY_MI,
        _ => 256,
    };
    if !(min..=32768).contains(&memory_mi) || memory_mi % 256 != 0 {
        return Err(bad(format!(
            "Memory is {min} MiB to 32 GiB, in steps of 256 MiB (memoryMi {min} to 32768, a multiple of 256)."
        )));
    }
    Ok(())
}

/// CPU and memory per instance, as the controller takes them.
fn resources_of(db: &Db) -> (String, String) {
    if db.size == "custom" {
        return (
            format!("{}m", db.cpu_millis.unwrap_or(500)),
            format!("{}Mi", db.memory_mi.unwrap_or(1024)),
        );
    }
    let (_, cpu, memory, _) = size_of(&db.size).unwrap_or(SIZES[0]);
    (cpu.to_owned(), memory.to_owned())
}

fn versions(engine: &str) -> &'static [&'static str] {
    match engine {
        "postgres" => &POSTGRES_VERSIONS,
        "mysql" => &MYSQL_VERSIONS,
        _ => &VALKEY_VERSIONS,
    }
}

/// A row, as the code needs it.
#[derive(sqlx::FromRow, Clone)]
struct Db {
    id: String,
    team_id: String,
    cluster_id: String,
    name: String,
    engine: String,
    version: String,
    size: String,
    storage_gi: i32,
    instances: i32,
    pooler: bool,
    rest: bool,
    deletion_protection: bool,
    namespace: String,
    password: String,
    token: String,
    readonly_token: String,
    state: String,
    /// {enabled, retentionDays, hour}: Postgres backups (point-in-time recovery).
    backup: Value,
    /// The Database object's restore block, for one made from backups.
    restore: Option<Value>,
    /// {sourceId, sourceName, targetTime}.
    restored_from: Option<Value>,
    /// Dedicated read replicas, 0 for none.
    read_replicas: i32,
    /// A custom size's CPU (millicores) and memory (MiB) per instance.
    cpu_millis: Option<i32>,
    memory_mi: Option<i32>,
    /// Postgres: the extensions Pyo turns on; KV: the modules it loads
    /// (names, as `set_extensions` takes them).
    extensions: Value,
}

async fn load(tx: &mut Transaction<'_, Postgres>, id: &str) -> ApiResult<Db> {
    sqlx::query_as::<_, Db>(
        "SELECT id, team_id, cluster_id, name, engine, version, size, storage_gi, instances, pooler, rest, deletion_protection, namespace, password, token, readonly_token, state, backup, restore, restored_from, read_replicas, cpu_millis, memory_mi, extensions
           FROM pyo.databases WHERE id = $1 FOR UPDATE",
    )
    .bind(id)
    .fetch_optional(&mut **tx)
    .await?
    .ok_or_else(|| ApiError::not_found("Database"))
}

fn host(db: &Db, service: &str) -> String {
    format!("{service}.{}.svc.cluster.local", db.namespace)
}

/// The Postgres and MySQL login (the controller's default owner and database).
const PG_USER: &str = "app";
const PG_DATABASE: &str = "app";
const MYSQL_PORT: u16 = 3306;

/// The variables a service gets for it, with the password and tokens as
/// given (the real ones, or a mask for showing).
fn variables(db: &Db, password: &str, token: &str, readonly_token: &str) -> Vec<(String, String)> {
    if db.engine == "postgres" {
        let direct = format!(
            "postgres://{PG_USER}:{password}@{}:5432/{PG_DATABASE}",
            host(db, &format!("{}-rw", db.name))
        );
        let pooled = format!(
            "postgres://{PG_USER}:{password}@{}:5432/{PG_DATABASE}",
            host(db, &format!("{}-pooler", db.name))
        );
        let mut v = vec![
            (
                "DATABASE_URL".into(),
                if db.pooler { pooled } else { direct.clone() },
            ),
            ("DATABASE_URL_DIRECT".into(), direct),
        ];
        if db.read_replicas > 0 {
            // Through the read replicas' own pooler.
            v.push((
                "DATABASE_URL_READ".into(),
                format!(
                    "postgres://{PG_USER}:{password}@{}:5432/{PG_DATABASE}",
                    host(db, &format!("{}-read-pooler", db.name))
                ),
            ));
        }
        v
    } else if db.engine == "mysql" {
        // Always the primary: no pooler, and no read address.
        let h = host(db, &format!("{}-primary", db.name));
        vec![
            (
                "DATABASE_URL".into(),
                format!("mysql://{PG_USER}:{password}@{h}:{MYSQL_PORT}/{PG_DATABASE}"),
            ),
            ("MYSQL_HOST".into(), h),
            ("MYSQL_PORT".into(), MYSQL_PORT.to_string()),
            ("MYSQL_USER".into(), PG_USER.into()),
            ("MYSQL_PASSWORD".into(), password.to_owned()),
            ("MYSQL_DATABASE".into(), PG_DATABASE.into()),
        ]
    } else {
        // Through pyo-kv: it follows failover and sends reads to read
        // replicas itself, so there's no separate read address.
        let mut v = vec![(
            "REDIS_URL".into(),
            format!("redis://default:{password}@{}:6379", host(db, &db.name)),
        )];
        if db.rest {
            v.push((
                "KV_REST_API_URL".into(),
                format!("http://{}", host(db, &format!("{}-rest", db.name))),
            ));
            v.push(("KV_REST_API_TOKEN".into(), token.to_owned()));
            // pyo-kv runs only reads with it.
            if !db.readonly_token.is_empty() {
                v.push((
                    "KV_REST_API_READ_ONLY_TOKEN".into(),
                    readonly_token.to_owned(),
                ));
            }
        }
        v
    }
}

/// Where a team's database backups go: its Backups destination, under
/// `databases/` (the controller adds <namespace>/<name>/), with its keys.
struct Dest {
    endpoint: String,
    bucket: String,
    path: String,
    region: String,
    access_key: String,
    secret: String,
}

async fn destination(
    s: &AppState,
    tx: &mut Transaction<'_, Postgres>,
    team_id: &str,
    cluster_id: &str,
) -> ApiResult<Dest> {
    let Some(t) = crate::teambackup::settings(&s.db, team_id).await? else {
        return Err(ApiError::conflict(
            "Set where the team's backups go first (Backups).",
        ));
    };
    if t.base.is_local() && !crate::teambackup::on_pyos_cluster(tx, cluster_id).await? {
        return Err(ApiError::conflict(
            "The team's backups go to Pyo's own store, which only the cluster Pyo runs on reaches. Use an S3 bucket in Backups for databases on other clusters.",
        ));
    }
    // The store on Pyo's cluster: the team's own bucket and login.
    let (r, access_key, secret) = crate::teambackup::destination(s, team_id, &t).await?;
    let path = if t.base.is_local() {
        "databases".to_owned()
    } else {
        [r.prefix.trim_matches('/'), "databases"]
            .iter()
            .filter(|p| !p.is_empty())
            .copied()
            .collect::<Vec<_>>()
            .join("/")
    };
    Ok(Dest {
        endpoint: r.endpoint.trim_end_matches('/').to_owned(),
        bucket: r.bucket,
        path,
        region: r.region.to_ascii_lowercase(),
        access_key,
        secret,
    })
}

fn backup_secret(db: &Db) -> String {
    format!("{}-backup", db.name)
}

/// What the agent applies for it.
async fn spec(
    s: &AppState,
    tx: &mut Transaction<'_, Postgres>,
    db: &Db,
) -> ApiResult<DatabaseSpec> {
    let vault = crate::vault::get();
    let password = vault.open(&db.password).await?;
    let (cpu, memory) = resources_of(db);
    let secret_name = format!("{}-credentials", db.name);
    let mut database = json!({
        "apiVersion": "pyo.dev/v1alpha1", "kind": "Database",
        "metadata": { "name": db.name, "namespace": db.namespace, "labels": { "app.kubernetes.io/managed-by": "pyo", "pyo.dev/database-id": db.id } },
        "spec": {
            "engine": db.engine, "version": db.version, "instances": db.instances,
            "storage": format!("{}Gi", db.storage_gi), "cpu": cpu, "memory": memory,
            "credentialsSecret": secret_name,
        },
    });
    if db.read_replicas > 0 {
        database["spec"]["readReplicas"] = json!({ "instances": db.read_replicas });
    }
    let (secret_type, env) = if db.engine == "postgres" {
        database["spec"]["postgres"] = json!({ "database": PG_DATABASE, "owner": PG_USER, "pooler": db.pooler, "extensions": db.extensions });
        (
            "kubernetes.io/basic-auth",
            vec![
                EnvVar {
                    key: "username".into(),
                    value: PG_USER.into(),
                },
                EnvVar {
                    key: "password".into(),
                    value: password,
                },
            ],
        )
    } else if db.engine == "mysql" {
        // The app login, as for Postgres: user and database "app".
        (
            "kubernetes.io/basic-auth",
            vec![
                EnvVar {
                    key: "username".into(),
                    value: PG_USER.into(),
                },
                EnvVar {
                    key: "password".into(),
                    value: password,
                },
            ],
        )
    } else {
        database["spec"]["rest"] = json!(db.rest);
        database["spec"]["valkey"] = json!({ "modules": db.extensions });
        (
            "Opaque",
            vec![
                EnvVar {
                    key: "username".into(),
                    value: "default".into(),
                },
                EnvVar {
                    key: "password".into(),
                    value: password,
                },
                EnvVar {
                    key: "token".into(),
                    value: vault.open(&db.token).await?,
                },
                EnvVar {
                    key: "readonly-token".into(),
                    value: vault.open(&db.readonly_token).await?,
                },
            ],
        )
    };
    // Backups and restoring (Postgres, MySQL): the storage's keys in a
    // Secret of its own beside it.
    let mut secrets = vec![];
    let backing_up = db.backup["enabled"].as_bool().unwrap_or(false);
    if point_in_time(&db.engine) && (backing_up || db.restore.is_some()) {
        let d = destination(s, tx, &db.team_id, &db.cluster_id).await?;
        if backing_up {
            database["spec"]["backup"] = json!({
                "enabled": true,
                "destination": { "endpointURL": d.endpoint, "bucket": d.bucket, "path": d.path, "region": d.region, "credentialsSecret": backup_secret(db) },
                "retentionDays": db.backup["retentionDays"].as_i64().unwrap_or(7),
                "baseBackupSchedule": format!("0 0 {} * * *", db.backup["hour"].as_i64().unwrap_or(3)),
            });
        }
        if let Some(r) = &db.restore {
            database["spec"]["restore"] = r.clone();
        }
        secrets.push(NamedSecret {
            name: backup_secret(db),
            env: vec![
                EnvVar {
                    key: "accessKeyId".into(),
                    value: d.access_key,
                },
                EnvVar {
                    key: "secretAccessKey".into(),
                    value: d.secret,
                },
            ],
        });
    }
    // Its latest import (database_imports.rs): the source's password or the
    // dump's token beside it while it runs, gone once it's done.
    let (import, import_secret, remove_secrets) =
        crate::database_imports::spec_part(tx, &db.id, &db.name).await?;
    if let Some(i) = import {
        database["spec"]["import"] = i;
    }
    secrets.extend(import_secret);
    Ok(DatabaseSpec {
        namespace: db.namespace.clone(),
        name: db.name.clone(),
        database,
        secret_name,
        secret_type: secret_type.into(),
        env,
        secrets,
        remove_secrets,
        team_id: db.team_id.clone(),
    })
}

/// A database to import into, as database_imports.rs needs it.
pub(crate) struct ImportTarget {
    pub team_id: String,
    pub cluster_id: String,
    pub name: String,
    pub engine: String,
    pub version: String,
    pub namespace: String,
    pub state: String,
    /// Made from backups: it has data, and can't take an import.
    pub restored: bool,
    pub extensions: Vec<String>,
}

pub(crate) async fn import_target(
    tx: &mut Transaction<'_, Postgres>,
    id: &str,
) -> ApiResult<ImportTarget> {
    let db = load(tx, id).await?;
    Ok(ImportTarget {
        team_id: db.team_id,
        cluster_id: db.cluster_id,
        name: db.name,
        engine: db.engine,
        version: db.version,
        namespace: db.namespace,
        state: db.state,
        restored: db.restore.is_some(),
        extensions: serde_json::from_value(db.extensions).unwrap_or_default(),
    })
}

/// An import into an existing database was recorded: turn on the
/// extensions it needs first (`turn_on`, Postgres's "managed" ones), and
/// send it with its import.
pub(crate) async fn import_into(
    s: &AppState,
    tx: &mut Transaction<'_, Postgres>,
    id: &str,
    turn_on: &[String],
) -> ApiResult<()> {
    let mut db = load(tx, id).await?;
    if !turn_on.is_empty() {
        let mut wanted: Vec<String> =
            serde_json::from_value(db.extensions.clone()).unwrap_or_default();
        wanted.extend(turn_on.iter().cloned());
        db.extensions = json!(check_extensions(&db.engine, &wanted)?);
        sqlx::query("UPDATE pyo.databases SET extensions = $2, updated_at = now() WHERE id = $1")
            .bind(&db.id)
            .bind(&db.extensions)
            .execute(&mut **tx)
            .await?;
    }
    send(s, tx, &db).await
}

/// Send one again (its import ended: without the import's Secret).
pub(crate) async fn send_by_id(
    s: &AppState,
    tx: &mut Transaction<'_, Postgres>,
    id: &str,
) -> ApiResult<()> {
    let db = load(tx, id).await?;
    if db.state != "active" {
        return Ok(());
    }
    send(s, tx, &db).await
}

/// Send it to the cluster's agent, once the databases system is there;
/// before that it waits, and `resend` sends it when the system is set up.
async fn send(s: &AppState, tx: &mut Transaction<'_, Postgres>, db: &Db) -> ApiResult<()> {
    if !system_there(tx, &db.cluster_id).await? {
        return Ok(());
    }
    send_now(s, tx, db).await
}

async fn send_now(s: &AppState, tx: &mut Transaction<'_, Postgres>, db: &Db) -> ApiResult<()> {
    let spec = spec(s, tx, db).await?;
    deploy::enqueue(tx, &db.cluster_id, Command::ApplyDatabase(Box::new(spec))).await?;
    Ok(())
}

/// The databases system is on the cluster (applied once, or found there by
/// the agent): its Database kind exists.
async fn system_there(tx: &mut Transaction<'_, Postgres>, cluster_id: &str) -> ApiResult<bool> {
    let row: Option<(String, Option<String>)> = sqlx::query_as("SELECT state, reported_version FROM pyo.cluster_bundles WHERE cluster_id = $1 AND bundle = 'databases'").bind(cluster_id).fetch_optional(&mut **tx).await?;
    Ok(row.is_some_and(|(state, reported)| {
        state == "applied" || reported.is_some_and(|v| !v.is_empty())
    }))
}

/// The databases system was just set up (or updated) on a cluster: send
/// every database there, waiting ones included. Applying is idempotent.
pub async fn resend(
    s: &AppState,
    tx: &mut Transaction<'_, Postgres>,
    cluster_id: &str,
) -> ApiResult<()> {
    let ids: Vec<String> = sqlx::query_scalar("SELECT id FROM pyo.databases WHERE cluster_id = $1 AND state = 'active' ORDER BY created_at").bind(cluster_id).fetch_all(&mut **tx).await?;
    for id in ids {
        let db = load(tx, &id).await?;
        if let Err(e) = send_now(s, tx, &db).await {
            tracing::warn!(database = %db.name, cluster = cluster_id, error = ?e, "couldn't send a database after setting up databases");
        }
    }
    Ok(())
}

fn label_of(db: &Db) -> Value {
    json!({ "kind": "database", "id": db.id, "label": db.name })
}

// ── Extensions: Postgres extensions and KV modules ─────────────────────────

/// The extensions Pyo's databases offer (pyo-db-controller's catalog, the
/// one its controller turns them on from). Postgres: "managed" ones Pyo
/// turns on and off; "additional" ones the database's owner creates itself.
/// KV: Valkey modules (the catalog's "kv" list), all "managed".
#[derive(Deserialize, serde::Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Extension {
    pub name: String,
    /// "postgres" or "valkey".
    #[serde(default)]
    pub engine: String,
    #[serde(default)]
    pub section: String,
    pub description: String,
    pub docs: String,
    /// Obsolete: listed only while a database has it on.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub hidden: bool,
    /// The build it is, shown beside its name ("Apache 2 Edition").
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub edition: String,
    /// Turning it on or off restarts the database (it loads a library).
    #[serde(default)]
    pub restart: bool,
}

pub fn extension_catalog() -> &'static [Extension] {
    #[derive(Deserialize)]
    struct Catalog {
        extensions: Vec<Extension>,
        kv: Vec<Extension>,
    }
    static C: std::sync::OnceLock<Vec<Extension>> = std::sync::OnceLock::new();
    C.get_or_init(|| {
        let c = serde_json::from_str::<Catalog>(include_str!(
            "../../pyo-db-controller/extensions.json"
        ))
        .expect("extensions.json");
        let pg = c.extensions.into_iter().map(|e| Extension {
            engine: "postgres".into(),
            ..e
        });
        // A module loads at start: turning one on or off restarts the database.
        let kv = c.kv.into_iter().map(|e| Extension {
            engine: "valkey".into(),
            section: "managed".into(),
            restart: true,
            ..e
        });
        pg.chain(kv).collect()
    })
}

/// The extensions asked for, as the database keeps them: Postgres's
/// "managed" ones or KV's modules, each once, in the catalog's order.
fn check_extensions(engine: &str, wanted: &[String]) -> ApiResult<Vec<String>> {
    if wanted.is_empty() {
        return Ok(vec![]);
    }
    if engine != "postgres" && engine != "valkey" {
        return Err(bad("Extensions are for Postgres and KV databases."));
    }
    let of_engine = || extension_catalog().iter().filter(|e| e.engine == engine);
    for w in wanted {
        match of_engine().find(|e| &e.name == w) {
            Some(e) if e.section == "managed" => {}
            Some(_) => {
                return Err(bad(format!(
                    "{w} isn't turned on here: connect to the database with its default role and run CREATE EXTENSION \"{w}\".",
                )));
            }
            None => {
                return Err(bad(format!(
                    "There's no extension {w:?} to turn on here. listDatabaseExtensions lists them, by engine."
                )));
            }
        }
    }
    Ok(of_engine()
        .filter(|e| wanted.contains(&e.name))
        .map(|e| e.name.clone())
        .collect())
}

/// Turn a Postgres database's extensions or a KV database's modules on and
/// off (the whole list that's on).
pub async fn set_extensions(
    s: &AppState,
    tx: &mut Transaction<'_, Postgres>,
    user: &User,
    id: &str,
    wanted: Vec<String>,
) -> ApiResult<Value> {
    let mut db = load(tx, id).await?;
    require_role(tx, user, &db.team_id, Role::Developer).await?;
    if db.state != "active" {
        return Err(ApiError::conflict("It's being deleted."));
    }
    let next = check_extensions(&db.engine, &wanted)?;
    let before: Vec<String> = serde_json::from_value(db.extensions.clone()).unwrap_or_default();
    // The modules come with Valkey 8.1.
    if db.engine == "valkey" && next.iter().any(|n| !before.contains(n)) && db.version != "8.1" {
        return Err(bad(
            "Modules need KV 8.1. Change the version in Settings first.",
        ));
    }
    if before == next {
        return Ok(json!({ "ok": true, "clusterId": db.cluster_id, "restarts": false }));
    }
    let changed: Vec<&String> = next
        .iter()
        .filter(|n| !before.contains(n))
        .chain(before.iter().filter(|b| !next.contains(b)))
        .collect();
    let restarts = changed.iter().any(|n| {
        extension_catalog()
            .iter()
            .any(|e| &e.name == *n && e.restart)
    });
    db.extensions = json!(next);
    sqlx::query("UPDATE pyo.databases SET extensions = $2 WHERE id = $1")
        .bind(&db.id)
        .bind(&db.extensions)
        .execute(&mut **tx)
        .await?;
    send(s, tx, &db).await?;
    let on: Vec<&str> = next
        .iter()
        .filter(|n| !before.contains(n))
        .map(String::as_str)
        .collect();
    let off: Vec<&str> = before
        .iter()
        .filter(|b| !next.contains(b))
        .map(String::as_str)
        .collect();
    let what = match (on.is_empty(), off.is_empty()) {
        (false, true) => format!("Turned on {} for {}", on.join(", "), db.name),
        (true, false) => format!("Turned off {} for {}", off.join(", "), db.name),
        _ => format!(
            "Turned on {} and off {} for {}",
            on.join(", "),
            off.join(", "),
            db.name
        ),
    };
    log_activity(
        tx,
        &db.team_id,
        &user.name,
        "database.updated",
        "info",
        &what,
        label_of(&db),
        json!({ "clusterId": db.cluster_id }),
    )
    .await?;
    Ok(json!({ "ok": true, "clusterId": db.cluster_id, "restarts": restarts }))
}

/// Its extensions as listed: each one that's on or on its way off, with
/// the controller's word on it ("enabled", "pending", "restarting",
/// "disabling" or "failed", and why). Null for other engines.
fn extensions_of(d: &Db, status: &Value) -> Value {
    if d.engine != "postgres" && d.engine != "valkey" {
        return Value::Null;
    }
    let wanted: Vec<String> = serde_json::from_value(d.extensions.clone()).unwrap_or_default();
    let reported: Vec<Value> = status["extensions"].as_array().cloned().unwrap_or_default();
    let of = |n: &str| reported.iter().find(|r| r["name"] == n);
    let mut out: Vec<Value> = wanted
        .iter()
        .map(|n| match of(n) {
            Some(r) => json!({ "name": n, "enabled": true, "state": r["state"], "message": r["message"].as_str().unwrap_or("") }),
            None => json!({ "name": n, "enabled": true, "state": "pending", "message": "" }),
        })
        .collect();
    for r in reported
        .iter()
        .filter(|r| !wanted.iter().any(|w| r["name"] == w.as_str()))
    {
        out.push(json!({ "name": r["name"], "enabled": false, "state": r["state"], "message": r["message"].as_str().unwrap_or("") }));
    }
    Value::Array(out)
}

/// Database extensions
///
/// The extensions Pyo's databases offer: Postgres extensions and KV
/// modules. Each one's `name`, `engine` ("postgres" or "valkey"), `section`
/// ("managed" ones are turned on with `setDatabaseExtensions`, and the
/// database's owner adds "additional" ones with CREATE EXTENSION), a
/// one-line `description`, its `docs`, its `edition` when it's a limited build,
/// `hidden` when it's obsolete (shown only while a database has it on), and
/// whether turning it on or off `restart`s the database (every KV module
/// does).
#[utoipa::path(get, path = "/database-extensions", operation_id = "listDatabaseExtensions", tag = "Databases",
    responses((status = 200, description = "`extensions`: each one's `name`, `engine` (\"postgres\" or \"valkey\"), `section`, `description`, `docs`, `edition` (when it's a limited build), `hidden` (when it's obsolete) and `restart`.", body = Object)),
    extensions(("x-pyo-access" = json!("read"))))]
pub(crate) async fn extensions(SessionUser(_user): SessionUser) -> ApiResult<Json<Value>> {
    Ok(Json(json!({ "extensions": extension_catalog() })))
}

/// A login: letters and digits only, so it needs no escaping in a URL.
fn secret() -> String {
    crate::ids::random_string(32)
}

/// Refuses a name whose objects would be another database's (`shop-rw`
/// beside Postgres `shop`), or are the namespace's own.
fn name_conflict(engine: &str, name: &str, others: &[(String, String)]) -> ApiResult<()> {
    match pyo_proto::database_names::conflicts(
        engine,
        name,
        others.iter().map(|(e, n)| (e.as_str(), n.as_str())),
    ) {
        None => Ok(()),
        Some("") => Err(bad("This name is reserved.")),
        Some(other) => Err(ApiError::conflict(format!(
            "The name conflicts with the database {other}."
        ))),
    }
}

pub struct Create {
    pub id: String,
    pub team_id: String,
    pub cluster_id: String,
    pub name: String,
    pub engine: String,
    pub size: String,
    pub instances: i32,
    pub version: Option<String>,
    pub storage_gi: Option<i32>,
    pub pooler: Option<bool>,
    pub rest: bool,
    /// Made from another's backups: its restore block and where it came from.
    pub restore: Option<Value>,
    pub restored_from: Option<Value>,
    /// A custom size's CPU (millicores) and memory (MiB).
    pub cpu_millis: Option<i32>,
    pub memory_mi: Option<i32>,
    pub read_replicas: Option<i32>,
    /// Postgres: extensions turned on with it.
    pub extensions: Vec<String>,
}

pub async fn create(
    s: &AppState,
    tx: &mut Transaction<'_, Postgres>,
    user: &User,
    c: Create,
) -> ApiResult<Value> {
    if !crate::ids::valid_client_id(&c.id, "mdb") {
        return Err(bad("Invalid database id."));
    }
    require_role(tx, user, &c.team_id, Role::Developer).await?;
    crate::util::require_dns_label(&c.name, "Database name")?;
    if c.name.len() > 40 {
        return Err(bad("A database name is at most 40 characters."));
    }
    check_engine(&c.engine)?;
    let (cpu_millis, memory_mi, preset_storage) = if c.size == "custom" {
        let (Some(cm), Some(mm)) = (c.cpu_millis, c.memory_mi) else {
            return Err(bad("A custom size needs cpuMillis and memoryMi."));
        };
        check_custom(&c.engine, cm, mm)?;
        (Some(cm), Some(mm), 10)
    } else {
        check_preset(&c.engine, &c.size)?;
        (None, None, size_of(&c.size)?.3)
    };
    let read_replicas = c.read_replicas.unwrap_or(0);
    check_read_replicas(&c.engine, read_replicas)?;
    check_instances(&c.engine, c.instances)?;
    let version = c.version.unwrap_or_else(|| {
        versions(&c.engine)
            .last()
            .copied()
            .unwrap_or_default()
            .to_owned()
    });
    if !versions(&c.engine).contains(&version.as_str()) {
        return Err(bad(format!(
            "The version is one of {}.",
            versions(&c.engine).join(", ")
        )));
    }
    let storage = c.storage_gi.unwrap_or(preset_storage);
    if !(1..=MAX_STORAGE_GI).contains(&storage) {
        return Err(bad(format!("Storage is 1 to {MAX_STORAGE_GI} GiB.")));
    }
    let cluster_team: Option<String> =
        sqlx::query_scalar("SELECT team_id FROM pyo.clusters WHERE id = $1")
            .bind(&c.cluster_id)
            .fetch_optional(&mut **tx)
            .await?;
    if cluster_team.as_deref() != Some(c.team_id.as_str()) {
        return Err(ApiError::not_found("Cluster"));
    }
    let extensions = check_extensions(&c.engine, &c.extensions)?;
    // A deleted one keeps its row (and name) until the cluster has removed it.
    let taken: Option<String> =
        sqlx::query_scalar("SELECT state FROM pyo.databases WHERE team_id = $1 AND name = $2")
            .bind(&c.team_id)
            .bind(&c.name)
            .fetch_optional(&mut **tx)
            .await?;
    match taken.as_deref() {
        Some("deleting") => {
            return Err(ApiError::conflict(format!(
                "{} is still being deleted. Try again in a few seconds.",
                c.name
            )));
        }
        Some(_) => {
            return Err(ApiError::conflict(format!(
                "The team already has a database called {}.",
                c.name
            )));
        }
        None => {}
    }
    // Its objects share the team's namespace with the others'.
    let others: Vec<(String, String)> =
        sqlx::query_as("SELECT engine, name FROM pyo.databases WHERE team_id = $1")
            .bind(&c.team_id)
            .fetch_all(&mut **tx)
            .await?;
    name_conflict(&c.engine, &c.name, &others)?;
    let vault = crate::vault::get();
    let valkey = c.engine == "valkey";
    let db = Db {
        id: c.id.clone(),
        team_id: c.team_id.clone(),
        cluster_id: c.cluster_id.clone(),
        name: c.name.clone(),
        engine: c.engine.clone(),
        version,
        size: c.size.clone(),
        storage_gi: storage,
        instances: c.instances,
        // A pooler by default for Postgres.
        pooler: c.engine == "postgres" && c.pooler.unwrap_or(true),
        rest: valkey && c.rest,
        deletion_protection: true,
        namespace: namespace_for(&c.team_id),
        password: vault.seal(&c.team_id, &secret()).await?,
        token: if valkey {
            vault.seal(&c.team_id, &secret()).await?
        } else {
            String::new()
        },
        readonly_token: if valkey {
            vault.seal(&c.team_id, &secret()).await?
        } else {
            String::new()
        },
        state: "active".into(),
        backup: json!({}),
        restore: c.restore.clone(),
        restored_from: c.restored_from.clone(),
        read_replicas,
        cpu_millis,
        memory_mi,
        extensions: json!(extensions),
    };
    sqlx::query(
        "INSERT INTO pyo.databases (id, team_id, cluster_id, name, engine, version, size, storage_gi, instances, pooler, rest, namespace, password, token, readonly_token, created_by, restore, restored_from, read_replicas, cpu_millis, memory_mi, extensions)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17, $18, $19, $20, $21, $22)",
    )
    .bind(&db.id)
    .bind(&db.team_id)
    .bind(&db.cluster_id)
    .bind(&db.name)
    .bind(&db.engine)
    .bind(&db.version)
    .bind(&db.size)
    .bind(db.storage_gi)
    .bind(db.instances)
    .bind(db.pooler)
    .bind(db.rest)
    .bind(&db.namespace)
    .bind(&db.password)
    .bind(&db.token)
    .bind(&db.readonly_token)
    .bind(&user.name)
    .bind(&db.restore)
    .bind(&db.restored_from)
    .bind(db.read_replicas)
    .bind(db.cpu_millis)
    .bind(db.memory_mi)
    .bind(&db.extensions)
    .execute(&mut **tx)
    .await?;
    // The cluster's first database sets databases up there; it's sent once
    // they're ready (`resend`).
    if !system_there(tx, &db.cluster_id).await?
        && !crate::bundles::sync_databases(tx, &db.cluster_id, true).await?
    {
        let cluster: String = sqlx::query_scalar("SELECT name FROM pyo.clusters WHERE id = $1")
            .bind(&db.cluster_id)
            .fetch_one(&mut **tx)
            .await?;
        let first: bool =
            sqlx::query_scalar("SELECT count(*) = 1 FROM pyo.databases WHERE cluster_id = $1")
                .bind(&db.cluster_id)
                .fetch_one(&mut **tx)
                .await?;
        if first {
            log_activity(
                tx,
                &db.team_id,
                "Pyo",
                "cluster.updated",
                "info",
                &format!("Setting up databases on {cluster}"),
                json!({ "kind": "cluster", "id": db.cluster_id, "label": cluster }),
                json!({ "clusterId": db.cluster_id }),
            )
            .await?;
        }
    }
    send(s, tx, &db).await?;
    let kind = if valkey { "KV database" } else { "database" };
    log_activity(
        tx,
        &db.team_id,
        &user.name,
        "database.created",
        "info",
        &format!("Created the {kind} {}", db.name),
        label_of(&db),
        json!({ "clusterId": db.cluster_id }),
    )
    .await?;
    Ok(json!({ "ok": true, "clusterId": db.cluster_id }))
}

pub struct Update {
    pub id: String,
    pub size: Option<String>,
    pub instances: Option<i32>,
    pub storage_gi: Option<i32>,
    pub version: Option<String>,
    pub pooler: Option<bool>,
    pub rest: Option<bool>,
    pub deletion_protection: Option<bool>,
    pub read_replicas: Option<i32>,
    pub cpu_millis: Option<i32>,
    pub memory_mi: Option<i32>,
}

pub async fn update(
    s: &AppState,
    tx: &mut Transaction<'_, Postgres>,
    user: &User,
    u: Update,
) -> ApiResult<Value> {
    let mut db = load(tx, &u.id).await?;
    require_role(tx, user, &db.team_id, Role::Developer).await?;
    if db.state != "active" {
        return Err(ApiError::conflict("It's being deleted."));
    }
    // Turning deletion protection off is for owners and admins.
    if u.deletion_protection == Some(false) && db.deletion_protection {
        require_role(tx, user, &db.team_id, Role::Admin).await?;
    }
    if let Some(s) = u.size {
        if s == "custom" {
            // From a preset: its CPU and memory unless given.
            if db.size != "custom" {
                let (_, cpu, mem, _) = size_of(&db.size)?;
                db.cpu_millis = Some(parse_millis(cpu));
                db.memory_mi = Some(parse_mi(mem));
            }
        } else {
            check_preset(&db.engine, &s)?;
            db.cpu_millis = None;
            db.memory_mi = None;
        }
        db.size = s;
    }
    if u.cpu_millis.is_some() || u.memory_mi.is_some() {
        if db.size != "custom" {
            return Err(bad("Set size \"custom\" to choose CPU and memory."));
        }
        db.cpu_millis = u.cpu_millis.or(db.cpu_millis);
        db.memory_mi = u.memory_mi.or(db.memory_mi);
    }
    if db.size == "custom" {
        check_custom(
            &db.engine,
            db.cpu_millis.unwrap_or(0),
            db.memory_mi.unwrap_or(0),
        )?;
    }
    if let Some(n) = u.instances {
        check_instances(&db.engine, n)?;
        check_mysql_grows(&db.engine, db.instances, n)?;
        db.instances = n;
    }
    if let Some(g) = u.storage_gi {
        if g < db.storage_gi {
            return Err(bad(format!(
                "Storage only grows: it's {} GiB now.",
                db.storage_gi
            )));
        }
        if g > MAX_STORAGE_GI {
            return Err(bad(format!("Storage is at most {MAX_STORAGE_GI} GiB.")));
        }
        db.storage_gi = g;
    }
    if let Some(v) = u.version {
        let list = versions(&db.engine);
        let (Some(new), Some(old)) = (
            list.iter().position(|x| *x == v),
            list.iter().position(|x| *x == db.version),
        ) else {
            return Err(bad(format!("The version is one of {}.", list.join(", "))));
        };
        if new < old {
            return Err(bad("A database's version only goes up."));
        }
        db.version = v;
    }
    if let Some(p) = u.pooler {
        if db.engine != "postgres" {
            return Err(bad("The connection pooler is for Postgres."));
        }
        db.pooler = p;
    }
    if let Some(r) = u.rest {
        if db.engine != "valkey" {
            return Err(bad("The REST API is for KV databases."));
        }
        db.rest = r;
    }
    if let Some(d) = u.deletion_protection {
        db.deletion_protection = d;
    }
    if let Some(n) = u.read_replicas {
        check_read_replicas(&db.engine, n)?;
        db.read_replicas = n;
    }
    sqlx::query(
        "UPDATE pyo.databases SET size = $2, instances = $3, storage_gi = $4, version = $5, pooler = $6, rest = $7, deletion_protection = $8, read_replicas = $9, cpu_millis = $10, memory_mi = $11, updated_at = now() WHERE id = $1",
    )
    .bind(&db.id)
    .bind(&db.size)
    .bind(db.instances)
    .bind(db.storage_gi)
    .bind(&db.version)
    .bind(db.pooler)
    .bind(db.rest)
    .bind(db.deletion_protection)
    .bind(db.read_replicas)
    .bind(db.cpu_millis)
    .bind(db.memory_mi)
    .execute(&mut **tx)
    .await?;
    send(s, tx, &db).await?;
    // Connected services' variables follow (pooler, REST, read replicas), on their next deploy.
    refresh_links(tx, &db).await?;
    log_activity(
        tx,
        &db.team_id,
        &user.name,
        "database.updated",
        "info",
        &format!("Changed {}", db.name),
        label_of(&db),
        json!({ "clusterId": db.cluster_id }),
    )
    .await?;
    // What was stored, so a client can tell a field this Pyo doesn't know
    // (ignored) from one it saved.
    Ok(json!({ "ok": true, "clusterId": db.cluster_id, "readReplicas": db.read_replicas }))
}

pub async fn delete(tx: &mut Transaction<'_, Postgres>, user: &User, id: &str) -> ApiResult<Value> {
    let db = load(tx, id).await?;
    require_role(tx, user, &db.team_id, Role::Admin).await?;
    // One made for an import that failed has nothing to protect.
    if db.deletion_protection && !crate::database_imports::failed_new(tx, id).await? {
        return Err(ApiError::conflict(format!(
            "{} has deletion protection on. Turn it off in its settings first.",
            db.name
        )));
    }
    // Services being deleted don't count: their links go with them.
    let linked: Vec<String> = sqlx::query_scalar("SELECT a.name FROM pyo.database_links l JOIN pyo.services a ON a.id = l.service_id WHERE l.database_id = $1 AND NOT a.deleting ORDER BY a.name").bind(id).fetch_all(&mut **tx).await?;
    if !linked.is_empty() {
        return Err(ApiError::conflict(format!(
            "{} is connected to {}. Disconnect it first.",
            db.name,
            linked.join(", ")
        )));
    }
    sqlx::query("UPDATE pyo.databases SET state = 'deleting', updated_at = now() WHERE id = $1")
        .bind(id)
        .execute(&mut **tx)
        .await?;
    drop_pending_applies(tx, &db.cluster_id, &db.namespace, &db.name).await?;
    // Its registration in Backups stops now and goes with it (its snapshots
    // stay in the repository, as when Backups stops backing one up).
    sqlx::query("UPDATE pyo.backup_databases SET enabled = false WHERE database_id = $1")
        .bind(id)
        .execute(&mut **tx)
        .await?;
    deploy::enqueue(
        tx,
        &db.cluster_id,
        Command::DeleteDatabase {
            namespace: db.namespace.clone(),
            name: db.name.clone(),
        },
    )
    .await?;
    log_activity(
        tx,
        &db.team_id,
        &user.name,
        "database.deleted",
        "warning",
        &format!("Deleted {}", db.name),
        label_of(&db),
        json!({ "clusterId": db.cluster_id }),
    )
    .await?;
    Ok(json!({ "ok": true, "clusterId": db.cluster_id }))
}

/// Turn a Postgres or MySQL database's backups (point-in-time recovery) on or off,
/// to the team's Backups destination: base backups daily at `hour` (UTC),
/// kept `retention_days`, and every change archived in between.
pub async fn set_backups(
    s: &AppState,
    tx: &mut Transaction<'_, Postgres>,
    user: &User,
    id: &str,
    b: BackupInput,
) -> ApiResult<Value> {
    let BackupInput {
        enabled,
        retention_days,
        hour,
        schedule,
        keep,
    } = b;
    let mut db = load(tx, id).await?;
    require_role(tx, user, &db.team_id, Role::Developer).await?;
    if db.state != "active" {
        return Err(ApiError::conflict("It's being deleted."));
    }
    if db.engine == "valkey" {
        return set_kv_backups(s, tx, user, &db, enabled, schedule, hour, keep).await;
    }
    let retention =
        retention_days.unwrap_or_else(|| db.backup["retentionDays"].as_u64().unwrap_or(7) as u32);
    if !(1..=90).contains(&retention) {
        return Err(bad("Backups are kept 1 to 90 days."));
    }
    let hour = hour.unwrap_or_else(|| db.backup["hour"].as_u64().unwrap_or(3) as u8);
    if hour > 23 {
        return Err(bad("The hour is 0 to 23 (UTC)."));
    }
    db.backup = json!({ "enabled": enabled, "retentionDays": retention, "hour": hour });
    sqlx::query("UPDATE pyo.databases SET backup = $2, updated_at = now() WHERE id = $1")
        .bind(id)
        .bind(&db.backup)
        .execute(&mut **tx)
        .await?;
    send(s, tx, &db).await?;
    let title = if enabled {
        format!("Turned on backups of {}", db.name)
    } else {
        format!("Turned off backups of {}", db.name)
    };
    log_activity(
        tx,
        &db.team_id,
        &user.name,
        "database.updated",
        "info",
        &title,
        label_of(&db),
        json!({ "clusterId": db.cluster_id }),
    )
    .await?;
    Ok(json!({ "ok": true, "clusterId": db.cluster_id }))
}

pub struct BackupInput {
    pub enabled: bool,
    pub retention_days: Option<u32>,
    pub hour: Option<u8>,
    pub schedule: Option<String>,
    pub keep: Option<u32>,
}

/// A KV database's backups: an RDB dump on a schedule through Backups
/// (databases.rs), as its own login, which Pyo holds. Turning them on the
/// first time registers it there; off pauses that registration.
#[allow(clippy::too_many_arguments)]
async fn set_kv_backups(
    s: &AppState,
    tx: &mut Transaction<'_, Postgres>,
    user: &User,
    db: &Db,
    enabled: bool,
    schedule: Option<String>,
    hour: Option<u8>,
    keep: Option<u32>,
) -> ApiResult<Value> {
    let current: Option<(String, String, i32, i32)> = sqlx::query_as(
        "SELECT id, schedule, hour, keep FROM pyo.backup_databases WHERE database_id = $1",
    )
    .bind(&db.id)
    .fetch_optional(&mut **tx)
    .await?;
    let schedule = schedule
        .or_else(|| current.as_ref().map(|c| c.1.clone()))
        .unwrap_or_else(|| "daily".into());
    let hour = hour
        .or_else(|| current.as_ref().map(|c| c.2 as u8))
        .unwrap_or(2);
    let keep = keep
        .or_else(|| current.as_ref().map(|c| c.3 as u32))
        .unwrap_or(14);
    crate::database_backups::schedule_ok(&schedule, hour, keep)?;
    if enabled {
        // Same rules as Postgres backups: the team's destination, reachable.
        destination(s, tx, &db.team_id, &db.cluster_id).await?;
    }
    let backup_id = match current {
        Some((bid, ..)) => {
            sqlx::query("UPDATE pyo.backup_databases SET enabled = $2, schedule = $3, hour = $4, keep = $5 WHERE id = $1")
                .bind(&bid)
                .bind(enabled)
                .bind(&schedule)
                .bind(i32::from(hour))
                .bind(keep as i32)
                .execute(&mut **tx)
                .await?;
            Some(bid)
        }
        None if enabled => {
            let taken: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM pyo.backup_databases WHERE team_id = $1 AND name = $2)").bind(&db.team_id).bind(&db.name).fetch_one(&mut **tx).await?;
            if taken {
                return Err(ApiError::conflict(format!(
                    "Backups already has a database named {}. Rename or remove that one there first.",
                    db.name
                )));
            }
            let bid = crate::ids::new_id("db");
            sqlx::query(
                "INSERT INTO pyo.backup_databases (id, team_id, cluster_id, name, engine, host, port, password_sealed, login, enabled, schedule, hour, keep, database_id)
                 VALUES ($1, $2, $3, $4, 'redis', $5, 6379, $6, 'default', true, $7, $8, $9, $10)",
            )
            .bind(&bid)
            .bind(&db.team_id)
            .bind(&db.cluster_id)
            .bind(&db.name)
            .bind(host(db, &db.name))
            .bind(&db.password)
            .bind(&schedule)
            .bind(i32::from(hour))
            .bind(keep as i32)
            .bind(&db.id)
            .execute(&mut **tx)
            .await?;
            Some(bid)
        }
        None => None,
    };
    let title = if enabled {
        format!("Turned on backups of {}", db.name)
    } else {
        format!("Turned off backups of {}", db.name)
    };
    log_activity(
        tx,
        &db.team_id,
        &user.name,
        "database.updated",
        "info",
        &title,
        label_of(db),
        json!({ "clusterId": db.cluster_id }),
    )
    .await?;
    Ok(json!({ "ok": true, "clusterId": db.cluster_id, "backupDatabaseId": backup_id }))
}

/// A KV database's backups, as Backups has them; null until first turned on.
async fn kv_backup(pool: &PgPool, id: &str) -> ApiResult<Value> {
    Ok(sqlx::query_scalar(
        "SELECT jsonb_build_object('enabled', enabled, 'schedule', schedule, 'hour', hour, 'keep', keep, 'backupDatabaseId', id)
           FROM pyo.backup_databases WHERE database_id = $1",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?
    .unwrap_or(Value::Null))
}

/// Dedicated read replicas as listed: how many, how many are ready, and each
/// one's server and lag (from the controller's status); null when off.
fn read_replicas_of(d: &Db, status: &Value) -> Value {
    if d.read_replicas <= 0 {
        return Value::Null;
    }
    let r = &status["readReplicas"];
    let members: Vec<Value> = r["members"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|m| json!({ "name": m["name"].as_str().unwrap_or(""), "ready": m["ready"].as_bool().unwrap_or(false), "server": m["node"].as_str().unwrap_or(""), "lagSeconds": m["lagSeconds"].as_f64() }))
        .collect();
    // `reported`: the controller reports them.
    json!({ "instances": d.read_replicas, "ready": r["ready"].as_i64().unwrap_or(0), "members": members, "reported": r.is_object() })
}

/// When a point-in-time target is acceptable: within what the backups
/// cover, [first recoverability point, `last`], where `last` is now
/// (Postgres archives every change as it happens) or the latest backup
/// (MySQL uploads its changes with each backup). None: the latest.
pub(crate) fn check_target(
    first: Option<&str>,
    last: Option<&str>,
    target: Option<&str>,
    now: jiff::Timestamp,
) -> ApiResult<Option<String>> {
    let first: jiff::Timestamp = first
        .filter(|f| !f.is_empty())
        .and_then(|f| f.parse().ok())
        .ok_or_else(|| {
            ApiError::conflict(
                "Its backups don't cover any moment yet: the first base backup hasn't finished.",
            )
        })?;
    let Some(t) = target.filter(|t| !t.is_empty()) else {
        return Ok(None);
    };
    let t: jiff::Timestamp = t
        .parse()
        .map_err(|_| bad("The time is an RFC 3339 timestamp, e.g. 2026-11-09T14:30:00Z."))?;
    let until = last
        .filter(|l| !l.is_empty())
        .and_then(|l| l.parse::<jiff::Timestamp>().ok());
    match until {
        Some(last) if t < first || t > last => Err(bad(format!(
            "Pick a moment from {first} to {last}, its latest backup."
        ))),
        None if t < first || t > now => Err(bad(format!("Pick a moment from {first} to now."))),
        _ => Ok(Some(t.to_string())),
    }
}

/// A new database from a Postgres or MySQL database's backups, at a moment
/// or the latest: same cluster, size and availability; made empty, then restored.
pub async fn restore(
    s: &AppState,
    tx: &mut Transaction<'_, Postgres>,
    user: &User,
    new_id: String,
    source_id: &str,
    name: String,
    target_time: Option<String>,
) -> ApiResult<Value> {
    let src = load(tx, source_id).await?;
    require_role(tx, user, &src.team_id, Role::Developer).await?;
    if !point_in_time(&src.engine) {
        return Err(bad("Restoring is for Postgres and MySQL databases."));
    }
    let status: Value = sqlx::query_scalar("SELECT status FROM pyo.databases WHERE id = $1")
        .bind(source_id)
        .fetch_one(&mut **tx)
        .await?;
    // MySQL: up to its latest backup (its changes are uploaded with each one).
    let last = if src.engine == "mysql" {
        Some(
            status["backup"]["lastSuccessfulBackup"]
                .as_str()
                .unwrap_or(""),
        )
    } else {
        None
    };
    if last == Some("") {
        return Err(ApiError::conflict(
            "Its backups don't cover any moment yet: the first backup hasn't finished.",
        ));
    }
    let target = check_target(
        status["backup"]["firstRecoverabilityPoint"].as_str(),
        last,
        target_time.as_deref(),
        jiff::Timestamp::now(),
    )?;
    let d = destination(s, tx, &src.team_id, &src.cluster_id).await?;
    let mut restore = json!({
        "fromBackup": {
            "endpointURL": d.endpoint, "bucket": d.bucket, "path": d.path, "region": d.region,
            "credentialsSecret": format!("{name}-backup"),
            "sourceName": src.name, "sourceNamespace": src.namespace,
        },
    });
    if let Some(t) = &target {
        restore["targetTime"] = json!(t);
    }
    let restored_from = json!({ "sourceId": src.id, "sourceName": src.name, "targetTime": target });
    let r = create(
        s,
        tx,
        user,
        Create {
            id: new_id,
            team_id: src.team_id.clone(),
            cluster_id: src.cluster_id.clone(),
            name,
            engine: src.engine.clone(),
            size: src.size.clone(),
            instances: src.instances,
            version: Some(src.version.clone()),
            storage_gi: Some(src.storage_gi),
            pooler: Some(src.pooler),
            rest: false,
            restore: Some(restore),
            restored_from: Some(restored_from),
            cpu_millis: src.cpu_millis,
            memory_mi: src.memory_mi,
            read_replicas: None,
            // Its data has their objects: their libraries load with it.
            extensions: serde_json::from_value(src.extensions.clone()).unwrap_or_default(),
        },
    )
    .await?;
    log_activity(
        tx,
        &src.team_id,
        &user.name,
        "database.restored",
        "info",
        &format!(
            "Restoring {} to {}",
            src.name,
            target.as_deref().unwrap_or("its latest")
        ),
        label_of(&src),
        json!({}),
    )
    .await?;
    Ok(r)
}

/// A connection's own names for the variables: `env` maps each key the
/// service gets to one of the database's variables (a template's, e.g.
/// WORDPRESS_DB_USER from MYSQL_USER), or to several joined by ":"
/// (WORDPRESS_DB_HOST from "MYSQL_HOST:MYSQL_PORT", host:port). None: the
/// variables as they are.
pub type EnvMap = std::collections::BTreeMap<String, String>;

fn mapped(
    db: &Db,
    vars: Vec<(String, String)>,
    env: Option<&EnvMap>,
) -> ApiResult<Vec<(String, String)>> {
    let Some(env) = env.filter(|e| !e.is_empty()) else {
        return Ok(vars);
    };
    let mut out = vec![];
    for (key, from) in env {
        if !crate::secretstores::env_key(key) {
            return Err(bad(format!(
                "{key} isn't a variable name: letters, digits and underscores, not starting with a digit."
            )));
        }
        let mut parts = vec![];
        for name in from.split(':') {
            let Some((_, v)) = vars.iter().find(|(k, _)| k == name) else {
                let names: Vec<&str> = vars.iter().map(|(k, _)| k.as_str()).collect();
                return Err(bad(format!(
                    "{} has no variable {name}: it has {}.",
                    db.name,
                    names.join(", ")
                )));
            };
            parts.push(v.as_str());
        }
        out.push((key.clone(), parts.join(":")));
    }
    Ok(out)
}

/// All of a database's variables, each under `prefix`: `{"MYSQL_DATABASE_URL":
/// "DATABASE_URL", …}`, as `connect` takes them.
pub async fn prefixed(
    tx: &mut Transaction<'_, Postgres>,
    id: &str,
    prefix: &str,
) -> ApiResult<EnvMap> {
    let db = load(tx, id).await?;
    let prefix = prefix.trim().trim_end_matches('_').to_ascii_uppercase();
    if !crate::secretstores::env_key(&format!("{prefix}_X")) {
        return Err(bad(
            "The prefix is letters, digits and underscores, not starting with a digit.",
        ));
    }
    Ok(prefix_names(
        &prefix,
        variables(&db, "", "", "").into_iter().map(|(k, _)| k),
    ))
}

/// Each name under `prefix`, unless it already starts with it (MYSQL_HOST
/// stays MYSQL_HOST under MYSQL).
fn prefix_names(prefix: &str, names: impl Iterator<Item = String>) -> EnvMap {
    names
        .map(|k| {
            (
                if k.starts_with(&format!("{prefix}_")) {
                    k.clone()
                } else {
                    format!("{prefix}_{k}")
                },
                k,
            )
        })
        .collect()
}

/// The prefix to suggest for an engine's variables.
fn engine_prefix(engine: &str) -> &'static str {
    match engine {
        "postgres" => "POSTGRES",
        "mysql" => "MYSQL",
        _ => "KV",
    }
}

/// For services that want the address one part at a time (a template's
/// POSTGRES_HOST, REDIS_PORT…): only offered under a service's own names.
/// Postgres's go straight to the primary, never through the pooler.
fn parts(db: &Db, password: &str) -> Vec<(String, String)> {
    let (host, port, user, database, prefix) = match db.engine.as_str() {
        "postgres" => (
            host(db, &format!("{}-rw", db.name)),
            "5432",
            PG_USER,
            Some(PG_DATABASE),
            "POSTGRES",
        ),
        "valkey" => (host(db, &db.name), "6379", "default", None, "REDIS"),
        _ => return vec![],
    };
    let mut v = vec![
        (format!("{prefix}_HOST"), host),
        (format!("{prefix}_PORT"), port.to_owned()),
    ];
    if prefix == "POSTGRES" {
        v.push((format!("{prefix}_USER"), user.to_owned()));
    }
    v.push((format!("{prefix}_PASSWORD"), password.to_owned()));
    if let Some(d) = database {
        v.push((format!("{prefix}_DATABASE"), d.to_owned()));
    }
    v
}

/// Put a database's variables in a service's (sealed, secret), replacing
/// the ones it set before.
async fn set_vars(
    tx: &mut Transaction<'_, Postgres>,
    db: &Db,
    service_id: &str,
    old: &[String],
    env: Option<&EnvMap>,
) -> ApiResult<Vec<String>> {
    let vault = crate::vault::get();
    let password = vault.open(&db.password).await?;
    let token = if db.token.is_empty() {
        String::new()
    } else {
        vault.open(&db.token).await?
    };
    let readonly_token = if db.readonly_token.is_empty() {
        String::new()
    } else {
        vault.open(&db.readonly_token).await?
    };
    let mut offered = variables(db, &password, &token, &readonly_token);
    // Under a service's own names (a template's), the address's parts too.
    if env.is_some_and(|e| !e.is_empty()) {
        offered.extend(parts(db, &password));
    }
    let vars = mapped(db, offered, env)?;
    let mut spec: Value =
        sqlx::query_scalar("SELECT spec FROM pyo.services WHERE id = $1 FOR UPDATE")
            .bind(service_id)
            .fetch_one(&mut **tx)
            .await?;
    let keys: Vec<String> = vars.iter().map(|(k, _)| k.clone()).collect();
    let mut env: Vec<Value> = spec["env"].as_array().cloned().unwrap_or_default();
    // A variable of the same name the service set itself: not overwritten.
    if let Some(k) = env
        .iter()
        .filter_map(|e| e["key"].as_str())
        .find(|k| keys.iter().any(|x| x == k) && !old.iter().any(|o| o == k))
    {
        // Another database's: this one goes under a prefix instead.
        let from: Option<String> = sqlx::query_scalar("SELECT d.name FROM pyo.database_links l JOIN pyo.databases d ON d.id = l.database_id WHERE l.service_id = $1 AND l.database_id <> $2 AND $3 = ANY(l.keys)")
            .bind(service_id)
            .bind(&db.id)
            .bind(k)
            .fetch_optional(&mut **tx)
            .await?;
        let p = engine_prefix(&db.engine);
        return Err(ApiError::conflict(match from {
            Some(other) => format!(
                "The service already gets {k} from {other}. Connect {} with a prefix, like {p} (its variables become {p}_{k}…).",
                db.name
            ),
            None => format!(
                "The service already has {k}. Rename or remove it first, or connect {} with a prefix, like {p}.",
                db.name
            ),
        }));
    }
    env.retain(|e| {
        !e["key"]
            .as_str()
            .is_some_and(|k| old.iter().any(|o| o == k) || keys.iter().any(|x| x == k))
    });
    for (k, v) in &vars {
        env.push(json!({ "id": crate::ids::new_id("env"), "key": k, "value": v, "secret": true }));
    }
    spec["env"] = Value::Array(env);
    vault.seal_env(&db.team_id, &mut spec).await?;
    sqlx::query("UPDATE pyo.services SET spec = $2, pending_changes = true, updated_at = now() WHERE id = $1").bind(service_id).bind(&spec).execute(&mut **tx).await?;
    Ok(keys)
}

async fn refresh_links(tx: &mut Transaction<'_, Postgres>, db: &Db) -> ApiResult<()> {
    let links: Vec<(String, Vec<String>, Option<Value>)> = sqlx::query_as(
        "SELECT service_id, keys, env FROM pyo.database_links WHERE database_id = $1",
    )
    .bind(&db.id)
    .fetch_all(&mut **tx)
    .await?;
    for (service_id, old, env) in links {
        let env: Option<EnvMap> = env.and_then(|e| serde_json::from_value(e).ok());
        let keys = set_vars(tx, db, &service_id, &old, env.as_ref()).await?;
        sqlx::query(
            "UPDATE pyo.database_links SET keys = $3 WHERE database_id = $1 AND service_id = $2",
        )
        .bind(&db.id)
        .bind(&service_id)
        .bind(&keys)
        .execute(&mut **tx)
        .await?;
    }
    Ok(())
}

/// Connect a service: its variables, under the names `env` gives (all of
/// them, as they are, without).
pub async fn connect(
    tx: &mut Transaction<'_, Postgres>,
    user: &User,
    id: &str,
    service_id: &str,
    env: Option<EnvMap>,
) -> ApiResult<Value> {
    let db = load(tx, id).await?;
    require_role(tx, user, &db.team_id, Role::Developer).await?;
    let (service_team, service_cluster, service_name): (String, String, String) = sqlx::query_as(
        "SELECT team_id, cluster_id, name FROM pyo.services WHERE id = $1 AND NOT deleting",
    )
    .bind(service_id)
    .fetch_optional(&mut **tx)
    .await?
    .ok_or_else(|| ApiError::not_found("Service"))?;
    if service_team != db.team_id {
        return Err(ApiError::not_found("Service"));
    }
    if service_cluster != db.cluster_id {
        return Err(ApiError::conflict(format!(
            "{service_name} runs on another cluster than {}. A service can only use a database on its own cluster.",
            db.name
        )));
    }
    let old: Vec<String> = sqlx::query_scalar(
        "SELECT keys FROM pyo.database_links WHERE database_id = $1 AND service_id = $2",
    )
    .bind(id)
    .bind(service_id)
    .fetch_optional(&mut **tx)
    .await?
    .unwrap_or_default();
    let env = env.filter(|e| !e.is_empty());
    let keys = set_vars(tx, &db, service_id, &old, env.as_ref()).await?;
    sqlx::query("INSERT INTO pyo.database_links (database_id, service_id, keys, env) VALUES ($1, $2, $3, $4) ON CONFLICT (database_id, service_id) DO UPDATE SET keys = $3, env = $4")
        .bind(id)
        .bind(service_id)
        .bind(&keys)
        .bind(env.as_ref().map(|e| json!(e)))
        .execute(&mut **tx)
        .await?;
    log_activity(
        tx,
        &db.team_id,
        &user.name,
        "database.connected",
        "info",
        &format!("Connected {} to {service_name}", db.name),
        label_of(&db),
        json!({ "serviceId": service_id }),
    )
    .await?;
    Ok(json!({ "ok": true, "variables": keys }))
}

pub async fn disconnect(
    tx: &mut Transaction<'_, Postgres>,
    user: &User,
    id: &str,
    service_id: &str,
) -> ApiResult<Value> {
    let db = load(tx, id).await?;
    require_role(tx, user, &db.team_id, Role::Developer).await?;
    let keys: Option<Vec<String>> = sqlx::query_scalar(
        "DELETE FROM pyo.database_links WHERE database_id = $1 AND service_id = $2 RETURNING keys",
    )
    .bind(id)
    .bind(service_id)
    .fetch_optional(&mut **tx)
    .await?;
    let Some(keys) = keys else {
        return Err(ApiError::not_found("Connection"));
    };
    let (mut spec, service_name): (Value, String) =
        sqlx::query_as("SELECT spec, name FROM pyo.services WHERE id = $1 FOR UPDATE")
            .bind(service_id)
            .fetch_one(&mut **tx)
            .await?;
    if let Some(env) = spec["env"].as_array_mut() {
        env.retain(|e| {
            !e["key"]
                .as_str()
                .is_some_and(|k| keys.iter().any(|x| x == k))
        });
    }
    sqlx::query("UPDATE pyo.services SET spec = $2, pending_changes = true, updated_at = now() WHERE id = $1").bind(service_id).bind(&spec).execute(&mut **tx).await?;
    log_activity(
        tx,
        &db.team_id,
        &user.name,
        "database.disconnected",
        "info",
        &format!("Disconnected {} from {service_name}", db.name),
        label_of(&db),
        json!({ "serviceId": service_id }),
    )
    .await?;
    Ok(json!({ "ok": true }))
}

pub async fn reveal(tx: &mut Transaction<'_, Postgres>, user: &User, id: &str) -> ApiResult<Value> {
    let db = load(tx, id).await?;
    require_role(tx, user, &db.team_id, Role::Admin).await?;
    let vault = crate::vault::get();
    let password = vault.open(&db.password).await?;
    let token = if db.token.is_empty() {
        Value::Null
    } else {
        json!(vault.open(&db.token).await?)
    };
    let readonly_token = if db.readonly_token.is_empty() {
        Value::Null
    } else {
        json!(vault.open(&db.readonly_token).await?)
    };
    log_activity(
        tx,
        &db.team_id,
        &user.name,
        "secret.revealed",
        "warning",
        &format!("Revealed the password of {}", db.name),
        label_of(&db),
        json!({}),
    )
    .await?;
    Ok(json!({ "ok": true, "password": password, "token": token, "readonlyToken": readonly_token }))
}

/// Changes to a database not yet handed to the agent go when it's deleted:
/// applied after the deletion, one would bring it back. (Handed-out ones
/// run before the deletion: the agent keeps a database's commands in order.)
async fn drop_pending_applies(
    tx: &mut Transaction<'_, Postgres>,
    cluster_id: &str,
    namespace: &str,
    name: &str,
) -> ApiResult<()> {
    sqlx::query(
        "UPDATE pyo.commands SET status = 'failed', done_at = now(), result = '{\"ok\":false,\"message\":\"The database is being deleted.\",\"logs\":[]}'
          WHERE cluster_id = $1 AND status = 'pending' AND command->>'kind' = 'apply_database' AND command->>'namespace' = $2 AND command->>'name' = $3",
    )
    .bind(cluster_id)
    .bind(namespace)
    .bind(name)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// A deleted database's row goes, once the agent says its Database is gone;
/// its id stays behind (`pyo.deleted_databases`), so a Database left on the
/// cluster with that id is known to be one to delete.
async fn forget(
    tx: &mut Transaction<'_, Postgres>,
    cluster_id: &str,
    namespace: &str,
    name: &str,
) -> ApiResult<()> {
    sqlx::query(
        "WITH gone AS (DELETE FROM pyo.databases WHERE cluster_id = $1 AND namespace = $2 AND name = $3 AND state = 'deleting' RETURNING id, cluster_id, namespace, name)
         INSERT INTO pyo.deleted_databases (id, cluster_id, namespace, name) SELECT id, cluster_id, namespace, name FROM gone
         ON CONFLICT (id) DO UPDATE SET deleted_at = now()",
    )
    .bind(cluster_id)
    .bind(namespace)
    .bind(name)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// What to do after a heartbeat, for databases being deleted and Database
/// objects Pyo has no database for (see [`plan_cleanup`]).
#[derive(Debug, Default, PartialEq)]
struct Cleanup {
    /// Send (again) a deletion: (namespace, name).
    delete: Vec<(String, String)>,
    /// Gone from the cluster: the row goes.
    forget: Vec<(String, String)>,
    /// On the cluster, labelled Pyo's, but no database of Pyo's (and not one
    /// it deleted): only reported, never deleted.
    unknown: Vec<(String, String, String)>,
}

/// One heartbeat's cleanup. `deleting`: this cluster's databases being
/// deleted (namespace, name); `known`: every (namespace, name) Pyo has a
/// database for here; `deleted`: ids Pyo deleted here; `in_flight`: those
/// with a deletion on its way (queued, with the agent, or just tried).
/// Deletions are asked for until the agent confirms; a Database brought
/// back after that (by id) is deleted again; anything else is left alone.
fn plan_cleanup(
    reports: &[DatabaseReport],
    listed: bool,
    deleting: &[(String, String)],
    known: &std::collections::HashSet<(String, String)>,
    deleted: &std::collections::HashSet<String>,
    in_flight: &std::collections::HashSet<(String, String)>,
) -> Cleanup {
    let mut out = Cleanup::default();
    let on_cluster: std::collections::HashSet<(String, String)> = reports
        .iter()
        .map(|r| (r.namespace.clone(), r.name.clone()))
        .collect();
    for key in deleting {
        if in_flight.contains(key) {
            continue;
        }
        if listed && !on_cluster.contains(key) {
            out.forget.push(key.clone());
        } else {
            out.delete.push(key.clone());
        }
    }
    for r in reports {
        let key = (r.namespace.clone(), r.name.clone());
        if known.contains(&key) {
            continue;
        }
        if !r.id.is_empty() && deleted.contains(&r.id) {
            if !in_flight.contains(&key) && !out.delete.contains(&key) {
                out.delete.push(key);
            }
        } else {
            out.unknown
                .push((r.namespace.clone(), r.name.clone(), r.id.clone()));
        }
    }
    out
}

/// Orphans already warned about by this process (heartbeats come every
/// few seconds).
static WARNED: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashSet<(String, String, String)>>,
> = std::sync::LazyLock::new(Default::default);

/// Deletions still unconfirmed are asked for again, a database confirmed
/// gone loses its row, and a Database Pyo deleted that's on the cluster
/// again is deleted again.
async fn clean_up(
    tx: &mut Transaction<'_, Postgres>,
    cluster_id: &str,
    reports: &[DatabaseReport],
    listed: bool,
) -> ApiResult<()> {
    let rows: Vec<(String, String, String)> =
        sqlx::query_as("SELECT namespace, name, state FROM pyo.databases WHERE cluster_id = $1")
            .bind(cluster_id)
            .fetch_all(&mut **tx)
            .await?;
    let deleting: Vec<(String, String)> = rows
        .iter()
        .filter(|(_, _, st)| st == "deleting")
        .map(|(ns, n, _)| (ns.clone(), n.clone()))
        .collect();
    let known: std::collections::HashSet<(String, String)> = rows
        .iter()
        .map(|(ns, n, _)| (ns.clone(), n.clone()))
        .collect();
    let ids: Vec<String> = reports
        .iter()
        .filter(|r| !r.id.is_empty())
        .map(|r| r.id.clone())
        .collect();
    if deleting.is_empty() && reports.iter().all(|r| known_has(&known, r)) {
        return Ok(());
    }
    let deleted: std::collections::HashSet<String> = sqlx::query_scalar(
        "SELECT id FROM pyo.deleted_databases WHERE cluster_id = $1 AND id = ANY($2)",
    )
    .bind(cluster_id)
    .bind(&ids)
    .fetch_all(&mut **tx)
    .await?
    .into_iter()
    .collect();
    // A deletion on its way: queued, with the agent (it answers within
    // minutes; one lost to a restart is asked again after 10), or tried in
    // the last 2 minutes (so a failing one is retried every 2).
    let in_flight: Vec<(String, String)> = sqlx::query_as(
        "SELECT DISTINCT command->>'namespace', command->>'name' FROM pyo.commands
          WHERE cluster_id = $1 AND command->>'kind' = 'delete_database'
            AND (status = 'pending' OR (status = 'sent' AND sent_at > now() - interval '10 minutes') OR created_at > now() - interval '2 minutes')",
    )
    .bind(cluster_id)
    .fetch_all(&mut **tx)
    .await?;
    let in_flight = in_flight.into_iter().collect();
    let plan = plan_cleanup(reports, listed, &deleting, &known, &deleted, &in_flight);
    for (ns, name) in &plan.forget {
        tracing::info!(cluster = cluster_id, namespace = %ns, database = %name, "database gone from the cluster; removing it");
        forget(tx, cluster_id, ns, name).await?;
    }
    for (ns, name) in plan.delete {
        tracing::warn!(cluster = cluster_id, namespace = %ns, database = %name, "database deleted in Pyo is still on the cluster; deleting it again");
        deploy::enqueue(
            tx,
            cluster_id,
            Command::DeleteDatabase {
                namespace: ns,
                name,
            },
        )
        .await?;
    }
    for (ns, name, id) in plan.unknown {
        if WARNED.lock().unwrap_or_else(|e| e.into_inner()).insert((
            ns.clone(),
            name.clone(),
            id.clone(),
        )) {
            tracing::warn!(cluster = cluster_id, namespace = %ns, database = %name, id = %id, "Database on the cluster that Pyo has no database for; leaving it");
        }
    }
    Ok(())
}

fn known_has(known: &std::collections::HashSet<(String, String)>, r: &DatabaseReport) -> bool {
    known.contains(&(r.namespace.clone(), r.name.clone()))
}

/// What a heartbeat found on a cluster. The ids of databases to send again
/// (an import of theirs just ended).
pub async fn reported(
    tx: &mut Transaction<'_, Postgres>,
    cluster_id: &str,
    reports: &[DatabaseReport],
    listed: bool,
) -> ApiResult<Vec<String>> {
    clean_up(tx, cluster_id, reports, listed).await?;
    let mut resend = vec![];
    for r in reports {
        let status = &r.status;
        // What changed, as events (failovers, members ready or not, backups).
        let old: Option<(String, Value)> = sqlx::query_as("SELECT id, status FROM pyo.databases WHERE cluster_id = $1 AND namespace = $2 AND name = $3 AND status IS DISTINCT FROM $4")
            .bind(cluster_id)
            .bind(&r.namespace)
            .bind(&r.name)
            .bind(status)
            .fetch_optional(&mut **tx)
            .await?;
        if let Some((id, old)) = old {
            crate::database_logs::record(tx, &id, &old, status).await?;
            if status.get("import").is_some()
                && crate::database_imports::observed(tx, &id, status).await?
            {
                resend.push(id);
            }
        }
        sqlx::query("UPDATE pyo.databases SET status = $4 WHERE cluster_id = $1 AND namespace = $2 AND name = $3 AND status IS DISTINCT FROM $4")
            .bind(cluster_id)
            .bind(&r.namespace)
            .bind(&r.name)
            .bind(status)
            .execute(&mut **tx)
            .await?;
    }
    Ok(resend)
}

/// The agent finished a database's command: a deletion removes the row, a
/// failure shows on the database.
pub async fn finished(
    tx: &mut Transaction<'_, Postgres>,
    cluster_id: &str,
    command: &Value,
    ok: bool,
    message: Option<&str>,
) -> ApiResult<()> {
    let (ns, name) = match command["kind"].as_str() {
        Some("apply_database") => (
            command["namespace"].as_str().unwrap_or(""),
            command["name"].as_str().unwrap_or(""),
        ),
        Some("delete_database") => {
            if ok {
                forget(
                    tx,
                    cluster_id,
                    command["namespace"].as_str().unwrap_or(""),
                    command["name"].as_str().unwrap_or(""),
                )
                .await?;
            }
            // A failure leaves it "deleting": heartbeats ask again (`reported`).
            return Ok(());
        }
        _ => return Ok(()),
    };
    if !ok {
        let status = json!({ "phase": "Failed", "message": message.unwrap_or("The cluster couldn't apply it.") });
        sqlx::query("UPDATE pyo.databases SET status = status || $4 WHERE cluster_id = $1 AND namespace = $2 AND name = $3").bind(cluster_id).bind(ns).bind(name).bind(status).execute(&mut **tx).await?;
    }
    Ok(())
}

/// The databases of these teams, as listed (never logins).
pub async fn listing(db: &PgPool, team_ids: &[String], id: Option<&str>) -> ApiResult<Vec<Value>> {
    let rows: Vec<Db> = sqlx::query_as::<_, Db>(
        "SELECT id, team_id, cluster_id, name, engine, version, size, storage_gi, instances, pooler, rest, deletion_protection, namespace, password, token, readonly_token, state, backup, restore, restored_from, read_replicas, cpu_millis, memory_mi, extensions
           FROM pyo.databases WHERE team_id = ANY($1) AND ($2::text IS NULL OR id = $2) ORDER BY created_at",
    )
    .bind(team_ids)
    .bind(id)
    .fetch_all(db)
    .await?;
    // Where the databases system is on their clusters: "setting up" while
    // it's being installed there (a first database waits for it).
    let clusters: Vec<String> = rows.iter().map(|d| d.cluster_id.clone()).collect();
    let systems = crate::bundles::databases_state(db, &clusters).await?;
    let imports = crate::database_imports::listing(
        db,
        &rows.iter().map(|d| d.id.clone()).collect::<Vec<_>>(),
    )
    .await?;
    let mut out = vec![];
    for d in rows {
        let setup = match systems.get(&d.cluster_id) {
            Some((state, _)) if state == "ready" => Value::Null,
            Some((state, message)) => {
                json!({ "state": if state == "failed" { "failed" } else { "installing" }, "message": message })
            }
            None => json!({ "state": "installing", "message": "" }),
        };
        let (status, created_at, links): (Value, i64, Value) = sqlx::query_as(
            "SELECT m.status, pyo.ms(m.created_at),
                    coalesce((SELECT jsonb_agg(jsonb_build_object('serviceId', l.service_id, 'variables', l.keys)) FROM pyo.database_links l JOIN pyo.services a ON a.id = l.service_id WHERE l.database_id = m.id AND NOT a.deleting), '[]')
               FROM pyo.databases m WHERE m.id = $1",
        )
        .bind(&d.id)
        .fetch_one(db)
        .await?;
        let (cpu, memory) = resources_of(&d);
        // Its instances and the servers they run on (from controllers that
        // report them; none from older ones), primary first.
        let members: Vec<Value> = status["members"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|m| json!({ "name": m["name"].as_str().unwrap_or(""), "role": m["role"].as_str().unwrap_or("replica"), "ready": m["ready"].as_bool().unwrap_or(false), "server": m["node"].as_str().unwrap_or("") }))
            .collect();
        // Connection details, password masked.
        let shown: Vec<Value> = variables(&d, "••••••••", "••••••••", "••••••••")
            .into_iter()
            .map(|(k, v)| json!({ "key": k, "value": v }))
            .collect();
        out.push(json!({
            "id": d.id, "teamId": d.team_id, "clusterId": d.cluster_id, "name": d.name, "engine": d.engine, "version": d.version,
            "size": d.size, "cpu": cpu, "memory": memory, "storageGi": d.storage_gi, "instances": d.instances,
            "pooler": d.pooler, "rest": d.rest, "deletionProtection": d.deletion_protection, "state": d.state,
            "status": status, "members": members, "createdAt": created_at, "connections": links,
            "backup": if point_in_time(&d.engine) { json!({ "enabled": d.backup["enabled"].as_bool().unwrap_or(false), "retentionDays": d.backup["retentionDays"].as_i64().unwrap_or(7), "hour": d.backup["hour"].as_i64().unwrap_or(3) }) } else { kv_backup(db, &d.id).await? },
            "restoredFrom": d.restored_from,
            "import": imports.get(&d.id).cloned().unwrap_or(Value::Null),
            "readReplicas": read_replicas_of(&d, &status),
            "extensions": extensions_of(&d, &status),
            "setup": setup,
            "cpuMillis": parse_millis(&cpu),
            "memoryMi": parse_mi(&memory),
            "username": if d.engine == "valkey" { "default" } else { PG_USER },
            "database": if d.engine == "valkey" { None } else { Some(PG_DATABASE) },
            "variables": shown,
        }));
    }
    Ok(out)
}

#[derive(Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub struct ListQuery {
    /// The team's id.
    team: String,
}

/// Databases
///
/// A team's Postgres and MySQL databases and Valkey
/// (Redis-compatible) KV databases: engine, version, size, instances, the cluster's status (phase,
/// primary, ready instances, failovers), the services connected to each and
/// their variables, passwords masked.
#[utoipa::path(get, path = "/databases", operation_id = "listDatabases", tag = "Databases",
    params(ListQuery),
    responses(
        (status = 200, description = "`databases`, each with `id`, `name`, `engine`, `version`, `size`, `cpu`, `memory`, `storageGi`, `instances`, `pooler`, `rest`, `deletionProtection`, `status` (with `backup`: `firstRecoverabilityPoint`, `lastSuccessfulBackup`, `lastArchivedWal`, `lastFailedBackup`, `message`), `backup`, `restoredFrom`, `readReplicas`, `extensions`, `setup`, `members`, `connections` and `variables` (passwords masked). `backup` has `enabled`, `retentionDays` and `hour` for Postgres and MySQL. For KV, once turned on, it has `enabled`, `schedule`, `hour`, `keep` and `backupDatabaseId` (its id in Backups). `readReplicas` is null when off, or has `instances`, `ready`, `reported` and `members` (each with `name`, `ready`, `server` and `lagSeconds`). `extensions` is null for MySQL. For Postgres and KV, it lists each extension or module that's on or being turned off, with `name`, `enabled`, `state` (\"enabled\", \"pending\", \"restarting\", \"disabling\" or \"failed\") and a `message`. `setup` is null once the cluster's databases are set up. While they're being set up or updated, it has `state` (\"installing\" or \"failed\") and a `message`. `members` lists each instance's `name`, `role` (\"primary\" or \"replica\"), `ready` and the `server` it runs on, primary first.", body = Object),
        (status = 404, description = "No such team, or you aren't in it.", body = ErrorBody),
    ),
    extensions(("x-pyo-access" = json!("read"))))]
pub(crate) async fn list(
    State(s): State<AppState>,
    SessionUser(user): SessionUser,
    Query(q): Query<ListQuery>,
) -> ApiResult<Json<Value>> {
    let mut tx = s.db.begin().await?;
    require_role(&mut tx, &user, &q.team, Role::Viewer).await?;
    tx.commit().await?;
    Ok(Json(
        json!({ "databases": listing(&s.db, &[q.team], None).await? }),
    ))
}

/// A database
///
/// One Postgres, MySQL or KV database, as `listDatabases` lists them.
#[utoipa::path(get, path = "/databases/{id}", operation_id = "getDatabase", tag = "Databases",
    params(("id" = String, Path, description = "The database's id.")),
    responses(
        (status = 200, description = "The database, as in `listDatabases`.", body = Object),
        (status = 404, description = "No such database, or you can't see it.", body = ErrorBody),
    ),
    extensions(("x-pyo-access" = json!("read"))))]
pub(crate) async fn get(
    State(s): State<AppState>,
    SessionUser(user): SessionUser,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    let mut tx = s.db.begin().await?;
    let team_id = team_of(&mut tx, "databases", &id).await?;
    require_role(&mut tx, &user, &team_id, Role::Viewer).await?;
    tx.commit().await?;
    let db = listing(&s.db, &[team_id], Some(&id))
        .await?
        .into_iter()
        .next()
        .ok_or_else(|| ApiError::not_found("Database"))?;
    Ok(Json(db))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_prefix_names_every_variable_once() {
        let names = |engine: &str, p: &str| {
            prefix_names(
                p,
                variables(&db(engine, false, false), "", "", "")
                    .into_iter()
                    .map(|(k, _)| k),
            )
        };
        let mysql = names("mysql", "MYSQL");
        assert_eq!(mysql["MYSQL_DATABASE_URL"], "DATABASE_URL");
        assert_eq!(mysql["MYSQL_HOST"], "MYSQL_HOST", "not MYSQL_MYSQL_HOST");
        let pg = names("postgres", "PG");
        assert_eq!(
            pg.keys().collect::<Vec<_>>(),
            ["PG_DATABASE_URL", "PG_DATABASE_URL_DIRECT"]
        );
    }

    fn db(engine: &str, pooler: bool, rest: bool) -> Db {
        Db {
            id: "mdb_x".into(),
            team_id: "team_abc".into(),
            cluster_id: "cl_x".into(),
            name: "main".into(),
            engine: engine.into(),
            version: "18".into(),
            size: "small".into(),
            storage_gi: 5,
            instances: 1,
            pooler,
            rest,
            deletion_protection: true,
            namespace: namespace_for("team_abc"),
            password: String::new(),
            token: String::new(),
            readonly_token: String::new(),
            state: "active".into(),
            backup: json!({}),
            restore: None,
            restored_from: None,
            read_replicas: 0,
            cpu_millis: None,
            memory_mi: None,
            extensions: json!([]),
        }
    }

    #[test]
    fn extensions_are_checked_against_the_catalog() {
        assert_eq!(
            check_extensions(
                "postgres",
                &["pg_cron".into(), "vector".into(), "vector".into()]
            )
            .unwrap(),
            ["pg_cron", "vector"]
        );
        assert!(
            check_extensions("mysql", &["vector".into()]).is_err(),
            "Postgres only"
        );
        assert!(check_extensions("mysql", &[]).unwrap().is_empty());
        let e = check_extensions("postgres", &["citext".into()]).unwrap_err();
        assert!(
            format!("{e:?}").contains("CREATE EXTENSION"),
            "the owner adds the additional ones"
        );
        assert!(check_extensions("postgres", &["postgis".into()]).is_err());
        let restart: Vec<&str> = extension_catalog()
            .iter()
            .filter(|e| e.restart)
            .map(|e| e.name.as_str())
            .collect();
        assert!(restart.contains(&"timescaledb") && !restart.contains(&"vector"));
        // As listed: the controller's word, or pending until it has one.
        let mut d = db("postgres", true, false);
        d.extensions = json!(["vector", "pg_cron"]);
        let st = json!({ "extensions": [{ "name": "pg_cron", "state": "restarting", "message": "Restarts the database to load it." }, { "name": "timescaledb", "state": "disabling", "message": "x" }] });
        let l = extensions_of(&d, &st);
        assert_eq!(
            l[0],
            json!({ "name": "vector", "enabled": true, "state": "pending", "message": "" })
        );
        assert_eq!(l[1]["state"], "restarting");
        assert_eq!(
            l[2],
            json!({ "name": "timescaledb", "enabled": false, "state": "disabling", "message": "x" })
        );
        assert!(extensions_of(&db("mysql", false, false), &st).is_null());
        // KV: its modules.
        assert_eq!(
            check_extensions("valkey", &["valkey-search".into(), "valkey-json".into()]).unwrap(),
            ["valkey-json", "valkey-search"]
        );
        assert!(check_extensions("valkey", &["vector".into()]).is_err());
        assert!(check_extensions("postgres", &["valkey-json".into()]).is_err());
        assert!(
            extension_catalog()
                .iter()
                .filter(|e| e.engine == "valkey")
                .all(|e| e.restart && e.section == "managed")
        );
        assert!(
            extension_catalog()
                .iter()
                .any(|e| e.name == "intagg" && e.hidden)
        );
    }

    #[test]
    fn single_node_or_high_availability() {
        for n in [1, 3, 4, 9] {
            assert!(check_instances("postgres", n).is_ok(), "{n}");
        }
        for n in [0, 2, 10] {
            assert!(check_instances("postgres", n).is_err(), "{n}");
        }
        for n in [1, 3, 5] {
            assert!(check_instances("valkey", n).is_ok(), "{n}");
        }
        for n in [2, 6, 9] {
            assert!(check_instances("valkey", n).is_err(), "{n}");
        }
        assert!(
            check_instances("valkey", 6)
                .unwrap_err()
                .message
                .contains("A KV database can have 2 to 4 replicas")
        );
    }

    #[test]
    fn custom_sizes() {
        assert!(check_custom("postgres", 750, 1536).is_ok());
        assert!(
            check_custom("postgres", 100, 1024).is_err(),
            "below 0.125 CPU"
        );
        assert!(
            check_custom("postgres", 600, 1024).is_err(),
            "not a 0.125 step"
        );
        assert!(
            check_custom("postgres", 500, 256).is_err(),
            "Postgres needs 512 MiB"
        );
        assert!(check_custom("valkey", 125, 256).is_ok());
        assert!(
            check_custom("valkey", 500, 300).is_err(),
            "not a 256 MiB step"
        );
        assert_eq!(
            (
                parse_millis("250m"),
                parse_millis("2"),
                parse_mi("512Mi"),
                parse_mi("4Gi")
            ),
            (250, 2000, 512, 4096)
        );
    }

    #[test]
    fn restore_targets() {
        let now: jiff::Timestamp = "2026-11-09T12:00:00Z".parse().unwrap();
        let first = Some("2026-11-08T03:00:00Z");
        assert_eq!(
            check_target(first, None, None, now).unwrap(),
            None,
            "latest"
        );
        assert_eq!(
            check_target(first, None, Some("2026-11-09T10:00:00Z"), now)
                .unwrap()
                .as_deref(),
            Some("2026-11-09T10:00:00Z")
        );
        assert!(
            check_target(first, None, Some("2026-11-07T10:00:00Z"), now).is_err(),
            "before the backups"
        );
        assert!(
            check_target(first, None, Some("2026-11-09T13:00:00Z"), now).is_err(),
            "in the future"
        );
        assert!(
            check_target(None, None, None, now).is_err(),
            "nothing to restore yet"
        );
        assert!(check_target(first, None, Some("yesterday"), now).is_err());
        // MySQL: up to its latest backup, not to now.
        let last = Some("2026-11-09T03:00:00Z");
        assert!(check_target(first, last, Some("2026-11-09T02:00:00Z"), now).is_ok());
        let e = check_target(first, last, Some("2026-11-09T10:00:00Z"), now).unwrap_err();
        assert!(e.message.contains("its latest backup"), "{}", e.message);
        assert_eq!(
            check_target(first, last, None, now).unwrap(),
            None,
            "the latest backup"
        );
    }

    #[test]
    fn connection_variables() {
        assert_eq!(namespace_for("team_q9pnllqh1p5f"), "pyo-db-q9pnllqh1p5f");
        let v = variables(&db("postgres", true, false), "pw", "", "");
        assert_eq!(
            v[0],
            (
                "DATABASE_URL".into(),
                "postgres://app:pw@main-pooler.pyo-db-abc.svc.cluster.local:5432/app".into()
            )
        );
        assert_eq!(
            v[1],
            (
                "DATABASE_URL_DIRECT".into(),
                "postgres://app:pw@main-rw.pyo-db-abc.svc.cluster.local:5432/app".into()
            )
        );
        let v = variables(&db("postgres", false, false), "pw", "", "");
        assert_eq!(v[0].1, v[1].1, "without the pooler, both are direct");
        let v = variables(&db("valkey", false, true), "pw", "tok", "");
        assert_eq!(
            v[0],
            (
                "REDIS_URL".into(),
                "redis://default:pw@main.pyo-db-abc.svc.cluster.local:6379".into()
            )
        );
        assert_eq!(
            v[1],
            (
                "KV_REST_API_URL".into(),
                "http://main-rest.pyo-db-abc.svc.cluster.local".into()
            )
        );
        assert_eq!(v[2], ("KV_REST_API_TOKEN".into(), "tok".into()));
        assert_eq!(v.len(), 3, "no read-only token, no variable");
        let mut ro = db("valkey", false, true);
        ro.readonly_token = "sealed".into();
        assert_eq!(
            variables(&ro, "pw", "tok", "ro")[3],
            ("KV_REST_API_READ_ONLY_TOKEN".into(), "ro".into())
        );
        assert_eq!(
            variables(&db("valkey", false, false), "pw", "tok", "ro").len(),
            1
        );
        let mut pg = db("postgres", true, false);
        pg.read_replicas = 2;
        assert_eq!(
            variables(&pg, "pw", "", "")[2],
            (
                "DATABASE_URL_READ".into(),
                "postgres://app:pw@main-read-pooler.pyo-db-abc.svc.cluster.local:5432/app".into()
            )
        );
        let mut kv = db("valkey", false, false);
        kv.read_replicas = 1;
        assert_eq!(
            variables(&kv, "pw", "", "").len(),
            1,
            "KV reads go through REDIS_URL"
        );
        assert!(
            check_read_replicas("postgres", 5).is_ok()
                && check_read_replicas("postgres", 6).is_err()
        );
        assert!(
            check_read_replicas("valkey", 4).is_ok()
                && check_read_replicas("valkey", 5).is_err()
                && check_read_replicas("valkey", 0).is_ok()
        );
    }

    #[test]
    fn mysql() {
        let mut my = db("mysql", false, false);
        my.version = "8.4".into();
        let v = variables(&my, "pw", "", "");
        let keys: Vec<&str> = v.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(
            keys,
            [
                "DATABASE_URL",
                "MYSQL_HOST",
                "MYSQL_PORT",
                "MYSQL_USER",
                "MYSQL_PASSWORD",
                "MYSQL_DATABASE"
            ]
        );
        assert_eq!(
            v[0].1,
            "mysql://app:pw@main-primary.pyo-db-abc.svc.cluster.local:3306/app"
        );
        assert_eq!(
            (
                v[1].1.as_str(),
                v[2].1.as_str(),
                v[3].1.as_str(),
                v[4].1.as_str(),
                v[5].1.as_str()
            ),
            (
                "main-primary.pyo-db-abc.svc.cluster.local",
                "3306",
                "app",
                "pw",
                "app"
            )
        );
        my.instances = 3;
        my.read_replicas = 2;
        my.pooler = true;
        assert_eq!(
            variables(&my, "pw", "", ""),
            v,
            "no pooler or read address, whatever the row says"
        );
        // Instances: 1, 3 or 5.
        for n in [1, 3, 5] {
            assert!(check_instances("mysql", n).is_ok(), "{n}");
        }
        for n in [0, 2, 4, 6, 9] {
            assert!(check_instances("mysql", n).is_err(), "{n}");
        }
        assert_eq!(
            check_instances("mysql", 4).unwrap_err().message,
            "A MySQL database runs on a single server (instances 1), or with 2 or 4 replicas (instances 3 or 5)."
        );
        assert!(
            !check_instances("postgres", 2)
                .unwrap_err()
                .message
                .contains("node")
        );
        // It only grows.
        assert!(
            check_mysql_grows("mysql", 1, 3).is_ok()
                && check_mysql_grows("mysql", 3, 5).is_ok()
                && check_mysql_grows("mysql", 3, 3).is_ok()
        );
        assert!(
            check_mysql_grows("mysql", 5, 3)
                .unwrap_err()
                .message
                .contains("only grow")
        );
        assert!(
            check_mysql_grows("mysql", 3, 1).is_err()
                && check_mysql_grows("postgres", 3, 1).is_ok()
        );
        // 1 GiB at least: no small preset, custom from 1024 MiB.
        assert!(check_preset("mysql", "small").is_err());
        assert!(
            check_preset("mysql", "medium").is_ok() && check_preset("postgres", "small").is_ok()
        );
        assert!(
            check_custom("mysql", 500, 768).is_err() && check_custom("mysql", 500, 1024).is_ok()
        );
        assert!(check_read_replicas("mysql", 0).is_ok());
        assert_eq!(
            check_read_replicas("mysql", 1).unwrap_err().message,
            "A MySQL database has no read replicas."
        );
        assert!(check_engine("mysql").is_ok() && check_engine("mariadb").is_err());
        assert_eq!(versions("mysql"), ["8.4"]);
        assert!(point_in_time("mysql") && point_in_time("postgres") && !point_in_time("valkey"));
    }

    #[test]
    fn variables_under_a_service_s_names() {
        let my = db("mysql", false, false);
        let vars = variables(&my, "pw", "", "");
        let env: EnvMap = [
            ("WORDPRESS_DB_HOST", "MYSQL_HOST:MYSQL_PORT"),
            ("WORDPRESS_DB_PASSWORD", "MYSQL_PASSWORD"),
            ("database__connection__user", "MYSQL_USER"),
        ]
        .into_iter()
        .map(|(a, b)| (a.to_owned(), b.to_owned()))
        .collect();
        let out = mapped(&my, vars.clone(), Some(&env)).unwrap();
        assert_eq!(
            out,
            [
                (
                    "WORDPRESS_DB_HOST".to_owned(),
                    "main-primary.pyo-db-abc.svc.cluster.local:3306".to_owned()
                ),
                ("WORDPRESS_DB_PASSWORD".to_owned(), "pw".to_owned()),
                ("database__connection__user".to_owned(), "app".to_owned()),
            ]
        );
        assert_eq!(
            mapped(&my, vars.clone(), None).unwrap(),
            vars,
            "without names, all as they are"
        );
        assert_eq!(
            mapped(&my, vars.clone(), Some(&EnvMap::new())).unwrap(),
            vars
        );
        let bad_from: EnvMap = [("X".to_owned(), "REDIS_URL".to_owned())].into();
        assert!(
            mapped(&my, vars.clone(), Some(&bad_from))
                .unwrap_err()
                .message
                .contains("main has no variable REDIS_URL")
        );
        let bad_key: EnvMap = [("1X".to_owned(), "MYSQL_HOST".to_owned())].into();
        assert!(mapped(&my, vars, Some(&bad_key)).is_err());
    }

    #[test]
    fn parts_of_the_address_for_templates() {
        let pg = db("postgres", true, false);
        let keys: Vec<String> = parts(&pg, "pw").into_iter().map(|(k, _)| k).collect();
        assert_eq!(
            keys,
            [
                "POSTGRES_HOST",
                "POSTGRES_PORT",
                "POSTGRES_USER",
                "POSTGRES_PASSWORD",
                "POSTGRES_DATABASE"
            ]
        );
        assert_eq!(
            parts(&pg, "pw")[0].1,
            "main-rw.pyo-db-abc.svc.cluster.local",
            "the primary, not the pooler"
        );
        let kv = parts(&db("valkey", false, false), "pw");
        assert_eq!(
            kv,
            [
                (
                    "REDIS_HOST".into(),
                    "main.pyo-db-abc.svc.cluster.local".into()
                ),
                ("REDIS_PORT".into(), "6379".into()),
                ("REDIS_PASSWORD".into(), "pw".into())
            ]
        );
        assert!(parts(&db("mysql", false, false), "pw").is_empty());
        let env: EnvMap = [(
            "POSTGRES_ADDR".to_owned(),
            "POSTGRES_HOST:POSTGRES_PORT".to_owned(),
        )]
        .into();
        let all = [variables(&pg, "pw", "", ""), parts(&pg, "pw")].concat();
        assert_eq!(
            mapped(&pg, all, Some(&env)).unwrap(),
            [(
                "POSTGRES_ADDR".to_owned(),
                "main-rw.pyo-db-abc.svc.cluster.local:5432".to_owned()
            )]
        );
    }

    #[test]
    fn names_that_meet_another_database() {
        let team = |v: &[(&str, &str)]| {
            v.iter()
                .map(|(e, n)| (e.to_string(), n.to_string()))
                .collect::<Vec<_>>()
        };
        let others = team(&[
            ("mysql", "shop"),
            ("postgres", "orders"),
            ("valkey", "cache"),
        ]);
        for (engine, name, other) in [
            ("postgres", "shop-primary", "shop"),
            ("valkey", "shop-grpc", "shop"),
            ("mysql", "orders-rw", "orders"),
            ("valkey", "orders-read", "orders"),
            ("postgres", "orders-r", "orders"),
            ("mysql", "orders-pooler", "orders"),
            ("postgres", "cache-kv", "cache"),
            ("mysql", "cache-sentinel", "cache"),
            ("postgres", "cache-primary", "cache"),
        ] {
            let e = name_conflict(engine, name, &others).unwrap_err();
            assert_eq!(
                e.message,
                format!("The name conflicts with the database {other}."),
                "{engine} {name}"
            );
        }
        // The other way round: a new database whose parts are another's name.
        let e = name_conflict("valkey", "app", &team(&[("postgres", "app-sentinel")])).unwrap_err();
        assert_eq!(
            e.message,
            "The name conflicts with the database app-sentinel."
        );
        assert!(name_conflict("postgres", "default", &[]).is_err());
        for name in ["shops", "shop2", "orders-archive", "caches"] {
            assert!(name_conflict("postgres", name, &others).is_ok(), "{name}");
        }
    }

    fn report(name: &str, id: &str) -> DatabaseReport {
        DatabaseReport {
            namespace: "pyo-db-t".into(),
            name: name.into(),
            id: id.into(),
            status: json!({}),
        }
    }

    fn keys(names: &[&str]) -> std::collections::HashSet<(String, String)> {
        names
            .iter()
            .map(|n| ("pyo-db-t".to_owned(), (*n).to_owned()))
            .collect()
    }

    fn key(name: &str) -> (String, String) {
        ("pyo-db-t".into(), name.into())
    }

    #[test]
    fn cleanup_retries_deletions_until_the_cluster_has_none() {
        let deleting = vec![key("gone"), key("stuck"), key("busy")];
        let known = keys(&["gone", "stuck", "busy", "live"]);
        let reports = [
            report("stuck", "mdb_stuck"),
            report("busy", "mdb_busy"),
            report("live", "mdb_live"),
        ];
        let plan = plan_cleanup(
            &reports,
            true,
            &deleting,
            &known,
            &Default::default(),
            &keys(&["busy"]),
        );
        // Not on the cluster any more: the row goes. Still there, nothing on
        // its way: asked again. On its way: left to it.
        assert_eq!(plan.forget, vec![key("gone")]);
        assert_eq!(plan.delete, vec![key("stuck")]);
        assert!(plan.unknown.is_empty());
    }

    #[test]
    fn cleanup_needs_the_whole_list_to_call_one_gone() {
        // An older agent, or a failed list: missing isn't gone.
        let plan = plan_cleanup(
            &[],
            false,
            &[key("gone")],
            &keys(&["gone"]),
            &Default::default(),
            &Default::default(),
        );
        assert!(plan.forget.is_empty());
        assert_eq!(plan.delete, vec![key("gone")]);
    }

    /// The e2e orphans: rows gone (the agent confirmed), then a late apply
    /// brought the Databases back. Those Pyo deleted are deleted again;
    /// others, never.
    #[test]
    fn cleanup_deletes_only_databases_pyo_deleted() {
        let reports = [
            report("e2e-pg", "mdb_e2epg00001"),
            report("e2e-cancel-db", "mdb_e2ecancel01"),
            report("hand-made", ""),
            report("restored", "mdb_other"),
            report("test-pg", "mdb_live"),
        ];
        let deleted = ["mdb_e2epg00001".to_owned(), "mdb_e2ecancel01".to_owned()].into();
        let plan = plan_cleanup(
            &reports,
            true,
            &[],
            &keys(&["test-pg"]),
            &deleted,
            &keys(&["e2e-cancel-db"]),
        );
        assert_eq!(plan.delete, vec![key("e2e-pg")]);
        assert!(plan.forget.is_empty());
        assert_eq!(
            plan.unknown,
            vec![
                ("pyo-db-t".into(), "hand-made".into(), String::new()),
                ("pyo-db-t".into(), "restored".into(), "mdb_other".into())
            ]
        );
    }

    #[test]
    fn cleanup_leaves_a_name_in_use_again_alone() {
        // Deleted, then a new database with the same name: Pyo's again.
        let plan = plan_cleanup(
            &[report("main", "mdb_old")],
            true,
            &[],
            &keys(&["main"]),
            &["mdb_old".to_owned()].into(),
            &Default::default(),
        );
        assert_eq!(plan, Cleanup::default());
    }

    /// Against a real Postgres (DATABASE_URL, a throwaway one): the
    /// deletion is asked until confirmed, the row goes with its id kept, and
    /// a Database brought back with that id is deleted again.
    #[tokio::test]
    #[ignore = "needs DATABASE_URL (a throwaway Postgres)"]
    async fn deletions_are_retried_and_orphans_cleaned_up() {
        let pool = PgPool::connect(&std::env::var("DATABASE_URL").unwrap())
            .await
            .unwrap();
        crate::migrate::run(&pool).await.unwrap();
        crate::vault::init(crate::vault::Vault::new(
            pool.clone(),
            crate::vault::Provider::from_env(),
        ));
        let run = crate::ids::new_id("t");
        let (team, cluster, ns) = (
            format!("team_{run}"),
            format!("cl_{run}"),
            format!("pyo-db-{run}"),
        );
        sqlx::query("INSERT INTO pyo.teams (id, name, slug) VALUES ($1, $1, $1)")
            .bind(&team)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO pyo.clusters (id, team_id, name, agent_token_hash) VALUES ($1, $2, $1, $1)").bind(&cluster).bind(&team).execute(&pool).await.unwrap();
        for (id, name, state) in [("mdb_a", "a", "deleting"), ("mdb_b", "b", "active")] {
            sqlx::query("INSERT INTO pyo.databases (id, team_id, cluster_id, name, engine, version, size, storage_gi, instances, namespace, password, state) VALUES ($1 || $5, $2, $3, $4, 'postgres', '18', 'small', 5, 1, $6, '', $7)")
                .bind(id).bind(&team).bind(&cluster).bind(name).bind(&run).bind(&ns).bind(state)
                .execute(&pool).await.unwrap();
        }
        let a_id = format!("mdb_a{run}");
        let rep = |name: &str, id: &str| DatabaseReport {
            namespace: ns.clone(),
            name: name.into(),
            id: id.into(),
            status: json!({}),
        };
        let deletes = || async {
            sqlx::query_scalar::<_, i64>("SELECT count(*) FROM pyo.commands WHERE cluster_id = $1 AND command->>'kind' = 'delete_database' AND command->>'name' = 'a'").bind(&cluster).fetch_one(&pool).await.unwrap()
        };
        // A pending change of "a" goes with its deletion.
        let mut tx = pool.begin().await.unwrap();
        deploy::enqueue(
            &mut tx,
            &cluster,
            Command::DeleteDatabase {
                namespace: "x".into(),
                name: "unrelated".into(),
            },
        )
        .await
        .unwrap();
        sqlx::query("INSERT INTO pyo.commands (id, cluster_id, command) VALUES ($1, $2, $3)")
            .bind(format!("cmd_apply{run}"))
            .bind(&cluster)
            .bind(json!({ "kind": "apply_database", "namespace": ns, "name": "a" }))
            .execute(&mut *tx)
            .await
            .unwrap();
        drop_pending_applies(&mut tx, &cluster, &ns, "a")
            .await
            .unwrap();
        tx.commit().await.unwrap();
        let st: String = sqlx::query_scalar("SELECT status FROM pyo.commands WHERE id = $1")
            .bind(format!("cmd_apply{run}"))
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(st, "failed");

        // "a" deleting, still on the cluster, nothing on its way: asked once.
        for _ in 0..2 {
            let mut tx = pool.begin().await.unwrap();
            reported(&mut tx, &cluster, &[rep("a", &a_id), rep("b", "")], true)
                .await
                .unwrap();
            tx.commit().await.unwrap();
        }
        assert_eq!(deletes().await, 1);
        // The agent failed it; tried long ago: asked again.
        sqlx::query("UPDATE pyo.commands SET status = 'failed', created_at = now() - interval '5 minutes' WHERE cluster_id = $1 AND command->>'name' = 'a'").bind(&cluster).execute(&pool).await.unwrap();
        let mut tx = pool.begin().await.unwrap();
        reported(&mut tx, &cluster, &[rep("a", &a_id)], true)
            .await
            .unwrap();
        tx.commit().await.unwrap();
        assert_eq!(deletes().await, 2);
        // Confirmed: the row goes, its id is kept.
        let mut tx = pool.begin().await.unwrap();
        finished(
            &mut tx,
            &cluster,
            &json!({ "kind": "delete_database", "namespace": ns, "name": "a" }),
            true,
            None,
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();
        let left: i64 = sqlx::query_scalar("SELECT count(*) FROM pyo.databases WHERE id = $1")
            .bind(&a_id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(left, 0);
        // Back on the cluster (a late apply): deleted again, once the last try is old.
        sqlx::query("UPDATE pyo.commands SET status = 'done', created_at = now() - interval '5 minutes' WHERE cluster_id = $1 AND command->>'name' = 'a'").bind(&cluster).execute(&pool).await.unwrap();
        let mut tx = pool.begin().await.unwrap();
        reported(
            &mut tx,
            &cluster,
            &[
                rep("a", &a_id),
                rep("b", ""),
                rep("stranger", "mdb_unknown"),
            ],
            true,
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();
        assert_eq!(deletes().await, 3);
        let strangers: i64 = sqlx::query_scalar("SELECT count(*) FROM pyo.commands WHERE cluster_id = $1 AND command->>'name' = 'stranger'").bind(&cluster).fetch_one(&pool).await.unwrap();
        assert_eq!(strangers, 0);
        // A row being deleted that the cluster no longer has goes on a heartbeat.
        sqlx::query("UPDATE pyo.databases SET state = 'deleting' WHERE id = $1")
            .bind(format!("mdb_b{run}"))
            .execute(&pool)
            .await
            .unwrap();
        let mut tx = pool.begin().await.unwrap();
        reported(&mut tx, &cluster, &[], true).await.unwrap();
        tx.commit().await.unwrap();
        let left: i64 =
            sqlx::query_scalar("SELECT count(*) FROM pyo.databases WHERE cluster_id = $1")
                .bind(&cluster)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(left, 0);
        let kept: i64 =
            sqlx::query_scalar("SELECT count(*) FROM pyo.deleted_databases WHERE cluster_id = $1")
                .bind(&cluster)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(kept, 2);
    }
}
