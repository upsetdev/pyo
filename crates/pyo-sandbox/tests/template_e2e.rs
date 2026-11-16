//! Makes a template from a real public image and boots a sandbox from it.
//! Runs only with `PYO_SANDBOX_E2E=1`, a `/dev/kvm` this user can open and
//! the internet, so plain `cargo test` skips it. Dev mode: no jailer, no
//! network, no root.
//!
//! ```text
//! crates/pyo-sandbox/scripts/e2e-assets.sh     # Firecracker and a kernel into .cache/
//! cargo build --release -p pyo-envd --target x86_64-unknown-linux-musl
//! PYO_SANDBOX_E2E=1 cargo test -p pyo-sandbox --test template_e2e -- --nocapture
//! ```
//!
//! Inputs as in `e2e.rs`, plus `PYO_SANDBOX_E2E_IMAGE` (default
//! `python:3.13-alpine`).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use pyo_proto::sandbox::token::{ANY_SANDBOX, Claims, SigningKey};
use pyo_proto::sandbox::wire::{Cmd, Scope};
use pyo_proto::sandbox::{
    DaemonCommand, DaemonResult, ErrorCode, ExecRequest, NetworkMode, Runtime, SandboxSpec,
    TemplateReport, TemplateState, sandbox_id_from,
};
use pyo_sandbox::api::{AppState, run_command};
use pyo_sandbox::config::Config;
use pyo_sandbox::detect::{self, Inputs};
use pyo_sandbox::firecracker::Firecracker;
use pyo_sandbox::manager::Manager;
use pyo_sandbox::storage::Layout;

const SERVER: &str = "srv-tmpl-e2e";
const TEAM: &str = "team-e2e";
const TEMPLATE: &str = "stp_e2e";

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

/// Kills this run's VMs and removes its files.
struct Cleanup(PathBuf);

impl Drop for Cleanup {
    fn drop(&mut self) {
        for e in std::fs::read_dir("/proc").into_iter().flatten().flatten() {
            let Ok(pid) = e.file_name().to_string_lossy().parse::<i32>() else {
                continue;
            };
            let cwd = std::fs::read_link(e.path().join("cwd")).unwrap_or_default();
            let comm = std::fs::read_to_string(e.path().join("comm")).unwrap_or_default();
            if cwd.starts_with(&self.0) && comm.trim() == "firecracker" {
                // SAFETY: a Firecracker process this test started.
                unsafe { libc::kill(pid, libc::SIGKILL) };
            }
        }
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn admin(key: &SigningKey) -> String {
    let now = pyo_sandbox::now_ms() / 1000;
    key.sign(&Claims {
        kid: String::new(),
        sbx: ANY_SANDBOX.into(),
        team: String::new(),
        srv: SERVER.into(),
        iat: now,
        exp: now + 3600,
        scopes: vec![Scope::Admin],
    })
}

async fn cmd(
    st: &AppState,
    token: Option<&str>,
    c: DaemonCommand,
) -> Result<DaemonResult, pyo_proto::sandbox::DaemonError> {
    run_command(st, token, c).await.map_err(|e| e.0)
}

fn shell(cmd: &str) -> ExecRequest {
    ExecRequest {
        cmd: Cmd::Shell(cmd.into()),
        cwd: None,
        env: Default::default(),
        stdin: None,
        timeout_ms: Some(20_000),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn makes_a_template_from_a_public_image() {
    if !enabled() {
        return;
    }
    let root = repo_root();
    let cache = root.join(".cache/sandbox-e2e");
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
    for p in [&fc_bin, &kernel, &envd] {
        assert!(
            p.exists(),
            "{} is missing. See the top of this file.",
            p.display()
        );
    }
    let image = std::env::var("PYO_SANDBOX_E2E_IMAGE").unwrap_or("python:3.13-alpine".into());
    let python = image.starts_with("python");

    let run = input("PYO_SANDBOX_E2E_RUN", cache.clone())
        .join(format!("tmpl-run-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&run);
    let _cleanup = Cleanup(run.clone());
    let key = SigningKey::generate("e2e");
    let cfg = Config {
        server_id: SERVER.into(),
        listen: "127.0.0.1:0".parse().unwrap(),
        root: run.clone(),
        firecracker: fc_bin.clone(),
        jailer: None,
        kernel: kernel.clone(),
        envd: envd.clone(),
        runsc: "runsc".into(),
        keys: vec![key.verifying_key()],
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
    let cap = detect::detect(Inputs {
        fc: &fc,
        layout: &layout,
        envd: &envd,
        runsc: std::path::Path::new("runsc"),
    })
    .await;
    assert_eq!(cap.runtime, Runtime::Firecracker, "{}", cap.summary());
    let mgr = Arc::new(Manager::new(cfg.clone(), layout.clone(), fc, cap));
    mgr.adopt().await;
    let st = AppState {
        mgr: mgr.clone(),
        keys: cfg.keys.clone(),
        server_id: SERVER.into(),
    };
    let token = admin(&key);
    let mut summary = serde_json::Map::new();
    let mut note = |k: &str, v: serde_json::Value| {
        println!("template e2e {k}: {v}");
        summary.insert(k.into(), v);
    };

    // ── Prepare: answers at once, then builds ──
    let t = Instant::now();
    let r = cmd(
        &st,
        Some(&token),
        DaemonCommand::PrepareTemplate {
            template: TEMPLATE.into(),
            image: image.clone(),
            plain_http: false,
            auth: None,
            max_mib: None,
        },
    )
    .await
    .unwrap();
    let DaemonResult::Template { template } = r else {
        panic!("not a template: {r:?}");
    };
    assert_eq!(template.state, TemplateState::Building);
    assert!(
        t.elapsed() < Duration::from_secs(2),
        "prepare answers at once"
    );

    // Bad names and images are refused before anything starts.
    for (name, img) in [("../x", "alpine"), ("ok", "not an image")] {
        let e = cmd(
            &st,
            Some(&token),
            DaemonCommand::PrepareTemplate {
                template: name.into(),
                image: img.into(),
                plain_http: false,
                auth: None,
                max_mib: None,
            },
        )
        .await
        .unwrap_err();
        assert_eq!(e.code, ErrorCode::InvalidSpec, "{e:?}");
    }

    // ── The report shows it building, then ready ──
    let mut phases = Vec::new();
    let done: TemplateReport = loop {
        let rep = mgr.report();
        let t = rep
            .templates
            .iter()
            .find(|t| t.name == TEMPLATE)
            .cloned()
            .expect("the template is in the report");
        if let Some(p) = t.build.as_ref().and_then(|b| b.phase.clone())
            && phases.last() != Some(&p)
        {
            println!("template e2e phase: {p} after {} ms", t0(&t));
            phases.push(p);
        }
        if t.state != TemplateState::Building {
            break t;
        }
        assert!(
            Instant::now().duration_since(t_start(&t)) < Duration::from_secs(600),
            "the build took too long"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    };
    let b = done.build.clone().unwrap();
    assert_eq!(done.state, TemplateState::Ready, "{:?}", b.error);
    assert!(phases.contains(&"pulling".to_string()), "{phases:?}");
    note("image", image.clone().into());
    note("digest", b.digest.clone().into());
    note("image_bytes", b.image_bytes.into());
    note("pulled", b.pulled.into());
    note("rootfs_bytes", b.rootfs_bytes.into());
    note("template_bytes", done.bytes.into());
    note("pull_ms", b.pull_ms.into());
    note("unpack_ms", b.unpack_ms.into());
    note("snapshot_ms", b.snapshot_ms.into());
    note("total_ms", (b.finished_at.unwrap() - b.started_at).into());
    note("snapshots", serde_json::json!(done.snapshots));
    assert_eq!(b.pulled, b.image_bytes);
    assert!(
        done.has_snapshot(NetworkMode::None, 1, 512, 2048),
        "{done:?}"
    );
    let cap = mgr.capacity();
    note("disk_templates", cap.disk_templates.into());
    assert!(cap.disk_templates >= done.bytes / 2 && cap.disk_free > 0);

    // Preparing it again leaves it as it is.
    let again = mgr
        .prepare_template(TEMPLATE, &image, false, None, None)
        .unwrap();
    assert_eq!(again.state, TemplateState::Ready);

    // ── A sandbox from it restores from the snapshot and runs commands ──
    let mut spec = SandboxSpec::new(sandbox_id_from([7; 16]), TEAM, TEMPLATE);
    spec.network = NetworkMode::None;
    let t = Instant::now();
    let s = mgr.create(spec.clone()).await.unwrap();
    note("create_ms", (t.elapsed().as_millis() as u64).into());
    note("start_ms", s.start_ms.into());
    assert!(s.from_snapshot);
    let out = mgr
        .exec(&spec.id, &shell("cat /etc/alpine-release; id -u; stat -c '%U:%G %a' /etc/shadow; ls -ld /tmp /mnt /proc"))
        .await
        .unwrap();
    note("exec", out.stdout.clone().into());
    assert_eq!(out.exit_code, Some(0), "{out:?}");
    assert!(
        out.stdout.contains("root:shadow 640"),
        "owners from the image: {}",
        out.stdout
    );
    if python {
        let out = mgr
            .exec(
                &spec.id,
                &shell(
                    "python3 -c 'import sys, sqlite3; print(sum(range(10)), sys.version_info[:2])'",
                ),
            )
            .await
            .unwrap();
        note("python", out.stdout.trim().into());
        assert_eq!(out.exit_code, Some(0), "{out:?}");
        assert!(out.stdout.starts_with("45 (3, 13)"), "{}", out.stdout);
    }
    let out = mgr
        .exec(&spec.id, &shell("echo hi > /etc/new && cat /etc/new"))
        .await
        .unwrap();
    assert_eq!(out.stdout.trim(), "hi", "the writable layer works");

    // ── Delete: refused while a sandbox uses it ──
    let e = mgr.delete_template(TEMPLATE).await.unwrap_err();
    assert_eq!(e.code, ErrorCode::Conflict);
    mgr.delete(&spec.id).await.unwrap();
    let r = cmd(
        &st,
        Some(&token),
        DaemonCommand::DeleteTemplate {
            template: TEMPLATE.into(),
        },
    )
    .await
    .unwrap();
    assert!(matches!(r, DaemonResult::TemplateDeleted { .. }));
    assert!(mgr.report().templates.iter().all(|t| t.name != TEMPLATE));
    assert!(!layout.template_dir(TEMPLATE).exists());

    // ── A build stopped halfway is removed, and an image that isn't there fails ──
    mgr.prepare_template(
        "stp_gone",
        "alpine:this-tag-does-not-exist-pyo",
        false,
        None,
        None,
    )
    .unwrap();
    let failed = loop {
        let t = mgr.template_report("stp_gone").unwrap();
        if t.state != TemplateState::Building {
            break t;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    };
    assert_eq!(failed.state, TemplateState::Failed);
    let msg = failed.build.unwrap().error.unwrap();
    note("missing_image_error", msg.clone().into());
    assert!(msg.contains("manifest"), "{msg}");
    mgr.prepare_template("stp_stop", &image, false, None, None)
        .unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    mgr.delete_template("stp_stop").await.unwrap();
    let t = Instant::now();
    while layout.template_dir("stp_stop").exists() {
        assert!(
            t.elapsed() < Duration::from_secs(120),
            "a stopped build goes away"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    println!(
        "template e2e summary: {}",
        serde_json::Value::Object(summary)
    );
}

fn t_start(t: &TemplateReport) -> Instant {
    let started = t.build.as_ref().map(|b| b.started_at).unwrap_or(0);
    let ago = pyo_sandbox::now_ms().saturating_sub(started);
    Instant::now() - Duration::from_millis(ago)
}

fn t0(t: &TemplateReport) -> u64 {
    pyo_sandbox::now_ms().saturating_sub(t.build.as_ref().map(|b| b.started_at).unwrap_or(0))
}
