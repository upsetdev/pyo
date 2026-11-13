//! Pyo agent: runs inside a k3s cluster and dials out to the Pyo API.
//!
//! - every few seconds it reports servers, workloads and service health
//! - it long-polls for commands (deploy, restart, stop, delete, cordon, logs)
//! - during a rollout it streams stages, Kubernetes events and failures back
//!
//! It never listens on a port and never sends cluster credentials anywhere.

mod actions;
mod api;
mod backup;
mod builder;
mod bundles;
mod cron;
mod databases;
mod dbimport;
mod dbmetrics;
mod dns;
mod drain;
mod firewall;
mod gateway;
mod gpu;
mod install;
mod kubelet;
mod locate;
mod observability;
mod observe;
mod rollout;
mod runs;
mod scope;
mod terminal;
mod tls;
mod upgrade;
mod volumes;
mod waf;

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::Duration,
};

use pyo_proto::{Command, CommandEnvelope, CommandResult};
use tokio::sync::watch;
use tracing_subscriber::EnvFilter;

pub const AGENT_VERSION: &str = env!("CARGO_PKG_VERSION");
/// Field manager for server-side apply; also the managed-by label value.
pub const MANAGER: &str = "pyo";

/// Rollouts being watched, so a Cancel command can stop them.
pub type Cancels = Arc<Mutex<HashMap<String, watch::Sender<bool>>>>;

/// Services with a rollout under way (service id → how many), so the
/// gateway's garbage collection leaves their objects alone while their
/// workload is still to come.
#[derive(Clone, Default)]
pub struct Deploying(Arc<Mutex<HashMap<String, usize>>>);

impl Deploying {
    /// Marks the service as deploying until the guard drops.
    pub fn start(&self, service_id: &str) -> DeployingGuard {
        *self
            .0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(service_id.to_owned())
            .or_default() += 1;
        DeployingGuard {
            set: self.clone(),
            service_id: service_id.to_owned(),
        }
    }

    pub fn contains(&self, service_id: &str) -> bool {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains_key(service_id)
    }
}

pub struct DeployingGuard {
    set: Deploying,
    service_id: String,
}

impl Drop for DeployingGuard {
    fn drop(&mut self) {
        let mut m = self.set.0.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(n) = m.get_mut(&self.service_id) {
            *n -= 1;
            if *n == 0 {
                m.remove(&self.service_id);
            }
        }
    }
}

#[derive(Clone)]
pub struct Agent {
    pub kube: kube::Client,
    pub api: api::PyoApi,
    pub cancels: Cancels,
    pub deploying: Deploying,
    /// The newest domains seen per service (gateway.rs).
    pub routes: gateway::RouteState,
    /// The customer's switches, as last read (each heartbeat).
    pub policy: scope::SharedPolicy,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Two TLS backends are compiled in (kube's and reqwest's); pick one for
    // everything that doesn't say, or building a TLS client panics.
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    let args: Vec<String> = std::env::args().collect();
    // `pyo-agent version`: what this image is (an update checks it runs).
    if args.get(1).map(String::as_str) == Some("version") {
        println!("{AGENT_VERSION}");
        return Ok(());
    }
    // `pyo-agent check-release <release.json>`: what the upgrader would
    // refuse in a release (the release workflow runs it before publishing).
    if args.get(1).map(String::as_str) == Some("check-release") {
        let path = args
            .get(2)
            .ok_or_else(|| anyhow::anyhow!("usage: pyo-agent check-release <release.json>"))?;
        return upgrade::check_release(path);
    }
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| "pyo_agent=info".into()),
        )
        .init();
    // `pyo-agent upgrade <release.json>`: the upgrader Job (upgrade.rs).
    if args.get(1).map(String::as_str) == Some("upgrade") {
        let path = args
            .get(2)
            .ok_or_else(|| anyhow::anyhow!("usage: pyo-agent upgrade <release.json>"))?;
        return upgrade::run_job(path).await;
    }

    let url =
        std::env::var("PYO_API_URL").map_err(|_| anyhow::anyhow!("PYO_API_URL is required"))?;
    let token = std::env::var("PYO_AGENT_TOKEN")
        .map_err(|_| anyhow::anyhow!("PYO_AGENT_TOKEN is required"))?;
    let kube = kube::Client::try_default().await?;
    let policy = scope::read_policy(&kube).await;
    let agent = Agent {
        kube,
        api: api::PyoApi::new(&url, &token)?,
        cancels: Default::default(),
        deploying: Default::default(),
        routes: Default::default(),
        policy: Arc::new(std::sync::RwLock::new(policy.clone())),
    };
    tracing::info!(version = AGENT_VERSION, api = %url, read_only = policy.read_only, "pyo agent starting");

    // Read only: nothing of Pyo's own is installed or changed either.
    if !policy.read_only {
        tokio::spawn(tls::ensure(agent.kube.clone()));
        tokio::spawn(gateway::ensure(
            agent.kube.clone(),
            agent.api.same_cluster(),
        ));
        tokio::spawn(locate::run(agent.kube.clone()));
        tokio::spawn(gpu::run(agent.kube.clone()));
        tokio::spawn(gateway::collect_loop(agent.clone()));
    }
    // Reads only: runs in read-only clusters too.
    tokio::spawn(dbmetrics::run(agent.kube.clone()));
    tokio::spawn(heartbeat_loop(agent.clone()));
    command_loop(agent).await;
    Ok(())
}

async fn heartbeat_loop(agent: Agent) {
    let mut tick = tokio::time::interval(Duration::from_secs(10));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // Observability as last applied here (None: not yet since this start).
    let mut observability_applied: Option<bool> = None;
    loop {
        tick.tick().await;
        let policy = scope::read_policy(&agent.kube).await;
        *agent.policy.write().unwrap_or_else(|e| e.into_inner()) = policy.clone();
        match observe::heartbeat(&agent.kube).await {
            Ok(mut hb) => {
                hb.host_proof = agent.api.host_proof(&agent.kube).await;
                hb.scope = Some(scope::detect(&agent.kube, policy.clone()).await);
                match agent.api.heartbeat(&hb).await {
                    Ok(_) if policy.read_only => {}
                    Ok(reply) => {
                        observe::label_locations(&agent.kube, &reply.server_locations).await;
                        observe::label_tags(&agent.kube, &reply.server_tags).await;
                        gpu::apply_policies(&agent.kube, &reply.gpu_policies).await;
                        if let Some(on) = reply.public_traffic {
                            gateway::set_public_traffic(&agent.kube, on).await;
                            if let Err(e) = rollout::set_public_ports(&agent.kube, on).await {
                                tracing::warn!(error = %format!("{e:#}"), "couldn't change public ports");
                            }
                        }
                        if let Some(address) = &reply.free_domain_address {
                            gateway::set_tailnet_web(&agent.kube, address == "tailscale").await;
                        }
                        databases::label_teams(&agent.kube, &reply.project_teams).await;
                        if let Some(on) = reply.observability
                            && observability_applied != Some(on)
                        {
                            match observability::set(&agent.kube, on, agent.api.same_cluster())
                                .await
                            {
                                Ok(()) => {
                                    tracing::info!(on, "observability applied");
                                    observability_applied = Some(on);
                                }
                                Err(e) => {
                                    tracing::warn!(error = %format!("{e:#}"), on, "couldn't apply observability; retrying")
                                }
                            }
                        }
                    }
                    Err(e) => tracing::warn!(error = %e, "heartbeat not accepted"),
                }
            }
            Err(e) => tracing::warn!(error = %e, "couldn't read cluster state"),
        }
        // The cluster's own upkeep, with or without the API.
        if !policy.read_only {
            dns::ensure(&agent.kube).await;
            databases::isolate_all(&agent.kube).await;
        }
    }
}

async fn command_loop(agent: Agent) {
    let mut backoff = Duration::from_secs(1);
    let mut order = Ordered::default();
    loop {
        match agent.api.poll(25).await {
            Ok(commands) => {
                backoff = Duration::from_secs(1);
                for c in commands {
                    let after = order.next(&c.command);
                    let agent = agent.clone();
                    tokio::spawn(async move {
                        // The one before it on the same object first; the
                        // next one waits until this one's `done` drops.
                        let _done = match after {
                            Some((previous, done)) => {
                                previous.await;
                                Some(done)
                            }
                            None => None,
                        };
                        run_command(agent, c).await;
                    });
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, retry_in = ?backoff, "couldn't reach the Pyo API");
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(Duration::from_secs(30));
            }
        }
    }
}

/// The objects a command changes, when commands on them must run in the
/// order they were queued: a database's apply sent before its delete would
/// otherwise finish after it and bring the database back, and a firewall
/// sync (a snapshot of the services' routes) running alongside a service's
/// delete would put the deleted routes back, and a bundle's install running
/// alongside its removal would write into a namespace being deleted. Firewall
/// syncs also take turns with the WAF's bundle: the API queues its install
/// before the sync that points routes at it, and its removal after the sync
/// that takes them away.
fn ordered_keys(c: &Command) -> Vec<String> {
    let mut keys = match c {
        Command::ApplyDatabase(spec) => vec![format!("database/{}/{}", spec.namespace, spec.name)],
        Command::DeleteDatabase { namespace, name } => vec![format!("database/{namespace}/{name}")],
        // A deleted project's services, then its volumes, then its
        // namespace (which is kept while it has any).
        Command::DeleteService(s) => vec![
            format!("namespace/{}", s.namespace),
            format!("service/{}", s.service_id),
        ],
        Command::DeleteVolume { namespace, .. } | Command::DeleteNamespace { namespace, .. } => {
            vec![format!("namespace/{namespace}")]
        }
        // A volume moved out of a deleted project's namespace comes before
        // that namespace's deletion; and one thing at a time per volume.
        Command::MoveVolume(m) => vec![
            format!("namespace/{}", m.from_namespace),
            format!("namespace/{}", m.to_namespace),
            format!("volume/{}", m.volume_id),
        ],
        Command::RestoreVolume(r) => vec![format!("volume/{}", r.volume_id)],
        Command::ForgetSnapshot(f) => vec![format!("volume/{}", f.volume_id)],
        Command::ResizeVolume { volume_id, .. } => vec![format!("volume/{volume_id}")],
        Command::SyncFirewall(sync) => sync
            .targets
            .iter()
            .map(|t| format!("service/{}", t.service.service_id))
            .chain([format!("bundle/{}", pyo_proto::bundles::WAF_ID)])
            .collect(),
        // Like a firewall sync, for one service: after its delete it would
        // put its routes back, and its WAF policy needs the WAF's bundle.
        Command::SyncRoutes(t) => vec![
            format!("service/{}", t.service.service_id),
            format!("bundle/{}", pyo_proto::bundles::WAF_ID),
        ],
        // A bundle turned off and on again: its install waits for the
        // removal (which ends once its namespace is gone).
        Command::ApplyBundle { bundle, .. } => vec![format!("bundle/{}", bundle.id)],
        Command::RemoveBundle { id, .. } => vec![format!("bundle/{id}")],
        _ => vec![],
    };
    keys.sort();
    keys.dedup();
    keys
}

type Done = futures::future::Shared<futures::future::BoxFuture<'static, ()>>;

/// Commands run side by side, except those on the same object
/// ([`ordered_keys`]): each of those waits for the ones before it.
#[derive(Default)]
struct Ordered {
    /// Per object, a future that resolves when its last command is done.
    last: HashMap<String, Done>,
}

impl Ordered {
    /// For a command on ordered objects: what to wait for first (the end of
    /// the previous command on each of them) and the sender to drop once
    /// done.
    fn next(
        &mut self,
        c: &Command,
    ) -> Option<(
        futures::future::BoxFuture<'static, ()>,
        tokio::sync::oneshot::Sender<()>,
    )> {
        use futures::FutureExt;
        // Forget objects whose last command is done.
        self.last
            .retain(|_, done| done.clone().now_or_never().is_none());
        let keys = ordered_keys(c);
        if keys.is_empty() {
            return None;
        }
        let (done, rx) = tokio::sync::oneshot::channel::<()>();
        let mine: Done = rx.map(|_| ()).boxed().shared();
        let previous: Vec<Done> = keys
            .into_iter()
            .filter_map(|k| self.last.insert(k, mine.clone()))
            .collect();
        let wait = async move {
            futures::future::join_all(previous).await;
        };
        Some((Box::pin(wait), done))
    }
}

async fn run_command(agent: Agent, envelope: CommandEnvelope) {
    let CommandEnvelope { id, command } = envelope;
    tracing::info!(%id, kind = command_kind(&command), "command");
    // The customer's policy first: refused commands do nothing.
    let refused = scope::refusal(
        &agent.policy.read().unwrap_or_else(|e| e.into_inner()),
        &command,
    );
    if let Some(why) = refused {
        tracing::info!(%id, kind = command_kind(&command), "refused by the cluster's policy");
        // A deployment shows why it didn't happen.
        if let Command::Deploy(spec) = &command {
            let p = pyo_proto::DeploymentProgress {
                status: "failed".into(),
                failure: Some(pyo_proto::Failure {
                    stage: "applying".into(),
                    reason: "Policy".into(),
                    message: why.into(),
                }),
                ..Default::default()
            };
            let _ = agent.api.progress(&spec.deployment_id, &p).await;
        }
        if let Err(e) = agent.api.command_result(&id, &failed(why)).await {
            tracing::warn!(%id, error = %e, "couldn't report command result");
        }
        return;
    }
    let result = match command {
        Command::Deploy(spec) => rollout::run(&agent, *spec).await,
        Command::Cancel { deployment_id } => {
            let sender = agent.cancels.lock().unwrap().remove(&deployment_id);
            match sender {
                Some(tx) => {
                    let _ = tx.send(true);
                    ok("Cancelling the rollout.")
                }
                None => {
                    // Not watching it (finished, or never started here): settle it now.
                    let p = pyo_proto::DeploymentProgress {
                        status: "cancelled".into(),
                        ..Default::default()
                    };
                    let _ = agent.api.progress(&deployment_id, &p).await;
                    ok("Cancelled.")
                }
            }
        }
        // The shell runs in its own task for as long as it's open.
        Command::OpenTerminal {
            session_id,
            service,
            pod,
            shell,
        } => {
            tokio::spawn(terminal::run(
                agent.clone(),
                session_id,
                service,
                pod,
                shell,
            ));
            ok("Opening a terminal.")
        }
        Command::Backup(spec) => backup::run(&agent, *spec).await,
        Command::ListSnapshots(spec) => backup::snapshots(&agent.kube, *spec).await,
        Command::ForgetSnapshot(spec) => backup::forget(&agent.kube, *spec).await,
        Command::RestoreVolume(spec) => volumes::restore(&agent.kube, *spec).await,
        Command::DbSetup {
            namespace,
            admin,
            password,
        } => backup::setup(&agent, &namespace, &admin, &password).await,
        Command::UpgradePyo(spec) => upgrade::start(&agent, *spec).await,
        // Answered before switching: the switch stops this process.
        Command::UpdateAgent { image } => {
            let result = match upgrade::check_agent_image(&agent.kube, &image).await {
                Ok(version) => CommandResult {
                    data: Some(serde_json::json!({ "version": version })),
                    ..ok(&format!("Restarting on agent {version}."))
                },
                Err(e) => failed(format!("{e:#}")),
            };
            let switch = result.ok;
            if let Err(e) = agent.api.command_result(&id, &result).await {
                tracing::warn!(%id, error = %e, "couldn't report command result; not switching");
                return;
            }
            if switch {
                tracing::info!(%image, "switching to a new agent image");
                if let Err(e) = upgrade::switch(&agent.kube, &image).await {
                    tracing::error!(error = %e, "couldn't switch to the new agent image");
                }
            }
            return;
        }
        // The same: the Deployment's pins change last, after reporting.
        Command::RefreshInstall(parts) => {
            let result = match install::refresh(&agent.kube, &parts).await {
                Ok(()) => ok(&format!(
                    "This cluster's install is now Pyo's ({}).",
                    parts.hash
                )),
                Err(e) => failed(format!("{e:#}")),
            };
            let finish = result.ok;
            if let Err(e) = agent.api.command_result(&id, &result).await {
                tracing::warn!(%id, error = %e, "couldn't report command result; leaving the Deployment as it is");
                return;
            }
            if finish {
                tracing::info!(install = %parts.hash, "refreshed the admission policy; updating this agent's image pins");
                if let Err(e) = install::finish(&agent.kube, &parts).await {
                    tracing::error!(error = %format!("{e:#}"), "couldn't update this agent's image pins");
                }
            }
            return;
        }
        other => actions::run(&agent, other).await,
    };
    if let Err(e) = agent.api.command_result(&id, &result).await {
        tracing::warn!(%id, error = %e, "couldn't report command result");
    }
}

pub fn ok(message: &str) -> CommandResult {
    CommandResult {
        ok: true,
        message: Some(message.into()),
        logs: vec![],
        data: None,
    }
}

pub fn failed(message: impl Into<String>) -> CommandResult {
    CommandResult {
        ok: false,
        message: Some(message.into()),
        logs: vec![],
        data: None,
    }
}

fn command_kind(c: &Command) -> &'static str {
    match c {
        Command::Deploy(_) => "deploy",
        Command::Restart(_) => "restart",
        Command::Stop(_) => "stop",
        Command::Scale { .. } => "scale",
        Command::Autoscale { .. } => "autoscale",
        Command::SyncCronJobs { .. } => "sync_cron_jobs",
        Command::RunCronJob { .. } => "run_cron_job",
        Command::DeleteService(_) => "delete_service",
        Command::SetSchedulable { .. } => "set_schedulable",
        Command::SetMaintenance { .. } => "set_maintenance",
        Command::FetchLogs { .. } => "fetch_logs",
        Command::Cancel { .. } => "cancel",
        Command::EnsureBuilder(_) => "ensure_builder",
        Command::RemoveBuilder { .. } => "remove_builder",
        Command::OpenTerminal { .. } => "open_terminal",
        Command::CreateJoinToken { .. } => "create_join_token",
        Command::RemoveServer { .. } => "remove_server",
        Command::SetIsolation { .. } => "set_isolation",
        Command::DeleteNamespace { .. } => "delete_namespace",
        Command::SetOriginPull { .. } => "set_origin_pull",
        Command::Backup(_) => "backup",
        Command::DbSetup { .. } => "db_setup",
        Command::UpgradePyo(_) => "upgrade_pyo",
        Command::UpdateAgent { .. } => "update_agent",
        Command::RefreshInstall(_) => "refresh_install",
        Command::SetTrafficMode { .. } => "set_traffic_mode",
        Command::SetWebProxyProtocol { .. } => "set_web_proxy_protocol",
        Command::SyncFirewall(_) => "sync_firewall",
        Command::SyncRoutes(_) => "sync_routes",
        Command::ApplyBundle { .. } => "apply_bundle",
        Command::RemoveBundle { .. } => "remove_bundle",
        Command::ApplyDatabase(_) => "apply_database",
        Command::DeleteDatabase { .. } => "delete_database",
        Command::DeleteVolume { .. } => "delete_volume",
        Command::FetchDatabaseLogs { .. } => "fetch_database_logs",
        Command::CheckDatabaseImport(_) => "check_database_import",
        Command::ListSnapshots(_) => "list_snapshots",
        Command::ForgetSnapshot(_) => "forget_snapshot",
        Command::RestoreVolume(_) => "restore_volume",
        Command::ResizeVolume { .. } => "resize_volume",
        Command::MoveVolume(_) => "move_volume",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn apply(name: &str) -> Command {
        Command::ApplyDatabase(Box::new(pyo_proto::DatabaseSpec {
            namespace: "pyo-db-t".into(),
            name: name.into(),
            database: serde_json::json!({}),
            secret_name: format!("{name}-credentials"),
            secret_type: "Opaque".into(),
            env: vec![],
            secrets: vec![],
            remove_secrets: vec![],
            team_id: String::new(),
        }))
    }

    fn delete(name: &str) -> Command {
        Command::DeleteDatabase {
            namespace: "pyo-db-t".into(),
            name: name.into(),
        }
    }

    fn service(id: &str, ns: &str) -> pyo_proto::ServiceRef {
        pyo_proto::ServiceRef {
            service_id: id.into(),
            name: format!("web-{id}"),
            namespace: ns.into(),
            workload_kind: "Deployment".into(),
        }
    }

    fn firewall_sync(ids: &[&str]) -> Command {
        let targets = ids
            .iter()
            .map(|id| pyo_proto::FirewallTarget {
                service: service(id, "p"),
                project_id: "prj_1".into(),
                ports: vec![],
                domains: vec![],
                routes_revision: 0,
                network_isolation: false,
                firewall: Default::default(),
                waf: None,
            })
            .collect();
        Command::SyncFirewall(Box::new(pyo_proto::FirewallSync {
            targets,
            waf: false,
        }))
    }

    #[test]
    fn database_namespace_service_and_bundle_commands_are_ordered() {
        assert_eq!(ordered_keys(&apply("a")), ordered_keys(&delete("a")));
        assert_ne!(ordered_keys(&apply("a")), ordered_keys(&delete("b")));
        assert!(
            ordered_keys(&Command::Cancel {
                deployment_id: "d".into()
            })
            .is_empty()
        );
        // A project's volumes, then its namespace.
        let volume = Command::DeleteVolume {
            namespace: "p".into(),
            name: "web-data".into(),
            volume_id: "vol_1".into(),
        };
        assert_eq!(
            ordered_keys(&volume),
            ordered_keys(&Command::DeleteNamespace {
                namespace: "p".into(),
                project_id: "prj_1".into()
            })
        );
        // A deleted project's services before them; and a service's delete
        // in line with the firewall syncs that touch it.
        let delete_service = Command::DeleteService(service("svc_1", "p"));
        assert_eq!(
            ordered_keys(&delete_service),
            ["namespace/p", "service/svc_1"]
        );
        assert_eq!(
            ordered_keys(&firewall_sync(&["svc_2", "svc_1", "svc_2"])),
            ["bundle/waf", "service/svc_1", "service/svc_2"]
        );
        // A service's routes sync: in line with its delete and the WAF's bundle.
        let Command::SyncFirewall(one) = firewall_sync(&["svc_1"]) else {
            unreachable!()
        };
        let routes = Command::SyncRoutes(Box::new(one.targets[0].clone()));
        assert_eq!(ordered_keys(&routes), ["bundle/waf", "service/svc_1"]);
        assert_eq!(ordered_keys(&firewall_sync(&[])), ["bundle/waf"]);
    }

    /// The e2e orphans: a firewall sync queued with a service's delete
    /// never runs alongside it; one queued after it waits for it (and then
    /// skips the service, whose workload is gone). Services it doesn't
    /// touch don't wait.
    #[tokio::test]
    async fn a_service_delete_and_firewall_syncs_take_turns() {
        let mut order = Ordered::default();
        let running = Arc::new(AtomicUsize::new(0));
        let overlap = Arc::new(AtomicUsize::new(0));
        let seen = Arc::new(Mutex::new(vec![]));
        let mut tasks = vec![];
        let cmds = [
            ("sync-before", firewall_sync(&["svc_1", "svc_2"]), 150u64),
            ("delete", Command::DeleteService(service("svc_1", "p")), 100),
            ("sync-after", firewall_sync(&["svc_1", "svc_2"]), 0),
            ("other", Command::DeleteService(service("svc_3", "q")), 0),
        ];
        for (label, c, slow) in cmds {
            let after = order.next(&c).expect("ordered");
            let (running, overlap, seen) = (running.clone(), overlap.clone(), seen.clone());
            tasks.push(tokio::spawn(async move {
                let (previous, _done) = after;
                previous.await;
                let shared = label != "other";
                if shared && running.fetch_add(1, Ordering::SeqCst) > 0 {
                    overlap.fetch_add(1, Ordering::SeqCst);
                }
                seen.lock().unwrap().push(label);
                tokio::time::sleep(Duration::from_millis(slow)).await;
                if shared {
                    running.fetch_sub(1, Ordering::SeqCst);
                }
            }));
        }
        for t in tasks {
            t.await.unwrap();
        }
        let seen = seen.lock().unwrap().clone();
        assert_eq!(overlap.load(Ordering::SeqCst), 0, "{seen:?}");
        let at = |l: &str| seen.iter().position(|x| *x == l).unwrap();
        assert!(
            at("sync-before") < at("delete") && at("delete") < at("sync-after"),
            "{seen:?}"
        );
        assert!(at("other") < at("delete"), "{seen:?}");
    }

    /// The first service's WAF on: the engine's install, then the sync that
    /// points routes at it. The last one off: the sync that takes them away,
    /// then the removal. Each in the order the API queued them.
    #[tokio::test]
    async fn the_waf_engine_and_firewall_syncs_take_turns() {
        let waf = pyo_proto::bundles::waf_bundle("waf:1", "geoip:1");
        let cmds = [
            (
                "install",
                Command::ApplyBundle {
                    bundle: Box::new(waf.clone()),
                    prune: vec![],
                },
                120u64,
            ),
            ("sync-on", firewall_sync(&["svc_1"]), 60),
            ("sync-off", firewall_sync(&["svc_2"]), 60),
            (
                "remove",
                Command::RemoveBundle {
                    id: waf.id.clone(),
                    objects: waf.refs(),
                },
                0,
            ),
        ];
        let mut order = Ordered::default();
        let seen = Arc::new(Mutex::new(vec![]));
        let mut tasks = vec![];
        for (label, c, slow) in cmds {
            let (previous, done) = order.next(&c).expect("ordered");
            let seen = seen.clone();
            tasks.push(tokio::spawn(async move {
                let _done = done;
                previous.await;
                seen.lock().unwrap().push(label);
                tokio::time::sleep(Duration::from_millis(slow)).await;
            }));
        }
        for t in tasks {
            t.await.unwrap();
        }
        assert_eq!(
            *seen.lock().unwrap(),
            ["install", "sync-on", "sync-off", "remove"]
        );
    }

    /// The WAF turned off and on again: the install waits for the removal;
    /// the databases bundle doesn't wait for either.
    #[tokio::test]
    async fn a_bundles_install_waits_for_its_removal() {
        let waf = pyo_proto::bundles::waf_bundle("waf:1", "geoip:1");
        let remove = Command::RemoveBundle {
            id: waf.id.clone(),
            objects: waf.refs(),
        };
        let install = Command::ApplyBundle {
            bundle: Box::new(waf),
            prune: vec![],
        };
        let databases = Command::ApplyBundle {
            bundle: Box::new(pyo_proto::bundles::databases_bundle(
                &pyo_proto::system::db_images_from_env(),
            )),
            prune: vec![],
        };
        assert_eq!(ordered_keys(&remove), ["bundle/waf"]);
        assert_eq!(ordered_keys(&remove), ordered_keys(&install));
        assert_eq!(ordered_keys(&databases), ["bundle/databases"]);
        let mut order = Ordered::default();
        let seen = Arc::new(Mutex::new(vec![]));
        let mut tasks = vec![];
        for (label, c, slow) in [
            ("remove", remove, 150u64),
            ("install", install, 0),
            ("databases", databases, 0),
        ] {
            let (previous, done) = order.next(&c).expect("ordered");
            let seen = seen.clone();
            tasks.push(tokio::spawn(async move {
                let _done = done;
                previous.await;
                seen.lock().unwrap().push(format!("{label}:start"));
                tokio::time::sleep(Duration::from_millis(slow)).await;
                seen.lock().unwrap().push(format!("{label}:end"));
            }));
        }
        for t in tasks {
            t.await.unwrap();
        }
        let seen = seen.lock().unwrap().clone();
        let at = |l: &str| seen.iter().position(|x| x == l).unwrap();
        assert!(at("remove:end") < at("install:start"), "{seen:?}");
        assert!(at("databases:end") < at("remove:end"), "{seen:?}");
    }

    /// The e2e orphans: an apply (deletion protection off) handed out with
    /// the delete right after it must finish first, however slow it is.
    #[tokio::test]
    async fn a_database_delete_waits_for_its_apply() {
        let mut order = Ordered::default();
        let step = Arc::new(AtomicUsize::new(0));
        let seen = Arc::new(Mutex::new(vec![]));
        let mut tasks = vec![];
        for (c, slow) in [(apply("a"), 200u64), (delete("a"), 0), (apply("b"), 0)] {
            let after = order.next(&c).expect("ordered");
            let (step, seen) = (step.clone(), seen.clone());
            let label = format!("{}:{}", command_kind(&c), ordered_keys(&c)[0]);
            tasks.push(tokio::spawn(async move {
                let (previous, _done) = after;
                previous.await;
                tokio::time::sleep(Duration::from_millis(slow)).await;
                seen.lock()
                    .unwrap()
                    .push((step.fetch_add(1, Ordering::SeqCst), label));
            }));
        }
        assert!(
            order
                .next(&Command::Cancel {
                    deployment_id: "d".into()
                })
                .is_none()
        );
        for t in tasks {
            t.await.unwrap();
        }
        let seen = seen.lock().unwrap().clone();
        let at = |l: &str| seen.iter().find(|(_, x)| x == l).unwrap().0;
        assert!(
            at("apply_database:database/pyo-db-t/a") < at("delete_database:database/pyo-db-t/a"),
            "{seen:?}"
        );
        // Another database doesn't wait.
        assert_eq!(at("apply_database:database/pyo-db-t/b"), 0, "{seen:?}");
    }

    #[tokio::test]
    async fn finished_objects_are_forgotten() {
        let mut order = Ordered::default();
        let (previous, done) = order.next(&apply("a")).unwrap();
        previous.await;
        drop(done);
        // The next one doesn't wait, and the finished entry is gone.
        let (previous, _done) = order.next(&delete("a")).unwrap();
        tokio::time::timeout(Duration::from_millis(100), previous)
            .await
            .expect("no wait");
        assert_eq!(order.last.len(), 1);
    }
}
