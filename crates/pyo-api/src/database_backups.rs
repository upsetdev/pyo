//! Databases a team backs up, set up like Databasus: pick the engine, give
//! an admin login once, and Pyo makes a read-only `pyo_backup` user
//! (agent backup.rs) that every backup then runs as. The admin login is used
//! for that and never stored.

use std::time::Duration;

use axum::{Json, extract::State};
use pyo_proto::{BackupSpec, Command, CommandResult, DbConnection};
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::{PgPool, Postgres, Transaction};

use crate::{
    AppState,
    auth::SessionUser,
    deploy,
    error::{ApiError, ApiResult},
    ids::new_id,
};

const ENGINES: &[&str] = &["postgres", "mysql", "mariadb", "mongo", "redis"];
/// Jobs for databases that aren't Pyo services run here.
const EXTERNAL_NS: &str = "pyo-backups";

/// A database to back up.
#[derive(Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct AddDatabase {
    team_id: String,
    /// A name for it, unique in the team.
    name: String,
    /// "postgres", "mysql", "mariadb", "mongo" or "redis".
    engine: String,
    /// The cluster that reaches it and runs its backups.
    cluster_id: String,
    /// The Pyo service it is, if it is one.
    #[serde(default)]
    service_id: Option<String>,
    host: String,
    port: u16,
    /// The database to back up. Not needed for Redis.
    #[serde(default)]
    database: String,
    /// A login allowed to create users: Pyo makes a backup user with it and doesn't keep it.
    admin_user: String,
    admin_password: String,
    enabled: bool,
    /// "daily" or "weekly".
    schedule: String,
    /// The hour of day it runs, 0 to 23 (UTC).
    hour: u8,
    /// How many backups to keep, 1 to 365.
    keep: u32,
}

/// Back up a database
///
/// Checks the admin login from the cluster, makes a backup user with it,
/// then saves the database and its schedule. Team owners and admins.
#[utoipa::path(post, path = "/database-backups", operation_id = "addDatabaseBackup", tag = "Databases",
    request_body = AddDatabase,
    responses(
        (status = 200, description = "Saved: `ok` and the new database's `id`.", body = Object),
        (status = 400, description = "A missing or wrong value, or the login didn't work.", body = crate::error::ErrorBody),
        (status = 403, description = "You aren't an owner or admin of the team, or the API key can only read.", body = crate::error::ErrorBody),
        (status = 404, description = "No such team or service.", body = crate::error::ErrorBody),
        (status = 409, description = "The team already backs up a database by that name.", body = crate::error::ErrorBody),
        (status = 504, description = "The cluster didn't finish checking in time (is it connected?).", body = crate::error::ErrorBody),
    ),
    extensions(("x-pyo-access" = json!("deploy"))))]
pub async fn add(
    State(s): State<AppState>,
    SessionUser(user): SessionUser,
    Json(a): Json<AddDatabase>,
) -> ApiResult<Json<Value>> {
    let name = crate::util::require_text(&a.name, "Name", 60)?.to_owned();
    if !ENGINES.contains(&a.engine.as_str()) {
        return Err(ApiError::bad_request("Pick a database engine."));
    }
    if a.host.trim().is_empty() || a.port == 0 {
        return Err(ApiError::bad_request("Enter the host and port."));
    }
    if a.engine == "postgres" && a.database.trim().is_empty() {
        return Err(ApiError::bad_request("Enter the database to back up."));
    }
    schedule_ok(&a.schedule, a.hour, a.keep)?;

    let mut tx = s.db.begin().await?;
    crate::ops::require_team_admin(&mut tx, &user, &a.team_id).await?;
    let cluster_team: Option<String> =
        sqlx::query_scalar("SELECT team_id FROM pyo.clusters WHERE id = $1")
            .bind(&a.cluster_id)
            .fetch_optional(&mut *tx)
            .await?;
    if cluster_team.as_deref() != Some(a.team_id.as_str()) {
        return Err(ApiError::bad_request("Pick one of this team's clusters."));
    }
    // A service's backups run in its namespace (so an isolated project's
    // network lets them in); an external database's in their own.
    let namespace = match &a.service_id {
        Some(service) => {
            let (team, ns): (String, String) =
                sqlx::query_as("SELECT a.team_id, p.namespace FROM pyo.services a JOIN pyo.projects p ON p.id = a.project_id WHERE a.id = $1")
                    .bind(service)
                    .fetch_optional(&mut *tx)
                    .await?
                    .ok_or_else(|| ApiError::not_found("Service"))?;
            if team != a.team_id {
                return Err(ApiError::not_found("Service"));
            }
            ns
        }
        None => EXTERNAL_NS.to_owned(),
    };
    let taken: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM pyo.backup_databases WHERE team_id = $1 AND name = $2)",
    )
    .bind(&a.team_id)
    .bind(&name)
    .fetch_one(&mut *tx)
    .await?;
    if taken {
        return Err(ApiError::conflict(format!(
            "This team already backs up a database named {name}."
        )));
    }
    let mut pw = [0u8; 24];
    rand::fill(&mut pw);
    let password = hex::encode(pw);
    let admin = DbConnection {
        engine: a.engine.clone(),
        host: a.host.trim().to_owned(),
        port: a.port,
        database: a.database.trim().to_owned(),
        user: a.admin_user.trim().to_owned(),
        password: a.admin_password.clone(),
    };
    let cmd_id = deploy::enqueue(
        &mut tx,
        &a.cluster_id,
        Command::DbSetup {
            namespace: namespace.clone(),
            admin: admin.clone(),
            password: password.clone(),
        },
    )
    .await?;
    tx.commit().await?;
    s.wake(&a.cluster_id).await;

    // The setup Job pulls the engine's image the first time: give it a while.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(150);
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
            return Err(ApiError::new(
                axum::http::StatusCode::GATEWAY_TIMEOUT,
                "The cluster didn't finish checking the database in time. Is its agent connected?",
            ));
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    };
    // Admin passwords stay out of the command log.
    sqlx::query("UPDATE pyo.commands SET command = command - 'admin' WHERE id = $1")
        .bind(&cmd_id)
        .execute(&s.db)
        .await?;
    if !result.ok {
        return Err(ApiError::bad_request(format!(
            "Couldn't set up the backup user: {}",
            result
                .message
                .unwrap_or_else(|| "the database refused".into())
        )));
    }

    let id = new_id("db");
    let sealed = crate::vault::get().seal(&a.team_id, &password).await?;
    let mut tx = s.db.begin().await?;
    sqlx::query(
        "INSERT INTO pyo.backup_databases (id, team_id, cluster_id, service_id, name, engine, host, port, database, password_sealed, enabled, schedule, hour, keep)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14)",
    )
    .bind(&id)
    .bind(&a.team_id)
    .bind(&a.cluster_id)
    .bind(&a.service_id)
    .bind(&name)
    .bind(&a.engine)
    .bind(&admin.host)
    .bind(i32::from(a.port))
    .bind(&admin.database)
    .bind(&sealed)
    .bind(a.enabled)
    .bind(&a.schedule)
    .bind(i32::from(a.hour))
    .bind(a.keep as i32)
    .execute(&mut *tx)
    .await?;
    crate::ops::log_activity(
        &mut tx,
        &a.team_id,
        &user.name,
        "backup.database",
        "info",
        &format!("Set up backups of {name}"),
        json!({ "kind": "database", "id": id, "label": name }),
        json!({}),
    )
    .await?;
    tx.commit().await?;
    Ok(Json(json!({ "ok": true, "id": id })))
}

pub fn schedule_ok(schedule: &str, hour: u8, keep: u32) -> ApiResult<()> {
    if !matches!(schedule, "daily" | "weekly") {
        return Err(ApiError::bad_request("Pick daily or weekly."));
    }
    if hour > 23 || !(1..=365).contains(&keep) {
        return Err(ApiError::bad_request(
            "Pick an hour from 0 to 23, and keep 1 to 365 backups.",
        ));
    }
    Ok(())
}

type Row = (
    String,
    String,
    Option<String>,
    String,
    String,
    String,
    i32,
    String,
    String,
    i32,
    Option<String>,
    Option<String>,
);

/// Queue a backup of one database (as its read-only user).
pub async fn queue(
    s: &AppState,
    tx: &mut Transaction<'_, Postgres>,
    id: &str,
    trigger: &str,
) -> ApiResult<String> {
    // A KV database's: its own login and namespace, from it.
    let (team, cluster, service_id, name, engine, host, port, database, sealed, keep, login, db_namespace): Row =
        sqlx::query_as(
            "SELECT d.team_id, d.cluster_id, d.service_id, d.name, d.engine, d.host, d.port, d.database, coalesce(m.password, d.password_sealed), d.keep, d.login, m.namespace
               FROM pyo.backup_databases d LEFT JOIN pyo.databases m ON m.id = d.database_id WHERE d.id = $1",
        )
            .bind(id)
            .fetch_optional(&mut **tx)
            .await?
            .ok_or_else(|| ApiError::not_found("Database"))?;
    let busy: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM pyo.backups WHERE database_id = $1 AND status IN ('queued', 'running'))").bind(id).fetch_one(&mut **tx).await?;
    if busy {
        return Err(ApiError::conflict(format!(
            "{name} is already being backed up."
        )));
    }
    let cfg = crate::teambackup::effective(&s.db, &team).await?;
    if cfg.base.is_local() && !crate::teambackup::on_pyos_cluster(tx, &cluster).await? {
        return Err(ApiError::conflict(crate::teambackup::LOCAL_ONLY));
    }
    let (_, access_key, secret_key) = crate::teambackup::destination(s, &team, &cfg).await?;
    let service = match &service_id {
        Some(a) => crate::ops::service_ref(tx, a).await?,
        None => pyo_proto::ServiceRef {
            service_id: id.to_owned(),
            name: name.clone(),
            namespace: db_namespace.unwrap_or_else(|| EXTERNAL_NS.into()),
            workload_kind: String::new(),
        },
    };
    let backup_id = new_id("bkp");
    let spec = BackupSpec {
        backup_id: backup_id.clone(),
        service,
        kind: "database".into(),
        claim: String::new(),
        engine: engine.clone(),
        tags: vec![format!("db={id}")],
        keep: keep.max(1) as u32,
        repository: crate::teambackup::repository_of(&cfg, &team),
        password: crate::vault::get().open(&cfg.repo_password_sealed).await?,
        access_key,
        secret_key,
        region: crate::teambackup::region_of(&cfg.base),
        connection: Some(DbConnection {
            engine,
            host,
            port: port as u16,
            database,
            user: login.unwrap_or_else(|| pyo_proto::BACKUP_DB_USER.into()),
            password: crate::vault::get().open(&sealed).await?,
        }),
    };
    let command_id = deploy::enqueue(tx, &cluster, Command::Backup(Box::new(spec))).await?;
    sqlx::query(
        "INSERT INTO pyo.backups (id, team_id, cluster_id, service_id, database_id, kind, status, trigger, label, command_id, started_at)
         VALUES ($1, $2, $3, $4, $5, 'database', 'running', $6, $7, $8, now())",
    )
    .bind(&backup_id)
    .bind(&team)
    .bind(&cluster)
    .bind(service_id.as_deref().unwrap_or(id))
    .bind(id)
    .bind(trigger)
    .bind(&name)
    .bind(&command_id)
    .execute(&mut **tx)
    .await?;
    crate::bg::wake(&s.db, &cluster).await;
    Ok(backup_id)
}

/// Scheduled backups of every database that's due (from the worker loop).
pub async fn tick(s: &AppState) -> anyhow::Result<()> {
    let due: Vec<(String, String, String, i32, i32, Option<i64>)> = sqlx::query_as(
        "SELECT d.id, d.team_id, d.schedule, d.hour, d.keep,
                (SELECT pyo.ms(max(b.created_at)) FROM pyo.backups b WHERE b.database_id = d.id AND b.trigger = 'schedule')
           FROM pyo.backup_databases d WHERE d.enabled",
    )
    .fetch_all(&s.db)
    .await?;
    for (id, team, schedule, hour, keep, last) in due {
        let cfg = crate::backup::Settings {
            schedule,
            hour: hour as u8,
            keep: keep as u32,
            ..Default::default()
        };
        if !crate::backup::due(&cfg, last) {
            continue;
        }
        let mut tx = s.db.begin().await?;
        match queue(s, &mut tx, &id, "schedule").await {
            Ok(_) => tx.commit().await?,
            Err(e) => {
                tracing::warn!(%team, database = %id, error = ?e, "couldn't queue a database backup")
            }
        }
    }
    Ok(())
}

/// The dashboard's view of a team's databases, with a year of backups each.
pub async fn snapshot(db: &PgPool, team: &str) -> ApiResult<Vec<Value>> {
    Ok(sqlx::query_scalar(
        "SELECT jsonb_build_object(
            'id', d.id, 'clusterId', d.cluster_id, 'serviceId', d.service_id, 'name', d.name, 'engine', d.engine,
            'host', d.host, 'port', d.port, 'database', d.database, 'user', coalesce(d.login, 'pyo_backup'),
            'databaseId', d.database_id,
            'enabled', d.enabled, 'schedule', d.schedule, 'hour', d.hour, 'keep', d.keep, 'createdAt', pyo.ms(d.created_at),
            'runs', coalesce((SELECT jsonb_agg(jsonb_build_object(
                'id', b.id, 'status', b.status, 'trigger', b.trigger, 'sizeBytes', b.size_bytes, 'addedBytes', b.added_bytes,
                'snapshotId', b.snapshot_id, 'error', b.error, 'createdAt', pyo.ms(b.created_at),
                'startedAt', pyo.ms(b.started_at), 'finishedAt', pyo.ms(b.finished_at)) ORDER BY b.created_at DESC)
              FROM pyo.backups b WHERE b.database_id = d.id AND b.created_at > now() - interval '371 days'), '[]'))
           FROM pyo.backup_databases d WHERE d.team_id = $1 ORDER BY d.name",
    )
    .bind(team)
    .fetch_all(db)
    .await?)
}
