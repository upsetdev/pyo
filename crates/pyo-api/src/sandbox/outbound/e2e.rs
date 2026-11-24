//! A real `pyo-sandbox` daemon in its outbound mode against Pyo's channel,
//! booting real Firecracker microVMs on this machine. Pyo Cloud's path from
//! end to end, with a stand-in signer (tests.rs `Local`), a stand-in
//! holder agent for the Tailscale key and a stand-in tailnet: the
//! server's tailnet address and names in the join file, as sandbox.sh
//! writes them, and a certificate of its own for that name. Runs only with `PYO_SANDBOX_E2E=1`, a
//! `/dev/kvm` this user can open and `DATABASE_URL` (a throwaway
//! Postgres), so plain `cargo test` skips it. See sandbox/e2e.rs for the
//! files it needs:
//!
//! ```text
//! PYO_SANDBOX_E2E=1 DATABASE_URL=… cargo test -p pyo-api sandbox::outbound::e2e -- --nocapture
//! ```
//!
//! The daemon joins with the code from `createSandboxJoinCode`, reports
//! over the channel and turns ready. Pyo then places a sandbox on it,
//! execs and moves files in it through the channel, and deletes it. The
//! daemon answers HTTPS on its tailnet name with a sandbox token. Reset
//! access stops it.
//!
//! A self-hosted Pyo's move: a daemon that joined at the door, with Pyo
//! calling it, runs its **Update** command and works over the channel from
//! then on, signed by Pyo's own key, its sandbox still running.

use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use pyo_proto::sandbox::outbound as wire;
use serde_json::json;

use super::tests::{Local, code_for, holder_agent, state, team};
use super::{Mode, channel, reset_access};
use crate::sandbox::{self as sbx, Need, daemon::Daemon, e2e as base, keys};
use crate::CommandSigner as _;
use crate::teamsec_tests::run_big;

async fn server_state(db: &sqlx::PgPool, id: &str) -> (String, Option<String>) {
    sqlx::query_as("SELECT state, error FROM pyo.sandbox_servers WHERE id = $1")
        .bind(id)
        .fetch_one(db)
        .await
        .unwrap()
}

fn log_of(dir: &std::path::Path) -> String {
    std::fs::read_to_string(dir.join("daemon.log")).unwrap_or_default()
}

#[test]
fn a_daemon_works_for_pyo_over_https() {
    run_big(async {
        if !base::enabled() {
            return;
        }
        if std::env::var("DATABASE_URL").is_err() {
            eprintln!("skipping: set DATABASE_URL (a throwaway Postgres)");
            return;
        }
        let mut s = state().await;
        keys::reset_cache();
        crate::sandbox::daemon::init(s.clone());
        // Wake-ups reach the long poll, as in Pyo.
        tokio::spawn(crate::bg::listen(s.clone()));
        let signer = Arc::new(Local::new());
        let t = team(&s.db, true).await;

        // Pyo's channel on this machine, where the daemon reaches it.
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let pyo_url = format!("http://{}", l.local_addr().unwrap());
        let mut config = (*s.config).clone();
        config.agent_api_url = pyo_url.clone();
        s.config = Arc::new(config);
        let app = Router::new()
            .nest(wire::API_PREFIX, channel::routes(s.clone(), Some(signer.clone())))
            .with_state(s.clone());
        tokio::spawn(async move { axum::serve(l, app).await });

        // The daemon's files, gone when the test ends.
        let root = base::repo_root();
        let cache = root.join(".cache/sandbox-e2e");
        let target = root.join(
            std::env::var_os("CARGO_TARGET_DIR")
                .map(std::path::PathBuf::from)
                .unwrap_or("target".into()),
        );
        let daemon = base::input("PYO_SANDBOX_DAEMON", target.join("debug/pyo-sandbox"));
        let fc = base::input("PYO_SANDBOX_FIRECRACKER", cache.join("bin/firecracker"));
        let kernel = base::input("PYO_SANDBOX_KERNEL", cache.join("vmlinux"));
        let envd = base::input(
            "PYO_SANDBOX_ENVD",
            target.join("x86_64-unknown-linux-musl/release/pyo-envd"),
        );
        for p in [&daemon, &fc, &kernel, &envd] {
            assert!(p.exists(), "{} is missing. See sandbox/e2e.rs.", p.display());
        }
        let dir = cache.join(format!("outbound-run-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut run = base::Run {
            dir: dir.clone(),
            daemon: None,
        };
        let pool = dir.join("pool");
        let template = format!("stp_out{}", crate::ids::random_string(8).to_lowercase());
        base::busybox_rootfs(
            &dir,
            &envd,
            &pool.join("templates").join(&template).join("rootfs.ext4"),
        );
        sqlx::query("INSERT INTO pyo.sandbox_templates (id, team_id, name, source, state, created_by) VALUES ($1, $2, 'busybox', '{\"kind\":\"image\",\"image\":\"busybox\"}', 'ready', 'test')")
            .bind(&template)
            .bind(&t.id)
            .execute(&s.db)
            .await
            .unwrap();

        // The stand-in tailnet: the server's name on it, and a certificate for
        // that name.
        let name = format!("sbx-1.{}", t.suffix);
        let tls_key = rcgen::KeyPair::generate().unwrap();
        let tls_cert = rcgen::CertificateParams::new(vec![name.clone()])
            .unwrap()
            .self_signed(&tls_key)
            .unwrap();
        std::fs::create_dir_all(dir.join("tls")).unwrap();
        std::fs::write(dir.join("tls/cert.pem"), tls_cert.pem()).unwrap();
        std::fs::write(dir.join("tls/key.pem"), tls_key.serialize_pem()).unwrap();

        // The join command's parts, as sandbox.sh hands them over.
        let j = code_for(&s, &signer, &t, "sbx-1").await;
        assert_eq!(j.pyo_url, pyo_url);
        let (port, https_port) = (base::free_port(), base::free_port());
        std::fs::write(
            dir.join("join.json"),
            json!({
                "pyoUrl": j.pyo_url, "serverId": j.server_id, "code": j.code, "commandKeys": j.command_keys,
                "address": "100.64.0.20", "hostname": "sbx-1", "tailnetName": name, "jailUid": 0, "jailGid": 0,
            })
            .to_string(),
        )
        .unwrap();
        std::fs::write(
            dir.join("local.json"),
            json!({
                "listen": format!("127.0.0.1:{port}"), "root": pool, "firecracker": fc, "kernel": kernel,
                "envd": envd, "jailer": null, "hostReserveMib": 1024,
                "tailnetHttps": { "listen": format!("127.0.0.1:{https_port}"), "cert": dir.join("tls/cert.pem"), "key": dir.join("tls/key.pem") },
            })
            .to_string(),
        )
        .unwrap();
        let config_path = dir.join("config.json");
        let out = Command::new(&daemon)
            .args(["join", "--config"])
            .arg(&config_path)
            .arg("--join")
            .arg(dir.join("join.json"))
            .env("RUST_LOG", "off")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "join: {}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(String::from_utf8_lossy(&out.stdout).contains(&format!("Joined Pyo as {}.", j.server_id)));
        assert!(!dir.join("join.json").exists(), "the code is gone with it");

        let log = std::fs::File::create(dir.join("daemon.log")).unwrap();
        run.daemon = Some(
            Command::new(&daemon)
                .args(["serve", "--dev", "--config"])
                .arg(&config_path)
                .env("RUST_LOG", "info")
                // The daemon logs to stdout.
                .stdout(log.try_clone().unwrap())
                .stderr(log)
                .spawn()
                .unwrap(),
        );

        // Its first report makes it ready.
        let server = j.server_id.clone();
        let t0 = std::time::Instant::now();
        loop {
            let (st, err) = server_state(&s.db, &server).await;
            if st == "ready" {
                break;
            }
            assert!(
                t0.elapsed() < Duration::from_secs(60),
                "{st} {err:?}\n{}",
                log_of(&dir)
            );
            tokio::time::sleep(Duration::from_millis(250)).await;
        }

        // Pyo places a sandbox on it, through the channel.
        let user = t.admin.clone();
        let req: sbx::CreateSandbox = serde_json::from_value(json!({
            "teamId": t.id, "template": "busybox", "memoryMib": 256, "diskMib": 256,
            "network": "none", "ttl": "5m", "env": { "GREETING": "hi" }
        }))
        .unwrap();
        let made = sbx::create(&s, &user, req).await;
        let made = made.unwrap_or_else(|e| panic!("{}\n{}", e.message, log_of(&dir)));
        let id = made["sandbox"]["id"].as_str().unwrap().to_owned();
        assert_eq!(made["sandbox"]["state"], "running", "{made}");
        assert_eq!(made["sandbox"]["serverId"], json!(server), "{made}");
        let conn = &made["connection"];
        assert_eq!(conn["url"], format!("https://{name}:{}", wire::TAILNET_PORT));
        assert_eq!(conn["webUrl"], conn["url"]);
        assert_eq!(made["sandbox"]["url"], conn["url"]);
        let token = conn["token"].as_str().unwrap().to_owned();

        // Exec and files from Pyo, through the channel.
        let f = sbx::find(&s, &user, &id, Need::Use).await.unwrap();
        let (d, client_token) = f.client(&s).await.unwrap();
        let r = d
            .exec(&client_token, &id, &base::exec_req("echo $GREETING from $(hostname); echo err >&2; exit 3"))
            .await
            .unwrap();
        assert_eq!(
            (r.exit_code, r.stdout.as_str(), r.stderr.as_str()),
            (Some(3), "hi from sandbox\n", "err\n")
        );
        // Output past the channel's cap is cut.
        let r = d
            .exec(&client_token, &id, &base::exec_req("yes a | head -c 1100000"))
            .await
            .unwrap();
        assert!(r.truncated && r.stdout.len() == wire::OUTPUT_CAP, "{} {}", r.truncated, r.stdout.len());
        let n = d
            .write_file(&client_token, &id, "/tmp/a.txt", b"over the channel".to_vec())
            .await
            .unwrap();
        assert_eq!(n, 16);
        assert_eq!(
            d.read_file(&client_token, &id, "/tmp/a.txt").await.unwrap(),
            b"over the channel"
        );
        let e = d
            .write_file(&client_token, &id, "/tmp/big", vec![0; wire::FILE_CAP + 1])
            .await
            .unwrap_err();
        assert!(e.message.contains("tailnet"), "{e}");

        // Devices on the tailnet: HTTPS on its tailnet name, with the token.
        let addr: std::net::SocketAddr = format!("127.0.0.1:{https_port}").parse().unwrap();
        let https = reqwest::Client::builder()
            .add_root_certificate(reqwest::Certificate::from_pem(tls_cert.pem().as_bytes()).unwrap())
            .resolve(&name, addr)
            .no_proxy()
            .build()
            .unwrap();
        let base_url = format!("https://{name}:{https_port}");
        let health = https.get(format!("{base_url}/v1/health")).send().await.unwrap();
        assert!(health.status().is_success());
        let r: serde_json::Value = https
            .post(format!("{base_url}/v1/sandboxes/{id}/exec"))
            .bearer_auth(&token)
            .json(&base::exec_req("cat /tmp/a.txt"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(r["stdout"], "over the channel", "{r}");
        let no = https
            .post(format!("{base_url}/v1/sandboxes/{id}/exec"))
            .json(&base::exec_req("true"))
            .send()
            .await
            .unwrap();
        assert_eq!(no.status(), reqwest::StatusCode::UNAUTHORIZED);
        // A page served over HTTPS may call it (CORS and Private Network Access).
        let pre = https
            .request(reqwest::Method::OPTIONS, format!("{base_url}/v1/sandboxes/{id}/exec"))
            .header("origin", "https://app.example.com")
            .header("access-control-request-method", "POST")
            .header("access-control-request-private-network", "true")
            .send()
            .await
            .unwrap();
        assert!(pre.status().is_success());
        assert_eq!(pre.headers()["access-control-allow-private-network"], "true");

        // Deleted through the channel.
        let v = sbx::delete(&s, &user, &id).await.unwrap();
        assert_eq!(v["state"], "stopped", "{v}");
        let report = Daemon::new(&server, "100.64.0.20").report("").await.unwrap();
        assert!(
            report.sandboxes.iter().all(|x| x.id != id || x.state.is_final()),
            "{:?}",
            report.sandboxes
        );

        // Reset access: Pyo no longer takes its key, and it stops calling.
        let agent = holder_agent(s.db.clone(), t.holder.clone());
        let again = reset_access(&s, signer.as_ref(), &t.admin, &server, &Mode::Hosted).await;
        agent.abort();
        again.unwrap();
        let t0 = std::time::Instant::now();
        while !log_of(&dir).contains("Pyo no longer takes this server's key") {
            assert!(t0.elapsed() < Duration::from_secs(60), "{}", log_of(&dir));
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        // It still serves the tailnet.
        assert!(https.get(format!("{base_url}/v1/health")).send().await.unwrap().status().is_success());
        println!("outbound e2e: {}", log_of(&dir).lines().count());

        drop(run);
        sqlx::query("DELETE FROM pyo.teams WHERE id = $1")
            .bind(&t.id)
            .execute(&s.db)
            .await
            .unwrap();
        for u in [&t.admin.id, &t.developer.id] {
            sqlx::query("DELETE FROM pyo.users WHERE id = $1")
                .bind(u)
                .execute(&s.db)
                .await
                .unwrap();
        }
        keys::reset_cache();
    })
}

/// A self-hosted Pyo (docs/cloud-sandbox-servers.md "Self-hosted"): a
/// server that joined at the door before works as it did, with Pyo
/// calling it, and shows that it needs an update. Its **Update** command
/// moves it to the channel on Pyo's tailnet address, with commands signed
/// by Pyo's own key. Its sandbox keeps running through the move. Reset
/// access stops it.
#[test]
fn a_self_hosted_server_moves_from_the_door_to_the_channel() {
    run_big(async {
        if !base::enabled() {
            return;
        }
        if std::env::var("DATABASE_URL").is_err() {
            eprintln!("skipping: set DATABASE_URL (a throwaway Postgres)");
            return;
        }
        let s = state().await;
        keys::reset_cache();
        crate::sandbox::daemon::init(s.clone());
        tokio::spawn(crate::bg::listen(s.clone()));
        let signer = Arc::new(super::signer::Local::load(&s.db, &s.sealer).await.unwrap());
        let t = team(&s.db, true).await;

        // Pyo's channel on this machine: its address on the tailnet here.
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let pyo_url = format!("http://{}", l.local_addr().unwrap());
        let app = Router::new()
            .nest(wire::API_PREFIX, channel::routes(s.clone(), Some(signer.clone())))
            .with_state(s.clone());
        tokio::spawn(async move { axum::serve(l, app).await });

        let root = base::repo_root();
        let cache = root.join(".cache/sandbox-e2e");
        let target = root.join(
            std::env::var_os("CARGO_TARGET_DIR")
                .map(std::path::PathBuf::from)
                .unwrap_or("target".into()),
        );
        let daemon = base::input("PYO_SANDBOX_DAEMON", target.join("debug/pyo-sandbox"));
        let fc = base::input("PYO_SANDBOX_FIRECRACKER", cache.join("bin/firecracker"));
        let kernel = base::input("PYO_SANDBOX_KERNEL", cache.join("vmlinux"));
        let envd = base::input(
            "PYO_SANDBOX_ENVD",
            target.join("x86_64-unknown-linux-musl/release/pyo-envd"),
        );
        for p in [&daemon, &fc, &kernel, &envd] {
            assert!(p.exists(), "{} is missing. See sandbox/e2e.rs.", p.display());
        }
        let dir = cache.join(format!("selfhosted-run-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut run = base::Run {
            dir: dir.clone(),
            daemon: None,
        };
        let pool = dir.join("pool");
        let template = format!("stp_sh{}", crate::ids::random_string(8).to_lowercase());
        base::busybox_rootfs(
            &dir,
            &envd,
            &pool.join("templates").join(&template).join("rootfs.ext4"),
        );
        sqlx::query("INSERT INTO pyo.sandbox_templates (id, team_id, name, source, state, created_by) VALUES ($1, $2, 'busybox', '{\"kind\":\"image\",\"image\":\"busybox\"}', 'ready', 'test')")
            .bind(&template)
            .bind(&t.id)
            .execute(&s.db)
            .await
            .unwrap();
        let name = format!("sbx-1.{}", t.suffix);
        let tls_key = rcgen::KeyPair::generate().unwrap();
        let tls_cert = rcgen::CertificateParams::new(vec![name.clone()])
            .unwrap()
            .self_signed(&tls_key)
            .unwrap();
        std::fs::create_dir_all(dir.join("tls")).unwrap();
        std::fs::write(dir.join("tls/cert.pem"), tls_cert.pem()).unwrap();
        std::fs::write(dir.join("tls/key.pem"), tls_key.serialize_pem()).unwrap();
        let (port, https_port) = (base::free_port(), base::free_port());
        let address = format!("127.0.0.1:{port}");
        // This machine's own settings, kept through the move (local.json).
        std::fs::write(
            dir.join("local.json"),
            json!({
                "listen": address, "root": pool, "firecracker": fc, "kernel": kernel,
                "envd": envd, "jailer": null, "hostReserveMib": 1024,
                "tailnetHttps": { "listen": format!("127.0.0.1:{https_port}"), "cert": dir.join("tls/cert.pem"), "key": dir.join("tls/key.pem") },
            })
            .to_string(),
        )
        .unwrap();

        // A server that joined at the door before: the config the door gave
        // it, and Pyo calling it.
        let server = crate::ids::new_id("sbs");
        let token_keys = keys::verifying_keys(&s.db, &s.sealer).await.unwrap();
        let config_path = dir.join("config.json");
        std::fs::write(
            &config_path,
            crate::sandbox::join::daemon_config(&server, "127.0.0.1", 0, 0, &token_keys).to_string(),
        )
        .unwrap();
        let serve = |log: &str| {
            let log = std::fs::File::create(dir.join(log)).unwrap();
            Command::new(&daemon)
                .args(["serve", "--dev", "--config"])
                .arg(&config_path)
                .env("RUST_LOG", "info")
                .stdout(log.try_clone().unwrap())
                .stderr(log)
                .spawn()
                .unwrap()
        };
        run.daemon = Some(serve("direct.log"));
        let health = format!("http://{address}/v1/health");
        let t0 = std::time::Instant::now();
        while !reqwest::get(&health).await.is_ok_and(|r| r.status().is_success()) {
            assert!(t0.elapsed() < Duration::from_secs(30), "the daemon didn't come up");
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        sqlx::query(
            "INSERT INTO pyo.sandbox_servers (id, team_id, name, address, state, created_by, joined_at, via_cluster)
             VALUES ($1, $2, 'sbx-old', $3, 'joining', 'test', now() - interval '1 hour', $4)",
        )
        .bind(&server)
        .bind(&t.id)
        .bind(&address)
        .bind(&t.holder)
        .execute(&s.db)
        .await
        .unwrap();
        crate::sandbox::runner::round(&s).await.unwrap();
        let view = || {
            let (db, team, id) = (s.db.clone(), t.id.clone(), server.clone());
            async move {
                let mut c = db.acquire().await.unwrap();
                sbx::server_views(&mut c, &[team], Some(&id)).await.unwrap().remove(0)
            }
        };
        let v = view().await;
        assert_eq!(v["state"], "ready", "{v}");
        // The dashboard asks for an update.
        assert_eq!((v["outbound"].clone(), v["needsUpdate"].clone()), (json!(false), json!(true)), "{v}");

        // It still works as before: Pyo places a sandbox on it and calls it.
        let user = t.admin.clone();
        let req: sbx::CreateSandbox = serde_json::from_value(json!({
            "teamId": t.id, "template": "busybox", "memoryMib": 256, "diskMib": 256,
            "network": "none", "ttl": "10m", "env": { "GREETING": "hi" }
        }))
        .unwrap();
        let made = sbx::create(&s, &user, req).await;
        let made = made.unwrap_or_else(|e| panic!("{}\n{}", e.message, log_of_file(&dir, "direct.log")));
        let id = made["sandbox"]["id"].as_str().unwrap().to_owned();
        assert_eq!(made["sandbox"]["serverId"], json!(server), "{made}");
        assert_eq!(made["connection"]["url"], format!("http://{address}"), "straight to it");
        let f = sbx::find(&s, &user, &id, Need::Use).await.unwrap();
        let (d, client_token) = f.client(&s).await.unwrap();
        let r = d
            .exec(&client_token, &id, &base::exec_req("echo $GREETING > /tmp/kept; cat /tmp/kept"))
            .await
            .unwrap();
        assert_eq!(r.stdout, "hi\n");

        // Update: a command for the same server, on Pyo's tailnet address,
        // with Pyo's own command key.
        let at = super::Where {
            pyo_name: Some(format!("pyo.{}", t.suffix)),
            tailnet_api: Some(pyo_url.clone()),
            public: None,
        };
        let agent = holder_agent(s.db.clone(), t.holder.clone());
        let j = reset_access(&s, signer.as_ref(), &t.admin, &server, &Mode::SelfHosted(at.clone())).await;
        agent.abort();
        let j = j.unwrap();
        assert_eq!((j.server_id.as_str(), j.pyo_url.as_str()), (server.as_str(), pyo_url.as_str()));
        assert_eq!(j.command_keys, signer.public_keys());

        // sandbox.sh runs it: the daemon joins, then restarts on its new
        // config. Its sandbox keeps running.
        std::fs::write(
            dir.join("join.json"),
            json!({
                "pyoUrl": j.pyo_url, "serverId": j.server_id, "code": j.code, "commandKeys": j.command_keys,
                "address": "100.64.0.20", "hostname": "sbx-1", "tailnetName": name, "jailUid": 0, "jailGid": 0,
            })
            .to_string(),
        )
        .unwrap();
        let out = Command::new(&daemon)
            .args(["join", "--config"])
            .arg(&config_path)
            .arg("--join")
            .arg(dir.join("join.json"))
            .env("RUST_LOG", "off")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "join: {}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        if let Some(mut old) = run.daemon.take() {
            let _ = old.kill();
            let _ = old.wait();
        }
        let joined = pyo_proto::now_ms();
        run.daemon = Some(serve("daemon.log"));
        let t0 = std::time::Instant::now();
        loop {
            let v = view().await;
            if v["state"] == "ready" && v["lastReportAt"].as_i64().unwrap_or(0) > joined {
                assert_eq!((v["outbound"].clone(), v["needsUpdate"].clone()), (json!(true), json!(false)), "{v}");
                break;
            }
            assert!(t0.elapsed() < Duration::from_secs(60), "{v}\n{}", log_of(&dir));
            tokio::time::sleep(Duration::from_millis(250)).await;
        }

        // Pyo reaches it through the channel now, and its sandbox is still
        // there.
        let f = sbx::find(&s, &user, &id, Need::Use).await.unwrap();
        let (d, client_token) = f.client(&s).await.unwrap();
        let r = d
            .exec(&client_token, &id, &base::exec_req("cat /tmp/kept"))
            .await
            .unwrap_or_else(|e| panic!("{e}\n{}", log_of(&dir)));
        assert_eq!(r.stdout, "hi\n");
        let signed: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM pyo.sandbox_commands WHERE server_id = $1 AND kind = 'exec' AND status = 'done'",
        )
        .bind(&server)
        .fetch_one(&s.db)
        .await
        .unwrap();
        assert!(signed >= 1, "the exec went as a signed command");
        assert_eq!(
            sbx::token(&s, &user, &id).await.unwrap().url,
            format!("https://{name}:{}", wire::TAILNET_PORT),
            "browsers reach it on its tailnet name"
        );

        // Reset access: Pyo no longer takes its key, and it stops calling.
        let agent = holder_agent(s.db.clone(), t.holder.clone());
        let again = reset_access(&s, signer.as_ref(), &t.admin, &server, &Mode::SelfHosted(at)).await;
        agent.abort();
        again.unwrap();
        let t0 = std::time::Instant::now();
        while !log_of(&dir).contains("Pyo no longer takes this server's key") {
            assert!(t0.elapsed() < Duration::from_secs(60), "{}", log_of(&dir));
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        println!("self-hosted e2e: {}", log_of(&dir).lines().count());

        drop(run);
        sqlx::query("DELETE FROM pyo.teams WHERE id = $1")
            .bind(&t.id)
            .execute(&s.db)
            .await
            .unwrap();
        for u in [&t.admin.id, &t.developer.id] {
            sqlx::query("DELETE FROM pyo.users WHERE id = $1")
                .bind(u)
                .execute(&s.db)
                .await
                .unwrap();
        }
        keys::reset_cache();
    })
}

fn log_of_file(dir: &std::path::Path, file: &str) -> String {
    std::fs::read_to_string(dir.join(file)).unwrap_or_default()
}
