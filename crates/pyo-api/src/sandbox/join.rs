//! A sandbox server's one-time join code, shared by every way a server
//! joins over the outbound channel (outbound/: `addSandboxServer`,
//! `createSandboxJoinCode`, **Update** and Hetzner sandbox servers, see
//! cloud/bootstrap.rs). A code is kept as its hash for 15 minutes and the
//! first ask burns it, whatever comes of it. The config the daemon gets
//! ([`daemon_config`]) is the same for every server.
//!
//! Until 0.52.0 a server joined at Pyo's tailnet door (`POST
//! /sandbox/join`), and Pyo called its daemon. A server that joined that
//! way takes no commands until **Update** moves it to the channel
//! (`super::NEEDS_UPDATE`).

use std::net::IpAddr;
use std::time::Duration;

use pyo_proto::sandbox::token::VerifyingKey;
use serde_json::{Value, json};
use sqlx::{PgPool, Postgres, Transaction};

use crate::{
    error::ApiResult,
    ids::{secret_token, sha256_hex},
    ratelimit::Limit,
};

/// How long a code works.
pub const CODE_TTL: Duration = Duration::from_secs(15 * 60);

/// Where sandbox.sh puts things on the server. The config Pyo hands over
/// points there.
pub const LIB_DIR: &str = "/usr/local/lib/pyo-sandbox";
pub const DATA_DIR: &str = "/var/lib/pyo-sandbox";

/// Wrong codes for one sandbox server.
pub const WRONG_CODE_SERVER: Limit = Limit {
    scope: "sandbox-join:server",
    max: 10,
    window_secs: 15 * 60,
    backoff_cap_secs: 0,
};
/// Wrong codes from anywhere.
pub const WRONG_CODE_ALL: Limit = Limit {
    scope: "sandbox-join:all",
    max: 60,
    window_secs: 15 * 60,
    backoff_cap_secs: 0,
};

/// A new code: 32 random bytes as hex.
pub fn new_code() -> String {
    secret_token()
}

/// Keeps a code (its hash) for a server, for [`CODE_TTL`]. Codes the server
/// had before stop working.
pub async fn keep(
    tx: &mut Transaction<'_, Postgres>,
    server_id: &str,
    code: &str,
) -> ApiResult<()> {
    sqlx::query("UPDATE pyo.sandbox_join_codes SET used_at = now() WHERE server_id = $1 AND used_at IS NULL")
        .bind(server_id)
        .execute(&mut **tx)
        .await?;
    sqlx::query(
        "INSERT INTO pyo.sandbox_join_codes (code_hash, server_id, expires_at)
         VALUES ($1, $2, now() + make_interval(secs => $3))",
    )
    .bind(sha256_hex(code))
    .bind(server_id)
    .bind(CODE_TTL.as_secs_f64())
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// Codes go a day after they expire.
pub async fn prune(db: &PgPool) -> sqlx::Result<()> {
    sqlx::query("DELETE FROM pyo.sandbox_join_codes WHERE expires_at < now() - interval '1 day'")
        .execute(db)
        .await?;
    Ok(())
}

/// Why Pyo refused a join.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    Unknown,
    Used,
    Expired,
    WrongServer,
    /// The server was removed, or joined already.
    Gone,
    /// The address it gave isn't a tailnet address.
    BadAddress,
    /// It can run no sandboxes: the detection's reason.
    NoRuntime(String),
}

impl Refusal {
    pub fn message(&self) -> String {
        match self {
            Refusal::Unknown => "Pyo doesn't know this code.".into(),
            Refusal::Used => "This code was used already. Make a new one in Pyo.".into(),
            Refusal::Expired => "This code expired. Make a new one in Pyo.".into(),
            Refusal::WrongServer => "This code is for another server.".into(),
            Refusal::Gone => {
                "This server was removed or has joined already. Add it again in Pyo.".into()
            }
            Refusal::BadAddress => "This server's tailnet address is missing or wrong.".into(),
            Refusal::NoRuntime(why) => format!("This server can't run sandboxes. {why}"),
        }
    }
}

/// A Tailscale address: where the daemon listens.
pub fn tailnet_address(a: &str) -> bool {
    match a.parse::<IpAddr>() {
        Ok(IpAddr::V4(v)) => {
            let [a, b, ..] = v.octets();
            a == 100 && (64..=127).contains(&b)
        }
        Ok(IpAddr::V6(v)) => {
            let s = v.segments();
            s[0] == 0xfd7a && s[1] == 0x115c && s[2] == 0xa1e0
        }
        Err(_) => false,
    }
}

/// The daemon's config, as `pyo-sandbox serve` reads it
/// (crates/pyo-sandbox/src/config.rs).
pub fn daemon_config(
    server_id: &str,
    address: &str,
    jail_uid: u32,
    jail_gid: u32,
    keys: &[VerifyingKey],
) -> Value {
    let listen = match address.parse::<IpAddr>() {
        Ok(IpAddr::V6(v)) => format!("[{v}]:{}", pyo_proto::sandbox::DAEMON_PORT),
        _ => format!("{address}:{}", pyo_proto::sandbox::DAEMON_PORT),
    };
    json!({
        "serverId": server_id,
        "listen": listen,
        "root": DATA_DIR,
        "firecracker": format!("{LIB_DIR}/firecracker"),
        "jailer": format!("{LIB_DIR}/jailer"),
        "kernel": format!("{LIB_DIR}/vmlinux"),
        "envd": format!("{LIB_DIR}/pyo-envd"),
        // sandbox.sh puts gVisor's runsc there on servers without KVM.
        "runsc": format!("{LIB_DIR}/runsc"),
        "keys": keys,
        "jailUid": jail_uid,
        "jailGid": jail_gid,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_tailnet_addresses() {
        for a in ["100.64.0.7", "100.127.1.1", "fd7a:115c:a1e0::9"] {
            assert!(tailnet_address(a), "{a}");
        }
        for a in [
            "",
            "10.0.0.1",
            "100.128.0.1",
            "203.0.113.7",
            "pyo.ts.net",
            "fd7a:115c:a1e1::1",
        ] {
            assert!(!tailnet_address(a), "{a}");
        }
    }

    #[test]
    fn the_config_is_what_the_daemon_reads() {
        let keys = [VerifyingKey {
            kid: "sbk_one".into(),
            public_key: "AAAA".into(),
        }];
        let v = daemon_config("sbs_one", "100.64.0.7", 998, 997, &keys);
        assert_eq!(v["listen"], "100.64.0.7:7070");
        assert_eq!(v["keys"][0]["publicKey"], "AAAA");
        assert_eq!(v["jailUid"], 998);
        assert_eq!(v["runsc"], "/usr/local/lib/pyo-sandbox/runsc");
        assert_eq!(
            daemon_config("s", "fd7a:115c:a1e0::9", 1, 1, &keys)["listen"],
            "[fd7a:115c:a1e0::9]:7070"
        );
    }

    #[test]
    fn a_new_server_fetches_sandbox_sh_off_the_tailnet() {
        let render = |base: &str| {
            crate::sandbox::outbound::Joining {
                server_id: "sbs_one".into(),
                team_id: None,
                name: "sbx".into(),
                code: "abc".into(),
                tailscale_key: "tskey-auth-1".into(),
                command_keys: "KEY1".into(),
                pyo_url: "https://pyo.tail1234.ts.net".into(),
                edge_domain: None,
                expires_at: 0,
            }
            .command(base, "0.4.0")
        };
        let pick = crate::join::pick_scripts_url_before_tailnet;
        // PYO_SCRIPTS_URL wins.
        assert!(
            render(&pick(Some("https://mirror.example.com/")))
                .starts_with("curl -fsSL https://mirror.example.com/sandbox.sh | bash -s -- ")
        );
        // Else this release's published sandbox.sh: on pyo.dev, or a
        // release candidate's on GitHub.
        for unset in [None, Some(" ")] {
            let c = render(&pick(unset));
            assert!(
                c.starts_with(&format!(
                    "curl -fsSL {}/sandbox.sh | bash -s -- ",
                    crate::join::published_scripts_url()
                )) && c.contains(" --version '0.4.0'"),
                "{c}"
            );
        }
    }

    #[test]
    fn refusals_are_plain() {
        for r in [
            Refusal::Unknown,
            Refusal::Used,
            Refusal::Expired,
            Refusal::WrongServer,
            Refusal::Gone,
            Refusal::BadAddress,
            Refusal::NoRuntime("No KVM and no gVisor.".into()),
        ] {
            let m = r.message();
            assert!(m.ends_with('.') && !m.contains(';'), "{m}");
        }
    }
}
