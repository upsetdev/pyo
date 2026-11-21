//! Teams under row security (rls.rs), against a throwaway Postgres
//! (DATABASE_URL): signing up, creating a team, inviting and joining, and
//! deleting a team, as a person does it through the real routes.
//!
//! Each runs twice, on the two logins Pyo runs as. Pyo's built-in Postgres
//! makes its login a superuser (POSTGRES_USER), as DATABASE_URL's usually
//! is here. Pyo Cloud's owns the tables without being a superuser or
//! bypassing row security, and may only switch to pyo_team
//! (pyo-cloud's postgres-bootstrap.sql). Either way a request's transaction
//! is lowered to pyo_team, which row security holds to its teams.

use axum::{
    Router,
    http::{Method, StatusCode},
};
use serde_json::json;
use sqlx::PgPool;
use sqlx::postgres::PgConnectOptions;

use crate::ids::random_string;
use crate::teamsec_tests::{Client, run_big};

/// Pyo's tables owned by a login that's neither a superuser nor bypasses
/// row security, in a database of its own, as on Pyo Cloud. Dropped by
/// [`drop_db`].
async fn cloud_db() -> (PgPool, String) {
    let opts: PgConnectOptions = std::env::var("DATABASE_URL")
        .expect("DATABASE_URL")
        .parse()
        .unwrap();
    let admin = PgPool::connect_with(opts.clone()).await.unwrap();
    let name = format!("pyo_cloud_{}", random_string(8).to_lowercase());
    // Roles are the server's, not a database's: one test at a time makes
    // them, after the tests beside it made pyo_team (migrations).
    crate::migrate::run(&admin).await.unwrap();
    sqlx::raw_sql(
        "SELECT pg_advisory_xact_lock(4243);
         DO $$ BEGIN
           IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'pyo_team') THEN CREATE ROLE pyo_team NOLOGIN; END IF;
           IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'pyo_test_app') THEN
             CREATE ROLE pyo_test_app LOGIN NOSUPERUSER NOBYPASSRLS NOCREATEROLE PASSWORD 'pyo';
           END IF;
         END $$;
         GRANT pyo_team TO pyo_test_app WITH INHERIT FALSE, SET TRUE;",
    )
    .execute(&admin)
    .await
    .unwrap();
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
        "CREATE DATABASE {name} OWNER pyo_test_app"
    )))
    .execute(&admin)
    .await
    .unwrap();
    admin.close().await;
    let db = PgPool::connect_with(
        opts.username("pyo_test_app")
            .password("pyo")
            .database(&name),
    )
    .await
    .unwrap();
    crate::migrate::run(&db).await.unwrap();
    let (su, bypass): (bool, bool) =
        sqlx::query_as("SELECT rolsuper, rolbypassrls FROM pg_roles WHERE rolname = current_user")
            .fetch_one(&db)
            .await
            .unwrap();
    assert!(!su && !bypass, "the owner isn't let past row security");
    (db, name)
}

async fn drop_db(db: PgPool, name: &str) {
    db.close().await;
    let admin = PgPool::connect(&std::env::var("DATABASE_URL").unwrap())
        .await
        .unwrap();
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
        "DROP DATABASE {name} WITH (FORCE)"
    )))
    .execute(&admin)
    .await
    .unwrap();
}

async fn installation_db() -> PgPool {
    let db = PgPool::connect(&std::env::var("DATABASE_URL").expect("DATABASE_URL"))
        .await
        .unwrap();
    crate::migrate::run(&db).await.unwrap();
    db
}

fn app(db: &PgPool) -> Router {
    let mut state = crate::sandbox::e2e::test_state(db.clone());
    let mut config = (*state.config).clone();
    config.allow_signup = true;
    state.config = std::sync::Arc::new(config);
    Router::new()
        .nest("/api/auth", crate::auth::routes())
        .nest("/api/v1", crate::ops::routes())
        .layer(axum::middleware::from_fn(crate::audit::middleware))
        .with_state(state)
}

/// A request's transaction is the restricted role, not the login.
async fn lowered_role_is_held_back(db: &PgPool) {
    let user = crate::auth::User {
        id: "usr_nobody".into(),
        name: "n".into(),
        email: "n@n.test".into(),
        timezone: "UTC".into(),
        is_admin: false,
        must_set_up: false,
        key: None,
    };
    let mut tx = crate::rls::begin(db, &user).await.unwrap();
    let (who, su, bypass): (String, bool, bool) = sqlx::query_as(
        "SELECT current_user::text, rolsuper, rolbypassrls FROM pg_roles WHERE rolname = current_user",
    )
    .fetch_one(&mut *tx)
    .await
    .unwrap();
    assert_eq!(
        (who.as_str(), su, bypass),
        ("pyo_team", false, false),
        "lowered to pyo_team"
    );
    tx.rollback().await.unwrap();
}

async fn sign_up(c: &mut Client, name: &str, email: &str, invite: Option<&str>) {
    let (st, body) = c
        .call(
            Method::POST,
            "/api/auth/signup",
            Some(json!({ "name": name, "email": email, "password": "correct horse battery", "invite": invite })),
        )
        .await;
    assert_eq!(st, StatusCode::OK, "{name} signs up: {body}");
}

async fn user_id(db: &PgPool, email: &str) -> String {
    sqlx::query_scalar("SELECT id FROM pyo.users WHERE email = $1")
        .bind(email)
        .fetch_one(db)
        .await
        .unwrap()
}

/// Each team the account is an active member of, with its role.
async fn memberships(db: &PgPool, user: &str) -> Vec<(String, String)> {
    sqlx::query_as(
        "SELECT team_id, role FROM pyo.members WHERE user_id = $1 AND status = 'active' ORDER BY joined_at, team_id",
    )
    .bind(user)
    .fetch_all(db)
    .await
    .unwrap()
}

fn invite_token(res: &serde_json::Value) -> String {
    res["link"]
        .as_str()
        .and_then(|l| l.split("invite=").nth(1))
        .unwrap_or_else(|| panic!("an invite link: {res}"))
        .to_owned()
}

async fn teams_as_a_person(db: PgPool) {
    lowered_role_is_held_back(&db).await;
    let app = app(&db);
    let run = random_string(8).to_lowercase();
    let email = |who: &str| format!("{who}{run}@teams.test");
    let client = |ip| Client {
        app: app.clone(),
        cookie: None,
        ip,
    };
    let (mut ada, mut bob, mut dee) = (
        client("203.0.113.21"),
        client("203.0.113.22"),
        client("203.0.113.23"),
    );

    // Signing up without an invite: a team of their own.
    sign_up(&mut ada, "Ada", &email("ada"), None).await;
    let ada_id = user_id(&db, &email("ada")).await;
    let own = memberships(&db, &ada_id).await;
    assert_eq!(own.len(), 1, "{own:?}");
    assert_eq!(own[0].1, "owner");

    // Creating a team: theirs, as its owner, with its activity.
    let shop = format!("team_shop{run}");
    let (st, res) = ada
        .op(json!({ "op": "createTeam", "id": shop, "name": format!("Shop {run}") }))
        .await;
    assert_eq!(st, StatusCode::OK, "createTeam: {res}");
    assert!(
        memberships(&db, &ada_id)
            .await
            .contains(&(shop.clone(), "owner".into()))
    );
    let created: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM pyo.activity WHERE team_id = $1 AND data->>'type' = 'team.created'",
    )
    .bind(&shop)
    .fetch_one(&db)
    .await
    .unwrap();
    assert_eq!(created, 1, "the new team's activity");
    // And works in it right away.
    let (st, res) = ada
        .op(json!({ "op": "updateTeam", "id": shop, "description": "the shop" }))
        .await;
    assert_eq!(st, StatusCode::OK, "updateTeam: {res}");

    // Inviting someone without an account: they join as they sign up.
    let (st, res) = ada
        .op(json!({ "op": "inviteMember", "id": format!("mem_bob{run}"), "teamId": shop, "email": email("bob"), "role": "developer" }))
        .await;
    assert_eq!(st, StatusCode::OK, "inviteMember: {res}");
    sign_up(&mut bob, "Bob", &email("bob"), Some(&invite_token(&res))).await;
    let bob_id = user_id(&db, &email("bob")).await;
    assert_eq!(
        memberships(&db, &bob_id).await,
        vec![(shop.clone(), "developer".into())],
        "only the team they were invited to"
    );

    // Someone who signed up meanwhile accepts the link signed in.
    let (st, res) = ada
        .op(json!({ "op": "inviteMember", "id": format!("mem_dee{run}"), "teamId": shop, "email": email("dee"), "role": "viewer" }))
        .await;
    assert_eq!(st, StatusCode::OK, "inviteMember: {res}");
    sign_up(&mut dee, "Dee", &email("dee"), None).await;
    let (st, res) = dee
        .call(
            Method::POST,
            "/api/auth/invite/accept",
            Some(json!({ "invite": invite_token(&res) })),
        )
        .await;
    assert_eq!(st, StatusCode::OK, "accept: {res}");
    let dee_id = user_id(&db, &email("dee")).await;
    assert!(
        memberships(&db, &dee_id)
            .await
            .contains(&(shop.clone(), "viewer".into()))
    );

    // A member creates a team of their own too.
    let bobs = format!("team_bob{run}");
    let (st, res) = bob
        .op(json!({ "op": "createTeam", "id": bobs, "name": format!("Bob {run}") }))
        .await;
    assert_eq!(st, StatusCode::OK, "Bob's createTeam: {res}");

    // A team that exists can't be made again, nor joined that way.
    let (st, res) = bob
        .op(json!({ "op": "createTeam", "id": own[0].0, "name": "Mine now" }))
        .await;
    assert_eq!(st, StatusCode::CONFLICT, "{res}");
    assert!(
        !memberships(&db, &bob_id)
            .await
            .iter()
            .any(|(t, _)| *t == own[0].0)
    );
    // Nor is anyone added outside the scope to a team that has members.
    let bob_user = crate::auth::User {
        id: bob_id.clone(),
        name: "Bob".into(),
        email: email("bob"),
        timezone: "UTC".into(),
        is_admin: false,
        must_set_up: false,
        key: None,
    };
    let mut tx = crate::rls::begin(&db, &bob_user).await.unwrap();
    let e = crate::ops::add_first_owner(&mut tx, &own[0].0, &bob_user)
        .await
        .unwrap_err();
    assert_eq!(e.status, StatusCode::CONFLICT);
    // And the transaction is back in Bob's teams afterwards.
    let (who,): (String,) = sqlx::query_as("SELECT current_user::text")
        .fetch_one(&mut *tx)
        .await
        .unwrap();
    assert_eq!(who, "pyo_team");
    let e = sqlx::query("INSERT INTO pyo.members (id, team_id, user_id, email, role, status) VALUES ($1, $2, $3, $4, 'owner', 'active')")
        .bind(format!("mem_x{run}"))
        .bind(&own[0].0)
        .bind(&bob_id)
        .bind(email("bob"))
        .execute(&mut *tx)
        .await
        .unwrap_err();
    assert_eq!(
        e.as_database_error().and_then(|d| d.code()).as_deref(),
        Some("42501"),
        "{e}"
    );
    tx.rollback().await.unwrap();

    // Deleting a team: it goes, with its members.
    let (st, res) = ada.op(json!({ "op": "deleteTeam", "id": shop })).await;
    assert_eq!(st, StatusCode::OK, "deleteTeam: {res}");
    let left: i64 = sqlx::query_scalar("SELECT count(*) FROM pyo.members WHERE team_id = $1")
        .bind(&shop)
        .fetch_one(&db)
        .await
        .unwrap();
    assert_eq!(left, 0);
    assert_eq!(memberships(&db, &ada_id).await, own);

    for u in [&ada_id, &bob_id, &dee_id] {
        let teams: Vec<String> = sqlx::query_scalar(
            "DELETE FROM pyo.members WHERE user_id = $1 AND role = 'owner' RETURNING team_id",
        )
        .bind(u)
        .fetch_all(&db)
        .await
        .unwrap();
        sqlx::query("DELETE FROM pyo.teams WHERE id = ANY($1)")
            .bind(&teams)
            .execute(&db)
            .await
            .unwrap();
        sqlx::query("DELETE FROM pyo.users WHERE id = $1")
            .bind(u)
            .execute(&db)
            .await
            .unwrap();
    }
}

#[test]
#[ignore = "needs DATABASE_URL (a throwaway Postgres)"]
fn create_team_as_a_normal_user() {
    run_big(async {
        teams_as_a_person(installation_db().await).await;
    });
}

#[test]
#[ignore = "needs DATABASE_URL (a throwaway Postgres)"]
fn create_team_as_a_normal_user_on_a_login_without_superuser() {
    run_big(async {
        let (db, name) = cloud_db().await;
        teams_as_a_person(db.clone()).await;
        drop_db(db, &name).await;
    });
}

/// An account that must set up two-factor first (the installation
/// requires it) has no teams in its scope, a new one included: it's told
/// so, and nothing is created. In a database of its own, as the rule
/// applies to the whole installation.
#[test]
#[ignore = "needs DATABASE_URL (a throwaway Postgres)"]
fn create_team_waits_for_required_two_factor() {
    run_big(async {
        let (db, name) = cloud_db().await;
        let run = random_string(8).to_lowercase();
        let user = crate::auth::User {
            id: format!("usr_{run}"),
            name: "Eve".into(),
            email: format!("eve{run}@teams.test"),
            timezone: "UTC".into(),
            is_admin: false,
            must_set_up: false,
            key: None,
        };
        sqlx::query(
            "INSERT INTO pyo.users (id, email, name, password_hash) VALUES ($1, $2, 'Eve', '')",
        )
        .bind(&user.id)
        .bind(&user.email)
        .execute(&db)
        .await
        .unwrap();
        sqlx::query("INSERT INTO pyo.settings (key, value) VALUES ('security', '{\"requireTwoFactor\": true}')")
            .execute(&db)
            .await
            .unwrap();
        let s = crate::sandbox::e2e::test_state(db.clone());
        let team = format!("team_{run}");
        let e = crate::ops::run_json(
            &s,
            &user,
            json!({ "op": "createTeam", "id": team, "name": "Eve's" }),
        )
        .await
        .unwrap_err();
        assert_eq!(e.status, StatusCode::FORBIDDEN, "{}", e.message);
        let made: bool =
            sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM pyo.teams WHERE id = $1)")
                .bind(&team)
                .fetch_one(&db)
                .await
                .unwrap();
        assert!(!made);
        drop_db(db, &name).await;
    });
}
