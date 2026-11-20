//! Following a service's logs (`pyo logs -f`, the dashboard's live logs)
//! or a database's: one signed command, then new lines pushed over a
//! WebSocket this agent dials to Pyo, instead of a command every few
//! seconds.
//!
//! First the recent lines, as `fetch_logs` (or `fetch_database_logs`)
//! reads them. Then each replica's output as Kubernetes streams it, from
//! just after its newest line sent. Replicas that start later (a rollout,
//! a failover) are followed from their start. A database's follow also
//! sends its pods' parts and its events when they change. It ends when Pyo
//! closes the socket, or after [`LOG_FOLLOW_SECS`].

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
/// How often a database's events are read again (in listings).
const EVENTS_EVERY: u32 = 3;
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

type Socket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// What a follower hands the socket: a line, or another frame (a
/// database's pods or events), in order.
enum Out {
    Line(LogLine),
    Frame(LogFrame),
}

/// How a pod's output lines are read: a service's or a database's.
type Reader = fn(&str, &str) -> LogLine;

fn frame(f: &LogFrame) -> Message {
    Message::text(serde_json::to_string(f).unwrap_or_default())
}

/// The follow's socket to Pyo, or none (and why, logged).
async fn dial(agent: &Agent, session_id: &str) -> Option<Socket> {
    match agent
        .api
        .socket(&format!("/log-follows/{session_id}"))
        .await
    {
        Ok(ws) => Some(ws),
        Err(e) => {
            tracing::warn!(%session_id, error = %e, "log follow: couldn't reach the API");
            None
        }
    }
}

/// It can't follow: says why and closes.
async fn refuse(mut ws: Socket, message: String) {
    let _ = ws.send(frame(&LogFrame::Error { message })).await;
    let _ = ws.close(None).await;
}

pub async fn run(agent: Agent, session_id: String, service: ServiceRef, start: Start) {
    let Some(ws) = dial(&agent, &session_id).await else {
        return;
    };
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
        Err(message) => return refuse(ws, message).await,
    };
    let lines = std::mem::take(&mut recent.lines);
    let (out, rx) = mpsc::channel::<Out>(4096);
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
        tokio::spawn(replicas(
            agent.clone(),
            service.clone(),
            start.replica,
            recent,
            floor(started, start.since_seconds),
            out,
        ))
    };
    pump(ws, vec![LogFrame::Lines { lines }], rx).await;
    follower.abort();
    tracing::debug!(%session_id, service = %service.name, "log follow ended");
}

/// Follows database `name` in namespace `ns`: its pods' parts, recent
/// lines and events first, then as they change.
pub async fn run_database(
    agent: Agent,
    session_id: String,
    ns: String,
    name: String,
    start: Start,
) {
    let Some(ws) = dial(&agent, &session_id).await else {
        return;
    };
    let started = pyo_proto::now_ms();
    let r = crate::databases::logs(
        &agent.kube,
        &ns,
        &name,
        start.tail_lines,
        start.since_seconds,
    )
    .await;
    if !r.ok {
        let message = r
            .message
            .unwrap_or_else(|| "Couldn't read the database's logs.".into());
        return refuse(ws, message).await;
    }
    let data = r.data.unwrap_or_default();
    let pods = data["pods"].clone();
    let events: Vec<serde_json::Value> =
        serde_json::from_value(data["events"].clone()).unwrap_or_default();
    let mut newest: HashMap<String, i64> = HashMap::new();
    for l in &r.logs {
        let n = newest.entry(l.source.clone()).or_insert(l.at);
        *n = (*n).max(l.at);
    }
    // Pods whose lines were all older than the ones sent: from now.
    for pod in pods.as_object().into_iter().flatten().map(|(k, _)| k) {
        newest.entry(pod.clone()).or_insert(started);
    }
    let (out, rx) = mpsc::channel::<Out>(4096);
    let follower = tokio::spawn(database(
        agent.clone(),
        ns,
        name.clone(),
        Seen {
            pods: pods.clone(),
            events: events.clone(),
            newest,
            floor: floor(started, start.since_seconds),
        },
        out,
    ));
    let first = vec![
        LogFrame::Pods { pods },
        LogFrame::Lines { lines: r.logs },
        LogFrame::Events { events },
    ];
    pump(ws, first, rx).await;
    follower.abort();
    tracing::debug!(%session_id, database = %name, "log follow ended");
}

/// Where replicas read without output start: the start of `since`, or of
/// their output.
fn floor(started: i64, since_seconds: Option<i64>) -> i64 {
    since_seconds
        .filter(|s| *s > 0)
        .map_or(0, |s| started - s * 1000)
}

/// Sends `first`, then what the follower hands over, until Pyo closes the
/// socket, the follower stops, or [`LOG_FOLLOW_SECS`] pass.
async fn pump(ws: Socket, first: Vec<LogFrame>, mut rx: mpsc::Receiver<Out>) {
    let (mut tx, mut inbound) = ws.split();
    let mut open = true;
    for f in &first {
        if tx.send(frame(f)).await.is_err() {
            open = false;
            break;
        }
    }
    if open {
        let deadline = tokio::time::sleep(Duration::from_secs(LOG_FOLLOW_SECS));
        tokio::pin!(deadline);
        let mut buf = Vec::with_capacity(FRAME_LINES);
        'follow: loop {
            tokio::select! {
                _ = &mut deadline => break,
                m = inbound.next() => match m {
                    Some(Ok(Message::Close(_)) | Err(_)) | None => break,
                    Some(Ok(_)) => {}
                },
                n = rx.recv_many(&mut buf, FRAME_LINES) => {
                    if n == 0 {
                        break;
                    }
                    for f in batch(std::mem::take(&mut buf)) {
                        if tx.send(frame(&f)).await.is_err() {
                            break 'follow;
                        }
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
    let _ = tx.close().await;
}

/// Lines next to each other go in one frame. Other frames keep their place.
fn batch(items: Vec<Out>) -> Vec<LogFrame> {
    let mut frames = vec![];
    let mut lines = vec![];
    for item in items {
        match item {
            Out::Line(l) => lines.push(l),
            Out::Frame(f) => {
                if !lines.is_empty() {
                    frames.push(LogFrame::Lines {
                        lines: std::mem::take(&mut lines),
                    });
                }
                frames.push(f);
            }
        }
    }
    if !lines.is_empty() {
        frames.push(LogFrame::Lines { lines });
    }
    frames
}

/// Follows each replica's output, and new replicas as they come, until
/// `out` closes.
async fn replicas(
    agent: Agent,
    service: ServiceRef,
    replica: String,
    recent: RecentLogs,
    floor: i64,
    out: mpsc::Sender<Out>,
) {
    let pods = Api::<Pod>::namespaced(agent.kube.clone(), &service.namespace);
    let labels = format!("{SERVICE_LABEL}={}", service.service_id);
    // Each replica's newest line sent: its stream goes on after it.
    let mut newest: HashMap<String, i64> = recent
        .pods
        .iter()
        .map(|p| (p.clone(), recent.newest.get(p).copied().unwrap_or(floor)))
        .collect();
    let mut streams = Streams::default();
    loop {
        streams.ended(&mut newest);
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
                    streams.follow(&pods, name, None, &newest, &out, crate::actions::log_line);
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

/// What a database's follow sent last.
struct Seen {
    pods: serde_json::Value,
    events: Vec<serde_json::Value>,
    /// Each pod's newest line sent.
    newest: HashMap<String, i64>,
    /// Where pods read without output start.
    floor: i64,
}

/// Follows a database's pods (new ones too: a failover, a new replica),
/// and sends its pods' parts and its events again when they change.
async fn database(agent: Agent, ns: String, name: String, mut seen: Seen, out: mpsc::Sender<Out>) {
    let pods = Api::<Pod>::namespaced(agent.kube.clone(), &ns);
    let floor = seen.floor;
    let mut newest: HashMap<String, i64> = std::mem::take(&mut seen.newest);
    let mut streams = Streams::default();
    let mut round = 0u32;
    loop {
        streams.ended(&mut newest);
        match crate::databases::member_pods(&pods, &name).await {
            Ok(list) => {
                let info = crate::databases::pods_info(&list);
                if info != seen.pods {
                    seen.pods = info.clone();
                    if out
                        .send(Out::Frame(LogFrame::Pods { pods: info }))
                        .await
                        .is_err()
                    {
                        return;
                    }
                }
                let names: HashSet<String> = list.iter().map(|p| p.name_any()).collect();
                newest.retain(|n, _| names.contains(n));
                for p in &list {
                    let pod = p.name_any();
                    // A new pod: from `floor`.
                    newest.entry(pod.clone()).or_insert(floor);
                    streams.follow(
                        &pods,
                        pod,
                        crate::databases::main_container(p),
                        &newest,
                        &out,
                        crate::databases::log_line,
                    );
                }
                round += 1;
                if round.is_multiple_of(EVENTS_EVERY) {
                    let events = crate::databases::events(&agent.kube, &ns, &name, &names).await;
                    if events != seen.events {
                        seen.events = events.clone();
                        if out
                            .send(Out::Frame(LogFrame::Events { events }))
                            .await
                            .is_err()
                        {
                            return;
                        }
                    }
                }
            }
            Err(e) => tracing::debug!(error = %e, "log follow: couldn't list the database's pods"),
        }
        tokio::select! {
            _ = out.closed() => return,
            _ = tokio::time::sleep(RELIST) => {}
        }
    }
}

/// The pods followed now, each by its own stream.
#[derive(Default)]
struct Streams {
    following: HashSet<String>,
    tasks: JoinSet<(String, i64)>,
}

impl Streams {
    /// Ended (the container restarted, or the pod went): its newest line,
    /// so a restart goes on from there.
    fn ended(&mut self, newest: &mut HashMap<String, i64>) {
        while let Some(r) = self.tasks.try_join_next() {
            if let Ok((name, at)) = r {
                self.following.remove(&name);
                newest.insert(name, at);
            }
        }
    }

    /// Follows pod `name` unless it already is: after its newest line
    /// sent, or from its start.
    fn follow(
        &mut self,
        pods: &Api<Pod>,
        name: String,
        container: Option<String>,
        newest: &HashMap<String, i64>,
        out: &mpsc::Sender<Out>,
        read: Reader,
    ) {
        if self.following.insert(name.clone()) {
            let after = newest.get(&name).copied().unwrap_or(0);
            self.tasks.spawn(stream(
                pods.clone(),
                name,
                container,
                after,
                out.clone(),
                read,
            ));
        }
    }
}

/// Which lines a stream starting after `after` (milliseconds) asks for:
/// from that second on (the lines in it up to `after` are dropped), or a
/// new replica's output from its start.
fn stream_params(after: i64, container: Option<String>) -> LogParams {
    let since = (after > 0)
        .then(|| k8s_openapi::jiff::Timestamp::from_second(after.div_euclid(1000)).ok())
        .flatten();
    LogParams {
        follow: true,
        timestamps: true,
        tail_lines: since.is_none().then_some(NEW_REPLICA_LINES),
        since_time: since,
        container,
        ..Default::default()
    }
}

/// One pod's output after `after`, until it ends. Gives back the pod's
/// name and its newest line's time.
async fn stream(
    pods: Api<Pod>,
    name: String,
    container: Option<String>,
    after: i64,
    out: mpsc::Sender<Out>,
    read: Reader,
) -> (String, i64) {
    let mut newest = after;
    match pods
        .log_stream(&name, &stream_params(after, container))
        .await
    {
        Ok(reader) => {
            let mut lines = reader.lines();
            while let Some(Ok(l)) = lines.next().await {
                let line = read(&name, &l);
                if line.at <= after {
                    continue;
                }
                newest = newest.max(line.at);
                if out.send(Out::Line(line)).await.is_err() {
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
    out: mpsc::Sender<Out>,
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
            if out.send(Out::Line(l)).await.is_err() {
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
        let p = stream_params(1_700_000_000_750, None);
        assert!(p.follow && p.timestamps);
        assert_eq!(
            p.since_time.map(|t| t.as_second()),
            Some(1_700_000_000),
            "the whole second, so no line after it is missed"
        );
        assert_eq!(p.tail_lines, None);
        let new = stream_params(0, Some("postgres".into()));
        assert_eq!(new.container.as_deref(), Some("postgres"));
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
