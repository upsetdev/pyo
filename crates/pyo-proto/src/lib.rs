//! Wire types between the Pyo API and the in-cluster agent.
//!
//! The agent always dials out: it long-polls the API for commands, posts
//! heartbeats with what it observes, and streams deployment progress back.
//! No kubeconfig or cluster credential ever leaves the cluster. JSON, camelCase,
//! timestamps in epoch milliseconds (same as the dashboard).

use serde::{Deserialize, Serialize};

pub mod ansi;
pub mod bundles;
pub mod database_names;
pub mod dbimport;
pub mod firewall;
pub mod flows;
pub mod memory;
pub mod rbac;
pub mod system;

/// Bumped on breaking changes; the API rejects agents it doesn't speak.
pub const PROTOCOL_VERSION: u32 = 1;

/// Header carrying the agent's bearer token is `Authorization: Bearer <token>`.
pub const AGENT_VERSION_HEADER: &str = "x-pyo-agent-version";

/// Random per-process id. A new one on a poll means the agent restarted, so
/// rollouts the previous process was watching are handed out again.
pub const AGENT_INSTANCE_HEADER: &str = "x-pyo-agent-instance";

// ── Heartbeat: what the agent sees ─────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct Heartbeat {
    pub protocol: u32,
    pub agent_version: String,
    pub kubernetes_version: String,
    pub servers: Vec<ServerReport>,
    pub workloads: Vec<WorkloadReport>,
    pub services: Vec<ServiceRuntime>,
    pub volumes: Vec<VolumeReport>,
    pub capabilities: Capabilities,
    /// TLS certificates of service domains (cert-manager).
    #[serde(default)]
    pub certificates: Vec<CertificateReport>,
    /// Proof that this is the cluster Pyo runs on ("This machine"): what
    /// [`host_proof`] makes of the host's bootstrap key (its
    /// pyo-system/pyo-bootstrap Secret, only there) and this agent's token.
    /// Pyo checks it; an agent's word alone counts for nothing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_proof: Option<String>,
    /// [`UPDATES_VERSION`] when the agent can update Pyo and itself
    /// (`UpgradePyo`, `UpdateAgent`); 0 from agents older than that.
    #[serde(default)]
    pub updates: u32,
    /// What the agent may do in this cluster, and the customer's switches.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<rbac::AgentScope>,
    /// The system bundles it finds installed.
    #[serde(default)]
    pub bundles: Vec<bundles::BundleReport>,
    /// Databases (Pyo's Database objects) and how
    /// they're doing.
    #[serde(default)]
    pub databases: Vec<DatabaseReport>,
    /// `databases` is the cluster's whole list (it was read), so a database
    /// missing from it is gone; false from older agents and when listing
    /// failed.
    #[serde(default)]
    pub databases_listed: bool,
    /// Each database member's usage and engine figures (database
    /// analytics); none from older agents.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub database_metrics: Vec<DatabaseMemberMetrics>,
}

/// The figures a database member can report in
/// [`DatabaseMemberMetrics::values`] (and the API stores): rates per second
/// for transactions, queries, pooler_queries, commands, cache_hits,
/// cache_misses, key_hits, key_misses, evictions, reads_replica,
/// reads_primary and replica_fallbacks; the rest as read.
pub const DATABASE_METRIC_KEYS: &[&str] = &[
    "transactions",
    "queries",
    "pooler_queries",
    "commands",
    "connections",
    "connections_active",
    "max_connections",
    "pooler_active",
    "pooler_waiting",
    "pooler_servers_active",
    "pooler_servers_idle",
    "pooler_maxwait_seconds",
    "cache_hits",
    "cache_misses",
    "key_hits",
    "key_misses",
    "evictions",
    "lag_seconds",
    "size_bytes",
    "memory_used_bytes",
    "maxmemory_bytes",
    "keys",
    "reads_replica",
    "reads_primary",
    "replica_fallbacks",
];

/// One part of a database (an instance, read replica, pooler, KV's
/// endpoint) as the agent last measured it: its pod's CPU, memory and
/// volumes, and what its metrics endpoint said, as named figures (rates per
/// second for counters). The names are the agent's own (dbmetrics.rs), not
/// the exporters'.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct DatabaseMemberMetrics {
    pub namespace: String,
    /// The Database's name.
    pub database: String,
    /// The pod.
    pub member: String,
    /// "instance", "read", "pooler", "read-pooler", "proxy" (KV's
    /// endpoint) or "sentinel".
    pub component: String,
    /// "primary" or "replica" for instances; "" otherwise.
    #[serde(default)]
    pub role: String,
    /// The server it runs on.
    #[serde(default)]
    pub server: String,
    /// Cores.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_mi: Option<f64>,
    /// Its persistent volumes, summed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disk_used_gi: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disk_capacity_gi: Option<f64>,
    /// From its metrics endpoint (absent: none, or it didn't answer).
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub values: std::collections::BTreeMap<String, f64>,
}

/// A database as the cluster has it: where, and its status as
/// pyo-db-controller writes it (phase, instances, primary,
/// endpoints, failovers).
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct DatabaseReport {
    pub namespace: String,
    pub name: String,
    /// The Pyo database id it was made for (its `pyo.dev/database-id`
    /// label); empty when unlabelled or from older agents.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub id: String,
    #[serde(default)]
    pub status: serde_json::Value,
}

/// A database for the agent to create or change: its namespace (a
/// managed one, so the databases controller may work in it), its Database
/// object, and its login Secret, written by Pyo so it keeps the passwords
/// (sealed with the team's key) itself.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DatabaseSpec {
    pub namespace: String,
    pub name: String,
    /// The Database object (pyo.dev/v1alpha1), as applied.
    pub database: serde_json::Value,
    /// The Secret's name and type ("kubernetes.io/basic-auth" or "Opaque").
    pub secret_name: String,
    pub secret_type: String,
    /// The Secret's keys and values. Called env so they're sealed at rest
    /// in the command queue like services' variables.
    pub env: Vec<EnvVar>,
    /// More Secrets beside it, e.g. the backup storage's keys
    /// (accessKeyId, secretAccessKey); sealed at rest the same way.
    #[serde(default)]
    pub secrets: Vec<NamedSecret>,
    /// Secrets of it to delete (an import's, once it's done): only ones
    /// labelled as this database's and Pyo's.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub remove_secrets: Vec<String>,
    /// The team that owns it: only its projects' namespaces (labelled
    /// [`TEAM_LABEL`]) may reach the databases namespace.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub team_id: String,
}

/// A namespace label naming the team that owns a project's namespace (and
/// a databases namespace): its databases let only that team's projects in.
pub const TEAM_LABEL: &str = "pyo/team-id";

/// A team's databases namespace: `pyo-db-<team>` (for team `team_<id>`).
pub fn databases_namespace(team_id: &str) -> String {
    let id: String = team_id
        .trim_start_matches("team_")
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    format!("pyo-db-{}", id.trim_matches('-'))
        .chars()
        .take(63)
        .collect()
}

/// A Secret by name, its keys as `env`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NamedSecret {
    pub name: String,
    pub env: Vec<EnvVar>,
}

/// What an agent that can run in-dashboard updates reports in `updates`.
pub const UPDATES_VERSION: u32 = 1;

/// A server that only runs builds: join.sh --builder labels and taints it,
/// and builder pods tolerate the taint.
pub const BUILDER_NODE_LABEL: &str = "pyo/builder";

/// The Secret (in pyo-system) with the host's bootstrap key, on the cluster
/// Pyo runs on only (pyo-up.sh, install.sh).
pub const BOOTSTRAP_SECRET: &str = "pyo-bootstrap";

/// An agent's proof that it runs on the cluster Pyo runs on: HMAC-SHA256
/// of its token's hash (`sha256_hex(token)`, which Pyo keeps) under the
/// host's bootstrap key, in hex. Bound to the token, so it's no use to
/// another cluster.
pub fn host_proof(bootstrap_key: &str, token_hash: &str) -> String {
    use sha2::{Digest, Sha256};
    const BLOCK: usize = 64;
    let mut key = [0u8; BLOCK];
    if bootstrap_key.len() > BLOCK {
        key[..32].copy_from_slice(&Sha256::digest(bootstrap_key.as_bytes()));
    } else {
        key[..bootstrap_key.len()].copy_from_slice(bootstrap_key.as_bytes());
    }
    let pad = |b: u8| key.iter().map(|k| k ^ b).collect::<Vec<u8>>();
    let inner = Sha256::new()
        .chain_update(pad(0x36))
        .chain_update(b"pyo-host-proof\n")
        .chain_update(token_hash.as_bytes())
        .finalize();
    let outer = Sha256::new()
        .chain_update(pad(0x5c))
        .chain_update(inner)
        .finalize();
    outer.iter().map(|b| format!("{b:02x}")).collect()
}

/// A token as Pyo keeps it: SHA-256, in hex.
pub fn token_hash(token: &str) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(token.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Whether an API URL is an in-cluster address (the cluster Pyo runs on).
pub fn in_cluster_url(url: &str) -> bool {
    let host = url
        .split("://")
        .nth(1)
        .unwrap_or(url)
        .split(['/', ':'])
        .next()
        .unwrap_or("");
    host.ends_with(".svc") || host.ends_with(".svc.cluster.local")
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CertificateReport {
    pub service_id: String,
    pub hostname: String,
    pub ready: bool,
    /// Why it isn't ready, from cert-manager.
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct Capabilities {
    /// The gateway: "envoy", or "none" until it answers.
    pub ingress_controller: String,
    /// Why the gateway failed to install (Helm's error, shortened), while
    /// it isn't answering; none while it's on its way or running.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gateway_error: Option<String>,
    /// "servicelb", "metallb" or "none".
    pub load_balancer: String,
    pub load_balancer_ips: Vec<String>,
    pub cert_manager: bool,
    pub metrics_server: bool,
    pub storage_classes: Vec<StorageClassReport>,
    pub pod_cidr: String,
    pub service_cidr: String,
    /// For the dashboard's security overview.
    #[serde(default)]
    pub security: SecurityReport,
    /// NVIDIA GPU support (servers, RuntimeClass, device plugin).
    #[serde(default)]
    pub gpu: GpuReport,
    /// The agent routes requests within a region (and spreads replicas
    /// across regions).
    #[serde(default)]
    pub regions: bool,
    /// The agent can switch a cluster's traffic mode (`SetTrafficMode`).
    #[serde(default)]
    pub traffic_modes: bool,
    /// The agent knows the "local" mode ([`TRAFFIC_LOCAL`]).
    #[serde(default)]
    pub traffic_local: bool,
    /// Tailscale addresses where websites (ports 80 and 443) are answered
    /// whether or not the cluster takes public traffic: the servers' the
    /// agent publishes them on (system.rs `TAILNET_WEB_SERVICE`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tailnet_web_ips: Vec<String>,
    /// Services (by id) whose WAF policy the gateway refused: their sites
    /// go unfiltered (the policy isn't in effect at all, whatever the
    /// service's mode), so the WAF isn't active there.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub waf_rejected: Vec<String>,
    /// Services (by id) whose WAF policy the gateway accepted: their WAF
    /// is in effect. One with its WAF on that isn't here yet, while a
    /// firewall sync is on its way, is still starting.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub waf_active: Vec<String>,
}

/// What the agent can see of the cluster's protections.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct SecurityReport {
    /// k3s Secrets encryption at rest: its provider ("secretbox", "aescbc"),
    /// "" when off, or none when the control plane doesn't say (not k3s).
    pub secrets_encryption: Option<String>,
    /// Namespaces of Pyo projects.
    pub namespaces: u32,
    /// Of those: enforcing a Pod Security Standard (baseline or restricted)...
    pub pod_security_enforced: u32,
    /// ...and with every running pod meeting the restricted one.
    pub pod_security_restricted: u32,
    /// Of those: isolated by Pyo's default-deny network policy.
    pub network_isolated: u32,
}

/// Pyo's default-deny NetworkPolicy in an isolated project's namespace.
pub const ISOLATION_POLICY: &str = "pyo-isolation";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StorageClassReport {
    pub name: String,
    pub provisioner: String,
    pub is_default: bool,
    pub reclaim_policy: String,
    pub volume_binding_mode: String,
    /// Its volumes can grow (`allowVolumeExpansion`).
    #[serde(default)]
    pub allow_volume_expansion: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerReport {
    pub name: String,
    /// "control-plane", "etcd", "worker".
    pub roles: Vec<String>,
    /// "ready", "not_ready" or "unknown".
    pub status: String,
    pub schedulable: bool,
    /// Set by Pyo's maintenance action (cordon plus an annotation).
    pub maintenance: bool,
    pub kubelet_version: String,
    pub os_image: String,
    pub arch: String,
    pub container_runtime: String,
    // The dashboard spells these internalIP / externalIP.
    #[serde(rename = "internalIP")]
    pub internal_ip: String,
    #[serde(rename = "externalIP")]
    pub external_ip: Option<String>,
    pub capacity: ServerCapacity,
    pub usage: ServerUsage,
    /// What Kubernetes can hand out to pods here (capacity less what the
    /// system keeps).
    #[serde(default)]
    pub allocatable: ServerRoom,
    /// What running and starting pods here ask for: the scheduler places a
    /// new one only where its requests fit in allocatable less this.
    #[serde(default)]
    pub requested: ServerRoom,
    pub pod_count: u32,
    pub conditions: Vec<ServerCondition>,
    pub created_at: i64,
    /// The server's public address and country, as the internet sees it
    /// (found by the agent; a tailnet IP says nothing about where it is).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub public_ip: Option<String>,
    /// Its public IPv4 and IPv6 address, each as the internet sees it, when
    /// it has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub public_ipv4: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub public_ipv6: Option<String>,
    /// Every address the cluster lists for it (internal and external, both
    /// families); `internalIP` and `externalIP` are the first of each.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub addresses: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub public_country: Option<String>,
    /// Its Tailscale IPv4 address (100.x), when it's on a tailnet.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tailnet_ip: Option<String>,
    /// Public IPv4 addresses on its own network cards: the internet reaches
    /// it there directly (not behind NAT).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub nic_public_ips: Vec<String>,
    /// NVIDIA GPUs Kubernetes can hand out here (allocatable nvidia.com/gpu).
    #[serde(default)]
    pub gpus: u32,
    /// The server has an NVIDIA GPU set up for containers ([`GPU_LABEL`]),
    /// whether or not the device plugin counts it yet.
    #[serde(default)]
    pub gpu_detected: bool,
    /// e.g. "NVIDIA GeForce RTX 4090", from [`GPU_MODEL_LABEL`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gpu_model: Option<String>,
    /// GPUs running pods ask for here.
    #[serde(default)]
    pub gpus_used: u32,
    /// A build-only server ([`BUILDER_NODE_LABEL`]): it never runs the
    /// gateway.
    #[serde(default)]
    pub builder: bool,
    /// Kept for GPU work: it has the [`GPU_TAINT`] taint.
    #[serde(default)]
    pub gpu_dedicated: bool,
    /// Each GPU's memory, from [`GPU_MEMORY_LABEL`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gpu_memory_mib: Option<u32>,
    /// "" (no NVIDIA GPU), "ready", "starting" (the device plugin hasn't
    /// counted it), "no-driver", "no-toolkit", or "no-runtime" (k3s hasn't
    /// picked up the toolkit: restart it).
    #[serde(default)]
    pub gpu_status: String,
}

/// On a GPU server kept for GPU work (`nvidia.com/gpu=present:NoSchedule`):
/// only pods that tolerate it run there. GPU services, the agent, its log
/// collector, builders, backups and the device plugin do.
pub const GPU_TAINT: &str = "nvidia.com/gpu";

/// The toleration for [`GPU_TAINT`], as a Kubernetes object.
pub fn gpu_toleration() -> serde_json::Value {
    serde_json::json!({ "key": GPU_TAINT, "operator": "Exists", "effect": "NoSchedule" })
}

/// Whether a GPU server is kept for GPU work (tainted) or also runs other
/// services, as Pyo has it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct GpuPolicy {
    pub server: String,
    pub dedicated: bool,
}

/// On a server whose NVIDIA GPU containers can use (driver and NVIDIA
/// Container Toolkit, so k3s has the `nvidia` runtime): `nvidia`. Set by
/// connect.sh/join.sh, or by the agent from the node's runtime handlers.
pub const GPU_LABEL: &str = "pyo/gpu";
/// The GPU's model with dashes for spaces, e.g. "NVIDIA-GeForce-RTX-4090".
pub const GPU_MODEL_LABEL: &str = "pyo/gpu-model";
/// Each GPU's memory in MiB, e.g. "24564".
pub const GPU_MEMORY_LABEL: &str = "pyo/gpu-memory-mib";
/// The extended resource NVIDIA's device plugin hands out.
pub const GPU_RESOURCE: &str = "nvidia.com/gpu";
/// The RuntimeClass (and k3s containerd runtime) GPU pods run with.
pub const GPU_RUNTIME_CLASS: &str = "nvidia";

/// A cluster's GPU support, for the dashboard.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct GpuReport {
    /// Servers with an NVIDIA GPU set up.
    pub servers: u32,
    /// GPUs Kubernetes can hand out, across servers.
    pub total: u32,
    /// The most on one server: what one service can ask for.
    pub max_per_server: u32,
    /// GPUs running pods ask for, across servers.
    #[serde(default)]
    pub used: u32,
    /// The GPU models, e.g. ["NVIDIA GeForce RTX 4090"].
    #[serde(default)]
    pub models: Vec<String>,
    /// The `nvidia` RuntimeClass exists.
    pub runtime_class: bool,
    /// NVIDIA's device plugin: "ready", "starting", "none" (no GPU servers),
    /// or why it isn't running.
    pub device_plugin: String,
}

/// Node annotations where the agent keeps what it found.
pub const PUBLIC_IP_ANNOTATION: &str = "pyo/public-ip";
pub const PUBLIC_IPV4_ANNOTATION: &str = "pyo/public-ipv4";
pub const PUBLIC_IPV6_ANNOTATION: &str = "pyo/public-ipv6";
pub const PUBLIC_COUNTRY_ANNOTATION: &str = "pyo/public-country";
pub const LOCATED_AT_ANNOTATION: &str = "pyo/located-at";
/// Its Tailscale IPv4 address ("" when it has none).
pub const TAILNET_IPV4_ANNOTATION: &str = "pyo/tailnet-ipv4";
/// Public IPv4 addresses on its own network cards, comma-separated ("" for none).
pub const NIC_PUBLIC_IPV4_ANNOTATION: &str = "pyo/nic-public-ipv4";

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ServerCapacity {
    pub cpu: f64,
    pub memory_gi: f64,
    pub pods: u32,
}

/// CPU (cores) and memory (GiB) pods can ask for on a server.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ServerRoom {
    pub cpu: f64,
    pub memory_gi: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ServerUsage {
    /// Cores in use; 0 when metrics-server isn't available.
    pub cpu: f64,
    pub memory_gi: f64,
    // From the kubelet's stats summary; None from older agents, or when the
    // summary couldn't be read this time.
    /// The root filesystem (where the kubelet keeps pods' data and logs).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root_fs: Option<FsUsage>,
    /// The container runtime's image filesystem (often the same disk).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image_fs: Option<FsUsage>,
    /// Network bytes per second, received and sent, since the last heartbeat.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub net_rx_bps: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub net_tx_bps: Option<f64>,
}

/// A filesystem's space, in GiB.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct FsUsage {
    pub used_gi: f64,
    pub capacity_gi: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerCondition {
    #[serde(rename = "type")]
    pub kind: String,
    pub status: String,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkloadReport {
    pub name: String,
    pub namespace: String,
    /// "Deployment", "StatefulSet", "DaemonSet".
    pub kind: String,
    /// "healthy", "progressing", "degraded", "failed", "stopped".
    pub status: String,
    pub desired: i32,
    pub ready: i32,
    pub cpu_millis: f64,
    pub memory_mi: f64,
    pub service_id: Option<String>,
    pub system: bool,
    /// Servers its running pods are on.
    pub servers: Vec<String>,
    pub created_at: i64,
}

/// Runtime state of a service Pyo manages, keyed by the service id label.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServiceRuntime {
    pub service_id: String,
    /// Its internal address inside the cluster (may change if it's made
    /// again); absent from older agents and services without ports.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cluster_ip: Option<String>,
    pub desired: i32,
    pub ready_replicas: i32,
    pub restarts: i32,
    pub cpu_millis: f64,
    pub memory_mi: f64,
    /// When the oldest ready pod started.
    pub since: Option<i64>,
    /// Each replica, for picking one to open a terminal in.
    #[serde(default)]
    pub replicas: Vec<ReplicaReport>,
    /// Replicas no server has room for yet: they start when there is.
    #[serde(default, skip_serializing_if = "none_waiting")]
    pub waiting: i32,
    /// A CronJob's latest run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_run: Option<CronRun>,
    /// The service's own cron jobs.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub cron_jobs: Vec<CronTaskReport>,
    /// Recent runs: its own if it's a cron job, or its cron jobs'.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub runs: Vec<CronRunReport>,
    /// Its replicas' totals, from the kubelet (None: older agent, or not read):
    /// ephemeral storage plus its volumes' used space, in MiB...
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub storage_mi: Option<f64>,
    /// ...and network bytes per second.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub net_rx_bps: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub net_tx_bps: Option<f64>,
}

/// One run of a cron job, as the cluster saw it.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CronRunReport {
    /// The Job's name: unique per run.
    pub job: String,
    /// Which of a service's cron jobs; None for a service that is one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cron_id: Option<String>,
    pub started_at: i64,
    pub finished_at: Option<i64>,
    /// None while it's running.
    pub succeeded: Option<bool>,
    pub exit_code: Option<i32>,
    /// Why it ended, when Kubernetes says: OOMKilled, DeadlineExceeded…
    pub reason: Option<String>,
    /// The end of what it printed, sent around the time it finishes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CronRun {
    pub started_at: i64,
    /// None while it's still running.
    pub succeeded: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReplicaReport {
    /// Pod name.
    pub name: String,
    pub server: String,
    pub ready: bool,
    pub started_at: Option<i64>,
    /// No server has room for it yet (CPU, memory or GPU).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub waiting: bool,
    // From the kubelet's stats summary (None: older agent, or not read).
    /// Its writable layer, logs and emptyDirs, in MiB.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ephemeral_mi: Option<f64>,
    /// Network bytes per second since the last heartbeat.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub net_rx_bps: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub net_tx_bps: Option<f64>,
}

/// Shells the terminal can start. `Auto` is bash when the image has it, else sh.
pub const TERMINAL_SHELLS: [&str; 5] = ["auto", "bash", "sh", "zsh", "ash"];

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VolumeReport {
    pub volume_id: String,
    /// "pending", "bound" or "error".
    pub status: String,
    pub message: Option<String>,
    /// Space used and its size, from the kubelet while a pod mounts it
    /// (None: older agent, unmounted, or not read).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<FsUsage>,
    /// Where its claim is, and its name: so a claim Pyo has no volume for
    /// can be shown (and deleted). Empty from older agents.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub namespace: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub name: String,
    /// Its requested size, in GiB (rounded up).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size_gi: Option<i64>,
    /// Its size as provisioned, in GiB (rounded up): below `size_gi`
    /// while it grows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capacity_gi: Option<i64>,
    /// The server it was provisioned on (its claim's `selected-node`),
    /// where a one-server class keeps its data; empty when not known.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub server: String,
}

// ── Commands: what the API asks for ────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CommandEnvelope {
    pub id: String,
    pub command: Command,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Command {
    /// Apply a service's manifests and report the rollout.
    Deploy(Box<DeploySpec>),
    /// Rolling restart (pod template annotation).
    Restart(ServiceRef),
    /// Scale to zero, keeping every object.
    Stop(ServiceRef),
    /// Change the number of replicas in place; pods aren't restarted.
    Scale {
        service: ServiceRef,
        replicas: i32,
    },
    /// Make a service's cron jobs match this list, built from its live pods.
    SyncCronJobs {
        service: ServiceRef,
        jobs: Vec<CronTask>,
    },
    /// Start one of a service's cron jobs now.
    RunCronJob {
        service: ServiceRef,
        id: String,
    },
    /// Turn autoscaling on, change it, or turn it off and hold at `replicas`.
    Autoscale {
        service: ServiceRef,
        autoscale: Option<Autoscale>,
        replicas: i32,
    },
    /// Remove the service's workload, Kubernetes Service, routes and secret. Volumes stay.
    DeleteService(ServiceRef),
    SetSchedulable {
        server: String,
        schedulable: bool,
    },
    /// Cordon plus a maintenance annotation. Pods are not evicted.
    SetMaintenance {
        server: String,
        on: bool,
    },
    /// Recent container output of the service's pods.
    FetchLogs {
        service: ServiceRef,
        tail_lines: i64,
    },
    /// Stop watching a rollout and report it cancelled.
    Cancel {
        deployment_id: String,
    },
    /// Run a builder (BuildKit + pyo-builder) pinned to one server.
    EnsureBuilder(Box<BuilderSpec>),
    /// Remove a builder's pod and secret.
    RemoveBuilder {
        builder_id: String,
    },
    /// Take a server out of the cluster. Without `force`: cordon it and evict
    /// its pods (the agent's own last) so they restart on other servers;
    /// refused when a volume keeps data on that server's disk. With `force`
    /// (the machine is gone): delete it, and its pods, from the cluster.
    RemoveServer {
        server: String,
        force: bool,
    },
    /// Check a database with its admin login and make (or reset) its
    /// read-only backup user; the admin login is used only for this.
    DbSetup {
        namespace: String,
        admin: DbConnection,
        password: String,
    },
    /// Back up a volume or a database (a Job in the service's namespace).
    Backup(Box<BackupSpec>),
    /// Turn a project namespace's default-deny network policy on or off.
    SetIsolation {
        namespace: String,
        isolate: bool,
    },
    /// A deleted project's namespace: delete it, only when it's that
    /// project's (see [`rbac::project_namespace_refusal`]). Kept while it
    /// still holds volumes.
    DeleteNamespace {
        namespace: String,
        project_id: String,
    },
    /// Require a client certificate from these CAs on every HTTPS domain of
    /// the cluster, or stop requiring one when `ca_pem` is empty.
    SetOriginPull {
        ca_pem: String,
        revision: String,
    },
    /// A short-lived bootstrap token a new server joins the cluster with
    /// (k3s agent `K3S_TOKEN=<id>.<secret>`).
    CreateJoinToken {
        token_id: String,
        token_secret: String,
        ttl_seconds: u64,
    },
    /// Open a shell in one of the service's running replicas: the agent dials
    /// back to `/api/agent/v1/terminals/<session_id>` over a WebSocket and
    /// relays the shell through it (see [`TerminalFrame`]).
    OpenTerminal {
        session_id: String,
        service: ServiceRef,
        /// A replica (pod) by name; any ready one when empty.
        #[serde(default)]
        pod: String,
        /// One of [`TERMINAL_SHELLS`].
        #[serde(default)]
        shell: String,
    },
    /// Only to the agent on the cluster Pyo runs on: update Pyo to a
    /// release. The agent runs the new release's upgrader as a Job in
    /// pyo-system and reports how it went.
    UpgradePyo(Box<UpgradeSpec>),
    /// Move this agent to another image (its own Deployment's). The agent
    /// restarts on it; the next heartbeat reports the new version.
    UpdateAgent {
        image: String,
    },
    /// Bring this cluster's install up to Pyo's (rbac.rs): its admission
    /// policy and the agent's image pins, once its roles are already as
    /// these. Only with the optional self-update role; the agent restarts
    /// when its pins change.
    RefreshInstall(Box<rbac::InstallParts>),
    /// Route requests the cluster's way ([`TRAFFIC_LOCAL`],
    /// [`TRAFFIC_REGION`] or [`TRAFFIC_SPREAD`]) on every service it runs,
    /// in place.
    SetTrafficMode {
        mode: String,
    },
    /// Read the visitor's address from a PROXY protocol header on the
    /// cluster's websites (ports 80 and 443), when one is sent: for a
    /// cluster behind another load balancer. Plain connections still work.
    SetWebProxyProtocol {
        on: bool,
    },
    /// Apply the firewall to every service on the cluster, in place.
    SyncFirewall(Box<FirewallSync>),
    /// Apply one service's routes (its domains, their certificates and
    /// redirects, and its proxied ports) in place, without touching its
    /// workload. The newest `routes_revision` the agent has seen wins.
    SyncRoutes(Box<FirewallTarget>),
    /// Install a system bundle (bundles.rs), or update it to this one:
    /// apply its objects, then delete `prune`, the ones the version before
    /// had and this one doesn't (only those labelled as the bundle's).
    ApplyBundle {
        bundle: Box<bundles::SystemBundle>,
        #[serde(default)]
        prune: Vec<bundles::ObjectRef>,
    },
    /// Remove a system bundle: delete these objects (only those labelled
    /// as its), its namespace last.
    RemoveBundle {
        id: String,
        objects: Vec<bundles::ObjectRef>,
    },
    /// Create or change a database (and its login Secret).
    ApplyDatabase(Box<DatabaseSpec>),
    /// Delete a database and its login Secret.
    DeleteDatabase {
        namespace: String,
        name: String,
    },
    /// Delete a volume's claim, and with it its data: only when it carries
    /// this volume id (`pyo/volume-id`) and no running pod mounts it. Done
    /// once the claim is gone (already gone is done).
    DeleteVolume {
        namespace: String,
        name: String,
        volume_id: String,
    },
    /// List a volume's snapshots in the team's restic repository (a Job in
    /// its namespace); `data` has `snapshots`, newest first.
    ListSnapshots(Box<SnapshotsSpec>),
    /// Delete one of a volume's snapshots from the team's restic repository
    /// (only one of this volume's; one already gone is done). The
    /// repository is pruned afterwards, in the background.
    ForgetSnapshot(Box<ForgetSpec>),
    /// Put a snapshot back into a volume: its service (if any) is stopped,
    /// the snapshot restored beside the data and swapped in, and the
    /// service started again, whatever happened.
    RestoreVolume(Box<RestoreSpec>),
    /// Grow a volume's claim to `size_gi` (its class must allow it). Done
    /// once asked; heartbeats report its new capacity.
    ResizeVolume {
        namespace: String,
        name: String,
        volume_id: String,
        size_gi: i64,
    },
    /// Move a volume's claim to another namespace, keeping its data: the
    /// volume behind it is kept, released and bound to a claim of the same
    /// name there.
    MoveVolume(Box<MoveVolumeSpec>),
    /// Read a database's output: the last `tail_lines` of each of
    /// its pods (since `since_seconds` ago, if given), and its events.
    /// Lines come back in `logs`; `data` has `events` and `pods` (each pod's
    /// labels that tell its part).
    FetchDatabaseLogs {
        namespace: String,
        name: String,
        tail_lines: i64,
        #[serde(default)]
        since_seconds: Option<i64>,
    },
    /// Before an import into a database: connect to the source (its
    /// version, extensions and whether every table can be read) and check
    /// the database to import into is empty, from a short Job in the
    /// team's databases namespace. `data` has what it found.
    CheckDatabaseImport(Box<dbimport::ImportCheck>),
}

// ── Releases and updates ───────────────────────────────────────────────────

/// A published release, as its `release.json` asset describes it
/// (scripts/release-manifest.ts writes it; release.yml publishes it).
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Release {
    /// "0.1.0", without a "v".
    pub version: String,
    /// Pyo's own images, pinned by digest.
    pub images: ReleaseImages,
    /// The Kubernetes objects of pyo-system, as JSON, images pinned. Strings
    /// may hold an installation's own values as `__TAILNET_IP__`,
    /// `__REGISTRY_PUSH__`, `__TAILNET_API__` and `__TAILNET_NAME__`; the upgrader fills them
    /// in from what's running.
    pub manifests: ReleaseManifests,
    /// Migration files of this release, e.g. "0002_rate_limits.sql".
    #[serde(default)]
    pub migrations: Vec<String>,
    /// The oldest version that can update straight to this one; any when None.
    #[serde(default)]
    pub min_from: Option<String>,
    /// Never applied automatically: an admin starts it.
    #[serde(default)]
    pub manual_only: bool,
    /// What to know or do before updating (breaking changes, manual steps).
    #[serde(default)]
    pub before_you_update: Vec<String>,
    /// The release's [`rbac::access_hash`]: its agent's roles. When it isn't
    /// the running Pyo's, every cluster needs its connect command after
    /// the update (updates.rs says so first). The release's own agent
    /// checks it (`pyo-agent check-release`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_access: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ReleaseImages {
    pub pyo: String,
    pub agent: String,
    pub builder: String,
    /// The connection collector (connection analytics).
    #[serde(default)]
    pub flows: String,
    /// The web application firewall's processor (from pyo-waf).
    #[serde(default)]
    pub waf: String,
    /// The proxies' GeoIP database (deploy/images/geoip.Dockerfile).
    #[serde(default)]
    pub geoip: String,
    /// pyo-db-controller, which runs databases (deploy/images/pyo.Dockerfile, target db-controller).
    #[serde(default)]
    pub db_controller: String,
    /// pyo-kv, beside each KV database (deploy/images/pyo.Dockerfile, target kv).
    #[serde(default)]
    pub kv: String,
    /// MySQL's operator, with Pyo's patches (deploy/images/moco).
    #[serde(default)]
    pub mysql_operator: String,
    /// MySQL's backup image, with Pyo's patches (deploy/images/moco).
    #[serde(default)]
    pub mysql_backup: String,
    /// Pyo's Postgres per version (deploy/images/postgres.Dockerfile).
    #[serde(default)]
    pub postgres16: String,
    #[serde(default)]
    pub postgres17: String,
    #[serde(default)]
    pub postgres18: String,
}

impl ReleaseImages {
    /// Every image, by its key in release.json. A release pins each image
    /// scripts/release-manifest.ts writes; adding one there means adding
    /// it here (a test compares the two).
    pub fn all(&self) -> [(&'static str, &str); 13] {
        [
            ("pyo", &self.pyo),
            ("agent", &self.agent),
            ("builder", &self.builder),
            ("flows", &self.flows),
            ("waf", &self.waf),
            ("geoip", &self.geoip),
            ("dbController", &self.db_controller),
            ("kv", &self.kv),
            ("mysqlOperator", &self.mysql_operator),
            ("mysqlBackup", &self.mysql_backup),
            ("postgres16", &self.postgres16),
            ("postgres17", &self.postgres17),
            ("postgres18", &self.postgres18),
        ]
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ReleaseManifests {
    /// Registry, BuildKit, Postgres, ClickHouse, backup store (platform.yaml).
    pub platform: Vec<serde_json::Value>,
    /// Pyo's web and worker Deployments and their Service (pyo.yaml).
    pub pyo: Vec<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpgradeSpec {
    /// Pyo's record of this update.
    pub update_id: String,
    /// The new release's agent image: the upgrader runs from it.
    pub image: String,
    pub release: Release,
}

/// A server uses a replica on itself, else any.
pub const TRAFFIC_LOCAL: &str = "local";
/// A server uses a replica in its region, else any.
pub const TRAFFIC_REGION: &str = "region";
/// Any healthy replica in the cluster, evenly.
pub const TRAFFIC_SPREAD: &str = "spread";

/// An image pinned by digest: `repo[:tag]@sha256:<64 hex>`.
pub fn pinned_image(image: &str) -> bool {
    match image.split_once("@sha256:") {
        Some((repo, digest)) => {
            !repo.is_empty()
                && !repo.contains(char::is_whitespace)
                && digest.len() == 64
                && digest
                    .bytes()
                    .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
        }
        None => false,
    }
}

/// A release's version as numbers: "1.2.3" or "v1.2.3". Pre-releases and
/// anything else don't parse.
pub fn parse_version(v: &str) -> Option<(u64, u64, u64)> {
    let mut parts = v.strip_prefix('v').unwrap_or(v).split('.');
    let n = |p: Option<&str>| {
        p.filter(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()))
            .and_then(|s| s.parse().ok())
    };
    let out = (n(parts.next())?, n(parts.next())?, n(parts.next())?);
    parts.next().is_none().then_some(out)
}

impl Release {
    /// What's wrong with it, if anything: a release Pyo would apply must
    /// name its version and pin every one of Pyo's images by digest, and
    /// its manifests may only use those (or images that aren't Pyo's).
    pub fn problems(&self) -> Vec<String> {
        let mut out = vec![];
        if parse_version(&self.version).is_none() {
            out.push(format!("\"{}\" isn't a release version.", self.version));
        }
        // pyo, agent and builder are required; any other image the release
        // names must be pinned too. Manifests may use only these.
        let mut ours = vec![];
        for (name, image) in self.images.all() {
            let required = matches!(name, "pyo" | "agent" | "builder");
            if image.is_empty() && !required {
                continue;
            }
            if pinned_image(image) {
                ours.push(image);
            } else {
                out.push(format!("The {name} image isn't pinned by digest: {image}"));
            }
        }
        if self.manifests.pyo.is_empty() {
            out.push("It has no manifests for Pyo.".into());
        }
        // Pyo's own images are named under one prefix, e.g. ghcr.io/upsetdev/pyo/.
        let prefix = image_repo(&self.images.pyo)
            .rsplit_once('/')
            .map(|(p, _)| format!("{p}/"))
            .unwrap_or_default();
        for o in self.manifests.platform.iter().chain(&self.manifests.pyo) {
            let mut images = vec![];
            collect_images(o, &mut images);
            for i in images {
                let is_ours = !prefix.is_empty() && i.starts_with(&prefix);
                if is_ours && !ours.contains(&i.as_str()) {
                    out.push(format!(
                        "A manifest uses {i}, which isn't one of the release's pinned images."
                    ));
                }
            }
        }
        if let Some(m) = &self.min_from
            && parse_version(m).is_none()
        {
            out.push(format!("minFrom \"{m}\" isn't a version."));
        }
        out
    }

    /// Whether `current` may update straight to this release.
    pub fn allows_from(&self, current: &str) -> bool {
        match (
            self.min_from.as_deref().and_then(parse_version),
            parse_version(current),
        ) {
            (None, _) => true,
            (Some(min), Some(cur)) => cur >= min,
            (Some(_), None) => false,
        }
    }
}

/// An image reference without its tag and digest: "ghcr.io/a/b:1@sha256:…"
/// is "ghcr.io/a/b" (a registry's port isn't a tag).
pub fn image_repo(image: &str) -> &str {
    let name = image.split('@').next().unwrap_or("");
    match name.rsplit_once(':') {
        Some((repo, tag)) if !tag.contains('/') => repo,
        _ => name,
    }
}

/// Every `image` value in a Kubernetes object, and image references in its
/// containers' env (Pyo passes its agent and builder images that way).
pub fn collect_images(v: &serde_json::Value, out: &mut Vec<String>) {
    match v {
        serde_json::Value::Object(m) => {
            for (k, x) in m {
                match (k.as_str(), x) {
                    ("image", serde_json::Value::String(s)) => out.push(s.clone()),
                    _ => collect_images(x, out),
                }
            }
            if let (Some(serde_json::Value::String(name)), Some(serde_json::Value::String(value))) =
                (m.get("name"), m.get("value"))
                && name.ends_with("_IMAGE")
            {
                out.push(value.clone());
            }
        }
        serde_json::Value::Array(a) => a.iter().for_each(|x| collect_images(x, out)),
        _ => {}
    }
}

/// Control messages on a terminal WebSocket, sent as text frames. Keystrokes
/// and shell output are binary frames.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum TerminalFrame {
    /// Browser to shell: the terminal's size changed.
    Resize { cols: u16, rows: u16 },
    /// Shell to browser: connected to this replica.
    Ready { pod: String },
    /// Shell to browser: it couldn't start or broke, and why.
    Error { message: String },
    /// Shell to browser: the shell exited.
    Exit,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BuilderSpec {
    pub builder_id: String,
    /// The server (its Kubernetes node name) to pin the builder to.
    pub server: String,
    /// URL the builder dials out to.
    pub api_url: String,
    pub token: String,
    pub image: String,
    /// Registry the builder pushes to over plain HTTP (host:port).
    pub registry_host: String,
}

// ── Builders ───────────────────────────────────────────────────────────────
// A builder (crates/pyo-builder) long-polls for build jobs with its own
// token, builds with BuildKit straight from the Git URL, pushes to Pyo's
// registry and reports the image digest.

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct BuilderHeartbeat {
    pub version: String,
    /// BuildKit answered `buildctl debug workers`.
    pub buildkit: bool,
    pub cpus: u32,
    pub memory_gi: f64,
    /// Job being built right now, if any.
    pub busy_with: Option<String>,
    /// The machine's name (a builder on your own server runs with its hostname).
    #[serde(default)]
    pub hostname: Option<String>,
}

/// The files a Git build looks for by default, in order: a Dockerfile wins
/// over a Containerfile when the build context has both.
pub const BUILD_FILES: [&str; 2] = ["Dockerfile", "Containerfile"];

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BuildJob {
    pub id: String,
    pub deployment_id: String,
    pub repo_url: String,
    pub branch: String,
    /// Pinned commit, or empty to build the branch head.
    pub commit: String,
    /// Path of the Dockerfile, relative to the build context. The default
    /// ("Dockerfile") also finds a Containerfile, and a Dockerfile wins when
    /// both are there; without either, the build is zero-config.
    pub dockerfile_path: String,
    /// Directory of the repository to build ("." for the root).
    pub build_context: String,
    pub build_args: Vec<EnvVar>,
    /// Where to push, without tag: "100.x.y.z:5005/pyo/services/svc_x".
    pub push_repository: String,
    /// The registry host above, pushed to over plain HTTP.
    pub registry_host: String,
    pub timeout_seconds: u64,
    /// Repositories connected through Pyo's GitHub App.
    #[serde(default)]
    pub github_installation_id: Option<i64>,
    /// Short-lived token to clone with (GitHub App installations); added
    /// when a builder claims the job, never stored.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clone_token: Option<String>,
}

/// A step of a build. `stage` is "building" or "pushing" (or empty for logs only).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct BuildProgress {
    pub stage: String,
    pub logs: Vec<LogLine>,
    /// Commit the branch resolved to, once known.
    pub commit: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct BuildResult {
    pub ok: bool,
    pub commit: String,
    /// "sha256:…" of the pushed image.
    pub digest: Option<String>,
    pub tag: Option<String>,
    pub message: Option<String>,
    /// Ports the image declares with EXPOSE ("80/tcp" -> 80).
    #[serde(default)]
    pub exposed_ports: Vec<u16>,
    /// How it was built: "Dockerfile" or "Containerfile", or what Railpack detected ("Next.js", "Go", …).
    #[serde(default)]
    pub detected: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServiceRef {
    pub service_id: String,
    pub name: String,
    pub namespace: String,
    /// "Deployment" or "StatefulSet".
    pub workload_kind: String,
}

/// A command run on a schedule with a service's own image and variables.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CronTask {
    pub id: String,
    pub name: String,
    /// Cron syntax, in UTC.
    pub schedule: String,
    pub command: String,
    #[serde(default)]
    pub paused: bool,
}

/// How one of a service's cron jobs is doing.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CronTaskReport {
    pub id: String,
    pub suspended: bool,
    pub last_run: Option<CronRun>,
}

/// Replicas follow CPU use, and memory if `target_memory` is set, between
/// `min` and `max`. Targets are average use as a percentage of what each
/// replica reserves. The target that asks for more replicas wins.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(rename_all = "camelCase")]
pub struct Autoscale {
    pub min: i32,
    pub max: i32,
    pub target_cpu: i32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_memory: Option<i32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeploySpec {
    pub deployment_id: String,
    pub service: ServiceRef,
    pub project_id: String,
    /// The project's team, as its namespace's [`TEAM_LABEL`].
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub team_id: String,
    pub image: String,
    pub replicas: i32,
    /// A CronJob's schedule, in cron syntax, in UTC.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub schedule: String,
    /// What a CronJob runs, through `sh -c`. Empty runs the image's default.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub command: String,
    /// The image's entrypoint, command, working directory or user replaced
    /// (a Docker Compose service's `entrypoint`, `command`, `working_dir`,
    /// `user`); none from an API that doesn't send it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub container: Option<ContainerOverrides>,
    /// Cron jobs that run with this service's image and variables.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub cron_jobs: Vec<CronTask>,
    /// The cluster's traffic mode; "" (from an older API) is [`TRAFFIC_REGION`].
    #[serde(default)]
    pub traffic_mode: String,
    /// With this set, `replicas` is only the starting size of a new service.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub autoscale: Option<Autoscale>,
    /// Every variable goes into a Secret of this deployment's own, which the
    /// pods read with envFrom.
    pub env: Vec<EnvVar>,
    pub ports: Vec<PortSpec>,
    /// The service's domains, which belong to it, not to a deployment: the
    /// API puts its current ones in when it hands the command out.
    pub domains: Vec<DomainSpec>,
    /// Which version of the service's domains `domains` is: it grows with
    /// each change. The agent applies the newest it has seen.
    #[serde(default)]
    pub routes_revision: i64,
    pub resources: Resources,
    pub health_check: HealthCheck,
    pub volumes: Vec<VolumeSpec>,
    /// Give up and report failure after this long.
    pub timeout_seconds: u64,
    /// The version that was live before. If this rollout fails or is
    /// cancelled, the agent puts it back. Only set on the agent's command,
    /// never stored with a deployment.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fallback: Option<Box<DeploySpec>>,
    /// Where its pods may run: node labels, e.g. the standard
    /// `topology.kubernetes.io/region` for a service pinned to a location.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub node_selector: std::collections::BTreeMap<String, String>,
    /// Servers picked by name: its pods run only on these.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub server_names: Vec<String>,
    /// The team's login for the image's registry, when it has one: the
    /// agent fetches it (never stored here) and pulls with it.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub pull_login_id: String,
    /// The project is network-isolated: only its own services, the ingress
    /// and the service's public ports reach its pods.
    #[serde(default)]
    pub network_isolation: bool,
    /// Who may reach it: its firewall. None: everyone.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub firewall: Option<firewall::Firewall>,
    /// The WAF's settings for the service; none while its WAF is off.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub waf: Option<Waf>,
}

/// The web application firewall for one service: the OWASP rules'
/// settings, and its custom rules that run first. It covers all its
/// domains. The WAF's engine runs on a cluster while any service there has
/// its WAF on.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Waf {
    /// The OWASP rules' settings.
    #[serde(default)]
    pub settings: WafSettings,
    /// The service's custom rules, in order. Only those turned on.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rules: Vec<WafRule>,
}

impl Waf {
    /// Its WAF is on (log only or block): its requests go through the
    /// WAF, custom rules first.
    pub fn on(&self) -> bool {
        self.settings.on()
    }
}

/// The OWASP rules' settings for a service.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WafSettings {
    /// "off", "log" (log only) or "block".
    pub mode: String,
    /// How strict: the Core Rule Set's paranoia level, 1 (fewest false
    /// alarms) to 4.
    pub level: u8,
    /// Rule ids not applied.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exceptions: Vec<u32>,
    /// The inbound anomaly score at which a request is blocked; 0 is the
    /// rule set's own (5).
    #[serde(default, skip_serializing_if = "is_zero")]
    pub threshold: u32,
}

fn is_zero(n: &u32) -> bool {
    *n == 0
}

fn none_waiting(n: &i32) -> bool {
    *n == 0
}

/// Inbound anomaly thresholds a service can pick (5 is the rule set's own).
pub const WAF_THRESHOLDS: [u32; 4] = [5, 10, 20, 50];

impl WafSettings {
    pub fn on(&self) -> bool {
        self.mode == "log" || self.mode == "block"
    }
}

/// A custom WAF rule, as the processor runs it (pyo-waf evaluates these
/// itself, before the OWASP rules: values are data, never rule text).
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WafRule {
    /// Pyo's number for it, in the reserved range (`WAF_RULE_IDS`): what
    /// findings list.
    pub id: u32,
    pub name: String,
    /// "all" (every condition) or "any" (one is enough).
    #[serde(rename = "match")]
    pub matching: String,
    pub conditions: Vec<WafCondition>,
    /// "block" (403), "log", "allow" (no later rule, no OWASP rules) or
    /// "skipOwasp" (later custom rules, no OWASP rules).
    pub action: String,
}

/// One test on a request.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WafCondition {
    /// One of `WAF_FIELDS`.
    pub field: String,
    /// The header's or cookie's name.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub name: String,
    /// One of `WAF_OPERATORS`.
    pub op: String,
    /// One value, or the list for "in".
    pub values: Vec<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub negate: bool,
}

/// Fields a condition reads.
pub const WAF_FIELDS: [&str; 10] = [
    "path",
    "query",
    "method",
    "host",
    "header",
    "userAgent",
    "cookie",
    "body",
    "ip",
    "country",
];
/// How it compares.
pub const WAF_OPERATORS: [&str; 6] = [
    "equals",
    "contains",
    "startsWith",
    "endsWith",
    "in",
    "regex",
];
pub const WAF_ACTIONS: [&str; 4] = ["block", "log", "allow", "skipOwasp"];
/// Custom rules' numbers: kept apart from the Core Rule Set's (900000 to
/// 999999) and Coraza's own.
pub const WAF_RULE_IDS: std::ops::RangeInclusive<u32> = 1_000_000..=1_999_999;
/// The most custom rules a service holds.
pub const WAF_MAX_RULES: usize = 50;
/// The most conditions one rule holds.
pub const WAF_MAX_CONDITIONS: usize = 10;
/// The most values an "in" list holds.
pub const WAF_MAX_VALUES: usize = 50;
/// A value's (or a regex's) longest length, in bytes.
pub const WAF_MAX_VALUE: usize = 256;

/// The firewall and WAF as they now stand, for every service on a cluster:
/// applied in place, without a redeploy.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct FirewallSync {
    pub targets: Vec<FirewallTarget>,
    /// Some service on this cluster has its WAF on: its engine runs there.
    #[serde(default)]
    pub waf: bool,
}

/// A running service's routes: its public ports (its live deployment's) and
/// its current domains, with its firewall and WAF.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FirewallTarget {
    pub service: ServiceRef,
    pub project_id: String,
    pub ports: Vec<PortSpec>,
    pub domains: Vec<DomainSpec>,
    /// As [`DeploySpec::routes_revision`].
    #[serde(default)]
    pub routes_revision: i64,
    #[serde(default)]
    pub network_isolation: bool,
    pub firewall: firewall::Firewall,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub waf: Option<Waf>,
}

/// What a service changes about how its image starts. Empty fields keep the
/// image's own.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContainerOverrides {
    /// Replaces the image's ENTRYPOINT.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub entrypoint: Vec<String>,
    /// Replaces the image's CMD (the entrypoint's arguments).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub command: Vec<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub working_dir: String,
    /// Numeric user and group ids to run as.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_as_user: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_as_group: Option<i64>,
}

impl ContainerOverrides {
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

/// A registry login, as the agent gets it to write a pull secret.
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RegistryLogin {
    pub host: String,
    pub username: String,
    pub password: String,
}

impl std::fmt::Debug for RegistryLogin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RegistryLogin")
            .field("host", &self.host)
            .field("username", &self.username)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EnvVar {
    pub key: String,
    pub value: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PortSpec {
    pub name: String,
    pub container_port: i32,
    /// "HTTP", "TCP" or "UDP".
    pub protocol: String,
    /// "internal", "http", "nodeport" or "loadbalancer".
    pub exposure: String,
    pub public_port: Option<i32>,
    /// A public TCP or UDP port ("loadbalancer") through the gateway:
    /// a listener of its own on the proxy's servers. None: published
    /// straight from the servers (ServiceLB), as before the proxy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proxy: Option<PortProxy>,
}

impl PortSpec {
    /// The port a public TCP or UDP port answers on, outside.
    pub fn public(&self) -> i32 {
        self.public_port.unwrap_or(self.container_port)
    }

    /// A public TCP or UDP port through the gateway.
    pub fn proxied(&self) -> bool {
        self.exposure == "loadbalancer" && self.proxy.is_some()
    }

    /// A public TCP or UDP port published straight from the servers.
    pub fn direct(&self) -> bool {
        self.exposure == "loadbalancer" && self.proxy.is_none()
    }

    /// "TCP" or "UDP": HTTP is TCP underneath.
    pub fn transport(&self) -> &'static str {
        if self.protocol == "UDP" { "UDP" } else { "TCP" }
    }
}

/// A proxied TCP or UDP port's options; all off by default. The gateway
/// can't do any of them for UDP (the API refuses them there).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PortProxy {
    /// Only these addresses ("allow"), or everyone but them ("deny").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ip_filter: Option<IpFilter>,
    /// At most this many open connections on the port, from everyone together.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connection_limit: Option<u32>,
    /// PROXY protocol to the service: "V1" or "V2"; "" for none.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub send_proxy_protocol: String,
    /// Read the visitor's address from a PROXY protocol header, when one is
    /// sent (a load balancer or tunnel in front); plain connections still work.
    #[serde(default)]
    pub accept_proxy_protocol: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IpFilter {
    /// "allow" (only these) or "deny" (everyone but these).
    pub mode: String,
    /// Addresses or ranges: "203.0.113.7", "198.51.100.0/24", "2001:db8::/32".
    pub cidrs: Vec<String>,
}

/// Service names can't start with this: a service's public ports that bypass
/// the gateway are published by a Service of this name and its own.
pub const PUBLIC_SERVICE_PREFIX: &str = "pyo-public-";

/// That Service's name: shortened with a hash past 63 characters.
pub fn public_service_name(name: &str) -> String {
    let full = format!("{PUBLIC_SERVICE_PREFIX}{name}");
    if full.len() <= 63 {
        return full;
    }
    let mut h: u32 = 2166136261;
    for b in name.bytes() {
        h = (h ^ b as u32).wrapping_mul(16777619);
    }
    format!("{}-{h:08x}", full[..54].trim_end_matches('-'))
}

/// Ports the servers themselves need: a public port is caught on every
/// server, connections from the server itself included, so one of these
/// would cut off SSH, the server's own DNS lookups, Kubernetes or the
/// network between servers.
pub const SERVER_TCP_PORTS: [i32; 6] = [22, 53, 2379, 2380, 6443, 10250];
pub const SERVER_UDP_PORTS: [i32; 4] = [53, 8472, 51820, 51821];

/// Public ports a service can't take: the servers' own (above), and the
/// gateway's (websites on 80 and 443, their in-proxy aliases 10080
/// and 10443, and Envoy's 19000-19003).
pub fn reserved_public_port(protocol: &str, port: i32) -> Option<&'static str> {
    if protocol == "UDP" {
        return SERVER_UDP_PORTS.contains(&port).then_some(
            "The servers need this port themselves (DNS lookups, and the network between servers).",
        );
    }
    match port {
        80 | 443 => Some("Ports 80 and 443 are for websites: add a domain to the service instead."),
        10080 | 10443 => Some("The gateway uses this port for websites (80 and 443 inside it)."),
        p if system::TAILNET_PORTS.contains(&p) => Some(
            "Pyo uses this port itself, on your tailnet (its dashboard, registry and storage).",
        ),
        19000..=19003 => Some("The gateway uses ports 19000 to 19003 itself."),
        p if SERVER_TCP_PORTS.contains(&p) => {
            Some("The servers need this port themselves (SSH, DNS lookups, Kubernetes).")
        }
        _ => None,
    }
}

/// Where the gateway listens for a public port inside its pods: as
/// Envoy Gateway does, ports below 1024 move up by 10000 (no privileges
/// needed). Two proxied ports with the same one would clash.
pub fn proxy_listen_port(public: i32) -> i32 {
    if public < 1024 {
        public + 10000
    } else {
        public
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DomainSpec {
    pub hostname: String,
    pub port_name: String,
    /// Certificate issuer: "letsencrypt", "selfsigned", "custom" (an
    /// uploaded certificate, `certificate_id`), or "" for plain HTTP.
    #[serde(default)]
    pub tls: String,
    /// The uploaded certificate, when `tls` is "custom". The agent fetches it
    /// from the API when it applies the domain, so keys never sit in specs.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub certificate_id: String,
    /// Send plain HTTP requests to HTTPS (when `tls` is set).
    #[serde(default)]
    pub redirect_https: bool,
}

/// An uploaded certificate, as the agent gets it: the chain and its key, PEM.
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TlsCertificate {
    pub certificate: String,
    pub private_key: String,
}

impl std::fmt::Debug for TlsCertificate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TlsCertificate")
            .field("private_key", &"…")
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct Resources {
    pub cpu_request: i64,
    pub cpu_limit: i64,
    pub memory_request: i64,
    pub memory_limit: i64,
    /// Whole NVIDIA GPUs, all on one server; 0 for none.
    #[serde(default)]
    pub gpu: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HealthCheck {
    /// "http", "tcp", "exec" or "none".
    #[serde(rename = "type")]
    pub kind: String,
    pub path: String,
    pub port: i32,
    pub command: String,
    pub initial_delay_seconds: i32,
    pub period_seconds: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VolumeSpec {
    pub volume_id: String,
    /// PVC name.
    pub name: String,
    pub mount_path: String,
    pub size_gi: i64,
    pub storage_class: String,
    pub access_mode: String,
}

// ── Replies ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct CommandResult {
    pub ok: bool,
    pub message: Option<String>,
    /// FetchLogs puts its lines here.
    pub logs: Vec<LogLine>,
    /// Structured results, e.g. a backup's snapshot and size.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}

/// Back up one volume or one database to the team's restic repository.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BackupSpec {
    pub backup_id: String,
    pub service: ServiceRef,
    /// "volume": the claim's files. "database": a dump made by `engine`.
    /// Sent as "target": Command is tagged with "kind".
    #[serde(rename = "target")]
    pub kind: String,
    /// The volume's claim (kind "volume").
    #[serde(default)]
    pub claim: String,
    /// "postgres", "mysql", "mariadb", "mongo" or "redis" (Redis or Valkey;
    /// kind "database").
    #[serde(default)]
    pub engine: String,
    /// Tags the snapshot carries, e.g. "svc=svc_x", "volume=pg-data".
    pub tags: Vec<String>,
    /// Snapshots kept per target; older ones are forgotten and pruned.
    pub keep: u32,
    /// restic's repository, e.g. "s3:https://s3.amazonaws.com/bucket/pyo/team_x".
    pub repository: String,
    pub password: String,
    pub access_key: String,
    pub secret_key: String,
    pub region: String,
    /// A database set up for backups: dumped as its read-only user, with
    /// the engine's client image (instead of the service's own env).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connection: Option<DbConnection>,
}

/// A team's restic repository and the keys to it.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ResticRepo {
    pub repository: String,
    pub password: String,
    pub access_key: String,
    pub secret_key: String,
    pub region: String,
}

/// List the snapshots of volume `volume_id` (tag `volume=<id>`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotsSpec {
    /// Names the Job; any short id.
    pub request_id: String,
    /// Where the Job runs: the volume's namespace.
    pub namespace: String,
    pub volume_id: String,
    #[serde(flatten)]
    pub repo: ResticRepo,
}

/// Delete snapshot `snapshot_id` of volume `volume_id`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ForgetSpec {
    /// Names the Job; any short id.
    pub request_id: String,
    /// Where the Job runs: the volume's namespace.
    pub namespace: String,
    pub volume_id: String,
    pub snapshot_id: String,
    #[serde(flatten)]
    pub repo: ResticRepo,
}

/// Restore snapshot `snapshot_id` into volume `volume_id` (claim `claim`
/// in `namespace`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RestoreSpec {
    pub restore_id: String,
    pub namespace: String,
    pub claim: String,
    pub volume_id: String,
    pub snapshot_id: String,
    /// The service that mounts it: stopped while it's restored, then
    /// started again at the size it had.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service: Option<ServiceRef>,
    #[serde(flatten)]
    pub repo: ResticRepo,
}

/// Move volume `volume_id`'s claim `name` from `from_namespace` to
/// `to_namespace` (made for project `project_id` of team `team_id` if it
/// isn't there yet), for service `service_id`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MoveVolumeSpec {
    pub volume_id: String,
    pub name: String,
    pub from_namespace: String,
    pub to_namespace: String,
    pub project_id: String,
    #[serde(default)]
    pub team_id: String,
    pub service_id: String,
}

/// How to reach a database, for a dump or for setting up its backup user.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct DbConnection {
    /// "postgres", "mysql", "mariadb", "mongo" or "redis".
    pub engine: String,
    pub host: String,
    pub port: u16,
    /// The database to back up; empty: all of them (MySQL, MongoDB).
    #[serde(default)]
    pub database: String,
    #[serde(default)]
    pub user: String,
    #[serde(default)]
    pub password: String,
}

/// The read-only user Pyo makes in each database it backs up.
pub const BACKUP_DB_USER: &str = "pyo_backup";

/// The image with an engine's client tools (pg_dump, mysqldump, ...).
/// Each is the newest release line its dump tool supports: pg_dump dumps
/// servers as old as 9.2 but refuses newer ones, so it tracks the newest
/// PostgreSQL. Restoring a PostgreSQL dump takes pg_restore 18 or newer.
pub fn db_tools_image(engine: &str) -> &'static str {
    match engine {
        "postgres" => "docker.io/library/postgres:18",
        "mysql" => "docker.io/library/mysql:8.4",
        "mariadb" => "docker.io/library/mariadb:11.8",
        "mongo" => "docker.io/library/mongo:8.0",
        _ => "docker.io/valkey/valkey:8.0",
    }
}

/// restic, pinned.
pub const RESTIC_IMAGE: &str = "docker.io/restic/restic:0.17.3";

/// One step of a rollout, posted as it happens.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct DeploymentProgress {
    /// A stage ("applying", "scheduling", "pulling", "starting",
    /// "waiting_ready") or a final status ("succeeded", "failed", "cancelled").
    pub status: String,
    pub events: Vec<KubeEvent>,
    pub logs: Vec<LogLine>,
    pub failure: Option<Failure>,
    /// Image that actually ran, as the node reports it
    /// ("docker.io/library/nginx@sha256:…"), once known.
    pub revision: Option<String>,
    /// On a failed or cancelled rollout with a fallback: whether the
    /// previous version was put back.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rolled_back: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KubeEvent {
    pub at: i64,
    /// "Normal" or "Warning".
    #[serde(rename = "type")]
    pub kind: String,
    pub reason: String,
    pub object: String,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(rename_all = "camelCase")]
pub struct LogLine {
    /// When it was printed, in milliseconds since 1970.
    pub at: i64,
    /// "debug", "info", "warn" or "error".
    pub level: String,
    /// The replica that printed it.
    pub source: String,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Failure {
    pub stage: String,
    pub reason: String,
    pub message: String,
}

pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Whether a Kubernetes error passes by itself: an operator's webhook or
/// API that isn't up yet, as right after it's installed (no endpoints,
/// connection refused, unavailable).
pub fn transient_error(message: &str) -> bool {
    let m = message.to_ascii_lowercase();
    [
        "no endpoints available",
        "connection refused",
        "failed calling webhook",
        "service unavailable",
        "the server is currently unable to handle the request",
    ]
    .iter()
    .any(|t| m.contains(t))
}

/// The standard Kubernetes labels for where a node is.
pub const REGION_LABEL: &str = "topology.kubernetes.io/region";
pub const ZONE_LABEL: &str = "topology.kubernetes.io/zone";

/// A node's location as labels, from "City, CC": the region is the country
/// code ("id"), the zone the city ("yogyakarta"), when there is one.
/// A region's name as a label value: "US East" is "us-east".
pub fn region_slug(name: &str) -> String {
    let mut out = String::new();
    for ch in name.to_lowercase().chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch);
        } else if !out.is_empty() && !out.ends_with('-') {
            out.push('-');
        }
    }
    out.trim_end_matches('-')
        .chars()
        .take(63)
        .collect::<String>()
        .trim_end_matches('-')
        .to_owned()
}

/// A region as labels. "City, CC" (from GeoIP, or typed so): the country
/// code is the region label and the city the zone. "CC": the country only.
/// Any other name ("US East"): its slug as both.
pub fn location_labels(location: &str) -> Option<(String, Option<String>)> {
    let (city, cc) = match location.rsplit_once(',') {
        Some((c, cc)) => (c.trim(), cc.trim()),
        None => ("", location.trim()),
    };
    if cc.len() != 2 || !cc.bytes().all(|b| b.is_ascii_alphabetic()) {
        let slug = region_slug(location);
        return (!slug.is_empty()).then(|| (slug.clone(), Some(slug)));
    }
    let mut zone = String::new();
    for ch in city.to_lowercase().chars() {
        if ch.is_ascii_alphanumeric() {
            zone.push(ch);
        } else if !zone.is_empty() && !zone.ends_with('-') {
            zone.push('-');
        }
    }
    let zone: String = zone
        .trim_end_matches('-')
        .chars()
        .take(63)
        .collect::<String>()
        .trim_end_matches('-')
        .to_owned();
    Some((cc.to_ascii_lowercase(), (!zone.is_empty()).then_some(zone)))
}

/// What the API tells the agent after a heartbeat: the location labels each
/// server should carry.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct HeartbeatReply {
    #[serde(default)]
    pub server_locations: Vec<ServerLocation>,
    /// Every server's tags (empty for none), as `pyo/tag-<tag>` labels.
    #[serde(default)]
    pub server_tags: Vec<ServerTags>,
    /// GPU servers: kept for GPU work (tainted) or not.
    #[serde(default)]
    pub gpu_policies: Vec<GpuPolicy>,
    /// The cluster takes public traffic: the gateway's proxies are
    /// published on its servers (every one but build-only servers). Off:
    /// they answer only inside the cluster (and, on the cluster Pyo runs
    /// on, on the tailnet). None (a reply without it): left as it is.
    #[serde(default)]
    pub public_traffic: Option<bool>,
    /// Where the cluster's free domains point: "public" or "tailscale".
    /// With "tailscale" the websites are published on every server's
    /// Tailscale address too (system.rs `TAILNET_WEB_SERVICE`), whatever
    /// the public traffic. None (a reply without it): left as it is.
    #[serde(default)]
    pub free_domain_address: Option<String>,
    /// Observability (an installation-wide setting): on, Vector ships the
    /// gateway's access logs and the servers' figures, and on the cluster
    /// Pyo runs on ClickHouse runs. Off: Vector is removed and ClickHouse
    /// stopped (its data kept). None (a reply without it): left as it is.
    #[serde(default)]
    pub observability: Option<bool>,
    /// The team of each project namespace on the cluster, as its
    /// [`TEAM_LABEL`] (one missing or changed by hand is put back).
    #[serde(default)]
    pub project_teams: Vec<ProjectTeam>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ProjectTeam {
    pub namespace: String,
    pub team_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerTags {
    pub server: String,
    pub tags: Vec<String>,
}

/// Servers carry each of their tags as a label `pyo/tag-<tag>: "true"`,
/// which services placed by tag select.
pub const TAG_LABEL_PREFIX: &str = "pyo/tag-";

/// A tag as people type it, if it's one: lowercase letters, digits and
/// dashes, up to 32 (it ends up in a label name).
pub fn valid_tag(tag: &str) -> Option<String> {
    let t = tag.trim().to_ascii_lowercase();
    let ok = !t.is_empty()
        && t.len() <= 32
        && t.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !t.starts_with('-')
        && !t.ends_with('-');
    ok.then_some(t)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerLocation {
    pub server: String,
    pub region: String,
    /// The zone label: the region's own name ("singapore", "us-east").
    /// Same-region routing and spreading go by it.
    pub zone: Option<String>,
}

#[cfg(test)]
mod location_tests {
    use super::*;

    #[test]
    fn host_proofs_are_hmac_sha256() {
        // As Python's hmac.new(key, b"pyo-host-proof\n" + msg, sha256) makes them.
        let p = host_proof("Jefe", "what do ya want for nothing?");
        assert_eq!(
            p,
            "cf3f5b1f4d394c848aeeb8f18944485249eaeaea4daacecf557cda76ea913086"
        );
        assert_eq!(p, host_proof("Jefe", "what do ya want for nothing?"));
        assert_ne!(
            p,
            host_proof("Jeff", "what do ya want for nothing?"),
            "another key"
        );
        assert_ne!(
            p,
            host_proof("Jefe", "what do ya want for nothing!"),
            "another token"
        );
        assert_eq!(
            host_proof(&"k".repeat(100), "t"),
            "dfb7064c9ec6922f16af181217ab758aebfe0699fcf64cb4031d064bb99e741b",
            "long keys are hashed first"
        );
    }

    #[test]
    fn same_cluster_urls() {
        assert!(in_cluster_url("http://pyo.pyo-system.svc:8000"));
        assert!(in_cluster_url(
            "http://pyo.pyo-system.svc.cluster.local:8000/api/agent/v1"
        ));
        assert!(!in_cluster_url("http://100.64.0.10:8000"));
        assert!(!in_cluster_url("https://pyo.example.com"));
    }

    #[test]
    fn delete_namespace_travels() {
        let c = Command::DeleteNamespace {
            namespace: "e2e".into(),
            project_id: "prj_1".into(),
        };
        let v = serde_json::to_value(&c).unwrap();
        assert_eq!(
            v,
            serde_json::json!({ "kind": "delete_namespace", "namespace": "e2e", "project_id": "prj_1" })
        );
        assert!(
            matches!(serde_json::from_value(v).unwrap(), Command::DeleteNamespace { namespace, project_id } if namespace == "e2e" && project_id == "prj_1")
        );
    }

    #[test]
    fn delete_volume_travels() {
        let c = Command::DeleteVolume {
            namespace: "e2e".into(),
            name: "web-data".into(),
            volume_id: "vol_1234".into(),
        };
        let v = serde_json::to_value(&c).unwrap();
        assert_eq!(
            v,
            serde_json::json!({ "kind": "delete_volume", "namespace": "e2e", "name": "web-data", "volume_id": "vol_1234" })
        );
        assert!(
            matches!(serde_json::from_value(v).unwrap(), Command::DeleteVolume { volume_id, .. } if volume_id == "vol_1234")
        );
        // Older agents' reports still read.
        let r: VolumeReport = serde_json::from_value(
            serde_json::json!({ "volumeId": "vol_1", "status": "bound", "message": null }),
        )
        .unwrap();
        assert!(r.namespace.is_empty() && r.size_gi.is_none());
    }

    /// Command is tagged with "kind": no variant may have a field of that
    /// name, or the tag is overwritten and the command can't be read back.
    #[test]
    fn backup_commands_round_trip() {
        let service = ServiceRef {
            service_id: "svc_x".into(),
            name: "x".into(),
            namespace: "ns".into(),
            workload_kind: "Deployment".into(),
        };
        let spec = BackupSpec {
            backup_id: "bkp_1".into(),
            service,
            kind: "volume".into(),
            claim: "data".into(),
            engine: String::new(),
            tags: vec!["svc=svc_x".into()],
            keep: 3,
            repository: "s3:http://store/b/t".into(),
            password: "p".into(),
            access_key: "a".into(),
            secret_key: "s".into(),
            region: "us-east-1".into(),
            connection: Some(DbConnection {
                engine: "postgres".into(),
                host: "h".into(),
                port: 5432,
                ..Default::default()
            }),
        };
        let v = serde_json::to_value(Command::Backup(Box::new(spec))).unwrap();
        assert_eq!(v["kind"], "backup");
        assert_eq!(v["target"], "volume");
        let back: Command = serde_json::from_value(v).unwrap();
        assert!(matches!(back, Command::Backup(s) if s.kind == "volume"));
        let setup = Command::DbSetup {
            namespace: "ns".into(),
            admin: DbConnection::default(),
            password: "p".into(),
        };
        let back: Command = serde_json::from_value(serde_json::to_value(setup).unwrap()).unwrap();
        assert!(matches!(back, Command::DbSetup { .. }));
    }

    #[test]
    fn pg_dump_is_new_enough() {
        // pg_dump refuses servers newer than itself.
        let tools: u32 = db_tools_image("postgres")
            .rsplit_once(':')
            .unwrap()
            .1
            .parse()
            .unwrap();
        assert!(
            system::POSTGRES_MAJORS
                .iter()
                .all(|m| m.parse::<u32>().unwrap() <= tools)
        );
    }

    #[test]
    fn tags() {
        assert_eq!(valid_tag(" GPU ").as_deref(), Some("gpu"));
        assert_eq!(valid_tag("eu-west-1").as_deref(), Some("eu-west-1"));
        assert_eq!(valid_tag("-x"), None);
        assert_eq!(valid_tag("a b"), None);
        assert_eq!(valid_tag(&"x".repeat(33)), None);
        // Prefix plus the longest tag is still a valid label name (63 max after the slash).
        assert!(format!("tag-{}", "x".repeat(32)).len() <= 63);
    }

    #[test]
    fn labels() {
        assert_eq!(
            location_labels("Yogyakarta, ID"),
            Some(("id".into(), Some("yogyakarta".into())))
        );
        assert_eq!(
            location_labels("São Paulo, BR"),
            Some(("br".into(), Some("s-o-paulo".into())))
        );
        assert_eq!(location_labels("DE"), Some(("de".into(), None)));
        assert_eq!(
            location_labels("Frankfurt"),
            Some(("frankfurt".into(), Some("frankfurt".into())))
        );
        assert_eq!(
            location_labels("US East"),
            Some(("us-east".into(), Some("us-east".into())))
        );
        assert_eq!(location_labels("!!"), None);
        assert_eq!(region_slug("US East"), "us-east");
        assert_eq!(region_slug("  Singapore (A) "), "singapore-a");
        assert_eq!(region_slug("!!"), "");
    }
}

#[cfg(test)]
mod release_tests {
    use super::*;
    use serde_json::json;

    const D: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    fn release() -> Release {
        let img = |n: &str| format!("ghcr.io/upsetdev/pyo/{n}:0.2.0@sha256:{D}");
        Release {
            version: "0.2.0".into(),
            images: ReleaseImages {
                pyo: img("pyo"),
                agent: img("agent"),
                builder: img("builder"),
                flows: img("flows"),
                waf: img("waf"),
                geoip: img("geoip"),
                db_controller: img("db-controller"),
                kv: img("kv"),
                mysql_operator: img("mysql-operator"),
                mysql_backup: img("mysql-backup"),
                postgres16: img("postgres-16"),
                postgres17: img("postgres-17"),
                postgres18: img("postgres-18"),
            },
            manifests: ReleaseManifests {
                platform: vec![
                    json!({ "kind": "StatefulSet", "spec": { "template": { "spec": { "containers": [{ "name": "pg", "image": "postgres:18.6-alpine" }] } } } }),
                ],
                pyo: vec![
                    json!({ "kind": "Deployment", "spec": { "template": { "spec": { "containers": [{
                    "name": "pyo", "image": img("pyo"),
                    "env": [{ "name": "PYO_AGENT_IMAGE", "value": img("agent") }, { "name": "PYO_BUILDER_IMAGE", "value": img("builder") }],
                }] } } } }),
                ],
            },
            ..Default::default()
        }
    }

    #[test]
    fn versions() {
        assert_eq!(parse_version("0.2.0"), Some((0, 2, 0)));
        assert_eq!(parse_version("v10.0.3"), Some((10, 0, 3)));
        assert!(parse_version("0.2.0-rc.1").is_none());
        assert!(parse_version("0.2").is_none());
        assert!(parse_version("0.2.0.1").is_none());
        assert!(parse_version("0.2.x").is_none());
        assert!(parse_version("0.10.0") > parse_version("0.9.9"));
    }

    #[test]
    fn digests() {
        assert!(pinned_image(&format!(
            "ghcr.io/upsetdev/pyo/pyo@sha256:{D}"
        )));
        assert!(pinned_image(&format!(
            "127.0.0.1:5005/pyo/pyo:1@sha256:{D}"
        )));
        assert!(!pinned_image("ghcr.io/upsetdev/pyo/pyo:0.2.0"));
        assert!(!pinned_image(&format!(
            "ghcr.io/upsetdev/pyo/pyo@sha256:{}",
            &D[1..]
        )));
        assert!(!pinned_image(&format!(
            "ghcr.io/upsetdev/pyo/pyo@sha256:{}",
            D.to_uppercase()
        )));
        assert_eq!(
            image_repo(&format!("127.0.0.1:5005/pyo/pyo:1@sha256:{D}")),
            "127.0.0.1:5005/pyo/pyo"
        );
        assert_eq!(
            image_repo("127.0.0.1:5005/pyo/pyo"),
            "127.0.0.1:5005/pyo/pyo"
        );
    }

    #[test]
    fn a_good_release_has_no_problems() {
        assert!(
            release().problems().is_empty(),
            "{:?}",
            release().problems()
        );
    }

    #[test]
    fn releases_must_pin_their_images() {
        let mut r = release();
        r.images.agent = "ghcr.io/upsetdev/pyo/agent:0.2.0".into();
        assert!(
            r.problems()
                .iter()
                .any(|p| p.contains("agent image isn't pinned"))
        );
        // A manifest naming one of Pyo's images that isn't the pinned one.
        let mut r = release();
        r.manifests.pyo[0]["spec"]["template"]["spec"]["containers"][0]["env"][0]["value"] =
            json!("ghcr.io/upsetdev/pyo/agent:latest");
        assert!(
            r.problems().iter().any(|p| p.contains("agent:latest")),
            "{:?}",
            r.problems()
        );
        let mut r = release();
        r.manifests.pyo.clear();
        assert!(!r.problems().is_empty());
    }

    /// v0.1.0's release.json as published: its upgrader refused it, as
    /// only pyo, agent and builder counted as the release's images.
    fn v0_1_0() -> (Release, serde_json::Value) {
        let text = include_str!("../testdata/release-0.1.0.json");
        (
            serde_json::from_str(text).unwrap(),
            serde_json::from_str(text).unwrap(),
        )
    }

    #[test]
    fn the_published_release_has_no_problems() {
        let (r, _) = v0_1_0();
        assert!(r.problems().is_empty(), "{:?}", r.problems());
    }

    #[test]
    fn every_image_release_json_pins_is_read() {
        let (r, raw) = v0_1_0();
        // Each of its images is a field here, with the same value.
        assert_eq!(serde_json::to_value(&r.images).unwrap(), raw["images"]);
        for (name, image) in r.images.all() {
            assert!(pinned_image(image), "{name}: {image}");
        }
        // And all() names every field.
        let fields = serde_json::to_value(ReleaseImages::default()).unwrap();
        let mut fields: Vec<&str> = fields
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        let mut named: Vec<&str> = ReleaseImages::default()
            .all()
            .iter()
            .map(|(n, _)| *n)
            .collect();
        fields.sort();
        named.sort();
        assert_eq!(fields, named);
    }

    /// The images scripts/release-manifest.ts writes into release.json are
    /// the ones a release is checked against, so a new image can't be
    /// published without the upgrader accepting it.
    #[test]
    fn release_manifest_script_writes_these_images() {
        let script = include_str!("../../../scripts/release-manifest.ts");
        let names = script
            .split_once("const names = {")
            .and_then(|(_, rest)| rest.split_once("} as const;"))
            .expect("release-manifest.ts has `const names = { … } as const;`")
            .0;
        let mut keys: Vec<String> = names
            .lines()
            .map(str::trim)
            .filter(|l| !l.starts_with("//"))
            .filter_map(|l| l.split_once(':'))
            .map(|(_, v)| v.trim().trim_end_matches(',').trim_matches('"').to_owned())
            .collect();
        let majors = script
            .split_once("for (const major of [")
            .and_then(|(_, rest)| rest.split_once(']'))
            .expect("release-manifest.ts lists Postgres versions")
            .0;
        keys.extend(
            majors
                .split(',')
                .map(|m| format!("postgres{}", m.trim().trim_matches('"'))),
        );
        keys.sort();
        let mut ours: Vec<String> = ReleaseImages::default()
            .all()
            .iter()
            .map(|(n, _)| n.to_string())
            .collect();
        ours.sort();
        assert_eq!(keys, ours);
    }

    #[test]
    fn unpinned_images_of_ours_are_refused() {
        let (r, _) = v0_1_0();
        let prefix = "ghcr.io/upsetdev/pyo";
        let with = |image: String| {
            let mut r = r.clone();
            let deployment = r
                .manifests
                .pyo
                .iter_mut()
                .find(|o| o["kind"] == "Deployment")
                .unwrap();
            deployment["spec"]["template"]["spec"]["containers"][0]["image"] = json!(image);
            r.problems()
        };
        // One of Pyo's images that the release doesn't pin, by tag or digest.
        for image in [
            format!("{prefix}/x:0.1.0"),
            format!("{prefix}/x:0.1.0@sha256:{D}"),
            format!("{prefix}/flows:0.1.0"),
            format!("{prefix}/flows:0.1.0@sha256:{D}"),
            format!("{prefix}/postgres:18-0.1.0"),
        ] {
            let p = with(image.clone());
            assert!(
                p.iter()
                    .any(|p| p.contains(&image) && p.contains("isn't one of")),
                "{image}: {p:?}"
            );
        }
        // Another of its pinned images is fine anywhere; so is anyone else's.
        assert!(with(r.images.mysql_backup.clone()).is_empty());
        assert!(with("docker.io/library/busybox:1".into()).is_empty());
        // An image the release names must be pinned itself.
        let mut bad = r.clone();
        bad.images.flows = format!("{prefix}/flows:0.1.0");
        assert!(
            bad.problems()
                .iter()
                .any(|p| p.contains("flows image isn't pinned")),
            "{:?}",
            bad.problems()
        );
        // and the manifests that use the old pin no longer match it.
        assert!(bad.problems().iter().any(|p| p.contains("isn't one of")));
    }

    #[test]
    fn min_from() {
        let mut r = release();
        assert!(r.allows_from("0.0.1"));
        r.min_from = Some("0.1.5".into());
        assert!(r.allows_from("0.1.5"));
        assert!(r.allows_from("0.1.9"));
        assert!(!r.allows_from("0.1.4"));
        assert!(!r.allows_from("dev"));
    }

    #[test]
    fn update_commands_round_trip() {
        let c = Command::UpgradePyo(Box::new(UpgradeSpec {
            update_id: "upd_1".into(),
            image: release().images.agent,
            release: release(),
        }));
        let v = serde_json::to_value(&c).unwrap();
        assert_eq!(v["kind"], "upgrade_pyo");
        assert!(
            matches!(serde_json::from_value::<Command>(v).unwrap(), Command::UpgradePyo(s) if s.release == release())
        );
        let v = serde_json::to_value(Command::UpdateAgent { image: "x".into() }).unwrap();
        assert_eq!(v, json!({ "kind": "update_agent", "image": "x" }));
    }
}

#[cfg(test)]
mod usage_compat {
    use super::*;

    /// A heartbeat from an agent older than disk and network usage still parses
    #[test]
    fn webhooks_not_up_yet_are_transient() {
        assert!(transient_error(
            r#"Internal error occurred: failed calling webhook "mcluster.cnpg.io": failed to call webhook: Post "https://cnpg-webhook-service.cnpg-system.svc:443/mutate": no endpoints available for service "cnpg-webhook-service""#
        ));
        assert!(transient_error(
            "dial tcp 10.43.0.5:443: connect: connection refused"
        ));
        assert!(!transient_error("instances must be 1 to 5"));
        assert!(!transient_error(
            "admission webhook denied the request: bad storage"
        ));
    }

    /// (N/N-1), with the new figures absent.
    #[test]
    fn older_heartbeat_parses() {
        let old = serde_json::json!({
            "protocol": 1, "agentVersion": "0.1.0", "kubernetesVersion": "v1.36.5+k3s1",
            "servers": [{
                "name": "sg-a", "roles": ["worker"], "status": "ready", "schedulable": true, "maintenance": false,
                "kubeletVersion": "v1", "osImage": "Ubuntu", "arch": "amd64", "containerRuntime": "containerd",
                "internalIP": "10.0.0.1", "externalIP": null,
                "capacity": { "cpu": 4.0, "memoryGi": 8.0, "pods": 110 }, "usage": { "cpu": 1.0, "memoryGi": 2.0 },
                "podCount": 3, "conditions": [], "createdAt": 0
            }],
            "workloads": [],
            "services": [{ "serviceId": "svc_1", "desired": 1, "readyReplicas": 1, "restarts": 0, "cpuMillis": 5.0, "memoryMi": 30.0, "since": null,
                       "replicas": [{ "name": "web-1", "server": "sg-a", "ready": true, "startedAt": null }] }],
            "volumes": [{ "volumeId": "vol_1", "status": "bound", "message": null }],
            "capabilities": { "ingressController": "envoy", "loadBalancer": "servicelb", "loadBalancerIps": [], "certManager": false,
                              "metricsServer": true, "storageClasses": [], "podCidr": "", "serviceCidr": "" }
        });
        let hb: Heartbeat = serde_json::from_value(old).expect("an older heartbeat parses");
        assert_eq!(hb.servers[0].usage.root_fs, None);
        assert_eq!(hb.servers[0].usage.net_rx_bps, None);
        assert_eq!(hb.services[0].storage_mi, None);
        assert_eq!(hb.services[0].replicas[0].ephemeral_mi, None);
        assert_eq!(hb.volumes[0].usage, None);
        // And absent fields aren't written back out (an older API sees what it knew).
        let out = serde_json::to_value(&hb.servers[0].usage).unwrap();
        assert_eq!(out, serde_json::json!({ "cpu": 1.0, "memoryGi": 2.0 }));
    }
}
