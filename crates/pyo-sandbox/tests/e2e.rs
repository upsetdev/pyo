//! Boots real Firecracker microVMs. Runs only with `PYO_SANDBOX_E2E=1` and
//! a `/dev/kvm` the current user can open, so plain `cargo test` skips it.
//! Dev mode: no jailer, no network.
//!
//! ```text
//! crates/pyo-sandbox/scripts/e2e-assets.sh     # Firecracker and a kernel into .cache/
//! cargo build --release -p pyo-envd --target x86_64-unknown-linux-musl
//! PYO_SANDBOX_E2E=1 cargo test -p pyo-sandbox --test e2e -- --nocapture
//! ```
//!
//! Inputs, with defaults from those steps: `PYO_SANDBOX_FIRECRACKER`,
//! `PYO_SANDBOX_KERNEL` and `PYO_SANDBOX_ENVD`. The template is the host's
//! static busybox plus envd, so nothing else is downloaded. The edge proxy
//! part needs `openssl` and `curl` on the host.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::http::{Request as HttpRequest, StatusCode};
use futures::{SinkExt, StreamExt};
use pyo_proto::sandbox::token::{ANY_SANDBOX, Claims, SigningKey};
use pyo_proto::sandbox::wire::{Cmd, ErrorKind, Frame, Kind, PtySpec, Request, Response, Scope};
use pyo_proto::sandbox::{
    DaemonCommand, DaemonResult, ExecResult, Kvm, NetworkMode, Runtime, SandboxSpec, SandboxStatus,
    sandbox_id_from,
};
use pyo_sandbox::api::{self, AppState};
use pyo_sandbox::config::Config;
use pyo_sandbox::detect::{self, Inputs};
use pyo_sandbox::edge::{Edge, EdgeConfig};
use pyo_sandbox::firecracker::Firecracker;
use pyo_sandbox::manager::Manager;
use pyo_sandbox::storage::{self, Layout};
use tower::ServiceExt;

const SERVER: &str = "srv-e2e";
const TEAM: &str = "team-e2e";

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap()
}

fn input(var: &str, default: PathBuf) -> PathBuf {
    std::env::var_os(var).map(PathBuf::from).unwrap_or(default)
}

fn enabled() -> bool {
    if std::env::var("PYO_SANDBOX_E2E").as_deref() != Ok("1") {
        eprintln!("skipping: set PYO_SANDBOX_E2E=1 to boot real microVMs");
        return false;
    }
    if std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/kvm")
        .is_err()
    {
        eprintln!("skipping: /dev/kvm isn't usable by this user");
        return false;
    }
    true
}

fn now_s() -> u64 {
    pyo_sandbox::now_ms() / 1000
}

struct Keys {
    key: SigningKey,
}

impl Keys {
    fn token(&self, sbx: &str, team: &str, scopes: &[Scope]) -> String {
        self.key.sign(&Claims {
            kid: String::new(),
            sbx: sbx.into(),
            team: team.into(),
            srv: SERVER.into(),
            iat: now_s(),
            exp: now_s() + 600,
            scopes: scopes.to_vec(),
        })
    }
    fn admin(&self) -> String {
        self.token(ANY_SANDBOX, "", &[Scope::Admin])
    }
}

async fn call(
    app: &axum::Router,
    method: &str,
    uri: &str,
    token: Option<&str>,
    body: Body,
) -> (StatusCode, Vec<u8>) {
    let mut req = HttpRequest::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json");
    if let Some(t) = token {
        req = req.header("authorization", format!("Bearer {t}"));
    }
    let res = app.clone().oneshot(req.body(body).unwrap()).await.unwrap();
    let status = res.status();
    let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, bytes.to_vec())
}

async fn json<T: serde::de::DeserializeOwned>(
    app: &axum::Router,
    method: &str,
    uri: &str,
    token: &str,
    body: serde_json::Value,
) -> T {
    let (s, b) = call(app, method, uri, Some(token), Body::from(body.to_string())).await;
    assert!(
        s.is_success(),
        "{method} {uri}: {s} {}",
        String::from_utf8_lossy(&b)
    );
    serde_json::from_slice(&b).unwrap()
}

fn sandbox_of(r: DaemonResult) -> SandboxStatus {
    match r {
        DaemonResult::Sandbox { sandbox } => sandbox,
        other => panic!("not a sandbox: {other:?}"),
    }
}

fn median(mut v: Vec<f64>) -> f64 {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[v.len() / 2]
}

/// A template root: the host's static busybox and envd.
async fn busybox_template(layout: &Layout, envd: &Path) {
    let dir = layout.root.join("template-src");
    let _ = std::fs::remove_dir_all(&dir);
    for d in [
        "bin", "sbin", "proc", "sys", "dev", "tmp", "run", "root", "mnt", "etc",
    ] {
        std::fs::create_dir_all(dir.join(d)).unwrap();
    }
    std::fs::copy("/usr/bin/busybox", dir.join("bin/busybox")).unwrap();
    let list = std::process::Command::new("/usr/bin/busybox")
        .arg("--list")
        .output()
        .unwrap();
    for applet in String::from_utf8_lossy(&list.stdout).lines() {
        let p = dir.join("bin").join(applet);
        if applet != "busybox" && !p.exists() {
            std::os::unix::fs::symlink("busybox", p).unwrap();
        }
    }
    std::fs::copy(envd, dir.join("sbin/pyo-envd")).unwrap();
    std::fs::write(dir.join("etc/passwd"), "root:x:0:0:root:/root:/bin/sh\n").unwrap();
    let out = layout.template_rootfs("busybox");
    std::fs::create_dir_all(out.parent().unwrap()).unwrap();
    storage::ext4_from_dir(&dir, &out, 64, "busybox")
        .await
        .unwrap();
    std::fs::remove_dir_all(&dir).unwrap();
}

/// curl through the edge, resolving every name to 127.0.0.1. Returns the
/// status, the headers and the body.
async fn curl(url: &str, cacert: &Path, extra: &[&str]) -> (u16, String, String) {
    let out = tokio::process::Command::new("curl")
        .args([
            "-sS",
            "-D",
            "-",
            "--max-time",
            "10",
            "--connect-to",
            "::127.0.0.1:",
        ])
        .arg("--cacert")
        .arg(cacert)
        .args(extra)
        .arg(url)
        .output()
        .await
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    let (head, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
    let status = head
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| {
            panic!(
                "curl {url}: {text} {}",
                String::from_utf8_lossy(&out.stderr)
            )
        });
    (status, head.to_string(), body.to_string())
}

/// The edge proxy: public and private ports over HTTPS and plain HTTP, the
/// token in its four places, and an upgraded connection.
async fn edge_proxy(state: &Arc<AppState>, keys: &Keys, run: &Path, id: &str) {
    use pyo_proto::sandbox::port_host;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let dir = run.join("edge");
    std::fs::create_dir_all(&dir).unwrap();
    let (cert, key) = (dir.join("cert.pem"), dir.join("key.pem"));
    let made = std::process::Command::new("openssl")
        .args([
            "req",
            "-x509",
            "-newkey",
            "ec",
            "-pkeyopt",
            "ec_paramgen_curve:prime256v1",
        ])
        .args(["-nodes", "-days", "1", "-subj", "/CN=*.sbx.test"])
        .args(["-addext", "subjectAltName=DNS:*.sbx.test", "-keyout"])
        .arg(&key)
        .arg("-out")
        .arg(&cert)
        .output()
        .unwrap();
    assert!(
        made.status.success(),
        "{}",
        String::from_utf8_lossy(&made.stderr)
    );
    let edge = |https: bool| EdgeConfig {
        domain: "sbx.test".into(),
        https: https.then(|| "127.0.0.1:0".parse().unwrap()),
        cert: https.then(|| cert.clone()),
        key: https.then(|| key.clone()),
        http: (!https).then(|| "127.0.0.1:0".parse().unwrap()),
    };
    let tls = Arc::new(Edge::new(state.clone(), edge(true)).unwrap())
        .start()
        .await
        .unwrap();
    let plain = Arc::new(Edge::new(state.clone(), edge(false)).unwrap())
        .start()
        .await
        .unwrap();
    state.mgr.set_edge_url(tls.base_url("sbx.test"));
    let https_port = tls.https.unwrap().port();
    let http_port = plain.http.unwrap().port();
    let url = |port: u16, tls: bool| {
        let host = port_host(port, id, "sbx.test");
        if tls {
            format!("https://{host}:{https_port}/")
        } else {
            format!("http://{host}:{http_port}/")
        }
    };
    let app = api::router(state.clone());
    let ports = keys.token(id, TEAM, &[Scope::Ports]);
    let open = |port: u16, public: bool| {
        let app = app.clone();
        let ports = ports.clone();
        async move {
            sandbox_of(
                json(
                    &app,
                    "POST",
                    &format!("/v1/sandboxes/{id}/ports"),
                    &ports,
                    serde_json::json!({"port": port, "public": public}),
                )
                .await,
            )
        }
    };

    // A web server and an echo after an upgrade, in the sandbox.
    let r = state
        .mgr
        .exec(
            id,
            &pyo_proto::sandbox::ExecRequest {
                cmd: Cmd::Shell(
                    "mkdir -p /root/www && echo hello-edge > /root/www/index.html \
                     && httpd -p 3000 -h /root/www \
                     && printf '%s\n' '#!/bin/sh' \
                       'while read -r l; do [ \"$l\" = \"$(printf \"\\r\")\" ] && break; done' \
                       'printf \"HTTP/1.1 101 Switching Protocols\\r\\nConnection: Upgrade\\r\\nUpgrade: echo\\r\\n\\r\\n\"' \
                       'exec cat' > /root/up.sh \
                     && (nc -ll -p 4000 -e /bin/sh /root/up.sh >/dev/null 2>&1 &)"
                        .into(),
                ),
                cwd: None,
                env: BTreeMap::new(),
                stdin: None,
                timeout_ms: Some(5000),
            },
        )
        .await
        .unwrap();
    assert_eq!(r.exit_code, Some(0), "{r:?}");

    // Closed until opened.
    let (s, _, b) = curl(&url(3000, true), &cert, &[]).await;
    assert_eq!(s, 404, "{b}");
    assert!(b.contains("isn't open"), "{b}");

    // Public.
    let st = open(3000, true).await;
    assert_eq!(
        st.ports,
        vec![pyo_proto::sandbox::OpenPort {
            port: 3000,
            public: true
        }]
    );
    let (s, h, b) = curl(&url(3000, true), &cert, &[]).await;
    assert_eq!((s, b.trim()), (200, "hello-edge"), "{h}");
    let (s, _, b) = curl(&url(3000, false), &cert, &[]).await;
    assert_eq!((s, b.trim()), (200, "hello-edge"));
    assert_eq!(
        state.mgr.capability().port_url(id, 3000).unwrap(),
        format!("https://{}:{https_port}", port_host(3000, id, "sbx.test"))
    );

    // Private: a token in the header, in a cookie, or once in the query.
    open(3000, false).await;
    let (s, _, _) = curl(&url(3000, true), &cert, &[]).await;
    assert_eq!(s, 401);
    let bearer = format!("Authorization: Bearer {ports}");
    let (s, _, b) = curl(&url(3000, true), &cert, &["-H", &bearer]).await;
    assert_eq!((s, b.trim()), (200, "hello-edge"));
    let cookie = format!("pyo_token={ports}");
    let (s, _, _) = curl(&url(3000, true), &cert, &["-b", &cookie]).await;
    assert_eq!(s, 200);
    let header = format!("X-Pyo-Token: {ports}");
    let (s, _, _) = curl(&url(3000, true), &cert, &["-H", &header]).await;
    assert_eq!(s, 200);
    let q = format!("{}?a=1&pyo_token={ports}", url(3000, true));
    let (s, h, _) = curl(&q, &cert, &[]).await;
    assert_eq!(s, 303, "{h}");
    let hl = h.to_ascii_lowercase();
    assert!(hl.contains("location: /?a=1"), "{h}");
    assert!(
        hl.contains("set-cookie: pyo_token=") && hl.contains("httponly") && hl.contains("secure"),
        "{h}"
    );
    // Tokens for another sandbox, or without the ports scope, don't do.
    let other = keys.token(&sandbox_id_from([77; 16]), TEAM, &[Scope::Ports]);
    let wrong = format!("Authorization: Bearer {other}");
    assert_eq!(curl(&url(3000, true), &cert, &["-H", &wrong]).await.0, 401);
    let exec_only = format!(
        "Authorization: Bearer {}",
        keys.token(id, TEAM, &[Scope::Exec])
    );
    assert_eq!(
        curl(&url(3000, true), &cert, &["-H", &exec_only]).await.0,
        401
    );
    let admin = format!("Authorization: Bearer {}", keys.admin());
    assert_eq!(curl(&url(3000, true), &cert, &["-H", &admin]).await.0, 401);

    // Nothing listening, closing, and names that aren't sandbox ports.
    open(3999, true).await;
    assert_eq!(curl(&url(3999, true), &cert, &[]).await.0, 502);
    let (s, _, _) = curl(
        &format!("https://3000-sbx-nosuchsandboxhere.sbx.test:{https_port}/"),
        &cert,
        &[],
    )
    .await;
    assert_eq!(s, 404);
    let closed: DaemonResult = {
        let (s, b) = call(
            &app,
            "DELETE",
            &format!("/v1/sandboxes/{id}/ports/3999"),
            Some(&ports),
            Body::empty(),
        )
        .await;
        assert!(s.is_success(), "{}", String::from_utf8_lossy(&b));
        serde_json::from_slice(&b).unwrap()
    };
    assert_eq!(sandbox_of(closed).ports.len(), 1);
    assert_eq!(curl(&url(3999, true), &cert, &[]).await.0, 404);

    // An upgraded connection carries bytes both ways.
    open(4000, true).await;
    let mut c = tokio::net::TcpStream::connect(("127.0.0.1", http_port))
        .await
        .unwrap();
    let req = format!(
        "GET / HTTP/1.1\r\nHost: {}\r\nConnection: Upgrade\r\nUpgrade: echo\r\n\r\n",
        port_host(4000, id, "sbx.test")
    );
    c.write_all(req.as_bytes()).await.unwrap();
    let mut got = Vec::new();
    let mut buf = [0u8; 1024];
    while !got.windows(4).any(|w| w == b"\r\n\r\n") {
        let n = tokio::time::timeout(Duration::from_secs(5), c.read(&mut buf))
            .await
            .unwrap()
            .unwrap();
        assert!(n > 0, "closed: {}", String::from_utf8_lossy(&got));
        got.extend_from_slice(&buf[..n]);
    }
    assert!(
        got.starts_with(b"HTTP/1.1 101"),
        "{}",
        String::from_utf8_lossy(&got)
    );
    c.write_all(b"ping through the edge\n").await.unwrap();
    let mut echo = Vec::new();
    while !echo.ends_with(b"\n") {
        let n = tokio::time::timeout(Duration::from_secs(5), c.read(&mut buf))
            .await
            .unwrap()
            .unwrap();
        assert!(n > 0);
        echo.extend_from_slice(&buf[..n]);
    }
    assert_eq!(echo, b"ping through the edge\n");
}

/// Firecracker processes running from a directory under `run`.
fn vms_under(run: &Path) -> Vec<u32> {
    let mut pids = Vec::new();
    for e in std::fs::read_dir("/proc").into_iter().flatten().flatten() {
        let Ok(pid) = e.file_name().to_string_lossy().parse::<u32>() else {
            continue;
        };
        let cwd = std::fs::read_link(e.path().join("cwd")).unwrap_or_default();
        if cwd.starts_with(run) && pyo_sandbox::firecracker::is_firecracker(pid) {
            pids.push(pid);
        }
    }
    pids
}

/// Kills this run's VMs and deletes its files, also when the test fails.
struct Cleanup(PathBuf);

impl Drop for Cleanup {
    fn drop(&mut self) {
        for pid in vms_under(&self.0) {
            // SAFETY: kill on a Firecracker process this test started.
            unsafe { libc::kill(pid as i32, libc::SIGKILL) };
        }
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn spec(n: u8) -> SandboxSpec {
    let mut s = SandboxSpec::new(sandbox_id_from([n; 16]), TEAM, "busybox");
    s.memory_mib = 256;
    s.disk_mib = 256;
    s.network = NetworkMode::None;
    s.env.insert("GREETING".into(), "hi".into());
    s
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn boots_real_microvms() {
    if !enabled() {
        return;
    }
    let root = repo_root();
    let cache = root.join(".cache/sandbox-e2e");
    // A relative CARGO_TARGET_DIR is relative to where cargo ran, the repo root.
    let target = root.join(
        std::env::var_os("CARGO_TARGET_DIR")
            .map(PathBuf::from)
            .unwrap_or("target".into()),
    );
    let fc_bin = input("PYO_SANDBOX_FIRECRACKER", cache.join("bin/firecracker"));
    let kernel = input("PYO_SANDBOX_KERNEL", cache.join("vmlinux"));
    let envd = input(
        "PYO_SANDBOX_ENVD",
        target.join("x86_64-unknown-linux-musl/release/pyo-envd"),
    );
    for (what, p) in [
        ("Firecracker", &fc_bin),
        ("the kernel", &kernel),
        ("pyo-envd (musl)", &envd),
    ] {
        assert!(
            p.exists(),
            "{what} isn't at {}. See the top of this file.",
            p.display()
        );
    }

    let run = cache.join(format!("run-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&run);
    let _cleanup = Cleanup(run.clone());
    let keys = Keys {
        key: SigningKey::generate("e2e"),
    };
    let cfg = Config {
        server_id: SERVER.into(),
        listen: "127.0.0.1:0".parse().unwrap(),
        root: run.clone(),
        firecracker: fc_bin.clone(),
        jailer: None,
        kernel: kernel.clone(),
        envd: envd.clone(),
        keys: vec![keys.key.verifying_key()],
        host_reserve_mib: Some(1024),
        memory_overcommit: 1.0,
        cpu_template: None,
        jail_uid: 0,
        jail_gid: 0,
        cgroup_parent: "pyo-sandbox".into(),
        network: Default::default(),
        edge: None,
        dev: true,
    };
    let layout = Layout::new(&run, false);
    layout.create().await.unwrap();
    let fc = Firecracker {
        firecracker: fc_bin,
        kernel,
        jailer: None,
        cpu_template: None,
        serial: false,
    };
    let mut summary = serde_json::Map::new();
    let mut note = |k: &str, v: serde_json::Value| {
        println!("e2e {k}: {v}");
        summary.insert(k.into(), v);
    };

    // ── Detection, with the real test boot ──
    let t = Instant::now();
    let cap = detect::detect(Inputs {
        fc: &fc,
        layout: &layout,
        envd: &envd,
    })
    .await;
    note("detect_total_ms", (t.elapsed().as_millis() as u64).into());
    note("capability", serde_json::to_value(&cap).unwrap());
    assert_eq!(cap.runtime, Runtime::Firecracker, "{}", cap.summary());
    assert_eq!(
        cap.kvm,
        Kvm::Native,
        "this laptop has native KVM: {}",
        cap.summary()
    );
    assert!(cap.boot_ms.unwrap() < 300);

    busybox_template(&layout, &envd).await;
    let mgr = Arc::new(Manager::new(cfg.clone(), layout.clone(), fc.clone(), cap));
    let state = Arc::new(AppState {
        mgr: mgr.clone(),
        keys: cfg.keys.clone(),
        server_id: SERVER.into(),
    });
    let app = api::router(state.clone());

    // ── Cold create through the API ──
    let a = spec(1);
    let ida = a.id.clone();
    let create_a = keys.token(&ida, TEAM, &[Scope::Create]);
    let t = Instant::now();
    let st = sandbox_of(
        json(
            &app,
            "POST",
            "/v1/sandboxes",
            &create_a,
            serde_json::to_value(&a).unwrap(),
        )
        .await,
    );
    note(
        "cold_create_api_ms",
        (t.elapsed().as_millis() as u64).into(),
    );
    note("cold_start_ms", st.start_ms.into());
    let up: ExecResult = json(
        &app,
        "POST",
        &format!("/v1/sandboxes/{ida}/exec"),
        &keys.token(&ida, TEAM, &[Scope::Exec]),
        serde_json::json!({"cmd": "cat /proc/uptime"}),
    )
    .await;
    note("cold_guest_uptime_at_first_exec", up.stdout.trim().into());
    assert!(!st.from_snapshot);

    // A create token for one team can't create in another.
    let mut other = spec(2);
    other.team = "someone-else".into();
    let (s, _) = call(
        &app,
        "POST",
        "/v1/sandboxes",
        Some(&keys.token(&other.id, TEAM, &[Scope::Create])),
        Body::from(serde_json::to_string(&other).unwrap()),
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    // Dev mode as a normal user has no networking, and says so. The
    // network e2e (`network_e2e.rs`) runs as root.
    let mut net = spec(3);
    net.network = NetworkMode::Internet;
    let (s, b) = call(
        &app,
        "POST",
        "/v1/sandboxes",
        Some(&keys.token(&net.id, TEAM, &[Scope::Create])),
        Body::from(serde_json::to_string(&net).unwrap()),
    )
    .await;
    assert_eq!(
        s,
        StatusCode::NOT_IMPLEMENTED,
        "{}",
        String::from_utf8_lossy(&b)
    );

    // ── Exec ──
    let exec_a = keys.token(&ida, TEAM, &[Scope::Exec]);
    let uri = format!("/v1/sandboxes/{ida}/exec");
    let r: ExecResult = json(
        &app,
        "POST",
        &uri,
        &exec_a,
        serde_json::json!({"cmd": "echo $GREETING; echo err >&2; exit 3"}),
    )
    .await;
    assert_eq!(
        (r.exit_code, r.stdout.as_str(), r.stderr.as_str()),
        (Some(3), "hi\n", "err\n")
    );
    let r: ExecResult = json(
        &app,
        "POST",
        &uri,
        &exec_a,
        serde_json::json!({"cmd": ["cat"], "stdin": "piped in"}),
    )
    .await;
    assert_eq!(r.stdout, "piped in");
    let r: ExecResult = json(
        &app,
        "POST",
        &uri,
        &exec_a,
        serde_json::json!({"cmd": "sleep 30", "timeoutMs": 300}),
    )
    .await;
    assert!(r.timed_out && r.signal == Some(9), "{r:?}");
    let r: ExecResult = json(
        &app,
        "POST",
        &uri,
        &exec_a,
        serde_json::json!({"cmd": "cat /proc/1/cmdline; echo; hostname; mount | grep -c overlay"}),
    )
    .await;
    assert!(r.stdout.starts_with("/sbin/pyo-envd"), "{r:?}");
    assert!(r.stdout.contains("sandbox\n"), "{r:?}");
    let mut rts = Vec::new();
    for _ in 0..20 {
        let t = Instant::now();
        let _: ExecResult = json(
            &app,
            "POST",
            &uri,
            &exec_a,
            serde_json::json!({"cmd": ["true"]}),
        )
        .await;
        rts.push(t.elapsed().as_secs_f64() * 1000.0);
    }
    note("exec_true_api_roundtrip_ms_median", median(rts).into());

    // ── Auth ──
    let (s, _) = call(&app, "POST", &uri, None, Body::from(r#"{"cmd":"true"}"#)).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    let files_a = keys.token(&ida, TEAM, &[Scope::Files]);
    let (s, b) = call(
        &app,
        "POST",
        &uri,
        Some(&files_a),
        Body::from(r#"{"cmd":"true"}"#),
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN, "{}", String::from_utf8_lossy(&b));
    let (s, _) = call(
        &app,
        "POST",
        &uri,
        Some(&keys.token(&spec(9).id, TEAM, &[Scope::Exec])),
        Body::from(r#"{"cmd":"true"}"#),
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    let (s, _) = call(
        &app,
        "POST",
        &uri,
        Some(&keys.token(&ida, "intruder", &[Scope::Exec])),
        Body::from(r#"{"cmd":"true"}"#),
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    let stranger = SigningKey::generate("e2e").sign(&Claims {
        kid: String::new(),
        sbx: ida.clone(),
        team: TEAM.into(),
        srv: SERVER.into(),
        iat: now_s(),
        exp: now_s() + 60,
        scopes: vec![Scope::Exec],
    });
    let (s, _) = call(
        &app,
        "POST",
        &uri,
        Some(&stranger),
        Body::from(r#"{"cmd":"true"}"#),
    )
    .await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);

    // ── Files ──
    let furi = |q: &str| format!("/v1/sandboxes/{ida}/files{q}");
    let payload: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
    let t = Instant::now();
    let (s, b) = call(
        &app,
        "PUT",
        &furi("?path=/root/data/blob.bin&mode=600"),
        Some(&files_a),
        Body::from(payload.clone()),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{}", String::from_utf8_lossy(&b));
    let (s, got) = call(
        &app,
        "GET",
        &furi("?path=/root/data/blob.bin"),
        Some(&files_a),
        Body::empty(),
    )
    .await;
    note(
        "file_300k_put_get_ms",
        (t.elapsed().as_millis() as u64).into(),
    );
    assert_eq!(s, StatusCode::OK);
    assert_eq!(got, payload);
    let list: Response = json(
        &app,
        "GET",
        &furi("/list?path=/root/data"),
        &files_a,
        serde_json::Value::Null,
    )
    .await;
    match list {
        Response::Entries { entries } => {
            assert_eq!(entries.len(), 1);
            assert_eq!(
                (entries[0].name.as_str(), entries[0].size, entries[0].mode),
                ("blob.bin", 300_000, 0o600)
            );
        }
        other => panic!("{other:?}"),
    }
    let (s, _) = call(
        &app,
        "GET",
        &furi("?path=/root/nope"),
        Some(&files_a),
        Body::empty(),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    // The root disk is read-only and shared, writes land in the overlay.
    let r: ExecResult = json(
        &app,
        "POST",
        &uri,
        &exec_a,
        serde_json::json!({"cmd": "echo x > /bin/new && ls /bin/new"}),
    )
    .await;
    assert_eq!(r.exit_code, Some(0), "{r:?}");

    // ── WebSocket: a terminal, and a request the token doesn't allow ──
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(axum::serve(listener, app.clone()).into_future());
    let ws_token = keys.token(&ida, TEAM, &[Scope::Pty]);
    let (mut ws, _) = tokio_tungstenite::connect_async(format!(
        "ws://{addr}/v1/sandboxes/{ida}/connect?token={ws_token}"
    ))
    .await
    .unwrap();
    let recv = |m: tokio_tungstenite::tungstenite::Message| Frame::decode(&m.into_data()).unwrap();
    let hello = recv(ws.next().await.unwrap().unwrap());
    assert_eq!(hello.kind, Kind::Hello);
    let send = |f: Frame| tokio_tungstenite::tungstenite::Message::Binary(f.encode().into());
    let pty = Request::Pty(PtySpec {
        cmd: Some(Cmd::Argv(vec!["/bin/sh".into()])),
        cwd: None,
        env: BTreeMap::new(),
        cols: 80,
        rows: 24,
    });
    ws.send(send(Frame::request(1, &pty))).await.unwrap();
    ws.send(send(Frame::new(
        Kind::Stdin,
        1,
        b"echo ws-$((40+2)); exit 5\n".to_vec(),
    )))
    .await
    .unwrap();
    ws.send(send(Frame::request(2, &Request::PortConnect { port: 80 })))
        .await
        .unwrap();
    let (mut out, mut exit, mut refused) = (Vec::new(), None, None);
    while exit.is_none() || refused.is_none() {
        let f = recv(
            tokio::time::timeout(Duration::from_secs(10), ws.next())
                .await
                .unwrap()
                .unwrap()
                .unwrap(),
        );
        match (f.stream, f.kind) {
            (1, Kind::Stdout) => out.extend(f.payload),
            (1, Kind::Response) => {
                if let Response::Exit { code, .. } = f.parse().unwrap() {
                    exit = Some(code);
                }
            }
            (2, Kind::Response) => refused = Some(f.parse::<Response>().unwrap()),
            _ => {}
        }
    }
    assert!(
        String::from_utf8_lossy(&out).contains("ws-42"),
        "{}",
        String::from_utf8_lossy(&out)
    );
    assert_eq!(exit, Some(Some(5)));
    assert!(
        matches!(
            refused,
            Some(Response::Error {
                kind: ErrorKind::PermissionDenied,
                ..
            })
        ),
        "{refused:?}"
    );
    drop(ws);
    server.abort();

    edge_proxy(&state, &keys, &run, &ida).await;
    note("edge_proxy", "ok".into());

    // ── Template snapshot, then restores ──
    let admin = keys.admin();
    let snap: DaemonResult = json(
        &app,
        "POST",
        "/v1/commands",
        &admin,
        serde_json::to_value(DaemonCommand::SnapshotTemplate {
            template: "busybox".into(),
            cpus: 1,
            memory_mib: 256,
            disk_mib: 256,
            network: None,
        })
        .unwrap(),
    )
    .await;
    let DaemonResult::TemplateSnapshot { key, took_ms, .. } = snap else {
        panic!("{snap:?}")
    };
    note("template_snapshot_ms", took_ms.into());
    let templates = mgr.report().templates;
    let busybox = templates.iter().find(|t| t.name == "busybox").unwrap();
    assert!(
        busybox.has_snapshot(NetworkMode::None, 1, 256, 256),
        "{templates:?}"
    );
    let sdir = layout.template_snapshot("busybox", &key);
    let mem = std::fs::metadata(sdir.join("vm.mem")).unwrap();
    use std::os::unix::fs::MetadataExt;
    note(
        "template_mem_file_mib",
        serde_json::json!({"apparent": mem.len() >> 20, "on_disk": (mem.blocks() * 512) >> 20}),
    );

    let mut restored = Vec::new();
    let mut starts = Vec::new();
    let mut walls = Vec::new();
    for n in 10..20u8 {
        let s = spec(n);
        let tok = keys.token(
            &s.id,
            TEAM,
            &[Scope::Create, Scope::Exec, Scope::Files, Scope::Lifecycle],
        );
        let t = Instant::now();
        let st = sandbox_of(
            json(
                &app,
                "POST",
                "/v1/sandboxes",
                &tok,
                serde_json::to_value(&s).unwrap(),
            )
            .await,
        );
        let r: ExecResult = json(
            &app,
            "POST",
            &format!("/v1/sandboxes/{}/exec", s.id),
            &tok,
            serde_json::json!({"cmd": ["true"]}),
        )
        .await;
        walls.push(t.elapsed().as_secs_f64() * 1000.0);
        assert_eq!(r.exit_code, Some(0));
        assert!(st.from_snapshot);
        starts.push(st.start_ms.unwrap() as f64);
        restored.push((s.id.clone(), tok));
    }
    note("restore_start_ms_median", median(starts.clone()).into());
    note("restore_start_ms_all", starts.into());
    note(
        "restore_create_plus_first_exec_api_ms_median",
        median(walls).into(),
    );

    // The clock was set after the restore.
    let (id1, tok1) = restored[0].clone();
    let r: ExecResult = json(
        &app,
        "POST",
        &format!("/v1/sandboxes/{id1}/exec"),
        &tok1,
        serde_json::json!({"cmd": "date +%s"}),
    )
    .await;
    let guest: i64 = r.stdout.trim().parse().unwrap();
    assert!(
        (guest - now_s() as i64).abs() <= 2,
        "guest clock {guest}, host {}",
        now_s()
    );
    // Copies are independent.
    let (id2, tok2) = restored[1].clone();
    let _: ExecResult = json(
        &app,
        "POST",
        &format!("/v1/sandboxes/{id1}/exec"),
        &tok1,
        serde_json::json!({"cmd": "echo 1 > /root/only-here"}),
    )
    .await;
    let r: ExecResult = json(
        &app,
        "POST",
        &format!("/v1/sandboxes/{id2}/exec"),
        &tok2,
        serde_json::json!({"cmd": "test -e /root/only-here"}),
    )
    .await;
    assert_eq!(r.exit_code, Some(1));

    // Memory: dev mode reads the VM process's RSS (no cgroup per VM).
    tokio::time::sleep(Duration::from_millis(300)).await;
    let report = mgr.report();
    let rss: Vec<f64> = report
        .sandboxes
        .iter()
        .filter(|s| s.from_snapshot)
        .filter_map(|s| s.memory_used)
        .map(|b| b as f64 / 1048576.0)
        .collect();
    note("restored_rss_mib_median", median(rss).into());
    note("capacity", serde_json::to_value(&report.capacity).unwrap());
    assert_eq!(report.capacity.running, 11);

    // ── Pause and resume ──
    let (id3, tok3) = restored[2].clone();
    let e3 = format!("/v1/sandboxes/{id3}/exec");
    let _: ExecResult = json(
        &app,
        "POST",
        &e3,
        &tok3,
        serde_json::json!({"cmd": "echo kept > /root/kept; sleep 1000 >/dev/null 2>&1 & echo $! > /root/pid"}),
    )
    .await;
    let t = Instant::now();
    let st = sandbox_of(
        json(
            &app,
            "POST",
            &format!("/v1/sandboxes/{id3}/pause"),
            &tok3,
            serde_json::Value::Null,
        )
        .await,
    );
    note("pause_ms", (t.elapsed().as_millis() as u64).into());
    assert_eq!(st.state, pyo_proto::sandbox::SandboxState::Paused);
    let pmem = std::fs::metadata(layout.vm_dir(&id3).join("pause.mem")).unwrap();
    note(
        "pause_mem_file_mib",
        serde_json::json!({"apparent": pmem.len() >> 20, "on_disk": (pmem.blocks() * 512) >> 20}),
    );
    let (s, _) = call(
        &app,
        "POST",
        &e3,
        Some(&tok3),
        Body::from(r#"{"cmd":"true"}"#),
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT);
    let t = Instant::now();
    let st = sandbox_of(
        json(
            &app,
            "POST",
            &format!("/v1/sandboxes/{id3}/resume"),
            &tok3,
            serde_json::Value::Null,
        )
        .await,
    );
    note("resume_api_ms", (t.elapsed().as_millis() as u64).into());
    note("resume_start_ms", st.start_ms.into());
    let r: ExecResult = json(
        &app,
        "POST",
        &e3,
        &tok3,
        serde_json::json!({"cmd": "cat /root/kept; kill -0 $(cat /root/pid) && echo alive"}),
    )
    .await;
    assert_eq!(r.stdout, "kept\nalive\n", "{r:?}");

    // ── A daemon restart keeps running VMs ──
    let mgr2 = Manager::new(cfg.clone(), layout.clone(), fc.clone(), mgr.capability());
    let adopted = mgr2.adopt().await;
    note("adopted_after_restart", adopted.into());
    assert_eq!(adopted, 11);
    let r = mgr2
        .exec(
            &ida,
            &serde_json::from_value(serde_json::json!({"cmd": "cat /root/data/blob.bin | wc -c"}))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r.stdout.trim(), "300000");

    // ── Lifetimes ──
    mgr.spawn_sweeper();
    let (id4, tok4) = restored[3].clone();
    let _ = sandbox_of(
        json(
            &app,
            "POST",
            &format!("/v1/sandboxes/{id4}/ttl"),
            &tok4,
            serde_json::json!({"ttlSecs": 1}),
        )
        .await,
    );
    let t = Instant::now();
    while mgr.find(&id4).is_ok() {
        assert!(
            t.elapsed() < Duration::from_secs(5),
            "the lifetime didn't end the sandbox"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(!layout.sandbox_top(&id4).exists());

    // ── Delete everything ──
    let (s, _) = call(
        &app,
        "DELETE",
        &format!("/v1/sandboxes/{ida}"),
        Some(&exec_a),
        Body::empty(),
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    let _: DaemonResult = json(
        &app,
        "DELETE",
        &format!("/v1/sandboxes/{ida}"),
        &keys.token(&ida, TEAM, &[Scope::Lifecycle]),
        serde_json::Value::Null,
    )
    .await;
    mgr.delete_all().await;
    assert_eq!(std::fs::read_dir(layout.sandboxes()).unwrap().count(), 0);
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        vms_under(&run),
        Vec::<u32>::new(),
        "Firecracker processes are left"
    );

    println!("e2e summary: {}", serde_json::Value::Object(summary));
}
