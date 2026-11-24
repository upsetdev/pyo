//! The outbound client against a stand-in Pyo on this machine: what it
//! runs and refuses, its key's renewal, and how it stops when the key is
//! revoked. No VMs: the work is a stand-in too.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::body::Bytes;
use axum::extract::{OriginalUri, Path, State};
use axum::http::{HeaderMap, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use pyo_proto::agentauth::{self, RequestKey};
use pyo_proto::sandbox::outbound::{self as wire, CommandEnvelope};
use pyo_proto::sandbox::wire::Cmd;
use pyo_proto::sandbox::{
    CapabilityReport, CapacityReport, DaemonCommand, DaemonResult, ErrorCode, ExecRequest,
    ExecResult, ServerReport,
};
use pyo_proto::signing::{SandboxClaims, Signer, sha256_hex};
use serde_json::json;

use super::store::{KeyFile, State as Kept, Store};
use super::*;

const SERVER: &str = "sbs_one";
const TEAM: &str = "team_one";

// ── A stand-in Pyo ─────────────────────────────────────────────────────────

#[derive(Default)]
struct Fake {
    /// Key ids it takes, with their public keys.
    keys: Mutex<HashMap<String, String>>,
    queue: Mutex<Vec<CommandEnvelope>>,
    results: Mutex<Vec<(String, DaemonResult)>>,
    reports: AtomicUsize,
    polls: AtomicUsize,
    /// The key ids requests came with, in order.
    seen: Mutex<Vec<String>>,
    renewals: AtomicUsize,
    /// The key ids renewals were asked with.
    renewed_with: Mutex<Vec<String>>,
    revoked: AtomicBool,
    /// Joins answered 409 before one is taken.
    not_listed: AtomicUsize,
    join_answer: Mutex<Option<serde_json::Value>>,
}

type F = State<Arc<Fake>>;

fn gone() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        axum::Json(json!({ "error": wire::KEY_GONE })),
    )
        .into_response()
}

impl Fake {
    /// Checks a signed request as Pyo does. Its key id, or the answer.
    #[allow(clippy::result_large_err)]
    fn check(
        &self,
        h: &HeaderMap,
        method: &Method,
        uri: &Uri,
        body: &[u8],
    ) -> Result<String, Response> {
        let get = |k: &str| {
            h.get(k)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .to_owned()
        };
        let (kid, sig) = (get(agentauth::KEY_HEADER), get(agentauth::SIGNATURE_HEADER));
        let pk = self.keys.lock().unwrap().get(&kid).cloned();
        let Some(pk) = pk.filter(|_| !self.revoked.load(Ordering::SeqCst)) else {
            return Err(gone());
        };
        let path = uri.path_and_query().map(|p| p.as_str()).unwrap_or("");
        let now = crate::now_ms() as i64 / 1000;
        agentauth::verify(&pk, &kid, method.as_str(), path, body, &sig, now).map_err(|e| {
            (
                StatusCode::UNAUTHORIZED,
                axum::Json(json!({ "error": e.to_string() })),
            )
                .into_response()
        })?;
        self.seen.lock().unwrap().push(kid.clone());
        Ok(kid)
    }
}

async fn poll(State(f): F, h: HeaderMap, m: Method, OriginalUri(u): OriginalUri) -> Response {
    if let Err(r) = f.check(&h, &m, &u, b"") {
        return r;
    }
    assert!(h.contains_key(wire::INSTANCE_HEADER));
    f.polls.fetch_add(1, Ordering::SeqCst);
    let out: Vec<CommandEnvelope> = std::mem::take(&mut *f.queue.lock().unwrap());
    if out.is_empty() {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    axum::Json(out).into_response()
}

async fn result(
    State(f): F,
    Path(id): Path<String>,
    h: HeaderMap,
    m: Method,
    OriginalUri(u): OriginalUri,
    b: Bytes,
) -> Response {
    if let Err(r) = f.check(&h, &m, &u, &b) {
        return r;
    }
    let r: DaemonResult = serde_json::from_slice(&b).unwrap();
    f.results.lock().unwrap().push((id, r));
    axum::Json(json!({ "ok": true })).into_response()
}

async fn report(
    State(f): F,
    h: HeaderMap,
    m: Method,
    OriginalUri(u): OriginalUri,
    b: Bytes,
) -> Response {
    if let Err(r) = f.check(&h, &m, &u, &b) {
        return r;
    }
    let r: ServerReport = serde_json::from_slice(&b).unwrap();
    assert_eq!(r.server_id, SERVER);
    f.reports.fetch_add(1, Ordering::SeqCst);
    axum::Json(json!({ "ok": true })).into_response()
}

async fn renew(
    State(f): F,
    h: HeaderMap,
    m: Method,
    OriginalUri(u): OriginalUri,
    b: Bytes,
) -> Response {
    let kid = match f.check(&h, &m, &u, &b) {
        Ok(k) => k,
        Err(r) => return r,
    };
    f.renewed_with.lock().unwrap().push(kid);
    let k: wire::RenewKey = serde_json::from_slice(&b).unwrap();
    let n = f.renewals.fetch_add(1, Ordering::SeqCst) + 1;
    let id = format!("sbsk_new{n}");
    f.keys.lock().unwrap().insert(id.clone(), k.public_key);
    axum::Json(wire::KeyAnswer {
        key_id: id,
        not_after: crate::now_ms() as i64 + 3_600_000,
    })
    .into_response()
}

async fn join(State(f): F, b: Bytes) -> Response {
    let ask: wire::JoinRequest = serde_json::from_slice(&b).unwrap();
    assert_eq!(
        (
            ask.address.as_str(),
            ask.hostname.as_str(),
            ask.tailnet_name.as_str()
        ),
        ("100.64.0.20", "sbx-1", "sbx-1.tail1234.ts.net")
    );
    assert!(ask.capability.is_some());
    if f.not_listed.load(Ordering::SeqCst) > 0 {
        f.not_listed.fetch_sub(1, Ordering::SeqCst);
        return (
            StatusCode::CONFLICT,
            axum::Json(json!({ "error": "not listed yet" })),
        )
            .into_response();
    }
    if ask.code != "the-code" {
        return (
            StatusCode::FORBIDDEN,
            axum::Json(json!({ "error": "That join code isn't valid." })),
        )
            .into_response();
    }
    f.keys
        .lock()
        .unwrap()
        .insert("sbsk_joined".into(), ask.public_key);
    let mut a = f.join_answer.lock().unwrap().clone().unwrap();
    a["serverId"] = json!(ask.server_id);
    axum::Json(a).into_response()
}

/// Serves the stand-in on this machine: its URL.
async fn serve(f: Arc<Fake>) -> String {
    let api = Router::new()
        .route(wire::JOIN_PATH, post(join))
        .route(wire::COMMANDS_PATH, get(poll))
        .route("/commands/{id}", post(result))
        .route(wire::REPORT_PATH, post(report))
        .route(wire::RENEW_KEY_PATH, post(renew))
        .with_state(f);
    let app = Router::new().nest(wire::API_PREFIX, api);
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await });
    format!("http://{addr}")
}

// ── A stand-in for the daemon's handlers ───────────────────────────────────

#[derive(Default)]
struct Ran(Mutex<Vec<DaemonCommand>>);

impl Work for Ran {
    fn run(&self, cmd: DaemonCommand) -> BoxFuture<'_, DaemonResult> {
        self.0.lock().unwrap().push(cmd.clone());
        Box::pin(async move {
            match cmd {
                DaemonCommand::Delete { id } => DaemonResult::Deleted { id },
                _ => DaemonResult::Error {
                    error: DaemonError::new(ErrorCode::Unsupported, "not here"),
                },
            }
        })
    }

    fn report(&self) -> ServerReport {
        ServerReport {
            protocol: 1,
            daemon_version: "test".into(),
            server_id: SERVER.into(),
            capability: CapabilityReport::default(),
            capacity: CapacityReport::default(),
            sandboxes: vec![],
            templates: vec![],
        }
    }
}

fn timing() -> Timing {
    Timing {
        poll_wait_secs: 1,
        report_every: Duration::from_millis(100),
        backoff_max: Duration::from_millis(200),
        backoff_min: Duration::from_millis(20),
    }
}

fn dir(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("pyo-sbx-out-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn signer() -> Signer {
    Signer::from_pkcs8(&Signer::generate_pkcs8().unwrap()).unwrap()
}

/// A command for `server` of `team`, signed by `s`, issued `age` ms ago.
fn envelope(
    s: &Signer,
    id: &str,
    server: &str,
    team: &str,
    seq: u64,
    cmd: &DaemonCommand,
    age: u64,
) -> CommandEnvelope {
    let json = serde_json::to_string(cmd).unwrap();
    let now = crate::now_ms() - age;
    let kind = serde_json::to_value(cmd).unwrap()["type"]
        .as_str()
        .unwrap()
        .to_owned();
    let (claims, sig) = s.sign_sandbox(&SandboxClaims {
        v: 1,
        id: id.into(),
        server: server.into(),
        team: team.into(),
        seq,
        iat: now,
        exp: now + 600_000,
        kind: kind.clone(),
        sha256: sha256_hex(json.as_bytes()),
    });
    CommandEnvelope {
        id: id.into(),
        kind,
        signed: pyo_proto::SignedCommandWire {
            claims,
            sig,
            kid: s.kid().into(),
            command: json,
            delegation: None,
        },
    }
}

/// A client of a server that joined the stand-in, with a key good for
/// `life_ms`, trusting `trusted`.
async fn client(
    name: &str,
    fake: &Arc<Fake>,
    trusted: &Signer,
    life_ms: i64,
) -> (Arc<Client<Ran>>, Arc<Ran>, Store) {
    let url = serve(fake.clone()).await;
    let store = Store::new(&dir(name));
    let pkcs8 = RequestKey::generate_pkcs8().unwrap();
    let pk = RequestKey::from_pkcs8(&pkcs8).unwrap().public_key();
    fake.keys.lock().unwrap().insert("sbsk_first".into(), pk);
    let now = crate::now_ms() as i64;
    store
        .save_key(&KeyFile::new("sbsk_first", now + life_ms, now, &pkcs8))
        .unwrap();
    store
        .save_state(&Kept {
            server_id: SERVER.into(),
            team_id: TEAM.into(),
            command_keys: trusted.public_key(),
            last_seq: 0,
        })
        .unwrap();
    let cfg = OutboundConfig {
        pyo_url: url,
        team_id: TEAM.into(),
        tailnet_name: "sbx-1.tail1234.ts.net".into(),
    };
    let work = Arc::new(Ran::default());
    let c = Client::load(work.clone(), store.clone(), &cfg, SERVER, timing()).unwrap();
    (c, work, store)
}

async fn until(what: &str, mut ok: impl FnMut() -> bool) {
    for _ in 0..200 {
        if ok() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("timed out waiting for {what}");
}

fn delete(id: &str) -> DaemonCommand {
    DaemonCommand::Delete { id: id.into() }
}

// ── Tests ──────────────────────────────────────────────────────────────────

/// Only commands signed by a trusted key for this server and team, new and
/// unexpired, run. The rest are answered `forbidden` with the reason.
#[tokio::test]
async fn commands_run_only_when_signed_for_this_server_and_team() {
    let fake = Arc::new(Fake::default());
    let s = signer();
    let (c, ran, store) = client("verify", &fake, &s, 3_600_000).await;
    let good = envelope(&s, "sbc_a", SERVER, TEAM, 1, &delete("sbx_a"), 0);
    let mut tampered = envelope(&s, "sbc_g", SERVER, TEAM, 7, &delete("sbx_g"), 0);
    tampered.signed.command = serde_json::to_string(&delete("sbx_other")).unwrap();
    let mut renamed = envelope(&s, "sbc_i", SERVER, TEAM, 8, &delete("sbx_i"), 0);
    renamed.id = "sbc_other".into();
    let queue = vec![
        good.clone(),
        envelope(&s, "sbc_b", "sbs_two", TEAM, 2, &delete("sbx_b"), 0),
        envelope(&s, "sbc_c", SERVER, "team_two", 3, &delete("sbx_c"), 0),
        // Played again: the same command, signature and number.
        good,
        // Signed 20 minutes ago, for 10.
        envelope(&s, "sbc_e", SERVER, TEAM, 4, &delete("sbx_e"), 20 * 60_000),
        // Signed by a key this server doesn't trust.
        envelope(&signer(), "sbc_f", SERVER, TEAM, 5, &delete("sbx_f"), 0),
        tampered,
        renamed,
        envelope(&s, "sbc_h", SERVER, TEAM, 9, &delete("sbx_h"), 0),
        // Signed for this server, but a sandbox of another team.
        envelope(
            &s,
            "sbc_k",
            SERVER,
            TEAM,
            10,
            &DaemonCommand::Create {
                spec: pyo_proto::sandbox::SandboxSpec::new("sbx_k", "team_two", "stp_x"),
            },
            0,
        ),
    ];
    *fake.queue.lock().unwrap() = queue;
    let run = tokio::spawn(c.clone().run());
    until("every answer", || fake.results.lock().unwrap().len() == 10).await;
    let ran_ids: Vec<String> = ran
        .0
        .lock()
        .unwrap()
        .iter()
        .filter_map(|c| c.sandbox_id().map(str::to_owned))
        .collect();
    assert_eq!(ran_ids, ["sbx_a", "sbx_h"]);
    let results: HashMap<String, DaemonResult> =
        fake.results.lock().unwrap().iter().cloned().collect();
    let why = |id: &str| match &results[id] {
        DaemonResult::Error { error } => {
            assert_eq!(error.code, ErrorCode::Forbidden, "{id}: {error}");
            error.message.clone()
        }
        other => panic!("{id} ran: {other:?}"),
    };
    assert!(
        why("sbc_b").contains("another sandbox server"),
        "{}",
        why("sbc_b")
    );
    assert!(why("sbc_c").contains("another team"), "{}", why("sbc_c"));
    assert!(why("sbc_e").contains("expired"), "{}", why("sbc_e"));
    assert!(why("sbc_f").contains("doesn't trust"), "{}", why("sbc_f"));
    assert!(
        why("sbc_g").contains("changed after it was signed"),
        "{}",
        why("sbc_g")
    );
    assert!(
        why("sbc_other").contains("id doesn't match"),
        "{}",
        why("sbc_other")
    );
    assert!(why("sbc_k").contains("another team"), "{}", why("sbc_k"));
    // The replay is answered under the first one's id: refused this time.
    assert_eq!(
        fake.results
            .lock()
            .unwrap()
            .iter()
            .filter(|(id, _)| id == "sbc_a")
            .count(),
        2
    );
    assert!(matches!(results["sbc_h"], DaemonResult::Deleted { .. }));
    // The last number taken is on disk.
    assert_eq!(store.state().unwrap().last_seq, 10);
    // A command numbered before the last one is refused.
    fake.queue
        .lock()
        .unwrap()
        .push(envelope(&s, "sbc_j", SERVER, TEAM, 6, &delete("sbx_j"), 0));
    until("the old number's answer", || {
        fake.results
            .lock()
            .unwrap()
            .iter()
            .any(|(id, _)| id == "sbc_j")
    })
    .await;
    assert!(
        !ran.0
            .lock()
            .unwrap()
            .iter()
            .any(|c| c.sandbox_id() == Some("sbx_j"))
    );
    fake.revoked.store(true, Ordering::SeqCst);
    tokio::time::timeout(Duration::from_secs(5), run)
        .await
        .unwrap()
        .unwrap();
    let _ = std::fs::remove_dir_all(&store.dir);
}

/// The key renews at half its life, with the key before it, and every
/// request after that uses the new one, also after a restart.
#[tokio::test]
async fn the_key_renews_at_half_its_life() {
    let fake = Arc::new(Fake::default());
    let s = signer();
    let (c, _, store) = client("renew", &fake, &s, 1_000).await;
    let run = tokio::spawn(c.clone().run());
    until("a renewal", || fake.renewals.load(Ordering::SeqCst) == 1).await;
    // Renewed with the first key.
    assert_eq!(*fake.renewed_with.lock().unwrap(), ["sbsk_first"]);
    let reports = fake.reports.load(Ordering::SeqCst);
    until("a report with the new key", || {
        fake.reports.load(Ordering::SeqCst) > reports + 1
    })
    .await;
    assert_eq!(fake.seen.lock().unwrap().last().unwrap(), "sbsk_new1");
    assert_eq!(c.key_id(), "sbsk_new1");
    let kept = store.key().unwrap();
    assert_eq!(kept.key_id, "sbsk_new1");
    assert!(kept.not_after > crate::now_ms() as i64 + 3_000_000);
    // Past the first key's life, it keeps working: no second renewal yet.
    tokio::time::sleep(Duration::from_millis(1_200)).await;
    assert!(!c.stopped());
    assert_eq!(fake.renewals.load(Ordering::SeqCst), 1);
    fake.revoked.store(true, Ordering::SeqCst);
    tokio::time::timeout(Duration::from_secs(5), run)
        .await
        .unwrap()
        .unwrap();
    let _ = std::fs::remove_dir_all(&store.dir);
}

/// A revoked key stops the client: no more polls, reports or renewals.
/// A key past its end does the same without asking Pyo.
#[tokio::test]
async fn a_revoked_key_stops_it() {
    let fake = Arc::new(Fake::default());
    let s = signer();
    let (c, _, store) = client("revoke", &fake, &s, 3_600_000).await;
    let run = tokio::spawn(c.clone().run());
    until("a report", || fake.reports.load(Ordering::SeqCst) > 0).await;
    fake.revoked.store(true, Ordering::SeqCst);
    tokio::time::timeout(Duration::from_secs(5), run)
        .await
        .unwrap()
        .unwrap();
    assert!(c.stopped());
    let (polls, reports) = (
        fake.polls.load(Ordering::SeqCst),
        fake.reports.load(Ordering::SeqCst),
    );
    // Taken back: it still doesn't call.
    fake.revoked.store(false, Ordering::SeqCst);
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(
        (
            fake.polls.load(Ordering::SeqCst),
            fake.reports.load(Ordering::SeqCst)
        ),
        (polls, reports)
    );
    let _ = std::fs::remove_dir_all(&store.dir);

    // A key whose time is over.
    let (c, _, store) = client("expired", &fake, &s, -1).await;
    tokio::time::timeout(Duration::from_secs(5), c.clone().run())
        .await
        .unwrap();
    assert!(c.stopped());
    let _ = std::fs::remove_dir_all(&store.dir);
}

#[test]
fn execs_over_the_channel_are_capped() {
    let req = |t: Option<u64>| DaemonCommand::Exec {
        id: "sbx_1".into(),
        request: ExecRequest {
            cmd: Cmd::Shell("true".into()),
            cwd: None,
            env: Default::default(),
            stdin: None,
            timeout_ms: t,
        },
    };
    for (asked, got) in [
        (None, 60_000),
        (Some(1_000), 1_000),
        (Some(0), wire::EXEC_MAX_MS),
        (Some(3_600_000), wire::EXEC_MAX_MS),
    ] {
        let mut c = req(asked);
        limit(&mut c);
        let DaemonCommand::Exec { request, .. } = c else {
            unreachable!()
        };
        assert_eq!(request.timeout_ms, Some(got), "{asked:?}");
    }
    let mut r = DaemonResult::Exec {
        result: ExecResult {
            exit_code: Some(0),
            signal: None,
            // A two-byte character across the cap.
            stdout: format!("{}é{}", "a".repeat(wire::OUTPUT_CAP - 1), "b".repeat(10)),
            stderr: "fine".into(),
            truncated: false,
            timed_out: false,
            duration_ms: 1,
        },
    };
    trim(&mut r);
    let DaemonResult::Exec { result } = r else {
        unreachable!()
    };
    assert!(result.truncated);
    assert_eq!(result.stdout.len(), wire::OUTPUT_CAP - 1);
    assert_eq!(result.stderr, "fine");
}

// ── The join ───────────────────────────────────────────────────────────────

fn answer_config(root: &std::path::Path) -> serde_json::Value {
    json!({
        "serverId": SERVER, "teamId": TEAM, "keyId": "sbsk_joined", "notAfter": crate::now_ms() + 86_400_000,
        "commandKeys": "",
        "config": {
            "serverId": SERVER, "listen": "100.64.0.20:7070", "root": root,
            "firecracker": "/x/firecracker", "kernel": "/x/vmlinux", "envd": "/x/pyo-envd", "keys": [],
            "outbound": { "pyoUrl": "https://agent.example.com", "teamId": TEAM, "tailnetName": "sbx-1.tail1234.ts.net" }
        }
    })
}

#[test]
fn tailscale_status_names_the_server() {
    let t = join::parse_status(
        br#"{"Self": {"HostName": "sbx-1", "DNSName": "sbx-1.tail1234.ts.net.", "TailscaleIPs": ["fd7a:115c:a1e0::1", "100.64.0.20"]}}"#,
    )
    .unwrap();
    assert_eq!(
        t,
        join::Tailnet {
            address: "100.64.0.20".into(),
            hostname: "sbx-1".into(),
            name: "sbx-1.tail1234.ts.net".into()
        }
    );
    assert!(join::parse_status(br#"{"Self": {"HostName": "sbx-1", "TailscaleIPs": []}}"#).is_err());
    assert!(join::parse_status(br#"{"BackendState": "NeedsLogin"}"#).is_err());
}

/// The join asks again while the tailnet doesn't list the server, keeps its
/// key, state and config root's only, and removes the join file. It
/// refuses command keys other than the join command's.
#[tokio::test]
async fn a_join_keeps_the_key_and_the_config() {
    use std::os::unix::fs::PermissionsExt;
    let fake = Arc::new(Fake::default());
    let url = serve(fake.clone()).await;
    let s = signer();
    let d = dir("join");
    let pool = d.join("pool");
    let mut a = answer_config(&pool);
    a["commandKeys"] = json!(s.public_key());
    *fake.join_answer.lock().unwrap() = Some(a);
    fake.not_listed.store(2, Ordering::SeqCst);
    let join_file = d.join("join.json");
    let write_join = |code: &str, key: &str| {
        // As sandbox.sh writes it: the join request without its key.
        let j = json!({
            "pyoUrl": url, "serverId": SERVER, "code": code, "commandKeys": key,
            "address": "100.64.0.20", "hostname": "sbx-1", "tailnetName": "sbx-1.tail1234.ts.net",
            "jailUid": 998, "jailGid": 997, "capability": CapabilityReport::default(),
        });
        crate::edge::write_private(&join_file, j.to_string().as_bytes()).unwrap();
    };
    let opts = || join::Options {
        retry: Duration::from_millis(20),
        deadline: Duration::from_secs(5),
    };
    let config = d.join("config.json");
    write_join("the-code", &s.public_key());
    let a = join::join(&config, &join_file, None, opts()).await.unwrap();
    assert_eq!(a.key_id, "sbsk_joined");
    assert!(!join_file.exists());
    let store = Store::new(&pool);
    let kept = store.state().unwrap();
    assert_eq!(
        (
            kept.server_id.as_str(),
            kept.team_id.as_str(),
            kept.last_seq
        ),
        (SERVER, TEAM, 0)
    );
    assert_eq!(kept.command_keys, s.public_key());
    assert_eq!(store.key().unwrap().key_id, "sbsk_joined");
    for f in [
        config.clone(),
        pool.join(store::KEY_FILE),
        pool.join(store::STATE_FILE),
    ] {
        let mode = std::fs::metadata(&f).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "{}", f.display());
    }
    let cfg = crate::config::Config::load(&config).unwrap();
    assert_eq!(cfg.outbound.unwrap().team_id, TEAM);

    // Pyo's refusal, in its words.
    write_join("wrong", &s.public_key());
    let e = join::join(&config, &join_file, None, opts())
        .await
        .unwrap_err();
    assert_eq!(
        e.to_string(),
        "Pyo refused this server. That join code isn't valid."
    );
    // Command keys that aren't the join command's.
    write_join("the-code", &signer().public_key());
    let e = join::join(&config, &join_file, None, opts())
        .await
        .unwrap_err();
    assert!(e.to_string().contains("command keys"), "{e}");
    // A bad command key never reaches Pyo.
    write_join("the-code", "not a key");
    let e = join::join(&config, &join_file, None, opts())
        .await
        .unwrap_err();
    assert!(e.to_string().contains("command key"), "{e}");
    // Never listed: it gives up once the code would run out.
    fake.not_listed.store(1000, Ordering::SeqCst);
    write_join("the-code", &s.public_key());
    let e = join::join(
        &config,
        &join_file,
        None,
        join::Options {
            retry: Duration::from_millis(20),
            deadline: Duration::from_millis(200),
        },
    )
    .await
    .unwrap_err();
    assert!(e.to_string().contains("ran out"), "{e}");
    let _ = std::fs::remove_dir_all(&d);
}
