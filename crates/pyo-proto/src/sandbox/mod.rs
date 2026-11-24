//! Pyo Sandbox: the types shared by the Pyo API, the `pyo-sandbox` daemon on
//! a sandbox server and `pyo-envd` inside each sandbox.
//!
//! - [`SandboxSpec`] is what a client asks for, [`SandboxStatus`] what it gets.
//! - [`CapabilityReport`] is what a sandbox server can run, found on every
//!   daemon start.
//! - [`DaemonCommand`] and [`DaemonResult`] are what Pyo sends the daemon.
//! - [`token`] is the short-lived signed token clients show the daemon.
//! - [`wire`] is the framed protocol between the daemon and `pyo-envd`.
//! - [`outbound`] is how a sandbox server on Pyo Cloud joins and gets its
//!   commands over HTTPS.
//!
//! JSON is camelCase with timestamps in epoch milliseconds, like the rest of
//! pyo-proto.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

pub mod outbound;
#[cfg(feature = "sandbox-token")]
pub mod token;
pub mod wire;

pub use wire::Scope;

/// Bumped on breaking changes between Pyo and the daemon.
pub const SANDBOX_PROTOCOL: u32 = 1;

/// Every sandbox ID starts with this.
pub const SANDBOX_ID_PREFIX: &str = "sbx_";

/// The daemon API's port on a sandbox server's tailnet address.
pub const DAEMON_PORT: u16 = 7070;

// ── Limits and defaults ────────────────────────────────────────────────────

pub const DEFAULT_CPUS: u32 = 1;
pub const MAX_CPUS: u32 = 32;
pub const DEFAULT_MEMORY_MIB: u32 = 512;
pub const MIN_MEMORY_MIB: u32 = 128;
pub const MAX_MEMORY_MIB: u32 = 64 * 1024;
/// Size of the sandbox's writable layer. It is a sparse file, so only what
/// the sandbox writes takes space.
pub const DEFAULT_DISK_MIB: u32 = 2048;
pub const MIN_DISK_MIB: u32 = 64;
pub const MAX_DISK_MIB: u32 = 256 * 1024;
pub const DEFAULT_TTL_SECS: u64 = 5 * 60;
/// The longest lifetime a sandbox can have. A sandbox may also have none
/// (`ttl_secs` of `None`): it runs until it's deleted.
pub const MAX_TTL_SECS: u64 = 365 * 24 * 60 * 60;
/// How "no lifetime" is written where a lifetime is asked for.
pub const NO_TTL: &str = "none";
pub const MAX_ENV: usize = 128;
pub const MAX_LABELS: usize = 32;
/// The most a template's files may take, unpacked, unless Pyo says
/// otherwise when it asks a server to prepare one.
pub const DEFAULT_TEMPLATE_MAX_MIB: u32 = 10 * 1024;

// ── Image contract ─────────────────────────────────────────────────────────

/// The label an image made for Pyo sandboxes carries: the contract it was
/// made for (upsetdev/pyo-sandbox-images). Images without it are anyone's
/// and are taken as they are.
pub const CONTRACT_LABEL: &str = "dev.pyo.sandbox.contract";

/// The image contracts this Pyo runs. Contract 1: pyo-envd runs commands as
/// root in /root with its own PATH, and runs /etc/pyo/start once at boot
/// when it's there.
pub const CONTRACTS: &[u32] = &[1];

/// Why an image whose [`CONTRACT_LABEL`] is `label` can't be used, or None
/// when it can.
pub fn contract_refusal(label: &str) -> Option<String> {
    let label = label.trim();
    match label.parse::<u32>() {
        Ok(n) if CONTRACTS.contains(&n) => None,
        Ok(n) => Some(format!(
            "The image is made for Pyo sandbox contract {n}, and this server runs contract {}. Update Pyo and the server first.",
            CONTRACTS.iter().max().copied().unwrap_or(1)
        )),
        Err(_) => Some(format!(
            "The image's {CONTRACT_LABEL} label is \"{label}\", which isn't a contract number."
        )),
    }
}

// ── Runtime detection ──────────────────────────────────────────────────────

/// Boot args of every sandbox VM, the detection test boot included. They
/// skip a PS/2 probe that costs about 520 ms and the serial console. The
/// native and nested threshold only holds with them.
pub const FAST_BOOT_ARGS: &str = "i8042.noaux i8042.nomux i8042.dumbkbd 8250.nr_uarts=0";

/// A test boot under this many milliseconds counts as native KVM, slower as
/// nested. A native boot takes about 75 ms with [`FAST_BOOT_ARGS`].
pub const NATIVE_BOOT_MS: u32 = 300;

/// The test boot gives up after this long.
pub const TEST_BOOT_TIMEOUT_MS: u32 = 5000;

/// Firecracker needs a host kernel at least this new.
pub const MIN_HOST_KERNEL: (u32, u32) = (5, 10);

/// The isolation a sandbox runs under.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Runtime {
    Firecracker,
    Gvisor,
    /// The server can run neither. Only in a [`CapabilityReport`].
    #[default]
    None,
}

impl Runtime {
    pub fn label(self) -> &'static str {
        match self {
            Runtime::Firecracker => "Firecracker",
            Runtime::Gvisor => "gVisor",
            Runtime::None => "No runtime",
        }
    }
}

/// How the server reaches KVM.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Kvm {
    Native,
    Nested,
    #[default]
    None,
}

/// Native or nested, from how long the test boot took.
pub fn classify_boot(boot_ms: u32) -> Kvm {
    if boot_ms < NATIVE_BOOT_MS {
        Kvm::Native
    } else {
        Kvm::Nested
    }
}

/// One detection check and what it found.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Check {
    pub name: String,
    pub ok: bool,
    pub detail: String,
}

/// What a sandbox server can run. The daemon finds it on every start and
/// sends it with every report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct CapabilityReport {
    pub runtime: Runtime,
    pub kvm: Kvm,
    /// How long the test boot took, when there was one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub boot_ms: Option<u32>,
    /// `x86_64` or `aarch64`.
    pub arch: String,
    pub cpus: u32,
    /// Host memory in bytes.
    pub memory: u64,
    /// Free bytes in the storage pool.
    pub disk: u64,
    /// `GenuineIntel`, `AuthenticAMD` or empty. Firecracker snapshots only
    /// resume on the same vendor.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub cpu_vendor: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub firecracker_version: String,
    /// The runsc release on gVisor servers, like `release-20261109.0`.
    /// Paused sandboxes only resume under the same one.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub gvisor_version: String,
    /// Whether sandboxes on this server can get a network.
    #[serde(default)]
    pub network: bool,
    /// The edge proxy's base URL, like `https://sbx.example.com`, when it
    /// runs. A port's URL comes from [`CapabilityReport::port_url`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub edge_url: Option<String>,
    /// The fingerprint ([`cert_fingerprint`]) of the certificate the edge
    /// serves over HTTPS, when it has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub edge_cert: Option<String>,
    /// The server's public IPv4 and IPv6 addresses, when its default route
    /// leaves from one. A server behind NAT has none: its DNS record has to
    /// be made by hand.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub public_ips: Vec<std::net::IpAddr>,
    #[serde(default)]
    pub checks: Vec<Check>,
    /// Why the server can't run sandboxes, in one plain sentence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl CapabilityReport {
    /// The one line the dashboard shows next to the runtime badge.
    pub fn summary(&self) -> String {
        match (self.runtime, self.kvm) {
            (Runtime::Firecracker, Kvm::Native) => match self.boot_ms {
                Some(ms) => format!("Firecracker on KVM. A test VM booted in {ms} ms."),
                None => "Firecracker on KVM.".into(),
            },
            (Runtime::Firecracker, _) => match self.boot_ms {
                Some(ms) => format!(
                    "Firecracker on nested KVM. A test VM booted in {ms} ms, so sandboxes start slower."
                ),
                None => "Firecracker on nested KVM.".into(),
            },
            (Runtime::Gvisor, _) => "gVisor, because this server has no usable KVM.".into(),
            (Runtime::None, _) => self
                .reason
                .clone()
                .unwrap_or_else(|| "This server can't run sandboxes.".into()),
        }
    }

    /// The URL of a sandbox port on this server's edge proxy.
    pub fn port_url(&self, sandbox: &str, port: u16) -> Option<String> {
        let base = self.edge_url.as_deref()?;
        let (scheme, rest) = base.split_once("://")?;
        let (domain, listen_port) = match rest.trim_end_matches('/').rsplit_once(':') {
            Some((d, p)) => (d, format!(":{p}")),
            None => (rest.trim_end_matches('/'), String::new()),
        };
        Some(format!(
            "{scheme}://{}{listen_port}",
            port_host(port, sandbox, domain)
        ))
    }

    /// The edge proxy's domain, from its URL.
    pub fn edge_domain(&self) -> Option<&str> {
        let rest = self.edge_url.as_deref()?.split_once("://")?.1;
        let host = rest.split(['/', ':']).next()?;
        (!host.is_empty()).then_some(host)
    }

    /// Whether a sandbox asking for `isolation` can run here.
    pub fn allows(&self, isolation: Isolation) -> bool {
        match isolation {
            Isolation::Auto => self.runtime != Runtime::None,
            Isolation::Vm => self.runtime == Runtime::Firecracker,
        }
    }
}

// ── The sandbox spec ───────────────────────────────────────────────────────

/// `auto` runs on Firecracker where it can and on gVisor elsewhere. `vm`
/// needs Firecracker and fails without it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Isolation {
    #[default]
    Auto,
    Vm,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum NetworkMode {
    /// Internet access, with the tailnet, the host, Pyo and cloud metadata
    /// always blocked.
    #[default]
    Internet,
    /// No network at all.
    None,
}

/// What a client asks for. Pyo fills in `id` and `team` before it sends the
/// spec to a sandbox server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SandboxSpec {
    pub id: String,
    pub team: String,
    /// A sandbox template name, like `node-22`.
    pub template: String,
    #[serde(default = "default_cpus")]
    pub cpus: u32,
    #[serde(default = "default_memory")]
    pub memory_mib: u32,
    #[serde(default = "default_disk")]
    pub disk_mib: u32,
    /// The sandbox is deleted this long after it starts. `None` (null):
    /// it runs until it's deleted.
    #[serde(default = "default_ttl")]
    pub ttl_secs: Option<u64>,
    /// Pause after this long without a command or connection. Pause and
    /// resume come in v0.4.x, so the daemon doesn't act on it yet.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pause_when_idle_secs: Option<u64>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub network: NetworkMode,
    #[serde(default)]
    pub isolation: Isolation,
    #[serde(default)]
    pub labels: BTreeMap<String, String>,
}

fn default_cpus() -> u32 {
    DEFAULT_CPUS
}
fn default_memory() -> u32 {
    DEFAULT_MEMORY_MIB
}
fn default_disk() -> u32 {
    DEFAULT_DISK_MIB
}
fn default_ttl() -> Option<u64> {
    Some(DEFAULT_TTL_SECS)
}

impl SandboxSpec {
    /// A spec with the defaults.
    pub fn new(
        id: impl Into<String>,
        team: impl Into<String>,
        template: impl Into<String>,
    ) -> Self {
        SandboxSpec {
            id: id.into(),
            team: team.into(),
            template: template.into(),
            cpus: DEFAULT_CPUS,
            memory_mib: DEFAULT_MEMORY_MIB,
            disk_mib: DEFAULT_DISK_MIB,
            ttl_secs: Some(DEFAULT_TTL_SECS),
            pause_when_idle_secs: None,
            env: BTreeMap::new(),
            network: NetworkMode::default(),
            isolation: Isolation::default(),
            labels: BTreeMap::new(),
        }
    }

    /// Checks the spec. The message is fit to show the user.
    pub fn validate(&self) -> Result<(), String> {
        if !is_sandbox_id(&self.id) {
            return Err(format!("{:?} isn't a sandbox ID.", self.id));
        }
        if self.team.is_empty() {
            return Err("The sandbox has no team.".into());
        }
        if !is_template_name(&self.template) {
            return Err(format!(
                "{:?} isn't a sandbox template name.",
                self.template
            ));
        }
        if !(1..=MAX_CPUS).contains(&self.cpus) {
            return Err(format!("CPUs must be 1 to {MAX_CPUS}."));
        }
        if !(MIN_MEMORY_MIB..=MAX_MEMORY_MIB).contains(&self.memory_mib) {
            return Err(format!(
                "Memory must be {MIN_MEMORY_MIB} MiB to {} GiB.",
                MAX_MEMORY_MIB / 1024
            ));
        }
        if !(MIN_DISK_MIB..=MAX_DISK_MIB).contains(&self.disk_mib) {
            return Err(format!(
                "Disk must be {MIN_DISK_MIB} MiB to {} GiB.",
                MAX_DISK_MIB / 1024
            ));
        }
        check_ttl(self.ttl_secs)?;
        if self.env.len() > MAX_ENV {
            return Err(format!(
                "A sandbox takes at most {MAX_ENV} environment variables."
            ));
        }
        if let Some(k) = self.env.keys().find(|k| !is_env_name(k)) {
            return Err(format!("{k:?} isn't a valid environment variable name."));
        }
        if self.labels.len() > MAX_LABELS {
            return Err(format!("A sandbox takes at most {MAX_LABELS} labels."));
        }
        if let Some(k) = self.labels.keys().find(|k| !is_label_key(k)) {
            return Err(format!("{k:?} isn't a valid label."));
        }
        if let Some(v) = self.labels.values().find(|v| v.len() > 256) {
            return Err(format!(
                "The label value {v:?} is longer than 256 characters."
            ));
        }
        Ok(())
    }
}

/// `sbx_` and 12 to 40 lowercase letters or digits.
pub fn is_sandbox_id(s: &str) -> bool {
    s.strip_prefix(SANDBOX_ID_PREFIX).is_some_and(|rest| {
        (12..=40).contains(&rest.len())
            && rest
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
    })
}

/// A sandbox ID from 16 random bytes, in base 32 (26 characters).
pub fn sandbox_id_from(random: [u8; 16]) -> String {
    const ALPHABET: &[u8; 32] = b"abcdefghijklmnopqrstuvwxyz234567";
    let n = u128::from_be_bytes(random);
    let mut out = String::from(SANDBOX_ID_PREFIX);
    for i in (0..26).rev() {
        out.push(ALPHABET[((n >> (i * 5)) & 31) as usize] as char);
    }
    out
}

/// Lowercase letters, digits, `-`, `_` and `.`, 1 to 63 long, starting with a
/// letter or digit. Template names end up in paths on the server.
pub fn is_template_name(s: &str) -> bool {
    (1..=63).contains(&s.len())
        && s.as_bytes()[0].is_ascii_alphanumeric()
        && s.bytes().all(|b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'-' | b'_' | b'.')
        })
}

pub fn is_env_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 256
        && !s.as_bytes()[0].is_ascii_digit()
        && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

pub fn is_label_key(s: &str) -> bool {
    (1..=63).contains(&s.len())
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'/'))
}

/// Whether a lifetime is one a sandbox can have. `None` is no lifetime.
/// The message is fit to show the user.
pub fn check_ttl(ttl_secs: Option<u64>) -> Result<(), String> {
    match ttl_secs {
        Some(t) if !(1..=MAX_TTL_SECS).contains(&t) => {
            Err("The lifetime must be 1 second to 365 days.".into())
        }
        _ => Ok(()),
    }
}

/// When a sandbox with this lifetime ends, from `now` (epoch
/// milliseconds). `None`: never.
pub fn expiry(now: u64, ttl_secs: Option<u64>) -> Option<u64> {
    ttl_secs.map(|t| now.saturating_add(t.saturating_mul(1000)))
}

/// Parses a lifetime like [`parse_ttl`], or [`NO_TTL`] for none:
/// `Some(None)`.
pub fn parse_lifetime(s: &str) -> Option<Option<u64>> {
    if s.trim().eq_ignore_ascii_case(NO_TTL) {
        return Some(None);
    }
    parse_ttl(s).map(Some)
}

/// Parses a lifetime like `90s`, `30m`, `1h`, `1h30m` or `7d`. A bare
/// number is seconds.
pub fn parse_ttl(s: &str) -> Option<u64> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    if let Ok(n) = s.parse::<u64>() {
        return Some(n);
    }
    let mut total: u64 = 0;
    let mut num = String::new();
    for c in s.chars() {
        if c.is_ascii_digit() {
            num.push(c);
            continue;
        }
        let n: u64 = num.parse().ok()?;
        num.clear();
        let unit = match c {
            's' => 1,
            'm' => 60,
            'h' => 3600,
            'd' => 86400,
            _ => return None,
        };
        total = total.checked_add(n.checked_mul(unit)?)?;
    }
    if !num.is_empty() {
        return None;
    }
    Some(total)
}

// ── Status ─────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SandboxState {
    Creating,
    Running,
    Pausing,
    Paused,
    Resuming,
    Stopping,
    Stopped,
    Failed,
}

impl SandboxState {
    /// Whether the sandbox is gone for good.
    pub fn is_final(self) -> bool {
        matches!(self, SandboxState::Stopped | SandboxState::Failed)
    }
}

/// A sandbox as the daemon sees it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SandboxStatus {
    pub id: String,
    pub team: String,
    pub template: String,
    pub state: SandboxState,
    pub runtime: Runtime,
    pub cpus: u32,
    pub memory_mib: u32,
    pub disk_mib: u32,
    pub network: NetworkMode,
    #[serde(default)]
    pub labels: BTreeMap<String, String>,
    pub created_at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<u64>,
    /// When its lifetime ends. `None`: it runs until it's deleted.
    #[serde(default)]
    pub expires_at: Option<u64>,
    /// From the start of the VM process to envd's ready signal, or to the
    /// first answer after a restore.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_ms: Option<u32>,
    /// Whether it started from a template snapshot.
    #[serde(default)]
    pub from_snapshot: bool,
    /// Host memory the sandbox uses now, in bytes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_used: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Ports open on the server's edge proxy.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ports: Vec<OpenPort>,
}

/// A sandbox port the edge proxy serves at `<port>-<sandbox>.<domain>`.
/// A public port answers anyone. A private one needs a sandbox token with
/// the `ports` scope on every request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OpenPort {
    pub port: u16,
    pub public: bool,
}

/// The host a sandbox port has on the edge proxy: `3000-sbx-abc.<domain>`.
/// The `_` of the sandbox ID becomes `-`, since DNS names and certificates
/// don't take `_`.
pub fn port_host(port: u16, sandbox: &str, domain: &str) -> String {
    format!("{port}-{}.{domain}", sandbox.replacen('_', "-", 1))
}

/// The other way round: the port and sandbox ID from a host on the edge
/// proxy, or None when it isn't one. Takes either form of the ID, any
/// case, and a `:port` at the end.
pub fn parse_port_host(host: &str, domain: &str) -> Option<(u16, String)> {
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    let host = match host.rsplit_once(':') {
        Some((h, p)) if p.bytes().all(|b| b.is_ascii_digit()) => h.to_string(),
        _ => host,
    };
    let domain = domain.trim_end_matches('.').to_ascii_lowercase();
    let label = host.strip_suffix(&domain)?.strip_suffix('.')?;
    let (port, id) = label.split_once('-')?;
    if port.is_empty() || port.starts_with('0') {
        return None;
    }
    let port: u16 = port.parse().ok().filter(|p| *p > 0)?;
    let id = match id.strip_prefix("sbx-") {
        Some(rest) => format!("{SANDBOX_ID_PREFIX}{rest}"),
        None => id.to_string(),
    };
    is_sandbox_id(&id).then_some((port, id))
}

/// The memory picture of a sandbox server, for the scheduler.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct CapacityReport {
    /// All bytes below are host memory.
    pub memory_total: u64,
    /// MemAvailable from /proc/meminfo.
    pub memory_available: u64,
    /// Kept back for the host so it never swaps itself to death.
    pub memory_reserved: u64,
    /// Sum of each sandbox's cgroup `memory.current`, which counts the
    /// kernel memory that PSS misses.
    pub memory_used: u64,
    /// Sum of the memory the running sandboxes asked for. Guest memory only
    /// grows, so this is the worst case.
    pub memory_committed: u64,
    pub running: u32,
    pub paused: u32,
    /// CPUs the running sandboxes asked for.
    pub cpus_committed: u32,
    /// Bytes free in the storage pool.
    #[serde(default)]
    pub disk_free: u64,
    /// Bytes the templates take: root disks, snapshots and builds.
    #[serde(default)]
    pub disk_templates: u64,
    /// Bytes the sandboxes take: their writable layers and pause snapshots.
    /// Files shared with a template count once, under templates.
    #[serde(default)]
    pub disk_sandboxes: u64,
}

impl CapacityReport {
    /// Bytes a new sandbox may still take without eating the reserve.
    pub fn headroom(&self) -> u64 {
        self.memory_available.saturating_sub(self.memory_reserved)
    }
}

/// A sandbox template on a server, and the sizes it has a snapshot for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct TemplateReport {
    pub name: String,
    /// Snapshot keys, each ending in [`snapshot_size_tag`].
    #[serde(default)]
    pub snapshots: Vec<String>,
    /// Only a ready template takes sandboxes. One put on the server by
    /// hand is ready.
    #[serde(default)]
    pub state: TemplateState,
    /// Bytes it takes on disk: its root disk, snapshots and build files.
    #[serde(default)]
    pub bytes: u64,
    /// How the server made it. None for a template put there by hand,
    /// which Pyo never deletes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build: Option<TemplateBuild>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum TemplateState {
    /// Pulling, unpacking or snapshotting.
    Building,
    #[default]
    Ready,
    Failed,
}

/// How a server made a template from an image, and how far it got.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct TemplateBuild {
    /// The image as Pyo gave it.
    pub image: String,
    /// The manifest digest it resolved to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub digest: Option<String>,
    /// While building: "queued", "pulling", "unpacking" or "snapshotting".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<String>,
    /// Bytes pulled so far, of `image_bytes` in all.
    #[serde(default)]
    pub pulled: u64,
    /// Bytes of the pulled layers unpacked so far, of `image_bytes` in
    /// all. Daemons before it leave it out.
    #[serde(default)]
    pub unpacked: u64,
    /// The image's compressed layers.
    #[serde(default)]
    pub image_bytes: u64,
    /// The root disk, as allocated on disk.
    #[serde(default)]
    pub rootfs_bytes: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pull_ms: Option<u64>,
    /// Unpacking the layers and making the root disk.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unpack_ms: Option<u64>,
    /// Booting it and taking its snapshots.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub started_at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<u64>,
}

/// A registry login for pulling a template's image.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RegistryAuth {
    pub username: String,
    pub password: String,
}

impl std::fmt::Debug for RegistryAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RegistryAuth")
            .field("username", &self.username)
            .finish_non_exhaustive()
    }
}

impl TemplateReport {
    /// Whether a sandbox of this size and network restores from a snapshot
    /// here.
    pub fn has_snapshot(
        &self,
        network: NetworkMode,
        cpus: u32,
        memory_mib: u32,
        disk_mib: u32,
    ) -> bool {
        let tag = snapshot_size_tag(cpus, memory_mib, disk_mib);
        self.snapshots.iter().any(|k| {
            k.strip_suffix(&tag).is_some_and(|head| {
                head.ends_with(SNAPSHOT_NET_TAG) == (network == NetworkMode::Internet)
            })
        })
    }
}

/// In a template snapshot's key right before the size: the snapshot has a
/// network interface. Sandboxes with network restore only from those, and
/// sandboxes without network only from the others, since a snapshot's
/// devices are fixed.
pub const SNAPSHOT_NET_TAG: &str = "-net";

/// The end of a template snapshot's key: the machine size it restores.
/// The key starts with the Firecracker version and CPU template, then
/// [`SNAPSHOT_NET_TAG`] for snapshots with a network interface.
pub fn snapshot_size_tag(cpus: u32, memory_mib: u32, disk_mib: u32) -> String {
    format!("-{cpus}c-{memory_mib}m-{disk_mib}d")
}

/// What a daemon reports to Pyo.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerReport {
    pub protocol: u32,
    pub daemon_version: String,
    pub server_id: String,
    pub capability: CapabilityReport,
    pub capacity: CapacityReport,
    pub sandboxes: Vec<SandboxStatus>,
    /// The templates on the server, so Pyo places sandboxes where theirs is.
    #[serde(default)]
    pub templates: Vec<TemplateReport>,
}

// ── Commands from Pyo and their results ────────────────────────────────────

/// What Pyo asks a daemon to do. `POST /v1/commands` takes one, and the
/// sandbox routes of the daemon map to the same commands.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum DaemonCommand {
    Create {
        spec: SandboxSpec,
    },
    Delete {
        id: String,
    },
    Pause {
        id: String,
    },
    Resume {
        id: String,
    },
    /// Moves the end of the lifetime to `ttl_secs` from now. `None`: the
    /// sandbox runs until it's deleted.
    SetTtl {
        id: String,
        ttl_secs: Option<u64>,
    },
    Get {
        id: String,
    },
    List,
    Report,
    /// Boots the template once, waits until envd is ready and snapshots
    /// it, so new sandboxes of this size restore instead of booting.
    #[serde(rename_all = "camelCase")]
    SnapshotTemplate {
        template: String,
        cpus: u32,
        memory_mib: u32,
        disk_mib: u32,
        /// With a network interface or without. Left out, it's `internet`
        /// on a server with networking and `none` elsewhere.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        network: Option<NetworkMode>,
    },
    /// Serves a sandbox port on the edge proxy. Opening an open port again
    /// changes whether it's public.
    OpenPort {
        id: String,
        port: u16,
        #[serde(default = "yes")]
        public: bool,
    },
    ClosePort {
        id: String,
        port: u16,
    },
    /// Puts the edge proxy's certificate and key in place (PEM) and serves
    /// them at once, without a restart. Admin only.
    SetEdgeCert {
        cert: String,
        key: SecretPem,
    },
    /// Makes a template from an OCI image: pulls it, unpacks its layers
    /// into a root disk with `pyo-envd`, then on Firecracker boots it once
    /// and snapshots it.
    /// Answers at once. The report shows how far it got. A template the
    /// server has already is left as it is.
    #[serde(rename_all = "camelCase")]
    PrepareTemplate {
        template: String,
        /// Like `python:3.13-alpine` or `100.64.0.1:5006/acme/app/api:1`.
        image: String,
        /// The registry speaks plain HTTP: Pyo's own, over the tailnet.
        #[serde(default)]
        plain_http: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        auth: Option<RegistryAuth>,
        /// The most its files may take, unpacked.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        max_mib: Option<u32>,
    },
    /// Deletes a template the server made, or stops its build. Refused
    /// while sandboxes still use it.
    DeleteTemplate {
        template: String,
    },
    /// How far a template got, without the rest of the report: cheap
    /// enough to ask every second while it's made. Daemons before it
    /// refuse it, and Pyo falls back to the report.
    Template {
        template: String,
    },
    /// Runs a command in the sandbox and waits for it, as
    /// `POST /v1/sandboxes/{id}/exec` does. For a server that gets its
    /// commands over HTTPS ([`outbound`]), whose output is kept to
    /// [`outbound::OUTPUT_CAP`] a stream.
    Exec {
        id: String,
        request: ExecRequest,
    },
    /// A file of the sandbox, as `GET .../files` reads it. Its bytes come
    /// back base64 ([`DaemonResult::File`]), at most [`outbound::FILE_CAP`].
    ReadFile {
        id: String,
        path: String,
    },
    /// Writes a file in the sandbox, as `PUT .../files` does. `data` is
    /// base64, at most [`outbound::FILE_CAP`] once decoded.
    WriteFile {
        id: String,
        path: String,
        data: String,
    },
}

/// A private key in a command: never printed.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SecretPem(pub String);

impl std::fmt::Debug for SecretPem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("<hidden>")
    }
}

/// A certificate's fingerprint: the SHA-256 of its DER, in lowercase hex.
pub fn cert_fingerprint(der: &[u8]) -> String {
    use sha2::Digest;
    sha2::Sha256::digest(der)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn yes() -> bool {
    true
}

impl DaemonCommand {
    /// The sandbox the command is about, if any.
    pub fn sandbox_id(&self) -> Option<&str> {
        match self {
            DaemonCommand::Create { spec } => Some(&spec.id),
            DaemonCommand::Delete { id }
            | DaemonCommand::Pause { id }
            | DaemonCommand::Resume { id }
            | DaemonCommand::SetTtl { id, .. }
            | DaemonCommand::OpenPort { id, .. }
            | DaemonCommand::ClosePort { id, .. }
            | DaemonCommand::Get { id }
            | DaemonCommand::Exec { id, .. }
            | DaemonCommand::ReadFile { id, .. }
            | DaemonCommand::WriteFile { id, .. } => Some(id),
            DaemonCommand::List
            | DaemonCommand::Report
            | DaemonCommand::SnapshotTemplate { .. }
            | DaemonCommand::SetEdgeCert { .. }
            | DaemonCommand::PrepareTemplate { .. }
            | DaemonCommand::DeleteTemplate { .. }
            | DaemonCommand::Template { .. } => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum DaemonResult {
    Sandbox {
        sandbox: SandboxStatus,
    },
    Deleted {
        id: String,
    },
    Sandboxes {
        sandboxes: Vec<SandboxStatus>,
    },
    Report {
        report: Box<ServerReport>,
    },
    #[serde(rename_all = "camelCase")]
    TemplateSnapshot {
        template: String,
        /// Firecracker version, CPU template and size. A sandbox restores
        /// from the snapshot with the same key.
        key: String,
        took_ms: u64,
    },
    /// The edge serves this certificate now.
    EdgeCert {
        fingerprint: String,
    },
    Template {
        template: TemplateReport,
    },
    TemplateDeleted {
        template: String,
    },
    /// An [`DaemonCommand::Exec`]'s answer.
    Exec {
        result: ExecResult,
    },
    /// A [`DaemonCommand::ReadFile`]'s answer: the file, base64.
    File {
        data: String,
    },
    /// A [`DaemonCommand::WriteFile`]'s answer: the bytes written.
    Written {
        bytes: u64,
    },
    Error {
        error: DaemonError,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ErrorCode {
    /// No token, a bad one or one that ran out.
    Unauthorized,
    /// The token doesn't allow the call.
    Forbidden,
    /// The sandbox itself refused, like a file its user can't read or
    /// write. Also 403, but the token is fine: a new one won't help.
    PermissionDenied,
    NotFound,
    Conflict,
    InvalidSpec,
    /// Not enough memory or CPU left on the server.
    NoCapacity,
    /// The server can't do this, like `isolation: vm` without KVM.
    Unsupported,
    /// The VM or envd failed.
    Runtime,
    Internal,
}

impl ErrorCode {
    pub fn http_status(self) -> u16 {
        match self {
            ErrorCode::Unauthorized => 401,
            ErrorCode::Forbidden | ErrorCode::PermissionDenied => 403,
            ErrorCode::NotFound => 404,
            ErrorCode::Conflict => 409,
            ErrorCode::InvalidSpec => 422,
            ErrorCode::NoCapacity => 503,
            ErrorCode::Unsupported => 501,
            ErrorCode::Runtime => 502,
            ErrorCode::Internal => 500,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DaemonError {
    pub code: ErrorCode,
    pub message: String,
}

impl DaemonError {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        DaemonError {
            code,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for DaemonError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for DaemonError {}

// ── Exec over REST ─────────────────────────────────────────────────────────

/// `POST /v1/sandboxes/{id}/exec`: run a command and wait for it. For
/// streaming, stdin and PTYs, clients use the WebSocket, which speaks
/// [`wire`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExecRequest {
    pub cmd: wire::Cmd,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Sent to the command's stdin, then stdin is closed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stdin: Option<String>,
    /// The command is killed after this long. 60 seconds when left out.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExecResult {
    /// None when a signal ended the command.
    pub exit_code: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signal: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    /// Output past the cap was dropped.
    #[serde(default)]
    pub truncated: bool,
    #[serde(default)]
    pub timed_out: bool,
    pub duration_ms: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_contracts() {
        assert_eq!(contract_refusal("1"), None);
        assert_eq!(contract_refusal(" 1\n"), None);
        let two = contract_refusal("2").unwrap();
        assert!(
            two.contains("contract 2") && two.contains("runs contract 1"),
            "{two}"
        );
        let bad = contract_refusal("one").unwrap();
        assert!(bad.contains("isn't a contract number"), "{bad}");
        for m in [two, bad] {
            assert!(!m.contains(';'), "{m}");
        }
    }

    fn spec() -> SandboxSpec {
        SandboxSpec::new("sbx_abcdefghijklmnop", "team1", "node-22")
    }

    #[test]
    fn spec_defaults_from_minimal_json() {
        let s: SandboxSpec = serde_json::from_str(
            r#"{"id":"sbx_abcdefghijklmnop","team":"t","template":"node-22"}"#,
        )
        .unwrap();
        assert_eq!(s.cpus, DEFAULT_CPUS);
        assert_eq!(s.memory_mib, DEFAULT_MEMORY_MIB);
        assert_eq!(s.ttl_secs, Some(DEFAULT_TTL_SECS));
        assert_eq!(s.network, NetworkMode::Internet);
        let s: SandboxSpec = serde_json::from_str(
            r#"{"id":"sbx_abcdefghijklmnop","team":"t","template":"node-22","ttlSecs":null}"#,
        )
        .unwrap();
        assert_eq!(s.ttl_secs, None, "null is no lifetime");
        s.validate().unwrap();
        assert_eq!(s.isolation, Isolation::Auto);
        s.validate().unwrap();
    }

    #[test]
    fn spec_json_is_camel_case() {
        let mut s = spec();
        s.network = NetworkMode::None;
        s.isolation = Isolation::Vm;
        let v = serde_json::to_value(&s).unwrap();
        assert_eq!(v["memoryMib"], 512);
        assert_eq!(v["ttlSecs"], 300);
        assert_eq!(v["network"], "none");
        assert_eq!(v["isolation"], "vm");
        assert!(v.get("pauseWhenIdleSecs").is_none());
    }

    #[test]
    fn spec_validation() {
        spec().validate().unwrap();
        let bad: &[fn(&mut SandboxSpec)] = &[
            |s| s.id = "abc".into(),
            |s| s.id = "sbx_UPPERCASEUPPER".into(),
            |s| s.team.clear(),
            |s| s.template = "../etc".into(),
            |s| s.cpus = 0,
            |s| s.cpus = MAX_CPUS + 1,
            |s| s.memory_mib = 64,
            |s| s.disk_mib = 1,
            |s| s.ttl_secs = Some(0),
            |s| s.ttl_secs = Some(MAX_TTL_SECS + 1),
            |s| {
                s.env.insert("1BAD".into(), "x".into());
            },
            |s| {
                s.env.insert("A=B".into(), "x".into());
            },
            |s| {
                s.labels.insert("".into(), "x".into());
            },
            |s| {
                s.labels.insert("k".into(), "x".repeat(300));
            },
        ];
        for (i, f) in bad.iter().enumerate() {
            let mut s = spec();
            f(&mut s);
            assert!(s.validate().is_err(), "case {i} should fail");
        }
    }

    #[test]
    fn sandbox_ids() {
        let id = sandbox_id_from([0xab; 16]);
        assert_eq!(id.len(), 4 + 26);
        assert!(is_sandbox_id(&id), "{id}");
        assert_ne!(id, sandbox_id_from([0xac; 16]));
        assert!(!is_sandbox_id("sbx_short"));
        assert!(!is_sandbox_id("sbx_abc/../../etcabc"));
    }

    #[test]
    fn template_names() {
        assert!(is_template_name("node-22"));
        assert!(is_template_name("python3.13"));
        assert!(!is_template_name(".hidden"));
        assert!(!is_template_name("a/b"));
        assert!(!is_template_name(""));
    }

    #[test]
    fn long_and_endless_lifetimes() {
        assert_eq!(check_ttl(None), Ok(()));
        assert_eq!(check_ttl(Some(1)), Ok(()));
        assert_eq!(check_ttl(Some(MAX_TTL_SECS)), Ok(()));
        assert!(check_ttl(Some(0)).is_err());
        assert!(check_ttl(Some(MAX_TTL_SECS + 1)).is_err());
        assert_eq!(parse_ttl("365d"), Some(MAX_TTL_SECS));
        assert_eq!(parse_lifetime("none"), Some(None));
        assert_eq!(parse_lifetime(" None "), Some(None));
        assert_eq!(parse_lifetime("30d"), Some(Some(30 * 86400)));
        assert_eq!(parse_lifetime("never"), None);
        assert_eq!(expiry(1_000, None), None);
        assert_eq!(
            expiry(1_000, Some(MAX_TTL_SECS)),
            Some(1_000 + MAX_TTL_SECS * 1000)
        );
        assert_eq!(expiry(u64::MAX, Some(10)), Some(u64::MAX));
    }

    #[test]
    fn ttl_parsing() {
        assert_eq!(parse_ttl("90"), Some(90));
        assert_eq!(parse_ttl("90s"), Some(90));
        assert_eq!(parse_ttl("30m"), Some(1800));
        assert_eq!(parse_ttl("1h"), Some(3600));
        assert_eq!(parse_ttl("1h30m"), Some(5400));
        assert_eq!(parse_ttl("1d"), Some(86400));
        assert_eq!(parse_ttl("1x"), None);
        assert_eq!(parse_ttl("h"), None);
        assert_eq!(parse_ttl("10m5"), None);
        assert_eq!(parse_ttl(""), None);
    }

    #[test]
    fn boot_classification_uses_the_threshold() {
        assert_eq!(classify_boot(73), Kvm::Native);
        assert_eq!(classify_boot(NATIVE_BOOT_MS - 1), Kvm::Native);
        assert_eq!(classify_boot(NATIVE_BOOT_MS), Kvm::Nested);
        // Without the fast boot args a native boot takes about 600 ms, which
        // is why every boot uses them.
        assert_eq!(classify_boot(600), Kvm::Nested);
        assert!(FAST_BOOT_ARGS.contains("i8042.noaux"));
        assert!(FAST_BOOT_ARGS.contains("8250.nr_uarts=0"));
    }

    #[test]
    fn capability_report_shape_and_summary() {
        let r = CapabilityReport {
            runtime: Runtime::Firecracker,
            kvm: Kvm::Native,
            boot_ms: Some(73),
            arch: "x86_64".into(),
            cpus: 22,
            memory: 30 << 30,
            disk: 100 << 30,
            ..Default::default()
        };
        let v = serde_json::to_value(&r).unwrap();
        for k in ["runtime", "kvm", "bootMs", "arch", "cpus", "memory", "disk"] {
            assert!(v.get(k).is_some(), "missing {k}");
        }
        assert_eq!(v["runtime"], "firecracker");
        assert_eq!(v["kvm"], "native");
        assert!(r.summary().contains("73 ms"));
        assert!(!r.summary().contains(';'));
        assert!(r.allows(Isolation::Vm));

        let g = CapabilityReport {
            runtime: Runtime::Gvisor,
            ..Default::default()
        };
        assert!(g.allows(Isolation::Auto));
        assert!(!g.allows(Isolation::Vm));
        let none = CapabilityReport {
            reason: Some("No KVM and no gVisor.".into()),
            ..Default::default()
        };
        assert!(!none.allows(Isolation::Auto));
        assert_eq!(none.summary(), "No KVM and no gVisor.");
    }

    #[test]
    fn commands_round_trip() {
        let cmds = vec![
            DaemonCommand::Create { spec: spec() },
            DaemonCommand::Delete {
                id: "sbx_abcdefghijklmnop".into(),
            },
            DaemonCommand::SetTtl {
                id: "sbx_abcdefghijklmnop".into(),
                ttl_secs: Some(60),
            },
            DaemonCommand::SetTtl {
                id: "sbx_abcdefghijklmnop".into(),
                ttl_secs: None,
            },
            DaemonCommand::List,
            DaemonCommand::SnapshotTemplate {
                template: "node-22".into(),
                cpus: 1,
                memory_mib: 512,
                disk_mib: 2048,
                network: Some(NetworkMode::None),
            },
            DaemonCommand::PrepareTemplate {
                template: "stp_abc".into(),
                image: "python:3.13-alpine".into(),
                plain_http: false,
                auth: Some(RegistryAuth {
                    username: "pyo-sandbox".into(),
                    password: "secret".into(),
                }),
                max_mib: Some(1024),
            },
            DaemonCommand::DeleteTemplate {
                template: "stp_abc".into(),
            },
            DaemonCommand::Template {
                template: "stp_abc".into(),
            },
        ];
        for c in cmds {
            let j = serde_json::to_string(&c).unwrap();
            assert_eq!(serde_json::from_str::<DaemonCommand>(&j).unwrap(), c);
        }
        let j = serde_json::to_value(DaemonCommand::Pause { id: "x".into() }).unwrap();
        assert_eq!(j, serde_json::json!({"type": "pause", "id": "x"}));
        assert_eq!(DaemonCommand::List.sandbox_id(), None);

        let e = DaemonResult::Error {
            error: DaemonError::new(ErrorCode::NoCapacity, "full"),
        };
        let j = serde_json::to_value(&e).unwrap();
        assert_eq!(j["error"]["code"], "noCapacity");
        assert_eq!(ErrorCode::NoCapacity.http_status(), 503);
    }

    #[test]
    fn port_hosts() {
        let id = "sbx_abcdefghijklmnop";
        let h = port_host(3000, id, "sbx.example.com");
        assert_eq!(h, "3000-sbx-abcdefghijklmnop.sbx.example.com");
        let want = Some((3000, id.to_string()));
        assert_eq!(parse_port_host(&h, "sbx.example.com"), want);
        assert_eq!(
            parse_port_host(
                "3000-SBX_abcdefghijklmnop.Sbx.Example.com:443",
                "sbx.example.com."
            ),
            want
        );
        for bad in [
            "3000-sbx-abcdefghijklmnop.other.com",
            "sbx-abcdefghijklmnop.sbx.example.com",
            "0-sbx-abcdefghijklmnop.sbx.example.com",
            "03000-sbx-abcdefghijklmnop.sbx.example.com",
            "70000-sbx-abcdefghijklmnop.sbx.example.com",
            "3000-sbx-short.sbx.example.com",
            "3000-sbx-abcdefghijklmnop.x.sbx.example.com",
            "sbx.example.com",
        ] {
            assert_eq!(parse_port_host(bad, "sbx.example.com"), None, "{bad}");
        }
        let cap = CapabilityReport {
            edge_url: Some("https://sbx.example.com".into()),
            ..Default::default()
        };
        assert_eq!(
            cap.port_url(id, 8080).unwrap(),
            "https://8080-sbx-abcdefghijklmnop.sbx.example.com"
        );
        let local = CapabilityReport {
            edge_url: Some("http://sbx.test:8081/".into()),
            ..Default::default()
        };
        assert_eq!(
            local.port_url(id, 1).unwrap(),
            "http://1-sbx-abcdefghijklmnop.sbx.test:8081"
        );
        assert_eq!(CapabilityReport::default().port_url(id, 1), None);
        assert_eq!(cap.edge_domain(), Some("sbx.example.com"));
        assert_eq!(local.edge_domain(), Some("sbx.test"));
        assert_eq!(CapabilityReport::default().edge_domain(), None);
        let set = DaemonCommand::SetEdgeCert {
            cert: "C".into(),
            key: SecretPem("K".into()),
        };
        assert!(
            !format!("{set:?}").contains('K'),
            "the key is never printed"
        );
        assert_eq!(
            serde_json::to_value(&set).unwrap(),
            serde_json::json!({"type": "setEdgeCert", "cert": "C", "key": "K"})
        );
        assert_eq!(
            cert_fingerprint(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        let open: DaemonCommand =
            serde_json::from_str(r#"{"type":"openPort","id":"x","port":3000}"#).unwrap();
        assert_eq!(
            open,
            DaemonCommand::OpenPort {
                id: "x".into(),
                port: 3000,
                public: true
            }
        );
    }

    #[test]
    fn template_snapshots_by_size() {
        let t = TemplateReport {
            name: "node-22".into(),
            snapshots: vec![
                format!("fc1.18.0-none{}", snapshot_size_tag(1, 512, 2048)),
                format!(
                    "fc1.18.0-none{SNAPSHOT_NET_TAG}{}",
                    snapshot_size_tag(2, 512, 2048)
                ),
            ],
            ..Default::default()
        };
        let (off, on) = (NetworkMode::None, NetworkMode::Internet);
        assert!(t.has_snapshot(off, 1, 512, 2048));
        assert!(!t.has_snapshot(on, 1, 512, 2048));
        assert!(t.has_snapshot(on, 2, 512, 2048));
        assert!(!t.has_snapshot(off, 2, 512, 2048));
        assert!(!t.has_snapshot(off, 1, 512, 20480));
        let r: ServerReport = serde_json::from_value(serde_json::json!({
            "protocol": 1, "daemonVersion": "0.4.0", "serverId": "s",
            "capability": CapabilityReport::default(), "capacity": CapacityReport::default(),
            "sandboxes": [],
        }))
        .unwrap();
        assert!(r.templates.is_empty(), "older reports have no templates");
        let hand: TemplateReport =
            serde_json::from_value(serde_json::json!({"name": "busybox"})).unwrap();
        assert_eq!(
            hand.state,
            TemplateState::Ready,
            "a template put there by hand is ready"
        );
        assert!(hand.build.is_none());
        let auth = RegistryAuth {
            username: "u".into(),
            password: "hunter2".into(),
        };
        assert!(!format!("{auth:?}").contains("hunter2"));
    }

    #[test]
    fn capacity_headroom_never_underflows() {
        let c = CapacityReport {
            memory_available: 1,
            memory_reserved: 10,
            ..Default::default()
        };
        assert_eq!(c.headroom(), 0);
    }
}
