//! A sandbox server that works over HTTPS (Pyo Cloud): it joins with a
//! one-time code, holds a key of its own, and long-polls Pyo for its
//! commands. Pyo never calls it. See docs/cloud-sandbox-servers.md.
//!
//! Every request after the join is signed with the server's key, the way
//! agents sign theirs on Pyo Cloud (pyo-proto `agentauth`, the same
//! headers). Every command comes signed by Pyo's signer under the
//! `pyosbx1` prefix (pyo-proto `signing`, [`crate::signing`] with the
//! `command-signing` feature): the daemon runs only the signed JSON, and
//! only when it's for its own server and team.

use serde::{Deserialize, Serialize};

use super::CapabilityReport;

/// Where the endpoints are, under the Pyo URL the join command gives.
pub const API_PREFIX: &str = "/api/sandbox-agent/v1";
/// Joins with the code (unsigned).
pub const JOIN_PATH: &str = "/join";
/// A new key, asked for with the current one.
pub const RENEW_KEY_PATH: &str = "/renew-key";
/// The long poll for commands (`?wait=<seconds>`, at most 55).
pub const COMMANDS_PATH: &str = "/commands";
/// The server's report, every 15 seconds.
pub const REPORT_PATH: &str = "/report";
/// Random per daemon process. A new one on a poll means the daemon
/// restarted, so commands handed to the one before go out again.
pub const INSTANCE_HEADER: &str = "x-pyo-sandbox-instance";
/// Pyo's 401 for a key that opens nothing any more. The daemon stops
/// polling and reporting when it gets it, and keeps its sandboxes.
pub const KEY_GONE: &str =
    "This server's key is unknown, expired or revoked. Join it again with a new command from Pyo.";
/// How a daemon's answer to a command it refused starts (code
/// `forbidden`), followed by ": <why>.". Pyo passes it on as it is.
pub const COMMAND_REFUSED: &str = "The sandbox server refused the command";
/// Where the daemon answers HTTPS on its tailnet name: the same API and
/// pages as [`super::DAEMON_PORT`], for browsers and SDKs on the tailnet.
pub const TAILNET_PORT: u16 = 7443;
/// The most of each output stream an exec over the channel keeps.
pub const OUTPUT_CAP: usize = 1 << 20;
/// The largest file read or written over the channel.
pub const FILE_CAP: usize = 2 << 20;
/// The longest an exec over the channel runs. One asking for longer is
/// stopped then, with `timedOut`.
pub const EXEC_MAX_MS: u64 = 5 * 60 * 1000;
/// The largest command Pyo queues (its JSON).
pub const MAX_COMMAND: usize = 3 << 20;

/// What the daemon sends with its code, over HTTPS.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JoinRequest {
    /// The server's id from the join command (`sbs_…`).
    pub server_id: String,
    pub code: String,
    /// The Ed25519 key the daemon made, base64url (32 bytes).
    pub public_key: String,
    /// Its tailnet IP.
    pub address: String,
    /// Its hostname as the tailnet lists it (`tailscale status`, Self.HostName).
    pub hostname: String,
    /// Its full name on the tailnet, like `sbx-1.tail1234.ts.net`.
    pub tailnet_name: String,
    /// The `pyo-sandbox` system user the jailer drops VMs to.
    pub jail_uid: u32,
    pub jail_gid: u32,
    /// `pyo-sandbox detect`'s report.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capability: Option<CapabilityReport>,
}

/// Pyo's answer to a join.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JoinAnswer {
    pub server_id: String,
    pub team_id: String,
    /// The daemon's key id, sent with every request (`pyo-agent-key`).
    pub key_id: String,
    /// When the key stops working (ms since the epoch). Renew at half.
    pub not_after: i64,
    /// The keys commands are signed with (base64url, comma separated). The
    /// daemon trusts the join command's `--command-key` and refuses to go
    /// on when these differ from it.
    pub command_keys: String,
    /// The daemon's config, as on a self-hosted Pyo, plus its outbound part.
    pub config: serde_json::Value,
}

/// A new key, signed with the current one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RenewKey {
    pub public_key: String,
}

/// A key's id and when it stops working (ms since the epoch).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KeyAnswer {
    pub key_id: String,
    pub not_after: i64,
}

/// A command for the daemon, as the long poll hands it out. The daemon
/// runs `signed.command` (a [`super::DaemonCommand`]'s JSON) once
/// `signed` checks out for its server and team, and posts its
/// [`super::DaemonResult`] to `/commands/{id}`. A refused command is
/// answered with an `error` result whose code is `forbidden`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CommandEnvelope {
    pub id: String,
    /// The command's `type`.
    pub kind: String,
    pub signed: crate::SignedCommandWire,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn channel_commands_read_as_pyo_sends_them() {
        use crate::sandbox::{DaemonCommand, DaemonResult};
        let c: DaemonCommand = serde_json::from_value(serde_json::json!({
            "type": "exec", "id": "sbx_1", "request": { "cmd": "echo hi", "timeoutMs": 1000 }
        }))
        .unwrap();
        assert_eq!(c.sandbox_id(), Some("sbx_1"));
        let c: DaemonCommand =
            serde_json::from_value(serde_json::json!({ "type": "writeFile", "id": "sbx_1", "path": "/a", "data": "aGk=" }))
                .unwrap();
        assert!(matches!(c, DaemonCommand::WriteFile { .. }));
        let r = serde_json::to_value(DaemonResult::Written { bytes: 2 }).unwrap();
        assert_eq!(r, serde_json::json!({ "type": "written", "bytes": 2 }));
        let r = serde_json::to_value(DaemonResult::File { data: "aGk=".into() }).unwrap();
        assert_eq!(r["type"], "file");
    }

    #[test]
    fn a_join_reads_as_the_daemon_sends_it() {
        let j: JoinRequest = serde_json::from_value(serde_json::json!({
            "serverId": "sbs_one", "code": "abc", "publicKey": "AAAA", "address": "100.64.0.7",
            "hostname": "sbx-1", "tailnetName": "sbx-1.tail1234.ts.net", "jailUid": 998, "jailGid": 997,
        }))
        .unwrap();
        assert_eq!(j.tailnet_name, "sbx-1.tail1234.ts.net");
        assert!(j.capability.is_none());
        let back = serde_json::to_value(&j).unwrap();
        assert!(back.get("capability").is_none());
    }
}
