//! A self-hosted Pyo's sandbox servers on the outbound channel
//! (docs/cloud-sandbox-servers.md "Self-hosted"): Pyo's own signer, where
//! the daemon reaches Pyo, the installation's servers (no team), and
//! servers that joined at the door moving over. The unit tests run
//! anywhere. The rest run against a throwaway Postgres:
//! `DATABASE_URL=… cargo test -p pyo-api --lib sandbox::outbound::self_hosted_tests -- --ignored`.

use std::sync::Arc;

use axum::{Router, http::Method, http::StatusCode};
use pyo_proto::agentauth::RequestKey;
use pyo_proto::sandbox::{DaemonCommand, DaemonResult, outbound as wire};
use pyo_proto::signing::{Refused, TrustedKeys};
use serde_json::json;

use super::signer::{INSTALLATION_TEAM, Local};
use super::tests::{Key, Team, ask, call, found, holder_agent, public_key, report_of, state, team};
use super::*;
use crate::CommandSigner as _;
use crate::teamsec_tests::run_big;

// ── Without a database ─────────────────────────────────────────────────────

#[test]
fn a_server_reaches_pyo_on_its_tailnet_first() {
    let name = Some("pyo.tail1234.ts.net");
    let api = Some("https://pyo.tail1234.ts.net/");
    let public = Some("https://pyo.example.com/");
    // On Pyo's own tailnet: the tailnet address, even with a public one.
    assert_eq!(
        pick_pyo_url(Some("tail1234.ts.net"), name, api, public),
        Ok("https://pyo.tail1234.ts.net".into())
    );
    assert_eq!(
        pick_pyo_url(Some("TAIL1234.ts.net."), name, api, None),
        Ok("https://pyo.tail1234.ts.net".into())
    );
    // Before Pyo has its HTTPS name there: its tailnet IP.
    assert_eq!(
        pick_pyo_url(None, None, Some("http://100.64.0.1:8000"), None),
        Ok("http://100.64.0.1:8000".into())
    );
    // A team on a tailnet of its own: Pyo's public address.
    assert_eq!(
        pick_pyo_url(Some("tail9999.ts.net"), name, api, public),
        Ok("https://pyo.example.com".into())
    );
    // Without one, or with one that isn't HTTPS: nowhere.
    for p in [None, Some("http://pyo.example.com"), Some("https://")] {
        assert_eq!(pick_pyo_url(Some("tail9999.ts.net"), name, api, p), Err(NOT_REACHABLE), "{p:?}");
    }
    // Pyo isn't on its tailnet yet.
    assert_eq!(pick_pyo_url(Some("tail1234.ts.net"), name, None, None), Err(NOT_ON_TAILNET));
    for m in [NOT_REACHABLE, NOT_ON_TAILNET] {
        assert!(m.ends_with('.') && !m.contains(';') && !m.contains(" - "), "{m}");
    }
}

fn direct() -> Found {
    Found {
        outbound: Some(false),
        state: Some("ready".into()),
        pyo_url: Some("https://pyo.tail1234.ts.net".into()),
        ..found()
    }
}

#[test]
fn a_server_that_joined_at_the_door_moves_over_on_a_self_hosted_pyo_only() {
    let pk = public_key();
    let good = ask("sbs_one", "c", &pk, "sbx-1", "sbx-1.tail1234.ts.net");
    let gone = Err(Refusal::Door(join::Refusal::Gone));
    assert_eq!(check(&direct(), &good, false), Ok(()));
    assert_eq!(check(&Found { state: Some("unreachable".into()), ..direct() }, &good, false), Ok(()));
    // Never on a hosted Pyo, without its Update command, or refused.
    assert_eq!(check(&direct(), &good, true), gone);
    assert_eq!(check(&Found { pyo_url: None, ..direct() }, &good, false), gone);
    assert_eq!(check(&Found { state: Some("refused".into()), ..direct() }, &good, false), gone);
    // A new server still waits for its join.
    assert_eq!(check(&found(), &good, false), Ok(()));
    assert_eq!(check(&Found { state: Some("ready".into()), ..found() }, &good, false), gone);
}

#[test]
fn the_installations_servers_are_a_self_hosted_pyos_only() {
    let pk = public_key();
    let good = ask("sbs_one", "c", &pk, "sbx-1", "sbx-1.tail1234.ts.net");
    let installation = || Found { team_id: None, ..found() };
    assert_eq!(check(&installation(), &good, false), Ok(()));
    assert_eq!(check(&installation(), &good, true), Err(Refusal::Door(join::Refusal::Gone)));
    assert_eq!(signer::claims_team(None), INSTALLATION_TEAM);
    assert_eq!(signer::claims_team(Some("team_a")), "team_a");
    // Never a team's id.
    assert!(!INSTALLATION_TEAM.starts_with("team_"));
}

// ── Against a throwaway Postgres ───────────────────────────────────────────

/// Pyo on the team's tailnet, as Pyo sees itself there.
fn on_tailnet_of(t: &Team) -> Where {
    Where {
        pyo_name: Some(format!("pyo.{}", t.suffix)),
        tailnet_api: Some(format!("https://pyo.{}", t.suffix)),
        public: None,
    }
}

fn app(s: &AppState, signer: Arc<Local>) -> Router {
    Router::new()
        .nest(wire::API_PREFIX, channel::routes(s.clone(), Some(signer)))
        .with_state(s.clone())
}

fn new_key() -> RequestKey {
    RequestKey::from_pkcs8(&RequestKey::generate_pkcs8().unwrap()).unwrap()
}

fn join_ask(j: &Joining, t: &Team, key: &RequestKey) -> wire::JoinRequest {
    ask(&j.server_id, &j.code, &key.public_key(), "sbx-1", &format!("sbx-1.{}", t.suffix))
}

async fn cleanup(s: &AppState, t: &Team) {
    sqlx::query("DELETE FROM pyo.teams WHERE id = $1").bind(&t.id).execute(&s.db).await.unwrap();
    for u in [&t.admin.id, &t.developer.id] {
        sqlx::query("DELETE FROM pyo.users WHERE id = $1").bind(u).execute(&s.db).await.unwrap();
    }
}

/// Pyo's own command key: made once, sealed, never the token key, and
/// numbers that only grow per server, for an outbound server of that team
/// only.
#[test]
#[ignore = "needs DATABASE_URL (a throwaway Postgres)"]
fn pyo_signs_with_a_command_key_of_its_own() {
    run_big(async {
        let s = state().await;
        let a = Local::load(&s.db, &s.sealer).await.unwrap();
        let b = Local::load(&s.db, &s.sealer).await.unwrap();
        assert_eq!(a.public_keys(), b.public_keys(), "made once");
        let (sealed, public): (String, String) = sqlx::query_as(
            "SELECT pkcs8_sealed, public_key FROM pyo.sandbox_command_keys WHERE retired_at IS NULL ORDER BY created_at DESC LIMIT 1",
        )
        .fetch_one(&s.db)
        .await
        .unwrap();
        assert_eq!(public, a.public_keys());
        assert!(sealed.starts_with("sb1:"), "sealed with PYO_SECRET_KEY");
        let tokens = crate::sandbox::keys::verifying_keys(&s.db, &s.sealer).await.unwrap();
        assert!(tokens.iter().all(|k| k.public_key != a.public_keys()), "not the token key");
        // Agents get nothing from it.
        assert!(a.sign("cl_x", &[]).await.is_err());

        let t = team(&s.db, true).await;
        let id = crate::ids::new_id("sbs");
        sqlx::query("INSERT INTO pyo.sandbox_servers (id, team_id, name, state, created_by, outbound) VALUES ($1, $2, 'sbx-k', 'joining', 'test', true)")
            .bind(&id)
            .bind(&t.id)
            .execute(&s.db)
            .await
            .unwrap();
        let item = |n: &str| crate::ToSign { id: n.into(), kind: "list".into(), sha256: "00".into(), command: None };
        let first = a.sign_sandbox(&id, &t.id, &[item("sbc_1"), item("sbc_2")]).await.unwrap();
        let next = b.sign_sandbox(&id, &t.id, &[item("sbc_3")]).await.unwrap();
        let trusted = TrustedKeys::parse(&a.public_keys()).unwrap();
        let mut seqs = vec![];
        for (sig, n) in first.iter().chain(&next).zip(["sbc_1", "sbc_2", "sbc_3"]) {
            let signed = pyo_proto::SignedCommandWire {
                claims: sig.claims.clone(),
                sig: sig.sig.clone(),
                kid: sig.kid.clone(),
                command: String::new(),
                delegation: None,
            };
            // The hash is of the command's JSON: checked as far as it goes.
            let e = trusted.verify_sandbox(Some(&signed), n, &id, &t.id, 0, pyo_proto::now_ms() as u64);
            assert!(matches!(e, Err(Refused::Tampered)), "{e:?}");
            let raw = base64::Engine::decode(&base64::engine::general_purpose::URL_SAFE_NO_PAD, &sig.claims).unwrap();
            let c: pyo_proto::signing::SandboxClaims = serde_json::from_slice(&raw).unwrap();
            assert_eq!((c.server.as_str(), c.team.as_str(), c.id.as_str()), (id.as_str(), t.id.as_str(), n));
            seqs.push(c.seq);
        }
        assert_eq!(seqs, [1, 2, 3], "one count per server, across processes");
        // Another team's, or a server that isn't outbound: refused.
        assert!(a.sign_sandbox(&id, "team_other", &[item("sbc_4")]).await.is_err());
        assert!(a.sign_sandbox(&id, INSTALLATION_TEAM, &[item("sbc_4")]).await.is_err());
        sqlx::query("UPDATE pyo.sandbox_servers SET outbound = false WHERE id = $1").bind(&id).execute(&s.db).await.unwrap();
        assert!(a.sign_sandbox(&id, &t.id, &[item("sbc_4")]).await.is_err());
        cleanup(&s, &t).await;
    })
}

/// A team's new server on a self-hosted Pyo: its command names Pyo's
/// tailnet address and Pyo's own key, it joins over the tailnet, gets
/// commands signed for it and its team, and Reset access stops its key.
#[test]
#[ignore = "needs DATABASE_URL (a throwaway Postgres)"]
fn a_teams_server_joins_over_the_tailnet() {
    run_big(async {
        let s = state().await;
        let signer = Arc::new(Local::load(&s.db, &s.sealer).await.unwrap());
        let t = team(&s.db, true).await;
        let at = on_tailnet_of(&t);

        // Developers can't add one.
        let e = add(&s, signer.as_ref(), &t.developer, Some(&t.id), "sbx-a", None, &at).await.unwrap_err();
        assert_eq!(e.status, StatusCode::FORBIDDEN);
        // A team on another tailnet with no public address: refused, plainly.
        let elsewhere = Where { pyo_name: Some("pyo.tail0000.ts.net".into()), ..on_tailnet_of(&t) };
        let e = add(&s, signer.as_ref(), &t.admin, Some(&t.id), "sbx-a", None, &elsewhere).await.unwrap_err();
        assert_eq!((e.status, e.message.as_str()), (StatusCode::CONFLICT, NOT_REACHABLE));
        // With one, that's where it goes.
        let agent = holder_agent(s.db.clone(), t.holder.clone());
        let public = Where { public: Some("https://pyo.example.com".into()), ..elsewhere };
        let j = add(&s, signer.as_ref(), &t.admin, Some(&t.id), "sbx-p", None, &public).await;
        assert_eq!(j.unwrap().pyo_url, "https://pyo.example.com");

        let j = add(&s, signer.as_ref(), &t.admin, Some(&t.id), "sbx-a", None, &at).await;
        agent.abort();
        let j = j.unwrap();
        assert_eq!(j.pyo_url, format!("https://pyo.{}", t.suffix));
        assert_eq!(j.command_keys, signer.public_keys());
        let cmd = j.command("https://scripts.example.com", "0.51.1");
        assert!(cmd.contains(&format!(" --pyo-url 'https://pyo.{}' ", t.suffix)), "{cmd}");
        assert!(cmd.contains(&format!(" --command-key '{}' ", signer.public_keys())), "{cmd}");
        assert!(!cmd.contains(" --pyo '"), "{cmd}");
        let (outbound, via): (bool, Option<String>) =
            sqlx::query_as("SELECT outbound, via_cluster FROM pyo.sandbox_servers WHERE id = $1")
                .bind(&j.server_id)
                .fetch_one(&s.db)
                .await
                .unwrap();
        assert!(outbound);
        assert_eq!(via.as_deref(), Some(t.holder.as_str()));


        let k = new_key();
        let a = join(&s.db, &s.sealer, &signer.public_keys(), "https://unused.example.com", false, &join_ask(&j, &t, &k))
            .await
            .unwrap();
        assert_eq!(a.team_id, t.id);
        assert_eq!(a.config["outbound"]["pyoUrl"], json!(j.pyo_url));
        assert_eq!(a.config["outbound"]["teamId"], json!(t.id));
        let key = Key(a.key_id, k);

        // Commands signed by Pyo's own key, for this server and team.
        let app = app(&s, signer.clone());
        let mut c = s.db.acquire().await.unwrap();
        let one = channel::enqueue(&mut c, &s.sealer, &j.server_id, Some(&t.id), &DaemonCommand::List).await.unwrap();
        drop(c);
        let (st, v) = call(&app, Method::GET, "/commands?wait=0", Some(&key), b"").await;
        assert_eq!(st, StatusCode::OK, "{v}");
        let got: Vec<wire::CommandEnvelope> = serde_json::from_value(v).unwrap();
        assert_eq!(got.len(), 1);
        let trusted = TrustedKeys::parse(&j.command_keys).unwrap();
        let now = pyo_proto::now_ms() as u64;
        let claims = trusted.verify_sandbox(Some(&got[0].signed), &one, &j.server_id, &t.id, 0, now).unwrap();
        assert_eq!(claims.kind, "list");
        assert!(trusted.verify_sandbox(Some(&got[0].signed), &one, &j.server_id, INSTALLATION_TEAM, 0, now).is_err());
        assert!(trusted.verify_sandbox(Some(&got[0].signed), &one, "sbs_other", &t.id, 0, now).is_err());
        assert!(trusted.verify_sandbox(Some(&got[0].signed), &one, &j.server_id, &t.id, claims.seq, now).is_err(), "replayed");

        // Reset access: its key stops at once.
        let e = reset_access(&s, signer.as_ref(), &t.developer, &j.server_id, &Mode::SelfHosted(at.clone())).await.unwrap_err();
        assert_eq!(e.status, StatusCode::FORBIDDEN);
        let agent = holder_agent(s.db.clone(), t.holder.clone());
        let again = reset_access(&s, signer.as_ref(), &t.admin, &j.server_id, &Mode::SelfHosted(at.clone())).await;
        agent.abort();
        again.unwrap();
        let (st, v) = call(&app, Method::GET, "/commands?wait=0", Some(&key), b"").await;
        assert_eq!((st, v["error"].as_str()), (StatusCode::UNAUTHORIZED, Some(channel::KEY_GONE)));
        cleanup(&s, &t).await;
    })
}

/// The installation's server (no team): it joins with the team
/// `installation` in its claims, its key and commands have no team, its
/// commands are sealed with the installation's key, and it works the
/// channel like any other.
#[test]
#[ignore = "needs DATABASE_URL (a throwaway Postgres)"]
fn the_installations_server_works_the_channel() {
    run_big(async {
        let s = state().await;
        let signer = Arc::new(Local::load(&s.db, &s.sealer).await.unwrap());
        let t = team(&s.db, true).await;
        let id = crate::ids::new_id("sbs");
        // It joins with a key from that team's tailnet.
        sqlx::query(
            "INSERT INTO pyo.sandbox_servers (id, name, state, created_by, outbound, via_cluster, pyo_url)
             VALUES ($1, $1, 'joining', 'test', true, $2, 'https://pyo.example.ts.net')",
        )
        .bind(&id)
        .bind(&t.holder)
        .execute(&s.db)
        .await
        .unwrap();
        let code = |c: &'static str| {
            let (db, id) = (s.db.clone(), id.clone());
            async move {
                let mut tx = db.begin().await.unwrap();
                join::keep(&mut tx, &id, c).await.unwrap();
                tx.commit().await.unwrap();
            }
        };
        let ask_with = |c: &str, k: &RequestKey| ask(&id, c, &k.public_key(), "sbx-1", &format!("sbx-1.{}", t.suffix));

        // A hosted Pyo has none.
        code("c0de0001").await;
        let e = join(&s.db, &s.sealer, &signer.public_keys(), "https://x", true, &ask_with("c0de0001", &new_key()))
            .await
            .unwrap_err();
        assert_eq!(e.message, join::Refusal::Gone.message());

        code("c0de0002").await;
        let k = new_key();
        let a = join(&s.db, &s.sealer, &signer.public_keys(), "https://x", false, &ask_with("c0de0002", &k))
            .await
            .unwrap();
        assert_eq!(a.team_id, INSTALLATION_TEAM);
        assert_eq!(a.config["outbound"]["teamId"], INSTALLATION_TEAM);
        assert_eq!(a.config["outbound"]["pyoUrl"], "https://pyo.example.ts.net");
        let team_of_key: Option<String> = sqlx::query_scalar("SELECT team_id FROM pyo.sandbox_server_keys WHERE id = $1")
            .bind(&a.key_id)
            .fetch_one(&s.db)
            .await
            .unwrap();
        assert_eq!(team_of_key, None);
        let key = Key(a.key_id, k);

        let app = app(&s, signer.clone());
        // Its report.
        let (st, v) = call(&app, Method::POST, wire::REPORT_PATH, Some(&key), &report_of(&id)).await;
        assert_eq!(st, StatusCode::OK, "{v}");
        // Its commands: sealed with the installation's key, signed for it.
        let mut c = s.db.acquire().await.unwrap();
        let cmd = channel::enqueue(&mut c, &s.sealer, &id, None, &DaemonCommand::Delete { id: "sbx_i1".into() })
            .await
            .unwrap();
        // Not as a team's.
        assert!(channel::enqueue(&mut c, &s.sealer, &id, Some(&t.id), &DaemonCommand::List).await.is_err());
        drop(c);
        let (sealed, team_of_cmd): (Option<String>, Option<String>) =
            sqlx::query_as("SELECT command_sealed, team_id FROM pyo.sandbox_commands WHERE id = $1")
                .bind(&cmd)
                .fetch_one(&s.db)
                .await
                .unwrap();
        let sealed = sealed.unwrap();
        assert!(sealed.starts_with("sb1:") && !sealed.contains("sbx_i1"), "{sealed}");
        assert_eq!(team_of_cmd, None);
        let (st, v) = call(&app, Method::GET, "/commands?wait=0", Some(&key), b"").await;
        assert_eq!(st, StatusCode::OK, "{v}");
        let got: Vec<wire::CommandEnvelope> = serde_json::from_value(v).unwrap();
        assert_eq!(got.len(), 1);
        let trusted = TrustedKeys::parse(&signer.public_keys()).unwrap();
        let now = pyo_proto::now_ms() as u64;
        trusted.verify_sandbox(Some(&got[0].signed), &cmd, &id, INSTALLATION_TEAM, 0, now).unwrap();
        assert!(trusted.verify_sandbox(Some(&got[0].signed), &cmd, &id, &t.id, 0, now).is_err());
        // Its answer.
        let body = serde_json::to_vec(&DaemonResult::Deleted { id: "sbx_i1".into() }).unwrap();
        let (st, _) = call(&app, Method::POST, &format!("/commands/{cmd}"), Some(&key), &body).await;
        assert_eq!(st, StatusCode::OK);
        let status: String = sqlx::query_scalar("SELECT status FROM pyo.sandbox_commands WHERE id = $1")
            .bind(&cmd)
            .fetch_one(&s.db)
            .await
            .unwrap();
        assert_eq!(status, "done");
        // A new key with its own.
        let renewed = new_key();
        let body = serde_json::to_vec(&json!({ "publicKey": renewed.public_key() })).unwrap();
        let (st, v) = call(&app, Method::POST, wire::RENEW_KEY_PATH, Some(&key), &body).await;
        assert_eq!(st, StatusCode::OK, "{v}");

        // Only the installation's admin resets it.
        let e = reset_access(&s, signer.as_ref(), &t.admin, &id, &Mode::SelfHosted(on_tailnet_of(&t))).await.unwrap_err();
        assert_eq!(e.status, StatusCode::FORBIDDEN);

        sqlx::query("DELETE FROM pyo.sandbox_servers WHERE id = $1").bind(&id).execute(&s.db).await.unwrap();
        let (st, _) = call(&app, Method::GET, "/commands?wait=0", Some(&key), b"").await;
        assert_eq!(st, StatusCode::UNAUTHORIZED);
        cleanup(&s, &t).await;
    })
}

/// A server that joined at the door before 0.52.0: Pyo doesn't call it,
/// every command against it and its sandboxes says to update it, and the
/// dashboard asks for an update. **Update** gives a command without
/// stopping anything, and its join over the channel moves it.
#[test]
#[ignore = "needs DATABASE_URL (a throwaway Postgres)"]
fn a_server_from_the_door_moves_to_the_channel() {
    run_big(async {
        let s = state().await;
        let signer = Arc::new(Local::load(&s.db, &s.sealer).await.unwrap());
        let t = team(&s.db, true).await;
        let id = crate::ids::new_id("sbs");
        sqlx::query(
            "INSERT INTO pyo.sandbox_servers (id, team_id, name, address, state, created_by, joined_at, via_cluster)
             VALUES ($1, $2, 'sbx-old', '100.64.0.30', 'ready', 'test', now() - interval '1 day', $3)",
        )
        .bind(&id)
        .bind(&t.id)
        .bind(&t.holder)
        .execute(&s.db)
        .await
        .unwrap();
        let view = || {
            let (db, team, id) = (s.db.clone(), t.id.clone(), id.clone());
            async move {
                let mut c = db.acquire().await.unwrap();
                crate::sandbox::server_views(&mut c, &[team], Some(&id)).await.unwrap().remove(0)
            }
        };
        let v = view().await;
        assert_eq!((v["outbound"].clone(), v["needsUpdate"].clone()), (json!(false), json!(true)), "{v}");

        // A sandbox on it: shown as needing the update, and every command
        // says so, plainly, before anything is sent.
        let sbx = format!("sbx_old{}", crate::ids::random_string(8).to_lowercase());
        sqlx::query(
            "INSERT INTO pyo.sandboxes (id, team_id, server_id, spec, state, created_by, expires_at, name)
             VALUES ($1, $2, $3, '{}', 'running', 'test', now() + interval '1 hour', 'old one')",
        )
        .bind(&sbx)
        .bind(&t.id)
        .bind(&id)
        .execute(&s.db)
        .await
        .unwrap();
        let sbx_view = crate::sandbox::sandbox_views(&s.db, std::slice::from_ref(&t.id), Some(&sbx), false)
            .await
            .unwrap()
            .remove(0);
        assert_eq!((sbx_view["serverNeedsUpdate"].clone(), sbx_view["url"].clone()), (json!(true), json!(null)), "{sbx_view}");
        let user = t.admin.clone();
        let needs = |e: crate::error::ApiError| (e.status, e.message);
        let want = (StatusCode::CONFLICT, crate::sandbox::NEEDS_UPDATE.to_owned());
        assert_eq!(needs(crate::sandbox::token(&s, &user, &sbx).await.unwrap_err()), want);
        assert_eq!(needs(crate::sandbox::pause(&s, &user, &sbx).await.unwrap_err()), want);
        assert_eq!(needs(crate::sandbox::delete(&s, &user, &sbx).await.unwrap_err()), want);
        assert_eq!(needs(crate::sandbox::open_port(&s, &user, &sbx, 3000, false).await.unwrap_err()), want);
        let f = crate::sandbox::find(&s, &user, &sbx, crate::sandbox::Need::Use).await.unwrap();
        assert_eq!(needs(f.daemon().err().unwrap()), want);
        let got = crate::sandbox::get(&s, &user, &sbx).await.unwrap();
        assert_eq!((got["connection"].clone(), got["connectionNote"].clone()), (json!(null), json!(crate::sandbox::NEEDS_UPDATE)), "{got}");
        let state: String = sqlx::query_scalar("SELECT state FROM pyo.sandboxes WHERE id = $1")
            .bind(&sbx)
            .fetch_one(&s.db)
            .await
            .unwrap();
        assert_eq!(state, "running", "a refused delete leaves it as it was");
        // A new sandbox isn't placed on it: named, or not.
        sqlx::query("INSERT INTO pyo.sandbox_templates (id, team_id, name, source, state, created_by) VALUES ($1, $2, 'busybox', '{\"kind\":\"image\",\"image\":\"busybox\"}', 'ready', 'test')")
            .bind(format!("stp_{sbx}"))
            .bind(&t.id)
            .execute(&s.db)
            .await
            .unwrap();
        let req = |server: Option<&str>| -> crate::sandbox::CreateSandbox {
            serde_json::from_value(json!({ "teamId": t.id, "template": "busybox", "server": server, "ttl": "5m", "network": "none" })).unwrap()
        };
        let e = crate::sandbox::create(&s, &user, req(Some("sbx-old"))).await.unwrap_err();
        assert_eq!(needs(e), want);
        // Unnamed: never placed on it. Other tests' installation servers may
        // be in this database, so the reason varies (scheduler.rs tests the
        // one for a team whose servers all need an update).
        assert!(crate::sandbox::create(&s, &user, req(None)).await.is_err());
        let placed: i64 = sqlx::query_scalar("SELECT count(*) FROM pyo.sandboxes WHERE server_id = $1")
            .bind(&id)
            .fetch_one(&s.db)
            .await
            .unwrap();
        assert_eq!(placed, 1, "only the one from before");

        // Update: the command, and nothing stops.
        let at = on_tailnet_of(&t);
        let e = reset_access(&s, signer.as_ref(), &t.developer, &id, &Mode::SelfHosted(at.clone())).await.unwrap_err();
        assert_eq!(e.status, StatusCode::FORBIDDEN);
        // Pyo Cloud has none of these.
        let e = reset_access(&s, signer.as_ref(), &t.admin, &id, &Mode::Hosted).await.unwrap_err();
        assert_eq!(e.status, StatusCode::CONFLICT);
        let agent = holder_agent(s.db.clone(), t.holder.clone());
        let j = reset_access(&s, signer.as_ref(), &t.admin, &id, &Mode::SelfHosted(at.clone())).await;
        agent.abort();
        let j = j.unwrap();
        assert_eq!(j.server_id, id);
        assert!(j.command("https://s.example.com", "0.51.1").contains(" --pyo-url "));
        let (outbound, state): (bool, String) =
            sqlx::query_as("SELECT outbound, state FROM pyo.sandbox_servers WHERE id = $1")
                .bind(&id)
                .fetch_one(&s.db)
                .await
                .unwrap();
        assert_eq!((outbound, state.as_str()), (false, "ready"), "nothing changes until it joins");

        // Run there: it joins over the channel, and Pyo stops calling it.
        let k = new_key();
        let ask = ask(&id, &j.code, &k.public_key(), "sbx-1", &format!("sbx-1.{}", t.suffix));
        let a = join(&s.db, &s.sealer, &signer.public_keys(), "https://unused.example.com", false, &ask).await.unwrap();
        assert_eq!(a.config["outbound"]["pyoUrl"], json!(j.pyo_url));
        let v = view().await;
        assert_eq!((v["outbound"].clone(), v["needsUpdate"].clone()), (json!(true), json!(false)), "{v}");
        assert!(v["joinedAt"].as_i64().unwrap() > pyo_proto::now_ms() - 60_000, "{v}");
        let app = app(&s, signer.clone());
        let key = Key(a.key_id, k);
        assert_eq!(call(&app, Method::GET, "/commands?wait=0", Some(&key), b"").await.0, StatusCode::OK);
        // Its sandbox reads as one Pyo reaches again.
        let f = crate::sandbox::find(&s, &user, &sbx, crate::sandbox::Need::Use).await.unwrap();
        assert!(f.daemon().is_ok());
        let sbx_view = crate::sandbox::sandbox_views(&s.db, std::slice::from_ref(&t.id), Some(&sbx), false)
            .await
            .unwrap()
            .remove(0);
        assert_eq!(sbx_view["serverNeedsUpdate"], json!(false), "{sbx_view}");
        assert_eq!(sbx_view["url"], json!(format!("https://sbx-1.{}:{}", t.suffix, wire::TAILNET_PORT)), "{sbx_view}");
        cleanup(&s, &t).await;
    })
}
