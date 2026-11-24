//! A small HTTPS client for Pyo's channel: one connection per request,
//! the public web's roots, no proxy. Plain HTTP only to this machine
//! (tests and a Pyo on the same host).

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, bail};
use http_body_util::{BodyExt, Full};
use hyper::body::Bytes;
use hyper::header::{self, HeaderValue};
use hyper::{Method, Request, StatusCode, Uri};
use hyper_util::rt::TokioIo;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// The most an answer may weigh (a poll's commands).
const MAX_BODY: usize = 64 << 20;
const USER_AGENT: &str = concat!("pyo-sandbox/", env!("CARGO_PKG_VERSION"));

/// Where Pyo is.
#[derive(Clone)]
pub struct Http {
    https: bool,
    host: String,
    port: u16,
    authority: String,
    /// The URL's own path, before the channel's prefix.
    base_path: String,
    tls: tokio_rustls::TlsConnector,
}

/// Whether `host` is this machine.
fn loopback(host: &str) -> bool {
    host == "localhost"
        || host
            .trim_start_matches('[')
            .trim_end_matches(']')
            .parse::<std::net::IpAddr>()
            .is_ok_and(|a| a.is_loopback())
}

impl Http {
    /// For Pyo at `url` (`https://…`, or `http://` to this machine).
    pub fn new(url: &str) -> anyhow::Result<Http> {
        let uri: Uri = url
            .trim()
            .trim_end_matches('/')
            .parse()
            .with_context(|| format!("{url} isn't a URL"))?;
        let host = uri.host().context("The Pyo URL has no host.")?.to_owned();
        let https = match uri.scheme_str() {
            Some("https") => true,
            Some("http") if loopback(&host) => false,
            Some("http") => bail!("Pyo's URL must be HTTPS."),
            _ => bail!("{url} isn't an HTTPS URL."),
        };
        let port = uri.port_u16().unwrap_or(if https { 443 } else { 80 });
        let authority = uri.authority().context("no host")?.as_str().to_owned();
        let mut roots = tokio_rustls::rustls::RootCertStore::empty();
        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        let provider = Arc::new(tokio_rustls::rustls::crypto::ring::default_provider());
        let cfg = tokio_rustls::rustls::ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .expect("ring has the default protocol versions")
            .with_root_certificates(roots)
            .with_no_client_auth();
        Ok(Http {
            https,
            host: host
                .trim_start_matches('[')
                .trim_end_matches(']')
                .to_owned(),
            port,
            authority,
            base_path: uri.path().trim_end_matches('/').to_owned(),
            tls: tokio_rustls::TlsConnector::from(Arc::new(cfg)),
        })
    }

    /// The full path for `path` under the channel's prefix, as signed.
    pub fn path(&self, path: &str) -> String {
        format!(
            "{}{}{path}",
            self.base_path,
            pyo_proto::sandbox::outbound::API_PREFIX
        )
    }

    /// One request: its status and body. `path` is the full path with
    /// its query ([`Http::path`]).
    pub async fn send(
        &self,
        method: Method,
        path: &str,
        headers: &[(&str, String)],
        body: Vec<u8>,
        timeout: Duration,
    ) -> anyhow::Result<(StatusCode, Vec<u8>)> {
        let mut req = Request::builder()
            .method(method)
            .uri(path)
            .header(header::HOST, &self.authority)
            .header(header::USER_AGENT, USER_AGENT);
        if !body.is_empty() {
            req = req.header(header::CONTENT_TYPE, "application/json");
        }
        for (k, v) in headers {
            req = req.header(*k, HeaderValue::from_str(v)?);
        }
        let req = req.body(Full::new(Bytes::from(body)))?;
        tokio::time::timeout(timeout, self.send_now(req))
            .await
            .context("Pyo didn't answer in time.")?
    }

    async fn send_now(&self, req: Request<Full<Bytes>>) -> anyhow::Result<(StatusCode, Vec<u8>)> {
        let tcp = tokio::time::timeout(
            CONNECT_TIMEOUT,
            tokio::net::TcpStream::connect((self.host.as_str(), self.port)),
        )
        .await
        .with_context(|| format!("connecting to {} timed out", self.authority))?
        .with_context(|| format!("connecting to {}", self.authority))?;
        let _ = tcp.set_nodelay(true);
        if self.https {
            let name = rustls_pki_types::ServerName::try_from(self.host.clone())
                .with_context(|| format!("{} isn't a host name", self.host))?;
            let tls = tokio::time::timeout(CONNECT_TIMEOUT, self.tls.connect(name, tcp))
                .await
                .with_context(|| format!("TLS with {} timed out", self.authority))?
                .with_context(|| format!("TLS with {}", self.authority))?;
            send_on(tls, req).await
        } else {
            send_on(tcp, req).await
        }
    }
}

async fn send_on<T>(io: T, req: Request<Full<Bytes>>) -> anyhow::Result<(StatusCode, Vec<u8>)>
where
    T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(io)).await?;
    tokio::spawn(async move {
        let _ = conn.await;
    });
    let res = sender.send_request(req).await?;
    let status = res.status();
    let mut body = res.into_body();
    let mut out = Vec::new();
    while let Some(frame) = body.frame().await {
        if let Ok(data) = frame?.into_data() {
            out.extend_from_slice(&data);
            if out.len() > MAX_BODY {
                bail!("Pyo's answer is too big.");
            }
        }
    }
    Ok((status, out))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn https_or_this_machine() {
        let h = Http::new("https://agent.example.com/").unwrap();
        assert_eq!(
            h.path("/commands?wait=25"),
            "/api/sandbox-agent/v1/commands?wait=25"
        );
        assert!(Http::new("http://127.0.0.1:8080").is_ok());
        assert!(Http::new("http://localhost:8080").is_ok());
        assert!(Http::new("http://agent.example.com").is_err());
        assert!(Http::new("ftp://agent.example.com").is_err());
        let p = Http::new("https://example.com/pyo").unwrap();
        assert_eq!(p.path("/report"), "/pyo/api/sandbox-agent/v1/report");
    }
}
