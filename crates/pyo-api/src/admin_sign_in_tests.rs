//! Where admins may sign in from, end to end, against a throwaway Postgres
//! (DATABASE_URL): saving the limit (never one that locks out the one
//! saving it), sign-in refused like a wrong password with an audit entry,
//! sessions as if signed out, API keys as a member, and everyone else
//! never affected. Through the real routes, as a browser and a script.

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

use crate::ids::{random_string, sha256_hex};
use crate::teamsec_tests::{Client, run_big};

const INSIDE: &str = "203.0.113.7";
const OUTSIDE: &str = "192.0.2.50";

/// GET /api/v1/me with an API key, from `ip`.
async fn me_with_key(app: &Router, key: &str, ip: &str) -> (StatusCode, Value) {
    let mut req = Request::builder()
        .method(Method::GET)
        .uri("/api/v1/me")
        .header(header::AUTHORIZATION, format!("Bearer {key}"))
        .body(Body::empty())
        .unwrap();
    req.extensions_mut().insert(ConnectInfo(
        format!("{ip}:4000").parse::<SocketAddr>().unwrap(),
    ));
    let res = app.clone().oneshot(req).await.unwrap();
    let st = res.status();
    let bytes = axum::body::to_bytes(res.into_body(), 1 << 20)
        .await
        .unwrap();
    (st, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

#[test]
#[ignore = "needs DATABASE_URL (a throwaway Postgres)"]
fn admins_sign_in_only_from_where_the_limit_allows() {
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
        let (adm, mem, team) = (
            format!("usr_adm{run}"),
            format!("usr_mem{run}"),
            format!("team_acl{run}"),
        );
        let email = |who: &str| format!("{who}{run}@acl.test");
        let password = "correct horse battery";
        let hash = crate::auth::hash_password(password).await.unwrap();
        for (u, n, admin) in [(&adm, "adm", true), (&mem, "mem", false)] {
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
        for (m, u, n, role) in [("a", &adm, "adm", "owner"), ("m", &mem, "mem", "developer")] {
            sqlx::query("INSERT INTO pyo.members (id, team_id, user_id, email, role, status) VALUES ($1, $2, $3, $4, $5, 'active')")
                .bind(format!("mem_{m}{run}"))
                .bind(&team)
                .bind(u)
                .bind(email(n))
                .bind(role)
                .execute(&db)
                .await
                .unwrap();
        }
        let key = format!("pyo_acl{run}");
        sqlx::query("INSERT INTO pyo.api_tokens (id, user_id, name, prefix, token_hash, access) VALUES ($1, $2, 'ci', 'pyo_', $3, 'full')")
            .bind(format!("tok_acl{run}"))
            .bind(&adm)
            .bind(sha256_hex(&key))
            .execute(&db)
            .await
            .unwrap();
        let client = |ip: &'static str| Client {
            app: app.clone(),
            cookie: None,
            ip,
        };
        let sign_in = |who: &'static str, ip: &'static str| {
            let (mut c, e) = (client(ip), email(who));
            async move {
                let (st, body) = c
                    .call(
                        Method::POST,
                        "/api/auth/login",
                        Some(json!({ "email": e, "password": password })),
                    )
                    .await;
                (st, body, c)
            }
        };

        // Off: the admin signs in from anywhere.
        let (st, _, mut inside) = sign_in("adm", INSIDE).await;
        assert_eq!(st, StatusCode::OK);
        let (st, snap) = inside.call(Method::GET, "/api/v1/state", None).await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(snap["adminSignIn"]["here"], INSIDE);
        assert_eq!(snap["adminSignIn"]["addresses"], json!([]));

        // A limit that leaves out the admin's own address: refused.
        let (st, body) = inside
            .op(json!({ "op": "saveAdminSignIn", "addresses": ["198.51.100.0/24"] }))
            .await;
        assert_eq!(st, StatusCode::BAD_REQUEST);
        assert_eq!(
            body["error"],
            format!("This would lock you out. Add your address {INSIDE} first.")
        );
        let (st, body) = inside
            .op(json!({ "op": "saveAdminSignIn", "tailnet": true }))
            .await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "{body}");
        // One that keeps it: saved plainly.
        let (st, body) = inside
            .op(json!({ "op": "saveAdminSignIn", "addresses": ["203.0.113.9/24", "2001:db8::/48"] }))
            .await;
        assert_eq!(st, StatusCode::OK, "{body}");
        assert_eq!(body["addresses"], json!(["203.0.113.0/24", "2001:db8::/48"]));
        // A member can't change it.
        let (_, _, mut member) = sign_in("mem", INSIDE).await;
        let (st, _) = member
            .op(json!({ "op": "saveAdminSignIn", "addresses": [] }))
            .await;
        assert_eq!(st, StatusCode::FORBIDDEN);

        // From outside, the right password answers as a wrong one, and the
        // admin's teams see why.
        let (st, refused, outside) = sign_in("adm", OUTSIDE).await;
        assert_eq!(st, StatusCode::UNAUTHORIZED);
        assert!(outside.cookie.is_none());
        let mut wrong = client(OUTSIDE);
        let (_, wrong_body) = wrong
            .call(
                Method::POST,
                "/api/auth/login",
                Some(json!({ "email": email("adm"), "password": "not the password" })),
            )
            .await;
        assert_eq!(refused, wrong_body);
        let (_, nobody) = client(OUTSIDE)
            .call(
                Method::POST,
                "/api/auth/login",
                Some(json!({ "email": email("nobody"), "password": password })),
            )
            .await;
        assert_eq!(refused, nobody, "no account: the same answer");
        let refusals: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM pyo.audit_events WHERE team_id = $1 AND action = 'auth.sign_in_refused' AND outcome = 'failed' AND ip = $2",
        )
        .bind(&team)
        .bind(OUTSIDE)
        .fetch_one(&db)
        .await
        .unwrap();
        assert_eq!(refusals, 1);

        // The admin's session from outside: as if signed out. Inside, it
        // still works.
        let mut stolen = Client {
            app: app.clone(),
            cookie: inside.cookie.clone(),
            ip: OUTSIDE,
        };
        let (st, _) = stolen.call(Method::GET, "/api/v1/state", None).await;
        assert_eq!(st, StatusCode::UNAUTHORIZED);
        let (st, _) = stolen.call(Method::GET, "/api/auth/me", None).await;
        assert_eq!(st, StatusCode::UNAUTHORIZED);
        let (st, _) = inside.call(Method::GET, "/api/v1/state", None).await;
        assert_eq!(st, StatusCode::OK);

        // The admin's API key from outside: a member, not an admin.
        let (st, me) = me_with_key(&app, &key, OUTSIDE).await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(me["isAdmin"], false);
        let (_, me) = me_with_key(&app, &key, INSIDE).await;
        assert_eq!(me["isAdmin"], true);

        // Everyone else, from anywhere.
        let (st, _, mut member_outside) = sign_in("mem", OUTSIDE).await;
        assert_eq!(st, StatusCode::OK);
        let (st, _) = member_outside.call(Method::GET, "/api/v1/state", None).await;
        assert_eq!(st, StatusCode::OK);

        // Off again: anywhere.
        let (st, _) = inside
            .op(json!({ "op": "saveAdminSignIn", "tailnet": false, "addresses": [] }))
            .await;
        assert_eq!(st, StatusCode::OK);
        let (st, _) = stolen.call(Method::GET, "/api/v1/state", None).await;
        assert_eq!(st, StatusCode::OK);
        let saved: bool =
            sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM pyo.settings WHERE key = 'admin_sign_in')")
                .fetch_one(&db)
                .await
                .unwrap();
        assert!(!saved);

        sqlx::query("DELETE FROM pyo.teams WHERE id = $1")
            .bind(&team)
            .execute(&db)
            .await
            .unwrap();
        sqlx::query("DELETE FROM pyo.users WHERE id = ANY ($1)")
            .bind(vec![adm.clone(), mem.clone()])
            .execute(&db)
            .await
            .unwrap();
    });
}
