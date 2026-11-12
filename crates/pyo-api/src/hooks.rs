//! Deploy hooks: `POST /api/hooks/deploy/{token}` deploys one service, or every
//! service of a project, with its current settings (Git services build their branch
//! head). For CI pipelines, a CMS publishing content, a cron job. A project's
//! hook skips stopped services.
//!
//! The token is the only credential, so it's long and random; the dashboard
//! reveals it only to developers of the team.

use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    routing::post,
};
use serde_json::{Value, json};

use crate::{
    AppState,
    deploy::{self, DeployRequest},
    error::{ApiError, ApiResult},
    ids::{new_id, sha256_hex},
    ops::log_activity,
};

pub fn routes() -> Router<AppState> {
    Router::new().route("/deploy/{token}", post(trigger))
}

async fn trigger(
    State(s): State<AppState>,
    Path(token): Path<String>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let hook: Option<(String, String, String, Option<String>, String)> =
        sqlx::query_as("SELECT id, team_id, project_id, service_id, name FROM pyo.deploy_hooks WHERE token_hash = $1").bind(sha256_hex(&token)).fetch_optional(&s.db).await?;
    let Some((hook_id, team_id, project_id, service_id, name)) = hook else {
        return Err(ApiError::not_found("Deploy hook"));
    };
    let services: Vec<(String, String, String)> = match &service_id {
        Some(a) => sqlx::query_as("SELECT id, name, status FROM pyo.services WHERE id = $1 AND NOT deleting").bind(a).fetch_all(&s.db).await?,
        None => sqlx::query_as("SELECT id, name, status FROM pyo.services WHERE project_id = $1 AND NOT deleting ORDER BY created_at").bind(&project_id).fetch_all(&s.db).await?,
    };
    let actor = format!("Deploy hook \"{name}\"");
    let (mut started, mut skipped) = (vec![], vec![]);
    for (id, service_name, status) in services {
        // A project's hook doesn't start services someone stopped; a service's own hook does.
        if service_id.is_none() && status == "stopped" {
            skipped.push(
                json!({ "serviceId": id, "service": service_name, "reason": "It's stopped." }),
            );
            continue;
        }
        // One transaction per service: a busy service doesn't stop the others.
        let mut tx = s.db.begin().await?;
        let dep = new_id("dpl");
        match deploy::create(
            &mut tx,
            DeployRequest {
                deployment_id: &dep,
                service_id: &id,
                trigger: "deploy_hook",
                actor: &actor,
                rollback_of: None,
                retry_of: None,
                commit: None,
            },
        )
        .await
        {
            Ok(cluster) => {
                tx.commit().await?;
                s.wake(&cluster).await;
                started
                    .push(json!({ "serviceId": id, "service": service_name, "deploymentId": dep }));
            }
            Err(e) => skipped
                .push(json!({ "serviceId": id, "service": service_name, "reason": e.message })),
        }
    }
    s.wake(&format!("builders:{team_id}")).await;
    let mut tx = s.db.begin().await?;
    sqlx::query("UPDATE pyo.deploy_hooks SET last_used_at = now() WHERE id = $1")
        .bind(&hook_id)
        .execute(&mut *tx)
        .await?;
    let what = if service_id.is_some() {
        "service"
    } else {
        "project"
    };
    log_activity(
        &mut tx,
        &team_id,
        &actor,
        "deployment.started",
        "info",
        &format!(
            "Deploy hook \"{name}\" started {} of the {what}'s deploys",
            started.len()
        ),
        json!({ "kind": "project", "id": project_id, "label": name }),
        json!({ "projectId": project_id, "serviceId": service_id }),
    )
    .await?;
    tx.commit().await?;
    let status = if started.is_empty() && !skipped.is_empty() {
        StatusCode::CONFLICT
    } else {
        StatusCode::ACCEPTED
    };
    Ok((
        status,
        Json(json!({ "ok": !started.is_empty(), "deployments": started, "skipped": skipped })),
    ))
}
