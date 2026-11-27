//! Pyo's whole control plane against a real `pyo-sandbox` daemon booting
//! real Firecracker microVMs on this machine, over the outbound channel.
//! Runs only with `PYO_SANDBOX_E2E=1`, a `/dev/kvm` this user can open and
//! `DATABASE_URL` (a throwaway Postgres), so plain `cargo test` skips it.
//! The daemon runs in dev mode (no jailer, no network), joined as
//! outbound/e2e.rs's [`Rig`] does.
//!
//! ```text
//! crates/pyo-sandbox/scripts/e2e-assets.sh     # Firecracker and a kernel into .cache/
//! cargo build --release -p pyo-envd --target x86_64-unknown-linux-musl
//! cargo build -p pyo-sandbox
//! PYO_SANDBOX_E2E=1 DATABASE_URL=… cargo test -p pyo-api sandbox::e2e -- --nocapture
//! ```
//!
//! `create` checks limits, picks the server and records the sandbox, the
//! client execs with the connection's token on the server's tailnet name,
//! reports meter usage, templates are made from public images and on first
//! use, and `delete` ends it.
//!
//! [`Rig`]: super::outbound::e2e::Rig

use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::Arc;
use std::time::Duration;

use pyo_proto::sandbox::ExecRequest;
use pyo_proto::sandbox::wire::Cmd;
use serde_json::json;
use sqlx::PgPool;

use super::outbound::e2e::{Rig, channel_here};
use super::outbound::tests::{Local, code_for, state, team};
use super::{daemon::Daemon, keys};
use crate::AppState;
use crate::teamsec_tests::run_big;

pub(crate) fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap()
}

pub(crate) fn enabled() -> bool {
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

pub(crate) fn input(var: &str, default: PathBuf) -> PathBuf {
    std::env::var_os(var).map(PathBuf::from).unwrap_or(default)
}

/// The daemon process and this run's files, gone when the test ends.
pub(crate) struct Run {
    pub(crate) dir: PathBuf,
    pub(crate) daemon: Option<Child>,
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
pub(crate) fn busybox_rootfs(dir: &Path, envd: &Path, out: &Path) {
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

pub(crate) fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

pub(crate) fn exec_req(cmd: &str) -> ExecRequest {
    ExecRequest {
        cmd: Cmd::Shell(cmd.into()),
        cwd: None,
        env: Default::default(),
        stdin: None,
        timeout_ms: Some(10_000),
    }
}

/// A runner round, then the server's report, asked over its channel and
/// kept as the reports it posts every 15 seconds are (outbound/channel.rs):
/// the test needn't wait for the next one.
async fn round(s: &AppState, team: &str, server: &str) {
    super::runner::round(s).await.unwrap();
    let r = Daemon::new(server).report().await.unwrap();
    let state: String = sqlx::query_scalar("SELECT state FROM pyo.sandbox_servers WHERE id = $1")
        .bind(server)
        .fetch_one(&s.db)
        .await
        .unwrap();
    let srv = super::runner::Server {
        id: server.to_owned(),
        state,
    };
    let mut tx = s.db.begin().await.unwrap();
    super::runner::apply_in(&mut tx, &srv, &r).await.unwrap();
    tx.commit().await.unwrap();
    let key = keys::signing_key(&s.db, &s.sealer).await.unwrap();
    super::templates::sync_server(&s.db, &key, server, Some(team), &r)
        .await
        .unwrap();
}

/// The whole control plane, over the channel, with a throwaway Postgres.
#[test]
fn the_control_plane_drives_a_real_daemon() {
    run_big(async {
        if !enabled() {
            return;
        }
        if std::env::var("DATABASE_URL").is_err() {
            eprintln!("skipping: set DATABASE_URL (a throwaway Postgres) for the whole control plane");
            return;
        }
        let mut s = state().await;
        keys::reset_cache();
        super::daemon::init(s.clone());
        tokio::spawn(crate::bg::listen(s.clone()));
        let signer = Arc::new(Local::new());
        let t = team(&s.db, true).await;
        channel_here(&mut s, signer.clone()).await;
        let db = s.db.clone();
        let (team, user) = (t.id.clone(), t.admin.clone());
        let template = format!("stp_e2e{}", crate::ids::random_string(8).to_lowercase());
        sqlx::query("INSERT INTO pyo.sandbox_templates (id, team_id, name, source, state, created_by) VALUES ($1, $2, 'busybox', '{\"kind\":\"image\",\"image\":\"busybox\"}', 'ready', 'test')")
            .bind(&template)
            .bind(&team)
            .execute(&db)
            .await
            .unwrap();
        let mut rig = Rig::new("api", &t, &template);
        let j = code_for(&s, &signer, &t, "e2e").await;
        let server = j.server_id.clone();

        // Not joined yet: nowhere to go.
        let req = |ttl: &str| -> super::CreateSandbox {
            serde_json::from_value(json!({ "teamId": team, "template": "busybox", "memoryMib": 256, "diskMib": 256, "network": "none", "ttl": ttl, "env": { "GREETING": "hi" } })).unwrap()
        };
        let e = super::create(&s, &user, req("5m")).await.unwrap_err();
        assert!(
            e.message.contains("No sandbox server is up"),
            "{}",
            e.message
        );

        // It joins and reports over the channel: ready.
        rig.join(&s, &j).await;
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
        assert_eq!(conn["url"], super::outbound::tailnet_url(&rig.name));
        let token = conn["token"].as_str().unwrap();
        let r = rig.exec(token, &id, "echo $GREETING from $(hostname)").await;
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
        assert_eq!(rig.exec(&t2.token, &id, "true").await.exit_code, Some(0));
        tokio::time::sleep(Duration::from_millis(1100)).await;
        round(&s, &team, &server).await;
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
            round(&s, &team, &server).await;
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
        let r = rig
            .exec(
                out["connection"]["token"].as_str().unwrap(),
                &id3,
                "cat /etc/os-release | head -1; ls -l /sbin/pyo-envd",
            )
            .await;
        println!("e2e template exec: {}", r.stdout);
        assert_eq!(r.exit_code, Some(0), "{r:?}");
        let e = super::delete_template(&s, &user, &tid).await.unwrap_err();
        assert_eq!(e.status, axum::http::StatusCode::CONFLICT, "{}", e.message);
        super::delete(&s, &user, &id3).await.unwrap();
        super::delete_template(&s, &user, &tid).await.unwrap();
        // The server drops its copy: now, or at the next round.
        let t0 = std::time::Instant::now();
        loop {
            round(&s, &team, &server).await;
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
            round(&s, &team, &server).await;
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
        let builds: Vec<serde_json::Value> = Daemon::new(&server)
            .report()
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
        let r = rig
            .exec(first["connection"]["token"].as_str().unwrap(), &first_id, "cat /etc/alpine-release")
            .await;
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
        let r = rig
            .exec(second["connection"]["token"].as_str().unwrap(), &second_id, "cat /etc/alpine-release")
            .await;
        assert!(r.stdout.starts_with("3.22"), "{r:?}");
        // The running sandbox keeps the old disk, which goes once it ends.
        round(&s, &team, &server).await;
        assert!(ready_on(&db, &team, &server).await.contains(&old_name));
        let r = rig
            .exec(first["connection"]["token"].as_str().unwrap(), &first_id, "cat /etc/alpine-release")
            .await;
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

        drop(rig);
        sqlx::query("DELETE FROM pyo.teams WHERE id = $1")
            .bind(&team)
            .execute(&db)
            .await
            .unwrap();
        for u in [&t.admin.id, &t.developer.id] {
            sqlx::query("DELETE FROM pyo.users WHERE id = $1")
                .bind(u)
                .execute(&db)
                .await
                .unwrap();
        }
        keys::reset_cache();
    })
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
        round(s, team, server).await;
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
