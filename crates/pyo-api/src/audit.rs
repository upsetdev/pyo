//! The audit log: who did what in a team, when, from where and how.
//!
//! Every entry of a team's activity that a person made goes here too
//! (ops.rs `log_activity`), with what the request says about them: their
//! account, the address and browser it came from, and whether it came from
//! the dashboard, an API key or an assistant (MCP). Security events that
//! aren't activity go here only: sign-ins and failed ones, two-factor
//! changes, recovery codes used.
//!
//! What a request knows about itself is kept for it by [`middleware`] (a
//! task-local), and filled in by the extractors that find out who it is
//! (auth.rs, oauth.rs), so `log_activity`'s many callers needn't pass it
//! along. Work a request hands to another task is recorded as Pyo's own.
//!
//! Addresses are kept as each team chose (`teams.audit_ips`: full,
//! anonymized or none) when the entry is written. Owners and admins read
//! the log on Team › Audit log and can download it as CSV. Entries are
//! kept a year on Pyo Cloud, and as the installation's admin chose when
//! self-hosted (Settings › Security).

use std::{
    net::SocketAddr,
    sync::{Arc, Mutex},
};

use axum::{
    extract::{ConnectInfo, Request, State},
    http::{HeaderValue, header},
    middleware::Next,
    response::{IntoResponse, Response},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::{PgConnection, PgPool};

use crate::{
    AppState,
    auth::{SessionUser, User},
    error::{ApiError, ApiResult, Json, Query},
};

/// How a request reached Pyo.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Via {
    Dashboard,
    /// An API key (scripts, the CLI).
    Api,
    /// An assistant connected over OAuth.
    Mcp,
    /// Pyo itself: background work, agents, webhooks.
    System,
}

impl Via {
    pub fn as_str(self) -> &'static str {
        match self {
            Via::Dashboard => "dashboard",
            Via::Api => "api",
            Via::Mcp => "mcp",
            Via::System => "system",
        }
    }
}

/// Who did it, as an entry records them.
#[derive(Debug, Clone)]
pub struct Who {
    pub id: Option<String>,
    pub name: String,
    pub email: String,
    pub via: Via,
    /// The API key's or assistant's name.
    pub key: Option<String>,
}

impl Who {
    /// A person, signing in or using the dashboard.
    pub fn person(user: &User) -> Self {
        Who {
            id: Some(user.id.clone()),
            name: user.name.clone(),
            email: user.email.clone(),
            via: Via::Dashboard,
            key: None,
        }
    }

    /// Someone who isn't signed in yet (a failed sign-in), by account.
    pub fn account(id: &str, name: &str, email: &str) -> Self {
        Who {
            id: Some(id.to_owned()),
            name: name.to_owned(),
            email: email.to_owned(),
            via: Via::Dashboard,
            key: None,
        }
    }

    /// The request's own person when it has one, else `name` as Pyo's own
    /// work ("pyo", a job acting for someone).
    fn current(name: &str) -> Self {
        if name != "pyo"
            && let Some(who) = REQUEST
                .try_with(|r| r.actor.lock().unwrap_or_else(|e| e.into_inner()).clone())
                .ok()
                .flatten()
        {
            return who;
        }
        Who {
            id: None,
            name: name.to_owned(),
            email: String::new(),
            via: Via::System,
            key: None,
        }
    }
}

/// What a request says about itself, for its entries.
struct RequestInfo {
    ip: String,
    user_agent: String,
    actor: Mutex<Option<Who>>,
}

tokio::task_local! {
    static REQUEST: Arc<RequestInfo>;
}

/// Keeps the request's address and browser for the entries it writes.
pub async fn middleware(req: Request, next: Next) -> Response {
    let info = Arc::new(request_info(&req));
    REQUEST.scope(info, next.run(req)).await
}

fn request_info(req: &Request) -> RequestInfo {
    let peer = req
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|c| c.0.ip().to_string())
        .unwrap_or_default();
    let h = |k: &str| {
        req.headers()
            .get_all(k)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .collect::<Vec<_>>()
            .join(",")
    };
    let ip = crate::ratelimit::visitor_ip(
        &peer,
        &h("cf-connecting-ip"),
        &h("x-forwarded-for"),
        crate::ratelimit::trusted_proxies(),
    );
    RequestInfo {
        ip,
        user_agent: user_agent(req.headers()),
        actor: Mutex::new(None),
    }
}

/// A browser's User-Agent, cut to a sane length.
pub fn user_agent(headers: &axum::http::HeaderMap) -> String {
    headers
        .get(header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .chars()
        .take(300)
        .collect()
}

/// The request is `user`'s, through `via` (and that key or assistant).
pub fn set_actor(user: &User, via: Via, key: Option<&str>) {
    let _ = REQUEST.try_with(|r| {
        *r.actor.lock().unwrap_or_else(|e| e.into_inner()) = Some(Who {
            id: Some(user.id.clone()),
            name: user.name.clone(),
            email: user.email.clone(),
            via,
            key: key.map(str::to_owned),
        })
    });
}

fn request_ip() -> String {
    REQUEST.try_with(|r| r.ip.clone()).unwrap_or_default()
}

fn request_agent() -> String {
    REQUEST
        .try_with(|r| r.user_agent.clone())
        .unwrap_or_default()
}

/// One entry, before it's written.
pub struct Entry<'a> {
    pub action: &'a str,
    pub title: &'a str,
    /// `{kind, id, label}`, as activity has it.
    pub target: Value,
    pub failed: bool,
}

/// The teams an entry goes to.
enum Teams<'a> {
    One(&'a str),
    /// Every team the account is an active member of.
    OfUser(&'a str),
}

async fn insert(
    conn: &mut PgConnection,
    teams: Teams<'_>,
    who: &Who,
    e: Entry<'_>,
) -> sqlx::Result<()> {
    let ip = request_ip();
    let anonymized = pyo_proto::traffic::mask_ip("anonymized", &ip);
    let full = pyo_proto::traffic::mask_ip("full", &ip);
    let (filter, team) = match teams {
        Teams::One(t) => ("t.id = $1", t),
        Teams::OfUser(u) => (
            "t.id IN (SELECT team_id FROM pyo.members WHERE user_id = $1 AND status = 'active')",
            u,
        ),
    };
    let sql = format!(
        "INSERT INTO pyo.audit_events (id, team_id, action, title, actor_id, actor_name, actor_email, via, key_name, ip, user_agent, target, outcome)
         SELECT 'aud_' || substr(md5(random()::text || clock_timestamp()::text || t.id), 1, 24), t.id, $2, $3, $4, $5, $6, $7, $8,
                nullif(CASE t.audit_ips WHEN 'full' THEN $9 WHEN 'anonymized' THEN $10 END, ''), $11, $12, $13
           FROM pyo.teams t WHERE {filter}"
    );
    sqlx::query(sqlx::AssertSqlSafe(sql))
        .bind(team)
        .bind(e.action)
        .bind(e.title)
        .bind(&who.id)
        .bind(&who.name)
        .bind(&who.email)
        .bind(who.via.as_str())
        .bind(&who.key)
        .bind(full)
        .bind(anonymized)
        .bind(if who.via == Via::System {
            String::new()
        } else {
            request_agent()
        })
        .bind(&e.target)
        .bind(if e.failed { "failed" } else { "ok" })
        .execute(conn)
        .await?;
    Ok(())
}

/// An entry of a team's activity (ops.rs `log_activity`), by whoever the
/// request is for. Pyo's own events aren't audited.
pub async fn from_activity(
    conn: &mut PgConnection,
    team_id: &str,
    actor: &str,
    kind: &str,
    title: &str,
    target: &Value,
) -> sqlx::Result<()> {
    let who = Who::current(actor);
    if who.via == Via::System && actor == "pyo" {
        return Ok(());
    }
    insert(
        conn,
        Teams::One(team_id),
        &who,
        Entry {
            action: kind,
            title,
            target: target.clone(),
            failed: false,
        },
    )
    .await
}

/// A security event of an account (a sign-in, a two-factor change), in
/// every team it's a member of. Never fails what it records: a missing
/// entry is logged instead.
pub async fn account_event(db: &PgPool, user_id: &str, who: &Who, e: Entry<'_>) {
    let action = e.action.to_owned();
    let out = async {
        let mut conn = db.acquire().await?;
        insert(&mut conn, Teams::OfUser(user_id), who, e).await
    }
    .await;
    if let Err(err) = out {
        tracing::warn!(error = %err, action, "audit entry not written");
    }
}

/// [`account_event`] within a transaction already open.
pub async fn account_event_in(
    conn: &mut PgConnection,
    user_id: &str,
    who: &Who,
    e: Entry<'_>,
) -> sqlx::Result<()> {
    insert(conn, Teams::OfUser(user_id), who, e).await
}

// ── Settings ───────────────────────────────────────────────────────────────

/// What the audit log may be kept for when self-hosted, in days.
pub const AUDIT_DAYS: [u16; 4] = [90, 180, 365, 730];
/// Pyo Cloud keeps it a year.
pub const HOSTED_AUDIT_DAYS: u16 = 365;

/// The installation's security settings ('security' in pyo.settings).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SecuritySettings {
    /// Everyone on this installation must use two-factor.
    #[serde(default)]
    pub require_two_factor: bool,
    /// How long audit entries are kept.
    #[serde(default = "default_days")]
    pub audit_days: u16,
}

fn default_days() -> u16 {
    HOSTED_AUDIT_DAYS
}

impl Default for SecuritySettings {
    fn default() -> Self {
        SecuritySettings {
            require_two_factor: false,
            audit_days: HOSTED_AUDIT_DAYS,
        }
    }
}

pub async fn settings(db: impl sqlx::PgExecutor<'_>) -> sqlx::Result<SecuritySettings> {
    let v: Option<Value> =
        sqlx::query_scalar("SELECT value FROM pyo.settings WHERE key = 'security'")
            .fetch_optional(db)
            .await?;
    let mut s: SecuritySettings = v
        .and_then(|v| serde_json::from_value(v).ok())
        .unwrap_or_default();
    if !AUDIT_DAYS.contains(&s.audit_days) {
        s.audit_days = HOSTED_AUDIT_DAYS;
    }
    if crate::hosted() {
        s.audit_days = HOSTED_AUDIT_DAYS;
    }
    Ok(s)
}

pub async fn save_settings(conn: &mut PgConnection, s: &SecuritySettings) -> ApiResult<()> {
    if !AUDIT_DAYS.contains(&s.audit_days) {
        return Err(ApiError::bad_request(
            "Keep the audit log 90, 180, 365 or 730 days.",
        ));
    }
    sqlx::query(
        "INSERT INTO pyo.settings (key, value) VALUES ('security', $1)
         ON CONFLICT (key) DO UPDATE SET value = excluded.value, updated_at = now()",
    )
    .bind(json!(s))
    .execute(conn)
    .await?;
    Ok(())
}

/// Entries older than the installation keeps them go, a batch at a time.
/// Returns how many went.
pub async fn prune(db: &PgPool) -> sqlx::Result<u64> {
    let days = settings(db).await?.audit_days;
    Ok(sqlx::query(
        "DELETE FROM pyo.audit_events WHERE id IN (
           SELECT id FROM pyo.audit_events WHERE at < now() - make_interval(days => $1) LIMIT 5000)",
    )
    .bind(i32::from(days))
    .execute(db)
    .await?
    .rows_affected())
}

// ── Reading ────────────────────────────────────────────────────────────────

/// Entries per page.
const PAGE: i64 = 100;
/// Rows in one CSV download.
const CSV_ROWS: i64 = 50_000;

#[derive(Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
#[serde(rename_all = "camelCase")]
pub struct AuditQuery {
    /// The team's id (`team_…`).
    team: String,
    /// Only what this account did (its user id), or `pyo` for Pyo's own.
    #[serde(default)]
    person: Option<String>,
    /// Only this action (`member.role_changed`) or kind of action (`member`).
    #[serde(default)]
    action: Option<String>,
    /// From this time on (milliseconds).
    #[serde(default)]
    from: Option<i64>,
    /// Up to this time (milliseconds).
    #[serde(default)]
    to: Option<i64>,
    /// The `next` of the page before.
    #[serde(default)]
    cursor: Option<String>,
    /// `csv` for a download of everything that matches (up to 50,000 rows).
    #[serde(default)]
    format: Option<String>,
}

/// Audit log
///
/// Who did what in a team, newest first: when, the account, the action,
/// how (dashboard, API key or assistant) and the address and browser it
/// came from, as the team keeps them. Sign-ins, failed sign-ins and
/// two-factor changes are here too. For the team's owners and admins.
#[utoipa::path(get, path = "/audit-log", operation_id = "listAuditLog", tag = "Teams",
    params(AuditQuery),
    responses(
        (status = 200, description = "`events` (each with `id`, `at`, `action`, `title`, `actor` {id, name, email}, `via`, `key`, `ip`, `userAgent`, `target` and `outcome`), `next` (the `cursor` for the next page, or null), `people` (who appears in the log), `ips` (what the team keeps of addresses) and `days` (how long entries are kept). With `format=csv`, a CSV file.", body = Object),
        (status = 403, description = "Not an owner or admin of the team.", body = crate::error::ErrorBody),
        (status = 404, description = "No such team, or it isn't yours to see.", body = crate::error::ErrorBody),
    ),
    extensions(("x-pyo-access" = json!("read"))))]
pub async fn list(
    State(s): State<AppState>,
    SessionUser(user): SessionUser,
    Query(q): Query<AuditQuery>,
) -> ApiResult<Response> {
    Ok(match read(&s, &user, q).await? {
        Read::Page(v) => Json(v).into_response(),
        Read::Csv { slug, body } => {
            let mut res = body.into_response();
            let h = res.headers_mut();
            h.insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static("text/csv; charset=utf-8"),
            );
            if let Ok(v) =
                HeaderValue::from_str(&format!("attachment; filename=\"{slug}-audit-log.csv\""))
            {
                h.insert(header::CONTENT_DISPOSITION, v);
            }
            res
        }
    })
}

/// [`list`] as JSON only (assistants' tool).
pub async fn page(
    State(s): State<AppState>,
    SessionUser(user): SessionUser,
    Query(mut q): Query<AuditQuery>,
) -> ApiResult<Json<Value>> {
    q.format = None;
    match read(&s, &user, q).await? {
        Read::Page(v) => Ok(Json(v)),
        Read::Csv { .. } => Err(ApiError::bad_request("Ask for JSON.")),
    }
}

enum Read {
    Page(Value),
    Csv { slug: String, body: String },
}

async fn read(s: &AppState, user: &User, q: AuditQuery) -> ApiResult<Read> {
    let days = settings(&s.db).await?.audit_days;
    let mut tx = crate::rls::begin(&s.db, user).await?;
    // Owners and admins, from the dashboard or with a key that reads.
    let role = crate::ops::require_role(&mut tx, user, &q.team, crate::ops::Role::Viewer).await?;
    if role < crate::ops::Role::Admin {
        return Err(ApiError::forbidden(
            "Only team owners and admins can see the audit log.",
        ));
    }
    let csv = q.format.as_deref() == Some("csv");
    let (cursor_at, cursor_id) = match q.cursor.as_deref().and_then(|c| c.split_once(':')) {
        Some((at, id)) => (at.parse::<i64>().ok(), Some(id.to_owned())),
        None => (None, None),
    };
    let action = q.action.as_deref().map(str::trim).filter(|a| !a.is_empty());
    let person = q.person.as_deref().map(str::trim).filter(|p| !p.is_empty());
    let rows: Vec<Value> = sqlx::query_scalar(
        "SELECT jsonb_build_object('id', id, 'at', pyo.ms(at), 'action', action, 'title', title,
                  'actor', jsonb_build_object('id', actor_id, 'name', actor_name, 'email', actor_email),
                  'via', via, 'key', key_name, 'ip', ip, 'userAgent', user_agent, 'target', target, 'outcome', outcome)
           FROM pyo.audit_events
          WHERE team_id = $1
            AND ($2::text IS NULL OR action = $2 OR action LIKE $2 || '.%')
            AND ($3::text IS NULL OR ($3 = 'pyo' AND via = 'system') OR actor_id = $3)
            AND ($4::bigint IS NULL OR at >= to_timestamp($4::float8 / 1000))
            AND ($5::bigint IS NULL OR at <= to_timestamp($5::float8 / 1000))
            AND ($6::bigint IS NULL OR (pyo.ms(at), id) < ($6, $7))
          ORDER BY at DESC, id DESC
          LIMIT $8",
    )
    .bind(&q.team)
    .bind(action)
    .bind(person)
    .bind(q.from)
    .bind(q.to)
    .bind(cursor_at)
    .bind(cursor_id.unwrap_or_default())
    .bind(if csv { CSV_ROWS } else { PAGE + 1 })
    .fetch_all(&mut *tx)
    .await?;
    if csv {
        let (slug,): (String,) = sqlx::query_as("SELECT slug FROM pyo.teams WHERE id = $1")
            .bind(&q.team)
            .fetch_one(&mut *tx)
            .await?;
        tx.commit().await?;
        return Ok(Read::Csv {
            slug,
            body: to_csv(&rows),
        });
    }
    let people: Vec<Value> = sqlx::query_scalar(
        "SELECT jsonb_build_object('id', actor_id, 'name', max(actor_name), 'email', max(actor_email))
           FROM pyo.audit_events WHERE team_id = $1 AND actor_id IS NOT NULL
          GROUP BY actor_id ORDER BY max(actor_name) LIMIT 500",
    )
    .bind(&q.team)
    .fetch_all(&mut *tx)
    .await?;
    let ips: String = sqlx::query_scalar("SELECT audit_ips FROM pyo.teams WHERE id = $1")
        .bind(&q.team)
        .fetch_one(&mut *tx)
        .await?;
    tx.commit().await?;
    let mut events = rows;
    let next = if events.len() as i64 > PAGE {
        events.truncate(PAGE as usize);
        events
            .last()
            .map(|e| format!("{}:{}", e["at"], e["id"].as_str().unwrap_or("")))
    } else {
        None
    };
    Ok(Read::Page(json!({
        "events": events,
        "next": next,
        "people": people,
        "ips": ips,
        "days": days,
    })))
}

/// The CSV download: one row per entry, times in UTC.
fn to_csv(rows: &[Value]) -> String {
    let mut out =
        String::from("time,person,email,action,title,via,key,ip,user_agent,outcome,target\n");
    for r in rows {
        let s = |v: &Value| v.as_str().unwrap_or("").to_owned();
        let at = r["at"]
            .as_i64()
            .and_then(|ms| jiff::Timestamp::from_millisecond(ms).ok())
            .map(|t| t.to_string())
            .unwrap_or_default();
        let fields = [
            at,
            s(&r["actor"]["name"]),
            s(&r["actor"]["email"]),
            s(&r["action"]),
            s(&r["title"]),
            s(&r["via"]),
            s(&r["key"]),
            s(&r["ip"]),
            s(&r["userAgent"]),
            s(&r["outcome"]),
            s(&r["target"]["label"]),
        ];
        let line: Vec<String> = fields.iter().map(|f| csv_field(f)).collect();
        out.push_str(&line.join(","));
        out.push('\n');
    }
    out
}

/// One CSV field, quoted when it needs to be. A field a spreadsheet would
/// run as a formula starts with a quote mark instead.
fn csv_field(f: &str) -> String {
    let f = if f.starts_with(['=', '+', '-', '@', '\t', '\r']) {
        format!("'{f}")
    } else {
        f.to_owned()
    };
    if f.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", f.replace('"', "\"\""))
    } else {
        f
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn csv_fields_are_quoted_and_never_formulas() {
        assert_eq!(csv_field("plain"), "plain");
        assert_eq!(csv_field("a,b"), "\"a,b\"");
        assert_eq!(csv_field("say \"hi\""), "\"say \"\"hi\"\"\"");
        assert_eq!(csv_field("=HYPERLINK(1)"), "'=HYPERLINK(1)");
        assert_eq!(csv_field("-1+1"), "'-1+1");
        let rows = vec![json!({
            "at": 1_794_000_000_000_i64, "action": "auth.sign_in", "title": "Signed in",
            "actor": { "name": "Ada", "email": "ada@example.com" }, "via": "dashboard",
            "ip": "203.0.113.7", "userAgent": "Firefox, on Linux", "outcome": "ok", "target": {}
        })];
        let csv = to_csv(&rows);
        let line = csv.lines().nth(1).unwrap();
        assert!(line.starts_with("2026-"), "{line}");
        assert!(line.contains(",Ada,ada@example.com,auth.sign_in,Signed in,dashboard,,203.0.113.7,\"Firefox, on Linux\",ok,"));
    }

    #[test]
    fn hosted_keeps_a_year_and_self_hosted_picks() {
        assert!(AUDIT_DAYS.contains(&HOSTED_AUDIT_DAYS));
        assert_eq!(SecuritySettings::default().audit_days, 365);
        let s: SecuritySettings =
            serde_json::from_value(json!({ "requireTwoFactor": true })).unwrap();
        assert_eq!((s.require_two_factor, s.audit_days), (true, 365));
    }
}
