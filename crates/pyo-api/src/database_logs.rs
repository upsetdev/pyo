//! A database's logs and events: each pod's recent output, read by
//! its cluster's agent (FetchDatabaseLogs), with every line's part
//! (primary, replica, read replica, pooler, sentinel, REST API) from the
//! status the topology uses, so parts follow failovers. Events are the
//! cluster's about the database, the status changes Pyo saw (recorded in
//! `reported`) and the team's activity on it. Team members only, like
//! service logs; not an MCP tool for now.

use std::time::Duration;

use axum::{
    Json,
    extract::{Path, Query, State},
};
use pyo_proto::{Command, CommandResult, now_ms};
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::{Postgres, Transaction};

use crate::{
    AppState,
    auth::SessionUser,
    deploy,
    error::{ApiError, ApiResult, ErrorBody},
    ops::{Role, require_role, team_of},
};

#[derive(Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub struct LogsQuery {
    /// "live" (the default: the last lines), "15m", "1h", "6h" or "24h".
    range: Option<String>,
}

/// A database's logs and events
///
/// The recent output of each of its parts, oldest first: `at`, `level`,
/// `source` (the instance), `role` ("primary", "replica", "read",
/// "pooler", "read-pooler", "sentinel", "proxy" (KV's endpoint) or "other"), `message`, and
/// `raw` for structured lines. `events`: what happened to it, newest first.
#[utoipa::path(get, path = "/databases/{id}/logs", operation_id = "getDatabaseLogs", tag = "Databases",
    params(("id" = String, Path, description = "The database's id."), LogsQuery),
    responses(
        (status = 200, description = "`logs`, `events` and `fetchedAt`.", body = Object),
        (status = 404, description = "No such database, or you aren't in its team.", body = ErrorBody),
        (status = 502, description = "The agent couldn't read them.", body = ErrorBody),
        (status = 504, description = "The cluster's agent didn't answer in time.", body = ErrorBody),
    ),
    extensions(("x-pyo-access" = json!("read"))))]
pub async fn logs(
    State(s): State<AppState>,
    SessionUser(user): SessionUser,
    Path(id): Path<String>,
    Query(q): Query<LogsQuery>,
) -> ApiResult<Json<Value>> {
    let (tail, since) = match q.range.as_deref().unwrap_or("live") {
        "live" => (500, None),
        "15m" => (3000, Some(15 * 60)),
        "1h" => (3000, Some(3600)),
        "6h" => (3000, Some(6 * 3600)),
        "24h" => (3000, Some(24 * 3600)),
        _ => {
            return Err(ApiError::bad_request(
                "The range is live, 15m, 1h, 6h or 24h.",
            ));
        }
    };
    let mut tx = s.db.begin().await?;
    let team_id = team_of(&mut tx, "databases", &id).await?;
    require_role(&mut tx, &user, &team_id, Role::Viewer).await?;
    let (cluster_id, namespace, name, status): (String, String, String, Value) = sqlx::query_as(
        "SELECT cluster_id, namespace, name, status FROM pyo.databases WHERE id = $1",
    )
    .bind(&id)
    .fetch_one(&mut *tx)
    .await?;
    let cmd_id = deploy::enqueue(
        &mut tx,
        &cluster_id,
        Command::FetchDatabaseLogs {
            namespace,
            name: name.clone(),
            tail_lines: tail,
            since_seconds: since,
        },
    )
    .await?;
    tx.commit().await?;
    s.wake(&cluster_id).await;

    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    let r: CommandResult = loop {
        let row: Option<(String, Option<Value>)> =
            sqlx::query_as("SELECT status, result FROM pyo.commands WHERE id = $1")
                .bind(&cmd_id)
                .fetch_optional(&s.db)
                .await?;
        if let Some((st, result)) = row
            && (st == "done" || st == "failed")
        {
            break result
                .and_then(|v| serde_json::from_value(v).ok())
                .unwrap_or_default();
        }
        if tokio::time::Instant::now() > deadline {
            return Err(ApiError::new(
                axum::http::StatusCode::GATEWAY_TIMEOUT,
                "The cluster's agent didn't answer in time. Is it connected?",
            ));
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    };
    if !r.ok {
        return Err(ApiError::new(
            axum::http::StatusCode::BAD_GATEWAY,
            r.message
                .unwrap_or_else(|| "The agent couldn't read the logs.".into()),
        ));
    }
    let data = r.data.unwrap_or_default();
    let pods = &data["pods"];
    let logs: Vec<Value> = r
        .logs
        .iter()
        .map(|l| {
            let (level, message, raw) = if l.level.is_empty() { parse_line(&l.message) } else { (l.level.clone(), l.message.clone(), None) };
            let mut v = json!({ "at": l.at, "level": level, "source": l.source, "role": role_of(&l.source, &name, &status, &pods[&l.source]["labels"]), "message": message });
            if let Some(raw) = raw {
                v["raw"] = raw;
            }
            v
        })
        .collect();

    // Events: the cluster's, the status changes Pyo recorded, and the
    // team's changes to it, newest first.
    let mut events: Vec<Value> = data["events"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|e| {
            let (kind, reason, message) = plain_event(e["type"].as_str().unwrap_or(""), e["reason"].as_str().unwrap_or(""), e["message"].as_str().unwrap_or(""));
            json!({ "at": e["at"], "type": kind, "reason": reason, "source": "cluster", "object": e["object"], "message": message, "count": e["count"] })
        })
        .collect();
    let recorded: Vec<(i64, String, String, String)> = sqlx::query_as(
        "SELECT pyo.ms(at), type, reason, message FROM pyo.database_events WHERE database_id = $1 AND at > now() - interval '30 days' ORDER BY at DESC LIMIT 200",
    )
    .bind(&id)
    .fetch_all(&s.db)
    .await?;
    events.extend(recorded.into_iter().map(|(at, t, reason, message)| json!({ "at": at, "type": t, "reason": reason, "source": "pyo", "message": message })));
    let changes: Vec<(i64, Value)> = sqlx::query_as(
        "SELECT pyo.ms(at), data FROM pyo.activity WHERE team_id = $1 AND data->'target'->>'id' = $2 AND at > now() - interval '30 days' ORDER BY at DESC LIMIT 100",
    )
    .bind(&team_id)
    .bind(&id)
    .fetch_all(&s.db)
    .await?;
    events.extend(changes.into_iter().map(|(at, d)| {
        json!({
            "at": at,
            "type": if d["status"] == "warning" || d["status"] == "error" { "Warning" } else { "Normal" },
            "reason": activity_reason(d["type"].as_str().unwrap_or("")),
            "source": "team",
            "message": format!("{} ({})", d["title"].as_str().unwrap_or(""), d["actor"]["name"].as_str().unwrap_or("")),
        })
    }));
    events.sort_by_key(|e| std::cmp::Reverse(e["at"].as_i64().unwrap_or(0)));
    Ok(Json(
        json!({ "logs": logs, "events": events, "fetchedAt": now_ms() }),
    ))
}

/// The controller's event reasons that read better in words: (type,
/// reason, message), the message its own when it has one.
fn plain_event(kind: &str, reason: &str, message: &str) -> (String, String, String) {
    let (kind, reason, fallback) = match reason {
        // MySQL: a replica that couldn't rejoin was started over from the primary.
        "ReplicaRebuilt" => (
            "Warning",
            "Replica rebuilt",
            "A replica was rebuilt to rejoin.",
        ),
        _ => (kind, reason, ""),
    };
    (
        kind.to_owned(),
        reason.to_owned(),
        if message.is_empty() {
            fallback.to_owned()
        } else {
            message.to_owned()
        },
    )
}

fn activity_reason(kind: &str) -> &'static str {
    match kind {
        "database.created" => "Created",
        "database.restored" => "Restored",
        "database.import_started" => "Import started",
        "database.import_finished" => "Imported",
        "database.import_failed" => "Import failed",
        "database.deleted" => "Deleting",
        "database.connected" => "Connected",
        "database.disconnected" => "Disconnected",
        "secret.revealed" => "Password shown",
        _ => "Changed",
    }
}

/// A pod's part in database `name`: from the status (members and read
/// replicas, which follow failovers), else its labels, else its name.
pub(crate) fn role_of(pod: &str, name: &str, status: &Value, labels: &Value) -> &'static str {
    for m in status["members"].as_array().into_iter().flatten() {
        if m["name"] == pod {
            return if m["role"] == "primary" {
                "primary"
            } else {
                "replica"
            };
        }
    }
    if status["readReplicas"]["members"]
        .as_array()
        .into_iter()
        .flatten()
        .any(|m| m["name"] == pod)
    {
        return "read";
    }
    let label = |k: &str| labels[k].as_str().unwrap_or("");
    let read_pooler = format!("{name}-read-pooler");
    let pooler = format!("{name}-pooler");
    if label("cnpg.io/poolerName") == read_pooler || pod.starts_with(&format!("{read_pooler}-")) {
        return "read-pooler";
    }
    if label("cnpg.io/poolerName") == pooler || pod.starts_with(&format!("{pooler}-")) {
        return "pooler";
    }
    // An import's Job (pyo-db-controller's), or CloudNativePG's import as
    // the database is made (`<name>-1-import-…`).
    let cnpg_job = pod
        .strip_prefix(&format!("{name}-"))
        .and_then(|r| r.split_once('-'))
        .is_some_and(|(n, r)| {
            n.chars().all(|c| c.is_ascii_digit())
                && (r.starts_with("import-") || r.starts_with("initdb-"))
        });
    if label("pyo.dev/component") == "import" || cnpg_job {
        return "import";
    }
    match label("pyo.dev/component") {
        "sentinel" => return "sentinel",
        // pyo-kv: KV's Redis and REST endpoint.
        "kv" => return "proxy",
        "read" | "valkey-read" => return "read",
        // MySQL's backup and restore Jobs.
        "backup" | "restore" => return "backup",
        _ => {}
    }
    if label("cnpg.io/cluster") == format!("{name}-read")
        || pod.starts_with(&format!("{name}-read-"))
    {
        return "read";
    }
    // CloudNativePG's, Pyo's, or MOCO's (MySQL) label.
    for k in [
        "cnpg.io/instanceRole",
        "pyo.dev/role",
        "moco.cybozu.com/role",
    ] {
        match label(k) {
            "primary" => return "primary",
            "replica" => return "replica",
            _ => {}
        }
    }
    if pod.starts_with(&format!("{name}-sentinel")) {
        return "sentinel";
    }
    if pod.starts_with(&format!("{name}-kv-")) || pod.starts_with(&format!("{name}-rest")) {
        return "proxy";
    }
    "other"
}

/// A line's level and message: CloudNativePG's JSON (keeping it as `raw`),
/// Valkey's and Sentinel's `pid:role date time <mark> text`, PgBouncer's
/// `date time zone [pid] LEVEL text`, else guessed from its words. All
/// of it reads the line without its ANSI colors; a text line keeps them
/// for the dashboard to paint.
pub(crate) fn parse_line(line: &str) -> (String, String, Option<Value>) {
    let t = line.trim();
    let plain = pyo_proto::ansi::strip(t);
    let p = plain.trim();
    if p.starts_with('{')
        && let Ok(v) = serde_json::from_str::<Value>(p)
    {
        let rec = &v["record"];
        let severity = rec["error_severity"].as_str().unwrap_or("");
        let level = match (severity, v["level"].as_str().unwrap_or("info")) {
            ("PANIC" | "FATAL" | "ERROR", _) | (_, "error" | "fatal" | "panic" | "dpanic") => {
                "error"
            }
            ("WARNING", _) | (_, "warn" | "warning") => "warn",
            ("DEBUG1" | "DEBUG2" | "DEBUG3" | "DEBUG4" | "DEBUG5", _) | (_, "debug" | "trace") => {
                "debug"
            }
            _ => "info",
        };
        let msg = rec["message"]
            .as_str()
            .map(str::to_owned)
            .unwrap_or_else(|| {
                let m = v["msg"].as_str().unwrap_or("");
                // A record without a message (e.g. a checkpoint): its logger's.
                if m == "record" {
                    v["logger"].as_str().unwrap_or("").to_owned()
                } else {
                    m.to_owned()
                }
            });
        let msg = match rec["detail"].as_str() {
            Some(d) if !d.is_empty() => format!("{msg} · {d}"),
            _ => msg,
        };
        return (level.into(), msg, Some(v));
    }
    // MySQL: "2026-11-09T12:00:00.000000Z 0 [Warning] [MY-010068] [Server] text"
    if let Some(level) = mysql_level(p) {
        return (level.into(), t.into(), None);
    }
    // Valkey: "1:M 09 Nov 2026 12:00:00.000 * Ready to accept connections"
    if let Some((head, _)) = p.split_once(' ')
        && head.len() >= 3
        && head.ends_with(|c: char| "XCSM".contains(c))
        && head[..head.len() - 2].chars().all(|c| c.is_ascii_digit())
        && head.as_bytes()[head.len() - 2] == b':'
    {
        let mark = p.split(' ').nth(5).unwrap_or("");
        let level = match mark {
            "#" => "warn",
            "." | "-" => "debug",
            _ => "info",
        };
        return (level.into(), t.into(), None);
    }
    // PgBouncer: "2026-11-09 12:00:00.000 UTC [1] LOG text"
    if let Some(i) = p.find("] ") {
        let rest = &p[i + 2..];
        let word = rest.split(' ').next().unwrap_or("");
        let level = match word {
            "LOG" | "NOTICE" | "INFO" => Some("info"),
            "WARNING" => Some("warn"),
            "ERROR" | "FATAL" | "PANIC" => Some("error"),
            "DEBUG" | "NOISE" => Some("debug"),
            _ => None,
        };
        if let Some(l) = level {
            return (l.into(), t.into(), None);
        }
    }
    (pyo_proto::ansi::level_of(p).into(), t.into(), None)
}

/// A MySQL error log line's level: its `[System]`, `[Note]`, `[Warning]`
/// or `[ERROR]` after the time and thread.
fn mysql_level(t: &str) -> Option<&'static str> {
    let mut words = t.splitn(4, ' ');
    let (time, thread, mark) = (words.next()?, words.next()?, words.next()?);
    if !(time.len() >= 20
        && time.as_bytes()[4] == b'-'
        && time.contains('T')
        && thread.chars().all(|c| c.is_ascii_digit()))
    {
        return None;
    }
    Some(match mark.to_ascii_lowercase().as_str() {
        "[error]" => "error",
        "[warning]" => "warn",
        "[note]" => "debug",
        "[system]" => "info",
        _ => return None,
    })
}

/// What changed between two statuses the cluster reported, worth an event:
/// (type, reason, message).
pub(crate) fn transitions(old: &Value, new: &Value) -> Vec<(&'static str, &'static str, String)> {
    let mut out = vec![];
    if old.is_null() || old.as_object().is_some_and(|o| o.is_empty()) {
        return out;
    }
    let s = |v: &Value, k: &str| v[k].as_str().unwrap_or("").to_owned();
    let (op, np) = (s(old, "primary"), s(new, "primary"));
    let (of, nf) = (
        old["failovers"].as_i64().unwrap_or(0),
        new["failovers"].as_i64().unwrap_or(0),
    );
    if !op.is_empty() && !np.is_empty() && op != np {
        out.push((
            "Warning",
            "Failover",
            format!("{np} is the primary now (was {op})."),
        ));
    } else if nf > of {
        out.push((
            "Warning",
            "Failover",
            format!(
                "Failed over{}.",
                if np.is_empty() {
                    String::new()
                } else {
                    format!(" to {np}")
                }
            ),
        ));
    }
    let (ophase, nphase) = (s(old, "phase"), s(new, "phase"));
    if !nphase.is_empty() && ophase != nphase {
        let msg = match s(new, "message") {
            m if m.is_empty() => format!("{nphase}."),
            m => format!("{nphase}: {m}"),
        };
        out.push((
            if matches!(nphase.as_str(), "Degraded" | "Failed") {
                "Warning"
            } else {
                "Normal"
            },
            "Status",
            msg,
        ));
    }
    // Members becoming ready or not.
    let ready = |v: &Value, path: &[&str]| -> Vec<(String, bool)> {
        let mut x = v;
        for p in path {
            x = &x[*p];
        }
        x.as_array()
            .into_iter()
            .flatten()
            .map(|m| {
                (
                    m["name"].as_str().unwrap_or("").to_owned(),
                    m["ready"].as_bool().unwrap_or(false),
                )
            })
            .collect()
    };
    for path in [&["members"][..], &["readReplicas", "members"][..]] {
        let before = ready(old, path);
        for (name, now) in ready(new, path) {
            match before.iter().find(|(n, _)| *n == name) {
                Some((_, was)) if *was != now => out.push(if now {
                    ("Normal", "Ready", format!("{name} is ready."))
                } else {
                    ("Warning", "Not ready", format!("{name} isn't ready."))
                }),
                None if !before.is_empty() => {
                    out.push(("Normal", "Added", format!("{name} was added.")))
                }
                _ => {}
            }
        }
        for (name, _) in &before {
            if !ready(new, path).iter().any(|(n, _)| n == name) {
                out.push(("Normal", "Removed", format!("{name} was removed.")));
            }
        }
    }
    let b = |v: &Value, k: &str| v["backup"][k].as_str().unwrap_or("").to_owned();
    if b(new, "lastSuccessfulBackup") != b(old, "lastSuccessfulBackup")
        && !b(new, "lastSuccessfulBackup").is_empty()
    {
        out.push(("Normal", "Backup", "A base backup finished.".into()));
    }
    if b(new, "lastFailedBackup") != b(old, "lastFailedBackup")
        && !b(new, "lastFailedBackup").is_empty()
    {
        let m = b(new, "message");
        out.push((
            "Warning",
            "Backup failed",
            if m.is_empty() {
                "A base backup failed.".into()
            } else {
                format!("A base backup failed: {m}")
            },
        ));
    }
    out
}

/// Before a reported status replaces the stored one: its changes, as events.
pub(crate) async fn record(
    tx: &mut Transaction<'_, Postgres>,
    id: &str,
    old: &Value,
    new: &Value,
) -> ApiResult<()> {
    for (t, reason, message) in transitions(old, new) {
        sqlx::query("INSERT INTO pyo.database_events (database_id, type, reason, message) VALUES ($1, $2, $3, $4)")
            .bind(id)
            .bind(t)
            .bind(reason)
            .bind(message)
            .execute(&mut **tx)
            .await?;
    }
    Ok(())
}

/// Old status events go (from the worker loop, with the other clean-ups).
pub async fn prune(db: &sqlx::PgPool) -> sqlx::Result<()> {
    sqlx::query("DELETE FROM pyo.database_events WHERE at < now() - interval '30 days'")
        .execute(db)
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines() {
        let cnpg = r#"{"level":"info","ts":"2026-11-09T12:00:00Z","logger":"postgres","msg":"record","logging_pod":"main-1","record":{"error_severity":"LOG","message":"checkpoint complete"}}"#;
        let (l, m, raw) = parse_line(cnpg);
        assert_eq!((l.as_str(), m.as_str()), ("info", "checkpoint complete"));
        assert!(raw.is_some());
        let (l, m, _) = parse_line(
            r#"{"level":"info","msg":"record","record":{"error_severity":"FATAL","message":"the database system is starting up"}}"#,
        );
        assert_eq!(
            (l.as_str(), m.as_str()),
            ("error", "the database system is starting up")
        );
        assert_eq!(
            parse_line("1:M 09 Nov 2026 12:00:00.000 # WARNING overcommit_memory").0,
            "warn"
        );
        assert_eq!(
            parse_line("1:S 09 Nov 2026 12:00:00.000 * Ready to accept connections tcp").0,
            "info"
        );
        assert_eq!(
            parse_line("2026-11-09 12:00:00.000 UTC [1] WARNING server login failed").0,
            "warn"
        );
        assert_eq!(
            parse_line("2026-11-09 12:00:00.000 UTC [1] LOG stats: 0 xacts/s").0,
            "info"
        );
        assert_eq!(parse_line("something failed with error").0, "error");
        assert_eq!(parse_line("2026-11-09T12:00:00.000000Z 0 [Warning] [MY-010068] [Server] CA certificate is self signed.").0, "warn");
        assert_eq!(parse_line("2026-11-09T12:00:00.000000Z 0 [System] [MY-010931] [Server] ready for connections. errors: none").0, "info");
        assert_eq!(
            parse_line(
                "2026-11-09T12:00:00.000000Z 8 [ERROR] [MY-013183] [InnoDB] Assertion failure"
            )
            .0,
            "error"
        );
        assert_eq!(
            parse_line(
                "2026-11-09T12:00:00.123456Z 12 [Error] [MY-010584] [Repl] Replica SQL: error"
            )
            .0,
            "error"
        );
        assert_eq!(
            parse_line(
                "2026-11-09T12:00:00.123456Z 0 [Note] [MY-011953] [InnoDB] Page cleaner took 4ms"
            )
            .0,
            "debug"
        );
        // ANSI colors: read without them, shown with them.
        let ghost = "[2026-11-10 06:47:15] \x1b[36mINFO\x1b[39m \"GET /ghost/api/admin/labels/?limit=100&page=1\" \x1b[32m200\x1b[39m 36ms";
        assert_eq!(parse_line(ghost), ("info".into(), ghost.into(), None));
        assert_eq!(parse_line("\x1b[31mERROR\x1b[0m disk full").0, "error");
        assert_eq!(
            parse_line(
                "2026-11-09 12:00:00.000 UTC [1] \x1b[33mWARNING\x1b[0m server login failed"
            ),
            (
                "warn".into(),
                "2026-11-09 12:00:00.000 UTC [1] \x1b[33mWARNING\x1b[0m server login failed".into(),
                None
            )
        );
        assert_eq!(
            parse_line("1:M 09 Nov 2026 12:00:00.000 \x1b[1m#\x1b[22m WARNING overcommit_memory").0,
            "warn"
        );
        let (l, m, raw) =
            parse_line("\x1b[2K{\"level\":\"error\",\"msg\":\"\x1b[31mboom\x1b[0m\"}");
        assert_eq!((l.as_str(), m.as_str()), ("error", "boom"));
        assert!(raw.is_some());
        assert_eq!(
            plain_event("Normal", "ReplicaRebuilt", ""),
            (
                "Warning".into(),
                "Replica rebuilt".into(),
                "A replica was rebuilt to rejoin.".into()
            )
        );
        assert_eq!(plain_event("Normal", "Failover", "x").1, "Failover");
    }

    #[test]
    fn roles() {
        let status = json!({ "members": [{ "name": "main-2", "role": "primary" }, { "name": "main-1", "role": "replica" }], "readReplicas": { "members": [{ "name": "main-read-1" }] } });
        let none = json!({});
        assert_eq!(role_of("main-2", "main", &status, &none), "primary");
        assert_eq!(role_of("main-1", "main", &status, &none), "replica");
        assert_eq!(role_of("main-read-1", "main", &status, &none), "read");
        assert_eq!(
            role_of("main-pooler-7c9-abc", "main", &status, &none),
            "pooler"
        );
        assert_eq!(
            role_of("main-read-pooler-7c9-abc", "main", &status, &none),
            "read-pooler"
        );
        assert_eq!(
            role_of(
                "x-0",
                "kv",
                &none,
                &json!({ "pyo.dev/component": "sentinel" })
            ),
            "sentinel"
        );
        assert_eq!(role_of("kv-rest-5d-x", "kv", &none, &none), "proxy");
        assert_eq!(role_of("kv-kv-7f9-x", "kv", &none, &none), "proxy");
        assert_eq!(
            role_of(
                "main-import-ab12cd-x7k",
                "main",
                &none,
                &json!({ "pyo.dev/component": "import" })
            ),
            "import"
        );
        assert_eq!(
            role_of(
                "main-1-import-x7k",
                "main",
                &none,
                &json!({ "pyo.dev/component": "postgres" })
            ),
            "import"
        );
        assert_eq!(
            role_of("x-1", "kv", &none, &json!({ "pyo.dev/component": "kv" })),
            "proxy"
        );
        // MySQL: from the status, else its engine's role label.
        let my = json!({ "members": [{ "name": "shop-1", "role": "primary" }, { "name": "shop-0", "role": "replica" }] });
        assert_eq!(role_of("shop-1", "shop", &my, &none), "primary");
        assert_eq!(role_of("shop-0", "shop", &my, &none), "replica");
        assert_eq!(
            role_of(
                "shop-2",
                "shop",
                &none,
                &json!({ "moco.cybozu.com/role": "replica" })
            ),
            "replica"
        );
        assert_eq!(
            role_of(
                "backup-shop-29x-abc",
                "shop",
                &none,
                &json!({ "pyo.dev/component": "backup" })
            ),
            "backup"
        );
    }

    #[test]
    fn changes() {
        let old = json!({ "phase": "Ready", "primary": "main-1", "failovers": 0, "members": [{ "name": "main-1", "ready": true }, { "name": "main-2", "ready": true }] });
        let new = json!({ "phase": "Degraded", "primary": "main-2", "failovers": 1, "members": [{ "name": "main-1", "ready": false }, { "name": "main-2", "ready": true }] });
        let t = transitions(&old, &new);
        let reasons: Vec<&str> = t.iter().map(|x| x.1).collect();
        assert_eq!(reasons, ["Failover", "Status", "Not ready"]);
        assert!(
            transitions(&json!({}), &new).is_empty(),
            "the first status isn't a change"
        );
        assert!(transitions(&old, &old).is_empty());
    }
}
