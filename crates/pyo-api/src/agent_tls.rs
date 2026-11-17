//! The agent endpoint over mutual TLS (Pyo Cloud's agent.pyo.dev).
//!
//! With `PYO_AGENT_MTLS_ADDR` set, Pyo also listens there for agents only:
//! TLS 1.3, its own CA (the agent pins it, no public CA counts), and a
//! client certificate from that CA naming the agent's cluster
//! (`pyo://team/<team>/cluster/<cluster>` as a URI name, valid a day). An
//! agent gets its first certificate at `/enroll` with the connect command's
//! token, which works once, and renews at `/renew` with the certificate it
//! has. The cluster comes from the certificate, never the request.
//!
//! With `PYO_AGENT_REQUIRE_CERT=true` the agent routes refuse a bearer
//! token alone, on this listener and the main one.
//!
//! The CA's key is a file here (`PYO_AGENT_CA_DIR`, made on first start);
//! a hosted build moves it to a KMS.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::{
    Router,
    extract::{ConnectInfo, State},
    http::StatusCode,
    routing::{get, post},
};
use rcgen::{
    CertificateParams, CertificateSigningRequestParams, DistinguishedName, DnType,
    ExtendedKeyUsagePurpose, IsCa, Issuer, KeyPair, KeyUsagePurpose, SanType, SerialNumber,
};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, pem::PemObject};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{
    AppState,
    agent::AgentCluster,
    error::{ApiError, ApiResult, Json},
    ids::sha256_hex,
};

/// A client certificate's lifetime.
pub const CERT_HOURS: i64 = 24;

/// Whether bearer tokens alone are refused on agent routes.
pub fn require_cert() -> bool {
    static R: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *R.get_or_init(|| {
        std::env::var("PYO_AGENT_REQUIRE_CERT").is_ok_and(|v| matches!(v.trim(), "1" | "true"))
    })
}

/// The agent CA, when this Pyo runs the mTLS endpoint.
pub fn ca() -> Option<&'static Ca> {
    CA.get()
}
static CA: std::sync::OnceLock<Ca> = std::sync::OnceLock::new();

/// Where agents reach the mTLS endpoint (`PYO_AGENT_ENDPOINT`), for the
/// install manifest. None without one.
pub fn endpoint() -> Option<String> {
    ca()?;
    std::env::var("PYO_AGENT_ENDPOINT")
        .ok()
        .map(|v| v.trim().trim_end_matches('/').to_owned())
        .filter(|v| !v.is_empty())
}

pub struct Ca {
    pem: String,
    key_pem: String,
}

impl Ca {
    /// The CA in `dir` (ca.pem, ca.key), made there on first start.
    pub fn load_or_create(dir: &std::path::Path) -> anyhow::Result<Self> {
        let (cert, key) = (dir.join("ca.pem"), dir.join("ca.key"));
        if cert.exists() && key.exists() {
            return Ok(Self {
                pem: std::fs::read_to_string(&cert)?,
                key_pem: std::fs::read_to_string(&key)?,
            });
        }
        std::fs::create_dir_all(dir)?;
        let key_pair = KeyPair::generate()?;
        let mut p = CertificateParams::new(Vec::<String>::new())?;
        let mut dn = DistinguishedName::new();
        dn.push(DnType::CommonName, "Pyo agent CA");
        p.distinguished_name = dn;
        p.is_ca = IsCa::Ca(rcgen::BasicConstraints::Constrained(0));
        p.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
        let now = time::OffsetDateTime::now_utc();
        p.not_before = now - time::Duration::hours(1);
        p.not_after = now + time::Duration::days(3650);
        let c = p.self_signed(&key_pair)?;
        let key_pem = key_pair.serialize_pem();
        {
            use std::io::Write;
            use std::os::unix::fs::OpenOptionsExt;
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&key)?;
            f.write_all(key_pem.as_bytes())?;
        }
        std::fs::write(&cert, c.pem())?;
        tracing::info!(dir = %dir.display(), "made the agent CA");
        Ok(Self { pem: c.pem(), key_pem })
    }

    pub fn pem(&self) -> &str {
        &self.pem
    }

    fn issuer(&self) -> anyhow::Result<Issuer<'static, KeyPair>> {
        let key = KeyPair::from_pem(&self.key_pem)?;
        Ok(Issuer::from_ca_cert_pem(&self.pem, key)?)
    }

    /// A client certificate for a cluster's agent from its CSR (the agent's
    /// own key): its serial (hex), PEM and expiry (ms).
    pub fn issue(&self, csr_pem: &str, team: &str, cluster: &str) -> anyhow::Result<(String, String, i64)> {
        let mut csr = CertificateSigningRequestParams::from_pem(csr_pem)?;
        let mut p = CertificateParams::new(Vec::<String>::new())?;
        p.subject_alt_names = vec![SanType::URI(
            format!("pyo://team/{team}/cluster/{cluster}").try_into()?,
        )];
        let mut dn = DistinguishedName::new();
        dn.push(DnType::CommonName, cluster);
        p.distinguished_name = dn;
        let now = time::OffsetDateTime::now_utc();
        p.not_before = now - time::Duration::minutes(5);
        let not_after = now + time::Duration::hours(CERT_HOURS);
        p.not_after = not_after;
        let serial: [u8; 16] = rand::random();
        // Positive, as X.509 wants.
        let mut serial = serial.to_vec();
        serial[0] &= 0x7f;
        let serial_hex = hex::encode(&serial).trim_start_matches("00").to_owned();
        p.serial_number = Some(SerialNumber::from(serial));
        p.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        p.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
        csr.params = p;
        let cert = csr.signed_by(&self.issuer()?)?;
        Ok((serial_hex, cert.pem(), (not_after.unix_timestamp_nanos() / 1_000_000) as i64))
    }

    /// The endpoint's own certificate, for these names (DNS names or IPs).
    fn server_cert(&self, names: &[String]) -> anyhow::Result<(String, String)> {
        let key = KeyPair::generate()?;
        let mut p = CertificateParams::new(Vec::<String>::new())?;
        for n in names {
            p.subject_alt_names.push(match n.parse::<std::net::IpAddr>() {
                Ok(ip) => SanType::IpAddress(ip),
                Err(_) => SanType::DnsName(n.as_str().try_into()?),
            });
        }
        let mut dn = DistinguishedName::new();
        dn.push(DnType::CommonName, names.first().map(String::as_str).unwrap_or("pyo-agent-endpoint"));
        p.distinguished_name = dn;
        let now = time::OffsetDateTime::now_utc();
        p.not_before = now - time::Duration::hours(1);
        p.not_after = now + time::Duration::days(365);
        p.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        let c = p.signed_by(&key, &self.issuer()?)?;
        Ok((c.pem(), key.serialize_pem()))
    }
}

/// The cluster a client certificate names, and its serial (hex).
pub struct CertIdentity {
    pub team: String,
    pub cluster: String,
    pub serial: String,
}

pub fn identity(der: &[u8]) -> Option<CertIdentity> {
    let (_, cert) = x509_parser::parse_x509_certificate(der).ok()?;
    let serial = hex::encode(cert.raw_serial());
    let san = cert.subject_alternative_name().ok()??;
    for name in &san.value.general_names {
        if let x509_parser::extensions::GeneralName::URI(uri) = name {
            let rest = uri.strip_prefix("pyo://team/")?;
            let (team, cluster) = rest.split_once("/cluster/")?;
            if team.is_empty() || cluster.is_empty() || cluster.contains('/') {
                return None;
            }
            return Some(CertIdentity {
                team: team.to_owned(),
                cluster: cluster.to_owned(),
                // Stored without leading zero bytes' ambiguity: as issued.
                serial: serial.trim_start_matches("00").to_owned(),
            });
        }
    }
    None
}

/// Who's on the other end of an agent-endpoint connection.
#[derive(Clone, Debug)]
pub struct AgentPeer {
    pub addr: SocketAddr,
    /// The verified client certificate (DER), when one was presented.
    pub cert: Option<Arc<CertificateDer<'static>>>,
}

impl axum::extract::connect_info::Connected<axum::serve::IncomingStream<'_, TlsListener>> for AgentPeer {
    fn connect_info(s: axum::serve::IncomingStream<'_, TlsListener>) -> Self {
        s.remote_addr().clone()
    }
}

/// Accepts TLS connections; handshakes run beside the accept loop, so a
/// slow client can't hold up others.
pub struct TlsListener {
    rx: tokio::sync::mpsc::Receiver<(tokio_rustls::server::TlsStream<tokio::net::TcpStream>, AgentPeer)>,
    local: SocketAddr,
}

impl axum::serve::Listener for TlsListener {
    type Io = tokio_rustls::server::TlsStream<tokio::net::TcpStream>;
    type Addr = AgentPeer;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        match self.rx.recv().await {
            Some(x) => x,
            // The accept loop ended: wait forever (the server stops on its own signal).
            None => std::future::pending().await,
        }
    }

    fn local_addr(&self) -> std::io::Result<Self::Addr> {
        Ok(AgentPeer { addr: self.local, cert: None })
    }
}

fn server_config(ca: &Ca, names: &[String]) -> anyhow::Result<rustls::ServerConfig> {
    let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    let mut roots = rustls::RootCertStore::empty();
    for c in CertificateDer::pem_slice_iter(ca.pem.as_bytes()) {
        roots.add(c?)?;
    }
    let verifier = rustls::server::WebPkiClientVerifier::builder_with_provider(Arc::new(roots), provider.clone())
        // Enrolling agents have no certificate yet; every other route wants one.
        .allow_unauthenticated()
        .build()?;
    let (cert_pem, key_pem) = ca.server_cert(names)?;
    let chain = CertificateDer::pem_slice_iter(cert_pem.as_bytes()).collect::<Result<Vec<_>, _>>()?;
    let key = PrivateKeyDer::from_pem_slice(key_pem.as_bytes())?;
    let mut cfg = rustls::ServerConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .with_client_cert_verifier(verifier)
        .with_single_cert(chain, key)?;
    cfg.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(cfg)
}

/// Starts the endpoint when `PYO_AGENT_MTLS_ADDR` is set.
pub async fn start(state: AppState, agent_routes: Router<AppState>) -> anyhow::Result<()> {
    let Some(addr) = std::env::var("PYO_AGENT_MTLS_ADDR").ok().filter(|v| !v.trim().is_empty()) else {
        return Ok(());
    };
    let addr: SocketAddr = addr.trim().parse()?;
    let dir = std::env::var("PYO_AGENT_CA_DIR").unwrap_or_else(|_| "/var/lib/pyo/agent-ca".into());
    let ca = Ca::load_or_create(std::path::Path::new(&dir))?;
    // The names the endpoint answers to: its URL's host, and any extra.
    let mut names: Vec<String> = std::env::var("PYO_AGENT_TLS_NAMES")
        .unwrap_or_default()
        .split(',')
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .collect();
    if let Some(host) = std::env::var("PYO_AGENT_ENDPOINT").ok().and_then(|u| {
        let rest = u.trim().split("://").nth(1)?.split('/').next()?.to_owned();
        let host = if rest.starts_with('[') {
            rest.trim_start_matches('[').split(']').next()?.to_owned()
        } else {
            rest.split(':').next()?.to_owned()
        };
        Some(host)
    }) {
        names.insert(0, host);
    }
    anyhow::ensure!(!names.is_empty(), "PYO_AGENT_MTLS_ADDR needs PYO_AGENT_ENDPOINT or PYO_AGENT_TLS_NAMES");
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(server_config(&ca, &names)?));
    let _ = CA.set(ca);
    let tcp = tokio::net::TcpListener::bind(addr).await?;
    let local = tcp.local_addr()?;
    let (tx, rx) = tokio::sync::mpsc::channel(256);
    tokio::spawn(async move {
        loop {
            let Ok((sock, peer)) = tcp.accept().await else {
                tokio::time::sleep(Duration::from_millis(50)).await;
                continue;
            };
            let acceptor = acceptor.clone();
            let tx = tx.clone();
            tokio::spawn(async move {
                let Ok(Ok(tls)) = tokio::time::timeout(Duration::from_secs(10), acceptor.accept(sock)).await else {
                    return;
                };
                let cert = tls
                    .get_ref()
                    .1
                    .peer_certificates()
                    .and_then(|c| c.first())
                    .map(|c| Arc::new(c.clone().into_owned()));
                let _ = tx.send((tls, AgentPeer { addr: peer, cert })).await;
            });
        }
    });
    let app = Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .nest("/api/agent/v1", agent_routes.merge(routes()))
        .layer(axum::middleware::from_fn(peer_info))
        .layer(axum::extract::DefaultBodyLimit::max(4 * 1024 * 1024))
        .with_state(state);
    tracing::info!(%addr, names = ?names, "agent endpoint (mTLS) listening");
    tokio::spawn(async move {
        let listener = TlsListener { rx, local };
        if let Err(e) = axum::serve(listener, app.into_make_service_with_connect_info::<AgentPeer>()).await {
            tracing::warn!(error = %e, "agent endpoint stopped");
        }
    });
    Ok(())
}

/// Handlers read the peer's address as everywhere else, and the agent
/// extractor reads its certificate.
async fn peer_info(
    ConnectInfo(peer): ConnectInfo<AgentPeer>,
    mut req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    req.extensions_mut().insert(ConnectInfo(peer.addr));
    req.extensions_mut().insert(peer);
    next.run(req).await
}

fn routes() -> Router<AppState> {
    Router::new()
        .route("/enroll", post(enroll))
        .route("/renew", post(renew))
}

#[derive(Deserialize)]
struct EnrollBody {
    token: String,
    csr: String,
}

/// The first client certificate, for the connect command's token. Once.
async fn enroll(
    State(s): State<AppState>,
    peer: Option<axum::Extension<AgentPeer>>,
    Json(b): Json<EnrollBody>,
) -> ApiResult<Json<Value>> {
    let ca = ca().ok_or_else(|| ApiError::not_found("Agent endpoint"))?;
    if peer.is_none() {
        return Err(ApiError::not_found("Agent endpoint"));
    }
    let hash = sha256_hex(b.token.trim());
    let mut tx = s.db.begin().await?;
    let row: Option<(String, String, Option<String>)> = sqlx::query_as(
        "SELECT id, team_id, agent_token_used_hash FROM pyo.clusters WHERE agent_token_hash = $1 FOR UPDATE",
    )
    .bind(&hash)
    .fetch_optional(&mut *tx)
    .await?;
    let (cluster, team, used) =
        row.ok_or_else(|| ApiError::new(StatusCode::UNAUTHORIZED, "Unknown connect token."))?;
    if used.as_deref() == Some(hash.as_str()) {
        return Err(ApiError::conflict(
            "This connect command was used already. Get a new one in the cluster's settings.",
        ));
    }
    let (serial, pem, not_after) = ca
        .issue(&b.csr, &team, &cluster)
        .map_err(|e| ApiError::bad_request(format!("Couldn't read the request: {e}")))?;
    sqlx::query("UPDATE pyo.clusters SET agent_cert_prev_serial = NULL, agent_cert_serial = $2, agent_token_used_hash = $3 WHERE id = $1")
        .bind(&cluster)
        .bind(&serial)
        .bind(&hash)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    tracing::info!(%cluster, "agent enrolled");
    Ok(Json(json!({ "certificate": pem, "notAfter": not_after, "ca": ca.pem() })))
}

#[derive(Deserialize)]
struct RenewBody {
    csr: String,
}

/// A new certificate for the one presented (its cluster, a fresh key).
async fn renew(
    State(s): State<AppState>,
    cluster: AgentCluster,
    peer: Option<axum::Extension<AgentPeer>>,
    Json(b): Json<RenewBody>,
) -> ApiResult<Json<Value>> {
    let ca = ca().ok_or_else(|| ApiError::not_found("Agent endpoint"))?;
    if !peer.is_some_and(|p| p.cert.is_some()) {
        return Err(ApiError::new(StatusCode::UNAUTHORIZED, "Renewing needs the current certificate."));
    }
    let (serial, pem, not_after) = ca
        .issue(&b.csr, &cluster.team_id, &cluster.id)
        .map_err(|e| ApiError::bad_request(format!("Couldn't read the request: {e}")))?;
    sqlx::query("UPDATE pyo.clusters SET agent_cert_prev_serial = agent_cert_serial, agent_cert_serial = $2 WHERE id = $1")
        .bind(&cluster.id)
        .bind(&serial)
        .execute(&s.db)
        .await?;
    Ok(Json(json!({ "certificate": pem, "notAfter": not_after, "ca": ca.pem() })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_cluster_certificate_names_its_cluster() {
        let dir = std::env::temp_dir().join(format!("pyo-ca-test-{}", rand::random::<u32>()));
        let ca = Ca::load_or_create(&dir).unwrap();
        let key = KeyPair::generate().unwrap();
        let csr = CertificateParams::new(Vec::<String>::new())
            .unwrap()
            .serialize_request(&key)
            .unwrap()
            .pem()
            .unwrap();
        let (serial, pem, _) = ca.issue(&csr, "team_a", "cl_1").unwrap();
        let der = CertificateDer::from_pem_slice(pem.as_bytes()).unwrap();
        let id = identity(&der).unwrap();
        assert_eq!((id.team.as_str(), id.cluster.as_str()), ("team_a", "cl_1"));
        assert_eq!(id.serial, serial);
        // The same CA again from its files.
        let again = Ca::load_or_create(&dir).unwrap();
        assert_eq!(again.pem(), ca.pem());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
