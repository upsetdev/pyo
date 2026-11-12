//! Volumes after they're made: detached from a service (kept, data and
//! all), attached to another service (moved to its project when that's
//! another one, on the same cluster), restored from a backup, grown, and
//! deleted.
//!
//! A restore stops the volume's service, restores the snapshot beside the
//! data on the same disk and swaps it in only once it's complete, then
//! starts the service again (agent volumes.rs, backup.rs). That works on
//! every storage class, keeps the volume (its id, server and backups) as
//! it is, and leaves the data untouched when the restore fails; it needs
//! room for the snapshot beside the data while it runs.
//!
//! A deleted volume keeps its row (`deleting`) until the cluster says its
//! claim is gone: the agent deletes it only when it carries the volume's id
//! and no running pod mounts it. Until then each heartbeat asks again (every
//! 2 minutes while it fails). A volume deleted with its service waits for
//! the service's objects to go first. Its id is kept (`deleted_volumes`), so
//! a new volume can't take it.
//!
//! Claims labelled as Pyo volumes that Pyo has no volume for are only
//! reported (`clusters.stray_volumes`), never deleted unasked.

use std::collections::HashSet;

use pyo_proto::{
    Command, CommandResult, MoveVolumeSpec, ResticRepo, RestoreSpec, SnapshotsSpec, VolumeReport,
    now_ms,
};
use serde_json::{Value, json};
use sqlx::{Postgres, Transaction};

use crate::{
    AppState,
    auth::{SessionUser, User},
    deploy,
    error::{ApiError, ApiResult, ErrorBody},
    ids::new_id,
    ops::{Role, log_activity, require_role, team_of},
};

/// A mount path as stored: absolute, no `..`, no trailing slash.
pub fn mount_path(raw: &str) -> ApiResult<String> {
    let path = raw.trim().trim_end_matches('/').to_owned();
    if !path.starts_with('/')
        || path.len() < 2
        || path.contains("..")
        || path.contains(char::is_whitespace)
    {
        return Err(ApiError::bad_request(
            "The folder is an absolute path, e.g. /data.",
        ));
    }
    Ok(path)
}

/// Why volume `name` (its `data`, `deleting`) can't change now: being
/// deleted, restored or moved. None when it can.
pub fn busy(name: &str, deleting: bool, data: &Value) -> Option<String> {
    if deleting {
        Some(format!("{name} is being deleted."))
    } else if data.get("restore").is_some_and(|r| !r.is_null()) {
        Some(format!(
            "{name} is being restored. Try again once it's done."
        ))
    } else if data.get("moving").is_some_and(|m| !m.is_null()) {
        Some(format!("{name} is being moved. Try again once it's done."))
    } else {
        None
    }
}

/// A service can't deploy, restart, stop or scale while one of its
/// volumes is being restored (the restore holds it stopped).
pub async fn not_restoring(tx: &mut Transaction<'_, Postgres>, service_id: &str) -> ApiResult<()> {
    let name: Option<String> = sqlx::query_scalar(
        "SELECT data->>'name' FROM pyo.volumes WHERE service_id = $1 AND data ? 'restore' LIMIT 1",
    )
    .bind(service_id)
    .fetch_optional(&mut **tx)
    .await?;
    match name {
        Some(n) => Err(ApiError::conflict(format!(
            "{n} is being restored. The service starts again once it's done."
        ))),
        None => Ok(()),
    }
}

/// Whether service `service_id` can take a volume (`single_writer`: one only
/// one server can use at a time): not while it autoscales, and not beside
/// cron jobs, whose runs would need the volume while the service holds it.
pub async fn target_ok(
    tx: &mut Transaction<'_, Postgres>,
    service_id: &str,
    single_writer: bool,
) -> ApiResult<()> {
    let (name, spec, deleting): (String, Value, bool) =
        sqlx::query_as("SELECT name, spec, deleting FROM pyo.services WHERE id = $1")
            .bind(service_id)
            .fetch_optional(&mut **tx)
            .await?
            .ok_or_else(|| ApiError::not_found("Service"))?;
    if deleting {
        return Err(ApiError::not_found("Service"));
    }
    if spec.get("autoscale").is_some_and(|a| !a.is_null()) {
        return Err(ApiError::conflict(
            "Turn off autoscaling first: a service with a volume runs as one replica.",
        ));
    }
    if single_writer && spec["cronJobs"].as_array().is_some_and(|j| !j.is_empty()) {
        return Err(ApiError::conflict(format!(
            "{name} has cron jobs, which can't run alongside a volume only one server can use."
        )));
    }
    Ok(())
}

/// Why a service with `spec` can't use a volume whose data is on `server`
/// (with `server_tags`, at `server_location`); None when it can run there.
pub fn placement_refusal(
    service: &str,
    spec: &Value,
    server: &str,
    server_tags: &[String],
    server_location: &str,
) -> Option<String> {
    let servers: Vec<&str> = spec["servers"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|s| s.as_str())
        .filter(|s| !s.is_empty())
        .collect();
    if !servers.is_empty() && !servers.contains(&server) {
        return Some(format!(
            "{service} only runs on {}, and this volume's data is on {server}.",
            servers.join(", ")
        ));
    }
    let missing: Vec<&str> = spec["serverTags"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|t| t.as_str())
        .filter(|t| !server_tags.iter().any(|s| s == t))
        .collect();
    if !missing.is_empty() {
        return Some(format!(
            "{service} runs on servers tagged {}, and {server}, where this volume's data is, isn't.",
            missing.join(", ")
        ));
    }
    let want = spec["location"].as_str().unwrap_or("");
    if let Some((region, zone)) = pyo_proto::location_labels(want) {
        let fits = match pyo_proto::location_labels(server_location) {
            Some((r, z)) => r == region && (zone.is_none() || zone == z),
            None => false,
        };
        if !fits {
            return Some(format!(
                "{service} runs in {want}, and {server}, where this volume's data is, isn't there."
            ));
        }
    }
    None
}

/// The server a volume's data is pinned to: the one it was made on, when
/// its storage class keeps data on one server's disk (or isn't known).
async fn pinned_server(
    tx: &mut Transaction<'_, Postgres>,
    cluster_id: &str,
    data: &Value,
) -> ApiResult<Option<String>> {
    let server = data["server"].as_str().unwrap_or("");
    if server.is_empty() {
        return Ok(None);
    }
    let caps: Value =
        sqlx::query_scalar("SELECT coalesce(capabilities, '{}') FROM pyo.clusters WHERE id = $1")
            .bind(cluster_id)
            .fetch_one(&mut **tx)
            .await?;
    let class = data["storageClass"].as_str().unwrap_or("");
    let replicated = caps["storageClasses"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|c| c["name"].as_str() == Some(class))
        .is_some_and(|c| c["replicated"] == json!(true));
    Ok((!replicated).then(|| server.to_owned()))
}

/// Unmount a volume from its service, from its next deploy. The volume
/// stays, data and all, for attaching again or deleting.
pub async fn detach(tx: &mut Transaction<'_, Postgres>, user: &User, id: &str) -> ApiResult<Value> {
    let team_id = team_of(tx, "volumes", id).await?;
    require_role(tx, user, &team_id, Role::Developer).await?;
    let (service_id, name, deleting, data): (Option<String>, String, bool, Value) =
        sqlx::query_as("SELECT service_id, data->>'name', deleting, data FROM pyo.volumes WHERE id = $1 FOR UPDATE").bind(id).fetch_one(&mut **tx).await?;
    if let Some(why) = busy(&name, deleting, &data) {
        return Err(ApiError::conflict(why));
    }
    let Some(service_id) = service_id else {
        return Ok(json!({ "ok": true }));
    };
    sqlx::query("UPDATE pyo.volumes SET service_id = NULL WHERE id = $1")
        .bind(id)
        .execute(&mut **tx)
        .await?;
    let (service, project_id): (String, String) =
        sqlx::query_as("UPDATE pyo.services SET pending_changes = true, updated_at = now() WHERE id = $1 RETURNING name, project_id").bind(&service_id).fetch_one(&mut **tx).await?;
    log_activity(
        tx,
        &team_id,
        &user.name,
        "volume.detached",
        "info",
        &format!("Detached volume {name} from {service}"),
        json!({ "kind": "volume", "id": id, "label": name }),
        json!({ "serviceId": service_id, "projectId": project_id }),
    )
    .await?;
    Ok(json!({ "ok": true }))
}

/// Mount a detached volume in a service on its cluster, from the service's
/// next deploy. A service of another project (or any, for a volume whose
/// project was deleted) gets it moved first: its claim goes to that
/// project's namespace, data and all ([`moved`] finishes the attach).
pub async fn attach(
    tx: &mut Transaction<'_, Postgres>,
    user: &User,
    id: &str,
    service_id: &str,
    mount_path_raw: &str,
    wake: &mut Vec<String>,
) -> ApiResult<Value> {
    let team_id = team_of(tx, "volumes", id).await?;
    require_role(tx, user, &team_id, Role::Developer).await?;
    let (project_id, cluster_id, attached, deleting, data, namespace): (Option<String>, String, Option<String>, bool, Value, Option<String>) = sqlx::query_as(
        "SELECT v.project_id, v.cluster_id, v.service_id, v.deleting, v.data, coalesce(v.namespace, p.namespace)
           FROM pyo.volumes v LEFT JOIN pyo.projects p ON p.id = v.project_id WHERE v.id = $1 FOR UPDATE OF v",
    )
    .bind(id)
    .fetch_one(&mut **tx)
    .await?;
    let name = data["name"].as_str().unwrap_or("").to_owned();
    if let Some(why) = busy(&name, deleting, &data) {
        return Err(ApiError::conflict(why));
    }
    let path = mount_path(mount_path_raw)?;
    if let Some(a) = &attached {
        if a == service_id && data["mountPath"].as_str() == Some(path.as_str()) {
            return Ok(json!({ "ok": true }));
        }
        let other: String = sqlx::query_scalar("SELECT name FROM pyo.services WHERE id = $1")
            .bind(a)
            .fetch_one(&mut **tx)
            .await?;
        return Err(ApiError::conflict(format!(
            "{name} is attached to {other}. Detach it first."
        )));
    }
    let row: Option<(String, String, String, String, Value, bool, String)> = sqlx::query_as(
        "SELECT a.team_id, a.project_id, a.cluster_id, a.name, a.spec, a.deleting, p.namespace FROM pyo.services a JOIN pyo.projects p ON p.id = a.project_id WHERE a.id = $1",
    )
    .bind(service_id)
    .fetch_optional(&mut **tx)
    .await?;
    let Some((
        service_team,
        service_project,
        service_cluster,
        service_name,
        spec,
        service_deleting,
        service_namespace,
    )) = row.filter(|r| r.0 == team_id)
    else {
        return Err(ApiError::not_found("Service"));
    };
    if service_deleting {
        return Err(ApiError::not_found("Service"));
    }
    if service_cluster != cluster_id {
        return Err(ApiError::bad_request(format!(
            "{name} is on another cluster than {service_name}. A volume can't move between clusters, so pick a service on its cluster."
        )));
    }
    target_ok(
        tx,
        service_id,
        data["accessMode"].as_str().unwrap_or("ReadWriteOnce") == "ReadWriteOnce",
    )
    .await?;
    let taken: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM pyo.volumes WHERE service_id = $1 AND data->>'mountPath' = $2)").bind(service_id).bind(&path).fetch_one(&mut **tx).await?;
    if taken {
        return Err(ApiError::conflict(format!(
            "Another volume is already mounted at {path}."
        )));
    }
    // Data on one server's disk: the service has to be able to run there.
    if let Some(server) = pinned_server(tx, &cluster_id, &data).await? {
        let (tags, location): (Vec<String>, String) = sqlx::query_as(
            "SELECT coalesce((SELECT t.tags FROM pyo.server_tags t WHERE t.cluster_id = c.id AND t.name = $2), '{}'),
                    coalesce((SELECT n.location FROM pyo.servers n WHERE n.cluster_id = c.id AND n.name = $2), c.location, '')
               FROM pyo.clusters c WHERE c.id = $1",
        )
        .bind(&cluster_id)
        .bind(&server)
        .fetch_one(&mut **tx)
        .await?;
        if let Some(why) = placement_refusal(&service_name, &spec, &server, &tags, &location) {
            return Err(ApiError::conflict(why));
        }
    }
    let target = json!({ "kind": "volume", "id": id, "label": name });
    let from_ns = namespace.unwrap_or_default();
    if project_id.as_deref() == Some(service_project.as_str()) || from_ns == service_namespace {
        sqlx::query("UPDATE pyo.volumes SET service_id = $2, project_id = $4, data = data || jsonb_build_object('mountPath', $3::text) WHERE id = $1")
            .bind(id)
            .bind(service_id)
            .bind(&path)
            .bind(&service_project)
            .execute(&mut **tx)
            .await?;
        sqlx::query(
            "UPDATE pyo.services SET pending_changes = true, updated_at = now() WHERE id = $1",
        )
        .bind(service_id)
        .execute(&mut **tx)
        .await?;
        log_activity(tx, &service_team, &user.name, "volume.attached", "info", &format!("Attached volume {name} to {service_name} at {path}"), target, json!({ "serviceId": service_id, "projectId": service_project, "clusterId": cluster_id }))
            .await?;
        return Ok(json!({ "ok": true }));
    }
    // Another project: its claim moves to that project's namespace first.
    if from_ns.is_empty() {
        return Err(ApiError::conflict(format!(
            "Pyo doesn't know where {name} is any more."
        )));
    }
    let clash: Option<String> = sqlx::query_scalar(
        "SELECT 'volume' FROM pyo.volumes v LEFT JOIN pyo.projects p ON p.id = v.project_id
          WHERE v.cluster_id = $1 AND v.id <> $4 AND v.data->>'name' = $2 AND coalesce(v.namespace, p.namespace) = $3
         UNION ALL
         SELECT 'stray' FROM pyo.clusters c, jsonb_array_elements(c.stray_volumes) e WHERE c.id = $1 AND e->>'namespace' = $3 AND e->>'name' = $2
         LIMIT 1",
    )
    .bind(&cluster_id)
    .bind(&name)
    .bind(&service_namespace)
    .bind(id)
    .fetch_optional(&mut **tx)
    .await?;
    if clash.is_some() {
        let project: String = sqlx::query_scalar("SELECT name FROM pyo.projects WHERE id = $1")
            .bind(&service_project)
            .fetch_one(&mut **tx)
            .await?;
        return Err(ApiError::conflict(format!(
            "{project} already has a volume named {name}."
        )));
    }
    let moving = json!({ "projectId": service_project, "serviceId": service_id, "mountPath": path, "fromNamespace": from_ns, "toNamespace": service_namespace, "startedAt": now_ms() });
    sqlx::query("UPDATE pyo.volumes SET data = (data - 'moveError') || jsonb_build_object('moving', $2::jsonb) WHERE id = $1").bind(id).bind(&moving).execute(&mut **tx).await?;
    deploy::enqueue(
        tx,
        &cluster_id,
        Command::MoveVolume(Box::new(MoveVolumeSpec {
            volume_id: id.to_owned(),
            name: name.clone(),
            from_namespace: from_ns,
            to_namespace: service_namespace,
            project_id: service_project.clone(),
            team_id: team_id.clone(),
            service_id: service_id.to_owned(),
        })),
    )
    .await?;
    wake.push(cluster_id.clone());
    let project: String = sqlx::query_scalar("SELECT name FROM pyo.projects WHERE id = $1")
        .bind(&service_project)
        .fetch_one(&mut **tx)
        .await?;
    log_activity(
        tx,
        &team_id,
        &user.name,
        "volume.moving",
        "info",
        &format!("Moving volume {name} to {project} for {service_name}"),
        target,
        json!({ "serviceId": service_id, "projectId": service_project, "clusterId": cluster_id }),
    )
    .await?;
    Ok(json!({ "ok": true, "moving": true }))
}

/// The agent moved a volume (or didn't): attached to the service it was
/// moved for, in its new project; or left where it was, with why.
async fn moved(
    tx: &mut Transaction<'_, Postgres>,
    cluster_id: &str,
    id: &str,
    ok: bool,
    message: Option<&str>,
) -> ApiResult<()> {
    let row: Option<(String, Option<String>, Value)> = sqlx::query_as("SELECT team_id, project_id, data FROM pyo.volumes WHERE id = $1 AND cluster_id = $2 FOR UPDATE").bind(id).bind(cluster_id).fetch_optional(&mut **tx).await?;
    let Some((team_id, old_project, data)) = row else {
        return Ok(());
    };
    let m = data["moving"].clone();
    if m.is_null() {
        return Ok(());
    }
    let name = data["name"].as_str().unwrap_or("").to_owned();
    let target = json!({ "kind": "volume", "id": id, "label": name });
    if !ok {
        let why = message.unwrap_or("The cluster couldn't move it.");
        sqlx::query("UPDATE pyo.volumes SET data = (data - 'moving') || jsonb_build_object('moveError', $2::text) WHERE id = $1").bind(id).bind(why).execute(&mut **tx).await?;
        log_activity(
            tx,
            &team_id,
            "Pyo",
            "volume.move_failed",
            "failure",
            &format!("Couldn't move volume {name}: {why}"),
            target,
            json!({ "clusterId": cluster_id }),
        )
        .await?;
        return Ok(());
    }
    let (project, service, path, from_ns, to_ns) = (
        m["projectId"].as_str().unwrap_or(""),
        m["serviceId"].as_str().unwrap_or(""),
        m["mountPath"].as_str().unwrap_or("/data"),
        m["fromNamespace"].as_str().unwrap_or(""),
        m["toNamespace"].as_str().unwrap_or(""),
    );
    // Attached, unless the service went (or took that folder) meanwhile.
    let service_row: Option<String> = sqlx::query_scalar(
        "SELECT a.name FROM pyo.services a WHERE a.id = $1 AND a.project_id = $2 AND NOT a.deleting
            AND NOT EXISTS (SELECT 1 FROM pyo.volumes v WHERE v.service_id = a.id AND v.data->>'mountPath' = $3)",
    )
    .bind(service)
    .bind(project)
    .bind(path)
    .fetch_optional(&mut **tx)
    .await?;
    sqlx::query("UPDATE pyo.volumes SET project_id = $2, namespace = $3, service_id = $4, data = (data - 'moving' - 'moveError') || jsonb_build_object('mountPath', $5::text) WHERE id = $1")
        .bind(id)
        .bind(project)
        .bind(to_ns)
        .bind(service_row.as_ref().map(|_| service))
        .bind(path)
        .execute(&mut **tx)
        .await?;
    if let Some(service_name) = &service_row {
        sqlx::query(
            "UPDATE pyo.services SET pending_changes = true, updated_at = now() WHERE id = $1",
        )
        .bind(service)
        .execute(&mut **tx)
        .await?;
        log_activity(
            tx,
            &team_id,
            "Pyo",
            "volume.attached",
            "info",
            &format!("Attached volume {name} to {service_name} at {path}"),
            target,
            json!({ "serviceId": service, "projectId": project, "clusterId": cluster_id }),
        )
        .await?;
    }
    // The last volume out of a deleted project's namespace lets it go.
    if let Some(old) = old_project.filter(|p| p != project) {
        let left: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM pyo.projects WHERE id = $1) OR EXISTS (SELECT 1 FROM pyo.volumes WHERE cluster_id = $2 AND (project_id = $1 OR namespace = $3))",
        )
        .bind(&old)
        .bind(cluster_id)
        .bind(from_ns)
        .fetch_one(&mut **tx)
        .await?;
        if !left && !from_ns.is_empty() {
            deploy::enqueue(
                tx,
                cluster_id,
                Command::DeleteNamespace {
                    namespace: from_ns.to_owned(),
                    project_id: old,
                },
            )
            .await?;
        }
    }
    Ok(())
}

// ── Restore, snapshots and growing ─────────────────────────────────────────

/// The team's restic repository, for a volume on `cluster_id`.
async fn repo_for(
    s: &AppState,
    tx: &mut Transaction<'_, Postgres>,
    team_id: &str,
    cluster_id: &str,
) -> ApiResult<ResticRepo> {
    let Some(cfg) = crate::teambackup::settings(&s.db, team_id).await? else {
        return Err(ApiError::conflict(
            "This team has no backups yet. Set them up under Backups.",
        ));
    };
    if cfg.base.is_local() && !crate::teambackup::on_pyos_cluster(tx, cluster_id).await? {
        return Err(ApiError::conflict(crate::teambackup::LOCAL_ONLY));
    }
    let password = crate::vault::get().open(&cfg.repo_password_sealed).await?;
    let (_, access_key, secret_key) = crate::teambackup::destination(s, team_id, &cfg).await?;
    Ok(ResticRepo {
        repository: crate::teambackup::repository_of(&cfg, team_id),
        password,
        access_key,
        secret_key,
        region: crate::teambackup::region_of(&cfg.base),
    })
}

/// A volume, for restoring, growing or listing snapshots: team, cluster,
/// service, deleting, data, namespace.
type VolumeRow = (String, String, Option<String>, bool, Value, Option<String>);

async fn volume_row(tx: &mut Transaction<'_, Postgres>, id: &str) -> ApiResult<VolumeRow> {
    sqlx::query_as(
        "SELECT v.team_id, v.cluster_id, v.service_id, v.deleting, v.data, coalesce(v.namespace, p.namespace)
           FROM pyo.volumes v LEFT JOIN pyo.projects p ON p.id = v.project_id WHERE v.id = $1 FOR UPDATE OF v",
    )
    .bind(id)
    .fetch_optional(&mut **tx)
    .await?
    .ok_or_else(|| ApiError::not_found("Volume"))
}

/// A snapshot id as restic writes it.
fn snapshot_id_ok(s: &str) -> bool {
    (8..=64).contains(&s.len()) && s.chars().all(|c| c.is_ascii_hexdigit())
}

/// Put a snapshot back into a volume. Its service is stopped while it
/// runs and started again after, whatever happened. Owners and admins.
pub async fn restore(
    s: &AppState,
    tx: &mut Transaction<'_, Postgres>,
    user: &User,
    id: &str,
    snapshot_id: &str,
    wake: &mut Vec<String>,
) -> ApiResult<Value> {
    let team_id = team_of(tx, "volumes", id).await?;
    require_role(tx, user, &team_id, Role::Admin).await?;
    let (_, cluster_id, service_id, deleting, data, namespace) = volume_row(tx, id).await?;
    let name = data["name"].as_str().unwrap_or("").to_owned();
    if let Some(why) = busy(&name, deleting, &data) {
        return Err(ApiError::conflict(why));
    }
    let snapshot_id = snapshot_id.trim().to_lowercase();
    if !snapshot_id_ok(&snapshot_id) {
        return Err(ApiError::bad_request(
            "Pick one of the volume's snapshots (its id).",
        ));
    }
    let Some(namespace) = namespace else {
        return Err(ApiError::conflict(format!(
            "Pyo doesn't know where {name} is any more."
        )));
    };
    let repo = repo_for(s, tx, &team_id, &cluster_id).await?;
    let mut service = None;
    let mut context = json!({ "clusterId": cluster_id });
    if let Some(sid) = &service_id {
        let (status, project_id, live): (String, String, Option<String>) = sqlx::query_as(
            "SELECT status, project_id, current_deployment_id FROM pyo.services WHERE id = $1",
        )
        .bind(sid)
        .fetch_one(&mut **tx)
        .await?;
        if status == "deploying" {
            return Err(ApiError::conflict(
                "A deploy is running. Restore once it finishes.",
            ));
        }
        context = json!({ "clusterId": cluster_id, "serviceId": sid, "projectId": project_id });
        if live.is_some() {
            service = Some(crate::ops::service_ref(tx, sid).await?);
        }
    }
    let restore_id = new_id("rst");
    let stops = service.as_ref().map(|s| s.name.clone());
    deploy::enqueue(
        tx,
        &cluster_id,
        Command::RestoreVolume(Box::new(RestoreSpec {
            restore_id: restore_id.clone(),
            namespace,
            claim: name.clone(),
            volume_id: id.to_owned(),
            snapshot_id: snapshot_id.clone(),
            service,
            repo,
        })),
    )
    .await?;
    let restore = json!({ "id": restore_id, "snapshotId": snapshot_id, "startedAt": now_ms() });
    sqlx::query("UPDATE pyo.volumes SET data = (data - 'restoreError') || jsonb_build_object('restore', $2::jsonb) WHERE id = $1").bind(id).bind(&restore).execute(&mut **tx).await?;
    if stops.is_some() {
        sqlx::query("UPDATE pyo.services SET status = 'restoring' WHERE id = $1")
            .bind(&service_id)
            .execute(&mut **tx)
            .await?;
    }
    wake.push(cluster_id);
    let short: String = snapshot_id.chars().take(8).collect();
    let title = match &stops {
        Some(svc) => format!(
            "Restoring volume {name} from snapshot {short}; {svc} is stopped until it's done"
        ),
        None => format!("Restoring volume {name} from snapshot {short}"),
    };
    log_activity(
        tx,
        &team_id,
        &user.name,
        "volume.restoring",
        "info",
        &title,
        json!({ "kind": "volume", "id": id, "label": name }),
        context,
    )
    .await?;
    Ok(json!({ "ok": true }))
}

async fn restored(
    tx: &mut Transaction<'_, Postgres>,
    cluster_id: &str,
    command: &Value,
    ok: bool,
    message: Option<&str>,
) -> ApiResult<()> {
    let id = command["volumeId"].as_str().unwrap_or("");
    let row: Option<(String, Option<String>, Value)> = sqlx::query_as("SELECT team_id, service_id, data FROM pyo.volumes WHERE id = $1 AND cluster_id = $2 FOR UPDATE").bind(id).bind(cluster_id).fetch_optional(&mut **tx).await?;
    let Some((team_id, service_id, data)) = row else {
        return Ok(());
    };
    // Only the restore this answers (not an older one's late answer).
    if data["restore"]["id"] != command["restoreId"] {
        return Ok(());
    }
    let name = data["name"].as_str().unwrap_or("");
    let short: String = data["restore"]["snapshotId"]
        .as_str()
        .unwrap_or("")
        .chars()
        .take(8)
        .collect();
    let target = json!({ "kind": "volume", "id": id, "label": name });
    let context = json!({ "clusterId": cluster_id, "serviceId": service_id });
    if ok {
        sqlx::query("UPDATE pyo.volumes SET data = (data - 'restore' - 'restoreError') || jsonb_build_object('restoredAt', $2::bigint) WHERE id = $1").bind(id).bind(now_ms()).execute(&mut **tx).await?;
        log_activity(
            tx,
            &team_id,
            "Pyo",
            "volume.restored",
            "success",
            &format!("Restored volume {name} from snapshot {short}"),
            target,
            context,
        )
        .await?;
    } else {
        let why = message.unwrap_or("The cluster couldn't restore it.");
        sqlx::query("UPDATE pyo.volumes SET data = (data - 'restore') || jsonb_build_object('restoreError', $2::text) WHERE id = $1").bind(id).bind(why).execute(&mut **tx).await?;
        log_activity(
            tx,
            &team_id,
            "Pyo",
            "volume.restore_failed",
            "failure",
            &format!("Couldn't restore volume {name}: {why}"),
            target,
            context,
        )
        .await?;
    }
    Ok(())
}

/// One of a volume's backups, as restic lists it.
#[derive(serde::Serialize, serde::Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Snapshot {
    /// Its id, to restore it.
    id: String,
    short_id: Option<String>,
    /// When it was taken, in milliseconds since 1970.
    time: i64,
    /// The volume's size then, in bytes; null when restic didn't record it.
    size_bytes: Option<i64>,
}

/// A volume's snapshots
///
/// Its backups in the team's restic repository, newest first, read from
/// its cluster on request (a short job there). Takes up to two minutes.
#[utoipa::path(get, path = "/volumes/{id}/snapshots", operation_id = "listVolumeSnapshots", tag = "Volumes",
    params(("id" = String, Path, description = "The volume's id (`vol_…`).")),
    responses(
        (status = 200, description = "`snapshots`: each one's `id`, `shortId`, `time` and `sizeBytes`, newest first.", body = Object),
        (status = 404, description = "No such volume, or it isn't yours to see.", body = ErrorBody),
        (status = 409, description = "The team has no backups, or its backup store can't be reached from the volume's cluster.", body = ErrorBody),
        (status = 502, description = "The cluster couldn't list them.", body = ErrorBody),
        (status = 504, description = "The cluster didn't answer in time (is it connected?).", body = ErrorBody),
    ),
    extensions(("x-pyo-access" = json!("read"))))]
pub(crate) async fn snapshots(
    axum::extract::State(s): axum::extract::State<AppState>,
    SessionUser(user): SessionUser,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> ApiResult<axum::Json<Value>> {
    let mut tx = s.db.begin().await?;
    let team_id = team_of(&mut tx, "volumes", &id).await?;
    require_role(&mut tx, &user, &team_id, Role::Developer).await?;
    let (_, cluster_id, _, deleting, data, namespace) = volume_row(&mut tx, &id).await?;
    let name = data["name"].as_str().unwrap_or("");
    if deleting {
        return Err(ApiError::conflict(format!("{name} is being deleted.")));
    }
    let Some(namespace) = namespace else {
        return Err(ApiError::conflict(format!(
            "Pyo doesn't know where {name} is any more."
        )));
    };
    let repo = repo_for(&s, &mut tx, &team_id, &cluster_id).await?;
    let cmd_id = deploy::enqueue(
        &mut tx,
        &cluster_id,
        Command::ListSnapshots(Box::new(SnapshotsSpec {
            request_id: new_id("snp"),
            namespace,
            volume_id: id.clone(),
            repo,
        })),
    )
    .await?;
    tx.commit().await?;
    s.wake(&cluster_id).await;
    let r = answer(&s, &cmd_id, "list the snapshots").await?;
    let list: Vec<Snapshot> = r
        .data
        .and_then(|d| serde_json::from_value(d["snapshots"].clone()).ok())
        .unwrap_or_default();
    Ok(axum::Json(json!({ "snapshots": list })))
}

/// Wait for a command the API waits on (a short Job on the cluster): its
/// result, or why there's none.
async fn answer(s: &AppState, cmd_id: &str, what: &str) -> ApiResult<CommandResult> {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(120);
    loop {
        let row: Option<(String, Option<Value>)> =
            sqlx::query_as("SELECT status, result FROM pyo.commands WHERE id = $1")
                .bind(cmd_id)
                .fetch_optional(&s.db)
                .await?;
        if let Some((status, result)) = row
            && (status == "done" || status == "failed")
        {
            let r: CommandResult = result
                .and_then(|v| serde_json::from_value(v).ok())
                .unwrap_or_default();
            if !r.ok {
                return Err(ApiError::new(
                    axum::http::StatusCode::BAD_GATEWAY,
                    r.message
                        .unwrap_or_else(|| format!("The cluster couldn't {what}.")),
                ));
            }
            return Ok(r);
        }
        if tokio::time::Instant::now() > deadline {
            return Err(ApiError::new(
                axum::http::StatusCode::GATEWAY_TIMEOUT,
                "The cluster's agent didn't answer in time. Is it connected?",
            ));
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
}

/// Delete a volume snapshot
///
/// Deletes one of a volume's backups from the team's restic repository,
/// with a short job on its cluster that waits for the volume's other
/// backups and restores. The space it held is freed shortly after. Deleting
/// one that's already gone succeeds. Owners and admins.
#[utoipa::path(delete, path = "/volumes/{id}/snapshots/{snapshotId}", operation_id = "deleteVolumeSnapshot", tag = "Volumes",
    params(
        ("id" = String, Path, description = "The volume's id (`vol_…`)."),
        ("snapshotId" = String, Path, description = "The snapshot's id, or its short id (see listVolumeSnapshots)."),
    ),
    responses(
        (status = 200, description = "`ok`: deleted (or already gone).", body = Object),
        (status = 400, description = "Not a snapshot id.", body = ErrorBody),
        (status = 403, description = "Only owners and admins, with an API key that has full access.", body = ErrorBody),
        (status = 404, description = "No such volume, or it isn't yours to see.", body = ErrorBody),
        (status = 409, description = "The team has no backups, or its backup store can't be reached from the volume's cluster.", body = ErrorBody),
        (status = 502, description = "The cluster couldn't delete it.", body = ErrorBody),
        (status = 504, description = "The cluster didn't answer in time (is it connected?).", body = ErrorBody),
    ),
    extensions(("x-pyo-access" = json!("full"))))]
pub(crate) async fn delete_snapshot(
    axum::extract::State(s): axum::extract::State<AppState>,
    SessionUser(user): SessionUser,
    axum::extract::Path((id, snapshot_id)): axum::extract::Path<(String, String)>,
) -> ApiResult<axum::Json<Value>> {
    if user
        .key
        .as_ref()
        .is_some_and(|k| k.access != crate::auth::KeyAccess::Full)
    {
        return Err(ApiError::forbidden(
            "Deleting a backup needs an API key with full access.",
        ));
    }
    let snapshot_id = snapshot_id.trim().to_lowercase();
    if !snapshot_id_ok(&snapshot_id) {
        return Err(ApiError::bad_request(
            "Pick one of the volume's snapshots (its id).",
        ));
    }
    let mut tx = s.db.begin().await?;
    let team_id = team_of(&mut tx, "volumes", &id).await?;
    require_role(&mut tx, &user, &team_id, Role::Admin).await?;
    let (_, cluster_id, service_id, deleting, data, namespace) = volume_row(&mut tx, &id).await?;
    let name = data["name"].as_str().unwrap_or("").to_owned();
    if deleting {
        return Err(ApiError::conflict(format!("{name} is being deleted.")));
    }
    let Some(namespace) = namespace else {
        return Err(ApiError::conflict(format!(
            "Pyo doesn't know where {name} is any more."
        )));
    };
    let repo = repo_for(&s, &mut tx, &team_id, &cluster_id).await?;
    let spec = pyo_proto::ForgetSpec {
        request_id: new_id("fgt"),
        namespace,
        volume_id: id.clone(),
        snapshot_id: snapshot_id.clone(),
        repo,
    };
    let cmd_id = deploy::enqueue(
        &mut tx,
        &cluster_id,
        Command::ForgetSnapshot(Box::new(spec)),
    )
    .await?;
    tx.commit().await?;
    s.wake(&cluster_id).await;
    let r = answer(&s, &cmd_id, "delete the snapshot").await?;
    let short: String = snapshot_id.chars().take(8).collect();
    let mut tx = s.db.begin().await?;
    log_activity(
        &mut tx,
        &team_id,
        &user.name,
        "volume.snapshot_deleted",
        "info",
        &format!("Deleted backup {short} of volume {name}"),
        json!({ "kind": "volume", "id": id, "label": name }),
        json!({ "clusterId": cluster_id, "serviceId": service_id }),
    )
    .await?;
    tx.commit().await?;
    Ok(axum::Json(json!({ "ok": true, "message": r.message })))
}

/// Why a volume on storage class `class` (as its cluster reports it) can't
/// grow; None when it can.
fn growth_refusal(class: &str, caps: &Value) -> Option<String> {
    let Some(sc) = caps["storageClasses"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|c| c["name"].as_str() == Some(class))
    else {
        return Some(format!(
            "The cluster doesn't report the {class} storage class, so Pyo can't tell whether it can grow."
        ));
    };
    if sc["provisioner"] == json!("rancher.io/local-path") {
        return Some(
            "local-path doesn't cap a volume: it can already use all the free disk on its server."
                .into(),
        );
    }
    if sc["allowVolumeExpansion"] != json!(true) {
        return Some(format!("The {class} storage class can't grow volumes."));
    }
    None
}

/// Grow a volume (never shrink it), on a storage class that can. Its
/// service keeps running; some classes finish when it next starts.
pub async fn resize(
    tx: &mut Transaction<'_, Postgres>,
    user: &User,
    id: &str,
    size_gi: i64,
    wake: &mut Vec<String>,
) -> ApiResult<Value> {
    let team_id = team_of(tx, "volumes", id).await?;
    require_role(tx, user, &team_id, Role::Admin).await?;
    let (_, cluster_id, service_id, deleting, data, namespace) = volume_row(tx, id).await?;
    let name = data["name"].as_str().unwrap_or("").to_owned();
    if let Some(why) = busy(&name, deleting, &data) {
        return Err(ApiError::conflict(why));
    }
    let now = data["resizeTo"]
        .as_i64()
        .or(data["sizeGi"].as_i64())
        .unwrap_or(1);
    if size_gi <= now {
        return Err(ApiError::bad_request(format!(
            "A volume can only grow: pick more than {now} GB."
        )));
    }
    if size_gi > 1000 {
        return Err(ApiError::bad_request("Pick a size up to 1000 GB."));
    }
    let caps: Value =
        sqlx::query_scalar("SELECT coalesce(capabilities, '{}') FROM pyo.clusters WHERE id = $1")
            .bind(&cluster_id)
            .fetch_one(&mut **tx)
            .await?;
    if let Some(why) = growth_refusal(data["storageClass"].as_str().unwrap_or(""), &caps) {
        return Err(ApiError::conflict(why));
    }
    if data["status"] != json!("bound") {
        return Err(ApiError::conflict(format!(
            "{name} isn't in use yet. It can grow once a service has used it."
        )));
    }
    let Some(namespace) = namespace else {
        return Err(ApiError::conflict(format!(
            "Pyo doesn't know where {name} is any more."
        )));
    };
    deploy::enqueue(
        tx,
        &cluster_id,
        Command::ResizeVolume {
            namespace,
            name: name.clone(),
            volume_id: id.to_owned(),
            size_gi,
        },
    )
    .await?;
    sqlx::query("UPDATE pyo.volumes SET data = (data - 'resizeError') || jsonb_build_object('resizeTo', $2::bigint) WHERE id = $1").bind(id).bind(size_gi).execute(&mut **tx).await?;
    wake.push(cluster_id.clone());
    let from = data["sizeGi"].as_i64().unwrap_or(0);
    log_activity(
        tx,
        &team_id,
        &user.name,
        "volume.resized",
        "info",
        &format!("Growing volume {name} from {from} to {size_gi} GB"),
        json!({ "kind": "volume", "id": id, "label": name }),
        json!({ "clusterId": cluster_id, "serviceId": service_id }),
    )
    .await?;
    Ok(json!({ "ok": true }))
}

async fn resize_failed(
    tx: &mut Transaction<'_, Postgres>,
    cluster_id: &str,
    id: &str,
    message: Option<&str>,
) -> ApiResult<()> {
    let row: Option<(String, String)> =
        sqlx::query_as("UPDATE pyo.volumes SET data = (data - 'resizeTo') || jsonb_build_object('resizeError', $3::text) WHERE id = $1 AND cluster_id = $2 AND data ? 'resizeTo' RETURNING team_id, data->>'name'")
            .bind(id)
            .bind(cluster_id)
            .bind(message.unwrap_or("The cluster couldn't grow it."))
            .fetch_optional(&mut **tx)
            .await?;
    if let Some((team_id, name)) = row {
        let why = message.unwrap_or("the cluster couldn't grow it");
        log_activity(
            tx,
            &team_id,
            "Pyo",
            "volume.resize_failed",
            "failure",
            &format!("Couldn't grow volume {name}: {why}"),
            json!({ "kind": "volume", "id": id, "label": name }),
            json!({ "clusterId": cluster_id }),
        )
        .await?;
    }
    Ok(())
}

/// Restores and moves the agent never answered (it restarted, say): failed
/// after longer than they can take, and a stopped service started again.
pub async fn sweep(state: &AppState) -> ApiResult<()> {
    let mut tx = state.db.begin().await?;
    let stale: Vec<(String, String, String, Option<String>, Value)> = sqlx::query_as(
        "SELECT id, team_id, cluster_id, service_id, data FROM pyo.volumes
          WHERE ((data->'restore'->>'startedAt')::bigint < $1 - 3 * 3600 * 1000) OR ((data->'moving'->>'startedAt')::bigint < $1 - 1800 * 1000)
          FOR UPDATE",
    )
    .bind(now_ms())
    .fetch_all(&mut *tx)
    .await?;
    for (id, _team, cluster, service, data) in stale {
        let why = "No answer from the cluster.";
        if data.get("restore").is_some() {
            let cmd = json!({ "volumeId": id, "restoreId": data["restore"]["id"] });
            restored(&mut tx, &cluster, &cmd, false, Some(why)).await?;
            // Started again at its size, unless it's stopped.
            if let Some(sid) = service {
                let row: Option<(String, Value, Option<String>)> = sqlx::query_as(
                    "SELECT status, spec, current_deployment_id FROM pyo.services WHERE id = $1",
                )
                .bind(&sid)
                .fetch_optional(&mut *tx)
                .await?;
                if let Some((status, spec, Some(_))) = row
                    && status != "stopped"
                {
                    let replicas = spec["replicas"].as_i64().unwrap_or(1).max(1) as i32;
                    let service = crate::ops::service_ref(&mut tx, &sid).await?;
                    deploy::enqueue(&mut tx, &cluster, Command::Scale { service, replicas })
                        .await?;
                }
            }
        } else {
            moved(&mut tx, &cluster, &id, false, Some(why)).await?;
        }
    }
    tx.commit().await?;
    Ok(())
}

/// A volume being deleted: team, cluster, service, deleting, name, namespace.
type DeleteRow = (String, String, Option<String>, bool, String, Option<String>);

/// Delete a detached volume and its data, for good: `id` is a volume's, or
/// (with `cluster_id`) a claim's on that cluster that Pyo has no volume for.
pub async fn delete(
    tx: &mut Transaction<'_, Postgres>,
    user: &User,
    id: &str,
    cluster_id: Option<&str>,
    wake: &mut Vec<String>,
) -> ApiResult<Value> {
    let row: Option<DeleteRow> = sqlx::query_as(
        "SELECT v.team_id, v.cluster_id, v.service_id, v.deleting, v.data->>'name', coalesce(v.namespace, p.namespace)
           FROM pyo.volumes v LEFT JOIN pyo.projects p ON p.id = v.project_id WHERE v.id = $1 FOR UPDATE OF v",
    )
    .bind(id)
    .fetch_optional(&mut **tx)
    .await?;
    let Some((team_id, cluster, service_id, deleting, name, namespace)) = row else {
        return match cluster_id {
            Some(c) => delete_stray(tx, user, c, id, wake).await,
            None => Err(ApiError::not_found("Volume")),
        };
    };
    require_role(tx, user, &team_id, Role::Admin).await?;
    let data: Value = sqlx::query_scalar("SELECT data FROM pyo.volumes WHERE id = $1")
        .bind(id)
        .fetch_one(&mut **tx)
        .await?;
    if !deleting && let Some(why) = busy(&name, false, &data) {
        return Err(ApiError::conflict(why));
    }
    if let Some(a) = service_id {
        let service: String = sqlx::query_scalar("SELECT name FROM pyo.services WHERE id = $1")
            .bind(&a)
            .fetch_one(&mut **tx)
            .await?;
        return Err(ApiError::conflict(format!(
            "{name} is attached to {service}. Detach it first."
        )));
    }
    if deleting {
        return Ok(json!({ "ok": true }));
    }
    let Some(namespace) = namespace else {
        return Err(ApiError::conflict(format!(
            "Pyo doesn't know where {name} is any more."
        )));
    };
    sqlx::query("UPDATE pyo.volumes SET deleting = true, namespace = $2, data = data - 'deleteError' WHERE id = $1").bind(id).bind(&namespace).execute(&mut **tx).await?;
    deploy::enqueue(
        tx,
        &cluster,
        Command::DeleteVolume {
            namespace,
            name: name.clone(),
            volume_id: id.to_owned(),
        },
    )
    .await?;
    wake.push(cluster.clone());
    log_activity(
        tx,
        &team_id,
        &user.name,
        "volume.deleted",
        "warning",
        &format!("Deleted volume {name}"),
        json!({ "kind": "volume", "id": id, "label": name }),
        json!({ "clusterId": cluster }),
    )
    .await?;
    Ok(json!({ "ok": true }))
}

/// A claim on `cluster_id` labelled as volume `id` that Pyo has no volume
/// for (from the cluster's last heartbeat).
async fn delete_stray(
    tx: &mut Transaction<'_, Postgres>,
    user: &User,
    cluster_id: &str,
    id: &str,
    wake: &mut Vec<String>,
) -> ApiResult<Value> {
    let team_id = team_of(tx, "clusters", cluster_id).await?;
    require_role(tx, user, &team_id, Role::Admin).await?;
    let strays: Value =
        sqlx::query_scalar("SELECT stray_volumes FROM pyo.clusters WHERE id = $1 FOR UPDATE")
            .bind(cluster_id)
            .fetch_one(&mut **tx)
            .await?;
    let Some(s) = strays
        .as_array()
        .into_iter()
        .flatten()
        .find(|s| s["volumeId"].as_str() == Some(id))
    else {
        return Err(ApiError::not_found("Volume"));
    };
    let (namespace, name) = (
        s["namespace"].as_str().unwrap_or("").to_owned(),
        s["name"].as_str().unwrap_or("").to_owned(),
    );
    sqlx::query(
        "UPDATE pyo.clusters SET stray_volumes = (SELECT coalesce(jsonb_agg(CASE WHEN e->>'volumeId' = $2 THEN (e - 'error') || '{\"deleting\": true}' ELSE e END), '[]') FROM jsonb_array_elements(stray_volumes) e)
          WHERE id = $1",
    )
    .bind(cluster_id)
    .bind(id)
    .execute(&mut **tx)
    .await?;
    deploy::enqueue(
        tx,
        cluster_id,
        Command::DeleteVolume {
            namespace,
            name: name.clone(),
            volume_id: id.to_owned(),
        },
    )
    .await?;
    wake.push(cluster_id.to_owned());
    log_activity(
        tx,
        &team_id,
        &user.name,
        "volume.deleted",
        "warning",
        &format!("Deleted volume {name}"),
        json!({ "kind": "volume", "id": id, "label": name }),
        json!({ "clusterId": cluster_id }),
    )
    .await?;
    Ok(json!({ "ok": true }))
}

/// A service's volumes, to be deleted once the service is gone (its delete
/// command done: see [`service_deleted`]). Their names.
pub async fn delete_with_service(
    tx: &mut Transaction<'_, Postgres>,
    user: &User,
    team_id: &str,
    service_id: &str,
) -> ApiResult<Vec<String>> {
    let any: bool =
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM pyo.volumes WHERE service_id = $1)")
            .bind(service_id)
            .fetch_one(&mut **tx)
            .await?;
    if !any {
        return Ok(vec![]);
    }
    require_role(tx, user, team_id, Role::Admin).await?;
    Ok(sqlx::query_scalar(
        "UPDATE pyo.volumes v SET deleting = true, namespace = coalesce(v.namespace, p.namespace), data = v.data - 'deleteError'
           FROM pyo.projects p WHERE v.service_id = $1 AND p.id = v.project_id RETURNING v.data->>'name'",
    )
    .bind(service_id)
    .fetch_all(&mut **tx)
    .await?)
}

/// A project being deleted, before its row (and its services') goes: its
/// volumes are kept (they keep its namespace on the cluster too), or deleted
/// when `delete`, each on its cluster after its service and before the
/// namespace. Those already being deleted with a service go now too, since
/// that service's row goes with the project. The names of those kept, or of
/// those deleted.
pub async fn project_deleted(
    tx: &mut Transaction<'_, Postgres>,
    project_id: &str,
    namespace: &str,
    delete: bool,
) -> ApiResult<Vec<String>> {
    sqlx::query(
        "UPDATE pyo.volumes SET namespace = $2 WHERE project_id = $1 AND namespace IS NULL",
    )
    .bind(project_id)
    .bind(namespace)
    .execute(&mut **tx)
    .await?;
    let kept: Vec<String> = if delete {
        vec![]
    } else {
        sqlx::query_scalar("SELECT data->>'name' FROM pyo.volumes WHERE project_id = $1 AND NOT deleting ORDER BY data->>'name'").bind(project_id).fetch_all(&mut **tx).await?
    };
    let rows: Vec<(String, String, String, String)> = sqlx::query_as(
        "UPDATE pyo.volumes SET deleting = true, data = data - 'deleteError'
          WHERE project_id = $1 AND (($2 AND NOT deleting) OR (deleting AND service_id IS NOT NULL))
          RETURNING id, cluster_id, namespace, data->>'name'",
    )
    .bind(project_id)
    .bind(delete)
    .fetch_all(&mut **tx)
    .await?;
    let mut names = vec![];
    for (id, cluster, ns, name) in rows {
        deploy::enqueue(
            tx,
            &cluster,
            Command::DeleteVolume {
                namespace: ns,
                name: name.clone(),
                volume_id: id,
            },
        )
        .await?;
        names.push(name);
    }
    names.sort();
    Ok(if delete { names } else { kept })
}

/// The agent finished deleting service `label` (its label id): its volumes
/// being deleted with it go now; if it failed, they're kept.
pub async fn service_deleted(
    tx: &mut Transaction<'_, Postgres>,
    cluster_id: &str,
    label: &str,
    ok: bool,
) -> ApiResult<()> {
    if !ok {
        sqlx::query("UPDATE pyo.volumes SET deleting = false WHERE deleting AND service_id IN (SELECT id FROM pyo.services WHERE id = $1 AND cluster_id = $2)")
            .bind(label)
            .bind(cluster_id)
            .execute(&mut **tx)
            .await?;
        return Ok(());
    }
    let rows: Vec<(String, String, String)> = sqlx::query_as(
        "SELECT v.id, v.namespace, v.data->>'name' FROM pyo.volumes v JOIN pyo.services a ON a.id = v.service_id
          WHERE a.id = $1 AND a.cluster_id = $2 AND a.deleting AND v.deleting AND v.namespace IS NOT NULL",
    )
    .bind(label)
    .bind(cluster_id)
    .fetch_all(&mut **tx)
    .await?;
    for (id, ns, name) in rows {
        deploy::enqueue(
            tx,
            cluster_id,
            Command::DeleteVolume {
                namespace: ns,
                name,
                volume_id: id,
            },
        )
        .await?;
    }
    Ok(())
}

/// A volume confirmed gone from the cluster: its row goes, its id stays
/// behind. The last of a deleted project's volumes there lets its
/// namespace go too.
async fn forget(tx: &mut Transaction<'_, Postgres>, cluster_id: &str, id: &str) -> ApiResult<()> {
    let gone: Option<(String, Option<String>)> = sqlx::query_as(
        "WITH gone AS (DELETE FROM pyo.volumes WHERE id = $1 AND cluster_id = $2 AND deleting RETURNING id, cluster_id, coalesce(namespace, '') AS namespace, data->>'name' AS name, project_id)
         INSERT INTO pyo.deleted_volumes (id, cluster_id, namespace, name) SELECT id, cluster_id, namespace, name FROM gone
         ON CONFLICT (id) DO UPDATE SET deleted_at = now()
         RETURNING namespace, (SELECT project_id FROM gone)",
    )
    .bind(id)
    .bind(cluster_id)
    .fetch_optional(&mut **tx)
    .await?;
    let Some((namespace, Some(project_id))) = gone else {
        return Ok(());
    };
    let left: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM pyo.projects WHERE id = $1) OR EXISTS (SELECT 1 FROM pyo.volumes WHERE cluster_id = $2 AND (project_id = $1 OR namespace = $3))",
    )
    .bind(&project_id)
    .bind(cluster_id)
    .bind(&namespace)
    .fetch_one(&mut **tx)
    .await?;
    if !left && !namespace.is_empty() {
        deploy::enqueue(
            tx,
            cluster_id,
            Command::DeleteNamespace {
                namespace,
                project_id,
            },
        )
        .await?;
    }
    Ok(())
}

/// The agent finished a volume's deletion: gone, or why not (shown on the
/// volume; heartbeats ask again).
pub async fn finished(
    tx: &mut Transaction<'_, Postgres>,
    cluster_id: &str,
    command: &Value,
    ok: bool,
    message: Option<&str>,
) -> ApiResult<()> {
    match command["kind"].as_str() {
        Some("delete_volume") => {}
        Some("restore_volume") => return restored(tx, cluster_id, command, ok, message).await,
        Some("move_volume") => {
            return moved(
                tx,
                cluster_id,
                command["volumeId"].as_str().unwrap_or(""),
                ok,
                message,
            )
            .await;
        }
        Some("resize_volume") if !ok => {
            return resize_failed(
                tx,
                cluster_id,
                command["volume_id"].as_str().unwrap_or(""),
                message,
            )
            .await;
        }
        _ => return Ok(()),
    }
    let id = command["volume_id"].as_str().unwrap_or("");
    if ok {
        forget(tx, cluster_id, id).await?;
        sqlx::query("UPDATE pyo.clusters SET stray_volumes = (SELECT coalesce(jsonb_agg(e), '[]') FROM jsonb_array_elements(stray_volumes) e WHERE e->>'volumeId' <> $2) WHERE id = $1")
            .bind(cluster_id)
            .bind(id)
            .execute(&mut **tx)
            .await?;
        return Ok(());
    }
    let message = message.unwrap_or("The cluster couldn't delete it.");
    sqlx::query("UPDATE pyo.volumes SET data = data || jsonb_build_object('deleteError', $3::text) WHERE id = $1 AND cluster_id = $2 AND deleting").bind(id).bind(cluster_id).bind(message).execute(&mut **tx).await?;
    sqlx::query(
        "UPDATE pyo.clusters SET stray_volumes = (SELECT coalesce(jsonb_agg(CASE WHEN e->>'volumeId' = $2 THEN (e - 'deleting') || jsonb_build_object('error', $3::text) ELSE e END), '[]') FROM jsonb_array_elements(stray_volumes) e)
          WHERE id = $1",
    )
    .bind(cluster_id)
    .bind(id)
    .bind(message)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// A volume row, for one heartbeat's cleanup.
#[derive(Debug, Clone, PartialEq)]
struct Row {
    id: String,
    namespace: String,
    name: String,
    attached: bool,
    deleting: bool,
}

#[derive(Debug, Default, PartialEq)]
struct Cleanup {
    /// Gone from the cluster: the row goes.
    forget: Vec<String>,
    /// Ask (again) for the claim's deletion: (namespace, name, id).
    delete: Vec<(String, String, String)>,
    /// Labelled as a Pyo volume, but Pyo has none (nor just deleted it).
    strays: Vec<Value>,
}

/// What a heartbeat's list of volume claims means for `rows` (this
/// cluster's volumes). `in_flight`: ids with a deletion on its way;
/// `just_deleted`: ids whose rows went moments ago (a heartbeat read before
/// that still lists them).
fn plan_cleanup(
    reports: &[VolumeReport],
    rows: &[Row],
    in_flight: &HashSet<String>,
    just_deleted: &HashSet<String>,
) -> Cleanup {
    let mut out = Cleanup::default();
    let on_cluster: HashSet<&str> = reports.iter().map(|r| r.volume_id.as_str()).collect();
    for r in rows.iter().filter(|r| r.deleting) {
        if !on_cluster.contains(r.id.as_str()) {
            out.forget.push(r.id.clone());
        } else if !r.attached && !in_flight.contains(&r.id) && !r.namespace.is_empty() {
            out.delete
                .push((r.namespace.clone(), r.name.clone(), r.id.clone()));
        }
    }
    let known: HashSet<&str> = rows.iter().map(|r| r.id.as_str()).collect();
    for v in reports {
        if known.contains(v.volume_id.as_str())
            || just_deleted.contains(&v.volume_id)
            || v.name.is_empty()
            || v.namespace.is_empty()
        {
            continue;
        }
        let mut s = json!({ "volumeId": v.volume_id, "namespace": v.namespace, "name": v.name, "sizeGi": v.size_gi, "server": v.server });
        if in_flight.contains(&v.volume_id) {
            s["deleting"] = json!(true);
        }
        out.strays.push(s);
    }
    out.strays.sort_by(|a, b| {
        (a["namespace"].as_str(), a["name"].as_str())
            .cmp(&(b["namespace"].as_str(), b["name"].as_str()))
    });
    out
}

/// A volume as stored: id, namespace, name, attached, deleting.
type ReportedRow = (String, Option<String>, Option<String>, bool, bool);

/// What a heartbeat found: each volume's state and server, deletions
/// confirmed or asked again, and claims Pyo has no volume for.
pub async fn reported(
    tx: &mut Transaction<'_, Postgres>,
    cluster_id: &str,
    reports: &[VolumeReport],
) -> ApiResult<()> {
    for v in reports {
        sqlx::query(
            "UPDATE pyo.volumes SET data = (data - 'usedGi') || jsonb_strip_nulls(jsonb_build_object('status', $3::text, 'statusMessage', $4::text, 'usedGi', $5::float8, 'server', nullif($6, '')))
              WHERE id = $1 AND cluster_id = $2",
        )
        .bind(&v.volume_id)
        .bind(cluster_id)
        .bind(&v.status)
        .bind(&v.message)
        // Space used, from the kubelet while it's mounted and reports the
        // volume's own space (not on local-path: that's the server's disk).
        .bind(v.usage.map(|u| u.used_gi))
        .bind(&v.server)
        .execute(&mut **tx)
        .await?;
        // Grown: its new size is the volume's.
        if let Some(cap) = v.capacity_gi {
            sqlx::query(
                "UPDATE pyo.volumes SET data = (data - 'resizeTo') || jsonb_build_object('sizeGi', (data->>'resizeTo')::bigint)
                  WHERE id = $1 AND cluster_id = $2 AND data ? 'resizeTo' AND (data->>'resizeTo')::bigint <= $3",
            )
            .bind(&v.volume_id)
            .bind(cluster_id)
            .bind(cap)
            .execute(&mut **tx)
            .await?;
        }
    }
    let rows: Vec<ReportedRow> =
        sqlx::query_as("SELECT id, namespace, data->>'name', service_id IS NOT NULL, deleting FROM pyo.volumes WHERE cluster_id = $1").bind(cluster_id).fetch_all(&mut **tx).await?;
    let rows: Vec<Row> = rows
        .into_iter()
        .map(|(id, ns, name, attached, deleting)| Row {
            id,
            namespace: ns.unwrap_or_default(),
            name: name.unwrap_or_default(),
            attached,
            deleting,
        })
        .collect();
    // A deletion on its way: queued, with the agent (one lost to a restart
    // is asked again after 10 minutes), or tried in the last 2 (so a
    // failing one is retried every 2).
    let in_flight: HashSet<String> = sqlx::query_scalar(
        "SELECT DISTINCT command->>'volume_id' FROM pyo.commands
          WHERE cluster_id = $1 AND command->>'kind' = 'delete_volume'
            AND (status = 'pending' OR (status = 'sent' AND sent_at > now() - interval '10 minutes') OR created_at > now() - interval '2 minutes')",
    )
    .bind(cluster_id)
    .fetch_all(&mut **tx)
    .await?
    .into_iter()
    .collect();
    let just_deleted: HashSet<String> =
        sqlx::query_scalar("SELECT id FROM pyo.deleted_volumes WHERE cluster_id = $1 AND deleted_at > now() - interval '2 minutes'").bind(cluster_id).fetch_all(&mut **tx).await?.into_iter().collect();
    let plan = plan_cleanup(reports, &rows, &in_flight, &just_deleted);
    for id in &plan.forget {
        tracing::info!(cluster = cluster_id, volume = %id, "volume gone from the cluster; removing it");
        forget(tx, cluster_id, id).await?;
    }
    for (namespace, name, volume_id) in plan.delete {
        tracing::info!(cluster = cluster_id, volume = %volume_id, "volume deleted in Pyo is still on the cluster; asking again");
        deploy::enqueue(
            tx,
            cluster_id,
            Command::DeleteVolume {
                namespace,
                name,
                volume_id,
            },
        )
        .await?;
    }
    // Keep a failed deletion's message until the next try.
    let before: Value = sqlx::query_scalar("SELECT stray_volumes FROM pyo.clusters WHERE id = $1")
        .bind(cluster_id)
        .fetch_one(&mut **tx)
        .await?;
    let mut strays = plan.strays;
    for s in &mut strays {
        if s.get("deleting").is_none()
            && let Some(e) = before
                .as_array()
                .into_iter()
                .flatten()
                .find(|b| b["volumeId"] == s["volumeId"])
                .and_then(|b| b.get("error"))
        {
            s["error"] = e.clone();
        }
    }
    sqlx::query("UPDATE pyo.clusters SET stray_volumes = $2 WHERE id = $1 AND stray_volumes IS DISTINCT FROM $2").bind(cluster_id).bind(json!(strays)).execute(&mut **tx).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(id: &str, name: &str) -> VolumeReport {
        VolumeReport {
            volume_id: id.into(),
            status: "bound".into(),
            message: None,
            usage: None,
            namespace: "shop".into(),
            name: name.into(),
            size_gi: Some(5),
            capacity_gi: Some(5),
            server: "a".into(),
        }
    }

    fn row(id: &str, attached: bool, deleting: bool) -> Row {
        Row {
            id: id.into(),
            namespace: "shop".into(),
            name: format!("{id}-data"),
            attached,
            deleting,
        }
    }

    fn ids(v: &[&str]) -> HashSet<String> {
        v.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn deletions_are_confirmed_by_the_cluster() {
        let reports = [
            report("vol_keep", "keep"),
            report("vol_busy", "busy"),
            report("vol_svc", "svc"),
            report("vol_asked", "asked"),
        ];
        let rows = [
            row("vol_keep", true, false),
            row("vol_gone", false, true),
            row("vol_busy", false, true),
            row("vol_svc", true, true),
            row("vol_asked", false, true),
        ];
        let plan = plan_cleanup(&reports, &rows, &ids(&["vol_asked"]), &ids(&[]));
        // Gone from the cluster: forgotten.
        assert_eq!(plan.forget, ["vol_gone"]);
        // Still there and detached, nothing on its way: asked again. Its
        // service's objects not gone yet, or asked already: waits.
        assert_eq!(
            plan.delete,
            [(
                "shop".to_owned(),
                "vol_busy-data".to_owned(),
                "vol_busy".to_owned()
            )]
        );
        assert!(plan.strays.is_empty());
    }

    #[test]
    fn claims_without_a_volume_are_only_reported() {
        let mut old = report("vol_old", "old");
        old.namespace = "a".into();
        let unnamed = VolumeReport {
            name: String::new(),
            ..report("vol_older_agent", "")
        };
        let reports = [
            report("vol_x", "x"),
            old,
            report("vol_just", "just"),
            unnamed,
            report("vol_y", "y"),
        ];
        let plan = plan_cleanup(
            &reports,
            &[row("vol_y", true, false)],
            &ids(&["vol_x"]),
            &ids(&["vol_just"]),
        );
        assert!(plan.forget.is_empty() && plan.delete.is_empty());
        let names: Vec<&str> = plan
            .strays
            .iter()
            .map(|s| s["name"].as_str().unwrap())
            .collect();
        assert_eq!(
            names,
            ["old", "x"],
            "sorted; not one just deleted, one without a name or one Pyo has"
        );
        assert_eq!(plan.strays[1]["deleting"], true);
        assert_eq!(plan.strays[1]["sizeGi"], 5);
        assert!(plan.strays[0].get("deleting").is_none());
    }

    #[test]
    fn a_pinned_volume_goes_where_its_server_is() {
        let tags = ["gpu".to_owned()];
        let spec = |v: Value| v;
        assert!(placement_refusal("web", &spec(json!({})), "a", &[], "").is_none());
        assert!(
            placement_refusal("web", &spec(json!({ "servers": ["a", "b"] })), "a", &[], "")
                .is_none()
        );
        assert!(
            placement_refusal("web", &spec(json!({ "servers": ["b"] })), "a", &[], "")
                .unwrap()
                .contains("only runs on b")
        );
        assert!(
            placement_refusal(
                "web",
                &spec(json!({ "serverTags": ["gpu"] })),
                "a",
                &tags,
                ""
            )
            .is_none()
        );
        assert!(
            placement_refusal(
                "web",
                &spec(json!({ "serverTags": ["gpu", "ssd"] })),
                "a",
                &tags,
                ""
            )
            .unwrap()
            .contains("tagged ssd")
        );
        assert!(
            placement_refusal(
                "web",
                &spec(json!({ "location": "DE" })),
                "a",
                &[],
                "Frankfurt, DE"
            )
            .is_none()
        );
        assert!(
            placement_refusal(
                "web",
                &spec(json!({ "location": "Frankfurt, DE" })),
                "a",
                &[],
                "Frankfurt, DE"
            )
            .is_none()
        );
        assert!(
            placement_refusal(
                "web",
                &spec(json!({ "location": "Berlin, DE" })),
                "a",
                &[],
                "Frankfurt, DE"
            )
            .is_some()
        );
        assert!(
            placement_refusal(
                "web",
                &spec(json!({ "location": "SG" })),
                "a",
                &[],
                "Frankfurt, DE"
            )
            .is_some()
        );
        assert!(
            placement_refusal("web", &spec(json!({ "location": "SG" })), "a", &[], "").is_some(),
            "unknown place"
        );
    }

    #[test]
    fn mount_paths() {
        assert_eq!(mount_path(" /data/ ").unwrap(), "/data");
        for bad in ["data", "/", "/a/../b", "/a b", ""] {
            assert!(mount_path(bad).is_err(), "{bad}");
        }
    }

    async fn asked(tx: &mut Transaction<'_, Postgres>, cluster: &str, vol: &str) -> i64 {
        sqlx::query_scalar("SELECT count(*) FROM pyo.commands WHERE cluster_id = $1 AND command->>'kind' = 'delete_volume' AND command->>'volume_id' = $2")
            .bind(cluster)
            .bind(vol)
            .fetch_one(&mut **tx)
            .await
            .unwrap()
    }

    /// Needs Postgres: DATABASE_URL=… cargo test -p pyo-api -- --ignored volumes::tests
    #[tokio::test]
    #[ignore]
    async fn volume_lifecycle_in_the_database() {
        let url = std::env::var("DATABASE_URL").expect("DATABASE_URL");
        let pool = sqlx::PgPool::connect(&url).await.unwrap();
        crate::migrate::run(&pool).await.unwrap();
        crate::vault::init(crate::vault::Vault::new(
            pool.clone(),
            crate::vault::Provider::from_env(),
        ));
        let run = crate::ids::new_id("t");
        let (team, cluster, project, service, other) = (
            format!("team_{run}"),
            format!("cl_{run}"),
            format!("prj_{run}"),
            format!("svc_{run}"),
            format!("svc_o{run}"),
        );
        let user = User {
            id: format!("usr_{run}"),
            name: "T".into(),
            email: format!("{run}@t.test"),
            timezone: "UTC".into(),
            is_admin: false,
            must_set_up: false,
            key: None,
        };
        let mut tx = pool.begin().await.unwrap();
        sqlx::query(
            "INSERT INTO pyo.users (id, email, name, password_hash) VALUES ($1, $2, 'T', '')",
        )
        .bind(&user.id)
        .bind(&user.email)
        .execute(&mut *tx)
        .await
        .unwrap();
        sqlx::query("INSERT INTO pyo.teams (id, name, slug) VALUES ($1, $1, $1)")
            .bind(&team)
            .execute(&mut *tx)
            .await
            .unwrap();
        sqlx::query("INSERT INTO pyo.members (id, team_id, user_id, email, role, status) VALUES ($1, $2, $3, $4, 'owner', 'active')").bind(format!("mem_{run}")).bind(&team).bind(&user.id).bind(&user.email).execute(&mut *tx).await.unwrap();
        sqlx::query("INSERT INTO pyo.clusters (id, team_id, name, agent_token_hash) VALUES ($1, $2, $1, $1)").bind(&cluster).bind(&team).execute(&mut *tx).await.unwrap();
        sqlx::query(
            "INSERT INTO pyo.projects (id, team_id, name, namespace) VALUES ($1, $2, 'shop', $3)",
        )
        .bind(&project)
        .bind(&team)
        .bind(format!("shop-{run}"))
        .execute(&mut *tx)
        .await
        .unwrap();
        for (id, name, spec) in [
            (&service, "web", json!({ "name": "web" })),
            (&other, "api", json!({ "name": "api", "servers": ["b"] })),
        ] {
            sqlx::query("INSERT INTO pyo.services (id, team_id, project_id, cluster_id, name, spec) VALUES ($1, $2, $3, $4, $5, $6)").bind(id).bind(&team).bind(&project).bind(&cluster).bind(name).bind(spec).execute(&mut *tx).await.unwrap();
        }
        let vol = format!("vol_{run}");
        sqlx::query("INSERT INTO pyo.volumes (id, team_id, cluster_id, project_id, service_id, data, namespace) VALUES ($1, $2, $3, $4, $5, $6, $7)")
            .bind(&vol)
            .bind(&team)
            .bind(&cluster)
            .bind(&project)
            .bind(&service)
            .bind(json!({ "name": "web-data", "mountPath": "/data", "sizeGi": 5, "storageClass": "local-path", "accessMode": "ReadWriteOnce", "server": "a" }))
            .bind(format!("shop-{run}"))
            .execute(&mut *tx)
            .await
            .unwrap();
        let mut wake = vec![];
        // Attached: can't be deleted.
        assert!(
            delete(&mut tx, &user, &vol, None, &mut wake)
                .await
                .unwrap_err()
                .message
                .contains("Detach it first")
        );
        // Detached: kept, unmounted from the next deploy.
        detach(&mut tx, &user, &vol).await.unwrap();
        let service_id: Option<String> =
            sqlx::query_scalar("SELECT service_id FROM pyo.volumes WHERE id = $1")
                .bind(&vol)
                .fetch_one(&mut *tx)
                .await
                .unwrap();
        assert!(service_id.is_none());
        // Its data is on server a; api only runs on b.
        assert!(
            attach(&mut tx, &user, &vol, &other, "/srv", &mut wake)
                .await
                .unwrap_err()
                .message
                .contains("only runs on b")
        );
        attach(&mut tx, &user, &vol, &service, "/files/", &mut wake)
            .await
            .unwrap();
        let path: String =
            sqlx::query_scalar("SELECT data->>'mountPath' FROM pyo.volumes WHERE id = $1")
                .bind(&vol)
                .fetch_one(&mut *tx)
                .await
                .unwrap();
        assert_eq!(path, "/files");
        detach(&mut tx, &user, &vol).await.unwrap();
        // Deleted: kept until the cluster confirms; asked once.
        delete(&mut tx, &user, &vol, None, &mut wake).await.unwrap();
        assert_eq!(wake, std::slice::from_ref(&cluster));
        assert!(
            attach(&mut tx, &user, &vol, &service, "/data", &mut wake)
                .await
                .is_err(),
            "being deleted"
        );
        assert_eq!(asked(&mut tx, &cluster, &vol).await, 1);
        // Still on the cluster, the deletion on its way: not asked again.
        reported(&mut tx, &cluster, &[report(&vol, "web-data")])
            .await
            .unwrap();
        assert_eq!(asked(&mut tx, &cluster, &vol).await, 1);
        // A failure shows on the volume.
        finished(
            &mut tx,
            &cluster,
            &json!({ "kind": "delete_volume", "volume_id": vol }),
            false,
            Some("web-data is still in use by web-1."),
        )
        .await
        .unwrap();
        let err: Option<String> =
            sqlx::query_scalar("SELECT data->>'deleteError' FROM pyo.volumes WHERE id = $1")
                .bind(&vol)
                .fetch_one(&mut *tx)
                .await
                .unwrap();
        assert_eq!(err.as_deref(), Some("web-data is still in use by web-1."));
        // Gone from the cluster's list: the row goes, the id stays.
        reported(&mut tx, &cluster, &[report("vol_stray", "left-over")])
            .await
            .unwrap();
        let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM pyo.volumes WHERE id = $1")
            .bind(&vol)
            .fetch_one(&mut *tx)
            .await
            .unwrap();
        assert_eq!(rows, 0);
        let kept: i64 =
            sqlx::query_scalar("SELECT count(*) FROM pyo.deleted_volumes WHERE id = $1")
                .bind(&vol)
                .fetch_one(&mut *tx)
                .await
                .unwrap();
        assert_eq!(kept, 1);
        // The claim with no volume is listed, and can be deleted by its id.
        let strays: Value =
            sqlx::query_scalar("SELECT stray_volumes FROM pyo.clusters WHERE id = $1")
                .bind(&cluster)
                .fetch_one(&mut *tx)
                .await
                .unwrap();
        assert_eq!(strays[0]["name"], "left-over");
        assert!(
            delete(&mut tx, &user, "vol_stray", None, &mut wake)
                .await
                .is_err(),
            "needs its cluster"
        );
        delete(&mut tx, &user, "vol_stray", Some(&cluster), &mut wake)
            .await
            .unwrap();
        let strays: Value =
            sqlx::query_scalar("SELECT stray_volumes FROM pyo.clusters WHERE id = $1")
                .bind(&cluster)
                .fetch_one(&mut *tx)
                .await
                .unwrap();
        assert_eq!(strays[0]["deleting"], true);
        finished(
            &mut tx,
            &cluster,
            &json!({ "kind": "delete_volume", "volume_id": "vol_stray" }),
            true,
            None,
        )
        .await
        .unwrap();
        let strays: Value =
            sqlx::query_scalar("SELECT stray_volumes FROM pyo.clusters WHERE id = $1")
                .bind(&cluster)
                .fetch_one(&mut *tx)
                .await
                .unwrap();
        assert_eq!(strays, json!([]));

        // Deleted with its service: asked only once the service's objects
        // are gone; a failed service deletion keeps it.
        let vol2 = format!("vol_b{run}");
        sqlx::query("INSERT INTO pyo.volumes (id, team_id, cluster_id, project_id, service_id, data, namespace) VALUES ($1, $2, $3, $4, $5, $6, $7)")
            .bind(&vol2)
            .bind(&team)
            .bind(&cluster)
            .bind(&project)
            .bind(&service)
            .bind(json!({ "name": "web-files", "mountPath": "/files", "sizeGi": 1 }))
            .bind(format!("shop-{run}"))
            .execute(&mut *tx)
            .await
            .unwrap();
        assert_eq!(
            delete_with_service(&mut tx, &user, &team, &service)
                .await
                .unwrap(),
            ["web-files"]
        );
        sqlx::query("UPDATE pyo.services SET deleting = true WHERE id = $1")
            .bind(&service)
            .execute(&mut *tx)
            .await
            .unwrap();
        reported(&mut tx, &cluster, &[report(&vol2, "web-files")])
            .await
            .unwrap();
        assert_eq!(
            asked(&mut tx, &cluster, &vol2).await,
            0,
            "the service is still there"
        );
        service_deleted(&mut tx, &cluster, &service, false)
            .await
            .unwrap();
        let deleting: bool = sqlx::query_scalar("SELECT deleting FROM pyo.volumes WHERE id = $1")
            .bind(&vol2)
            .fetch_one(&mut *tx)
            .await
            .unwrap();
        assert!(!deleting);
        delete_with_service(&mut tx, &user, &team, &service)
            .await
            .unwrap();
        service_deleted(&mut tx, &cluster, &service, true)
            .await
            .unwrap();
        assert_eq!(asked(&mut tx, &cluster, &vol2).await, 1);
        sqlx::query("DELETE FROM pyo.services WHERE id = $1")
            .bind(&service)
            .execute(&mut *tx)
            .await
            .unwrap();
        // The project goes, keeping a volume that holds its namespace; the
        // last volume confirmed gone lets the namespace go.
        sqlx::query("DELETE FROM pyo.services WHERE project_id = $1")
            .bind(&project)
            .execute(&mut *tx)
            .await
            .unwrap();
        sqlx::query("DELETE FROM pyo.projects WHERE id = $1")
            .bind(&project)
            .execute(&mut *tx)
            .await
            .unwrap();
        let count_ns = "SELECT count(*) FROM pyo.commands WHERE cluster_id = $1 AND command->>'kind' = 'delete_namespace'";
        finished(
            &mut tx,
            &cluster,
            &json!({ "kind": "delete_volume", "volume_id": vol2 }),
            true,
            None,
        )
        .await
        .unwrap();
        let n: i64 = sqlx::query_scalar(count_ns)
            .bind(&cluster)
            .fetch_one(&mut *tx)
            .await
            .unwrap();
        assert_eq!(n, 1);

        // A project deleted with its services: a volume already being
        // deleted with one goes now (its service's row goes with the
        // project); the others are kept, or all deleted.
        let (project2, service2) = (format!("prj_2{run}"), format!("svc_2{run}"));
        sqlx::query(
            "INSERT INTO pyo.projects (id, team_id, name, namespace) VALUES ($1, $2, 'blog', $3)",
        )
        .bind(&project2)
        .bind(&team)
        .bind(format!("blog-{run}"))
        .execute(&mut *tx)
        .await
        .unwrap();
        sqlx::query("INSERT INTO pyo.services (id, team_id, project_id, cluster_id, name, spec, deleting) VALUES ($1, $2, $3, $4, 'blog', '{}', true)").bind(&service2).bind(&team).bind(&project2).bind(&cluster).execute(&mut *tx).await.unwrap();
        let (going, kept) = (format!("vol_g{run}"), format!("vol_k{run}"));
        for (id, name, deleting) in [(&going, "blog-cache", true), (&kept, "blog-data", false)] {
            sqlx::query("INSERT INTO pyo.volumes (id, team_id, cluster_id, project_id, service_id, data, deleting) VALUES ($1, $2, $3, $4, $5, $6, $7)")
                .bind(id)
                .bind(&team)
                .bind(&cluster)
                .bind(&project2)
                .bind(&service2)
                .bind(json!({ "name": name, "mountPath": "/data", "sizeGi": 1 }))
                .bind(deleting)
                .execute(&mut *tx)
                .await
                .unwrap();
        }
        assert_eq!(
            project_deleted(&mut tx, &project2, &format!("blog-{run}"), false)
                .await
                .unwrap(),
            ["blog-data"]
        );
        assert_eq!(asked(&mut tx, &cluster, &going).await, 1);
        assert_eq!(asked(&mut tx, &cluster, &kept).await, 0);
        assert_eq!(
            project_deleted(&mut tx, &project2, &format!("blog-{run}"), true)
                .await
                .unwrap(),
            ["blog-cache", "blog-data"]
        );
        assert_eq!(asked(&mut tx, &cluster, &kept).await, 1);
        tx.rollback().await.unwrap();
    }

    /// Needs Postgres: DATABASE_URL=… cargo test -p pyo-api -- --ignored volumes::tests
    #[tokio::test]
    #[ignore]
    async fn moves_restores_and_growth_in_the_database() {
        let url = std::env::var("DATABASE_URL").expect("DATABASE_URL");
        let pool = sqlx::PgPool::connect(&url).await.unwrap();
        crate::migrate::run(&pool).await.unwrap();
        crate::vault::init(crate::vault::Vault::new(
            pool.clone(),
            crate::vault::Provider::from_env(),
        ));
        let run = crate::ids::new_id("t");
        let (team, cluster) = (format!("team_{run}"), format!("cl_{run}"));
        let user = User {
            id: format!("usr_{run}"),
            name: "T".into(),
            email: format!("{run}@t.test"),
            timezone: "UTC".into(),
            is_admin: false,
            must_set_up: false,
            key: None,
        };
        let mut tx = pool.begin().await.unwrap();
        sqlx::query(
            "INSERT INTO pyo.users (id, email, name, password_hash) VALUES ($1, $2, 'T', '')",
        )
        .bind(&user.id)
        .bind(&user.email)
        .execute(&mut *tx)
        .await
        .unwrap();
        sqlx::query("INSERT INTO pyo.teams (id, name, slug) VALUES ($1, $1, $1)")
            .bind(&team)
            .execute(&mut *tx)
            .await
            .unwrap();
        sqlx::query("INSERT INTO pyo.members (id, team_id, user_id, email, role, status) VALUES ($1, $2, $3, $4, 'owner', 'active')").bind(format!("mem_{run}")).bind(&team).bind(&user.id).bind(&user.email).execute(&mut *tx).await.unwrap();
        let caps = json!({ "storageClasses": [
            { "name": "local-path", "provisioner": "rancher.io/local-path", "replicated": false },
            { "name": "longhorn", "provisioner": "driver.longhorn.io", "replicated": true, "allowVolumeExpansion": true },
            { "name": "fixed", "provisioner": "example.com/fixed", "replicated": true },
        ] });
        sqlx::query("INSERT INTO pyo.clusters (id, team_id, name, agent_token_hash, capabilities) VALUES ($1, $2, $1, $1, $3)").bind(&cluster).bind(&team).bind(&caps).execute(&mut *tx).await.unwrap();
        let (pa, pb, pc) = (
            format!("prj_a{run}"),
            format!("prj_b{run}"),
            format!("prj_c{run}"),
        );
        for (id, name) in [(&pa, "shop"), (&pb, "blog"), (&pc, "old")] {
            sqlx::query(
                "INSERT INTO pyo.projects (id, team_id, name, namespace) VALUES ($1, $2, $3, $4)",
            )
            .bind(id)
            .bind(&team)
            .bind(name)
            .bind(format!("{name}-{run}"))
            .execute(&mut *tx)
            .await
            .unwrap();
        }
        let (sa, sb) = (format!("svc_a{run}"), format!("svc_b{run}"));
        for (id, project, name) in [(&sa, &pa, "web"), (&sb, &pb, "api")] {
            sqlx::query("INSERT INTO pyo.services (id, team_id, project_id, cluster_id, name, spec) VALUES ($1, $2, $3, $4, $5, '{}')").bind(id).bind(&team).bind(project).bind(&cluster).bind(name).execute(&mut *tx).await.unwrap();
        }
        let add = |id: String, project: String, name: &'static str, class: &'static str| json!({ "id": id, "project": project, "data": { "name": name, "sizeGi": 5, "storageClass": class, "accessMode": "ReadWriteOnce", "status": "bound", "mountPath": "/data" } });
        let (v1, v2, v3) = (
            format!("vol_1{run}"),
            format!("vol_2{run}"),
            format!("vol_3{run}"),
        );
        for v in [
            add(v1.clone(), pa.clone(), "web-data", "local-path"),
            add(v2.clone(), pc.clone(), "old-data", "longhorn"),
            add(v3.clone(), pa.clone(), "api-data", "fixed"),
        ] {
            sqlx::query("INSERT INTO pyo.volumes (id, team_id, cluster_id, project_id, data) VALUES ($1, $2, $3, $4, $5)")
                .bind(v["id"].as_str())
                .bind(&team)
                .bind(&cluster)
                .bind(v["project"].as_str())
                .bind(&v["data"])
                .execute(&mut *tx)
                .await
                .unwrap();
        }
        const COUNT: &str = "SELECT count(*) FROM pyo.commands WHERE cluster_id = $1 AND command->>'kind' = $2 AND ($3 = '' OR command->>'namespace' = $3)";
        let mut wake = vec![];

        // Same project: attached at once.
        attach(&mut tx, &user, &v1, &sa, "/data", &mut wake)
            .await
            .unwrap();
        detach(&mut tx, &user, &v1).await.unwrap();
        // Another project: moved first, then attached.
        let r = attach(&mut tx, &user, &v1, &sb, "/files", &mut wake)
            .await
            .unwrap();
        assert_eq!(r["moving"], true);
        assert_eq!(wake, std::slice::from_ref(&cluster));
        let n: i64 = sqlx::query_scalar(COUNT)
            .bind(&cluster)
            .bind("move_volume")
            .bind("")
            .fetch_one(&mut *tx)
            .await
            .unwrap();
        assert_eq!(n, 1);
        let again = attach(&mut tx, &user, &v1, &sb, "/files", &mut wake)
            .await
            .unwrap_err();
        assert!(again.message.contains("being moved"), "{}", again.message);
        assert!(
            delete(&mut tx, &user, &v1, None, &mut wake)
                .await
                .unwrap_err()
                .message
                .contains("being moved")
        );
        finished(
            &mut tx,
            &cluster,
            &json!({ "kind": "move_volume", "volumeId": v1 }),
            true,
            None,
        )
        .await
        .unwrap();
        let (project, ns, service, data): (String, String, Option<String>, Value) = sqlx::query_as(
            "SELECT project_id, namespace, service_id, data FROM pyo.volumes WHERE id = $1",
        )
        .bind(&v1)
        .fetch_one(&mut *tx)
        .await
        .unwrap();
        assert_eq!(
            (project.as_str(), ns, service.as_deref()),
            (pb.as_str(), format!("blog-{run}"), Some(sb.as_str()))
        );
        assert_eq!(data["mountPath"], "/files");
        assert!(data.get("moving").is_none());
        let pending: bool =
            sqlx::query_scalar("SELECT pending_changes FROM pyo.services WHERE id = $1")
                .bind(&sb)
                .fetch_one(&mut *tx)
                .await
                .unwrap();
        assert!(pending, "deploy to mount it");

        // A deleted project's volume goes to any project; its namespace
        // goes once it's empty.
        sqlx::query("UPDATE pyo.volumes SET namespace = $2 WHERE id = $1")
            .bind(&v2)
            .bind(format!("old-{run}"))
            .execute(&mut *tx)
            .await
            .unwrap();
        sqlx::query("DELETE FROM pyo.projects WHERE id = $1")
            .bind(&pc)
            .execute(&mut *tx)
            .await
            .unwrap();
        // The name is taken in shop already? No: web-data moved out. A clash:
        sqlx::query(
            "UPDATE pyo.volumes SET data = data || '{\"name\": \"api-data\"}' WHERE id = $1",
        )
        .bind(&v2)
        .execute(&mut *tx)
        .await
        .unwrap();
        let clash = attach(&mut tx, &user, &v2, &sa, "/old", &mut wake)
            .await
            .unwrap_err();
        assert!(
            clash
                .message
                .contains("already has a volume named api-data"),
            "{}",
            clash.message
        );
        sqlx::query(
            "UPDATE pyo.volumes SET data = data || '{\"name\": \"old-data\"}' WHERE id = $1",
        )
        .bind(&v2)
        .execute(&mut *tx)
        .await
        .unwrap();
        attach(&mut tx, &user, &v2, &sa, "/old", &mut wake)
            .await
            .unwrap();
        finished(
            &mut tx,
            &cluster,
            &json!({ "kind": "move_volume", "volumeId": v2 }),
            true,
            None,
        )
        .await
        .unwrap();
        let ns_gone: i64 = sqlx::query_scalar(COUNT)
            .bind(&cluster)
            .bind("delete_namespace")
            .bind(format!("old-{run}"))
            .fetch_one(&mut *tx)
            .await
            .unwrap();
        assert_eq!(ns_gone, 1);

        // A failed move leaves it where it was, with why.
        detach(&mut tx, &user, &v2).await.unwrap();
        attach(&mut tx, &user, &v2, &sb, "/old", &mut wake)
            .await
            .unwrap();
        finished(
            &mut tx,
            &cluster,
            &json!({ "kind": "move_volume", "volumeId": v2 }),
            false,
            Some("old-data is still in use by web-1."),
        )
        .await
        .unwrap();
        let (project, service, data): (String, Option<String>, Value) =
            sqlx::query_as("SELECT project_id, service_id, data FROM pyo.volumes WHERE id = $1")
                .bind(&v2)
                .fetch_one(&mut *tx)
                .await
                .unwrap();
        assert_eq!((project.as_str(), service), (pa.as_str(), None));
        assert_eq!(data["moveError"], "old-data is still in use by web-1.");

        // Growing: never on local-path or a class that can't; only up.
        assert!(
            resize(&mut tx, &user, &v1, 10, &mut wake)
                .await
                .unwrap_err()
                .message
                .contains("local-path")
        );
        assert!(
            resize(&mut tx, &user, &v3, 10, &mut wake)
                .await
                .unwrap_err()
                .message
                .contains("can't grow")
        );
        assert!(
            resize(&mut tx, &user, &v2, 5, &mut wake)
                .await
                .unwrap_err()
                .message
                .contains("only grow")
        );
        resize(&mut tx, &user, &v2, 8, &mut wake).await.unwrap();
        let n: i64 = sqlx::query_scalar(COUNT)
            .bind(&cluster)
            .bind("resize_volume")
            .bind("")
            .fetch_one(&mut *tx)
            .await
            .unwrap();
        assert_eq!(n, 1);
        // Not yet: the cluster still reports 5.
        let rep = |cap: i64| VolumeReport {
            capacity_gi: Some(cap),
            size_gi: Some(8),
            ..report(&v2, "old-data")
        };
        reported(&mut tx, &cluster, &[rep(5)]).await.unwrap();
        let data: Value = sqlx::query_scalar("SELECT data FROM pyo.volumes WHERE id = $1")
            .bind(&v2)
            .fetch_one(&mut *tx)
            .await
            .unwrap();
        assert_eq!(
            (data["sizeGi"].as_i64(), data["resizeTo"].as_i64()),
            (Some(5), Some(8))
        );
        reported(&mut tx, &cluster, &[rep(8)]).await.unwrap();
        let data: Value = sqlx::query_scalar("SELECT data FROM pyo.volumes WHERE id = $1")
            .bind(&v2)
            .fetch_one(&mut *tx)
            .await
            .unwrap();
        assert_eq!(
            (data["sizeGi"].as_i64(), data.get("resizeTo")),
            (Some(8), None)
        );
        // A failed one says why.
        resize(&mut tx, &user, &v2, 9, &mut wake).await.unwrap();
        finished(
            &mut tx,
            &cluster,
            &json!({ "kind": "resize_volume", "volume_id": v2 }),
            false,
            Some("quota"),
        )
        .await
        .unwrap();
        let data: Value = sqlx::query_scalar("SELECT data FROM pyo.volumes WHERE id = $1")
            .bind(&v2)
            .fetch_one(&mut *tx)
            .await
            .unwrap();
        assert_eq!(
            (data["resizeError"].as_str(), data.get("resizeTo")),
            (Some("quota"), None)
        );

        // Restoring: its service can't deploy meanwhile; the answer settles it.
        sqlx::query("UPDATE pyo.volumes SET data = data || '{\"restore\": {\"id\": \"rst_1\", \"snapshotId\": \"abcdef12\"}}' WHERE id = $1").bind(&v1).execute(&mut *tx).await.unwrap();
        assert!(
            not_restoring(&mut tx, &sb)
                .await
                .unwrap_err()
                .message
                .contains("being restored")
        );
        assert!(
            detach(&mut tx, &user, &v1)
                .await
                .unwrap_err()
                .message
                .contains("being restored")
        );
        // An older restore's late answer changes nothing.
        finished(
            &mut tx,
            &cluster,
            &json!({ "kind": "restore_volume", "volumeId": v1, "restoreId": "rst_0" }),
            true,
            None,
        )
        .await
        .unwrap();
        assert!(not_restoring(&mut tx, &sb).await.is_err());
        finished(
            &mut tx,
            &cluster,
            &json!({ "kind": "restore_volume", "volumeId": v1, "restoreId": "rst_1" }),
            false,
            Some("That snapshot isn't one of this volume's."),
        )
        .await
        .unwrap();
        not_restoring(&mut tx, &sb).await.unwrap();
        let data: Value = sqlx::query_scalar("SELECT data FROM pyo.volumes WHERE id = $1")
            .bind(&v1)
            .fetch_one(&mut *tx)
            .await
            .unwrap();
        assert_eq!(
            data["restoreError"],
            "That snapshot isn't one of this volume's."
        );
        let kinds: Vec<String> = sqlx::query_scalar(
            "SELECT data->>'type' FROM pyo.activity WHERE team_id = $1 ORDER BY at",
        )
        .bind(&team)
        .fetch_all(&mut *tx)
        .await
        .unwrap();
        for k in [
            "volume.moving",
            "volume.attached",
            "volume.move_failed",
            "volume.resized",
            "volume.resize_failed",
            "volume.restore_failed",
        ] {
            assert!(kinds.iter().any(|x| x == k), "{k} in {kinds:?}");
        }
        tx.rollback().await.unwrap();
    }

    #[test]
    fn what_can_grow() {
        let caps = json!({ "storageClasses": [
            { "name": "local-path", "provisioner": "rancher.io/local-path", "allowVolumeExpansion": true },
            { "name": "longhorn", "provisioner": "driver.longhorn.io", "allowVolumeExpansion": true },
            { "name": "nfs", "provisioner": "nfs" },
        ] });
        assert!(
            growth_refusal("local-path", &caps)
                .unwrap()
                .contains("free disk"),
            "no quota, whatever it says"
        );
        assert!(growth_refusal("longhorn", &caps).is_none());
        assert!(growth_refusal("nfs", &caps).unwrap().contains("can't grow"));
        assert!(growth_refusal("gone", &caps).is_some());
        assert!(
            snapshot_id_ok("0123abcd") && !snapshot_id_ok("latest") && !snapshot_id_ok("0123abc")
        );
        assert!(
            busy("v", false, &json!({ "restore": { "id": "r" } }))
                .unwrap()
                .contains("restored")
        );
        assert!(busy("v", false, &json!({ "restore": null })).is_none());
    }
}
