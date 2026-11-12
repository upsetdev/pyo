//! Teams' backups of their volumes and databases, with restic, to the team's
//! own S3-compatible bucket. The agent runs each as a Job (agent backup.rs).
//!
//! Databases (PostgreSQL, MySQL, MariaDB, MongoDB, Redis/Valkey) are dumped
//! by their own tool, so a backup is consistent; their volumes aren't also
//! copied raw. Other volumes are backed up as files. restic encrypts it all;
//! the repository password is sealed with the team's key (vault.rs).

use std::time::Duration;

use anyhow::Context;
use pyo_proto::{BackupSpec, Command, ServiceRef};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::{PgPool, Postgres, Transaction};

use crate::{AppState, backup, deploy, error::ApiResult, ids::new_id};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct TeamSettings {
    #[serde(flatten)]
    pub base: backup::Settings,
    /// restic's repository password, sealed with the team's key.
    #[serde(default)]
    pub repo_password_sealed: String,
    /// "all": everything but `exceptions` (new ones included); "selected":
    /// only `exceptions`. Ids are services' (databases) and volumes'.
    #[serde(default = "all")]
    pub mode: String,
    #[serde(default)]
    pub exceptions: Vec<String>,
}

fn all() -> String {
    "all".into()
}

impl TeamSettings {
    pub fn includes(&self, target_id: &str) -> bool {
        let listed = self.exceptions.iter().any(|e| e == target_id);
        if self.mode == "selected" {
            listed
        } else {
            !listed
        }
    }
}

pub async fn settings(db: &PgPool, team_id: &str) -> anyhow::Result<Option<TeamSettings>> {
    let v: Option<Value> =
        sqlx::query_scalar("SELECT value FROM pyo.team_backup_settings WHERE team_id = $1")
            .bind(team_id)
            .fetch_optional(db)
            .await?;
    Ok(v.and_then(|v| serde_json::from_value(v).ok()))
}

/// The team's settings, or the default: this server's store, volumes off,
/// with a repository password made (and saved) on first use.
pub async fn effective(db: &PgPool, team_id: &str) -> anyhow::Result<TeamSettings> {
    if let Some(s) = settings(db, team_id).await? {
        return Ok(s);
    }
    let s = TeamSettings {
        base: backup::Settings::local_default(false),
        repo_password_sealed: repo_password(team_id, None).await?,
        mode: all(),
        exceptions: vec![],
    };
    save(db, team_id, &s).await?;
    Ok(s)
}

pub async fn save(db: &PgPool, team_id: &str, s: &TeamSettings) -> anyhow::Result<()> {
    sqlx::query(
        "INSERT INTO pyo.team_backup_settings (team_id, value) VALUES ($1, $2)
         ON CONFLICT (team_id) DO UPDATE SET value = excluded.value, updated_at = now()",
    )
    .bind(team_id)
    .bind(serde_json::to_value(s)?)
    .execute(db)
    .await?;
    Ok(())
}

/// A new team gets a random repository password; a team keeps its own.
pub async fn repo_password(
    team_id: &str,
    existing: Option<&TeamSettings>,
) -> anyhow::Result<String> {
    if let Some(s) = existing.filter(|s| !s.repo_password_sealed.is_empty()) {
        return Ok(s.repo_password_sealed.clone());
    }
    let mut b = [0u8; 32];
    rand::fill(&mut b);
    crate::vault::get().seal(team_id, &hex::encode(b)).await
}

/// Which dump tool a database service needs, by template or image.
pub fn engine(spec: &Value) -> Option<&'static str> {
    let template = spec["source"]["templateId"].as_str().unwrap_or("");
    let image = spec["source"]["image"]
        .as_str()
        .unwrap_or("")
        .to_lowercase();
    let by = |t: &str, i: &str| {
        template == t || image.rsplit('/').next().is_some_and(|n| n.starts_with(i))
    };
    if by("postgresql", "postgres") {
        Some("postgres")
    } else if by("mariadb", "mariadb") {
        Some("mariadb")
    } else if by("mysql", "mysql") {
        Some("mysql")
    } else if by("mongodb", "mongo") {
        Some("mongo")
    } else if by("valkey", "valkey")
        || image
            .rsplit('/')
            .next()
            .is_some_and(|n| n.starts_with("redis"))
    {
        Some("redis")
    } else {
        None
    }
}

/// What a team backs up: each database service, and each other service's volumes.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Target {
    /// The service's id, or the volume's.
    pub id: String,
    pub kind: &'static str,
    /// Null for a volume that isn't attached.
    pub service_id: Option<String>,
    pub cluster_id: String,
    pub label: String,
    pub engine: Option<&'static str>,
    pub volume_id: Option<String>,
    pub claim: Option<String>,
    /// Where a volume that isn't attached is.
    #[serde(skip)]
    pub namespace: Option<String>,
}

pub async fn targets(db: &PgPool, team_id: &str) -> anyhow::Result<Vec<Target>> {
    // Labelled "project / service": the same name can be in two projects.
    let services: Vec<(String, String, String, Value, bool)> = sqlx::query_as(
        "SELECT a.id, a.cluster_id, p.name || ' / ' || a.name, a.spec, a.current_deployment_id IS NOT NULL
           FROM pyo.services a JOIN pyo.projects p ON p.id = a.project_id
          WHERE a.team_id = $1 AND NOT a.deleting ORDER BY p.name, a.name",
    )
    .bind(team_id)
    .fetch_all(db)
    .await?;
    let volumes: Vec<(String, Option<String>, String)> =
        sqlx::query_as("SELECT id, service_id, coalesce(data->>'name', id) FROM pyo.volumes WHERE team_id = $1 AND service_id IS NOT NULL AND NOT deleting ORDER BY data->>'name'")
            .bind(team_id)
            .fetch_all(db)
            .await?;
    let mut out = Vec::new();
    for (id, cluster, name, spec, deployed) in &services {
        if !deployed {
            continue;
        }
        // A database's files aren't a consistent backup: set it up under
        // Databases (a dump as a read-only user) instead.
        if engine(spec).is_some() {
            continue;
        }
        for (vid, _, vname) in volumes.iter().filter(|(_, a, _)| a.as_deref() == Some(id)) {
            out.push(Target {
                id: vid.clone(),
                kind: "volume",
                service_id: Some(id.clone()),
                cluster_id: cluster.clone(),
                label: format!("{name} · {vname}"),
                engine: None,
                volume_id: Some(vid.clone()),
                claim: Some(vname.clone()),
                namespace: None,
            });
        }
    }
    // Volumes that aren't attached (their project's, or a deleted
    // project's), once a service has used them.
    let detached: Vec<(String, String, String, Option<String>, String)> = sqlx::query_as(
        "SELECT v.id, v.cluster_id, coalesce(p.name, 'Deleted project'), coalesce(v.namespace, p.namespace), coalesce(v.data->>'name', v.id)
           FROM pyo.volumes v LEFT JOIN pyo.projects p ON p.id = v.project_id
          WHERE v.team_id = $1 AND v.service_id IS NULL AND NOT v.deleting AND v.data->>'status' = 'bound' AND NOT (v.data ? 'moving')
          ORDER BY v.data->>'name'",
    )
    .bind(team_id)
    .fetch_all(db)
    .await?;
    for (vid, cluster, project, ns, vname) in detached {
        out.push(Target {
            id: vid.clone(),
            kind: "volume",
            service_id: None,
            cluster_id: cluster,
            label: format!("{project} · {vname}"),
            engine: None,
            volume_id: Some(vid),
            claim: Some(vname),
            namespace: ns,
        });
    }
    Ok(out)
}

pub fn repository_of(cfg: &TeamSettings, team_id: &str) -> String {
    repository(&cfg.base, team_id)
}

fn repository(cfg: &backup::Settings, team_id: &str) -> String {
    if cfg.is_local() {
        let url = backup::local_store().map(|(u, _, _)| u).unwrap_or_default();
        return format!(
            "s3:{}/{}",
            url.trim_end_matches('/'),
            backup::team_bucket(team_id)
        );
    }
    let prefix = cfg.prefix.trim().trim_matches('/');
    let path = if prefix.is_empty() {
        team_id.to_owned()
    } else {
        format!("{prefix}/{team_id}")
    };
    format!(
        "s3:{}/{}/{path}",
        cfg.endpoint.trim_end_matches('/'),
        cfg.bucket
    )
}

/// Where the team's backups go, resolved, and the key for it: for the store
/// on Pyo's cluster, the team's own bucket and key (backupstore.rs), never
/// Pyo's key.
pub async fn destination(
    state: &AppState,
    team_id: &str,
    cfg: &TeamSettings,
) -> anyhow::Result<(backup::Settings, String, String)> {
    let mut r = cfg.base.resolved()?;
    if cfg.base.is_local() {
        let (key, secret) = backup::team_login(&state.db, team_id).await?;
        r.bucket = backup::team_bucket(team_id);
        r.access_key = key.clone();
        return Ok((r, key, secret));
    }
    let (key, secret) = backup::credentials(state, &r, None)?;
    Ok((r, key, secret))
}

/// The region a backup signs with: Garage's own for the store on Pyo's
/// cluster, else the bucket's (us-east-1 when none was entered).
pub fn region_of(cfg: &backup::Settings) -> String {
    if cfg.is_local() {
        crate::backupstore::REGION.into()
    } else if cfg.region.trim().is_empty() {
        "us-east-1".into()
    } else {
        cfg.region.trim().to_owned()
    }
}

/// Whether a cluster is the one Pyo runs on (agent.rs checks that itself):
/// the only one Pyo's own store answers, and the only one it hands a login
/// to it.
pub async fn on_pyos_cluster(
    tx: &mut Transaction<'_, Postgres>,
    cluster_id: &str,
) -> sqlx::Result<bool> {
    Ok(
        sqlx::query_scalar::<_, bool>("SELECT same_cluster FROM pyo.clusters WHERE id = $1")
            .bind(cluster_id)
            .fetch_optional(&mut **tx)
            .await?
            .unwrap_or(false),
    )
}

/// Why a backup to Pyo's own store can't run on another cluster.
pub const LOCAL_ONLY: &str = "Pyo's own backup store is only reachable from the cluster Pyo runs on. Back up to an S3 bucket (Backups) for services on other clusters.";

/// Queue backups of these targets (all of the team's when `only` is None).
pub async fn queue(
    state: &AppState,
    tx: &mut Transaction<'_, Postgres>,
    team_id: &str,
    only: Option<&str>,
    trigger: &str,
) -> ApiResult<usize> {
    let cfg = effective(&state.db, team_id).await?;
    let password = crate::vault::get().open(&cfg.repo_password_sealed).await?;
    let (_, access_key, secret) = destination(state, team_id, &cfg).await?;
    let mut n = 0;
    for t in targets(&state.db, team_id).await? {
        // One asked for by name runs; otherwise only what the team picked.
        match only {
            Some(o) if o != t.id => continue,
            None if !cfg.includes(&t.id) => continue,
            _ => {}
        }
        // The store's login stays on Pyo's cluster.
        if cfg.base.is_local() && !on_pyos_cluster(tx, &t.cluster_id).await? {
            if only.is_some() {
                return Err(crate::error::ApiError::conflict(LOCAL_ONLY));
            }
            continue;
        }
        // One at a time per target (a volume's, wherever it's attached).
        let busy: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM pyo.backups WHERE (CASE WHEN $2::text IS NULL THEN service_id = $1 AND volume_id IS NULL ELSE volume_id = $2 END) AND status IN ('queued', 'running'))",
        )
        .bind(&t.service_id)
        .bind(&t.volume_id)
        .fetch_one(&mut **tx)
        .await?;
        if busy {
            continue;
        }
        let id = new_id("bkp");
        // A volume that isn't attached: only where it is matters.
        let service = match &t.service_id {
            Some(sid) => crate::ops::service_ref(tx, sid).await?,
            None => ServiceRef {
                service_id: String::new(),
                name: String::new(),
                namespace: t.namespace.clone().unwrap_or_default(),
                workload_kind: String::new(),
            },
        };
        let mut tags: Vec<String> = t.service_id.iter().map(|s| format!("svc={s}")).collect();
        if let Some(v) = &t.volume_id {
            tags.push(format!("volume={v}"));
        }
        let spec = BackupSpec {
            backup_id: id.clone(),
            service,
            kind: t.kind.into(),
            claim: t.claim.clone().unwrap_or_default(),
            engine: t.engine.unwrap_or("").into(),
            tags,
            keep: cfg.base.keep,
            repository: repository(&cfg.base, team_id),
            password: password.clone(),
            access_key: access_key.clone(),
            secret_key: secret.clone(),
            region: region_of(&cfg.base),
            connection: None,
        };
        let command_id =
            deploy::enqueue(tx, &t.cluster_id, Command::Backup(Box::new(spec))).await?;
        sqlx::query(
            "INSERT INTO pyo.backups (id, team_id, cluster_id, service_id, volume_id, kind, status, trigger, label, command_id, started_at)
             VALUES ($1, $2, $3, $4, $5, $6, 'running', $7, $8, $9, now())",
        )
        .bind(&id)
        .bind(team_id)
        .bind(&t.cluster_id)
        .bind(&t.service_id)
        .bind(&t.volume_id)
        .bind(t.kind)
        .bind(trigger)
        .bind(&t.label)
        .bind(&command_id)
        .execute(&mut **tx)
        .await?;
        crate::bg::wake(&state.db, &t.cluster_id).await;
        n += 1;
    }
    Ok(n)
}

/// The agent's answer to a Backup command.
pub async fn finished(
    tx: &mut Transaction<'_, Postgres>,
    command: &Value,
    r: &pyo_proto::CommandResult,
) -> anyhow::Result<()> {
    let id = command["backupId"].as_str().unwrap_or("");
    let d = r.data.clone().unwrap_or(Value::Null);
    sqlx::query(
        "UPDATE pyo.backups SET status = $2, snapshot_id = $3, size_bytes = $4, added_bytes = $5, error = $6, finished_at = now() WHERE id = $1",
    )
    .bind(id)
    .bind(if r.ok { "succeeded" } else { "failed" })
    .bind(d["snapshotId"].as_str())
    .bind(d["bytes"].as_i64())
    .bind(d["added"].as_i64())
    .bind(if r.ok { None } else { r.message.clone() })
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// Backup commands carry the repository password and storage key: sealed
/// while queued, opened for the agent.
/// The secret fields of backup and database-setup commands.
fn secret_fields(cmd: &mut Value) -> Vec<&mut Value> {
    let kind = cmd["kind"].as_str().unwrap_or("").to_owned();
    let Some(o) = cmd.as_object_mut() else {
        return vec![];
    };
    let mut out = Vec::new();
    for (k, v) in o.iter_mut() {
        match (kind.as_str(), k.as_str()) {
            (
                "backup" | "db_setup" | "list_snapshots" | "forget_snapshot" | "restore_volume",
                "password",
            )
            | ("backup" | "list_snapshots" | "forget_snapshot" | "restore_volume", "secretKey") => {
                out.push(v)
            }
            ("backup", "connection") | ("db_setup", "admin") => {
                if let Some(p) = v.get_mut("password") {
                    out.push(p);
                }
            }
            _ => {}
        }
    }
    out
}

pub async fn seal_command(team_id: &str, cmd: &mut Value) -> anyhow::Result<()> {
    for v in secret_fields(cmd) {
        if let Some(plain) = v
            .as_str()
            .filter(|p| !p.is_empty() && !crate::vault::Vault::is_sealed(p))
            .map(str::to_owned)
        {
            *v = json!(crate::vault::get().seal(team_id, &plain).await?);
        }
    }
    Ok(())
}

pub async fn open_command(cmd: &mut Value) -> anyhow::Result<()> {
    for v in secret_fields(cmd) {
        if let Some(sealed) = v.as_str().map(str::to_owned) {
            *v = json!(crate::vault::get().open(&sealed).await?);
        }
    }
    Ok(())
}

/// Worker loop: scheduled backups per team, and runs the agent never answered.
pub async fn runner(state: AppState) {
    loop {
        let st = state.clone();
        crate::bg::as_leader(&state.db, crate::bg::LOCK_TEAM_BACKUP, || async move {
            if let Err(e) = tick(&st).await {
                tracing::warn!(error = %e, "team backup runner");
            }
        })
        .await;
        tokio::time::sleep(Duration::from_secs(60)).await;
    }
}

async fn tick(state: &AppState) -> anyhow::Result<()> {
    if let Err(e) = crate::database_backups::tick(state).await {
        tracing::warn!(error = %e, "database backups");
    }
    if let Err(e) = crate::backupstore::sweep(&state.db).await {
        tracing::warn!(error = %format!("{e:#}"), "backup store clean-up");
    }
    if let Err(e) = crate::volumes::sweep(state).await {
        tracing::warn!(error = ?e, "volume restores and moves");
    }
    sqlx::query("UPDATE pyo.backups SET status = 'failed', error = 'No answer from the cluster', finished_at = now() WHERE status = 'running' AND started_at < now() - interval '2 hours'")
        .execute(&state.db)
        .await?;
    let teams: Vec<(String, Value)> =
        sqlx::query_as("SELECT team_id, value FROM pyo.team_backup_settings")
            .fetch_all(&state.db)
            .await?;
    for (team, v) in teams {
        let Ok(cfg) = serde_json::from_value::<TeamSettings>(v) else {
            continue;
        };
        if !cfg.base.enabled {
            continue;
        }
        let last: Option<i64> = sqlx::query_scalar("SELECT pyo.ms(max(created_at)) FROM pyo.backups WHERE team_id = $1 AND trigger = 'schedule'")
            .bind(&team)
            .fetch_one(&state.db)
            .await?;
        if backup::due(&cfg.base, last) {
            let mut tx = state.db.begin().await?;
            match queue(state, &mut tx, &team, None, "schedule").await {
                Ok(n) => {
                    tx.commit().await?;
                    tracing::info!(%team, targets = n, "queued scheduled backups");
                }
                Err(e) => tracing::warn!(%team, error = ?e, "couldn't queue backups"),
            }
        }
    }
    Ok(())
}

/// The settings a team's backups use: its own, else (nothing saved) what a
/// backup would use, this server's store. None: nowhere to back up to.
fn shown_settings(saved: Option<TeamSettings>) -> Option<TeamSettings> {
    saved.or_else(|| {
        backup::local_store().map(|_| TeamSettings {
            base: backup::Settings::local_default(false),
            repo_password_sealed: String::new(),
            mode: all(),
            exceptions: vec![],
        })
    })
}

/// A volume's backups as the API reports them (`volume.backup`), from the
/// team's settings and its runs: "unsupported" (nothing to back it up to,
/// or not something Pyo backs up as files, say a database's), "disabled"
/// (left out), "failed" (its last run did) or "enabled". `lastAt`: its
/// last good backup.
pub fn volume_status(
    cfg: Option<&TeamSettings>,
    target: bool,
    volume_id: &str,
    last: Option<&(String, i64, Option<String>)>,
    last_good: Option<i64>,
) -> Value {
    let status = match cfg {
        Some(c) if target => {
            if !c.includes(volume_id) {
                "disabled"
            } else if last.is_some_and(|l| l.0 == "failed") {
                "failed"
            } else {
                "enabled"
            }
        }
        _ => "unsupported",
    };
    let mut out = json!({ "status": status });
    if let Some(at) = last_good {
        out["lastAt"] = json!(at);
    }
    if let Some((st, at, error)) = last {
        if st == "running" || st == "queued" {
            out["running"] = json!(true);
        }
        if st == "failed" {
            out["failedAt"] = json!(at);
            out["error"] = json!(error);
        }
    }
    if let Some(c) = cfg.filter(|_| status != "unsupported") {
        out["schedule"] = json!(if c.base.enabled {
            c.base.schedule.as_str()
        } else {
            "manual"
        });
    }
    out
}

/// Each of a team's volumes' `backup` (see [`volume_status`]), by id.
pub async fn volume_statuses(
    db: &PgPool,
    team_id: &str,
) -> anyhow::Result<std::collections::HashMap<String, Value>> {
    let cfg = shown_settings(settings(db, team_id).await?);
    let targets: std::collections::HashSet<String> = targets(db, team_id)
        .await?
        .into_iter()
        .filter_map(|t| t.volume_id)
        .collect();
    let last: Vec<(String, String, i64, Option<String>)> = sqlx::query_as(
        "SELECT DISTINCT ON (volume_id) volume_id, status, pyo.ms(coalesce(finished_at, created_at)), error
           FROM pyo.backups WHERE team_id = $1 AND volume_id IS NOT NULL ORDER BY volume_id, created_at DESC",
    )
    .bind(team_id)
    .fetch_all(db)
    .await?;
    let good: Vec<(String, i64)> = sqlx::query_as(
        "SELECT volume_id, pyo.ms(max(coalesce(finished_at, created_at))) FROM pyo.backups WHERE team_id = $1 AND volume_id IS NOT NULL AND status = 'succeeded' GROUP BY volume_id",
    )
    .bind(team_id)
    .fetch_all(db)
    .await?;
    let ids: Vec<String> = sqlx::query_scalar("SELECT id FROM pyo.volumes WHERE team_id = $1")
        .bind(team_id)
        .fetch_all(db)
        .await?;
    Ok(ids
        .into_iter()
        .map(|id| {
            let l = last
                .iter()
                .find(|l| l.0 == id)
                .map(|l| (l.1.clone(), l.2, l.3.clone()));
            let g = good.iter().find(|g| g.0 == id).map(|g| g.1);
            let v = volume_status(cfg.as_ref(), targets.contains(&id), &id, l.as_ref(), g);
            (id, v)
        })
        .collect())
}

/// The dashboard's view: each of the user's teams' settings, targets and runs.
pub async fn snapshot(db: &PgPool, team_ids: &[String]) -> ApiResult<Vec<Value>> {
    let mut out = Vec::new();
    for team in team_ids {
        // Nothing saved: what a backup would use (this server's store).
        let cfg = shown_settings(settings(db, team).await?);
        let history: Vec<Value> = sqlx::query_scalar(
            "SELECT jsonb_build_object('id', id, 'serviceId', service_id, 'volumeId', volume_id, 'kind', kind, 'status', status, 'trigger', trigger,
                                      'label', label, 'snapshotId', snapshot_id, 'sizeBytes', size_bytes, 'addedBytes', added_bytes,
                                      'error', error, 'createdAt', pyo.ms(created_at), 'finishedAt', pyo.ms(finished_at))
               FROM pyo.backups WHERE team_id = $1 ORDER BY created_at DESC LIMIT 40",
        )
        .bind(team)
        .fetch_all(db)
        .await?;
        out.push(json!({
            "teamId": team,
            "settings": cfg.as_ref().map(|c| {
                let mut p = c.base.public();
                p["repository"] = json!(repository(&c.base, team));
                p["mode"] = json!(c.mode);
                p["exceptions"] = json!(c.exceptions);
                p
            }),
            "databases": crate::database_backups::snapshot(db, team).await?,
            "targets": targets(db, team).await.context("targets").map_err(crate::error::ApiError::from)?.into_iter().map(|t| {
                let included = cfg.as_ref().is_none_or(|c| c.includes(&t.id));
                let mut v = serde_json::to_value(&t).unwrap_or(Value::Null);
                v["included"] = json!(included);
                v
            }).collect::<Vec<_>>(),
            "history": history,
        }));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `volume.backup` worked out from a real database: DATABASE_URL=<a
    /// throwaway Postgres> cargo test -p pyo-api -- --ignored volume_backup_status
    #[tokio::test]
    #[ignore = "needs DATABASE_URL (a throwaway Postgres)"]
    async fn volume_backup_status_in_the_database() {
        let pool = PgPool::connect(&std::env::var("DATABASE_URL").expect("DATABASE_URL"))
            .await
            .unwrap();
        crate::migrate::run(&pool).await.unwrap();
        let run = new_id("t");
        let (team, cluster, project) = (
            format!("team_{run}"),
            format!("cl_{run}"),
            format!("prj_{run}"),
        );
        sqlx::query("INSERT INTO pyo.teams (id, name, slug) VALUES ($1, $1, $1)")
            .bind(&team)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO pyo.clusters (id, team_id, name, agent_token_hash) VALUES ($1, $2, $1, $1)").bind(&cluster).bind(&team).execute(&pool).await.unwrap();
        sqlx::query(
            "INSERT INTO pyo.projects (id, team_id, name, namespace) VALUES ($1, $2, 'shop', $1)",
        )
        .bind(&project)
        .bind(&team)
        .execute(&pool)
        .await
        .unwrap();
        // Detached and used (a target), and one never used (not one).
        let (used, off, fresh) = (
            format!("vol_u{run}"),
            format!("vol_o{run}"),
            format!("vol_f{run}"),
        );
        for (id, status) in [(&used, "bound"), (&off, "bound"), (&fresh, "pending")] {
            sqlx::query("INSERT INTO pyo.volumes (id, team_id, cluster_id, project_id, data, namespace) VALUES ($1, $2, $3, $4, $5, $4)")
                .bind(id)
                .bind(&team)
                .bind(&cluster)
                .bind(&project)
                .bind(json!({ "name": id, "status": status, "backup": { "status": "unsupported" } }))
                .execute(&pool)
                .await
                .unwrap();
        }
        let backup = |id: String, vol: &String, status: &'static str, ago: i32| {
            sqlx::query("INSERT INTO pyo.backups (id, team_id, cluster_id, volume_id, kind, status, trigger, label, created_at, finished_at) VALUES ($1, $2, $3, $4, 'volume', $5, 'manual', 'x', now() - make_interval(mins => $6), now() - make_interval(mins => $6))")
                .bind(id)
                .bind(&team)
                .bind(&cluster)
                .bind(vol.clone())
                .bind(status)
                .bind(ago)
        };
        backup(format!("bkp_1{run}"), &used, "succeeded", 30)
            .execute(&pool)
            .await
            .unwrap();
        // Nowhere to back up to (no settings, no store of Pyo's): unsupported.
        if backup::local_store().is_none() {
            let all = volume_statuses(&pool, &team).await.unwrap();
            assert!(
                all.values().all(|v| v["status"] == "unsupported"),
                "{all:?}"
            );
            assert!(
                all[&used]["lastAt"].as_i64().is_some(),
                "its last backup still shows"
            );
        }
        let cfg = TeamSettings {
            base: backup::Settings::local_default(true),
            repo_password_sealed: "x".into(),
            mode: all(),
            exceptions: vec![off.clone()],
        };
        save(&pool, &team, &cfg).await.unwrap();
        let all = volume_statuses(&pool, &team).await.unwrap();
        assert_eq!(all[&used]["status"], "enabled");
        assert_eq!(all[&used]["schedule"], "daily");
        assert_eq!(all[&off]["status"], "disabled");
        assert_eq!(all[&fresh]["status"], "unsupported");
        // Its latest run failed: failed, with the last good one's time.
        backup(format!("bkp_2{run}"), &used, "failed", 5)
            .execute(&pool)
            .await
            .unwrap();
        let v = &volume_statuses(&pool, &team).await.unwrap()[&used];
        assert_eq!(v["status"], "failed");
        assert!(v["lastAt"].as_i64().unwrap() < v["failedAt"].as_i64().unwrap());
        // A good one after it: enabled again.
        backup(format!("bkp_3{run}"), &used, "succeeded", 1)
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(
            volume_statuses(&pool, &team).await.unwrap()[&used]["status"],
            "enabled"
        );
        sqlx::query("DELETE FROM pyo.teams WHERE id = $1")
            .bind(&team)
            .execute(&pool)
            .await
            .unwrap();
    }

    #[test]
    fn a_volumes_backup_status() {
        let cfg = TeamSettings {
            base: backup::Settings::local_default(false),
            repo_password_sealed: String::new(),
            mode: all(),
            exceptions: vec!["vol_off".into()],
        };
        let ok = ("succeeded".to_owned(), 2_000, None);
        let bad = ("failed".to_owned(), 3_000, Some("no space".to_owned()));
        let running = ("running".to_owned(), 4_000, None);
        assert_eq!(
            volume_status(None, true, "vol_a", None, None),
            json!({ "status": "unsupported" })
        );
        assert_eq!(
            volume_status(Some(&cfg), false, "vol_a", None, None),
            json!({ "status": "unsupported" })
        );
        assert_eq!(
            volume_status(Some(&cfg), true, "vol_a", Some(&ok), Some(2_000)),
            json!({ "status": "enabled", "lastAt": 2_000, "schedule": "manual" })
        );
        let failed = volume_status(Some(&cfg), true, "vol_a", Some(&bad), Some(2_000));
        assert_eq!(
            (
                failed["status"].as_str(),
                failed["lastAt"].as_i64(),
                failed["failedAt"].as_i64(),
                failed["error"].as_str()
            ),
            (Some("failed"), Some(2_000), Some(3_000), Some("no space"))
        );
        assert_eq!(
            volume_status(Some(&cfg), true, "vol_a", Some(&running), Some(2_000))["running"],
            true
        );
        assert_eq!(
            volume_status(Some(&cfg), true, "vol_off", Some(&ok), Some(2_000))["status"],
            "disabled"
        );
        let mut daily = cfg.clone();
        daily.base.enabled = true;
        assert_eq!(
            volume_status(Some(&daily), true, "vol_a", None, None)["schedule"],
            daily.base.schedule.as_str()
        );
    }

    #[test]
    fn engines_by_template_or_image() {
        let t = |tpl: &str, img: &str| {
            engine(&json!({ "source": { "templateId": tpl, "image": img } }))
        };
        assert_eq!(
            t("postgresql", "docker.io/library/postgres"),
            Some("postgres")
        );
        assert_eq!(t("", "ghcr.io/x/postgres-custom"), Some("postgres"));
        assert_eq!(t("mysql", ""), Some("mysql"));
        assert_eq!(t("", "docker.io/library/mariadb"), Some("mariadb"));
        assert_eq!(t("mongodb", ""), Some("mongo"));
        assert_eq!(t("valkey", "docker.io/valkey/valkey"), Some("redis"));
        assert_eq!(t("", "docker.io/library/redis"), Some("redis"));
        assert_eq!(t("", "docker.io/library/nginx"), None);
        assert_eq!(t("grafana", "docker.io/grafana/grafana"), None);
    }

    #[test]
    fn selection() {
        let mut c = TeamSettings {
            mode: "all".into(),
            exceptions: vec!["vol_a".into()],
            ..Default::default()
        };
        assert!(!c.includes("vol_a"));
        assert!(
            c.includes("svc_new"),
            "new targets are in when backing up everything"
        );
        c.mode = "selected".into();
        assert!(c.includes("vol_a"));
        assert!(!c.includes("svc_new"), "only picked ones when selecting");
    }

    #[test]
    fn every_secret_in_commands_is_found() {
        let mut backup = json!({ "kind": "backup", "password": "r", "secretKey": "s", "accessKey": "a", "connection": { "password": "db", "user": "u" } });
        let found: Vec<String> = secret_fields(&mut backup)
            .into_iter()
            .map(|v| v.as_str().unwrap().to_owned())
            .collect();
        assert_eq!(found.len(), 3);
        assert!(
            found.contains(&"r".into())
                && found.contains(&"s".into())
                && found.contains(&"db".into())
        );
        let mut setup = json!({ "kind": "db_setup", "password": "ro", "admin": { "password": "root", "user": "postgres" } });
        assert_eq!(secret_fields(&mut setup).len(), 2);
        let mut deploy = json!({ "kind": "deploy", "password": "not ours" });
        assert!(secret_fields(&mut deploy).is_empty());
        // A volume's restore and snapshot list carry the repository's keys at the top.
        let repo = pyo_proto::ResticRepo {
            password: "p".into(),
            secret_key: "k".into(),
            ..Default::default()
        };
        let restore = Command::RestoreVolume(Box::new(pyo_proto::RestoreSpec {
            restore_id: "r".into(),
            namespace: "n".into(),
            claim: "c".into(),
            volume_id: "v".into(),
            snapshot_id: "s".into(),
            service: None,
            repo: repo.clone(),
        }));
        let list = Command::ListSnapshots(Box::new(pyo_proto::SnapshotsSpec {
            request_id: "q".into(),
            namespace: "n".into(),
            volume_id: "v".into(),
            repo: repo.clone(),
        }));
        let forget = Command::ForgetSnapshot(Box::new(pyo_proto::ForgetSpec {
            request_id: "q".into(),
            namespace: "n".into(),
            volume_id: "v".into(),
            snapshot_id: "s".into(),
            repo,
        }));
        for c in [restore, list, forget] {
            let mut v = serde_json::to_value(&c).unwrap();
            let found: Vec<String> = secret_fields(&mut v)
                .into_iter()
                .map(|v| v.as_str().unwrap().to_owned())
                .collect();
            assert_eq!(found, ["p", "k"], "{v}");
        }
    }

    #[test]
    fn repository_paths() {
        let cfg = backup::Settings {
            destination: "s3".into(),
            endpoint: "https://s3.example.com/".into(),
            bucket: "b".into(),
            prefix: "/pyo/".into(),
            ..Default::default()
        };
        assert_eq!(
            repository(&cfg, "team_1"),
            "s3:https://s3.example.com/b/pyo/team_1"
        );
    }

    #[test]
    fn regions() {
        assert_eq!(region_of(&backup::Settings::local_default(true)), "garage");
        let s3 = |r: &str| backup::Settings {
            destination: "s3".into(),
            region: r.into(),
            ..Default::default()
        };
        assert_eq!(region_of(&s3("")), "us-east-1");
        assert_eq!(region_of(&s3(" eu-west-1 ")), "eu-west-1");
    }
}
