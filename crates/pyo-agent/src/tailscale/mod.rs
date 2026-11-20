//! Tailscale through the agent (pyo-proto's tailscale.rs).
//!
//! - **The client**: this cluster's own OAuth client, kept as a Secret in
//!   pyo-system ([`SECRET`]). Only this agent uses it. Scopes:
//!   `devices:core` and `auth_keys` (both with `tag:pyo`), and `services`.
//!   It comes from the one-token setup ([`setup`]), a client pasted by
//!   hand, or the one Pyo kept before clusters kept their own.
//! - **Joining**: each join command carries a fresh auth key from it:
//!   single-use, preauthorized, `tag:pyo`, for an hour ([`Client::auth_key`]).
//! - **Names on the tailnet** are Tailscale Services ("svc:blog" is
//!   "blog.<tailnet>.ts.net"). [`sync`] keeps the cluster's as Pyo wants
//!   them: makes each (marked as this cluster's), approves the cluster's
//!   servers as its hosts, deletes the ones no longer used, and never
//!   touches one it didn't make. The servers do the rest (tailnet.rs).

pub mod sandbox;
pub mod setup;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use k8s_openapi::api::core::v1::Secret;
use kube::api::{DeleteParams, Patch, PatchParams};
use pyo_proto::CommandResult;
use pyo_proto::tailscale::{
    self as ts, Check, ClientState, LockWaiting, NameCheck, NameState, SECRET, SECRET_ID,
    SECRET_KEY, TAG, TailnetSync, TailnetSynced,
};
use serde_json::{Value, json};

use crate::{failed, ok};

/// Tailscale's API. `PYO_TAILSCALE_API_URL` points the agent at another
/// (a fake one, in Pyo's own end-to-end tests).
pub(crate) fn api_url() -> String {
    std::env::var("PYO_TAILSCALE_API_URL")
        .ok()
        .map(|u| u.trim().trim_end_matches('/').to_owned())
        .filter(|u| !u.is_empty())
        .unwrap_or_else(|| "https://api.tailscale.com/api/v2".to_owned())
}

const NS: &str = "pyo-system";

pub(crate) fn http() -> &'static reqwest::Client {
    static C: OnceLock<reqwest::Client> = OnceLock::new();
    C.get_or_init(|| {
        reqwest::Client::builder()
            .user_agent("pyo")
            .timeout(Duration::from_secs(30))
            .build()
            .expect("http client")
    })
}

// ── The cluster's client ────────────────────────────────────────────────────

/// The OAuth client, with its access token while it lasts.
pub struct Client {
    /// The API ("https://api.tailscale.com/api/v2"; a mock in tests).
    base: String,
    id: String,
    secret: String,
    token: Mutex<Option<(String, Instant)>>,
}

/// One per client ID and secret, so tokens are reused.
pub fn client(id: &str, secret: &str) -> Arc<Client> {
    static C: Mutex<Option<Arc<Client>>> = Mutex::new(None);
    let base = api_url();
    let mut g = C.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(c) = g
        .as_ref()
        .filter(|c| c.id == id && c.secret == secret && c.base == base)
    {
        return c.clone();
    }
    let c = Arc::new(Client::at(&base, id, secret));
    *g = Some(c.clone());
    c
}

impl Client {
    pub fn at(base: &str, id: &str, secret: &str) -> Self {
        Client {
            base: base.trim_end_matches('/').to_owned(),
            id: id.to_owned(),
            secret: secret.to_owned(),
            token: Mutex::new(None),
        }
    }
}

/// The client kept in pyo-system: its ID and secret, when there's one.
pub async fn stored(kube: &kube::Client) -> anyhow::Result<Option<(String, String)>> {
    let api = kube::Api::<Secret>::namespaced(kube.clone(), NS);
    let Some(s) = api.get_opt(SECRET).await? else {
        return Ok(None);
    };
    let data = s.data.unwrap_or_default();
    let read = |k: &str| {
        data.get(k)
            .map(|b| String::from_utf8_lossy(&b.0).trim().to_owned())
            .filter(|v| !v.is_empty())
    };
    Ok(read(SECRET_ID).zip(read(SECRET_KEY)))
}

/// The kept client, ready to use.
pub async fn saved(kube: &kube::Client) -> anyhow::Result<Option<Arc<Client>>> {
    Ok(stored(kube).await?.map(|(id, secret)| client(&id, &secret)))
}

/// Keeps the client in pyo-system, replacing any other.
async fn keep(kube: &kube::Client, id: &str, secret: &str) -> anyhow::Result<()> {
    let api = kube::Api::<Secret>::namespaced(kube.clone(), NS);
    let s = json!({
        "apiVersion": "v1",
        "kind": "Secret",
        "metadata": { "name": SECRET, "namespace": NS, "labels": { "app.kubernetes.io/managed-by": crate::MANAGER } },
        "type": "Opaque",
        "stringData": { SECRET_ID: id, SECRET_KEY: secret },
    });
    api.patch(
        SECRET,
        &PatchParams::apply(crate::MANAGER).force(),
        &Patch::Apply(&s),
    )
    .await?;
    Ok(())
}

async fn forget(kube: &kube::Client) -> anyhow::Result<()> {
    let api = kube::Api::<Secret>::namespaced(kube.clone(), NS);
    match api.delete(SECRET, &DeleteParams::default()).await {
        Ok(_) => Ok(()),
        Err(kube::Error::Api(e)) if e.code == 404 => Ok(()),
        Err(e) => Err(e.into()),
    }
}

// ── Tailscale's API ─────────────────────────────────────────────────────────

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

/// What Tailscale answered: its body, and the ETag when there's one.
pub(crate) struct Reply {
    pub text: String,
    pub etag: Option<String>,
}

/// Sends a request to Tailscale's API. Anything but a 2xx is a [`TsError`]
/// with Tailscale's own message, and status 0 when it couldn't be reached.
pub(crate) async fn send(req: reqwest::RequestBuilder) -> Result<Reply, TsError> {
    let r = req.send().await.map_err(|e| TsError {
        status: 0,
        message: format!("Couldn't reach Tailscale: {}", e.without_url()),
    })?;
    let status = r.status().as_u16();
    let etag = r
        .headers()
        .get(reqwest::header::ETAG)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let text = r.text().await.unwrap_or_default();
    if !(200..300).contains(&status) {
        let message = serde_json::from_str::<Value>(&text)
            .ok()
            .and_then(|v| v["message"].as_str().map(str::to_owned))
            .unwrap_or_else(|| text.trim().chars().take(300).collect());
        return Err(TsError { status, message });
    }
    Ok(Reply { text, etag })
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

impl Client {
    pub(crate) async fn access_token(&self) -> anyhow::Result<String> {
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
            .await
            .map_err(|e| TsError {
                status: 0,
                message: format!("Couldn't reach Tailscale: {}", e.without_url()),
            })?;
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
        let text = send(req).await?.text;
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

    /// The tailnet's machines with their hostname, and their key while
    /// Tailnet Lock waits for its signature.
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
            "description": ts::key_description(description),
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

/// Pyo's machines (tag:pyo) that Tailnet Lock keeps out until a signing
/// device signs their key: their hostname, and the command that signs it.
pub fn lock_waiting(devices: &[Device]) -> Vec<LockWaiting> {
    devices
        .iter()
        .filter(|d| {
            !d.lock_error.is_empty()
                && d.node_key.starts_with("nodekey:")
                && d.tags.iter().any(|t| t == TAG)
        })
        .map(|d| LockWaiting {
            name: d.hostname.clone(),
            sign_command: format!("tailscale lock sign {}", d.node_key),
        })
        .collect()
}

/// Pyo's machines (tag:pyo), with what Tailnet Lock waits for.
pub fn machines(devices: &[Device]) -> Vec<ts::Machine> {
    devices
        .iter()
        .filter(|d| d.tags.iter().any(|t| t == TAG))
        .map(|d| ts::Machine {
            node_id: d.node_id.clone(),
            hostname: d.hostname.clone(),
            sign_command: (!d.lock_error.is_empty() && d.node_key.starts_with("nodekey:"))
                .then(|| format!("tailscale lock sign {}", d.node_key)),
        })
        .collect()
}

/// What a wanted name is on the tailnet.
#[derive(Debug, PartialEq)]
pub enum Claim {
    /// Pyo's (made by this cluster): its addresses.
    Ours(Vec<String>),
    /// Someone else's, or one Pyo can't read.
    Taken,
}

/// A wanted name: Pyo's own Service, made now (marked `mark`) if there's
/// none by that name at all. Read directly first, whatever the list said: a
/// name that exists and isn't marked with one of `marks` is never written.
pub async fn claim(c: &Client, name: &str, mark: &str, marks: &[&str]) -> anyhow::Result<Claim> {
    match c.service(name).await? {
        Some((comment, addrs)) if marks.contains(&comment.as_str()) => Ok(Claim::Ours(addrs)),
        Some(_) => Ok(Claim::Taken),
        None => {
            let addrs = c.create_service(name, mark).await?;
            tracing::info!(name, "tailnet name made");
            Ok(Claim::Ours(addrs))
        }
    }
}

/// Deletes a Service only if its own record has one of `marks`.
pub async fn delete_if_ours(c: &Client, name: &str, marks: &[&str]) -> anyhow::Result<bool> {
    match c.service(name).await? {
        Some((comment, _)) if marks.contains(&comment.as_str()) => {
            c.delete_service(name).await?;
            Ok(true)
        }
        _ => Ok(false),
    }
}

// ── Checking the client ─────────────────────────────────────────────────────

fn check(name: &str, ok: bool, message: impl Into<String>) -> Check {
    Check {
        name: name.to_owned(),
        ok,
        message: message.into(),
    }
}

/// What the client can do, check by check, saying what's missing.
pub async fn test(c: &Client) -> (Vec<Check>, Option<String>) {
    let mut out = Vec::new();
    let mut suffix = None;
    if let Err(e) = c.access_token().await {
        out.push(check(
            "client",
            false,
            format!("Couldn't sign in with it. {e}"),
        ));
        return (out, None);
    }
    out.push(check("client", true, "Signed in."));
    match c.devices().await {
        Ok(d) => {
            suffix = d.iter().find_map(|(_, n, _)| ts::suffix_of(n));
            out.push(check("devices", true, "Can read the tailnet's machines."));
        }
        Err(e) => out.push(check(
            "devices",
            false,
            format!("Needs the Devices › Core scope with tag:pyo. {e}"),
        )),
    }
    match c.services().await {
        Ok(_) => out.push(check("services", true, "Can manage Services.")),
        Err(e) => out.push(check(
            "services",
            false,
            format!("Needs the Services scope. {e}"),
        )),
    }
    match c.auth_key("Pyo test").await {
        Ok((id, _)) => {
            let _ = c.delete_key(&id).await;
            out.push(check("authKeys", true, "Can make auth keys with tag:pyo."));
        }
        Err(e) => out.push(check(
            "authKeys",
            false,
            format!("Needs the Auth Keys scope with tag:pyo, and tag:pyo in the tailnet policy's tagOwners. {e}"),
        )),
    }
    (out, suffix)
}

/// The secret's last characters, to tell it apart.
fn hint(secret: &str) -> String {
    let tail: String = secret
        .chars()
        .rev()
        .take(4)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    format!("…{tail}")
}

/// Takes an OAuth client once it signs in: checks it and keeps it. A
/// client that doesn't sign in is refused, and the kept one stays.
pub async fn use_client(
    kube: &kube::Client,
    base: &str,
    id: &str,
    secret: &str,
) -> Result<ClientState, String> {
    let id = id.trim();
    let secret = secret.trim();
    if id.is_empty() || id.len() > 100 || !id.bytes().all(|b| b.is_ascii_alphanumeric()) {
        return Err("Paste the client ID from Tailscale's Trust credentials page.".into());
    }
    if !(secret.starts_with("tskey-client-")
        && secret.len() > 20
        && !secret.contains(char::is_whitespace))
    {
        return Err("A client secret starts with tskey-client-.".into());
    }
    let (checks, suffix) = test(&Client::at(base, id, secret)).await;
    if let Some(c) = checks.iter().find(|c| c.name == "client" && !c.ok) {
        return Err(c.message.clone());
    }
    keep(kube, id, secret)
        .await
        .map_err(|e| format!("Couldn't keep the client in the cluster: {e:#}"))?;
    tracing::info!(client = %id, "tailscale client kept");
    Ok(ClientState {
        ok: checks.iter().all(|c| c.ok),
        checks,
        client_id: id.to_owned(),
        hint: hint(secret),
        suffix,
    })
}

// ── Keeping the names ───────────────────────────────────────────────────────

/// Approvals asked for already: (Service, node).
static APPROVED: Mutex<BTreeSet<(String, String)>> = Mutex::new(BTreeSet::new());

const TAKEN: &str = "Taken on your tailnet";

/// The cluster's names on the tailnet as Pyo wants them ([`TailnetSync`]).
pub async fn sync(c: &Client, w: &TailnetSync) -> anyhow::Result<TailnetSynced> {
    let marks: Vec<&str> = std::iter::once(w.mark.as_str())
        .chain(w.adopt.iter().map(String::as_str))
        .filter(|m| !m.is_empty())
        .collect();
    if marks.is_empty() {
        anyhow::bail!("no mark for the cluster's names");
    }
    let devices = c.devices().await?;
    let suffix = devices.iter().find_map(|(_, n, _)| ts::suffix_of(n));
    let remote = c.services().await?;
    let ours = |n: &str| {
        remote
            .get(n)
            .is_some_and(|(cm, _)| marks.contains(&cm.as_str()))
    };
    let machine = |n: &str| {
        w.peers.iter().any(|p| p == n)
            || devices
                .iter()
                .any(|(_, full, _)| full.split('.').next() == Some(n))
    };
    let taken = |n: &str| (remote.contains_key(n) && !ours(n)) || machine(n);

    // Pyo's own names: picked here when they have none, and sandboxes'
    // picked again when someone else has it.
    let mut wanted: Vec<(String, String)> = Vec::new();
    for o in &w.own {
        let others =
            |n: &str| wanted.iter().any(|(x, _)| x == n) || w.domains.iter().any(|d| d == n);
        let name = if o.keep {
            match o.current.as_deref().filter(|n| !n.is_empty()) {
                Some(n) => n.to_owned(),
                None => ts::pick_name(&o.base, &o.more, |n| taken(n) || others(n)),
            }
        } else {
            ts::own_name(o.current.as_deref(), &o.base, &o.more, |n| {
                taken(n) || others(n)
            })
        };
        if !ts::valid_name(&name) {
            continue;
        }
        if o.current.as_deref().is_some_and(|c| c != name) {
            tracing::info!(old = ?o.current, new = name, kind = o.kind, "tailnet name taken; picked another");
        }
        wanted.push((name, o.kind.clone()));
    }
    for d in &w.domains {
        if ts::valid_name(d) && !wanted.iter().any(|(n, _)| n == d) {
            wanted.push((d.clone(), "domain".into()));
        }
    }

    let mut names = Vec::new();
    for (name, kind) in &wanted {
        let (addrs, error) = match remote.get(name.as_str()) {
            Some((_, addrs)) if ours(name) => (addrs.clone(), None),
            Some(_) => (Vec::new(), Some(TAKEN.to_owned())),
            None if machine(name) => (Vec::new(), Some(TAKEN.to_owned())),
            None => match claim(c, name, &w.mark, &marks).await {
                Ok(Claim::Ours(a)) => (a, None),
                Ok(Claim::Taken) => (Vec::new(), Some(TAKEN.to_owned())),
                Err(e) => {
                    tracing::warn!(name, error = %e, "couldn't make the tailnet name");
                    (
                        Vec::new(),
                        Some("Couldn't make it on your tailnet".to_owned()),
                    )
                }
            },
        };
        names.push(NameState {
            name: name.clone(),
            kind: kind.clone(),
            addrs,
            error,
        });
    }

    // Ours, no longer used: deleted on the tailnet, and the servers take it
    // off. Only names Pyo made for this cluster, each read again first.
    let mut deleted = Vec::new();
    for name in w
        .made
        .iter()
        .filter(|n| ours(n) && !wanted.iter().any(|(x, _)| x == *n))
    {
        match delete_if_ours(c, name, &marks).await {
            Ok(true) => {
                tracing::info!(name, "tailnet name deleted");
                deleted.push(name.clone());
            }
            Ok(false) => tracing::warn!(name, "not deleted: not Pyo's on the tailnet"),
            Err(e) => tracing::warn!(name, error = %e, "couldn't delete the tailnet name"),
        }
    }

    // The servers: tagged, and approved as hosts of the names they advertise.
    let tagged: BTreeMap<&str, bool> = devices
        .iter()
        .map(|(id, _, tags)| (id.as_str(), tags.iter().any(|t| t == TAG)))
        .collect();
    for sv in &w.servers {
        let id = sv.node_id.as_str();
        if !id.is_empty() && tagged.get(id) == Some(&false) {
            match c.tag(id).await {
                Ok(()) => tracing::info!(server = %sv.server, "tagged tag:pyo"),
                Err(e) => {
                    tracing::warn!(server = %sv.server, error = %e, "couldn't tag the server")
                }
            }
        }
    }
    for n in names.iter().filter(|n| n.error.is_none()) {
        let name = &n.name;
        let nodes: Vec<&str> = w
            .servers
            .iter()
            .filter(|sv| sv.advertised.iter().any(|a| a == name))
            .map(|sv| sv.node_id.as_str())
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
        for node in nodes {
            let ok = hosts.get(node) == Some(&true) || c.approve(name, node).await.is_ok();
            if ok && let Ok(mut a) = APPROVED.lock() {
                a.insert((name.clone(), node.to_owned()));
            }
        }
    }

    // Machines whose server was deleted: off the tailnet.
    let mut forgotten = Vec::new();
    for node in &w.forget {
        match c.delete_device(node).await {
            Ok(()) => forgotten.push(node.clone()),
            Err(e) => tracing::warn!(node, error = %e, "couldn't take the machine off the tailnet"),
        }
    }
    let (lock_waiting, machines) = match c.device_details().await {
        Ok(d) => (lock_waiting(&d), machines(&d)),
        Err(e) => {
            tracing::warn!(error = %e, "tailnet lock: couldn't read the tailnet");
            (Vec::new(), Vec::new())
        }
    };
    Ok(TailnetSynced {
        suffix,
        names,
        deleted,
        lock_waiting,
        machines,
        forgotten,
    })
}

/// Whether a name typed for a tailnet domain can be had.
pub async fn check_name(c: &Client, name: &str, marks: &[&str]) -> anyhow::Result<NameCheck> {
    let devices = c.devices().await?;
    let suffix = devices.iter().find_map(|(_, n, _)| ts::suffix_of(n));
    let remote = c.services().await?;
    let mine = remote
        .get(name)
        .is_some_and(|(cm, _)| marks.contains(&cm.as_str()));
    let machine = devices
        .iter()
        .any(|(_, full, _)| full.split('.').next() == Some(name));
    let state = if (remote.contains_key(name) && !mine) || machine {
        "taken"
    } else {
        // The list can leave some out: the Service itself, read directly.
        match c.service(name).await? {
            None => "free",
            Some((comment, _)) if marks.contains(&comment.as_str()) => "ours",
            Some(_) => "taken",
        }
    };
    Ok(NameCheck {
        state: state.into(),
        suffix,
    })
}

// ── The commands ────────────────────────────────────────────────────────────

fn with_data(mut r: CommandResult, data: impl serde::Serialize) -> CommandResult {
    r.data = serde_json::to_value(data).ok();
    r
}

const NO_CLIENT: &str = "Tailscale isn't set up on this cluster.";

async fn need_client(kube: &kube::Client) -> Result<Arc<Client>, CommandResult> {
    match saved(kube).await {
        Ok(Some(c)) => Ok(c),
        Ok(None) => Err(failed(NO_CLIENT)),
        Err(e) => Err(failed(format!(
            "Couldn't read the cluster's Tailscale client: {e:#}"
        ))),
    }
}

/// [`pyo_proto::Command::TailscaleSetup`].
pub async fn run_setup(
    kube: &kube::Client,
    token: String,
    version: Option<String>,
) -> CommandResult {
    let host = setup::Cluster {
        kube: kube.clone(),
        base: api_url(),
    };
    let api = setup::Api::at(&api_url(), setup::Token::from(token));
    match version {
        None => match setup::plan(&api, &host).await {
            Ok(p) => with_data(ok("Read the tailnet."), p),
            Err(e) => failed(e),
        },
        Some(v) => {
            let out = setup::apply(&api, &host, &v).await;
            let mut data = serde_json::to_value(&out).unwrap_or(Value::Null);
            // The client the cluster keeps now, as Pyo shows it (never
            // its secret).
            if let Ok(Some((id, secret))) = stored(kube).await {
                data["client"] = json!({ "clientId": id, "hint": hint(&secret) });
            }
            CommandResult {
                data: Some(data),
                ..ok("Set up.")
            }
        }
    }
}

/// [`pyo_proto::Command::TailscaleClient`].
pub async fn run_client(kube: &kube::Client, id: &str, secret: &str) -> CommandResult {
    match use_client(kube, &api_url(), id, secret).await {
        Ok(state) => with_data(ok("Kept the Tailscale client."), state),
        Err(e) => failed(e),
    }
}

/// [`pyo_proto::Command::TailscaleTest`].
pub async fn run_test(kube: &kube::Client) -> CommandResult {
    let Ok(Some((id, secret))) = stored(kube).await else {
        return failed(NO_CLIENT);
    };
    let (checks, suffix) = test(&client(&id, &secret)).await;
    with_data(
        ok("Checked."),
        ClientState {
            ok: checks.iter().all(|c| c.ok),
            checks,
            client_id: id,
            hint: hint(&secret),
            suffix,
        },
    )
}

/// [`pyo_proto::Command::TailscaleSync`].
pub async fn run_sync(kube: &kube::Client, w: &TailnetSync) -> CommandResult {
    let c = match need_client(kube).await {
        Ok(c) => c,
        Err(r) => return r,
    };
    match sync(&c, w).await {
        Ok(s) => with_data(ok("Synced."), s),
        Err(e) => failed(format!("{e:#}")),
    }
}

/// [`pyo_proto::Command::TailscaleKey`].
pub async fn run_key(kube: &kube::Client, description: &str) -> CommandResult {
    let c = match need_client(kube).await {
        Ok(c) => c,
        Err(r) => return r,
    };
    match c.auth_key(description).await {
        Ok((_, key)) => with_data(ok("Made a key."), json!({ ts::KEY_FIELD: key })),
        Err(e) => {
            tracing::warn!(error = %format!("{e:#}"), "Tailscale made no auth key");
            failed("Tailscale made no auth key. Check Tailscale on this cluster.")
        }
    }
}

/// [`pyo_proto::Command::TailscaleCheckName`].
pub async fn run_check_name(kube: &kube::Client, name: &str, marks: &[String]) -> CommandResult {
    let c = match need_client(kube).await {
        Ok(c) => c,
        Err(r) => return r,
    };
    let marks: Vec<&str> = marks.iter().map(String::as_str).collect();
    match check_name(&c, name, &marks).await {
        Ok(n) => with_data(ok("Checked."), n),
        Err(e) => failed(format!("{e:#}")),
    }
}

/// [`pyo_proto::Command::TailscaleRemove`]: Pyo's names deleted while the
/// client still can, then the client forgotten.
pub async fn run_remove(kube: &kube::Client, names: &[String], marks: &[String]) -> CommandResult {
    let marks: Vec<&str> = marks.iter().map(String::as_str).collect();
    if let Ok(Some(c)) = saved(kube).await {
        for name in names {
            if let Err(e) = delete_if_ours(&c, name, &marks).await {
                tracing::warn!(name, error = %e, "couldn't delete the tailnet name");
            }
        }
    }
    match forget(kube).await {
        Ok(()) => ok("Removed Tailscale."),
        Err(e) => failed(format!(
            "Couldn't remove the cluster's Tailscale client: {e:#}"
        )),
    }
}

#[cfg(test)]
mod tests;
