use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, bail};
use clap::{Parser, Subcommand};
use pyo_proto::sandbox::{Check, Runtime};
use pyo_sandbox::api::{self, AppState};
use pyo_sandbox::config::Config;
use pyo_sandbox::detect::{self, Inputs};
use pyo_sandbox::edge::Edge;
use pyo_sandbox::firecracker::{Firecracker, Jailer};
use pyo_sandbox::manager::Manager;
use pyo_sandbox::network::Network;
use pyo_sandbox::storage::Layout;

#[derive(Parser)]
#[command(name = "pyo-sandbox", version, about = "The Pyo Sandbox daemon")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Detect the runtime, adopt running VMs and serve the API.
    Serve(Opts),
    /// Detect the runtime and print the capability report as JSON.
    Detect(Opts),
}

#[derive(clap::Args)]
struct Opts {
    #[arg(
        long,
        env = "PYO_SANDBOX_CONFIG",
        default_value = "/etc/pyo-sandbox/config.json"
    )]
    config: PathBuf,
    /// Run Firecracker without the jailer as the current user. For trying
    /// it on a laptop, never on a real server.
    #[arg(long)]
    dev: bool,
}

struct Setup {
    cfg: Config,
    layout: Layout,
    fc: Firecracker,
}

async fn setup(o: &Opts) -> anyhow::Result<Setup> {
    let mut cfg = Config::load(&o.config)?;
    cfg.dev |= o.dev;
    let jailed = cfg.use_jailer()?;
    let layout = Layout::new(&cfg.root, jailed);
    layout.create().await?;
    let fc = Firecracker {
        firecracker: cfg.firecracker.clone(),
        kernel: cfg.kernel.clone(),
        jailer: jailed.then(|| Jailer {
            jailer: cfg.jailer.clone().expect("checked by use_jailer"),
            uid: cfg.jail_uid,
            gid: cfg.jail_gid,
            chroot_base: cfg.root.join("jail"),
            cgroup_parent: cfg.cgroup_parent.clone(),
        }),
        cpu_template: cfg.cpu_template.clone(),
        serial: std::env::var_os("PYO_SANDBOX_SERIAL").is_some(),
    };
    Ok(Setup { cfg, layout, fc })
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Detect(o) => {
            let s = setup(&o).await?;
            let mut report = detect::detect(Inputs {
                fc: &s.fc,
                layout: &s.layout,
                envd: &s.cfg.envd,
            })
            .await;
            // Only checks. `serve` puts the table in place, and reports
            // networking once that worked.
            let (_, check) = open_network(&s, report.runtime).await;
            report.network = check.ok;
            report.checks.push(check);
            println!("{}", serde_json::to_string_pretty(&report)?);
            Ok(())
        }
        Cmd::Serve(o) => serve(setup(&o).await?).await,
    }
}

/// Sandbox networking, when this server can have it, and the check for the
/// report. Taps belong to the jail user, so Firecracker can open them after
/// dropping root.
async fn open_network(s: &Setup, runtime: Runtime) -> (Option<Network>, Check) {
    let res = if runtime != Runtime::Firecracker {
        Err(anyhow::anyhow!(
            "Sandbox networking needs Firecracker for now."
        ))
    } else {
        let (uid, gid) = match &s.fc.jailer {
            Some(j) => (j.uid, j.gid),
            // SAFETY: plain getters.
            None => unsafe { (libc::geteuid(), libc::getegid()) },
        };
        Network::open(s.cfg.network.clone(), uid, gid).await
    };
    match res {
        Ok(n) => {
            let detail = format!("pool {}", n.cfg.pool);
            (
                Some(n),
                Check {
                    name: "network".into(),
                    ok: true,
                    detail,
                },
            )
        }
        Err(e) => (
            None,
            Check {
                name: "network".into(),
                ok: false,
                detail: format!("{e:#}"),
            },
        ),
    }
}

async fn serve(s: Setup) -> anyhow::Result<()> {
    if s.cfg.listen.ip().is_unspecified() && !s.cfg.dev {
        bail!("The daemon API must listen on the tailnet address, not on every address.");
    }
    let mut capability = detect::detect(Inputs {
        fc: &s.fc,
        layout: &s.layout,
        envd: &s.cfg.envd,
    })
    .await;
    let (net, check) = open_network(&s, capability.runtime).await;
    if !check.ok {
        tracing::warn!("{}", check.detail);
    }
    capability.checks.push(check);
    tracing::info!(runtime = ?capability.runtime, kvm = ?capability.kvm, boot_ms = ?capability.boot_ms, "{}", capability.summary());
    let keys = s.cfg.keys.clone();
    let server_id = s.cfg.server_id.clone();
    let listen = s.cfg.listen;
    let edge_cfg = s.cfg.edge.clone();
    let mgr = Arc::new(Manager::new(s.cfg, s.layout, s.fc, capability).with_network(net));
    let adopted = mgr.adopt().await;
    if adopted > 0 {
        tracing::info!(adopted, "took over sandboxes from before the restart");
    }
    mgr.spawn_sweeper();
    let state = Arc::new(AppState {
        mgr: mgr.clone(),
        keys,
        server_id,
    });
    if let Some(cfg) = edge_cfg {
        let domain = cfg.domain.clone();
        let edge = Arc::new(Edge::new(state.clone(), cfg)?);
        let bound = edge.start().await?;
        let url = bound.base_url(&domain);
        tracing::info!(https = ?bound.https, http = ?bound.http, url = ?url, "the edge proxy is serving");
        mgr.set_edge_url(url);
    }
    let app = api::router(state);
    let listener = tokio::net::TcpListener::bind(listen)
        .await
        .with_context(|| format!("listening on {listen}"))?;
    tracing::info!(%listen, "pyo-sandbox is serving");
    axum::serve(listener, app).await?;
    Ok(())
}
