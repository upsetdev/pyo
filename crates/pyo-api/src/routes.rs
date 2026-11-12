//! A service's domains belong to the service, not to a deployment: they're
//! stored with it (`spec.domains`), and a change goes live in place
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
            }
        })
        .collect()
}

/// Whether two `spec.domains` route differently (not just their DNS or
/// certificate status).
pub fn differ(a: &Value, b: &Value) -> bool {
    let v = |d: &Value| serde_json::to_value(domain_specs(d)).unwrap_or(Value::Null);
    v(a) != v(b)
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
    if !sync {
        return Ok(None);
    }
    let Some((cluster_id, target)) = target(tx, service_id).await? else {
        return Ok(None);
    };
    crate::deploy::enqueue(tx, &cluster_id, Command::SyncRoutes(Box::new(target))).await?;
    Ok(Some(cluster_id))
}

/// What a routes sync applies: the live deployment's service and ports (or,
/// before its first rollout is done, that rollout's), its domains, firewall
/// and WAF as they are now. None for a service never deployed (its first
/// deploy brings its domains) or being deleted.
async fn target(
    tx: &mut Transaction<'_, Postgres>,
    service_id: &str,
) -> ApiResult<Option<(String, FirewallTarget)>> {
    let row: Option<(String, Value, Value, i64, bool)> = sqlx::query_as(
        "SELECT a.cluster_id, d.spec, coalesce(a.spec->'domains', '[]'), a.routes_revision, p.network_isolation
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
    let Some((cluster_id, spec, domains, revision, isolation)) = row else {
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
    Ok(Some((
        cluster_id,
        FirewallTarget {
            service,
            project_id: spec["projectId"].as_str().unwrap_or("").to_owned(),
            ports: serde_json::from_value(spec["ports"].clone()).unwrap_or_default(),
            domains: domain_specs(&domains),
            routes_revision: revision,
            network_isolation: isolation,
            firewall,
            waf,
        },
    )))
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

/// One spec or target with its service's current domains and revision.
async fn current(db: &PgPool, o: &mut Value) -> anyhow::Result<()> {
    let Some(id) = o["service"]["serviceId"].as_str() else {
        return Ok(());
    };
    let row: Option<(Value, i64)> = sqlx::query_as(
        "SELECT coalesce(spec->'domains', '[]'), routes_revision FROM pyo.services WHERE id = $1",
    )
    .bind(id)
    .fetch_optional(db)
    .await?;
    if let Some((domains, revision)) = row {
        o["domains"] = serde_json::to_value(domain_specs(&domains)).unwrap_or(json!([]));
        o["routesRevision"] = json!(revision);
    }
    Ok(())
}

/// Its domains route now: each one whose DNS checks out is active, and its
/// certificate issued (an uploaded one), off, or on its way. With
/// `revision`, only while the service's domains are still that version.
pub async fn mark_routed(
    tx: &mut Transaction<'_, Postgres>,
    service_id: &str,
    revision: Option<i64>,
) -> ApiResult<()> {
    sqlx::query(
        "UPDATE pyo.services SET spec = jsonb_set(spec, '{domains}', coalesce((
                  SELECT jsonb_agg(d || jsonb_build_object(
                           -- Ones still waiting for DNS stay so until the DNS watcher sees them resolve.
                           'status', CASE WHEN d->>'status' IN ('pending_dns', 'verifying') THEN d->>'status' ELSE 'active' END, 'tls',
                           CASE WHEN coalesce(d->>'tlsMode', 'auto') = 'off' THEN 'disabled'
                                WHEN d->>'tlsMode' = 'custom' OR d->>'tls' = 'issued' THEN 'issued' ELSE 'pending' END)
                           - CASE WHEN d->>'status' IN ('pending_dns', 'verifying') THEN '' ELSE 'error' END)
                    FROM jsonb_array_elements(spec->'domains') d), '[]'))
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
    }
}

#[cfg(test)]
mod lifecycle_tests {
    use super::*;
    use crate::{AppState, auth::User};

    /// Domains go live in place, and deploys use the service's current ones.
    /// Needs Postgres: DATABASE_URL=… cargo test -p pyo-api -- --ignored routes::lifecycle_tests
    #[test]
    #[ignore]
    fn domain_changes_sync_and_deploys_use_the_current_domains() {
        std::thread::Builder::new()
            .stack_size(64 << 20)
            .spawn(|| {
                tokio::runtime::Builder::new_multi_thread()
                    .enable_all()
                    .build()
                    .unwrap()
                    .block_on(lifecycle())
            })
            .unwrap()
            .join()
            .unwrap();
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
            tailnet_api_url: String::new(),
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

        // old.example.com removed: gone live too.
        let only_new = service(&live, "web", json!([domain("dom_new", &new_host)]));
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
    }
}
