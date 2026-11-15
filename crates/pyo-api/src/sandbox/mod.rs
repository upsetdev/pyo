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
pub mod limits;
pub mod scheduler;

/// "1h", "1h30m", "90s".
pub fn format_ttl(secs: u64) -> String {
    let (h, m, s) = (secs / 3600, secs % 3600 / 60, secs % 60);
    let mut out = String::new();
    if h > 0 {
        out.push_str(&format!("{h}h"));
    }
    if m > 0 {
        out.push_str(&format!("{m}m"));
    }
    if s > 0 || out.is_empty() {
        out.push_str(&format!("{s}s"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lifetimes() {
        assert_eq!(format_ttl(3600), "1h");
        assert_eq!(format_ttl(5400), "1h30m");
        assert_eq!(format_ttl(90), "1m30s");
        assert_eq!(format_ttl(0), "0s");
    }
}
