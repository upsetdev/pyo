//! Alerts: important events sent to a team's Telegram, Discord, Slack or
//! webhook channels.
//!
//! They come from the activity log, the same events the bell shows. When an
//! entry is one of the [`Event`]s ([`classify`]), [`on_activity`] raises it
//! in the same transaction, so it goes out only once that commits. Each
//! channel gets a job of its own (jobs.rs, "alert.send"): a slow or failing
//! channel never holds up the request or rollout that caused the alert, and
//! is retried on its own.
//!
//! Channels belong to a team and its owners and admins manage them: alerts
//! are about the team's services and servers, so they go where the team
//! looks, not to whoever set them up. Where a channel sends (a bot token, a
//! webhook URL with its token in it, a signing secret) is sealed with the
//! team's key (vault.rs). The dashboard sees a hint only.
//!
//! Requests go through `crate::outbound`, so never to this machine, link-local
//! addresses (cloud metadata) or the cluster's own networks. Discord and
//! Slack only to their own webhook hosts, Telegram only to its API. A
//! webhook takes https, or plain http to a private network address (a LAN,
//! the tailnet) where the installation allows private networks, as for a
//! team's registry or bucket. A hosted Pyo (Pyo Cloud: one Pyo for many
//! teams, outside their clusters) never sends to private networks, even if
//! its settings forget to turn them off: they'd be its own.
//!
//! One alert per subject (`key`) per [`WINDOW_SECS`]. A server that flaps
//! sends "offline" and "back online", then nothing until the window ends,
//! when "offline" goes out again if it's still offline ([`flush`]).

use std::time::Duration;

use hmac::{Hmac, Mac};
use reqwest::Url;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::Sha256;
use sqlx::{PgPool, Postgres, Transaction};

use crate::{
    AppState,
    error::{ApiError, ApiResult},
    outbound::{self, Reach, Refused},
};

/// One alert per subject in this long.
pub const WINDOW_SECS: i64 = 15 * 60;
/// Per request.
const TIMEOUT: Duration = Duration::from_secs(10);
/// Waits between tries within one job run. A job that still fails is run
/// again later (jobs.rs backs off up to an hour).
const RETRY_DELAYS: [Duration; 2] = [Duration::from_secs(1), Duration::from_secs(3)];
/// The most a 429's Retry-After is waited for within one run.
const MAX_RETRY_AFTER: Duration = Duration::from_secs(10);
const TELEGRAM_API: &str = "https://api.telegram.org";
const DISCORD_HOSTS: [&str; 4] = [
    "discord.com",
    "discordapp.com",
    "canary.discord.com",
    "ptb.discord.com",
];
const SLACK_HOST: &str = "hooks.slack.com";

// ── Events ─────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Event {
    DeployFailed,
    ServerOffline,
    ServerOnline,
    /// Low on disk space or memory (pressure.rs), and no longer.
    ServerPressure,
    ServerPressureEnded,
    BackupFailed,
    CertificateExpiring,
    CertificateFailed,
    /// The Send test button.
    Test,
}

/// What a channel can get, each on or off: the settings `events` takes.
pub const TOPICS: [&str; 5] = [
    "deploy_failed",
    "server_offline",
    "server_pressure",
    "backup_failed",
    "certificate",
];

impl Event {
    /// The setting it falls under.
    pub fn topic(self) -> &'static str {
        match self {
            Event::DeployFailed => "deploy_failed",
            Event::ServerOffline | Event::ServerOnline => "server_offline",
            Event::ServerPressure | Event::ServerPressureEnded => "server_pressure",
            Event::BackupFailed => "backup_failed",
            Event::CertificateExpiring | Event::CertificateFailed => "certificate",
            Event::Test => "test",
        }
    }

    /// Its name in a webhook's body and the X-Pyo-Event header.
    pub fn name(self) -> &'static str {
        match self {
            Event::DeployFailed => "deploy_failed",
            Event::ServerOffline => "server_offline",
            Event::ServerOnline => "server_online",
            Event::ServerPressure => "server_pressure",
            Event::ServerPressureEnded => "server_pressure_ended",
            Event::BackupFailed => "backup_failed",
            Event::CertificateExpiring => "certificate_expiring",
            Event::CertificateFailed => "certificate_failed",
            Event::Test => "test",
        }
    }

    /// The event this one ends: "back online" ends "offline".
    fn ends(self) -> Option<Event> {
        match self {
            Event::ServerOnline => Some(Event::ServerOffline),
            Event::ServerPressureEnded => Some(Event::ServerPressure),
            _ => None,
        }
    }

    /// A state that lasts until another event ends it. One that comes again
    /// within the window after it ended is held, and sent if it still holds
    /// once the window is over.
    fn lasts(self) -> bool {
        matches!(self, Event::ServerOffline | Event::ServerPressure)
    }
}

/// The alert an activity entry raises, if any. Its `kind` and what it's
/// about (`target.kind`): "domain.certificate_expiring" is also used for
/// an mTLS change that failed, which isn't one.
pub fn classify(kind: &str, target: &Value) -> Option<Event> {
    Some(match kind {
        "deployment.failed" => Event::DeployFailed,
        "cluster.unreachable" | "server.offline" => Event::ServerOffline,
        "cluster.reachable" | "server.online" => Event::ServerOnline,
        crate::pressure::STARTED => Event::ServerPressure,
        crate::pressure::ENDED => Event::ServerPressureEnded,
        "backup.failed" => Event::BackupFailed,
        "domain.certificate_expiring" if target["kind"] == "certificate" => {
            Event::CertificateExpiring
        }
        "domain.certificate_failed" => Event::CertificateFailed,
        _ => return None,
    })
}

/// One alert, as every channel gets it. No secrets or environment values:
/// names, what happened and where to look.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Alert {
    pub event: Event,
    /// What it's about, for the one-per-window rule: "server:<cluster>/<name>".
    pub key: String,
    /// What happened, in one line: "Deploy failed: web".
    pub title: String,
    pub team: String,
    pub team_slug: String,
    /// More about it, in order: [("Project", "shop")].
    pub facts: Vec<(String, String)>,
    /// Its dashboard page under the team: "/deployments/dpl_1".
    pub path: String,
    /// When it happened (unix ms).
    pub at: i64,
}

impl Alert {
    /// The dashboard page, at Pyo's address. None when Pyo has none.
    pub fn link(&self, base: &str) -> Option<String> {
        let base = base.trim().trim_end_matches('/');
        (!base.is_empty()).then(|| format!("{base}/{}{}", self.team_slug, self.path))
    }
}

// ── Messages ───────────────────────────────────────────────────────────────

/// Plain text: the title, the team and facts, the link.
pub fn plain_text(a: &Alert, link: Option<&str>) -> String {
    let mut lines = vec![a.title.clone(), format!("Team: {}", a.team)];
    lines.extend(a.facts.iter().map(|(k, v)| format!("{k}: {v}")));
    lines.extend(link.map(str::to_owned));
    lines.join("\n")
}

/// Slack's mrkdwn: &, < and > are its only special characters.
fn slack_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Discord's markdown, so a name can't format the message or link elsewhere.
fn discord_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if matches!(
            c,
            '\\' | '*' | '_' | '~' | '`' | '|' | '>' | '#' | '[' | ']' | '(' | ')' | '<' | '@'
        ) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

fn clip(s: String, max: usize) -> String {
    if s.chars().count() <= max {
        s
    } else {
        let mut t: String = s.chars().take(max - 1).collect();
        t.push('…');
        t
    }
}

pub fn slack_body(a: &Alert, link: Option<&str>) -> Value {
    let mut lines = vec![
        format!("*{}*", slack_escape(&a.title)),
        format!("Team: {}", slack_escape(&a.team)),
    ];
    lines.extend(
        a.facts
            .iter()
            .map(|(k, v)| format!("{}: {}", slack_escape(k), slack_escape(v))),
    );
    if let Some(l) = link {
        lines.push(format!("<{}|Open in Pyo>", slack_escape(l)));
    }
    json!({ "text": clip(lines.join("\n"), 3000) })
}

pub fn discord_body(a: &Alert, link: Option<&str>) -> Value {
    let mut lines = vec![
        format!("**{}**", discord_escape(&a.title)),
        format!("Team: {}", discord_escape(&a.team)),
    ];
    lines.extend(
        a.facts
            .iter()
            .map(|(k, v)| format!("{}: {}", discord_escape(k), discord_escape(v))),
    );
    // In angle brackets: no preview card under it.
    lines.extend(link.map(|l| format!("<{l}>")));
    json!({
        "username": "Pyo",
        "content": clip(lines.join("\n"), 2000),
        // Never @everyone, @here or anyone else.
        "allowed_mentions": { "parse": [] },
    })
}

pub fn telegram_body(chat_id: &str, a: &Alert, link: Option<&str>) -> Value {
    // No parse_mode: nothing in it is markup.
    json!({
        "chat_id": chat_id,
        "text": clip(plain_text(a, link), 4000),
        "link_preview_options": { "is_disabled": true },
    })
}

pub fn webhook_body(a: &Alert, link: Option<&str>) -> Value {
    let details: serde_json::Map<String, Value> = a
        .facts
        .iter()
        .map(|(k, v)| (k.to_lowercase(), json!(v)))
        .collect();
    json!({
        "event": a.event.name(),
        "title": a.title,
        "team": { "name": a.team, "slug": a.team_slug },
        "details": details,
        "url": link,
        "at": a.at,
    })
}

/// `X-Pyo-Signature`: HMAC-SHA256 of "<timestamp>.<body>" with the
/// webhook's secret, as "sha256=<hex>". The timestamp is in the signature,
/// so a receiver that refuses old ones can't be sent a recorded request again.
pub fn sign(secret: &str, timestamp: i64, body: &[u8]) -> String {
    let mut mac =
        <Hmac<Sha256> as hmac::KeyInit>::new_from_slice(secret.as_bytes()).expect("any key length");
    mac.update(timestamp.to_string().as_bytes());
    mac.update(b".");
    mac.update(body);
    format!("sha256={}", hex::encode(mac.finalize().into_bytes()))
}

// ── Channels ───────────────────────────────────────────────────────────────

/// Where a channel sends, as it's sealed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum Target {
    Telegram {
        token: String,
        chat_id: String,
    },
    Discord {
        url: String,
    },
    Slack {
        url: String,
    },
    Webhook {
        url: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        secret: Option<String>,
    },
}

impl Target {
    pub fn kind(&self) -> &'static str {
        match self {
            Target::Telegram { .. } => "telegram",
            Target::Discord { .. } => "discord",
            Target::Slack { .. } => "slack",
            Target::Webhook { .. } => "webhook",
        }
    }

    /// Where requests may go: private networks only for a webhook, and only
    /// where the installation allows them.
    fn reach(&self, private_allowed: bool) -> Reach {
        Reach {
            private: matches!(self, Target::Webhook { .. }) && private_allowed,
        }
    }

    /// What the dashboard shows of it: never the token or the URL's path.
    pub fn hint(&self) -> String {
        let last4 = |s: &str| {
            let tail: String = s
                .trim_end_matches('/')
                .chars()
                .rev()
                .take(4)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect();
            format!("…{tail}")
        };
        match self {
            Target::Telegram { chat_id, .. } => format!("Chat {chat_id}"),
            Target::Discord { url } | Target::Slack { url } => format!("Webhook {}", last4(url)),
            Target::Webhook { url, secret } => {
                let host = Url::parse(url)
                    .ok()
                    .and_then(|u| {
                        u.host_str().map(|h| match u.port() {
                            Some(p) => format!("{h}:{p}"),
                            None => h.to_owned(),
                        })
                    })
                    .unwrap_or_default();
                if secret.is_some() {
                    format!("{host}, signed")
                } else {
                    host
                }
            }
        }
    }
}

fn telegram_token_ok(t: &str) -> bool {
    let Some((bot, key)) = t.split_once(':') else {
        return false;
    };
    !bot.is_empty()
        && bot.len() <= 20
        && bot.bytes().all(|b| b.is_ascii_digit())
        && (30..=80).contains(&key.len())
        && key
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// A chat's id (negative for groups and channels) or a public channel's @name.
fn telegram_chat_ok(c: &str) -> bool {
    if let Some(name) = c.strip_prefix('@') {
        return (5..=32).contains(&name.len())
            && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_');
    }
    let digits = c.strip_prefix('-').unwrap_or(c);
    (1..=20).contains(&digits.len()) && digits.bytes().all(|b| b.is_ascii_digit())
}

/// A URL's form for its kind, before any address is looked up.
fn url_form(kind: &str, raw: &str) -> Result<Url, String> {
    let raw = raw.trim();
    let invalid = || {
        match kind {
        "discord" => "Paste the webhook URL from Discord. It starts with https://discord.com/api/webhooks/.".to_owned(),
        "slack" => "Paste the incoming webhook URL from Slack. It starts with https://hooks.slack.com/services/.".to_owned(),
        _ => "Enter the webhook's URL, starting with https://.".to_owned(),
    }
    };
    if raw.is_empty() || raw.len() > 2048 {
        return Err(invalid());
    }
    let url = outbound::parse(raw).map_err(|_| invalid())?;
    let host = url.host_str().unwrap_or("").to_ascii_lowercase();
    let https = url.scheme() == "https" && url.port().is_none_or(|p| p == 443);
    let ok = match kind {
        "discord" => {
            https
                && DISCORD_HOSTS.contains(&host.as_str())
                && url.path().starts_with("/api/webhooks/")
        }
        "slack" => https && host == SLACK_HOST && url.path().starts_with("/services/"),
        // Plain http is checked against the address once it's looked up.
        _ => matches!(url.scheme(), "https" | "http"),
    };
    if ok { Ok(url) } else { Err(invalid()) }
}

/// What a request to `url` may not reach, in the dashboard's words.
fn refused(e: &Refused) -> String {
    match e {
        Refused::Plain => "Use https://. Plain http only goes to a private network address.".into(),
        e => e.to_string(),
    }
}

/// Checks a channel's target as it's saved: its form, and for a webhook,
/// the addresses its host has now.
async fn check_target(t: &Target, private_allowed: bool) -> Result<(), String> {
    match t {
        Target::Telegram { token, chat_id } => {
            if !telegram_token_ok(token) {
                return Err("Paste the bot token from @BotFather, like 123456:ABC-DEF….".into());
            }
            if !telegram_chat_ok(chat_id) {
                return Err(
                    "The chat ID is a number, like -1001234567890, or a channel's @name.".into(),
                );
            }
            Ok(())
        }
        Target::Discord { url } => url_form("discord", url).map(|_| ()),
        Target::Slack { url } => url_form("slack", url).map(|_| ()),
        Target::Webhook { url, secret } => {
            if secret.as_ref().is_some_and(|s| s.len() > 200) {
                return Err("Keep the secret under 200 characters.".into());
            }
            let u = url_form("webhook", url)?;
            outbound::check(&u, t.reach(private_allowed))
                .await
                .map_err(|e| refused(&e))
        }
    }
}

/// Whether a webhook may go to a private network: where the installation
/// allows it, and never on a hosted Pyo.
fn private_ok() -> bool {
    outbound::private_allowed() && !crate::hosted()
}

// ── Sending ────────────────────────────────────────────────────────────────

/// One request, ready to send.
#[derive(Debug, Clone)]
pub struct Outgoing {
    pub url: String,
    pub headers: Vec<(&'static str, String)>,
    pub body: Vec<u8>,
    /// The service's own error text is worth showing (Telegram's "chat not
    /// found"); an arbitrary webhook's isn't.
    detail: bool,
}

/// The request that sends `a` to `t`. `telegram` is Telegram's API (a
/// local server in tests); `now` (unix seconds) goes in a webhook's signature.
pub fn outgoing(t: &Target, a: &Alert, link: Option<&str>, telegram: &str, now: i64) -> Outgoing {
    let json_body = |v: Value| serde_json::to_vec(&v).unwrap_or_default();
    let mut headers = vec![("content-type", "application/json".to_owned())];
    let (url, body, detail) = match t {
        Target::Telegram { token, chat_id } => (
            format!("{}/bot{token}/sendMessage", telegram.trim_end_matches('/')),
            json_body(telegram_body(chat_id, a, link)),
            true,
        ),
        Target::Discord { url } => (url.clone(), json_body(discord_body(a, link)), true),
        Target::Slack { url } => (url.clone(), json_body(slack_body(a, link)), true),
        Target::Webhook { url, secret } => {
            let body = json_body(webhook_body(a, link));
            headers.push(("x-pyo-event", a.event.name().to_owned()));
            headers.push(("x-pyo-timestamp", now.to_string()));
            if let Some(s) = secret.as_deref().filter(|s| !s.is_empty()) {
                headers.push(("x-pyo-signature", sign(s, now, &body)));
            }
            (url.clone(), body, false)
        }
    };
    headers.push(("user-agent", format!("Pyo/{}", env!("CARGO_PKG_VERSION"))));
    Outgoing {
        url,
        headers,
        body,
        detail,
    }
}

/// Why a send failed, and whether trying again could help.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failure {
    pub retry: bool,
    /// For the dashboard: "401", "400: chat not found", "Timed out".
    pub message: String,
}

impl Failure {
    fn new(retry: bool, message: impl Into<String>) -> Self {
        Self {
            retry,
            message: message.into(),
        }
    }
}

/// The service's reason in its answer, short: Telegram's `description`,
/// Discord's `message`, or Slack's plain text ("invalid_token").
fn reason_in(body: &str) -> Option<String> {
    let v: Option<Value> = serde_json::from_str(body).ok();
    let text = match v {
        Some(v) => v["description"]
            .as_str()
            .or(v["message"].as_str())
            .map(str::to_owned)?,
        None => body.to_owned(),
    };
    let line = text.lines().next().unwrap_or("").trim();
    let line = line.strip_prefix("Bad Request: ").unwrap_or(line);
    (!line.is_empty()).then(|| clip(line.to_owned(), 120))
}

fn transport_failure(e: &reqwest::Error) -> Failure {
    // A name that resolves somewhere it mayn't go: the guard's refusal.
    let mut src: Option<&(dyn std::error::Error + 'static)> = Some(e);
    while let Some(s) = src {
        if let Some(r) = s.downcast_ref::<Refused>() {
            return Failure::new(false, refused(r));
        }
        src = s.source();
    }
    if e.is_timeout() {
        Failure::new(true, "Timed out")
    } else if e.is_connect() {
        Failure::new(true, "Couldn't connect")
    } else {
        Failure::new(true, "The request failed")
    }
}

/// One try.
async fn send_once(
    client: &reqwest::Client,
    out: &Outgoing,
) -> Result<(), (Failure, Option<Duration>)> {
    let mut req = client.post(&out.url).body(out.body.clone());
    for (k, v) in &out.headers {
        req = req.header(*k, v);
    }
    let r = req
        .send()
        .await
        .map_err(|e| (transport_failure(&e), None))?;
    let status = r.status();
    if status.is_success() {
        return Ok(());
    }
    let after = r
        .headers()
        .get("retry-after")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<f64>().ok())
        .filter(|s| s.is_finite() && *s >= 0.0)
        .map(|s| Duration::from_secs_f64(s).min(MAX_RETRY_AFTER));
    let body = if out.detail {
        r.text().await.unwrap_or_default()
    } else {
        String::new()
    };
    let code = status.as_u16().to_string();
    let message = match reason_in(&body) {
        Some(why) => format!("{code}: {why}"),
        None => code,
    };
    let retry = status.as_u16() == 429 || status.is_server_error();
    Err((Failure::new(retry, message), after))
}

/// Sends, trying again after each of `delays` while it might help (a 5xx,
/// a 429, a timeout). A 429's Retry-After is waited for instead, up to 10s.
pub async fn send(
    client: &reqwest::Client,
    out: &Outgoing,
    delays: &[Duration],
) -> Result<(), Failure> {
    let mut waits = delays.iter();
    loop {
        match send_once(client, out).await {
            Ok(()) => return Ok(()),
            Err((f, after)) => {
                let Some(wait) = waits.next().filter(|_| f.retry) else {
                    return Err(f);
                };
                tokio::time::sleep(after.unwrap_or(*wait)).await;
            }
        }
    }
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Sends one alert to one channel, through the guard.
async fn deliver(t: &Target, a: &Alert) -> Result<(), Failure> {
    let link = a.link(&crate::git::public_url(None));
    let out = outgoing(t, a, link.as_deref(), TELEGRAM_API, now_secs());
    let reach = t.reach(private_ok());
    let url = Url::parse(&out.url).map_err(|_| Failure::new(false, "The address isn't valid"))?;
    // An IP address in the URL isn't resolved, so the client's guard never
    // sees it: checked here, names again as the client connects.
    outbound::check(&url, reach).await.map_err(|e| {
        let retry = e == Refused::Unknown;
        Failure::new(retry, refused(&e))
    })?;
    let client =
        outbound::client(reach, TIMEOUT).map_err(|_| Failure::new(true, "The request failed"))?;
    send(&client, &out, &RETRY_DELAYS).await
}

// ── One alert per subject per window ──────────────────────────────────────

/// The last alert sent about a subject.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Last {
    /// Seconds since it was sent.
    pub age: i64,
    pub event: Event,
    /// One is held back, to send once the window ends.
    pub held: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Send,
    /// Within the window: kept, and sent when it ends if nothing ends it first.
    Hold,
    Drop,
    /// It ended what was held, so neither goes.
    DropHeld,
}

pub fn decide(last: Option<&Last>, event: Event) -> Decision {
    if let Some(ended) = event.ends() {
        return match last {
            // The state it ends was held, never sent: nothing to take back.
            Some(l) if l.held => Decision::DropHeld,
            // "Back online" after the "offline" that went out, at once.
            Some(l) if l.event == ended => Decision::Send,
            _ => Decision::Drop,
        };
    }
    match last {
        None => Decision::Send,
        Some(l) if l.age >= WINDOW_SECS => Decision::Send,
        // Back and gone again within the window: later, if it stays.
        Some(l) if event.lasts() && l.event.ends() == Some(event) => Decision::Hold,
        _ => Decision::Drop,
    }
}

/// Queues an alert for each of the team's channels that gets it, unless
/// one about the same subject went out within the window.
pub async fn raise(
    tx: &mut Transaction<'_, Postgres>,
    team_id: &str,
    alert: &Alert,
) -> ApiResult<()> {
    let channels = channels_for(tx, team_id, alert.event).await?;
    if channels.is_empty() {
        return Ok(());
    }
    // Two events about one subject at once take turns.
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 7))")
        .bind(format!("alert:{team_id}:{}", alert.key))
        .execute(&mut **tx)
        .await?;
    let row: Option<(i64, String, bool)> = sqlx::query_as(
        "SELECT extract(epoch FROM now() - sent_at)::bigint, event, held IS NOT NULL
           FROM pyo.alert_state WHERE team_id = $1 AND key = $2",
    )
    .bind(team_id)
    .bind(&alert.key)
    .fetch_optional(&mut **tx)
    .await?;
    let last = row.and_then(|(age, event, held)| {
        let event = serde_json::from_value(json!(event)).ok()?;
        Some(Last { age, event, held })
    });
    match decide(last.as_ref(), alert.event) {
        Decision::Send => {
            sqlx::query(
                "INSERT INTO pyo.alert_state (team_id, key, sent_at, event) VALUES ($1, $2, now(), $3)
                 ON CONFLICT (team_id, key) DO UPDATE SET sent_at = now(), event = excluded.event, held = NULL",
            )
            .bind(team_id)
            .bind(&alert.key)
            .bind(alert.event.name())
            .execute(&mut **tx)
            .await?;
            for id in channels {
                queue(&mut **tx, &id, alert).await?;
            }
        }
        Decision::Hold => {
            sqlx::query("UPDATE pyo.alert_state SET held = $3 WHERE team_id = $1 AND key = $2")
                .bind(team_id)
                .bind(&alert.key)
                .bind(serde_json::to_value(alert).map_err(anyhow::Error::from)?)
                .execute(&mut **tx)
                .await?;
        }
        Decision::DropHeld => {
            sqlx::query("UPDATE pyo.alert_state SET held = NULL WHERE team_id = $1 AND key = $2")
                .bind(team_id)
                .bind(&alert.key)
                .execute(&mut **tx)
                .await?;
        }
        Decision::Drop => {}
    }
    Ok(())
}

async fn channels_for(
    tx: &mut Transaction<'_, Postgres>,
    team_id: &str,
    event: Event,
) -> ApiResult<Vec<String>> {
    Ok(sqlx::query_scalar(
        "SELECT id FROM pyo.alert_channels WHERE team_id = $1 AND NOT ($2 = ANY(muted)) ORDER BY created_at",
    )
    .bind(team_id)
    .bind(event.topic())
    .fetch_all(&mut **tx)
    .await?)
}

async fn queue<'e>(
    db: impl sqlx::PgExecutor<'e>,
    channel_id: &str,
    alert: &Alert,
) -> sqlx::Result<()> {
    crate::jobs::enqueue(
        db,
        "alert.send",
        json!({ "channelId": channel_id, "alert": alert }),
    )
    .await
}

/// Held alerts whose window is over go out now (the deploy sweeper runs it).
pub async fn flush(db: &PgPool) -> ApiResult<()> {
    let mut tx = db.begin().await?;
    let due: Vec<(String, String, Value)> = sqlx::query_as(
        "SELECT team_id, key, held FROM pyo.alert_state
          WHERE held IS NOT NULL AND sent_at < now() - make_interval(secs => $1)
          FOR UPDATE SKIP LOCKED",
    )
    .bind(WINDOW_SECS as f64)
    .fetch_all(&mut *tx)
    .await?;
    for (team_id, key, held) in due {
        let alert: Option<Alert> = serde_json::from_value(held).ok();
        if let Some(a) = &alert {
            for id in channels_for(&mut tx, &team_id, a.event).await? {
                queue(&mut *tx, &id, a).await?;
            }
        }
        sqlx::query(
            "UPDATE pyo.alert_state SET held = NULL, sent_at = now(), event = coalesce($3, event)
              WHERE team_id = $1 AND key = $2",
        )
        .bind(&team_id)
        .bind(&key)
        .bind(alert.map(|a| a.event.name()))
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(())
}

// ── From the activity log ──────────────────────────────────────────────────

/// Raises the alert an activity entry calls for. An alert never fails what
/// caused it: a problem here is logged, and undone on its own savepoint.
pub async fn on_activity(
    tx: &mut Transaction<'_, Postgres>,
    team_id: &str,
    kind: &str,
    target: &Value,
    context: &Value,
) {
    let Some(event) = classify(kind, target) else {
        return;
    };
    let mut sp = match sqlx::Acquire::begin(&mut **tx).await {
        Ok(sp) => sp,
        Err(e) => {
            tracing::warn!(%kind, error = %e, "alert not raised");
            return;
        }
    };
    let r = async {
        let Some(alert) = build(&mut sp, team_id, event, target, context).await? else {
            return Ok(());
        };
        raise(&mut sp, team_id, &alert).await
    }
    .await;
    match r {
        Ok(()) => {
            if let Err(e) = sp.commit().await {
                tracing::warn!(%kind, error = %e, "alert not raised");
            }
        }
        Err(e) => {
            tracing::warn!(%kind, error = ?e.message, "alert not raised");
            let _ = sp.rollback().await;
        }
    }
}

fn now_ms() -> i64 {
    pyo_proto::now_ms()
}

/// The alert for an activity entry, with the names it needs. None when what
/// it's about is gone.
async fn build(
    tx: &mut Transaction<'_, Postgres>,
    team_id: &str,
    event: Event,
    target: &Value,
    context: &Value,
) -> ApiResult<Option<Alert>> {
    let Some((team, team_slug)): Option<(String, String)> =
        sqlx::query_as("SELECT name, slug FROM pyo.teams WHERE id = $1")
            .bind(team_id)
            .fetch_optional(&mut **tx)
            .await?
    else {
        return Ok(None);
    };
    let s = |v: &Value| v.as_str().unwrap_or("").to_owned();
    let target_id = s(&target["id"]);
    let label = s(&target["label"]);
    let alert = |key: String, title: String, facts: Vec<(&str, String)>, path: String| Alert {
        event,
        key,
        title,
        team: team.clone(),
        team_slug: team_slug.clone(),
        facts: facts.into_iter().map(|(k, v)| (k.to_owned(), v)).collect(),
        path,
        at: now_ms(),
    };
    Ok(Some(match event {
        Event::DeployFailed => {
            let service_id = s(&context["serviceId"]);
            let Some((service, project)): Option<(String, String)> = sqlx::query_as(
                "SELECT s.name, p.name FROM pyo.services s JOIN pyo.projects p ON p.id = s.project_id WHERE s.id = $1",
            )
            .bind(&service_id)
            .fetch_optional(&mut **tx)
            .await?
            else {
                return Ok(None);
            };
            alert(
                format!("deploy:{service_id}"),
                format!("Deploy failed: {service}"),
                vec![("Project", project)],
                format!("/deployments/{target_id}"),
            )
        }
        Event::ServerOffline | Event::ServerOnline if target["kind"] == "cluster" => {
            let Some(name): Option<String> =
                sqlx::query_scalar("SELECT name FROM pyo.clusters WHERE id = $1")
                    .bind(&target_id)
                    .fetch_optional(&mut **tx)
                    .await?
            else {
                return Ok(None);
            };
            let title = if event == Event::ServerOffline {
                format!("Cluster {name} is unreachable")
            } else {
                format!("Cluster {name} is back online")
            };
            alert(
                format!("cluster:{target_id}"),
                title,
                vec![],
                format!("/clusters/{name}"),
            )
        }
        Event::ServerOffline | Event::ServerOnline => {
            let cluster_id = s(&context["clusterId"]);
            let Some(cluster): Option<String> =
                sqlx::query_scalar("SELECT name FROM pyo.clusters WHERE id = $1")
                    .bind(&cluster_id)
                    .fetch_optional(&mut **tx)
                    .await?
            else {
                return Ok(None);
            };
            let title = if event == Event::ServerOffline {
                format!("Server {label} is offline")
            } else {
                format!("Server {label} is back online")
            };
            alert(
                format!("server:{cluster_id}/{label}"),
                title,
                vec![("Cluster", cluster.clone())],
                format!("/clusters/{cluster}/{label}"),
            )
        }
        Event::ServerPressure | Event::ServerPressureEnded => {
            let cluster_id = s(&context["clusterId"]);
            let Some(cluster): Option<String> =
                sqlx::query_scalar("SELECT name FROM pyo.clusters WHERE id = $1")
                    .bind(&cluster_id)
                    .fetch_optional(&mut **tx)
                    .await?
            else {
                return Ok(None);
            };
            let what: Vec<&str> = context["shortOf"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .collect();
            let step = if event == Event::ServerPressure {
                crate::pressure::Step::Start
            } else {
                crate::pressure::Step::End
            };
            alert(
                format!("pressure:{cluster_id}/{label}"),
                crate::pressure::title(&format!("Server {label}"), &what, step),
                vec![("Cluster", cluster.clone())],
                format!("/clusters/{cluster}/{label}"),
            )
        }
        Event::BackupFailed => {
            let of = s(&context["backupOf"]);
            alert(
                format!("backup:{}", if of.is_empty() { &label } else { &of }),
                format!("Backup failed: {label}"),
                vec![],
                "/backups".into(),
            )
        }
        Event::CertificateExpiring => {
            // An uploaded certificate, or one a CDN proves itself with.
            let mut row: Option<(String, i64)> = None;
            let mut path = "/certificates";
            for (table, page) in [
                ("certificates", "/certificates"),
                ("origin_pull_certificates", "/mtls"),
            ] {
                // `table` is one of the two names above.
                let sql = format!(
                    "SELECT name, ceil(extract(epoch FROM not_after - now()) / 86400)::bigint FROM pyo.{table} WHERE id = $1"
                );
                row = sqlx::query_as(sqlx::AssertSqlSafe(sql))
                    .bind(&target_id)
                    .fetch_optional(&mut **tx)
                    .await?;
                if row.is_some() {
                    path = page;
                    break;
                }
            }
            let Some((name, days)) = row else {
                return Ok(None);
            };
            let title = match days {
                ..=0 => format!("Certificate {name} has expired"),
                1 => format!("Certificate {name} expires in 1 day"),
                d => format!("Certificate {name} expires in {d} days"),
            };
            alert(format!("cert:{target_id}"), title, vec![], path.into())
        }
        Event::CertificateFailed => {
            let service_id = s(&context["serviceId"]);
            let Some((service, namespace)): Option<(String, String)> = sqlx::query_as(
                "SELECT s.name, p.namespace FROM pyo.services s JOIN pyo.projects p ON p.id = s.project_id WHERE s.id = $1",
            )
            .bind(&service_id)
            .fetch_optional(&mut **tx)
            .await?
            else {
                return Ok(None);
            };
            alert(
                format!("tls:{service_id}/{label}"),
                format!("Couldn't renew the certificate for {label}"),
                vec![("Service", service.clone())],
                format!("/{namespace}/{service}/domains"),
            )
        }
        Event::Test => return Ok(None),
    }))
}

// ── Jobs and the last result ───────────────────────────────────────────────

async fn target_of(db: &PgPool, channel_id: &str) -> anyhow::Result<Option<Target>> {
    let sealed: Option<String> =
        sqlx::query_scalar("SELECT target_sealed FROM pyo.alert_channels WHERE id = $1")
            .bind(channel_id)
            .fetch_optional(db)
            .await?;
    let Some(sealed) = sealed else {
        return Ok(None);
    };
    let plain = crate::vault::get().open(&sealed).await?;
    Ok(Some(serde_json::from_str(&plain)?))
}

/// Keeps how the last send went, for the dashboard.
async fn record(db: &PgPool, channel_id: &str, r: &Result<(), Failure>) -> sqlx::Result<()> {
    match r {
        Ok(()) => sqlx::query(
            "UPDATE pyo.alert_channels SET last_sent_at = now(), last_error = NULL WHERE id = $1",
        )
        .bind(channel_id)
        .execute(db)
        .await
        .map(|_| ()),
        Err(f) => sqlx::query(
            "UPDATE pyo.alert_channels SET last_error = $2, last_error_at = now() WHERE id = $1",
        )
        .bind(channel_id)
        .bind(&f.message)
        .execute(db)
        .await
        .map(|_| ()),
    }
}

/// The "alert.send" job: one alert to one channel. A failure that could
/// pass (a 5xx, a timeout) is an error, so the job runs again later; one
/// that can't (a 401, a bad chat) is only recorded.
pub async fn run_job(s: &AppState, p: &Value) -> anyhow::Result<()> {
    let id = p["channelId"].as_str().unwrap_or("");
    let alert: Alert = serde_json::from_value(p["alert"].clone())?;
    // Removed since.
    let Some(target) = target_of(&s.db, id).await? else {
        return Ok(());
    };
    let r = deliver(&target, &alert).await;
    record(&s.db, id, &r).await?;
    match r {
        Err(f) if f.retry => anyhow::bail!("{}", f.message),
        _ => Ok(()),
    }
}

// ── Operations ─────────────────────────────────────────────────────────────

/// Fields of a channel as an operation gives them. Left out on a change:
/// the saved value stays.
#[derive(Debug, Default)]
pub struct Given {
    pub webhook_url: Option<String>,
    pub token: Option<String>,
    pub chat_id: Option<String>,
    /// A webhook's signing secret. Empty: no signature.
    pub secret: Option<String>,
}

fn trimmed(v: &Option<String>) -> Option<String> {
    v.as_deref().map(|s| s.trim().to_owned())
}

/// The target from what's given, over what's saved (a change).
fn target_from(kind: &str, g: &Given, saved: Option<&Target>) -> Result<Target, String> {
    let need = |v: Option<String>, what: &str| {
        v.filter(|s| !s.is_empty())
            .ok_or_else(|| format!("Enter the {what}."))
    };
    let url = || {
        trimmed(&g.webhook_url).or_else(|| match saved {
            Some(Target::Discord { url } | Target::Slack { url } | Target::Webhook { url, .. }) => {
                Some(url.clone())
            }
            _ => None,
        })
    };
    Ok(match kind {
        "telegram" => {
            let (t0, c0) = match saved {
                Some(Target::Telegram { token, chat_id }) => {
                    (Some(token.clone()), Some(chat_id.clone()))
                }
                _ => (None, None),
            };
            Target::Telegram {
                token: need(trimmed(&g.token).or(t0), "bot token")?,
                chat_id: need(trimmed(&g.chat_id).or(c0), "chat ID")?,
            }
        }
        "discord" => Target::Discord {
            url: need(url(), "webhook URL")?,
        },
        "slack" => Target::Slack {
            url: need(url(), "webhook URL")?,
        },
        "webhook" => {
            let secret = match trimmed(&g.secret) {
                Some(s) => Some(s).filter(|s| !s.is_empty()),
                None => match saved {
                    Some(Target::Webhook { secret, .. }) => secret.clone(),
                    _ => None,
                },
            };
            Target::Webhook {
                url: need(url(), "webhook URL")?,
                secret,
            }
        }
        _ => return Err("Pick Telegram, Discord, Slack or a webhook.".into()),
    })
}

/// The topics a channel doesn't get, from those it does (all when none are given).
fn muted_from(events: Option<&[String]>) -> ApiResult<Vec<String>> {
    let Some(events) = events else {
        return Ok(vec![]);
    };
    if let Some(bad) = events.iter().find(|e| !TOPICS.contains(&e.as_str())) {
        return Err(ApiError::bad_request(format!(
            "There's no alert event {bad}. Pick from {}.",
            TOPICS.join(", ")
        )));
    }
    if events.is_empty() {
        return Err(ApiError::bad_request("Pick at least one event."));
    }
    Ok(TOPICS
        .iter()
        .filter(|t| !events.iter().any(|e| e == *t))
        .map(|t| (*t).to_owned())
        .collect())
}

async fn seal(team_id: &str, t: &Target) -> ApiResult<String> {
    let plain = serde_json::to_string(t).map_err(anyhow::Error::from)?;
    let sealed = crate::vault::get().seal(team_id, &plain).await?;
    if !crate::vault::Vault::is_sealed(&sealed) {
        return Err(ApiError::new(
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            "This installation has no PYO_SECRET_KEY, so it can't store the channel.",
        ));
    }
    Ok(sealed)
}

#[allow(clippy::too_many_arguments)]
pub async fn add(
    tx: &mut Transaction<'_, Postgres>,
    id: &str,
    team_id: &str,
    kind: &str,
    name: &str,
    events: Option<&[String]>,
    given: &Given,
) -> ApiResult<()> {
    let name = crate::util::require_text(name, "Name", 60)?;
    let muted = muted_from(events)?;
    let target = target_from(kind, given, None).map_err(ApiError::bad_request)?;
    check_target(&target, private_ok())
        .await
        .map_err(ApiError::bad_request)?;
    let sealed = seal(team_id, &target).await?;
    sqlx::query(
        "INSERT INTO pyo.alert_channels (id, team_id, kind, name, target_sealed, hint, muted)
         VALUES ($1, $2, $3, $4, $5, $6, $7)",
    )
    .bind(id)
    .bind(team_id)
    .bind(target.kind())
    .bind(&name)
    .bind(&sealed)
    .bind(target.hint())
    .bind(&muted)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// Changes a channel: its name, events, and where it sends (checked again).
pub async fn update(
    tx: &mut Transaction<'_, Postgres>,
    id: &str,
    team_id: &str,
    name: Option<&str>,
    events: Option<&[String]>,
    given: &Given,
) -> ApiResult<String> {
    let (kind, saved_name, sealed): (String, String, String) = sqlx::query_as(
        "SELECT kind, name, target_sealed FROM pyo.alert_channels WHERE id = $1 FOR UPDATE",
    )
    .bind(id)
    .fetch_one(&mut **tx)
    .await?;
    let name = match name {
        Some(n) => crate::util::require_text(n, "Name", 60)?,
        None => saved_name,
    };
    if events.is_some() {
        sqlx::query("UPDATE pyo.alert_channels SET muted = $2 WHERE id = $1")
            .bind(id)
            .bind(muted_from(events)?)
            .execute(&mut **tx)
            .await?;
    }
    let changes_target = given.webhook_url.is_some()
        || given.token.is_some()
        || given.chat_id.is_some()
        || given.secret.is_some();
    if changes_target {
        let saved: Target = serde_json::from_str(&crate::vault::get().open(&sealed).await?)
            .map_err(anyhow::Error::from)?;
        let target = target_from(&kind, given, Some(&saved)).map_err(ApiError::bad_request)?;
        check_target(&target, private_ok())
            .await
            .map_err(ApiError::bad_request)?;
        // A new place to send: its last result was somewhere else.
        sqlx::query(
            "UPDATE pyo.alert_channels SET target_sealed = $2, hint = $3, last_sent_at = NULL, last_error = NULL, last_error_at = NULL WHERE id = $1",
        )
        .bind(id)
        .bind(seal(team_id, &target).await?)
        .bind(target.hint())
        .execute(&mut **tx)
        .await?;
    }
    sqlx::query("UPDATE pyo.alert_channels SET name = $2 WHERE id = $1")
        .bind(id)
        .bind(&name)
        .execute(&mut **tx)
        .await?;
    Ok(name)
}

/// The Send test button: a test alert to one channel now, with no
/// transaction open. Its result is the channel's last one.
pub async fn test(s: &AppState, id: &str) -> ApiResult<()> {
    let (team, team_slug, name): (String, String, String) = sqlx::query_as(
        "SELECT t.name, t.slug, c.name FROM pyo.alert_channels c JOIN pyo.teams t ON t.id = c.team_id WHERE c.id = $1",
    )
    .bind(id)
    .fetch_optional(&s.db)
    .await?
    .ok_or_else(|| ApiError::not_found("Alert channel"))?;
    let Some(target) = target_of(&s.db, id).await? else {
        return Err(ApiError::not_found("Alert channel"));
    };
    let alert = Alert {
        event: Event::Test,
        key: format!("test:{id}"),
        title: "Test alert from Pyo".into(),
        team,
        team_slug,
        facts: vec![("Channel".into(), name)],
        path: "/notifications".into(),
        at: now_ms(),
    };
    let r = deliver(&target, &alert).await;
    record(&s.db, id, &r).await?;
    r.map_err(|f| {
        ApiError::new(
            axum::http::StatusCode::BAD_GATEWAY,
            format!("It didn't arrive: {}", f.message),
        )
    })
}

#[cfg(test)]
mod tests;
