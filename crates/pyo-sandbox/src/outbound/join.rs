//! `pyo-sandbox join`: a server joins Pyo over HTTPS with its one-time
//! code. sandbox.sh runs it once the server is on the team's tailnet.
//!
//! sandbox.sh hands over the code and the command key in a file only
//! root can read ([`JoinFile`], `/etc/pyo-sandbox/join.json`), never in
//! arguments. The daemon makes an Ed25519 key, posts its public half with
//! the code and the server's tailnet address and names, and gets its
//! config back. It writes that config, its key and its state (store.rs),
//! and removes the join file.
//!
//! Pyo answers 409 while the team's tailnet doesn't list the server yet:
//! the join asks again every few seconds until the code runs out. A
//! refusal (403, 422, 429 and the like) ends it with Pyo's reason.

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, bail};
use hyper::{Method, StatusCode};
use pyo_proto::agentauth::RequestKey;
use pyo_proto::sandbox::CapabilityReport;
use pyo_proto::sandbox::outbound::{self as wire, JoinAnswer, JoinRequest};
use pyo_proto::signing::TrustedKeys;
use serde::{Deserialize, Serialize};

use super::http::Http;
use super::store::{KeyFile, State, Store};

/// Where sandbox.sh puts the join file.
pub const JOIN_FILE: &str = "/etc/pyo-sandbox/join.json";
/// How long a code works: the join stops asking then.
const CODE_LIFE: Duration = Duration::from_secs(15 * 60);

/// What sandbox.sh hands the daemon for its join (root only, 0600).
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JoinFile {
    /// The join command's `--pyo-url`.
    pub pyo_url: String,
    /// Its `--server` (`sbs_…`).
    pub server_id: String,
    /// Its `--code`.
    pub code: String,
    /// Its `--command-key`: the keys commands are signed with
    /// (base64url, comma separated).
    pub command_key: String,
    /// The `pyo-sandbox` system user the jailer drops VMs to.
    pub jail_uid: u32,
    pub jail_gid: u32,
}

impl std::fmt::Debug for JoinFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JoinFile")
            .field("pyo_url", &self.pyo_url)
            .field("server_id", &self.server_id)
            .finish_non_exhaustive()
    }
}

/// The server on its tailnet, as `tailscale status --json` has it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tailnet {
    pub address: String,
    pub hostname: String,
    /// Like `sbx-1.tail1234.ts.net` (no trailing dot).
    pub name: String,
}

/// Reads `tailscale status --json`.
pub fn parse_status(json: &[u8]) -> anyhow::Result<Tailnet> {
    #[derive(Deserialize)]
    struct Status {
        #[serde(rename = "Self")]
        me: Option<Peer>,
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "PascalCase")]
    struct Peer {
        #[serde(default)]
        host_name: String,
        #[serde(rename = "DNSName", default)]
        dns_name: String,
        #[serde(rename = "TailscaleIPs", default)]
        ips: Vec<String>,
    }
    let s: Status = serde_json::from_slice(json).context("Tailscale's status can't be read.")?;
    let me = s.me.context("Tailscale's status has no Self.")?;
    let address = me
        .ips
        .iter()
        .find(|a| a.parse::<std::net::Ipv4Addr>().is_ok())
        .or(me.ips.first())
        .cloned()
        .context("This server has no tailnet address yet. Join the tailnet first.")?;
    let name = me.dns_name.trim_end_matches('.').to_ascii_lowercase();
    if name.is_empty() || me.host_name.is_empty() {
        bail!("Tailscale doesn't give this server a name yet. Check that MagicDNS is on.");
    }
    Ok(Tailnet {
        address,
        hostname: me.host_name,
        name,
    })
}

/// The server on its tailnet, from the `tailscale` command
/// (`PYO_SANDBOX_TAILSCALE` names another one).
pub async fn tailnet() -> anyhow::Result<Tailnet> {
    let bin = std::env::var("PYO_SANDBOX_TAILSCALE").unwrap_or_else(|_| "tailscale".into());
    let out = tokio::process::Command::new(&bin)
        .args(["status", "--json"])
        .output()
        .await
        .with_context(|| format!("running {bin} status"))?;
    if !out.status.success() {
        bail!(
            "Tailscale isn't running here: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    parse_status(&out.stdout)
}

/// Keys as a sorted list, to compare two lists.
fn key_set(keys: &str) -> Vec<String> {
    let mut v: Vec<String> = keys
        .split(',')
        .map(str::trim)
        .filter(|k| !k.is_empty())
        .map(str::to_owned)
        .collect();
    v.sort();
    v.dedup();
    v
}

/// How a join goes, for tests.
pub struct Options {
    /// How long to wait between asks while the tailnet doesn't list the
    /// server yet.
    pub retry: Duration,
    pub deadline: Duration,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            retry: Duration::from_secs(5),
            deadline: CODE_LIFE,
        }
    }
}

/// Joins with the code in `join_file`, then writes the daemon's config to
/// `config` and its key and state next to its pool. Prints what happens.
pub async fn join(
    config: &Path,
    join_file: &Path,
    capability: Option<CapabilityReport>,
    tailnet: &Tailnet,
    opts: Options,
) -> anyhow::Result<JoinAnswer> {
    let j: JoinFile = serde_json::from_slice(
        &std::fs::read(join_file).with_context(|| format!("reading {}", join_file.display()))?,
    )
    .with_context(|| format!("parsing {}", join_file.display()))?;
    TrustedKeys::parse(&j.command_key)
        .map_err(|e| anyhow::anyhow!("The command key in the join command can't be read: {e}"))?;
    let http = Http::new(&j.pyo_url)?;
    let pkcs8 = RequestKey::generate_pkcs8().map_err(anyhow::Error::msg)?;
    let key = RequestKey::from_pkcs8(&pkcs8).map_err(anyhow::Error::msg)?;
    let ask = JoinRequest {
        server_id: j.server_id.clone(),
        code: j.code.clone(),
        public_key: key.public_key(),
        address: tailnet.address.clone(),
        hostname: tailnet.hostname.clone(),
        tailnet_name: tailnet.name.clone(),
        jail_uid: j.jail_uid,
        jail_gid: j.jail_gid,
        capability,
    };
    let body = serde_json::to_vec(&ask)?;
    let path = http.path(wire::JOIN_PATH);
    let deadline = tokio::time::Instant::now() + opts.deadline;
    let mut waiting = false;
    let answer: JoinAnswer = loop {
        let sent = http
            .send(
                Method::POST,
                &path,
                &[],
                body.clone(),
                Duration::from_secs(30),
            )
            .await;
        match sent {
            Ok((s, b)) if s.is_success() => {
                break serde_json::from_slice(&b).context("Pyo's answer can't be read.")?;
            }
            Ok((StatusCode::CONFLICT, _)) => {
                if !waiting {
                    println!("Waiting for your tailnet to list this server…");
                    waiting = true;
                }
            }
            Ok((s, b)) if s.is_server_error() => {
                tracing::debug!(status = %s, "Pyo can't take the join yet: {}", super::message(&b));
            }
            Ok((_, b)) => bail!("Pyo refused this server. {}", super::message(&b)),
            Err(e) => tracing::debug!("couldn't reach Pyo: {e:#}"),
        }
        if tokio::time::Instant::now() + opts.retry > deadline {
            bail!(
                "The join code ran out before Pyo took this server. Copy a fresh command from Pyo and run it again."
            );
        }
        tokio::time::sleep(opts.retry).await;
    };
    if answer.server_id != j.server_id || answer.config["serverId"] != j.server_id.as_str() {
        bail!(
            "Pyo's answer is for another server. Copy a fresh command from Pyo and run it again."
        );
    }
    if key_set(&answer.command_keys) != key_set(&j.command_key) {
        bail!(
            "Pyo's command keys aren't the ones in the join command. Copy a fresh command from Pyo and run it again."
        );
    }
    if answer
        .config
        .get("outbound")
        .is_none_or(|o| o["teamId"] != answer.team_id.as_str())
    {
        bail!(
            "Pyo's answer has no config for this mode. Update the server and run the command again."
        );
    }
    // The config first, so the pool's place is the one it names.
    crate::edge::write_private(config, &serde_json::to_vec_pretty(&answer.config)?)?;
    let cfg = crate::config::Config::load(config)?;
    let store = Store::new(&cfg.root);
    // The same server keeps the numbers it ran: they only grow.
    let last_seq = store
        .state()
        .ok()
        .filter(|s| s.server_id == answer.server_id)
        .map_or(0, |s| s.last_seq);
    store.save_key(&KeyFile::new(
        &answer.key_id,
        answer.not_after,
        crate::now_ms() as i64,
        &pkcs8,
    ))?;
    store.save_state(&State {
        server_id: answer.server_id.clone(),
        team_id: answer.team_id.clone(),
        command_keys: j.command_key.clone(),
        last_seq,
    })?;
    // The code is burnt: nothing in it is any use now.
    let _ = std::fs::remove_file(join_file);
    println!("Joined Pyo as {}.", answer.server_id);
    Ok(answer)
}
