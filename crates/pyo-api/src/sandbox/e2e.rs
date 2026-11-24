//! Pyo against a real `pyo-sandbox` daemon booting real Firecracker
//! microVMs on this machine. Runs only with `PYO_SANDBOX_E2E=1` and a
//! `/dev/kvm` this user can open, so plain `cargo test` skips it. The daemon
//! runs in dev mode (no jailer, no network) on 127.0.0.1.
//!
//! ```text
//! crates/pyo-sandbox/scripts/e2e-assets.sh     # Firecracker and a kernel into .cache/
//! cargo build --release -p pyo-envd --target x86_64-unknown-linux-musl
//! cargo build -p pyo-sandbox
//! PYO_SANDBOX_E2E=1 cargo test -p pyo-api sandbox::e2e -- --nocapture
//! ```
//!
//! Without a database, Pyo's own pieces drive the daemon: its signing key
//! and tokens, the scheduler over the daemon's report, and its daemon
//! client. With `DATABASE_URL` (a throwaway Postgres) as well, the whole
//! control plane does: the runner reads the report, `create` checks limits,
//! picks the server and records the sandbox, the client execs with the
//! connection's token, and `delete` ends it.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use pyo_proto::sandbox::token::SigningKey;
use pyo_proto::sandbox::wire::Cmd;
use pyo_proto::sandbox::{
    DaemonCommand, ErrorCode, ExecRequest, NetworkMode, SandboxSpec, SandboxState,
};
use serde_json::json;
use sqlx::PgPool;

use super::{daemon::Daemon, keys, scheduler};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap()
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

fn input(var: &str, default: PathBuf) -> PathBuf {
    std::env::var_os(var).map(PathBuf::from).unwrap_or(default)
}

/// The daemon process and this run's files, gone when the test ends.
struct Run {
    dir: PathBuf,
    daemon: Option<Child>,
}

impl Drop for Run {
    fn drop(&mut self) {
        if let Some(mut d) = self.daemon.take() {
            let _ = d.kill();
            let _ = d.wait();
        }
        // VMs outlive the daemon by design: kill this run's.
        for e in std::fs::read_dir("/proc").into_iter().flatten().flatten() {
            let Ok(pid) = e.file_name().to_string_lossy().parse::<i32>() else {
                continue;
            };
            let cwd = std::fs::read_link(e.path().join("cwd")).unwrap_or_default();
            let comm = std::fs::read_to_string(e.path().join("comm")).unwrap_or_default();
            if cwd.starts_with(&self.dir) && comm.trim() == "firecracker" {
                // SAFETY: a Firecracker process this test started.
                unsafe { libc::kill(pid, libc::SIGKILL) };
            }
        }
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// A template root disk: the host's static busybox and envd.
fn busybox_rootfs(dir: &Path, envd: &Path, out: &Path) {
    let src = dir.join("template-src");
    for d in [
        "bin", "sbin", "proc", "sys", "dev", "tmp", "run", "root", "mnt", "etc",
    ] {
        std::fs::create_dir_all(src.join(d)).unwrap();
    }
    std::fs::copy("/usr/bin/busybox", src.join("bin/busybox")).unwrap();
    let list = Command::new("/usr/bin/busybox")
        .arg("--list")
        .output()
        .unwrap();
    for applet in String::from_utf8_lossy(&list.stdout).lines() {
        let p = src.join("bin").join(applet);
        if applet != "busybox" && !p.exists() {
            std::os::unix::fs::symlink("busybox", p).unwrap();
        }
    }
    std::fs::copy(envd, src.join("sbin/pyo-envd")).unwrap();
    std::fs::write(src.join("etc/passwd"), "root:x:0:0:root:/root:/bin/sh\n").unwrap();
    std::fs::create_dir_all(out.parent().unwrap()).unwrap();
    let mkfs = ["/usr/sbin/mkfs.ext4", "/sbin/mkfs.ext4"]
        .into_iter()
        .find(|p| Path::new(p).exists())
        .unwrap_or("mkfs.ext4");
    let ok = Command::new(mkfs)
        .args(["-q", "-F", "-L", "busybox", "-d"])
        .arg(&src)
        .arg(out)
        .arg("64M")
        .status()
        .unwrap()
        .success();
    assert!(ok, "mkfs.ext4 failed");
    std::fs::remove_dir_all(&src).unwrap();
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// Sets up a daemon for `server` checking `key`'s tokens, with the
/// template `template` in its pool. Returns the run and the daemon's
/// address.
async fn start_daemon(server: &str, template: &str, key: &SigningKey) -> (Run, String) {
    let root = repo_root();
    let cache = root.join(".cache/sandbox-e2e");
    let target = root.join(
        std::env::var_os("CARGO_TARGET_DIR")
            .map(PathBuf::from)
            .unwrap_or("target".into()),
    );
    let daemon = input("PYO_SANDBOX_DAEMON", target.join("debug/pyo-sandbox"));
    let fc = input("PYO_SANDBOX_FIRECRACKER", cache.join("bin/firecracker"));
    let kernel = input("PYO_SANDBOX_KERNEL", cache.join("vmlinux"));
    let envd = input(
        "PYO_SANDBOX_ENVD",
        target.join("x86_64-unknown-linux-musl/release/pyo-envd"),
    );
    for (what, p) in [
        ("pyo-sandbox", &daemon),
        ("Firecracker", &fc),
        ("the kernel", &kernel),
        ("pyo-envd (musl)", &envd),
    ] {
        assert!(
            p.exists(),
            "{what} isn't at {}. See the top of this file.",
            p.display()
        );
    }
    // Each test its own directory: they run in parallel, and each removes
    // its directory when it ends.
    static RUNS: AtomicUsize = AtomicUsize::new(0);
    let n = RUNS.fetch_add(1, Ordering::Relaxed);
    let dir = cache.join(format!("api-run-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let mut run = Run {
        dir: dir.clone(),
        daemon: None,
    };
    busybox_rootfs(
        &dir,
        &envd,
        &dir.join("pool/templates")
            .join(template)
            .join("rootfs.ext4"),
    );

    let port = free_port();
    let address = format!("127.0.0.1:{port}");
    // The config Pyo hands a joining server, pointed at this machine.
    let mut cfg = super::join::daemon_config(server, "127.0.0.1", 0, 0, &[key.verifying_key()]);
    cfg["listen"] = json!(address);
    cfg["root"] = json!(dir.join("pool"));
    cfg["firecracker"] = json!(fc);
    cfg["kernel"] = json!(kernel);
    cfg["envd"] = json!(envd);
    cfg["hostReserveMib"] = json!(1024);
    cfg.as_object_mut().unwrap().remove("jailer");
    let cfg_path = dir.join("config.json");
    std::fs::write(&cfg_path, cfg.to_string()).unwrap();
    let child = Command::new(&daemon)
        .args(["serve", "--dev", "--config"])
        .arg(&cfg_path)
        .env("RUST_LOG", "warn")
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    run.daemon = Some(child);
    let health = format!("http://{address}/v1/health");
    for _ in 0..100 {
        if reqwest::get(&health)
            .await
            .is_ok_and(|r| r.status().is_success())
        {
            return (run, address);
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("the daemon didn't come up on {address}");
}

fn exec_req(cmd: &str) -> ExecRequest {
    ExecRequest {
        cmd: Cmd::Shell(cmd.into()),
        cwd: None,
        env: Default::default(),
        stdin: None,
        timeout_ms: Some(10_000),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pyo_drives_a_real_daemon() {
    if !enabled() {
        return;
    }
    let server = "sbs_e2e";
    let template = "stp_e2ebusybox";
    let team = "tm_e2e";
    let key = SigningKey::generate("sbk_e2e");
    let (_run, address) = start_daemon(server, template, &key).await;
    let d = Daemon::new(&address);

    // The report, as the runner reads it, and the scheduler's pick.
    let report = d.report(&keys::admin_token(&key, server)).await.unwrap();
    assert_eq!(report.server_id, server);
    assert!(
        report.templates.iter().any(|t| t.name == template),
        "{:?}",
        report.templates
    );
    let candidate = scheduler::Candidate {
        id: server.into(),
        name: "e2e".into(),
        team_id: Some(team.into()),
        enabled: true,
        ready: true,
        report_age_secs: 0,
        capability: report.capability.clone(),
        capacity: report.capacity.clone(),
        templates: report.templates.clone(),
        pending_bytes: 0,
    };
    let mut spec = SandboxSpec::new(super::new_sandbox_id(), team, template);
    spec.memory_mib = 256;
    spec.disk_mib = 256;
    spec.network = NetworkMode::None;
    spec.env.insert("GREETING".into(), "hi".into());
    let picked = scheduler::pick(std::slice::from_ref(&candidate), &spec).unwrap();
    assert_eq!(picked.id, server);

    // Pyo creates it with a create token for that sandbox only.
    let st = d
        .sandbox(
            &keys::create_token(&key, &spec.id, team, server),
            &DaemonCommand::Create { spec: spec.clone() },
        )
        .await
        .unwrap();
    assert_eq!(st.state, SandboxState::Running);
    println!("e2e start_ms: {:?}", st.start_ms);

    // The client execs with its own token.
    let client = keys::client_token(&key, &spec.id, team, server);
    let r = d
        .exec(
            &client.token,
            &spec.id,
            &exec_req("echo $GREETING; echo err >&2; exit 3"),
        )
        .await
        .unwrap();
    assert_eq!(
        (r.exit_code, r.stdout.as_str(), r.stderr.as_str()),
        (Some(3), "hi\n", "err\n")
    );
    d.write_file(&client.token, &spec.id, "/tmp/a.txt", b"from pyo".to_vec())
        .await
        .unwrap();
    assert_eq!(
        d.read_file(&client.token, &spec.id, "/tmp/a.txt")
            .await
            .unwrap(),
        b"from pyo"
    );
    // A client token can't end the sandbox, nor reach another.
    let e = d
        .command(
            &client.token,
            &DaemonCommand::Delete {
                id: spec.id.clone(),
            },
        )
        .await
        .unwrap_err();
    assert_eq!(e.code, ErrorCode::Forbidden, "{e}");
    let other = keys::client_token(&key, "sbx_otherotherotherot", team, server);
    let e = d
        .exec(&other.token, &spec.id, &exec_req("true"))
        .await
        .unwrap_err();
    assert_eq!(e.code, ErrorCode::Forbidden, "{e}");
    // Another installation's key opens nothing.
    let stranger = keys::client_token(&SigningKey::generate("sbk_e2e"), &spec.id, team, server);
    let e = d
        .exec(&stranger.token, &spec.id, &exec_req("true"))
        .await
        .unwrap_err();
    assert_eq!(e.code, ErrorCode::Unauthorized, "{e}");

    let admin = keys::admin_token(&key, server);
    d.command(
        &admin,
        &DaemonCommand::Delete {
            id: spec.id.clone(),
        },
    )
    .await
    .unwrap();
    assert!(
        d.report(&admin)
            .await
            .unwrap()
            .sandboxes
            .iter()
            .all(|x| x.id != spec.id || x.state.is_final())
    );
}

/// The whole control plane, with a throwaway Postgres.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_control_plane_drives_a_real_daemon() {
    if !enabled() {
        return;
    }
    let Ok(url) = std::env::var("DATABASE_URL") else {
        eprintln!("skipping: set DATABASE_URL (a throwaway Postgres) for the whole control plane");
        return;
    };
    let db = PgPool::connect(&url).await.unwrap();
    crate::migrate::run(&db).await.unwrap();
    let s = test_state(db.clone());
    keys::reset_cache();
    // Pyo issues the key the daemon trusts, as a join hands it over.
    let key = keys::signing_key(&s.db, &s.sealer).await.unwrap();
    let run_id = crate::ids::random_string(8);
    let (team, server, template) = (
        format!("tm_e2e{run_id}"),
        format!("sbs_e2e{run_id}"),
        format!("stp_e2e{run_id}"),
    );
    let (_run, address) = start_daemon(&server, &template, &key).await;

    let user = crate::auth::User {
        id: format!("usr_e2e{run_id}"),
        name: "E2E".into(),
        email: format!("{run_id}@e2e.test"),
        timezone: "UTC".into(),
        is_admin: false,
        must_set_up: false,
        key: None,
    };
    for q in ["INSERT INTO pyo.users (id, email, name, password_hash) VALUES ($1, $2, 'E2E', '')"] {
        sqlx::query(q)
            .bind(&user.id)
            .bind(&user.email)
            .execute(&db)
            .await
            .unwrap();
    }
    sqlx::query("INSERT INTO pyo.teams (id, name, slug) VALUES ($1, 'E2E', $1)")
        .bind(&team)
        .execute(&db)
        .await
        .unwrap();
    sqlx::query("INSERT INTO pyo.members (id, team_id, user_id, email, role, status) VALUES ($1, $2, $3, $4, 'owner', 'active')")
        .bind(format!("mem_e2e{run_id}"))
        .bind(&team)
        .bind(&user.id)
        .bind(&user.email)
        .execute(&db)
        .await
        .unwrap();
    // A server that has joined, and the template on it.
    sqlx::query("INSERT INTO pyo.sandbox_servers (id, team_id, name, address, state, created_by, joined_at) VALUES ($1, $2, 'e2e', $3, 'joining', 'test', now())")
        .bind(&server)
        .bind(&team)
        .bind(&address)
        .execute(&db)
        .await
        .unwrap();
    sqlx::query("INSERT INTO pyo.sandbox_templates (id, team_id, name, source, state, created_by) VALUES ($1, $2, 'busybox', '{\"kind\":\"image\",\"image\":\"busybox\"}', 'ready', 'test')")
        .bind(&template)
        .bind(&team)
        .execute(&db)
        .await
        .unwrap();

    // No report yet: nowhere to go.
    let req = |ttl: &str| -> super::CreateSandbox {
        serde_json::from_value(json!({ "teamId": team, "template": "busybox", "memoryMib": 256, "diskMib": 256, "network": "none", "ttl": ttl, "env": { "GREETING": "hi" } })).unwrap()
    };
    let e = super::create(&s, &user, req("5m")).await.unwrap_err();
    assert!(
        e.message.contains("No sandbox server is up"),
        "{}",
        e.message
    );

    // The runner reads the report: ready.
    super::runner::round(&s).await.unwrap();
    let srv = &super::server_views(
        &mut db.acquire().await.unwrap(),
        std::slice::from_ref(&team),
        Some(&server),
    )
    .await
    .unwrap()[0];
    assert_eq!(srv["state"], "ready", "{srv}");
    assert_eq!(srv["capability"]["runtime"], "firecracker");

    // Limits: a lifetime past the team's longest is refused.
    let mut c = db.acquire().await.unwrap();
    super::limits::save(
        &mut c,
        &team,
        &super::limits::Limits {
            max_ttl_secs: Some(600),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    drop(c);
    let e = super::create(&s, &user, req("1h")).await.unwrap_err();
    assert_eq!(e.status, axum::http::StatusCode::FORBIDDEN, "{}", e.message);
    let e = super::create(&s, &user, req("none")).await.unwrap_err();
    assert_eq!(e.status, axum::http::StatusCode::FORBIDDEN, "{}", e.message);

    let out = super::create(&s, &user, req("5m")).await.unwrap();
    let id = out["sandbox"]["id"].as_str().unwrap().to_owned();
    assert_eq!(out["sandbox"]["state"], "running", "{out}");
    assert_eq!(out["sandbox"]["env"], json!(["GREETING"]), "names only");
    let conn = &out["connection"];
    assert_eq!(conn["url"], format!("http://{address}"));
    let token = conn["token"].as_str().unwrap();
    let r = Daemon::new(&address)
        .exec(token, &id, &exec_req("echo $GREETING from $(hostname)"))
        .await
        .unwrap();
    assert_eq!(r.stdout, "hi from sandbox\n");
    let kept: serde_json::Value =
        sqlx::query_scalar("SELECT spec FROM pyo.sandboxes WHERE id = $1")
            .bind(&id)
            .fetch_one(&db)
            .await
            .unwrap();
    assert_eq!(kept["env"]["GREETING"], "", "env values aren't kept");

    // getSandbox brings a fresh connection, and no ports yet.
    let got = super::get(&s, &user, &id).await.unwrap();
    assert_eq!(got["connection"]["sandboxId"], json!(id), "{got}");
    assert_eq!(got["ports"], json!([]), "{got}");

    // A new token, then the runner meters it.
    let t2 = super::token(&s, &user, &id).await.unwrap();
    assert!(
        Daemon::new(&address)
            .exec(&t2.token, &id, &exec_req("true"))
            .await
            .is_ok()
    );
    tokio::time::sleep(Duration::from_millis(1100)).await;
    super::runner::round(&s).await.unwrap();
    let secs: Option<i64> = sqlx::query_scalar(
        "SELECT sum(seconds)::bigint FROM pyo.sandbox_usage WHERE sandbox_id = $1",
    )
    .bind(&id)
    .fetch_one(&db)
    .await
    .unwrap();
    assert!(secs.unwrap_or(0) >= 1, "{secs:?}");

    // Extend within the limit, then delete.
    let v = super::extend(&s, &user, &id, Some(&json!("10m")))
        .await
        .unwrap();
    assert!(v["expiresAt"].as_i64().unwrap() > pyo_proto::now_ms() + 9 * 60_000);
    let v = super::delete(&s, &user, &id).await.unwrap();
    assert_eq!(v["state"], "stopped");
    let e = super::token(&s, &user, &id).await.unwrap_err();
    assert_eq!(e.status, axum::http::StatusCode::CONFLICT);
    let got = super::get(&s, &user, &id).await.unwrap();
    assert!(got["connection"].is_null(), "{got}");
    assert_eq!(got["connectionNote"], "The sandbox has ended.");

    // Without teamId: the user's only team.
    let mut no_team = req("5m");
    no_team.team_id = None;
    let out = super::create(&s, &user, no_team).await.unwrap();
    assert_eq!(out["sandbox"]["teamId"], json!(team), "{out}");
    let id2 = out["sandbox"]["id"].as_str().unwrap().to_owned();
    super::delete(&s, &user, &id2).await.unwrap();

    // ── A template from a public image, made by the server ──
    let image = std::env::var("PYO_SANDBOX_E2E_IMAGE").unwrap_or("alpine:3.22".into());
    let made = super::create_template(
        &s,
        &user,
        serde_json::from_value(json!({ "teamId": team, "name": "img-e2e", "image": image }))
            .unwrap(),
    )
    .await
    .unwrap();
    // The push to the server runs in the background, so it may already be building.
    assert!(
        made["state"] == "pending" || made["state"] == "building",
        "{made}"
    );
    let tid = made["id"].as_str().unwrap().to_owned();
    let dup = super::create_template(
        &s,
        &user,
        serde_json::from_value(json!({ "teamId": team, "name": "img-e2e", "image": "alpine" }))
            .unwrap(),
    )
    .await
    .unwrap_err();
    assert_eq!(dup.status, axum::http::StatusCode::CONFLICT);
    let t0 = std::time::Instant::now();
    let view = loop {
        super::runner::round(&s).await.unwrap();
        let v = super::template_views(&db, std::slice::from_ref(&team), Some(&tid))
            .await
            .unwrap()
            .remove(0);
        println!(
            "e2e template after {} ms: {} {}",
            t0.elapsed().as_millis(),
            v["state"],
            v["servers"]
        );
        if v["state"] == "ready" || v["state"] == "failed" {
            break v;
        }
        assert!(t0.elapsed() < Duration::from_secs(600), "{v}");
        tokio::time::sleep(Duration::from_secs(1)).await;
    };
    assert_eq!(view["state"], "ready", "{view}");
    let on = &view["servers"][0];
    assert_eq!(
        (on["id"].as_str(), on["state"].as_str()),
        (Some(server.as_str()), Some("ready")),
        "{view}"
    );
    println!("e2e template: {}", view["servers"]);
    let tmpl_req: super::CreateSandbox = serde_json::from_value(
        json!({ "teamId": team, "template": "img-e2e", "network": "none", "ttl": "5m" }),
    )
    .unwrap();
    let out = super::create(&s, &user, tmpl_req).await.unwrap();
    let id3 = out["sandbox"]["id"].as_str().unwrap().to_owned();
    let r = Daemon::new(&address)
        .exec(
            out["connection"]["token"].as_str().unwrap(),
            &id3,
            &exec_req("cat /etc/os-release | head -1; ls -l /sbin/pyo-envd"),
        )
        .await
        .unwrap();
    println!("e2e template exec: {}", r.stdout);
    assert_eq!(r.exit_code, Some(0), "{r:?}");
    let e = super::delete_template(&s, &user, &tid).await.unwrap_err();
    assert_eq!(e.status, axum::http::StatusCode::CONFLICT, "{}", e.message);
    super::delete(&s, &user, &id3).await.unwrap();
    super::delete_template(&s, &user, &tid).await.unwrap();
    // The server drops its copy: now, or at the next round.
    let t0 = std::time::Instant::now();
    loop {
        super::runner::round(&s).await.unwrap();
        let srv = &super::server_views(
            &mut db.acquire().await.unwrap(),
            std::slice::from_ref(&team),
            Some(&server),
        )
        .await
        .unwrap()[0];
        let names: Vec<&str> = srv["templates"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|t| t["name"].as_str())
            .collect();
        if !names.contains(&tid.as_str()) {
            assert!(
                names.contains(&template.as_str()),
                "the hand-made one stays"
            );
            break;
        }
        assert!(t0.elapsed() < Duration::from_secs(60), "{names:?}");
        tokio::time::sleep(Duration::from_millis(500)).await;
    }

    // ── The built-in template `base`: the default, and how a release moves it ──
    // Two small public images stand in for two releases' base, of Pyos
    // newer than this one, so the rounds below leave them as they are.
    use super::base;
    sqlx::query("DELETE FROM pyo.sandbox_templates WHERE id = $1")
        .bind(base::BASE.id)
        .execute(&db)
        .await
        .unwrap();
    let old = base::Wanted {
        image:
            "alpine:3.21@sha256:bc21487101ae47172509130ee1092cc273a831968d64b617776d1f69c81e42ab"
                .into(),
        release: "99.0.1".into(),
        ..Default::default()
    };
    let new = base::Wanted {
        image:
            "alpine:3.22@sha256:49340b346798875b5592e5d420241f2f9cfba5f47e62ebb1446a1b34734e898f"
                .into(),
        release: "99.0.2".into(),
        ..Default::default()
    };
    let rev = |w: &base::Wanted| {
        super::templates::server_name(base::BASE.id, base::revision(&w.image).as_deref())
    };
    let (old_name, new_name) = (rev(&old), rev(&new));
    // Listed, ready to use, before Pyo adds it.
    let listed = super::template_views(&db, std::slice::from_ref(&team), None)
        .await
        .unwrap();
    assert_eq!(listed[0]["name"], "base", "{listed:?}");
    assert_eq!(listed[0]["servers"], json!([]));
    assert_eq!(
        base::seed_from(&s, &base::BASE, &old).await.unwrap(),
        base::Seed::Insert
    );
    // The server doesn't make it ahead of use.
    for _ in 0..3 {
        super::runner::round(&s).await.unwrap();
    }
    let srv = &super::server_views(
        &mut db.acquire().await.unwrap(),
        std::slice::from_ref(&team),
        Some(&server),
    )
    .await
    .unwrap()[0];
    assert!(
        srv["templates"]
            .as_array()
            .unwrap()
            .iter()
            .all(|t| !t["name"].as_str().unwrap_or("").starts_with("stp_base")),
        "{srv}"
    );
    let views = super::template_views(&db, std::slice::from_ref(&team), Some(base::BASE.id))
        .await
        .unwrap();
    assert_eq!(views[0]["state"], "ready", "ready to use: {}", views[0]);
    assert_eq!(views[0]["servers"], json!([]), "no server has it yet");
    // No template: base. Two at once on a server without it: the server
    // makes it once, both wait for it, then both start.
    let no_template: super::CreateSandbox =
        serde_json::from_value(json!({ "teamId": team, "network": "none", "ttl": "5m" })).unwrap();
    let t0 = std::time::Instant::now();
    let a = tokio::spawn({
        let (s, user, req) = (s.clone(), user.clone(), no_template.clone());
        async move { super::create(&s, &user, req).await }
    });
    let b = tokio::spawn({
        let (s, user, req) = (s.clone(), user.clone(), no_template.clone());
        async move { super::create(&s, &user, req).await }
    });
    // Meanwhile the sandboxes say they're waiting for base.
    let mut saw_preparing = false;
    while !(a.is_finished() && b.is_finished()) {
        let list = super::sandbox_views(&db, std::slice::from_ref(&team), None, true)
            .await
            .unwrap();
        saw_preparing |= list.iter().any(|x| x["preparing"] == true);
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let first = a.await.unwrap().unwrap();
    let other = b.await.unwrap().unwrap();
    println!("e2e base on first use: {} ms", t0.elapsed().as_millis());
    assert!(saw_preparing);
    assert_eq!(first["sandbox"]["template"], "base", "{first}");
    assert_eq!(first["sandbox"]["preparing"], false, "{first}");
    assert_eq!(other["sandbox"]["state"], "running", "{other}");
    let builds: Vec<serde_json::Value> = Daemon::new(&address)
        .report(&keys::admin_token(&key, &server))
        .await
        .unwrap()
        .templates
        .into_iter()
        .filter(|t| t.name.starts_with("stp_base"))
        .map(|t| json!({ "name": t.name, "startedAt": t.build.map(|b| b.started_at) }))
        .collect();
    assert_eq!(builds.len(), 1, "{builds:?}");
    let views = super::template_views(&db, std::slice::from_ref(&team), Some(base::BASE.id))
        .await
        .unwrap();
    assert_eq!(views[0]["servers"][0]["id"], json!(server), "{}", views[0]);
    super::delete(&s, &user, other["sandbox"]["id"].as_str().unwrap())
        .await
        .unwrap();
    // On the server asked for, by name. One the team doesn't have is refused.
    let mut on_e2e = no_template.clone();
    on_e2e.server = Some("e2e".into());
    let pinned = super::create(&s, &user, on_e2e.clone()).await.unwrap();
    assert_eq!(pinned["sandbox"]["serverId"], json!(server), "{pinned}");
    super::delete(&s, &user, pinned["sandbox"]["id"].as_str().unwrap())
        .await
        .unwrap();
    on_e2e.server = Some("nowhere".into());
    let e = super::create(&s, &user, on_e2e).await.unwrap_err();
    assert_eq!(e.status, axum::http::StatusCode::NOT_FOUND, "{}", e.message);
    let first_id = first["sandbox"]["id"].as_str().unwrap().to_owned();
    let on: String =
        sqlx::query_scalar("SELECT spec->>'template' FROM pyo.sandboxes WHERE id = $1")
            .bind(&first_id)
            .fetch_one(&db)
            .await
            .unwrap();
    assert_eq!(on, old_name);
    let r = Daemon::new(&address)
        .exec(
            first["connection"]["token"].as_str().unwrap(),
            &first_id,
            &exec_req("cat /etc/alpine-release"),
        )
        .await
        .unwrap();
    assert!(r.stdout.starts_with("3.21"), "{r:?}");

    // A new release: made beside the old one, new sandboxes move over.
    assert_eq!(
        base::seed_from(&s, &base::BASE, &new).await.unwrap(),
        base::Seed::Move
    );
    assert_eq!(
        base::seed_from(&s, &base::BASE, &old).await.unwrap(),
        base::Seed::Keep,
        "the old Pyo doesn't move it back"
    );
    wait_for(&s, &team, &server, &new_name, false).await;
    let second = super::create(&s, &user, no_template).await.unwrap();
    let second_id = second["sandbox"]["id"].as_str().unwrap().to_owned();
    let r = Daemon::new(&address)
        .exec(
            second["connection"]["token"].as_str().unwrap(),
            &second_id,
            &exec_req("cat /etc/alpine-release"),
        )
        .await
        .unwrap();
    assert!(r.stdout.starts_with("3.22"), "{r:?}");
    // The running sandbox keeps the old disk, which goes once it ends.
    super::runner::round(&s).await.unwrap();
    assert!(ready_on(&db, &team, &server).await.contains(&old_name));
    let r = Daemon::new(&address)
        .exec(
            first["connection"]["token"].as_str().unwrap(),
            &first_id,
            &exec_req("cat /etc/alpine-release"),
        )
        .await
        .unwrap();
    assert!(r.stdout.starts_with("3.21"), "{r:?}");
    super::delete(&s, &user, &first_id).await.unwrap();
    wait_for(&s, &team, &server, &old_name, true).await;
    let views = super::template_views(&db, std::slice::from_ref(&team), Some(base::BASE.id))
        .await
        .unwrap();
    assert_eq!(views[0]["state"], "ready", "{}", views[0]);
    println!("e2e base: {}", views[0]["servers"]);
    super::delete(&s, &user, &second_id).await.unwrap();
    sqlx::query("DELETE FROM pyo.sandbox_templates WHERE id = $1")
        .bind(base::BASE.id)
        .execute(&db)
        .await
        .unwrap();
    super::templates::forget(s.clone(), base::BASE.id.into()).await;

    sqlx::query("DELETE FROM pyo.teams WHERE id = $1")
        .bind(&team)
        .execute(&db)
        .await
        .unwrap();
    sqlx::query("DELETE FROM pyo.users WHERE id = $1")
        .bind(&user.id)
        .execute(&db)
        .await
        .unwrap();
    keys::reset_cache();
}

/// The templates a server reports ready, by the names it keeps them under.
async fn ready_on(db: &PgPool, team: &str, server: &str) -> Vec<String> {
    let srv = &super::server_views(
        &mut db.acquire().await.unwrap(),
        &[team.to_owned()],
        Some(server),
    )
    .await
    .unwrap()[0];
    srv["templates"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|t| t["state"] == "ready" || t["state"].is_null())
        .filter_map(|t| t["name"].as_str().map(str::to_owned))
        .collect()
}

/// Runs the runner until the server has `name` ready, or (`gone`) no longer has it.
async fn wait_for(s: &crate::AppState, team: &str, server: &str, name: &str, gone: bool) {
    let t0 = std::time::Instant::now();
    loop {
        super::runner::round(s).await.unwrap();
        let names = ready_on(&s.db, team, server).await;
        if names.iter().any(|n| n == name) != gone {
            return;
        }
        assert!(
            t0.elapsed() < Duration::from_secs(300),
            "{name} gone={gone}: {names:?}"
        );
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

pub(crate) fn test_state(db: PgPool) -> crate::AppState {
    let config = crate::Config {
        agent_api_url: String::new(),
        agent_image: String::new(),
        waf_image: pyo_proto::system::WAF_IMAGE_DEFAULT.into(),
        geoip_image: pyo_proto::system::GEOIP_IMAGE_DEFAULT.into(),
        cookie_secure: false,
        allow_signup: false,
        builder_image: String::new(),
        relay_addr: None,
    };
    let (_stop, stopping) = tokio::sync::watch::channel(false);
    crate::AppState {
        db: db.clone(),
        config: std::sync::Arc::new(config),
        observability: crate::observability::Observability::from_env(db),
        sealer: std::sync::Arc::new(crate::secrets::Sealer::for_tests()),
        wakers: Default::default(),
        terminals: Default::default(),
        stopping,
    }
}
