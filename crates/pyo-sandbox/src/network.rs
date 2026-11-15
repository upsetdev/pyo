//! TODO(network): sandbox networking. Needs root, so it comes with the
//! first real sandbox server. Until then every sandbox runs with
//! `network: none` and only vsock. The capability report says
//! `network: false`, and creating a sandbox with `network: internet` is
//! refused with a plain message.
//!
//! The design (PLAN.md "Network"):
//! - One tap device per VM (`pyo<n>`), made before Firecracker starts and
//!   handed to it as `network-interfaces` in vm.json. Under the jailer the
//!   tap is created in the host namespace and passed by name.
//! - Each VM gets a /30 from a private pool (100.100.0.0/16 or a range the
//!   installer picks that doesn't clash with the tailnet or cluster
//!   networks), NATed out through the host's default route.
//! - nftables, one table `inet pyo_sandbox`, with a set per sandbox:
//!   - always dropped: the tailnet (100.64.0.0/10), the host's own
//!     addresses, Pyo's API, cluster networks, 169.254.169.254 and other
//!     link-local and private ranges
//!   - egress allow and deny lists per sandbox (v0.4.x)
//!   - no traffic between sandboxes
//! - DNS: the daemon writes `/etc/resolv.conf` into the guest through envd
//!   after start, pointing at a resolver the rules allow.
//! - A restored snapshot keeps the guest's interface config, so every
//!   template snapshot uses the same guest address and the host side
//!   (tap plus NAT) is what differs per sandbox.
//! - gVisor servers get a network namespace per sandbox with a veth and a
//!   bridge, made by Pyo, with the same nftables rules (gVisor spike,
//!   item 22).
