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

/// The most connections a process's pool opens, from Postgres's
/// max_connections (set by the server's memory: 40, 80 or 120).
///
/// The worker's floor: each of its periodic loops holds a connection for
/// its leader lock while it runs and needs another for its queries, and
/// they all start at once; with the job listener that's 9 held, so 12
/// leaves them room. Pyo's own backups and migrations run in these pools.
///
/// What it must fit: an update runs two web and two worker processes
/// (maxSurge 1), so 2 × (web + worker) + spare ≤ max_connections
/// (6 spare: pool_fits_postgres checks it for each tier): 40 → 5 + 12 (34 + 6),
/// 80 → 10 + 16 (52 + 6), 120 → 15 + 24 (78 + 6).
pub fn pool_size(role: Role, max_connections: u32) -> u32 {
    let web = (max_connections / 8).clamp(5, 20);
    let worker = (max_connections / 5).clamp(12, 30);
    match role {
        Role::Web => web,
        Role::Worker => worker,
        Role::All => web + worker,
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Connections kept free of Pyo's pools: psql by hand, a restore.
    const POOL_SPARE: u32 = 6;

    /// pyo-postgres's start script (platform.yaml), as the kubelet runs it.
    fn postgres_script() -> String {
        let doc = include_str!("../../../deploy/k8s/platform.yaml")
            .split("\n---")
            .filter_map(|d| serde_norway::from_str::<serde_json::Value>(d).ok())
            .find(|d| d["kind"] == "StatefulSet" && d["metadata"]["name"] == "pyo-postgres")
            .unwrap();
        let c = &doc["spec"]["template"]["spec"]["containers"][0];
        assert_eq!(c["command"], serde_json::json!(["sh", "-c"]));
        c["args"][0].as_str().unwrap().replace("$$(", "$(")
    }

    /// The script's Postgres arguments on a server with `mib` of memory
    /// (a stand-in entrypoint prints them).
    fn postgres_args(mib: u64) -> String {
        let dir = std::env::temp_dir().join(format!("pyo-pg-{}-{mib}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("meminfo"),
            format!("MemTotal: {} kB\n", mib * 1024),
        )
        .unwrap();
        let entry = dir.join("docker-entrypoint.sh");
        std::fs::write(&entry, "#!/bin/sh\necho \"$@\"\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&entry, std::fs::Permissions::from_mode(0o755)).unwrap();
        let out = std::process::Command::new("sh")
            .arg("-c")
            .arg(postgres_script())
            .env("PYO_MEMINFO", dir.join("meminfo"))
            .env("PATH", format!("{}:/usr/bin:/bin", dir.display()))
            .output()
            .unwrap();
        String::from_utf8(out.stdout).unwrap()
    }

    fn setting(args: &str, name: &str) -> u32 {
        args.split_whitespace()
            .find_map(|a| a.strip_prefix(&format!("{name}=")))
            .unwrap_or_else(|| panic!("{name} in {args}"))
            .parse()
            .unwrap()
    }

    /// Postgres follows the server's memory, by pyo-proto's tiers, and
    /// every tier's max_connections holds Pyo's pools while an update runs
    /// two of each process.
    #[test]
    fn pool_fits_postgres() {
        use pyo_proto::memory::{LARGE_MIB, SMALL_MIB};
        let tiers = [
            (1945, 40, "shared_buffers=16MB"),
            (SMALL_MIB - 1, 40, "shared_buffers=16MB"),
            (SMALL_MIB, 80, "shared_buffers=32MB"),
            (LARGE_MIB - 1, 80, "shared_buffers=32MB"),
            (LARGE_MIB, 120, "shared_buffers=64MB"),
            (32768, 120, "shared_buffers=64MB"),
        ];
        for (mib, max, buffers) in tiers {
            let args = postgres_args(mib);
            assert!(args.starts_with("postgres "), "{mib}: {args}");
            assert!(args.contains(buffers), "{mib}: {args}");
            assert!(args.contains("huge_pages=off"), "{mib}: {args}");
            assert_eq!(setting(&args, "max_connections"), max, "{mib}");
            let (web, worker) = (pool_size(Role::Web, max), pool_size(Role::Worker, max));
            assert!(worker >= 12, "the worker's loops need 12: {max}");
            assert!(
                2 * (web + worker) + POOL_SPARE <= max,
                "{max}: 2 × ({web} + {worker}) + {POOL_SPARE}"
            );
        }
        assert_eq!(
            (pool_size(Role::Web, 80), pool_size(Role::Worker, 80)),
            (10, 16)
        );
    }
}
