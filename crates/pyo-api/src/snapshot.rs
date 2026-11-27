//! Everything the dashboard shows for the signed-in user, in the dashboard's
//! own shapes (dash/src/lib/pyo/types.ts). Postgres builds most of the JSON.

use serde_json::{Value, json};
use sqlx::{PgConnection, PgPool};

use crate::{auth::User, error::ApiResult};

/// Deployments per team in the snapshot, newest first.
const DEPLOYMENTS_PER_TEAM: i64 = 100;
/// Deployments whose build/rollout log lines are included.
const DEPLOYMENTS_WITH_LOGS: i64 = 30;
const LOG_LINES: i64 = 400;

/// Queries here are constants (two interpolate Pyo's own constants); ids are bound.
async fn values(db: &mut PgConnection, sql: &str, team_ids: &[String]) -> ApiResult<Vec<Value>> {
    Ok(
        sqlx::query_scalar::<_, Value>(sqlx::AssertSqlSafe(sql.to_owned()))
            .bind(team_ids)
            .fetch_all(db)
            .await?,
    )
}

pub async fn build(db: &PgPool, user: &User, agent_api_url: &str) -> ApiResult<Value> {
    // The installation's own settings and the account's keys first, on the
    // pool: the teams' transaction below takes no other connection while
    // it's open, so a busy pool can't leave requests waiting on each other.
    let email = crate::mail::settings(db).await?;
    let github = crate::github::app(db).await?;
    let reached = crate::publicdash::reached(db).await?;
    let public_dashboard = if user.is_admin {
        crate::publicdash::snapshot(db).await?
    } else {
        Value::Null
    };
    let control_backup = if user.is_admin {
        crate::backup::snapshot(db).await?
    } else {
        Value::Null
    };
    let updates = if user.is_admin {
        crate::updates::snapshot(db).await?
    } else {
        Value::Null
    };

    let api_tokens: Vec<Value> = sqlx::query_scalar(
        "SELECT jsonb_build_object('id', id, 'name', name, 'prefix', prefix, 'createdAt', pyo.ms(created_at), 'lastUsedAt', pyo.ms(last_used_at),
                                  'expiresAt', pyo.ms(expires_at), 'access', access, 'teamIds', team_ids)
           FROM pyo.api_tokens WHERE user_id = $1 AND $2 ORDER BY created_at",
    )
    .bind(&user.id)
    // Keys don't list other keys.
    .bind(user.key.is_none())
    .fetch_all(db)
    .await?;
    let mut oauth_grants: Vec<Value> = sqlx::query_scalar(
        "SELECT jsonb_build_object('id', g.id, 'client', c.name, 'createdAt', pyo.ms(g.created_at), 'lastUsedAt', pyo.ms(g.last_used_at), 'scope', g.scope)
           FROM pyo.oauth_grants g JOIN pyo.oauth_clients c ON c.id = g.client_id
          WHERE g.user_id = $1 AND g.refresh_expires_at > now() ORDER BY g.created_at",
    )
    .bind(&user.id)
    .fetch_all(db)
    .await?;
    // What each may do, in API key terms.
    for g in &mut oauth_grants {
        let access = crate::oauth::scope_access(g["scope"].as_str().unwrap_or(""));
        g["access"] = json!(access.as_str());
        if let Some(o) = g.as_object_mut() {
            o.remove("scope");
        }
    }
    // Two-factor: the account's own, and the teams it can't open until
    // it's on (twofactor.rs). The installation's security settings for
    // its admin.
    let two_factor = crate::twofactor::summary(db, &user.id).await?;
    let security_settings = if user.is_admin {
        json!(crate::audit::settings(db).await?)
    } else {
        Value::Null
    };
    // Where admins may sign in from, and where this request comes from.
    let admin_sign_in = if user.is_admin {
        crate::admin_sign_in::snapshot(db, &crate::audit::request_source()).await?
    } else {
        Value::Null
    };
    // Tailscale (Integrations › Tailscale): the client only for admins.
    let tailscale = crate::tailscale::snapshot(db, user.is_admin).await?;
    // Observability: all of it for the installation's admin, only whether
    // it's on for everyone else. Each team gets the analytics limit it picks
    // within (below).
    let observability_settings = crate::observability::settings_for(db, user.is_admin).await?;
    let analytics_months_limit = crate::retention::load(db).await?.analytics_months;

    // The teams' rows through one transaction lowered to the teams the
    // user (or the API key) may see (rls.rs): a query here that forgot its
    // team check still can't show another team's.
    let mut tx = crate::rls::begin(db, user).await?;
    let db: &mut PgConnection = &mut tx;
    let team_ids = crate::rls::team_ids(db).await?;

    let mut teams = values(
        &mut *db,
        "SELECT jsonb_build_object(
            'id', id, 'name', name, 'slug', slug, 'description', description,
            'createdAt', pyo.ms(created_at), 'defaultClusterId', default_cluster_id,
            'preferences', preferences, 'agentUpdates', agent_updates,
            'analyticsMonths', analytics_months,
            'requireTwoFactor', require_two_factor, 'auditIps', audit_ips)
           FROM pyo.teams WHERE id = ANY($1) ORDER BY created_at",
        &team_ids,
    )
    .await?;
    // The longest a team may keep its analytics (the installation's).
    for t in &mut teams {
        t["analyticsMonthsLimit"] = json!(analytics_months_limit);
    }

    let members = values(
        &mut *db,
        "SELECT jsonb_build_object(
            'id', m.id, 'teamId', m.team_id, 'name', coalesce(u.name, ''), 'email', m.email,
            -- Gravatar's key: sha256 of the trimmed, lowercased email.
            'gravatar', encode(sha256(convert_to(lower(trim(m.email)), 'UTF8')), 'hex'),
            'role', m.role, 'status', m.status, 'joinedAt', pyo.ms(m.joined_at),
            'twoFactor', EXISTS (SELECT 1 FROM pyo.user_totp t WHERE t.user_id = m.user_id AND t.enabled_at IS NOT NULL))
           FROM pyo.members m LEFT JOIN pyo.users u ON u.id = m.user_id
          WHERE m.team_id = ANY($1) ORDER BY m.joined_at",
        &team_ids,
    )
    .await?;

    let projects = values(
        &mut *db,
        "SELECT jsonb_build_object(
            'id', id, 'teamId', team_id, 'name', name, 'namespace', namespace,
            'description', description, 'networkIsolation', network_isolation,
            'createdAt', pyo.ms(created_at), 'updatedAt', pyo.ms(updated_at))
           FROM pyo.projects WHERE team_id = ANY($1) ORDER BY created_at",
        &team_ids,
    )
    .await?;

    let mut clusters = values(
        &mut *db,
        "SELECT jsonb_build_object(
            'id', id, 'teamId', team_id, 'name', name, 'description', description, 'tags', to_jsonb(tags),
            'distribution', 'k3s', 'version', version, 'status', status, 'location', location,
            'connection', jsonb_build_object('method', 'agent', 'apiEndpoint', '', 'agentVersion', agent_version),
            -- The cluster Pyo runs on: This machine, in the dashboard
            -- (also while the host's setup is connecting it).
            'sameCluster', same_cluster OR from_host_setup,
            'originPull', origin_pull,
            'nameFromHost', name_from_host,
            -- As the last heartbeat stored them; shaped by `capabilities` below.
            'capabilities', coalesce(capabilities, '{}'::jsonb),
            'trafficMode', coalesce(traffic_mode, 'region'),
            'webProxyProtocol', web_proxy_protocol,
            'publicTraffic', public_traffic,
            -- 'public', 'tailscale' or 'tunnel' (Cloudflare Tunnel).
            'traffic', traffic,
            -- Traffic paths (traffic_paths.rs): the Tailnet switch, TCP and
            -- UDP ports while public ports are off (null: as before), and
            -- public ports waiting for domains to move to the tunnel.
            'tailnetWeb', tailnet_web,
            'publicPortsSet', public_ports,
            'closingPublic', close_public_after IS NOT NULL,
            -- 'public' or 'tailscale' as chosen; freeDomainAddress below
            -- is the one in use (freedomain.rs).
            'freeDomainAddressSet', free_domain_address,
            -- Volumes on it that Pyo has no record of (volumes.rs).
            'strayVolumes', stray_volumes,
            -- Its preview domain, as stored (shaped below: previews/domain.rs).
            'previewDomain', preview_domain,
            'podCidr', coalesce(capabilities->>'podCidr', ''),
            'serviceCidr', coalesce(capabilities->>'serviceCidr', ''),
            'createdAt', pyo.ms(created_at),
            -- Its connect command works until then, unless its agent connected (install.rs).
            'connectExpiresAt', CASE WHEN connect_token_used_at IS NULL THEN pyo.ms(connect_token_expires_at) END,
            -- Its agent's last update (updates.rs), for a hosted Pyo's cluster page.
            'agentUpdate', (SELECT jsonb_build_object('status', m.status, 'message', m.result->>'message', 'at', pyo.ms(m.created_at))
                              FROM pyo.commands m WHERE m.cluster_id = clusters.id AND m.command->>'kind' = 'update_agent'
                             ORDER BY m.created_at DESC LIMIT 1),
            'lastSeenAt', coalesce(pyo.ms(last_seen_at), 0))
           FROM pyo.clusters WHERE team_id = ANY($1) ORDER BY created_at",
        &team_ids,
    )
    .await?;

    // Where the WAF's engine stands on each cluster that has it, and the
    // services whose WAF policy the gateway refused (from the heartbeat),
    // for the services' WAF below.
    let ids: Vec<String> = clusters
        .iter()
        .filter_map(|c| c["id"].as_str().map(str::to_owned))
        .collect();
    let waf_engines = crate::bundles::waf_state(&mut *db, &ids).await?;
    let mut waf_rejected: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut waf_active: std::collections::HashSet<String> = std::collections::HashSet::new();
    let syncing = crate::bundles::syncing_firewall(&mut *db, &ids).await?;
    // Clusters whose databases restart once their install is this Pyo's
    // (the cluster settings say so by the refresh command).
    let databases_restart = crate::bundles::databases_restart(&mut *db, &ids).await?;
    for c in &mut clusters {
        if c["id"]
            .as_str()
            .is_some_and(|id| databases_restart.contains(id))
        {
            c["databasesRestart"] = json!(true);
        }
        waf_rejected.extend(
            c["capabilities"]["wafRejected"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|s| s.as_str().map(str::to_owned)),
        );
        waf_active.extend(
            c["capabilities"]["wafActive"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|s| s.as_str().map(str::to_owned)),
        );
        c["capabilities"] = capabilities(&c["capabilities"]);
    }

    let servers = values(
        &mut *db,
        "SELECT n.report || jsonb_build_object('id', n.id, 'clusterId', n.cluster_id,
                                               'location', coalesce(n.location, c.location), 'locationSource', coalesce(n.location_source, 'cluster'),
                                               'removal', n.removal,
                                               'tags', coalesce((SELECT to_jsonb(t.tags) FROM pyo.server_tags t WHERE t.cluster_id = n.cluster_id AND t.name = n.name), '[]'),
                                               'regularServices', (SELECT t.regular_services FROM pyo.server_tags t WHERE t.cluster_id = n.cluster_id AND t.name = n.name))
           FROM pyo.servers n JOIN pyo.clusters c ON c.id = n.cluster_id
          WHERE c.team_id = ANY($1) ORDER BY n.name",
        &team_ids,
    )
    .await?;
    // Its Cloudflare Tunnel, if it has one.
    crate::cloudflare::tunnel_loop::attach(&mut *db, &team_ids, &mut clusters).await?;
    // Its traffic paths, as the Traffic card's switches show them.
    for c in &mut clusters {
        crate::traffic_paths::shown(c);
    }
    // Its public and Tailscale addresses, and where its free domains point.
    for c in &mut clusters {
        let f = crate::freedomain::Cluster {
            reached: if c["sameCluster"] == json!(true) {
                reached
            } else {
                None
            },
            public_traffic: c["publicTraffic"] != json!(false),
            set: c["freeDomainAddressSet"].as_str().map(str::to_owned),
            lb_ips: crate::freedomain::ips(&c["capabilities"], "loadBalancerIPs"),
            tailnet_web: crate::freedomain::ips(&c["capabilities"], "tailnetWebIps"),
            servers: servers
                .iter()
                .filter(|n| n["clusterId"] == c["id"])
                .cloned()
                .collect(),
            tunnel: c["traffic"] == json!("tunnel"),
            tunnel_domain: c["tunnel"]["defaultDomain"]
                .as_str()
                .filter(|_| c["tunnel"]["state"] != json!("removing"))
                .map(str::to_owned),
        };
        if let (Some(o), Value::Object(shown)) = (c.as_object_mut(), f.shown()) {
            o.extend(shown);
        }
        // Its preview domain: the wildcard record to make, at its public IP.
        let shown = crate::previews::domain::show(&c["previewDomain"], f.public_ip());
        c["previewDomain"] = shown;
    }
    // The tailnet its tailnet domains are on: Tailscale set up on the
    // cluster, and a server of it on the tailnet. And the cluster's
    // Tailscale as the dashboard shows it (never a secret).
    let tailnets: Vec<(String, String, Option<Value>, bool, Value)> = sqlx::query_as(
        "SELECT id, team_id, tailscale,
                EXISTS (SELECT 1 FROM jsonb_array_elements(coalesce(capabilities->'tailnet', '[]')) t WHERE t->>'state' = 'Running'),
                coalesce(capabilities->'tailnet', '[]')
           FROM pyo.clusters WHERE team_id = ANY($1) ORDER BY created_at, id",
    )
    .bind(&team_ids)
    .fetch_all(&mut *db)
    .await?;
    let mut teams_tailnets = std::collections::BTreeMap::new();
    for team in &team_ids {
        if let Some(t) = crate::tailscale::team_tailnet(&mut *db, team).await? {
            teams_tailnets.insert(team.clone(), t);
        }
    }
    for c in &mut clusters {
        let row = tailnets.iter().find(|r| c["id"] == json!(r.0));
        let t = row
            .and_then(|r| r.2.clone())
            .and_then(|v| serde_json::from_value::<crate::tailscale::Tailnet>(v).ok());
        let on = row.is_some_and(|r| r.3);
        c["tailnet"] = json!(t.as_ref().and_then(|t| t.suffix.clone()).filter(|_| on));
        c["tailscale"] = match (row, &t) {
            (Some(r), Some(t)) => crate::tailscale::cluster_view(&r.0, t, teams_tailnets.get(&r.1)),
            _ => Value::Null,
        };
    }
    // Each team's Tailscale (Integrations › Tailscale), with where each of
    // its clusters stands.
    for team in &mut teams {
        let Some(tt) = team["id"].as_str().and_then(|id| teams_tailnets.get(id)) else {
            continue;
        };
        type Mine = (
            String,
            Option<crate::tailscale::Tailnet>,
            Vec<pyo_proto::ServerTailnet>,
        );
        let mine: Vec<Mine> = tailnets
            .iter()
            .filter(|r| r.1 == tt.team_id)
            .map(|r| {
                let t = r.2.clone().and_then(|v| serde_json::from_value(v).ok());
                let servers = serde_json::from_value(r.4.clone()).unwrap_or_default();
                (r.0.clone(), t, servers)
            })
            .collect();
        team["tailscale"] = crate::tailscale::team_view(&mut *db, tt, &mine).await?;
    }

    // Workloads come from each cluster's last heartbeat.
    let workloads = values(
        &mut *db,
        "SELECT w || jsonb_build_object(
            'id', c.id || ':' || (w->>'namespace') || '/' || (w->>'name'),
            'clusterId', c.id,
            'replicas', jsonb_build_object('desired', w->'desired', 'ready', w->'ready'),
            'cpuMillis', w->'cpuMillis', 'memoryMi', w->'memoryMi',
            'projectId', (SELECT a.project_id FROM pyo.services a WHERE a.id = w->>'serviceId'))
           FROM pyo.clusters c, jsonb_array_elements(c.workloads) w
          WHERE c.team_id = ANY($1)",
        &team_ids,
    )
    .await?;

    let services = values(
        &mut *db,
        "SELECT a.spec || jsonb_build_object(
            'id', a.id, 'teamId', a.team_id, 'projectId', a.project_id, 'clusterId', a.cluster_id,
            'name', a.name, 'status', a.status, 'runtime', a.runtime,
            'currentDeploymentId', a.current_deployment_id, 'pendingChanges', a.pending_changes,
            'waf', a.waf, 'behindCdn', a.behind_cdn,
            -- A preview's copy: the service it previews (previews/).
            'previewOf', a.preview_of,
            'createdAt', pyo.ms(a.created_at), 'updatedAt', pyo.ms(a.updated_at))
           -- Being deleted (kept apart below), or why the last delete failed.
           || CASE WHEN a.deleting THEN jsonb_build_object('deleting', true) ELSE '{}' END
           || CASE WHEN a.delete_error IS NULL THEN '{}' ELSE jsonb_build_object('deleteError', a.delete_error) END
           FROM pyo.services a WHERE a.team_id = ANY($1) ORDER BY a.created_at",
        &team_ids,
    )
    .await?;
    let mut services = services;
    crate::cloudflare::attach(&mut *db, &team_ids, &mut services).await?;
    // Each domain's path, and whether it's served (its path is on).
    for a in &mut services {
        if let Some(c) = clusters.iter().find(|c| c["id"] == a["clusterId"]) {
            let paths = crate::traffic_paths::of_shown(c);
            let dd = c["tunnel"]["defaultDomain"].as_str().map(str::to_owned);
            crate::traffic_paths::mark(&mut a["domains"], paths, dd.as_deref());
        }
    }
    // A service with its WAF on: where it stands ("installing" while the
    // engine starts on its cluster, "ready", "failed" with a message).
    for a in &mut services {
        if crate::waf::is_on(&a["waf"]) {
            let engine = a["clusterId"].as_str().and_then(|c| waf_engines.get(c));
            let rejected = a["id"].as_str().is_some_and(|id| waf_rejected.contains(id));
            // Turned on: "Starting" until the sync that points its routes
            // at the WAF is done and the gateway took the policy.
            let starting = a["clusterId"].as_str().is_some_and(|c| syncing.contains(c))
                && !a["id"].as_str().is_some_and(|id| waf_active.contains(id));
            let (state, message) = crate::bundles::waf_shown(engine, rejected, starting);
            a["wafState"] = json!({ "state": state, "message": message });
        }
    }

    let deployments = values(
        &mut *db,
        &format!(
            "WITH recent AS (
               SELECT d.*, row_number() OVER (PARTITION BY d.team_id ORDER BY d.created_at DESC) AS rn
                 FROM pyo.deployments d WHERE d.team_id = ANY($1)
             )
             SELECT r.data || jsonb_build_object(
                 'id', r.id, 'teamId', r.team_id, 'projectId', r.project_id, 'serviceId', r.service_id,
                 'clusterId', r.cluster_id, 'status', r.status,
                 'createdAt', pyo.ms(r.created_at), 'finishedAt', pyo.ms(r.finished_at),
                 'logs', CASE WHEN r.rn <= {DEPLOYMENTS_WITH_LOGS} THEN coalesce((
                   SELECT jsonb_agg(l.j ORDER BY l.id) FROM (
                     SELECT id, jsonb_build_object('id', 'log_' || id, 'at', pyo.ms(at), 'level', level, 'source', source, 'message', message) AS j
                       FROM pyo.deployment_logs WHERE deployment_id = r.id ORDER BY id DESC LIMIT {LOG_LINES}
                   ) l), '[]') ELSE '[]' END)
               FROM recent r WHERE r.rn <= {DEPLOYMENTS_PER_TEAM}
              ORDER BY r.created_at DESC"
        ),
        &team_ids,
    )
    .await?;

    let mut volumes = values(
        &mut *db,
        "SELECT data || jsonb_build_object('id', id, 'teamId', team_id, 'clusterId', cluster_id, 'projectId', project_id, 'serviceId', service_id, 'deleting', deleting, 'createdAt', pyo.ms(created_at))
           FROM pyo.volumes WHERE team_id = ANY($1) ORDER BY created_at",
        &team_ids,
    )
    .await?;
    // Their backups: from the team's settings and runs.
    let mut backups = std::collections::HashMap::new();
    for team in &team_ids {
        backups.extend(crate::teambackup::volume_statuses(&mut *db, team).await?);
    }
    for v in volumes.iter_mut() {
        let id = v["id"].as_str().unwrap_or("");
        v["backup"] = backups
            .remove(id)
            .unwrap_or_else(|| json!({ "status": "unsupported" }));
    }

    let activity = values(
        &mut *db,
        "SELECT data || jsonb_build_object('id', id, 'teamId', team_id, 'at', pyo.ms(at))
           FROM (SELECT *, row_number() OVER (PARTITION BY team_id ORDER BY at DESC) rn FROM pyo.activity WHERE team_id = ANY($1)) x
          WHERE rn <= 100 ORDER BY at DESC",
        &team_ids,
    )
    .await?;

    let builders = values(
        &mut *db,
        "SELECT jsonb_build_object(
            'id', id, 'teamId', team_id, 'name', name, 'kind', kind, 'clusterId', cluster_id, 'server', server,
            'status', status, 'statusMessage', status_message, 'info', info,
            'createdAt', pyo.ms(created_at), 'lastSeenAt', pyo.ms(last_seen_at))
           FROM pyo.builders WHERE team_id = ANY($1) ORDER BY created_at",
        &team_ids,
    )
    .await?;

    let registry_logins = values(
        &mut *db,
        "SELECT jsonb_build_object('id', id, 'teamId', team_id, 'host', host, 'username', username, 'verified', verified,
                                  'createdAt', pyo.ms(created_at), 'lastUsedAt', pyo.ms(last_used_at))
           FROM pyo.registry_logins WHERE team_id = ANY($1) ORDER BY host",
        &team_ids,
    )
    .await?;
    // Without their credentials.
    let secret_stores = values(
        &mut *db,
        "SELECT jsonb_build_object('id', id, 'teamId', team_id, 'kind', kind, 'name', name, 'addr', addr, 'options', options, 'createdAt', pyo.ms(created_at))
           FROM pyo.secret_stores WHERE team_id = ANY($1) ORDER BY created_at",
        &team_ids,
    )
    .await?;
    // Where alerts go, without the token or URL: a hint, the events it
    // gets and how its last send went.
    let topics = crate::alerts::TOPICS
        .iter()
        .map(|t| format!("'{t}'"))
        .collect::<Vec<_>>()
        .join(", ");
    let alert_channels = values(
        &mut *db,
        &format!(
            "SELECT jsonb_build_object('id', id, 'teamId', team_id, 'kind', kind, 'name', name, 'hint', hint,
                                      'events', to_jsonb(ARRAY(SELECT t FROM unnest(ARRAY[{topics}]) t WHERE NOT (t = ANY(muted)))),
                                      'lastSentAt', pyo.ms(last_sent_at), 'lastError', last_error, 'lastErrorAt', pyo.ms(last_error_at),
                                      'createdAt', pyo.ms(created_at))
               FROM pyo.alert_channels WHERE team_id = ANY($1) ORDER BY created_at"
        ),
        &team_ids,
    )
    .await?;
    let firewall_rules = values(
        &mut *db,
        "SELECT jsonb_build_object('id', id, 'teamId', team_id, 'serviceId', service_id, 'action', action, 'operator', operator,
                                  'sources', to_jsonb(sources), 'requests', requests, 'per', per,
                                  'note', note, 'createdBy', created_by, 'createdAt', pyo.ms(created_at))
           FROM pyo.firewall_rules WHERE team_id = ANY($1) ORDER BY position, created_at, id",
        &team_ids,
    )
    .await?;
    let waf_rules = values(
        &mut *db,
        "SELECT jsonb_build_object('id', id, 'teamId', team_id, 'serviceId', service_id, 'number', number, 'name', name,
                                  'enabled', enabled, 'position', position, 'match', matching, 'conditions', conditions, 'action', action,
                                  'createdBy', created_by, 'createdAt', pyo.ms(created_at))
           FROM pyo.waf_rules WHERE team_id = ANY($1) ORDER BY service_id, position, created_at, id",
        &team_ids,
    )
    .await?;
    // Public material only: the private keys were never kept.
    let origin_pull_certificates = values(
        &mut *db,
        "SELECT jsonb_build_object('id', id, 'teamId', team_id, 'name', name, 'commonName', common_name, 'caCertificate', ca_pem,
                                  'fingerprint', fingerprint, 'notAfter', pyo.ms(not_after), 'createdAt', pyo.ms(created_at))
           FROM pyo.origin_pull_certificates WHERE team_id = ANY($1) ORDER BY created_at",
        &team_ids,
    )
    .await?;
    let certificates = values(
        &mut *db,
        "SELECT jsonb_build_object('id', id, 'teamId', team_id, 'name', name, 'hostnames', hostnames, 'issuer', issuer,
                                  'notBefore', pyo.ms(not_before), 'notAfter', pyo.ms(not_after), 'fingerprint', fingerprint,
                                  'createdAt', pyo.ms(created_at), 'updatedAt', pyo.ms(updated_at))
           FROM pyo.certificates WHERE team_id = ANY($1) ORDER BY created_at",
        &team_ids,
    )
    .await?;
    // Hooks without their tokens; RevealDeployHook gives the URL.
    let deploy_hooks = values(
        &mut *db,
        "SELECT jsonb_build_object('id', id, 'teamId', team_id, 'projectId', project_id, 'serviceId', service_id, 'name', name,
                'createdBy', created_by, 'createdAt', pyo.ms(created_at), 'lastUsedAt', pyo.ms(last_used_at))
           FROM pyo.deploy_hooks WHERE team_id = ANY($1) ORDER BY created_at",
        &team_ids,
    )
    .await?;
    let mut github_installations = values(
        &mut *db,
        "SELECT jsonb_build_object('id', id, 'teamId', team_id, 'account', account, 'accountType', account_type,
                'repositorySelection', repository_selection, 'createdAt', pyo.ms(created_at),
                'permissions', permissions, 'events', events)
           FROM pyo.github_installations WHERE team_id = ANY($1) ORDER BY account",
        &team_ids,
    )
    .await?;
    // What previews still need approved on GitHub, and where.
    for i in github_installations.iter_mut() {
        crate::previews::github::show_installation(i);
    }

    // Git connections, never their tokens.
    let git_connections = values(
        &mut *db,
        "SELECT jsonb_build_object('id', c.id, 'teamId', c.team_id, 'provider', c.provider, 'url', c.base_url,
                'account', c.account, 'publicUrl', c.public_url, 'info', c.info, 'createdAt', pyo.ms(c.created_at),
                'webhooks', coalesce((SELECT jsonb_agg(jsonb_build_object('repo', w.repo, 'active', w.hook_id IS NOT NULL, 'error', w.error) ORDER BY w.repo)
                                       FROM pyo.git_webhooks w WHERE w.connection_id = c.id), '[]'::jsonb))
           FROM pyo.git_connections c WHERE c.team_id = ANY($1) ORDER BY c.created_at",
        &team_ids,
    )
    .await?;

    // Which keys are saved, never the keys themselves.
    // Teams' cloud providers (Integrations › Hetzner, the token's end only)
    // and the servers Pyo made there.
    let (cloud_connections, cloud_servers) = crate::cloud::snapshot(&mut *db, &team_ids).await?;

    // Gravatar's key for the profile picture: sha256 of the trimmed, lowercased email.
    let mut me = json!(user);
    me["gravatar"] = json!(crate::ids::sha256_hex(&user.email.trim().to_lowercase()));
    me["twoFactor"] = two_factor;
    let databases = crate::databases::listing(&mut *db, &team_ids, None).await?;
    let previews = crate::previews::snapshot(&mut *db, &team_ids).await?;
    // Sandbox servers the teams can use (theirs and the installation's).
    let sandbox_servers = crate::sandbox::server_views(&mut *db, &team_ids, None).await?;
    // Cloudflare accounts: their zones, never the token.
    let cloudflare = crate::cloudflare::snapshot(&mut *db, user, &team_ids).await?;
    // Each team's backups of its volumes and databases.
    let team_backups = crate::teambackup::snapshot(&mut *db, &team_ids).await?;
    tx.commit().await?;
    // Sealed values open through the vault's own connections, now that
    // the transaction is done.
    let services = for_dashboard(services).await?;
    // Services being deleted, apart: shown as such until the agent says
    // they're gone (or brings them back with why not). Scripts and
    // assistants read `services` as before.
    let (deleting_services, services): (Vec<Value>, Vec<Value>) = services
        .into_iter()
        .partition(|a| a["deleting"] == json!(true));
    // How teams' values are encrypted; the security overview.
    let encryption = crate::vault::get().status(&team_ids).await?;

    let mut snapshot = json!({
        "user": me,
        "teams": teams,
        "members": members,
        "projects": projects,
        "clusters": clusters,
        // Databases (never their logins).
        "databases": databases,
        "servers": servers,
        "systemWorkloads": workloads,
        "services": services,
        "deployments": deployments,
        // Previews of pull requests and merge requests (previews/).
        "previews": previews,
        "volumes": volumes,
        "activity": activity,
        "builders": builders,
        "tailscale": tailscale,
        "cloudConnections": cloud_connections,
        "cloudServers": cloud_servers,
        // None: the dashboard hides sandboxes.
        "sandboxServers": sandbox_servers,
        // Pyo's dashboard on the public IP: the installation's admin only.
        "publicDashboard": public_dashboard,
        "cloudflare": cloudflare,
        "agentApiUrl": agent_api_url,
        // Whether email goes out; the settings themselves only for admins.
        // Observability: whether it's on, the rest for the installation's
        // admin only.
        "observabilitySettings": observability_settings,
        // Two-factor for everyone, and how long the audit log is kept: the
        // installation's admin only.
        "securitySettings": security_settings,
        "email": email.map(|e| if user.is_admin { e.public() } else { json!({ "fromEmail": e.from_email }) }),
        "github": github.map(|g| g.public()),
        "githubInstallations": github_installations,
        "gitConnections": git_connections,
        "deployHooks": deploy_hooks,
        // Your own API tokens: names and prefixes, never the tokens.
        "apiTokens": api_tokens,
        // Private registry logins, without their passwords.
        "registryLogins": registry_logins,
        // Uploaded certificates, without their keys.
        "certificates": certificates,
        "originPullCertificates": origin_pull_certificates,
        "firewallRules": firewall_rules,
        "wafRules": waf_rules,
        "secretStores": secret_stores,
        "alertChannels": alert_channels,
        // Assistants you connected over OAuth (MCP).
        "oauthGrants": oauth_grants,
        "encryption": encryption,
        "teamBackups": team_backups,
        // Backups of Pyo's own data: the installation's admin only.
        "controlBackup": control_backup,
        "updates": updates,
    });
    // Services being deleted (set here: the object above is at json!'s
    // recursion limit).
    snapshot["deletingServices"] = json!(deleting_services);
    // Where admins may sign in from: the installation's admin only.
    snapshot["adminSignIn"] = admin_sign_in;
    Ok(snapshot)
}

/// A cluster's capabilities for the dashboard, from what its last heartbeat
/// stored (agent.rs `heartbeat`): defaults where an agent left one out.
fn capabilities(stored: &Value) -> Value {
    let or = |k: &str, default: Value| {
        stored
            .get(k)
            .filter(|v| !v.is_null())
            .cloned()
            .unwrap_or(default)
    };
    json!({
        "ingressController": or("ingressController", json!("none")),
        "gatewayError": or("gatewayError", Value::Null),
        // Servers where the gateway can't get its ports: {server, ports, by}.
        "gatewayPortsTaken": or("gatewayPortsTaken", json!([])),
        "loadBalancer": or("loadBalancer", json!("none")),
        "loadBalancerIPs": or("loadBalancerIps", json!([])),
        "certManager": or("certManager", json!(false)),
        "metricsServer": or("metricsServer", json!(false)),
        "storageClasses": or("storageClasses", json!([])),
        "security": or("security", Value::Null),
        "gpu": or("gpu", Value::Null),
        "trafficModes": or("trafficModes", json!(false)),
        "trafficLocal": or("trafficLocal", json!(false)),
        "tailnetWebIps": or("tailnetWebIps", json!([])),
        "agentScope": or("agentScope", Value::Null),
        // Whether the agent can update Pyo and itself (updates.rs).
        "updates": or("updates", json!(0)),
        // Whether its install is this Pyo's (install.rs): the cluster
        // settings' "Updating" and "Update needed".
        "agentInstall": or("agentInstall", Value::Null),
        // Its agent runs Cloudflare Tunnel connectors.
        "tunnel": or("tunnel", json!(false)),
    })
}

/// Env values are sealed at rest. Plain ones are opened for the dashboard;
/// secret ones never leave the API: blank, with `hasValue` saying one is set.
async fn for_dashboard(mut services: Vec<Value>) -> ApiResult<Vec<Value>> {
    for service in services.iter_mut() {
        crate::template_source::show(service);
        live_without_error(service);
        let service_id = service["id"].clone();
        // The service's variables, then its previews' (the same rules).
        if let Some(env) = service.get_mut("env").and_then(Value::as_array_mut) {
            shown_env(env, &service_id).await;
        }
        if let Some(env) = service
            .get_mut("previews")
            .and_then(|p| p.get_mut("env"))
            .and_then(Value::as_array_mut)
        {
            shown_env(env, &service_id).await;
        }
    }
    Ok(services)
}

/// A live domain has no error to show: one left from before it went live
/// (a DNS check's, before it moved to the Cloudflare Tunnel) is stale.
fn live_without_error(service: &mut Value) {
    for d in service["domains"].as_array_mut().into_iter().flatten() {
        if d["status"] == json!("active")
            && let Some(o) = d.as_object_mut()
        {
            o.remove("error");
        }
    }
}

/// Variables as the dashboard gets them: a secret's value never (only that
/// it has one), the others opened.
async fn shown_env(env: &mut [Value], service_id: &Value) {
    let vault = crate::vault::get();
    for e in env.iter_mut() {
        let value = e["value"].as_str().unwrap_or("").to_owned();
        if e["secret"] == json!(true) {
            e["hasValue"] = json!(!value.is_empty());
            e["value"] = json!("");
        } else if crate::vault::Vault::is_sealed(&value) {
            // One unreadable value mustn't take the whole dashboard down.
            match vault.open(&value).await {
                Ok(v) => e["value"] = json!(v),
                Err(err) => {
                    tracing::error!(service = %service_id, key = %e["key"], error = %err, "can't open an environment variable");
                    e["value"] = json!("");
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_live_domain_shows_no_stale_error() {
        let mut service = json!({ "domains": [
            { "hostname": "a.example.com", "status": "active", "tunnel": "on", "error": "a.example.com doesn't resolve yet." },
            { "hostname": "b.example.com", "status": "pending_dns", "error": "b.example.com doesn't resolve yet." },
        ] });
        live_without_error(&mut service);
        assert!(service["domains"][0].get("error").is_none());
        assert!(service["domains"][1]["error"].is_string());
    }

    #[test]
    fn capabilities_pass_the_install_and_updates_through() {
        let install = json!({ "state": "outdated", "message": "Run this on the cluster.", "command": "curl … | sh" });
        let c = capabilities(
            &json!({ "ingressController": "envoy", "loadBalancerIps": ["1.2.3.4"], "certManager": true, "updates": 2, "agentInstall": install }),
        );
        assert_eq!(c["agentInstall"], install);
        assert_eq!(c["updates"], 2);
        assert_eq!(c["ingressController"], "envoy");
        assert_eq!(c["loadBalancerIPs"], json!(["1.2.3.4"]));
        assert_eq!(c["certManager"], true);
        let taken = json!([{ "server": "server-1", "ports": [80, 443], "by": "Traefik" }]);
        let c = capabilities(&json!({ "gatewayPortsTaken": taken }));
        assert_eq!(c["gatewayPortsTaken"], taken);
    }

    #[test]
    fn capabilities_default_what_an_agent_left_out() {
        let c = capabilities(&json!({}));
        assert_eq!(c["ingressController"], "none");
        assert_eq!(c["loadBalancer"], "none");
        assert_eq!(c["loadBalancerIPs"], json!([]));
        assert_eq!(c["storageClasses"], json!([]));
        assert_eq!(c["metricsServer"], false);
        assert_eq!(c["updates"], 0);
        assert!(c["agentInstall"].is_null());
        assert!(c["agentScope"].is_null());
        assert_eq!(c["gatewayPortsTaken"], json!([]));
    }
}
