//! Wildcard certificates for sandbox servers' edge proxies: Pyo gets one
//! for `*.<edge domain>` from Let's Encrypt with a DNS-01 challenge in the
//! team's Cloudflare connection, and puts it on the server.
//!
//! - Pyo gets the certificate, never the sandbox server. Sandbox servers
//!   run untrusted code, so the Cloudflare token stays in Pyo.
//! - Only for an edge domain in a zone the Cloudflare connection can edit.
//!   Elsewhere the certificate goes on the server by hand, as before.
//! - The key is sealed like Pyo's other credentials (secrets.rs). It goes
//!   to the server over the tailnet with `SetEdgeCert` and an admin token.
//!   The daemon writes it with mode 0600 and serves it at once.
//! - The runner ([`run`]) renews a certificate 30 days before it runs out
//!   (or a third of its lifetime, when that's shorter), puts it back on a
//!   server that reports another one, and does nothing while no server
//!   has an edge domain.
//! - `PYO_ACME_DIRECTORY` picks the ACME server: `staging` for Let's
//!   Encrypt's staging, or a directory URL. Let's Encrypt otherwise.
//! - The wildcard record `*.<domain>` (A, and AAAA with a public IPv6
//!   address) can be made too, when asked ([`create_dns`]), pointing at
//!   the public addresses the server reports. Records already there are
//!   never changed.

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use axum::body::Bytes;
use axum::http::StatusCode;
use instant_acme::{
    Account, AccountCredentials, AuthorizationStatus, BodyWrapper, BytesResponse, ChallengeType,
    HttpClient, Identifier, LetsEncrypt, NewAccount, NewOrder, OrderStatus, RetryPolicy,
};
use pyo_proto::sandbox::{self as proto, DaemonCommand, DaemonResult, SecretPem};
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::PgPool;

use super::{Need, daemon::Daemon, keys};
use crate::{
    AppState,
    auth::User,
    cloudflare::{self, CfError, Cloudflare, NewRecord, Record, Zone},
    error::{ApiError, ApiResult},
    ops::{Role, log_activity, require_role},
};

/// How long before it runs out a certificate is renewed, at most.
const RENEW_BEFORE: Duration = Duration::from_secs(30 * 86_400);
/// An issue that hasn't finished after this long was cut off (Pyo
/// restarted): it may start again.
const STALE_SECS: f64 = 15.0 * 60.0;
/// A failed renewal is tried again after this long.
const RETRY_SECS: f64 = 6.0 * 3600.0;

// ── The ACME server ─────────────────────────────────────────────────────────

/// The ACME directory: `PYO_ACME_DIRECTORY` ("staging", "production" or a
/// URL), Let's Encrypt by default.
pub fn directory_url() -> String {
    directory_from(std::env::var("PYO_ACME_DIRECTORY").ok().as_deref())
}

fn directory_from(v: Option<&str>) -> String {
    match v.map(str::trim).unwrap_or_default() {
        "" | "production" => LetsEncrypt::Production.url().to_owned(),
        "staging" => LetsEncrypt::Staging.url().to_owned(),
        url => url.to_owned(),
    }
}

/// instant-acme's HTTP, through reqwest.
struct Reqwest(reqwest::Client);

fn http_error(e: impl std::error::Error + Send + Sync + 'static) -> instant_acme::Error {
    instant_acme::Error::Other(Box::new(e))
}

impl HttpClient for Reqwest {
    fn request(
        &self,
        req: axum::http::Request<BodyWrapper<Bytes>>,
    ) -> Pin<Box<dyn Future<Output = Result<BytesResponse, instant_acme::Error>> + Send>> {
        let client = self.0.clone();
        Box::pin(async move {
            let (parts, body) = req.into_parts();
            let body = axum::body::to_bytes(axum::body::Body::new(body), 1 << 20)
                .await
                .map_err(http_error)?;
            let res = client
                .request(parts.method, parts.uri.to_string())
                .headers(parts.headers)
                .body(body)
                .send()
                .await
                .map_err(http_error)?;
            let mut head = axum::http::Response::builder().status(res.status());
            if let Some(h) = head.headers_mut() {
                *h = res.headers().clone();
            }
            let bytes = res.bytes().await.map_err(http_error)?;
            let (parts, ()) = head.body(()).map_err(http_error)?.into_parts();
            Ok(BytesResponse {
                parts,
                body: Box::new(bytes),
            })
        })
    }
}

/// Pyo's ACME account at `directory`: made once and kept sealed in the
/// settings, one per directory.
async fn account(s: &AppState, client: reqwest::Client, directory: &str) -> ApiResult<Account> {
    let saved: Option<Value> =
        sqlx::query_scalar("SELECT value FROM pyo.settings WHERE key = 'acme_account'")
            .fetch_optional(&s.db)
            .await?;
    if let Some(v) = saved
        && v["directory"] == json!(directory)
        && let Some(sealed) = v["credentials"].as_str()
        && let Ok(plain) = s.sealer.open(sealed)
        && let Ok(creds) = serde_json::from_str::<AccountCredentials>(&plain)
    {
        return Account::builder_with_http(Box::new(Reqwest(client)))
            .from_credentials(creds)
            .await
            .map_err(acme_error);
    }
    let (account, creds) = Account::builder_with_http(Box::new(Reqwest(client)))
        .create(
            &NewAccount {
                contact: &[],
                terms_of_service_agreed: true,
                only_return_existing: false,
            },
            directory.to_owned(),
            None,
        )
        .await
        .map_err(acme_error)?;
    let sealed = s
        .sealer
        .seal(&serde_json::to_string(&creds).map_err(anyhow::Error::from)?)?;
    sqlx::query(
        "INSERT INTO pyo.settings (key, value) VALUES ('acme_account', $1)
         ON CONFLICT (key) DO UPDATE SET value = $1",
    )
    .bind(json!({ "directory": directory, "credentials": sealed }))
    .execute(&s.db)
    .await?;
    Ok(account)
}

fn acme_error(e: instant_acme::Error) -> ApiError {
    ApiError::new(
        StatusCode::BAD_GATEWAY,
        format!("Let's Encrypt refused: {}", short(&e.to_string())),
    )
}

fn short(m: &str) -> String {
    m.chars().take(300).collect()
}

// ── Getting a certificate ──────────────────────────────────────────────────

/// Waits until a TXT record can be seen, so the ACME server finds it.
pub(crate) trait Propagation {
    async fn wait(&self, name: &str, value: &str);
}

/// Asks Cloudflare's resolver over HTTPS until the record shows, for two
/// minutes at most. The zone is at Cloudflare, so that's quick.
struct DnsOverHttps(reqwest::Client);

impl Propagation for DnsOverHttps {
    async fn wait(&self, name: &str, value: &str) {
        for _ in 0..24 {
            let found = self
                .0
                .get("https://cloudflare-dns.com/dns-query")
                .query(&[("name", name), ("type", "TXT")])
                .header("accept", "application/dns-json")
                .send()
                .await
                .ok()
                .map(|r| r.json::<Value>());
            if let Some(f) = found
                && let Ok(v) = f.await
                && v["Answer"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .any(|a| a["data"].as_str().map(|d| d.trim_matches('"')) == Some(value))
            {
                return;
            }
            tokio::time::sleep(Duration::from_secs(5)).await;
        }
    }
}

/// The DNS-01 record for a wildcard of `domain`.
pub fn challenge_name(domain: &str) -> String {
    format!("_acme-challenge.{domain}")
}

/// A certificate as issued: the PEM chain and its key.
pub(crate) struct Issued {
    pub chain: String,
    pub key: String,
}

/// Gets a certificate for `*.<domain>`: an order, the DNS-01 TXT record in
/// `zone_id`, the challenge, then the certificate. The TXT records it made
/// are deleted whatever happens.
pub(crate) async fn obtain<C: Cloudflare, P: Propagation>(
    account: &Account,
    cf: &C,
    zone_id: &str,
    marker: &str,
    domain: &str,
    wait: &P,
    retry: &RetryPolicy,
) -> Result<Issued, String> {
    let name = challenge_name(domain);
    // TXT records a cut-off attempt left.
    if let Ok(found) = cf.records(zone_id, &name).await {
        for r in found
            .iter()
            .filter(|r| r.kind == "TXT" && cloudflare::ours(r, marker, &name))
        {
            let _ = cf.delete(zone_id, &r.id).await;
        }
    }
    let mut made: Vec<String> = vec![];
    let out = issue_order(account, cf, zone_id, marker, domain, wait, retry, &mut made).await;
    for id in &made {
        if let Err(e) = cf.delete(zone_id, id).await
            && e != CfError::NotFound
        {
            tracing::warn!(%domain, error = %e, "edge certificate: couldn't delete the challenge record");
        }
    }
    out
}

#[allow(clippy::too_many_arguments)]
async fn issue_order<C: Cloudflare, P: Propagation>(
    account: &Account,
    cf: &C,
    zone_id: &str,
    marker: &str,
    domain: &str,
    wait: &P,
    retry: &RetryPolicy,
    made: &mut Vec<String>,
) -> Result<Issued, String> {
    let le = |e: instant_acme::Error| format!("Let's Encrypt refused: {}", short(&e.to_string()));
    let name = challenge_name(domain);
    let ids = [Identifier::Dns(format!("*.{domain}"))];
    let mut order = account.new_order(&NewOrder::new(&ids)).await.map_err(le)?;
    let mut authzs = order.authorizations();
    while let Some(a) = authzs.next().await {
        let mut a = a.map_err(le)?;
        match a.status {
            AuthorizationStatus::Pending => {}
            AuthorizationStatus::Valid => continue,
            s => return Err(format!("Let's Encrypt's authorization is {s:?}.")),
        }
        let mut ch = a
            .challenge(ChallengeType::Dns01)
            .ok_or("Let's Encrypt offered no DNS challenge.")?;
        let value = ch.key_authorization().dns_value();
        let r = cf
            .create(
                zone_id,
                &NewRecord {
                    kind: "TXT".into(),
                    name: name.clone(),
                    content: format!("\"{value}\""),
                    proxied: false,
                    comment: marker.to_owned(),
                },
            )
            .await
            .map_err(|e| e.to_string())?;
        made.push(r.id);
        wait.wait(&name, &value).await;
        ch.set_ready().await.map_err(le)?;
    }
    match order.poll_ready(retry).await.map_err(le)? {
        OrderStatus::Ready => {}
        s => {
            let why = order
                .state()
                .error
                .as_ref()
                .and_then(|p| p.detail.clone())
                .unwrap_or_else(|| format!("The order is {s:?}."));
            return Err(format!(
                "Let's Encrypt couldn't check {name}: {}",
                short(&why)
            ));
        }
    }
    let key = order.finalize().await.map_err(le)?;
    let chain = order.poll_certificate(retry).await.map_err(le)?;
    Ok(Issued { chain, key })
}

// ── Records and views ──────────────────────────────────────────────────────

#[derive(sqlx::FromRow)]
struct Server {
    team_id: Option<String>,
    name: String,
    edge_domain: Option<String>,
    capability: Option<Value>,
}

async fn server(db: &PgPool, id: &str) -> ApiResult<Server> {
    sqlx::query_as(
        "SELECT team_id, name, edge_domain, capability FROM pyo.sandbox_servers WHERE id = $1",
    )
    .bind(id)
    .fetch_optional(db)
    .await?
    .ok_or_else(|| ApiError::not_found("Sandbox server"))
}

impl Server {
    fn report(&self) -> proto::CapabilityReport {
        self.capability
            .clone()
            .and_then(|c| serde_json::from_value(c).ok())
            .unwrap_or_default()
    }
}

/// The servers' edges, as `edge` on each server view: its domain, the
/// certificate's state, the Cloudflare zone that covers it and the
/// wildcard record. `team_ids` are the viewer's teams: an installation's
/// server is offered the first of their connections that covers its
/// domain.
pub async fn attach(db: &PgPool, team_ids: &[String], servers: &mut [Value]) -> sqlx::Result<()> {
    let ids: Vec<String> = servers
        .iter()
        .filter(|v| v["edgeDomain"].is_string())
        .filter_map(|v| v["id"].as_str().map(str::to_owned))
        .collect();
    if ids.is_empty() {
        return Ok(());
    }
    let rows: Vec<(String, Value)> = sqlx::query_as(
        "SELECT server_id, jsonb_build_object('domain', domain, 'dnsTeamId', dns_team_id, 'state', state,
                'notBefore', pyo.ms(not_before), 'notAfter', pyo.ms(not_after), 'issuedAt', pyo.ms(issued_at),
                'error', error, 'hasCert', cert_pem IS NOT NULL, 'fingerprint', fingerprint, 'dns', dns,
                'stale', state = 'issuing' AND updated_at < now() - make_interval(secs => $2))
           FROM pyo.sandbox_edges WHERE server_id = ANY($1)",
    )
    .bind(&ids)
    .bind(STALE_SECS)
    .fetch_all(db)
    .await?;
    let accounts: Vec<(String, Value)> = sqlx::query_as(
        "SELECT team_id, zones FROM pyo.cloudflare_accounts WHERE team_id = ANY($1)
            OR team_id IN (SELECT team_id FROM pyo.sandbox_servers WHERE id = ANY($2) AND team_id IS NOT NULL)
            OR team_id IN (SELECT dns_team_id FROM pyo.sandbox_edges WHERE server_id = ANY($2))",
    )
    .bind(team_ids)
    .bind(&ids)
    .fetch_all(db)
    .await?;
    let zones = |team: &str| -> Vec<Zone> {
        accounts
            .iter()
            .find(|(t, _)| t == team)
            .and_then(|(_, z)| serde_json::from_value(z.clone()).ok())
            .unwrap_or_default()
    };
    for v in servers.iter_mut() {
        let Some(domain) = v["edgeDomain"].as_str().map(str::to_owned) else {
            continue;
        };
        let id = v["id"].as_str().unwrap_or_default();
        let row = rows.iter().find(|(s, _)| s == id).map(|(_, r)| r.clone());
        let report: proto::CapabilityReport =
            serde_json::from_value(v["capability"].clone()).unwrap_or_default();
        // Whose Cloudflare: the server's team's, the one picked before, or
        // the first of the viewer's that covers the domain.
        let picked = row
            .as_ref()
            .and_then(|r| r["dnsTeamId"].as_str().map(str::to_owned));
        let candidates: Vec<String> = match v["teamId"].as_str() {
            Some(t) => vec![t.to_owned()],
            None => picked.into_iter().chain(team_ids.iter().cloned()).collect(),
        };
        let covered = candidates
            .iter()
            .find_map(|t| cloudflare::zone_for(&domain, &zones(t)).map(|z| (t.clone(), z.clone())));
        let cert = match &row {
            Some(r) if r["state"] != "none" || r["hasCert"] == true => {
                let mut state = r["state"].clone();
                let mut error = r["error"].clone();
                if r["stale"] == true {
                    state = json!("failed");
                    error = json!("It was cut off. Try again.");
                }
                let domain_moved = r["domain"].as_str() != Some(domain.as_str());
                json!({
                    "state": if domain_moved { json!("none") } else { state },
                    "notAfter": r["notAfter"], "issuedAt": r["issuedAt"],
                    "error": error,
                    "served": report.edge_cert.is_some() && report.edge_cert.as_deref() == r["fingerprint"].as_str(),
                })
            }
            _ => json!({ "state": "none" }),
        };
        let ips: Vec<Value> = report
            .public_ips
            .iter()
            .map(|ip| json!({ "type": if ip.is_ipv4() { "A" } else { "AAAA" }, "content": ip.to_string() }))
            .collect();
        v["edge"] = json!({
            "domain": domain,
            "https": report.edge_url.as_deref().is_some_and(|u| u.starts_with("https://")),
            "cert": cert,
            "cloudflare": covered.map(|(t, z)| json!({ "teamId": t, "zone": z.name, "canEdit": z.can_edit })),
            "dns": {
                "name": format!("*.{domain}"),
                "records": ips,
                "made": row.as_ref().map(|r| r["dns"].clone()).unwrap_or(Value::Null),
            },
        });
    }
    Ok(())
}

// ── Asking for a certificate and a record ──────────────────────────────────

#[derive(Debug, Clone, Default, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EdgeRequest {
    /// The team whose Cloudflare connection to use, for one of the
    /// installation's servers. A team's own server always uses its team's.
    #[serde(default)]
    pub team_id: Option<String>,
}

/// Who may ask, and whose Cloudflare connection answers: a team's server
/// uses its team's (owners and admins), the installation's servers a team
/// its admin also administers.
async fn allowed(s: &AppState, user: &User, srv: &Server, req: &EdgeRequest) -> ApiResult<String> {
    let mut tx = s.db.begin().await?;
    let team = match &srv.team_id {
        Some(t) => {
            super::require(&mut tx, user, t, Need::Manage).await?;
            t.clone()
        }
        None => {
            super::require_install_admin(user)?;
            let t = req.team_id.clone().ok_or_else(|| {
                ApiError::bad_request("Pick the team whose Cloudflare connection to use.")
            })?;
            require_role(&mut tx, user, &t, Role::Admin).await?;
            t
        }
    };
    tx.commit().await?;
    Ok(team)
}

/// The team's Cloudflare token and the zone that covers `domain`, which
/// it must be able to edit.
async fn covering(s: &AppState, team: &str, domain: &str) -> ApiResult<(String, Zone)> {
    let mut tx = s.db.begin().await?;
    let a = cloudflare::account(s, &mut tx, team)
        .await?
        .ok_or_else(|| ApiError::bad_request("Connect Cloudflare under Integrations first."))?;
    tx.commit().await?;
    let zone = cloudflare::zone_for(domain, &a.zones)
        .cloned()
        .ok_or_else(|| {
            ApiError::bad_request(format!(
                "No connected Cloudflare zone covers {domain}. Put a certificate on the server by hand."
            ))
        })?;
    if !zone.can_edit {
        return Err(ApiError::bad_request(format!(
            "The Cloudflare token can't edit DNS in {}. Give it DNS:Edit, then test it under Integrations.",
            zone.name
        )));
    }
    Ok((a.token, zone))
}

async fn install_marker(s: &AppState) -> ApiResult<String> {
    let mut tx = s.db.begin().await?;
    let id = cloudflare::install_id(&mut tx).await?;
    tx.commit().await?;
    Ok(cloudflare::marker(&id))
}

/// Gets (or renews) the server's certificate in the background. Its state
/// shows on the server.
pub async fn issue(s: &AppState, user: &User, id: &str, req: EdgeRequest) -> ApiResult<Value> {
    let srv = server(&s.db, id).await?;
    let team = allowed(s, user, &srv, &req).await?;
    let Some(domain) = srv.edge_domain.clone() else {
        return Err(ApiError::bad_request(
            "This server has no edge domain. Add it again with one.",
        ));
    };
    covering(s, &team, &domain).await?;
    if !s.sealer.enabled() {
        return Err(ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "This installation has no PYO_SECRET_KEY, so it can't keep a certificate's key.",
        ));
    }
    if !start(&s.db, id, &domain, Some(&team)).await? {
        return Err(ApiError::conflict(
            "Pyo is getting this certificate already.",
        ));
    }
    if let Some(t) = &srv.team_id {
        let mut tx = s.db.begin().await?;
        log_activity(
            &mut tx,
            t,
            &user.name,
            "sandbox.server",
            "info",
            &format!("Asked for a certificate for *.{domain}"),
            json!({ "kind": "sandboxServer", "id": id, "label": srv.name }),
            json!({}),
        )
        .await?;
        tx.commit().await?;
    }
    let s2 = s.clone();
    let id2 = id.to_owned();
    tokio::spawn(async move { run_issue(&s2, &id2).await });
    Ok(json!({ "state": "issuing" }))
}

/// Marks the edge as issuing, unless it is already (and wasn't cut off).
async fn start(db: &PgPool, id: &str, domain: &str, team: Option<&str>) -> sqlx::Result<bool> {
    let n = sqlx::query(
        "INSERT INTO pyo.sandbox_edges (server_id, domain, dns_team_id, state) VALUES ($1, $2, $3, 'issuing')
         ON CONFLICT (server_id) DO UPDATE SET state = 'issuing', error = NULL, retry_at = NULL,
                dns_team_id = coalesce($3, pyo.sandbox_edges.dns_team_id), updated_at = now()
          WHERE pyo.sandbox_edges.state <> 'issuing'
             OR pyo.sandbox_edges.updated_at < now() - make_interval(secs => $4)",
    )
    .bind(id)
    .bind(domain)
    .bind(team)
    .bind(STALE_SECS)
    .execute(db)
    .await?
    .rows_affected();
    Ok(n > 0)
}

/// Gets the certificate and puts it on the server, recording how it went.
async fn run_issue(s: &AppState, id: &str) {
    let res = get_cert(s, id).await;
    let (status, title) = match &res {
        Ok(domain) => ("success", format!("Got a certificate for *.{domain}")),
        Err(e) => {
            tracing::warn!(server = %id, error = %e, "edge certificate failed");
            let _ = sqlx::query(
                "UPDATE pyo.sandbox_edges SET state = 'failed', error = $2, updated_at = now(),
                        retry_at = now() + make_interval(secs => $3) WHERE server_id = $1",
            )
            .bind(id)
            .bind(e)
            .bind(RETRY_SECS)
            .execute(&s.db)
            .await;
            (
                "failure",
                "Couldn't get a certificate for the sandbox edge".to_owned(),
            )
        }
    };
    if res.is_ok()
        && let Err(e) = deliver(s, id).await
    {
        tracing::info!(server = %id, error = %e, "edge certificate: the server takes it later");
    }
    if let Ok(srv) = server(&s.db, id).await
        && let Some(t) = &srv.team_id
        && let Ok(mut tx) = s.db.begin().await
    {
        let _ = log_activity(
            &mut tx,
            t,
            "Pyo",
            "sandbox.server",
            status,
            &title,
            json!({ "kind": "sandboxServer", "id": id, "label": srv.name }),
            json!({}),
        )
        .await;
        let _ = tx.commit().await;
    }
}

/// Gets a certificate for the server's edge domain and saves it. Returns
/// the domain, or why it failed in a sentence.
async fn get_cert(s: &AppState, id: &str) -> Result<String, String> {
    let srv = server(&s.db, id).await.map_err(|e| e.message)?;
    let domain = srv
        .edge_domain
        .clone()
        .ok_or("The server has no edge domain anymore.")?;
    let team: Option<String> =
        sqlx::query_scalar("SELECT dns_team_id FROM pyo.sandbox_edges WHERE server_id = $1")
            .bind(id)
            .fetch_optional(&s.db)
            .await
            .map_err(|e| e.to_string())?
            .flatten();
    let team = team
        .or(srv.team_id.clone())
        .ok_or("No team's Cloudflare connection is picked for it.")?;
    let (token, zone) = covering(s, &team, &domain).await.map_err(|e| e.message)?;
    let marker = install_marker(s).await.map_err(|e| e.message)?;
    let client = crate::outbound::client(
        crate::outbound::Reach::installation(),
        Duration::from_secs(30),
    )
    .map_err(|e| e.to_string())?;
    let account = account(s, client, &directory_url())
        .await
        .map_err(|e| e.message)?;
    let cf = cloudflare::Http::new(&token).map_err(|e| e.message)?;
    let doh = crate::outbound::client(
        crate::outbound::Reach { private: false },
        Duration::from_secs(10),
    )
    .map_err(|e| e.to_string())?;
    let retry = RetryPolicy::new()
        .initial_delay(Duration::from_secs(2))
        .timeout(Duration::from_secs(180));
    let issued = obtain(
        &account,
        &cf,
        &zone.id,
        &marker,
        &domain,
        &DnsOverHttps(doh),
        &retry,
    )
    .await?;
    save(s, id, &domain, &issued).await?;
    Ok(domain)
}

/// Checks an issued certificate: it's for `*.<domain>`, its key fits, and
/// it hasn't run out.
pub(crate) fn check(
    issued: &Issued,
    domain: &str,
    now: i64,
) -> Result<crate::certs::Parsed, String> {
    let p = crate::certs::parse(&issued.chain, &issued.key, now).map_err(|e| e.message)?;
    let wildcard = format!("*.{domain}");
    if !p
        .hostnames
        .iter()
        .any(|h| h.eq_ignore_ascii_case(&wildcard))
    {
        return Err(format!("The certificate isn't for {wildcard}."));
    }
    Ok(p)
}

/// Checks and keeps an issued certificate, its key sealed.
async fn save(s: &AppState, id: &str, domain: &str, issued: &Issued) -> Result<(), String> {
    let p = check(issued, domain, pyo_proto::now_ms() / 1000)?;
    let sealed = s.sealer.seal(&p.key_pem).map_err(|e| e.to_string())?;
    sqlx::query(
        "UPDATE pyo.sandbox_edges SET state = 'ready', domain = $2, cert_pem = $3, key_sealed = $4,
                fingerprint = $5, not_before = to_timestamp($6), not_after = to_timestamp($7),
                issued_at = now(), error = NULL, retry_at = NULL, updated_at = now()
          WHERE server_id = $1",
    )
    .bind(id)
    .bind(domain)
    .bind(&p.chain_pem)
    .bind(&sealed)
    .bind(&p.fingerprint)
    .bind(p.not_before as f64)
    .bind(p.not_after as f64)
    .execute(&s.db)
    .await
    .map_err(|e| e.to_string())?;
    Ok(())
}

/// Puts the saved certificate on the server.
async fn deliver(s: &AppState, id: &str) -> Result<(), String> {
    let row: Option<(Option<String>, Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT sv.address, e.cert_pem, e.key_sealed FROM pyo.sandbox_edges e
           JOIN pyo.sandbox_servers sv ON sv.id = e.server_id WHERE e.server_id = $1",
    )
    .bind(id)
    .fetch_optional(&s.db)
    .await
    .map_err(|e| e.to_string())?;
    let Some((Some(address), Some(cert), Some(sealed))) = row else {
        return Err("The server hasn't joined, or there's no certificate.".into());
    };
    let key = s
        .sealer
        .open(&sealed)
        .map_err(|_| "Pyo can't open the certificate's key.".to_owned())?;
    let signing = keys::signing_key(&s.db, &s.sealer)
        .await
        .map_err(|e| e.message)?;
    let cmd = DaemonCommand::SetEdgeCert {
        cert,
        key: SecretPem(key),
    };
    match Daemon::new(&address)
        .command(&keys::admin_token(&signing, id), &cmd)
        .await
    {
        Ok(DaemonResult::EdgeCert { .. }) => Ok(()),
        Ok(other) => Err(format!("The server answered something else: {other:?}")),
        Err(e) => Err(e.message),
    }
}

// ── The wildcard record ────────────────────────────────────────────────────

/// What became of the wildcard record.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct DnsOutcome {
    /// "created" (all there, some or all by Pyo), "exists" (all there
    /// already), "conflict" (other records answer for the name).
    pub status: &'static str,
    pub made: Vec<Record>,
    pub existing: Vec<Record>,
}

/// Makes `name` point at `want` (type, address): records already right are
/// kept, missing ones are made, and other A, AAAA or CNAME records on the
/// name stop it without changing anything.
pub(crate) async fn ensure_wildcard<C: Cloudflare>(
    cf: &C,
    zone_id: &str,
    name: &str,
    want: &[(String, String)],
    marker: &str,
) -> Result<DnsOutcome, CfError> {
    let found = cf.records(zone_id, name).await?;
    let addressing = |r: &&Record| {
        matches!(r.kind.as_str(), "A" | "AAAA" | "CNAME")
            && r.name.trim_end_matches('.').eq_ignore_ascii_case(name)
    };
    let wanted = |r: &Record| {
        want.iter()
            .any(|(k, c)| r.kind == *k && r.content.eq_ignore_ascii_case(c))
    };
    let others: Vec<Record> = found
        .iter()
        .filter(addressing)
        .filter(|r| !wanted(r))
        .cloned()
        .collect();
    if !others.is_empty() {
        return Ok(DnsOutcome {
            status: "conflict",
            made: vec![],
            existing: others,
        });
    }
    let mut made = vec![];
    for (kind, content) in want {
        if found
            .iter()
            .any(|r| r.kind == *kind && r.content.eq_ignore_ascii_case(content))
        {
            continue;
        }
        made.push(
            cf.create(
                zone_id,
                &NewRecord {
                    kind: kind.clone(),
                    name: name.to_owned(),
                    content: content.clone(),
                    proxied: false,
                    comment: marker.to_owned(),
                },
            )
            .await?,
        );
    }
    Ok(DnsOutcome {
        status: if made.is_empty() { "exists" } else { "created" },
        made,
        existing: vec![],
    })
}

/// Makes the wildcard record `*.<edge domain>` in Cloudflare, pointing at
/// the server's public addresses.
pub async fn create_dns(s: &AppState, user: &User, id: &str, req: EdgeRequest) -> ApiResult<Value> {
    let srv = server(&s.db, id).await?;
    let team = allowed(s, user, &srv, &req).await?;
    let Some(domain) = srv.edge_domain.clone() else {
        return Err(ApiError::bad_request(
            "This server has no edge domain. Add it again with one.",
        ));
    };
    let (token, zone) = covering(s, &team, &domain).await?;
    let want: Vec<(String, String)> = srv
        .report()
        .public_ips
        .iter()
        .map(|ip| {
            let kind = if ip.is_ipv4() { "A" } else { "AAAA" };
            (kind.to_owned(), ip.to_string())
        })
        .collect();
    if want.is_empty() {
        return Err(ApiError::conflict(
            "The server has no public IP address to point it at. Make the record by hand.",
        ));
    }
    let name = format!("*.{domain}");
    let marker = install_marker(s).await?;
    let out = ensure_wildcard(
        &cloudflare::Http::new(&token)?,
        &zone.id,
        &name,
        &want,
        &marker,
    )
    .await
    .map_err(|e| ApiError::bad_request(e.to_string()))?;
    let dns = json!({
        "zoneId": zone.id,
        "status": out.status,
        "records": out.made.iter().map(|r| json!({ "id": r.id, "type": r.kind, "content": r.content })).collect::<Vec<_>>(),
        "existing": out.existing.iter().map(|r| json!({ "type": r.kind, "content": r.content })).collect::<Vec<_>>(),
    });
    sqlx::query(
        "INSERT INTO pyo.sandbox_edges (server_id, domain, dns_team_id, dns) VALUES ($1, $2, $3, $4)
         ON CONFLICT (server_id) DO UPDATE SET dns = $4, dns_team_id = $3, updated_at = now()",
    )
    .bind(id)
    .bind(&domain)
    .bind(&team)
    .bind(&dns)
    .execute(&s.db)
    .await?;
    if let Some(t) = &srv.team_id {
        let mut tx = s.db.begin().await?;
        log_activity(
            &mut tx,
            t,
            &user.name,
            "sandbox.server",
            if out.status == "conflict" {
                "warning"
            } else {
                "success"
            },
            &match out.status {
                "conflict" => format!("{name} has another DNS record in Cloudflare"),
                "exists" => format!("{name} already points to {} in Cloudflare", srv.name),
                _ => format!("Created the DNS record of {name} in Cloudflare"),
            },
            json!({ "kind": "sandboxServer", "id": id, "label": srv.name }),
            json!({}),
        )
        .await?;
        tx.commit().await?;
    }
    Ok(json!({
        "ok": out.status != "conflict",
        "status": out.status,
        "existing": dns["existing"],
    }))
}

// ── The runner's part ──────────────────────────────────────────────────────

/// Whether a certificate is due for renewal: 30 days before it runs out,
/// or a third of its lifetime when that's shorter.
pub fn due(not_before: i64, not_after: i64, now: i64) -> bool {
    let life = (not_after - not_before).max(0);
    let before = (RENEW_BEFORE.as_secs() as i64).min(life / 3);
    now >= not_after - before
}

/// One round, after the servers' reports: puts certificates back on
/// servers that serve another one, and renews those due. Idle while no
/// server has an edge domain.
pub async fn run(s: &AppState) -> anyhow::Result<()> {
    let any: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM pyo.sandbox_servers WHERE edge_domain IS NOT NULL)",
    )
    .fetch_one(&s.db)
    .await?;
    if !any {
        return Ok(());
    }
    // Servers that serve another certificate than Pyo's (a reinstall, or
    // the push after issuing didn't reach them).
    let mismatched: Vec<String> = sqlx::query_scalar(
        "SELECT e.server_id FROM pyo.sandbox_edges e JOIN pyo.sandbox_servers sv ON sv.id = e.server_id
          WHERE e.cert_pem IS NOT NULL AND e.domain = sv.edge_domain AND sv.state = 'ready'
            AND e.not_after > now()
            AND (sv.capability->>'edgeCert') IS DISTINCT FROM e.fingerprint
          LIMIT 20",
    )
    .fetch_all(&s.db)
    .await?;
    for id in mismatched {
        match deliver(s, &id).await {
            Ok(()) => tracing::info!(server = %id, "edge certificate put back on the server"),
            Err(e) => {
                tracing::debug!(server = %id, error = %e, "edge certificate: couldn't put it on the server")
            }
        }
    }
    // Renewals: ready ones close to running out, and failed renewals whose
    // wait is over.
    let rows: Vec<(String, String, i64, i64)> = sqlx::query_as(
        "SELECT e.server_id, e.domain, extract(epoch FROM e.not_before)::bigint, extract(epoch FROM e.not_after)::bigint
           FROM pyo.sandbox_edges e JOIN pyo.sandbox_servers sv ON sv.id = e.server_id
          WHERE e.cert_pem IS NOT NULL AND e.domain = sv.edge_domain
            AND (e.state = 'ready' OR (e.state = 'failed' AND e.retry_at < now()))
            AND e.not_after < now() + make_interval(secs => $1)",
    )
    .bind(RENEW_BEFORE.as_secs() as f64)
    .fetch_all(&s.db)
    .await?;
    let now = pyo_proto::now_ms() / 1000;
    for (id, domain, nb, na) in rows {
        if !due(nb, na, now) || !start(&s.db, &id, &domain, None).await? {
            continue;
        }
        tracing::info!(server = %id, %domain, "renewing the edge certificate");
        let s2 = s.clone();
        tokio::spawn(async move { run_issue(&s2, &id).await });
    }
    Ok(())
}

#[cfg(test)]
mod tests;
