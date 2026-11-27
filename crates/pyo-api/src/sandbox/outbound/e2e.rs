//! A real `pyo-sandbox` daemon against Pyo's channel, booting real
//! Firecracker microVMs on this machine, with a stand-in signer, a
//! stand-in holder agent for the Tailscale key and a stand-in tailnet: the
//! server's tailnet address and names in the join file, as sandbox.sh
//! writes them, and a certificate of its own for that name. Runs only with
//! `PYO_SANDBOX_E2E=1`, a `/dev/kvm` this user can open and `DATABASE_URL`
//! (a throwaway Postgres), so plain `cargo test` skips it. See
//! sandbox/e2e.rs for the files it needs:
//!
//! ```text
//! PYO_SANDBOX_E2E=1 DATABASE_URL=… cargo test -p pyo-api sandbox::outbound::e2e -- --nocapture
//! ```
//!
//! Pyo Cloud's path: the daemon joins with the code from
//! `createSandboxJoinCode`, reports over the channel and turns ready. Pyo
//! then places a sandbox on it, execs and moves files in it through the
//! channel, and deletes it. The daemon answers HTTPS on its tailnet name
//! with a sandbox token. Reset access stops it.
//!
//! A self-hosted Pyo's server that joined at the door before 0.52.0: its
//! daemon doesn't serve its old config, Pyo refuses commands for it with
//! the update notice, and its **Update** command moves it to the channel,
//! signed by Pyo's own key.
//!
//! [`Rig`] is the daemon on this machine, for sandbox/e2e.rs too.

use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use pyo_proto::sandbox::{ExecResult, outbound as wire};
use serde_json::json;

use super::tests::{Local, Team, code_for, holder_agent, state, team};
use super::{Joining, Mode, channel, reset_access};
use crate::sandbox::{self as sbx, Need, daemon::Daemon, e2e as base, keys};
use crate::teamsec_tests::run_big;
use crate::{AppState, CommandSigner};

/// Pyo's channel on this machine, where a daemon reaches it: its URL. The
/// state's agent URL becomes it, as Pyo Cloud's `PYO_AGENT_API_URL`.
pub(crate) async fn channel_here(s: &mut AppState, signer: Arc<dyn CommandSigner>) -> String {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let pyo_url = format!("http://{}", l.local_addr().unwrap());
    let mut config = (*s.config).clone();
    config.agent_api_url = pyo_url.clone();
    s.config = Arc::new(config);
    let app = Router::new()
        .nest(wire::API_PREFIX, channel::routes(s.clone(), Some(signer)))
        .with_state(s.clone());
    tokio::spawn(async move { axum::serve(l, app).await });
    pyo_url
}

/// A `pyo-sandbox` daemon on this machine, as `sbx-1` on team `t`'s
/// stand-in tailnet, with a busybox template in its pool. Its files go
/// when it's dropped.
pub(crate) struct Rig {
    pub run: base::Run,
    pub dir: PathBuf,
    pub daemon: PathBuf,
    /// Its tailnet name.
    pub name: String,
    pub https_port: u16,
    pub cert_pem: String,
}

impl Rig {
    /// Its files under `.cache/sandbox-e2e/<label>-run-<pid>`, the busybox
    /// template `template` in its pool, its certificate and local.json.
    pub(crate) fn new(label: &str, t: &Team, template: &str) -> Rig {
        let root = base::repo_root();
        let cache = root.join(".cache/sandbox-e2e");
        let target = root.join(
            std::env::var_os("CARGO_TARGET_DIR")
                .map(PathBuf::from)
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
        let dir = cache.join(format!("{label}-run-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let run = base::Run {
            dir: dir.clone(),
            daemon: None,
        };
        let pool = dir.join("pool");
        base::busybox_rootfs(
            &dir,
            &envd,
            &pool.join("templates").join(template).join("rootfs.ext4"),
        );
        // The stand-in tailnet: the server's name on it, and a certificate
        // for that name.
        let name = format!("sbx-1.{}", t.suffix);
        let tls_key = rcgen::KeyPair::generate().unwrap();
        let tls_cert = rcgen::CertificateParams::new(vec![name.clone()])
            .unwrap()
            .self_signed(&tls_key)
            .unwrap();
        std::fs::create_dir_all(dir.join("tls")).unwrap();
        std::fs::write(dir.join("tls/cert.pem"), tls_cert.pem()).unwrap();
        std::fs::write(dir.join("tls/key.pem"), tls_key.serialize_pem()).unwrap();
        let https_port = base::free_port();
        // This machine's own settings, kept through joins (local.json).
        std::fs::write(
            dir.join("local.json"),
            json!({
                "listen": format!("127.0.0.1:{}", base::free_port()), "root": pool, "firecracker": fc,
                "kernel": kernel, "envd": envd, "jailer": null, "hostReserveMib": 1024,
                "tailnetHttps": { "listen": format!("127.0.0.1:{https_port}"), "cert": dir.join("tls/cert.pem"), "key": dir.join("tls/key.pem") },
            })
            .to_string(),
        )
        .unwrap();
        Rig {
            run,
            dir,
            daemon,
            name,
            https_port,
            cert_pem: tls_cert.pem(),
        }
    }

    pub(crate) fn config(&self) -> PathBuf {
        self.dir.join("config.json")
    }

    pub(crate) fn log(&self) -> String {
        std::fs::read_to_string(self.dir.join("daemon.log")).unwrap_or_default()
    }

    /// `pyo-sandbox serve` on its config, logging to daemon.log.
    pub(crate) fn serve(&self) -> std::process::Child {
        let log = std::fs::File::create(self.dir.join("daemon.log")).unwrap();
        Command::new(&self.daemon)
            .args(["serve", "--dev", "--config"])
            .arg(self.config())
            .env("RUST_LOG", "info")
            // The daemon logs to stdout.
            .stdout(log.try_clone().unwrap())
            .stderr(log)
            .spawn()
            .unwrap()
    }

    /// Runs `j`'s command here, as sandbox.sh does: the join, then the
    /// daemon on its new config. Waits until Pyo has its first report over
    /// the channel.
    pub(crate) async fn join(&mut self, s: &AppState, j: &Joining) {
        std::fs::write(
            self.dir.join("join.json"),
            json!({
                "pyoUrl": j.pyo_url, "serverId": j.server_id, "code": j.code, "commandKeys": j.command_keys,
                "address": "100.64.0.20", "hostname": "sbx-1", "tailnetName": self.name, "jailUid": 0, "jailGid": 0,
            })
            .to_string(),
        )
        .unwrap();
        let out = Command::new(&self.daemon)
            .args(["join", "--config"])
            .arg(self.config())
            .arg("--join")
            .arg(self.dir.join("join.json"))
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
        assert!(!self.dir.join("join.json").exists(), "the code is gone with it");
        if let Some(mut old) = self.run.daemon.take() {
            let _ = old.kill();
            let _ = old.wait();
        }
        let joined = pyo_proto::now_ms();
        self.run.daemon = Some(self.serve());
        let t0 = std::time::Instant::now();
        loop {
            let (ready, error): (bool, Option<String>) = sqlx::query_as(
                "SELECT outbound AND state = 'ready' AND coalesce(pyo.ms(last_report_at), 0) > $2, error
                   FROM pyo.sandbox_servers WHERE id = $1",
            )
            .bind(&j.server_id)
            .bind(joined)
            .fetch_one(&s.db)
            .await
            .unwrap();
            if ready {
                return;
            }
            assert!(
                t0.elapsed() < Duration::from_secs(60),
                "{error:?}\n{}",
                self.log()
            );
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    }

    /// A client for its HTTPS on the tailnet name, and that address.
    pub(crate) fn https(&self) -> (reqwest::Client, String) {
        let addr: std::net::SocketAddr = format!("127.0.0.1:{}", self.https_port).parse().unwrap();
        let c = reqwest::Client::builder()
            .add_root_certificate(reqwest::Certificate::from_pem(self.cert_pem.as_bytes()).unwrap())
            .resolve(&self.name, addr)
            .no_proxy()
            .build()
            .unwrap();
        (c, format!("https://{}:{}", self.name, self.https_port))
    }

    /// An exec with a sandbox token, as a device on the tailnet makes it.
    pub(crate) async fn exec(&self, token: &str, id: &str, cmd: &str) -> ExecResult {
        let (c, url) = self.https();
        let r = c
            .post(format!("{url}/v1/sandboxes/{id}/exec"))
            .bearer_auth(token)
            .json(&base::exec_req(cmd))
            .send()
            .await
            .unwrap();
        let status = r.status();
        let body = r.text().await.unwrap();
        assert!(status.is_success(), "{status} {body}");
        serde_json::from_str(&body).unwrap()
    }
}

async fn cleanup(s: &AppState, t: &Team) {
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
}

async fn busybox_for(s: &AppState, t: &Team, prefix: &str) -> String {
    let template = format!("stp_{prefix}{}", crate::ids::random_string(8).to_lowercase());
    sqlx::query("INSERT INTO pyo.sandbox_templates (id, team_id, name, source, state, created_by) VALUES ($1, $2, 'busybox', '{\"kind\":\"image\",\"image\":\"busybox\"}', 'ready', 'test')")
        .bind(&template)
        .bind(&t.id)
        .execute(&s.db)
        .await
        .unwrap();
    template
}

fn enabled() -> bool {
    if !base::enabled() {
        return false;
    }
    if std::env::var("DATABASE_URL").is_err() {
        eprintln!("skipping: set DATABASE_URL (a throwaway Postgres)");
        return false;
    }
    true
}

#[test]
fn a_daemon_works_for_pyo_over_https() {
    run_big(async {
        if !enabled() {
            return;
        }
        let mut s = state().await;
        keys::reset_cache();
        crate::sandbox::daemon::init(s.clone());
        // Wake-ups reach the long poll, as in Pyo.
        tokio::spawn(crate::bg::listen(s.clone()));
        let signer = Arc::new(Local::new());
        let t = team(&s.db, true).await;
        let pyo_url = channel_here(&mut s, signer.clone()).await;
        let template = busybox_for(&s, &t, "out").await;
        let mut rig = Rig::new("outbound", &t, &template);

        // The join command's parts, as sandbox.sh hands them over.
        let j = code_for(&s, &signer, &t, "sbx-1").await;
        assert_eq!(j.pyo_url, pyo_url);
        rig.join(&s, &j).await;
        let server = j.server_id.clone();
        let name = rig.name.clone();

        // Pyo places a sandbox on it, through the channel.
        let user = t.admin.clone();
        let req: sbx::CreateSandbox = serde_json::from_value(json!({
            "teamId": t.id, "template": "busybox", "memoryMib": 256, "diskMib": 256,
            "network": "none", "ttl": "5m", "env": { "GREETING": "hi" }
        }))
        .unwrap();
        let made = sbx::create(&s, &user, req).await;
        let made = made.unwrap_or_else(|e| panic!("{}\n{}", e.message, rig.log()));
        let id = made["sandbox"]["id"].as_str().unwrap().to_owned();
        assert_eq!(made["sandbox"]["state"], "running", "{made}");
        assert_eq!(made["sandbox"]["serverId"], json!(server), "{made}");
        let conn = &made["connection"];
        assert_eq!(conn["url"], format!("https://{name}:{}", wire::TAILNET_PORT));
        assert_eq!(conn["webUrl"], conn["url"]);
        assert_eq!(made["sandbox"]["url"], conn["url"]);
        assert_eq!(made["sandbox"]["serverNeedsUpdate"], false);
        let token = conn["token"].as_str().unwrap().to_owned();

        // Exec and files from Pyo, through the channel.
        let f = sbx::find(&s, &user, &id, Need::Use).await.unwrap();
        let (d, _) = f.daemon().unwrap();
        let r = d
            .exec(&id, &base::exec_req("echo $GREETING from $(hostname); echo err >&2; exit 3"))
            .await
            .unwrap();
        assert_eq!(
            (r.exit_code, r.stdout.as_str(), r.stderr.as_str()),
            (Some(3), "hi from sandbox\n", "err\n")
        );
        // Output past the channel's cap is cut.
        let r = d.exec(&id, &base::exec_req("yes a | head -c 1100000")).await.unwrap();
        assert!(r.truncated && r.stdout.len() == wire::OUTPUT_CAP, "{} {}", r.truncated, r.stdout.len());
        let n = d.write_file(&id, "/tmp/a.txt", b"over the channel".to_vec()).await.unwrap();
        assert_eq!(n, 16);
        assert_eq!(d.read_file(&id, "/tmp/a.txt").await.unwrap(), b"over the channel");
        let e = d.write_file(&id, "/tmp/big", vec![0; wire::FILE_CAP + 1]).await.unwrap_err();
        assert!(e.message.contains("tailnet"), "{e}");

        // Devices on the tailnet: HTTPS on its tailnet name, with the token.
        let (https, base_url) = rig.https();
        let health = https.get(format!("{base_url}/v1/health")).send().await.unwrap();
        assert!(health.status().is_success());
        assert_eq!(rig.exec(&token, &id, "cat /tmp/a.txt").await.stdout, "over the channel");
        let no = https
            .post(format!("{base_url}/v1/sandboxes/{id}/exec"))
            .json(&base::exec_req("true"))
            .send()
            .await
            .unwrap();
        assert_eq!(no.status(), reqwest::StatusCode::UNAUTHORIZED);
        // Pyo's own calls never come here: no route for them.
        for (m, path) in [
            (reqwest::Method::GET, "/v1/report".to_owned()),
            (reqwest::Method::POST, "/v1/commands".to_owned()),
            (reqwest::Method::DELETE, format!("/v1/sandboxes/{id}")),
        ] {
            let r = https.request(m.clone(), format!("{base_url}{path}")).bearer_auth(&token).send().await.unwrap();
            assert!(
                matches!(r.status().as_u16(), 404 | 405),
                "{m} {path}: {}",
                r.status()
            );
        }
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
        // Nothing else listens: its old plain HTTP port is gone.
        let local: serde_json::Value =
            serde_json::from_slice(&std::fs::read(rig.dir.join("local.json")).unwrap()).unwrap();
        let old = format!("http://{}/v1/health", local["listen"].as_str().unwrap());
        assert!(reqwest::get(&old).await.is_err(), "{old} answered");

        // Deleted through the channel.
        let v = sbx::delete(&s, &user, &id).await.unwrap();
        assert_eq!(v["state"], "stopped", "{v}");
        let report = Daemon::new(&server).report().await.unwrap();
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
        while !rig.log().contains("Pyo no longer takes this server's key") {
            assert!(t0.elapsed() < Duration::from_secs(60), "{}", rig.log());
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        // It still serves the tailnet.
        assert!(https.get(format!("{base_url}/v1/health")).send().await.unwrap().status().is_success());
        println!("outbound e2e: {}", rig.log().lines().count());

        drop(rig);
        cleanup(&s, &t).await;
    })
}

/// A self-hosted Pyo (docs/cloud-sandbox-servers.md "Self-hosted"): a
/// server that joined at the door before 0.52.0. Its daemon doesn't serve
/// the door's config any more and says why. Pyo refuses commands for it
/// with the update notice. Its **Update** command moves it to the channel
/// on Pyo's tailnet address, with commands signed by Pyo's own key, and
/// it works from then on. Reset access stops it.
#[test]
fn a_self_hosted_server_from_the_door_moves_to_the_channel() {
    run_big(async {
        if !enabled() {
            return;
        }
        let mut s = state().await;
        keys::reset_cache();
        crate::sandbox::daemon::init(s.clone());
        tokio::spawn(crate::bg::listen(s.clone()));
        let signer = Arc::new(super::signer::Local::load(&s.db, &s.sealer).await.unwrap());
        let t = team(&s.db, true).await;
        // Pyo's channel on this machine: its address on the tailnet here.
        let pyo_url = channel_here(&mut s, signer.clone()).await;
        let template = busybox_for(&s, &t, "sh").await;
        let mut rig = Rig::new("selfhosted", &t, &template);

        // A server that joined at the door before: the config the door
        // gave it, and its row.
        let server = crate::ids::new_id("sbs");
        let token_keys = keys::verifying_keys(&s.db, &s.sealer).await.unwrap();
        std::fs::write(
            rig.config(),
            crate::sandbox::join::daemon_config(&server, "127.0.0.1", 0, 0, &token_keys).to_string(),
        )
        .unwrap();
        sqlx::query(
            "INSERT INTO pyo.sandbox_servers (id, team_id, name, address, state, created_by, joined_at, last_report_at)
             VALUES ($1, $2, 'sbx-old', '100.64.0.20', 'ready', 'test', now() - interval '1 day', now() - interval '1 hour')",
        )
        .bind(&server)
        .bind(&t.id)
        .execute(&s.db)
        .await
        .unwrap();

        // This release's daemon on the old config stops, and says why.
        let mut old = rig.serve();
        let t0 = std::time::Instant::now();
        let status = loop {
            if let Some(st) = old.try_wait().unwrap() {
                break st;
            }
            assert!(t0.elapsed() < Duration::from_secs(30), "it kept running: {}", rig.log());
            tokio::time::sleep(Duration::from_millis(100)).await;
        };
        assert!(!status.success());
        assert!(rig.log().contains("needs an update. Press Update on it in Pyo"), "{}", rig.log());

        // Pyo shows it needs an update, and takes no sandbox for it.
        let view = || {
            let (db, team, id) = (s.db.clone(), t.id.clone(), server.clone());
            async move {
                let mut c = db.acquire().await.unwrap();
                sbx::server_views(&mut c, &[team], Some(&id)).await.unwrap().remove(0)
            }
        };
        let v = view().await;
        assert_eq!((v["outbound"].clone(), v["needsUpdate"].clone()), (json!(false), json!(true)), "{v}");
        let user = t.admin.clone();
        let req: sbx::CreateSandbox = serde_json::from_value(json!({
            "teamId": t.id, "template": "busybox", "server": "sbx-old", "memoryMib": 256, "diskMib": 256,
            "network": "none", "ttl": "10m", "env": { "GREETING": "hi" }
        }))
        .unwrap();
        let e = sbx::create(&s, &user, req.clone()).await.unwrap_err();
        assert_eq!((e.status.as_u16(), e.message.as_str()), (409, sbx::NEEDS_UPDATE));
        let e = Daemon::new(&server).report().await.unwrap_err();
        assert_eq!(e.message, sbx::NEEDS_UPDATE);

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

        // sandbox.sh runs it: the daemon joins, then starts on its new config.
        rig.join(&s, &j).await;
        let v = view().await;
        assert_eq!((v["outbound"].clone(), v["needsUpdate"].clone()), (json!(true), json!(false)), "{v}");

        // Pyo places a sandbox on it and reaches it through the channel.
        let made = sbx::create(&s, &user, req).await;
        let made = made.unwrap_or_else(|e| panic!("{}\n{}", e.message, rig.log()));
        let id = made["sandbox"]["id"].as_str().unwrap().to_owned();
        assert_eq!(made["sandbox"]["serverId"], json!(server), "{made}");
        let f = sbx::find(&s, &user, &id, Need::Use).await.unwrap();
        let (d, _) = f.daemon().unwrap();
        let r = d
            .exec(&id, &base::exec_req("echo $GREETING > /tmp/kept; cat /tmp/kept"))
            .await
            .unwrap_or_else(|e| panic!("{e}\n{}", rig.log()));
        assert_eq!(r.stdout, "hi\n");
        let signed: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM pyo.sandbox_commands WHERE server_id = $1 AND kind = 'exec' AND status = 'done'",
        )
        .bind(&server)
        .fetch_one(&s.db)
        .await
        .unwrap();
        assert!(signed >= 1, "the exec went as a signed command");
        let conn = sbx::token(&s, &user, &id).await.unwrap();
        assert_eq!(
            conn.url,
            format!("https://{}:{}", rig.name, wire::TAILNET_PORT),
            "browsers reach it on its tailnet name"
        );
        assert_eq!(rig.exec(&conn.token, &id, "cat /tmp/kept").await.stdout, "hi\n");
        sbx::delete(&s, &user, &id).await.unwrap();

        // Reset access: Pyo no longer takes its key, and it stops calling.
        let agent = holder_agent(s.db.clone(), t.holder.clone());
        let again = reset_access(&s, signer.as_ref(), &t.admin, &server, &Mode::SelfHosted(at)).await;
        agent.abort();
        again.unwrap();
        let t0 = std::time::Instant::now();
        while !rig.log().contains("Pyo no longer takes this server's key") {
            assert!(t0.elapsed() < Duration::from_secs(60), "{}", rig.log());
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        println!("self-hosted e2e: {}", rig.log().lines().count());

        drop(rig);
        cleanup(&s, &t).await;
    })
}
