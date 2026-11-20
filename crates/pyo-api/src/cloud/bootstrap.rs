//! How a server Pyo made gets its join details: never in its user data.
//!
//! The user data holds two one-time values ([`super::user_data`]): a
//! single-use Tailscale key and a single-use code made here ([`new_code`]).
//! The code is random, kept only as its hash, expires after [`CODE_TTL`],
//! and is bound to the cloud server, its cluster (or the sandbox server it
//! becomes) and its role (join.sh, connect.sh or sandbox.sh). Its join
//! details wait here sealed until then ([`keep`]). A sandbox server's
//! details get a fresh sandbox join code when they're handed over
//! (`sandbox::join`), so its 15 minutes start then.
//!
//! On its first boot the server joins the tailnet with the key, then asks
//! Pyo's tailnet door (pyo-forward's :5006, which admits tailnet peers
//! only) with the code: `POST /cloud/bootstrap` ([`handle`]). The first ask
//! burns the code, whatever comes of it. Pyo answers only:
//! - a peer on the tailnet, by the real address of the connection, never a
//!   header: a tailnet address, or pyo-forward on a server's host, which
//!   admitted the tailnet peer by its own real address ([`tailnet_peer`]);
//! - a code it knows, unused and unexpired, for that server, cluster and
//!   role ([`verify`]).
//!
//! Wrong codes are rate-limited. The server then shows as Joining.

use crate::error::Query;
use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

use axum::{
    Router,
    body::Bytes,
    extract::{ConnectInfo, State},
    http::{StatusCode, header},
    response::{IntoResponse, Response},
    routing::post,
};
use serde::Deserialize;
use sqlx::{PgPool, Postgres, Transaction};

use crate::{
    AppState,
    error::ApiResult,
    ids::{secret_token, sha256_hex},
    ratelimit::{self, Limit},
    secrets::Sealer,
};

/// How long a code works.
pub const CODE_TTL: Duration = Duration::from_secs(15 * 60);
/// Where servers ask, on Pyo's tailnet door.
pub const PATH: &str = "/cloud/bootstrap";

/// Wrong codes for one cloud server.
pub const WRONG_CODE_SERVER: Limit = Limit {
    scope: "cloud-bootstrap:server",
    max: 10,
    window_secs: 15 * 60,
    backoff_cap_secs: 0,
};
/// Wrong codes from anywhere: every ask comes through pyo-forward, so
/// there's no address to tell askers apart.
pub const WRONG_CODE_ALL: Limit = Limit {
    scope: "cloud-bootstrap:all",
    max: 60,
    window_secs: 15 * 60,
    backoff_cap_secs: 0,
};

/// What the server runs once it has its details.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    /// join.sh: a new server of a cluster.
    Join,
    /// connect.sh: a new cluster's first server.
    Connect,
    /// sandbox.sh: a sandbox server.
    Sandbox,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Role::Join => "join",
            Role::Connect => "connect",
            Role::Sandbox => "sandbox",
        }
    }
}

/// A server's join details: the script and its arguments. The boot script
/// reads them as lines, `script=<url>` then `arg=<value>` each, and runs
/// the script in its own shell with them, so they're never on a command
/// line nor in a file.
#[derive(Debug, Clone, PartialEq)]
pub struct Material {
    pub script: String,
    pub args: Vec<String>,
}

impl Material {
    /// From a script's flags and values (join.rs `args`, `connect_args`).
    pub fn new(script: String, args: &[(&str, Option<String>)]) -> Self {
        let mut flat = Vec::new();
        for (flag, value) in args {
            flat.push((*flag).to_owned());
            flat.extend(value.clone());
        }
        Self { script, args: flat }
    }

    /// The lines Pyo answers with. A value with a line break can't be one.
    pub fn render(&self) -> anyhow::Result<String> {
        let mut out = format!("script={}\n", self.script);
        for a in &self.args {
            out.push_str(&format!("arg={a}\n"));
        }
        anyhow::ensure!(
            out.lines().count() == self.args.len() + 1 && !out.contains(['\r', '\0']),
            "a join detail has a line break"
        );
        Ok(out)
    }
}

/// A new code: 32 random bytes as hex.
pub fn new_code() -> String {
    secret_token()
}

/// Keeps a code (its hash) and the details it opens, sealed, for
/// [`CODE_TTL`]. After the cloud server's row, in the same transaction.
/// `owner` is its cluster, or the sandbox server it becomes.
pub async fn keep(
    tx: &mut Transaction<'_, Postgres>,
    sealer: &Sealer,
    code: &str,
    cloud_server_id: &str,
    owner: &str,
    role: Role,
    material: &Material,
) -> ApiResult<()> {
    let sealed = sealer.seal(&material.render()?)?;
    let (cluster, sandbox) = match role {
        Role::Sandbox => (None, Some(owner)),
        Role::Join | Role::Connect => (Some(owner), None),
    };
    sqlx::query(
        "INSERT INTO pyo.cloud_bootstrap (code_hash, cloud_server_id, cluster_id, sandbox_server_id, role, material_sealed, expires_at)
         VALUES ($1, $2, $3, $4, $5, $6, now() + make_interval(secs => $7))",
    )
    .bind(sha256_hex(code))
    .bind(cloud_server_id)
    .bind(cluster)
    .bind(sandbox)
    .bind(role.as_str())
    .bind(sealed)
    .bind(CODE_TTL.as_secs_f64())
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// Drops the details of a server being deleted: its code opens nothing.
pub async fn forget(tx: &mut Transaction<'_, Postgres>, cloud_server_id: &str) -> ApiResult<()> {
    sqlx::query("UPDATE pyo.cloud_bootstrap SET material_sealed = NULL WHERE cloud_server_id = $1")
        .bind(cloud_server_id)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

/// Expired details go at once, their codes a day later (until then, a late
/// ask is told the code expired).
pub async fn prune(db: &PgPool) -> sqlx::Result<()> {
    sqlx::query(
        "UPDATE pyo.cloud_bootstrap SET material_sealed = NULL WHERE material_sealed IS NOT NULL AND expires_at < now()",
    )
    .execute(db)
    .await?;
    sqlx::query("DELETE FROM pyo.cloud_bootstrap WHERE expires_at < now() - interval '1 day'")
        .execute(db)
        .await?;
    Ok(())
}

/// Whether a connection's real peer is on the tailnet: a Tailscale address
/// (100.64.0.0/10, fd7a:115c:a1e0::/48), or a process on a server's host
/// (k3s's pod network, at the server's own end: pyo-forward, whose :5006
/// admits tailnet peers only). Pods, Pyo's gateway included, and every
/// other address are refused. Headers never count.
pub fn tailnet_peer(ip: IpAddr) -> bool {
    match ip.to_canonical() {
        IpAddr::V4(v) => {
            let [a, b, ..] = v.octets();
            (a == 100 && (64..=127).contains(&b)) || ratelimit::from_server_host(&ip)
        }
        IpAddr::V6(v) => {
            let s = v.segments();
            (s[0] == 0xfd7a && s[1] == 0x115c && s[2] == 0xa1e0) || ratelimit::from_server_host(&ip)
        }
    }
}

/// Why Pyo refused an ask.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    NotTailnet,
    Unknown,
    Used,
    Expired,
    WrongServer,
    WrongCluster,
    WrongRole,
    /// The server is being deleted, or failed.
    Gone,
}

impl Refusal {
    /// For Pyo's log.
    pub fn log(self) -> &'static str {
        match self {
            Refusal::NotTailnet => "the ask didn't come from the tailnet",
            Refusal::Unknown => "no such code",
            Refusal::Used => "the code was used already",
            Refusal::Expired => "the code expired",
            Refusal::WrongServer => "the code is for another server",
            Refusal::WrongCluster => "the code is for another cluster",
            Refusal::WrongRole => "the code is for another role",
            Refusal::Gone => "the server is being deleted or failed",
        }
    }

    /// For the server's /var/log/pyo-join.log.
    pub fn message(self) -> &'static str {
        match self {
            Refusal::NotTailnet => "Ask over the tailnet.",
            Refusal::Unknown => "Pyo doesn't know this code.",
            Refusal::Used => "This code was used already. Delete this server and make a new one.",
            Refusal::Expired => "This code expired. Delete this server and make a new one.",
            Refusal::WrongServer | Refusal::WrongCluster | Refusal::WrongRole => {
                "This code is for another server."
            }
            Refusal::Gone => "This server is being deleted.",
        }
    }
}

/// What a server asks with, besides its code.
#[derive(Debug, Clone, Deserialize)]
pub struct Ask {
    /// Its cloud server ID ("cs_…").
    pub server: String,
    /// Its cluster, or the sandbox server it becomes ("sbs_…").
    pub cluster: String,
    pub role: Role,
}

/// A code, burnt.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Claimed {
    pub cloud_server_id: String,
    pub cluster_id: String,
    pub role: String,
    pub expired: bool,
    pub material_sealed: Option<String>,
    /// The cloud server's state, None once it's gone.
    pub state: Option<String>,
}

/// Whether a burnt code lets this ask in.
pub fn verify(c: &Claimed, ask: &Ask) -> Result<(), Refusal> {
    if c.expired {
        return Err(Refusal::Expired);
    }
    if c.cloud_server_id != ask.server {
        return Err(Refusal::WrongServer);
    }
    if c.cluster_id != ask.cluster {
        return Err(Refusal::WrongCluster);
    }
    if c.role != ask.role.as_str() {
        return Err(Refusal::WrongRole);
    }
    let alive = matches!(c.state.as_deref(), Some("creating" | "booting" | "joining"));
    if !alive || c.material_sealed.is_none() {
        return Err(Refusal::Gone);
    }
    Ok(())
}

/// Burns a code on its first use, whatever comes of it: the row as it was.
/// Used already, or unknown, otherwise.
async fn claim(db: &PgPool, code: &str) -> sqlx::Result<Result<Claimed, Refusal>> {
    let hash = sha256_hex(code);
    let claimed: Option<Claimed> = sqlx::query_as(
        "WITH prev AS (
           SELECT code_hash, material_sealed FROM pyo.cloud_bootstrap
            WHERE code_hash = $1 AND used_at IS NULL FOR UPDATE
         )
         UPDATE pyo.cloud_bootstrap c SET used_at = now(), material_sealed = NULL
           FROM prev
          WHERE c.code_hash = prev.code_hash
         RETURNING c.cloud_server_id, coalesce(c.cluster_id, c.sandbox_server_id, '') AS cluster_id,
                   c.role, c.expires_at <= now() AS expired,
                   prev.material_sealed,
                   (SELECT state FROM pyo.cloud_servers WHERE id = c.cloud_server_id) AS state",
    )
    .bind(&hash)
    .fetch_optional(db)
    .await?;
    if let Some(c) = claimed {
        return Ok(Ok(c));
    }
    let known: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM pyo.cloud_bootstrap WHERE code_hash = $1)",
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

fn text(status: StatusCode, body: impl Into<String>) -> (StatusCode, String) {
    (status, body.into())
}

/// The answer to an ask: the join details, or why not.
pub async fn answer(
    db: &PgPool,
    sealer: &Sealer,
    peer: IpAddr,
    ask: &Ask,
    code: &str,
) -> (StatusCode, String) {
    if !tailnet_peer(peer) {
        tracing::warn!(%peer, server = %ask.server, "cloud server setup refused: {}", Refusal::NotTailnet.log());
        return text(StatusCode::FORBIDDEN, Refusal::NotTailnet.message());
    }
    let code = code.trim();
    let fits = |s: &str| {
        !s.is_empty() && s.len() <= 64 && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
    };
    if !fits(&ask.server) || !fits(&ask.cluster) || code.len() > 256 {
        return text(
            StatusCode::BAD_REQUEST,
            "Ask with a server, a cluster, a role and a code.",
        );
    }
    let limits = [
        (WRONG_CODE_ALL, "all"),
        (WRONG_CODE_SERVER, ask.server.as_str()),
    ];
    if let Err(e) = ratelimit::take(db, &limits).await {
        tracing::warn!(server = %ask.server, "cloud server setup refused: too many wrong codes");
        return text(e.status, e.message);
    }
    let claimed = match claim(db, code).await {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(error = %e, "cloud server setup: couldn't check a code");
            return text(
                StatusCode::SERVICE_UNAVAILABLE,
                "Pyo couldn't check the code. Trying again.",
            );
        }
    };
    // Only wrong codes count.
    if !matches!(claimed, Err(Refusal::Unknown)) {
        for (l, k) in &limits {
            let _ = ratelimit::forgive(db, l, k).await;
        }
    }
    let material = claimed.and_then(|c| {
        verify(&c, ask)?;
        c.material_sealed.ok_or(Refusal::Gone)
    });
    let sealed = match material {
        Ok(m) => m,
        Err(r) => {
            tracing::warn!(server = %ask.server, role = ask.role.as_str(), "cloud server setup refused: {}", r.log());
            return text(StatusCode::FORBIDDEN, r.message());
        }
    };
    let mut material = match sealer.open(&sealed) {
        Ok(m) => m,
        Err(e) => {
            tracing::warn!(server = %ask.server, error = %e, "cloud server setup: couldn't open its details");
            return text(
                StatusCode::FORBIDDEN,
                "Pyo couldn't open this server's details. Delete it and make a new one.",
            );
        }
    };
    // A sandbox server's join code is made now, so sandbox.sh has its full
    // 15 minutes from here.
    if ask.role == Role::Sandbox {
        match sandbox_code(db, &ask.cluster).await {
            Ok(code) => material.push_str(&format!("arg=--code\narg={code}\n")),
            Err(e) => {
                tracing::warn!(server = %ask.server, error = %e.message, "cloud server setup: couldn't make a sandbox join code");
                return text(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "Pyo couldn't make this server's join code. Delete it and make a new one.",
                );
            }
        }
    }
    if let Err(e) = sqlx::query(
        "UPDATE pyo.cloud_servers SET state = 'joining', error = NULL, updated_at = now() WHERE id = $1 AND state IN ('creating', 'booting')",
    )
    .bind(&ask.server)
    .execute(db)
    .await
    {
        tracing::warn!(server = %ask.server, error = %e, "cloud server setup: couldn't mark it joining");
    }
    tracing::info!(server = %ask.server, role = ask.role.as_str(), "cloud server setup: details handed over");
    text(StatusCode::OK, material)
}

/// A new sandbox join code for a sandbox server Pyo made.
async fn sandbox_code(db: &PgPool, sandbox_server: &str) -> ApiResult<String> {
    let code = crate::sandbox::join::new_code();
    let mut tx = db.begin().await?;
    crate::sandbox::join::keep(&mut tx, sandbox_server, &code).await?;
    tx.commit().await?;
    Ok(code)
}

/// POST /cloud/bootstrap?server=…&cluster=…&role=join|connect|sandbox on
/// Pyo's tailnet door, with the code as the body.
pub async fn handle(
    State(s): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Query(ask): Query<Ask>,
    body: Bytes,
) -> Response {
    let code = String::from_utf8_lossy(&body);
    let (status, body) = answer(&s.db, &s.sealer, peer.ip(), &ask, &code).await;
    (
        status,
        [
            (header::CONTENT_TYPE, "text/plain; charset=utf-8"),
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
        .layer(axum::extract::DefaultBodyLimit::max(1024))
        .with_state(state)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn only_tailnet_peers() {
        // Tailscale's addresses.
        assert!(tailnet_peer(ip("100.64.0.7")));
        assert!(tailnet_peer(ip("100.127.255.1")));
        assert!(tailnet_peer(ip("fd7a:115c:a1e0::1234")));
        assert!(tailnet_peer(ip("::ffff:100.100.1.2")));
        // pyo-forward on a server's host: the pod network's server end.
        assert!(tailnet_peer(ip("10.42.0.1")));
        assert!(tailnet_peer(ip("10.42.3.0")));
        // Pods (Pyo's gateway, services), other networks, the internet.
        for p in [
            "10.42.0.17",
            "10.42.3.9",
            "10.43.0.1",
            "192.168.1.1",
            "172.16.0.1",
            "127.0.0.1",
            "203.0.113.7",
            "100.128.0.1",
            "100.63.255.255",
            "2001:db8::1",
            "fd7a:115c:a1e1::1",
        ] {
            assert!(!tailnet_peer(ip(p)), "{p}");
        }
    }

    fn claimed() -> Claimed {
        Claimed {
            cloud_server_id: "cs_one".into(),
            cluster_id: "cl_one".into(),
            role: "join".into(),
            expired: false,
            material_sealed: Some("sb1:x".into()),
            state: Some("booting".into()),
        }
    }

    fn ask(server: &str, cluster: &str, role: Role) -> Ask {
        Ask {
            server: server.into(),
            cluster: cluster.into(),
            role,
        }
    }

    #[test]
    fn a_code_is_bound_to_its_server_cluster_and_role() {
        let ok = ask("cs_one", "cl_one", Role::Join);
        assert_eq!(verify(&claimed(), &ok), Ok(()));
        assert_eq!(
            verify(&claimed(), &ask("cs_two", "cl_one", Role::Join)),
            Err(Refusal::WrongServer)
        );
        assert_eq!(
            verify(&claimed(), &ask("cs_one", "cl_two", Role::Join)),
            Err(Refusal::WrongCluster)
        );
        assert_eq!(
            verify(&claimed(), &ask("cs_one", "cl_one", Role::Connect)),
            Err(Refusal::WrongRole)
        );
        let mut c = claimed();
        c.expired = true;
        assert_eq!(verify(&c, &ok), Err(Refusal::Expired));
        let mut c = claimed();
        c.state = Some("removing".into());
        assert_eq!(verify(&c, &ok), Err(Refusal::Gone));
        let mut c = claimed();
        c.material_sealed = None;
        assert_eq!(verify(&c, &ok), Err(Refusal::Gone));
    }

    #[test]
    fn details_are_lines() {
        let m = Material::new(
            "https://pyo.tail1234.ts.net/install/join.sh".into(),
            &[
                ("--server", Some("100.64.0.1".into())),
                ("--token", Some("abc123.0123456789abcdef".into())),
                ("--builder", None),
                ("--name", Some("it's web 1".into())),
            ],
        );
        assert_eq!(
            m.render().unwrap(),
            "script=https://pyo.tail1234.ts.net/install/join.sh\narg=--server\narg=100.64.0.1\narg=--token\narg=abc123.0123456789abcdef\narg=--builder\narg=--name\narg=it's web 1\n"
        );
        let bad = Material::new("x".into(), &[("--name", Some("a\nscript=evil".into()))]);
        assert!(bad.render().is_err());
        let bad = Material::new("x\r".into(), &[]);
        assert!(bad.render().is_err());
    }

    #[test]
    fn codes_are_random_and_long() {
        let (a, b) = (new_code(), new_code());
        assert_ne!(a, b);
        assert_eq!(a.len(), 64);
        assert!(a.bytes().all(|b| b.is_ascii_hexdigit()));
    }

    /// Against a throwaway Postgres: `DATABASE_URL=… cargo test -p pyo-api
    /// cloud::bootstrap -- --ignored`.
    #[tokio::test]
    #[ignore = "needs DATABASE_URL (a throwaway Postgres)"]
    async fn the_endpoint_burns_codes_and_refuses_the_rest() {
        let db = PgPool::connect(&std::env::var("DATABASE_URL").expect("DATABASE_URL"))
            .await
            .unwrap();
        crate::migrate::run(&db).await.unwrap();
        let sealer = Sealer::for_tests();
        let run = &new_code()[..8];
        let (team, cluster) = (format!("tm_boot{run}"), format!("cl_boot{run}"));
        sqlx::query("INSERT INTO pyo.teams (id, name, slug) VALUES ($1, 'Boot', $1)")
            .bind(&team)
            .execute(&db)
            .await
            .unwrap();
        sqlx::query("INSERT INTO pyo.clusters (id, team_id, name, agent_token_hash) VALUES ($1, $2, $1, 'x')")
            .bind(&cluster)
            .bind(&team)
            .execute(&db)
            .await
            .unwrap();
        for l in [WRONG_CODE_ALL] {
            ratelimit::reset(&db, &l, "all").await.unwrap();
        }
        let material = Material::new(
            "https://pyo.tail1234.ts.net/install/join.sh".into(),
            &[("--token", Some("abc123.0123456789abcdef".into()))],
        );
        // A cloud server with a code, `expired` or not.
        let server = async |n: &str, expired: bool| -> (String, String) {
            let id = format!("cs_{n}{run}");
            sqlx::query(
                "INSERT INTO pyo.cloud_servers (id, team_id, provider, cluster_id, name, location, server_type, monthly, remote_id, state, created_by)
                 VALUES ($1, $2, 'hetzner', $3, $1, 'fsn1', 'cx22', 4.29, '1', 'booting', 'test')",
            )
            .bind(&id)
            .bind(&team)
            .bind(&cluster)
            .execute(&db)
            .await
            .unwrap();
            let code = new_code();
            let mut tx = db.begin().await.unwrap();
            keep(
                &mut tx,
                &sealer,
                &code,
                &id,
                &cluster,
                Role::Join,
                &material,
            )
            .await
            .unwrap();
            tx.commit().await.unwrap();
            if expired {
                sqlx::query("UPDATE pyo.cloud_bootstrap SET expires_at = now() - interval '1 minute' WHERE cloud_server_id = $1")
                    .bind(&id)
                    .execute(&db)
                    .await
                    .unwrap();
            }
            (id, code)
        };
        let tailnet = ip("100.64.0.9");
        let state = async |id: &str| -> String {
            sqlx::query_scalar("SELECT state FROM pyo.cloud_servers WHERE id = $1")
                .bind(id)
                .fetch_one(&db)
                .await
                .unwrap()
        };

        // Not from the tailnet: refused, and the code still works after.
        let (one, code) = server("one", false).await;
        let a = ask(&one, &cluster, Role::Join);
        for p in ["203.0.113.7", "10.42.0.17", "192.168.1.20"] {
            let (st, body) = answer(&db, &sealer, ip(p), &a, &code).await;
            assert_eq!(st, StatusCode::FORBIDDEN, "{p}");
            assert_eq!(body, Refusal::NotTailnet.message());
        }
        // Burnt on first use: the details once, then never again.
        let (st, body) = answer(&db, &sealer, tailnet, &a, &format!("{code}\n")).await;
        assert_eq!(st, StatusCode::OK, "{body}");
        assert_eq!(body, material.render().unwrap());
        assert_eq!(state(&one).await, "joining");
        let left: Option<String> = sqlx::query_scalar(
            "SELECT material_sealed FROM pyo.cloud_bootstrap WHERE cloud_server_id = $1",
        )
        .bind(&one)
        .fetch_one(&db)
        .await
        .unwrap();
        assert_eq!(left, None, "the details are gone once handed over");
        let (st, body) = answer(&db, &sealer, tailnet, &a, &code).await;
        assert_eq!(st, StatusCode::FORBIDDEN);
        assert_eq!(body, Refusal::Used.message());

        // Expired.
        let (two, code) = server("two", true).await;
        let (st, body) = answer(
            &db,
            &sealer,
            tailnet,
            &ask(&two, &cluster, Role::Join),
            &code,
        )
        .await;
        assert_eq!(
            (st, body.as_str()),
            (StatusCode::FORBIDDEN, Refusal::Expired.message())
        );
        assert_eq!(state(&two).await, "booting");

        // The wrong role burns it too.
        let (three, code) = server("three", false).await;
        let (st, body) = answer(
            &db,
            &sealer,
            tailnet,
            &ask(&three, &cluster, Role::Connect),
            &code,
        )
        .await;
        assert_eq!(
            (st, body.as_str()),
            (StatusCode::FORBIDDEN, "This code is for another server.")
        );
        let (_, body) = answer(
            &db,
            &sealer,
            tailnet,
            &ask(&three, &cluster, Role::Join),
            &code,
        )
        .await;
        assert_eq!(body, Refusal::Used.message());
        // Another server's code.
        let (four, code) = server("four", false).await;
        let (_, body) = answer(
            &db,
            &sealer,
            tailnet,
            &ask(&three, &cluster, Role::Join),
            &code,
        )
        .await;
        assert_eq!(body, "This code is for another server.");
        assert_eq!(state(&four).await, "booting");

        // A server being deleted: its code opens nothing.
        let (five, code) = server("five", false).await;
        let mut tx = db.begin().await.unwrap();
        forget(&mut tx, &five).await.unwrap();
        tx.commit().await.unwrap();
        let (_, body) = answer(
            &db,
            &sealer,
            tailnet,
            &ask(&five, &cluster, Role::Join),
            &code,
        )
        .await;
        assert_eq!(body, Refusal::Gone.message());

        // Wrong codes: refused, then rate-limited, the right one too.
        let (six, code) = server("six", false).await;
        let a = ask(&six, &cluster, Role::Join);
        for _ in 0..WRONG_CODE_SERVER.max {
            let (st, body) = answer(&db, &sealer, tailnet, &a, &new_code()).await;
            assert_eq!(
                (st, body.as_str()),
                (StatusCode::FORBIDDEN, Refusal::Unknown.message())
            );
        }
        let (st, _) = answer(&db, &sealer, tailnet, &a, &new_code()).await;
        assert_eq!(st, StatusCode::TOO_MANY_REQUESTS);
        let (st, _) = answer(&db, &sealer, tailnet, &a, &code).await;
        assert_eq!(st, StatusCode::TOO_MANY_REQUESTS, "nothing more is checked");
        // Unburnt: once the window ends, it works.
        ratelimit::reset(&db, &WRONG_CODE_SERVER, &six)
            .await
            .unwrap();
        let (st, _) = answer(&db, &sealer, tailnet, &a, &code).await;
        assert_eq!(st, StatusCode::OK);

        // A sandbox server: its details come with a fresh sandbox join code,
        // made as they're handed over, which sandbox.sh then redeems.
        let sbs = format!("sbs_boot{run}");
        sqlx::query("INSERT INTO pyo.sandbox_servers (id, team_id, name, state, created_by) VALUES ($1, $2, $1, 'joining', 'test')")
            .bind(&sbs)
            .bind(&team)
            .execute(&db)
            .await
            .unwrap();
        let seven = format!("cs_seven{run}");
        sqlx::query(
            "INSERT INTO pyo.cloud_servers (id, team_id, provider, sandbox_server_id, name, location, server_type, monthly, remote_id, state, created_by)
             VALUES ($1, $2, 'hetzner', $3, $1, 'fsn1', 'cx22', 4.29, '7', 'booting', 'test')",
        )
        .bind(&seven)
        .bind(&team)
        .bind(&sbs)
        .execute(&db)
        .await
        .unwrap();
        let sandbox_material = Material::new(
            "https://pyo.tail1234.ts.net/install/sandbox.sh".into(),
            &[
                ("--server", Some(sbs.clone())),
                ("--version", Some("0.4.0".into())),
            ],
        );
        let code = new_code();
        let mut tx = db.begin().await.unwrap();
        keep(
            &mut tx,
            &sealer,
            &code,
            &seven,
            &sbs,
            Role::Sandbox,
            &sandbox_material,
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();
        let (st, body) = answer(
            &db,
            &sealer,
            tailnet,
            &ask(&seven, &cluster, Role::Sandbox),
            &code,
        )
        .await;
        assert_eq!(
            (st, body.as_str()),
            (StatusCode::FORBIDDEN, "This code is for another server."),
            "a cluster's ID isn't the sandbox server's"
        );
        let code = new_code();
        let mut tx = db.begin().await.unwrap();
        keep(
            &mut tx,
            &sealer,
            &code,
            &seven,
            &sbs,
            Role::Sandbox,
            &sandbox_material,
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();
        let (st, body) = answer(
            &db,
            &sealer,
            tailnet,
            &ask(&seven, &sbs, Role::Sandbox),
            &code,
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{body}");
        let base = sandbox_material.render().unwrap();
        assert!(body.starts_with(&base), "{body}");
        let join_code = body[base.len()..]
            .strip_prefix("arg=--code\narg=")
            .and_then(|r| r.strip_suffix('\n'))
            .unwrap_or_else(|| panic!("no join code in {body}"));
        assert_eq!(join_code.len(), 64);
        let live: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM pyo.sandbox_join_codes WHERE server_id = $1 AND used_at IS NULL AND code_hash = $2",
        )
        .bind(&sbs)
        .bind(sha256_hex(join_code))
        .fetch_one(&db)
        .await
        .unwrap();
        assert_eq!(live, 1, "sandbox.sh can redeem it");
        assert_eq!(state(&seven).await, "joining");

        // Expired details go at once, their rows a day later.
        prune(&db).await.unwrap();
        let left: Option<String> = sqlx::query_scalar(
            "SELECT material_sealed FROM pyo.cloud_bootstrap WHERE cloud_server_id = $1",
        )
        .bind(&two)
        .fetch_one(&db)
        .await
        .unwrap();
        assert_eq!(left, None);

        sqlx::query("DELETE FROM pyo.teams WHERE id = $1")
            .bind(&team)
            .execute(&db)
            .await
            .unwrap();
        ratelimit::reset(&db, &WRONG_CODE_ALL, "all").await.unwrap();
    }
}
