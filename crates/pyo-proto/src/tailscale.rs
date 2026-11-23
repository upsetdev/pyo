//! Tailscale through the agent: what Pyo asks a cluster's agent to do on the
//! cluster's tailnet, and what comes back.
//!
//! Each cluster keeps its own Tailscale OAuth client, as a Secret in
//! pyo-system ([`SECRET`]). Pyo never holds it: the setup (one pasted API
//! access token), join keys, Tailscale Services and their approvals all
//! run in the agent, as signed commands. Answers carry no secret, except a
//! join key ([`KEY_FIELD`]), which Pyo passes into a join command once and
//! then wipes.

use serde::{Deserialize, Serialize};

/// The tag Pyo's servers and Services carry.
pub const TAG: &str = "tag:pyo";
/// Names Pyo keeps for itself: services' domains can't take them.
pub const RESERVED: [&str; 3] = ["pyo", "registry", "sandboxes"];
/// The cluster's OAuth client: a Secret in pyo-system, with
/// [`SECRET_ID`] and [`SECRET_KEY`].
pub const SECRET: &str = "pyo-tailscale";
pub const SECRET_ID: &str = "client-id";
pub const SECRET_KEY: &str = "client-secret";
/// Where a join key comes back in a [`crate::Command::TailscaleKey`] result.
pub const KEY_FIELD: &str = "authKey";

/// What a Service Pyo made says about itself (its comment): the
/// installation and the cluster whose client made it.
pub fn marker(install: &str, cluster: &str) -> String {
    format!("Made by Pyo ({install} {cluster})")
}

/// The marker Pyo used before each cluster had its own client: the
/// installation's only. The cluster that takes over the installation's
/// client counts these as its own too.
pub fn install_marker(install: &str) -> String {
    format!("Made by Pyo ({install})")
}

/// "server-1.tail1234.ts.net" → "tail1234.ts.net".
pub fn suffix_of(name: &str) -> Option<String> {
    let (_, rest) = name.trim_end_matches('.').split_once('.')?;
    rest.ends_with(".ts.net").then(|| rest.to_ascii_lowercase())
}

/// A name on the tailnet: one DNS label.
pub fn valid_name(n: &str) -> bool {
    !n.is_empty()
        && n.len() <= 63
        && n.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !n.starts_with('-')
        && !n.ends_with('-')
}

/// One of Pyo's own names that it keeps up to date: the one it has while
/// that isn't `taken` (someone else's Service, a machine, or another of
/// Pyo's names), else the first free one [`pick_name`] finds.
pub fn own_name(
    current: Option<&str>,
    base: &str,
    more: &str,
    taken: impl Fn(&str) -> bool,
) -> String {
    match current {
        Some(n) if !n.is_empty() && !taken(n) => n.to_owned(),
        _ => pick_name(base, more, taken),
    }
}

/// The first free name: `base`, then `base-<more>`, then `base-<more>-2`…
pub fn pick_name(base: &str, more: &str, taken: impl Fn(&str) -> bool) -> String {
    let cut = |x: &str| {
        x.chars()
            .take(63)
            .collect::<String>()
            .trim_end_matches('-')
            .to_owned()
    };
    let first = cut(base);
    if !taken(&first) {
        return first;
    }
    let second = cut(&format!("{base}-{more}"));
    if !taken(&second) {
        return second;
    }
    (2..)
        .map(|i| cut(&format!("{base}-{more}-{i}")))
        .find(|n| !taken(n))
        .unwrap_or_default()
}

/// What Tailscale takes as a key's description: letters, digits, spaces,
/// dashes and underscores, at most 50 characters. "Pyo: join web-1"
/// becomes "Pyo join web-1".
pub fn key_description(s: &str) -> String {
    let kept: String = s
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, ' ' | '-' | '_'))
        .collect();
    let mut out = kept.split_whitespace().collect::<Vec<_>>().join(" ");
    out.truncate(50);
    out.trim_end().to_owned()
}

/// One of Pyo's own names on the tailnet (only on the cluster Pyo runs on):
/// "dashboard", "registry" or "sandboxes".
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OwnName {
    pub kind: String,
    /// The name it has now, when it has one.
    #[serde(default)]
    pub current: Option<String>,
    /// "pyo", "registry" or "sandboxes".
    pub base: String,
    /// What makes it unique when `base` is taken (the cluster's name).
    pub more: String,
    /// Kept even when someone else has it ("Taken on your tailnet"),
    /// rather than picked again: the dashboard's and the registry's, which
    /// people may have bookmarked.
    #[serde(default)]
    pub keep: bool,
}

/// One of the cluster's servers on the tailnet.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncServer {
    pub server: String,
    /// Its stable node ID.
    pub node_id: String,
    /// The Services it advertises ("blog").
    #[serde(default)]
    pub advertised: Vec<String>,
}

/// The cluster's names on its tailnet, as Pyo wants them
/// ([`crate::Command::TailscaleSync`]). The agent makes each (marked
/// [`TailnetSync::mark`]), approves the cluster's servers as its hosts,
/// tags the servers, deletes the ones Pyo made that are no longer wanted,
/// and never touches a Service it didn't make.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TailnetSync {
    /// What marks the Services this cluster makes.
    pub mark: String,
    /// Older marks that are this cluster's too (the installation's, for the
    /// cluster that took over its client).
    #[serde(default)]
    pub adopt: Vec<String>,
    /// Pyo's own names, picked here when they have none yet.
    #[serde(default)]
    pub own: Vec<OwnName>,
    /// Services' tailnet domains on this cluster ("blog").
    #[serde(default)]
    pub domains: Vec<String>,
    /// Names Pyo made for this cluster before: deleted when no longer
    /// wanted (each read again first).
    #[serde(default)]
    pub made: Vec<String>,
    /// The tailnet's machines as the cluster's servers see them.
    #[serde(default)]
    pub peers: Vec<String>,
    #[serde(default)]
    pub servers: Vec<SyncServer>,
    /// Machines to take off the tailnet (node IDs of tag:pyo machines
    /// whose server was deleted).
    #[serde(default)]
    pub forget: Vec<String>,
    /// The cluster these names are for: the team's holder syncs the names
    /// of every cluster of its team. For the agent's logs only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cluster: Option<String>,
}

/// What became of an old OAuth client a setup was asked to delete
/// ([`crate::Command::TailscaleSetup`]'s `delete_clients`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OldClientDeleted {
    pub client_id: String,
    /// "deleted", "missing" (not on this tailnet, or deleted already),
    /// "kept" (the cluster keeps it now) or "failed".
    pub state: String,
    /// Why it failed, as people read it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// One name, as the agent left it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NameState {
    pub name: String,
    /// "dashboard", "registry", "sandboxes" or "domain".
    pub kind: String,
    /// Its VIPs, IPv4 then IPv6.
    #[serde(default)]
    pub addrs: Vec<String>,
    /// Why it isn't working, as people read it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// A machine Tailnet Lock keeps out until a signing device signs its key.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LockWaiting {
    pub name: String,
    pub sign_command: String,
}

/// What a [`TailnetSync`] did (its result's `data`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TailnetSynced {
    /// The tailnet's MagicDNS suffix ("tail1234.ts.net"), when seen.
    #[serde(default)]
    pub suffix: Option<String>,
    /// Every wanted name (own ones as picked).
    #[serde(default)]
    pub names: Vec<NameState>,
    /// Names deleted on the tailnet.
    #[serde(default)]
    pub deleted: Vec<String>,
    #[serde(default)]
    pub lock_waiting: Vec<LockWaiting>,
    /// Pyo's machines on the tailnet (tag:pyo).
    #[serde(default)]
    pub machines: Vec<Machine>,
    /// Machines taken off the tailnet, of [`TailnetSync::forget`].
    #[serde(default)]
    pub forgotten: Vec<String>,
}

/// One of Pyo's machines on the tailnet (tag:pyo).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Machine {
    pub node_id: String,
    pub hostname: String,
    /// Tailnet Lock waits for its key to be signed: the command that signs it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sign_command: Option<String>,
}

/// What a name typed for a tailnet domain is on the cluster's tailnet
/// ([`crate::Command::TailscaleCheckName`]'s `data`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NameCheck {
    /// "free", "ours" or "taken".
    pub state: String,
    #[serde(default)]
    pub suffix: Option<String>,
}

/// One check of the client (Test, or after saving it).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Check {
    pub name: String,
    pub ok: bool,
    pub message: String,
}

/// What [`crate::Command::TailscaleClient`] and
/// [`crate::Command::TailscaleTest`] say (their `data`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClientState {
    pub ok: bool,
    pub checks: Vec<Check>,
    #[serde(default)]
    pub client_id: String,
    /// The secret's last characters, to tell it apart.
    #[serde(default)]
    pub hint: String,
    #[serde(default)]
    pub suffix: Option<String>,
}

/// A call to a sandbox server's daemon on the cluster's tailnet
/// ([`crate::Command::SandboxCall`]): Pyo can't reach the tailnet when it
/// runs elsewhere (Pyo Cloud), so the cluster's agent makes it. Only the
/// daemon's own API, on its port, at a tailnet address.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SandboxCall {
    /// The server's tailnet address (100.64.0.0/10 or fd7a:115c:a1e0::/48).
    pub address: String,
    /// "GET", "POST", "PUT" or "DELETE".
    pub method: String,
    /// "/v1/…".
    pub path: String,
    /// The short-lived token Pyo signed for this call.
    pub token: String,
    /// JSON, or base64 bytes with `raw`.
    #[serde(default)]
    pub body: Option<String>,
    /// The body is bytes (base64), and so is the answer.
    #[serde(default)]
    pub raw: bool,
    #[serde(default)]
    pub content_type: Option<String>,
    pub timeout_secs: u64,
}

/// The daemon's answer ([`SandboxCall`]'s `data`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SandboxAnswer {
    /// 0 when it couldn't be reached.
    pub status: u16,
    /// Text, or base64 for a raw call.
    pub body: String,
    #[serde(default)]
    pub content_type: Option<String>,
}

/// Whether `ip` is an address on a tailnet: 100.64.0.0/10 or
/// fd7a:115c:a1e0::/48.
pub fn tailnet_ip(ip: &str) -> bool {
    match ip.parse::<std::net::IpAddr>() {
        Ok(std::net::IpAddr::V4(v)) => {
            let o = v.octets();
            o[0] == 100 && (64..128).contains(&o[1])
        }
        Ok(std::net::IpAddr::V6(v)) => {
            let s = v.segments();
            s[0] == 0xfd7a && s[1] == 0x115c && s[2] == 0xa1e0
        }
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_descriptions_are_what_tailscale_takes() {
        assert_eq!(key_description("Pyo: join web-1"), "Pyo join web-1");
        assert_eq!(
            key_description("Pyo: sandbox server sbx_1"),
            "Pyo sandbox server sbx_1"
        );
        let long = key_description(&format!("Pyo: join {}", "a".repeat(80)));
        assert_eq!(long.len(), 50);
    }

    #[test]
    fn names() {
        assert_eq!(
            suffix_of("server-1.tail1234.ts.net."),
            Some("tail1234.ts.net".into())
        );
        assert_eq!(suffix_of("example.com"), None);
        assert!(
            valid_name("blog-2") && !valid_name("Blog") && !valid_name("-a") && !valid_name("a.b")
        );
        let taken = ["pyo", "pyo-home"];
        assert_eq!(
            pick_name("pyo", "home", |n| taken.contains(&n)),
            "pyo-home-2"
        );
        assert_eq!(
            pick_name("registry", "home", |n| taken.contains(&n)),
            "registry"
        );
        assert_eq!(pick_name("pyo", "home", |n| n == "pyo"), "pyo-home");
        let long = "a".repeat(70);
        assert!(pick_name(&long, "x", |_| false).len() <= 63);
        assert_eq!(marker("abc", "cl_1"), "Made by Pyo (abc cl_1)");
        assert_eq!(install_marker("abc"), "Made by Pyo (abc)");
    }

    #[test]
    fn tailnet_addresses() {
        assert!(tailnet_ip("100.64.0.7") && tailnet_ip("100.127.255.1"));
        assert!(tailnet_ip("fd7a:115c:a1e0::1"));
        assert!(!tailnet_ip("100.128.0.1") && !tailnet_ip("10.0.0.1"));
        assert!(!tailnet_ip("169.254.169.254") && !tailnet_ip("localhost"));
    }
}
