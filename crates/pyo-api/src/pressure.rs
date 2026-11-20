//! A server low on disk space or memory, in the activity: its kubelet's
//! DiskPressure and MemoryPressure conditions, as each heartbeat reports
//! them (per cluster, from the agent there). When they hold, the kubelet
//! evicts pods, and their controllers start new ones it may evict again.
//!
//! One entry when it starts and one when it ends. One that starts again
//! within [`WINDOW_SECS`] of ending isn't logged until the window is over,
//! and only if it still holds, so a server going in and out of pressure
//! logs (and alerts) once per window, like alerts.rs does.

use pyo_proto::ServerCondition;
use serde_json::{Value, json};
use sqlx::{Postgres, Transaction};

use crate::{alerts::WINDOW_SECS, error::ApiResult, ops::log_activity};

pub const STARTED: &str = "server.pressure";
pub const ENDED: &str = "server.pressure_ended";

/// The conditions, and what each says the server is short of.
const KINDS: [(&str, &str); 2] = [("DiskPressure", "disk space"), ("MemoryPressure", "memory")];

/// What the server is short of, from its conditions: "disk space", "memory".
pub fn short_of(conditions: &[ServerCondition]) -> Vec<&'static str> {
    KINDS
        .iter()
        .filter(|(kind, _)| {
            conditions
                .iter()
                .any(|c| c.kind == *kind && c.status == "True")
        })
        .map(|(_, what)| *what)
        .collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    Start,
    End,
}

/// What to log, from whether the server is short now and its last pressure
/// entry (its type, and seconds since).
pub fn step(short: bool, last: Option<(&str, i64)>) -> Option<Step> {
    match (short, last) {
        (true, None) => Some(Step::Start),
        (true, Some((kind, age))) if kind == ENDED && age >= WINDOW_SECS => Some(Step::Start),
        // Logged already, or ended within the window: later, if it holds.
        (true, _) => None,
        (false, Some((kind, _))) if kind == STARTED => Some(Step::End),
        (false, _) => None,
    }
}

/// "web-1 is low on disk space", "web-1 has enough memory again".
pub fn title(server: &str, what: &[&str], step: Step) -> String {
    let what = if what.is_empty() {
        "disk space and memory".to_owned()
    } else {
        what.join(" and ")
    };
    match step {
        Step::Start => format!("{server} is low on {what}"),
        Step::End => format!("{server} has enough {what} again"),
    }
}

/// Logs the servers that started or stopped being short, from what they
/// report now and their last report (`before`: name, status, conditions).
pub async fn record(
    tx: &mut Transaction<'_, Postgres>,
    team_id: &str,
    cluster_id: &str,
    servers: &[pyo_proto::ServerReport],
    before: &[(String, String, Value)],
) -> ApiResult<()> {
    for n in servers {
        let now = short_of(&n.conditions);
        let last_report = before.iter().find(|(name, _, _)| *name == n.name);
        let was: Vec<&str> = last_report
            .and_then(|(_, _, c)| serde_json::from_value::<Vec<ServerCondition>>(c.clone()).ok())
            .map(|c| short_of(&c))
            .unwrap_or_default();
        let was_ready = last_report.is_some_and(|(_, status, _)| status == "ready");
        if now.is_empty() {
            // One that stopped reporting (its conditions unknown) isn't
            // fine yet. One that's fine, and was (and was reporting then),
            // has nothing to end.
            if n.status != "ready" || (was.is_empty() && was_ready) {
                continue;
            }
        }
        let last: Option<(String, i64, Value)> = sqlx::query_as(
            "SELECT data->>'type', extract(epoch FROM now() - at)::bigint, coalesce(data->'shortOf', '[]'::jsonb) FROM pyo.activity
              WHERE team_id = $1 AND data->>'type' IN ($2, $3)
                AND data->>'clusterId' = $4 AND data->'target'->>'id' = $5
              ORDER BY at DESC LIMIT 1",
        )
        .bind(team_id)
        .bind(STARTED)
        .bind(ENDED)
        .bind(cluster_id)
        .bind(&n.name)
        .fetch_optional(&mut **tx)
        .await?;
        let Some(s) = step(
            !now.is_empty(),
            last.as_ref().map(|(k, a, _)| (k.as_str(), *a)),
        ) else {
            continue;
        };
        // What ended: what it was short of when that was logged.
        let started_with: Vec<String> = last
            .and_then(|(_, _, w)| serde_json::from_value(w).ok())
            .unwrap_or_default();
        let (kind, status, what): (_, _, Vec<&str>) = match s {
            Step::Start => (STARTED, "failure", now),
            Step::End if !started_with.is_empty() => (
                ENDED,
                "success",
                started_with.iter().map(String::as_str).collect(),
            ),
            Step::End => (ENDED, "success", was),
        };
        log_activity(
            tx,
            team_id,
            "pyo",
            kind,
            status,
            &title(&n.name, &what, s),
            json!({ "kind": "server", "id": n.name, "label": n.name }),
            json!({ "clusterId": cluster_id, "shortOf": what }),
        )
        .await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(kind: &str, status: &str) -> ServerCondition {
        ServerCondition {
            kind: kind.into(),
            status: status.into(),
            message: String::new(),
        }
    }

    #[test]
    fn short_of_disk_or_memory() {
        let ok = [
            c("Ready", "True"),
            c("DiskPressure", "False"),
            c("MemoryPressure", "False"),
            c("PIDPressure", "False"),
        ];
        assert!(short_of(&ok).is_empty());
        assert_eq!(
            short_of(&[c("Ready", "True"), c("DiskPressure", "True")]),
            ["disk space"]
        );
        assert_eq!(
            short_of(&[c("MemoryPressure", "True"), c("DiskPressure", "True")]),
            ["disk space", "memory"]
        );
        // Unknown (a server that stopped reporting) isn't pressure.
        assert!(short_of(&[c("DiskPressure", "Unknown")]).is_empty());
    }

    #[test]
    fn once_when_it_starts_and_once_when_it_ends() {
        assert_eq!(step(true, None), Some(Step::Start));
        assert_eq!(step(true, Some((STARTED, 5))), None);
        assert_eq!(step(true, Some((STARTED, 99_999))), None);
        assert_eq!(step(false, Some((STARTED, 60))), Some(Step::End));
        assert_eq!(step(false, Some((ENDED, 60))), None);
        assert_eq!(step(false, None), None);
    }

    #[test]
    fn starting_again_waits_out_the_window() {
        assert_eq!(step(true, Some((ENDED, 60))), None);
        assert_eq!(step(true, Some((ENDED, WINDOW_SECS - 1))), None);
        assert_eq!(step(true, Some((ENDED, WINDOW_SECS))), Some(Step::Start));
    }

    /// Heartbeats a minute apart, short or not: what gets logged, and when.
    fn simulate(short: &[bool]) -> Vec<(usize, Step)> {
        let mut last: Option<(&str, i64)> = None;
        let mut out = vec![];
        for (m, &s) in short.iter().enumerate() {
            let at = m as i64 * 60;
            if let Some(st) = step(s, last.map(|(k, t)| (k, at - t))) {
                out.push((m, st));
                last = Some((if st == Step::Start { STARTED } else { ENDED }, at));
            }
        }
        out
    }

    #[test]
    fn a_server_in_and_out_of_pressure_logs_once_per_window() {
        // In and out every minute for 40 minutes.
        let flaps: Vec<bool> = (0..40).map(|m| m % 2 == 0).collect();
        assert_eq!(
            simulate(&flaps),
            [
                (0, Step::Start),
                (1, Step::End),
                (16, Step::Start),
                (17, Step::End),
                (32, Step::Start),
                (33, Step::End)
            ]
        );
        // Short for good: one entry.
        assert_eq!(simulate(&[true; 30]), [(0, Step::Start)]);
        // Ended, then short again within the window and staying so: logged
        // once the window is over.
        let mut v = vec![true, false];
        v.extend([true; 20]);
        assert_eq!(
            simulate(&v),
            [(0, Step::Start), (1, Step::End), (16, Step::Start)]
        );
    }

    #[test]
    fn titles_are_plain() {
        assert_eq!(
            title("s1", &["disk space"], Step::Start),
            "s1 is low on disk space"
        );
        assert_eq!(
            title("s1", &["disk space", "memory"], Step::End),
            "s1 has enough disk space and memory again"
        );
    }
}
