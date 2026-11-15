//! The daemon's config file (JSON). The `sandbox.sh` installer writes it
//! when a server joins.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use pyo_proto::sandbox::token::VerifyingKey;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Config {
    /// This server's ID in Pyo. Tokens name it.
    pub server_id: String,
    /// The daemon API listens here only. In production that's the
    /// server's tailnet address, so the API never faces the internet.
    pub listen: SocketAddr,
    /// The storage pool. See `storage` for the layout.
    #[serde(default = "default_root")]
    pub root: PathBuf,
    pub firecracker: PathBuf,
    /// Needed when running as root outside dev mode.
    #[serde(default)]
    pub jailer: Option<PathBuf>,
    /// The guest kernel.
    pub kernel: PathBuf,
    /// pyo-envd, for the detection test boot.
    pub envd: PathBuf,
    /// Pyo's public keys for checking tokens.
    pub keys: Vec<VerifyingKey>,
    /// Memory kept back for the host, in MiB. Default: a tenth of the
    /// host's memory, at least 512 MiB.
    #[serde(default)]
    pub host_reserve_mib: Option<u64>,
    /// How far the sum of the memory sandboxes asked for may go past what
    /// the host has (after the reserve). Guest memory only grows, so 1.0
    /// never overcommits.
    #[serde(default = "default_overcommit")]
    pub memory_overcommit: f64,
    /// A fixed Firecracker CPU template for every VM. Snapshots are keyed by
    /// it, so a paused sandbox resumes on any server with the same CPU
    /// vendor.
    #[serde(default)]
    pub cpu_template: Option<String>,
    /// The uid and gid the jailer drops each VM to (the `pyo-sandbox`
    /// system user).
    #[serde(default = "default_jail_id")]
    pub jail_uid: u32,
    #[serde(default = "default_jail_id")]
    pub jail_gid: u32,
    /// The cgroup under which the jailer puts each VM.
    #[serde(default = "default_cgroup")]
    pub cgroup_parent: String,
    /// Dev mode: run Firecracker directly as the current user, without the
    /// jailer, seccomp per jail or cgroups. Never on a real server.
    #[serde(default)]
    pub dev: bool,
}

fn default_root() -> PathBuf {
    PathBuf::from("/var/lib/pyo-sandbox")
}
fn default_overcommit() -> f64 {
    1.0
}
fn default_jail_id() -> u32 {
    // Replaced by the installer with the real `pyo-sandbox` user.
    65534
}
fn default_cgroup() -> String {
    "pyo-sandbox".into()
}

impl Config {
    pub fn load(path: &Path) -> anyhow::Result<Config> {
        let text =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let c: Config =
            serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        Ok(c)
    }

    /// Whether VMs run under the jailer. Root outside dev mode always does,
    /// and dev mode never does. A normal user outside dev mode is refused.
    pub fn use_jailer(&self) -> anyhow::Result<bool> {
        if self.dev {
            return Ok(false);
        }
        if !is_root() {
            bail!(
                "pyo-sandbox runs as root. Use dev mode to try it as a normal user without the jailer."
            );
        }
        if self.jailer.is_none() {
            bail!("The config has no jailer path. It's needed when running as root.");
        }
        Ok(true)
    }

    pub fn reserve_bytes(&self, memory_total: u64) -> u64 {
        match self.host_reserve_mib {
            Some(m) => m << 20,
            None => (memory_total / 10).max(512 << 20),
        }
    }
}

pub fn is_root() -> bool {
    // SAFETY: geteuid can't fail.
    unsafe { libc::geteuid() == 0 }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn minimal() -> Config {
        serde_json::from_value(serde_json::json!({
            "serverId": "srv1",
            "listen": "100.64.0.1:7070",
            "firecracker": "/usr/local/bin/firecracker",
            "kernel": "/var/lib/pyo-sandbox/kernels/vmlinux",
            "envd": "/usr/local/lib/pyo-sandbox/pyo-envd",
            "keys": [{"kid": "k1", "publicKey": "AAAA"}],
        }))
        .unwrap()
    }

    #[test]
    fn defaults() {
        let c = minimal();
        assert_eq!(c.root, PathBuf::from("/var/lib/pyo-sandbox"));
        assert_eq!(c.memory_overcommit, 1.0);
        assert!(!c.dev);
        assert_eq!(c.reserve_bytes(30 << 30), 3 << 30);
        assert_eq!(c.reserve_bytes(1 << 30), 512 << 20);
    }

    #[test]
    fn jailer_rules() {
        let mut c = minimal();
        c.dev = true;
        assert!(!c.use_jailer().unwrap());
        c.dev = false;
        if is_root() {
            assert!(c.use_jailer().is_err());
        } else {
            let e = c.use_jailer().unwrap_err().to_string();
            assert!(e.contains("root"), "{e}");
        }
    }
}
