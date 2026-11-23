//! How a sandbox server joins: the same pattern as a cloud server's
//! bootstrap code (cloud/bootstrap.rs).
//!
//! 1. A team admin adds a sandbox server (`addSandboxServer`). Pyo makes
//!    its row (joining), a one-time code ([`new_code`], kept as its hash,
//!    15 minutes) and a single-use Tailscale key, and shows one command:
//!    `curl -fsSL …/sandbox.sh | bash -s -- --pyo … --server … --code …`.
//! 2. sandbox.sh joins the tailnet with the key, installs the daemon and
//!    runs its detection, then asks Pyo's tailnet door (:5006, tailnet
//!    peers only) with the code: `POST /sandbox/join?server=<id>`
//!    ([`handle`]). The first ask burns the code, whatever comes of it.
//! 3. Pyo answers with the daemon's whole config ([`daemon_config`]): the
//!    server's ID, Pyo's verifying keys and where the daemon listens (its
//!    tailnet address). sandbox.sh writes it and starts the daemon.
//! 4. Pyo's runner reads the daemon's report over the tailnet, and the
//!    server turns ready.
//!
//! A server that can run neither Firecracker nor gVisor is refused, with
//! the detection's reason.

use crate::error::Query;
use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

use axum::{
    Router,
    extract::{ConnectInfo, State},
    http::{StatusCode, header},
    response::{IntoResponse, Response},
    routing::post,
};
use pyo_proto::sandbox::{CapabilityReport, Runtime, token::VerifyingKey};
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::{PgPool, Postgres, Transaction};

use crate::{
    AppState,
    cloud::bootstrap::tailnet_peer,
    error::ApiResult,
    ids::{secret_token, sha256_hex},
    ratelimit::{self, Limit},
    secrets::Sealer,
};

/// How long a code works.
pub const CODE_TTL: Duration = Duration::from_secs(15 * 60);
/// Where sandbox.sh asks, on Pyo's tailnet door.
pub const PATH: &str = "/sandbox/join";

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

/// Why Pyo refused an ask.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    NotTailnet,
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
            Refusal::NotTailnet => "Ask over the tailnet.".into(),
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

/// A code, burnt.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Claimed {
    pub server_id: String,
    pub expired: bool,
    /// The server's state, None once it's gone.
    pub state: Option<String>,
}

/// Whether a burnt code lets this server in.
pub fn verify(c: &Claimed, server: &str) -> Result<(), Refusal> {
    if c.expired {
        return Err(Refusal::Expired);
    }
    if c.server_id != server {
        return Err(Refusal::WrongServer);
    }
    if c.state.as_deref() != Some("joining") {
        return Err(Refusal::Gone);
    }
    Ok(())
}

/// A Tailscale address: where Pyo will reach the daemon.
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

/// Burns a code on its first use.
async fn claim(db: &PgPool, code: &str) -> sqlx::Result<Result<Claimed, Refusal>> {
    let hash = sha256_hex(code);
    let claimed: Option<Claimed> = sqlx::query_as(
        "UPDATE pyo.sandbox_join_codes c SET used_at = now()
          WHERE c.code_hash = $1 AND c.used_at IS NULL
         RETURNING c.server_id, c.expires_at <= now() AS expired,
                   (SELECT state FROM pyo.sandbox_servers WHERE id = c.server_id) AS state",
    )
    .bind(&hash)
    .fetch_optional(db)
    .await?;
    if let Some(c) = claimed {
        return Ok(Ok(c));
    }
    let known: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM pyo.sandbox_join_codes WHERE code_hash = $1)",
    )
    .bind(&hash)
    .fetch_one(db)
    .await?;
    Ok(Err(if known {
        Refusal::Used
    } else {
        Refusal::Unknown
    }))
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

/// What sandbox.sh sends with its code.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JoinBody {
    pub code: String,
    /// Its tailnet IP, where the daemon will listen.
    pub address: String,
    /// The `pyo-sandbox` system user the jailer drops VMs to.
    pub jail_uid: u32,
    pub jail_gid: u32,
    /// `pyo-sandbox detect`'s report.
    #[serde(default)]
    pub capability: Option<CapabilityReport>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Ask {
    pub server: String,
}

fn text(status: StatusCode, body: impl Into<String>) -> (StatusCode, String) {
    (status, body.into())
}

/// The answer to an ask: the daemon's config as JSON, or why not.
pub async fn answer(
    db: &PgPool,
    sealer: &Sealer,
    peer: IpAddr,
    server: &str,
    body: &[u8],
) -> (StatusCode, String) {
    if !tailnet_peer(peer) {
        tracing::warn!(%peer, %server, "sandbox server join refused: not from the tailnet");
        return text(StatusCode::FORBIDDEN, Refusal::NotTailnet.message());
    }
    let fits = |s: &str| {
        !s.is_empty() && s.len() <= 64 && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
    };
    let Ok(ask) = serde_json::from_slice::<JoinBody>(body) else {
        return text(
            StatusCode::BAD_REQUEST,
            "Ask with a code, the server's tailnet address and its user IDs.",
        );
    };
    let code = ask.code.trim();
    if !fits(server) || code.len() > 256 {
        return text(StatusCode::BAD_REQUEST, "Ask with a server and a code.");
    }
    let limits = [(WRONG_CODE_ALL, "all"), (WRONG_CODE_SERVER, server)];
    if let Err(e) = ratelimit::take(db, &limits).await {
        tracing::warn!(%server, "sandbox server join refused: too many wrong codes");
        return text(e.status, e.message);
    }
    let claimed = match claim(db, code).await {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(error = %e, "sandbox server join: couldn't check a code");
            return text(
                StatusCode::SERVICE_UNAVAILABLE,
                "Pyo couldn't check the code. Try again.",
            );
        }
    };
    if !matches!(claimed, Err(Refusal::Unknown)) {
        for (l, k) in &limits {
            let _ = ratelimit::forgive(db, l, k).await;
        }
    }
    let checked = claimed.and_then(|c| verify(&c, server)).and_then(|()| {
        if !tailnet_address(ask.address.trim()) {
            return Err(Refusal::BadAddress);
        }
        match &ask.capability {
            Some(c) if c.runtime == Runtime::None => Err(Refusal::NoRuntime(c.summary())),
            _ => Ok(()),
        }
    });
    if let Err(r) = checked {
        tracing::warn!(%server, "sandbox server join refused: {r:?}");
        if let Refusal::NoRuntime(why) = &r {
            let _ = sqlx::query(
                "UPDATE pyo.sandbox_servers SET state = 'refused', error = $2, capability = $3, updated_at = now() WHERE id = $1 AND state = 'joining'",
            )
            .bind(server)
            .bind(why)
            .bind(json!(ask.capability))
            .execute(db)
            .await;
            return text(StatusCode::UNPROCESSABLE_ENTITY, r.message());
        }
        return text(StatusCode::FORBIDDEN, r.message());
    }
    let keys = match super::keys::verifying_keys(db, sealer).await {
        Ok(k) => k,
        Err(e) => return text(e.status, e.message),
    };
    let address = ask.address.trim();
    let config = daemon_config(server, address, ask.jail_uid, ask.jail_gid, &keys);
    if let Err(e) = sqlx::query(
        "UPDATE pyo.sandbox_servers SET address = $2, capability = coalesce($3, capability), joined_at = now(),
                error = NULL, updated_at = now() WHERE id = $1",
    )
    .bind(server)
    .bind(address)
    .bind(ask.capability.as_ref().map(|c| json!(c)))
    .execute(db)
    .await
    {
        tracing::warn!(%server, error = %e, "sandbox server join: couldn't save its address");
        return text(StatusCode::SERVICE_UNAVAILABLE, "Pyo couldn't save this server. Make a new code in Pyo.");
    }
    tracing::info!(%server, %address, "sandbox server joined, waiting for its daemon");
    text(StatusCode::OK, config.to_string())
}

/// POST /sandbox/join?server=… on Pyo's tailnet door.
pub async fn handle(
    State(s): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Query(ask): Query<Ask>,
    body: axum::body::Bytes,
) -> Response {
    let (status, body) = answer(&s.db, &s.sealer, peer.ip(), &ask.server, &body).await;
    let kind = if status == StatusCode::OK {
        "application/json"
    } else {
        "text/plain; charset=utf-8"
    };
    (
        status,
        [
            (header::CONTENT_TYPE, kind),
            (header::CACHE_CONTROL, "no-store"),
        ],
        body,
    )
        .into_response()
}

/// The route, for the tailnet door's listener.
pub fn router(state: AppState) -> Router {
    Router::new()
        .route(PATH, post(handle))
        .layer(axum::extract::DefaultBodyLimit::max(64 * 1024))
        .with_state(state)
}

/// Single-quoted for the shell.
fn q(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// The command that makes a machine a sandbox server.
pub struct Command<'a> {
    /// Where scripts come from, off the tailnet the server isn't on yet
    /// ([`crate::join::scripts_url_before_tailnet`]).
    pub base: &'a str,
    /// Pyo's tailnet door, "pyo.tail1234.ts.net:5006".
    pub door: &'a str,
    pub server_id: &'a str,
    pub name: &'a str,
    pub code: &'a str,
    pub tailscale_key: &'a str,
    /// The Pyo release whose daemon it installs.
    pub version: &'a str,
    /// Serves sandbox ports at `<port>-<sandbox>.<domain>`.
    pub edge_domain: Option<&'a str>,
}

impl Command<'_> {
    pub fn render(&self) -> String {
        let mut c = format!(
            "curl -fsSL {}/sandbox.sh | bash -s -- --pyo {} --server {} --name {} --code {} --tailscale-key {} --version {}",
            self.base,
            q(self.door),
            q(self.server_id),
            q(self.name),
            q(self.code),
            q(self.tailscale_key),
            q(self.version),
        );
        if let Some(d) = self.edge_domain {
            c.push_str(&format!(" --edge-domain {}", q(d)));
        }
        c
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn claimed() -> Claimed {
        Claimed {
            server_id: "sbs_one".into(),
            expired: false,
            state: Some("joining".into()),
        }
    }

    #[test]
    fn a_code_is_bound_to_its_server() {
        assert_eq!(verify(&claimed(), "sbs_one"), Ok(()));
        assert_eq!(verify(&claimed(), "sbs_two"), Err(Refusal::WrongServer));
        let mut c = claimed();
        c.expired = true;
        assert_eq!(verify(&c, "sbs_one"), Err(Refusal::Expired));
        for state in [None, Some("ready"), Some("refused")] {
            let mut c = claimed();
            c.state = state.map(Into::into);
            assert_eq!(verify(&c, "sbs_one"), Err(Refusal::Gone));
        }
    }

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
    fn the_command_quotes_everything() {
        // The server isn't on the tailnet yet: this release's scripts on
        // pyo.dev.
        let base = crate::join::pick_scripts_url_before_tailnet(None);
        assert_eq!(base, crate::join::published_scripts_url());
        let c = Command {
            base: &base,
            door: "pyo.tail1234.ts.net:5006",
            server_id: "sbs_one",
            name: "it's sbx",
            code: "abc",
            tailscale_key: "tskey-auth-1",
            version: "0.4.0",
            edge_domain: None,
        };
        assert_eq!(
            c.render(),
            format!(
                "curl -fsSL {base}/sandbox.sh | bash -s -- --pyo 'pyo.tail1234.ts.net:5006' --server 'sbs_one' --name 'it'\\''s sbx' --code 'abc' --tailscale-key 'tskey-auth-1' --version '0.4.0'"
            )
        );
        let c = Command {
            edge_domain: Some("sbx.example.com"),
            ..c
        };
        assert!(
            c.render()
                .ends_with(" --version '0.4.0' --edge-domain 'sbx.example.com'"),
            "{}",
            c.render()
        );
    }

    #[test]
    fn a_new_server_fetches_sandbox_sh_off_the_tailnet() {
        let render = |base: &str| {
            Command {
                base,
                door: "pyo.tail1234.ts.net:5006",
                server_id: "sbs_one",
                name: "sbx",
                code: "abc",
                tailscale_key: "tskey-auth-1",
                version: "0.4.0",
                edge_domain: None,
            }
            .render()
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
            Refusal::NotTailnet,
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

    /// Against a throwaway Postgres: `DATABASE_URL=… cargo test -p pyo-api
    /// sandbox::join -- --ignored`.
    #[tokio::test]
    #[ignore = "needs DATABASE_URL (a throwaway Postgres)"]
    async fn the_door_burns_codes_and_hands_over_the_config() {
        let db = PgPool::connect(&std::env::var("DATABASE_URL").expect("DATABASE_URL"))
            .await
            .unwrap();
        crate::migrate::run(&db).await.unwrap();
        let sealer = Sealer::for_tests();
        super::super::keys::reset_cache();
        let run = &new_code()[..8];
        let team = format!("tm_sbxjoin{run}");
        sqlx::query("INSERT INTO pyo.teams (id, name, slug) VALUES ($1, 'Sbx', $1)")
            .bind(&team)
            .execute(&db)
            .await
            .unwrap();
        ratelimit::reset(&db, &WRONG_CODE_ALL, "all").await.unwrap();
        let server = async |n: &str| -> (String, String) {
            let id = format!("sbs_{n}{run}");
            sqlx::query("INSERT INTO pyo.sandbox_servers (id, team_id, name, state, created_by) VALUES ($1, $2, $1, 'joining', 'test')")
                .bind(&id)
                .bind(&team)
                .execute(&db)
                .await
                .unwrap();
            let code = new_code();
            let mut tx = db.begin().await.unwrap();
            keep(&mut tx, &id, &code).await.unwrap();
            tx.commit().await.unwrap();
            (id, code)
        };
        let body = |code: &str, address: &str| {
            json!({ "code": code, "address": address, "jailUid": 998, "jailGid": 998,
                    "capability": { "runtime": "firecracker", "kvm": "native", "arch": "x86_64", "cpus": 4, "memory": 1, "disk": 1 } })
            .to_string()
        };
        let tailnet: IpAddr = "100.64.0.9".parse().unwrap();

        // Not from the tailnet: refused, the code still works after.
        let (one, code) = server("one").await;
        let (st, _) = answer(
            &db,
            &sealer,
            "203.0.113.7".parse().unwrap(),
            &one,
            body(&code, "100.64.0.20").as_bytes(),
        )
        .await;
        assert_eq!(st, StatusCode::FORBIDDEN);
        // A bad address burns it.
        let (two, code2) = server("two").await;
        let (st, msg) = answer(
            &db,
            &sealer,
            tailnet,
            &two,
            body(&code2, "10.0.0.1").as_bytes(),
        )
        .await;
        assert_eq!(
            (st, msg),
            (StatusCode::FORBIDDEN, Refusal::BadAddress.message())
        );
        let (_, msg) = answer(
            &db,
            &sealer,
            tailnet,
            &two,
            body(&code2, "100.64.0.21").as_bytes(),
        )
        .await;
        assert_eq!(msg, Refusal::Used.message());
        // The right ask: the config, once.
        let (st, cfg) = answer(
            &db,
            &sealer,
            tailnet,
            &one,
            body(&code, "100.64.0.20").as_bytes(),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{cfg}");
        let cfg: Value = serde_json::from_str(&cfg).unwrap();
        assert_eq!(cfg["serverId"], json!(one));
        assert_eq!(cfg["listen"], "100.64.0.20:7070");
        assert!(
            cfg["keys"][0]["publicKey"]
                .as_str()
                .is_some_and(|k| k.len() == 43)
        );
        let addr: Option<String> =
            sqlx::query_scalar("SELECT address FROM pyo.sandbox_servers WHERE id = $1")
                .bind(&one)
                .fetch_one(&db)
                .await
                .unwrap();
        assert_eq!(addr.as_deref(), Some("100.64.0.20"));
        let (_, msg) = answer(
            &db,
            &sealer,
            tailnet,
            &one,
            body(&code, "100.64.0.20").as_bytes(),
        )
        .await;
        assert_eq!(msg, Refusal::Used.message());
        // Another server's code.
        let (three, code3) = server("three").await;
        let (_, msg) = answer(
            &db,
            &sealer,
            tailnet,
            &one,
            body(&code3, "100.64.0.22").as_bytes(),
        )
        .await;
        assert_eq!(msg, Refusal::WrongServer.message());
        let _ = three;
        // A server that can run nothing is refused with the reason.
        let (four, code4) = server("four").await;
        let none = json!({ "code": code4, "address": "100.64.0.23", "jailUid": 1, "jailGid": 1,
                           "capability": { "runtime": "none", "kvm": "none", "arch": "x86_64", "cpus": 1, "memory": 1, "disk": 1, "reason": "No KVM and no gVisor." } });
        let (st, msg) = answer(&db, &sealer, tailnet, &four, none.to_string().as_bytes()).await;
        assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY);
        assert!(msg.contains("No KVM and no gVisor."), "{msg}");
        let state: String =
            sqlx::query_scalar("SELECT state FROM pyo.sandbox_servers WHERE id = $1")
                .bind(&four)
                .fetch_one(&db)
                .await
                .unwrap();
        assert_eq!(state, "refused");
        // Expired.
        let (five, code5) = server("five").await;
        sqlx::query("UPDATE pyo.sandbox_join_codes SET expires_at = now() - interval '1 minute' WHERE server_id = $1")
            .bind(&five)
            .execute(&db)
            .await
            .unwrap();
        let (_, msg) = answer(
            &db,
            &sealer,
            tailnet,
            &five,
            body(&code5, "100.64.0.24").as_bytes(),
        )
        .await;
        assert_eq!(msg, Refusal::Expired.message());
        // Wrong codes are rate-limited.
        let (six, code6) = server("six").await;
        for _ in 0..WRONG_CODE_SERVER.max {
            let (_, msg) = answer(
                &db,
                &sealer,
                tailnet,
                &six,
                body(&new_code(), "100.64.0.25").as_bytes(),
            )
            .await;
            assert_eq!(msg, Refusal::Unknown.message());
        }
        let (st, _) = answer(
            &db,
            &sealer,
            tailnet,
            &six,
            body(&code6, "100.64.0.25").as_bytes(),
        )
        .await;
        assert_eq!(st, StatusCode::TOO_MANY_REQUESTS);
        ratelimit::reset(&db, &WRONG_CODE_SERVER, &six)
            .await
            .unwrap();
        prune(&db).await.unwrap();
        sqlx::query("DELETE FROM pyo.teams WHERE id = $1")
            .bind(&team)
            .execute(&db)
            .await
            .unwrap();
        ratelimit::reset(&db, &WRONG_CODE_ALL, "all").await.unwrap();
    }
}
