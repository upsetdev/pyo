//! Team observability end to end, against a throwaway Postgres
//! (DATABASE_URL): the installation's defaults and limits, a team choosing
//! within them, who may change them, what reaches the cluster, notices
//! when a limit goes down, and the older ways to set request logs and
//! analytics still working. Through the real routes, as a browser.

use axum::{Router, http::Method, http::StatusCode};
use serde_json::{Value, json};
use sqlx::PgPool;

use crate::ids::random_string;
use crate::teamsec_tests::{ADMIN_SIGN_IN, Client, fresh_ip, run_big};

async fn titles(db: &PgPool, team: &str) -> Vec<String> {
    sqlx::query_scalar("SELECT data->>'title' FROM pyo.activity WHERE team_id = $1 ORDER BY at, id")
        .bind(team)
        .fetch_all(db)
        .await
        .unwrap()
}

fn team_of<'a>(snap: &'a Value, team: &str) -> &'a Value {
    snap["teams"].as_array().unwrap().iter().find(|t| t["id"] == json!(team)).expect("the team")
}

#[test]
#[ignore = "needs DATABASE_URL (a throwaway Postgres)"]
fn teams_choose_within_the_installation_limits() {
    run_big(async {
        let _turn = ADMIN_SIGN_IN.lock().await;
        let db = PgPool::connect(&std::env::var("DATABASE_URL").expect("DATABASE_URL")).await.unwrap();
        crate::migrate::run(&db).await.unwrap();
        crate::vault::init(crate::vault::Vault::new(db.clone(), crate::vault::Provider::from_env()));
        let state = crate::sandbox::e2e::test_state(db.clone());
        let obs = state.observability.clone();
        let app = Router::new()
            .nest("/api/auth", crate::auth::routes())
            .nest("/api/v1", crate::ops::routes())
            .layer(axum::middleware::from_fn(crate::audit::middleware))
            .with_state(state);
        // The installation's settings are shared by every test: put back after.
        let saved: Option<Value> = sqlx::query_scalar("SELECT value FROM pyo.settings WHERE key = 'observability'")
            .fetch_optional(&db)
            .await
            .unwrap();

        let run = random_string(8).to_lowercase();
        let id = |p: &str, t: &str| format!("{p}_{t}{run}");
        let (adm, own, dev) = (id("usr", "adm"), id("usr", "own"), id("usr", "dev"));
        let team = id("team", "obs");
        let cluster = id("cl", "obs");
        let email = |who: &str| format!("{who}{run}@teamobs.test");
        let password = "correct horse battery";
        let hash = crate::auth::hash_password(password).await.unwrap();
        for (u, n, admin) in [(&adm, "adm", true), (&own, "own", false), (&dev, "dev", false)] {
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
        sqlx::query("INSERT INTO pyo.teams (id, name, slug) VALUES ($1, $1, $1)").bind(&team).execute(&db).await.unwrap();
        for (m, u, n, role) in [("o", &own, "own", "owner"), ("d", &dev, "dev", "developer")] {
            sqlx::query("INSERT INTO pyo.members (id, team_id, user_id, email, role, status) VALUES ($1, $2, $3, $4, $5, 'active')")
                .bind(id("mem", m))
                .bind(&team)
                .bind(u)
                .bind(email(n))
                .bind(role)
                .execute(&db)
                .await
                .unwrap();
        }
        sqlx::query("INSERT INTO pyo.clusters (id, team_id, name, agent_token_hash) VALUES ($1, $2, $1, $1)")
            .bind(&cluster)
            .bind(&team)
            .execute(&db)
            .await
            .unwrap();
        let signed_in = |who: &'static str| {
            let e = email(who);
            let mut c = Client { app: app.clone(), cookie: None, ip: fresh_ip() };
            async move {
                let (st, _) = c.call(Method::POST, "/api/auth/login", Some(json!({ "email": e, "password": password }))).await;
                assert_eq!(st, StatusCode::OK, "{who} signs in");
                c
            }
        };
        let mut admin = signed_in("adm").await;
        let mut owner = signed_in("own").await;
        let mut developer = signed_in("dev").await;
        let install = |ip: &str, max_ip: &str, qs: &str, max_qs: &str| {
            json!({ "op": "saveObservabilitySettings", "ipMode": ip, "maxIpMode": max_ip, "queryStrings": qs, "maxQueryStrings": max_qs })
        };

        // The installation: no addresses by default, anonymized at most.
        let (st, out) = admin.op(install("full", "anonymized", "keep", "keep")).await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "a default above its limit: {out}");
        let (st, out) = owner.op(install("none", "full", "keep", "keep")).await;
        assert_eq!(st, StatusCode::FORBIDDEN, "not the installation's admin: {out}");
        let (st, out) = admin.op(install("none", "anonymized", "keep", "keep")).await;
        assert_eq!(st, StatusCode::OK, "{out}");
        let (_, all) = admin.call(Method::GET, "/api/v1/observability/settings", None).await;
        assert_eq!((all["ipMode"].as_str(), all["maxIpMode"].as_str()), (Some("none"), Some("anonymized")));
        assert_eq!((all["queryStrings"].as_str(), all["maxQueryStrings"].as_str()), (Some("keep"), Some("keep")));
        let (_, mine) = owner.call(Method::GET, "/api/v1/observability/settings", None).await;
        assert!(mine.get("maxIpMode").is_none(), "the installation's details are its admin's: {mine}");

        // The team follows the defaults until it chooses.
        let (_, snap) = owner.call(Method::GET, "/api/v1/state", None).await;
        let t = team_of(&snap, &team);
        assert_eq!(t["observability"], json!({ "on": true, "requestLogs": true, "ipMode": "none", "queryStrings": "keep" }));
        assert_eq!(t["observabilityLimits"], json!({ "ipMode": "anonymized", "queryStrings": "keep" }));
        assert_eq!(t["observabilityDefaults"], json!({ "ipMode": "none", "queryStrings": "keep" }));
        assert!(t["ipMode"].is_null() && t["observabilityOn"].is_null());

        // Developers and viewers can't change it.
        let set = |v: Value| {
            let mut o = json!({ "op": "setTeamObservability", "teamId": team });
            o.as_object_mut().unwrap().extend(v.as_object().unwrap().clone());
            o
        };
        let (st, _) = developer.op(set(json!({ "ipMode": "anonymized" }))).await;
        assert!(st == StatusCode::FORBIDDEN || st == StatusCode::NOT_FOUND, "{st}");

        // Owners choose within the limit.
        let (st, out) = owner.op(set(json!({ "ipMode": "full" }))).await;
        assert_eq!(st, StatusCode::BAD_REQUEST);
        assert!(out["error"].as_str().unwrap().contains("anonymized"), "{out}");
        let (st, out) = owner.op(set(json!({ "ipMode": "everything" }))).await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "{out}");
        let (st, out) = owner
            .op(set(json!({ "ipMode": "anonymized", "queryStrings": "names", "requestLogs": false, "enabled": false })))
            .await;
        assert_eq!(st, StatusCode::OK, "{out}");
        let (_, snap) = owner.call(Method::GET, "/api/v1/state", None).await;
        let t = team_of(&snap, &team);
        assert_eq!(t["observability"], json!({ "on": false, "requestLogs": false, "ipMode": "anonymized", "queryStrings": "names" }));
        assert_eq!((t["ipMode"].as_str(), t["queryStrings"].as_str()), (Some("anonymized"), Some("names")));
        assert_eq!((t["observabilityOn"].as_bool(), t["requestLogsOn"].as_bool()), (Some(false), Some(false)));
        let seen = titles(&db, &team).await;
        for title in [
            "Turned observability off",
            "Turned request logs off",
            "Visitor IP addresses: anonymized",
            "Query strings: names only",
        ] {
            assert!(seen.iter().any(|t| t == title), "{title} in {seen:?}");
        }
        // Saved again as it is: nothing new in the activity.
        let n = seen.len();
        let (st, _) = owner.op(set(json!({ "ipMode": "anonymized", "enabled": false }))).await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(titles(&db, &team).await.len(), n);
        // Its servers stop collecting at once on this replica.
        assert!(!obs.team_on(&team).await);

        // What the cluster's traffic store is told.
        let ctx = crate::totals::context(&db, &cluster, &team).await.unwrap();
        assert_eq!((ctx.ip_mode.as_str(), ctx.query_strings.as_str(), ctx.request_logs_off), ("anonymized", "names", true));
        let (st, _) = owner.op(set(json!({ "enabled": true, "requestLogs": true }))).await;
        assert_eq!(st, StatusCode::OK);
        assert!(obs.team_on(&team).await);
        let ctx = crate::totals::context(&db, &cluster, &team).await.unwrap();
        assert!(!ctx.request_logs_off);

        // A lower limit: the team keeps less at once and hears of it. Its
        // own choice stays, and comes back when the limit is raised.
        let (st, out) = admin.op(install("none", "none", "names", "names")).await;
        assert_eq!(st, StatusCode::OK, "{out}");
        let seen = titles(&db, &team).await;
        assert!(seen.iter().any(|t| t == "Visitor IP addresses are no longer stored"), "{seen:?}");
        assert!(!seen.iter().any(|t| t.starts_with("Query strings now")), "names only already: {seen:?}");
        let (_, snap) = owner.call(Method::GET, "/api/v1/state", None).await;
        let t = team_of(&snap, &team);
        assert_eq!(t["observability"]["ipMode"], "none");
        assert_eq!(t["ipMode"], "anonymized");
        let ctx = crate::totals::context(&db, &cluster, &team).await.unwrap();
        assert_eq!(ctx.ip_mode, "none");
        let (st, _) = admin.op(install("none", "full", "keep", "keep")).await;
        assert_eq!(st, StatusCode::OK);
        let (_, snap) = owner.call(Method::GET, "/api/v1/state", None).await;
        assert_eq!(team_of(&snap, &team)["observability"]["ipMode"], "anonymized");
        // Back to the default.
        let (st, _) = owner.op(set(json!({ "ipMode": "default", "queryStrings": "default" }))).await;
        assert_eq!(st, StatusCode::OK);
        let (ip, qs): (Option<String>, Option<String>) =
            sqlx::query_as("SELECT ip_mode, query_strings FROM pyo.teams WHERE id = $1").bind(&team).fetch_one(&db).await.unwrap();
        assert_eq!((ip, qs), (None, None));

        // An older dashboard saves the address setting alone: the limit
        // follows a default above it, and nothing else changes.
        let (st, _) = admin.op(json!({ "op": "saveObservabilitySettings", "ipMode": "anonymized" })).await;
        assert_eq!(st, StatusCode::OK);
        let (_, all) = admin.call(Method::GET, "/api/v1/observability/settings", None).await;
        assert_eq!((all["ipMode"].as_str(), all["maxIpMode"].as_str()), (Some("anonymized"), Some("full")));
        let (st, _) = admin.op(install("none", "none", "keep", "keep")).await;
        assert_eq!(st, StatusCode::OK);
        let (st, _) = admin.op(json!({ "op": "saveObservabilitySettings", "ipMode": "full" })).await;
        assert_eq!(st, StatusCode::OK);
        let (_, all) = admin.call(Method::GET, "/api/v1/observability/settings", None).await;
        assert_eq!((all["ipMode"].as_str(), all["maxIpMode"].as_str()), (Some("full"), Some("full")));

        // Request logs and analytics: the new op, and updateTeam's older
        // fields, which still work.
        let (st, out) = owner.op(set(json!({ "requestLogMaxMb": 50 }))).await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "{out}");
        let (st, out) = owner.op(set(json!({ "requestLogMaxDays": 32 }))).await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "{out}");
        let (st, out) = owner.op(set(json!({ "requestLogMaxMb": 2048, "requestLogMaxDays": 3, "analyticsMonths": 3 }))).await;
        assert_eq!(st, StatusCode::OK, "{out}");
        let (prefs, months): (Value, Option<i16>) =
            sqlx::query_as("SELECT preferences, analytics_months FROM pyo.teams WHERE id = $1").bind(&team).fetch_one(&db).await.unwrap();
        assert_eq!((prefs["requestLogs"].clone(), months), (json!({ "maxMb": 2048, "maxDays": 3 }), Some(3)));
        let ctx = crate::totals::context(&db, &cluster, &team).await.unwrap();
        assert_eq!((ctx.max_mb, ctx.max_days), (2048, 3));
        let (st, out) = owner
            .op(json!({ "op": "updateTeam", "id": team, "analyticsMonths": 6, "preferences": { "requestLogs": { "maxMb": 1024, "maxDays": 14 } } }))
            .await;
        assert_eq!(st, StatusCode::OK, "{out}");
        let (prefs, months): (Value, Option<i16>) =
            sqlx::query_as("SELECT preferences, analytics_months FROM pyo.teams WHERE id = $1").bind(&team).fetch_one(&db).await.unwrap();
        assert_eq!((prefs["requestLogs"].clone(), months), (json!({ "maxMb": 1024, "maxDays": 14 }), Some(6)));

        // Clean up, the installation's settings as they were.
        match saved {
            Some(v) => sqlx::query("UPDATE pyo.settings SET value = $1 WHERE key = 'observability'").bind(v).execute(&db).await.unwrap(),
            None => sqlx::query("DELETE FROM pyo.settings WHERE key = 'observability'").execute(&db).await.unwrap(),
        };
        sqlx::query("DELETE FROM pyo.teams WHERE id = $1").bind(&team).execute(&db).await.unwrap();
        for u in [&adm, &own, &dev] {
            sqlx::query("DELETE FROM pyo.users WHERE id = $1").bind(u).execute(&db).await.unwrap();
        }
    });
}
