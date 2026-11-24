//! HTTPS on the server's tailnet name, for a server that works for Pyo
//! Cloud (outbound.rs): the same API and pages as the plain listener, for
//! browsers and SDKs on the team's tailnet. A dashboard served over HTTPS
//! can't call a plain HTTP address.
//!
//! The certificate is the one `tailscale cert` makes for the server's
//! tailnet name (sandbox.sh puts it in place and renews it). The listener
//! starts without one and serves once it's there, and takes a new one
//! from the next connection on.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use hyper_util::rt::{TokioIo, TokioTimer};
use serde::{Deserialize, Serialize};

use crate::edge::EdgeTls;

/// How often the listener looks for a renewed certificate.
const WATCH: Duration = Duration::from_secs(30);
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(30);

/// The `tailnetHttps` part of the config. Left out, an outbound server
/// listens on its tailnet address at [`pyo_proto::sandbox::outbound::TAILNET_PORT`]
/// with the certificate in `tailnet/` beside its config
/// (`/etc/pyo-sandbox/tailnet`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TailnetHttps {
    #[serde(default)]
    pub listen: Option<SocketAddr>,
    /// PEM certificate chain for the server's tailnet name.
    #[serde(default = "default_cert")]
    pub cert: PathBuf,
    /// Its PEM private key.
    #[serde(default = "default_key")]
    pub key: PathBuf,
}

fn default_cert() -> PathBuf {
    PathBuf::from("/etc/pyo-sandbox/tailnet/cert.pem")
}

fn default_key() -> PathBuf {
    PathBuf::from("/etc/pyo-sandbox/tailnet/key.pem")
}

impl TailnetHttps {
    /// The certificate and key in `<dir>/tailnet`, where sandbox.sh's
    /// `tailscale cert` puts them (`dir`: the config's folder).
    pub fn in_dir(dir: &std::path::Path) -> TailnetHttps {
        TailnetHttps {
            listen: None,
            cert: dir.join("tailnet/cert.pem"),
            key: dir.join("tailnet/key.pem"),
        }
    }
}

impl Default for TailnetHttps {
    fn default() -> Self {
        TailnetHttps {
            listen: None,
            cert: default_cert(),
            key: default_key(),
        }
    }
}

/// Serves `app` over HTTPS on `listen` in the background. Returns the
/// address it listens on.
pub async fn serve(
    app: axum::Router,
    listen: SocketAddr,
    name: &str,
    cfg: &TailnetHttps,
) -> anyhow::Result<SocketAddr> {
    let tls = Arc::new(EdgeTls::named(
        "the tailnet listener",
        name,
        cfg.cert.clone(),
        cfg.key.clone(),
    ));
    let l = tokio::net::TcpListener::bind(listen)
        .await
        .with_context(|| format!("listening on {listen}"))?;
    let bound = l.local_addr()?;
    let watch = Arc::downgrade(&tls);
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(WATCH).await;
            let Some(t) = watch.upgrade() else { return };
            if let Err(e) = t.reload() {
                tracing::debug!(error = %format!("{e:#}"), "tailnet certificate");
            }
        }
    });
    tokio::spawn(async move {
        loop {
            let (tcp, _) = match l.accept().await {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!(error = %e, "tailnet listener: accept failed");
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    continue;
                }
            };
            let _ = tcp.set_nodelay(true);
            let (tls, app) = (tls.clone(), app.clone());
            tokio::spawn(async move {
                let Some(acc) = tls.acceptor() else {
                    return;
                };
                let Ok(Ok(stream)) = tokio::time::timeout(HANDSHAKE_TIMEOUT, acc.accept(tcp)).await
                else {
                    return;
                };
                let svc = hyper_util::service::TowerToHyperService::new(app);
                let _ = hyper::server::conn::http1::Builder::new()
                    .timer(TokioTimer::new())
                    .header_read_timeout(HANDSHAKE_TIMEOUT)
                    .serve_connection(TokioIo::new(stream), svc)
                    .with_upgrades()
                    .await;
            });
        }
    });
    tracing::info!(%bound, %name, "serving HTTPS on the tailnet name");
    Ok(bound)
}
