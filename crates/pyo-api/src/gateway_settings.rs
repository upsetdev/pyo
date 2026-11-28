//! A cluster's Gateway section (docs/traffic-and-gateway-design.md §10 to
//! §15): the gateway's parts as the agent reports them (its heartbeat's
//! `capabilities.gateway`), and a few cluster-wide settings.
//!
//! - **Listener settings**, HTTP/3 and the TLS minimum: the agent writes
//!   them into the client policies (`SetWebSettings`). HTTP/3 on or off
//!   replaces ServiceLB's pod on each server (one cut per server), and the
//!   TLS minimum changes every HTTPS filter chain (every HTTPS connection
//!   drains once). The answer's `drops` says so.
//! - **Route settings**, compression and the timeout defaults: each service's
//!   routes carry them (routes.rs `current` puts them in when a command is
//!   handed out). Changing one sends every running service's routes again,
//!   routes only: no connection drops.

use serde_json::{Value, json};
use sqlx::{Postgres, Transaction};

use crate::{
    auth::User,
    cloudflare::tunnel_ops::{INSTALL_FIRST, INSTALL_UPDATING},
    error::{ApiError, ApiResult},
    ops::{Role, log_activity, require_role, team_of},
};

pub const REQUEST_RANGE: &str = "The request timeout is 1 to 3600 seconds, or 0 for no limit.";
pub const IDLE_RANGE: &str = "The idle timeout is 60 to 86400 seconds (1 minute to 24 hours).";
pub const IDLE_SHORT: &str = "The idle timeout must be at least the request timeout.";
pub const TLS_VERSIONS: &str = "The minimum TLS version is 1.2 or 1.3.";
pub const PUBLIC_FIRST: &str = "Turn on public ports first. HTTP/3 needs them.";
pub const AGENT_FIRST: &str = "Update this cluster's agent first.";

/// What `setClusterGateway` asks: unset fields stay as they are.
#[derive(Debug, Default, Clone)]
pub struct Ask {
    pub http3: Option<bool>,
    pub compression: Option<bool>,
    pub tls_min: Option<String>,
    pub request_timeout: Option<u32>,
    pub idle_timeout: Option<u32>,
}

/// A cluster's gateway settings, as stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settings {
    pub http3: bool,
    pub compression: bool,
    pub tls_min: String,
    pub request_timeout: u32,
    pub idle_timeout: u32,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            http3: false,
            compression: false,
            tls_min: "1.2".into(),
            request_timeout: pyo_proto::system::REQUEST_TIMEOUT_SECS,
            idle_timeout: pyo_proto::system::IDLE_TIMEOUT_SECS,
        }
    }
}

impl Settings {
    /// These settings with `ask`'s, checked. Plain words on what's wrong.
    pub fn with(&self, ask: &Ask) -> Result<Settings, &'static str> {
        let tls_min = match ask.tls_min.as_deref().map(str::trim) {
            None => self.tls_min.clone(),
            Some(v) if pyo_proto::system::TLS_MIN_VERSIONS.contains(&v) => v.to_owned(),
            Some(_) => return Err(TLS_VERSIONS),
        };
        let request_timeout = ask.request_timeout.unwrap_or(self.request_timeout);
        if request_timeout > pyo_proto::system::MAX_REQUEST_TIMEOUT_SECS {
            return Err(REQUEST_RANGE);
        }
        let idle_timeout = ask.idle_timeout.unwrap_or(self.idle_timeout);
        if !(pyo_proto::system::MIN_IDLE_TIMEOUT_SECS..=pyo_proto::system::MAX_IDLE_TIMEOUT_SECS).contains(&idle_timeout) {
            return Err(IDLE_RANGE);
        }
        // A request may take its whole timeout: its stream isn't closed for
        // being quiet before then. No limit (0) has nothing to compare.
        if request_timeout > 0 && idle_timeout < request_timeout {
            return Err(IDLE_SHORT);
        }
        Ok(Settings {
            http3: ask.http3.unwrap_or(self.http3),
            compression: ask.compression.unwrap_or(self.compression),
            tls_min,
            request_timeout,
            idle_timeout,
        })
    }

    /// Whether saving `to` drops open connections once: HTTP/3 (ServiceLB's
    /// pod on each server is replaced) or the TLS minimum (every HTTPS
    /// filter chain changes). The rest changes routes only.
    pub fn drops(&self, to: &Settings) -> bool {
        self.http3 != to.http3 || self.tls_min != to.tls_min
    }

    /// Whether services' routes change: compression or a timeout default.
    pub fn routes_change(&self, to: &Settings) -> bool {
        self.compression != to.compression
            || self.request_timeout != to.request_timeout
            || self.idle_timeout != to.idle_timeout
    }

    /// Whether the agent needs the Gateway settings for this change (all but
    /// the request timeout default, which agents have always written).
    pub fn needs_agent(&self, to: &Settings) -> bool {
        self.drops(to) || self.compression != to.compression || self.idle_timeout != to.idle_timeout
    }

    pub fn json(&self) -> Value {
        json!({ "http3": self.http3, "compression": self.compression, "tlsMin": self.tls_min,
                "requestTimeout": self.request_timeout, "idleTimeout": self.idle_timeout })
    }

    /// What changed, for the activity log: "HTTP/3 on, compression off".
    pub fn said(&self, to: &Settings) -> Vec<String> {
        let on = |b: bool| if b { "on" } else { "off" };
        let mut out = vec![];
        if self.http3 != to.http3 {
            out.push(format!("HTTP/3 {}", on(to.http3)));
        }
        if self.compression != to.compression {
            out.push(format!("compression {}", on(to.compression)));
        }
        if self.tls_min != to.tls_min {
            out.push(format!("TLS minimum {}", to.tls_min));
        }
        if self.request_timeout != to.request_timeout {
            out.push(match to.request_timeout {
                0 => "no request timeout by default".into(),
                n => format!("request timeout {n} s by default"),
            });
        }
        if self.idle_timeout != to.idle_timeout {
            out.push(format!("idle timeout {} s by default", to.idle_timeout));
        }
        out
    }
}

/// Whether the cluster's agent and install can take the Gateway settings:
/// the agent says it writes them, and its install is this Pyo's (the new
/// admission rules come with the connect command).
pub fn agent_ready(capabilities: &Value) -> Result<(), &'static str> {
    if capabilities["gatewaySettings"] != json!(true) {
        return Err(AGENT_FIRST);
    }
    match capabilities["agentInstall"]["state"].as_str() {
        Some("current") | None => Ok(()),
        Some("updating") => Err(INSTALL_UPDATING),
        _ => Err(INSTALL_FIRST),
    }
}

/// Why HTTP/3 can't go on: another Service publishes UDP port 443 on the
/// servers, as the agent last reported.
pub fn udp443_taken(capabilities: &Value) -> Option<String> {
    capabilities["gateway"]["udp443Taken"]
        .as_str()
        .filter(|s| !s.is_empty())
        .map(|by| format!("UDP port 443 is in use on the servers by {by}. Free it first."))
}

#[derive(sqlx::FromRow)]
struct Row {
    name: String,
    status: String,
    traffic: String,
    capabilities: Value,
    http3: bool,
    compression: bool,
    tls_min: String,
    request_timeout: i32,
    idle_timeout: i32,
}

fn bad(m: impl Into<String>) -> ApiError {
    ApiError::bad_request(m.into())
}

/// `setClusterGateway`.
pub async fn set(
    tx: &mut Transaction<'_, Postgres>,
    user: &User,
    cluster_id: &str,
    ask: Ask,
    wake: &mut Vec<String>,
) -> ApiResult<Value> {
    let team_id = team_of(tx, "clusters", cluster_id).await?;
    require_role(tx, user, &team_id, Role::Admin).await?;
    let row: Row = sqlx::query_as(
        "SELECT name, status, traffic, coalesce(capabilities, '{}') AS capabilities, http3, compression, tls_min, request_timeout, idle_timeout
           FROM pyo.clusters WHERE id = $1 FOR UPDATE",
    )
    .bind(cluster_id)
    .fetch_one(&mut **tx)
    .await?;
    let now = Settings {
        http3: row.http3,
        compression: row.compression,
        tls_min: row.tls_min.clone(),
        request_timeout: u32::try_from(row.request_timeout).unwrap_or(pyo_proto::system::REQUEST_TIMEOUT_SECS),
        idle_timeout: u32::try_from(row.idle_timeout).unwrap_or(pyo_proto::system::IDLE_TIMEOUT_SECS),
    };
    let want = now.with(&ask).map_err(bad)?;
    if want == now {
        return Ok(json!({ "ok": true, "drops": false, "settings": now.json() }));
    }
    if now.needs_agent(&want) {
        agent_ready(&row.capabilities).map_err(bad)?;
    }
    if now.drops(&want) && row.status != "connected" && row.status != "degraded" {
        return Err(ApiError::conflict(format!("{}'s agent isn't connected. Try again once it is.", row.name)));
    }
    if want.http3 && !now.http3 {
        if row.traffic != "public" {
            return Err(bad(PUBLIC_FIRST));
        }
        if let Some(why) = udp443_taken(&row.capabilities) {
            return Err(ApiError::conflict(why));
        }
    }
    sqlx::query(
        "UPDATE pyo.clusters SET http3 = $2, compression = $3, tls_min = $4, request_timeout = $5, idle_timeout = $6 WHERE id = $1",
    )
    .bind(cluster_id)
    .bind(want.http3)
    .bind(want.compression)
    .bind(&want.tls_min)
    .bind(want.request_timeout as i32)
    .bind(want.idle_timeout as i32)
    .execute(&mut **tx)
    .await?;
    if now.drops(&want) {
        crate::deploy::enqueue(
            tx,
            cluster_id,
            pyo_proto::Command::SetWebSettings {
                http3: (now.http3 != want.http3).then_some(want.http3),
                tls13: (now.tls_min != want.tls_min).then_some(want.tls_min == "1.3"),
            },
        )
        .await?;
    }
    // Every running service's routes again, with the new defaults (put in
    // when the sync is handed out, routes.rs `current`): routes only.
    if now.routes_change(&want) {
        sqlx::query("UPDATE pyo.services SET routes_revision = routes_revision + 1 WHERE cluster_id = $1 AND NOT deleting")
            .bind(cluster_id)
            .execute(&mut **tx)
            .await?;
        let any: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM pyo.services WHERE cluster_id = $1 AND current_deployment_id IS NOT NULL AND NOT deleting)",
        )
        .bind(cluster_id)
        .fetch_one(&mut **tx)
        .await?;
        if any {
            crate::firewall::sync(tx, cluster_id).await?;
        }
    }
    log_activity(
        tx,
        &team_id,
        &user.name,
        "cluster.updated",
        "info",
        &format!("Changed {}'s gateway: {}", row.name, now.said(&want).join(", ")),
        json!({ "kind": "cluster", "id": cluster_id, "label": row.name }),
        json!({ "clusterId": cluster_id }),
    )
    .await?;
    wake.push(cluster_id.to_owned());
    Ok(json!({ "ok": true, "drops": now.drops(&want), "settings": want.json() }))
}

/// Against a throwaway Postgres: `setClusterGateway` and the migration.
#[cfg(test)]
mod db_tests {
    use serde_json::{Value, json};

    /// The migration gives existing clusters today's gateway. A viewer
    /// can't change it; an agent before the section, or an install not
    /// refreshed, takes only the request timeout default. The listener
    /// settings queue `SetWebSettings`; the route settings reach every
    /// service's routes when they're handed out, after a firewall sync.
    #[test]
    #[ignore = "needs DATABASE_URL (a throwaway Postgres)"]
    fn the_gateway_settings_end_to_end() {
        crate::teamsec_tests::run_big(async {
            let db = sqlx::PgPool::connect(&std::env::var("DATABASE_URL").expect("DATABASE_URL"))
                .await
                .unwrap();
            crate::migrate::run(&db).await.unwrap();
            let s = crate::rls::tests::state(db.clone());
            let run = crate::ids::random_string(8).to_lowercase();
            let id = |p: &str| format!("{p}_gw{run}");
            let (team, cluster, project, svc, dpl) = (id("team"), id("cl"), id("prj"), id("svc"), id("dpl"));
            let q = |sql: &'static str| sqlx::query(sql);
            q("INSERT INTO pyo.teams (id, name, slug) VALUES ($1, $1, $1)").bind(&team).execute(&db).await.unwrap();
            let mut users = vec![];
            for (who, role) in [("ada", "admin"), ("bob", "viewer")] {
                let usr = id(&format!("usr{who}"));
                let email = format!("{who}{run}@example.com");
                q("INSERT INTO pyo.users (id, email, name, password_hash) VALUES ($1, $2, $3, 'x')")
                    .bind(&usr).bind(&email).bind(who).execute(&db).await.unwrap();
                q("INSERT INTO pyo.members (id, team_id, user_id, email, role, status) VALUES ($1, $2, $3, $4, $5, 'active')")
                    .bind(id(&format!("mem{who}"))).bind(&team).bind(&usr).bind(&email).bind(role).execute(&db).await.unwrap();
                users.push(crate::auth::User {
                    id: usr,
                    name: who.into(),
                    email,
                    timezone: "UTC".into(),
                    is_admin: false,
                    must_set_up: false,
                    key: None,
                });
            }
            let (admin, viewer) = (&users[0], &users[1]);
            // An agent from before the section.
            q("INSERT INTO pyo.clusters (id, team_id, name, agent_token_hash, status, last_seen_at, traffic, capabilities)
               VALUES ($1, $2, 'eu', $1, 'connected', now(), 'public', '{\"agentInstall\": {\"state\": \"current\"}}')")
                .bind(&cluster).bind(&team).execute(&db).await.unwrap();
            q("INSERT INTO pyo.projects (id, team_id, name, namespace) VALUES ($1, $2, 'shop', $1)").bind(&project).bind(&team).execute(&db).await.unwrap();
            let domains = json!([{ "id": "dom_shop", "hostname": format!("shop-{run}.example.com"), "portName": "http", "tlsMode": "auto", "status": "active" }]);
            q("INSERT INTO pyo.services (id, team_id, project_id, cluster_id, name, spec, status) VALUES ($1, $2, $3, $4, 'web', jsonb_build_object('domains', $5::jsonb), 'running')")
                .bind(&svc).bind(&team).bind(&project).bind(&cluster).bind(&domains).execute(&db).await.unwrap();
            let service = pyo_proto::ServiceRef { service_id: svc.clone(), name: "web".into(), namespace: project.clone(), workload_kind: "Deployment".into() };
            let spec = json!({ "service": service, "projectId": project, "ports": [{ "name": "http", "containerPort": 80, "protocol": "HTTP", "exposure": "http", "publicPort": null }] });
            q("INSERT INTO pyo.deployments (id, team_id, project_id, service_id, cluster_id, status, data, spec) VALUES ($1, $2, $3, $4, $5, 'running', '{}', $6)")
                .bind(&dpl).bind(&team).bind(&project).bind(&svc).bind(&cluster).bind(&spec).execute(&db).await.unwrap();
            q("UPDATE pyo.services SET current_deployment_id = $2 WHERE id = $1").bind(&svc).bind(&dpl).execute(&db).await.unwrap();

            let gw = |body: Value| {
                let mut b = body;
                b["op"] = json!("setClusterGateway");
                b["clusterId"] = json!(cluster);
                b
            };
            let stored = |db: sqlx::PgPool, c: String| async move {
                sqlx::query_as::<_, (bool, bool, String, i32, i32)>(
                    "SELECT http3, compression, tls_min, request_timeout, idle_timeout FROM pyo.clusters WHERE id = $1",
                )
                .bind(c)
                .fetch_one(&db)
                .await
                .unwrap()
            };
            let commands = |db: sqlx::PgPool, c: String| async move {
                let rows: Vec<Value> = sqlx::query_scalar("SELECT command FROM pyo.commands WHERE cluster_id = $1 AND status = 'pending' ORDER BY created_at")
                    .bind(&c)
                    .fetch_all(&db)
                    .await
                    .unwrap();
                // This cluster's only: the other tests share the database.
                sqlx::query("UPDATE pyo.commands SET status = 'done' WHERE cluster_id = $1 AND status = 'pending'")
                    .bind(&c)
                    .execute(&db)
                    .await
                    .unwrap();
                rows
            };
            let handed = |db: sqlx::PgPool, svc: String| async move {
                let mut command = json!({ "kind": "sync_routes", "service": { "serviceId": svc } });
                crate::routes::refresh(&db, &mut command).await.unwrap();
                command
            };
            commands(db.clone(), cluster.clone()).await;

            // Today's gateway, and routes handed out as before.
            assert_eq!(stored(db.clone(), cluster.clone()).await, (false, false, "1.2".into(), 300, 3600));
            let h = handed(db.clone(), svc.clone()).await;
            assert_eq!((&h["requestTimeout"], &h["compression"], &h["idleTimeout"]), (&Value::Null, &json!(false), &Value::Null));
            let snap = crate::snapshot::build(&db, admin, "").await.unwrap();
            let c = snap["clusters"].as_array().unwrap().iter().find(|c| c["id"] == json!(cluster)).unwrap().clone();
            assert_eq!(c["gateway"]["settings"], json!({ "http3": false, "compression": false, "tlsMin": "1.2", "requestTimeout": 300, "idleTimeout": 3600 }));
            assert_eq!(c["gateway"]["status"], Value::Null);
            assert_eq!(c["capabilities"]["gatewaySettings"], json!(false));

            // A viewer can't.
            let e = crate::ops::run_json(&s, viewer, gw(json!({ "compression": true }))).await.unwrap_err();
            assert_eq!(e.status, axum::http::StatusCode::FORBIDDEN);
            // An agent before the section: only the request timeout.
            let e = crate::ops::run_json(&s, admin, gw(json!({ "compression": true }))).await.unwrap_err();
            assert_eq!(e.message, super::AGENT_FIRST);
            let r = crate::ops::run_json(&s, admin, gw(json!({ "requestTimeout": 60 }))).await.unwrap();
            assert_eq!(r["drops"], json!(false));
            let sent = commands(db.clone(), cluster.clone()).await;
            assert_eq!(sent.iter().map(|c| c["kind"].as_str().unwrap()).collect::<Vec<_>>(), ["sync_firewall"]);
            assert_eq!(handed(db.clone(), svc.clone()).await["requestTimeout"], json!(60));
            // A service's own wins.
            q("UPDATE pyo.services SET spec = spec || '{\"requestTimeout\": 0}' WHERE id = $1").bind(&svc).execute(&db).await.unwrap();
            assert_eq!(handed(db.clone(), svc.clone()).await["requestTimeout"], json!(0));
            q("UPDATE pyo.services SET spec = spec - 'requestTimeout' WHERE id = $1").bind(&svc).execute(&db).await.unwrap();

            // The section's agent, with its install not refreshed: refused.
            q("UPDATE pyo.clusters SET capabilities = '{\"gatewaySettings\": true, \"agentInstall\": {\"state\": \"outdated\"}}' WHERE id = $1")
                .bind(&cluster).execute(&db).await.unwrap();
            let e = crate::ops::run_json(&s, admin, gw(json!({ "tlsMin": "1.3" }))).await.unwrap_err();
            assert_eq!(e.message, crate::cloudflare::tunnel_ops::INSTALL_FIRST);
            q("UPDATE pyo.clusters SET capabilities = '{\"gatewaySettings\": true, \"agentInstall\": {\"state\": \"current\"}, \"gateway\": {\"udp443Taken\": \"games/quic\"}}' WHERE id = $1")
                .bind(&cluster).execute(&db).await.unwrap();

            // Route settings: routes only, every service's again.
            let r = crate::ops::run_json(&s, admin, gw(json!({ "compression": true, "idleTimeout": 7200 }))).await.unwrap();
            assert_eq!(r["drops"], json!(false));
            let sent = commands(db.clone(), cluster.clone()).await;
            assert_eq!(sent.iter().map(|c| c["kind"].as_str().unwrap()).collect::<Vec<_>>(), ["sync_firewall"]);
            let h = handed(db.clone(), svc.clone()).await;
            assert_eq!((&h["compression"], &h["idleTimeout"]), (&json!(true), &json!(7200)));
            // Checked in plain words.
            let e = crate::ops::run_json(&s, admin, gw(json!({ "idleTimeout": 30 }))).await.unwrap_err();
            assert_eq!(e.message, super::IDLE_RANGE);
            let e = crate::ops::run_json(&s, admin, gw(json!({ "tlsMin": "1.0" }))).await.unwrap_err();
            assert_eq!(e.message, super::TLS_VERSIONS);

            // HTTP/3 waits while another Service has UDP 443.
            let e = crate::ops::run_json(&s, admin, gw(json!({ "http3": true }))).await.unwrap_err();
            assert_eq!(e.message, "UDP port 443 is in use on the servers by games/quic. Free it first.");
            q("UPDATE pyo.clusters SET capabilities = capabilities - 'gateway' WHERE id = $1").bind(&cluster).execute(&db).await.unwrap();
            // And for public ports.
            q("UPDATE pyo.clusters SET traffic = 'tailscale' WHERE id = $1").bind(&cluster).execute(&db).await.unwrap();
            let e = crate::ops::run_json(&s, admin, gw(json!({ "http3": true }))).await.unwrap_err();
            assert_eq!(e.message, super::PUBLIC_FIRST);
            q("UPDATE pyo.clusters SET traffic = 'public' WHERE id = $1").bind(&cluster).execute(&db).await.unwrap();

            // Listener settings: the agent's command, and the answer says
            // open connections drop.
            let r = crate::ops::run_json(&s, admin, gw(json!({ "http3": true, "tlsMin": "1.3" }))).await.unwrap();
            assert_eq!(r["drops"], json!(true));
            let sent = commands(db.clone(), cluster.clone()).await;
            assert_eq!(sent, [json!({ "kind": "set_web_settings", "http3": true, "tls13": true })]);
            assert_eq!(stored(db.clone(), cluster.clone()).await, (true, true, "1.3".into(), 60, 7200));
            // Only what changed is sent.
            crate::ops::run_json(&s, admin, gw(json!({ "http3": true, "tlsMin": "1.2" }))).await.unwrap();
            assert_eq!(commands(db.clone(), cluster.clone()).await, [json!({ "kind": "set_web_settings", "tls13": false })]);
            // The same again: nothing at all.
            let r = crate::ops::run_json(&s, admin, gw(json!({ "http3": true }))).await.unwrap();
            assert_eq!(r["drops"], json!(false));
            assert!(commands(db.clone(), cluster.clone()).await.is_empty());

            let activity: Vec<String> = sqlx::query_scalar("SELECT data->>'title' FROM pyo.activity WHERE team_id = $1 ORDER BY at")
                .bind(&team)
                .fetch_all(&db)
                .await
                .unwrap();
            assert!(activity.iter().any(|t| t == "Changed eu's gateway: HTTP/3 on, TLS minimum 1.3"), "{activity:?}");
            q("DELETE FROM pyo.teams WHERE id = $1").bind(&team).execute(&db).await.unwrap();
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ask() -> Ask {
        Ask::default()
    }

    #[test]
    fn existing_clusters_keep_what_their_gateway_does() {
        // The migration's defaults are today's gateway: nothing on, 300 s
        // and 1 hour. A save that changes nothing does nothing.
        let s = Settings::default();
        assert_eq!(s.with(&ask()).unwrap(), s);
        assert!(!s.drops(&s) && !s.routes_change(&s) && !s.needs_agent(&s));
        let d = crate::routes::Defaults::of(300, false, 3600);
        assert_eq!(d, crate::routes::Defaults::default(), "routes are written as before");
        assert_eq!(d.request_timeout(None), None);
        assert_eq!(d.request_timeout(Some(30)), Some(30));
    }

    #[test]
    fn the_cluster_defaults_reach_services_without_their_own() {
        let d = crate::routes::Defaults::of(60, true, 7200);
        assert_eq!(d.request_timeout(None), Some(60));
        assert_eq!(d.request_timeout(Some(0)), Some(0), "a service's own wins, no limit too");
        assert!(d.compression);
        assert_eq!(d.idle_timeout, Some(7200));
        assert_eq!(crate::routes::Defaults::of(0, false, 3600).request_timeout(None), Some(0));
    }

    #[test]
    fn only_http3_and_the_tls_minimum_drop_connections() {
        let s = Settings::default();
        let with = |a: Ask| s.with(&a).unwrap();
        assert!(s.drops(&with(Ask { http3: Some(true), ..ask() })));
        assert!(s.drops(&with(Ask { tls_min: Some("1.3".into()), ..ask() })));
        for routes_only in [
            Ask { compression: Some(true), ..ask() },
            Ask { request_timeout: Some(60), ..ask() },
            Ask { idle_timeout: Some(7200), ..ask() },
        ] {
            let to = with(routes_only);
            assert!(!s.drops(&to), "{to:?}");
            assert!(s.routes_change(&to), "{to:?}");
        }
        // The request timeout default is written by every agent.
        assert!(!s.needs_agent(&with(Ask { request_timeout: Some(60), ..ask() })));
        assert!(s.needs_agent(&with(Ask { compression: Some(true), ..ask() })));
        assert!(s.needs_agent(&with(Ask { idle_timeout: Some(600), ..ask() })));
    }

    #[test]
    fn settings_are_checked_in_plain_words() {
        let s = Settings::default();
        let err = |a: Ask| s.with(&a).unwrap_err();
        assert_eq!(err(Ask { tls_min: Some("1.1".into()), ..ask() }), TLS_VERSIONS);
        assert_eq!(err(Ask { request_timeout: Some(3601), ..ask() }), REQUEST_RANGE);
        assert_eq!(err(Ask { idle_timeout: Some(59), ..ask() }), IDLE_RANGE);
        assert_eq!(err(Ask { idle_timeout: Some(86_401), ..ask() }), IDLE_RANGE);
        assert_eq!(err(Ask { request_timeout: Some(600), idle_timeout: Some(300), ..ask() }), IDLE_SHORT);
        // No limit has nothing to compare: any idle timeout goes.
        assert!(s.with(&Ask { request_timeout: Some(0), idle_timeout: Some(60), ..ask() }).is_ok());
        assert!(s.with(&Ask { tls_min: Some(" 1.3 ".into()), ..ask() }).is_ok());
        for m in [REQUEST_RANGE, IDLE_RANGE, IDLE_SHORT, TLS_VERSIONS, PUBLIC_FIRST, AGENT_FIRST] {
            assert!(!m.contains(';') && !m.contains(" - ") && m.ends_with('.'), "{m}");
        }
    }

    #[test]
    fn the_agent_and_its_install_come_first() {
        assert_eq!(agent_ready(&json!({})), Err(AGENT_FIRST));
        assert_eq!(agent_ready(&json!({ "gatewaySettings": true })), Ok(()));
        assert_eq!(
            agent_ready(&json!({ "gatewaySettings": true, "agentInstall": { "state": "outdated" } })),
            Err(INSTALL_FIRST)
        );
        assert_eq!(
            agent_ready(&json!({ "gatewaySettings": true, "agentInstall": { "state": "updating" } })),
            Err(INSTALL_UPDATING)
        );
        assert_eq!(
            udp443_taken(&json!({ "gateway": { "udp443Taken": "shop/game" } })).as_deref(),
            Some("UDP port 443 is in use on the servers by shop/game. Free it first.")
        );
        assert_eq!(udp443_taken(&json!({ "gateway": {} })), None);
    }

    #[test]
    fn the_activity_line_names_what_changed() {
        let s = Settings::default();
        let to = s
            .with(&Ask { http3: Some(true), compression: Some(true), tls_min: Some("1.3".into()), request_timeout: Some(0), idle_timeout: Some(7200) })
            .unwrap();
        assert_eq!(
            s.said(&to),
            ["HTTP/3 on", "compression on", "TLS minimum 1.3", "no request timeout by default", "idle timeout 7200 s by default"]
        );
    }
}
