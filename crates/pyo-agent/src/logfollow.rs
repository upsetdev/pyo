//! Following a service's logs (`pyo logs -f`, the dashboard's live logs):
//! one signed command, then new lines pushed over a WebSocket this agent
//! dials to Pyo, instead of a command every few seconds.
//!
//! First the recent lines, as `fetch_logs` reads them. Then each replica's
//! output as Kubernetes streams it, from just after its newest line sent.
//! Replicas that start later (a rollout) are followed from their start. It
//! ends when Pyo closes the socket, or after [`LOG_FOLLOW_SECS`].

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use futures::{AsyncBufReadExt, SinkExt, StreamExt};
use k8s_openapi::api::core::v1::Pod;
use kube::{
    Api, ResourceExt,
    api::{ListParams, LogParams},
};
use pyo_proto::{LOG_FOLLOW_SECS, LogFrame, LogLine, ServiceRef};
use tokio::{sync::mpsc, task::JoinSet};
use tokio_tungstenite::tungstenite::Message;

use crate::{Agent, actions::RecentLogs, observe::SERVICE_LABEL};

/// How often the replicas are listed again, for new ones.
const RELIST: Duration = Duration::from_secs(5);
/// How often a static site's requests are read again.
const STATIC_POLL: Duration = Duration::from_secs(2);
/// An empty frame this long after the last one, so proxies keep the
/// connection open while nothing is printed.
const KEEPALIVE: Duration = Duration::from_secs(25);
/// The most lines in one frame.
const FRAME_LINES: usize = 500;
/// The most lines of a new replica's output from before it was followed.
const NEW_REPLICA_LINES: i64 = 2000;

/// What to send first.
pub struct Start {
    pub tail_lines: i64,
    pub since_seconds: Option<i64>,
    pub replica: String,
}

fn frame(f: &LogFrame) -> Message {
    Message::text(serde_json::to_string(f).unwrap_or_default())
}

pub async fn run(agent: Agent, session_id: String, service: ServiceRef, start: Start) {
    let ws = match agent
        .api
        .socket(&format!("/log-follows/{session_id}"))
        .await
    {
        Ok(ws) => ws,
        Err(e) => {
            return tracing::warn!(%session_id, error = %e, "log follow: couldn't reach the API");
        }
    };
    let (mut tx, mut rx) = ws.split();
    let started = pyo_proto::now_ms();
    let mut recent = match crate::actions::read_logs(
        &agent,
        &service,
        start.tail_lines,
        start.since_seconds,
        &start.replica,
    )
    .await
    {
        Ok(r) => r,
        Err(message) => {
            let _ = tx.send(frame(&LogFrame::Error { message })).await;
            let _ = tx.close().await;
            return;
        }
    };
    let lines = std::mem::take(&mut recent.lines);
    let (out, mut lines_rx) = mpsc::channel::<LogLine>(4096);
    let follower = if recent.static_site {
        let newest = lines.iter().map(|l| l.at).max().unwrap_or(started);
        let seen = lines
            .iter()
            .filter(|l| l.at == newest)
            .map(line_key)
            .collect();
        tokio::spawn(static_site(
            agent.clone(),
            service.service_id.clone(),
            newest,
            seen,
            out,
        ))
    } else {
        // Replicas read without output: from the start of `since`, or of
        // their output.
        let floor = start
            .since_seconds
            .filter(|s| *s > 0)
            .map_or(0, |s| started - s * 1000);
        tokio::spawn(replicas(
            agent.clone(),
            service.clone(),
            start.replica,
            recent,
            floor,
            out,
        ))
    };
    if tx.send(frame(&LogFrame::Lines { lines })).await.is_ok() {
        let deadline = tokio::time::sleep(Duration::from_secs(LOG_FOLLOW_SECS));
        tokio::pin!(deadline);
        let mut buf = Vec::with_capacity(FRAME_LINES);
        loop {
            tokio::select! {
                _ = &mut deadline => break,
                m = rx.next() => match m {
                    Some(Ok(Message::Close(_)) | Err(_)) | None => break,
                    Some(Ok(_)) => {}
                },
                n = lines_rx.recv_many(&mut buf, FRAME_LINES) => {
                    if n == 0 {
                        break;
                    }
                    let lines = std::mem::take(&mut buf);
                    if tx.send(frame(&LogFrame::Lines { lines })).await.is_err() {
                        break;
                    }
                }
                _ = tokio::time::sleep(KEEPALIVE) => {
                    if tx.send(frame(&LogFrame::Lines { lines: vec![] })).await.is_err() {
                        break;
                    }
                }
            }
        }
    }
    follower.abort();
    let _ = tx.close().await;
    tracing::debug!(%session_id, service = %service.name, "log follow ended");
}

/// Follows each replica's output, and new replicas as they come, until
/// `out` closes.
async fn replicas(
    agent: Agent,
    service: ServiceRef,
    replica: String,
    recent: RecentLogs,
    floor: i64,
    out: mpsc::Sender<LogLine>,
) {
    let pods = Api::<Pod>::namespaced(agent.kube.clone(), &service.namespace);
    let labels = format!("{SERVICE_LABEL}={}", service.service_id);
    // Each replica's newest line sent: its stream goes on after it.
    let mut newest: HashMap<String, i64> = recent
        .pods
        .iter()
        .map(|p| (p.clone(), recent.newest.get(p).copied().unwrap_or(floor)))
        .collect();
    let mut following: HashSet<String> = HashSet::new();
    let mut streams: JoinSet<(String, i64)> = JoinSet::new();
    loop {
        // Ended (the container restarted, or the replica went): its
        // newest line, so a restart goes on from there.
        while let Some(r) = streams.try_join_next() {
            if let Ok((name, at)) = r {
                following.remove(&name);
                newest.insert(name, at);
            }
        }
        match pods.list(&ListParams::default().labels(&labels)).await {
            Ok(list) => {
                let names: HashSet<String> = list
                    .items
                    .iter()
                    .map(|p| p.name_any())
                    .filter(|n| replica.is_empty() || *n == replica)
                    .collect();
                newest.retain(|n, _| names.contains(n));
                for name in names {
                    if following.insert(name.clone()) {
                        let after = newest.get(&name).copied().unwrap_or(0);
                        streams.spawn(stream(pods.clone(), name, after, out.clone()));
                    }
                }
            }
            Err(e) => tracing::debug!(error = %e, "log follow: couldn't list replicas"),
        }
        tokio::select! {
            _ = out.closed() => return,
            _ = tokio::time::sleep(RELIST) => {}
        }
    }
}

/// Which lines a stream starting after `after` (milliseconds) asks for:
/// from that second on (the lines in it up to `after` are dropped), or a
/// new replica's output from its start.
fn stream_params(after: i64) -> LogParams {
    let since = (after > 0)
        .then(|| k8s_openapi::jiff::Timestamp::from_second(after.div_euclid(1000)).ok())
        .flatten();
    LogParams {
        follow: true,
        timestamps: true,
        tail_lines: since.is_none().then_some(NEW_REPLICA_LINES),
        since_time: since,
        ..Default::default()
    }
}

/// One replica's output after `after`, until it ends. Gives back the
/// replica's name and its newest line's time.
async fn stream(
    pods: Api<Pod>,
    name: String,
    after: i64,
    out: mpsc::Sender<LogLine>,
) -> (String, i64) {
    let mut newest = after;
    match pods.log_stream(&name, &stream_params(after)).await {
        Ok(reader) => {
            let mut lines = reader.lines();
            while let Some(Ok(l)) = lines.next().await {
                let line = crate::actions::log_line(&name, &l);
                if line.at <= after {
                    continue;
                }
                newest = newest.max(line.at);
                if out.send(line).await.is_err() {
                    break;
                }
            }
        }
        // Not started yet, or gone: tried again on the next listing.
        Err(e) => tracing::debug!(pod = %name, error = %e, "log follow: no stream"),
    }
    (name, newest)
}

fn line_key(l: &LogLine) -> String {
    format!("{}\u{0}{}\u{0}{}", l.at, l.source, l.message)
}

/// A static site's requests, read again every few seconds from pyo-static's
/// output (inside the cluster: no command each time).
async fn static_site(
    agent: Agent,
    service_id: String,
    mut newest: i64,
    mut seen: HashSet<String>,
    out: mpsc::Sender<LogLine>,
) {
    loop {
        tokio::select! {
            _ = out.closed() => return,
            _ = tokio::time::sleep(STATIC_POLL) => {}
        }
        let Ok(mut lines) = crate::sites::logs(&agent.kube, &service_id, 500).await else {
            continue;
        };
        lines.sort_by_key(|l| l.at);
        for l in fresh(&mut newest, &mut seen, lines) {
            if out.send(l).await.is_err() {
                return;
            }
        }
    }
}

/// The lines not sent yet: after `newest`, or at it and not in `seen`
/// (the keys of the lines sent at `newest`). Moves both along.
fn fresh(newest: &mut i64, seen: &mut HashSet<String>, lines: Vec<LogLine>) -> Vec<LogLine> {
    let mut out = vec![];
    for l in lines {
        if l.at < *newest {
            continue;
        }
        if l.at > *newest {
            *newest = l.at;
            seen.clear();
        }
        if seen.insert(line_key(&l)) {
            out.push(l);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(at: i64, message: &str) -> LogLine {
        LogLine {
            at,
            level: "info".into(),
            source: "pyo-static-1".into(),
            message: message.into(),
        }
    }

    #[test]
    fn a_replica_goes_on_from_the_second_of_its_newest_line() {
        let p = stream_params(1_700_000_000_750);
        assert!(p.follow && p.timestamps);
        assert_eq!(
            p.since_time.map(|t| t.as_second()),
            Some(1_700_000_000),
            "the whole second, so no line after it is missed"
        );
        assert_eq!(p.tail_lines, None);
        let new = stream_params(0);
        assert_eq!(new.since_time, None);
        assert_eq!(new.tail_lines, Some(NEW_REPLICA_LINES));
    }

    #[test]
    fn static_lines_are_sent_once() {
        let mut newest = 10;
        let mut seen: HashSet<String> = [line_key(&line(10, "a"))].into();
        let got = fresh(
            &mut newest,
            &mut seen,
            vec![line(9, "old"), line(10, "a"), line(10, "b"), line(12, "c")],
        );
        let words: Vec<&str> = got.iter().map(|l| l.message.as_str()).collect();
        assert_eq!(words, ["b", "c"]);
        assert_eq!(newest, 12);
        let again = fresh(&mut newest, &mut seen, vec![line(12, "c"), line(13, "d")]);
        assert_eq!(again.len(), 1);
        assert_eq!(again[0].message, "d");
    }
}
