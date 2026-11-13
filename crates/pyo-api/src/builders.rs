//! Builders: where Git sources become images.
//!
//! A builder is a BuildKit daemon plus `pyo-builder`, as a pod pinned to one
//! server of a cluster (the agent runs it), on that cluster's server or a
//! build-only server that joined it. It dials out here with its own token: heartbeats, a long-poll for build jobs, build logs and the
//! result. See deploy.rs for what happens to a finished build.

use std::time::Duration;

use axum::{
    Json, Router,
    extract::{FromRequestParts, Path, Query, State},
    http::{StatusCode, request::Parts},
    routing::{get, post},
};
use pyo_proto::{BuildJob, BuildProgress, BuildResult, BuilderHeartbeat, DeploymentProgress};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{
    AppState, deploy,
    error::{ApiError, ApiResult},
    ids::{secret_token, sha256_hex},
};

/// Bump when the cluster builder's pod spec changes (the agent builds it from
/// EnsureBuilder); builders on an older one get it re-sent. 2: unprivileged,
/// read-only pyo-builder. 3: BuildKit started on demand.
pub const CLUSTER_BUILDER_SPEC: i32 = 3;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/heartbeat", post(heartbeat))
        .route("/jobs", get(poll))
        .route("/jobs/{id}/progress", post(progress))
        .route("/jobs/{id}/result", post(result))
}

/// The builder a bearer token belongs to.
pub struct BuilderAuth {
    pub id: String,
    pub team_id: String,
}

impl FromRequestParts<AppState> for BuilderAuth {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let token = parts
            .headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .ok_or_else(|| ApiError::new(StatusCode::UNAUTHORIZED, "Missing builder token."))?;
        let row: Option<(String, String)> = sqlx::query_as(
            "SELECT id, team_id FROM pyo.builders WHERE token_hash = $1 AND status <> 'removing'",
        )
        .bind(sha256_hex(token))
        .fetch_optional(&state.db)
        .await?;
        let (id, team_id) = row.ok_or_else(|| {
            ApiError::new(
                StatusCode::UNAUTHORIZED,
                "Unknown builder token. Was the builder removed?",
            )
        })?;
        Ok(BuilderAuth { id, team_id })
    }
}

async fn heartbeat(
    State(s): State<AppState>,
    b: BuilderAuth,
    Json(hb): Json<BuilderHeartbeat>,
) -> ApiResult<Json<Value>> {
    let (status, message) = if hb.buildkit {
        ("ready", None)
    } else {
        ("error", Some("BuildKit isn't answering on this builder."))
    };
    sqlx::query(
        "UPDATE pyo.builders SET status = $2, status_message = $3, last_seen_at = now(),
                info = (info - 'nodeKey') || $4 || CASE WHEN kind = 'server' AND $5::text IS NOT NULL THEN jsonb_build_object('hostname', $5::text) ELSE '{}' END
          WHERE id = $1 AND status <> 'removing'",
    )
    .bind(&b.id)
    .bind(status)
    .bind(message)
    .bind(json!({ "version": hb.version, "cpus": hb.cpus, "memoryGi": hb.memory_gi, "busyWith": hb.busy_with }))
    .bind(&hb.hostname)
    .execute(&s.db)
    .await?;
    Ok(Json(json!({ "ok": true })))
}

#[derive(Deserialize)]
struct PollQuery {
    wait: Option<u64>,
}

/// Hand the oldest queued build of the team to this builder, waiting up to
/// `wait` seconds for one.
async fn poll(
    State(s): State<AppState>,
    b: BuilderAuth,
    Query(q): Query<PollQuery>,
) -> ApiResult<Json<Option<BuildJob>>> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(q.wait.unwrap_or(25).min(55));
    let waker = s.waker(&format!("builders:{}", b.team_id)).await;
    loop {
        let notified = waker.notified();
        // Claim the job, and note on its deployment which builder built it.
        let row: Option<(Value,)> = sqlx::query_as(
            "WITH j AS (
               UPDATE pyo.build_jobs SET status = 'running', builder_id = $2, started_at = now()
                WHERE id = (SELECT id FROM pyo.build_jobs WHERE team_id = $1 AND status = 'queued' ORDER BY created_at LIMIT 1 FOR UPDATE SKIP LOCKED)
                RETURNING job, deployment_id
             ), d AS (
               UPDATE pyo.deployments SET data = data || jsonb_build_object('builderId', $2::text) WHERE id = (SELECT deployment_id FROM j)
             )
             SELECT job FROM j",
        )
        .bind(&b.team_id)
        .bind(&b.id)
        .fetch_optional(&s.db)
        .await?;
        if let Some((job,)) = row {
            let mut job: BuildJob = serde_json::from_value(job).map_err(anyhow::Error::from)?;
            // Pushed through this builder's way to the registry (its door).
            let cluster: Option<String> =
                sqlx::query_scalar("SELECT cluster_id FROM pyo.builders WHERE id = $1")
                    .bind(&b.id)
                    .fetch_one(&s.db)
                    .await?;
            let host = match cluster {
                Some(c) => registry_for(&s.db, &c).await?,
                None => String::new(),
            };
            if let Some(rest) = job
                .push_repository
                .strip_prefix(&format!("{}/", job.registry_host))
            {
                job.push_repository = format!("{host}/{rest}");
            }
            job.registry_host = host;
            if let Some(inst) = job.github_installation_id {
                // Private repositories: clone with a short-lived token, and
                // resolve the branch through the API (git ls-remote can't).
                match crate::github::installation_token(&s, inst).await {
                    Ok(tok) => job.clone_token = Some(tok),
                    Err(e) => {
                        tracing::warn!(job = %job.id, error = %format!("{e:#}"), "no GitHub token for build")
                    }
                }
                if job.commit.is_empty()
                    && let Some(repo) = crate::github::full_name(&job.repo_url)
                    && let Ok(sha) = crate::github::branch_head(&s, inst, &repo, &job.branch).await
                {
                    job.commit = sha;
                }
                crate::jobs::enqueue_now(
                    &s,
                    "github.status",
                    crate::jobs::github_status(&job.deployment_id, "pending", "Building…"),
                )
                .await;
            }
            return Ok(Json(Some(job)));
        }
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        if left.is_zero() || s.stopping() {
            return Ok(Json(None));
        }
        let _ = tokio::time::timeout(left.min(Duration::from_secs(5)), notified).await;
    }
}

/// The deployment and cluster of a job this builder is running.
async fn running_job(s: &AppState, b: &BuilderAuth, job_id: &str) -> ApiResult<(String, String)> {
    sqlx::query_as(
        "SELECT j.deployment_id, d.cluster_id FROM pyo.build_jobs j JOIN pyo.deployments d ON d.id = j.deployment_id
          WHERE j.id = $1 AND j.builder_id = $2 AND j.status = 'running'",
    )
    .bind(job_id)
    .bind(&b.id)
    .fetch_optional(&s.db)
    .await?
    .ok_or_else(|| ApiError::new(StatusCode::GONE, "This build isn't running any more (cancelled?)."))
}

async fn progress(
    State(s): State<AppState>,
    b: BuilderAuth,
    Path(id): Path<String>,
    Json(p): Json<BuildProgress>,
) -> ApiResult<Json<Value>> {
    let (deployment_id, cluster_id) = running_job(&s, &b, &id).await?;
    if let Some(commit) = &p.commit {
        sqlx::query(
            "UPDATE pyo.deployments SET data = data || jsonb_build_object('revision', $2::text, 'revisionLabel', left($2::text, 7)) WHERE id = $1",
        )
        .bind(&deployment_id)
        .bind(commit)
        .execute(&s.db)
        .await?;
    }
    let dp = DeploymentProgress {
        status: p.stage.clone(),
        logs: p.logs,
        ..Default::default()
    };
    deploy::apply_progress(&s, &cluster_id, &deployment_id, dp).await?;
    Ok(Json(json!({ "ok": true })))
}

async fn result(
    State(s): State<AppState>,
    b: BuilderAuth,
    Path(id): Path<String>,
    Json(r): Json<BuildResult>,
) -> ApiResult<Json<Value>> {
    if let Some(cluster) = deploy::finish_build(&s, &id, &b.id, r).await? {
        s.wake(&cluster).await;
    }
    Ok(Json(json!({ "ok": true })))
}

/// Adds a builder on a server of one of the team's clusters (kind
/// "cluster") and asks its agent to start it.
pub async fn add_on_server(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    config: &crate::Config,
    id: &str,
    team_id: &str,
    name: &str,
    cluster_id: &str,
    node: &str,
) -> ApiResult<()> {
    let token = secret_token();
    sqlx::query("INSERT INTO pyo.builders (id, team_id, name, kind, cluster_id, server, token_hash, info) VALUES ($1, $2, $3, 'cluster', $4, $5, $6, jsonb_build_object('specVersion', $7::int, 'image', $8::text))")
        .bind(id)
        .bind(team_id)
        .bind(name)
        .bind(cluster_id)
        .bind(node)
        .bind(sha256_hex(&token))
        .bind(CLUSTER_BUILDER_SPEC)
        .bind(&config.builder_image)
        .execute(&mut **tx)
        .await?;
    let spec = pyo_proto::BuilderSpec {
        builder_id: id.to_owned(),
        server: node.to_owned(),
        api_url: config.agent_api_url.clone(),
        token,
        image: config.builder_image.clone(),
        registry_host: registry_for(&mut **tx, cluster_id).await?,
    };
    deploy::enqueue(
        tx,
        cluster_id,
        pyo_proto::Command::EnsureBuilder(Box::new(spec)),
    )
    .await?;
    Ok(())
}

/// Where a builder on this cluster pushes (deploy.rs `builder_registry`):
/// the registry door, logged in with the builder's own token. Empty when it
/// can't reach Pyo's registry (another cluster, without a tailnet).
async fn registry_for<'e>(db: impl sqlx::PgExecutor<'e>, cluster_id: &str) -> sqlx::Result<String> {
    let ours: bool = sqlx::query_scalar(
        "SELECT coalesce((SELECT same_cluster FROM pyo.clusters WHERE id = $1), false)",
    )
    .bind(cluster_id)
    .fetch_one(db)
    .await?;
    Ok(deploy::builder_registry(ours).unwrap_or_default())
}

/// The server a new team's first builder runs on: the control plane of the
/// server Pyo runs on (ready first), else any.
fn first_builder_server(nodes: &[pyo_proto::ServerReport]) -> Option<&str> {
    let cp = |n: &&pyo_proto::ServerReport| n.roles.iter().any(|r| r == "control-plane");
    nodes
        .iter()
        .find(|n| cp(n) && n.status == "ready")
        .or_else(|| nodes.iter().find(|n| n.status == "ready"))
        .or_else(|| nodes.iter().find(cp))
        .or(nodes.first())
        .map(|n| n.name.as_str())
}

/// The first time the server Pyo runs on connects, a team without builders
/// gets one there, named after the server: Git deploys work from the start.
/// Only then, so a builder removed later stays removed. Returns its name.
pub async fn first_on_this_server(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    config: &crate::Config,
    cluster_id: &str,
    team_id: &str,
    nodes: &[pyo_proto::ServerReport],
) -> ApiResult<Option<String>> {
    let Some(node) = first_builder_server(nodes) else {
        return Ok(None);
    };
    let have: bool =
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM pyo.builders WHERE team_id = $1)")
            .bind(team_id)
            .fetch_one(&mut **tx)
            .await?;
    if have {
        return Ok(None);
    }
    let name = crate::agent::host_label(node).unwrap_or_else(|| "local".into());
    let id = crate::ids::new_id("bld");
    add_on_server(tx, config, &id, team_id, &name, cluster_id, node).await?;
    crate::ops::log_activity(
        tx,
        team_id,
        "Pyo",
        "team.updated",
        "info",
        &format!("Added builder {name}"),
        json!({ "kind": "team", "id": team_id, "label": name }),
        json!({}),
    )
    .await?;
    Ok(Some(name))
}

/// Moves builders along: re-sends outdated builder specs, marks quiet
/// builders offline and fails builds nobody finished.
pub async fn watcher(s: AppState) {
    let mut tick = tokio::time::interval(Duration::from_secs(10));
    loop {
        tick.tick().await;
        if let Some(Err(e)) =
            crate::bg::as_leader(&s.db, crate::bg::LOCK_BUILDERS, || watch_once(&s)).await
        {
            tracing::warn!(error = %format!("{e:#}"), "builder watcher");
        }
    }
}

/// Re-send the pod spec to cluster builders made with an older one, or with
/// another builder image (Pyo was updated). Only the token's hash is stored,
/// so each gets a fresh token with it.
async fn refresh_cluster_builders(s: &AppState) -> anyhow::Result<()> {
    // Also builders on a new server, once it has joined its cluster and
    // is ready (they start at spec version 0). A builder in the middle of a
    // build moves to a new image after it.
    let outdated: Vec<(String, String, String)> = sqlx::query_as(
        "SELECT b.id, b.cluster_id, b.server FROM pyo.builders b
          WHERE b.kind IN ('cluster', 'server') AND b.status <> 'removing' AND b.cluster_id IS NOT NULL AND b.server IS NOT NULL
            AND (coalesce((b.info->>'specVersion')::int, 1) < $1
                 OR (b.info->>'image' IS DISTINCT FROM $2 AND b.info->>'busyWith' IS NULL AND b.status = 'ready'))
            AND (b.kind = 'cluster' OR EXISTS (
              SELECT 1 FROM pyo.servers n WHERE n.cluster_id = b.cluster_id AND n.name = b.server AND n.report->>'status' = 'ready'))",
    )
    .bind(CLUSTER_BUILDER_SPEC)
    .bind(&s.config.builder_image)
    .fetch_all(&s.db)
    .await?;
    for (id, cluster_id, node) in outdated {
        let token = secret_token();
        let mut tx = s.db.begin().await?;
        sqlx::query(
            "UPDATE pyo.builders SET token_hash = $2, info = info || jsonb_build_object('specVersion', $3::int, 'image', $4::text),
                    status_message = CASE WHEN kind = 'server' THEN 'Starting the builder.' ELSE status_message END WHERE id = $1",
        )
            .bind(&id)
            .bind(sha256_hex(&token))
            .bind(CLUSTER_BUILDER_SPEC)
            .bind(&s.config.builder_image)
            .execute(&mut *tx)
            .await?;
        let spec = pyo_proto::BuilderSpec {
            builder_id: id.clone(),
            server: node,
            api_url: s.config.agent_api_url.clone(),
            token,
            image: s.config.builder_image.clone(),
            registry_host: registry_for(&mut *tx, &cluster_id).await?,
        };
        deploy::enqueue(
            &mut tx,
            &cluster_id,
            pyo_proto::Command::EnsureBuilder(Box::new(spec)),
        )
        .await
        .map_err(|e| anyhow::anyhow!("{}", e.message))?;
        tx.commit().await?;
        s.wake(&cluster_id).await;
        tracing::info!(builder = %id, "re-sent builder spec");
    }
    Ok(())
}

async fn watch_once(s: &AppState) -> anyhow::Result<()> {
    refresh_cluster_builders(s).await?;
    sqlx::query("UPDATE pyo.builders SET status = 'offline' WHERE status = 'ready' AND last_seen_at < now() - interval '60 seconds'").execute(&s.db).await?;
    // Builds nobody finished (builder gone mid-build) fail after an hour.
    let stale: Vec<(String, String)> = sqlx::query_as(
        "UPDATE pyo.build_jobs SET status = 'failed', finished_at = now() WHERE status = 'running' AND started_at < now() - interval '60 minutes'
          RETURNING deployment_id, (SELECT cluster_id FROM pyo.deployments d WHERE d.id = deployment_id)",
    )
    .fetch_all(&s.db)
    .await?;
    for (dep, cluster) in stale {
        let p = DeploymentProgress {
            status: "failed".into(),
            failure: Some(pyo_proto::Failure {
                stage: "building".into(),
                reason: "BuilderLost".into(),
                message: "The builder stopped reporting on this build.".into(),
            }),
            ..Default::default()
        };
        let _ = deploy::apply_progress(s, &cluster, &dep, p).await;
    }

    Ok(())
}

#[cfg(test)]
pub(crate) mod first_builder_tests {
    use super::*;

    pub(crate) fn node(name: &str, roles: &[&str], status: &str) -> pyo_proto::ServerReport {
        serde_json::from_value(json!({
            "name": name, "roles": roles, "status": status, "schedulable": true, "maintenance": false,
            "kubeletVersion": "", "osImage": "", "arch": "amd64", "containerRuntime": "", "internalIP": "10.0.0.1", "externalIP": null,
            "capacity": pyo_proto::ServerCapacity::default(), "usage": pyo_proto::ServerUsage::default(), "podCount": 0, "conditions": [], "createdAt": 0,
        }))
        .unwrap()
    }

    #[test]
    fn the_first_builder_goes_on_the_control_plane() {
        let nodes = [
            node("w1", &["worker"], "ready"),
            node("server-1", &["control-plane", "worker"], "ready"),
        ];
        assert_eq!(first_builder_server(&nodes), Some("server-1"));
        // Not ready yet: a ready server first.
        let nodes = [
            node("w1", &["worker"], "ready"),
            node("server-1", &["control-plane"], "not_ready"),
        ];
        assert_eq!(first_builder_server(&nodes), Some("w1"));
        assert_eq!(
            first_builder_server(&[node("server-1", &["control-plane"], "unknown")]),
            Some("server-1")
        );
        assert_eq!(first_builder_server(&[]), None);
    }

    fn config() -> crate::Config {
        crate::Config {
            agent_api_url: "http://pyo.pyo-system:8000".into(),
            agent_image: String::new(),
            waf_image: String::new(),
            geoip_image: String::new(),
            cookie_secure: false,
            allow_signup: false,
            builder_image: "builder@sha256:00".into(),
            tailnet_api_url: String::new(),
            relay_addr: None,
        }
    }

    /// Against a real Postgres (DATABASE_URL, a throwaway one): a team
    /// without builders gets one on its server, named after it; a team
    /// that has one gets none.
    #[tokio::test]
    #[ignore = "needs DATABASE_URL (a throwaway Postgres)"]
    async fn a_new_team_gets_its_first_builder_on_this_server() {
        let pool = sqlx::PgPool::connect(&std::env::var("DATABASE_URL").unwrap())
            .await
            .unwrap();
        crate::migrate::run(&pool).await.unwrap();
        crate::vault::init(crate::vault::Vault::new(
            pool.clone(),
            crate::vault::Provider::from_env(),
        ));
        let run = crate::ids::new_id("t");
        let (team, cluster) = (format!("team_{run}"), format!("cl_{run}"));
        sqlx::query("INSERT INTO pyo.teams (id, name, slug) VALUES ($1, $1, $1)")
            .bind(&team)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO pyo.clusters (id, team_id, name, agent_token_hash) VALUES ($1, $2, 'local', $1)").bind(&cluster).bind(&team).execute(&pool).await.unwrap();
        let nodes = [node("Server-1.lan", &["control-plane", "worker"], "ready")];

        for _ in 0..2 {
            let mut tx = pool.begin().await.unwrap();
            first_on_this_server(&mut tx, &config(), &cluster, &team, &nodes)
                .await
                .unwrap();
            tx.commit().await.unwrap();
        }
        let rows: Vec<(String, String, String, Option<String>)> = sqlx::query_as(
            "SELECT name, kind, status, server FROM pyo.builders WHERE team_id = $1",
        )
        .bind(&team)
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(
            rows,
            vec![(
                "server-1".into(),
                "cluster".into(),
                "provisioning".into(),
                Some("Server-1.lan".into())
            )]
        );
        let sent: i64 = sqlx::query_scalar("SELECT count(*) FROM pyo.commands WHERE cluster_id = $1 AND command->>'kind' = 'ensure_builder'").bind(&cluster).fetch_one(&pool).await.unwrap();
        assert_eq!(sent, 1);

        sqlx::query("DELETE FROM pyo.teams WHERE id = $1")
            .bind(&team)
            .execute(&pool)
            .await
            .unwrap();
    }
}
