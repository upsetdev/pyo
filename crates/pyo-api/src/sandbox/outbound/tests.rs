//! Outbound sandbox servers. The unit tests run anywhere. The rest run
//! against a throwaway Postgres, with a stand-in holder agent for Tailscale
//! keys and a local signer:
//! `DATABASE_URL=… cargo test -p pyo-api --lib sandbox::outbound::tests -- --ignored`.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use axum::{
    Router,
    body::Body,
    http::{Method, Request, StatusCode},
};
use pyo_proto::agentauth::RequestKey;
use pyo_proto::sandbox::{
    CapabilityReport, DaemonCommand, DaemonResult, ErrorCode, outbound as wire,
};
use pyo_proto::signing::{SandboxClaims, Signer as KeySigner, TrustedKeys};
use serde_json::{Value, json};
use sqlx::PgPool;
use tower::ServiceExt as _;

use super::*;
use crate::{AppState, auth::User};

// ── Without a database ─────────────────────────────────────────────────────

#[test]
fn a_name_is_on_the_teams_tailnet_only() {
    assert!(on_tailnet("sbx-1.tail1234.ts.net", "tail1234.ts.net"));
    assert!(on_tailnet("SBX-1.tail1234.ts.net.", "tail1234.ts.net"));
    for (name, suffix) in [
        ("sbx-1.tail9999.ts.net", "tail1234.ts.net"),
        ("tail1234.ts.net", "tail1234.ts.net"),
        ("a.b.tail1234.ts.net", "tail1234.ts.net"),
        ("sbx-1xtail1234.ts.net", "tail1234.ts.net"),
        ("sbx_1.tail1234.ts.net", "tail1234.ts.net"),
        ("sbx-1.tail1234.ts.net", ""),
        ("", "tail1234.ts.net"),
    ] {
        assert!(!on_tailnet(name, suffix), "{name} on {suffix}");
    }
}

#[test]
fn only_listed_machines() {
    let m = json!([{ "nodeId": "n1", "hostname": "sbx-1" }, { "nodeId": "n2", "hostname": "web-1" }]);
    assert!(listed(Some(&m), "sbx-1"));
    assert!(listed(Some(&m), "SBX-1"));
    assert!(!listed(Some(&m), "sbx-2"));
    assert!(!listed(Some(&m), ""));
    assert!(!listed(None, "sbx-1"));
}

fn found() -> Found {
    Found {
        server_id: "sbs_one".into(),
        expired: false,
        used: false,
        state: Some("joining".into()),
        team_id: Some("team_one".into()),
        outbound: Some(true),
        suffix: Some("tail1234.ts.net".into()),
        machines: Some(json!([{ "nodeId": "n1", "hostname": "sbx-1" }])),
    }
}

fn ask(server: &str, code: &str, public_key: &str, hostname: &str, tailnet_name: &str) -> wire::JoinRequest {
    wire::JoinRequest {
        server_id: server.into(),
        code: code.into(),
        public_key: public_key.into(),
        address: "100.64.0.20".into(),
        hostname: hostname.into(),
        tailnet_name: tailnet_name.into(),
        jail_uid: 998,
        jail_gid: 998,
        capability: Some(capability(pyo_proto::sandbox::Runtime::Firecracker)),
    }
}

fn capability(runtime: pyo_proto::sandbox::Runtime) -> CapabilityReport {
    serde_json::from_value(json!({
        "runtime": runtime, "kvm": "native", "arch": "x86_64", "cpus": 4, "memory": 1, "disk": 1,
        "reason": "No KVM and no gVisor."
    }))
    .unwrap()
}

fn public_key() -> String {
    RequestKey::from_pkcs8(&RequestKey::generate_pkcs8().unwrap()).unwrap().public_key()
}

#[test]
fn a_join_is_checked_in_order() {
    let pk = public_key();
    let good = ask("sbs_one", "c", &pk, "sbx-1", "sbx-1.tail1234.ts.net");
    assert_eq!(check(&found(), &good), Ok(()));
    let door = |r| Err(Refusal::Door(r));
    let mut f = found();
    f.used = true;
    assert_eq!(check(&f, &good), door(join::Refusal::Used));
    let mut f = found();
    f.expired = true;
    assert_eq!(check(&f, &good), door(join::Refusal::Expired));
    assert_eq!(
        check(&found(), &ask("sbs_two", "c", &pk, "sbx-1", "sbx-1.tail1234.ts.net")),
        door(join::Refusal::WrongServer)
    );
    for f in [
        Found { state: Some("ready".into()), ..found() },
        Found { state: None, ..found() },
        Found { outbound: Some(false), ..found() },
        Found { team_id: None, ..found() },
    ] {
        assert_eq!(check(&f, &good), door(join::Refusal::Gone));
    }
    assert_eq!(
        check(&found(), &ask("sbs_one", "c", "not-a-key", "sbx-1", "sbx-1.tail1234.ts.net")),
        Err(Refusal::BadKey)
    );
    let mut a = good.clone();
    a.address = "10.0.0.1".into();
    assert_eq!(check(&found(), &a), door(join::Refusal::BadAddress));
    let mut a = good.clone();
    a.capability = Some(capability(pyo_proto::sandbox::Runtime::None));
    assert!(matches!(check(&found(), &a), Err(Refusal::Door(join::Refusal::NoRuntime(_)))));
    assert_eq!(
        check(&found(), &ask("sbs_one", "c", &pk, "sbx-1", "sbx-1.tail9999.ts.net")),
        Err(Refusal::OtherTailnet)
    );
    assert_eq!(
        check(&found(), &ask("sbs_one", "c", &pk, "sbx-2", "sbx-2.tail1234.ts.net")),
        Err(Refusal::NotSeen)
    );
    assert_eq!(check(&Found { suffix: None, ..found() }, &good), Err(Refusal::NotSeen));
}

#[test]
fn refusals_are_plain() {
    for r in [Refusal::BadKey, Refusal::OtherTailnet, Refusal::NotSeen, Refusal::Door(join::Refusal::Used)] {
        let m = r.message();
        assert!(m.ends_with('.') && !m.contains(';'), "{m}");
    }
    assert_eq!(Refusal::NotSeen.status(), StatusCode::CONFLICT);
    assert_eq!(Refusal::OtherTailnet.status(), StatusCode::FORBIDDEN);
}

#[test]
fn the_command_quotes_everything() {
    let j = Joining {
        server_id: "sbs_one".into(),
        team_id: "team_one".into(),
        name: "it's sbx".into(),
        code: "abc".into(),
        tailscale_key: "tskey-auth-1".into(),
        command_keys: "KEY1,KEY2".into(),
        pyo_url: "https://agent.example.com".into(),
        edge_domain: None,
        expires_at: 0,
    };
    assert_eq!(
        j.command("https://scripts.example.com", "0.51.1"),
        "curl -fsSL https://scripts.example.com/sandbox.sh | bash -s -- --pyo-url 'https://agent.example.com' --server 'sbs_one' --name 'it'\\''s sbx' --code 'abc' --tailscale-key 'tskey-auth-1' --command-key 'KEY1,KEY2' --version '0.51.1'"
    );
    let j = Joining { edge_domain: Some("sbx.example.com".into()), ..j };
    assert!(j.command("https://s.example.com", "0.51.1").ends_with(" --edge-domain 'sbx.example.com'"));
}

#[test]
fn browsers_reach_a_server_on_its_tailnet_name() {
    assert_eq!(tailnet_url("sbx-1.tail1234.ts.net"), "https://sbx-1.tail1234.ts.net:7443");
    assert_eq!(tailnet_url("sbx-1.tail1234.ts.net."), "https://sbx-1.tail1234.ts.net:7443");
}

/// A sandbox on a server that joined over HTTPS: its connection and its
/// view give the server's HTTPS address on the tailnet, pages included.
#[tokio::test]
#[ignore = "needs DATABASE_URL (a throwaway Postgres)"]
async fn sandboxes_on_it_open_on_its_tailnet_name() {
    let s = state().await;
    let signer = Local::new();
    let t = team(&s.db, true).await;
    let (j, _) = joined(&s, &signer, &t, "sbx-1").await;
    let id = crate::sandbox::new_sandbox_id();
    sqlx::query(
        "INSERT INTO pyo.sandboxes (id, team_id, server_id, spec, state, created_by, expires_at, name)
         VALUES ($1, $2, $3, '{}', 'running', 'test', now() + interval '1 hour', 'one')",
    )
    .bind(&id)
    .bind(&t.id)
    .bind(&j.server_id)
    .execute(&s.db)
    .await
    .unwrap();
    let want = format!("https://sbx-1.{}:7443", t.suffix);
    let c = crate::sandbox::connection(&s, &id, &t.id, &j.server_id, "100.64.0.9").await.unwrap();
    assert_eq!(c.url, want);
    assert_eq!(c.web_url.as_deref(), Some(want.as_str()));
    let v = crate::sandbox::sandbox_views(&s.db, std::slice::from_ref(&t.id), Some(id.as_str()), false).await.unwrap();
    assert_eq!(v[0]["url"], want);
    assert!(v[0].get("tailnetUrl").is_none());
}

#[test]
fn kinds_are_the_commands_types() {
    assert_eq!(channel::kind_of(&DaemonCommand::Delete { id: "sbx_1".into() }), "delete");
    assert_eq!(channel::kind_of(&DaemonCommand::Report), "report");
    assert_eq!(
        channel::kind_of(&DaemonCommand::ClosePort { id: "sbx_1".into(), port: 3000 }),
        "closePort"
    );
}

// ── Against a throwaway Postgres ───────────────────────────────────────────

/// A signer kept here, for sandbox commands only: a sequence per server.
struct Local {
    key: KeySigner,
    seq: AtomicU64,
    /// Refuse every command of this kind.
    refuse: Option<&'static str>,
    down: std::sync::atomic::AtomicBool,
}

impl Local {
    fn new() -> Self {
        Local {
            key: KeySigner::from_pkcs8(&KeySigner::generate_pkcs8().unwrap()).unwrap(),
            seq: AtomicU64::new(0),
            refuse: Some("setEdgeCert"),
            down: Default::default(),
        }
    }
}

impl crate::CommandSigner for Local {
    fn sign<'a>(
        &'a self,
        _: &'a str,
        _: &'a [crate::ToSign],
    ) -> futures::future::BoxFuture<'a, anyhow::Result<Vec<crate::Signature>>> {
        Box::pin(async { anyhow::bail!("not for agents here") })
    }
    fn sign_reply<'a>(
        &'a self,
        _: &'a str,
        _: &'a crate::ReplyToSign,
    ) -> futures::future::BoxFuture<'a, anyhow::Result<crate::Signature>> {
        Box::pin(async { anyhow::bail!("not for agents here") })
    }
    fn public_keys(&self) -> String {
        self.key.public_key()
    }
    fn sign_sandbox<'a>(
        &'a self,
        server_id: &'a str,
        team_id: &'a str,
        items: &'a [crate::ToSign],
    ) -> futures::future::BoxFuture<'a, anyhow::Result<Vec<crate::Signature>>> {
        Box::pin(async move {
            anyhow::ensure!(!self.down.load(Ordering::SeqCst), "the signer is down");
            let now = pyo_proto::now_ms() as u64;
            Ok(items
                .iter()
                .map(|t| {
                    if Some(t.kind.as_str()) == self.refuse {
                        return crate::Signature { refused: Some("not this kind".into()), ..Default::default() };
                    }
                    let (claims, sig) = self.key.sign_sandbox(&SandboxClaims {
                        v: 1,
                        id: t.id.clone(),
                        server: server_id.into(),
                        team: team_id.into(),
                        seq: self.seq.fetch_add(1, Ordering::SeqCst) + 1,
                        iat: now,
                        exp: now + 600_000,
                        kind: t.kind.clone(),
                        sha256: t.sha256.clone(),
                    });
                    crate::Signature { claims, sig, kid: self.key.kid().into(), refused: None, delegation: None }
                })
                .collect())
        })
    }
}

struct Team {
    id: String,
    holder: String,
    suffix: String,
    admin: User,
    developer: User,
}

async fn state() -> AppState {
    let db = PgPool::connect(&std::env::var("DATABASE_URL").expect("DATABASE_URL")).await.unwrap();
    crate::migrate::run(&db).await.unwrap();
    // Commands are sealed with the team's data key: a key of its own here
    // when the process has none.
    let provider = crate::vault::Provider::from_env().or(Some(crate::vault::Provider::Local {
        current: zeroize::Zeroizing::new([7u8; 32]),
        previous: None,
    }));
    crate::vault::init(crate::vault::Vault::new(db.clone(), provider));
    crate::sandbox::e2e::test_state(db)
}

async fn user(db: &PgPool, team: &str, run: &str, role: &str) -> User {
    let u = User {
        id: format!("usr_{role}{run}"),
        name: role.into(),
        email: format!("{role}{run}@sbx.test"),
        timezone: "UTC".into(),
        is_admin: false,
        must_set_up: false,
        key: None,
    };
    sqlx::query("INSERT INTO pyo.users (id, email, name, password_hash) VALUES ($1, $2, $3, '')")
        .bind(&u.id)
        .bind(&u.email)
        .bind(&u.name)
        .execute(db)
        .await
        .unwrap();
    sqlx::query("INSERT INTO pyo.members (id, team_id, user_id, email, role, status) VALUES ($1, $2, $3, $4, $5, 'active')")
        .bind(format!("mem_{role}{run}"))
        .bind(team)
        .bind(&u.id)
        .bind(&u.email)
        .bind(role)
        .execute(db)
        .await
        .unwrap();
    u
}

/// A team with Tailscale kept on a connected holder, which lists `sbx-1`.
/// Without `tailscale`, a team with none.
async fn team(db: &PgPool, tailscale: bool) -> Team {
    let run = crate::ids::random_string(10).to_lowercase();
    let (id, holder, suffix) = (format!("team_o{run}"), format!("cl_o{run}"), format!("tail{run}.ts.net"));
    sqlx::query("INSERT INTO pyo.teams (id, name, slug) VALUES ($1, $1, $1)").bind(&id).execute(db).await.unwrap();
    sqlx::query("INSERT INTO pyo.clusters (id, team_id, name, agent_token_hash, status, tailscale) VALUES ($1, $2, $1, $1, 'connected', $3)")
        .bind(&holder)
        .bind(&id)
        .bind(json!({ "clientId": "kT", "suffix": suffix }))
        .execute(db)
        .await
        .unwrap();
    if tailscale {
        sqlx::query("INSERT INTO pyo.team_tailscale (team_id, holder_cluster_id, client_id, suffix, machines) VALUES ($1, $2, 'kT', $3, $4)")
            .bind(&id)
            .bind(&holder)
            .bind(&suffix)
            .bind(json!([{ "nodeId": "n1", "hostname": "sbx-1" }]))
            .execute(db)
            .await
            .unwrap();
    }
    let admin = user(db, &id, &run, "admin").await;
    let developer = user(db, &id, &run, "developer").await;
    Team { id, holder, suffix, admin, developer }
}

/// The holder's agent: answers its Tailscale keys until stopped.
fn holder_agent(db: PgPool, holder: String) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            let ids: Vec<String> = sqlx::query_scalar(
                "SELECT id FROM pyo.commands WHERE cluster_id = $1 AND command->>'kind' = 'tailscale_key' AND status = 'pending'",
            )
            .bind(&holder)
            .fetch_all(&db)
            .await
            .unwrap_or_default();
            for id in ids {
                let r = pyo_proto::CommandResult {
                    ok: true,
                    message: None,
                    logs: vec![],
                    data: Some(json!({ pyo_proto::tailscale::KEY_FIELD: format!("tskey-auth-{id}") })),
                };
                let _ = sqlx::query("UPDATE pyo.commands SET status = 'done', result = $2, done_at = now() WHERE id = $1")
                    .bind(&id)
                    .bind(serde_json::to_value(&r).unwrap())
                    .execute(&db)
                    .await;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
}

async fn code_for(s: &AppState, signer: &Local, t: &Team, name: &str) -> Joining {
    let agent = holder_agent(s.db.clone(), t.holder.clone());
    let j = create_join_code(
        s,
        signer,
        &t.admin,
        CreateJoinCode { team_id: t.id.clone(), name: name.into(), edge_domain: None },
    )
    .await;
    agent.abort();
    j.unwrap()
}

fn join_ask(j: &Joining, t: &Team, key: &RequestKey) -> wire::JoinRequest {
    ask(&j.server_id, &j.code, &key.public_key(), "sbx-1", &format!("sbx-1.{}", t.suffix))
}

async fn joined(s: &AppState, signer: &Local, t: &Team, name: &str) -> (Joining, Key) {
    let j = code_for(s, signer, t, name).await;
    let k = RequestKey::from_pkcs8(&RequestKey::generate_pkcs8().unwrap()).unwrap();
    let a = join(&s.db, &s.sealer, &signer.public_keys(), "https://agent.example.com", &join_ask(&j, t, &k))
        .await
        .unwrap();
    (j, Key(a.key_id, k))
}

use crate::CommandSigner as _;

struct Key(String, RequestKey);

fn app(s: &AppState, signer: Arc<Local>) -> Router {
    Router::new()
        .nest(wire::API_PREFIX, channel::routes(s.clone(), Some(signer)))
        .with_state(s.clone())
}

/// A request to the channel, signed at `t` (now when None) with `key`.
async fn call_at(
    app: &Router,
    method: Method,
    path: &str,
    key: Option<&Key>,
    body: &[u8],
    t: Option<i64>,
    headers: &[(&str, &str)],
) -> (StatusCode, Value) {
    let full = format!("{}{path}", wire::API_PREFIX);
    let mut req = Request::builder().method(method.clone()).uri(&full).header("content-type", "application/json");
    if let Some(Key(id, k)) = key {
        let t = t.unwrap_or(pyo_proto::now_ms() / 1000);
        for (h, v) in k.sign(id, method.as_str(), &full, body, t) {
            req = req.header(h, v);
        }
    }
    for (h, v) in headers {
        req = req.header(*h, *v);
    }
    let res = app.clone().oneshot(req.body(Body::from(body.to_vec())).unwrap()).await.unwrap();
    let status = res.status();
    let bytes = axum::body::to_bytes(res.into_body(), 1 << 20).await.unwrap();
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

async fn call(app: &Router, method: Method, path: &str, key: Option<&Key>, body: &[u8]) -> (StatusCode, Value) {
    call_at(app, method, path, key, body, None, &[]).await
}

fn report_of(server: &str) -> Vec<u8> {
    serde_json::to_vec(&json!({
        "protocol": 1, "daemonVersion": "0.51.1", "serverId": server,
        "capability": capability(pyo_proto::sandbox::Runtime::Firecracker),
        "capacity": pyo_proto::sandbox::CapacityReport::default(),
        "sandboxes": [], "templates": []
    }))
    .unwrap()
}

/// Join codes: made by owners and admins of a team with Tailscale, kept as
/// their hash, good for 15 minutes, burnt on first use, wrong ones and
/// new ones rate-limited.
#[tokio::test]
#[ignore = "needs DATABASE_URL (a throwaway Postgres)"]
async fn join_codes_work_once_for_a_while() {
    let s = state().await;
    let signer = Local::new();
    let t = team(&s.db, true).await;
    let none = team(&s.db, false).await;

    // Who may make one.
    let req = |team: &str| CreateJoinCode { team_id: team.into(), name: "sbx-a".into(), edge_domain: None };
    let e = create_join_code(&s, &signer, &t.developer, req(&t.id)).await.unwrap_err();
    assert_eq!(e.status, StatusCode::FORBIDDEN);
    let e = create_join_code(&s, &signer, &none.admin, req(&t.id)).await.unwrap_err();
    assert!(matches!(e.status, StatusCode::FORBIDDEN | StatusCode::NOT_FOUND), "{}", e.message);
    let e = create_join_code(&s, &signer, &none.admin, req(&none.id)).await.unwrap_err();
    assert_eq!((e.status, e.message.as_str()), (StatusCode::BAD_REQUEST, TAILSCALE_NEEDED));

    // Made, with a key from the holder, kept only as its hash.
    let j = code_for(&s, &signer, &t, "sbx-a").await;
    assert!(j.tailscale_key.starts_with("tskey-auth-"));
    assert_eq!(j.command_keys, signer.public_keys());
    let (hashed, plain): (i64, i64) = sqlx::query_as(
        "SELECT count(*) FILTER (WHERE code_hash = $1), count(*) FILTER (WHERE code_hash = $2) FROM pyo.sandbox_join_codes WHERE server_id = $3",
    )
    .bind(sha256_hex(&j.code))
    .bind(&j.code)
    .bind(&j.server_id)
    .fetch_one(&s.db)
    .await
    .unwrap();
    assert_eq!((hashed, plain), (1, 0));
    let (state, outbound): (String, bool) =
        sqlx::query_as("SELECT state, outbound FROM pyo.sandbox_servers WHERE id = $1")
            .bind(&j.server_id)
            .fetch_one(&s.db)
            .await
            .unwrap();
    assert_eq!((state.as_str(), outbound), ("joining", true));
    // A new code for the same server ends the one before.
    let j2 = code_for(&s, &signer, &t, "sbx-a").await;
    assert_eq!(j2.server_id, j.server_id);
    let k = RequestKey::from_pkcs8(&RequestKey::generate_pkcs8().unwrap()).unwrap();
    let keys = signer.public_keys();
    let try_join = |a: wire::JoinRequest| {
        let (db, sealer, keys) = (s.db.clone(), s.sealer.clone(), keys.clone());
        async move { join(&db, &sealer, &keys, "https://agent.example.com", &a).await }
    };
    let e = try_join(join_ask(&j, &t, &k)).await.unwrap_err();
    assert_eq!(e.message, join::Refusal::Used.message());

    // Expired: refused, and burnt.
    sqlx::query("UPDATE pyo.sandbox_join_codes SET expires_at = now() - interval '1 second' WHERE server_id = $1")
        .bind(&j2.server_id)
        .execute(&s.db)
        .await
        .unwrap();
    let e = try_join(join_ask(&j2, &t, &k)).await.unwrap_err();
    assert_eq!(e.message, join::Refusal::Expired.message());
    sqlx::query("UPDATE pyo.sandbox_join_codes SET expires_at = now() + interval '1 minute' WHERE server_id = $1")
        .bind(&j2.server_id)
        .execute(&s.db)
        .await
        .unwrap();
    let e = try_join(join_ask(&j2, &t, &k)).await.unwrap_err();
    assert_eq!(e.message, join::Refusal::Used.message());

    // Another server's code: refused and burnt.
    let other = code_for(&s, &signer, &t, "sbx-b").await;
    let j3 = code_for(&s, &signer, &t, "sbx-a").await;
    let mut wrong = join_ask(&j3, &t, &k);
    wrong.server_id = other.server_id.clone();
    assert_eq!(try_join(wrong).await.unwrap_err().message, join::Refusal::WrongServer.message());
    assert_eq!(try_join(join_ask(&j3, &t, &k)).await.unwrap_err().message, join::Refusal::Used.message());

    // The right ask, once.
    let j4 = code_for(&s, &signer, &t, "sbx-a").await;
    assert!(try_join(join_ask(&j4, &t, &k)).await.is_ok());
    assert_eq!(try_join(join_ask(&j4, &t, &k)).await.unwrap_err().message, join::Refusal::Used.message());
    // Once it reported in, no new code for that name.
    sqlx::query("UPDATE pyo.sandbox_servers SET state = 'ready' WHERE id = $1")
        .bind(&j4.server_id)
        .execute(&s.db)
        .await
        .unwrap();
    let agent = holder_agent(s.db.clone(), t.holder.clone());
    let e = create_join_code(&s, &signer, &t.admin, req(&t.id)).await.unwrap_err();
    agent.abort();
    assert_eq!(e.status, StatusCode::CONFLICT);

    // Wrong codes are rate-limited per server.
    ratelimit::reset(&s.db, &join::WRONG_CODE_ALL, "all").await.unwrap();
    let b = code_for(&s, &signer, &t, "sbx-b").await;
    for _ in 0..join::WRONG_CODE_SERVER.max {
        let mut a = join_ask(&b, &t, &k);
        a.code = join::new_code();
        assert_eq!(try_join(a).await.unwrap_err().message, join::Refusal::Unknown.message());
    }
    let e = try_join(join_ask(&b, &t, &k)).await.unwrap_err();
    assert_eq!(e.status, StatusCode::TOO_MANY_REQUESTS);
    ratelimit::reset(&s.db, &join::WRONG_CODE_SERVER, &b.server_id).await.unwrap();
    ratelimit::reset(&s.db, &join::WRONG_CODE_ALL, "all").await.unwrap();
    assert!(try_join(join_ask(&b, &t, &k)).await.is_ok());

    // New codes are rate-limited per team.
    for _ in 0..CODES_PER_TEAM.max {
        ratelimit::take_after(&s.db, &CODES_PER_TEAM, &t.id).await;
    }
    let e = create_join_code(&s, &signer, &t.admin, CreateJoinCode { team_id: t.id.clone(), name: "sbx-c".into(), edge_domain: None })
        .await
        .unwrap_err();
    assert_eq!(e.status, StatusCode::TOO_MANY_REQUESTS);
    ratelimit::reset(&s.db, &CODES_PER_TEAM, &t.id).await.unwrap();
}

/// The join checks the server is on the team's tailnet and listed there,
/// then keeps its key and answers with its config.
#[tokio::test]
#[ignore = "needs DATABASE_URL (a throwaway Postgres)"]
async fn a_join_registers_the_servers_key() {
    let s = state().await;
    let signer = Local::new();
    let t = team(&s.db, true).await;
    let keys = signer.public_keys();
    let k = RequestKey::from_pkcs8(&RequestKey::generate_pkcs8().unwrap()).unwrap();

    // On another tailnet: refused, and the code is burnt.
    let j = code_for(&s, &signer, &t, "sbx-1").await;
    let mut a = join_ask(&j, &t, &k);
    a.tailnet_name = "sbx-1.tail0000.ts.net".into();
    let e = join(&s.db, &s.sealer, &keys, "https://agent.example.com", &a).await.unwrap_err();
    assert_eq!(e.message, Refusal::OtherTailnet.message());
    let e = join(&s.db, &s.sealer, &keys, "https://agent.example.com", &join_ask(&j, &t, &k)).await.unwrap_err();
    assert_eq!(e.message, join::Refusal::Used.message());

    // Not listed yet: asked to wait, the code still works, and the holder
    // reads the tailnet again soon.
    let j = code_for(&s, &signer, &t, "sbx-1").await;
    let mut a = join_ask(&j, &t, &k);
    a.hostname = "sbx-2".into();
    a.tailnet_name = format!("sbx-2.{}", t.suffix);
    let e = join(&s.db, &s.sealer, &keys, "https://agent.example.com", &a).await.unwrap_err();
    assert_eq!((e.status, e.message.clone()), (StatusCode::CONFLICT, Refusal::NotSeen.message()));
    let sent_at: Value = sqlx::query_scalar("SELECT tailscale->'sentAt' FROM pyo.clusters WHERE id = $1")
        .bind(&t.holder)
        .fetch_one(&s.db)
        .await
        .unwrap();
    assert_eq!(sent_at, json!(0));
    sqlx::query("UPDATE pyo.team_tailscale SET machines = machines || '[{\"nodeId\": \"n2\", \"hostname\": \"sbx-2\"}]' WHERE team_id = $1")
        .bind(&t.id)
        .execute(&s.db)
        .await
        .unwrap();
    let answer = join(&s.db, &s.sealer, &keys, "https://agent.example.com", &a).await.unwrap();
    assert_eq!(answer.server_id, j.server_id);
    assert_eq!(answer.team_id, t.id);
    assert_eq!(answer.command_keys, keys);
    assert!(answer.not_after > pyo_proto::now_ms() + 23 * 3600 * 1000);
    assert_eq!(answer.config["serverId"], json!(j.server_id));
    assert_eq!(answer.config["listen"], "100.64.0.20:7070");
    assert_eq!(answer.config["outbound"]["pyoUrl"], "https://agent.example.com");
    assert_eq!(answer.config["outbound"]["teamId"], json!(t.id));
    assert!(answer.config["keys"][0]["publicKey"].is_string());
    let (pk, team_id): (String, String) =
        sqlx::query_as("SELECT public_key, team_id FROM pyo.sandbox_server_keys WHERE id = $1 AND revoked_at IS NULL")
            .bind(&answer.key_id)
            .fetch_one(&s.db)
            .await
            .unwrap();
    assert_eq!((pk, team_id), (k.public_key(), t.id.clone()));
    let (address, name): (String, String) =
        sqlx::query_as("SELECT address, tailnet_name FROM pyo.sandbox_servers WHERE id = $1")
            .bind(&j.server_id)
            .fetch_one(&s.db)
            .await
            .unwrap();
    assert_eq!((address.as_str(), name), ("100.64.0.20", format!("sbx-2.{}", t.suffix)));

    // A server that can run nothing is refused with the reason.
    let j = code_for(&s, &signer, &t, "sbx-3").await;
    let mut a = join_ask(&j, &t, &k);
    a.capability = Some(capability(pyo_proto::sandbox::Runtime::None));
    let e = join(&s.db, &s.sealer, &keys, "https://agent.example.com", &a).await.unwrap_err();
    assert_eq!(e.status, StatusCode::UNPROCESSABLE_ENTITY);
    let state: String = sqlx::query_scalar("SELECT state FROM pyo.sandbox_servers WHERE id = $1")
        .bind(&j.server_id)
        .fetch_one(&s.db)
        .await
        .unwrap();
    assert_eq!(state, "refused");

    // Over HTTP too.
    let j = code_for(&s, &signer, &t, "sbx-4").await;
    let app = app(&s, Arc::new(Local::new()));
    let body = serde_json::to_vec(&join_ask(&j, &t, &k)).unwrap();
    let (st, v) = call(&app, Method::POST, wire::JOIN_PATH, None, &body).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["serverId"], json!(j.server_id));
    let (st, v) = call(&app, Method::POST, wire::JOIN_PATH, None, &body).await;
    assert_eq!((st, v["error"].as_str()), (StatusCode::FORBIDDEN, Some(join::Refusal::Used.message().as_str())));
}

/// Requests carry the server's signature: unsigned, replayed, stale,
/// changed, expired or another server's are refused, and a key reaches
/// only its own server's commands.
#[tokio::test]
#[ignore = "needs DATABASE_URL (a throwaway Postgres)"]
async fn a_key_opens_only_its_own_servers_channel() {
    let s = state().await;
    let signer = Arc::new(Local::new());
    let (ta, tb) = (team(&s.db, true).await, team(&s.db, true).await);
    let (a, ka) = joined(&s, &signer, &ta, "sbx-a").await;
    let (b, kb) = joined(&s, &signer, &tb, "sbx-b").await;
    let app = app(&s, signer.clone());
    let poll = "/commands?wait=0";

    // Unsigned.
    assert_eq!(call(&app, Method::GET, poll, None, b"").await.0, StatusCode::UNAUTHORIZED);
    // Signed: nothing to do yet.
    let (st, v) = call(&app, Method::GET, poll, Some(&ka), b"").await;
    assert_eq!((st, v), (StatusCode::OK, json!([])));
    // Replayed: the same signature twice.
    let full = format!("{}{poll}", wire::API_PREFIX);
    let headers = ka.1.sign(&ka.0, "GET", &full, b"", pyo_proto::now_ms() / 1000);
    let send = |h: [(&'static str, String); 2]| {
        let app = app.clone();
        let full = full.clone();
        async move {
            let mut req = Request::builder().method(Method::GET).uri(&full);
            for (k, v) in h {
                req = req.header(k, v);
            }
            app.oneshot(req.body(Body::empty()).unwrap()).await.unwrap().status()
        }
    };
    assert_eq!(send(headers.clone()).await, StatusCode::OK);
    assert_eq!(send(headers).await, StatusCode::UNAUTHORIZED);
    // Stale, or from the future.
    let now = pyo_proto::now_ms() / 1000;
    for t in [now - 120, now + 120] {
        let (st, _) = call_at(&app, Method::GET, poll, Some(&ka), b"", Some(t), &[]).await;
        assert_eq!(st, StatusCode::UNAUTHORIZED);
    }
    // A body other than the one signed.
    let mut req = Request::builder().method(Method::POST).uri(format!("{}{}", wire::API_PREFIX, wire::REPORT_PATH));
    for (h, v) in ka.1.sign(&ka.0, "POST", &format!("{}{}", wire::API_PREFIX, wire::REPORT_PATH), &report_of(&a.server_id), now) {
        req = req.header(h, v);
    }
    let res = app
        .clone()
        .oneshot(req.header("content-type", "application/json").body(Body::from(report_of(&b.server_id))).unwrap())
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    // A's key id with B's key, and the other way.
    let forged = Key(ka.0.clone(), RequestKey::from_pkcs8(&RequestKey::generate_pkcs8().unwrap()).unwrap());
    assert_eq!(call(&app, Method::GET, poll, Some(&forged), b"").await.0, StatusCode::UNAUTHORIZED);
    let swapped = Key(kb.0.clone(), RequestKey::from_pkcs8(&RequestKey::generate_pkcs8().unwrap()).unwrap());
    assert_eq!(call(&app, Method::GET, poll, Some(&swapped), b"").await.0, StatusCode::UNAUTHORIZED);

    // Each key gets its own server's commands only.
    let mut c = s.db.acquire().await.unwrap();
    let cmd_a = channel::enqueue(&mut c, &a.server_id, &ta.id, &DaemonCommand::Delete { id: "sbx_a1".into() })
        .await
        .unwrap();
    let cmd_b = channel::enqueue(&mut c, &b.server_id, &tb.id, &DaemonCommand::Delete { id: "sbx_b1".into() })
        .await
        .unwrap();
    // Not for a server of another team.
    assert!(channel::enqueue(&mut c, &a.server_id, &tb.id, &DaemonCommand::Report).await.is_err());
    drop(c);
    let (_, v) = call(&app, Method::GET, poll, Some(&ka), b"").await;
    let got: Vec<wire::CommandEnvelope> = serde_json::from_value(v).unwrap();
    assert_eq!(got.iter().map(|e| e.id.as_str()).collect::<Vec<_>>(), [cmd_a.as_str()]);
    // A's key answering B's command, or reporting as B.
    let done = serde_json::to_vec(&DaemonResult::Deleted { id: "sbx_b1".into() }).unwrap();
    assert_eq!(call(&app, Method::POST, &format!("/commands/{cmd_b}"), Some(&ka), &done).await.0, StatusCode::NOT_FOUND);
    assert_eq!(
        call(&app, Method::POST, wire::REPORT_PATH, Some(&ka), &report_of(&b.server_id)).await.0,
        StatusCode::BAD_REQUEST
    );
    let (_, v) = call(&app, Method::GET, poll, Some(&kb), b"").await;
    assert_eq!(v[0]["id"], json!(cmd_b));
    // Its own report.
    let (st, v) = call(&app, Method::POST, wire::REPORT_PATH, Some(&ka), &report_of(&a.server_id)).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let state: String = sqlx::query_scalar("SELECT state FROM pyo.sandbox_servers WHERE id = $1")
        .bind(&a.server_id)
        .fetch_one(&s.db)
        .await
        .unwrap();
    assert_eq!(state, "ready");

    // An expired key.
    sqlx::query("UPDATE pyo.sandbox_server_keys SET expires_at = now() - interval '1 second' WHERE id = $1")
        .bind(&kb.0)
        .execute(&s.db)
        .await
        .unwrap();
    let (st, v) = call(&app, Method::GET, poll, Some(&kb), b"").await;
    assert_eq!((st, v["error"].as_str()), (StatusCode::UNAUTHORIZED, Some(channel::KEY_GONE)));
}

/// Row security: a transaction lowered to one team sees and writes only
/// that team's keys and commands.
#[tokio::test]
#[ignore = "needs DATABASE_URL (a throwaway Postgres)"]
async fn row_security_keeps_teams_apart() {
    let s = state().await;
    let signer = Local::new();
    let (ta, tb) = (team(&s.db, true).await, team(&s.db, true).await);
    let (a, _) = joined(&s, &signer, &ta, "sbx-a").await;
    let (b, _) = joined(&s, &signer, &tb, "sbx-b").await;
    let mut c = s.db.acquire().await.unwrap();
    channel::enqueue(&mut c, &a.server_id, &ta.id, &DaemonCommand::Report).await.unwrap();
    channel::enqueue(&mut c, &b.server_id, &tb.id, &DaemonCommand::Report).await.unwrap();
    drop(c);
    let mut tx = s.db.begin().await.unwrap();
    crate::rls::scope_team(&mut tx, &ta.id).await.unwrap();
    for table in ["sandbox_server_keys", "sandbox_commands"] {
        let teams: Vec<String> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "SELECT DISTINCT team_id FROM pyo.{table} WHERE server_id = ANY($1)"
        )))
        .bind([&a.server_id, &b.server_id])
        .fetch_all(&mut *tx)
        .await
        .unwrap();
        assert_eq!(teams, std::slice::from_ref(&ta.id), "{table}");
    }
    let wrote = sqlx::query("INSERT INTO pyo.sandbox_commands (id, server_id, team_id, kind) VALUES ('sbc_x', $1, $2, 'report')")
        .bind(&b.server_id)
        .bind(&tb.id)
        .execute(&mut *tx)
        .await;
    assert!(wrote.is_err(), "another team's row can't be written");
    tx.rollback().await.unwrap();
}

/// The long poll: commands signed for their server and team, in order; it
/// waits, and wakes for a new one; results come back to whoever asked;
/// refused and unsignable commands are handled.
#[tokio::test]
#[ignore = "needs DATABASE_URL (a throwaway Postgres)"]
async fn the_long_poll_hands_out_signed_commands() {
    let s = state().await;
    let signer = Arc::new(Local::new());
    let t = team(&s.db, true).await;
    let (j, k) = joined(&s, &signer, &t, "sbx-a").await;
    let app = app(&s, signer.clone());
    let trusted = TrustedKeys::parse(&signer.public_keys()).unwrap();

    // Two commands, in the order they were queued, signed for this server.
    let mut c = s.db.acquire().await.unwrap();
    let one = channel::enqueue(&mut c, &j.server_id, &t.id, &DaemonCommand::Delete { id: "sbx_1".into() })
        .await
        .unwrap();
    let two = channel::enqueue(&mut c, &j.server_id, &t.id, &DaemonCommand::Report).await.unwrap();
    drop(c);
    let sealed: Option<String> = sqlx::query_scalar("SELECT command_sealed FROM pyo.sandbox_commands WHERE id = $1")
        .bind(&one)
        .fetch_one(&s.db)
        .await
        .unwrap();
    assert!(!sealed.unwrap().contains("sbx_1"), "kept sealed");
    let (st, v) = call(&app, Method::GET, "/commands?wait=0", Some(&k), b"").await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let got: Vec<wire::CommandEnvelope> = serde_json::from_value(v).unwrap();
    assert_eq!(got.iter().map(|e| e.id.as_str()).collect::<Vec<_>>(), [one.as_str(), two.as_str()]);
    let mut last = 0;
    for e in &got {
        let c = trusted.verify_sandbox(Some(&e.signed), &e.id, &j.server_id, &t.id, last, pyo_proto::now_ms() as u64).unwrap();
        assert_eq!(c.kind, e.kind);
        last = c.seq;
        assert!(trusted.verify_sandbox(Some(&e.signed), &e.id, &j.server_id, "team_other", 0, pyo_proto::now_ms() as u64).is_err());
    }
    assert_eq!(serde_json::from_str::<DaemonCommand>(&got[0].signed.command).unwrap(), DaemonCommand::Delete { id: "sbx_1".into() });
    // Taken once.
    assert_eq!(call(&app, Method::GET, "/commands?wait=0", Some(&k), b"").await.1, json!([]));

    // Answers: once, for a command that went out.
    let deleted = serde_json::to_vec(&DaemonResult::Deleted { id: "sbx_1".into() }).unwrap();
    assert_eq!(call(&app, Method::POST, &format!("/commands/{one}"), Some(&k), &deleted).await.0, StatusCode::OK);
    assert_eq!(call(&app, Method::POST, &format!("/commands/{one}"), Some(&k), &deleted).await.0, StatusCode::NOT_FOUND);
    assert_eq!(call(&app, Method::POST, "/commands/sbc_unknown", Some(&k), &deleted).await.0, StatusCode::NOT_FOUND);
    let (status, sealed): (String, Option<String>) =
        sqlx::query_as("SELECT status, command_sealed FROM pyo.sandbox_commands WHERE id = $1")
            .bind(&one)
            .fetch_one(&s.db)
            .await
            .unwrap();
    assert_eq!((status.as_str(), sealed), ("done", None));

    // A waiting poll wakes for a new command, and `ask` gets its answer.
    let key_id = k.0.clone();
    let daemon = {
        let app = app.clone();
        tokio::spawn(async move {
            let started = tokio::time::Instant::now();
            let (_, v) = call(&app, Method::GET, "/commands?wait=20", Some(&k), b"").await;
            let got: Vec<wire::CommandEnvelope> = serde_json::from_value(v).unwrap();
            assert_eq!(got.len(), 1);
            assert!(started.elapsed() < Duration::from_secs(15), "woke for it");
            let r = serde_json::to_vec(&DaemonResult::Deleted { id: "sbx_2".into() }).unwrap();
            let (st, _) = call(&app, Method::POST, &format!("/commands/{}", got[0].id), Some(&k), &r).await;
            assert_eq!(st, StatusCode::OK);
            k
        })
    };
    tokio::time::sleep(Duration::from_millis(300)).await;
    let r = channel::ask(&s, &j.server_id, &DaemonCommand::Delete { id: "sbx_2".into() }, Duration::from_secs(20))
        .await
        .unwrap();
    assert_eq!(r, DaemonResult::Deleted { id: "sbx_2".into() });
    let k = daemon.await.unwrap();
    assert_eq!(k.0, key_id);

    // Nobody takes it in time: taken back.
    let e = channel::ask(&s, &j.server_id, &DaemonCommand::Report, Duration::from_millis(300)).await.unwrap_err();
    assert_eq!(e.code, ErrorCode::Runtime);
    assert_eq!(call(&app, Method::GET, "/commands?wait=0", Some(&k), b"").await.1, json!([]));

    // The signer refuses one: it fails with the reason, the rest go out.
    let mut c = s.db.acquire().await.unwrap();
    let edge = DaemonCommand::SetEdgeCert { cert: "c".into(), key: pyo_proto::sandbox::SecretPem("k".into()) };
    let refused = channel::enqueue(&mut c, &j.server_id, &t.id, &edge).await.unwrap();
    let fine = channel::enqueue(&mut c, &j.server_id, &t.id, &DaemonCommand::List).await.unwrap();
    drop(c);
    let (_, v) = call(&app, Method::GET, "/commands?wait=0", Some(&k), b"").await;
    assert_eq!(v.as_array().unwrap().len(), 1);
    assert_eq!(v[0]["id"], json!(fine));
    let (status, result): (String, Value) = sqlx::query_as("SELECT status, result FROM pyo.sandbox_commands WHERE id = $1")
        .bind(&refused)
        .fetch_one(&s.db)
        .await
        .unwrap();
    assert_eq!(status, "failed");
    assert!(result["error"]["message"].as_str().unwrap().contains("signer refused"), "{result}");

    // The signer is down: back to the queue, out again once it's up.
    signer.down.store(true, Ordering::SeqCst);
    let mut c = s.db.acquire().await.unwrap();
    let later = channel::enqueue(&mut c, &j.server_id, &t.id, &DaemonCommand::List).await.unwrap();
    drop(c);
    assert_eq!(call(&app, Method::GET, "/commands?wait=0", Some(&k), b"").await.0, StatusCode::SERVICE_UNAVAILABLE);
    signer.down.store(false, Ordering::SeqCst);
    let (_, v) = call(&app, Method::GET, "/commands?wait=0", Some(&k), b"").await;
    assert_eq!(v[0]["id"], json!(later));

    // A restarted daemon: what went to the process before fails.
    let (_, v) = {
        let mut c = s.db.acquire().await.unwrap();
        channel::enqueue(&mut c, &j.server_id, &t.id, &DaemonCommand::List).await.unwrap();
        drop(c);
        call_at(&app, Method::GET, "/commands?wait=0", Some(&k), b"", None, &[(wire::INSTANCE_HEADER, "one")]).await
    };
    let lost = v[0]["id"].as_str().unwrap().to_owned();
    call_at(&app, Method::GET, "/commands?wait=0", Some(&k), b"", None, &[(wire::INSTANCE_HEADER, "two")]).await;
    let status: String = sqlx::query_scalar("SELECT status FROM pyo.sandbox_commands WHERE id = $1")
        .bind(&lost)
        .fetch_one(&s.db)
        .await
        .unwrap();
    assert_eq!(status, "failed");

    // Waiting too long fails a command, in the runner's sweep.
    let mut c = s.db.acquire().await.unwrap();
    let old = channel::enqueue(&mut c, &j.server_id, &t.id, &DaemonCommand::List).await.unwrap();
    drop(c);
    sqlx::query("UPDATE pyo.sandbox_commands SET created_at = now() - interval '1 hour' WHERE id = $1")
        .bind(&old)
        .execute(&s.db)
        .await
        .unwrap();
    channel::sweep(&s.db).await.unwrap();
    let status: String = sqlx::query_scalar("SELECT status FROM pyo.sandbox_commands WHERE id = $1")
        .bind(&old)
        .fetch_one(&s.db)
        .await
        .unwrap();
    assert_eq!(status, "failed");
}

/// Renewing swaps in a new key; Reset access and removing the server
/// revoke them at once.
#[tokio::test]
#[ignore = "needs DATABASE_URL (a throwaway Postgres)"]
async fn keys_renew_and_are_revoked() {
    let s = state().await;
    let signer = Arc::new(Local::new());
    let t = team(&s.db, true).await;
    let (j, k) = joined(&s, &signer, &t, "sbx-a").await;
    let app = app(&s, signer.clone());
    let poll = "/commands?wait=0";

    // Renewed: the new key works, the old one only for a few minutes.
    let next = RequestKey::from_pkcs8(&RequestKey::generate_pkcs8().unwrap()).unwrap();
    let body = serde_json::to_vec(&wire::RenewKey { public_key: next.public_key() }).unwrap();
    let (st, v) = call(&app, Method::POST, wire::RENEW_KEY_PATH, Some(&k), &body).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let renewed: wire::KeyAnswer = serde_json::from_value(v).unwrap();
    let k2 = Key(renewed.key_id.clone(), next);
    assert_eq!(call(&app, Method::GET, poll, Some(&k2), b"").await.0, StatusCode::OK);
    assert_eq!(call(&app, Method::GET, poll, Some(&k), b"").await.0, StatusCode::OK);
    let left: f64 = sqlx::query_scalar("SELECT extract(epoch FROM expires_at - now())::float8 FROM pyo.sandbox_server_keys WHERE id = $1")
        .bind(&k.0)
        .fetch_one(&s.db)
        .await
        .unwrap();
    assert!(left <= RENEWED_KEY_GRACE.as_secs_f64() + 1.0, "{left}");
    let bad = serde_json::to_vec(&json!({ "publicKey": "nope" })).unwrap();
    assert_eq!(call(&app, Method::POST, wire::RENEW_KEY_PATH, Some(&k2), &bad).await.0, StatusCode::BAD_REQUEST);
    assert_eq!(call(&app, Method::POST, wire::RENEW_KEY_PATH, None, &body).await.0, StatusCode::UNAUTHORIZED);

    // Reset access: every key stops, waiting commands fail, a new code
    // joins the same server again.
    let mut c = s.db.acquire().await.unwrap();
    let waiting = channel::enqueue(&mut c, &j.server_id, &t.id, &DaemonCommand::List).await.unwrap();
    drop(c);
    let e = reset_access(&s, signer.as_ref(), &t.developer, &j.server_id).await.unwrap_err();
    assert_eq!(e.status, StatusCode::FORBIDDEN);
    let agent = holder_agent(s.db.clone(), t.holder.clone());
    let again = reset_access(&s, signer.as_ref(), &t.admin, &j.server_id).await;
    agent.abort();
    let again = again.unwrap();
    assert_eq!(again.server_id, j.server_id);
    for key in [&k, &k2] {
        let (st, v) = call(&app, Method::GET, poll, Some(key), b"").await;
        assert_eq!((st, v["error"].as_str()), (StatusCode::UNAUTHORIZED, Some(channel::KEY_GONE)));
    }
    let status: String = sqlx::query_scalar("SELECT status FROM pyo.sandbox_commands WHERE id = $1")
        .bind(&waiting)
        .fetch_one(&s.db)
        .await
        .unwrap();
    assert_eq!(status, "failed");
    let fresh = RequestKey::from_pkcs8(&RequestKey::generate_pkcs8().unwrap()).unwrap();
    let a = join(&s.db, &s.sealer, &signer.public_keys(), "https://agent.example.com", &join_ask(&again, &t, &fresh))
        .await
        .unwrap();
    let k3 = Key(a.key_id, fresh);
    assert_eq!(call(&app, Method::GET, poll, Some(&k3), b"").await.0, StatusCode::OK);

    // Removed: its keys go with it.
    crate::sandbox::remove_server(&s, &t.admin, &j.server_id, false).await.unwrap();
    assert_eq!(call(&app, Method::GET, poll, Some(&k3), b"").await.0, StatusCode::UNAUTHORIZED);
    let keys: i64 = sqlx::query_scalar("SELECT count(*) FROM pyo.sandbox_server_keys WHERE server_id = $1")
        .bind(&j.server_id)
        .fetch_one(&s.db)
        .await
        .unwrap();
    assert_eq!(keys, 0);
    // Not for a self-hosted server.
    let e = reset_access(&s, signer.as_ref(), &t.admin, "sbs_none").await.unwrap_err();
    assert_eq!(e.status, StatusCode::NOT_FOUND);
}
