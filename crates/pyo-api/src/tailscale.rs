//! Tailscale: the installation's OAuth client (Integrations › Tailscale) and
//! what Pyo does with it.
//!
//! - **The client** is installation-wide, set by an admin, kept sealed in
//!   `pyo.settings` ('tailscale'). Only this process uses it. Scopes:
//!   `devices:core` and `auth_keys` (both with `tag:pyo`), and `services`.
//! - **Joining**: each join command carries a fresh auth key from it:
//!   single-use, preauthorized, `tag:pyo`, for an hour ([`join_key`]).
//! - **Names on the tailnet** are Tailscale Services ("svc:blog" is
//!   "blog.<tailnet>.ts.net"): Pyo's own (`pyo`, `registry`) on the cluster
//!   it runs on, and services' tailnet domains on theirs. [`tick`] keeps
//!   them as the domains say: makes each (marked with this installation's
//!   ID, [`marker`]), approves the cluster's servers as its hosts, deletes
//!   the ones no longer used, and never touches one it didn't make. The
//!   servers do the rest (the agent's tailnet.rs).

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::PgPool;

use crate::AppState;
use crate::error::{ApiError, ApiResult};

const API: &str = "https://api.tailscale.com/api/v2";
/// The tag Pyo's servers and Services carry.
pub const TAG: &str = "tag:pyo";
/// Names Pyo keeps for itself: services' domains can't take them.
pub const RESERVED: [&str; 2] = ["pyo", "registry"];

/// The saved client, and Pyo's own names on the tailnet.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Settings {
    pub client_id: String,
    pub secret_sealed: String,
    /// The secret's last characters, to tell it apart.
    pub hint: String,
    /// The dashboard's name ("pyo"), once picked or set.
    #[serde(default)]
    pub dashboard_name: Option<String>,
    /// The registry's name ("registry").
    #[serde(default)]
    pub registry_name: Option<String>,
    /// The tailnet's MagicDNS suffix ("tail1234.ts.net"), as last seen.
    #[serde(default)]
    pub suffix: Option<String>,
}

pub async fn settings(db: &PgPool) -> ApiResult<Option<Settings>> {
    let v: Option<Value> =
        sqlx::query_scalar("SELECT value FROM pyo.settings WHERE key = 'tailscale'")
            .fetch_optional(db)
            .await?;
    Ok(v.and_then(|v| serde_json::from_value(v).ok()))
}

pub async fn save(db: &PgPool, s: &Settings) -> ApiResult<()> {
    sqlx::query("INSERT INTO pyo.settings (key, value) VALUES ('tailscale', $1) ON CONFLICT (key) DO UPDATE SET value = excluded.value, updated_at = now()")
        .bind(serde_json::to_value(s).map_err(anyhow::Error::from)?)
        .execute(db)
        .await?;
    Ok(())
}

/// This installation's ID (made once, kept with Pyo's data, so a restore
/// keeps it): what marks the Services it made.
pub async fn install_id(db: &PgPool) -> ApiResult<String> {
    let id = format!("{:032x}", rand::random::<u128>());
    sqlx::query("INSERT INTO pyo.settings (key, value) VALUES ('install', jsonb_build_object('id', $1::text)) ON CONFLICT (key) DO NOTHING")
        .bind(&id)
        .execute(db)
        .await?;
    let v: String =
        sqlx::query_scalar("SELECT value->>'id' FROM pyo.settings WHERE key = 'install'")
            .fetch_one(db)
            .await?;
    Ok(v)
}

/// What a Service Pyo made says about itself (its comment).
pub fn marker(install: &str) -> String {
    format!("Made by Pyo ({install})")
}

// ── The API ─────────────────────────────────────────────────────────────────

fn http() -> &'static reqwest::Client {
    static C: OnceLock<reqwest::Client> = OnceLock::new();
    C.get_or_init(|| {
        reqwest::Client::builder()
            .user_agent("pyo")
            .timeout(Duration::from_secs(30))
            .build()
            .expect("http client")
    })
}

/// The OAuth client, with its access token while it lasts.
pub struct Client {
    id: String,
    secret: String,
    token: Mutex<Option<(String, Instant)>>,
}

/// One per client ID and secret, so tokens are reused.
pub fn client(id: &str, secret: &str) -> std::sync::Arc<Client> {
    static C: Mutex<Option<std::sync::Arc<Client>>> = Mutex::new(None);
    let mut g = C.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(c) = g.as_ref().filter(|c| c.id == id && c.secret == secret) {
        return c.clone();
    }
    let c = std::sync::Arc::new(Client {
        id: id.to_owned(),
        secret: secret.to_owned(),
        token: Mutex::new(None),
    });
    *g = Some(c.clone());
    c
}

/// The saved client, opened.
pub async fn saved_client(s: &AppState) -> ApiResult<Option<(Settings, std::sync::Arc<Client>)>> {
    let Some(cfg) = settings(&s.db).await? else {
        return Ok(None);
    };
    let secret = s.sealer.open(&cfg.secret_sealed)?;
    let c = client(&cfg.client_id, &secret);
    Ok(Some((cfg, c)))
}

/// What Tailscale said, as people read it.
#[derive(Debug)]
pub struct TsError {
    pub status: u16,
    pub message: String,
}

impl std::fmt::Display for TsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Tailscale answered {}: {}", self.status, self.message)
    }
}

impl std::error::Error for TsError {}

impl Client {
    async fn access_token(&self) -> anyhow::Result<String> {
        if let Some((t, at)) = self.token.lock().unwrap_or_else(|e| e.into_inner()).clone()
            && at.elapsed() < Duration::from_secs(45 * 60)
        {
            return Ok(t);
        }
        let r = http()
            .post(format!("{API}/oauth/token"))
            .header(reqwest::header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(
                reqwest::Url::parse_with_params(
                    "http://form/",
                    &[("client_id", self.id.as_str()), ("client_secret", self.secret.as_str())],
                )?
                .query()
                .unwrap_or("")
                .to_owned(),
            )
            .send()
            .await?;
        let status = r.status().as_u16();
        let body: Value = r.json().await.unwrap_or(Value::Null);
        let Some(t) = body["access_token"].as_str().filter(|_| status == 200) else {
            return Err(TsError {
                status,
                message: body["message"]
                    .as_str()
                    .or(body["error_description"].as_str())
                    .or(body["error"].as_str())
                    .unwrap_or("the client ID or secret isn't right")
                    .to_owned(),
            }
            .into());
        };
        *self.token.lock().unwrap_or_else(|e| e.into_inner()) =
            Some((t.to_owned(), Instant::now()));
        Ok(t.to_owned())
    }

    async fn call(&self, method: reqwest::Method, path: &str, body: Option<&Value>) -> anyhow::Result<Value> {
        let token = self.access_token().await?;
        let mut req = http().request(method, format!("{API}{path}")).bearer_auth(token);
        if let Some(b) = body {
            req = req.json(b);
        }
        let r = req.send().await?;
        let status = r.status().as_u16();
        let text = r.text().await.unwrap_or_default();
        if !(200..300).contains(&status) {
            let message = serde_json::from_str::<Value>(&text)
                .ok()
                .and_then(|v| v["message"].as_str().map(str::to_owned))
                .unwrap_or_else(|| text.trim().chars().take(300).collect());
            return Err(TsError { status, message }.into());
        }
        Ok(if text.trim().is_empty() {
            Value::Null
        } else {
            serde_json::from_str(&text)?
        })
    }

    /// The tailnet's Services: name ("blog") → (comment, addresses).
    pub async fn services(&self) -> anyhow::Result<BTreeMap<String, (String, Vec<String>)>> {
        let v = self.call(reqwest::Method::GET, "/tailnet/-/services", None).await?;
        Ok(v["vipServices"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|s| {
                let name = s["name"].as_str()?.strip_prefix("svc:")?.to_owned();
                Some((name, (s["comment"].as_str().unwrap_or("").to_owned(), strings(&s["addrs"]))))
            })
            .collect())
    }

    /// Make a Service: ports 80 and 443, tagged, marked as Pyo's. Its
    /// addresses come back.
    pub async fn create_service(&self, name: &str, comment: &str) -> anyhow::Result<Vec<String>> {
        let body = json!({ "name": format!("svc:{name}"), "comment": comment, "ports": ["tcp:80", "tcp:443"], "tags": [TAG] });
        let v = self
            .call(reqwest::Method::PUT, &format!("/tailnet/-/services/svc:{name}"), Some(&body))
            .await?;
        Ok(strings(&v["addrs"]))
    }

    pub async fn delete_service(&self, name: &str) -> anyhow::Result<()> {
        self.call(reqwest::Method::DELETE, &format!("/tailnet/-/services/svc:{name}"), None)
            .await
            .map(|_| ())
    }

    /// A Service's hosts: node ID → approved.
    pub async fn hosts(&self, name: &str) -> anyhow::Result<BTreeMap<String, bool>> {
        let v = self
            .call(reqwest::Method::GET, &format!("/tailnet/-/services/svc:{name}/devices"), None)
            .await?;
        Ok(v["hosts"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|h| {
                Some((
                    h["stableNodeID"].as_str()?.to_owned(),
                    h["approvalLevel"].as_str().is_some_and(|l| l.starts_with("approved")),
                ))
            })
            .collect())
    }

    pub async fn approve(&self, name: &str, node: &str) -> anyhow::Result<()> {
        self.call(
            reqwest::Method::POST,
            &format!("/tailnet/-/services/svc:{name}/device/{node}/approved"),
            Some(&json!({ "approved": true })),
        )
        .await
        .map(|_| ())
    }

    /// The tailnet's machines: (node ID, full name, tags).
    pub async fn devices(&self) -> anyhow::Result<Vec<(String, String, Vec<String>)>> {
        let v = self.call(reqwest::Method::GET, "/tailnet/-/devices", None).await?;
        Ok(v["devices"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|d| {
                Some((
                    d["nodeId"].as_str()?.to_owned(),
                    d["name"].as_str().unwrap_or("").trim_end_matches('.').to_owned(),
                    strings(&d["tags"]),
                ))
            })
            .collect())
    }

    pub async fn tag(&self, node: &str) -> anyhow::Result<()> {
        self.call(reqwest::Method::POST, &format!("/device/{node}/tags"), Some(&json!({ "tags": [TAG] })))
            .await
            .map(|_| ())
    }

    /// A single-use, preauthorized auth key with `tag:pyo`, for an hour:
    /// (its ID, the key).
    pub async fn auth_key(&self, description: &str) -> anyhow::Result<(String, String)> {
        let body = json!({
            "capabilities": { "devices": { "create": { "reusable": false, "ephemeral": false, "preauthorized": true, "tags": [TAG] } } },
            "expirySeconds": 3600,
            "description": description,
        });
        let v = self.call(reqwest::Method::POST, "/tailnet/-/keys", Some(&body)).await?;
        match (v["id"].as_str(), v["key"].as_str()) {
            (Some(id), Some(k)) => Ok((id.to_owned(), k.to_owned())),
            _ => anyhow::bail!("Tailscale made no key"),
        }
    }

    pub async fn delete_key(&self, id: &str) -> anyhow::Result<()> {
        self.call(reqwest::Method::DELETE, &format!("/tailnet/-/keys/{id}"), None)
            .await
            .map(|_| ())
    }
}

fn strings(v: &Value) -> Vec<String> {
    v.as_array()
        .map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_owned)).collect())
        .unwrap_or_default()
}

/// "server-1.tail1234.ts.net" → "tail1234.ts.net".
pub fn suffix_of(name: &str) -> Option<String> {
    let (_, rest) = name.trim_end_matches('.').split_once('.')?;
    rest.ends_with(".ts.net").then(|| rest.to_ascii_lowercase())
}

/// A name on the tailnet: one DNS label.
pub fn valid_name(n: &str) -> bool {
    !n.is_empty()
        && n.len() <= 63
        && n.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !n.starts_with('-')
        && !n.ends_with('-')
}

/// The first free name: `base`, then `base-<more>`, then `base-<more>-2`…
pub fn pick_name(base: &str, more: &str, taken: impl Fn(&str) -> bool) -> String {
    let cut = |x: &str| x.chars().take(63).collect::<String>().trim_end_matches('-').to_owned();
    let first = cut(base);
    if !taken(&first) {
        return first;
    }
    let second = cut(&format!("{base}-{more}"));
    if !taken(&second) {
        return second;
    }
    (2..)
        .map(|i| cut(&format!("{base}-{more}-{i}")))
        .find(|n| !taken(n))
        .unwrap_or_default()
}

// ── Checking the client ─────────────────────────────────────────────────────

/// One check of the Test button.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Check {
    pub name: &'static str,
    pub ok: bool,
    pub message: String,
}

/// What the client can do, check by check, saying what's missing.
pub async fn test(c: &Client) -> (Vec<Check>, Option<String>) {
    let mut out = Vec::new();
    let mut suffix = None;
    if let Err(e) = c.access_token().await {
        out.push(Check { name: "client", ok: false, message: format!("Couldn't sign in with it. {e}") });
        return (out, None);
    }
    out.push(Check { name: "client", ok: true, message: "Signed in.".into() });
    match c.devices().await {
        Ok(d) => {
            suffix = d.iter().find_map(|(_, n, _)| suffix_of(n));
            out.push(Check { name: "devices", ok: true, message: "Can read the tailnet's machines.".into() });
        }
        Err(e) => out.push(Check { name: "devices", ok: false, message: format!("Needs the Devices › Core scope with tag:pyo. {e}") }),
    }
    match c.services().await {
        Ok(_) => out.push(Check { name: "services", ok: true, message: "Can manage Services.".into() }),
        Err(e) => out.push(Check { name: "services", ok: false, message: format!("Needs the Services scope. {e}") }),
    }
    match c.auth_key("Pyo test").await {
        Ok((id, _)) => {
            let _ = c.delete_key(&id).await;
            out.push(Check { name: "authKeys", ok: true, message: "Can make auth keys with tag:pyo.".into() });
        }
        Err(e) => out.push(Check {
            name: "authKeys",
            ok: false,
            message: format!("Needs the Auth Keys scope with tag:pyo, and tag:pyo in the tailnet policy's tagOwners. {e}"),
        }),
    }
    (out, suffix)
}

// ── Keeping the Services ────────────────────────────────────────────────────

pub async fn runner(state: AppState) {
    loop {
        let st = state.clone();
        crate::bg::as_leader(&state.db, crate::bg::LOCK_TAILSCALE, || async move {
            if let Err(e) = tick(&st).await {
                tracing::warn!(error = %format!("{e:?}"), "tailscale");
            }
        })
        .await;
        tokio::time::sleep(Duration::from_secs(20)).await;
    }
}

/// Every web process: Pyo's HTTPS names as the runner last saved them.
pub async fn addresses_follower(db: PgPool) {
    loop {
        if let Ok(Some(v)) =
            sqlx::query_scalar::<_, Value>("SELECT value FROM pyo.settings WHERE key = 'addresses'")
                .fetch_optional(&db)
                .await
        {
            let s = |k: &str| v[k].as_str().filter(|x| !x.is_empty()).map(str::to_owned);
            crate::web::set_addresses(crate::web::Addresses {
                tailnet: s("tailnet"),
                registry: s("registry"),
                public: s("public"),
            });
        }
        tokio::time::sleep(Duration::from_secs(10)).await;
    }
}

/// Saves one of Pyo's HTTPS names ("tailnet", "registry", "public").
pub async fn save_address(db: &PgPool, key: &str, host: Option<&str>) -> ApiResult<()> {
    sqlx::query(
        "INSERT INTO pyo.settings (key, value) VALUES ('addresses', jsonb_build_object($1::text, $2::text))
         ON CONFLICT (key) DO UPDATE SET value = pyo.settings.value || jsonb_build_object($1::text, $2::text), updated_at = now()",
    )
    .bind(key)
    .bind(host.unwrap_or(""))
    .execute(db)
    .await?;
    Ok(())
}

/// A server's Tailscale as its cluster last reported it.
#[derive(Debug, Clone)]
struct Server {
    cluster_id: String,
    status: pyo_proto::ServerTailnet,
}

async fn servers(db: &PgPool) -> ApiResult<Vec<Server>> {
    let rows: Vec<(String, Value)> = sqlx::query_as(
        "SELECT id, coalesce(capabilities->'tailnet', '[]') FROM pyo.clusters WHERE status <> 'disconnected'",
    )
    .fetch_all(db)
    .await?;
    Ok(rows
        .into_iter()
        .flat_map(|(cluster_id, v)| {
            serde_json::from_value::<Vec<pyo_proto::ServerTailnet>>(v)
                .unwrap_or_default()
                .into_iter()
                .map(move |status| Server { cluster_id: cluster_id.clone(), status })
        })
        .collect())
}

/// The names on the tailnet Pyo wants: (name, kind, cluster).
async fn wanted(db: &PgPool, cfg: &Settings, suffix: &str) -> ApiResult<Vec<(String, &'static str, String)>> {
    let mut out = Vec::new();
    let host: Option<String> =
        sqlx::query_scalar("SELECT id FROM pyo.clusters WHERE same_cluster LIMIT 1")
            .fetch_optional(db)
            .await?;
    if let Some(h) = &host {
        if let Some(n) = &cfg.dashboard_name {
            out.push((n.clone(), "dashboard", h.clone()));
        }
        if let Some(n) = &cfg.registry_name {
            out.push((n.clone(), "registry", h.clone()));
        }
    }
    let domains: Vec<(String, String)> = sqlx::query_as(
        "SELECT s.cluster_id, lower(d->>'hostname') FROM pyo.services s, jsonb_array_elements(s.spec->'domains') d
          WHERE NOT s.deleting AND lower(d->>'hostname') LIKE '%.' || $1",
    )
    .bind(suffix)
    .fetch_all(db)
    .await?;
    for (cluster, host) in domains {
        if let Some(label) = host.strip_suffix(&format!(".{suffix}")).filter(|l| valid_name(l)) {
            out.push((label.to_owned(), "domain", cluster));
        }
    }
    out.sort();
    out.dedup_by(|a, b| a.0 == b.0);
    Ok(out)
}

/// Approvals asked for already: (Service, node).
static APPROVED: Mutex<BTreeSet<(String, String)>> = Mutex::new(BTreeSet::new());

pub async fn tick(s: &AppState) -> ApiResult<()> {
    let Some((mut cfg, c)) = saved_client(s).await? else {
        for k in ["tailnet", "registry"] {
            save_address(&s.db, k, None).await?;
        }
        return Ok(());
    };
    let install = install_id(&s.db).await?;
    let mark = marker(&install);
    let devices = c.devices().await.map_err(anyhow::Error::from)?;
    let suffix = devices
        .iter()
        .find_map(|(_, n, _)| suffix_of(n))
        .or(cfg.suffix.clone());
    let Some(suffix) = suffix else {
        return Ok(());
    };
    let remote = c.services().await.map_err(anyhow::Error::from)?;
    let ours = |n: &str| remote.get(n).is_some_and(|(cm, _)| *cm == mark);
    let machine = |n: &str| {
        devices
            .iter()
            .any(|(_, full, _)| full.split('.').next() == Some(n))
    };
    // Pyo's own names, picked once: "pyo", then "pyo-<cluster>"…
    let mut changed = cfg.suffix.as_deref() != Some(suffix.as_str());
    cfg.suffix = Some(suffix.clone());
    let host_cluster: Option<String> =
        sqlx::query_scalar("SELECT name FROM pyo.clusters WHERE same_cluster LIMIT 1")
            .fetch_optional(&s.db)
            .await?;
    if let Some(cluster) = &host_cluster {
        let taken = |n: &str| (remote.contains_key(n) && !ours(n)) || machine(n);
        let more = crate::util::slugify(cluster);
        if cfg.dashboard_name.is_none() {
            cfg.dashboard_name = Some(pick_name("pyo", &more, |n| taken(n)));
            changed = true;
        }
        if cfg.registry_name.is_none() {
            cfg.registry_name = Some(pick_name("registry", &more, |n| taken(n)));
            changed = true;
        }
    }
    if changed {
        save(&s.db, &cfg).await?;
    }
    let wanted = wanted(&s.db, &cfg, &suffix).await?;
    let wanted_names: BTreeSet<&str> = wanted.iter().map(|w| w.0.as_str()).collect();
    for (name, kind, cluster) in &wanted {
        let (addrs, error) = match remote.get(name.as_str()) {
            Some((_, addrs)) if ours(name) => (addrs.clone(), None),
            Some(_) => (Vec::new(), Some("Taken on your tailnet")),
            None if machine(name) => (Vec::new(), Some("Taken on your tailnet")),
            None => match c.create_service(name, &mark).await {
                Ok(a) => {
                    tracing::info!(name, "tailnet name made");
                    (a, None)
                }
                Err(e) => {
                    tracing::warn!(name, error = %e, "couldn't make the tailnet name");
                    (Vec::new(), Some("Couldn't make it on your tailnet"))
                }
            },
        };
        sqlx::query(
            "INSERT INTO pyo.tailnet_services (name, kind, cluster_id, addrs, error) VALUES ($1, $2, $3, $4, $5)
             ON CONFLICT (name) DO UPDATE SET kind = excluded.kind, cluster_id = excluded.cluster_id, addrs = excluded.addrs, error = excluded.error, retired_at = NULL, updated_at = now()
             WHERE (pyo.tailnet_services.kind, pyo.tailnet_services.cluster_id, pyo.tailnet_services.addrs, pyo.tailnet_services.error, pyo.tailnet_services.retired_at) IS DISTINCT FROM (excluded.kind, excluded.cluster_id, excluded.addrs, excluded.error, NULL)",
        )
        .bind(name)
        .bind(kind)
        .bind(cluster)
        .bind(&addrs)
        .bind(error)
        .execute(&s.db)
        .await?;
    }
    // Ours, no longer used: deleted on the tailnet, and its servers take it off.
    for name in remote.keys().filter(|n| ours(n) && !wanted_names.contains(n.as_str())) {
        match c.delete_service(name).await {
            Ok(()) => tracing::info!(name, "tailnet name deleted"),
            Err(e) => tracing::warn!(name, error = %e, "couldn't delete the tailnet name"),
        }
    }
    sqlx::query("UPDATE pyo.tailnet_services SET retired_at = now(), updated_at = now() WHERE retired_at IS NULL AND NOT (name = ANY($1))")
        .bind(wanted_names.iter().map(|n| n.to_string()).collect::<Vec<_>>())
        .execute(&s.db)
        .await?;
    sqlx::query("DELETE FROM pyo.tailnet_services WHERE retired_at < now() - interval '7 days'")
        .execute(&s.db)
        .await?;

    // The servers: tagged, and approved as hosts of their cluster's names.
    let servers = servers(&s.db).await?;
    let tagged: BTreeMap<&str, bool> = devices
        .iter()
        .map(|(id, _, tags)| (id.as_str(), tags.iter().any(|t| t == TAG)))
        .collect();
    for sv in &servers {
        let id = sv.status.node_id.as_str();
        if !id.is_empty() && tagged.get(id) == Some(&false) {
            match c.tag(id).await {
                Ok(()) => tracing::info!(server = %sv.status.server, "tagged tag:pyo"),
                Err(e) => tracing::warn!(server = %sv.status.server, error = %e, "couldn't tag the server"),
            }
        }
    }
    for (name, _, cluster) in &wanted {
        if !ours(name) && !remote.is_empty() && remote.contains_key(name.as_str()) {
            continue;
        }
        let nodes: Vec<&str> = servers
            .iter()
            .filter(|sv| sv.cluster_id == *cluster && sv.status.advertised.iter().any(|a| a == name))
            .map(|sv| sv.status.node_id.as_str())
            .filter(|n| !n.is_empty())
            .collect();
        let asked = |n: &str| {
            APPROVED
                .lock()
                .map(|a| a.contains(&(name.clone(), n.to_owned())))
                .unwrap_or(false)
        };
        if nodes.iter().all(|n| asked(n)) {
            continue;
        }
        let Ok(hosts) = c.hosts(name).await else { continue };
        for n in nodes {
            let ok = hosts.get(n) == Some(&true) || c.approve(name, n).await.is_ok();
            if ok && let Ok(mut a) = APPROVED.lock() {
                a.insert((name.clone(), n.to_owned()));
            }
        }
    }

    // Pyo's own names, once a server keeps their certificate.
    let has_cert = |name: &str, cluster: &str| {
        servers
            .iter()
            .any(|sv| sv.cluster_id == cluster && sv.status.certificates.iter().any(|c| c == name))
    };
    let host_id: Option<String> =
        sqlx::query_scalar("SELECT id FROM pyo.clusters WHERE same_cluster LIMIT 1")
            .fetch_optional(&s.db)
            .await?;
    let ready = |kind: &str| {
        wanted
            .iter()
            .find(|(n, k, cl)| *k == kind && ours(n) && host_id.as_deref() == Some(cl.as_str()) && has_cert(n, cl))
            .map(|(n, _, _)| format!("{n}.{suffix}"))
    };
    save_address(&s.db, "tailnet", ready("dashboard").as_deref()).await?;
    save_address(&s.db, "registry", ready("registry").as_deref()).await?;

    // Services' tailnet domains: their certificate as their state.
    for (name, kind, cluster) in &wanted {
        if *kind != "domain" {
            continue;
        }
        let host = format!("{name}.{suffix}");
        let (tls, error) = match remote.get(name.as_str()) {
            _ if !ours(name) => ("failed", Some("Taken on your tailnet")),
            _ if has_cert(name, cluster) => ("issued", None),
            _ => ("pending", None),
        };
        sqlx::query(
            "UPDATE pyo.services SET spec = jsonb_set(spec, '{domains}', (
               SELECT coalesce(jsonb_agg(CASE WHEN lower(d->>'hostname') = $2
                                              THEN (d - 'tlsError') || jsonb_strip_nulls(jsonb_build_object('tls', $3::text, 'tlsError', $4::text))
                                              ELSE d END), '[]')
                 FROM jsonb_array_elements(spec->'domains') d))
              WHERE cluster_id = $1 AND NOT deleting
                AND EXISTS (SELECT 1 FROM jsonb_array_elements(spec->'domains') d
                             WHERE lower(d->>'hostname') = $2 AND (coalesce(d->>'tls', '') <> $3 OR coalesce(d->>'tlsError', '') <> coalesce($4, '')))",
        )
        .bind(cluster)
        .bind(&host)
        .bind(tls)
        .bind(error)
        .execute(&s.db)
        .await?;
    }
    Ok(())
}

/// What a cluster's agent hosts on the tailnet (its heartbeat reply). None
/// without Tailscale set up.
pub async fn wanted_for(db: &PgPool, cluster_id: &str, same_cluster: bool) -> ApiResult<Option<pyo_proto::TailnetWanted>> {
    let Some(cfg) = settings(db).await? else {
        return Ok(None);
    };
    let Some(suffix) = cfg.suffix.clone() else {
        return Ok(None);
    };
    let rows: Vec<(String, String, Option<String>, Vec<String>, Option<String>, bool)> = sqlx::query_as(
        "SELECT name, kind, cluster_id, addrs, error, retired_at IS NOT NULL FROM pyo.tailnet_services",
    )
    .fetch_all(db)
    .await?;
    let mut w = pyo_proto::TailnetWanted { suffix: suffix.clone(), ..Default::default() };
    for (name, kind, cluster, addrs, error, retired) in rows {
        let here = cluster.as_deref() == Some(cluster_id) && !retired && error.is_none() && !addrs.is_empty();
        if !here {
            w.retired.push(name);
            continue;
        }
        if same_cluster {
            match kind.as_str() {
                "dashboard" => w.own.dashboard = Some(format!("{name}.{suffix}")),
                "registry" => w.own.registry = Some(format!("{name}.{suffix}")),
                _ => {}
            }
        }
        w.services.push(pyo_proto::TailnetService { name, addrs });
    }
    Ok(Some(w))
}

/// A join command's auth key: single-use, preauthorized, tag:pyo, an hour.
pub async fn join_key(s: &AppState, what: &str) -> ApiResult<String> {
    let Some((_, c)) = saved_client(s).await? else {
        return Err(ApiError::bad_request(
            "Set up Tailscale in Integrations first. Servers talk over your tailnet, with no open ports.",
        ));
    };
    c.auth_key(what)
        .await
        .map(|(_, k)| k)
        .map_err(|e| ApiError::bad_request(format!("Couldn't make an auth key. {e}")))
}

/// Whether a tailnet domain someone typed can be had: Pyo's reserved names
/// and its own names are refused, and so is a name something else on the
/// tailnet has.
pub async fn check_domain(s: &AppState, host: &str) -> ApiResult<()> {
    let Some((cfg, c)) = saved_client(s).await? else {
        return Err(ApiError::bad_request("Tailnet domains need Tailscale. Set it up in Integrations."));
    };
    let Some(suffix) = cfg.suffix.as_deref() else {
        return Err(ApiError::bad_request("Pyo hasn't seen your tailnet yet. Try again in a minute."));
    };
    let host = host.to_ascii_lowercase();
    let Some(label) = host.strip_suffix(&format!(".{suffix}")) else {
        return Err(ApiError::bad_request(format!("A tailnet domain ends with .{suffix}.")));
    };
    if !valid_name(label) {
        return Err(ApiError::bad_request("Use one name: lowercase letters, digits and dashes."));
    }
    if RESERVED.contains(&label)
        || cfg.dashboard_name.as_deref() == Some(label)
        || cfg.registry_name.as_deref() == Some(label)
    {
        return Err(ApiError::bad_request("Pyo uses this name itself."));
    }
    let mark = marker(&install_id(&s.db).await?);
    let remote = c.services().await.map_err(anyhow::Error::from)?;
    let mine = remote.get(label).is_some_and(|(cm, _)| *cm == mark);
    let machine = c
        .devices()
        .await
        .map_err(anyhow::Error::from)?
        .iter()
        .any(|(_, full, _)| full.split('.').next() == Some(label));
    if (remote.contains_key(label) && !mine) || machine {
        return Err(ApiError::conflict("Taken on your tailnet"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        assert_eq!(suffix_of("server-1.tail1234.ts.net."), Some("tail1234.ts.net".into()));
        assert_eq!(suffix_of("example.com"), None);
        assert!(valid_name("blog-2") && !valid_name("Blog") && !valid_name("-a") && !valid_name("a.b"));
        let taken = ["pyo", "pyo-home"];
        assert_eq!(pick_name("pyo", "home", |n| taken.contains(&n)), "pyo-home-2");
        assert_eq!(pick_name("registry", "home", |n| taken.contains(&n)), "registry");
        assert_eq!(pick_name("pyo", "home", |n| n == "pyo"), "pyo-home");
        let long = "a".repeat(70);
        assert!(pick_name(&long, "x", |_| false).len() <= 63);
        assert_eq!(marker("abc"), "Made by Pyo (abc)");
    }
}
