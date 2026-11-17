//! What the agent trusts, when its install says so (Pyo Cloud):
//!
//! - `PYO_AGENT_CA`: the only CA the API's agent endpoint may present (no
//!   public CAs). The agent proves who it is with a client certificate from
//!   that CA, for its cluster only, valid a day. Its key is made here and
//!   never leaves the cluster (Secret `pyo-agent-identity`). The first one
//!   comes with the connect command's token, which works once; later ones
//!   with the certificate it replaces, at half its lifetime.
//! - `PYO_COMMAND_KEY` and `PYO_CLUSTER_ID`: commands run only when the
//!   signer signed them for this cluster, newer than the last one run, and
//!   not expired (pyo-proto `signing`). The last number run is kept in the
//!   same Secret, so a restart doesn't open the door to replays.
//!
//! Without them (self-hosted), nothing here runs: the token alone works.

use std::sync::{Arc, RwLock};
use std::time::Duration;

use anyhow::Context;
use k8s_openapi::api::core::v1::Secret;
use kube::api::{Patch, PatchParams};
use pyo_proto::signing::{Refused, TrustedKeys};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::sign::CertifiedKey;
use serde_json::{Value, json};

pub const IDENTITY_SECRET: &str = "pyo-agent-identity";

// ── Client certificate ──────────────────────────────────────────────────────

/// The client certificate new connections present; swapped on renewal.
#[derive(Debug, Default)]
pub struct CertSlot(RwLock<Option<Arc<CertifiedKey>>>);

impl rustls::client::ResolvesClientCert for CertSlot {
    fn resolve(&self, _: &[&[u8]], _: &[rustls::SignatureScheme]) -> Option<Arc<CertifiedKey>> {
        self.0.read().ok()?.clone()
    }
    fn has_certs(&self) -> bool {
        self.0.read().map(|c| c.is_some()).unwrap_or(false)
    }
}

impl CertSlot {
    fn set(&self, cert_pem: &str, key_pem: &str) -> anyhow::Result<()> {
        let certs = CertificateDer::pem_slice_iter(cert_pem.as_bytes())
            .collect::<Result<Vec<_>, _>>()
            .context("the certificate isn't PEM")?;
        anyhow::ensure!(!certs.is_empty(), "no certificate");
        let key = PrivateKeyDer::from_pem_slice(key_pem.as_bytes()).context("the key isn't PEM")?;
        let provider = rustls::crypto::aws_lc_rs::default_provider();
        let ck = CertifiedKey::from_der(certs, key, &provider)?;
        *self.0.write().map_err(|_| anyhow::anyhow!("lock"))? = Some(Arc::new(ck));
        Ok(())
    }
}

/// The agent's TLS to the API: Pyo's agent CA only, TLS 1.3, and the
/// client certificate in `slot`.
pub fn client_config(ca_pem: &str, slot: Arc<CertSlot>) -> anyhow::Result<rustls::ClientConfig> {
    let mut roots = rustls::RootCertStore::empty();
    for c in CertificateDer::pem_slice_iter(ca_pem.as_bytes()) {
        roots.add(c.context("PYO_AGENT_CA isn't PEM")?)?;
    }
    anyhow::ensure!(!roots.is_empty(), "PYO_AGENT_CA holds no certificate");
    let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    let mut cfg = rustls::ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .with_root_certificates(roots)
        .with_client_cert_resolver(slot);
    cfg.alpn_protocols = vec![b"http/1.1".to_vec()];
    // Every connection proves who it is anew: a resumed session would carry
    // the identity it started with (none, for the one that enrolled).
    cfg.resumption = rustls::client::Resumption::disabled();
    Ok(cfg)
}

/// What's kept in the identity Secret.
#[derive(Default, Clone)]
struct Stored {
    key: String,
    cert: String,
    not_after: u64,
    last_seq: u64,
}

async fn load(kube: &kube::Client) -> Stored {
    let api = kube::Api::<Secret>::namespaced(kube.clone(), pyo_proto::rbac::NS);
    let Ok(Some(s)) = api.get_opt(IDENTITY_SECRET).await else {
        return Stored::default();
    };
    let get = |k: &str| {
        s.data
            .as_ref()
            .and_then(|d| d.get(k))
            .and_then(|v| String::from_utf8(v.0.clone()).ok())
            .unwrap_or_default()
    };
    Stored {
        key: get("key.pem"),
        cert: get("cert.pem"),
        not_after: get("not-after").trim().parse().unwrap_or(0),
        last_seq: get("last-seq").trim().parse().unwrap_or(0),
    }
}

/// Writes some of the Secret's keys. Each `manager` owns its own keys
/// (server-side apply drops a manager's keys it leaves out).
async fn save(kube: &kube::Client, manager: &str, fields: Value) -> anyhow::Result<()> {
    use base64::Engine as _;
    let api = kube::Api::<Secret>::namespaced(kube.clone(), pyo_proto::rbac::NS);
    let data: serde_json::Map<String, Value> = fields
        .as_object()
        .into_iter()
        .flatten()
        .map(|(k, v)| {
            let raw = v.as_str().unwrap_or_default();
            (k.clone(), json!(base64::engine::general_purpose::STANDARD.encode(raw)))
        })
        .collect();
    let doc = json!({
        "apiVersion": "v1",
        "kind": "Secret",
        "metadata": { "name": IDENTITY_SECRET, "namespace": pyo_proto::rbac::NS, "labels": { "app.kubernetes.io/part-of": "pyo" } },
        "type": "Opaque",
        "data": data,
    });
    api.patch(IDENTITY_SECRET, &PatchParams::apply(manager).force(), &Patch::Apply(doc))
        .await
        .with_context(|| format!("couldn't save {IDENTITY_SECRET}"))?;
    Ok(())
}

/// The agent's identity at the API's agent endpoint.
pub struct Identity {
    pub slot: Arc<CertSlot>,
    pub tls: rustls::ClientConfig,
    base: String,
    token: String,
    kube: kube::Client,
}

impl Identity {
    /// From `PYO_AGENT_CA`; None when the install doesn't set it.
    pub fn from_env(base: &str, token: &str, kube: &kube::Client) -> anyhow::Result<Option<Self>> {
        let Some(ca) = std::env::var("PYO_AGENT_CA").ok().filter(|v| !v.trim().is_empty()) else {
            return Ok(None);
        };
        let slot = Arc::new(CertSlot::default());
        let tls = client_config(&ca, slot.clone())?;
        Ok(Some(Self {
            slot,
            tls,
            base: base.trim_end_matches('/').to_owned(),
            token: token.to_owned(),
            kube: kube.clone(),
        }))
    }

    fn http(&self) -> anyhow::Result<reqwest::Client> {
        Ok(reqwest::Client::builder()
            .use_preconfigured_tls(self.tls.clone())
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(30))
            .build()?)
    }

    /// A certificate to start with: the stored one while it has more than
    /// an hour left, else a new one for the connect command's token.
    /// Retries until it has one.
    pub async fn ensure(&self) {
        let mut wait = Duration::from_secs(2);
        loop {
            match self.try_ensure().await {
                Ok(()) => return,
                Err(e) => {
                    tracing::warn!(error = %format!("{e:#}"), retry_in = ?wait, "no client certificate yet");
                    tokio::time::sleep(wait).await;
                    wait = (wait * 2).min(Duration::from_secs(60));
                }
            }
        }
    }

    async fn try_ensure(&self) -> anyhow::Result<()> {
        let s = load(&self.kube).await;
        if !s.cert.is_empty() && s.not_after > pyo_proto::now_ms() as u64 + 3_600_000 {
            self.slot.set(&s.cert, &s.key)?;
            tracing::info!(not_after = s.not_after, "using the stored client certificate");
            return Ok(());
        }
        self.request("enroll", Some(&self.token)).await
    }

    /// A new key and certificate, through `/enroll` (with the token) or
    /// `/renew` (with the current certificate).
    async fn request(&self, how: &str, token: Option<&str>) -> anyhow::Result<()> {
        let key = rcgen::KeyPair::generate()?;
        let csr = rcgen::CertificateParams::new(Vec::<String>::new())?.serialize_request(&key)?.pem()?;
        let mut body = json!({ "csr": csr });
        if let Some(t) = token {
            body["token"] = json!(t);
        }
        let res = self
            .http()?
            .post(format!("{}/api/agent/v1/{how}", self.base))
            .json(&body)
            .send()
            .await?;
        let status = res.status();
        let v: Value = res.json().await.unwrap_or_default();
        anyhow::ensure!(status.is_success(), "{status}: {}", v["error"].as_str().or(v["message"].as_str()).unwrap_or(""));
        let cert = v["certificate"].as_str().unwrap_or("").to_owned();
        let not_after = v["notAfter"].as_u64().unwrap_or(0);
        anyhow::ensure!(!cert.is_empty() && not_after > 0, "the API sent no certificate");
        let key_pem = key.serialize_pem();
        self.slot.set(&cert, &key_pem)?;
        save(
            &self.kube,
            "pyo-agent-identity",
            json!({ "key.pem": key_pem, "cert.pem": cert, "not-after": not_after.to_string() }),
        )
        .await?;
        tracing::info!(how, not_after, "got a client certificate");
        Ok(())
    }

    /// Renews the certificate at half its lifetime, for as long as the agent runs.
    pub async fn renew_loop(self: Arc<Self>) {
        loop {
            tokio::time::sleep(Duration::from_secs(600)).await;
            let s = load(&self.kube).await;
            let left = s.not_after.saturating_sub(pyo_proto::now_ms() as u64);
            // Certificates last a day: renew once less than half is left.
            if left > 12 * 3_600_000 {
                continue;
            }
            if let Err(e) = self.request("renew", None).await {
                tracing::warn!(error = %format!("{e:#}"), left_ms = left, "couldn't renew the client certificate");
            }
        }
    }
}

// ── Signed commands ─────────────────────────────────────────────────────────

/// Checks commands against the signer's keys, for this cluster.
pub struct Verifier {
    keys: TrustedKeys,
    cluster: String,
    last: tokio::sync::Mutex<u64>,
    kube: kube::Client,
}

impl Verifier {
    /// From `PYO_COMMAND_KEY` and `PYO_CLUSTER_ID`; None when the install
    /// doesn't set a key. A key without the cluster's id is an error: the
    /// agent would otherwise run commands signed for any cluster.
    pub async fn from_env(kube: &kube::Client) -> anyhow::Result<Option<Self>> {
        let Some(k) = std::env::var("PYO_COMMAND_KEY").ok().filter(|v| !v.trim().is_empty()) else {
            return Ok(None);
        };
        let keys = TrustedKeys::parse(&k).map_err(|e| anyhow::anyhow!("PYO_COMMAND_KEY: {e}"))?;
        let cluster = std::env::var("PYO_CLUSTER_ID")
            .ok()
            .filter(|v| !v.trim().is_empty())
            .context("PYO_COMMAND_KEY needs PYO_CLUSTER_ID")?;
        let last = load(kube).await.last_seq;
        tracing::info!(cluster = %cluster, last_seq = last, "running signed commands only");
        Ok(Some(Self {
            keys,
            cluster,
            last: tokio::sync::Mutex::new(last),
            kube: kube.clone(),
        }))
    }

    /// Keeps the commands that check out, each with its signed JSON in
    /// place of what came beside it. The rest, by id, with why.
    pub async fn check(&self, items: Vec<Value>) -> (Vec<Value>, Vec<(String, Refused)>) {
        let mut last = self.last.lock().await;
        let before = *last;
        let mut ok = Vec::with_capacity(items.len());
        let mut refused = vec![];
        for mut item in items {
            let id = item["id"].as_str().unwrap_or("").to_owned();
            let signed: Option<pyo_proto::SignedCommandWire> =
                serde_json::from_value(item["signed"].clone()).ok();
            match self.keys.verify(signed.as_ref(), &id, &self.cluster, *last, pyo_proto::now_ms() as u64) {
                Ok(claims) => match serde_json::from_str::<Value>(&signed.map(|s| s.command).unwrap_or_default()) {
                    Ok(command) => {
                        *last = claims.seq;
                        item["command"] = command;
                        ok.push(item);
                    }
                    Err(_) => refused.push((id, Refused::BadClaims)),
                },
                Err(why) => refused.push((id, why)),
            }
        }
        if *last != before
            && let Err(e) = save(&self.kube, "pyo-agent-commands", json!({ "last-seq": last.to_string() })).await
        {
            tracing::warn!(error = %format!("{e:#}"), "couldn't keep the last command number");
        }
        (ok, refused)
    }
}

// ── Signed requests ─────────────────────────────────────────────────────────

/// The agent's request key (`PYO_AGENT_AUTH=signed`): made here, its public
/// half enrolled with the connect command's token, renewed at half its day.
/// Every request to Pyo is signed with it (pyo-proto `agentauth`), so who
/// the agent is survives a proxy that ends TLS. Kept in pyo-agent-identity.
pub struct KeyIdentity {
    key: RwLock<Option<(String, Arc<pyo_proto::agentauth::RequestKey>)>>,
    /// Pyo's endpoint certificate's CA, when it isn't a public one.
    pub tls: Option<rustls::ClientConfig>,
    base: String,
    token: String,
    kube: kube::Client,
}

fn b64(b: &[u8]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(b)
}

fn unb64(s: &str) -> Option<Vec<u8>> {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.decode(s.trim()).ok()
}

struct StoredKey {
    pkcs8: Vec<u8>,
    key_id: String,
    not_after: u64,
    token_sha: String,
}

async fn load_key(kube: &kube::Client) -> Option<StoredKey> {
    let api = kube::Api::<Secret>::namespaced(kube.clone(), pyo_proto::rbac::NS);
    let s = api.get_opt(IDENTITY_SECRET).await.ok()??;
    let d = s.data?;
    let get = |k: &str| d.get(k).and_then(|v| String::from_utf8(v.0.clone()).ok()).unwrap_or_default();
    let pkcs8 = unb64(&get("request-key"))?;
    let key_id = get("key-id");
    (!key_id.is_empty()).then(|| StoredKey {
        pkcs8,
        key_id,
        not_after: get("key-not-after").trim().parse().unwrap_or(0),
        token_sha: get("key-token"),
    })
}

impl KeyIdentity {
    /// From `PYO_AGENT_AUTH=signed`; None otherwise.
    pub fn from_env(base: &str, token: &str, kube: &kube::Client) -> anyhow::Result<Option<Self>> {
        if std::env::var("PYO_AGENT_AUTH").ok().as_deref().map(str::trim) != Some("signed") {
            return Ok(None);
        }
        let tls = match std::env::var("PYO_AGENT_CA").ok().filter(|v| !v.trim().is_empty()) {
            Some(ca) => Some(client_config(&ca, Arc::new(CertSlot::default()))?),
            None => None,
        };
        Ok(Some(Self {
            key: RwLock::new(None),
            tls,
            base: base.trim_end_matches('/').to_owned(),
            token: token.to_owned(),
            kube: kube.clone(),
        }))
    }

    /// The headers that sign a request (`path` with its query).
    pub fn headers(&self, method: &str, path: &str, body: &[u8]) -> Vec<(&'static str, String)> {
        let Some((id, key)) = self.key.read().ok().and_then(|k| k.clone()) else {
            return vec![];
        };
        key.sign(&id, method, path, body, pyo_proto::now_ms() / 1000).to_vec()
    }

    fn http(&self) -> anyhow::Result<reqwest::Client> {
        let mut b = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(30));
        if let Some(tls) = &self.tls {
            b = b.use_preconfigured_tls(tls.clone());
        }
        Ok(b.build()?)
    }

    /// A key to start with: the stored one while it has more than an hour
    /// left and came from this token, else a new one. Retries until it has one.
    pub async fn ensure(&self) {
        let mut wait = Duration::from_secs(2);
        loop {
            match self.try_ensure().await {
                Ok(()) => return,
                Err(e) => {
                    tracing::warn!(error = %format!("{e:#}"), retry_in = ?wait, "no request key yet");
                    tokio::time::sleep(wait).await;
                    wait = (wait * 2).min(Duration::from_secs(60));
                }
            }
        }
    }

    async fn try_ensure(&self) -> anyhow::Result<()> {
        if let Some(s) = load_key(&self.kube).await
            && s.token_sha == pyo_proto::token_hash(&self.token)
            && s.not_after > pyo_proto::now_ms() as u64 + 3_600_000
        {
            let key = pyo_proto::agentauth::RequestKey::from_pkcs8(&s.pkcs8).map_err(anyhow::Error::msg)?;
            *self.key.write().map_err(|_| anyhow::anyhow!("lock"))? = Some((s.key_id.clone(), Arc::new(key)));
            tracing::info!(key = %s.key_id, "using the stored request key");
            return Ok(());
        }
        self.request(true).await
    }

    /// A new key, enrolled with the token (`first`) or swapped in with the current one.
    async fn request(&self, first: bool) -> anyhow::Result<()> {
        let pkcs8 = pyo_proto::agentauth::RequestKey::generate_pkcs8().map_err(anyhow::Error::msg)?;
        let key = pyo_proto::agentauth::RequestKey::from_pkcs8(&pkcs8).map_err(anyhow::Error::msg)?;
        let (path, body) = if first {
            ("/api/agent/v1/enroll-key", json!({ "token": self.token, "publicKey": key.public_key() }))
        } else {
            ("/api/agent/v1/renew-key", json!({ "publicKey": key.public_key() }))
        };
        let body = serde_json::to_vec(&body)?;
        let url = format!("{}{path}", self.base);
        let mut req = self.http()?.post(&url).header("content-type", "application/json");
        if !first {
            let signed_path = reqwest::Url::parse(&url)?.path().to_owned();
            for (k, v) in self.headers("POST", &signed_path, &body) {
                req = req.header(k, v);
            }
        }
        let res = req.body(body).send().await?;
        let status = res.status();
        let v: Value = res.json().await.unwrap_or_default();
        anyhow::ensure!(status.is_success(), "{status}: {}", v["error"].as_str().unwrap_or(""));
        let key_id = v["keyId"].as_str().unwrap_or("").to_owned();
        let not_after = v["notAfter"].as_u64().unwrap_or(0);
        anyhow::ensure!(!key_id.is_empty(), "Pyo sent no key id");
        save(
            &self.kube,
            "pyo-agent-key",
            json!({
                "request-key": b64(&pkcs8),
                "key-id": key_id,
                "key-not-after": not_after.to_string(),
                "key-token": pyo_proto::token_hash(&self.token),
            }),
        )
        .await?;
        *self.key.write().map_err(|_| anyhow::anyhow!("lock"))? = Some((key_id.clone(), Arc::new(key)));
        tracing::info!(key = %key_id, first, not_after, "got a request key");
        Ok(())
    }

    /// Swaps in a new key at half the current one's day, while the agent runs.
    pub async fn renew_loop(self: Arc<Self>) {
        loop {
            tokio::time::sleep(Duration::from_secs(600)).await;
            let left = load_key(&self.kube)
                .await
                .map(|s| s.not_after.saturating_sub(pyo_proto::now_ms() as u64))
                .unwrap_or(0);
            if left > 12 * 3_600_000 {
                continue;
            }
            if let Err(e) = self.request(false).await {
                tracing::warn!(error = %format!("{e:#}"), left_ms = left, "couldn't renew the request key");
            }
        }
    }
}

/// `pyo-agent sign-request METHOD PATH [--at UNIX_SECONDS] [--key-id ID]`:
/// the two headers that sign a request with this agent's stored key, empty
/// body. For tests of the API's checks (a replay, a stale time, a key id
/// that isn't this key's).
pub async fn sign_request_cli(args: &[String]) -> anyhow::Result<()> {
    let (Some(method), Some(path)) = (args.first(), args.get(1)) else {
        anyhow::bail!("usage: pyo-agent sign-request METHOD PATH [--at UNIX_SECONDS] [--key-id ID]");
    };
    let mut at = pyo_proto::now_ms() / 1000;
    let mut key_id = None;
    let mut i = 2;
    while i < args.len() {
        match (args[i].as_str(), args.get(i + 1)) {
            ("--at", Some(v)) => at = v.parse()?,
            ("--key-id", Some(v)) => key_id = Some(v.clone()),
            _ => anyhow::bail!("unknown option {}", args[i]),
        }
        i += 2;
    }
    let kube = kube::Client::try_default().await?;
    let s = load_key(&kube).await.context("no request key here")?;
    let key = pyo_proto::agentauth::RequestKey::from_pkcs8(&s.pkcs8).map_err(anyhow::Error::msg)?;
    for (k, v) in key.sign(key_id.as_deref().unwrap_or(&s.key_id), method, path, b"", at) {
        println!("{k}: {v}");
    }
    Ok(())
}
