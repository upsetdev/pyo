//! Pyo Sandbox, the control plane: records, keys, limits, metering and the
//! scheduler. Sandboxes run on dedicated sandbox servers (the `pyo-sandbox`
//! daemon), which clients then talk to directly with a short-lived token
//! Pyo signs, so Pyo isn't in the hot path.
//!
//! - [`keys`]: the installation's signing key and the tokens.
//! - [`join`]: a sandbox server's one-time join code and sandbox.sh.
//! - [`scheduler`]: which server a new sandbox runs on.
//! - [`limits`]: per-team caps, checked on create.
//! - [`daemon`]: Pyo's calls to a daemon over the tailnet.
//! - [`runner`]: reads each server's report, keeps the records in step,
//!   meters usage and ends sandboxes past their lifetime.
//! - [`api`]: the REST endpoints. [`mcp`]: the assistants' tools.
//!
//! Sandboxes belong to a team, with an optional project as a label. Sandbox
//! servers and templates belong to a team, or to the installation (no team),
//! which every team may use. Nothing here runs or shows until the first
//! sandbox server exists ([`available`]).

pub mod keys;
