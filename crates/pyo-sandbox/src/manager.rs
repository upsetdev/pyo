//! Sandboxes on this server: create, delete, pause, resume, lifetimes and
//! the report Pyo reads. Each runs as a Firecracker VM or, on servers
//! without KVM, under gVisor ([`Instance`]).
//!
//! The daemon enforces lifetimes itself, so sandboxes still end on time
//! while Pyo restarts. Each sandbox's record is written to `sandbox.json`
//! in its VM directory, so a restarted daemon adopts the VMs that still
//! run.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use pyo_proto::sandbox::{
    CapabilityReport, CapacityReport, DaemonError, ErrorCode, ExecRequest, ExecResult, Isolation,
    MAX_TTL_SECS, NetworkMode, OpenPort, Runtime, SANDBOX_PROTOCOL, SandboxSpec, SandboxState,
    SandboxStatus, ServerReport, TemplateReport, TemplateState,
};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use crate::capacity::{self, MemorySource};
use crate::config::Config;
use crate::envd;
use crate::firecracker::{DirHandle, Firecracker, Machine, Nic, SCRATCH, Vm};
use crate::gvisor::{self, Container, Gvisor};
use crate::network::{Lease, Network};
use crate::storage::{self, Layout, snapshot_key};

/// Name of the pause snapshot files in a VM directory.
const PAUSE: &str = "pause";
/// Name of the template snapshot files linked into a VM directory.
const TEMPLATE: &str = "template";
const BOOT_TIMEOUT: Duration = Duration::from_secs(10);
const ENVD_TIMEOUT: Duration = Duration::from_secs(3);

type Result<T> = std::result::Result<T, DaemonError>;

fn err(code: ErrorCode, msg: impl Into<String>) -> DaemonError {
    DaemonError::new(code, msg)
}

fn runtime(e: anyhow::Error) -> DaemonError {
    err(ErrorCode::Runtime, format!("{e:#}"))
}

/// What runs a sandbox.
#[derive(Debug)]
pub enum Instance {
    Vm(Vm),
    Gvisor(Container),
}

impl Instance {
    pub fn pid(&self) -> u32 {
        match self {
            Instance::Vm(v) => v.pid,
            Instance::Gvisor(c) => c.pid,
        }
    }

    pub fn exited(&mut self) -> bool {
        match self {
            Instance::Vm(v) => v.exited(),
            Instance::Gvisor(c) => c.exited(),
        }
    }

    pub async fn kill(&mut self) {
        match self {
            Instance::Vm(v) => v.kill().await,
            Instance::Gvisor(c) => c.kill().await,
        }
    }
}

/// How to reach a sandbox's envd, taken while holding its lock.
enum Dial {
    Vsock(DirHandle, PathBuf),
    Door(Arc<gvisor::Door>),
}

pub struct Sandbox {
    pub spec: SandboxSpec,
    status: StdMutex<SandboxStatus>,
    vm: Mutex<Option<Instance>>,
    /// The tap and addresses of a sandbox with network. Kept while paused.
    pub lease: Option<Lease>,
}

impl Sandbox {
    pub fn status(&self) -> SandboxStatus {
        self.status
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }

    fn update(&self, f: impl FnOnce(&mut SandboxStatus)) -> SandboxStatus {
        let mut s = self.status.lock().unwrap_or_else(|p| p.into_inner());
        f(&mut s);
        s.clone()
    }
}

/// What `sandbox.json` holds.
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Record {
    spec: SandboxSpec,
    status: SandboxStatus,
    pid: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    lease: Option<Lease>,
}

pub struct Manager {
    pub cfg: Config,
    pub layout: Layout,
    pub fc: Firecracker,
    /// runsc, on gVisor servers.
    pub gv: Option<Gvisor>,
    capability: StdMutex<CapabilityReport>,
    sandboxes: StdMutex<HashMap<String, Arc<Sandbox>>>,
    net: Option<Arc<Network>>,
    /// The edge's certificate, when it serves HTTPS.
    edge_tls: StdMutex<Option<Arc<crate::edge::EdgeTls>>>,
    /// Template builds in progress (see `templates`).
    pub(crate) builds: StdMutex<HashMap<String, Arc<crate::templates::Build>>>,
    /// One template build at a time.
    pub(crate) build_gate: tokio::sync::Semaphore,
}

impl Manager {
    pub fn new(
        cfg: Config,
        layout: Layout,
        fc: Firecracker,
        capability: CapabilityReport,
    ) -> Manager {
        Manager {
            cfg,
            layout,
            fc,
            gv: None,
            capability: StdMutex::new(capability),
            sandboxes: StdMutex::new(HashMap::new()),
            net: None,
            edge_tls: StdMutex::new(None),
            builds: StdMutex::new(HashMap::new()),
            build_gate: tokio::sync::Semaphore::new(1),
        }
    }

    /// runsc, which a gVisor server runs its sandboxes with.
    pub fn with_gvisor(mut self, gv: Option<Gvisor>) -> Manager {
        self.gv = gv;
        self
    }

    /// Sandbox networking. `adopt` puts its table in place, and the
    /// capability report says `network: true` once that worked.
    pub fn with_network(mut self, net: Option<Network>) -> Manager {
        self.net = net.map(Arc::new);
        self
    }

    /// The network, when sandboxes here can have one.
    fn network(&self) -> Option<&Arc<Network>> {
        self.net.as_ref().filter(|_| self.capability().network)
    }

    pub(crate) fn has_network(&self) -> bool {
        self.network().is_some()
    }

    /// What the server can run, with the edge's URL and certificate as
    /// they are now.
    pub fn capability(&self) -> CapabilityReport {
        let mut c = self
            .capability
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone();
        if let Some(t) = self.edge_tls() {
            c.edge_url = t.base_url();
            c.edge_cert = t.fingerprint();
        }
        c
    }

    /// The edge's certificate, once an edge with HTTPS listens.
    pub fn set_edge_tls(&self, tls: Arc<crate::edge::EdgeTls>) {
        *self.edge_tls.lock().unwrap_or_else(|p| p.into_inner()) = Some(tls);
    }

    pub fn edge_tls(&self) -> Option<Arc<crate::edge::EdgeTls>> {
        self.edge_tls
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }

    fn all(&self) -> Vec<Arc<Sandbox>> {
        self.sandboxes
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .values()
            .cloned()
            .collect()
    }

    pub fn find(&self, id: &str) -> Result<Arc<Sandbox>> {
        self.sandboxes
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(id)
            .cloned()
            .ok_or_else(|| {
                err(
                    ErrorCode::NotFound,
                    format!("There is no sandbox {id} on this server."),
                )
            })
    }

    fn memory_source(&self, id: &str, inst: &Instance) -> MemorySource {
        match inst {
            Instance::Vm(vm) => {
                MemorySource::for_vm(self.layout.jailed, &self.cfg.cgroup_parent, id, vm.pid)
            }
            // runsc puts the sentry and the gofer in the sandbox's cgroup.
            Instance::Gvisor(c) => {
                MemorySource::for_vm(true, &self.cfg.cgroup_parent, &c.id, c.pid)
            }
        }
    }

    pub fn status(&self, id: &str) -> Result<SandboxStatus> {
        let s = self.find(id)?;
        let mut st = s.status();
        if let Ok(vm) = s.vm.try_lock()
            && let Some(vm) = vm.as_ref()
        {
            st.memory_used = self.memory_source(id, vm).read();
        }
        Ok(st)
    }

    pub fn list(&self) -> Vec<SandboxStatus> {
        let mut v: Vec<SandboxStatus> = self
            .all()
            .iter()
            .filter_map(|s| self.status(&s.spec.id).ok())
            .collect();
        v.sort_by_key(|s| s.created_at);
        v
    }

    pub fn capacity(&self) -> CapacityReport {
        let mem = capacity::meminfo();
        let mut c = CapacityReport {
            memory_total: mem.total,
            memory_available: mem.available,
            memory_reserved: self.cfg.reserve_bytes(mem.total),
            ..Default::default()
        };
        // Disk: templates first, so files a sandbox shares with its
        // template (hard links) count under templates.
        let mut seen = std::collections::HashSet::new();
        c.disk_free = storage::free_bytes(&self.layout.root);
        c.disk_templates =
            crate::templates::disk_bytes(&self.layout.root.join("templates"), &mut seen);
        c.disk_sandboxes = crate::templates::disk_bytes(&self.layout.sandboxes(), &mut seen);
        for s in self.list() {
            match s.state {
                SandboxState::Paused => c.paused += 1,
                st if st.is_final() => {}
                _ => {
                    c.running += 1;
                    c.memory_committed += (s.memory_mib as u64) << 20;
                    c.cpus_committed += s.cpus;
                    c.memory_used += s.memory_used.unwrap_or(0);
                }
            }
        }
        c
    }

    pub fn report(&self) -> ServerReport {
        ServerReport {
            protocol: SANDBOX_PROTOCOL,
            daemon_version: env!("CARGO_PKG_VERSION").into(),
            server_id: self.cfg.server_id.clone(),
            capability: self.capability(),
            capacity: self.capacity(),
            sandboxes: self.list(),
            templates: self.templates(),
        }
    }

    /// The templates in the storage pool: their state, snapshots and size.
    pub fn templates(&self) -> Vec<TemplateReport> {
        storage::dir_names(&self.layout.root.join("templates"))
            .into_iter()
            .filter_map(|name| self.template_report(&name))
            .collect()
    }

    // ── Create ─────────────────────────────────────────────────────────────

    pub async fn create(&self, spec: SandboxSpec) -> Result<SandboxStatus> {
        spec.validate()
            .map_err(|m| err(ErrorCode::InvalidSpec, m))?;
        let cap = self.capability();
        if !cap.allows(spec.isolation) {
            let msg = match (spec.isolation, cap.runtime) {
                (Isolation::Vm, Runtime::Gvisor) => {
                    "This server has no KVM, so it can't run a VM sandbox.".into()
                }
                _ => cap.summary(),
            };
            return Err(err(ErrorCode::Unsupported, msg));
        }
        let on_gvisor = cap.runtime == Runtime::Gvisor;
        if on_gvisor && self.gv.is_none() {
            return Err(err(
                ErrorCode::Unsupported,
                "This server has no runsc, so it can't run gVisor sandboxes.",
            ));
        }
        let net = match spec.network {
            NetworkMode::Internet => Some(self.network().cloned().ok_or_else(|| {
                err(
                    ErrorCode::Unsupported,
                    "This server has no sandbox networking. Create the sandbox with network set to none.",
                )
            })?),
            NetworkMode::None => None,
        };
        let rootfs = self.layout.template_rootfs(&spec.template);
        match self.template_report(&spec.template).map(|t| t.state) {
            Some(TemplateState::Ready) if rootfs.exists() => {}
            Some(TemplateState::Building) => {
                return Err(err(
                    ErrorCode::Conflict,
                    format!(
                        "The sandbox template {} is still being made on this server.",
                        spec.template
                    ),
                ));
            }
            _ => {
                return Err(err(
                    ErrorCode::NotFound,
                    format!(
                        "The sandbox template {} isn't on this server.",
                        spec.template
                    ),
                ));
            }
        }
        let key = self.snapshot_key(net.is_some(), spec.cpus, spec.memory_mib, spec.disk_mib);
        let snap_dir = self.layout.template_snapshot(&spec.template, &key);
        // gVisor starts cold: its restores cost more and share no pages.
        let from_snapshot = !on_gvisor && snap_dir.join("vm.snap").exists();
        if self
            .sandboxes
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .contains_key(&spec.id)
        {
            return Err(err(
                ErrorCode::Conflict,
                format!("Sandbox {} already exists.", spec.id),
            ));
        }
        let lease = match &net {
            Some(n) => Some(n.acquire().await.map_err(runtime)?),
            None => None,
        };
        // Gives the lease back when the create fails from here on.
        let give_back = |lease: Option<Lease>| {
            let net = net.clone();
            async move {
                if let (Some(n), Some(l)) = (net, lease) {
                    n.release(&l).await;
                }
            }
        };

        let now = crate::now_ms();
        let status = SandboxStatus {
            id: spec.id.clone(),
            team: spec.team.clone(),
            template: spec.template.clone(),
            state: SandboxState::Creating,
            runtime: if on_gvisor {
                Runtime::Gvisor
            } else {
                Runtime::Firecracker
            },
            cpus: spec.cpus,
            memory_mib: spec.memory_mib,
            disk_mib: spec.disk_mib,
            network: spec.network,
            labels: spec.labels.clone(),
            created_at: now,
            started_at: None,
            expires_at: now + spec.ttl_secs * 1000,
            start_ms: None,
            from_snapshot,
            memory_used: None,
            error: None,
            ports: Vec::new(),
        };
        let sbx = Arc::new(Sandbox {
            spec: spec.clone(),
            status: StdMutex::new(status),
            vm: Mutex::new(None),
            lease: lease.clone(),
        });
        let mut vm_slot = sbx.vm.lock().await;
        let admitted = {
            // Admission and insertion under one lock, so two creates can't
            // both take the last room.
            let mut all = self.sandboxes.lock().unwrap_or_else(|p| p.into_inner());
            if all.contains_key(&spec.id) {
                Err(err(
                    ErrorCode::Conflict,
                    format!("Sandbox {} already exists.", spec.id),
                ))
            } else {
                let committed: u64 = all
                    .values()
                    .map(|s| s.status())
                    .filter(|s| !s.state.is_final() && s.state != SandboxState::Paused)
                    .map(|s| (s.memory_mib as u64) << 20)
                    .sum();
                let mem = capacity::meminfo();
                capacity::admits(
                    mem,
                    self.cfg.reserve_bytes(mem.total),
                    self.cfg.memory_overcommit,
                    committed,
                    (spec.memory_mib as u64) << 20,
                    from_snapshot,
                )
                .map_err(|m| err(ErrorCode::NoCapacity, m))
                .map(|()| {
                    all.insert(spec.id.clone(), sbx.clone());
                })
            }
        };
        if let Err(e) = admitted {
            give_back(lease).await;
            return Err(e);
        }
        let nic = lease
            .as_ref()
            .zip(net.as_ref())
            .map(|(l, n)| (l, n.guest_network(l)));
        let nic = nic.as_ref().map(|(l, g)| Nic {
            tap: &l.tap,
            guest: Some(g),
        });

        let dir = self.layout.vm_dir(&spec.id);
        let started = if on_gvisor {
            self.gvisor_start(&spec, &dir, &rootfs, lease.as_ref(), false)
                .await
                .map(|(c, took)| (Instance::Gvisor(c), took))
        } else if from_snapshot {
            self.start_from_snapshot(&spec, &dir, &rootfs, &snap_dir, nic)
                .await
                .map(|(vm, took)| (Instance::Vm(vm), took))
        } else {
            self.cold_start(&spec, &dir, &rootfs, nic)
                .await
                .map(|(vm, took)| (Instance::Vm(vm), took))
        };
        match started {
            Ok((vm, took)) => {
                let pid = vm.pid();
                *vm_slot = Some(vm);
                let st = sbx.update(|s| {
                    let now = crate::now_ms();
                    s.state = SandboxState::Running;
                    s.started_at = Some(now);
                    s.expires_at = now + spec.ttl_secs * 1000;
                    s.start_ms = Some(took.as_millis() as u32);
                });
                self.persist(&sbx, Some(pid));
                tracing::info!(id = %spec.id, ms = took.as_millis() as u64, from_snapshot, "sandbox started");
                Ok(st)
            }
            Err(e) => {
                self.sandboxes
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .remove(&spec.id);
                self.remove_files(&spec.id).await;
                give_back(lease).await;
                tracing::warn!(id = %spec.id, error = %format!("{e:#}"), "sandbox didn't start");
                Err(runtime(e))
            }
        }
    }

    /// Starts, or with `resume` restores, a sandbox under runsc.
    async fn gvisor_start(
        &self,
        spec: &SandboxSpec,
        dir: &Path,
        rootfs: &Path,
        lease: Option<&Lease>,
        resume: bool,
    ) -> anyhow::Result<(Container, Duration)> {
        let gv = self
            .gv
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("this server has no runsc"))?;
        tokio::fs::create_dir_all(dir).await?;
        let template_root = gv.template_root(rootfs).await?;
        let scratch = if resume {
            None
        } else {
            Some(storage::empty_ext4(&self.layout.cache(), spec.disk_mib).await?)
        };
        let netns = lease
            .zip(self.net.as_ref())
            .and_then(|(l, n)| n.netns_path(l));
        let dns = self
            .net
            .as_ref()
            .map(|n| n.cfg.dns.clone())
            .unwrap_or_default();
        let id = storage::instance_id(&spec.id);
        let m = gvisor::Machine {
            id: &id,
            dir,
            template_root: &template_root,
            scratch: scratch.as_deref(),
            cpus: spec.cpus,
            memory_mib: spec.memory_mib,
            netns: netns.as_deref(),
            dns: &dns,
            cgroup_parent: &gv.cgroup_parent,
        };
        if resume {
            gv.restore(&m, BOOT_TIMEOUT).await
        } else {
            gv.start(&m, BOOT_TIMEOUT).await
        }
    }

    /// Removes a sandbox's directory. On gVisor its container and mounts go
    /// first, so nothing reaches into the template's disk.
    async fn remove_files(&self, id: &str) {
        let top = self.layout.sandbox_top(id);
        if let Some(gv) = &self.gv {
            gv.teardown(&storage::instance_id(id), &top).await;
        }
        let _ = tokio::fs::remove_dir_all(top).await;
    }

    pub fn snapshot_key(&self, network: bool, cpus: u32, memory_mib: u32, disk_mib: u32) -> String {
        snapshot_key(
            &self.capability().firecracker_version,
            self.fc.cpu_template.as_deref(),
            network,
            cpus,
            memory_mib,
            disk_mib,
        )
    }

    async fn cold_start(
        &self,
        spec: &SandboxSpec,
        dir: &Path,
        rootfs: &Path,
        nic: Option<Nic<'_>>,
    ) -> anyhow::Result<(Vm, Duration)> {
        let scratch = storage::empty_ext4(&self.layout.cache(), spec.disk_mib).await?;
        let m = Machine {
            id: &spec.id,
            dir,
            rootfs,
            scratch: Some(&scratch),
            cpus: spec.cpus,
            memory_mib: spec.memory_mib,
            nic,
        };
        self.fc.cold_boot(&m, BOOT_TIMEOUT).await
    }

    async fn start_from_snapshot(
        &self,
        spec: &SandboxSpec,
        dir: &Path,
        rootfs: &Path,
        snap_dir: &Path,
        nic: Option<Nic<'_>>,
    ) -> anyhow::Result<(Vm, Duration)> {
        tokio::fs::create_dir_all(dir).await?;
        // The template snapshot is shared by every sandbox and never
        // copied. Only the writable layer is, and it's sparse.
        let dev = !self.layout.jailed;
        storage::link(
            &snap_dir.join("vm.snap"),
            &dir.join(format!("{TEMPLATE}.snap")),
            dev,
        )?;
        storage::link(
            &snap_dir.join("vm.mem"),
            &dir.join(format!("{TEMPLATE}.mem")),
            dev,
        )?;
        let scratch = snap_dir.join(SCRATCH);
        let m = Machine {
            id: &spec.id,
            dir,
            rootfs,
            scratch: Some(&scratch),
            cpus: spec.cpus,
            memory_mib: spec.memory_mib,
            nic,
        };
        self.fc.restore(&m, TEMPLATE, BOOT_TIMEOUT).await
    }

    /// Boots a template once, waits for envd and snapshots it. New sandboxes
    /// of the same size restore from it.
    pub async fn snapshot_template(
        &self,
        template: &str,
        cpus: u32,
        memory_mib: u32,
        disk_mib: u32,
        network: Option<NetworkMode>,
    ) -> Result<(String, u64)> {
        if !pyo_proto::sandbox::is_template_name(template) {
            return Err(err(
                ErrorCode::InvalidSpec,
                format!("{template:?} isn't a sandbox template name."),
            ));
        }
        if self.capability().runtime != Runtime::Firecracker {
            return Err(err(
                ErrorCode::Unsupported,
                "Template snapshots need Firecracker.",
            ));
        }
        let rootfs = self.layout.template_rootfs(template);
        if !rootfs.exists() {
            return Err(err(
                ErrorCode::NotFound,
                format!("The sandbox template {template} isn't on this server."),
            ));
        }
        let net = match network {
            None | Some(NetworkMode::Internet) => self.network().cloned(),
            Some(NetworkMode::None) => None,
        };
        if network == Some(NetworkMode::Internet) && net.is_none() {
            return Err(err(
                ErrorCode::Unsupported,
                "This server has no sandbox networking, so it can't snapshot a template with network.",
            ));
        }
        let t0 = std::time::Instant::now();
        let key = self.snapshot_key(net.is_some(), cpus, memory_mib, disk_mib);
        let final_dir = self.layout.template_snapshot(template, &key);
        let build_id = format!("tmpl-{}", crate::now_ms());
        let dir = self.layout.vm_dir(&build_id);
        // The build VM gets a tap only so the snapshot has an interface.
        // Each restore points it at its own tap and sets its own address.
        let lease = match &net {
            Some(n) => Some(n.acquire().await.map_err(runtime)?),
            None => None,
        };
        let res = async {
            let scratch = storage::empty_ext4(&self.layout.cache(), disk_mib).await?;
            let m = Machine {
                id: &build_id,
                dir: &dir,
                rootfs: &rootfs,
                scratch: Some(&scratch),
                cpus,
                memory_mib,
                nic: lease.as_ref().map(|l| Nic {
                    tap: &l.tap,
                    guest: None,
                }),
            };
            let (mut vm, _) = self.fc.cold_boot(&m, BOOT_TIMEOUT).await?;
            let snap = vm.snapshot("vm").await;
            vm.kill().await;
            snap?;
            let tmp = final_dir.with_extension("tmp");
            let _ = tokio::fs::remove_dir_all(&tmp).await;
            tokio::fs::create_dir_all(&tmp).await?;
            for f in ["vm.snap", "vm.mem", SCRATCH] {
                tokio::fs::rename(dir.join(f), tmp.join(f)).await?;
            }
            storage::dig_holes(&tmp.join("vm.mem")).await?;
            let _ = tokio::fs::remove_dir_all(&final_dir).await;
            tokio::fs::rename(&tmp, &final_dir).await?;
            anyhow::Ok(())
        }
        .await;
        let _ = tokio::fs::remove_dir_all(self.layout.sandbox_top(&build_id)).await;
        if let (Some(n), Some(l)) = (&net, &lease) {
            n.release(l).await;
        }
        res.map_err(runtime)?;
        Ok((key, t0.elapsed().as_millis() as u64))
    }

    // ── Lifecycle ──────────────────────────────────────────────────────────

    pub async fn delete(&self, id: &str) -> Result<()> {
        let sbx = self.find(id)?;
        let mut vm = sbx.vm.lock().await;
        if let Some(mut v) = vm.take() {
            v.kill().await;
        }
        // Its mounts and runsc state go before its network namespace.
        self.remove_files(id).await;
        if let (Some(l), Some(n)) = (&sbx.lease, &self.net) {
            n.release(l).await;
        }
        sbx.update(|s| s.state = SandboxState::Stopped);
        self.sandboxes
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(id);
        tracing::info!(id, "sandbox deleted");
        Ok(())
    }

    /// Snapshots the VM to local disk and stops it, which frees its memory.
    ///
    /// TODO(garage): upload the snapshot to Garage (holes punched, then
    /// compressed), so a sandbox resumes on any server with the same CPU
    /// vendor and a dead server loses nothing. Look at diff snapshots for
    /// pause and UFFD lazy loading for resume.
    pub async fn pause(&self, id: &str) -> Result<SandboxStatus> {
        let sbx = self.find(id)?;
        let mut slot = sbx.vm.lock().await;
        if sbx.status().state != SandboxState::Running {
            return Err(err(
                ErrorCode::Conflict,
                "Only a running sandbox can be paused.",
            ));
        }
        let Some(inst) = slot.take() else {
            return Err(err(ErrorCode::Conflict, "The sandbox has no VM."));
        };
        sbx.update(|s| s.state = SandboxState::Pausing);
        let mut vm = match inst {
            Instance::Vm(vm) => vm,
            Instance::Gvisor(mut c) => {
                return match c.checkpoint().await {
                    Ok(()) => {
                        let st = sbx.update(|s| s.state = SandboxState::Paused);
                        self.persist(&sbx, None);
                        Ok(st)
                    }
                    Err(e) => {
                        *slot = Some(Instance::Gvisor(c));
                        sbx.update(|s| s.state = SandboxState::Running);
                        Err(runtime(e))
                    }
                };
            }
        };
        let snap = vm.snapshot(PAUSE).await;
        if let Err(e) = snap {
            let _ = vm.resume().await;
            *slot = Some(Instance::Vm(vm));
            sbx.update(|s| s.state = SandboxState::Running);
            return Err(runtime(e));
        }
        vm.kill().await;
        let _ = storage::dig_holes(&vm.dir.join(format!("{PAUSE}.mem"))).await;
        let st = sbx.update(|s| s.state = SandboxState::Paused);
        self.persist(&sbx, None);
        Ok(st)
    }

    pub async fn resume(&self, id: &str) -> Result<SandboxStatus> {
        let sbx = self.find(id)?;
        let mut slot = sbx.vm.lock().await;
        if sbx.status().state != SandboxState::Paused {
            return Err(err(
                ErrorCode::Conflict,
                "Only a paused sandbox can be resumed.",
            ));
        }
        sbx.update(|s| s.state = SandboxState::Resuming);
        let dir = self.layout.vm_dir(id);
        let rootfs = self.layout.template_rootfs(&sbx.spec.template);
        if sbx.status().runtime == Runtime::Gvisor {
            return match self
                .gvisor_start(&sbx.spec, &dir, &rootfs, sbx.lease.as_ref(), true)
                .await
            {
                Ok((c, took)) => {
                    let pid = c.pid;
                    *slot = Some(Instance::Gvisor(c));
                    let st = sbx.update(|s| {
                        s.state = SandboxState::Running;
                        s.start_ms = Some(took.as_millis() as u32);
                    });
                    self.persist(&sbx, Some(pid));
                    Ok(st)
                }
                Err(e) => {
                    sbx.update(|s| s.state = SandboxState::Paused);
                    Err(runtime(e))
                }
            };
        }
        let guest = match (&sbx.lease, &self.net) {
            (Some(l), Some(n)) => Some(n.guest_network(l)),
            _ => None,
        };
        // The writable layer is already in place, so nothing is copied.
        let m = Machine {
            id,
            dir: &dir,
            rootfs: &rootfs,
            scratch: None,
            cpus: sbx.spec.cpus,
            memory_mib: sbx.spec.memory_mib,
            nic: sbx.lease.as_ref().map(|l| Nic {
                tap: &l.tap,
                guest: guest.as_ref(),
            }),
        };
        match self.fc.restore(&m, PAUSE, BOOT_TIMEOUT).await {
            Ok((vm, took)) => {
                let pid = vm.pid;
                *slot = Some(Instance::Vm(vm));
                // The restored VM has the memory file mapped, so it can go.
                for ext in ["snap", "mem"] {
                    let _ = tokio::fs::remove_file(dir.join(format!("{PAUSE}.{ext}"))).await;
                }
                let st = sbx.update(|s| {
                    s.state = SandboxState::Running;
                    s.start_ms = Some(took.as_millis() as u32);
                });
                self.persist(&sbx, Some(pid));
                Ok(st)
            }
            Err(e) => {
                sbx.update(|s| s.state = SandboxState::Paused);
                Err(runtime(e))
            }
        }
    }

    /// The edge proxy's base URL, once it listens. Ports get their URLs
    /// from it.
    pub fn set_edge_url(&self, url: Option<String>) {
        self.capability
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .edge_url = url;
    }

    /// Serves a port on the edge proxy, or changes whether it's public.
    pub async fn open_port(&self, id: &str, port: u16, public: bool) -> Result<SandboxStatus> {
        if port == 0 {
            return Err(err(ErrorCode::InvalidSpec, "The port must be 1 to 65535."));
        }
        self.change_ports(id, |ports| {
            ports.retain(|p| p.port != port);
            ports.push(OpenPort { port, public });
            ports.sort_by_key(|p| p.port);
        })
        .await
    }

    pub async fn close_port(&self, id: &str, port: u16) -> Result<SandboxStatus> {
        self.change_ports(id, |ports| ports.retain(|p| p.port != port))
            .await
    }

    async fn change_ports(
        &self,
        id: &str,
        f: impl FnOnce(&mut Vec<OpenPort>),
    ) -> Result<SandboxStatus> {
        let sbx = self.find(id)?;
        let vm = sbx.vm.lock().await;
        if sbx.status().state.is_final() {
            return Err(err(ErrorCode::Conflict, "The sandbox has ended."));
        }
        let st = sbx.update(|s| f(&mut s.ports));
        self.persist(&sbx, vm.as_ref().map(Instance::pid));
        Ok(st)
    }

    pub fn set_ttl(&self, id: &str, ttl_secs: u64) -> Result<SandboxStatus> {
        if !(1..=MAX_TTL_SECS).contains(&ttl_secs) {
            return Err(err(
                ErrorCode::InvalidSpec,
                "The lifetime must be 1 second to 24 hours.",
            ));
        }
        let sbx = self.find(id)?;
        let st = sbx.update(|s| s.expires_at = crate::now_ms() + ttl_secs * 1000);
        Ok(st)
    }

    /// A new connection to the sandbox's envd.
    pub async fn envd(&self, id: &str) -> Result<(Arc<Sandbox>, envd::Conn)> {
        let sbx = self.find(id)?;
        let dial = {
            let vm = sbx.vm.lock().await;
            match (sbx.status().state, vm.as_ref()) {
                (SandboxState::Running, Some(Instance::Vm(vm))) => {
                    let (pin, path) = vm.vsock_pinned().map_err(runtime)?;
                    Dial::Vsock(pin, path)
                }
                (SandboxState::Running, Some(Instance::Gvisor(c))) => Dial::Door(c.door.clone()),
                (SandboxState::Paused, _) => {
                    return Err(err(
                        ErrorCode::Conflict,
                        "The sandbox is paused. Resume it first.",
                    ));
                }
                _ => return Err(err(ErrorCode::Conflict, "The sandbox isn't running.")),
            }
        };
        let conn = match dial {
            Dial::Vsock(_pin, vsock) => envd::Conn::connect(&vsock, ENVD_TIMEOUT).await,
            Dial::Door(door) => door.accept(ENVD_TIMEOUT).await,
        }
        .map_err(runtime)?;
        Ok((sbx, conn))
    }

    pub async fn exec(&self, id: &str, req: &ExecRequest) -> Result<ExecResult> {
        let (sbx, mut conn) = self.envd(id).await?;
        conn.exec(req, &sbx.spec.env).await.map_err(runtime)
    }

    // ── Upkeep ─────────────────────────────────────────────────────────────

    /// Deletes sandboxes past their lifetime and marks VMs that died.
    pub async fn sweep(&self) {
        let now = crate::now_ms();
        for sbx in self.all() {
            let st = sbx.status();
            if now >= st.expires_at {
                tracing::info!(id = %st.id, "sandbox lifetime is over");
                let _ = self.delete(&st.id).await;
                continue;
            }
            if st.state == SandboxState::Running
                && let Ok(mut vm) = sbx.vm.try_lock()
                && vm.as_mut().is_some_and(|v| v.exited())
            {
                *vm = None;
                sbx.update(|s| {
                    s.state = SandboxState::Failed;
                    s.error = Some("The sandbox VM stopped on its own.".into());
                });
                tracing::warn!(id = %st.id, "sandbox VM exited");
            }
        }
    }

    pub fn spawn_sweeper(self: &Arc<Self>) {
        let me = self.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(1));
            loop {
                tick.tick().await;
                me.sweep().await;
            }
        });
    }

    fn persist(&self, sbx: &Sandbox, pid: Option<u32>) {
        let rec = Record {
            spec: sbx.spec.clone(),
            status: sbx.status(),
            pid,
            lease: sbx.lease.clone(),
        };
        let path = self.layout.vm_dir(&sbx.spec.id).join("sandbox.json");
        if let Err(e) = std::fs::write(&path, serde_json::to_vec_pretty(&rec).unwrap_or_default()) {
            tracing::warn!(error = %e, path = %path.display(), "can't save the sandbox record");
        }
    }

    /// Takes over the sandboxes a previous daemon left. Running VMs whose
    /// process is still there and paused sandboxes are kept. The rest is
    /// cleaned up. Then puts the network table in place for the kept ones,
    /// which removes the taps of the rest.
    pub async fn adopt(&self) -> usize {
        self.adopt_templates().await;
        let mut leases = Vec::new();
        let adopted = self.adopt_vms(&mut leases).await;
        if let Some(net) = &self.net {
            match net.sync(&leases).await {
                Ok(()) => {
                    self.capability
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .network = true;
                    tracing::info!(pool = %net.cfg.pool, "sandbox networking is on");
                }
                Err(e) => {
                    let detail = format!("{e:#}");
                    tracing::warn!(error = %detail, "sandbox networking is off");
                    self.capability
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .checks
                        .push(pyo_proto::sandbox::Check {
                            name: "networkSetup".into(),
                            ok: false,
                            detail,
                        });
                }
            }
        }
        adopted
    }

    async fn adopt_vms(&self, leases: &mut Vec<(Lease, bool)>) -> usize {
        let Ok(mut rd) = tokio::fs::read_dir(self.layout.sandboxes()).await else {
            return 0;
        };
        let mut adopted = 0;
        while let Ok(Some(e)) = rd.next_entry().await {
            // The directory is named by the instance ID (see storage).
            let name = e.file_name().to_string_lossy().into_owned();
            let dir = self.layout.vm_dir(&name);
            let rec: Option<Record> = std::fs::read(dir.join("sandbox.json"))
                .ok()
                .and_then(|b| serde_json::from_slice(&b).ok());
            // A sandbox of the other runtime can't be kept: KVM came or went.
            let runtime_now = self.capability().runtime;
            let keep = match &rec {
                Some(r) if r.status.runtime != runtime_now => None,
                Some(r) if r.status.state == SandboxState::Paused => Some(None),
                Some(r) if runtime_now == Runtime::Gvisor => match &self.gv {
                    Some(gv) => gv
                        .adopt(&name, &dir, r.lease.is_some())
                        .await
                        .map(|c| Some(Instance::Gvisor(c))),
                    None => None,
                },
                Some(r) => match r.pid {
                    Some(pid) if crate::firecracker::is_firecracker(pid) => {
                        Vm::adopt(&r.spec.id, &dir, pid)
                            .ok()
                            .map(|vm| Some(Instance::Vm(vm)))
                    }
                    _ => None,
                },
                None => None,
            };
            match (rec, keep) {
                (Some(r), Some(vm)) if storage::instance_id(&r.spec.id) == name => {
                    let id = r.spec.id.clone();
                    if let Some(l) = &r.lease {
                        leases.push((l.clone(), vm.is_some()));
                    }
                    let sbx = Arc::new(Sandbox {
                        spec: r.spec,
                        status: StdMutex::new(r.status),
                        vm: Mutex::new(vm),
                        lease: r.lease,
                    });
                    self.sandboxes
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .insert(id.clone(), sbx);
                    adopted += 1;
                }
                _ => {
                    if let Some(gv) = &self.gv {
                        gv.teardown(&name, &self.layout.sandbox_top(&name)).await;
                    }
                    let _ = tokio::fs::remove_dir_all(self.layout.sandbox_top(&name)).await;
                }
            }
        }
        adopted
    }

    /// Stops every VM. For tests and for turning sandboxes off.
    pub async fn delete_all(&self) {
        for s in self.all() {
            let _ = self.delete(&s.spec.id).await;
        }
    }

    pub fn root(&self) -> PathBuf {
        self.layout.root.clone()
    }
}
