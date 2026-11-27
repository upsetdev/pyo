//! The Pyo Sandbox daemon. It runs on dedicated sandbox servers, outside
//! clusters, and turns Pyo's commands into Firecracker microVMs, or gVisor
//! sandboxes on servers without KVM. It gets Pyo's commands over its
//! outbound channel (`outbound`): Pyo never connects to it. Clients on the
//! tailnet talk to it directly with short-lived tokens Pyo signs, so Pyo
//! isn't in the hot path.

pub mod api;
pub mod capacity;
pub mod config;
pub mod detect;
pub mod edge;
pub mod envd;
pub mod firecracker;
pub mod gvisor;
pub mod manager;
pub mod network;
pub mod oci;
pub mod outbound;
pub mod rootfs;
pub mod storage;
pub mod tailnet;
pub mod templates;

/// Why `serve` stops on a server that joined at Pyo's tailnet door before
/// 0.52.0: Pyo no longer calls daemons, and its config has no channel.
pub const NEEDS_UPDATE: &str = "This server joined Pyo the old way and needs an update. Press Update on it in Pyo, then run the command it shows here.";

/// Epoch milliseconds.
pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
