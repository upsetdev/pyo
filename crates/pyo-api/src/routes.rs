//! A service's domains belong to the service, not to a deployment: they're
//! stored with it (`spec.domains`, with its `spec.requestTimeout`, which
//! goes with them everywhere below), and a change goes live in place
//! (`SyncRoutes`), without a redeploy. Each change bumps the service's
//! `routes_revision`. Every command that carries domains (a deploy and its
//! fallback, a routes sync, a firewall sync) gets the service's current ones
//! when it's handed to the agent ([`refresh`]), so a rollback, a retry or a
//! deploy that waited for its build uses the domains of today; and the agent
//! applies the newest revision it has seen (gateway.rs `RouteState`), so a
//! sync and a deploy racing each other end on the newest domains.

use pyo_proto::{Command, DomainSpec, FirewallTarget};
use serde_json::{Value, json};
use sqlx::{PgPool, Postgres, Transaction};

use crate::error::ApiResult;

/// A service's domains (its `spec.domains`) as the agent applies them.
pub fn domain_specs(domains: &Value) -> Vec<DomainSpec> {
    let s = |d: &Value, k: &str| d[k].as_str().unwrap_or("").to_owned();
    domains
        .as_array()
        .into_iter()
        .flatten()
        .map(|d| {
            let hostname = s(d, "hostname");
            let tls = crate::deploy::tls_issuer(d["tlsMode"].as_str().unwrap_or(""), &hostname)
                .to_owned();
            DomainSpec {
                certificate_id: if tls == "custom" {
                    s(d, "certificateId")
                } else {
                    String::new()
                },
                tls,
                redirect_https: d["redirectHttps"].as_bool().unwrap_or(true),
                port_name: s(d, "portName"),
                hostname,
                // Through the cluster's Cloudflare Tunnel (cloudflare/tunnel.rs).
                tunnel: match d["tunnel"].as_str() {
                    Some(t @ (pyo_proto::TUNNEL_SWITCHING | pyo_proto::TUNNEL_ON)) => t.to_owned(),
                    _ => String::new(),
                },
                // Its rules in Pyo Gateway (checked when they were saved).
                redirects: serde_json::from_value(d["redirects"].clone()).unwrap_or_default(),
                headers: serde_json::from_value(d["headers"].clone()).unwrap_or_default(),
                // Its path (paths.rs): saved tidy.
                path: match crate::paths::path_of(d) {
                    "/" => String::new(),
                    p => p.to_owned(),
                },
                strip_prefix: d["stripPrefix"] == json!(true),
                // Filled in when it's handed out (paths.rs `specs`).
                primary_of: String::new(),
            }
        })
        .collect()
}

/// A domain's rules in Pyo Gateway (pyo-proto's rules.rs), as they're
/// saved: checked, then written back tidied (paths without a trailing "/",
/// hostnames in lower case). None leaves no empty fields behind.
pub fn tidy_rules(d: &mut Value) -> Result<(), String> {
    use pyo_proto::rules::{HeaderRules, Redirect};
    let host = d["hostname"].as_str().unwrap_or("").to_owned();
    let routes = !d["portName"].as_str().unwrap_or("").is_empty();
    let redirects: Vec<Redirect> = match &d["redirects"] {
        Value::Null => vec![],
        v => serde_json::from_value(v.clone())
            .map_err(|_| format!("The redirects of {host} aren't valid."))?,
    };
    let headers: HeaderRules = match &d["headers"] {
        Value::Null => HeaderRules::default(),
        v => serde_json::from_value(v.clone())
            .map_err(|_| format!("The header rules of {host} aren't valid."))?,
    };
    let redirects = pyo_proto::rules::check_redirects(&host, &redirects, routes)?;
    let headers = pyo_proto::rules::tidy_headers(headers);
    pyo_proto::rules::check_headers(&headers)?;
    let Some(o) = d.as_object_mut() else {
        return Ok(());
    };
    if redirects.is_empty() {
        o.remove("redirects");
    } else {
        o.insert("redirects".into(), json!(redirects));
    }
    if headers.is_empty() {
        o.remove("headers");
    } else {
        o.insert("headers".into(), json!(headers));
    }
    Ok(())
}

/// The rules a domain has: what an agent needs to write them (pyo-proto's
/// `Capabilities::gateway_rules`): 2 for paths, 1 for redirects and header
/// rules, else 0.
fn rules_level(domains: &Value) -> u8 {
    let all: Vec<&Value> = domains.as_array().into_iter().flatten().collect();
    if all
        .iter()
        .any(|d| crate::paths::path_of(d) != "/" || !crate::paths::holds(d))
    {
        return 2;
    }
    let any = all.iter().any(|d| {
        d["redirects"].as_array().is_some_and(|r| !r.is_empty())
            || d["headers"].as_object().is_some_and(|h| !h.is_empty())
    });
    u8::from(any)
}

/// Rules a cluster's agent doesn't write yet are refused, when they're new
/// (a save that keeps them as they were still goes through). A cluster
/// whose agent never reported can have them: its agent is this release's.
pub async fn rules_ok(
    tx: &mut Transaction<'_, Postgres>,
    cluster_id: &str,
    spec: &Value,
    old: &Value,
) -> ApiResult<()> {
    let wanted = rules_level(&spec["domains"]);
    if wanted == 0 || wanted <= rules_level(&old["domains"]) {
        return Ok(());
    }
    let row: Option<(Option<String>, Option<i64>)> = sqlx::query_as(
        "SELECT agent_version, (capabilities->>'gatewayRules')::bigint FROM pyo.clusters WHERE id = $1",
    )
    .bind(cluster_id)
    .fetch_optional(&mut **tx)
    .await?;
    match row {
        Some((Some(_), level)) if level.unwrap_or(0) < i64::from(wanted) => {
            Err(crate::error::ApiError::conflict(if wanted >= 2 {
                "This cluster's agent doesn't apply paths yet. Update the agent first."
            } else {
                "This cluster's agent doesn't apply redirects and header rules yet. Update the agent first."
            }))
        }
        _ => Ok(()),
    }
}

/// A service's own request timeout (its `spec.requestTimeout`, seconds,
/// 0 for none): None leaves the default
/// ([`pyo_proto::system::REQUEST_TIMEOUT_SECS`]).
pub fn request_timeout(spec: &Value) -> Option<u32> {
    spec["requestTimeout"]
        .as_u64()
        .map(|n| n.min(u64::from(pyo_proto::system::MAX_REQUEST_TIMEOUT_SECS)) as u32)
}

/// Whether two `spec.domains` route differently (not just their DNS or
/// certificate status).
pub fn differ(a: &Value, b: &Value) -> bool {
    let v = |d: &Value| serde_json::to_value(domain_specs(d)).unwrap_or(Value::Null);
    let held = |d: &Value| -> Vec<bool> {
        d.as_array()
            .into_iter()
            .flatten()
            .map(crate::paths::holds)
            .collect()
    };
    v(a) != v(b) || held(a) != held(b)
}

/// Whether a cluster's routes are sent again after a heartbeat: its agent
/// writes routes differently from the last time Pyo saw it (a new agent,
/// with a new default for every route). Not for an agent that doesn't say.
pub fn resend_wanted(reported: &str, last: Option<&str>) -> bool {
    !reported.is_empty() && last != Some(reported)
}

/// The cluster's agent reported how it writes routes
/// ([`pyo_proto::Heartbeat::routes_version`]), `last` being what it said
/// before: when that changed, every running service there gets its routes
/// again as they now stand (with their firewall and WAF,
/// `firewall::sync`), so a newer agent's routes reach the services already
/// there without each one being changed. Gives whether a sync was queued.
pub async fn resend_on_change(
    tx: &mut Transaction<'_, Postgres>,
    cluster_id: &str,
    reported: &str,
    last: Option<&str>,
) -> ApiResult<bool> {
    if !resend_wanted(reported, last) {
        return Ok(false);
    }
    let any: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM pyo.services WHERE cluster_id = $1 AND current_deployment_id IS NOT NULL AND NOT deleting)",
    )
    .bind(cluster_id)
    .fetch_one(&mut **tx)
    .await?;
    if any {
        crate::firewall::sync(tx, cluster_id).await?;
    }
    Ok(any)
}

/// A service's domains changed (already saved): a new revision, and, when
/// it runs on its cluster (or its rollout is under way), its routes follow
/// now, unless `sync` is off (a deploy that goes out with them). The cluster
/// to wake comes back when a sync was queued.
pub async fn changed(
    tx: &mut Transaction<'_, Postgres>,
    service_id: &str,
    sync: bool,
) -> ApiResult<Option<String>> {
    sqlx::query("UPDATE pyo.services SET routes_revision = routes_revision + 1 WHERE id = $1")
        .bind(service_id)
        .execute(&mut **tx)
        .await?;
    // The services on its hostnames follow: what they take from it (a
    // hostname it holds), or which service holds them, may have changed.
    let mut wake = None;
    for other in crate::paths::sharers(tx, service_id).await? {
        sqlx::query("UPDATE pyo.services SET routes_revision = routes_revision + 1 WHERE id = $1")
            .bind(&other)
            .execute(&mut **tx)
            .await?;
        if let Some((cluster_id, target)) = target(tx, &other).await? {
            crate::deploy::enqueue(tx, &cluster_id, Command::SyncRoutes(Box::new(target))).await?;
            wake = Some(cluster_id);
        }
    }
    if !sync {
        return Ok(wake);
    }
    let Some((cluster_id, target)) = target(tx, service_id).await? else {
        return Ok(wake);
    };
    crate::deploy::enqueue(tx, &cluster_id, Command::SyncRoutes(Box::new(target))).await?;
    Ok(Some(cluster_id))
}

/// [`changed`], with the routes sync's command (to wait for the agent to
/// have it): (cluster, command id). None when nothing was queued (the
/// service never ran, or is being deleted).
pub async fn changed_waiting(
    tx: &mut Transaction<'_, Postgres>,
    service_id: &str,
) -> ApiResult<Option<(String, String)>> {
    sqlx::query("UPDATE pyo.services SET routes_revision = routes_revision + 1 WHERE id = $1")
        .bind(service_id)
        .execute(&mut **tx)
        .await?;
    let Some((cluster_id, target)) = target(tx, service_id).await? else {
        return Ok(None);
    };
    let id = crate::deploy::enqueue(tx, &cluster_id, Command::SyncRoutes(Box::new(target))).await?;
    Ok(Some((cluster_id, id)))
}

/// What a routes sync applies: the live deployment's service and ports (or,
/// before its first rollout is done, that rollout's), its domains, firewall
/// and WAF as they are now. None for a service never deployed (its first
/// deploy brings its domains) or being deleted.
async fn target(
    tx: &mut Transaction<'_, Postgres>,
    service_id: &str,
) -> ApiResult<Option<(String, FirewallTarget)>> {
    let row: Option<(String, Value, Value, i64, bool, Value)> = sqlx::query_as(
        "SELECT a.cluster_id, d.spec, coalesce(a.spec->'domains', '[]'), a.routes_revision, p.network_isolation,
                jsonb_build_object('requestTimeout', a.spec->'requestTimeout')
           FROM pyo.services a JOIN pyo.projects p ON p.id = a.project_id
           JOIN pyo.deployments d ON d.service_id = a.id
          WHERE a.id = $1 AND NOT a.deleting
            AND (d.id = a.current_deployment_id OR d.status NOT IN ('succeeded', 'failed', 'cancelled'))
          ORDER BY d.id = a.current_deployment_id DESC, d.created_at DESC
          LIMIT 1",
    )
    .bind(service_id)
    .fetch_optional(&mut **tx)
    .await?;
    let Some((cluster_id, spec, domains, revision, isolation, timeout)) = row else {
        return Ok(None);
    };
    let Ok(service) = serde_json::from_value(spec["service"].clone()) else {
        return Ok(None);
    };
    let firewall = crate::firewall::for_service(tx, service_id).await?;
    let engine = crate::waf::wanted(tx, &cluster_id).await?;
    let waf = if engine {
        Some(crate::waf::for_service(tx, service_id).await?)
    } else {
        None
    };
    let tailnet_web: bool = sqlx::query_scalar("SELECT tailnet_web FROM pyo.clusters WHERE id = $1")
        .bind(&cluster_id)
        .fetch_optional(&mut **tx)
        .await?
        .unwrap_or(true);
    let mut specs = domain_specs(&domains);
    served(&mut specs, tailnet_web);
    Ok(Some((
        cluster_id,
        FirewallTarget {
            service,
            project_id: spec["projectId"].as_str().unwrap_or("").to_owned(),
            ports: serde_json::from_value(spec["ports"].clone()).unwrap_or_default(),
            domains: specs,
            routes_revision: revision,
            request_timeout: request_timeout(&timeout),
            compression: false,
            idle_timeout: None,
            network_isolation: isolation,
            firewall,
            waf,
            site: serde_json::from_value(spec["site"].clone()).unwrap_or_default(),
        },
    )))
}

/// Leave out the domains whose path is off: on the tailnet while the
/// cluster's Tailnet switch is off (traffic_paths.rs). Their HTTPRoutes go.
/// A ts.net name has no certificate of its own (the tailnet Gateway's
/// listener has Tailscale's), so that listener doesn't change.
pub fn served(specs: &mut Vec<DomainSpec>, tailnet_web: bool) {
    specs.retain(|d| crate::traffic_paths::served_on_tailnet(&d.hostname, tailnet_web));
}

/// A command as it's handed to the agent: whatever it carries of a
/// service's routes gets the service's domains as they are now (a deploy
/// and the version it falls back to, a routes sync, each service of a
/// firewall sync). The queued command itself stays as it was.
pub async fn refresh(db: &PgPool, command: &mut Value) -> anyhow::Result<()> {
    match command["kind"].as_str() {
        Some("deploy") => {
            current(db, command).await?;
            if command["fallback"].is_object() {
                current(db, &mut command["fallback"]).await?;
            }
        }
        Some("sync_routes") => current(db, command).await?,
        Some("sync_firewall") => {
            for t in command["targets"].as_array_mut().into_iter().flatten() {
                current(db, t).await?;
            }
        }
        _ => {}
    }
    Ok(())
}

/// One spec or target with its service's current domains, request
/// timeout and revision, and its cluster's route settings (the Gateway
/// section's compression and timeout defaults).
async fn current(db: &PgPool, o: &mut Value) -> anyhow::Result<()> {
    let Some(id) = o["service"]["serviceId"].as_str() else {
        return Ok(());
    };
    let row: Option<(Value, i64, bool, i32, bool, i32)> = sqlx::query_as(
        "SELECT jsonb_build_object('domains', coalesce(a.spec->'domains', '[]'), 'requestTimeout', a.spec->'requestTimeout'), a.routes_revision,
                coalesce(c.tailnet_web, true), coalesce(c.request_timeout, 300), coalesce(c.compression, false), coalesce(c.idle_timeout, 3600)
           FROM pyo.services a LEFT JOIN pyo.clusters c ON c.id = a.cluster_id WHERE a.id = $1",
    )
    .bind(id)
    .fetch_optional(db)
    .await?;
    if let Some((routing, revision, tailnet_web, request_default, compression, idle_default)) = row {
        let mut conn = db.acquire().await?;
        let mut specs = crate::paths::specs(&mut conn, id, &routing["domains"]).await?;
        served(&mut specs, tailnet_web);
        let d = Defaults::of(request_default, compression, idle_default);
        o["domains"] = serde_json::to_value(specs).unwrap_or(json!([]));
        o["requestTimeout"] = json!(d.request_timeout(request_timeout(&routing)));
        o["compression"] = json!(d.compression);
        o["idleTimeout"] = json!(d.idle_timeout);
        o["routesRevision"] = json!(revision);
    }
    Ok(())
}

/// A cluster's route settings for its services (the Gateway section).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Defaults {
    /// The request timeout for services without their own: None while it's
    /// Pyo's own default ([`pyo_proto::system::REQUEST_TIMEOUT_SECS`]), so
    /// routes are written as before.
    pub request_default: Option<u32>,
    pub compression: bool,
    /// None while it's the client policies' 1 hour: routes carry none.
    pub idle_timeout: Option<u32>,
}

impl Defaults {
    /// From the cluster's columns.
    pub fn of(request_default: i32, compression: bool, idle: i32) -> Self {
        let request = u32::try_from(request_default)
            .unwrap_or(pyo_proto::system::REQUEST_TIMEOUT_SECS)
            .min(pyo_proto::system::MAX_REQUEST_TIMEOUT_SECS);
        let idle = u32::try_from(idle).unwrap_or(pyo_proto::system::IDLE_TIMEOUT_SECS);
        Defaults {
            request_default: (request != pyo_proto::system::REQUEST_TIMEOUT_SECS).then_some(request),
            compression,
            idle_timeout: (idle != pyo_proto::system::IDLE_TIMEOUT_SECS).then_some(idle),
        }
    }

    /// A service's request timeout: its own, else the cluster's default.
    pub fn request_timeout(self, own: Option<u32>) -> Option<u32> {
        own.or(self.request_default)
    }
}

/// Its domains route now: each one whose DNS checks out is active, and its
/// certificate issued (an uploaded one), off, or on its way. Tailnet
/// domains keep their state: it follows the cluster's tailnet
/// (tailscale.rs). With `revision`, only while the service's domains are
/// still that version.
pub async fn mark_routed(
    tx: &mut Transaction<'_, Postgres>,
    service_id: &str,
    revision: Option<i64>,
) -> ApiResult<()> {
    sqlx::query(
        "UPDATE pyo.services SET spec = jsonb_set(spec, '{domains}', coalesce((
                  SELECT jsonb_agg(CASE WHEN lower(d->>'hostname') LIKE '%.ts.net' THEN d ELSE d || jsonb_build_object(
                           -- Ones still waiting for DNS stay so until the DNS watcher sees them resolve.
                           'status', CASE WHEN d->>'status' IN ('pending_dns', 'verifying') THEN d->>'status' ELSE 'active' END, 'tls',
                           CASE WHEN coalesce(d->>'tlsMode', 'auto') = 'off' THEN 'disabled'
                                WHEN d->>'tlsMode' = 'custom' OR d->>'tls' = 'issued' THEN 'issued' ELSE 'pending' END)
                           - CASE WHEN d->>'status' IN ('pending_dns', 'verifying') THEN '' ELSE 'error' END END)
                    FROM jsonb_array_elements(CASE WHEN jsonb_typeof(spec->'domains') = 'array' THEN spec->'domains' ELSE '[]' END) d), '[]'))
          WHERE id = $1 AND ($2::bigint IS NULL OR routes_revision = $2)",
    )
    .bind(service_id)
    .bind(revision)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// The agent finished a routes sync: its domains route, or the team hears
/// why they don't.
pub async fn finished(
    tx: &mut Transaction<'_, Postgres>,
    team_id: &str,
    command: &Value,
    ok: bool,
    message: Option<&str>,
) -> ApiResult<()> {
    if command["kind"] != json!("sync_routes") {
        return Ok(());
    }
    let service_id = command["service"]["serviceId"].as_str().unwrap_or("");
    if ok {
        return mark_routed(
            tx,
            service_id,
            Some(command["routesRevision"].as_i64().unwrap_or(0)),
        )
        .await;
    }
    let row: Option<(String, String)> =
        sqlx::query_as("SELECT name, project_id FROM pyo.services WHERE id = $1 AND NOT deleting")
            .bind(service_id)
            .fetch_optional(&mut **tx)
            .await?;
    if let Some((name, project_id)) = row {
        let why = message.unwrap_or("the agent failed");
        crate::ops::log_activity(
            tx,
            team_id,
            "Pyo",
            "domain.sync_failed",
            "warning",
            &format!("Couldn't update the domains of {name}: {why}"),
            json!({ "kind": "service", "id": service_id, "label": name }),
            json!({ "projectId": project_id, "serviceId": service_id }),
        )
        .await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A cluster's routes go out again when its agent writes them
    /// differently, not on every heartbeat, nor for an agent that doesn't
    /// say how it writes them.
    #[test]
    fn routes_go_out_again_when_the_agent_writes_them_differently() {
        assert!(resend_wanted("a1", None), "first time it says");
        assert!(resend_wanted("b2", Some("a1")), "updated");
        assert!(!resend_wanted("a1", Some("a1")), "same agent");
        assert!(!resend_wanted("", Some("a1")), "older agent");
        assert!(!resend_wanted("", None), "older agent");
    }

    #[test]
    fn only_routing_changes_count() {
        let a = json!([{ "id": "dom_1", "hostname": "shop.example.com", "portName": "http", "tlsMode": "auto", "redirectHttps": true, "status": "pending_dns", "tls": "pending" }]);
        let mut b = a.clone();
        b[0]["status"] = json!("active");
        b[0]["tls"] = json!("issued");
        b[0]["dnsCheckedAt"] = json!(1);
        assert!(!differ(&a, &b), "status only");
        b[0]["redirectHttps"] = json!(false);
        assert!(differ(&a, &b), "redirect");
        assert!(differ(&a, &json!([])), "removed");
        let mut c = a.clone();
        c[0]["tlsMode"] = json!("custom");
        c[0]["certificateId"] = json!("crt_1");
        assert!(differ(&a, &c), "certificate");
        assert_eq!(domain_specs(&c)[0].certificate_id, "crt_1");
        assert_eq!(domain_specs(&json!(null)).len(), 0);
        // Rules change routes.
        let mut r = a.clone();
        r[0]["redirects"] = json!([{ "path": "/old", "toPath": "/new" }]);
        assert!(differ(&a, &r), "redirects");
        assert_eq!(domain_specs(&r)[0].redirects[0].to_path, "/new");
        let mut h = a.clone();
        h[0]["headers"] = json!({ "response": { "remove": ["X-Powered-By"] } });
        assert!(differ(&a, &h), "headers");
    }

    /// Rules are saved checked and tidied, and empty ones leave nothing.
    #[test]
    fn rules_are_saved_tidy() {
        let mut d = json!({ "hostname": "www.example.com", "portName": "", "redirects": [{ "toHost": "Example.com" }], "headers": { "response": { "set": [], "remove": [] } } });
        tidy_rules(&mut d).unwrap();
        assert_eq!(
            d["redirects"],
            json!([{ "path": "/", "toHost": "example.com", "status": 301, "keepPath": true }])
        );
        assert!(d.get("headers").is_none());
        assert_eq!(rules_level(&json!([d])), 1);
        assert_eq!(rules_level(&json!([{ "hostname": "a.example.com" }])), 0);
        let mut bad = json!({ "hostname": "example.com", "portName": "http", "redirects": [{ "toHost": "other.example" }] });
        assert!(
            tidy_rules(&mut bad)
                .unwrap_err()
                .contains("routes to a port")
        );
        let mut odd = json!({ "hostname": "example.com", "portName": "http", "redirects": "nope" });
        assert_eq!(
            tidy_rules(&mut odd).unwrap_err(),
            "The redirects of example.com aren't valid."
        );
        let mut own = json!({ "hostname": "example.com", "portName": "http", "headers": { "request": { "set": [{ "name": "X-Forwarded-For", "value": "1.2.3.4" }] } } });
        assert_eq!(
            tidy_rules(&mut own).unwrap_err(),
            "X-Forwarded-For is set by Pyo Gateway and can't be changed."
        );
    }
}

#[cfg(test)]
mod lifecycle_tests {
    use super::*;
    use crate::teamsec_tests::run_big;
    use crate::{AppState, auth::User};

    /// Domains go live in place, and deploys use the service's current ones.
    /// Needs Postgres: DATABASE_URL=… cargo test -p pyo-api -- --ignored routes::lifecycle_tests
    #[test]
    #[ignore]
    fn domain_changes_sync_and_deploys_use_the_current_domains() {
        run_big(lifecycle());
    }

    /// Paths: services of a team share a hostname by path, the first
    /// holds it, the others follow it, and when the holder goes another
    /// takes it over with its DNS record.
    /// Needs Postgres: DATABASE_URL=… cargo test -p pyo-api -- --ignored routes::lifecycle_tests
    #[test]
    #[ignore]
    fn services_share_a_hostname_by_path() {
        run_big(paths());
    }

    async fn paths() {
        let db = PgPool::connect(&std::env::var("DATABASE_URL").expect("DATABASE_URL"))
            .await
            .unwrap();
        crate::migrate::run(&db).await.unwrap();
        crate::vault::init(crate::vault::Vault::new(
            db.clone(),
            crate::vault::Provider::from_env(),
        ));
        let (_stop, stopping) = tokio::sync::watch::channel(false);
        let s = AppState {
            db: db.clone(),
            config: std::sync::Arc::new(crate::Config {
                agent_api_url: String::new(),
                agent_image: String::new(),
                waf_image: pyo_proto::system::WAF_IMAGE_DEFAULT.into(),
                geoip_image: pyo_proto::system::GEOIP_IMAGE_DEFAULT.into(),
                cookie_secure: false,
                allow_signup: false,
                builder_image: String::new(),
                relay_addr: None,
            }),
            observability: crate::observability::Observability::from_env(db.clone()),
            sealer: std::sync::Arc::new(crate::secrets::Sealer::from_env()),
            wakers: Default::default(),
            terminals: Default::default(),
            stopping,
        };
        let run = crate::ids::new_id("t");
        let label = run.replace('_', "-");
        let host = format!("shop-{label}.example.com");
        let (team, other_team) = (format!("team_{run}"), format!("team_o{run}"));
        let (cluster, far) = (format!("cl_{run}"), format!("cl_f{run}"));
        let project = format!("prj_{run}");
        let ns = format!("shop-{label}");
        let user = User {
            id: format!("usr_{run}"),
            name: "T".into(),
            email: format!("{run}@t.test"),
            timezone: "UTC".into(),
            is_admin: false,
            must_set_up: false,
            key: None,
        };
        let q = |sql: &'static str| sqlx::query(sql);
        q("INSERT INTO pyo.users (id, email, name, password_hash) VALUES ($1, $2, 'T', '')")
            .bind(&user.id)
            .bind(&user.email)
            .execute(&db)
            .await
            .unwrap();
        for t in [&team, &other_team] {
            q("INSERT INTO pyo.teams (id, name, slug) VALUES ($1, $1, $1)")
                .bind(t)
                .execute(&db)
                .await
                .unwrap();
            q("INSERT INTO pyo.members (id, team_id, user_id, email, role, status) VALUES ($1, $2, $3, $4, 'developer', 'active')")
                .bind(format!("mem_{t}")).bind(t).bind(&user.id).bind(&user.email).execute(&db).await.unwrap();
        }
        for (c, t) in [
            (&cluster, &team),
            (&far, &team),
            (&format!("cl_o{run}"), &other_team),
        ] {
            q("INSERT INTO pyo.clusters (id, team_id, name, agent_token_hash, status, last_seen_at) VALUES ($1, $2, $1, $1, 'connected', now())")
                .bind(c).bind(t).execute(&db).await.unwrap();
        }
        q("INSERT INTO pyo.projects (id, team_id, name, namespace) VALUES ($1, $2, 'shop', $3)")
            .bind(&project)
            .bind(&team)
            .bind(&ns)
            .execute(&db)
            .await
            .unwrap();
        let other_project = format!("prj_o{run}");
        q("INSERT INTO pyo.projects (id, team_id, name, namespace) VALUES ($1, $2, 'shop', $3)")
            .bind(&other_project)
            .bind(&other_team)
            .bind(format!("o-{label}"))
            .execute(&db)
            .await
            .unwrap();
        let domain = |id: &str, path: &str| {
            let mut d = json!({ "id": id, "hostname": host, "portName": "http", "status": "pending_dns", "tls": "pending", "tlsMode": "letsencrypt", "redirectHttps": true, "createdAt": 1 });
            if path != "/" {
                d["path"] = json!(path);
            }
            d
        };
        let service = |id: &str, name: &str, project: &str, cluster: &str, domains: Value| {
            json!({
                "id": id, "projectId": project, "clusterId": cluster, "name": name, "source": { "type": "image", "image": "nginx", "reference": "1.27" },
                "replicas": 1, "env": [], "ports": [{ "id": "port_1", "name": "http", "protocol": "HTTP", "containerPort": 80, "exposure": "http" }],
                "domains": domains, "resources": {}, "healthCheck": { "type": "none" },
            })
        };
        let (web, api, docs) = (
            format!("svc_w{run}"),
            format!("svc_a{run}"),
            format!("svc_d{run}"),
        );
        let create = |svc: Value| {
            let (s, user) = (&s, &user);
            async move {
                crate::ops::run_json(s, user, json!({ "op": "createService", "service": svc, "deploymentId": crate::ids::new_id("dpl") })).await
            }
        };
        let save = |svc: Value| {
            let (s, user) = (&s, &user);
            async move {
                crate::ops::run_json(s, user, json!({ "op": "saveService", "service": svc })).await
            }
        };
        let domains_of = |id: String| {
            let db = db.clone();
            async move {
                sqlx::query_scalar::<_, Value>(
                    "SELECT spec->'domains' FROM pyo.services WHERE id = $1",
                )
                .bind(id)
                .fetch_one(&db)
                .await
                .unwrap()
            }
        };
        // web holds the hostname.
        create(service(
            &web,
            "web",
            &project,
            &cluster,
            json!([domain("dom_w", "/")]),
        ))
        .await
        .unwrap();
        // api routes /api of it, stripped: a path of it, with web's HTTPS settings.
        let mut a = domain("dom_a", "/api/");
        a["stripPrefix"] = json!(true);
        a["tlsMode"] = json!("selfsigned");
        a["tunnel"] = json!("on");
        create(service(&api, "api", &project, &cluster, json!([a])))
            .await
            .unwrap();
        let d = domains_of(api.clone()).await;
        assert_eq!(
            (
                d[0]["path"].as_str(),
                d[0]["primary"].as_bool(),
                d[0]["tlsMode"].as_str()
            ),
            (Some("/api"), Some(false), Some("letsencrypt"))
        );
        assert!(d[0].get("tunnel").is_none());
        // Handed out: it joins web's listener.
        let mut conn = db.acquire().await.unwrap();
        let specs = crate::paths::specs(&mut conn, &api, &d).await.unwrap();
        drop(conn);
        assert_eq!(
            (
                specs[0].primary_of.clone(),
                specs[0].path.clone(),
                specs[0].strip_prefix,
                specs[0].tls.clone()
            ),
            (
                format!("{ns}-web"),
                "/api".to_owned(),
                true,
                "letsencrypt".to_owned()
            )
        );
        // The same path again, another cluster, another team, redirects on a path.
        let taken = create(service(
            &docs,
            "docs",
            &project,
            &cluster,
            json!([domain("dom_d", "/api")]),
        ))
        .await
        .unwrap_err();
        assert_eq!(taken.message, format!("{host}/api is already used by api."));
        let far_svc = create(service(
            &format!("svc_f{run}"),
            "far",
            &project,
            &far,
            json!([domain("dom_f", "/far")]),
        ))
        .await
        .unwrap_err();
        assert!(
            far_svc.message.contains("on another cluster"),
            "{}",
            far_svc.message
        );
        let theirs = create(service(
            &format!("svc_o{run}"),
            "other",
            &other_project,
            &format!("cl_o{run}"),
            json!([domain("dom_o", "/x")]),
        ))
        .await
        .unwrap_err();
        assert_eq!(
            theirs.message,
            format!("{host} is already used by another service.")
        );
        let mut r = domain("dom_d", "/docs");
        r["redirects"] = json!([{ "path": "/old", "toPath": "/new" }]);
        let redirected = create(service(&docs, "docs", &project, &cluster, json!([r])))
            .await
            .unwrap_err();
        assert_eq!(
            redirected.message,
            format!("Redirects of {host} are set on web.")
        );
        // A redirect of web where api routes: refused.
        let mut w = domain("dom_w", "/");
        w["redirects"] = json!([{ "path": "/api", "toHost": "elsewhere.example.com" }]);
        let both = save(service(&web, "web", &project, &cluster, json!([w])))
            .await
            .unwrap_err();
        assert!(both.message.contains("routes to api"), "{}", both.message);

        // web's DNS record; then web stops serving the hostname: api takes
        // it over, with the record and web's HTTPS settings.
        q("INSERT INTO pyo.dns_records (service_id, domain_id, team_id, hostname, zone_id, zone_name, status) VALUES ($1, 'dom_w', $2, $3, 'z1', 'example.com', 'created')")
            .bind(&web).bind(&team).bind(&host).execute(&db).await.unwrap();
        save(service(&web, "web", &project, &cluster, json!([])))
            .await
            .unwrap();
        let d = domains_of(api.clone()).await;
        assert!(d[0].get("primary").is_none(), "api holds it: {d}");
        assert_eq!(d[0]["tlsMode"], "letsencrypt");
        let moved: (String, String) =
            sqlx::query_as("SELECT service_id, domain_id FROM pyo.dns_records WHERE team_id = $1")
                .bind(&team)
                .fetch_one(&db)
                .await
                .unwrap();
        assert_eq!(moved, (api.clone(), "dom_a".to_owned()));
        // web comes back at "/": a path of it now. Then api is deleted:
        // web takes it over.
        save(service(
            &web,
            "web",
            &project,
            &cluster,
            json!([domain("dom_w2", "/")]),
        ))
        .await
        .unwrap();
        assert_eq!(domains_of(web.clone()).await[0]["primary"], json!(false));
        crate::ops::run_json(&s, &user, json!({ "op": "deleteService", "id": api }))
            .await
            .unwrap();
        let d = domains_of(web.clone()).await;
        assert!(d[0].get("primary").is_none(), "web holds it again: {d}");
        let moved: (String, String) =
            sqlx::query_as("SELECT service_id, domain_id FROM pyo.dns_records WHERE team_id = $1")
                .bind(&team)
                .fetch_one(&db)
                .await
                .unwrap();
        assert_eq!(moved, (web.clone(), "dom_w2".to_owned()));
    }

    async fn lifecycle() {
        let db = PgPool::connect(&std::env::var("DATABASE_URL").expect("DATABASE_URL"))
            .await
            .unwrap();
        crate::migrate::run(&db).await.unwrap();
        crate::vault::init(crate::vault::Vault::new(
            db.clone(),
            crate::vault::Provider::from_env(),
        ));
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
        let s = AppState {
            db: db.clone(),
            config: std::sync::Arc::new(config),
            observability: crate::observability::Observability::from_env(db.clone()),
            sealer: std::sync::Arc::new(crate::secrets::Sealer::from_env()),
            wakers: Default::default(),
            terminals: Default::default(),
            stopping,
        };
        let run = crate::ids::new_id("t");
        let host = run.replace('_', "-");
        let (team, cluster, project, ns) = (
            format!("team_{run}"),
            format!("cl_{run}"),
            format!("prj_{run}"),
            format!("shop-{host}"),
        );
        let (live, fresh) = (format!("svc_l{run}"), format!("svc_f{run}"));
        let user = User {
            id: format!("usr_{run}"),
            name: "T".into(),
            email: format!("{run}@t.test"),
            timezone: "UTC".into(),
            is_admin: false,
            must_set_up: false,
            key: None,
        };
        let domain = |id: &str, host: &str| json!({ "id": id, "hostname": host, "portName": "http", "status": "pending_dns", "tls": "pending", "tlsMode": "auto", "redirectHttps": true, "createdAt": 1 });
        let service = |id: &str, name: &str, domains: Value| {
            json!({
                "id": id, "projectId": project, "clusterId": cluster, "name": name, "source": { "type": "image", "image": "nginx", "reference": "1.27" },
                "replicas": 1, "env": [], "ports": [{ "id": "port_1", "name": "http", "protocol": "HTTP", "containerPort": 80, "exposure": if domains == json!([]) { "internal" } else { "http" } }],
                "domains": domains, "resources": {}, "healthCheck": { "type": "none" },
            })
        };
        // As stored: without the dashboard's bookkeeping.
        let spec_of = |v: &Value| {
            let mut v = v.clone();
            for k in ["id", "projectId", "clusterId"] {
                v.as_object_mut().unwrap().remove(k);
            }
            v
        };
        let mut tx = db.begin().await.unwrap();
        sqlx::query(
            "INSERT INTO pyo.users (id, email, name, password_hash) VALUES ($1, $2, 'T', '')",
        )
        .bind(&user.id)
        .bind(&user.email)
        .execute(&mut *tx)
        .await
        .unwrap();
        sqlx::query("INSERT INTO pyo.teams (id, name, slug) VALUES ($1, $1, $1)")
            .bind(&team)
            .execute(&mut *tx)
            .await
            .unwrap();
        sqlx::query("INSERT INTO pyo.members (id, team_id, user_id, email, role, status) VALUES ($1, $2, $3, $4, 'developer', 'active')").bind(format!("mem_{run}")).bind(&team).bind(&user.id).bind(&user.email).execute(&mut *tx).await.unwrap();
        sqlx::query("INSERT INTO pyo.clusters (id, team_id, name, agent_token_hash, status, last_seen_at) VALUES ($1, $2, $1, $1, 'connected', now())").bind(&cluster).bind(&team).execute(&mut *tx).await.unwrap();
        sqlx::query(
            "INSERT INTO pyo.projects (id, team_id, name, namespace) VALUES ($1, $2, 'shop', $3)",
        )
        .bind(&project)
        .bind(&team)
        .bind(&ns)
        .execute(&mut *tx)
        .await
        .unwrap();
        // `live` runs, deployed with old.example.com; `fresh` was never deployed.
        let old = service(
            &live,
            "web",
            json!([domain("dom_old", &format!("old-{host}.example.com"))]),
        );
        for (id, name, spec) in [
            (&live, "web", &old),
            (&fresh, "api", &service(&fresh, "api", json!([]))),
        ] {
            sqlx::query("INSERT INTO pyo.services (id, team_id, project_id, cluster_id, name, spec, status) VALUES ($1, $2, $3, $4, $5, $6, 'running')")
                .bind(id).bind(&team).bind(&project).bind(&cluster).bind(name).bind(spec_of(spec)).execute(&mut *tx).await.unwrap();
        }
        let first = format!("dpl_1{run}");
        let deployed =
            crate::deploy::build_spec(&first, &live, &project, &ns, &spec_of(&old), &[]).unwrap();
        sqlx::query("INSERT INTO pyo.deployments (id, team_id, project_id, service_id, cluster_id, status, data, spec) VALUES ($1, $2, $3, $4, $5, 'succeeded', '{}', $6)")
            .bind(&first).bind(&team).bind(&project).bind(&live).bind(&cluster).bind(serde_json::to_value(&deployed).unwrap()).execute(&mut *tx).await.unwrap();
        sqlx::query("UPDATE pyo.services SET current_deployment_id = $2 WHERE id = $1")
            .bind(&live)
            .bind(&first)
            .execute(&mut *tx)
            .await
            .unwrap();
        tx.commit().await.unwrap();

        let seen = std::sync::Mutex::new(0usize);
        let queued = || async {
            let all: Vec<Value> = sqlx::query_scalar(
                "SELECT command FROM pyo.commands WHERE cluster_id = $1 ORDER BY created_at",
            )
            .bind(&cluster)
            .fetch_all(&db)
            .await
            .unwrap();
            let mut n = seen.lock().unwrap();
            let new = all[*n..].to_vec();
            *n = all.len();
            new
        };
        let hosts = |v: &Value| {
            v["domains"]
                .as_array()
                .unwrap()
                .iter()
                .map(|d| d["hostname"].as_str().unwrap().to_owned())
                .collect::<Vec<_>>()
        };
        let row = |id: &str| {
            let db = db.clone();
            let id = id.to_owned();
            async move {
                sqlx::query_as::<_, (i64, bool)>(
                    "SELECT routes_revision, pending_changes FROM pyo.services WHERE id = $1",
                )
                .bind(&id)
                .fetch_one(&db)
                .await
                .unwrap()
            }
        };
        let (new_host, old_host) = (
            format!("new-{host}.example.com"),
            format!("old-{host}.example.com"),
        );

        // A domain added to a running service: live now, no redeploy.
        let both = service(
            &live,
            "web",
            json!([domain("dom_old", &old_host), domain("dom_new", &new_host)]),
        );
        crate::ops::run_json(&s, &user, json!({ "op": "saveService", "service": both }))
            .await
            .unwrap();
        let q = queued().await;
        assert_eq!(q.len(), 1, "{q:?}");
        assert_eq!(q[0]["kind"], "sync_routes");
        assert_eq!(hosts(&q[0]), [old_host.clone(), new_host.clone()]);
        assert_eq!(
            (
                q[0]["routesRevision"].as_i64(),
                q[0]["service"]["serviceId"].as_str()
            ),
            (Some(1), Some(live.as_str()))
        );
        assert_eq!(row(&live).await, (1, false), "no pending change");
        // Its status changing (the DNS watcher, the agent) is no change.
        let mut checked = both.clone();
        checked["domains"][1]["status"] = json!("active");
        crate::ops::run_json(
            &s,
            &user,
            json!({ "op": "saveService", "service": checked }),
        )
        .await
        .unwrap();
        assert!(queued().await.is_empty());
        assert_eq!(row(&live).await, (1, false));

        // A service never deployed: stored only; its first deploy brings it.
        crate::ops::run_json(&s, &user, json!({ "op": "saveService", "service": service(&fresh, "api", json!([domain("dom_f", &format!("api-{host}.example.com"))])) })).await.unwrap();
        assert!(queued().await.is_empty());
        assert_eq!(row(&fresh).await, (1, false));

        // The sync's result: routed, but only while the domains are still that version.
        let mut tx = db.begin().await.unwrap();
        finished(&mut tx, &team, &q[0], true, None).await.unwrap();
        tx.commit().await.unwrap();
        let spec: Value = sqlx::query_scalar("SELECT spec FROM pyo.services WHERE id = $1")
            .bind(&live)
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(
            spec["domains"][1]["status"], "active",
            "the DNS watcher's, kept"
        );
        assert_eq!(
            spec["domains"][0]["status"], "pending_dns",
            "still waiting for DNS"
        );

        // old.example.com removed: gone live too. Sent as the dashboard
        // has the service, with what the API shows of it.
        let mut only_new = service(&live, "web", json!([domain("dom_new", &new_host)]));
        only_new["behindCdn"] = json!(null);
        only_new["wafState"] = json!({ "state": "ready", "message": "" });
        crate::ops::run_json(
            &s,
            &user,
            json!({ "op": "saveService", "service": only_new }),
        )
        .await
        .unwrap();
        let q = queued().await;
        assert_eq!(
            (q.len(), q[0]["kind"].as_str(), hosts(&q[0])),
            (1, Some("sync_routes"), vec![new_host.clone()])
        );
        assert!(!row(&live).await.1, "no pending change");

        // A rollback to the first deployment (deployed with old.example.com)
        // keeps today's domains, and so does the version it falls back to.
        crate::ops::run_json(&s, &user, json!({ "op": "deploy", "serviceId": live, "deploymentId": format!("dpl_2{run}"), "trigger": "rollback", "rollbackOf": first })).await.unwrap();
        let q = queued().await;
        let mut deploy = q
            .iter()
            .find(|c| c["kind"] == "deploy")
            .cloned()
            .expect("a deploy");
        assert_eq!(
            hosts(&deploy),
            [old_host.as_str()],
            "queued as that deployment had it"
        );
        refresh(&db, &mut deploy).await.unwrap();
        assert_eq!(
            (hosts(&deploy), deploy["routesRevision"].as_i64()),
            (vec![new_host.clone()], Some(2))
        );
        assert_eq!(hosts(&deploy["fallback"]), [new_host.as_str()]);
        // A domain added while it's queued: handed out with it.
        let again = service(
            &live,
            "web",
            json!([domain("dom_new", &new_host), domain("dom_old", &old_host)]),
        );
        crate::ops::run_json(&s, &user, json!({ "op": "saveService", "service": again }))
            .await
            .unwrap();
        assert_eq!(
            queued().await[0]["kind"],
            "sync_routes",
            "synced during the rollout too"
        );
        refresh(&db, &mut deploy).await.unwrap();
        assert_eq!(
            (hosts(&deploy), deploy["routesRevision"].as_i64()),
            (vec![new_host.clone(), old_host.clone()], Some(3))
        );
        // An older sync's result doesn't mark the newer domains.
        let mut tx = db.begin().await.unwrap();
        finished(&mut tx, &team, &json!({ "kind": "sync_routes", "service": { "serviceId": live }, "routesRevision": 2 }), true, None).await.unwrap();
        tx.commit().await.unwrap();

        // Its redirect to HTTPS turned off: live, no redeploy.
        let mut plain = service(
            &live,
            "web",
            json!([domain("dom_new", &new_host), domain("dom_old", &old_host)]),
        );
        plain["domains"][0]["redirectHttps"] = json!(false);
        plain["behindCdn"] = json!(null);
        crate::ops::run_json(&s, &user, json!({ "op": "saveService", "service": plain }))
            .await
            .unwrap();
        let q = queued().await;
        assert_eq!(
            (
                q.len(),
                q[0]["kind"].as_str(),
                &q[0]["domains"][0]["redirectHttps"]
            ),
            (1, Some("sync_routes"), &json!(false))
        );
        assert!(!row(&live).await.1, "no pending change");

        // Redirects and header rules: refused while the cluster's agent
        // says it doesn't write them, then live in place, saved tidy.
        sqlx::query(
            "UPDATE pyo.clusters SET agent_version = '0.51.0', capabilities = '{}' WHERE id = $1",
        )
        .bind(&cluster)
        .execute(&db)
        .await
        .unwrap();
        let www_host = format!("www-{host}.example.com");
        let mut ruled = service(
            &live,
            "web",
            json!([domain("dom_new", &new_host), domain("dom_www", &www_host)]),
        );
        ruled["domains"][0]["redirects"] =
            json!([{ "path": "/old/", "toPath": "/new", "status": 302 }]);
        ruled["domains"][0]["headers"] = json!({ "response": { "set": [{ "name": "X-Frame-Options", "value": " DENY " }], "remove": ["X-Powered-By"] } });
        ruled["domains"][1]["portName"] = json!("");
        ruled["domains"][1]["redirects"] = json!([{ "toHost": new_host.to_uppercase() }]);
        let refused =
            crate::ops::run_json(&s, &user, json!({ "op": "saveService", "service": ruled }))
                .await
                .unwrap_err();
        assert!(
            refused.message.contains("Update the agent first"),
            "{}",
            refused.message
        );
        assert!(queued().await.is_empty());
        sqlx::query("UPDATE pyo.clusters SET capabilities = '{\"gatewayRules\": 1}' WHERE id = $1")
            .bind(&cluster)
            .execute(&db)
            .await
            .unwrap();
        crate::ops::run_json(&s, &user, json!({ "op": "saveService", "service": ruled }))
            .await
            .unwrap();
        let q = queued().await;
        assert_eq!((q.len(), q[0]["kind"].as_str()), (1, Some("sync_routes")));
        assert_eq!(
            q[0]["domains"][0]["redirects"],
            json!([{ "path": "/old", "toPath": "/new", "status": 302, "keepPath": true }])
        );
        assert_eq!(
            q[0]["domains"][0]["headers"]["response"]["set"][0]["value"],
            "DENY"
        );
        assert_eq!(
            (
                q[0]["domains"][1]["portName"].as_str(),
                q[0]["domains"][1]["redirects"][0]["toHost"].as_str()
            ),
            (Some(""), Some(new_host.as_str()))
        );
        let spec: Value = sqlx::query_scalar("SELECT spec FROM pyo.services WHERE id = $1")
            .bind(&live)
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(
            spec["domains"][0]["redirects"][0]["path"], "/old",
            "stored tidy"
        );
        // A redirect of the whole domain on one that routes to a port.
        let mut hiding = ruled.clone();
        hiding["domains"][0]["redirects"] = json!([{ "toHost": www_host }]);
        let refused =
            crate::ops::run_json(&s, &user, json!({ "op": "saveService", "service": hiding }))
                .await
                .unwrap_err();
        assert!(
            refused.message.contains("can't redirect as a whole"),
            "{}",
            refused.message
        );
        // A domain with neither a port nor a redirect.
        let mut nowhere = ruled.clone();
        nowhere["domains"][1]["redirects"] = json!([]);
        let refused = crate::ops::run_json(
            &s,
            &user,
            json!({ "op": "saveService", "service": nowhere }),
        )
        .await
        .unwrap_err();
        assert!(
            refused
                .message
                .contains("needs a port to route to, or a redirect"),
            "{}",
            refused.message
        );

        // Every domain removed, so its port is no longer "http": its routes
        // go now, still no redeploy.
        crate::ops::run_json(
            &s,
            &user,
            json!({ "op": "saveService", "service": service(&live, "web", json!([])) }),
        )
        .await
        .unwrap();
        let q = queued().await;
        assert_eq!(
            (q.len(), q[0]["kind"].as_str(), hosts(&q[0]).len()),
            (1, Some("sync_routes"), 0)
        );
        assert!(!row(&live).await.1, "no pending change");
    }
}
