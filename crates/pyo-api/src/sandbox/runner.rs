//! The sandbox loop, on one worker at a time: every [`TICK`] it reads each
//! sandbox server's report over the tailnet, keeps servers and sandboxes in
//! step with it, meters usage per minute, and ends sandboxes past their
//! lifetime (the daemons end them too, so they end on time while Pyo is
//! down). It does nothing, not even take its lock, until the first sandbox
//! server exists.

use std::collections::HashMap;
use std::time::Duration;

use futures::StreamExt;
use pyo_proto::sandbox::{DaemonCommand, ErrorCode, Runtime, SandboxState, ServerReport};
use serde_json::json;
use sqlx::PgPool;

use super::{daemon::Daemon, end, keys, save_status};
use crate::AppState;

pub const TICK: Duration = Duration::from_secs(15);
/// A server whose report is this old is unreachable.
pub const UNREACHABLE_SECS: i64 = 60;
/// A sandbox past its lifetime by this much is ended by Pyo.
const GRACE_SECS: i64 = 30;
/// A sandbox still creating after this long failed. Its create may first
/// wait for its server to make `base` ([`super::firstuse::LIMIT`]).
const CREATE_TIMEOUT_SECS: i64 = super::firstuse::LIMIT.as_secs() as i64 + 120;

pub async fn runner(s: AppState) {
    let mut tick = tokio::time::interval(TICK);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tick.tick().await;
        if s.stopping() {
            return;
        }
        match any_servers(&s.db).await {
            Ok(true) => {}
            Ok(false) => continue,
            Err(e) => {
                tracing::warn!(error = %e, "sandboxes: couldn't check for sandbox servers");
                continue;
            }
        }
        let db = s.db.clone();
        crate::bg::as_leader(&db, crate::bg::LOCK_SANDBOXES, || async {
            if let Err(e) = round(&s).await {
                tracing::warn!(error = %format!("{e:#}"), "sandboxes: a round failed");
            }
        })
        .await;
    }
}

async fn any_servers(db: &PgPool) -> sqlx::Result<bool> {
    sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM pyo.sandbox_servers)")
        .fetch_one(db)
        .await
}

#[derive(sqlx::FromRow)]
pub(crate) struct Server {
    pub id: String,
    pub team_id: Option<String>,
    pub address: String,
    pub state: String,
}

/// One round: reports, then lifetimes.
pub async fn round(s: &AppState) -> anyhow::Result<()> {
    let _ = super::join::prune(&s.db).await;
    if let Err(e) = super::outbound::channel::sweep(&s.db).await {
        tracing::warn!(error = %e, "sandboxes: couldn't sweep outbound servers' commands");
    }
    // Pyo's own templates, at the images it has now.
    if let Err(e) = super::base::ensure_all(s).await {
        tracing::warn!(error = %format!("{e:#}"), "sandboxes: couldn't keep Pyo's templates");
    }
    let servers: Vec<Server> = sqlx::query_as(
        // Outbound servers (Pyo Cloud) post their reports (outbound/channel.rs).
        "SELECT id, team_id, address, state FROM pyo.sandbox_servers WHERE address IS NOT NULL AND state <> 'refused' AND NOT outbound",
    )
    .fetch_all(&s.db)
    .await?;
    if !servers.is_empty() {
        let key = keys::signing_key(&s.db, &s.sealer)
            .await
            .map_err(|e| anyhow::anyhow!(e.message))?;
        futures::stream::iter(servers)
            .for_each_concurrent(8, |srv| {
                let key = key.clone();
                async move {
                    let token = keys::admin_token(&key, &srv.id);
                    let r = Daemon::new(&srv.id, &srv.address).report(&token).await;
                    let res = match r {
                        Ok(report) => {
                            let applied = apply_report(&s.db, &srv, &report).await;
                            if applied.is_ok() && report.server_id == srv.id {
                                // Templates it lacks, and ones Pyo dropped.
                                if let Err(e) = super::templates::sync_server(
                                    &s.db,
                                    &key,
                                    &srv.id,
                                    srv.team_id.as_deref(),
                                    &srv.address,
                                    &report,
                                )
                                .await
                                {
                                    tracing::warn!(server = %srv.id, error = %e, "sandboxes: couldn't sync templates");
                                }
                            }
                            applied
                        }
                        Err(e) => missed_report(&s.db, &srv, &e.message).await,
                    };
                    if let Err(e) = res {
                        tracing::warn!(server = %srv.id, error = %e, "sandboxes: couldn't save a report");
                    }
                }
            })
            .await;
    }
    // Outbound servers' reports come in over their channel: theirs count too.
    if let Err(e) = super::templates::update_states(&s.db).await {
        tracing::warn!(error = %e, "sandboxes: couldn't update template states");
    }
    sweep(s).await?;
    super::edgecert::run(s).await
}

/// Saves a report: the server's capability and capacity, its sandboxes'
/// states, and the seconds they ran since the last report.
async fn apply_report(db: &PgPool, srv: &Server, r: &ServerReport) -> sqlx::Result<()> {
    if r.server_id != srv.id {
        return missed_report(
            db,
            srv,
            "The daemon there belongs to another sandbox server.",
        )
        .await;
    }
    let mut tx = db.begin().await?;
    apply_in(&mut tx, srv, r).await?;
    tx.commit().await
}

/// [`apply_report`] in a transaction, for a report that is the server's own.
pub(crate) async fn apply_in(
    tx: &mut sqlx::PgConnection,
    srv: &Server,
    r: &ServerReport,
) -> sqlx::Result<()> {
    let since: Option<f64> = sqlx::query_scalar(
        "SELECT extract(epoch FROM now() - last_report_at)::double precision FROM pyo.sandbox_servers WHERE id = $1",
    )
    .bind(&srv.id)
    .fetch_one(&mut *tx)
    .await?;
    let (state, error) = if r.capability.runtime == Runtime::None {
        ("refused", Some(r.capability.summary()))
    } else {
        ("ready", None)
    };
    sqlx::query(
        "UPDATE pyo.sandbox_servers SET state = $2, error = $3, capability = $4, capacity = $5, templates = $6,
                daemon_version = $7, edge_domain = coalesce($8, edge_domain), last_report_at = now(), updated_at = now()
          WHERE id = $1",
    )
    .bind(&srv.id)
    .bind(state)
    .bind(&error)
    .bind(json!(r.capability))
    .bind(json!(r.capacity))
    .bind(json!(r.templates))
    .bind(&r.daemon_version)
    .bind(r.capability.edge_domain())
    .execute(&mut *tx)
    .await?;
    if srv.state != state {
        tracing::info!(server = %srv.id, %state, "sandbox server is {state}");
    }

    // Sandboxes Pyo has on this server, and what the server says of them.
    let ours: Vec<(String, String, String, i64)> = sqlx::query_as(
        "SELECT id, team_id, state, extract(epoch FROM now() - created_at)::bigint FROM pyo.sandboxes
          WHERE server_id = $1 AND state NOT IN ('stopped', 'failed')",
    )
    .bind(&srv.id)
    .fetch_all(&mut *tx)
    .await?;
    let theirs: HashMap<&str, _> = r.sandboxes.iter().map(|x| (x.id.as_str(), x)).collect();
    let seconds = since
        .map(|s| s.clamp(0.0, 60.0).round() as i32)
        .unwrap_or(0);
    for (id, team, state, age) in ours {
        match theirs.get(id.as_str()) {
            Some(st) => {
                save_status(&mut *tx, st).await?;
                if st.state == SandboxState::Running && seconds > 0 {
                    meter(
                        &mut *tx,
                        &id,
                        &team,
                        &srv.id,
                        st.cpus,
                        st.memory_mib,
                        seconds,
                    )
                    .await?;
                }
            }
            // Still being made, or not reported yet.
            None if state == "creating" && age < CREATE_TIMEOUT_SECS => {}
            // The server ended it (its lifetime ran out) or lost it.
            None => end(&mut *tx, &id, "stopped", None).await?,
        }
    }
    Ok(())
}

async fn meter(
    tx: &mut sqlx::PgConnection,
    id: &str,
    team: &str,
    server: &str,
    cpus: u32,
    memory_mib: u32,
    seconds: i32,
) -> sqlx::Result<()> {
    sqlx::query(
        "INSERT INTO pyo.sandbox_usage (sandbox_id, minute, team_id, server_id, cpus, memory_mib, seconds)
         VALUES ($1, date_trunc('minute', now()), $2, $3, $4, $5, $6)
         ON CONFLICT (sandbox_id, minute) DO UPDATE SET seconds = least(pyo.sandbox_usage.seconds + $6, 60),
           cpus = $4, memory_mib = $5",
    )
    .bind(id)
    .bind(team)
    .bind(server)
    .bind(cpus as i32)
    .bind(memory_mib as i32)
    .bind(seconds)
    .execute(tx)
    .await?;
    Ok(())
}

/// No report: unreachable once the last one is old.
async fn missed_report(db: &PgPool, srv: &Server, why: &str) -> sqlx::Result<()> {
    let n = sqlx::query(
        "UPDATE pyo.sandbox_servers SET state = 'unreachable', error = $2, updated_at = now()
          WHERE id = $1 AND state <> 'unreachable'
            AND coalesce(last_report_at, joined_at, created_at) < now() - make_interval(secs => $3)",
    )
    .bind(&srv.id)
    .bind(why)
    .bind(UNREACHABLE_SECS as f64)
    .execute(db)
    .await?
    .rows_affected();
    if n > 0 {
        tracing::warn!(server = %srv.id, %why, "sandbox server is unreachable");
    }
    Ok(())
}

/// Ends what's past its lifetime, retries deletes that didn't reach their
/// server, and fails creates that never finished.
async fn sweep(s: &AppState) -> anyhow::Result<()> {
    let mut c = s.db.acquire().await?;
    let stuck: Vec<String> = sqlx::query_scalar(
        "SELECT id FROM pyo.sandboxes WHERE state = 'creating' AND created_at < now() - make_interval(secs => $1)",
    )
    .bind(CREATE_TIMEOUT_SECS as f64)
    .fetch_all(&mut *c)
    .await?;
    for id in stuck {
        end(&mut c, &id, "failed", Some("It didn't start in time.")).await?;
    }
    // An outbound server that isn't ready is asked nothing: its answer
    // would only come at its next poll.
    let due: Vec<(String, Option<String>, Option<String>, bool, bool)> = sqlx::query_as(
        "SELECT x.id, x.server_id, sv.address,
                coalesce(x.expires_at < now() - make_interval(secs => $1), false),
                coalesce(sv.outbound AND sv.state <> 'ready', false)
           FROM pyo.sandboxes x LEFT JOIN pyo.sandbox_servers sv ON sv.id = x.server_id
          WHERE x.state NOT IN ('stopped', 'failed')
            AND (x.state = 'stopping' OR x.expires_at < now() - make_interval(secs => $1))
          LIMIT 200",
    )
    .bind(GRACE_SECS as f64)
    .fetch_all(&mut *c)
    .await?;
    if due.is_empty() {
        return Ok(());
    }
    let key = keys::signing_key(&s.db, &s.sealer)
        .await
        .map_err(|e| anyhow::anyhow!(e.message))?;
    for (id, server, address, expired, asleep) in due {
        let gone = match (server, address) {
            _ if asleep => false,
            (Some(server), Some(address)) => {
                let token = keys::admin_token(&key, &server);
                match Daemon::new(&server, &address)
                    .command(&token, &DaemonCommand::Delete { id: id.clone() })
                    .await
                {
                    Ok(_) => true,
                    Err(e) if e.code == ErrorCode::NotFound => true,
                    Err(e) => {
                        tracing::debug!(%id, error = %e, "sandboxes: couldn't delete, trying again");
                        false
                    }
                }
            }
            _ => true,
        };
        // Past its lifetime, its server ends it on its own.
        if gone || expired {
            end(&mut c, &id, "stopped", None).await?;
        }
    }
    Ok(())
}
