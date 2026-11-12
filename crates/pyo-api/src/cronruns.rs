//! Cron job run history: what the clusters report, kept past the few runs
//! Kubernetes holds on to. A run that fails says so in the activity feed,
//! once, which is what notifications follow.

use axum::{
    Json,
    extract::{Query, State},
};
use pyo_proto::CronRunReport;
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::{Postgres, Transaction};

use crate::{AppState, auth::SessionUser, error::ApiResult};

/// Runs kept per cron job.
const KEEP: i64 = 50;

fn status(r: &CronRunReport) -> &'static str {
    match r.succeeded {
        Some(true) => "succeeded",
        Some(false) => "failed",
        None => "running",
    }
}

/// Whether a run the table doesn't have was already pruned: it's no newer
/// than the oldest of the full set kept (None: the set isn't full).
fn pruned(started_at: i64, oldest_kept: Option<f64>) -> bool {
    oldest_kept.is_some_and(|o| (started_at as f64) <= o)
}

/// Store what a heartbeat says about one service's runs.
pub async fn record(
    tx: &mut Transaction<'_, Postgres>,
    cluster_id: &str,
    label: &str,
    runs: &[CronRunReport],
) -> ApiResult<()> {
    if runs.is_empty() {
        return Ok(());
    }
    let service: Option<(String, String, String, Value)> =
        sqlx::query_as("SELECT id, team_id, name, spec FROM pyo.services WHERE id = $1 AND cluster_id = $2 AND NOT deleting").bind(label).bind(cluster_id).fetch_optional(&mut **tx).await?;
    let Some((service_id, team_id, service_name, spec)) = service else {
        return Ok(());
    };
    for r in runs {
        let cron_id = r.cron_id.clone().unwrap_or_default();
        let before: Option<String> = sqlx::query_scalar(
            "SELECT status FROM pyo.cron_runs WHERE service_id = $1 AND job = $2",
        )
        .bind(&service_id)
        .bind(&r.job)
        .fetch_optional(&mut **tx)
        .await?;
        // A run older than the kept ones was pruned: the cluster still keeps
        // it (its last few failed Jobs), but it's not new, so it's not stored
        // again, or said again in the activity feed on every heartbeat.
        if before.is_none() {
            let oldest_kept: Option<f64> = sqlx::query_scalar(
                "SELECT (extract(epoch FROM min(started_at)) * 1000)::float8 FROM (SELECT started_at FROM pyo.cron_runs WHERE service_id = $1 AND cron_id = $2 ORDER BY started_at DESC LIMIT $3) k
                  WHERE (SELECT count(*) FROM pyo.cron_runs WHERE service_id = $1 AND cron_id = $2) >= $3",
            )
            .bind(&service_id)
            .bind(&cron_id)
            .bind(KEEP)
            .fetch_one(&mut **tx)
            .await?;
            if pruned(r.started_at, oldest_kept) {
                continue;
            }
        }
        sqlx::query(
            "INSERT INTO pyo.cron_runs (service_id, job, cron_id, started_at, finished_at, status, exit_code, reason, output)
             VALUES ($1, $2, $3, to_timestamp($4::float8 / 1000), to_timestamp($5::float8 / 1000), $6, $7, $8, $9)
             ON CONFLICT (service_id, job) DO UPDATE SET finished_at = excluded.finished_at, status = excluded.status,
               exit_code = excluded.exit_code, reason = excluded.reason, output = coalesce(excluded.output, cron_runs.output)",
        )
        .bind(&service_id)
        .bind(&r.job)
        .bind(&cron_id)
        .bind(r.started_at as f64)
        .bind(r.finished_at.map(|t| t as f64))
        .bind(status(r))
        .bind(r.exit_code)
        .bind(&r.reason)
        .bind(&r.output)
        .execute(&mut **tx)
        .await?;
        if status(r) == "failed" && before.as_deref() != Some("failed") {
            let job_name = spec["cronJobs"]
                .as_array()
                .and_then(|jobs| jobs.iter().find(|j| j["id"] == json!(cron_id)))
                .and_then(|j| j["name"].as_str())
                .map(str::to_owned)
                .unwrap_or_else(|| service_name.clone());
            let why = match (r.exit_code, r.reason.as_deref()) {
                (_, Some(reason)) => format!(" ({reason})"),
                (Some(code), None) => format!(" (exit code {code})"),
                _ => String::new(),
            };
            crate::ops::log_activity(
                tx,
                &team_id,
                "Pyo",
                "cron.failed",
                "error",
                &format!("Cron job {job_name} failed{why}"),
                json!({ "kind": "service", "id": service_id, "label": service_name }),
                json!({ "serviceId": service_id, "cronId": cron_id }),
            )
            .await?;
        }
        if before.is_none() {
            sqlx::query(
                "DELETE FROM pyo.cron_runs WHERE service_id = $1 AND cron_id = $2 AND job NOT IN
                   (SELECT job FROM pyo.cron_runs WHERE service_id = $1 AND cron_id = $2 ORDER BY started_at DESC LIMIT $3)",
            )
            .bind(&service_id)
            .bind(&cron_id)
            .bind(KEEP)
            .execute(&mut **tx)
            .await?;
        }
    }
    Ok(())
}

#[derive(Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub struct RunsQuery {
    /// The service's id (`svc_…`).
    service: String,
    /// The cron job's id. Empty for a service that is itself a cron job.
    #[serde(default)]
    cron: String,
}

/// Cron job runs
///
/// A cron job's recent runs, newest first: when each started and finished,
/// how it ended, and the end of what it printed.
#[utoipa::path(get, path = "/cron-runs", operation_id = "listCronRuns", tag = "Cron jobs",
    params(RunsQuery),
    responses(
        (status = 200, description = "`runs`: each with `job`, `startedAt`, `finishedAt`, `status` (running, succeeded or failed), `exitCode`, `reason` and `output`.", body = Object),
        (status = 400, description = "No `service`.", body = crate::error::ErrorBody),
        (status = 404, description = "No such service, or it isn't yours to see.", body = crate::error::ErrorBody),
    ),
    extensions(("x-pyo-access" = json!("read"))))]
pub async fn list(
    State(s): State<AppState>,
    SessionUser(user): SessionUser,
    Query(q): Query<RunsQuery>,
) -> ApiResult<Json<Value>> {
    let mut tx = s.db.begin().await?;
    let team_id = crate::ops::team_of(&mut tx, "services", &q.service).await?;
    crate::ops::require_role(&mut tx, &user, &team_id, crate::ops::Role::Viewer).await?;
    let runs: Vec<Value> = sqlx::query_scalar(
        "SELECT jsonb_build_object('job', job, 'startedAt', pyo.ms(started_at), 'finishedAt', pyo.ms(finished_at), 'status', status,
                                  'exitCode', exit_code, 'reason', reason, 'output', output)
           FROM pyo.cron_runs WHERE service_id = $1 AND cron_id = $2 ORDER BY started_at DESC LIMIT $3",
    )
    .bind(&q.service)
    .bind(&q.cron)
    .bind(KEEP)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(Json(json!({ "runs": runs })))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The cluster keeps its last few failed Jobs after Pyo pruned them:
    /// they're not new runs, so no new failure each heartbeat.
    #[test]
    fn pruned_runs_stay_pruned() {
        let oldest = Some(1_794_149_100_000.0);
        assert!(pruned(1_794_100_000_000, oldest), "older than the kept set");
        assert!(pruned(1_794_149_100_000, oldest));
        assert!(!pruned(1_794_189_600_000, oldest), "a new run");
        assert!(
            !pruned(1_794_100_000_000, None),
            "not full yet: everything's kept"
        );
    }
}
