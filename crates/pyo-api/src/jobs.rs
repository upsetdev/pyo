//! Durable background jobs in Postgres: things that must happen eventually
//! even if a process dies mid-way (reporting a commit status to GitHub or GitLab,
//! deleting an old image, retrying an email, sending an alert). Enqueued in the same
//! transaction as what caused them where possible; claimed with
//! `FOR UPDATE SKIP LOCKED` by any worker; retried with backoff; a job whose
//! worker vanished is picked up again after a few minutes.

use std::time::Duration;

use serde_json::{Value, json};
use sqlx::{PgExecutor, postgres::PgListener};

use crate::AppState;

const MAX_ATTEMPTS: i32 = 8;

/// Queue a job; workers are woken when the surrounding transaction commits.
pub async fn enqueue<'e>(db: impl PgExecutor<'e>, kind: &str, payload: Value) -> sqlx::Result<()> {
    sqlx::query("WITH j AS (INSERT INTO pyo.jobs (kind, payload) VALUES ($1, $2) RETURNING id) SELECT pg_notify('pyo_jobs', '') FROM j")
        .bind(kind)
        .bind(payload)
        .execute(db)
        .await?;
    Ok(())
}

/// Queue from outside a transaction, logging rather than failing the caller.
pub async fn enqueue_now(s: &AppState, kind: &str, payload: Value) {
    if let Err(e) = enqueue(&s.db, kind, payload).await {
        tracing::warn!(%kind, error = %e, "job not queued");
    }
}

/// Run jobs as they come (woken by NOTIFY, with a poll as a fallback).
pub async fn runner(s: AppState) {
    let mut listener = PgListener::connect_with(&s.db).await.ok();
    if let Some(l) = listener.as_mut()
        && l.listen("pyo_jobs").await.is_err()
    {
        listener = None;
    }
    loop {
        // Drain what's due, then wait for a notification or the next poll.
        loop {
            match claim_and_run(&s).await {
                Ok(true) => continue,
                Ok(false) => break,
                Err(e) => {
                    tracing::warn!(error = %e, "job runner");
                    break;
                }
            }
        }
        match listener.as_mut() {
            Some(l) => {
                let _ = tokio::time::timeout(Duration::from_secs(5), l.recv()).await;
            }
            None => tokio::time::sleep(Duration::from_secs(5)).await,
        }
    }
}

/// Run one due job, if there is one.
async fn claim_and_run(s: &AppState) -> sqlx::Result<bool> {
    let job: Option<(i64, String, Value, i32)> = sqlx::query_as(
        "UPDATE pyo.jobs SET status = 'running', locked_at = now(), attempts = attempts + 1
          WHERE id = (SELECT id FROM pyo.jobs
                       WHERE (status = 'queued' AND run_at <= now())
                          OR (status = 'running' AND locked_at < now() - interval '5 minutes')
                       ORDER BY run_at LIMIT 1 FOR UPDATE SKIP LOCKED)
          RETURNING id, kind, payload, attempts",
    )
    .fetch_optional(&s.db)
    .await?;
    let Some((id, kind, payload, attempts)) = job else {
        return Ok(false);
    };
    match run(s, &kind, &payload).await {
        Ok(()) => {
            sqlx::query("UPDATE pyo.jobs SET status = 'done', finished_at = now(), last_error = NULL WHERE id = $1").bind(id).execute(&s.db).await?;
        }
        Err(e) => {
            let msg: String = format!("{e:#}").chars().take(1000).collect();
            tracing::warn!(%kind, id, attempts, error = %msg, "job failed");
            if attempts >= MAX_ATTEMPTS {
                sqlx::query("UPDATE pyo.jobs SET status = 'failed', finished_at = now(), last_error = $2 WHERE id = $1").bind(id).bind(&msg).execute(&s.db).await?;
            } else {
                // 30s, 1m, 2m, … up to an hour.
                let wait = (30_i64 << (attempts - 1).clamp(0, 7)).min(3600);
                sqlx::query("UPDATE pyo.jobs SET status = 'queued', run_at = now() + make_interval(secs => $2), last_error = $3 WHERE id = $1")
                    .bind(id)
                    .bind(wait as f64)
                    .bind(&msg)
                    .execute(&s.db)
                    .await?;
            }
        }
    }
    Ok(true)
}

async fn run(s: &AppState, kind: &str, p: &Value) -> anyhow::Result<()> {
    let str_of = |k: &str| p[k].as_str().unwrap_or("").to_owned();
    match kind {
        "git.status" => {
            crate::git::report(
                s,
                &str_of("deploymentId"),
                &str_of("state"),
                &str_of("description"),
            )
            .await
        }
        "git.hooks" => crate::git::sync_hooks(s, &str_of("connectionId")).await,
        "git.revoke" => crate::git::revoke(s, p).await,
        "registry.delete" => crate::deploy::delete_image(&str_of("repo"), &str_of("digest")).await,
        "email.invite" => {
            crate::ops::send_invite_email(
                s,
                &str_of("teamId"),
                &str_of("email"),
                &str_of("role"),
                &str_of("inviter"),
                p["invite"].as_str(),
            )
            .await
        }
        "alert.send" => crate::alerts::run_job(s, p).await,
        // Previews (previews/): the bot, a service's settings, a redeploy.
        "preview.report" => crate::previews::report(s, &str_of("pullId")).await,
        "preview.sync" => crate::previews::sync_service(s, &str_of("serviceId")).await,
        "preview.reconcile" => crate::previews::reconcile(s, &str_of("pullId"))
            .await
            .map_err(|e| anyhow::anyhow!("{}", e.message)),
        other => anyhow::bail!("unknown job kind {other}"),
    }
}

/// Finished jobs older than a week are deleted; failed ones are kept a month.
/// Also prunes old MCP calls, logs, audit entries, rate limits and
/// long-expired API keys.
pub async fn prune(s: &AppState) -> sqlx::Result<()> {
    sqlx::query("DELETE FROM pyo.jobs WHERE (status = 'done' AND finished_at < now() - interval '7 days') OR (status = 'failed' AND finished_at < now() - interval '30 days')")
        .execute(&s.db)
        .await?;
    sqlx::query("DELETE FROM pyo.mcp_calls WHERE created_at < now() - interval '30 days'")
        .execute(&s.db)
        .await?;
    // Expired API keys stay listed (as expired) for a month.
    sqlx::query("DELETE FROM pyo.api_tokens WHERE expires_at < now() - interval '30 days'")
        .execute(&s.db)
        .await?;
    // Imports from other platforms: their results are shown for a month.
    sqlx::query("DELETE FROM pyo.platform_imports WHERE created_at < now() - interval '30 days'")
        .execute(&s.db)
        .await?;
    prune_deployment_logs(&s.db).await?;
    // The audit log, as long as the installation keeps it (a year on Pyo Cloud).
    crate::audit::prune(&s.db).await?;
    crate::database_logs::prune(&s.db).await?;
    crate::ratelimit::prune(&s.db).await?;
    Ok(())
}

/// Build and deploy logs are kept as long as the installation's retention
/// says (90 days by default, 30 to 365: Settings › Observability), on Pyo
/// Cloud and self-hosted: a batch at a time, so a large backlog doesn't
/// hold the table up (this runs every 15 seconds). Returns the lines
/// deleted.
pub async fn prune_deployment_logs(db: &sqlx::PgPool) -> sqlx::Result<u64> {
    let days = match crate::retention::load(db).await {
        Ok(r) => r.build_log_days,
        Err(_) => crate::retention::Retention::default().build_log_days,
    };
    Ok(sqlx::query(
        "DELETE FROM pyo.deployment_logs WHERE id IN (
           SELECT id FROM pyo.deployment_logs WHERE at < now() - make_interval(days => $1) LIMIT 5000)",
    )
    .bind(i32::from(days))
    .execute(db)
    .await?
    .rows_affected())
}

/// Payload helpers, so enqueuers and the runner agree on field names.
pub fn git_status(deployment_id: &str, state: &str, description: &str) -> Value {
    json!({ "deploymentId": deployment_id, "state": state, "description": description })
}

#[cfg(test)]
mod log_tests {
    /// Against a real Postgres (DATABASE_URL, a throwaway one): lines older
    /// than 90 days go, newer ones stay; with the setting at 30 days, those
    /// older than 30 go.
    #[tokio::test]
    #[ignore = "needs DATABASE_URL (a throwaway Postgres)"]
    async fn build_logs_go_after_the_settings_days() {
        let pool = sqlx::PgPool::connect(&std::env::var("DATABASE_URL").unwrap()).await.unwrap();
        crate::migrate::run(&pool).await.unwrap();
        let run = crate::ids::new_id("t");
        let (team, cluster, dpl) = (format!("team_{run}"), format!("cl_{run}"), format!("dpl_{run}"));
        let q = |sql: &'static str| sqlx::query(sql);
        q("INSERT INTO pyo.teams (id, name, slug) VALUES ($1, $1, $1)").bind(&team).execute(&pool).await.unwrap();
        q("INSERT INTO pyo.clusters (id, team_id, name, agent_token_hash) VALUES ($1, $2, $1, $1)").bind(&cluster).bind(&team).execute(&pool).await.unwrap();
        q("INSERT INTO pyo.projects (id, team_id, name, namespace) VALUES ($1, $2, 'p', $1)").bind(format!("prj_{run}")).bind(&team).execute(&pool).await.unwrap();
        q("INSERT INTO pyo.services (id, team_id, project_id, cluster_id, name, spec) VALUES ($1, $2, $3, $4, 'web', '{}')").bind(format!("svc_{run}")).bind(&team).bind(format!("prj_{run}")).bind(&cluster).execute(&pool).await.unwrap();
        q("INSERT INTO pyo.deployments (id, team_id, project_id, service_id, cluster_id, status, data, spec) VALUES ($1, $2, $3, $4, $5, 'succeeded', '{}', '{}')").bind(&dpl).bind(&team).bind(format!("prj_{run}")).bind(format!("svc_{run}")).bind(&cluster).execute(&pool).await.unwrap();
        for days in [120, 91, 89, 1] {
            q("INSERT INTO pyo.deployment_logs (deployment_id, at, level, source, message) VALUES ($1, now() - make_interval(days => $2), 'info', 'build', 'x')")
                .bind(&dpl)
                .bind(days)
                .execute(&pool)
                .await
                .unwrap();
        }
        // The default: 90 days.
        q("DELETE FROM pyo.settings WHERE key = 'observability'").execute(&pool).await.unwrap();
        while super::prune_deployment_logs(&pool).await.unwrap() > 0 {}
        let left = || async {
            sqlx::query_scalar::<_, i32>("SELECT extract(day FROM now() - at)::int FROM pyo.deployment_logs WHERE deployment_id = $1 ORDER BY at")
                .bind(&dpl)
                .fetch_all(&pool)
                .await
                .unwrap()
        };
        assert_eq!(left().await, [89, 1]);
        // The installation keeps them 30 days.
        q("INSERT INTO pyo.settings (key, value) VALUES ('observability', '{\"retention\": {\"analyticsMonths\": 12, \"metricsDays\": 30, \"buildLogDays\": 30}}')")
            .execute(&pool)
            .await
            .unwrap();
        while super::prune_deployment_logs(&pool).await.unwrap() > 0 {}
        assert_eq!(left().await, [1]);
        q("DELETE FROM pyo.settings WHERE key = 'observability'").execute(&pool).await.unwrap();
        q("DELETE FROM pyo.teams WHERE id = $1").bind(&team).execute(&pool).await.unwrap();
    }
}
