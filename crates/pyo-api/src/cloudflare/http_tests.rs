//! The real client against the fake Cloudflare API (fake.rs): what Pyo
//! sends and reads over HTTP for a tunnel, start to end. And, against a
//! throwaway Postgres, the whole tunnel loop: a cluster's tunnel made, a
//! domain switched (both paths, then only the tunnel), switched back, a
//! deleted service's record cleaned up, and the tunnel removed in order.

use serde_json::{Value, json};

use super::fake::FakeCf;
use super::tunnel::{self, TunnelApi};
use super::tunnel_loop::{Switched, ensure_tunnel, issue, revert_record, switch_record};
use super::*;
use crate::teamsec_tests::run_big;

const TOKEN: &str = "fake-token-0123456789abcdef";
const MARK: &str = "Made by Pyo (abc123)";
const INSTALL: &str = "abc123";
const ZONE: &str = "023e105f4ecef8ad9ca31a8372d0c353";
const ACCOUNT: &str = "699d98642c564d2e855e9661899b7252";

/// The tests here that point Pyo at a fake Cloudflare run one at a time:
/// `PYO_DEV_CLOUDFLARE_API` is one for the whole process.
static FAKE_API: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// The tests here that store a Cloudflare token seal it with the vault,
/// so their run needs PYO_SECRET_KEY. From the run's environment, never set
/// here: the vault is the process's, made by whichever test starts first,
/// so a key set by one test would count only when that test came first.
fn secret_key() {
    std::env::var("PYO_SECRET_KEY").expect("PYO_SECRET_KEY: these tests seal a token with the vault");
}

async fn fake() -> (FakeCf, Http) {
    let cf = FakeCf::start(TOKEN, &[(ZONE, "example.com", ACCOUNT)]).await;
    let http = Http::at(TOKEN, &cf.base);
    (cf, http)
}

#[tokio::test]
async fn a_token_is_checked_over_http() {
    let (cf, http) = fake().await;
    let zones = inspect(&http, INSTALL).await.unwrap();
    assert_eq!(zones.len(), 1);
    assert_eq!(zones[0].account_id, ACCOUNT);
    assert_eq!(zones[0].account_name, format!("{ACCOUNT} Co"));
    assert!(zones[0].tunnel_ready(), "{zones:?}");
    // The probes leave nothing behind.
    assert!(cf.records(ZONE).is_empty());
    assert!(cf.tunnels().iter().all(|t| !t["deleted_at"].is_null()));
    // Without tunnel permission in the account: DNS only there.
    cf.state.lock().unwrap().no_tunnels = vec![ACCOUNT.into()];
    let zones = inspect(&http, INSTALL).await.unwrap();
    assert!(!zones[0].tunnel_ready());
    assert_eq!(zones[0].tunnel.missing, [tunnel::TUNNEL_EDIT]);
    // A wrong token: refused, said plainly.
    let wrong = Http::at("another-token-0123456789", &cf.base);
    assert!(matches!(inspect(&wrong, INSTALL).await, Err(CfError::Denied(_))));
}

#[tokio::test]
async fn a_check_deletes_only_this_installations_old_probes() {
    let (cf, http) = fake().await;
    // Another installation's probe, someone's own tunnel, this
    // installation's probe from a check that stopped halfway, and one from
    // a check running right now.
    cf.put_tunnel(ACCOUNT, "pyo-check-zzz999-a1b2c3", 60);
    cf.put_tunnel(ACCOUNT, "pyo-check-mine", 60);
    cf.put_tunnel(ACCOUNT, "pyo-check-abc123-a1b2c3", 60);
    cf.put_tunnel(ACCOUNT, "pyo-check-abc123-d4e5f6", 0);
    let zones = inspect(&http, INSTALL).await.unwrap();
    assert!(zones[0].tunnel_ready(), "{zones:?}");
    let gone = |name: &str| {
        cf.tunnels()
            .iter()
            .any(|t| t["name"] == name && !t["deleted_at"].is_null())
    };
    assert!(!gone("pyo-check-zzz999-a1b2c3"));
    assert!(!gone("pyo-check-mine"));
    assert!(gone("pyo-check-abc123-a1b2c3"));
    assert!(!gone("pyo-check-abc123-d4e5f6"));
    // The check's own probe is named for this installation and is gone.
    let ts = cf.tunnels();
    assert_eq!(ts.len(), 5);
    let own = ts[4]["name"].as_str().unwrap();
    assert!(own.starts_with("pyo-check-abc123-") && own.len() == 23, "{own}");
    assert!(!ts[4]["deleted_at"].is_null());
    // It only ever listed its own probes.
    assert!(
        cf.calls()
            .iter()
            .filter(|c| c.starts_with("DELETE "))
            .count()
            == 3
    );
}

#[tokio::test]
async fn a_tunnel_runs_its_course_over_http() {
    let (cf, http) = fake().await;
    let row = tunnel_loop::Row {
        cluster_id: "cl_1".into(),
        team_id: "team_1".into(),
        account_id: ACCOUNT.into(),
        ..Default::default()
    };
    // Made, with routes and a token for the agent.
    let (t, _) = ensure_tunnel(&http, &row, "abc123").await.unwrap();
    assert_eq!(t.name, "pyo-cl_1-abc123");
    let config = tunnel::ingress(&["blog.example.com".into()]);
    assert_eq!(http.put_config(ACCOUNT, &t.id, &config).await.unwrap(), 1);
    assert_eq!(http.config(ACCOUNT, &t.id).await.unwrap(), (1, config));
    let token = http.tunnel_token(ACCOUNT, &t.id).await.unwrap();
    assert!(token.starts_with("eyJ"));
    // Its certificate, issued for a CSR made here, reads as a certificate.
    let names = tunnel::cert_names(&[("blog.example.com".into(), "example.com".into())], Some("example.com")).unwrap();
    let (cert, key) = issue(&http, &names).await.unwrap();
    let parsed = crate::certs::parse(&cert.pem, &key, pyo_proto::now_ms() / 1000).expect("a certificate Pyo can use");
    assert!(parsed.hostnames.contains(&"*.example.com".to_owned()));
    assert!(parsed.not_after - parsed.not_before > 5000 * 86_400, "15 years");
    // A record by hand is in the way: asked first, then replaced and kept.
    cf.put_record(ZONE, json!({ "id": "a1", "type": "A", "name": "blog.example.com", "content": "198.51.100.9", "proxied": true, "ttl": 300, "comment": "by hand" }));
    let target = tunnel::cname_target(&t.id);
    let out = switch_record(&http, ZONE, "blog.example.com", &target, false, MARK).await.unwrap();
    assert!(matches!(out, Switched::Confirm(ref r) if r[0].content == "198.51.100.9"));
    let Switched::Done { record, previous } = switch_record(&http, ZONE, "blog.example.com", &target, true, MARK).await.unwrap() else {
        panic!("not switched")
    };
    assert_eq!(record.kind, "CNAME");
    assert!(record.proxied);
    let now = cf.records(ZONE);
    assert_eq!(now.len(), 1);
    assert_eq!(now[0]["content"], json!(target));
    // Back exactly as it was.
    revert_record(&http, ZONE, "blog.example.com", Some(&record.id), &previous, None, MARK).await.unwrap();
    let back = cf.records(ZONE);
    assert_eq!(back.len(), 1);
    for (k, v) in [("type", json!("A")), ("content", json!("198.51.100.9")), ("proxied", json!(true)), ("ttl", json!(300)), ("comment", json!("by hand"))] {
        assert_eq!(back[0][k], v, "{k}");
    }
    // Rotated, then taken apart.
    http.rotate_secret(ACCOUNT, &t.id, &tunnel::new_secret()).await.unwrap();
    assert_ne!(http.tunnel_token(ACCOUNT, &t.id).await.unwrap(), token);
    cf.set_status(&t.id, "healthy", &["FRA", "AMS"]);
    let read = http.tunnel(ACCOUNT, &t.id).await.unwrap();
    assert_eq!((read.status.as_str(), read.connections.len()), ("healthy", 2));
    // Connected: Cloudflare refuses to delete it until they're dropped.
    assert!(http.delete_tunnel(ACCOUNT, &t.id).await.is_err());
    http.drop_connections(ACCOUNT, &t.id).await.unwrap();
    http.delete_tunnel(ACCOUNT, &t.id).await.unwrap();
    assert!(http.tunnel(ACCOUNT, &t.id).await.unwrap().deleted);
    http.revoke_origin_cert(&cert.id).await.unwrap();
    assert!(cf.state.lock().unwrap().certs.iter().all(|c| c.2));
}

// ── The whole loop, against a throwaway Postgres ───────────────────────────

/// The domain as its service has it now.
async fn domain(db: &sqlx::PgPool, service: &str) -> Value {
    sqlx::query_scalar("SELECT spec->'domains'->0 FROM pyo.services WHERE id = $1")
        .bind(service)
        .fetch_one(db)
        .await
        .unwrap()
}

/// What the agent was sent, last first.
async fn commands(db: &sqlx::PgPool, cluster: &str) -> Vec<String> {
    sqlx::query_scalar("SELECT command->>'kind' FROM pyo.commands WHERE cluster_id = $1 ORDER BY created_at DESC")
        .bind(cluster)
        .fetch_all(db)
        .await
        .unwrap()
}

/// Against a throwaway Postgres and the fake Cloudflare: a cluster in
/// tunnel mode gets its tunnel, routes, certificate and the agent's token;
/// a domain switches with both paths first and only the tunnel once its
/// record moved, goes back with the record it replaced, a deleted
/// service's record is cleaned up, and the tunnel is taken apart.
#[test]
#[ignore = "needs DATABASE_URL (a throwaway Postgres) and PYO_SECRET_KEY"]
fn the_tunnel_loop_end_to_end() {
    secret_key();
    run_big(async {
        let _one = FAKE_API.lock().await;
        let cf = FakeCf::start(TOKEN, &[(ZONE, "example.com", ACCOUNT)]).await;
        // SAFETY: set while FAKE_API is held; nothing else here reads it yet.
        unsafe {
            std::env::set_var("PYO_DEV_CLOUDFLARE_API", &cf.base);
        }
        let db = sqlx::PgPool::connect(&std::env::var("DATABASE_URL").expect("DATABASE_URL"))
            .await
            .unwrap();
        crate::migrate::run(&db).await.unwrap();
        crate::vault::init(crate::vault::Vault::new(db.clone(), crate::vault::Provider::from_env()));
        let s = crate::rls::tests::state(db.clone());
        let run = crate::ids::random_string(8).to_lowercase();
        let (team, cluster, project, svc, dpl) = (
            format!("team_tun{run}"),
            format!("cl_tun{run}"),
            format!("prj_tun{run}"),
            format!("svc_tun{run}"),
            format!("dpl_tun{run}"),
        );
        let host = format!("blog-{run}.example.com");
        let q = |sql: &'static str| sqlx::query(sql);
        q("INSERT INTO pyo.teams (id, name, slug) VALUES ($1, $1, $1)").bind(&team).execute(&db).await.unwrap();
        q("INSERT INTO pyo.clusters (id, team_id, name, agent_token_hash, status, last_seen_at, traffic, capabilities)
           VALUES ($1, $2, $1, $1, 'connected', now(), 'tunnel', '{\"tunnel\": true, \"agentInstall\": {\"state\": \"current\"}}')")
            .bind(&cluster).bind(&team).execute(&db).await.unwrap();
        q("INSERT INTO pyo.projects (id, team_id, name, namespace) VALUES ($1, $2, 'shop', $1)").bind(&project).bind(&team).execute(&db).await.unwrap();
        let domains = json!([{ "id": "dom_1", "hostname": host, "portName": "http", "tlsMode": "auto", "tunnel": "switching", "status": "pending_dns" }]);
        q("INSERT INTO pyo.services (id, team_id, project_id, cluster_id, name, spec, status) VALUES ($1, $2, $3, $4, 'web', jsonb_build_object('domains', $5::jsonb), 'running')")
            .bind(&svc).bind(&team).bind(&project).bind(&cluster).bind(&domains).execute(&db).await.unwrap();
        let service = pyo_proto::ServiceRef { service_id: svc.clone(), name: "web".into(), namespace: project.clone(), workload_kind: "Deployment".into() };
        let spec = json!({ "service": service, "projectId": project, "ports": [{ "name": "http", "containerPort": 80, "protocol": "HTTP", "exposure": "http", "publicPort": null }] });
        q("INSERT INTO pyo.deployments (id, team_id, project_id, service_id, cluster_id, status, data, spec) VALUES ($1, $2, $3, $4, $5, 'running', '{}', $6)")
            .bind(&dpl).bind(&team).bind(&project).bind(&svc).bind(&cluster).bind(&spec).execute(&db).await.unwrap();
        q("UPDATE pyo.services SET current_deployment_id = $2 WHERE id = $1").bind(&svc).bind(&dpl).execute(&db).await.unwrap();
        // The team's Cloudflare, as connecting it leaves it.
        let zones = json!([{ "id": ZONE, "name": "example.com", "canEdit": true, "accountId": ACCOUNT, "accountName": "Acme", "tunnel": { "ready": true } }]);
        let sealed = crate::vault::get().seal_credential(&team, TOKEN).await.unwrap();
        q("INSERT INTO pyo.cloudflare_accounts (team_id, token_sealed, zones) VALUES ($1, $2, $3)").bind(&team).bind(&sealed).bind(&zones).execute(&db).await.unwrap();
        // Someone's record on the name, then the tunnel and the switch asked for.
        cf.put_record(ZONE, json!({ "id": format!("hand{run}"), "type": "A", "name": host, "content": "198.51.100.9", "proxied": false, "ttl": 300 }));
        q("INSERT INTO pyo.cluster_tunnels (cluster_id, team_id, account_id, tunnel_name) VALUES ($1, $2, $3, 'pending')")
            .bind(&cluster).bind(&team).bind(ACCOUNT).execute(&db).await.unwrap();
        q("INSERT INTO pyo.dns_records (service_id, domain_id, team_id, hostname, zone_id, zone_name, type, status) VALUES ($1, 'dom_1', $2, $3, $4, 'example.com', 'CNAME', 'switching')")
            .bind(&svc).bind(&team).bind(&host).bind(ZONE).execute(&db).await.unwrap();
        let record = |db: sqlx::PgPool, svc: String| async move {
            sqlx::query_as::<_, (String, String, Value)>("SELECT status, via, previous FROM pyo.dns_records WHERE service_id = $1")
                .bind(svc)
                .fetch_optional(&db)
                .await
                .unwrap()
        };

        // A pass: the tunnel, its routes and certificate; the record waits for a yes.
        tunnel_loop::pass(&s).await.unwrap();
        let tunnel_id: String = sqlx::query_scalar("SELECT tunnel_id FROM pyo.cluster_tunnels WHERE cluster_id = $1").bind(&cluster).fetch_one(&db).await.unwrap();
        let config = cf.state.lock().unwrap().configs.iter().find(|c| c.0 == tunnel_id).map(|c| c.2.clone()).expect("routes");
        assert_eq!(config["ingress"][0]["hostname"], json!(host));
        let d = domain(&db, &svc).await;
        assert_eq!((d["tlsMode"].as_str(), d["tunnel"].as_str()), (Some("custom"), Some("switching")), "the tunnel's certificate before its record moves: {d}");
        assert!(commands(&db, &cluster).await.contains(&"tunnel_set".to_owned()), "the agent's token");
        // The record waits for the agent's routes (both paths answer first).
        assert_eq!(record(db.clone(), svc.clone()).await.unwrap().0, "switching");
        let agent_done = |db: sqlx::PgPool, cluster: String| async move {
            sqlx::query("UPDATE pyo.commands SET status = 'done' WHERE cluster_id = $1")
                .bind(cluster)
                .execute(&db)
                .await
                .unwrap();
        };
        agent_done(db.clone(), cluster.clone()).await;
        tunnel_loop::pass(&s).await.unwrap();
        let (status, via, _) = record(db.clone(), svc.clone()).await.unwrap();
        assert_eq!((status.as_str(), via.as_str()), ("conflict", "direct"));
        assert_eq!(cf.records(ZONE).len(), 1, "someone's record, untouched");

        // Replace and switch: kept, then only through the tunnel.
        q("UPDATE pyo.dns_records SET status = 'switching', replace = true WHERE service_id = $1").bind(&svc).execute(&db).await.unwrap();
        tunnel_loop::pass(&s).await.unwrap();
        let (status, via, previous) = record(db.clone(), svc.clone()).await.unwrap();
        assert_eq!((status.as_str(), via.as_str()), ("created", "tunnel"));
        assert_eq!(previous[0]["content"], "198.51.100.9");
        assert_eq!(domain(&db, &svc).await["tunnel"], "on");
        let now = cf.records(ZONE);
        assert_eq!(now.len(), 1);
        assert_eq!(now[0]["content"], json!(tunnel::cname_target(&tunnel_id)));

        // Switch back with the kept record: both paths first, then it's back.
        q("UPDATE pyo.dns_records SET status = 'reverting', restore = true, routes_command = NULL WHERE service_id = $1").bind(&svc).execute(&db).await.unwrap();
        tunnel_loop::pass(&s).await.unwrap();
        assert!(record(db.clone(), svc.clone()).await.is_none());
        let d = domain(&db, &svc).await;
        assert!(d.get("tunnel").is_none() && d["tlsMode"] == "auto", "{d}");
        let back = cf.records(ZONE);
        assert_eq!((back.len(), back[0]["content"].as_str(), back[0]["ttl"].as_i64()), (1, Some("198.51.100.9"), Some(300)));

        // Switched again, then the service deleted: its CNAME goes with it.
        cf.state.lock().unwrap().records.clear();
        q("UPDATE pyo.services SET spec = jsonb_set(spec, '{domains,0,tunnel}', '\"switching\"') WHERE id = $1").bind(&svc).execute(&db).await.unwrap();
        q("INSERT INTO pyo.dns_records (service_id, domain_id, team_id, hostname, zone_id, zone_name, type, status) VALUES ($1, 'dom_1', $2, $3, $4, 'example.com', 'CNAME', 'switching')")
            .bind(&svc).bind(&team).bind(&host).bind(ZONE).execute(&db).await.unwrap();
        tunnel_loop::pass(&s).await.unwrap();
        agent_done(db.clone(), cluster.clone()).await;
        tunnel_loop::pass(&s).await.unwrap();
        assert_eq!(cf.records(ZONE).len(), 1);
        q("DELETE FROM pyo.services WHERE id = $1").bind(&svc).execute(&db).await.unwrap();
        tunnel_loop::pass(&s).await.unwrap();
        assert!(cf.records(ZONE).is_empty(), "a deleted service's tunnel record goes");

        // A default domain: its wildcard record, and a free name under it
        // goes only through the tunnel, active once the wildcard is there.
        let free = format!("web-{run}.example.com");
        let svc2 = format!("svc_tunf{run}");
        let domains = json!([{ "id": "dom_2", "hostname": free, "portName": "http", "tlsMode": "auto", "status": "pending_dns" }]);
        q("INSERT INTO pyo.services (id, team_id, project_id, cluster_id, name, spec, status) VALUES ($1, $2, $3, $4, 'web2', jsonb_build_object('domains', $5::jsonb), 'running')")
            .bind(&svc2).bind(&team).bind(&project).bind(&cluster).bind(&domains).execute(&db).await.unwrap();
        q("UPDATE pyo.cluster_tunnels SET default_domain = 'example.com', default_zone_id = $2 WHERE cluster_id = $1")
            .bind(&cluster).bind(ZONE).execute(&db).await.unwrap();
        tunnel_loop::pass(&s).await.unwrap();
        tunnel_loop::pass(&s).await.unwrap();
        let wildcard: Vec<Value> = cf.records(ZONE).into_iter().filter(|r| r["name"] == "*.example.com").collect();
        assert_eq!(wildcard.len(), 1);
        assert_eq!(wildcard[0]["content"], json!(tunnel::cname_target(&tunnel_id)));
        let d = domain(&db, &svc2).await;
        assert_eq!((d["tunnel"].as_str(), d["status"].as_str()), (Some("on"), Some("active")), "{d}");
        let config = cf.state.lock().unwrap().configs.iter().find(|c| c.0 == tunnel_id).map(|c| c.2.clone()).unwrap();
        let hosts: Vec<Value> = config["ingress"].as_array().unwrap().iter().map(|r| r["hostname"].clone()).collect();
        // The free name is routed by name. The wildcard record has no
        // wildcard route: any other name gets the tunnel's own 404.
        assert_eq!(hosts, [json!(free), Value::Null]);
        // The free name has no record of its own: the wildcard covers it.
        assert!(cf.records(ZONE).iter().all(|r| r["name"] != json!(free)));

        // Rotated while the agent runs the tunnel: its heartbeat keeps
        // reporting the same tunnel id, so the new token must go out with
        // the rotation itself, not wait for the id to go missing.
        let sets = |db: sqlx::PgPool, cluster: String| async move {
            commands(&db, &cluster).await.iter().filter(|k| *k == "tunnel_set").count()
        };
        let before = sets(db.clone(), cluster.clone()).await;
        q("UPDATE pyo.cluster_tunnels SET rotate = true, token_sent_at = now(), agent_status = jsonb_build_object('tunnelId', tunnel_id) WHERE cluster_id = $1")
            .bind(&cluster).execute(&db).await.unwrap();
        tunnel_loop::pass(&s).await.unwrap();
        assert_eq!(sets(db.clone(), cluster.clone()).await, before + 1, "the new token, sent with the rotation");
        let rotate: bool = sqlx::query_scalar("SELECT rotate FROM pyo.cluster_tunnels WHERE cluster_id = $1").bind(&cluster).fetch_one(&db).await.unwrap();
        assert!(!rotate);
        tunnel_loop::pass(&s).await.unwrap();
        assert_eq!(sets(db.clone(), cluster.clone()).await, before + 1, "once");

        // Removed: no routes but the catch-all, the tunnel deleted, its certificate revoked.
        q("UPDATE pyo.clusters SET traffic = 'public' WHERE id = $1").bind(&cluster).execute(&db).await.unwrap();
        q("UPDATE pyo.cluster_tunnels SET state = 'removing', agent_status = '{}' WHERE cluster_id = $1").bind(&cluster).execute(&db).await.unwrap();
        tunnel_loop::pass(&s).await.unwrap();
        let left: i64 = sqlx::query_scalar("SELECT count(*) FROM pyo.cluster_tunnels WHERE cluster_id = $1").bind(&cluster).fetch_one(&db).await.unwrap();
        assert_eq!(left, 0);
        let t = cf.tunnels().into_iter().find(|t| t["id"] == json!(tunnel_id)).unwrap();
        assert!(!t["deleted_at"].is_null());
        assert!(cf.state.lock().unwrap().certs.iter().all(|c| c.2), "revoked");
        let calls = cf.calls();
        let at = |c: &str| calls.iter().rposition(|x| x.starts_with(c)).unwrap();
        assert!(at("PUT /accounts") < at("DELETE /accounts"), "routes emptied before the tunnel goes: {calls:?}");
        assert!(cf.records(ZONE).is_empty(), "Pyo's wildcard record goes too");
        let leftovers: i64 = sqlx::query_scalar("SELECT count(*) FROM pyo.tunnel_leftovers WHERE team_id = $1").bind(&team).fetch_one(&db).await.unwrap();
        assert_eq!(leftovers, 0);
        q("DELETE FROM pyo.teams WHERE id = $1").bind(&team).execute(&db).await.unwrap();
    });
}

/// Against a throwaway Postgres and the fake Cloudflare: a service saved
/// back the way the MCP tools show it (values as `•••`) keeps its real
/// values, and `removeDomain` takes a domain off with Pyo's record, without
/// a whole-service save from the client.
#[test]
#[ignore = "needs DATABASE_URL (a throwaway Postgres) and PYO_SECRET_KEY"]
fn hidden_values_are_kept_and_domains_come_off() {
    secret_key();
    run_big(async {
        let _one = FAKE_API.lock().await;
        let cf = FakeCf::start(TOKEN, &[(ZONE, "example.com", ACCOUNT)]).await;
        // SAFETY: set while FAKE_API is held; nothing else here reads it yet.
        unsafe {
            std::env::set_var("PYO_DEV_CLOUDFLARE_API", &cf.base);
        }
        let db = sqlx::PgPool::connect(&std::env::var("DATABASE_URL").expect("DATABASE_URL"))
            .await
            .unwrap();
        crate::migrate::run(&db).await.unwrap();
        crate::vault::init(crate::vault::Vault::new(db.clone(), crate::vault::Provider::from_env()));
        let s = crate::rls::tests::state(db.clone());
        let run = crate::ids::random_string(8).to_lowercase();
        let id = |p: &str| format!("{p}_dom{run}");
        let (team, cluster, project, svc, usr) = (id("team"), id("cl"), id("prj"), id("svc"), id("usr"));
        let q = |sql: &'static str| sqlx::query(sql);
        q("INSERT INTO pyo.teams (id, name, slug) VALUES ($1, $1, $1)").bind(&team).execute(&db).await.unwrap();
        q("INSERT INTO pyo.users (id, email, name, password_hash) VALUES ($1, $2, 'ada', 'x')")
            .bind(&usr).bind(format!("ada{run}@example.com")).execute(&db).await.unwrap();
        q("INSERT INTO pyo.members (id, team_id, user_id, email, role, status) VALUES ($1, $2, $3, $4, 'developer', 'active')")
            .bind(id("mem")).bind(&team).bind(&usr).bind(format!("ada{run}@example.com")).execute(&db).await.unwrap();
        let user = crate::auth::User {
            id: usr.clone(),
            name: "ada".into(),
            email: format!("ada{run}@example.com"),
            timezone: "UTC".into(),
            is_admin: false,
            must_set_up: false,
            key: None,
        };
        q("INSERT INTO pyo.clusters (id, team_id, name, agent_token_hash, status, last_seen_at) VALUES ($1, $2, $1, $1, 'connected', now())")
            .bind(&cluster).bind(&team).execute(&db).await.unwrap();
        q("INSERT INTO pyo.projects (id, team_id, name, namespace) VALUES ($1, $2, 'shop', $1)").bind(&project).bind(&team).execute(&db).await.unwrap();
        let (a, b) = (format!("a-{run}.example.com"), format!("b-{run}.example.com"));
        let mut spec = json!({
            "name": "web", "replicas": 1, "source": { "type": "image", "image": "nginx:1.27" },
            "ports": [{ "name": "http", "containerPort": 80, "protocol": "HTTP", "exposure": "http" }],
            "domains": [
                { "id": "dom_a", "hostname": a, "portName": "http", "status": "active", "tls": "active", "tlsMode": "auto" },
                { "id": "dom_b", "hostname": b, "portName": "http", "status": "active", "tls": "active", "tlsMode": "auto" },
            ],
            "env": [
                { "id": "e1", "key": "API_URL", "value": "https://api.example.com" },
                { "id": "e2", "key": "TOKEN", "value": "s3cret-token", "secret": true },
            ],
        });
        crate::vault::get().seal_env(&team, &mut spec).await.unwrap();
        q("INSERT INTO pyo.services (id, team_id, project_id, cluster_id, name, spec, status) VALUES ($1, $2, $3, $4, 'web', $5, 'stopped')")
            .bind(&svc).bind(&team).bind(&project).bind(&cluster).bind(&spec).execute(&db).await.unwrap();
        // The team's Cloudflare, and the record Pyo made for the first domain.
        let zones = json!([{ "id": ZONE, "name": "example.com", "canEdit": true, "accountId": ACCOUNT, "accountName": "Acme", "tunnel": { "ready": true } }]);
        let sealed = crate::vault::get().seal_credential(&team, TOKEN).await.unwrap();
        q("INSERT INTO pyo.cloudflare_accounts (team_id, token_sealed, zones) VALUES ($1, $2, $3)").bind(&team).bind(&sealed).bind(&zones).execute(&db).await.unwrap();
        let mut tx = db.begin().await.unwrap();
        let mark = marker(&install_id(&mut tx).await.unwrap());
        tx.commit().await.unwrap();
        let rec = format!("rec{run}");
        cf.put_record(ZONE, json!({ "id": rec, "type": "A", "name": a, "content": "203.0.113.7", "proxied": false, "ttl": 1, "comment": mark }));
        q("INSERT INTO pyo.dns_records (service_id, domain_id, team_id, hostname, zone_id, zone_name, record_id, type, content, status) VALUES ($1, 'dom_a', $2, $3, $4, 'example.com', $5, 'A', '203.0.113.7', 'created')")
            .bind(&svc).bind(&team).bind(&a).bind(ZONE).bind(&rec).execute(&db).await.unwrap();
        let stored = |db: sqlx::PgPool, svc: String| async move {
            let mut spec: Value = sqlx::query_scalar("SELECT spec FROM pyo.services WHERE id = $1").bind(svc).fetch_one(&db).await.unwrap();
            crate::vault::get().open_env(&mut spec).await.unwrap();
            spec
        };
        let values = |spec: &Value| -> Vec<String> {
            spec["env"].as_array().unwrap().iter().map(|e| e["value"].as_str().unwrap().to_owned()).collect()
        };

        // Saved back as get_state shows it: every value kept.
        let mut shown = stored(db.clone(), svc.clone()).await;
        shown["id"] = json!(svc);
        shown["env"] = json!([
            { "id": "e1", "key": "API_URL", "value": "•••" },
            { "id": "e2", "key": "TOKEN", "value": "•••", "secret": true, "hasValue": true },
        ]);
        shown["description"] = json!("moved");
        crate::ops::run_json(&s, &user, json!({ "op": "saveService", "service": shown })).await.unwrap();
        let now = stored(db.clone(), svc.clone()).await;
        assert_eq!(now["description"], "moved");
        assert_eq!(values(&now), ["https://api.example.com", "s3cret-token"]);
        // A hidden value with nothing saved, or a hidden part: refused, nothing changed.
        let mut bad = shown.clone();
        bad["env"].as_array_mut().unwrap().push(json!({ "key": "NEW_KEY", "value": "•••" }));
        let e = crate::ops::run_json(&s, &user, json!({ "op": "saveService", "service": bad })).await.unwrap_err();
        assert_eq!(e.message, "NEW_KEY's value is hidden (•••). Send its real value.");
        let mut bad = shown.clone();
        bad["env"][0]["value"] = json!("https://••••••••@api.example.com");
        let e = crate::ops::run_json(&s, &user, json!({ "op": "saveService", "service": bad })).await.unwrap_err();
        assert_eq!(e.message, "API_URL has a hidden part (•••). Send its full value.");
        assert_eq!(values(&stored(db.clone(), svc.clone()).await), ["https://api.example.com", "s3cret-token"]);

        // Removed by hostname: Pyo's record goes too, the port stays public for the other.
        let r = crate::ops::run_json(&s, &user, json!({ "op": "removeDomain", "serviceId": svc, "domainId": a.to_uppercase() })).await.unwrap();
        assert_eq!(r["removed"], json!(a));
        let now = stored(db.clone(), svc.clone()).await;
        let hosts: Vec<&str> = now["domains"].as_array().unwrap().iter().map(|d| d["hostname"].as_str().unwrap()).collect();
        assert_eq!(hosts, [b.as_str()]);
        assert_eq!(now["ports"][0]["exposure"], "http");
        assert!(cf.records(ZONE).iter().all(|r| r["id"] != json!(rec)), "Pyo's record is deleted");
        let left: i64 = sqlx::query_scalar("SELECT count(*) FROM pyo.dns_records WHERE service_id = $1").bind(&svc).fetch_one(&db).await.unwrap();
        assert_eq!(left, 0);
        assert_eq!(values(&now), ["https://api.example.com", "s3cret-token"], "the variables are untouched");
        // A record someone made by hand is never touched.
        cf.put_record(ZONE, json!({ "id": format!("hand{run}"), "type": "A", "name": b, "content": "198.51.100.9", "proxied": false, "ttl": 300, "comment": "by hand" }));
        crate::ops::run_json(&s, &user, json!({ "op": "removeDomain", "serviceId": svc, "domainId": "dom_b" })).await.unwrap();
        let now = stored(db.clone(), svc.clone()).await;
        assert_eq!(now["domains"], json!([]));
        assert_eq!(now["ports"][0]["exposure"], "internal", "no domain uses it anymore");
        assert!(cf.records(ZONE).iter().any(|r| r["id"] == json!(format!("hand{run}"))));
        // One it doesn't have.
        let e = crate::ops::run_json(&s, &user, json!({ "op": "removeDomain", "serviceId": svc, "domainId": "dom_zz" })).await.unwrap_err();
        assert_eq!(e.status, axum::http::StatusCode::NOT_FOUND);
        q("DELETE FROM pyo.teams WHERE id = $1").bind(&team).execute(&db).await.unwrap();
        q("DELETE FROM pyo.users WHERE id = $1").bind(&usr).execute(&db).await.unwrap();
    });
}

/// Against a throwaway Postgres and the fake Cloudflare: the domain proof
/// records Pyo writes go once it's done with them, or once the domain is
/// gone, and only Pyo's own: one made by hand, for another team or by
/// another installation stays.
#[test]
#[ignore = "needs DATABASE_URL (a throwaway Postgres) and PYO_SECRET_KEY"]
fn proof_records_pyo_made_are_cleaned_up() {
    secret_key();
    run_big(async {
        let _one = FAKE_API.lock().await;
        let cf = FakeCf::start(TOKEN, &[(ZONE, "example.com", ACCOUNT)]).await;
        // SAFETY: set while FAKE_API is held; nothing else here reads it yet.
        unsafe {
            std::env::set_var("PYO_DEV_CLOUDFLARE_API", &cf.base);
        }
        let db = sqlx::PgPool::connect(&std::env::var("DATABASE_URL").expect("DATABASE_URL"))
            .await
            .unwrap();
        crate::migrate::run(&db).await.unwrap();
        crate::vault::init(crate::vault::Vault::new(db.clone(), crate::vault::Provider::from_env()));
        let s = crate::rls::tests::state(db.clone());
        let run = crate::ids::random_string(8).to_lowercase();
        let id = |p: &str| format!("{p}_prf{run}");
        let (team, cluster, project) = (id("team"), id("cl"), id("prj"));
        let host = format!("shop-{run}.example.com");
        let name = crate::domainproof::record(&host);
        let value = crate::domainproof::token(&team, &host);
        let q = |sql: &'static str| sqlx::query(sql);
        q("INSERT INTO pyo.teams (id, name, slug) VALUES ($1, $1, $1)").bind(&team).execute(&db).await.unwrap();
        let zones = json!([{ "id": ZONE, "name": "example.com", "canEdit": true, "accountId": ACCOUNT, "accountName": "Acme", "tunnel": { "ready": true } }]);
        let sealed = crate::vault::get().seal_credential(&team, TOKEN).await.unwrap();
        q("INSERT INTO pyo.cloudflare_accounts (team_id, token_sealed, zones) VALUES ($1, $2, $3)").bind(&team).bind(&sealed).bind(&zones).execute(&db).await.unwrap();
        let proofs = || -> Vec<Value> { cf.records(ZONE).into_iter().filter(|r| r["name"] == json!(name)).collect() };
        let write = |db: sqlx::PgPool, team: String, host: String| async move {
            let mut tx = db.begin().await.unwrap();
            assert!(write_proof(&mut tx, &team, &host).await.unwrap());
            tx.commit().await.unwrap();
        };

        // Written by Pyo, with its comment and the team's token.
        write(db.clone(), team.clone(), host.clone()).await;
        let mut tx = db.begin().await.unwrap();
        let mark = marker(&install_id(&mut tx).await.unwrap());
        tx.commit().await.unwrap();
        let p = proofs();
        assert_eq!(p.len(), 1);
        assert_eq!((p[0]["comment"].as_str(), p[0]["content"].as_str()), (Some(mark.as_str()), Some(format!("\"{value}\"").as_str())));

        // The domain removed: the job deletes it.
        let job = json!({ "teamId": team, "hosts": [host] });
        proofs_job(&s, &job).await.unwrap();
        assert!(proofs().is_empty());
        // Not while the team uses the domain again.
        write(db.clone(), team.clone(), host.clone()).await;
        q("INSERT INTO pyo.clusters (id, team_id, name, agent_token_hash, status) VALUES ($1, $2, $1, $1, 'connected')").bind(&cluster).bind(&team).execute(&db).await.unwrap();
        q("INSERT INTO pyo.projects (id, team_id, name, namespace) VALUES ($1, $2, 'shop', $1)").bind(&project).bind(&team).execute(&db).await.unwrap();
        q("INSERT INTO pyo.services (id, team_id, project_id, cluster_id, name, spec, status) VALUES ($1, $2, $3, $4, 'web', jsonb_build_object('domains', $5::jsonb), 'running')")
            .bind(id("svc")).bind(&team).bind(&project).bind(&cluster).bind(json!([{ "id": "dom_1", "hostname": host }])).execute(&db).await.unwrap();
        proofs_job(&s, &job).await.unwrap();
        assert_eq!(proofs().len(), 1, "the team uses it again");

        // Done with: only Pyo's own goes. By hand (no comment, or another
        // installation's), and another team's, stay.
        cf.put_record(ZONE, json!({ "id": "hand", "type": "TXT", "name": name, "content": value, "ttl": 1 }));
        cf.put_record(ZONE, json!({ "id": "other-team", "type": "TXT", "name": name, "content": format!("\"{}\"", crate::domainproof::token("team_other", &host)), "ttl": 1, "comment": mark }));
        cf.put_record(ZONE, json!({ "id": "other-install", "type": "TXT", "name": name, "content": value, "ttl": 1, "comment": "Made by Pyo (zzz999)" }));
        let mut tx = db.begin().await.unwrap();
        assert_eq!(remove_proof(&mut tx, &team, &host).await.unwrap(), 1);
        tx.commit().await.unwrap();
        let left: Vec<Value> = proofs().iter().map(|r| r["id"].clone()).collect();
        assert_eq!(left, [json!("hand"), json!("other-team"), json!("other-install")]);
        // Without a record of Pyo's, nothing to do.
        let mut tx = db.begin().await.unwrap();
        assert_eq!(remove_proof(&mut tx, &team, &host).await.unwrap(), 0);
        tx.commit().await.unwrap();
        q("DELETE FROM pyo.teams WHERE id = $1").bind(&team).execute(&db).await.unwrap();
    });
}
