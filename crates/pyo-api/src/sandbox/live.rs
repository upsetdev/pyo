//! How far a template got on the servers making it, asked of them as the
//! templates list is read. The runner reads every server's report each
//! [`super::runner::TICK`], too slow for a progress bar. While a server
//! builds a template, the list asks it with `Template`, which answers from
//! memory, and lays the answer over what the last report said. Nothing is
//! saved: the next round saves the report as before.
//!
//! - Answers are kept for [`FRESH`], so many pages open at once ask each
//!   server once a second at most.
//! - A server that doesn't answer in [`WAIT`], or a daemon too old for the
//!   command, keeps what its last report said, and isn't asked again for
//!   [`BACKOFF`].

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use futures::StreamExt;
use pyo_proto::sandbox::{DaemonCommand, DaemonResult, TemplateReport, TemplateState};
use serde_json::{Value, json};

use super::{daemon::Daemon, keys};
use crate::AppState;

const FRESH: Duration = Duration::from_millis(900);
const WAIT: Duration = Duration::from_millis(1500);
const BACKOFF: Duration = Duration::from_secs(15);

/// Server id and template key: when asked, and what it said.
type Cache = HashMap<(String, String), (Instant, Option<TemplateReport>)>;
static CACHE: LazyLock<Mutex<Cache>> = LazyLock::new(Default::default);

/// Server ids and template keys still building in `rows`.
fn building(rows: &[Value]) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = rows
        .iter()
        .flat_map(|r| r["servers"].as_array().into_iter().flatten())
        .filter(|s| s["state"] == "building")
        .filter_map(|s| Some((s["id"].as_str()?.to_owned(), s["key"].as_str()?.to_owned())))
        .collect();
    out.sort();
    out.dedup();
    out
}

/// Lays what the servers say now over `rows`.
pub async fn overlay(s: &AppState, rows: &mut [Value]) {
    let wanted = building(rows);
    if wanted.is_empty() {
        return;
    }
    let now = Instant::now();
    let stale: Vec<(String, String)> = {
        let cache = CACHE.lock().unwrap_or_else(|p| p.into_inner());
        wanted
            .iter()
            .filter(|k| match cache.get(*k) {
                Some((at, Some(_))) => now.duration_since(*at) >= FRESH,
                Some((at, None)) => now.duration_since(*at) >= BACKOFF,
                None => true,
            })
            .cloned()
            .collect()
    };
    if !stale.is_empty() {
        ask(s, stale).await;
    }
    let cache = CACHE.lock().unwrap_or_else(|p| p.into_inner());
    let answers: HashMap<&(String, String), &TemplateReport> = wanted
        .iter()
        .filter_map(|k| Some((k, cache.get(k)?.1.as_ref()?)))
        .collect();
    for r in rows.iter_mut() {
        for v in r["servers"].as_array_mut().into_iter().flatten() {
            let k = (
                v["id"].as_str().unwrap_or_default().to_owned(),
                v["key"].as_str().unwrap_or_default().to_owned(),
            );
            if let Some(t) = answers.get(&k) {
                merge(v, t);
            }
        }
        roll_up(r);
    }
}

/// Asks each server how far its templates got, and keeps the answers.
async fn ask(s: &AppState, keys_: Vec<(String, String)>) {
    let ids: Vec<String> = keys_.iter().map(|(id, _)| id.clone()).collect();
    let addresses: HashMap<String, String> = match sqlx::query_as::<_, (String, String)>(
        "SELECT id, address FROM pyo.sandbox_servers WHERE id = ANY($1) AND address IS NOT NULL",
    )
    .bind(&ids)
    .fetch_all(&s.db)
    .await
    {
        Ok(v) => v.into_iter().collect(),
        Err(e) => {
            tracing::debug!(error = %e, "sandbox templates: couldn't find the servers to ask");
            return;
        }
    };
    let Ok(key) = keys::signing_key(&s.db, &s.sealer).await else {
        return;
    };
    let answers: Vec<((String, String), Option<TemplateReport>)> = futures::stream::iter(keys_)
        .map(|(id, name)| {
            let key = key.clone();
            let address = addresses.get(&id).cloned();
            async move {
                let answer = match address {
                    Some(a) => {
                        let token = keys::admin_token(&key, &id);
                        let cmd = DaemonCommand::Template {
                            template: name.clone(),
                        };
                        match tokio::time::timeout(WAIT, Daemon::new(&id, &a).command(&token, &cmd))
                            .await
                        {
                            Ok(Ok(DaemonResult::Template { template })) => Some(template),
                            _ => None,
                        }
                    }
                    None => None,
                };
                ((id, name), answer)
            }
        })
        .buffer_unordered(8)
        .collect()
        .await;
    let now = Instant::now();
    let mut cache = CACHE.lock().unwrap_or_else(|p| p.into_inner());
    // Answers long past use go.
    cache.retain(|_, (at, _)| now.duration_since(*at) < BACKOFF * 4);
    for (k, a) in answers {
        cache.insert(k, (now, a));
    }
}

fn state_name(s: TemplateState) -> &'static str {
    match s {
        TemplateState::Building => "building",
        TemplateState::Ready => "ready",
        TemplateState::Failed => "failed",
    }
}

/// One server's view of a template, as the server says it now.
fn merge(v: &mut Value, t: &TemplateReport) {
    v["state"] = json!(state_name(t.state));
    if t.state == TemplateState::Ready {
        v["bytes"] = json!(t.bytes);
        v["snapshots"] = json!(t.snapshots);
    }
    if let Some(b) = &t.build {
        v["phase"] = json!(b.phase);
        v["pulled"] = json!(b.pulled);
        v["unpacked"] = json!(b.unpacked);
        v["imageBytes"] = json!(b.image_bytes);
        v["pullMs"] = json!(b.pull_ms);
        v["unpackMs"] = json!(b.unpack_ms);
        v["snapshotMs"] = json!(b.snapshot_ms);
        v["error"] = json!(b.error);
    }
}

/// A template's state from its servers' as they are now, until the next
/// round saves it: ready once one has it, building while one makes it.
/// Pyo's own and failed ones stay as they are.
fn roll_up(r: &mut Value) {
    if r["builtin"] == true || !(r["state"] == "pending" || r["state"] == "building") {
        return;
    }
    let servers = r["servers"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default();
    let any = |st: &str| servers.iter().any(|s| s["state"] == st);
    if any("ready") {
        r["state"] = json!("ready");
        r["error"] = Value::Null;
    } else if any("building") {
        r["state"] = json!("building");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pyo_proto::sandbox::TemplateBuild;

    fn row(state: &str, servers: Value) -> Value {
        json!({ "id": "stp_a", "builtin": false, "state": state, "error": null, "servers": servers })
    }

    #[test]
    fn only_building_servers_are_asked() {
        let rows = vec![
            row(
                "building",
                json!([
                    { "id": "sbs_1", "key": "stp_a", "state": "building" },
                    { "id": "sbs_2", "key": "stp_a", "state": "ready" },
                    { "id": "sbs_3", "key": null, "state": "pending" },
                ]),
            ),
            row(
                "building",
                json!([{ "id": "sbs_1", "key": "stp_a", "state": "building" }]),
            ),
        ];
        assert_eq!(building(&rows), vec![("sbs_1".into(), "stp_a".into())]);
    }

    #[test]
    fn an_answer_replaces_the_last_report() {
        let mut v = json!({ "id": "sbs_1", "state": "building", "phase": "pulling", "pulled": 10, "imageBytes": 100 });
        let t = TemplateReport {
            name: "stp_a".into(),
            state: TemplateState::Building,
            build: Some(TemplateBuild {
                phase: Some("unpacking".into()),
                pulled: 100,
                unpacked: 40,
                image_bytes: 100,
                ..Default::default()
            }),
            ..Default::default()
        };
        merge(&mut v, &t);
        assert_eq!(v["phase"], "unpacking");
        assert_eq!(v["unpacked"], 40);
        assert_eq!(v["pulled"], 100);

        let done = TemplateReport {
            state: TemplateState::Ready,
            bytes: 5,
            snapshots: vec!["k".into()],
            build: Some(TemplateBuild::default()),
            ..t
        };
        merge(&mut v, &done);
        assert_eq!(v["state"], "ready");
        assert_eq!(v["phase"], Value::Null);
        assert_eq!(v["bytes"], 5);
    }

    #[test]
    fn the_template_follows_its_servers() {
        let mut r = row(
            "pending",
            json!([{ "state": "building" }, { "state": "pending" }]),
        );
        roll_up(&mut r);
        assert_eq!(r["state"], "building");
        let mut r = row(
            "building",
            json!([{ "state": "building" }, { "state": "ready" }]),
        );
        roll_up(&mut r);
        assert_eq!(r["state"], "ready");
        let mut r = row("failed", json!([{ "state": "ready" }]));
        roll_up(&mut r);
        assert_eq!(r["state"], "failed");
        let mut r = row("ready", json!([{ "state": "building" }]));
        r["builtin"] = json!(true);
        roll_up(&mut r);
        assert_eq!(r["state"], "ready");
    }
}
