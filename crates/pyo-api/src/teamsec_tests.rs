//! Team security end to end, against a throwaway Postgres (DATABASE_URL):
//! two-factor setup and sign-in, recovery codes, teams that require it, the
//! installation's and Pyo Cloud staff's rule, and the audit log with what
//! it records and who may read it. Through the real routes, as a browser.

use std::net::SocketAddr;

use axum::{
    Router,
    body::Body,
    extract::ConnectInfo,
    http::{Method, Request, StatusCode, header},
};
use serde_json::{Value, json};
use sqlx::PgPool;
use tower::ServiceExt;

use crate::ids::random_string;

thread_local! {
    static ON_SHARED_RUNTIME: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Runs a test on a thread with a big stack, on one runtime that every
/// Postgres test shares and that lives as long as the process. The vault is
/// the process's and keeps the pool of the test that set it up first, and
/// a connection belongs to the runtime of the test that opened it. A
/// runtime of the test's own would shut down when that test ends, and a
/// query on one of its connections then fails in whichever test still
/// runs ("A Tokio 1.x context was found, but it is being shutdown."). So
/// every test that uses the vault runs here, and [`crate::vault::get`]
/// fails one that doesn't.
pub(crate) fn run_big<F: std::future::Future<Output = ()> + Send + 'static>(f: F) {
    static RUNTIME: std::sync::LazyLock<tokio::runtime::Runtime> = std::sync::LazyLock::new(|| {
        tokio::runtime::Builder::new_multi_thread()
            .thread_stack_size(64 << 20)
            .on_thread_start(|| ON_SHARED_RUNTIME.set(true))
            .enable_all()
            .build()
            .unwrap()
    });
    std::thread::Builder::new()
        .stack_size(64 << 20)
        .spawn(|| {
            ON_SHARED_RUNTIME.set(true);
            RUNTIME.block_on(f)
        })
        .unwrap()
        .join()
        .unwrap();
}

/// Whether this thread runs the runtime [`run_big`] shares.
pub(crate) fn on_shared_runtime() -> bool {
    ON_SHARED_RUNTIME.get()
}

pub(crate) struct Client {
    pub(crate) app: Router,
    pub(crate) cookie: Option<String>,
    pub(crate) ip: &'static str,
}

impl Client {
    pub(crate) async fn call(
        &mut self,
        method: Method,
        path: &str,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        let (st, bytes) = self.raw(method, path, body).await;
        (st, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
    }

    async fn raw(
        &mut self,
        method: Method,
        path: &str,
        body: Option<Value>,
    ) -> (StatusCode, Vec<u8>) {
        let mut req = Request::builder()
            .method(method)
            .uri(path)
            .header("x-pyo-csrf", "1")
            .header(header::USER_AGENT, "TestBrowser/1.0");
        if let Some(c) = &self.cookie {
            req = req.header(header::COOKIE, c);
        }
        let mut req = match body {
            Some(b) => req
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(b.to_string()))
                .unwrap(),
            None => req.body(Body::empty()).unwrap(),
        };
        req.extensions_mut().insert(ConnectInfo(
            format!("{}:4000", self.ip).parse::<SocketAddr>().unwrap(),
        ));
        let res = self.app.clone().oneshot(req).await.unwrap();
        let st = res.status();
        if let Some(c) = res.headers().get(header::SET_COOKIE) {
            let c = c.to_str().unwrap();
            if c.starts_with("pyo_session=") {
                self.cookie = Some(c.split(';').next().unwrap().to_owned());
            }
        }
        let bytes = axum::body::to_bytes(res.into_body(), 64 << 20)
            .await
            .unwrap();
        (st, bytes.to_vec())
    }

    pub(crate) async fn op(&mut self, op: Value) -> (StatusCode, Value) {
        self.call(Method::POST, "/api/v1/ops", Some(op)).await
    }
}

/// The current code of an account's app, from its sealed secret.
async fn code_now(db: &PgPool, user: &str, offset_steps: i64) -> String {
    let sealed: String =
        sqlx::query_scalar("SELECT secret_sealed FROM pyo.user_totp WHERE user_id = $1")
            .bind(user)
            .fetch_one(db)
            .await
            .unwrap();
    let b32 = crate::secrets::Sealer::for_tests().open(&sealed).unwrap();
    let secret = totp_rs::Secret::try_from_base32(&b32).unwrap();
    let t = totp_rs::Builder::new().with_secret(secret).build().unwrap();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    t.generate((now + offset_steps * 30) as u64).to_string()
}

/// An audit row as the checks read it: action, outcome, via, ip, user
/// agent, actor name, actor id.
type Row = (
    String,
    String,
    String,
    Option<String>,
    String,
    String,
    Option<String>,
);

async fn audit_rows(db: &PgPool, team: &str) -> Vec<Row> {
    sqlx::query_as(
        "SELECT action, outcome, via, ip, user_agent, actor_name, actor_id FROM pyo.audit_events WHERE team_id = $1 ORDER BY at, id",
    )
    .bind(team)
    .fetch_all(db)
    .await
    .unwrap()
}

fn has(rows: &[Row], action: &str) -> bool {
    rows.iter().any(|r| r.0 == action)
}

#[test]
#[ignore = "needs DATABASE_URL (a throwaway Postgres)"]
fn two_factor_required_teams_and_the_audit_log() {
    run_big(async {
        let db = PgPool::connect(&std::env::var("DATABASE_URL").expect("DATABASE_URL"))
            .await
            .unwrap();
        crate::migrate::run(&db).await.unwrap();
        crate::vault::init(crate::vault::Vault::new(
            db.clone(),
            crate::vault::Provider::from_env(),
        ));
        let state = crate::sandbox::e2e::test_state(db.clone());
        let app = Router::new()
            .nest("/api/auth", crate::auth::routes())
            .nest("/api/v1", crate::ops::routes())
            .layer(axum::middleware::from_fn(crate::audit::middleware))
            .with_state(state);
        let run = random_string(8).to_lowercase();
        let id = |p: &str, t: &str| format!("{p}_{t}{run}");
        let (ada, bob) = (id("usr", "ada"), id("usr", "bob"));
        let (team_a, team_b) = (id("team", "a"), id("team", "b"));
        let email = |who: &str| format!("{who}{run}@teamsec.test");
        let hash = crate::auth::hash_password("correct horse battery")
            .await
            .unwrap();
        for (u, n) in [(&ada, "ada"), (&bob, "bob")] {
            sqlx::query(
                "INSERT INTO pyo.users (id, email, name, password_hash) VALUES ($1, $2, $3, $4)",
            )
            .bind(u)
            .bind(email(n))
            .bind(n)
            .bind(&hash)
            .execute(&db)
            .await
            .unwrap();
        }
        for t in [&team_a, &team_b] {
            sqlx::query("INSERT INTO pyo.teams (id, name, slug) VALUES ($1, $1, $1)")
                .bind(t)
                .execute(&db)
                .await
                .unwrap();
        }
        // Ada owns A, Bob develops in A and B; B's owner is Ada too.
        for (m, t, u, n, role) in [
            ("a1", &team_a, &ada, "ada", "owner"),
            ("a2", &team_a, &bob, "bob", "developer"),
            ("b1", &team_b, &ada, "ada", "owner"),
            ("b2", &team_b, &bob, "bob", "developer"),
        ] {
            sqlx::query("INSERT INTO pyo.members (id, team_id, user_id, email, role, status) VALUES ($1, $2, $3, $4, $5, 'active')")
                .bind(id("mem", m))
                .bind(t)
                .bind(u)
                .bind(email(n))
                .bind(role)
                .execute(&db)
                .await
                .unwrap();
        }
        let mut a = Client {
            app: app.clone(),
            cookie: None,
            ip: "203.0.113.7",
        };
        let mut b = Client {
            app: app.clone(),
            cookie: None,
            ip: "198.51.100.9",
        };

        // ── Signing in: recorded with address, browser and how.
        let (st, _) = a
            .call(
                Method::POST,
                "/api/auth/login",
                Some(json!({ "email": email("ada"), "password": "correct horse battery" })),
            )
            .await;
        assert_eq!(st, StatusCode::OK);
        assert!(a.cookie.is_some(), "no two-factor yet: signed in");
        let (st, _) = b
            .call(
                Method::POST,
                "/api/auth/login",
                Some(json!({ "email": email("bob"), "password": "wrong password!" })),
            )
            .await;
        assert_eq!(st, StatusCode::UNAUTHORIZED);
        let (st, _) = b
            .call(
                Method::POST,
                "/api/auth/login",
                Some(json!({ "email": email("bob"), "password": "correct horse battery" })),
            )
            .await;
        assert_eq!(st, StatusCode::OK);
        let rows = audit_rows(&db, &team_a).await;
        let sign_in = rows
            .iter()
            .find(|r| r.0 == "auth.sign_in" && r.5 == "ada")
            .expect("Ada's sign-in");
        assert_eq!(
            (sign_in.1.as_str(), sign_in.2.as_str()),
            ("ok", "dashboard")
        );
        assert_eq!(sign_in.3.as_deref(), Some("203.0.113.7"));
        assert_eq!(sign_in.4, "TestBrowser/1.0");
        assert_eq!(sign_in.6.as_deref(), Some(ada.as_str()));
        let failed = rows
            .iter()
            .find(|r| r.0 == "auth.sign_in_failed")
            .expect("Bob's wrong password");
        assert_eq!((failed.1.as_str(), failed.5.as_str()), ("failed", "bob"));
        assert!(
            has(&audit_rows(&db, &team_b).await, "auth.sign_in_failed"),
            "in each of Bob's teams"
        );

        // ── Setting up two-factor: the password, then a code.
        let (st, _) = a
            .call(
                Method::POST,
                "/api/auth/two-factor/setup",
                Some(json!({ "password": "nope nope nope" })),
            )
            .await;
        assert_eq!(st, StatusCode::FORBIDDEN);
        let (st, setup) = a
            .call(
                Method::POST,
                "/api/auth/two-factor/setup",
                Some(json!({ "password": "correct horse battery" })),
            )
            .await;
        assert_eq!(st, StatusCode::OK, "{setup}");
        assert!(
            setup["uri"]
                .as_str()
                .unwrap()
                .starts_with("otpauth://totp/Pyo:")
        );
        assert!(setup["qr"].as_str().unwrap().contains("<svg"));
        let stored: String =
            sqlx::query_scalar("SELECT secret_sealed FROM pyo.user_totp WHERE user_id = $1")
                .bind(&ada)
                .fetch_one(&db)
                .await
                .unwrap();
        assert!(stored.starts_with("sb1:"), "sealed at rest");
        assert!(!stored.contains(&setup["secret"].as_str().unwrap().replace(' ', "")));
        let (st, _) = a
            .call(
                Method::POST,
                "/api/auth/two-factor/enable",
                Some(json!({ "code": "000000" })),
            )
            .await;
        assert_eq!(st, StatusCode::BAD_REQUEST);
        let code = code_now(&db, &ada, 0).await;
        let (st, on) = a
            .call(
                Method::POST,
                "/api/auth/two-factor/enable",
                Some(json!({ "code": code })),
            )
            .await;
        assert_eq!(st, StatusCode::OK, "{on}");
        let codes: Vec<String> = serde_json::from_value(on["recoveryCodes"].clone()).unwrap();
        assert_eq!(codes.len(), 10);
        let (_, me) = a.call(Method::GET, "/api/auth/me", None).await;
        assert_eq!(me["twoFactor"]["enabled"], true);
        assert_eq!(me["twoFactor"]["recoveryCodesLeft"], 10);
        assert!(has(
            &audit_rows(&db, &team_a).await,
            "auth.two_factor_enabled"
        ));

        // ── Signing in now asks for a code, and gives no session first.
        let mut a2 = Client {
            app: app.clone(),
            cookie: None,
            ip: "203.0.113.7",
        };
        let (st, first) = a2
            .call(
                Method::POST,
                "/api/auth/login",
                Some(json!({ "email": email("ada"), "password": "correct horse battery" })),
            )
            .await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(first["twoFactor"], true);
        assert!(a2.cookie.is_none(), "no session before the code");
        let challenge = first["challenge"].as_str().unwrap().to_owned();
        let (st, _) = a2
            .call(
                Method::POST,
                "/api/auth/login/code",
                Some(json!({ "challenge": challenge, "code": "123456" })),
            )
            .await;
        assert_eq!(st, StatusCode::UNAUTHORIZED);
        assert!(a2.cookie.is_none());
        let (st, _) = a2
            .call(
                Method::POST,
                "/api/auth/login/code",
                Some(json!({ "challenge": challenge, "code": codes[0] })),
            )
            .await;
        assert_eq!(st, StatusCode::OK, "a recovery code");
        assert!(a2.cookie.is_some());
        // The same recovery code again: no.
        let mut a3 = Client {
            app: app.clone(),
            cookie: None,
            ip: "203.0.113.7",
        };
        let (_, first) = a3
            .call(
                Method::POST,
                "/api/auth/login",
                Some(json!({ "email": email("ada"), "password": "correct horse battery" })),
            )
            .await;
        let (st, _) = a3
            .call(
                Method::POST,
                "/api/auth/login/code",
                Some(json!({ "challenge": first["challenge"], "code": codes[0] })),
            )
            .await;
        assert_eq!(st, StatusCode::UNAUTHORIZED, "a recovery code works once");
        let (st, _) = a3
            .call(
                Method::POST,
                "/api/auth/login/code",
                Some(json!({ "challenge": first["challenge"], "code": codes[1] })),
            )
            .await;
        assert_eq!(st, StatusCode::OK);
        let rows = audit_rows(&db, &team_a).await;
        assert!(has(&rows, "auth.recovery_code_used"));
        assert!(
            rows.iter()
                .any(|r| r.0 == "auth.sign_in_failed" && r.5 == "ada"),
            "the wrong code"
        );
        // A challenge alone isn't a session.
        let (st, _) = Client {
            app: app.clone(),
            cookie: None,
            ip: "203.0.113.7",
        }
        .call(Method::GET, "/api/v1/state", None)
        .await;
        assert_eq!(st, StatusCode::UNAUTHORIZED);

        // ── A team that requires two-factor: Bob can't open it, his account still works.
        let (st, out) = b
            .op(json!({ "op": "setTeamSecurity", "teamId": team_a, "requireTwoFactor": true }))
            .await;
        assert_eq!(st, StatusCode::FORBIDDEN, "developers can't: {out}");
        let (st, out) = a.op(json!({ "op": "setTeamSecurity", "teamId": team_a, "requireTwoFactor": true, "auditIps": "anonymized" })).await;
        assert_eq!(st, StatusCode::OK, "{out}");
        let (st, state) = b.call(Method::GET, "/api/v1/state", None).await;
        assert_eq!(st, StatusCode::OK);
        let teams: Vec<&str> = state["teams"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|t| t["id"].as_str())
            .collect();
        assert!(
            !teams.contains(&team_a.as_str()) && teams.contains(&team_b.as_str()),
            "{teams:?}"
        );
        assert_eq!(
            state["user"]["twoFactor"]["lockedTeams"][0]["id"],
            json!(team_a)
        );
        assert_eq!(state["user"]["twoFactor"]["setupNeeded"], false);
        let (st, _) = b
            .op(json!({ "op": "updateMemberRole", "id": id("mem", "a2"), "role": "viewer" }))
            .await;
        assert_eq!(st, StatusCode::NOT_FOUND, "team A is out of reach");
        // Ada's still in, with Bob shown without two-factor.
        let (_, state) = a.call(Method::GET, "/api/v1/state", None).await;
        let bob_member = state["members"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["id"] == json!(id("mem", "a2")))
            .unwrap()
            .clone();
        assert_eq!(bob_member["twoFactor"], false);
        assert_eq!(
            state["teams"]
                .as_array()
                .unwrap()
                .iter()
                .find(|t| t["id"] == json!(team_a))
                .unwrap()["requireTwoFactor"],
            true
        );
        // Required: Ada can't turn hers off.
        let code = code_now(&db, &ada, 1).await;
        let (st, _) = a
            .call(
                Method::POST,
                "/api/auth/two-factor/disable",
                Some(json!({ "code": code })),
            )
            .await;
        assert_eq!(st, StatusCode::FORBIDDEN);

        // ── What the audit log records from here on: anonymized addresses.
        let (st, out) = a
            .op(json!({ "op": "updateMemberRole", "id": id("mem", "a2"), "role": "viewer" }))
            .await;
        assert_eq!(st, StatusCode::OK, "{out}");
        let (st, key) = a.op(json!({ "op": "createApiToken", "id": format!("tok_{run}"), "name": "deploy bot", "access": "read", "teamIds": [team_a] })).await;
        assert_eq!(st, StatusCode::OK, "{key}");
        let rows = audit_rows(&db, &team_a).await;
        let role = rows
            .iter()
            .find(|r| r.0 == "member.role_changed")
            .expect("role change");
        assert_eq!(role.3.as_deref(), Some("203.0.113.0"), "anonymized");
        assert!(has(&rows, "api_key.created"));
        assert!(
            !has(&audit_rows(&db, &team_b).await, "api_key.created"),
            "only the key's teams"
        );
        let feed: i64 = sqlx::query_scalar("SELECT count(*) FROM pyo.activity WHERE team_id = $1 AND data->>'type' = 'api_key.created'").bind(&team_a).fetch_one(&db).await.unwrap();
        assert_eq!(feed, 1, "in the team's notifications too");
        // Using the key: recorded as the API, with its name.
        let token = key["token"].as_str().unwrap().to_owned();
        let mut req = Request::builder()
            .method(Method::GET)
            .uri(format!("/api/v1/audit-log?team={team_a}"))
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .body(Body::empty())
            .unwrap();
        req.extensions_mut()
            .insert(ConnectInfo("192.0.2.1:1".parse::<SocketAddr>().unwrap()));
        let res = app.clone().oneshot(req).await.unwrap();
        assert_eq!(
            res.status(),
            StatusCode::OK,
            "owners may read it with a key too"
        );
        let (st, out) = a
            .op(json!({ "op": "revokeApiToken", "id": format!("tok_{run}") }))
            .await;
        assert_eq!(st, StatusCode::OK, "{out}");
        assert!(has(&audit_rows(&db, &team_a).await, "api_key.revoked"));

        // ── Reading it: owners and admins only.
        let (st, log) = a
            .call(
                Method::GET,
                &format!("/api/v1/audit-log?team={team_a}"),
                None,
            )
            .await;
        assert_eq!(st, StatusCode::OK, "{log}");
        assert_eq!(log["ips"], "anonymized");
        assert_eq!(log["days"], 365);
        assert!(
            log["people"]
                .as_array()
                .unwrap()
                .iter()
                .any(|p| p["id"] == json!(ada))
        );
        let (_, only_auth) = a
            .call(
                Method::GET,
                &format!("/api/v1/audit-log?team={team_a}&action=auth"),
                None,
            )
            .await;
        assert!(
            only_auth["events"]
                .as_array()
                .unwrap()
                .iter()
                .all(|e| e["action"].as_str().unwrap().starts_with("auth."))
        );
        let (_, only_bob) = a
            .call(
                Method::GET,
                &format!("/api/v1/audit-log?team={team_a}&person={bob}"),
                None,
            )
            .await;
        let events = only_bob["events"].as_array().unwrap();
        assert!(!events.is_empty() && events.iter().all(|e| e["actor"]["id"] == json!(bob)));
        let (_, none) = a
            .call(
                Method::GET,
                &format!("/api/v1/audit-log?team={team_a}&from=4102444800000"),
                None,
            )
            .await;
        assert!(
            none["events"].as_array().unwrap().is_empty(),
            "a date filter"
        );
        let (st, csv) = a
            .raw(
                Method::GET,
                &format!("/api/v1/audit-log?team={team_a}&format=csv"),
                None,
            )
            .await;
        assert_eq!(st, StatusCode::OK);
        let csv = String::from_utf8(csv).unwrap();
        assert!(csv.starts_with("time,person,email,action,"));
        assert!(csv.contains(",member.role_changed,"));
        // Bob in team B is a developer: no.
        let (st, _) = b
            .call(
                Method::GET,
                &format!("/api/v1/audit-log?team={team_b}"),
                None,
            )
            .await;
        assert_eq!(st, StatusCode::FORBIDDEN);
        // And team A he can't open at all.
        let (st, _) = b
            .call(
                Method::GET,
                &format!("/api/v1/audit-log?team={team_a}"),
                None,
            )
            .await;
        assert_eq!(st, StatusCode::NOT_FOUND);

        // ── Off again: the requirement first, then her own, with a code.
        let (st, _) = a
            .op(json!({ "op": "setTeamSecurity", "teamId": team_a, "requireTwoFactor": false }))
            .await;
        assert_eq!(st, StatusCode::OK);
        let code = code_now(&db, &ada, 1).await;
        let (st, out) = a
            .call(
                Method::POST,
                "/api/auth/two-factor/disable",
                Some(json!({ "code": code })),
            )
            .await;
        assert_eq!(st, StatusCode::OK, "{out}");
        assert!(has(
            &audit_rows(&db, &team_b).await,
            "auth.two_factor_disabled"
        ));
        let feed: i64 = sqlx::query_scalar("SELECT count(*) FROM pyo.activity WHERE team_id = $1 AND data->>'type' = 'auth.two_factor_disabled'").bind(&team_b).fetch_one(&db).await.unwrap();
        assert_eq!(feed, 1, "the team hears of it");
        assert!(
            sqlx::query_scalar::<_, i64>(
                "SELECT count(*) FROM pyo.recovery_codes WHERE user_id = $1"
            )
            .bind(&ada)
            .fetch_one(&db)
            .await
            .unwrap()
                == 0
        );

        // ── Pyo Cloud staff (admins of a hosted Pyo) always need it; the
        // installation's rule needs it of everyone.
        sqlx::query("UPDATE pyo.users SET is_admin = true WHERE id = $1")
            .bind(&ada)
            .execute(&db)
            .await
            .unwrap();
        let teams_of = |user: String, hosted: bool| {
            let db = db.clone();
            async move {
                sqlx::query_scalar::<_, String>("SELECT pyo.member_team_ids($1, NULL, $2)")
                    .bind(user)
                    .bind(hosted)
                    .fetch_one(&db)
                    .await
                    .unwrap()
            }
        };
        assert_eq!(
            teams_of(ada.clone(), true).await,
            "",
            "staff without two-factor: no teams"
        );
        assert!(
            !teams_of(ada.clone(), false).await.is_empty(),
            "self-hosted admins: as before"
        );
        assert!(
            !teams_of(bob.clone(), true).await.is_empty(),
            "everyone else on Cloud: as before"
        );
        let staff_admin: bool = sqlx::query_scalar(
            "SELECT is_admin AND NOT pyo.needs_two_factor(id, true) FROM pyo.users WHERE id = $1",
        )
        .bind(&ada)
        .fetch_one(&db)
        .await
        .unwrap();
        assert!(!staff_admin, "and no admin powers until it's on");
        sqlx::query("INSERT INTO pyo.user_totp (user_id, secret_sealed, enabled_at) VALUES ($1, 'sb1:x', now())").bind(&ada).execute(&db).await.unwrap();
        assert!(
            !teams_of(ada.clone(), true).await.is_empty(),
            "staff with two-factor: in"
        );
        // The installation's rule, in a transaction of its own: other tests
        // run beside this one on the same database.
        let mut tx = db.begin().await.unwrap();
        sqlx::query("INSERT INTO pyo.settings (key, value) VALUES ('security', '{\"requireTwoFactor\": true, \"auditDays\": 365}') ON CONFLICT (key) DO UPDATE SET value = excluded.value")
            .execute(&mut *tx)
            .await
            .unwrap();
        let (bobs, needs): (String, bool) = sqlx::query_as(
            "SELECT pyo.member_team_ids($1, NULL, false), pyo.needs_two_factor($1, false)",
        )
        .bind(&bob)
        .fetch_one(&mut *tx)
        .await
        .unwrap();
        assert_eq!(
            (bobs.as_str(), needs),
            ("", true),
            "the installation requires it"
        );
        let adas: String = sqlx::query_scalar("SELECT pyo.member_team_ids($1, NULL, false)")
            .bind(&ada)
            .fetch_one(&mut *tx)
            .await
            .unwrap();
        assert!(!adas.is_empty(), "accounts with it: as before");
        tx.rollback().await.unwrap();

        // Nothing a team's request can change or remove in its log.
        let mut tx = db.begin().await.unwrap();
        let user = crate::auth::User {
            id: ada.clone(),
            name: "ada".into(),
            email: email("ada"),
            timezone: "UTC".into(),
            is_admin: false,
            must_set_up: false,
            key: None,
        };
        crate::rls::scope(&mut tx, &user).await.unwrap();
        let n = sqlx::query("DELETE FROM pyo.audit_events WHERE team_id = $1")
            .bind(&team_a)
            .execute(&mut *tx)
            .await
            .unwrap()
            .rows_affected();
        assert_eq!(n, 0, "append only");
        let n = sqlx::query("UPDATE pyo.audit_events SET title = 'x' WHERE team_id = $1")
            .bind(&team_a)
            .execute(&mut *tx)
            .await
            .unwrap()
            .rows_affected();
        assert_eq!(n, 0);
        tx.rollback().await.unwrap();

        for t in [&team_a, &team_b] {
            sqlx::query("DELETE FROM pyo.teams WHERE id = $1")
                .bind(t)
                .execute(&db)
                .await
                .unwrap();
        }
        for u in [&ada, &bob] {
            sqlx::query("DELETE FROM pyo.users WHERE id = $1")
                .bind(u)
                .execute(&db)
                .await
                .unwrap();
        }
    });
}

/// The installation's settings are its admins' only: a team owner who isn't
/// one (a Pyo Cloud customer) reads whether observability is on, and the
/// analytics limit with its team, never the installation's details.
#[test]
#[ignore = "needs DATABASE_URL (a throwaway Postgres)"]
fn only_installation_admins_read_installation_settings() {
    run_big(async {
        let db = PgPool::connect(&std::env::var("DATABASE_URL").expect("DATABASE_URL"))
            .await
            .unwrap();
        crate::migrate::run(&db).await.unwrap();
        crate::vault::init(crate::vault::Vault::new(
            db.clone(),
            crate::vault::Provider::from_env(),
        ));
        let state = crate::sandbox::e2e::test_state(db.clone());
        let app = Router::new()
            .nest("/api/auth", crate::auth::routes())
            .nest("/api/v1", crate::ops::routes())
            .layer(axum::middleware::from_fn(crate::audit::middleware))
            .with_state(state);
        let run = random_string(8).to_lowercase();
        let id = |p: &str, t: &str| format!("{p}_{t}{run}");
        let (adm, own) = (id("usr", "adm"), id("usr", "own"));
        let team = id("team", "vis");
        let email = |who: &str| format!("{who}{run}@settings.test");
        let password = "correct horse battery";
        let hash = crate::auth::hash_password(password).await.unwrap();
        for (u, n, admin) in [(&adm, "adm", true), (&own, "own", false)] {
            sqlx::query("INSERT INTO pyo.users (id, email, name, password_hash, is_admin) VALUES ($1, $2, $3, $4, $5)")
                .bind(u)
                .bind(email(n))
                .bind(n)
                .bind(&hash)
                .bind(admin)
                .execute(&db)
                .await
                .unwrap();
        }
        sqlx::query("INSERT INTO pyo.teams (id, name, slug) VALUES ($1, $1, $1)")
            .bind(&team)
            .execute(&db)
            .await
            .unwrap();
        for (m, u, n) in [("a", &adm, "adm"), ("o", &own, "own")] {
            sqlx::query("INSERT INTO pyo.members (id, team_id, user_id, email, role, status) VALUES ($1, $2, $3, $4, 'owner', 'active')")
                .bind(id("mem", m))
                .bind(&team)
                .bind(u)
                .bind(email(n))
                .execute(&db)
                .await
                .unwrap();
        }
        let signed_in = |who: &'static str, ip: &'static str| {
            let e = email(who);
            let mut c = Client { app: app.clone(), cookie: None, ip };
            async move {
                let (st, _) = c
                    .call(Method::POST, "/api/auth/login", Some(json!({ "email": e, "password": password })))
                    .await;
                assert_eq!(st, StatusCode::OK, "{who} signs in");
                c
            }
        };
        let mut admin = signed_in("adm", "203.0.113.7").await;
        let mut owner = signed_in("own", "198.51.100.9").await;

        // The admin reads everything.
        let (st, all) = admin.call(Method::GET, "/api/v1/observability/settings", None).await;
        assert_eq!(st, StatusCode::OK);
        assert!(all["ipMode"].is_string());
        let limit = all["retention"]["analyticsMonths"].as_u64().expect("retention");
        let (_, snap) = admin.call(Method::GET, "/api/v1/state", None).await;
        assert!(snap["observabilitySettings"]["retention"].is_object());
        assert!(snap["securitySettings"].is_object());

        // The team's owner: whether it's on, and the limit with the team.
        let (st, mine) = owner.call(Method::GET, "/api/v1/observability/settings", None).await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(mine, json!({ "enabled": all["enabled"] }));
        let (st, snap) = owner.call(Method::GET, "/api/v1/state", None).await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(snap["observabilitySettings"], json!({ "enabled": all["enabled"] }));
        assert!(snap["securitySettings"].is_null());
        assert!(snap["adminSignIn"].is_null());
        let t = snap["teams"]
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["id"] == json!(team))
            .expect("the team");
        assert_eq!(t["analyticsMonthsLimit"], json!(limit));

        sqlx::query("DELETE FROM pyo.teams WHERE id = $1")
            .bind(&team)
            .execute(&db)
            .await
            .unwrap();
        for u in [&adm, &own] {
            sqlx::query("DELETE FROM pyo.users WHERE id = $1")
                .bind(u)
                .execute(&db)
                .await
                .unwrap();
        }
    });
}
