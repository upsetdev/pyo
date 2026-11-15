//! TODO(gvisor): the gVisor driver, for servers without usable KVM, such
//! as Hetzner Cloud. Detection already finds runsc and reports
//! `runtime: gvisor`. The manager refuses gVisor sandboxes until this
//! lands. It needs root and a box where runsc runs (not this laptop).
//!
//! The design (PLAN.md "Runtime" and the gVisor spike, items 15 to 22):
//! - `runsc --platform=systrap`, never the KVM platform. runsc runs as
//!   root. One OCI bundle per sandbox, with the template's root
//!   filesystem read-only and an overlay for writes.
//! - envd runs in dial mode (`PYO_ENVD_DIAL=unix:/pyo/host.sock`) with
//!   `--host-uds=open`. It never listens, because a bound host socket
//!   blocks `runsc checkpoint`. The daemon listens on a per-sandbox
//!   `host.sock`, and envd's first frame on each connection is the ready
//!   signal. Before a checkpoint the daemon closes the listener and every
//!   connection.
//! - Templates are prepared root filesystems with cold starts (37 ms), not
//!   memory snapshots. Restores cost more and don't share pages.
//! - Pause and resume use `runsc checkpoint` and `runsc restore`. Images
//!   are small (15 MiB, 4.8 MiB with flate) and go to Garage as they are.
//!   The cache is keyed by the runsc release.
//! - Memory is the sandbox cgroup's `memory.current` (`cgroupsPath`), which
//!   matches the host within 1 MiB.
//! - Starts are CPU bound, about 13 per second per vCPU, so the scheduler
//!   spreads bursts.
