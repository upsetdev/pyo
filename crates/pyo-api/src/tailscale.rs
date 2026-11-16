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
/// Why a join or connect command can't be made yet.
pub const NEEDED: &str = "Servers join over your tailnet. Set up Tailscale in Integrations first.";

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
    /// The API ("https://api.tailscale.com/api/v2"; a mock in tests).
    base: String,
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
        base: API.to_owned(),
    });
    *g = Some(c.clone());
    c
}

#[cfg(test)]
fn test_client(base: &str) -> Client {
    Client {
        base: base.to_owned(),
        id: "id".into(),
        secret: "secret".into(),
        token: Mutex::new(None),
    }
}

/// What a wanted name is on the tailnet.
#[derive(Debug, PartialEq)]
pub enum Claim {
    /// Pyo's (made by this installation): its addresses.
    Ours(Vec<String>),
    /// Someone else's, or one Pyo can't read.
    Taken,
}

/// A wanted name: Pyo's own Service, made now if there's none by that name
/// at all. Read directly first, whatever the list said: a name that
/// exists and isn't marked as this installation's is never written.
pub async fn claim(c: &Client, name: &str, mark: &str) -> anyhow::Result<Claim> {
    match c.service(name).await? {
        Some((comment, addrs)) if comment == mark => Ok(Claim::Ours(addrs)),
        Some(_) => Ok(Claim::Taken),
        None => {
            let addrs = c.create_service(name, mark).await?;
            tracing::info!(name, "tailnet name made");
            Ok(Claim::Ours(addrs))
        }
    }
}

/// Deletes a Service only if its own record has this installation's mark.
pub async fn delete_if_ours(c: &Client, name: &str, mark: &str) -> anyhow::Result<bool> {
    match c.service(name).await? {
        Some((comment, _)) if comment == mark => {
            c.delete_service(name).await?;
            Ok(true)
        }
        _ => Ok(false),
    }
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
            .post(format!("{}/oauth/token", self.base))
            .header(
                reqwest::header::CONTENT_TYPE,
                "application/x-www-form-urlencoded",
            )
            .body(
                reqwest::Url::parse_with_params(
                    "http://form/",
                    &[
                        ("client_id", self.id.as_str()),
                        ("client_secret", self.secret.as_str()),
                    ],
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

    async fn call(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<&Value>,
    ) -> anyhow::Result<Value> {
        let token = self.access_token().await?;
        let mut req = http()
            .request(method, format!("{}{path}", self.base))
            .bearer_auth(token);
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
        let v = self
            .call(reqwest::Method::GET, "/tailnet/-/services", None)
            .await?;
        Ok(v["vipServices"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|s| {
                let name = s["name"].as_str()?.strip_prefix("svc:")?.to_owned();
                Some((
                    name,
                    (
                        s["comment"].as_str().unwrap_or("").to_owned(),
                        strings(&s["addrs"]),
                    ),
                ))
            })
            .collect())
    }

    /// Make a Service: ports 80 and 443, tagged, marked as Pyo's. Its
    /// addresses come back.
    pub async fn create_service(&self, name: &str, comment: &str) -> anyhow::Result<Vec<String>> {
        let body = json!({ "name": format!("svc:{name}"), "comment": comment, "ports": ["tcp:80", "tcp:443"], "tags": [TAG] });
        let v = self
            .call(
                reqwest::Method::PUT,
                &format!("/tailnet/-/services/svc:{name}"),
                Some(&body),
            )
            .await?;
        Ok(strings(&v["addrs"]))
    }

    /// One Service, read directly: its comment and addresses, or None when
    /// there's no such Service (404). The list leaves some out (Tailscale's
    /// API reference: it has the Services of the admin console's
    /// "Advertised" tab, so not one defined and hosted nowhere). Any other
    /// answer is an error, 403 included ("access to the Service is
    /// forbidden"), so a name Pyo can't read is never taken.
    ///
    /// There's no create-only write: PUT creates or updates, with no
    /// If-Match or If-None-Match for Services (only the policy file has
    /// ETags). So this read is what keeps Pyo off Services it didn't make.
    pub async fn service(&self, name: &str) -> anyhow::Result<Option<(String, Vec<String>)>> {
        match self
            .call(
                reqwest::Method::GET,
                &format!("/tailnet/-/services/svc:{name}"),
                None,
            )
            .await
        {
            Ok(v) => Ok(Some((
                v["comment"].as_str().unwrap_or("").to_owned(),
                strings(&v["addrs"]),
            ))),
            Err(e) if e.downcast_ref::<TsError>().is_some_and(|t| t.status == 404) => Ok(None),
            Err(e) => Err(e),
        }
    }

    pub async fn delete_service(&self, name: &str) -> anyhow::Result<()> {
        self.call(
            reqwest::Method::DELETE,
            &format!("/tailnet/-/services/svc:{name}"),
            None,
        )
        .await
        .map(|_| ())
    }

    /// A Service's hosts: node ID → approved.
    pub async fn hosts(&self, name: &str) -> anyhow::Result<BTreeMap<String, bool>> {
        let v = self
            .call(
                reqwest::Method::GET,
                &format!("/tailnet/-/services/svc:{name}/devices"),
                None,
            )
            .await?;
        Ok(v["hosts"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|h| {
                Some((
                    h["stableNodeID"].as_str()?.to_owned(),
                    h["approvalLevel"]
                        .as_str()
                        .is_some_and(|l| l.starts_with("approved")),
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
        let v = self
            .call(reqwest::Method::GET, "/tailnet/-/devices", None)
            .await?;
        Ok(v["devices"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|d| {
                Some((
                    d["nodeId"].as_str()?.to_owned(),
                    d["name"]
                        .as_str()
                        .unwrap_or("")
                        .trim_end_matches('.')
                        .to_owned(),
                    strings(&d["tags"]),
                ))
            })
            .collect())
    }

    /// The tailnet's machines with what Pyo's cloud servers need: their
    /// hostname, and their key while Tailnet Lock waits for its signature.
    pub async fn device_details(&self) -> anyhow::Result<Vec<Device>> {
        let v = self
            .call(
                reqwest::Method::GET,
                "/tailnet/-/devices?fields=default",
                None,
            )
            .await?;
        Ok(v["devices"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|d| {
                Some(Device {
                    node_id: d["nodeId"].as_str()?.to_owned(),
                    hostname: d["hostname"].as_str().unwrap_or("").to_owned(),
                    tags: strings(&d["tags"]),
                    node_key: d["nodeKey"].as_str().unwrap_or("").to_owned(),
                    lock_error: d["tailnetLockError"].as_str().unwrap_or("").to_owned(),
                })
            })
            .collect())
    }

    /// Takes a machine off the tailnet (only tag:pyo ones: the client's
    /// Devices › Core scope is for that tag).
    pub async fn delete_device(&self, node: &str) -> anyhow::Result<()> {
        if node.is_empty() || !node.bytes().all(|b| b.is_ascii_alphanumeric()) {
            anyhow::bail!("not a node ID");
        }
        match self
            .call(reqwest::Method::DELETE, &format!("/device/{node}"), None)
            .await
        {
            Err(e) if e.downcast_ref::<TsError>().is_some_and(|t| t.status == 404) => Ok(()),
            r => r.map(|_| ()),
        }
    }

    pub async fn tag(&self, node: &str) -> anyhow::Result<()> {
        self.call(
            reqwest::Method::POST,
            &format!("/device/{node}/tags"),
            Some(&json!({ "tags": [TAG] })),
        )
        .await
        .map(|_| ())
    }

    /// A single-use, preauthorized auth key with `tag:pyo`, for an hour:
    /// (its ID, the key).
    pub async fn auth_key(&self, description: &str) -> anyhow::Result<(String, String)> {
        let body = json!({
            "capabilities": { "devices": { "create": { "reusable": false, "ephemeral": false, "preauthorized": true, "tags": [TAG] } } },
            "expirySeconds": 3600,
            "description": key_description(description),
        });
        let v = self
            .call(reqwest::Method::POST, "/tailnet/-/keys", Some(&body))
            .await?;
        match (v["id"].as_str(), v["key"].as_str()) {
            (Some(id), Some(k)) => Ok((id.to_owned(), k.to_owned())),
            _ => anyhow::bail!("Tailscale made no key"),
        }
    }

    pub async fn delete_key(&self, id: &str) -> anyhow::Result<()> {
        self.call(
            reqwest::Method::DELETE,
            &format!("/tailnet/-/keys/{id}"),
            None,
        )
        .await
        .map(|_| ())
    }
}

/// What Tailscale takes as a key's description: letters, digits, spaces,
/// dashes and underscores, at most 50 characters. "Pyo: join web-1"
/// becomes "Pyo join web-1".
fn key_description(s: &str) -> String {
    let kept: String = s
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, ' ' | '-' | '_'))
        .collect();
    let mut out = kept.split_whitespace().collect::<Vec<_>>().join(" ");
    out.truncate(50);
    out.trim_end().to_owned()
}

/// A machine on the tailnet ([`Client::device_details`]).
#[derive(Debug, Clone, PartialEq)]
pub struct Device {
    pub node_id: String,
    pub hostname: String,
    pub tags: Vec<String>,
    /// "nodekey:…"
    pub node_key: String,
    /// Set while Tailnet Lock waits for this machine's key to be signed.
    pub lock_error: String,
}

/// Pyo's machines (tag:pyo) that Tailnet Lock keeps out until a signing
/// device signs their key: their hostname, and the command that signs it.
pub fn lock_waiting(devices: &[Device]) -> Vec<(String, String)> {
    devices
        .iter()
        .filter(|d| {
            !d.lock_error.is_empty()
                && d.node_key.starts_with("nodekey:")
                && d.tags.iter().any(|t| t == TAG)
        })
        .map(|d| {
            (
                d.hostname.clone(),
                format!("tailscale lock sign {}", d.node_key),
            )
        })
        .collect()
}

#[derive(Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub struct LockQuery {
    /// The team adding the server (slug or id).
    team: String,
}

/// Machines waiting for a Tailnet Lock signature
///
/// With Tailnet Lock on, a new server stays off the tailnet until a
/// signing device signs its key. These are the ones Pyo's commands added
/// that wait, with the command to run on a signing device. Read from
/// Tailscale at most every 15 seconds. Empty without Tailscale set up.
#[utoipa::path(get, path = "/tailnet/lock", operation_id = "getTailnetLock", tag = "Servers",
    params(LockQuery),
    responses(
        (status = 200, description = "`waiting`: each with `name` (the machine's hostname) and `signCommand`.", body = Object),
        (status = 403, description = "Only owners and admins add servers.", body = crate::error::ErrorBody),
        (status = 404, description = "No such team, or you aren't in it.", body = crate::error::ErrorBody),
    ),
    extensions(("x-pyo-access" = json!("read"))))]
pub async fn lock(
    axum::extract::State(s): axum::extract::State<AppState>,
    crate::auth::SessionUser(user): crate::auth::SessionUser,
    crate::error::Query(q): crate::error::Query<LockQuery>,
) -> ApiResult<crate::error::Json<Value>> {
    let mut tx = s.db.begin().await?;
    crate::ops::require_role(&mut tx, &user, &q.team, crate::ops::Role::Admin).await?;
    drop(tx);
    /// When the list was read, and what it said.
    type Seen = Option<(Instant, Vec<(String, String)>)>;
    static SEEN: Mutex<Seen> = Mutex::new(None);
    let fresh = |seen: &Seen| {
        seen.as_ref()
            .filter(|(at, _)| at.elapsed() < Duration::from_secs(15))
            .map(|(_, w)| w.clone())
    };
    let cached = fresh(&SEEN.lock().unwrap_or_else(|e| e.into_inner()));
    let waiting = match cached {
        Some(w) => w,
        None => {
            let w = match saved_client(&s).await? {
                Some((_, c)) => match c.device_details().await {
                    Ok(d) => lock_waiting(&d),
                    Err(e) => {
                        tracing::warn!(error = %e, "tailnet lock: couldn't read the tailnet");
                        Vec::new()
                    }
                },
                None => Vec::new(),
            };
            *SEEN.lock().unwrap_or_else(|e| e.into_inner()) = Some((Instant::now(), w.clone()));
            w
        }
    };
    let waiting: Vec<Value> = waiting
        .into_iter()
        .map(|(name, sign)| json!({ "name": name, "signCommand": sign }))
        .collect();
    Ok(crate::error::Json(json!({ "waiting": waiting })))
}

fn strings(v: &Value) -> Vec<String> {
    v.as_array()
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(str::to_owned))
                .collect()
        })
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
        && n.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !n.starts_with('-')
        && !n.ends_with('-')
}

/// The first free name: `base`, then `base-<more>`, then `base-<more>-2`…
pub fn pick_name(base: &str, more: &str, taken: impl Fn(&str) -> bool) -> String {
    let cut = |x: &str| {
        x.chars()
            .take(63)
            .collect::<String>()
            .trim_end_matches('-')
            .to_owned()
    };
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
        out.push(Check {
            name: "client",
            ok: false,
            message: format!("Couldn't sign in with it. {e}"),
        });
        return (out, None);
    }
    out.push(Check {
        name: "client",
        ok: true,
        message: "Signed in.".into(),
    });
    match c.devices().await {
        Ok(d) => {
            suffix = d.iter().find_map(|(_, n, _)| suffix_of(n));
            out.push(Check {
                name: "devices",
                ok: true,
                message: "Can read the tailnet's machines.".into(),
            });
        }
        Err(e) => out.push(Check {
            name: "devices",
            ok: false,
            message: format!("Needs the Devices › Core scope with tag:pyo. {e}"),
        }),
    }
    match c.services().await {
        Ok(_) => out.push(Check {
            name: "services",
            ok: true,
            message: "Can manage Services.".into(),
        }),
        Err(e) => out.push(Check {
            name: "services",
            ok: false,
            message: format!("Needs the Services scope. {e}"),
        }),
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
            if let Err(e) = crate::publicdash::tick(&st).await {
                tracing::warn!(error = %format!("{e:?}"), "public dashboard");
            }
            if let Err(e) = crate::cloud::tick(&st).await {
                tracing::warn!(error = %e.message, "cloud servers");
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
                host_ip: s("hostIp"),
                host_name: s("hostName"),
            });
        }
        tokio::time::sleep(Duration::from_secs(10)).await;
    }
}

/// Saves one of Pyo's addresses: its HTTPS names ("tailnet", "registry",
/// "public") and this machine's on the tailnet ("hostIp", "hostName").
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

/// The tailnet's machines as Pyo's servers see them (their tailscaled's
/// peers): the API's device list only shows what the client's tags allow.
async fn peer_names(db: &PgPool) -> ApiResult<BTreeSet<String>> {
    Ok(servers(db)
        .await?
        .into_iter()
        .flat_map(|s| s.status.peers)
        .collect())
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
                .map(move |status| Server {
                    cluster_id: cluster_id.clone(),
                    status,
                })
        })
        .collect())
}

/// The names on the tailnet Pyo wants: (name, kind, cluster).
async fn wanted(
    db: &PgPool,
    cfg: &Settings,
    suffix: &str,
) -> ApiResult<Vec<(String, &'static str, String)>> {
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
        if let Some(label) = host
            .strip_suffix(&format!(".{suffix}"))
            .filter(|l| valid_name(l))
        {
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
    let devices = c.devices().await?;
    let suffix = devices
        .iter()
        .find_map(|(_, n, _)| suffix_of(n))
        .or(cfg.suffix.clone());
    let Some(suffix) = suffix else {
        return Ok(());
    };
    let remote = c.services().await?;
    let ours = |n: &str| remote.get(n).is_some_and(|(cm, _)| *cm == mark);
    let peers = peer_names(&s.db).await?;
    let machine = |n: &str| {
        peers.contains(n)
            || devices
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
            None => match claim(&c, name, &mark).await {
                Ok(Claim::Ours(a)) => (a, None),
                Ok(Claim::Taken) => (Vec::new(), Some("Taken on your tailnet")),
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
    // Ours, no longer used: deleted on the tailnet, and its servers take it
    // off. Only names Pyo made (its own rows), each read again first.
    let made: BTreeSet<String> = sqlx::query_scalar("SELECT name FROM pyo.tailnet_services")
        .fetch_all(&s.db)
        .await?
        .into_iter()
        .collect();
    for name in remote
        .keys()
        .filter(|n| ours(n) && made.contains(*n) && !wanted_names.contains(n.as_str()))
    {
        match delete_if_ours(&c, name, &mark).await {
            Ok(true) => tracing::info!(name, "tailnet name deleted"),
            Ok(false) => tracing::warn!(name, "not deleted: not Pyo's on the tailnet"),
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
                Err(e) => {
                    tracing::warn!(server = %sv.status.server, error = %e, "couldn't tag the server")
                }
            }
        }
    }
    for (name, _, cluster) in &wanted {
        if !ours(name) && !remote.is_empty() && remote.contains_key(name.as_str()) {
            continue;
        }
        let nodes: Vec<&str> = servers
            .iter()
            .filter(|sv| {
                sv.cluster_id == *cluster && sv.status.advertised.iter().any(|a| a == name)
            })
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
        let Ok(hosts) = c.hosts(name).await else {
            continue;
        };
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
            .find(|(n, k, cl)| {
                *k == kind && ours(n) && host_id.as_deref() == Some(cl.as_str()) && has_cert(n, cl)
            })
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

/// A row of `pyo.tailnet_services`: name, kind, cluster, addresses,
/// error, retired.
type ServiceRow = (
    String,
    String,
    Option<String>,
    Vec<String>,
    Option<String>,
    bool,
);

/// What a cluster's agent hosts on the tailnet (its heartbeat reply). None
/// without Tailscale set up.
pub async fn wanted_for(
    db: &PgPool,
    cluster_id: &str,
    same_cluster: bool,
) -> ApiResult<Option<pyo_proto::TailnetWanted>> {
    let rows: Vec<ServiceRow> = sqlx::query_as(
        "SELECT name, kind, cluster_id, addrs, error, retired_at IS NOT NULL FROM pyo.tailnet_services",
    )
    .fetch_all(db)
    .await?;
    // Without Tailscale (removed): every server takes Pyo's names off.
    let suffix = match settings(db).await?.and_then(|c| c.suffix) {
        Some(x) => x,
        None if rows.is_empty() => return Ok(None),
        None => String::new(),
    };
    let mut w = pyo_proto::TailnetWanted {
        suffix: suffix.clone(),
        ..Default::default()
    };
    for (name, kind, cluster, addrs, error, retired) in rows {
        let here = !suffix.is_empty()
            && cluster.as_deref() == Some(cluster_id)
            && !retired
            && error.is_none()
            && !addrs.is_empty();
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

/// Tailscale made no auth key: the details go to the log, the user gets one
/// plain sentence and where to look.
/// `server` names what is being added, e.g. "a sandbox server".
fn no_auth_key(server: &'static str) -> impl FnOnce(anyhow::Error) -> ApiError {
    move |e| {
        tracing::warn!(error = %format!("{e:#}"), "Tailscale made no auth key");
        ApiError::bad_request(format!("Set up Tailscale in Integrations to add {server}."))
    }
}

/// A join command's auth key: single-use, preauthorized, tag:pyo, an hour.
pub async fn join_key(s: &AppState, what: &str, server: &'static str) -> ApiResult<String> {
    let Some((_, c)) = saved_client(s).await? else {
        return Err(ApiError::bad_request(NEEDED));
    };
    c.auth_key(what)
        .await
        .map(|(_, k)| k)
        .map_err(no_auth_key(server))
}

/// Whether a tailnet domain someone typed can be had: Pyo's reserved names
/// and its own names are refused, and so is a name something else on the
/// tailnet has.
pub async fn check_domain(s: &AppState, host: &str) -> ApiResult<()> {
    let Some((cfg, c)) = saved_client(s).await? else {
        return Err(ApiError::bad_request(
            "Tailnet domains need Tailscale. Set it up in Integrations.",
        ));
    };
    let Some(suffix) = cfg.suffix.as_deref() else {
        return Err(ApiError::bad_request(
            "Pyo hasn't seen your tailnet yet. Try again in a minute.",
        ));
    };
    let host = host.to_ascii_lowercase();
    let Some(label) = host.strip_suffix(&format!(".{suffix}")) else {
        return Err(ApiError::bad_request(format!(
            "A tailnet domain ends with .{suffix}."
        )));
    };
    if !valid_name(label) {
        return Err(ApiError::bad_request(
            "Use one name: lowercase letters, digits and dashes.",
        ));
    }
    if RESERVED.contains(&label)
        || cfg.dashboard_name.as_deref() == Some(label)
        || cfg.registry_name.as_deref() == Some(label)
    {
        return Err(ApiError::bad_request("Pyo uses this name itself."));
    }
    let mark = marker(&install_id(&s.db).await?);
    let remote = c.services().await?;
    let mine = remote.get(label).is_some_and(|(cm, _)| *cm == mark);
    let machine = peer_names(&s.db).await?.contains(label)
        || c.devices()
            .await?
            .iter()
            .any(|(_, full, _)| full.split('.').next() == Some(label));
    if (remote.contains_key(label) && !mine) || machine {
        return Err(ApiError::conflict("Taken on your tailnet"));
    }
    // The list can leave some out: the Service itself, read directly.
    match c.service(label).await {
        Ok(None) => Ok(()),
        Ok(Some((comment, _))) if comment == mark => Ok(()),
        Ok(Some(_)) => Err(ApiError::conflict("Taken on your tailnet")),
        Err(e) => {
            tracing::warn!(error = %format!("{e:#}"), "tailnet name check failed");
            Err(ApiError::bad_request(
                "Couldn't check the name on your tailnet. Try again in a minute.",
            ))
        }
    }
}

// ── Integrations › Tailscale ────────────────────────────────────────────────

/// Saves the OAuth client once it signs in (a missing secret keeps the saved
/// one), and says what it can do, check by check.
pub async fn save_client(s: &AppState, client_id: &str, secret: Option<&str>) -> ApiResult<Value> {
    let client_id = client_id.trim();
    if client_id.is_empty()
        || client_id.len() > 100
        || !client_id.bytes().all(|b| b.is_ascii_alphanumeric())
    {
        return Err(ApiError::bad_request(
            "Paste the client ID from Tailscale's Trust credentials page.",
        ));
    }
    let saved = settings(&s.db).await?;
    let secret = match secret.map(str::trim).filter(|x| !x.is_empty()) {
        Some(x)
            if x.starts_with("tskey-client-")
                && x.len() > 20
                && !x.contains(char::is_whitespace) =>
        {
            x.to_owned()
        }
        Some(_) => {
            return Err(ApiError::bad_request(
                "A client secret starts with tskey-client-.",
            ));
        }
        None => match &saved {
            Some(c) if c.client_id == client_id => s.sealer.open(&c.secret_sealed)?,
            _ => return Err(ApiError::bad_request("Paste the client secret too.")),
        },
    };
    if !s.sealer.enabled() {
        return Err(ApiError::new(
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            "This installation has no PYO_SECRET_KEY, so it can't store the secret.",
        ));
    }
    let (checks, suffix) = test(&client(client_id, &secret)).await;
    if let Some(c) = checks.iter().find(|c| c.name == "client" && !c.ok) {
        return Err(ApiError::bad_request(c.message.clone()));
    }
    let mut cfg = saved.unwrap_or_default();
    // Another tailnet: Pyo's names there are picked again.
    if cfg.suffix.is_some() && suffix.is_some() && cfg.suffix != suffix {
        cfg.dashboard_name = None;
        cfg.registry_name = None;
    }
    cfg.client_id = client_id.to_owned();
    cfg.hint = format!("…{}", &secret[secret.len() - 4..]);
    cfg.secret_sealed = s.sealer.seal(&secret)?;
    cfg.suffix = suffix.or(cfg.suffix);
    save(&s.db, &cfg).await?;
    Ok(json!({ "ok": checks.iter().all(|c| c.ok), "checks": checks }))
}

/// The Test button: the saved client's checks.
pub async fn test_saved(s: &AppState) -> ApiResult<Value> {
    let Some((mut cfg, c)) = saved_client(s).await? else {
        return Err(ApiError::bad_request("Tailscale isn't set up."));
    };
    let (checks, suffix) = test(&c).await;
    if suffix.is_some() && cfg.suffix != suffix {
        cfg.suffix = suffix;
        save(&s.db, &cfg).await?;
    }
    Ok(json!({ "ok": checks.iter().all(|c| c.ok), "checks": checks }))
}

/// Forgets the client: Pyo's Services on the tailnet are deleted while it
/// still can, and its servers take them off.
pub async fn remove(s: &AppState) -> ApiResult<()> {
    if let Some((_, c)) = saved_client(s).await? {
        let mark = marker(&install_id(&s.db).await?);
        // Only names Pyo made (its own rows), and each checked again.
        let made: Vec<String> = sqlx::query_scalar("SELECT name FROM pyo.tailnet_services")
            .fetch_all(&s.db)
            .await?;
        for name in &made {
            if let Err(e) = delete_if_ours(&c, name, &mark).await {
                tracing::warn!(name, error = %e, "couldn't delete the tailnet name");
            }
        }
    }
    sqlx::query("UPDATE pyo.tailnet_services SET retired_at = coalesce(retired_at, now()), updated_at = now()")
        .execute(&s.db)
        .await?;
    sqlx::query("DELETE FROM pyo.settings WHERE key = 'tailscale'")
        .execute(&s.db)
        .await?;
    // Without the tailnet, the public dashboard is the way in again.
    sqlx::query("DELETE FROM pyo.settings WHERE key = 'public_dashboard'")
        .execute(&s.db)
        .await?;
    for k in ["tailnet", "registry"] {
        save_address(&s.db, k, None).await?;
    }
    Ok(())
}

/// Renames Pyo's own name on the tailnet ("dashboard" or "registry"). The
/// old one goes once the new one is made.
pub async fn rename(s: &AppState, kind: &str, name: &str) -> ApiResult<()> {
    let name = name.trim().to_ascii_lowercase();
    let Some((mut cfg, c)) = saved_client(s).await? else {
        return Err(ApiError::bad_request("Tailscale isn't set up."));
    };
    if !valid_name(&name) {
        return Err(ApiError::bad_request(
            "Use one name: lowercase letters, digits and dashes.",
        ));
    }
    let other = match kind {
        "dashboard" => &cfg.registry_name,
        "registry" => &cfg.dashboard_name,
        _ => {
            return Err(ApiError::bad_request(
                "The kind is \"dashboard\" or \"registry\".",
            ));
        }
    };
    let domain: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM pyo.tailnet_services WHERE name = $1 AND kind = 'domain' AND retired_at IS NULL)",
    )
    .bind(&name)
    .fetch_one(&s.db)
    .await?;
    if other.as_deref() == Some(name.as_str()) || domain {
        return Err(ApiError::conflict("Pyo uses this name already."));
    }
    let mark = marker(&install_id(&s.db).await?);
    let remote = c.services().await?;
    let machine = peer_names(&s.db).await?.contains(&name)
        || c.devices()
            .await?
            .iter()
            .any(|(_, full, _)| full.split('.').next() == Some(name.as_str()));
    let direct = c.service(&name).await?;
    if remote.get(&name).is_some_and(|(cm, _)| *cm != mark)
        || direct.is_some_and(|(cm, _)| cm != mark)
        || machine
    {
        return Err(ApiError::conflict("Taken on your tailnet"));
    }
    match kind {
        "dashboard" => cfg.dashboard_name = Some(name),
        _ => cfg.registry_name = Some(name),
    }
    save(&s.db, &cfg).await
}

/// For the dashboard: whether Tailscale is set up and Pyo's names on it;
/// the client (never the secret) only for admins.
pub async fn snapshot(db: &PgPool, admin: bool) -> ApiResult<Value> {
    let Some(cfg) = settings(db).await? else {
        return Ok(Value::Null);
    };
    let a = crate::web::addresses();
    let mut v = json!({
        "suffix": cfg.suffix,
        "dashboard": a.tailnet,
        "registry": a.registry,
        "onTailnet": a.host_ip.is_some(),
        "hostMove": host_move(db).await?,
    });
    if admin {
        let rows: Vec<(String, Option<String>)> = sqlx::query_as(
            "SELECT name, error FROM pyo.tailnet_services WHERE kind IN ('dashboard', 'registry') AND retired_at IS NULL",
        )
        .fetch_all(db)
        .await?;
        let error = |n: &Option<String>| {
            n.as_ref()
                .and_then(|n| rows.iter().find(|r| r.0 == *n))
                .and_then(|r| r.1.clone())
        };
        v["clientId"] = json!(cfg.client_id);
        v["hint"] = json!(cfg.hint);
        v["dashboardName"] = json!(cfg.dashboard_name);
        v["dashboardError"] = json!(error(&cfg.dashboard_name));
        v["registryName"] = json!(cfg.registry_name);
        v["registryError"] = json!(error(&cfg.registry_name));
        v["hostName"] = json!(a.host_name);
    }
    Ok(v)
}

/// Pyo's own server moving k3s onto the tailnet: `{"state": "moving"}`, or
/// `{"state": "failed", "error"}`. Null once done, or when it never did.
async fn host_move(db: &PgPool) -> ApiResult<Value> {
    let v: Option<(Value, bool)> = sqlx::query_as(
        "SELECT value, updated_at < now() - interval '30 minutes' FROM pyo.settings WHERE key = 'host_k3s'",
    )
    .fetch_optional(db)
    .await?;
    Ok(match v {
        // A move that never said how it ended: Pyo's own restart lost it.
        Some((v, stale)) if v["state"] == "moving" && !stale => v,
        Some((v, _)) if v["state"] == "failed" => v,
        _ => Value::Null,
    })
}

/// When Pyo's server last got a key to join with: one at a time.
static HOST_KEY_AT: Mutex<Option<Instant>> = Mutex::new(None);

/// Pyo's own server, on its timer (install.sh `host_units`): what its
/// Tailscale says (`ip` and `name`, empty when it's not on a tailnet). Kept
/// as Pyo's tailnet address. Answers `{"state": "off"}` without Tailscale,
/// `{"state": "ok"}` once it's on the tailnet, or `{"state": "join",
/// "authKey"}` to join it (at most one key every ten minutes).
pub async fn host_report(
    s: &AppState,
    ip: &str,
    name: &str,
    k3s: &str,
    error: &str,
) -> ApiResult<Value> {
    // Moving k3s onto the tailnet: what the dashboard shows meanwhile.
    if matches!(k3s, "moving" | "done" | "failed") {
        let error: String = error.trim().chars().take(300).collect();
        sqlx::query(
            "INSERT INTO pyo.settings (key, value) VALUES ('host_k3s', jsonb_build_object('state', $1::text, 'error', $2::text))
             ON CONFLICT (key) DO UPDATE SET value = excluded.value, updated_at = now()",
        )
        .bind(k3s)
        .bind((k3s == "failed").then_some(error))
        .execute(&s.db)
        .await?;
        tracing::info!(state = k3s, "this machine: k3s onto the tailnet");
    }
    let ip = ip.trim();
    let ip = crate::freedomain::tailnet_v4(ip).map(|v| v.to_string());
    let name = name.trim().trim_end_matches('.').to_ascii_lowercase();
    let name = (ip.is_some() && name.ends_with(".ts.net") && name.split('.').all(valid_name))
        .then_some(name);
    let a = crate::web::addresses();
    if a.host_ip != ip || a.host_name != name {
        save_address(&s.db, "hostIp", ip.as_deref()).await?;
        save_address(&s.db, "hostName", name.as_deref()).await?;
    }
    let Some((_, c)) = saved_client(s).await? else {
        return Ok(json!({ "state": "off" }));
    };
    if ip.is_some() {
        return Ok(json!({ "state": "ok" }));
    }
    {
        let mut at = HOST_KEY_AT.lock().unwrap_or_else(|e| e.into_inner());
        if at.is_some_and(|t| t.elapsed() < Duration::from_secs(600)) {
            return Ok(json!({ "state": "wait" }));
        }
        *at = Some(Instant::now());
    }
    let (_, key) = c
        .auth_key("Pyo: its own server")
        .await
        .map_err(no_auth_key("this server"))?;
    tracing::info!("this machine: joining the tailnet");
    Ok(json!({ "state": "join", "authKey": key }))
}

#[cfg(test)]
mod tests {
    #[test]
    fn key_descriptions_are_what_tailscale_takes() {
        use super::key_description;
        assert_eq!(key_description("Pyo: join web-1"), "Pyo join web-1");
        assert_eq!(
            key_description("Pyo: sandbox server sbx_1"),
            "Pyo sandbox server sbx_1"
        );
        assert_eq!(key_description("Pyo: its own server"), "Pyo its own server");
        let long = key_description(&format!("Pyo: join {}", "a".repeat(80)));
        assert_eq!(long.len(), 50);
    }

    use super::*;

    /// A mock Tailscale API: a Service ("docker", someone else's) that the
    /// list leaves out but a direct read finds. Pyo must refuse the name and
    /// never write to it.
    #[tokio::test]
    async fn a_service_the_list_leaves_out_is_never_written() {
        use axum::{
            Json, Router,
            extract::Path,
            routing::{get, post},
        };
        use std::sync::Arc;
        let writes: Arc<Mutex<Vec<String>>> = Arc::default();
        let w = writes.clone();
        let app = Router::new()
            .route("/oauth/token", post(|| async { Json(json!({ "access_token": "t" })) }))
            .route("/tailnet/-/services", get(|| async { Json(json!({ "vipServices": [] })) }))
            .route(
                "/tailnet/-/services/{svc}",
                get(|Path(svc): Path<String>| async move {
                    match svc.as_str() {
                        "svc:docker" => (axum::http::StatusCode::OK, Json(json!({ "name": "svc:docker", "comment": "the user's", "addrs": ["100.100.1.1"] }))),
                        "svc:mine" => (axum::http::StatusCode::OK, Json(json!({ "name": "svc:mine", "comment": "Made by Pyo (abc)", "addrs": ["100.100.1.2"] }))),
                        _ => (axum::http::StatusCode::NOT_FOUND, Json(json!({ "message": "not found" }))),
                    }
                })
                .put({
                    let w = w.clone();
                    move |Path(svc): Path<String>| {
                        let w = w.clone();
                        async move {
                            w.lock().unwrap().push(format!("PUT {svc}"));
                            Json(json!({ "addrs": ["100.100.1.3"] }))
                        }
                    }
                })
                .delete(move |Path(svc): Path<String>| {
                    let w = w.clone();
                    async move {
                        w.lock().unwrap().push(format!("DELETE {svc}"));
                        Json(json!({}))
                    }
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let c = test_client(&format!("http://{addr}"));
        let mark = marker("abc");
        assert!(
            c.services().await.unwrap().is_empty(),
            "the list leaves it out"
        );
        assert_eq!(claim(&c, "docker", &mark).await.unwrap(), Claim::Taken);
        assert!(!delete_if_ours(&c, "docker", &mark).await.unwrap());
        assert_eq!(
            claim(&c, "mine", &mark).await.unwrap(),
            Claim::Ours(vec!["100.100.1.2".into()])
        );
        assert_eq!(
            claim(&c, "blog", &mark).await.unwrap(),
            Claim::Ours(vec!["100.100.1.3".into()])
        );
        assert!(delete_if_ours(&c, "mine", &mark).await.unwrap());
        assert_eq!(*writes.lock().unwrap(), ["PUT svc:blog", "DELETE svc:mine"]);
    }

    #[test]
    fn machines_waiting_for_a_signature() {
        let d = |host: &str, tag: &str, key: &str, err: &str| Device {
            node_id: "n".into(),
            hostname: host.into(),
            tags: vec![tag.into()],
            node_key: key.into(),
            lock_error: err.into(),
        };
        let waiting = lock_waiting(&[
            d(
                "sbx-1",
                TAG,
                "nodekey:ab12",
                "tailnet lock: node key not signed",
            ),
            d("laptop", "tag:other", "nodekey:cd34", "not signed"),
            d("web-1", TAG, "nodekey:ef56", ""),
        ]);
        assert_eq!(
            waiting,
            [(
                "sbx-1".to_owned(),
                "tailscale lock sign nodekey:ab12".to_owned()
            )]
        );
    }

    #[test]
    fn names() {
        assert_eq!(
            suffix_of("server-1.tail1234.ts.net."),
            Some("tail1234.ts.net".into())
        );
        assert_eq!(suffix_of("example.com"), None);
        assert!(
            valid_name("blog-2") && !valid_name("Blog") && !valid_name("-a") && !valid_name("a.b")
        );
        let taken = ["pyo", "pyo-home"];
        assert_eq!(
            pick_name("pyo", "home", |n| taken.contains(&n)),
            "pyo-home-2"
        );
        assert_eq!(
            pick_name("registry", "home", |n| taken.contains(&n)),
            "registry"
        );
        assert_eq!(pick_name("pyo", "home", |n| n == "pyo"), "pyo-home");
        let long = "a".repeat(70);
        assert!(pick_name(&long, "x", |_| false).len() <= 63);
        assert_eq!(marker("abc"), "Made by Pyo (abc)");
    }
}
