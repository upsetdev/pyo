//! Everything the dashboard shows for the signed-in user, in the dashboard's
//! own shapes (dash/src/lib/pyo/types.ts). Postgres builds most of the JSON.

use serde_json::{Value, json};
use sqlx::PgPool;

use crate::{auth::User, error::ApiResult};

/// Deployments per team in the snapshot, newest first.
const DEPLOYMENTS_PER_TEAM: i64 = 100;
/// Deployments whose build/rollout log lines are included.
const DEPLOYMENTS_WITH_LOGS: i64 = 30;
const LOG_LINES: i64 = 400;

/// Queries here are constants (one interpolates numeric limits); ids are bound.
async fn values(db: &PgPool, sql: &str, team_ids: &[String]) -> ApiResult<Vec<Value>> {
    Ok(
        sqlx::query_scalar::<_, Value>(sqlx::AssertSqlSafe(sql.to_owned()))
            .bind(team_ids)
            .fetch_all(db)
            .await?,
    )
}

pub async fn build(db: &PgPool, user: &User, agent_api_url: &str) -> ApiResult<Value> {
    let email = crate::mail::settings(db).await?;
    let github = crate::github::app(db).await?;
    let mut team_ids: Vec<String> = sqlx::query_scalar(
        "SELECT team_id FROM pyo.members WHERE user_id = $1 AND status = 'active'",
    )
    .bind(&user.id)
    .fetch_all(db)
    .await?;
    // An API key for some teams sees only those.
    if let Some(allowed) = user.key.as_ref().and_then(|k| k.teams.as_ref()) {
        team_ids.retain(|t| allowed.contains(t));
    }

    let teams = values(
        db,
        "SELECT jsonb_build_object(
            'id', id, 'name', name, 'slug', slug, 'description', description,
            'createdAt', pyo.ms(created_at), 'defaultClusterId', default_cluster_id,
            'preferences', preferences)
           FROM pyo.teams WHERE id = ANY($1) ORDER BY created_at",
        &team_ids,
    )
    .await?;

    let members = values(
        db,
        "SELECT jsonb_build_object(
            'id', m.id, 'teamId', m.team_id, 'name', coalesce(u.name, ''), 'email', m.email,
            -- Gravatar's key: sha256 of the trimmed, lowercased email.
            'gravatar', encode(sha256(convert_to(lower(trim(m.email)), 'UTF8')), 'hex'),
            'role', m.role, 'status', m.status, 'joinedAt', pyo.ms(m.joined_at))
           FROM pyo.members m LEFT JOIN pyo.users u ON u.id = m.user_id
          WHERE m.team_id = ANY($1) ORDER BY m.joined_at",
        &team_ids,
    )
    .await?;

    let projects = values(
        db,
        "SELECT jsonb_build_object(
            'id', id, 'teamId', team_id, 'name', name, 'namespace', namespace,
            'description', description, 'networkIsolation', network_isolation,
            'createdAt', pyo.ms(created_at), 'updatedAt', pyo.ms(updated_at))
           FROM pyo.projects WHERE team_id = ANY($1) ORDER BY created_at",
        &team_ids,
    )
    .await?;

    let mut clusters = values(
        db,
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
            -- Volumes on it that Pyo has no record of (volumes.rs).
            'strayVolumes', stray_volumes,
            'podCidr', coalesce(capabilities->>'podCidr', ''),
            'serviceCidr', coalesce(capabilities->>'serviceCidr', ''),
            'createdAt', pyo.ms(created_at),
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
    let waf_engines = crate::bundles::waf_state(db, &ids).await?;
    let mut waf_rejected: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut waf_active: std::collections::HashSet<String> = std::collections::HashSet::new();
    let syncing = crate::bundles::syncing_firewall(db, &ids).await?;
    for c in &mut clusters {
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
        db,
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

    // Workloads come from each cluster's last heartbeat.
    let workloads = values(
        db,
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
        db,
        "SELECT a.spec || jsonb_build_object(
            'id', a.id, 'teamId', a.team_id, 'projectId', a.project_id, 'clusterId', a.cluster_id,
            'name', a.name, 'status', a.status, 'runtime', a.runtime,
            'currentDeploymentId', a.current_deployment_id, 'pendingChanges', a.pending_changes,
            'waf', a.waf,
            'createdAt', pyo.ms(a.created_at), 'updatedAt', pyo.ms(a.updated_at))
           FROM pyo.services a WHERE a.team_id = ANY($1) AND NOT a.deleting ORDER BY a.created_at",
        &team_ids,
    )
    .await?;
    let mut services = for_dashboard(services).await?;
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
        db,
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
        db,
        "SELECT data || jsonb_build_object('id', id, 'teamId', team_id, 'clusterId', cluster_id, 'projectId', project_id, 'serviceId', service_id, 'deleting', deleting, 'createdAt', pyo.ms(created_at))
           FROM pyo.volumes WHERE team_id = ANY($1) ORDER BY created_at",
        &team_ids,
    )
    .await?;
    // Their backups: from the team's settings and runs.
    let mut backups = std::collections::HashMap::new();
    for team in &team_ids {
        backups.extend(crate::teambackup::volume_statuses(db, team).await?);
    }
    for v in volumes.iter_mut() {
        let id = v["id"].as_str().unwrap_or("");
        v["backup"] = backups
            .remove(id)
            .unwrap_or_else(|| json!({ "status": "unsupported" }));
    }

    let activity = values(
        db,
        "SELECT data || jsonb_build_object('id', id, 'teamId', team_id, 'at', pyo.ms(at))
           FROM (SELECT *, row_number() OVER (PARTITION BY team_id ORDER BY at DESC) rn FROM pyo.activity WHERE team_id = ANY($1)) x
          WHERE rn <= 100 ORDER BY at DESC",
        &team_ids,
    )
    .await?;

    let builders = values(
        db,
        "SELECT jsonb_build_object(
            'id', id, 'teamId', team_id, 'name', name, 'kind', kind, 'clusterId', cluster_id, 'server', server,
            'status', status, 'statusMessage', status_message, 'info', info,
            'createdAt', pyo.ms(created_at), 'lastSeenAt', pyo.ms(last_seen_at))
           FROM pyo.builders WHERE team_id = ANY($1) ORDER BY created_at",
        &team_ids,
    )
    .await?;

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
    let registry_logins = values(
        db,
        "SELECT jsonb_build_object('id', id, 'teamId', team_id, 'host', host, 'username', username, 'verified', verified,
                                  'createdAt', pyo.ms(created_at), 'lastUsedAt', pyo.ms(last_used_at))
           FROM pyo.registry_logins WHERE team_id = ANY($1) ORDER BY host",
        &team_ids,
    )
    .await?;
    // Without their credentials.
    let secret_stores = values(
        db,
        "SELECT jsonb_build_object('id', id, 'teamId', team_id, 'kind', kind, 'name', name, 'addr', addr, 'options', options, 'createdAt', pyo.ms(created_at))
           FROM pyo.secret_stores WHERE team_id = ANY($1) ORDER BY created_at",
        &team_ids,
    )
    .await?;
    let firewall_rules = values(
        db,
        "SELECT jsonb_build_object('id', id, 'teamId', team_id, 'serviceId', service_id, 'action', action, 'operator', operator,
                                  'sources', to_jsonb(sources), 'requests', requests, 'per', per,
                                  'note', note, 'createdBy', created_by, 'createdAt', pyo.ms(created_at))
           FROM pyo.firewall_rules WHERE team_id = ANY($1) ORDER BY position, created_at, id",
        &team_ids,
    )
    .await?;
    let waf_rules = values(
        db,
        "SELECT jsonb_build_object('id', id, 'teamId', team_id, 'serviceId', service_id, 'number', number, 'name', name,
                                  'enabled', enabled, 'position', position, 'match', matching, 'conditions', conditions, 'action', action,
                                  'createdBy', created_by, 'createdAt', pyo.ms(created_at))
           FROM pyo.waf_rules WHERE team_id = ANY($1) ORDER BY service_id, position, created_at, id",
        &team_ids,
    )
    .await?;
    // Public material only: the private keys were never kept.
    let origin_pull_certificates = values(
        db,
        "SELECT jsonb_build_object('id', id, 'teamId', team_id, 'name', name, 'commonName', common_name, 'caCertificate', ca_pem,
                                  'fingerprint', fingerprint, 'notAfter', pyo.ms(not_after), 'createdAt', pyo.ms(created_at))
           FROM pyo.origin_pull_certificates WHERE team_id = ANY($1) ORDER BY created_at",
        &team_ids,
    )
    .await?;
    let certificates = values(
        db,
        "SELECT jsonb_build_object('id', id, 'teamId', team_id, 'name', name, 'hostnames', hostnames, 'issuer', issuer,
                                  'notBefore', pyo.ms(not_before), 'notAfter', pyo.ms(not_after), 'fingerprint', fingerprint,
                                  'createdAt', pyo.ms(created_at), 'updatedAt', pyo.ms(updated_at))
           FROM pyo.certificates WHERE team_id = ANY($1) ORDER BY created_at",
        &team_ids,
    )
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
        g["access"] = json!(match access {
            crate::auth::KeyAccess::Read => "read",
            crate::auth::KeyAccess::Deploy => "deploy",
            crate::auth::KeyAccess::Full => "full",
        });
        if let Some(o) = g.as_object_mut() {
            o.remove("scope");
        }
    }
    // Hooks without their tokens; RevealDeployHook gives the URL.
    let deploy_hooks = values(
        db,
        "SELECT jsonb_build_object('id', id, 'teamId', team_id, 'projectId', project_id, 'serviceId', service_id, 'name', name,
                'createdBy', created_by, 'createdAt', pyo.ms(created_at), 'lastUsedAt', pyo.ms(last_used_at))
           FROM pyo.deploy_hooks WHERE team_id = ANY($1) ORDER BY created_at",
        &team_ids,
    )
    .await?;
    let github_installations = values(
        db,
        "SELECT jsonb_build_object('id', id, 'teamId', team_id, 'account', account, 'accountType', account_type,
                'repositorySelection', repository_selection, 'createdAt', pyo.ms(created_at))
           FROM pyo.github_installations WHERE team_id = ANY($1) ORDER BY account",
        &team_ids,
    )
    .await?;

    // Which keys are saved, never the keys themselves.
    let integrations = values(
        db,
        "SELECT jsonb_build_object('teamId', team_id, 'kind', kind, 'hint', hint, 'createdAt', pyo.ms(created_at))
           FROM pyo.integrations WHERE team_id = ANY($1)",
        &team_ids,
    )
    .await?;

    // Gravatar's key for the profile picture: sha256 of the trimmed, lowercased email.
    let mut me = json!(user);
    me["gravatar"] = json!(crate::ids::sha256_hex(&user.email.trim().to_lowercase()));
    let databases = crate::databases::listing(db, &team_ids, None).await?;
    let observability_settings = json!({ "ipMode": crate::observability::ip_mode(db).await? });
    Ok(json!({
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
        "volumes": volumes,
        "activity": activity,
        "builders": builders,
        "integrations": integrations,
        "agentApiUrl": agent_api_url,
        // Whether email goes out; the settings themselves only for admins.
        // What analytics keeps of visitors' IPs (installation-wide).
        "observabilitySettings": observability_settings,
        "email": email.map(|e| if user.is_admin { e.public() } else { json!({ "fromEmail": e.from_email }) }),
        "github": github.map(|g| g.public()),
        "githubInstallations": github_installations,
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
        // Assistants you connected over OAuth (MCP).
        "oauthGrants": oauth_grants,
        // How teams' values are encrypted; the security overview.
        "encryption": crate::vault::get().status(&team_ids).await?,
        // Each team's backups of its volumes and databases.
        "teamBackups": crate::teambackup::snapshot(db, &team_ids).await?,
        // Backups of Pyo's own data: the installation's admin only.
        "controlBackup": if user.is_admin { crate::backup::snapshot(db).await? } else { Value::Null },
        "updates": if user.is_admin { crate::updates::snapshot(db).await? } else { Value::Null },
    }))
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
        "loadBalancer": or("loadBalancer", json!("none")),
        "loadBalancerIPs": or("loadBalancerIps", json!([])),
        "certManager": or("certManager", json!(false)),
        "metricsServer": or("metricsServer", json!(false)),
        "storageClasses": or("storageClasses", json!([])),
        "security": or("security", Value::Null),
        "gpu": or("gpu", Value::Null),
        "trafficModes": or("trafficModes", json!(false)),
        "trafficLocal": or("trafficLocal", json!(false)),
        "agentScope": or("agentScope", Value::Null),
        // Whether the agent can update Pyo and itself (updates.rs).
        "updates": or("updates", json!(0)),
        // Whether its install is this Pyo's (install.rs): the cluster
        // settings' "Updating" and "Update needed".
        "agentInstall": or("agentInstall", Value::Null),
    })
}

/// Env values are sealed at rest. Plain ones are opened for the dashboard;
/// secret ones never leave the API: blank, with `hasValue` saying one is set.
async fn for_dashboard(mut services: Vec<Value>) -> ApiResult<Vec<Value>> {
    let vault = crate::vault::get();
    for service in services.iter_mut() {
        crate::template_source::show(service);
        let service_id = service["id"].clone();
        let Some(env) = service.get_mut("env").and_then(Value::as_array_mut) else {
            continue;
        };
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
    Ok(services)
}

#[cfg(test)]
mod tests {
    use super::*;

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
    }
}
