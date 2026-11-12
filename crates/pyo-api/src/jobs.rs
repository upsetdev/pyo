//! Durable background jobs in Postgres: things that must happen eventually
//! even if a process dies mid-way (reporting a commit status to GitHub,
//! deleting an old image, retrying an email). Enqueued in the same
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
        "github.status" => {
            crate::github::report(
                s,
                &str_of("deploymentId"),
                &str_of("state"),
                &str_of("description"),
            )
            .await
        }
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
        other => anyhow::bail!("unknown job kind {other}"),
    }
}

/// Finished jobs older than a week are deleted; failed ones are kept a month.
/// Also prunes old MCP calls, logs, rate limits and long-expired API keys.
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
    crate::database_logs::prune(&s.db).await?;
    crate::ratelimit::prune(&s.db).await?;
    Ok(())
}

/// Payload helpers, so enqueuers and the runner agree on field names.
pub fn github_status(deployment_id: &str, state: &str, description: &str) -> Value {
    json!({ "deploymentId": deployment_id, "state": state, "description": description })
}
