//! Connection analytics (off by default): the collector
//! (`pyo-flows`, a DaemonSet in pyo-system) sends the API what it saw of
//! connections to the cluster's public TCP and UDP ports, a minute at a time.
//! Connection metadata only: who connected to which port, how long, how many
//! bytes; nothing inside a connection.

use serde::{Deserialize, Serialize};

/// A public port the collector watches: what clients connect to on a server.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FlowPort {
    pub port: u16,
    /// "TCP" or "UDP".
    pub protocol: String,
}

/// What the collector watches, from the API.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FlowPorts {
    pub ports: Vec<FlowPort>,
}

/// One minute of one client's connections to one port on one server.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FlowRecord {
    /// Start of the minute, seconds since 1970.
    pub minute: i64,
    pub port: u16,
    pub protocol: String,
    pub client: String,
    /// Connections (UDP: flows) that started this minute.
    pub connections: u32,
    /// Connections that ended this minute, and how long they lasted in
    /// total (known when the collector saw them start).
    pub ended: u32,
    pub duration_ms: u64,
    /// Client to server ("in") and back, counted when a connection ends.
    pub bytes_in: u64,
    pub bytes_out: u64,
    pub packets_in: u64,
    pub packets_out: u64,
}

/// A batch from one server's collector.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FlowBatch {
    pub node: String,
    pub records: Vec<FlowRecord>,
    /// The server counts bytes and packets (nf_conntrack_acct on).
    pub bytes_counted: bool,
    /// Connections left out since the last batch: more distinct clients in
    /// a minute than the collector keeps, or events the kernel dropped.
    #[serde(default)]
    pub overflow: u64,
}

/// The collector's image for an agent image: same registry and tag, named
/// `flows` (`…/pyo/agent:0.1.0` → `…/pyo/flows:0.1.0`). A digest-pinned
/// agent image gives no flows image; the release pins that one itself.
pub fn flows_image_for(agent_image: &str) -> Option<String> {
    let (repo, rest) = agent_image.rsplit_once('/')?;
    let tag = rest.strip_prefix("agent")?;
    (!tag.contains('@')).then(|| format!("{repo}/flows{tag}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_names() {
        assert_eq!(
            flows_image_for("127.0.0.1:5005/pyo/agent:dev").as_deref(),
            Some("127.0.0.1:5005/pyo/flows:dev")
        );
        assert_eq!(
            flows_image_for("ghcr.io/upsetdev/pyo/agent:0.2.0@sha256:ab"),
            None
        );
    }
}
