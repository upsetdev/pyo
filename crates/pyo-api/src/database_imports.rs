//! Importing into a built-in Postgres or MySQL database (databases.rs):
//! from another database, by its URL, or from a dump file uploaded here.
//! Owners and admins.
//!
//! - **Where:** into a new database (made for it), or into one of the
//!   team's that is empty.
//! - **Before:** the cluster's agent runs a short check Job
//!   (`CheckDatabaseImport`): it connects to the source as the import will
//!   (reachable, the login, TLS, the version, its extensions, every table
//!   readable) and makes sure the database to import into is empty. A file
//!   was looked at as it was uploaded (its format, the version it was dumped
//!   from, the extensions it makes).
//! - **Extensions:** Postgres extensions the source has that Pyo turns on
//!   ("managed") are turned on first; the "additional" ones the import makes
//!   itself; any other is refused, by name.
//! - **How:** the Database's `spec.import` (pyo-db-controller's import.rs):
//!   CloudNativePG's own import for a new Postgres database from a URL, a
//!   Job otherwise. Its progress is the database's `import` (from the
//!   controller's status, each heartbeat), its output in the database's logs.
//! - **Secrets:** the source's password is sealed with the team's key here
//!   (and in the command queue); it reaches the cluster in the Secret
//!   `<name>-import`, which goes once the import is done. It's dropped here
//!   once it succeeded (kept sealed after a failure, for a retry).
//! - **Files:** uploaded in two steps: `POST /database-imports/uploads`
//!   (the name and size, checked up front) gives a URL (`upload_target`) and
//!   a one-time token; the file PUT there is streamed into the team's bucket
//!   in Pyo's own store (backupstore.rs) under `imports/`, at most
//!   `PYO_DATABASE_IMPORT_MAX_BYTES` (10 GiB by default). The import's Job
//!   fetches it from here with a token of its own. A file goes once its
//!   import succeeded, or after `PYO_DATABASE_IMPORT_RETENTION_HOURS` (24)
//!   unless an import is using it; an upload never sent, once its URL
//!   expired.

use std::time::Duration;

use axum::{
    Json, Router,
    body::Body,
    extract::{DefaultBodyLimit, Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::Response,
    routing::{get, post, put},
};
use futures::StreamExt;
use pyo_proto::{
    Command, CommandResult, EnvVar, NamedSecret,
    dbimport::{Connection, ImportCheck, Target},
};
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::{PgPool, Postgres, Transaction};

use crate::{
    AppState,
    auth::{KeyAccess, SessionUser, User},
    deploy,
    error::{ApiError, ApiResult, ErrorBody},
    ops::{Role, log_activity, require_role},
};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/database-imports/check", post(check))
        .route("/database-imports", post(start))
        .route("/database-imports/{id}/retry", post(retry))
        .route("/database-imports/uploads", post(create_upload))
        .route(
            "/database-imports/uploads/{id}/content",
            put(upload_content).layer(DefaultBodyLimit::disable()),
        )
}

/// For the import Jobs on the clusters: the uploaded dump.
pub fn agent_routes() -> Router<AppState> {
    Router::new().route("/database-imports/{id}/dump", get(dump))
}

const GIB: u64 = 1 << 30;
/// Parts of an upload, as they go to the store.
const PART: usize = 16 << 20;
/// What of a file is read for its extensions and version (its start).
const SNIFF: usize = 16 << 20;

fn max_bytes() -> u64 {
    std::env::var("PYO_DATABASE_IMPORT_MAX_BYTES")
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .filter(|n| *n > 0)
        .unwrap_or(10 * GIB)
}

fn retention_hours() -> i64 {
    std::env::var("PYO_DATABASE_IMPORT_RETENTION_HOURS")
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .filter(|n| *n > 0)
        .unwrap_or(24)
}

/// What a database that isn't empty has: "1 table, view or function",
/// "3 tables, views or functions".
fn objects_text(n: i64) -> String {
    if n == 1 {
        "1 table, view or function".into()
    } else {
        format!("{n} tables, views or functions")
    }
}

/// "10 GiB", "100 MiB".
fn size_text(b: u64) -> String {
    if b >= GIB && b.is_multiple_of(GIB) {
        format!("{} GiB", b / GIB)
    } else if b >= GIB {
        format!("{:.1} GiB", b as f64 / GIB as f64)
    } else {
        format!("{} MiB", b.div_ceil(1 << 20))
    }
}

fn bad(m: impl Into<String>) -> ApiError {
    ApiError::bad_request(m)
}

/// Owners and admins, with a key that may change things.
async fn admin(tx: &mut Transaction<'_, Postgres>, user: &User, team_id: &str) -> ApiResult<()> {
    if user
        .key
        .as_ref()
        .is_some_and(|k| k.access != KeyAccess::Full)
    {
        return Err(ApiError::forbidden(
            "Importing needs an API key with full access.",
        ));
    }
    require_role(tx, user, team_id, Role::Admin)
        .await
        .map(|_| ())
}

// ── The source ─────────────────────────────────────────────────────────────

fn decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && i + 2 < b.len()
            && let Ok(v) =
                u8::from_str_radix(std::str::from_utf8(&b[i + 1..i + 3]).unwrap_or("zz"), 16)
        {
            out.push(v);
            i += 3;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// A postgres:// or mysql:// URL: its connection and password.
pub fn parse_url(engine: &str, url: &str) -> ApiResult<(Connection, String)> {
    let example = if engine == "postgres" {
        "postgres://user:password@host:5432/database"
    } else {
        "mysql://user:password@host:3306/database"
    };
    let u = reqwest::Url::parse(url.trim())
        .map_err(|_| bad(format!("That isn't a URL. It looks like {example}.")))?;
    let schemes: &[&str] = if engine == "postgres" {
        &["postgres", "postgresql"]
    } else {
        &["mysql"]
    };
    if !schemes.contains(&u.scheme()) {
        return Err(bad(format!(
            "A {} URL starts with {}://, like {example}.",
            if engine == "postgres" {
                "Postgres"
            } else {
                "MySQL"
            },
            schemes[0]
        )));
    }
    let host = u
        .host_str()
        .unwrap_or("")
        .trim_matches(['[', ']'])
        .to_owned();
    if host.is_empty() {
        return Err(bad(format!(
            "The URL has no host. It looks like {example}."
        )));
    }
    let user = decode(u.username());
    if user.is_empty() {
        return Err(bad(format!(
            "The URL has no user. It looks like {example}."
        )));
    }
    let password = u.password().map(decode).unwrap_or_default();
    let database = decode(u.path().trim_start_matches('/'));
    if database.is_empty() || database.contains('/') {
        return Err(bad(format!(
            "Add the database to copy to the URL's end: {example}."
        )));
    }
    let mut tls = String::new();
    for (k, v) in u.query_pairs() {
        let v = v.to_ascii_lowercase().replace('_', "-");
        let mode = match (k.as_ref(), v.as_str()) {
            ("sslmode" | "ssl-mode" | "ssl_mode", "disable" | "disabled") => "disable",
            ("sslmode" | "ssl-mode" | "ssl_mode", "allow" | "prefer" | "preferred") => "prefer",
            ("sslmode" | "ssl-mode" | "ssl_mode", "require" | "required") => "require",
            ("sslmode" | "ssl-mode" | "ssl_mode", "verify-ca") => "verify-ca",
            ("sslmode" | "ssl-mode" | "ssl_mode", "verify-full" | "verify-identity") => {
                "verify-full"
            }
            ("ssl", "true" | "1") => "require",
            ("ssl", "false" | "0") => "disable",
            ("sslaccept", "strict") => "verify-full",
            ("sslmode" | "ssl-mode" | "ssl_mode", other) => {
                return Err(bad(format!(
                    "The URL's sslmode {other} isn't one Pyo knows: disable, prefer, require, verify-ca or verify-full."
                )));
            }
            _ => continue,
        };
        tls = mode.to_owned();
    }
    let port = u
        .port()
        .unwrap_or(if engine == "postgres" { 5432 } else { 3306 });
    Ok((
        Connection {
            host,
            port,
            user,
            database,
            tls,
        },
        password,
    ))
}

/// Where it's from, for people: "shop on db.example.com".
fn label_of(c: &Connection) -> String {
    format!("{} on {}:{}", c.database, c.host, c.port)
}

// ── Asking the cluster ─────────────────────────────────────────────────────

/// Runs the check on the cluster and waits for it (the Job pulls its image
/// the first time): what it found, or why it failed.
async fn run_check(s: &AppState, cluster_id: &str, check: ImportCheck) -> ApiResult<Value> {
    let mut tx = s.db.begin().await?;
    let cmd_id = deploy::enqueue(
        &mut tx,
        cluster_id,
        Command::CheckDatabaseImport(Box::new(check)),
    )
    .await?;
    tx.commit().await?;
    s.wake(cluster_id).await;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(200);
    let result = loop {
        let row: Option<(String, Option<Value>)> =
            sqlx::query_as("SELECT status, result FROM pyo.commands WHERE id = $1")
                .bind(&cmd_id)
                .fetch_optional(&s.db)
                .await?;
        if let Some((status, result)) = row
            && (status == "done" || status == "failed")
        {
            break result
                .and_then(|v| serde_json::from_value::<CommandResult>(v).ok())
                .unwrap_or_default();
        }
        if tokio::time::Instant::now() > deadline {
            let _ = sqlx::query("UPDATE pyo.commands SET command = command - 'env' WHERE id = $1")
                .bind(&cmd_id)
                .execute(&s.db)
                .await;
            return Err(ApiError::new(
                StatusCode::GATEWAY_TIMEOUT,
                "The cluster didn't finish checking in time. Is its agent connected?",
            ));
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    };
    // The password stays out of the command log, even sealed.
    sqlx::query("UPDATE pyo.commands SET command = command - 'env' WHERE id = $1")
        .bind(&cmd_id)
        .execute(&s.db)
        .await?;
    if !result.ok {
        return Err(ApiError::new(
            StatusCode::BAD_GATEWAY,
            format!(
                "Couldn't check the import: {}",
                result
                    .message
                    .unwrap_or_else(|| "the cluster couldn't run the check".into())
            ),
        ));
    }
    Ok(result.data.unwrap_or_default())
}

// ── Requests ───────────────────────────────────────────────────────────────

/// A new database to import into, as `createDatabase` takes it (the team's
/// owners and admins).
#[derive(Deserialize, utoipa::ToSchema, Clone)]
#[serde(rename_all = "camelCase")]
pub struct NewDatabase {
    /// A new id you choose: `mdb_` then 4 to 40 lowercase letters, digits, dashes or underscores.
    id: String,
    cluster_id: String,
    /// Lowercase letters, digits and dashes, unique in the team.
    name: String,
    /// "small", "medium", "large", "xlarge", or "custom" with `cpuMillis` and `memoryMi`.
    size: String,
    /// 1, or high availability as `createDatabase` takes it.
    instances: i32,
    /// Postgres "16", "17" or "18", by default the newest and never older than the source. MySQL "8.4".
    #[serde(default)]
    version: Option<String>,
    #[serde(default)]
    storage_gi: Option<i32>,
    /// Postgres: a connection pooler in front (on by default).
    #[serde(default)]
    pooler: Option<bool>,
    #[serde(default)]
    cpu_millis: Option<i32>,
    #[serde(default)]
    memory_mi: Option<i32>,
    #[serde(default)]
    read_replicas: Option<i32>,
    /// Postgres: more extensions Pyo turns on with it (the source's are turned on anyway).
    #[serde(default)]
    extensions: Option<Vec<String>>,
}

/// An import: where to (`databaseId`, or `newDatabase`), and from where
/// (`url`, or `uploadId`).
#[derive(Deserialize, utoipa::ToSchema, Clone)]
#[serde(rename_all = "camelCase")]
pub struct StartImport {
    team_id: String,
    /// "postgres" or "mysql".
    engine: String,
    /// An existing, empty database of the team's to import into.
    #[serde(default)]
    database_id: Option<String>,
    /// Or a new database, made for the import.
    #[serde(default)]
    new_database: Option<NewDatabase>,
    /// The database to copy, as a URL the cluster can reach: `postgres://user:password@host:5432/database` or `mysql://user:password@host:3306/database`, with `?sslmode=require` (or `ssl-mode=REQUIRED`), `verify-ca` or `verify-full` for TLS.
    #[serde(default)]
    url: Option<String>,
    /// Or a dump uploaded first (`createDatabaseDumpUpload`, then its file sent).
    #[serde(default)]
    upload_id: Option<String>,
    /// A new database made by an import from another platform: where it
    /// came from, saved with it. Set by platform_import, never by a request.
    #[serde(skip)]
    #[schema(ignore)]
    pub(crate) imported_from: Option<Value>,
}

/// The database imported into, as far as an import needs it.
struct Into_ {
    team_id: String,
    cluster_id: String,
    engine: String,
    version: String,
    namespace: String,
    name: String,
    /// Exists already (else it's made).
    existing: bool,
    extensions: Vec<String>,
}

/// An uploaded file, as stored.
#[derive(sqlx::FromRow)]
struct Upload {
    id: String,
    team_id: String,
    engine: Option<String>,
    file_name: String,
    size: i64,
    format: String,
    extensions: Value,
    source_version: String,
}

/// What the checks found and what will be done: the source's `version`,
/// its `sizeBytes`, the extensions turned on first, and the ones the import
/// makes itself.
struct Plan {
    found: Value,
    turn_on: Vec<String>,
    made_by_import: Vec<String>,
}

fn postgres_major(v: &str) -> Option<i64> {
    v.split('.').next()?.trim().parse().ok()
}

/// The checks of the source and the database imported into, as a plan, or
/// why it can't be done.
async fn plan(
    s: &AppState,
    user: &User,
    req: &StartImport,
) -> ApiResult<(Into_, Plan, Option<(Connection, String)>, Option<Upload>)> {
    if req.engine != "postgres" && req.engine != "mysql" {
        return Err(bad("Imports are into Postgres and MySQL databases."));
    }
    let mut tx = s.db.begin().await?;
    admin(&mut tx, user, &req.team_id).await?;
    let into = match (&req.database_id, &req.new_database) {
        (Some(id), None) => {
            let t = crate::databases::import_target(&mut tx, id).await?;
            if t.team_id != req.team_id {
                return Err(ApiError::not_found("Database"));
            }
            if t.engine != req.engine {
                return Err(bad(format!(
                    "{} isn't a {} database.",
                    t.name,
                    if req.engine == "postgres" {
                        "Postgres"
                    } else {
                        "MySQL"
                    }
                )));
            }
            if t.state != "active" {
                return Err(ApiError::conflict(format!("{} is being deleted.", t.name)));
            }
            if t.restored {
                return Err(ApiError::conflict(format!(
                    "{} was restored from backups, so it isn't empty. Import into a new database.",
                    t.name
                )));
            }
            if let Some(state) = latest_state(&mut tx, id).await?
                && (state == "pending" || state == "running")
            {
                return Err(ApiError::conflict(format!(
                    "{} is already importing.",
                    t.name
                )));
            }
            Into_ {
                team_id: t.team_id,
                cluster_id: t.cluster_id,
                engine: t.engine,
                version: t.version,
                namespace: t.namespace,
                name: t.name,
                existing: true,
                extensions: t.extensions,
            }
        }
        (None, Some(n)) => {
            let cluster_team: Option<String> =
                sqlx::query_scalar("SELECT team_id FROM pyo.clusters WHERE id = $1")
                    .bind(&n.cluster_id)
                    .fetch_optional(&mut *tx)
                    .await?;
            if cluster_team.as_deref() != Some(req.team_id.as_str()) {
                return Err(ApiError::not_found("Cluster"));
            }
            let taken: bool = sqlx::query_scalar(
                "SELECT EXISTS (SELECT 1 FROM pyo.databases WHERE team_id = $1 AND name = $2)",
            )
            .bind(&req.team_id)
            .bind(&n.name)
            .fetch_one(&mut *tx)
            .await?;
            if taken {
                return Err(ApiError::conflict(format!(
                    "The team already has a database called {}.",
                    n.name
                )));
            }
            Into_ {
                team_id: req.team_id.clone(),
                cluster_id: n.cluster_id.clone(),
                engine: req.engine.clone(),
                version: n.version.clone().unwrap_or_default(),
                namespace: crate::databases::namespace_for(&req.team_id),
                name: n.name.clone(),
                existing: false,
                extensions: n.extensions.clone().unwrap_or_default(),
            }
        }
        _ => {
            return Err(bad(
                "Give either databaseId (an empty database) or newDatabase.",
            ));
        }
    };
    let (source, upload) = match (&req.url, &req.upload_id) {
        (Some(url), None) => (Some(parse_url(&req.engine, url)?), None),
        (None, Some(id)) => {
            let u: Option<Upload> = sqlx::query_as("SELECT id, team_id, engine, file_name, size, format, extensions, source_version FROM pyo.database_import_uploads WHERE id = $1 AND state = 'uploaded'").bind(id).fetch_optional(&mut *tx).await?;
            let u = u.filter(|u| u.team_id == req.team_id).ok_or_else(|| {
                bad("No such upload, or its file isn't sent yet (or it went): upload it again.")
            })?;
            let engine = u.engine.as_deref().unwrap_or(if u.format == "dump" {
                "postgres"
            } else {
                req.engine.as_str()
            });
            if engine != req.engine {
                return Err(bad(format!(
                    "{} is a {} dump.",
                    u.file_name,
                    if engine == "postgres" {
                        "Postgres"
                    } else {
                        "MySQL"
                    }
                )));
            }
            (None, Some(u))
        }
        _ => {
            return Err(bad(
                "Give either url (the database to copy) or uploadId (an uploaded dump).",
            ));
        }
    };
    tx.commit().await?;

    // The cluster checks what it can reach.
    let mut found = json!({});
    if source.is_some() || into.existing {
        let target = into.existing.then(|| Target {
            host: format!(
                "{}-{}.{}.svc.cluster.local",
                into.name,
                if into.engine == "postgres" {
                    "rw"
                } else {
                    "primary"
                },
                into.namespace
            ),
            secret: format!("{}-credentials", into.name),
        });
        let check = ImportCheck {
            namespace: into.namespace.clone(),
            engine: into.engine.clone(),
            source: source.as_ref().map(|(c, _)| c.clone()),
            target,
            env: source
                .as_ref()
                .map(|(_, p)| {
                    vec![EnvVar {
                        key: "password".into(),
                        value: p.clone(),
                    }]
                })
                .unwrap_or_default(),
        };
        found = run_check(s, &into.cluster_id, check).await?;
    }
    if let Some(e) = found.get("error") {
        let message = e["message"]
            .as_str()
            .unwrap_or("The check failed.")
            .to_owned();
        return Err(bad(message));
    }
    if into.existing {
        let n = found["targetObjects"].as_i64().unwrap_or(-1);
        if n != 0 {
            return Err(ApiError::conflict(if n > 0 {
                format!(
                    "{} isn't empty: it has {}. Import only into an empty database, or into a new one.",
                    into.name,
                    objects_text(n)
                )
            } else {
                format!(
                    "Couldn't tell whether {} is empty. Is it running?",
                    into.name
                )
            }));
        }
    }

    // The version and the extensions.
    let mut extensions: Vec<String> = vec![];
    if let Some(u) = &upload {
        extensions = serde_json::from_value(u.extensions.clone()).unwrap_or_default();
        if !u.source_version.is_empty() {
            found["source"] = json!({ "version": u.source_version });
        }
    }
    if into.engine == "postgres" {
        if source.is_some() {
            if found["unreadable"].as_i64().unwrap_or(0) > 0 {
                return Err(bad(format!(
                    "The source's user can't read {} of its tables or sequences. Grant it SELECT on them, or use a user that can read everything.",
                    found["unreadable"]
                )));
            }
            extensions = serde_json::from_value(found["extensions"].clone()).unwrap_or_default();
            let num = found["source"]["versionNum"].as_i64().unwrap_or(0);
            if num > 0 && num < 90_200 {
                return Err(bad(format!(
                    "The source runs Postgres {}, older than 9.2, which pg_dump can't copy. Upgrade it first.",
                    found["source"]["version"].as_str().unwrap_or("")
                )));
            }
        }
        let source_major = found["source"]["version"]
            .as_str()
            .and_then(postgres_major)
            .or_else(|| found["source"]["major"].as_i64());
        let newest: i64 = crate::databases::POSTGRES_VERSIONS
            .last()
            .and_then(|v| v.parse().ok())
            .unwrap_or(18);
        if let Some(m) = source_major {
            if m > newest {
                return Err(bad(format!(
                    "The source runs Postgres {m}, newer than Pyo's newest ({newest}). Its dump can't be read into an older Postgres."
                )));
            }
            if let Some(v) = postgres_major(&into.version)
                && v < m
            {
                return Err(bad(if into.existing {
                    format!(
                        "The source runs Postgres {m} and {} runs {v}. Import into a database on Postgres {m} or newer.",
                        into.name
                    )
                } else {
                    format!("The source runs Postgres {m}: pick version {m} or newer.")
                }));
            }
        }
    } else if let Some(src) = found.get("source").filter(|v| v.is_object()) {
        if src["mariadb"].as_bool().unwrap_or(false)
            || src["version"]
                .as_str()
                .is_some_and(|v| v.to_ascii_lowercase().contains("mariadb"))
        {
            return Err(bad(format!(
                "The source is MariaDB ({}). Pyo imports from MySQL 5.7 to 8.4.",
                src["version"].as_str().unwrap_or("")
            )));
        }
        let v = src["version"].as_str().unwrap_or("");
        let mut parts = v.split(['.', '-']).filter_map(|p| p.parse::<i64>().ok());
        let (major, minor) = (parts.next().unwrap_or(0), parts.next().unwrap_or(0));
        if major > 8 || (major == 8 && minor > 4) {
            return Err(bad(format!(
                "The source runs MySQL {v}, newer than this database's 8.4. Its dump can't be loaded into an older MySQL."
            )));
        }
        if major > 0 && (major < 5 || (major == 5 && minor < 7)) {
            return Err(bad(format!(
                "The source runs MySQL {v}. Pyo imports from MySQL 5.7 to 8.4."
            )));
        }
    }
    let catalog = crate::databases::extension_catalog();
    let mut turn_on = vec![];
    let mut made_by_import = vec![];
    let mut missing = vec![];
    for e in extensions.iter().filter(|e| e.as_str() != "plpgsql") {
        match catalog
            .iter()
            .find(|c| c.engine == "postgres" && &c.name == e)
        {
            Some(c) if c.section == "managed" => {
                if !into.extensions.contains(e) {
                    turn_on.push(e.clone());
                }
            }
            Some(_) => made_by_import.push(e.clone()),
            None => missing.push(e.clone()),
        }
    }
    if !missing.is_empty() {
        return Err(bad(format!(
            "The source uses {} {}, which Pyo's Postgres doesn't have. Remove {} from the source first (DROP EXTENSION), then import.",
            if missing.len() == 1 {
                "the extension"
            } else {
                "the extensions"
            },
            missing.join(", "),
            if missing.len() == 1 { "it" } else { "them" }
        )));
    }
    Ok((
        into,
        Plan {
            found,
            turn_on,
            made_by_import,
        },
        source,
        upload,
    ))
}

fn plan_json(p: &Plan) -> Value {
    json!({
        "source": p.found.get("source").cloned().unwrap_or(Value::Null),
        "extensions": { "turnedOn": p.turn_on, "madeByImport": p.made_by_import },
    })
}

/// Check an import
///
/// Everything `startDatabaseImport` checks, without importing. The cluster
/// connects to the source, which must be reachable from there, and checks
/// the login, TLS, its version, its extensions and that every table is
/// readable. It also checks that the database to import into is empty.
/// Owners and admins.
#[utoipa::path(post, path = "/database-imports/check", operation_id = "checkDatabaseImport", tag = "Databases",
    request_body = StartImport,
    responses(
        (status = 200, description = "It can be imported: `source` (`version`, `sizeBytes`), and `extensions`: `turnedOn` (Pyo turns them on first) and `madeByImport`.", body = Object),
        (status = 400, description = "Why not: the URL, the source can't be reached or refuses the login, TLS, its version, an extension Pyo's Postgres doesn't have.", body = ErrorBody),
        (status = 409, description = "The database to import into isn't empty, or is importing.", body = ErrorBody),
        (status = 504, description = "The cluster didn't answer in time (is it connected?).", body = ErrorBody),
    ),
    extensions(("x-pyo-access" = json!("full"))))]
pub async fn check(
    State(s): State<AppState>,
    SessionUser(user): SessionUser,
    Json(req): Json<StartImport>,
) -> ApiResult<Json<Value>> {
    let (_, p, _, _) = plan(&s, &user, &req).await?;
    let mut out = plan_json(&p);
    out["ok"] = json!(true);
    Ok(Json(out))
}

/// Import into a database
///
/// Copies a Postgres or MySQL database into one of the team's: a new one
/// (`newDatabase`), or an empty one (`databaseId`). From `url` (the source
/// must be reachable from the cluster) or `uploadId` (a dump uploaded
/// first). It's checked first, as `checkDatabaseImport`. Then the
/// database's `import` shows how it goes ("pending", "running", "succeeded"
/// or "failed", with a `phase`), and its logs have the import's output.
/// Writes made to the source during the import aren't copied. Owners and
/// admins.
#[utoipa::path(post, path = "/database-imports", operation_id = "startDatabaseImport", tag = "Databases",
    request_body = StartImport,
    responses(
        (status = 200, description = "Started: `importId`, `databaseId`, `clusterId`, and what the check found (`source`, `extensions`).", body = Object),
        (status = 400, description = "Why not (as `checkDatabaseImport`).", body = ErrorBody),
        (status = 403, description = "You aren't an owner or admin of the team, or the API key can't make changes.", body = ErrorBody),
        (status = 409, description = "The database isn't empty or is importing, or the name is taken.", body = ErrorBody),
        (status = 504, description = "The cluster didn't answer in time (is it connected?).", body = ErrorBody),
    ),
    extensions(("x-pyo-access" = json!("full"))))]
pub async fn start(
    State(s): State<AppState>,
    SessionUser(user): SessionUser,
    Json(req): Json<StartImport>,
) -> ApiResult<Json<Value>> {
    let (into, p, source, upload) = plan(&s, &user, &req).await?;
    let mut tx = s.db.begin().await?;
    admin(&mut tx, &user, &req.team_id).await?;
    let database_id = match (&req.database_id, &req.new_database) {
        (Some(id), _) => id.clone(),
        (None, Some(n)) => n.id.clone(),
        _ => unreachable!("checked in plan"),
    };
    let block = Block::new(
        &into,
        &database_id,
        source.as_ref().map(|(c, _)| c),
        upload.as_ref(),
        !into.existing,
    );
    let id = block.id.clone();
    insert(
        &mut tx,
        &into.team_id,
        &database_id,
        &block,
        source.as_ref().map(|(_, pw)| pw.as_str()),
        upload.as_ref(),
        !into.existing,
        &user.name,
    )
    .await?;
    if let Some(n) = &req.new_database {
        // Without one, the newest: never older than the source (checked).
        let version = n.version.clone();
        let mut extensions = n.extensions.clone().unwrap_or_default();
        extensions.extend(p.turn_on.iter().cloned());
        crate::databases::create(
            &s,
            &mut tx,
            &user,
            crate::databases::Create {
                id: n.id.clone(),
                team_id: req.team_id.clone(),
                cluster_id: n.cluster_id.clone(),
                name: n.name.clone(),
                engine: into.engine.clone(),
                size: n.size.clone(),
                instances: n.instances,
                version,
                storage_gi: n.storage_gi,
                pooler: n.pooler,
                rest: false,
                restore: None,
                restored_from: None,
                cpu_millis: n.cpu_millis,
                memory_mi: n.memory_mi,
                read_replicas: n.read_replicas,
                extensions,
                imported_from: req.imported_from.clone(),
            },
        )
        .await?;
    } else {
        crate::databases::import_into(&s, &mut tx, &database_id, &p.turn_on).await?;
    }
    let target = json!({ "kind": "database", "id": database_id, "label": into.name });
    log_activity(
        &mut tx,
        &into.team_id,
        &user.name,
        "database.import_started",
        "info",
        &format!("Importing {} into {}", block.from, into.name),
        target,
        json!({ "clusterId": into.cluster_id, "importId": id }),
    )
    .await?;
    tx.commit().await?;
    s.wake(&into.cluster_id).await;
    let mut out = plan_json(&p);
    out["ok"] = json!(true);
    out["importId"] = json!(id);
    out["databaseId"] = json!(database_id);
    out["clusterId"] = json!(into.cluster_id);
    Ok(Json(out))
}

/// Retry an import
///
/// Runs a failed import again, from the same source. After a failure, Pyo
/// keeps the URL's password (sealed), and an uploaded dump for
/// `PYO_DATABASE_IMPORT_RETENTION_HOURS` (24 by default). A new database
/// made for the import is made again. A database that already existed must
/// still be empty. Owners and admins.
#[utoipa::path(post, path = "/database-imports/{id}/retry", operation_id = "retryDatabaseImport", tag = "Databases",
    params(("id" = String, Path, description = "The import's id (the database's `import.id`).")),
    responses(
        (status = 200, description = "Started again: the new `importId`.", body = Object),
        (status = 400, description = "Why not (as `checkDatabaseImport`).", body = ErrorBody),
        (status = 409, description = "It didn't fail, there's a newer one, or the dump is gone.", body = ErrorBody),
    ),
    extensions(("x-pyo-access" = json!("full"))))]
pub async fn retry(
    State(s): State<AppState>,
    SessionUser(user): SessionUser,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    let mut tx = s.db.begin().await?;
    type Row = (
        String,
        String,
        String,
        String,
        Value,
        String,
        Option<String>,
        bool,
    );
    let row: Option<Row> =
        sqlx::query_as("SELECT team_id, database_id, state, source, spec, password, upload_id, new_database FROM pyo.database_imports WHERE id = $1").bind(&id).fetch_optional(&mut *tx).await?;
    let (team_id, database_id, state, source, spec, sealed, upload_id, new_database) =
        row.ok_or_else(|| ApiError::not_found("Import"))?;
    admin(&mut tx, &user, &team_id).await?;
    let latest: Option<String> = sqlx::query_scalar("SELECT id FROM pyo.database_imports WHERE database_id = $1 ORDER BY created_at DESC LIMIT 1").bind(&database_id).fetch_optional(&mut *tx).await?;
    if latest.as_deref() != Some(id.as_str()) {
        return Err(ApiError::conflict(
            "A newer import of this database exists: retry that one.",
        ));
    }
    if state != "failed" {
        return Err(ApiError::conflict("Only a failed import can be retried."));
    }
    let t = crate::databases::import_target(&mut tx, &database_id).await?;
    let password = if source == "url" {
        crate::vault::get().open(&sealed).await?
    } else {
        String::new()
    };
    let upload: Option<Upload> = match &upload_id {
        Some(u) => sqlx::query_as("SELECT id, team_id, engine, file_name, size, format, extensions, source_version FROM pyo.database_import_uploads WHERE id = $1 AND state = 'uploaded'").bind(u).fetch_optional(&mut *tx).await?,
        None => None,
    };
    if source == "file" && upload.is_none() {
        return Err(ApiError::conflict(format!(
            "The dump is gone: files are kept {}. Upload it again.",
            match retention_hours() {
                1 => "1 hour".to_owned(),
                h => format!("{h} hours"),
            }
        )));
    }
    tx.commit().await?;
    let conn: Option<Connection> = (source == "url")
        .then(|| serde_json::from_value(spec["connection"].clone()).unwrap_or_default());
    // A database made for the import is checked as new (it's made again);
    // one that existed must still be empty.
    if source == "url" || !new_database {
        let check = ImportCheck {
            namespace: t.namespace.clone(),
            engine: t.engine.clone(),
            source: conn.clone(),
            target: (!new_database || t.engine == "mysql" || source == "file").then(|| Target {
                host: format!(
                    "{}-{}.{}.svc.cluster.local",
                    t.name,
                    if t.engine == "postgres" {
                        "rw"
                    } else {
                        "primary"
                    },
                    t.namespace
                ),
                secret: format!("{}-credentials", t.name),
            }),
            env: if source == "url" {
                vec![EnvVar {
                    key: "password".into(),
                    value: password.clone(),
                }]
            } else {
                vec![]
            },
        };
        let found = run_check(&s, &t.cluster_id, check).await?;
        if let Some(e) = found.get("error") {
            return Err(bad(e["message"]
                .as_str()
                .unwrap_or("The check failed.")
                .to_owned()));
        }
        if let Some(n) = found["targetObjects"].as_i64()
            && n != 0
        {
            return Err(ApiError::conflict(format!(
                "{} isn't empty any more: it has {}.",
                t.name,
                objects_text(n)
            )));
        }
    }
    let into = Into_ {
        team_id: team_id.clone(),
        cluster_id: t.cluster_id.clone(),
        engine: t.engine.clone(),
        version: t.version.clone(),
        namespace: t.namespace.clone(),
        name: t.name.clone(),
        existing: !new_database,
        extensions: t.extensions.clone(),
    };
    let mut tx = s.db.begin().await?;
    admin(&mut tx, &user, &team_id).await?;
    let block = Block::new(
        &into,
        &database_id,
        conn.as_ref(),
        upload.as_ref(),
        new_database && t.engine == "postgres" && source == "url",
    );
    let new_id = block.id.clone();
    insert(
        &mut tx,
        &team_id,
        &database_id,
        &block,
        (source == "url").then_some(password.as_str()),
        upload.as_ref(),
        new_database,
        &user.name,
    )
    .await?;
    // The old one's password isn't needed any more.
    sqlx::query("UPDATE pyo.database_imports SET password = '' WHERE id = $1")
        .bind(&id)
        .execute(&mut *tx)
        .await?;
    crate::databases::import_into(&s, &mut tx, &database_id, &[]).await?;
    log_activity(
        &mut tx,
        &team_id,
        &user.name,
        "database.import_started",
        "info",
        &format!("Importing {} into {} again", block.from, t.name),
        json!({ "kind": "database", "id": database_id, "label": t.name }),
        json!({ "clusterId": t.cluster_id, "importId": new_id }),
    )
    .await?;
    tx.commit().await?;
    s.wake(&t.cluster_id).await;
    Ok(Json(
        json!({ "ok": true, "importId": new_id, "databaseId": database_id }),
    ))
}

/// The Database's `spec.import`, and what's kept beside it.
struct Block {
    id: String,
    source: &'static str,
    from: String,
    spec: Value,
}

impl Block {
    fn new(
        into: &Into_,
        database_id: &str,
        conn: Option<&Connection>,
        upload: Option<&Upload>,
        new_database: bool,
    ) -> Self {
        let _ = database_id;
        let id = crate::ids::new_id("imp");
        let secret = format!("{}-import", into.name);
        match (conn, upload) {
            (Some(c), _) => Block {
                id: id.clone(),
                source: "url",
                from: label_of(c),
                spec: json!({
                    "id": id, "source": "url", "secret": secret,
                    // A new Postgres database: CloudNativePG imports it as it's made.
                    "bootstrap": new_database && into.engine == "postgres",
                    "connection": { "host": c.host, "port": c.port, "user": c.user, "database": c.database, "tls": c.tls },
                }),
            },
            (None, Some(u)) => Block {
                id: id.clone(),
                source: "file",
                from: u.file_name.clone(),
                spec: json!({ "id": id, "source": "file", "secret": secret, "format": u.format, "size": u.size }),
            },
            (None, None) => unreachable!("an import has a source"),
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn insert(
    tx: &mut Transaction<'_, Postgres>,
    team_id: &str,
    database_id: &str,
    b: &Block,
    password: Option<&str>,
    upload: Option<&Upload>,
    new_database: bool,
    by: &str,
) -> ApiResult<()> {
    let vault = crate::vault::get();
    let sealed = match password {
        Some(p) if !p.is_empty() => vault.seal(team_id, p).await?,
        _ => String::new(),
    };
    // The file's download token: its hash checks it, sealed it goes to the cluster.
    let (token_hash, token) = if upload.is_some() {
        let t = crate::ids::random_string(40);
        (crate::ids::sha256_hex(&t), vault.seal(team_id, &t).await?)
    } else {
        (String::new(), String::new())
    };
    sqlx::query(
        "INSERT INTO pyo.database_imports (id, team_id, database_id, source, source_label, password, spec, upload_id, token_hash, token, new_database, created_by)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)",
    )
    .bind(&b.id)
    .bind(team_id)
    .bind(database_id)
    .bind(b.source)
    .bind(&b.from)
    .bind(sealed)
    .bind(&b.spec)
    .bind(upload.map(|u| u.id.clone()))
    .bind(token_hash)
    .bind(token)
    .bind(new_database)
    .bind(by)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

async fn latest_state(
    tx: &mut Transaction<'_, Postgres>,
    database_id: &str,
) -> ApiResult<Option<String>> {
    Ok(sqlx::query_scalar("SELECT state FROM pyo.database_imports WHERE database_id = $1 ORDER BY created_at DESC LIMIT 1").bind(database_id).fetch_optional(&mut **tx).await?)
}

// ── For databases.rs ───────────────────────────────────────────────────────

/// What the Database gets of its latest import: `spec.import`, the Secret
/// beside it while it runs, or that Secret's name to delete once it's done.
pub(crate) async fn spec_part(
    tx: &mut Transaction<'_, Postgres>,
    database_id: &str,
    database_name: &str,
) -> ApiResult<(Option<Value>, Option<NamedSecret>, Vec<String>)> {
    let row: Option<(String, String, Value, String, String)> =
        sqlx::query_as("SELECT state, source, spec, password, token FROM pyo.database_imports WHERE database_id = $1 ORDER BY created_at DESC LIMIT 1").bind(database_id).fetch_optional(&mut **tx).await?;
    let Some((state, source, spec, password, token)) = row else {
        return Ok((None, None, vec![]));
    };
    let secret = format!("{database_name}-import");
    if state != "pending" && state != "running" {
        return Ok((Some(spec), None, vec![secret]));
    }
    let vault = crate::vault::get();
    let env = if source == "url" {
        vec![EnvVar {
            key: "password".into(),
            value: if password.is_empty() {
                String::new()
            } else {
                vault.open(&password).await?
            },
        }]
    } else {
        let id = spec["id"].as_str().unwrap_or("");
        vec![
            // The agent puts its own address for Pyo's API in.
            EnvVar {
                key: "url".into(),
                value: format!("$PYO_API_URL/api/agent/v1/database-imports/{id}/dump"),
            },
            EnvVar {
                key: "token".into(),
                value: vault.open(&token).await?,
            },
        ]
    };
    Ok((Some(spec), Some(NamedSecret { name: secret, env }), vec![]))
}

/// The latest import of each of these databases, as listed.
pub(crate) async fn listing(
    db: &PgPool,
    ids: &[String],
) -> ApiResult<std::collections::HashMap<String, Value>> {
    let rows: Vec<(String, Value)> = sqlx::query_as(
        "SELECT DISTINCT ON (database_id) database_id,
                jsonb_build_object('id', id, 'state', state, 'phase', phase, 'message', message, 'source', source, 'from', source_label,
                  'newDatabase', new_database, 'createdAt', pyo.ms(created_at), 'startedAt', pyo.ms(started_at), 'finishedAt', pyo.ms(finished_at),
                  'retryable', state = 'failed' AND (source <> 'file' OR EXISTS (SELECT 1 FROM pyo.database_import_uploads u WHERE u.id = upload_id)))
           FROM pyo.database_imports WHERE database_id = ANY($1) ORDER BY database_id, created_at DESC",
    )
    .bind(ids)
    .fetch_all(db)
    .await?;
    Ok(rows.into_iter().collect())
}

/// Whether a database's import made it and failed: it can be deleted
/// whatever its deletion protection says.
pub(crate) async fn failed_new(
    tx: &mut Transaction<'_, Postgres>,
    database_id: &str,
) -> ApiResult<bool> {
    Ok(sqlx::query_scalar("SELECT new_database AND state = 'failed' FROM pyo.database_imports WHERE database_id = $1 ORDER BY created_at DESC LIMIT 1")
        .bind(database_id)
        .fetch_optional(&mut **tx)
        .await?
        .unwrap_or(false))
}

/// A heartbeat's word on a database's import (`status.import`): its row
/// follows, with an activity event when it ends. True when it just ended
/// (the Database is sent again, without the Secret).
pub(crate) async fn observed(
    tx: &mut Transaction<'_, Postgres>,
    database_id: &str,
    status: &Value,
) -> ApiResult<bool> {
    let st = &status["import"];
    let Some(import_id) = st["id"].as_str() else {
        return Ok(false);
    };
    let row: Option<(String, String, String, String, String)> = sqlx::query_as(
        "SELECT i.state, i.team_id, d.name, i.source_label, d.cluster_id FROM pyo.database_imports i JOIN pyo.databases d ON d.id = i.database_id WHERE i.id = $1 AND i.database_id = $2 FOR UPDATE OF i",
    )
    .bind(import_id)
    .bind(database_id)
    .fetch_optional(&mut **tx)
    .await?;
    let Some((state, team_id, name, from, cluster_id)) = row else {
        return Ok(false);
    };
    if state == "succeeded" || state == "failed" {
        return Ok(false);
    }
    let phase = st["phase"].as_str().unwrap_or("");
    let message: String = st["message"]
        .as_str()
        .unwrap_or("")
        .chars()
        .take(2000)
        .collect();
    let next = match phase {
        "Running" => "running",
        "Succeeded" => "succeeded",
        "Failed" => "failed",
        _ => "pending",
    };
    sqlx::query(
        "UPDATE pyo.database_imports SET state = $2, phase = $3, message = $4,
                started_at = CASE WHEN $2 IN ('running', 'succeeded', 'failed') THEN coalesce(started_at, now()) ELSE started_at END,
                finished_at = CASE WHEN $2 IN ('succeeded', 'failed') THEN now() ELSE finished_at END,
                password = CASE WHEN $2 = 'succeeded' THEN '' ELSE password END
          WHERE id = $1",
    )
    .bind(import_id)
    .bind(next)
    .bind(if next == "failed" || next == "succeeded" { "" } else { st["message"].as_str().unwrap_or("") })
    .bind(if next == "failed" { message.as_str() } else { "" })
    .execute(&mut **tx)
    .await?;
    let target = json!({ "kind": "database", "id": database_id, "label": name });
    match next {
        "succeeded" => {
            log_activity(
                tx,
                &team_id,
                "Pyo",
                "database.import_finished",
                "info",
                &format!("Imported {from} into {name}"),
                target,
                json!({ "clusterId": cluster_id, "importId": import_id }),
            )
            .await?;
            Ok(true)
        }
        "failed" => {
            log_activity(
                tx,
                &team_id,
                "Pyo",
                "database.import_failed",
                "warning",
                &format!("Importing {from} into {name} failed: {message}"),
                target,
                json!({ "clusterId": cluster_id, "importId": import_id }),
            )
            .await?;
            Ok(true)
        }
        _ => Ok(false),
    }
}

// ── Files ──────────────────────────────────────────────────────────────────

/// The team's bucket in Pyo's own store.
struct Store {
    http: reqwest::Client,
    endpoint: reqwest::Url,
    bucket: String,
    access_key: String,
    secret: String,
}

impl Store {
    async fn for_team(db: &PgPool, team_id: &str) -> ApiResult<Self> {
        let Some((url, _, _)) = crate::backup::local_store() else {
            return Err(ApiError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "This installation has no storage of its own for uploads. Import from a URL instead.",
            ));
        };
        let (access_key, secret) = crate::backupstore::team_login(db, team_id).await?;
        let endpoint =
            reqwest::Url::parse(url.trim_end_matches('/')).map_err(anyhow::Error::from)?;
        Ok(Self {
            http: reqwest::Client::new(),
            endpoint,
            bucket: crate::backupstore::team_bucket(team_id),
            access_key,
            secret,
        })
    }

    fn request(
        &self,
        method: reqwest::Method,
        key: &str,
        query: &[(&str, &str)],
        body: Vec<u8>,
    ) -> reqwest::RequestBuilder {
        use crate::backup::{Request, enc, sigv4};
        let base = &self.endpoint;
        let host = match base.port() {
            Some(p) => format!("{}:{p}", base.host_str().unwrap_or("")),
            None => base.host_str().unwrap_or("").to_owned(),
        };
        let path = enc(
            &format!(
                "{}/{}/{}",
                base.path().trim_end_matches('/'),
                self.bucket,
                key
            )
            .replace("//", "/"),
            true,
        );
        let mut q: Vec<(String, String)> = query
            .iter()
            .map(|(k, v)| (enc(k, false), enc(v, false)))
            .collect();
        q.sort();
        let canonical = q
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join("&");
        let signed = sigv4(&Request {
            method: method.as_str(),
            host: &host,
            path: &path,
            query: &canonical,
            body: &body,
            service: "s3",
            region: crate::backupstore::REGION,
            access_key: &self.access_key,
            secret: &self.secret,
        });
        let url = format!(
            "{}://{host}{path}{}",
            base.scheme(),
            if canonical.is_empty() {
                String::new()
            } else {
                format!("?{canonical}")
            }
        );
        self.http
            .request(method, url)
            .header("x-amz-date", signed.amz_date)
            .header("x-amz-content-sha256", signed.payload_hash)
            .header("authorization", signed.authorization)
            .body(body)
    }

    async fn send(
        &self,
        method: reqwest::Method,
        key: &str,
        query: &[(&str, &str)],
        body: Vec<u8>,
    ) -> anyhow::Result<reqwest::Response> {
        let res = self
            .request(method, key, query, body)
            .timeout(Duration::from_secs(600))
            .send()
            .await?;
        if !res.status().is_success() {
            let status = res.status();
            let text = res.text().await.unwrap_or_default();
            anyhow::bail!(
                "the store answered {status}: {}",
                text.chars().take(300).collect::<String>()
            );
        }
        Ok(res)
    }

    async fn delete(&self, key: &str) -> anyhow::Result<()> {
        match self
            .request(reqwest::Method::DELETE, key, &[], vec![])
            .timeout(Duration::from_secs(60))
            .send()
            .await?
        {
            r if r.status().is_success() || r.status() == reqwest::StatusCode::NOT_FOUND => Ok(()),
            r => anyhow::bail!("the store answered {}", r.status()),
        }
    }
}

/// What a dump's start says: the extensions it makes and the version it
/// was dumped from.
pub fn sniff(engine: &str, text: &[u8]) -> (Vec<String>, String) {
    let t = String::from_utf8_lossy(text);
    let mut exts: Vec<String> = vec![];
    if engine != "mysql" {
        let re = regex::Regex::new(r#"CREATE EXTENSION (?:IF NOT EXISTS )?"?([A-Za-z0-9_-]+)"?"#)
            .expect("regex");
        for c in re.captures_iter(&t) {
            let e = c[1].to_owned();
            if !exts.contains(&e) {
                exts.push(e);
            }
        }
    }
    let pg = || {
        regex::Regex::new(r"Dumped from database version (\d+(?:\.\d+)?)")
            .expect("regex")
            .captures(&t)
            .map(|c| c[1].to_owned())
    };
    let my = || {
        regex::Regex::new(r"(?m)^-- Server version\s+([0-9][0-9A-Za-z.-]*)")
            .expect("regex")
            .captures(&t)
            .map(|c| c[1].to_owned())
    };
    let version = match engine {
        "postgres" => pg(),
        "mysql" => my(),
        _ => pg().or_else(my),
    };
    (exts, version.unwrap_or_default())
}

/// The format a file's name says: "sql", "sql.gz" or "dump" (Postgres; ""
/// for an engine not known yet takes either's).
fn format_of(engine: &str, name: &str) -> ApiResult<&'static str> {
    let n = name.to_ascii_lowercase();
    let f = if n.ends_with(".sql.gz") {
        "sql.gz"
    } else if n.ends_with(".sql") {
        "sql"
    } else if engine != "mysql"
        && (n.ends_with(".dump") || n.ends_with(".backup") || n.ends_with(".pgdump"))
    {
        "dump"
    } else {
        return Err(bad(if engine != "mysql" {
            "Upload a .sql, .sql.gz or .dump (pg_dump -Fc) file."
        } else {
            "Upload a .sql or .sql.gz file (from mysqldump)."
        }));
    };
    Ok(f)
}

/// How long an upload's URL may be used to start sending the file.
const UPLOAD_URL_SECS: i64 = 3600;

/// A dump about to be uploaded.
#[derive(Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct NewUpload {
    /// The team's id.
    team_id: String,
    /// "postgres" or "mysql", when known: the file's type is checked for it up front. Else the import says which.
    #[serde(default)]
    engine: Option<String>,
    /// The file's name: .sql, .sql.gz, or (Postgres) .dump.
    filename: String,
    /// Its size in bytes: the file sent must be exactly this.
    size: u64,
    /// Its media type, if any. It comes back in `headers`.
    #[serde(default)]
    content_type: Option<String>,
}

/// Where and how to send an upload's file.
struct UploadTarget {
    url: String,
    method: &'static str,
    headers: Value,
    /// Milliseconds since 1970.
    expires_at: i64,
}

/// Where the client sends the file. Here: back to this API with a one-time
/// token of the upload's own (no session or API key), so a client sends
/// only `url` and `headers`. A relative URL is relative to the Pyo address
/// called. The hosted build will hand out a presigned URL to the team's own
/// storage here instead.
fn upload_target(
    upload_id: &str,
    token: &str,
    content_type: &str,
    expires_at: i64,
) -> UploadTarget {
    UploadTarget {
        url: format!("/api/v1/database-imports/uploads/{upload_id}/content?token={token}"),
        method: "PUT",
        headers: json!({ "content-type": content_type }),
        expires_at,
    }
}

/// Start an upload
///
/// The first step of uploading a dump to import: say what it is and get
/// where to send it. Then `PUT` the file to `uploadUrl` as it is, before
/// `expiresAt`, with exactly the `headers` given and nothing else. Send no
/// cookies or API key, since the URL may be on another host. Then call
/// `startDatabaseImport` with `uploadId`. Postgres takes `.sql`, `.sql.gz`
/// or a custom-format `.dump` (pg_dump -Fc). MySQL takes `.sql` or
/// `.sql.gz` (mysqldump). The dump stays in the team's storage in Pyo until
/// its import succeeds, or for `PYO_DATABASE_IMPORT_RETENTION_HOURS` (24 by
/// default) when unused. At most `PYO_DATABASE_IMPORT_MAX_BYTES` (10 GiB by
/// default). Owners and admins.
#[utoipa::path(post, path = "/database-imports/uploads", operation_id = "createDatabaseDumpUpload", tag = "Databases",
    request_body = NewUpload,
    responses(
        (status = 200, description = "`uploadId` (for `startDatabaseImport`), and where to send the file: `uploadUrl` (relative to your Pyo's address when it starts with `/`), `method` (\"PUT\"), `headers` (send exactly these) and `expiresAt` (start sending before it).", body = Object),
        (status = 400, description = "Not a file Pyo can import, by its name, or empty.", body = ErrorBody),
        (status = 403, description = "You aren't an owner or admin of the team, or the API key can't make changes.", body = ErrorBody),
        (status = 413, description = "Bigger than this installation takes.", body = ErrorBody),
        (status = 503, description = "This installation has no storage for uploads.", body = ErrorBody),
    ),
    extensions(("x-pyo-access" = json!("full"))))]
pub async fn create_upload(
    State(s): State<AppState>,
    SessionUser(user): SessionUser,
    Json(req): Json<NewUpload>,
) -> ApiResult<Json<Value>> {
    let engine = req.engine.as_deref().filter(|e| !e.is_empty());
    if engine.is_some_and(|e| e != "postgres" && e != "mysql") {
        return Err(bad("Imports are into Postgres and MySQL databases."));
    }
    let format = format_of(engine.unwrap_or(""), &req.filename)?;
    let file_name: String = req
        .filename
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or("dump")
        .chars()
        .filter(|c| !c.is_control())
        .take(200)
        .collect();
    if req.size == 0 {
        return Err(bad("The file is empty."));
    }
    let max = max_bytes();
    if req.size > max {
        return Err(ApiError::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            format!(
                "The file is bigger than {}, the most this installation takes.",
                size_text(max)
            ),
        ));
    }
    let content_type: String = req
        .content_type
        .as_deref()
        .map(str::trim)
        .filter(|c| {
            !c.is_empty() && c.len() <= 200 && c.bytes().all(|b| b.is_ascii_graphic() || b == b' ')
        })
        .unwrap_or("application/octet-stream")
        .to_owned();
    let mut tx = s.db.begin().await?;
    admin(&mut tx, &user, &req.team_id).await?;
    tx.commit().await?;
    // Fails early when there's no store.
    Store::for_team(&s.db, &req.team_id).await?;
    let id = crate::ids::new_id("upl");
    let token = crate::ids::secret_token();
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    let expires_at = now_ms + UPLOAD_URL_SECS * 1000;
    sqlx::query(
        "INSERT INTO pyo.database_import_uploads (id, team_id, engine, file_name, size, content_type, format, token_hash, token_expires_at, object_key, created_by)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, to_timestamp($9::bigint / 1000.0), $10, $11)",
    )
    .bind(&id)
    .bind(&req.team_id)
    .bind(engine)
    .bind(&file_name)
    .bind(req.size as i64)
    .bind(&content_type)
    .bind(format)
    .bind(crate::ids::sha256_hex(&token))
    .bind(expires_at)
    .bind(format!("imports/{id}"))
    .bind(&user.name)
    .execute(&s.db)
    .await?;
    let t = upload_target(&id, &token, &content_type, expires_at);
    Ok(Json(
        json!({ "uploadId": id, "uploadUrl": t.url, "method": t.method, "headers": t.headers, "expiresAt": t.expires_at, "format": format }),
    ))
}

#[derive(Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub struct ContentQuery {
    /// The upload's one-time token, from `uploadUrl`.
    token: String,
}

/// An upload being sent, as `upload_content` claimed it.
#[derive(sqlx::FromRow)]
struct Claimed {
    team_id: String,
    engine: Option<String>,
    size: i64,
    format: String,
    object_key: String,
}

/// Send an upload's file
///
/// The second step: the file, as the request's body, to the `uploadUrl`
/// that `createDatabaseDumpUpload` gave (its `token` is the only
/// credential: send no API key). Once only, before its `expiresAt`, and
/// exactly the size declared.
#[utoipa::path(put, path = "/database-imports/uploads/{id}/content", operation_id = "uploadDatabaseDump", tag = "Databases", security(()),
    params(("id" = String, Path, description = "The upload's id."), ContentQuery),
    request_body(content = String, content_type = "application/octet-stream", description = "The file."),
    responses(
        (status = 200, description = "Stored: `uploadId`, `size`, `sha256`, `format`, `extensions` (Postgres: the ones it makes) and `sourceVersion` (what it was dumped from, when it says).", body = Object),
        (status = 400, description = "Not the size declared, or not what its name says.", body = ErrorBody),
        (status = 403, description = "The token is wrong, used already or expired.", body = ErrorBody),
        (status = 503, description = "This installation has no storage for uploads.", body = ErrorBody),
    ),
    extensions(("x-pyo-access" = json!("none"))))]
pub async fn upload_content(
    State(s): State<AppState>,
    Path(id): Path<String>,
    Query(q): Query<ContentQuery>,
    headers: HeaderMap,
    body: Body,
) -> ApiResult<Json<Value>> {
    // The token is used up here, whatever happens next.
    let claimed: Option<Claimed> = sqlx::query_as(
        "UPDATE pyo.database_import_uploads SET state = 'uploading', token_hash = NULL
          WHERE id = $1 AND token_hash = $2 AND state = 'pending' AND token_expires_at > now()
          RETURNING team_id, engine, size, format, object_key",
    )
    .bind(&id)
    .bind(crate::ids::sha256_hex(&q.token))
    .fetch_optional(&s.db)
    .await?;
    let Some(u) = claimed else {
        return Err(ApiError::forbidden(
            "This upload URL is wrong, used already or expired: start the upload again.",
        ));
    };
    let result = store_file(&s.db, &id, &u, &headers, body).await;
    if let Err(e) = &result {
        tracing::info!(upload = %id, error = %e.message, "an import's upload failed");
        let _ = sqlx::query(
            "DELETE FROM pyo.database_import_uploads WHERE id = $1 AND state = 'uploading'",
        )
        .bind(&id)
        .execute(&s.db)
        .await;
    }
    result
}

/// Stream the file into the store (multipart), check it, and mark the
/// upload "uploaded"; on an error, the store's upload is aborted.
async fn store_file(
    db: &PgPool,
    id: &str,
    u: &Claimed,
    headers: &HeaderMap,
    body: Body,
) -> ApiResult<Json<Value>> {
    use sha2::Digest;
    let size = u.size as u64;
    let mismatch = |got: u64| {
        bad(format!(
            "The file is {got} bytes, not the {size} declared: start the upload again."
        ))
    };
    if let Some(len) = headers
        .get("content-length")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<u64>().ok())
        && len != size
    {
        return Err(mismatch(len));
    }
    let store = Store::for_team(db, &u.team_id).await?;
    let key = u.object_key.as_str();
    let started = store
        .send(reqwest::Method::POST, key, &[("uploads", "")], vec![])
        .await
        .map_err(|e| {
            ApiError::new(
                StatusCode::BAD_GATEWAY,
                format!("Couldn't store the file: {e:#}"),
            )
        })?;
    let xml = started.text().await.unwrap_or_default();
    let upload_id = xml
        .split("<UploadId>")
        .nth(1)
        .and_then(|s| s.split("</UploadId>").next())
        .unwrap_or("")
        .to_owned();
    if upload_id.is_empty() {
        return Err(ApiError::new(
            StatusCode::BAD_GATEWAY,
            "Couldn't store the file: the store started no upload.",
        ));
    }
    // For the sweeper, should this process stop half-way.
    sqlx::query("UPDATE pyo.database_import_uploads SET multipart_id = $2 WHERE id = $1")
        .bind(id)
        .bind(&upload_id)
        .execute(db)
        .await?;
    let abort = || {
        let req = store.request(
            reqwest::Method::DELETE,
            key,
            &[("uploadId", &upload_id)],
            vec![],
        );
        async move {
            let _ = req.send().await;
        }
    };

    let mut stream = body.into_data_stream();
    let mut buf: Vec<u8> = Vec::with_capacity(PART);
    let mut parts: Vec<String> = vec![];
    let mut total: u64 = 0;
    let mut hash = sha2::Sha256::new();
    // The start of the file, decompressed when it's gzip.
    let mut head: Vec<u8> = Vec::new();
    let mut gz: Option<flate2::write::GzDecoder<Vec<u8>>> = None;
    let mut first: Vec<u8> = Vec::new();
    let result: ApiResult<()> = async {
        loop {
            let chunk = stream.next().await;
            let last = chunk.is_none();
            if let Some(c) = chunk {
                let c = c.map_err(|e| bad(format!("The upload broke off: {e}")))?;
                total += c.len() as u64;
                if total > size {
                    return Err(bad(format!(
                        "The file is bigger than the {size} bytes declared: start the upload again."
                    )));
                }
                hash.update(&c);
                if first.len() < 8 {
                    first.extend_from_slice(&c[..c.len().min(8 - first.len())]);
                }
                if first.starts_with(&[0x1f, 0x8b]) {
                    let d = gz.get_or_insert_with(|| flate2::write::GzDecoder::new(Vec::new()));
                    if d.get_ref().len() < SNIFF {
                        use std::io::Write;
                        let _ = d.write_all(&c);
                    }
                } else if head.len() < SNIFF {
                    head.extend_from_slice(&c[..c.len().min(SNIFF - head.len())]);
                }
                buf.extend_from_slice(&c);
            }
            if last && total != size {
                return Err(mismatch(total));
            }
            if buf.len() >= PART || (last && (!buf.is_empty() || parts.is_empty())) {
                let n = (parts.len() + 1).to_string();
                let part = std::mem::replace(&mut buf, Vec::with_capacity(PART));
                let res = store
                    .send(
                        reqwest::Method::PUT,
                        key,
                        &[("partNumber", &n), ("uploadId", &upload_id)],
                        part,
                    )
                    .await
                    .map_err(|e| {
                        ApiError::new(
                            StatusCode::BAD_GATEWAY,
                            format!("Couldn't store the file: {e:#}"),
                        )
                    })?;
                parts.push(
                    res.headers()
                        .get("etag")
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("")
                        .to_owned(),
                );
            }
            if last {
                return Ok(());
            }
        }
    }
    .await;
    if let Err(e) = result {
        abort().await;
        return Err(e);
    }
    let is_gz = first.starts_with(&[0x1f, 0x8b]);
    let problem = match u.format.as_str() {
        "sql.gz" if !is_gz => Some("The file isn't gzip-compressed: name it .sql."),
        "sql" if is_gz => Some("The file is gzip-compressed: name it .sql.gz."),
        "dump" if !first.starts_with(b"PGDMP") => Some(
            "The file isn't a custom-format dump (pg_dump -Fc). Upload a plain .sql file, or a .dump made with pg_dump -Fc.",
        ),
        _ => None,
    };
    if let Some(p) = problem {
        abort().await;
        return Err(bad(p));
    }
    let mut complete = String::from("<CompleteMultipartUpload>");
    for (i, etag) in parts.iter().enumerate() {
        complete.push_str(&format!(
            "<Part><PartNumber>{}</PartNumber><ETag>{}</ETag></Part>",
            i + 1,
            etag.replace('&', "&amp;").replace('<', "&lt;")
        ));
    }
    complete.push_str("</CompleteMultipartUpload>");
    if let Err(e) = store
        .send(
            reqwest::Method::POST,
            key,
            &[("uploadId", &upload_id)],
            complete.into_bytes(),
        )
        .await
    {
        abort().await;
        return Err(ApiError::new(
            StatusCode::BAD_GATEWAY,
            format!("Couldn't store the file: {e:#}"),
        ));
    }
    // What was decompressed (a file cut off past the start is fine).
    if let Some(mut d) = gz {
        use std::io::Write;
        let _ = d.flush();
        head = std::mem::take(d.get_mut());
    }
    let (extensions, source_version) = sniff(
        u.engine.as_deref().unwrap_or(""),
        &head[..head.len().min(SNIFF)],
    );
    let sha256 = hex::encode(hash.finalize());
    let done = sqlx::query(
        "UPDATE pyo.database_import_uploads SET state = 'uploaded', size = $2, sha256 = $3, extensions = $4, source_version = $5, multipart_id = '', uploaded_at = now()
          WHERE id = $1 AND state = 'uploading'",
    )
    .bind(id)
    .bind(total as i64)
    .bind(&sha256)
    .bind(json!(extensions))
    .bind(&source_version)
    .execute(db)
    .await?;
    if done.rows_affected() == 0 {
        // Swept meanwhile.
        let _ = store.delete(key).await;
        return Err(ApiError::conflict(
            "The upload took too long: start it again.",
        ));
    }
    Ok(Json(
        json!({ "ok": true, "uploadId": id, "size": total, "sha256": sha256, "format": u.format, "extensions": extensions, "sourceVersion": source_version }),
    ))
}

/// The dump, for its import's Job (with the import's token).
async fn dump(
    State(s): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> ApiResult<Response> {
    let token = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or("");
    if token.is_empty() {
        return Err(ApiError::new(StatusCode::UNAUTHORIZED, "Missing token."));
    }
    let row: Option<(String, String, Option<String>)> =
        sqlx::query_as("SELECT i.team_id, u.object_key, i.upload_id FROM pyo.database_imports i JOIN pyo.database_import_uploads u ON u.id = i.upload_id WHERE i.id = $1 AND i.token_hash = $2 AND i.state IN ('pending', 'running') AND u.state = 'uploaded'")
            .bind(&id)
            .bind(crate::ids::sha256_hex(token))
            .fetch_optional(&s.db)
            .await?;
    let Some((team_id, key, _)) = row else {
        return Err(ApiError::new(
            StatusCode::UNAUTHORIZED,
            "No such import, or it's done.",
        ));
    };
    let store = Store::for_team(&s.db, &team_id).await?;
    let res = store
        .request(reqwest::Method::GET, &key, &[], vec![])
        .send()
        .await
        .map_err(|e| {
            ApiError::new(
                StatusCode::BAD_GATEWAY,
                format!("Couldn't read the file: {e}"),
            )
        })?;
    if !res.status().is_success() {
        return Err(ApiError::new(
            StatusCode::BAD_GATEWAY,
            format!(
                "Couldn't read the file: the store answered {}",
                res.status()
            ),
        ));
    }
    let len = res.content_length();
    let mut r = Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "application/octet-stream");
    if let Some(l) = len {
        r = r.header("content-length", l);
    }
    r.body(Body::from_stream(res.bytes_stream()))
        .map_err(|e| ApiError::from(anyhow::Error::from(e)))
}

/// Files whose import succeeded, or unused past the retention, go (with
/// the other clean-ups, every few minutes); so do uploads whose URL expired
/// unused, and ones never finished within the retention (the store's
/// multipart upload aborted).
pub async fn sweep(db: &PgPool) -> anyhow::Result<()> {
    let gone: Vec<(String, String, String, String)> = sqlx::query_as(
        "SELECT u.id, u.team_id, u.object_key, u.multipart_id FROM pyo.database_import_uploads u
          WHERE (u.state = 'pending' AND u.token_expires_at < now())
             OR (u.state = 'uploading' AND u.created_at < now() - make_interval(hours => $1))
             OR (u.state = 'uploaded' AND EXISTS (SELECT 1 FROM pyo.database_imports i WHERE i.upload_id = u.id AND i.state = 'succeeded'))
             OR (u.state = 'uploaded' AND u.created_at < now() - make_interval(hours => $1)
                 AND NOT EXISTS (SELECT 1 FROM pyo.database_imports i WHERE i.upload_id = u.id AND i.state IN ('pending', 'running')))",
    )
    .bind(retention_hours() as i32)
    .fetch_all(db)
    .await?;
    for (id, team, key, multipart) in gone {
        let store = match Store::for_team(db, &team).await {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(upload = %id, error = %e.message, "couldn't reach the store to delete an import's file");
                continue;
            }
        };
        if !multipart.is_empty() {
            match store
                .request(
                    reqwest::Method::DELETE,
                    &key,
                    &[("uploadId", &multipart)],
                    vec![],
                )
                .timeout(Duration::from_secs(60))
                .send()
                .await
            {
                Ok(r)
                    if r.status().is_success() || r.status() == reqwest::StatusCode::NOT_FOUND => {}
                Ok(r) => {
                    tracing::warn!(upload = %id, status = %r.status(), "couldn't abort an import's unfinished upload");
                    continue;
                }
                Err(e) => {
                    tracing::warn!(upload = %id, error = %e, "couldn't abort an import's unfinished upload");
                    continue;
                }
            }
        }
        if let Err(e) = store.delete(&key).await {
            tracing::warn!(upload = %id, error = %format!("{e:#}"), "couldn't delete an import's file");
            continue;
        }
        sqlx::query("DELETE FROM pyo.database_import_uploads WHERE id = $1")
            .bind(&id)
            .execute(db)
            .await?;
        tracing::info!(upload = %id, "deleted an import's file");
    }
    Ok(())
}

/// The worker loop: `sweep` every 10 minutes.
pub async fn runner(state: AppState) {
    loop {
        let db = state.db.clone();
        crate::bg::as_leader(&state.db, crate::bg::LOCK_DATABASE_IMPORTS, || async move {
            if let Err(e) = sweep(&db).await {
                tracing::warn!(error = %format!("{e:#}"), "database import files");
            }
        })
        .await;
        tokio::time::sleep(Duration::from_secs(600)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counted() {
        assert_eq!(objects_text(1), "1 table, view or function");
        assert_eq!(objects_text(3), "3 tables, views or functions");
    }

    #[test]
    fn urls() {
        let (c, pw) = parse_url("postgres", "postgresql://app%40x:p%40ss%2Fw@db.example.com:6543/shop?sslmode=verify-full&application_name=x").unwrap();
        assert_eq!(
            (
                c.host.as_str(),
                c.port,
                c.user.as_str(),
                c.database.as_str(),
                c.tls.as_str(),
                pw.as_str()
            ),
            (
                "db.example.com",
                6543,
                "app@x",
                "shop",
                "verify-full",
                "p@ss/w"
            )
        );
        let (c, _) = parse_url("postgres", "postgres://u@[::1]/d").unwrap();
        assert_eq!((c.host.as_str(), c.port, c.tls.as_str()), ("::1", 5432, ""));
        let (c, pw) = parse_url(
            "mysql",
            "mysql://root:secret@aws.connect.psdb.cloud/shop?sslaccept=strict",
        )
        .unwrap();
        assert_eq!(
            (c.port, c.tls.as_str(), pw.as_str()),
            (3306, "verify-full", "secret")
        );
        assert_eq!(
            parse_url("mysql", "mysql://u:p@h/d?ssl-mode=REQUIRED")
                .unwrap()
                .0
                .tls,
            "require"
        );
        assert!(
            parse_url("mysql", "postgres://u:p@h/d")
                .unwrap_err()
                .message
                .contains("mysql://")
        );
        assert!(
            parse_url("postgres", "postgres://u:p@h:5432")
                .unwrap_err()
                .message
                .contains("database")
        );
        assert!(
            parse_url("postgres", "postgres://h/d")
                .unwrap_err()
                .message
                .contains("user")
        );
        assert!(parse_url("postgres", "not a url").is_err());
        assert!(parse_url("postgres", "postgres://u:p@h/d?sslmode=bogus").is_err());
    }

    #[test]
    fn what_a_dump_says() {
        let pg = b"--\n-- PostgreSQL database dump\n--\n\n-- Dumped from database version 17.2 (Debian 17.2-1)\n-- Dumped by pg_dump version 18.0\nCREATE EXTENSION IF NOT EXISTS citext WITH SCHEMA public;\nCREATE EXTENSION IF NOT EXISTS \"uuid-ossp\" WITH SCHEMA public;\n";
        assert_eq!(
            sniff("postgres", pg),
            (
                vec!["citext".to_owned(), "uuid-ossp".to_owned()],
                "17.2".to_owned()
            )
        );
        let my = b"-- MySQL dump 10.13  Distrib 8.4.3, for Linux (x86_64)\n--\n-- Host: x    Database: shop\n-- ------------------------------------------------------\n-- Server version\t8.0.36\n";
        assert_eq!(sniff("mysql", my), (vec![], "8.0.36".to_owned()));
        assert_eq!(format_of("postgres", "x.SQL.GZ").unwrap(), "sql.gz");
        assert_eq!(format_of("postgres", "x.dump").unwrap(), "dump");
        assert!(format_of("mysql", "x.dump").is_err() && format_of("postgres", "x.tar").is_err());
        assert_eq!(format_of("", "x.dump").unwrap(), "dump");
        assert_eq!(sniff("", my), (vec![], "8.0.36".to_owned()));
        assert_eq!(sniff("", pg).1, "17.2");
        let t = upload_target("upl_x", "tok", "application/octet-stream", 5);
        assert_eq!(
            (t.url.as_str(), t.method, t.headers["content-type"].as_str()),
            (
                "/api/v1/database-imports/uploads/upl_x/content?token=tok",
                "PUT",
                Some("application/octet-stream")
            )
        );
        assert_eq!(postgres_major("17.2"), Some(17));
        assert_eq!(postgres_major("9.6"), Some(9));
        assert_eq!(
            (
                size_text(10 * GIB),
                size_text(100 << 20),
                size_text(GIB + GIB / 2)
            ),
            (
                "10 GiB".to_owned(),
                "100 MiB".to_owned(),
                "1.5 GiB".to_owned()
            )
        );
    }
}
