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
use pyo_sandbox::gvisor::Gvisor;
use pyo_sandbox::manager::Manager;
use pyo_sandbox::network::{LinkKind, Network};
use pyo_sandbox::storage::Layout;
use pyo_sandbox::{outbound, tailnet};

#[derive(Parser)]
#[command(name = "pyo-sandbox", version, about = "The Pyo Sandbox daemon")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Detect the runtime, adopt running VMs, work for Pyo over its channel
    /// and serve the API on the tailnet.
    Serve(Opts),
    /// Detect the runtime and print the capability report as JSON.
    Detect(Opts),
    /// Join Pyo over HTTPS with the one-time code in the join file, and
    /// write the config `serve` reads.
    Join(JoinOpts),
}

#[derive(clap::Args)]
struct JoinOpts {
    /// Where the config goes.
    #[arg(
        long,
        env = "PYO_SANDBOX_CONFIG",
        default_value = "/etc/pyo-sandbox/config.json"
    )]
    config: PathBuf,
    /// The join file sandbox.sh wrote: Pyo's URL, the server, the code and
    /// the command key. Removed once the server joined.
    #[arg(long, default_value = pyo_sandbox::outbound::join::JOIN_FILE)]
    join: PathBuf,
    /// `pyo-sandbox detect`'s report, sent with the join.
    #[arg(long)]
    capability: Option<PathBuf>,
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
    /// The config's folder: the tailnet certificate is in `tailnet/` there.
    dir: PathBuf,
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
    let dir = o
        .config
        .parent()
        .filter(|d| !d.as_os_str().is_empty())
        .unwrap_or(std::path::Path::new("."))
        .to_owned();
    Ok(Setup {
        cfg,
        dir,
        layout,
        fc,
    })
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
                runsc: &s.cfg.runsc,
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
        Cmd::Join(o) => join(o).await,
    }
}

/// Joins, or ends the process with one plain sentence on stderr: sandbox.sh
/// shows the last line to the person who ran it.
async fn join(o: JoinOpts) -> anyhow::Result<()> {
    if let Err(e) = try_join(&o).await {
        let line = e.to_string().replace('\n', " ");
        eprintln!("{}", line.strip_prefix("Error: ").unwrap_or(&line));
        std::process::exit(1);
    }
    Ok(())
}

async fn try_join(o: &JoinOpts) -> anyhow::Result<()> {
    use pyo_sandbox::outbound::join;
    let capability = match &o.capability {
        Some(p) => Some(
            std::fs::read(p)
                .ok()
                .and_then(|b| serde_json::from_slice(&b).ok())
                .with_context(|| format!("The report {} can't be read.", p.display()))?,
        ),
        None => None,
    };
    join::join(&o.config, &o.join, capability, join::Options::default()).await?;
    Ok(())
}

/// Sandbox networking, when this server can have it, and the check for the
/// report. Taps belong to the jail user, so Firecracker can open them after
/// dropping root. gVisor sandboxes get a veth pair in a namespace instead.
async fn open_network(s: &Setup, runtime: Runtime) -> (Option<Network>, Check) {
    let (uid, gid) = match &s.fc.jailer {
        Some(j) => (j.uid, j.gid),
        // SAFETY: plain getters.
        None => unsafe { (libc::geteuid(), libc::getegid()) },
    };
    let res = match runtime {
        Runtime::Firecracker => Network::open(s.cfg.network.clone(), LinkKind::Tap, uid, gid).await,
        Runtime::Gvisor => Network::open(s.cfg.network.clone(), LinkKind::Veth, uid, gid).await,
        Runtime::None => Err(anyhow::anyhow!("This server can't run sandboxes.")),
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
    // A server that joined at Pyo's tailnet door before 0.52.0: Pyo no
    // longer calls it, and it has no channel until it joins again.
    let Some(outbound) = s.cfg.outbound.clone() else {
        bail!(pyo_sandbox::NEEDS_UPDATE);
    };
    let mut capability = detect::detect(Inputs {
        fc: &s.fc,
        layout: &s.layout,
        envd: &s.cfg.envd,
        runsc: &s.cfg.runsc,
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
    let tailnet_https = s
        .cfg
        .tailnet_https
        .clone()
        .unwrap_or_else(|| tailnet::TailnetHttps::in_dir(&s.dir));
    let root = s.cfg.root.clone();
    // gVisor sandboxes live in the plain sandboxes directory: no jail.
    let gvisor = (capability.runtime == Runtime::Gvisor).then(|| {
        Gvisor::new(
            s.cfg.runsc.clone(),
            s.cfg.root.join("runsc"),
            s.cfg.cgroup_parent.clone(),
        )
    });
    let mut layout = s.layout;
    if gvisor.is_some() {
        layout.jailed = false;
        layout.create().await?;
    }
    let mgr = Arc::new(
        Manager::new(s.cfg, layout, s.fc, capability)
            .with_network(net)
            .with_gvisor(gvisor),
    );
    let adopted = mgr.adopt().await;
    if adopted > 0 {
        tracing::info!(adopted, "took over sandboxes from before the restart");
    }
    mgr.spawn_sweeper();
    let state = Arc::new(AppState {
        mgr: mgr.clone(),
        keys,
        server_id: server_id.clone(),
    });
    if let Some(cfg) = edge_cfg {
        let domain = cfg.domain.clone();
        let edge = Arc::new(Edge::new(state.clone(), cfg)?);
        let bound = edge.start().await?;
        let url = bound.base_url(&domain);
        tracing::info!(https = ?bound.https, http = ?bound.http, url = ?url, "the edge proxy is serving");
        mgr.set_edge_url(url);
    }
    // Pyo never calls this server: it calls Pyo for its work, and serves
    // browsers and SDKs on the tailnet over HTTPS.
    let app = api::router(state.clone());
    let at = tailnet_https.listen.unwrap_or(std::net::SocketAddr::new(
        listen.ip(),
        pyo_proto::sandbox::outbound::TAILNET_PORT,
    ));
    let bound = tailnet::serve(app, at, &outbound.tailnet_name, &tailnet_https).await?;
    tracing::info!(listen = %bound, "pyo-sandbox is serving");
    let client = outbound::Client::load(
        state.clone(),
        outbound::store::Store::new(&root),
        &outbound,
        &server_id,
        outbound::Timing::default(),
    )?;
    client.run().await;
    // Pyo no longer takes its key: its sandboxes keep running, and the
    // tailnet keeps reaching them.
    std::future::pending::<()>().await;
    Ok(())
}
