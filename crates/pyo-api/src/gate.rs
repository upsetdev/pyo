//! A hosted Pyo's teams whose plan is paused or has ended (Pyo Cloud
//! Terms, sections 9 and 11): their clusters get no new commands. One
//! check, [`Entitlements::can_command`](crate::Entitlements::can_command),
//! asked where commands are queued (a person's or API key's request is
//! refused with its message) and where agents take them (anything else
//! held back fails with it, unsigned).
//!
//! Still allowed: heartbeats and their replies (not commands), so servers
//! keep reporting. Commands that only read what the dashboard shows. And
//! removals, so a team can delete its things and leave.
//!
//! Self-hosted Pyo has no entitlements: nothing is held back.
//!
//! What was held back isn't lost: the team is marked, and once its plan
//! allows commands again its clusters get what Pyo wants there sent once
//! more ([`resume`]).

use axum::http::StatusCode;
use sqlx::PgConnection;

use crate::error::{ApiError, ApiResult};

/// Commands that only read.
const READS: &[&str] = &[
    "fetch_logs",
    "follow_logs",
    "fetch_database_logs",
    "query_traffic",
    "list_snapshots",
    "cancel",
];

/// Commands that remove a team's things.
const REMOVALS: &[&str] = &[
    "delete_service",
    "delete_database",
    "delete_volume",
    "delete_namespace",
    "remove_bundle",
    "remove_builder",
    "remove_server",
    "forget_snapshot",
    "tailscale_remove",
];

/// Commands that follow a change made elsewhere, a removal's too (the
/// firewall without a deleted service's WAF, the static file server
/// without its last site). Queued without an error, then held back with
/// the rest.
const FOLLOW_ONS: &[&str] = &[
    "sync_routes",
    "sync_firewall",
    "sync_registry_tokens",
    "sync_cron_jobs",
    "apply_bundle",
    "tailscale_sync",
];

/// Why a team's clusters get no new commands, when the entitlement doesn't say.
pub const STOPPED: &str = "This team's Pyo Cloud plan is paused or has ended, so its servers get no new commands. Your services keep running. Pay in Settings › Billing to resume.";

/// Whether a command of this kind still goes out while a team's plan is
/// paused or has ended.
pub fn allowed(kind: &str) -> bool {
    READS.contains(&kind) || REMOVALS.contains(&kind)
}

/// Whether queueing a command of this kind asks the entitlement: for a
/// person's or API key's request (`request`), and a kind that's held back
/// and isn't a change's follow-on.
fn asks_at_queue(kind: &str, request: bool) -> bool {
    request && !allowed(kind) && !FOLLOW_ONS.contains(&kind)
}

/// Before a command of `kind` is queued for one of `team_id`'s clusters:
/// fails with 402 and the entitlement's message when a person's or API
/// key's request asks for one its team may not get now. Commands queued
/// by Pyo itself go in the queue, and [`held_back`] fails them.
pub(crate) async fn check_queue(
    conn: &mut PgConnection,
    team_id: &str,
    kind: &str,
) -> ApiResult<()> {
    let Some(e) = crate::ENTITLEMENTS.get() else {
        return Ok(());
    };
    if allowed(kind) {
        return Ok(());
    }
    // A request's transaction is lowered to its teams (rls.rs). A hosted
    // build's billing tables are outside row security.
    let lifted = crate::rls::lift(conn).await?;
    let out = if asks_at_queue(kind, lifted.was_scoped()) {
        e.can_command(conn, team_id).await
    } else {
        Ok(())
    };
    lifted.lower(conn).await?;
    out.map_err(|m| ApiError::new(StatusCode::PAYMENT_REQUIRED, m))
}

/// When an agent takes commands: why `team_id`'s clusters get none now
/// but reads and removals, if so.
pub(crate) async fn held_back(db: &sqlx::PgPool, team_id: &str) -> ApiResult<Option<String>> {
    let Some(e) = crate::ENTITLEMENTS.get() else {
        return Ok(None);
    };
    let mut conn = db.acquire().await?;
    Ok(e.can_command(&mut conn, team_id).await.err())
}

/// A command of `team_id`'s was held back: [`resume`] sends its clusters
/// their syncs again once its plan allows it.
pub(crate) async fn mark_held(db: &sqlx::PgPool, team_id: &str) {
    let _ = sqlx::query(
        "UPDATE pyo.teams SET commands_held_at = now() WHERE id = $1 AND commands_held_at IS NULL",
    )
    .bind(team_id)
    .execute(db)
    .await;
}

/// From the worker's sweep: each team that had commands held back and may
/// get commands again has its clusters' syncs queued once more (the
/// clusters to wake come back). Asks only for marked teams.
pub async fn resume(s: &crate::AppState) -> ApiResult<()> {
    let Some(e) = crate::ENTITLEMENTS.get() else {
        return Ok(());
    };
    let teams: Vec<String> =
        sqlx::query_scalar("SELECT id FROM pyo.teams WHERE commands_held_at IS NOT NULL")
            .fetch_all(&s.db)
            .await?;
    for team in teams {
        let mut tx = s.db.begin().await?;
        // Once: whoever takes the mark sends them.
        let marked: Option<String> = sqlx::query_scalar(
            "SELECT id FROM pyo.teams WHERE id = $1 AND commands_held_at IS NOT NULL FOR UPDATE SKIP LOCKED",
        )
        .bind(&team)
        .fetch_optional(&mut *tx)
        .await?;
        if marked.is_none() || e.can_command(&mut tx, &team).await.is_err() {
            continue;
        }
        sqlx::query("UPDATE pyo.teams SET commands_held_at = NULL WHERE id = $1")
            .bind(&team)
            .execute(&mut *tx)
            .await?;
        let clusters = resend(&mut tx, &team).await?;
        tx.commit().await?;
        tracing::info!(%team, clusters = clusters.len(), "commands allowed again: syncs sent again");
        for c in clusters {
            s.wake(&c).await;
        }
    }
    Ok(())
}

/// What Pyo wants on each of a team's clusters, queued again: each running
/// service's routes, firewall and WAF (with the WAF's bundle), Pyo Static,
/// cron jobs, the registry's tokens and the tailnet's names. Deploys and
/// other changes a person asked for aren't: they were told why they
/// didn't happen. Gives back the clusters.
async fn resend(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    team_id: &str,
) -> ApiResult<Vec<String>> {
    let clusters: Vec<String> =
        sqlx::query_scalar("SELECT id FROM pyo.clusters WHERE team_id = $1 ORDER BY created_at")
            .bind(team_id)
            .fetch_all(&mut **tx)
            .await?;
    for c in &clusters {
        crate::firewall::sync(tx, c).await?;
        crate::sites::sync(tx, c).await?;
        let crons: Vec<(String, serde_json::Value)> = sqlx::query_as(
            "SELECT id, spec->'cronJobs' FROM pyo.services
              WHERE cluster_id = $1 AND current_deployment_id IS NOT NULL AND NOT deleting AND spec ? 'cronJobs'
              ORDER BY created_at",
        )
        .bind(c)
        .fetch_all(&mut **tx)
        .await?;
        for (service_id, jobs) in crons {
            let service = crate::ops::service_ref(tx, &service_id).await?;
            let jobs = serde_json::from_value(jobs).unwrap_or_default();
            crate::deploy::enqueue(tx, c, pyo_proto::Command::SyncCronJobs { service, jobs })
                .await?;
        }
        crate::tailscale::send_again(tx, c).await?;
    }
    crate::registrydoor::sync_clusters(tx, team_id, None).await?;
    Ok(clusters)
}

/// A hosted build's plans, for tests: teams named `team_ended_…` have an
/// ended plan, and those in [`test_plans::PAUSED`] a paused one. Deploys
/// are refused, so commands are too (the default).
#[cfg(test)]
pub(crate) mod test_plans {
    use std::collections::BTreeSet;
    use std::sync::Mutex;

    pub static PAUSED: Mutex<BTreeSet<String>> = Mutex::new(BTreeSet::new());

    struct Plans;

    impl crate::Entitlements for Plans {
        fn can_add_server<'a>(
            &'a self,
            _: &'a mut sqlx::PgConnection,
            _: &'a str,
        ) -> futures::future::BoxFuture<'a, Result<(), String>> {
            Box::pin(async { Ok(()) })
        }
        fn can_deploy<'a>(
            &'a self,
            _: &'a mut sqlx::PgConnection,
            team_id: &'a str,
        ) -> futures::future::BoxFuture<'a, Result<(), String>> {
            let paused = PAUSED.lock().unwrap().contains(team_id);
            Box::pin(async move {
                if team_id.starts_with("team_ended_") || paused {
                    Err("This team's subscription ended.".into())
                } else {
                    Ok(())
                }
            })
        }
    }

    /// This test process is a hosted build with these plans.
    pub fn install() {
        let _ = crate::ENTITLEMENTS.set(std::sync::Arc::new(Plans));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_and_removals_go_out() {
        for k in [
            "fetch_logs",
            "follow_logs",
            "query_traffic",
            "delete_service",
            "remove_server",
            "cancel",
        ] {
            assert!(allowed(k), "{k}");
        }
        for k in [
            "deploy",
            "restart",
            "scale",
            "open_terminal",
            "update_agent",
            "backup",
            "sync_routes",
            "apply_bundle",
        ] {
            assert!(!allowed(k), "{k}");
        }
    }

    #[test]
    fn only_a_request_for_a_change_is_refused_when_queued() {
        assert!(asks_at_queue("restart", true));
        assert!(asks_at_queue("open_terminal", true));
        // Pyo's own (a scheduled backup, an agent update): held back later.
        assert!(!asks_at_queue("backup", false));
        assert!(!asks_at_queue("update_agent", false));
        // A deleted service's firewall sync doesn't stop the delete.
        assert!(!asks_at_queue("sync_firewall", true));
        assert!(!asks_at_queue("delete_service", true));
        assert!(!asks_at_queue("fetch_logs", true));
    }

    #[test]
    fn follow_ons_are_held_back() {
        for k in FOLLOW_ONS {
            assert!(!allowed(k), "{k}");
        }
    }

    #[test]
    fn the_message_is_plain() {
        assert!(!STOPPED.contains(';') && !STOPPED.contains(" - ") && !STOPPED.contains('—'));
    }
}
