//! pyo-static: serves every static site of a cluster (Pyo Static).
//!
//! The gateway sends a static site's requests here with `x-pyo-site:
//! <service>/<deployment>` (pyo-proto's site.rs), set on its routes: which
//! site and which of its folders. So a deploy or rollback is only a route
//! change, and this server keeps no state of its own but its cache.
//!
//! Folders are read through Pyo (`/api/static/v1`, read-only, with the
//! cluster's token in the `pyo-static-login` Secret) and kept in a cache on
//! the pod's disk: the deploy's manifest, and each file by its SHA-256 (a
//! file that didn't change between deploys is fetched once). A site whose
//! folder is cached keeps answering while Pyo or its store can't be
//! reached. Before a deploy moves a site's routes here, the agent asks for
//! its folder to be fetched (`POST /_pyo/warm`), so the first visitor
//! doesn't wait.
//!
//! Without `x-pyo-site`, only `/_pyo/ready` and `/_pyo/warm` answer.

mod cache;
mod serve;

use std::{net::SocketAddr, path::PathBuf, sync::Arc};

use anyhow::Context;
use axum::{Router, body::Body, extract::State, http::Request, response::Response};
use tracing_subscriber::EnvFilter;

const VERSION: &str = env!("CARGO_PKG_VERSION");

struct Args {
    listen: SocketAddr,
    cache: PathBuf,
    login: PathBuf,
}

fn args() -> anyhow::Result<Args> {
    let mut a = Args {
        listen: "0.0.0.0:8080".parse()?,
        cache: "/cache".into(),
        login: "/etc/pyo-static".into(),
    };
    let mut it = std::env::args().skip(1);
    while let Some(k) = it.next() {
        let v = it.next().with_context(|| format!("{k} needs a value"))?;
        match k.as_str() {
            "--listen" => a.listen = v.parse().context("--listen")?,
            "--cache" => a.cache = v.into(),
            "--login" => a.login = v.into(),
            _ => anyhow::bail!("unknown argument {k}"),
        }
    }
    Ok(a)
}

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| "pyo_static=info".into()),
        )
        .with_writer(std::io::stderr)
        .init();
    let a = args()?;
    let cache = Arc::new(cache::Cache::open(a.cache, a.login)?);
    tokio::spawn(cache::Cache::keeper(cache.clone()));
    let app = Router::new().fallback(handle).with_state(cache);
    let listener = tokio::net::TcpListener::bind(a.listen).await?;
    tracing::info!(version = VERSION, listen = %a.listen, "pyo-static starting");
    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            if let Ok(mut term) =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            {
                term.recv().await;
            }
        })
        .await?;
    Ok(())
}

async fn handle(State(cache): State<Arc<cache::Cache>>, req: Request<Body>) -> Response {
    serve::handle(&cache, req).await
}
