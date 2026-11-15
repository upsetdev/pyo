//! A small OCI registry client: just enough to pull an image for a
//! template. It resolves a reference to a manifest for this server's
//! platform and downloads the layers, each checked against its digest.
//!
//! It speaks the distribution API over HTTPS (or plain HTTP for Pyo's own
//! registry on the tailnet) and logs in the two ways registries ask for:
//! Basic, and a Bearer token from the registry's token service, anonymous
//! or with the login it was given. Redirects to a blob store are followed,
//! without the login once the host changes.
//!
//! Every request gets its own connection. A pull makes a handful, so
//! pooling wouldn't buy anything.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Context, bail};
use http_body_util::{BodyExt, Empty};
use hyper::body::{Bytes, Incoming};
use hyper::header::{self, HeaderValue};
use hyper::{Request, Response, StatusCode, Uri};
use hyper_util::rt::TokioIo;
use pyo_proto::sandbox::RegistryAuth;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// A body that sends nothing for this long is given up on.
const IDLE_TIMEOUT: Duration = Duration::from_secs(60);
/// Manifests and token answers are small.
const MAX_SMALL_BODY: usize = 4 << 20;
const MAX_REDIRECTS: usize = 5;
const USER_AGENT: &str = concat!("pyo-sandbox/", env!("CARGO_PKG_VERSION"));

const INDEX_TYPES: [&str; 2] = [
    "application/vnd.oci.image.index.v1+json",
    "application/vnd.docker.distribution.manifest.list.v2+json",
];
const MANIFEST_TYPES: [&str; 2] = [
    "application/vnd.oci.image.manifest.v1+json",
    "application/vnd.docker.distribution.manifest.v2+json",
];

/// An image reference split into where it lives and what it names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageRef {
    /// `host[:port]` of the registry's API.
    pub registry: String,
    pub repository: String,
    /// A tag or a `sha256:` digest.
    pub reference: String,
}

impl ImageRef {
    /// Parses `alpine`, `python:3.13-alpine`, `ghcr.io/o/r:t`,
    /// `100.64.0.1:5006/team/p/s@sha256:…`. Docker Hub names get its API
    /// host and the `library/` of official images.
    pub fn parse(image: &str) -> anyhow::Result<ImageRef> {
        let image = image.trim();
        if image.is_empty() || image.len() > 512 || image.contains(char::is_whitespace) {
            bail!("{image:?} isn't an image reference.");
        }
        let (name, reference) = match image.split_once('@') {
            Some((n, d)) => (n, d.to_string()),
            None => {
                // A tag's colon comes after the last slash. A colon before
                // it is a registry port.
                let slash = image.rfind('/').map(|i| i + 1).unwrap_or(0);
                match image[slash..].rfind(':') {
                    Some(i) => (&image[..slash + i], image[slash + i + 1..].to_string()),
                    None => (image, "latest".to_string()),
                }
            }
        };
        let (registry, repository) = match name.split_once('/') {
            Some((first, rest))
                if first.contains('.') || first.contains(':') || first == "localhost" =>
            {
                (first.to_string(), rest.to_string())
            }
            _ => ("docker.io".to_string(), name.to_string()),
        };
        let host_ok = match registry.rsplit_once(':') {
            Some((h, port)) => !h.is_empty() && port.parse::<u16>().is_ok(),
            None => !registry.is_empty(),
        } && registry
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b".-:[]".contains(&b));
        if !host_ok {
            bail!("{image:?} names a bad registry.");
        }
        let repository = if registry == "docker.io" && !repository.contains('/') {
            format!("library/{repository}")
        } else {
            repository
        };
        let registry = match registry.as_str() {
            "docker.io" | "index.docker.io" => "registry-1.docker.io".to_string(),
            _ => registry,
        };
        let repo_ok = !repository.is_empty()
            && repository.split('/').all(|p| {
                !p.is_empty()
                    && p.bytes().all(|b| {
                        b.is_ascii_lowercase() || b.is_ascii_digit() || b"._-".contains(&b)
                    })
            });
        if !repo_ok {
            bail!("{image:?} isn't an image reference.");
        }
        let ref_ok = if let Some(hex) = reference.strip_prefix("sha256:") {
            hex.len() == 64 && hex.bytes().all(|b| b.is_ascii_hexdigit())
        } else {
            !reference.is_empty()
                && reference.len() <= 128
                && reference
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
        };
        if !ref_ok {
            bail!("{image:?} has a bad tag or digest.");
        }
        Ok(ImageRef {
            registry,
            repository,
            reference,
        })
    }
}

/// One layer of an image.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Layer {
    pub media_type: String,
    pub digest: String,
    pub size: u64,
}

/// What a pull resolves to.
#[derive(Debug, Clone)]
pub struct Image {
    /// The platform manifest's digest.
    pub digest: String,
    pub layers: Vec<Layer>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Manifest {
    #[serde(default)]
    schema_version: u32,
    #[serde(default)]
    manifests: Option<Vec<IndexEntry>>,
    #[serde(default)]
    layers: Option<Vec<Layer>>,
}

#[derive(Deserialize)]
struct IndexEntry {
    digest: String,
    #[serde(default)]
    platform: Option<Platform>,
}

#[derive(Deserialize)]
struct Platform {
    #[serde(default)]
    architecture: String,
    #[serde(default)]
    os: String,
    #[serde(default)]
    variant: Option<String>,
}

/// The OCI name of this machine's architecture.
pub fn oci_arch() -> &'static str {
    match std::env::consts::ARCH {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        other => other,
    }
}

pub struct Client {
    image: ImageRef,
    plain_http: bool,
    auth: Option<RegistryAuth>,
    bearer: Option<String>,
    tls: tokio_rustls::TlsConnector,
}

impl Client {
    pub fn new(image: ImageRef, plain_http: bool, auth: Option<RegistryAuth>) -> Client {
        let mut roots = tokio_rustls::rustls::RootCertStore::empty();
        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        let provider = Arc::new(tokio_rustls::rustls::crypto::ring::default_provider());
        let cfg = tokio_rustls::rustls::ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .expect("ring has the default protocol versions")
            .with_root_certificates(roots)
            .with_no_client_auth();
        Client {
            image,
            plain_http,
            auth,
            bearer: None,
            tls: tokio_rustls::TlsConnector::from(Arc::new(cfg)),
        }
    }

    fn url(&self, path: &str) -> String {
        let scheme = if self.plain_http { "http" } else { "https" };
        format!(
            "{scheme}://{}/v2/{}/{path}",
            self.image.registry, self.image.repository
        )
    }

    /// The manifest for this server's platform and its layers.
    pub async fn resolve(&mut self) -> anyhow::Result<Image> {
        let reference = self.image.reference.clone();
        let (body, digest) = self.manifest(&reference).await?;
        let m: Manifest = serde_json::from_slice(&body).context("reading the image manifest")?;
        let m = if let Some(list) = m.manifests {
            let arch = oci_arch();
            let pick = list
                .iter()
                .find(|e| {
                    e.platform.as_ref().is_some_and(|p| {
                        p.os == "linux"
                            && p.architecture == arch
                            && (arch != "arm64" || p.variant.as_deref().is_none_or(|v| v == "v8"))
                    })
                })
                .with_context(|| format!("The image has no linux/{arch} variant."))?;
            let (body, digest) = self.manifest(&pick.digest.clone()).await?;
            let m: Manifest =
                serde_json::from_slice(&body).context("reading the image manifest")?;
            (m, digest)
        } else {
            (m, digest)
        };
        let (m, digest) = m;
        if m.schema_version == 1 {
            bail!("The image uses the old Docker schema 1. Push it again with a newer Docker.");
        }
        if m.manifests.is_some() {
            bail!("The image index points at another index.");
        }
        let layers = m.layers.context("The image manifest has no layers.")?;
        for l in &layers {
            if l.media_type.contains("foreign") {
                bail!("The image has a foreign layer ({}).", l.media_type);
            }
        }
        Ok(Image { digest, layers })
    }

    /// A manifest's body and digest. A digest reference is checked.
    async fn manifest(&mut self, reference: &str) -> anyhow::Result<(Vec<u8>, String)> {
        let accept = INDEX_TYPES
            .iter()
            .chain(MANIFEST_TYPES.iter())
            .copied()
            .collect::<Vec<_>>()
            .join(", ");
        let res = self
            .get(&self.url(&format!("manifests/{reference}")), Some(&accept))
            .await?;
        let status = res.status();
        let body = read_small(res).await?;
        if !status.is_success() {
            bail!(
                "The registry answered {status} for the manifest of {}: {}",
                self.image.repository,
                error_text(&body)
            );
        }
        let digest = format!("sha256:{}", hex(&Sha256::digest(&body)));
        if reference.starts_with("sha256:") && digest != reference {
            bail!("The manifest doesn't match its digest {reference}.");
        }
        Ok((body, digest))
    }

    /// Downloads a blob to `out`, checks its digest and size, and counts
    /// its bytes in `progress`.
    pub async fn blob(
        &mut self,
        layer: &Layer,
        out: &Path,
        progress: &AtomicU64,
    ) -> anyhow::Result<()> {
        let Some(want) = layer.digest.strip_prefix("sha256:") else {
            bail!("The layer digest {} isn't sha256.", layer.digest);
        };
        if want.len() != 64 || !want.bytes().all(|b| b.is_ascii_hexdigit()) {
            bail!("The layer digest {} is malformed.", layer.digest);
        }
        let res = self
            .get(&self.url(&format!("blobs/{}", layer.digest)), None)
            .await?;
        let status = res.status();
        if !status.is_success() {
            let body = read_small(res).await.unwrap_or_default();
            bail!(
                "The registry answered {status} for layer {}: {}",
                layer.digest,
                error_text(&body)
            );
        }
        let mut file = tokio::fs::File::create(out)
            .await
            .with_context(|| format!("creating {}", out.display()))?;
        let mut hash = Sha256::new();
        let mut got: u64 = 0;
        let mut body = res.into_body();
        loop {
            let frame = tokio::time::timeout(IDLE_TIMEOUT, body.frame())
                .await
                .context("The registry stopped sending the layer.")?;
            let Some(frame) = frame else { break };
            let frame = frame.context("reading a layer")?;
            if let Ok(data) = frame.into_data() {
                got += data.len() as u64;
                if got > layer.size {
                    bail!("Layer {} is bigger than its manifest says.", layer.digest);
                }
                hash.update(&data);
                file.write_all(&data).await?;
                progress.fetch_add(data.len() as u64, Ordering::Relaxed);
            }
        }
        file.flush().await?;
        if got != layer.size {
            bail!(
                "Layer {} ended after {got} of {} bytes.",
                layer.digest,
                layer.size
            );
        }
        if !hex(&hash.finalize()).eq_ignore_ascii_case(want) {
            bail!("Layer {} doesn't match its digest.", layer.digest);
        }
        Ok(())
    }

    /// GET with the registry's login: tries, and on a 401 logs in the way
    /// the registry asks and tries once more.
    async fn get(&mut self, url: &str, accept: Option<&str>) -> anyhow::Result<Response<Incoming>> {
        for attempt in 0..2 {
            let mut headers: Vec<(header::HeaderName, String)> = Vec::new();
            if let Some(a) = accept {
                headers.push((header::ACCEPT, a.to_string()));
            }
            if let Some(t) = &self.bearer {
                headers.push((header::AUTHORIZATION, format!("Bearer {t}")));
            } else if let Some(a) = &self.auth {
                headers.push((header::AUTHORIZATION, basic(a)));
            }
            let res = self.send(url, &headers).await?;
            if res.status() != StatusCode::UNAUTHORIZED || attempt == 1 {
                if res.status() == StatusCode::UNAUTHORIZED {
                    bail!(
                        "The registry {} refused the login for {}.",
                        self.image.registry,
                        self.image.repository
                    );
                }
                return Ok(res);
            }
            let challenge = res
                .headers()
                .get(header::WWW_AUTHENTICATE)
                .and_then(|v| v.to_str().ok())
                .unwrap_or_default()
                .to_string();
            let (scheme, params) = parse_challenge(&challenge);
            if scheme.eq_ignore_ascii_case("bearer") {
                self.bearer = Some(self.token(&params).await?);
            } else if self.auth.is_none() {
                bail!(
                    "The registry {} needs a login for {}.",
                    self.image.registry,
                    self.image.repository
                );
            }
        }
        unreachable!("the loop returns")
    }

    /// A token from the registry's token service, for pulling this
    /// repository.
    async fn token(&self, params: &[(String, String)]) -> anyhow::Result<String> {
        let get = |k: &str| {
            params
                .iter()
                .find(|(n, _)| n.eq_ignore_ascii_case(k))
                .map(|(_, v)| v.as_str())
        };
        let realm = get("realm").context("The registry's login challenge has no realm.")?;
        if !realm.starts_with("https://") && !(self.plain_http && realm.starts_with("http://")) {
            bail!("The registry's token service {realm} isn't HTTPS.");
        }
        let scope = format!("repository:{}:pull", self.image.repository);
        let mut url = format!("{realm}?scope={}", encode(&scope));
        if let Some(service) = get("service") {
            url.push_str(&format!("&service={}", encode(service)));
        }
        let headers: Vec<(header::HeaderName, String)> = match &self.auth {
            Some(a) => vec![(header::AUTHORIZATION, basic(a))],
            None => vec![],
        };
        let res = self.send(&url, &headers).await?;
        let status = res.status();
        let body = read_small(res).await?;
        if !status.is_success() {
            bail!(
                "The registry's token service answered {status}: {}",
                error_text(&body)
            );
        }
        #[derive(Deserialize)]
        struct Answer {
            #[serde(default)]
            token: Option<String>,
            #[serde(default)]
            access_token: Option<String>,
        }
        let a: Answer = serde_json::from_slice(&body).context("reading the registry token")?;
        a.token
            .or(a.access_token)
            .filter(|t| !t.is_empty())
            .context("The registry's token service gave no token.")
    }

    /// One GET, following redirects. The login goes only to the host it
    /// was meant for.
    async fn send(
        &self,
        url: &str,
        headers: &[(header::HeaderName, String)],
    ) -> anyhow::Result<Response<Incoming>> {
        let first: Uri = url.parse().with_context(|| format!("bad URL {url}"))?;
        let mut uri = first.clone();
        for _ in 0..=MAX_REDIRECTS {
            let same_host = uri.authority() == first.authority();
            let res = self.send_once(&uri, headers, same_host).await?;
            if !res.status().is_redirection() || res.status() == StatusCode::NOT_MODIFIED {
                return Ok(res);
            }
            let loc = res
                .headers()
                .get(header::LOCATION)
                .and_then(|v| v.to_str().ok())
                .context("The registry redirected without a location.")?;
            uri = if loc.starts_with('/') {
                let auth = uri.authority().context("no host")?.as_str();
                let scheme = uri.scheme_str().unwrap_or("https");
                format!("{scheme}://{auth}{loc}").parse()?
            } else {
                loc.parse()
                    .with_context(|| format!("bad redirect location {loc}"))?
            };
            if uri.scheme_str() == Some("http") && first.scheme_str() == Some("https") {
                bail!("The registry redirected from HTTPS to HTTP.");
            }
        }
        bail!("The registry redirected too often.")
    }

    async fn send_once(
        &self,
        uri: &Uri,
        headers: &[(header::HeaderName, String)],
        with_login: bool,
    ) -> anyhow::Result<Response<Incoming>> {
        let https = match uri.scheme_str() {
            Some("https") => true,
            Some("http") => false,
            _ => bail!("{uri} isn't HTTP."),
        };
        let host = uri.host().context("no host")?.to_string();
        let port = uri.port_u16().unwrap_or(if https { 443 } else { 80 });
        let path = uri.path_and_query().map(|p| p.as_str()).unwrap_or("/");
        let authority = uri.authority().context("no host")?.as_str();
        let mut req = Request::get(path)
            .header(header::HOST, authority)
            .header(header::USER_AGENT, USER_AGENT);
        for (k, v) in headers {
            if *k == header::AUTHORIZATION && !with_login {
                continue;
            }
            req = req.header(k, HeaderValue::from_str(v)?);
        }
        let req = req.body(Empty::<Bytes>::new())?;
        let host_port = host
            .trim_start_matches('[')
            .trim_end_matches(']')
            .to_string();
        let tcp = tokio::time::timeout(
            CONNECT_TIMEOUT,
            tokio::net::TcpStream::connect((host_port.as_str(), port)),
        )
        .await
        .with_context(|| format!("connecting to {host}:{port} timed out"))?
        .with_context(|| format!("connecting to {host}:{port}"))?;
        if https {
            let name = rustls_pki_types::ServerName::try_from(host_port.clone())
                .with_context(|| format!("{host} isn't a host name"))?;
            let tls = tokio::time::timeout(CONNECT_TIMEOUT, self.tls.connect(name, tcp))
                .await
                .with_context(|| format!("TLS with {host} timed out"))?
                .with_context(|| format!("TLS with {host}"))?;
            send_on(tls, req).await
        } else {
            send_on(tcp, req).await
        }
    }
}

async fn send_on<T>(io: T, req: Request<Empty<Bytes>>) -> anyhow::Result<Response<Incoming>>
where
    T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(io)).await?;
    tokio::spawn(async move {
        let _ = conn.await;
    });
    Ok(tokio::time::timeout(IDLE_TIMEOUT, sender.send_request(req))
        .await
        .context("The registry didn't answer in time.")??)
}

async fn read_small(res: Response<Incoming>) -> anyhow::Result<Vec<u8>> {
    let mut body = res.into_body();
    let mut out = Vec::new();
    while let Some(frame) = tokio::time::timeout(IDLE_TIMEOUT, body.frame())
        .await
        .context("The registry stopped sending.")?
    {
        if let Ok(data) = frame?.into_data() {
            out.extend_from_slice(&data);
            if out.len() > MAX_SMALL_BODY {
                bail!("The registry's answer is too big.");
            }
        }
    }
    Ok(out)
}

/// The registry's own error message, or the start of the body.
fn error_text(body: &[u8]) -> String {
    #[derive(Deserialize)]
    struct Errors {
        errors: Vec<E>,
    }
    #[derive(Deserialize)]
    struct E {
        #[serde(default)]
        message: String,
    }
    if let Ok(e) = serde_json::from_slice::<Errors>(body)
        && let Some(first) = e.errors.first()
    {
        return first.message.clone();
    }
    String::from_utf8_lossy(body).chars().take(200).collect()
}

fn basic(a: &RegistryAuth) -> String {
    use base64::Engine as _;
    format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode(format!("{}:{}", a.username, a.password))
    )
}

/// `Bearer realm="…",service="…"` into its scheme and parameters.
pub fn parse_challenge(v: &str) -> (String, Vec<(String, String)>) {
    let v = v.trim();
    let (scheme, rest) = v.split_once(' ').unwrap_or((v, ""));
    let mut params = Vec::new();
    let mut chars = rest.chars().peekable();
    loop {
        while chars.peek().is_some_and(|c| *c == ',' || c.is_whitespace()) {
            chars.next();
        }
        let key: String = chars.by_ref().take_while(|c| *c != '=').collect();
        if key.is_empty() {
            break;
        }
        let mut val = String::new();
        if chars.peek() == Some(&'"') {
            chars.next();
            while let Some(c) = chars.next() {
                match c {
                    '\\' => {
                        if let Some(n) = chars.next() {
                            val.push(n);
                        }
                    }
                    '"' => break,
                    c => val.push(c),
                }
            }
        } else {
            while let Some(c) = chars.peek() {
                if *c == ',' {
                    break;
                }
                val.push(*c);
                chars.next();
            }
        }
        params.push((key.trim().to_string(), val.trim().to_string()));
    }
    (scheme.to_string(), params)
}

fn encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

pub fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(reg: &str, repo: &str, reference: &str) -> ImageRef {
        ImageRef {
            registry: reg.into(),
            repository: repo.into(),
            reference: reference.into(),
        }
    }

    #[test]
    fn references() {
        let hub = "registry-1.docker.io";
        assert_eq!(
            ImageRef::parse("alpine").unwrap(),
            r(hub, "library/alpine", "latest")
        );
        assert_eq!(
            ImageRef::parse("python:3.13-alpine").unwrap(),
            r(hub, "library/python", "3.13-alpine")
        );
        assert_eq!(
            ImageRef::parse("docker.io/bitnami/redis:7").unwrap(),
            r(hub, "bitnami/redis", "7")
        );
        assert_eq!(
            ImageRef::parse("ghcr.io/o/r/x:v1.2").unwrap(),
            r("ghcr.io", "o/r/x", "v1.2")
        );
        let d = format!("sha256:{}", "a".repeat(64));
        assert_eq!(
            ImageRef::parse(&format!("100.64.0.1:5006/acme/app/api@{d}")).unwrap(),
            r("100.64.0.1:5006", "acme/app/api", &d)
        );
        assert_eq!(
            ImageRef::parse("localhost:5000/x").unwrap(),
            r("localhost:5000", "x", "latest")
        );
        for bad in [
            "",
            "a b",
            "UPPER",
            "x@sha256:abc",
            "x:bad/tag",
            "ghcr.io//x",
            "x:t@g",
        ] {
            assert!(ImageRef::parse(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn challenges() {
        let (s, p) = parse_challenge(
            r#"Bearer realm="https://auth.docker.io/token",service="registry.docker.io",scope="repository:library/alpine:pull""#,
        );
        assert_eq!(s, "Bearer");
        assert_eq!(
            p,
            vec![
                ("realm".into(), "https://auth.docker.io/token".into()),
                ("service".into(), "registry.docker.io".into()),
                ("scope".into(), "repository:library/alpine:pull".into()),
            ]
        );
        let (s, p) = parse_challenge(r#"Basic realm="Pyo registry""#);
        assert_eq!(s, "Basic");
        assert_eq!(p, vec![("realm".into(), "Pyo registry".into())]);
        assert_eq!(encode("repository:a/b:pull"), "repository%3Aa%2Fb%3Apull");
    }
}
