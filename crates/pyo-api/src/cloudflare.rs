//! Cloudflare DNS: a team connects its Cloudflare account with an API token
//! (Zone:Read and DNS:Edit, on the zones it picks), and Pyo creates the DNS
//! record of a domain added to a service when a connected zone covers it.
//!
//! - The token is sealed like other integration keys (secrets.rs) and only
//!   ever sent to `https://api.cloudflare.com`, through outbound.rs's client
//!   (internet only, no redirects). It's never logged or shown again. Only
//!   team owners and admins connect, test, replace or remove it.
//! - The record: an A record to the cluster's public IPv4 address, as the
//!   dashboard's DNS box says (freedomain.rs `public_ip`). Without one,
//!   there's nothing to point it at, and Pyo says so.
//! - Never overwritten: an A, AAAA or CNAME record already on the name with
//!   another value is a conflict, shown with what's there. Replacing it is
//!   for team owners and admins, and asked for each time.
//! - Records Pyo makes carry the comment "Made by Pyo (<installation id>)",
//!   and their id is kept. Pyo deletes a record only when both match.
//! - Proxied by Cloudflare: made DNS-only first and proxied once the
//!   domain's certificate is issued ([`proxy_ready`]), so Let's Encrypt's
//!   first HTTP-01 check reaches the cluster directly.
//! - A proxied record suggests the service's "Behind Cloudflare" firewall
//!   switch (the dashboard's DNS record line), until it's on.
//! - Cloudflare Tunnel (tunnel.rs): with Cloudflare Tunnel:Edit in the
//!   account and SSL and Certificates:Edit in a zone, the zone is
//!   tunnel-ready. Both are optional: DNS:Edit alone still connects.

use crate::error::Json;
use crate::error::Query;
use std::{collections::HashMap, time::Duration};

use axum::{extract::State, http::StatusCode};
use reqwest::{Method, Url};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::{Postgres, Transaction};

use crate::{
    AppState,
    auth::{SessionUser, User},
    error::{ApiError, ApiResult},
    ops::{Role, log_activity, require_role, team_of},
    outbound::{self, Reach},
};

/// Cloudflare's API: the only place the token goes.
pub const API: &str = "https://api.cloudflare.com/client/v4";
const TIMEOUT: Duration = Duration::from_secs(20);
/// Zones looked at in one test, at most.
const MAX_ZONES: usize = 200;

pub(crate) mod tunnel;
pub(crate) mod tunnel_loop;
pub(crate) mod tunnel_ops;
use tunnel::TunnelApi;

// ── Zones and records ──────────────────────────────────────────────────────

/// A zone the token sees, as stored and shown.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Zone {
    pub id: String,
    pub name: String,
    /// Pyo can create and delete DNS records in it.
    pub can_edit: bool,
    /// The permission it lacks, when it can't.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub missing: Option<String>,
    /// The Cloudflare account it's in. A tunnel lives in one account and
    /// only takes names from that account's zones.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub account_id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub account_name: String,
    /// Whether it can use Cloudflare Tunnel.
    #[serde(default)]
    pub tunnel: TunnelAccess,
}

/// Whether a zone can use Cloudflare Tunnel: the token may edit DNS in it,
/// make tunnels in its account and issue its Origin CA certificates.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct TunnelAccess {
    pub ready: bool,
    /// The tunnel permissions the token lacks here.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub missing: Vec<String>,
}

impl Zone {
    /// It can take domains through a tunnel.
    pub fn tunnel_ready(&self) -> bool {
        self.can_edit && self.tunnel.ready
    }
}

/// A DNS record at Cloudflare.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Record {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub name: String,
    pub content: String,
    #[serde(default)]
    pub proxied: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub comment: Option<String>,
}

/// A record to create.
#[derive(Debug, Clone, PartialEq)]
pub struct NewRecord {
    pub kind: String,
    pub name: String,
    pub content: String,
    pub proxied: bool,
    pub comment: String,
}

/// Why a call to Cloudflare failed, in words to show.
#[derive(Debug, Clone, PartialEq)]
pub enum CfError {
    /// The token isn't accepted, or lacks the permission for this.
    Denied(String),
    NotFound,
    /// Any other refusal, with Cloudflare's message.
    Refused(String),
    Unreachable,
}

impl std::fmt::Display for CfError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CfError::Denied(m) if m.is_empty() => f.write_str("Cloudflare refused the token."),
            CfError::Denied(m) => write!(f, "Cloudflare refused the token: {m}"),
            CfError::NotFound => f.write_str("Cloudflare doesn't have that record."),
            CfError::Refused(m) => write!(f, "Cloudflare said: {m}"),
            CfError::Unreachable => f.write_str("Pyo can't reach Cloudflare. Try again."),
        }
    }
}

fn api_error(e: CfError) -> ApiError {
    match e {
        CfError::Denied(_) | CfError::Refused(_) => ApiError::bad_request(e.to_string()),
        CfError::NotFound => ApiError::not_found("DNS record"),
        CfError::Unreachable => ApiError::new(StatusCode::BAD_GATEWAY, e.to_string()),
    }
}

/// What Pyo asks of Cloudflare. [`Http`] is the real one; the tests use a
/// fake account.
pub(crate) trait Cloudflare {
    /// The records with exactly this name.
    async fn records(&self, zone_id: &str, name: &str) -> Result<Vec<Record>, CfError>;
    async fn record(&self, zone_id: &str, id: &str) -> Result<Record, CfError>;
    async fn create(&self, zone_id: &str, r: &NewRecord) -> Result<Record, CfError>;
    async fn set_proxied(&self, zone_id: &str, id: &str, proxied: bool) -> Result<(), CfError>;
    async fn delete(&self, zone_id: &str, id: &str) -> Result<(), CfError>;
}

/// The zone a hostname is in: the longest connected zone it's in or is.
pub fn zone_for<'a>(host: &str, zones: &'a [Zone]) -> Option<&'a Zone> {
    let host = host.trim().trim_end_matches('.').to_ascii_lowercase();
    zones
        .iter()
        .filter(|z| {
            let name = z.name.trim_end_matches('.').to_ascii_lowercase();
            !name.is_empty()
                && (host == name
                    || host
                        .strip_suffix(name.as_str())
                        .is_some_and(|rest| rest.ends_with('.') && rest.len() > 1))
        })
        .max_by_key(|z| z.name.trim_end_matches('.').len())
}

/// The comment on records Pyo makes.
pub fn marker(install_id: &str) -> String {
    format!("Made by Pyo ({install_id})")
}

/// A record this installation made for this hostname.
pub fn ours(r: &Record, marker: &str, host: &str) -> bool {
    r.comment.as_deref() == Some(marker) && same_name(&r.name, host)
}

fn same_name(a: &str, b: &str) -> bool {
    a.trim_end_matches('.')
        .eq_ignore_ascii_case(b.trim_end_matches('.'))
}

/// Record types that answer for a name where a website's address goes.
fn addressing(kind: &str) -> bool {
    matches!(kind, "A" | "AAAA" | "CNAME")
}

/// What to do about a name, given the records on it.
#[derive(Debug, Clone, PartialEq)]
pub enum Plan {
    /// Nothing there: create it.
    Create,
    /// It already points where it should, and nothing else answers for it.
    Have(Record),
    /// Other records answer for the name: these.
    Conflict(Vec<Record>),
}

/// The plan for a record of `kind` with `content` on `host`.
pub fn plan(kind: &str, content: &str, host: &str, found: &[Record]) -> Plan {
    let at: Vec<&Record> = found
        .iter()
        .filter(|r| addressing(&r.kind) && same_name(&r.name, host))
        .collect();
    let same = at
        .iter()
        .find(|r| r.kind == kind && r.content.eq_ignore_ascii_case(content));
    let others: Vec<Record> = at
        .iter()
        .filter(|r| same.is_none_or(|s| s.id != r.id))
        .map(|r| (*r).clone())
        .collect();
    match (same, others.is_empty()) {
        (_, false) => Plan::Conflict(others),
        (Some(r), true) => Plan::Have((*r).clone()),
        (None, true) => Plan::Create,
    }
}

/// Whether a domain's certificate no longer needs Let's Encrypt to reach
/// the cluster directly: it's issued, or it isn't from Let's Encrypt.
pub fn proxy_now(domain: &Value) -> bool {
    let host = domain["hostname"].as_str().unwrap_or("");
    crate::deploy::tls_issuer(domain["tlsMode"].as_str().unwrap_or(""), host) != "letsencrypt"
        || domain["tls"] == json!("issued")
}

/// A token as Cloudflare makes them: one line of URL-safe characters.
pub fn token_ok(t: &str) -> bool {
    (20..=512).contains(&t.len())
        && t.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
}

/// "…abcd": enough to tell tokens apart.
fn hint(t: &str) -> String {
    format!("…{}", &t[t.len().saturating_sub(4)..])
}

// ── Checking a token ───────────────────────────────────────────────────────

/// Whether Pyo can edit DNS in a zone: it reads the records, then makes and
/// deletes a TXT record `_pyo-check.<zone>` (a token with DNS:Read only can
/// read, not write). The record's text carries this installation's id:
/// Cloudflare refuses a record identical to one that exists, so with the
/// same text a probe another installation left behind in a shared zone
/// would fail this check. The permission it lacks, or None.
pub(crate) async fn missing_in<C: Cloudflare>(
    cf: &C,
    zone: &(String, String),
    install_id: &str,
) -> Result<Option<String>, CfError> {
    let (id, name) = zone;
    let marker = &marker(install_id);
    let probe = format!("_pyo-check.{name}");
    let found = match cf.records(id, &probe).await {
        Ok(f) => f,
        Err(CfError::Denied(_)) => return Ok(Some("DNS:Edit".into())),
        Err(e) => return Err(e),
    };
    // One left by a check that stopped halfway.
    for r in found.iter().filter(|r| ours(r, marker, &probe)) {
        let _ = cf.delete(id, &r.id).await;
    }
    let made = cf
        .create(
            id,
            &NewRecord {
                kind: "TXT".into(),
                name: probe,
                content: format!("\"pyo-check-{install_id}\""),
                proxied: false,
                comment: marker.to_owned(),
            },
        )
        .await;
    match made {
        Ok(r) => match cf.delete(id, &r.id).await {
            // Already gone (another check by this installation cleaned
            // up): nothing is left behind either way.
            Ok(()) | Err(CfError::NotFound) => Ok(None),
            Err(e) => Err(e),
        },
        Err(CfError::Denied(_)) => Ok(Some("DNS:Edit".into())),
        Err(e) => Err(e),
    }
}

/// The zones a token covers, and what it can do in each: DNS per zone,
/// tunnels once per account, Origin CA certificates per zone.
pub(crate) async fn inspect<C: Cloudflare + TunnelApi>(
    cf: &C,
    install_id: &str,
) -> Result<Vec<Zone>, CfError> {
    let mut zones = match cf.zone_accounts().await {
        Ok(z) => z,
        Err(CfError::Denied(m)) => {
            return Err(CfError::Denied(if m.is_empty() {
                "check it was copied whole and has Zone:Read.".into()
            } else {
                m
            }));
        }
        Err(e) => return Err(e),
    };
    zones.sort_by(|a, b| a.name.cmp(&b.name));
    zones.truncate(MAX_ZONES);
    let mut tunnels: HashMap<String, Option<String>> = HashMap::new();
    let mut out = vec![];
    for z in &zones {
        let missing = missing_in(cf, &(z.id.clone(), z.name.clone()), install_id).await?;
        let mut lacks = vec![];
        if z.account_id.is_empty() {
            lacks.push(tunnel::TUNNEL_EDIT.to_owned());
        } else {
            if !tunnels.contains_key(&z.account_id) {
                let m = tunnel::tunnel_missing(cf, &z.account_id, install_id).await?;
                tunnels.insert(z.account_id.clone(), m);
            }
            if let Some(Some(m)) = tunnels.get(&z.account_id) {
                lacks.push(m.clone());
            }
        }
        if let Some(m) = tunnel::certs_missing(cf, &z.id).await? {
            lacks.push(m);
        }
        out.push(Zone {
            id: z.id.clone(),
            name: z.name.to_ascii_lowercase(),
            can_edit: missing.is_none(),
            missing: missing.clone(),
            account_id: z.account_id.clone(),
            account_name: z.account_name.clone(),
            tunnel: TunnelAccess {
                ready: missing.is_none() && lacks.is_empty(),
                missing: lacks,
            },
        });
    }
    Ok(out)
}

/// What's wrong with a token's zones, in sentences. Empty when it's fine.
pub fn problems(zones: &[Zone]) -> Vec<String> {
    if zones.is_empty() {
        return vec![
            "The token can't see any zones. Give it Zone:Read on the zones Pyo may use.".into(),
        ];
    }
    let mut out: Vec<String> = zones
        .iter()
        .filter_map(|z| {
            z.missing
                .as_ref()
                .map(|m| format!("{}: give the token {m}.", z.name))
        })
        .collect();
    // Tunnel permissions are optional: a zone with neither is DNS only.
    // One of the two means a tunnel was meant, so the other is a problem.
    for z in zones.iter().filter(|z| z.can_edit) {
        if z.tunnel.missing.len() == 1 {
            out.push(format!("{}: give the token {}.", z.name, z.tunnel.missing[0]));
        }
    }
    out
}

// ── Cloudflare over HTTP ───────────────────────────────────────────────────

/// Cloudflare's API with a team's token.
pub(crate) struct Http {
    client: reqwest::Client,
    token: String,
    /// [`API`], or a debug build's fake ([`api_base`]).
    base: String,
}

/// Where Cloudflare's API is: [`API`]. A debug build may point it at a fake
/// with `PYO_DEV_CLOUDFLARE_API` (`http://127.0.0.1:8787/client/v4`, for the
/// dashboard and Pyo Cloud e2e tests). Release builds never read it.
pub(crate) fn api_base() -> String {
    #[cfg(debug_assertions)]
    if let Ok(b) = std::env::var("PYO_DEV_CLOUDFLARE_API")
        && !b.trim().is_empty()
    {
        return b.trim().trim_end_matches('/').to_owned();
    }
    API.to_owned()
}

// Never print the token.
impl std::fmt::Debug for Http {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Http { token: <hidden> }")
    }
}

/// Ids from Cloudflare go into paths: hex, nothing else.
fn id_ok(id: &str) -> bool {
    !id.is_empty() && id.len() <= 64 && id.bytes().all(|b| b.is_ascii_alphanumeric())
}

/// Cloudflare's own message, from its `errors` list.
fn message(v: &Value) -> String {
    v["errors"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|e| e["message"].as_str())
        .map(|m| m.chars().take(300).collect::<String>())
        .collect::<Vec<_>>()
        .join(" ")
}

/// Error codes that mean the token itself isn't accepted (whatever the
/// HTTP status): invalid, expired or malformed.
fn token_code(v: &Value) -> bool {
    v["errors"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|e| e["code"].as_i64())
        .any(|c| matches!(c, 1000 | 6003 | 6111 | 9106 | 9109 | 10000))
}

impl Http {
    pub fn new(token: &str) -> ApiResult<Self> {
        let base = api_base();
        // Only a debug build's fake is on a private address.
        let reach = Reach {
            private: base != API,
        };
        Ok(Self {
            client: outbound::client(reach, TIMEOUT)
                .map_err(|e| ApiError::from(anyhow::Error::from(e)))?,
            token: token.to_owned(),
            base,
        })
    }

    /// A client for the tests' fake Cloudflare (fake.rs).
    #[cfg(test)]
    pub fn at(token: &str, base: &str) -> Self {
        Self {
            client: reqwest::Client::new(),
            token: token.to_owned(),
            base: base.trim_end_matches('/').to_owned(),
        }
    }

    fn url(&self, path: &str) -> Url {
        // A fixed base and checked ids: always parses.
        Url::parse(&format!("{}{path}", self.base)).expect("Cloudflare URL")
    }

    async fn call(&self, method: Method, url: Url, body: Option<Value>) -> Result<Value, CfError> {
        let real = url.host_str() == Some("api.cloudflare.com") && url.scheme() == "https";
        if !(real || (self.base != API && url.as_str().starts_with(&self.base))) {
            return Err(CfError::Unreachable);
        }
        let mut rq = self.client.request(method, url).bearer_auth(&self.token);
        if let Some(b) = body {
            rq = rq.json(&b);
        }
        let res = rq.send().await.map_err(|_| CfError::Unreachable)?;
        let status = res.status();
        let v: Value = res.json().await.unwrap_or(Value::Null);
        if status.is_success() && v["success"] != json!(false) {
            return Ok(v);
        }
        let m = message(&v);
        Err(match status.as_u16() {
            401 | 403 => CfError::Denied(m),
            404 => CfError::NotFound,
            _ if token_code(&v) => CfError::Denied(m),
            _ if m.is_empty() => CfError::Refused(format!("HTTP {status}")),
            _ => CfError::Refused(m),
        })
    }

    fn record_path(zone_id: &str, id: Option<&str>) -> Result<String, CfError> {
        if !id_ok(zone_id) || id.is_some_and(|i| !id_ok(i)) {
            return Err(CfError::NotFound);
        }
        Ok(match id {
            Some(i) => format!("/zones/{zone_id}/dns_records/{i}"),
            None => format!("/zones/{zone_id}/dns_records"),
        })
    }
}

fn record_from(v: &Value) -> Result<Record, CfError> {
    serde_json::from_value(v.clone())
        .map_err(|_| CfError::Refused("an answer Pyo can't read".into()))
}

impl Cloudflare for Http {
    async fn records(&self, zone_id: &str, name: &str) -> Result<Vec<Record>, CfError> {
        let mut url = self.url(&Self::record_path(zone_id, None)?);
        url.query_pairs_mut()
            .append_pair("name", name)
            .append_pair("per_page", "100");
        let v = self.call(Method::GET, url, None).await?;
        v["result"]
            .as_array()
            .into_iter()
            .flatten()
            .map(record_from)
            .collect()
    }

    async fn record(&self, zone_id: &str, id: &str) -> Result<Record, CfError> {
        let url = self.url(&Self::record_path(zone_id, Some(id))?);
        record_from(&self.call(Method::GET, url, None).await?["result"])
    }

    async fn create(&self, zone_id: &str, r: &NewRecord) -> Result<Record, CfError> {
        let url = self.url(&Self::record_path(zone_id, None)?);
        let body = json!({
            "type": r.kind, "name": r.name, "content": r.content,
            "ttl": 1, "proxied": r.proxied, "comment": r.comment,
        });
        record_from(&self.call(Method::POST, url, Some(body)).await?["result"])
    }

    async fn set_proxied(&self, zone_id: &str, id: &str, proxied: bool) -> Result<(), CfError> {
        let url = self.url(&Self::record_path(zone_id, Some(id))?);
        self.call(Method::PATCH, url, Some(json!({ "proxied": proxied })))
            .await
            .map(|_| ())
    }

    async fn delete(&self, zone_id: &str, id: &str) -> Result<(), CfError> {
        let url = self.url(&Self::record_path(zone_id, Some(id))?);
        self.call(Method::DELETE, url, None).await.map(|_| ())
    }
}

// ── Stored ─────────────────────────────────────────────────────────────────

/// This installation's id, for the comment on records it makes: made once.
pub async fn install_id(tx: &mut Transaction<'_, Postgres>) -> ApiResult<String> {
    let mut b = [0u8; 6];
    rand::fill(&mut b);
    sqlx::query(
        "INSERT INTO pyo.settings (key, value) VALUES ('installation', jsonb_build_object('id', $1::text))
         ON CONFLICT (key) DO NOTHING",
    )
    .bind(hex::encode(b))
    .execute(&mut **tx)
    .await?;
    Ok(
        sqlx::query_scalar("SELECT value->>'id' FROM pyo.settings WHERE key = 'installation'")
            .fetch_one(&mut **tx)
            .await?,
    )
}

/// A team's account: its token, opened, and zones.
pub(crate) struct Account {
    pub token: String,
    pub zones: Vec<Zone>,
}

pub(crate) async fn account(
    s: &AppState,
    tx: &mut Transaction<'_, Postgres>,
    team_id: &str,
) -> ApiResult<Option<Account>> {
    let row: Option<(String, Value)> = sqlx::query_as(
        "SELECT token_sealed, zones FROM pyo.cloudflare_accounts WHERE team_id = $1",
    )
    .bind(team_id)
    .fetch_optional(&mut **tx)
    .await?;
    let Some((sealed, zones)) = row else {
        return Ok(None);
    };
    let token = crate::vault::get().open_credential(&s.sealer, &sealed).await.map_err(|_| {
        ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "Pyo can't open the saved Cloudflare token. Connect Cloudflare again.",
        )
    })?;
    Ok(Some(Account {
        token,
        zones: serde_json::from_value(zones).unwrap_or_default(),
    }))
}

fn no_account() -> ApiError {
    ApiError::bad_request("Connect Cloudflare under Integrations first.")
}

/// Who may do what with a team's Cloudflare connection: see its zones
/// (everyone in the team, so they're offered when adding a domain), use it
/// for a domain's record (who can change services), replace a record
/// that's there, and see, change or remove the token itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    ListZones,
    CreateRecord,
    DeleteRecord,
    ReplaceRecord,
    ManageToken,
}

pub fn role_for(a: Action) -> Role {
    match a {
        Action::ListZones => Role::Viewer,
        Action::CreateRecord | Action::DeleteRecord => Role::Developer,
        Action::ReplaceRecord | Action::ManageToken => Role::Admin,
    }
}

/// A connection as a member sees it: never the token, and its hint and
/// who connected it only for owners and admins.
pub fn shown(row: &Value, role: Role) -> Value {
    let mut v = json!({
        "teamId": row["teamId"], "zones": row["zones"],
        "createdAt": row["createdAt"], "testedAt": row["testedAt"],
    });
    if role >= role_for(Action::ManageToken) {
        v["hint"] = row["hint"].clone();
        v["createdBy"] = row["createdBy"].clone();
    }
    v
}

fn parse_role(r: &str) -> Role {
    match r {
        "owner" => Role::Owner,
        "admin" => Role::Admin,
        "developer" => Role::Developer,
        _ => Role::Viewer,
    }
}

/// The teams' connections, for the snapshot.
pub async fn snapshot(
    db: &mut sqlx::PgConnection,
    user: &User,
    team_ids: &[String],
) -> ApiResult<Vec<Value>> {
    let rows: Vec<(Value, String)> = sqlx::query_as(
        "SELECT jsonb_build_object('teamId', c.team_id, 'zones', c.zones, 'hint', c.hint, 'createdBy', c.created_by,
                                  'createdAt', pyo.ms(c.created_at), 'testedAt', pyo.ms(c.tested_at)), m.role
           FROM pyo.cloudflare_accounts c
           JOIN pyo.members m ON m.team_id = c.team_id AND m.user_id = $2 AND m.status = 'active'
          WHERE c.team_id = ANY($1)",
    )
    .bind(team_ids)
    .bind(&user.id)
    .fetch_all(&mut *db)
    .await?;
    Ok(rows
        .iter()
        .map(|(row, role)| shown(row, parse_role(role)))
        .collect())
}

/// Each domain's record, as `dnsRecord` on the services' domains.
pub async fn attach(
    db: &mut sqlx::PgConnection,
    team_ids: &[String],
    services: &mut [Value],
) -> ApiResult<()> {
    let rows: Vec<(String, String, Value)> = sqlx::query_as(
        "SELECT service_id, domain_id, jsonb_build_object(
                'zone', zone_name, 'type', type, 'content', content, 'proxied', proxied, 'proxy', proxy,
                'status', status, 'error', error, 'existing', existing, 'mine', record_id IS NOT NULL,
                'via', via, 'previous', previous,
                'updatedAt', pyo.ms(updated_at))
           FROM pyo.dns_records WHERE team_id = ANY($1)",
    )
    .bind(team_ids)
    .fetch_all(&mut *db)
    .await?;
    if rows.is_empty() {
        return Ok(());
    }
    let mut by: HashMap<(String, String), Value> =
        rows.into_iter().map(|(s, d, v)| ((s, d), v)).collect();
    for a in services.iter_mut() {
        let id = a["id"].as_str().unwrap_or("").to_owned();
        for d in a["domains"].as_array_mut().into_iter().flatten() {
            let key = (id.clone(), d["id"].as_str().unwrap_or("").to_owned());
            if let Some(r) = by.remove(&key) {
                d["dnsRecord"] = r;
            }
        }
    }
    Ok(())
}

/// A service was saved: records of domains it no longer has (or whose
/// hostname changed) are forgotten. What's at Cloudflare stays, except a
/// record through the Cloudflare Tunnel: the tunnel loop deletes Pyo's
/// CNAME. The record it replaced isn't put back (decision 5), unless a
/// switch back that puts it back was already asked for.
pub async fn forget_removed(
    tx: &mut Transaction<'_, Postgres>,
    service_id: &str,
    spec: &Value,
) -> ApiResult<()> {
    let (ids, hosts): (Vec<String>, Vec<String>) = spec["domains"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|d| {
            (
                d["id"].as_str().unwrap_or("").to_owned(),
                d["hostname"].as_str().unwrap_or("").to_lowercase(),
            )
        })
        .unzip();
    sqlx::query(
        "UPDATE pyo.dns_records r SET status = 'reverting', restore = r.status = 'reverting' AND r.restore,
                routes_command = NULL, updated_at = now()
          WHERE r.service_id = $1 AND r.via = 'tunnel'
            AND NOT EXISTS (SELECT 1 FROM unnest($2::text[], $3::text[]) AS d(id, host)
                             WHERE d.id = r.domain_id AND d.host = lower(r.hostname))",
    )
    .bind(service_id)
    .bind(&ids)
    .bind(&hosts)
    .execute(&mut **tx)
    .await?;
    sqlx::query(
        "DELETE FROM pyo.dns_records r WHERE r.service_id = $1 AND r.via <> 'tunnel'
            AND NOT EXISTS (SELECT 1 FROM unnest($2::text[], $3::text[]) AS d(id, host)
                             WHERE d.id = r.domain_id AND d.host = lower(r.hostname))",
    )
    .bind(service_id)
    .bind(&ids)
    .bind(&hosts)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

// ── Proof of a domain (Pyo Cloud) ──────────────────────────────────────────

/// The team's Cloudflare, for its proof of `host`: the client and the zone
/// that covers the host, when the team's token can edit DNS there.
async fn proof_access(
    tx: &mut Transaction<'_, Postgres>,
    team_id: &str,
    host: &str,
) -> ApiResult<Option<(Http, String)>> {
    let row: Option<(String, Value)> = sqlx::query_as(
        "SELECT token_sealed, zones FROM pyo.cloudflare_accounts WHERE team_id = $1",
    )
    .bind(team_id)
    .fetch_optional(&mut **tx)
    .await?;
    let Some((sealed, zones)) = row else {
        return Ok(None);
    };
    let zones: Vec<Zone> = serde_json::from_value(zones).unwrap_or_default();
    let Some(zone) = zone_for(host, &zones).filter(|z| z.can_edit).cloned() else {
        return Ok(None);
    };
    // Sealed by the vault, or (older keys) with PYO_SECRET_KEY.
    let token = if sealed.starts_with(crate::secrets::PREFIX) {
        crate::secrets::Sealer::from_env().open(&sealed)
    } else {
        crate::vault::get().open(&sealed).await
    };
    let Ok(token) = token else {
        return Ok(None);
    };
    Ok(Some((Http::new(&token)?, zone.id)))
}

/// Pyo Cloud's proof of owning `host` (domainproof.rs), written by Pyo:
/// the TXT record `_pyo-verify.<host>` with the team's token, when the
/// team's Cloudflare token can edit DNS in a zone that covers it. So Cloud
/// teams with Cloudflare don't add it by hand. Whether it's there now.
/// Pyo deletes it again once the proof is done ([`remove_proof`]).
pub(crate) async fn write_proof(
    tx: &mut Transaction<'_, Postgres>,
    team_id: &str,
    host: &str,
) -> ApiResult<bool> {
    let Some((cf, zone_id)) = proof_access(tx, team_id, host).await? else {
        return Ok(false);
    };
    let name = crate::domainproof::record(host);
    let value = crate::domainproof::token(team_id, host);
    let mark = marker(&install_id(tx).await?);
    let found = cf.records(&zone_id, &name).await.map_err(api_error)?;
    if found
        .iter()
        .any(|r| r.kind == "TXT" && r.content.trim_matches('"') == value)
    {
        return Ok(true);
    }
    cf.create(
        &zone_id,
        &NewRecord {
            kind: "TXT".into(),
            name,
            content: format!("\"{value}\""),
            proxied: false,
            comment: mark,
        },
    )
    .await
    .map_err(api_error)?;
    Ok(true)
}

/// Delete the proof records this installation made for a team's `host`:
/// TXT records at `_pyo-verify.<host>` with Pyo's comment and the team's
/// token (`value`). One made by hand, by another installation or for
/// another team stays. How many went.
pub(crate) async fn delete_proofs<C: Cloudflare>(
    cf: &C,
    zone_id: &str,
    host: &str,
    value: &str,
    mark: &str,
) -> Result<usize, CfError> {
    let name = crate::domainproof::record(host);
    let mut gone = 0;
    for r in cf.records(zone_id, &name).await? {
        if r.kind != "TXT" || !ours(&r, mark, &name) || r.content.trim_matches('"') != value {
            continue;
        }
        match cf.delete(zone_id, &r.id).await {
            Ok(()) | Err(CfError::NotFound) => gone += 1,
            Err(e) => return Err(e),
        }
    }
    Ok(gone)
}

/// The proof of `host` is done, abandoned, or the domain is gone: the
/// records Pyo made for it go ([`delete_proofs`]). How many went; none
/// without the team's Cloudflare.
pub(crate) async fn remove_proof(
    tx: &mut Transaction<'_, Postgres>,
    team_id: &str,
    host: &str,
) -> ApiResult<usize> {
    let Some((cf, zone_id)) = proof_access(tx, team_id, host).await? else {
        return Ok(0);
    };
    let mark = marker(&install_id(tx).await?);
    let value = crate::domainproof::token(team_id, host);
    delete_proofs(&cf, &zone_id, host, &value, &mark)
        .await
        .map_err(api_error)
}

/// The hostnames `old` has and `new` doesn't, lowercase.
pub(crate) fn removed_hosts(old: &Value, new: &Value) -> Vec<String> {
    let hosts = |spec: &Value| -> Vec<String> {
        spec["domains"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|d| d["hostname"].as_str())
            .map(|h| h.trim().to_lowercase())
            .collect()
    };
    let now = hosts(new);
    let mut gone: Vec<String> = hosts(old).into_iter().filter(|h| !now.contains(h)).collect();
    gone.sort();
    gone.dedup();
    gone
}

/// Domains a team no longer has: the proof records Pyo made for them go,
/// in the background (the `domain.proofs` job), when the team has
/// Cloudflare. `hosts`: the hostnames removed.
pub(crate) async fn proofs_after_removal(
    tx: &mut Transaction<'_, Postgres>,
    team_id: &str,
    hosts: &[String],
) -> ApiResult<()> {
    if hosts.is_empty() {
        return Ok(());
    }
    let connected: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM pyo.cloudflare_accounts WHERE team_id = $1)",
    )
    .bind(team_id)
    .fetch_one(&mut **tx)
    .await?;
    if connected {
        crate::jobs::enqueue(
            &mut **tx,
            "domain.proofs",
            json!({ "teamId": team_id, "hosts": hosts }),
        )
        .await?;
    }
    Ok(())
}

/// The `domain.proofs` job: each removed host's proof records, unless the
/// team uses the host again by now.
pub(crate) async fn proofs_job(s: &AppState, p: &Value) -> anyhow::Result<()> {
    let team_id = p["teamId"].as_str().unwrap_or("");
    for host in p["hosts"].as_array().into_iter().flatten().filter_map(Value::as_str) {
        let mut tx = s.db.begin().await?;
        let used: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM pyo.services a, jsonb_array_elements(a.spec->'domains') d
                             WHERE a.team_id = $1 AND NOT a.deleting AND lower(d->>'hostname') = lower($2))",
        )
        .bind(team_id)
        .bind(host)
        .fetch_one(&mut *tx)
        .await?;
        if !used {
            remove_proof(&mut tx, team_id, host)
                .await
                .map_err(|e| anyhow::anyhow!("{}", e.message))?;
        }
        tx.commit().await?;
    }
    Ok(())
}

// ── The connection ─────────────────────────────────────────────────────────

/// Connect (or replace) a team's token: checked first, then sealed.
pub async fn connect(
    s: &AppState,
    tx: &mut Transaction<'_, Postgres>,
    user: &User,
    team_id: &str,
    token: &str,
) -> ApiResult<Value> {
    require_role(tx, user, team_id, role_for(Action::ManageToken)).await?;
    let token = token.trim();
    if !token_ok(token) {
        return Err(ApiError::bad_request(
            "Paste the Cloudflare API token, as Cloudflare shows it.",
        ));
    }
    if !s.sealer.enabled() {
        return Err(ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "This installation has no PYO_SECRET_KEY, so it can't store keys.",
        ));
    }
    let install = install_id(tx).await?;
    let zones = inspect(&Http::new(token)?, &install)
        .await
        .map_err(api_error)?;
    if zones.is_empty() {
        return Err(ApiError::bad_request(problems(&zones).join(" ")));
    }
    if !zones.iter().any(|z| z.can_edit) {
        return Err(ApiError::bad_request(
            "The token can't edit DNS in any of its zones. Give it DNS:Edit.",
        ));
    }
    sqlx::query(
        "INSERT INTO pyo.cloudflare_accounts (team_id, token_sealed, hint, zones, created_by) VALUES ($1, $2, $3, $4, $5)
         ON CONFLICT (team_id) DO UPDATE SET token_sealed = $2, hint = $3, zones = $4, created_by = $5, created_at = now(), tested_at = now()",
    )
    .bind(team_id)
    .bind(crate::vault::get().seal_credential(team_id, token).await?)
    .bind(hint(token))
    .bind(json!(zones))
    .bind(&user.name)
    .execute(&mut **tx)
    .await?;
    log_activity(
        tx,
        team_id,
        &user.name,
        "integration.cloudflare",
        "info",
        &format!("Connected Cloudflare ({})", zone_count(zones.len())),
        json!({ "kind": "integration", "id": "cloudflare", "label": "Cloudflare" }),
        json!({}),
    )
    .await?;
    Ok(json!({ "ok": true, "zones": zones, "problems": problems(&zones) }))
}

fn zone_count(n: usize) -> String {
    if n == 1 {
        "1 zone".into()
    } else {
        format!("{n} zones")
    }
}

/// Check the saved token again, and keep what it covers now.
pub async fn test(
    s: &AppState,
    tx: &mut Transaction<'_, Postgres>,
    user: &User,
    team_id: &str,
) -> ApiResult<Value> {
    require_role(tx, user, team_id, role_for(Action::ManageToken)).await?;
    let a = account(s, tx, team_id).await?.ok_or_else(no_account)?;
    let install = install_id(tx).await?;
    let zones = match inspect(&Http::new(&a.token)?, &install).await {
        Ok(z) => z,
        Err(e @ CfError::Denied(_)) => {
            return Ok(json!({ "ok": false, "zones": [], "problems": [e.to_string()] }));
        }
        Err(e) => return Err(api_error(e)),
    };
    sqlx::query(
        "UPDATE pyo.cloudflare_accounts SET zones = $2, tested_at = now() WHERE team_id = $1",
    )
    .bind(team_id)
    .bind(json!(zones))
    .execute(&mut **tx)
    .await?;
    let problems = problems(&zones);
    Ok(json!({ "ok": problems.is_empty(), "zones": zones, "problems": problems }))
}

/// Forget the token. Records Pyo made stay at Cloudflare. Refused while a
/// cluster of the team has a tunnel.
pub async fn remove(
    tx: &mut Transaction<'_, Postgres>,
    user: &User,
    team_id: &str,
) -> ApiResult<Value> {
    require_role(tx, user, team_id, role_for(Action::ManageToken)).await?;
    // Without the token, Pyo could no longer change or remove a tunnel.
    let tunneled: Option<String> = sqlx::query_scalar(
        "SELECT c.name FROM pyo.cluster_tunnels t JOIN pyo.clusters c ON c.id = t.cluster_id WHERE t.team_id = $1 ORDER BY c.name LIMIT 1",
    )
    .bind(team_id)
    .fetch_optional(&mut **tx)
    .await?;
    if let Some(name) = tunneled {
        return Err(ApiError::conflict(format!(
            "Remove the Cloudflare Tunnel of {name} first. Its websites keep working until then."
        )));
    }
    let gone = sqlx::query("DELETE FROM pyo.cloudflare_accounts WHERE team_id = $1")
        .bind(team_id)
        .execute(&mut **tx)
        .await?
        .rows_affected();
    if gone > 0 {
        log_activity(
            tx,
            team_id,
            &user.name,
            "integration.cloudflare",
            "warning",
            "Removed the Cloudflare token",
            json!({ "kind": "integration", "id": "cloudflare", "label": "Cloudflare" }),
            json!({}),
        )
        .await?;
    }
    Ok(json!({ "ok": true }))
}

#[derive(Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub struct CloudflareQuery {
    /// The team's id.
    team: String,
}

/// Cloudflare zones
///
/// The team's Cloudflare connection: the zones its token covers as of the
/// last test, and whether Pyo can edit DNS in each. Pyo creates a domain's
/// DNS record when one of these zones covers it. Never the token: owners
/// and admins also see its last four characters (`hint`).
#[utoipa::path(get, path = "/cloudflare", operation_id = "getCloudflare", tag = "Integrations",
    params(CloudflareQuery),
    responses(
        (status = 200, description = "`connected`, and when connected `zones` (each with `id`, `name`, `canEdit` and, when it can't, `missing`: the permission the token lacks, `accountId`, `accountName`, and `tunnel`: `ready` and the tunnel permissions it lacks as `missing`), `createdAt`, `testedAt`, and for owners and admins `hint` and `createdBy`.", body = Object),
        (status = 404, description = "No such team, or you aren't in it.", body = crate::error::ErrorBody),
    ),
    extensions(("x-pyo-access" = json!("read"))))]
pub async fn get(
    State(s): State<AppState>,
    SessionUser(user): SessionUser,
    Query(q): Query<CloudflareQuery>,
) -> ApiResult<Json<Value>> {
    let mut tx = crate::rls::begin(&s.db, &user).await?;
    let role = require_role(&mut tx, &user, &q.team, role_for(Action::ListZones)).await?;
    let row: Option<Value> = sqlx::query_scalar(
        "SELECT jsonb_build_object('teamId', team_id, 'zones', zones, 'hint', hint, 'createdBy', created_by,
                                  'createdAt', pyo.ms(created_at), 'testedAt', pyo.ms(tested_at))
           FROM pyo.cloudflare_accounts WHERE team_id = $1",
    )
    .bind(&q.team)
    .fetch_optional(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(Json(match row {
        Some(r) => {
            let mut v = shown(&r, role);
            v["connected"] = json!(true);
            v
        }
        None => json!({ "connected": false, "zones": [] }),
    }))
}

// ── A domain's record ──────────────────────────────────────────────────────

/// A domain as stored on its service.
struct Domain {
    team_id: String,
    cluster_id: String,
    host: String,
    spec: Value,
}

async fn domain(
    tx: &mut Transaction<'_, Postgres>,
    service_id: &str,
    domain_id: &str,
) -> ApiResult<Domain> {
    let (team_id, cluster_id, spec): (String, String, Value) = sqlx::query_as(
        "SELECT team_id, cluster_id, spec FROM pyo.services WHERE id = $1 AND NOT deleting",
    )
    .bind(service_id)
    .fetch_optional(&mut **tx)
    .await?
    .ok_or_else(|| ApiError::not_found("Service"))?;
    let d = spec["domains"]
        .as_array()
        .and_then(|ds| ds.iter().find(|d| d["id"] == json!(domain_id)))
        .cloned()
        .ok_or_else(|| ApiError::not_found("Domain"))?;
    Ok(Domain {
        team_id,
        cluster_id,
        host: d["hostname"].as_str().unwrap_or("").to_lowercase(),
        spec: d,
    })
}

/// A domain's record row, as it stands now.
#[derive(Debug, Clone, PartialEq)]
struct Row {
    zone_id: String,
    zone_name: String,
    record_id: Option<String>,
    kind: String,
    content: String,
    proxied: bool,
    proxy: bool,
    status: &'static str,
    error: Option<String>,
    existing: Vec<Record>,
}

async fn save_row(
    tx: &mut Transaction<'_, Postgres>,
    service_id: &str,
    domain_id: &str,
    d: &Domain,
    r: &Row,
) -> ApiResult<()> {
    sqlx::query(
        "INSERT INTO pyo.dns_records (service_id, domain_id, team_id, hostname, zone_id, zone_name, record_id, type, content, proxied, proxy, status, error, existing)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14)
         ON CONFLICT (service_id, domain_id) DO UPDATE SET hostname = $4, zone_id = $5, zone_name = $6, record_id = $7, type = $8, content = $9,
           proxied = $10, proxy = $11, status = $12, error = $13, existing = $14, updated_at = now()",
    )
    .bind(service_id)
    .bind(domain_id)
    .bind(&d.team_id)
    .bind(&d.host)
    .bind(&r.zone_id)
    .bind(&r.zone_name)
    .bind(&r.record_id)
    .bind(&r.kind)
    .bind(&r.content)
    .bind(r.proxied)
    .bind(r.proxy)
    .bind(r.status)
    .bind(&r.error)
    .bind(json!(r.existing))
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// What happened to a record, for the caller and the row.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Outcome {
    pub status: &'static str,
    pub record: Option<Record>,
    pub replaced: Vec<Record>,
    pub existing: Vec<Record>,
    pub error: Option<String>,
}

/// Make `host`'s record point at `ip` in `zone_id`: create it, find it
/// already there, or report what's in the way. With `replace`, the records
/// in the way are deleted first. Never touches records of other names or
/// types than A, AAAA and CNAME.
pub(crate) async fn ensure<C: Cloudflare>(
    cf: &C,
    zone_id: &str,
    host: &str,
    ip: &str,
    proxied: bool,
    replace: bool,
    marker: &str,
) -> Result<Outcome, CfError> {
    let found = cf.records(zone_id, host).await?;
    let mut replaced = vec![];
    let have = match plan("A", ip, host, &found) {
        Plan::Conflict(others) if !replace => {
            return Ok(Outcome {
                status: "conflict",
                record: None,
                replaced,
                existing: others,
                error: None,
            });
        }
        Plan::Conflict(others) => {
            for r in &others {
                match cf.delete(zone_id, &r.id).await {
                    Ok(()) | Err(CfError::NotFound) => replaced.push(r.clone()),
                    Err(e) => return Err(e),
                }
            }
            // What's left is the record already pointing there, if any.
            found.into_iter().find(|r| {
                !others.iter().any(|o| o.id == r.id)
                    && r.kind == "A"
                    && r.content.eq_ignore_ascii_case(ip)
                    && same_name(&r.name, host)
            })
        }
        Plan::Have(r) => Some(r),
        Plan::Create => None,
    };
    if let Some(r) = have {
        let status = if ours(&r, marker, host) {
            "created"
        } else {
            "exists"
        };
        return Ok(Outcome {
            status,
            record: Some(r),
            replaced,
            existing: vec![],
            error: None,
        });
    }
    let r = cf
        .create(
            zone_id,
            &NewRecord {
                kind: "A".into(),
                name: host.to_owned(),
                content: ip.to_owned(),
                proxied,
                comment: marker.to_owned(),
            },
        )
        .await?;
    Ok(Outcome {
        status: "created",
        record: Some(r),
        replaced,
        existing: vec![],
        error: None,
    })
}

/// Create the DNS record of a service's domain in the team's Cloudflare
/// account.
pub async fn create_record(
    s: &AppState,
    tx: &mut Transaction<'_, Postgres>,
    user: &User,
    service_id: &str,
    domain_id: &str,
    proxied: bool,
    replace: bool,
) -> ApiResult<Value> {
    let team_id = team_of(tx, "services", service_id).await?;
    require_role(
        tx,
        user,
        &team_id,
        role_for(if replace {
            Action::ReplaceRecord
        } else {
            Action::CreateRecord
        }),
    )
    .await?;
    let d = domain(tx, service_id, domain_id).await?;
    if crate::freedomain::ip_in(&d.host).is_some() {
        return Err(ApiError::bad_request("A free domain needs no DNS record."));
    }
    let a = account(s, tx, &team_id).await?.ok_or_else(no_account)?;
    let zone = zone_for(&d.host, &a.zones).cloned().ok_or_else(|| {
        ApiError::bad_request(format!("No connected Cloudflare zone covers {}.", d.host))
    })?;
    if !zone.can_edit {
        return Err(ApiError::bad_request(format!(
            "The Cloudflare token can't edit DNS in {}. Give it DNS:Edit, then test it under Integrations.",
            zone.name
        )));
    }
    let mut row = Row {
        zone_id: zone.id.clone(),
        zone_name: zone.name.clone(),
        record_id: None,
        kind: "A".into(),
        content: String::new(),
        proxied: false,
        proxy: proxied,
        status: "failed",
        error: None,
        existing: vec![],
    };
    // Where the dashboard's DNS box points it: the cluster's public IPv4.
    let ip = crate::freedomain::load(tx, &d.cluster_id)
        .await?
        .and_then(|(_, c)| c.public_ip());
    let Some(ip) = ip else {
        row.error = Some("The cluster has no public IP address to point it at.".into());
        save_row(tx, service_id, domain_id, &d, &row).await?;
        return Ok(json!({ "ok": false, "status": "failed", "error": row.error }));
    };
    row.content = ip.to_string();
    let marker = marker(&install_id(tx).await?);
    let cf = Http::new(&a.token)?;
    // Proxied once the certificate is issued (proxy_ready), unless it
    // already is or doesn't come from Let's Encrypt.
    let now = proxied && proxy_now(&d.spec);
    let out = match ensure(&cf, &zone.id, &d.host, &row.content, now, replace, &marker).await {
        Ok(o) => o,
        Err(e) => Outcome {
            status: "failed",
            record: None,
            replaced: vec![],
            existing: vec![],
            error: Some(e.to_string()),
        },
    };
    row.status = out.status;
    row.error = out.error.clone();
    row.existing = out.existing.clone();
    if let Some(r) = &out.record {
        row.proxied = r.proxied;
        row.record_id = (out.status == "created").then(|| r.id.clone());
        // Only Pyo's own record is ever changed later.
        row.proxy = proxied && row.record_id.is_some();
        if row.proxy && !r.proxied && now {
            // Ours, found already there DNS-only: proxied now.
            match cf.set_proxied(&zone.id, &r.id, true).await {
                Ok(()) => row.proxied = true,
                Err(e) => row.error = Some(e.to_string()),
            }
        }
    }
    save_row(tx, service_id, domain_id, &d, &row).await?;
    let title = match out.status {
        "created" if !out.replaced.is_empty() => {
            format!("Replaced the DNS record of {} in Cloudflare", d.host)
        }
        "created" => format!("Created the DNS record of {} in Cloudflare", d.host),
        "exists" => format!("{} already points to the cluster in Cloudflare", d.host),
        "conflict" => format!("{} has another DNS record in Cloudflare", d.host),
        _ => format!("Couldn't create the DNS record of {} in Cloudflare", d.host),
    };
    log_activity(
        tx,
        &team_id,
        &user.name,
        "domain.dns_record",
        match out.status {
            "created" | "exists" => "success",
            "conflict" => "warning",
            _ => "failure",
        },
        &title,
        json!({ "kind": "service", "id": service_id, "label": d.host }),
        json!({ "serviceId": service_id }),
    )
    .await?;
    Ok(json!({
        "ok": matches!(out.status, "created" | "exists"),
        "status": out.status,
        "type": row.kind,
        "content": row.content,
        "proxied": row.proxied,
        "proxy": row.proxy,
        "existing": out.existing,
        "replaced": out.replaced,
        "error": row.error,
    }))
}

/// Delete the record of `row` if, at Cloudflare, it's still the one this
/// installation made for the hostname. Gone already counts as deleted.
pub(crate) async fn delete_ours<C: Cloudflare>(
    cf: &C,
    zone_id: &str,
    record_id: &str,
    host: &str,
    marker: &str,
) -> Result<bool, CfError> {
    let r = match cf.record(zone_id, record_id).await {
        Ok(r) => r,
        Err(CfError::NotFound) => return Ok(true),
        Err(e) => return Err(e),
    };
    if !ours(&r, marker, host) {
        return Ok(false);
    }
    match cf.delete(zone_id, record_id).await {
        Ok(()) | Err(CfError::NotFound) => Ok(true),
        Err(e) => Err(e),
    }
}

/// Delete the DNS record Pyo made for a domain (only that one).
pub async fn delete_record(
    s: &AppState,
    tx: &mut Transaction<'_, Postgres>,
    user: &User,
    service_id: &str,
    domain_id: &str,
) -> ApiResult<Value> {
    let team_id = team_of(tx, "services", service_id).await?;
    require_role(tx, user, &team_id, role_for(Action::DeleteRecord)).await?;
    let row: Option<(String, Option<String>, String)> = sqlx::query_as(
        "SELECT zone_id, record_id, hostname FROM pyo.dns_records WHERE service_id = $1 AND domain_id = $2",
    )
    .bind(service_id)
    .bind(domain_id)
    .fetch_optional(&mut **tx)
    .await?;
    let Some((zone_id, Some(record_id), host)) = row else {
        return Err(ApiError::bad_request(
            "Pyo didn't make this domain's DNS record, so it leaves it.",
        ));
    };
    let a = account(s, tx, &team_id).await?.ok_or_else(no_account)?;
    let marker = marker(&install_id(tx).await?);
    let deleted = delete_ours(&Http::new(&a.token)?, &zone_id, &record_id, &host, &marker)
        .await
        .map_err(api_error)?;
    if !deleted {
        return Err(ApiError::conflict(
            "That record was changed outside Pyo, so Pyo leaves it.",
        ));
    }
    sqlx::query("DELETE FROM pyo.dns_records WHERE service_id = $1 AND domain_id = $2")
        .bind(service_id)
        .bind(domain_id)
        .execute(&mut **tx)
        .await?;
    log_activity(
        tx,
        &team_id,
        &user.name,
        "domain.dns_record",
        "info",
        &format!("Deleted the DNS record of {host} in Cloudflare"),
        json!({ "kind": "service", "id": service_id, "label": host }),
        json!({ "serviceId": service_id }),
    )
    .await?;
    Ok(json!({ "ok": true }))
}

/// Records waiting to be proxied: turned on once their domain's
/// certificate is issued (or doesn't come from Let's Encrypt). Runs with
/// the DNS watcher.
pub async fn proxy_ready(s: &AppState) -> ApiResult<()> {
    let due: Vec<(String, String, String, String, String, Value)> = sqlx::query_as(
        "SELECT r.service_id, r.domain_id, r.team_id, r.zone_id, r.record_id, d
           FROM pyo.dns_records r JOIN pyo.services a ON a.id = r.service_id, jsonb_array_elements(a.spec->'domains') d
          WHERE r.proxy AND NOT r.proxied AND r.status = 'created' AND r.record_id IS NOT NULL
            AND d->>'id' = r.domain_id AND NOT a.deleting
          LIMIT 50",
    )
    .fetch_all(&s.db)
    .await?;
    let mut tokens: HashMap<String, Option<String>> = HashMap::new();
    for (service_id, domain_id, team_id, zone_id, record_id, d) in due {
        if !proxy_now(&d) {
            continue;
        }
        if !tokens.contains_key(&team_id) {
            let mut tx = s.db.begin().await?;
            let t = account(s, &mut tx, &team_id)
                .await
                .ok()
                .flatten()
                .map(|a| a.token);
            tx.commit().await?;
            tokens.insert(team_id.clone(), t);
        }
        let Some(Some(token)) = tokens.get(&team_id) else {
            continue;
        };
        let (proxied, error) = match Http::new(token)?
            .set_proxied(&zone_id, &record_id, true)
            .await
        {
            Ok(()) => (true, None),
            Err(e) => (false, Some(e.to_string())),
        };
        sqlx::query(
            "UPDATE pyo.dns_records SET proxied = $3, error = $4, updated_at = now() WHERE service_id = $1 AND domain_id = $2",
        )
        .bind(&service_id)
        .bind(&domain_id)
        .bind(proxied)
        .bind(&error)
        .execute(&s.db)
        .await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests;
#[cfg(test)]
mod fake;
#[cfg(test)]
mod http_tests;
