//! What lets several Pyo processes share the work, so upgrades and crashes
//! don't stop anything:
//!
//! - roles: one binary runs as `web` (dashboard and API), `worker`
//!   (periodic loops and jobs) or `all` (both, the default for self-hosting);
//! - wake-ups across processes: Postgres NOTIFY on `pyo_wake`, so a deploy
//!   queued on one web replica wakes an agent long-polling another;
//! - leader locks: each periodic loop runs on one process at a time
//!   (a Postgres advisory lock, released if that process dies);
//! - shutdown: on SIGTERM, readiness fails and long-polls return, so the
//!   load balancer drains the process before it exits.

use std::time::Duration;

use sqlx::{PgPool, postgres::PgListener};
use tokio::sync::watch;

use crate::AppState;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    All,
    Web,
    Worker,
}

impl Role {
    pub fn from_env() -> anyhow::Result<Self> {
        match std::env::var("PYO_ROLE").unwrap_or_default().as_str() {
            "" | "all" => Ok(Self::All),
            "web" => Ok(Self::Web),
            "worker" => Ok(Self::Worker),
            other => anyhow::bail!("PYO_ROLE is \"{other}\". Use web, worker or all"),
        }
    }
    /// Serves the dashboard, the API, agents and builders.
    pub fn web(self) -> bool {
        self != Self::Worker
    }
    /// Runs periodic loops and jobs.
    pub fn worker(self) -> bool {
        self != Self::Web
    }
}

/// Advisory lock keys of the periodic loops.
pub const LOCK_SWEEPER: i64 = 7_001;
pub const LOCK_BUILDERS: i64 = 7_002;
pub const LOCK_GITHUB: i64 = 7_003;
pub const LOCK_DNS: i64 = 7_004;
pub const LOCK_BACKUP: i64 = 7_005;
pub const LOCK_TEAM_BACKUP: i64 = 7_006;
pub const LOCK_UPDATES: i64 = 7_007;
/// Not a loop's: the backup store's layout, set up by one process at a time.
pub const LOCK_BACKUP_STORE: i64 = 7_008;
pub const LOCK_DATABASE_IMPORTS: i64 = 7_031;

/// Run `f` if this process gets the loop's lock; another process holding it
/// means it's that one's turn. The lock lives on one connection for the run.
pub async fn as_leader<F, Fut, T>(db: &PgPool, key: i64, f: F) -> Option<T>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = T>,
{
    let mut conn = db.acquire().await.ok()?;
    let got: bool = sqlx::query_scalar("SELECT pg_try_advisory_lock($1)")
        .bind(key)
        .fetch_one(&mut *conn)
        .await
        .ok()?;
    if !got {
        return None;
    }
    let out = f().await;
    let _ = sqlx::query("SELECT pg_advisory_unlock($1)")
        .bind(key)
        .execute(&mut *conn)
        .await;
    Some(out)
}

/// Wake long-polls waiting on `key` (a cluster id, or "builders:<team>") in
/// every web process.
pub async fn wake(db: &PgPool, key: &str) {
    if let Err(e) = sqlx::query("SELECT pg_notify('pyo_wake', $1)")
        .bind(key)
        .execute(db)
        .await
    {
        tracing::warn!(%key, error = %e, "wake-up not sent");
    }
}

/// Deliver wake-ups from any process to this one's waiting long-polls.
/// Reconnects if the connection drops; a missed wake-up only means a poll
/// waits for its timeout.
pub async fn listen(state: AppState) {
    loop {
        let r = async {
            let mut l = PgListener::connect_with(&state.db).await?;
            l.listen("pyo_wake").await?;
            loop {
                let n = l.recv().await?;
                let key = n.payload();
                if let Some(prefix) = key.strip_suffix('*') {
                    state.notify_prefix(prefix).await;
                } else {
                    // Local only: re-broadcasting here would loop forever.
                    state.waker(key).await.notify_waiters();
                }
            }
            #[allow(unreachable_code)]
            anyhow::Ok(())
        }
        .await;
        if let Err(e) = r {
            tracing::warn!(error = %e, "wake-up listener lost its connection; reconnecting");
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

/// SIGTERM or Ctrl-C: flips `stopping` so readiness fails and long-polls
/// return, then lets the server finish requests in flight.
pub async fn shutdown_signal(stopping: watch::Sender<bool>) {
    let ctrl_c = tokio::signal::ctrl_c();
    let term = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut s) => {
                s.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    tokio::select! {
        _ = ctrl_c => {}
        _ = term => {}
    }
    tracing::info!("shutting down: draining requests");
    let _ = stopping.send(true);
}
