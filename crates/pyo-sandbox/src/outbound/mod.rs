//! The outbound mode: a sandbox server on Pyo Cloud, which Pyo can't
//! reach. The daemon calls Pyo instead, over HTTPS
//! (docs/cloud-sandbox-servers.md):
//!
//! - It joins once with a one-time code ([`join`]) and keeps a key of its
//!   own ([`store`]). Every request after that is signed with it, with the
//!   agents' headers (pyo-proto `agentauth`).
//! - It long-polls for its commands. Each is signed by Pyo's signer for
//!   this server and its team, under the `pyosbx1` prefix. The daemon runs
//!   one only when it checks out against the keys from the join command,
//!   for its own server and team, with a sequence number past the last it
//!   took, and before it expires. It keeps that number on disk before it
//!   runs the command, so a command never runs twice. Anything else is
//!   refused and answered as `forbidden`.
//! - It runs commands through the daemon's own handlers (`api::run`), each
//!   in its own task, and posts each one's result.
//! - It posts its report every 15 seconds.
//! - It renews its key at half its life.
//! - When Pyo says its key is unknown, expired or revoked (Reset access,
//!   or the server was removed), it stops calling Pyo and logs that it
//!   needs a new join command. The sandboxes keep running.
//!
//! Browsers and SDKs on the tailnet reach the daemon's API over HTTPS on
//! its tailnet name ([`crate::tailnet`]).

pub mod http;
pub mod join;
pub mod store;
#[cfg(test)]
mod tests;

use std::sync::Arc;
use std::time::Duration;

use futures::future::BoxFuture;
use hyper::{Method, StatusCode};
use pyo_proto::agentauth::RequestKey;
use pyo_proto::sandbox::outbound::{self as wire, CommandEnvelope};
use pyo_proto::sandbox::{DaemonCommand, DaemonError, DaemonResult, ErrorCode, ServerReport};
use pyo_proto::signing::TrustedKeys;
use serde::{Deserialize, Serialize};
use tokio::sync::watch;

use self::http::Http;
use self::store::{KeyFile, State, Store};

/// The `outbound` part of the daemon's config, from Pyo's answer to the
/// join.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OutboundConfig {
    /// Where Pyo's channel is (`<pyoUrl>/api/sandbox-agent/v1`).
    pub pyo_url: String,
    pub team_id: String,
    /// The server's full name on the tailnet, like `sbx-1.tail1234.ts.net`.
    pub tailnet_name: String,
}

/// What commands ask for: the daemon's own handlers.
pub trait Work: Send + Sync + 'static {
    fn run(&self, cmd: DaemonCommand) -> BoxFuture<'_, DaemonResult>;
    fn report(&self) -> ServerReport;
}

impl Work for crate::api::AppState {
    fn run(&self, cmd: DaemonCommand) -> BoxFuture<'_, DaemonResult> {
        Box::pin(async move {
            match crate::api::run(self, crate::api::Caller::Pyo, cmd).await {
                Ok(r) => r,
                Err(crate::api::ApiError(error)) => DaemonResult::Error { error },
            }
        })
    }

    fn report(&self) -> ServerReport {
        self.mgr.report()
    }
}

/// How often things happen. Shorter in tests.
#[derive(Debug, Clone)]
pub struct Timing {
    /// Seconds Pyo holds a poll open when there's nothing to do.
    pub poll_wait_secs: u64,
    pub report_every: Duration,
    /// The longest wait between tries after a failure (from a second).
    pub backoff_max: Duration,
    pub backoff_min: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Timing {
            poll_wait_secs: 25,
            report_every: Duration::from_secs(15),
            backoff_max: Duration::from_secs(30),
            backoff_min: Duration::from_secs(1),
        }
    }
}

/// Why a request to Pyo didn't go through.
#[derive(Debug)]
enum Failed {
    /// Pyo doesn't take the server's key any more. Nothing more to do.
    Gone,
    Other(String),
}

/// The key in use.
struct Current {
    file: KeyFile,
    key: Arc<RequestKey>,
}

/// The outbound client of one daemon.
pub struct Client<W: Work> {
    http: Http,
    work: Arc<W>,
    store: Store,
    server_id: String,
    team_id: String,
    command_keys: String,
    trusted: TrustedKeys,
    key: std::sync::Mutex<Current>,
    /// The last command number taken. Held while a command is checked and
    /// its number kept, so two are never checked against the same one.
    last_seq: tokio::sync::Mutex<u64>,
    /// Random per process: Pyo hands commands sent to an earlier process
    /// out as failed rather than again.
    instance: String,
    stop: watch::Sender<bool>,
    timing: Timing,
}

/// Epoch milliseconds.
fn now_ms() -> i64 {
    crate::now_ms() as i64
}

/// The `error` of Pyo's JSON answer, or the start of its body.
fn message(body: &[u8]) -> String {
    #[derive(Deserialize)]
    struct E {
        error: String,
    }
    match serde_json::from_slice::<E>(body) {
        Ok(e) => e.error,
        Err(_) => String::from_utf8_lossy(body).chars().take(200).collect(),
    }
}

fn error_result(code: ErrorCode, msg: impl Into<String>) -> DaemonResult {
    DaemonResult::Error {
        error: DaemonError::new(code, msg),
    }
}

/// A command as the channel runs it: an exec stops by
/// [`wire::EXEC_MAX_MS`].
pub fn limit(cmd: &mut DaemonCommand) {
    if let DaemonCommand::Exec { request, .. } = cmd {
        let t = request.timeout_ms.unwrap_or(60_000);
        request.timeout_ms = Some(if t == 0 {
            wire::EXEC_MAX_MS
        } else {
            t.min(wire::EXEC_MAX_MS)
        });
    }
}

/// An answer as the channel carries it: an exec's output is kept to
/// [`wire::OUTPUT_CAP`] a stream.
pub fn trim(r: &mut DaemonResult) {
    if let DaemonResult::Exec { result } = r {
        for s in [&mut result.stdout, &mut result.stderr] {
            if s.len() > wire::OUTPUT_CAP {
                let mut end = wire::OUTPUT_CAP;
                while !s.is_char_boundary(end) {
                    end -= 1;
                }
                s.truncate(end);
                result.truncated = true;
            }
        }
    }
}

/// Waits from `min` up to `max` between tries.
struct Backoff {
    next: Duration,
    min: Duration,
    max: Duration,
}

impl Backoff {
    fn new(t: &Timing) -> Backoff {
        Backoff {
            next: t.backoff_min,
            min: t.backoff_min,
            max: t.backoff_max,
        }
    }

    fn reset(&mut self) {
        self.next = self.min;
    }

    async fn wait(&mut self, stop: &mut watch::Receiver<bool>) {
        sleep_or_stop(self.next, stop).await;
        self.next = (self.next * 2).min(self.max);
    }
}

/// Sleeps for `d`, or until the client stops.
async fn sleep_or_stop(d: Duration, stop: &mut watch::Receiver<bool>) {
    tokio::select! {
        _ = tokio::time::sleep(d) => {}
        _ = stop.wait_for(|s| *s) => {}
    }
}

impl<W: Work> Client<W> {
    /// The client of a server that joined, from what it keeps on disk.
    pub fn load(
        work: Arc<W>,
        store: Store,
        cfg: &OutboundConfig,
        server_id: &str,
        timing: Timing,
    ) -> anyhow::Result<Arc<Self>> {
        let state = store.state()?;
        let key = store.key()?;
        if state.server_id != server_id || state.team_id != cfg.team_id {
            anyhow::bail!(
                "The server's key is for another server or team than its config. Run the join command from Pyo again."
            );
        }
        let trusted = TrustedKeys::parse(&state.command_keys)
            .map_err(|e| anyhow::anyhow!("The command keys kept at join can't be read: {e}"))?;
        let request_key = key.request_key()?;
        let (stop, _) = watch::channel(false);
        Ok(Arc::new(Client {
            http: Http::new(&cfg.pyo_url)?,
            work,
            store,
            server_id: state.server_id,
            team_id: state.team_id,
            command_keys: state.command_keys,
            trusted,
            key: std::sync::Mutex::new(Current {
                file: key,
                key: Arc::new(request_key),
            }),
            last_seq: tokio::sync::Mutex::new(state.last_seq),
            instance: pyo_proto::signing::new_nonce(),
            stop,
            timing,
        }))
    }

    /// Whether it stopped: Pyo doesn't take its key any more.
    pub fn stopped(&self) -> bool {
        *self.stop.borrow()
    }

    /// The id of the key in use.
    pub fn key_id(&self) -> String {
        self.current().0
    }

    fn current(&self) -> (String, Arc<RequestKey>, i64, i64) {
        let c = self.key.lock().unwrap_or_else(|p| p.into_inner());
        (
            c.file.key_id.clone(),
            c.key.clone(),
            c.file.issued_at,
            c.file.not_after,
        )
    }

    /// Polls, reports and renews until Pyo no longer takes the key.
    pub async fn run(self: Arc<Self>) {
        tracing::info!(server = %self.server_id, pyo = %self.http.path(""), "working for Pyo over HTTPS");
        tokio::join!(self.poll_loop(), self.report_loop(), self.renew_loop());
    }

    /// Stops for good, once.
    fn gone(&self) {
        if !self.stop.send_replace(true) {
            tracing::error!(
                server = %self.server_id,
                "Pyo no longer takes this server's key. It was revoked, it expired or the server was removed. The sandboxes keep running. To connect the server again, run a new join command from Pyo."
            );
        }
    }

    /// One request signed with the server's key.
    async fn signed(
        &self,
        method: Method,
        path: &str,
        body: Vec<u8>,
        extra: &[(&str, String)],
        timeout: Duration,
    ) -> Result<(StatusCode, Vec<u8>), Failed> {
        let mut stop = self.stop.subscribe();
        if *stop.borrow() {
            return Err(Failed::Gone);
        }
        let (key_id, key, _, not_after) = self.current();
        if now_ms() >= not_after {
            self.gone();
            return Err(Failed::Gone);
        }
        let full = self.http.path(path);
        let t = now_ms() / 1000;
        let mut headers: Vec<(&str, String)> = key
            .sign(&key_id, method.as_str(), &full, &body, t)
            .into_iter()
            .collect();
        headers.extend(extra.iter().cloned());
        let sent = tokio::select! {
            r = self.http.send(method, &full, &headers, body, timeout) => r,
            _ = stop.wait_for(|s| *s) => return Err(Failed::Gone),
        };
        match sent {
            Ok((StatusCode::UNAUTHORIZED, b)) => {
                let why = message(&b);
                if why == wire::KEY_GONE {
                    self.gone();
                    return Err(Failed::Gone);
                }
                Err(Failed::Other(format!("Pyo refused the request: {why}")))
            }
            Ok(r) => Ok(r),
            Err(e) => Err(Failed::Other(format!("{e:#}"))),
        }
    }

    // ── Commands ───────────────────────────────────────────────────────────

    async fn poll_loop(self: &Arc<Self>) {
        let mut stop = self.stop.subscribe();
        let mut backoff = Backoff::new(&self.timing);
        let path = format!(
            "{}?wait={}",
            wire::COMMANDS_PATH,
            self.timing.poll_wait_secs
        );
        let headers = [(wire::INSTANCE_HEADER, self.instance.clone())];
        let timeout = Duration::from_secs(self.timing.poll_wait_secs + 20);
        while !self.stopped() {
            match self
                .signed(Method::GET, &path, vec![], &headers, timeout)
                .await
            {
                Ok((s, body)) if s.is_success() => {
                    match serde_json::from_slice::<Vec<CommandEnvelope>>(&body) {
                        Ok(list) => {
                            backoff.reset();
                            for env in list {
                                self.take(env).await;
                            }
                        }
                        Err(e) => {
                            tracing::warn!(error = %e, "Pyo's commands can't be read");
                            backoff.wait(&mut stop).await;
                        }
                    }
                }
                Ok((s, body)) => {
                    tracing::warn!(status = %s, "Pyo didn't hand out commands: {}", message(&body));
                    backoff.wait(&mut stop).await;
                }
                Err(Failed::Gone) => return,
                Err(Failed::Other(e)) => {
                    tracing::warn!("couldn't reach Pyo for commands: {e}");
                    backoff.wait(&mut stop).await;
                }
            }
        }
    }

    /// Checks a command and runs it in its own task, or refuses it.
    async fn take(self: &Arc<Self>, env: CommandEnvelope) {
        let checked = {
            let mut last = self.last_seq.lock().await;
            match self.trusted.verify_sandbox(
                Some(&env.signed),
                &env.id,
                &self.server_id,
                &self.team_id,
                *last,
                now_ms() as u64,
            ) {
                Ok(c) => {
                    // Kept before it runs: a crash then never runs it again.
                    let kept = self.store.save_state(&State {
                        server_id: self.server_id.clone(),
                        team_id: self.team_id.clone(),
                        command_keys: self.command_keys.clone(),
                        last_seq: c.seq,
                    });
                    match kept {
                        Ok(()) => {
                            *last = c.seq;
                            Ok(())
                        }
                        Err(e) => Err(error_result(
                            ErrorCode::Runtime,
                            format!("The sandbox server couldn't keep the command's number: {e:#}"),
                        )),
                    }
                }
                Err(why) => {
                    tracing::warn!(command = %env.id, kind = %env.kind, "refused a command from Pyo: {why}");
                    Err(error_result(
                        ErrorCode::Forbidden,
                        format!("The sandbox server refused the command: {why}."),
                    ))
                }
            }
        };
        let me = self.clone();
        tokio::spawn(async move {
            let result = match checked {
                Ok(()) => me.execute(&env.signed.command).await,
                Err(r) => r,
            };
            me.answer(&env.id, &result).await;
        });
    }

    async fn execute(&self, json: &str) -> DaemonResult {
        let mut cmd: DaemonCommand = match serde_json::from_str(json) {
            Ok(c) => c,
            Err(e) => {
                return error_result(
                    ErrorCode::Unsupported,
                    format!(
                        "This server's daemon can't read the command. Update the server. ({e})"
                    ),
                );
            }
        };
        // A team's server runs only that team's sandboxes.
        if let DaemonCommand::Create { spec } = &cmd
            && spec.team != self.team_id
        {
            return error_result(
                ErrorCode::Forbidden,
                "The sandbox is for another team than this server's.",
            );
        }
        limit(&mut cmd);
        let mut r = self.work.run(cmd).await;
        trim(&mut r);
        r
    }

    /// Posts a command's result, a few times while Pyo can't be reached.
    async fn answer(&self, id: &str, result: &DaemonResult) {
        let Ok(body) = serde_json::to_vec(result) else {
            return;
        };
        let mut stop = self.stop.subscribe();
        let mut backoff = Backoff::new(&self.timing);
        let path = format!("{}/{id}", wire::COMMANDS_PATH);
        for _ in 0..6 {
            match self
                .signed(
                    Method::POST,
                    &path,
                    body.clone(),
                    &[],
                    Duration::from_secs(30),
                )
                .await
            {
                Ok((s, _)) if s.is_success() => return,
                // Pyo doesn't wait for it any more.
                Ok((StatusCode::NOT_FOUND, _)) => return,
                Ok((s, b)) if s.is_client_error() && s != StatusCode::TOO_MANY_REQUESTS => {
                    tracing::warn!(command = %id, status = %s, "Pyo didn't take a command's result: {}", message(&b));
                    return;
                }
                Ok((s, b)) => {
                    tracing::warn!(command = %id, status = %s, "Pyo didn't take a command's result yet: {}", message(&b))
                }
                Err(Failed::Gone) => return,
                Err(Failed::Other(e)) => {
                    tracing::warn!(command = %id, "couldn't post a command's result: {e}")
                }
            }
            backoff.wait(&mut stop).await;
        }
    }

    // ── Reports ────────────────────────────────────────────────────────────

    async fn report_loop(self: &Arc<Self>) {
        let mut stop = self.stop.subscribe();
        while !self.stopped() {
            let body = serde_json::to_vec(&self.work.report()).unwrap_or_default();
            match self
                .signed(
                    Method::POST,
                    wire::REPORT_PATH,
                    body,
                    &[],
                    Duration::from_secs(20),
                )
                .await
            {
                Ok((s, _)) if s.is_success() => {}
                Ok((s, b)) => {
                    tracing::warn!(status = %s, "Pyo didn't take the report: {}", message(&b))
                }
                Err(Failed::Gone) => return,
                Err(Failed::Other(e)) => tracing::warn!("couldn't send the report to Pyo: {e}"),
            }
            sleep_or_stop(self.timing.report_every, &mut stop).await;
        }
    }

    // ── The key ────────────────────────────────────────────────────────────

    async fn renew_loop(self: &Arc<Self>) {
        let mut stop = self.stop.subscribe();
        let mut backoff = Backoff::new(&self.timing);
        while !self.stopped() {
            let (_, _, issued, not_after) = self.current();
            let at = issued + (not_after - issued) / 2;
            let wait = Duration::from_millis((at - now_ms()).max(0) as u64);
            sleep_or_stop(wait, &mut stop).await;
            if self.stopped() {
                return;
            }
            match self.renew().await {
                Ok(()) => backoff.reset(),
                Err(Failed::Gone) => return,
                Err(Failed::Other(e)) => {
                    tracing::warn!("couldn't renew this server's key: {e}");
                    if now_ms() >= not_after {
                        self.gone();
                        return;
                    }
                    backoff.wait(&mut stop).await;
                }
            }
        }
    }

    /// A new key, asked for with the current one.
    async fn renew(&self) -> Result<(), Failed> {
        let pkcs8 = RequestKey::generate_pkcs8().map_err(Failed::Other)?;
        let key = RequestKey::from_pkcs8(&pkcs8).map_err(Failed::Other)?;
        let body = serde_json::to_vec(&wire::RenewKey {
            public_key: key.public_key(),
        })
        .unwrap_or_default();
        let (s, b) = self
            .signed(
                Method::POST,
                wire::RENEW_KEY_PATH,
                body,
                &[],
                Duration::from_secs(30),
            )
            .await?;
        if !s.is_success() {
            return Err(Failed::Other(format!("Pyo answered {s}: {}", message(&b))));
        }
        let a: wire::KeyAnswer = serde_json::from_slice(&b)
            .map_err(|e| Failed::Other(format!("Pyo's answer can't be read: {e}")))?;
        let file = KeyFile::new(&a.key_id, a.not_after, now_ms(), &pkcs8);
        // In use from now on, kept or not: the old one ends soon.
        if let Err(e) = self.store.save_key(&file) {
            tracing::error!(error = %format!("{e:#}"), "couldn't keep the new key on disk. It works until the daemon restarts");
        }
        let mut c = self.key.lock().unwrap_or_else(|p| p.into_inner());
        *c = Current {
            file,
            key: Arc::new(key),
        };
        tracing::info!(key = %a.key_id, "renewed this server's key");
        Ok(())
    }
}
