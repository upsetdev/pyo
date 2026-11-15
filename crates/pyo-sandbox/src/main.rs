use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, bail};
use clap::{Parser, Subcommand};
use pyo_sandbox::api::{self, AppState};
use pyo_sandbox::config::Config;
use pyo_sandbox::detect::{self, Inputs};
use pyo_sandbox::firecracker::{Firecracker, Jailer};
use pyo_sandbox::manager::Manager;
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
            let report = detect::detect(Inputs {
                fc: &s.fc,
                layout: &s.layout,
                envd: &s.cfg.envd,
            })
            .await;
            println!("{}", serde_json::to_string_pretty(&report)?);
            Ok(())
        }
        Cmd::Serve(o) => serve(setup(&o).await?).await,
    }
}

async fn serve(s: Setup) -> anyhow::Result<()> {
    if s.cfg.listen.ip().is_unspecified() && !s.cfg.dev {
        bail!("The daemon API must listen on the tailnet address, not on every address.");
    }
    let capability = detect::detect(Inputs {
        fc: &s.fc,
        layout: &s.layout,
        envd: &s.cfg.envd,
    })
    .await;
    tracing::info!(runtime = ?capability.runtime, kvm = ?capability.kvm, boot_ms = ?capability.boot_ms, "{}", capability.summary());
    let keys = s.cfg.keys.clone();
    let server_id = s.cfg.server_id.clone();
    let listen = s.cfg.listen;
    let mgr = Arc::new(Manager::new(s.cfg, s.layout, s.fc, capability));
    let adopted = mgr.adopt().await;
    if adopted > 0 {
        tracing::info!(adopted, "took over sandboxes from before the restart");
    }
    mgr.spawn_sweeper();
    let app = api::router(Arc::new(AppState {
        mgr,
        keys,
        server_id,
    }));
    let listener = tokio::net::TcpListener::bind(listen)
        .await
        .with_context(|| format!("listening on {listen}"))?;
    tracing::info!(%listen, "pyo-sandbox is serving");
    axum::serve(listener, app).await?;
    Ok(())
}
