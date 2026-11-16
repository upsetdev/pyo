//! The Pyo Sandbox daemon. It runs on dedicated sandbox servers, outside
//! clusters, and turns Pyo's commands into Firecracker microVMs, or gVisor
//! sandboxes on servers without KVM. Clients
//! talk to it directly with short-lived tokens Pyo signs, so Pyo isn't in
//! the hot path.

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
pub mod rootfs;
pub mod storage;
pub mod templates;

/// Epoch milliseconds.
pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
