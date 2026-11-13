//! Endpoints the in-cluster agent dials out to: heartbeats with what it sees,
//! a long-poll for commands, and reports on commands and rollouts.

use std::time::Duration;

use axum::{
    Json, Router,
    extract::{FromRequestParts, Path, Query, State},
    http::{StatusCode, request::Parts},
    routing::{get, post},
};
use pyo_proto::{CommandEnvelope, CommandResult, DeploymentProgress, Heartbeat, PROTOCOL_VERSION};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{
    AppState, deploy,
    error::{ApiError, ApiResult},
    ids::{new_id, sha256_hex},
    ops::log_activity,
};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/heartbeat", post(heartbeat))
        .route("/commands", get(poll))
        .route("/commands/{id}", post(command_result))
        .route("/deployments/{id}/progress", post(progress))
        .route("/access-logs", post(access_logs))
        .route("/host-metrics", post(host_metrics))
        .route("/certificates/{id}", get(certificate))
        .route("/registry-logins/{id}", get(registry_login))
}

/// The cluster an agent's bearer token belongs to.
pub struct AgentCluster {
    pub id: String,
    pub team_id: String,
}

impl FromRequestParts<AppState> for AgentCluster {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let token = parts
            .headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .ok_or_else(|| ApiError::new(StatusCode::UNAUTHORIZED, "Missing agent token."))?;
        let row: Option<(String, String)> =
            sqlx::query_as("SELECT id, team_id FROM pyo.clusters WHERE agent_token_hash = $1")
                .bind(sha256_hex(token))
                .fetch_optional(&state.db)
                .await?;
        let (id, team_id) = row.ok_or_else(|| {
            ApiError::new(
                StatusCode::UNAUTHORIZED,
                "Unknown agent token. Was the cluster removed or the token rotated?",
            )
        })?;
        Ok(AgentCluster { id, team_id })
    }
}

/// An uploaded certificate with its key, for a cluster of the team that owns it.
async fn certificate(
    State(s): State<AppState>,
    cluster: AgentCluster,
    Path(id): Path<String>,
) -> ApiResult<Json<pyo_proto::TlsCertificate>> {
    let row: Option<(String, String)> = sqlx::query_as(
        "SELECT chain_pem, key_sealed FROM pyo.certificates WHERE id = $1 AND team_id = $2",
    )
    .bind(&id)
    .bind(&cluster.team_id)
    .fetch_optional(&s.db)
    .await?;
    let (certificate, sealed) = row.ok_or_else(|| ApiError::not_found("Certificate"))?;
    let private_key = s.sealer.open(&sealed).map_err(ApiError::from)?;
    Ok(Json(pyo_proto::TlsCertificate {
        certificate,
        private_key,
    }))
}

/// A registry login with its password, for a cluster of the team that owns it.
async fn registry_login(
    State(s): State<AppState>,
    cluster: AgentCluster,
    Path(id): Path<String>,
) -> ApiResult<Json<pyo_proto::RegistryLogin>> {
    let row: Option<(String, String, String)> = sqlx::query_as("UPDATE pyo.registry_logins SET last_used_at = now() WHERE id = $1 AND team_id = $2 RETURNING host, username, secret_sealed")
        .bind(&id)
        .bind(&cluster.team_id)
        .fetch_optional(&s.db)
        .await?;
    let (host, username, sealed) = row.ok_or_else(|| ApiError::not_found("Registry login"))?;
    let password = s.sealer.open(&sealed).map_err(ApiError::from)?;
    Ok(Json(pyo_proto::RegistryLogin {
        host,
        username,
        password,
    }))
}

fn storage_class_json(sc: &pyo_proto::StorageClassReport) -> Value {
    json!({
        "name": sc.name,
        "provisioner": sc.provisioner,
        "isDefault": sc.is_default,
        // local-path keeps data on one node; anything else is assumed replicated.
        "replicated": sc.provisioner != "rancher.io/local-path",
        "reclaimPolicy": sc.reclaim_policy,
        "volumeBindingMode": sc.volume_binding_mode,
        "allowVolumeExpansion": sc.allow_volume_expansion,
    })
}

/// Whether a heartbeat's `host_proof` shows the agent runs where the host's
/// bootstrap key is (`PYO_BOOTSTRAP_KEY`, the pyo-bootstrap Secret on Pyo's
/// own cluster), for its own token. Pyo's store logins, "This machine" and
/// Pyo's updates go by this, never by what an agent says.
fn proves_host(bootstrap_key: Option<&str>, proof: Option<&str>, token_hash: &str) -> bool {
    let key = bootstrap_key.unwrap_or("").trim();
    match proof {
        Some(p) if key.len() >= 32 => crate::util::constant_time_eq(
            p.as_bytes(),
            pyo_proto::host_proof(key, token_hash).as_bytes(),
        ),
        _ => false,
    }
}

/// A service's runtime and status from a heartbeat ($1 id, $2 cluster, $3
/// runtime, $4 desired, $5 ready): the status only when no rollout owns it,
/// and "restoring" while a volume's restore holds it (stopped, then
/// starting again).
const SERVICE_STATUS: &str = "UPDATE pyo.services SET runtime = $3,
        status = CASE
          WHEN status = 'deploying' THEN status
          WHEN EXISTS (SELECT 1 FROM pyo.volumes v WHERE v.service_id = services.id AND v.data ? 'restore') THEN 'restoring'
          WHEN $4 = 0 THEN 'stopped'
          WHEN $5 >= LEAST($4, coalesce((spec->'autoscale'->>'min')::int, $4)) THEN 'running'
          WHEN $5 = 0 THEN 'failed'
          ELSE 'degraded' END
  WHERE id = $1 AND cluster_id = $2 AND NOT deleting";

async fn heartbeat(
    State(s): State<AppState>,
    cluster: AgentCluster,
    Json(hb): Json<Heartbeat>,
) -> ApiResult<Json<Value>> {
    if hb.protocol != PROTOCOL_VERSION {
        return Err(ApiError::bad_request(format!(
            "This API speaks agent protocol {PROTOCOL_VERSION}; the agent sent {}. Update the agent.",
            hb.protocol
        )));
    }
    let mut tx = s.db.begin().await?;
    // The cluster Pyo runs on, only when its agent proves it.
    let token_hash: String =
        sqlx::query_scalar("SELECT agent_token_hash FROM pyo.clusters WHERE id = $1")
            .bind(&cluster.id)
            .fetch_one(&mut *tx)
            .await?;
    let same_cluster = proves_host(
        std::env::var("PYO_BOOTSTRAP_KEY").ok().as_deref(),
        hb.host_proof.as_deref(),
        &token_hash,
    );
    if same_cluster {
        // One cluster is Pyo's: not one it was connected as before.
        sqlx::query("UPDATE pyo.clusters SET same_cluster = false WHERE same_cluster AND id <> $1")
            .bind(&cluster.id)
            .execute(&mut *tx)
            .await?;
    }
    // Its install against this Pyo's: refreshed by the agent when it may.
    let was_install: Option<String> = sqlx::query_scalar(
        "SELECT capabilities->'agentInstall'->>'state' FROM pyo.clusters WHERE id = $1",
    )
    .bind(&cluster.id)
    .fetch_one(&mut *tx)
    .await?;
    let (agent_install, refresh_sent) =
        crate::install::sync(&mut tx, &cluster.id, hb.scope.as_ref(), same_cluster).await?;
    let any_not_ready = hb.servers.iter().any(|n| n.status != "ready");
    let status = if hb.servers.is_empty() || any_not_ready {
        "degraded"
    } else {
        "connected"
    };
    let caps = &hb.capabilities;
    let capabilities = json!({
        "ingressController": caps.ingress_controller,
        // Why the gateway failed to install, while it isn't answering.
        "gatewayError": caps.gateway_error,
        "loadBalancer": caps.load_balancer,
        "loadBalancerIps": caps.load_balancer_ips,
        "certManager": caps.cert_manager,
        "metricsServer": caps.metrics_server,
        "storageClasses": caps.storage_classes.iter().map(storage_class_json).collect::<Vec<_>>(),
        "podCidr": caps.pod_cidr,
        "serviceCidr": caps.service_cidr,
        "security": caps.security,
        "gpu": caps.gpu,
        "regions": caps.regions,
        "trafficModes": caps.traffic_modes,
        "trafficLocal": caps.traffic_local,
        // Services whose WAF policy the gateway refused (bundles.rs `waf_shown`).
        "wafRejected": caps.waf_rejected,
        // And those whose WAF policy it accepted (bundles.rs `waf_shown`).
        "wafActive": caps.waf_active,
        // Whether this agent can update Pyo and itself (updates.rs).
        "updates": hb.updates,
        // What the agent may do there, and the cluster owner's switches.
        "agentScope": hb.scope,
        // Whether its install is this Pyo's (install.rs).
        "agentInstall": agent_install,
    });
    let workloads = serde_json::to_value(&hb.workloads).map_err(anyhow::Error::from)?;
    let was: String =
        sqlx::query_scalar("SELECT status FROM pyo.clusters WHERE id = $1 FOR UPDATE")
            .bind(&cluster.id)
            .fetch_one(&mut *tx)
            .await?;
    sqlx::query(
        "UPDATE pyo.clusters SET status = $2, version = $3, agent_version = $4, capabilities = $5, workloads = $6, same_cluster = $7, last_seen_at = now() WHERE id = $1",
    )
    .bind(&cluster.id)
    .bind(status)
    .bind(&hb.kubernetes_version)
    .bind(&hb.agent_version)
    .bind(&capabilities)
    .bind(workloads)
    .bind(same_cluster)
    .execute(&mut *tx)
    .await?;
    name_from_host(&mut tx, &cluster.id, &cluster.team_id, &hb.servers).await?;
    // The server Pyo runs on, connecting for the first time: the team's
    // first builder goes there.
    let builder_added = was == "pending"
        && same_cluster
        && crate::builders::first_on_this_server(
            &mut tx,
            &s.config,
            &cluster.id,
            &cluster.team_id,
            &hb.servers,
        )
        .await?
        .is_some();
    crate::bundles::reported(&mut tx, &cluster.id, &hb.bundles).await?;
    // Databases follow Pyo: updated here once this Pyo has a newer version
    // (and the cluster's install lets the agent apply it).
    crate::bundles::sync_databases(&mut tx, &cluster.id, false).await?;
    // The WAF's engine too (where a service has its WAF on), once the
    // install that pins its images is current again.
    if agent_install["state"] == "current" && was_install.as_deref().is_some_and(|w| w != "current")
    {
        let wanted = crate::waf::wanted(&mut tx, &cluster.id).await?;
        crate::bundles::sync_waf(&mut tx, &cluster.id, wanted).await?;
    }
    // An import that just ended: its database is sent again, without the import's Secret.
    for id in
        crate::databases::reported(&mut tx, &cluster.id, &hb.databases, hb.databases_listed).await?
    {
        crate::databases::send_by_id(&s, &mut tx, &id).await?;
    }
    if was == "pending" || was == "unreachable" {
        let name: String = sqlx::query_scalar("SELECT name FROM pyo.clusters WHERE id = $1")
            .bind(&cluster.id)
            .fetch_one(&mut *tx)
            .await?;
        let (kind, title, st) = if was == "pending" {
            ("cluster.registered", format!("Connected {name}"), "success")
        } else {
            (
                "cluster.registered",
                format!("{name} is reachable again"),
                "success",
            )
        };
        log_activity(
            &mut tx,
            &cluster.team_id,
            "pyo",
            kind,
            st,
            &title,
            json!({ "kind": "cluster", "id": cluster.id, "label": name }),
            json!({ "clusterId": cluster.id }),
        )
        .await?;
    }

    // Servers: upsert what's reported, drop what's gone.
    let names: Vec<String> = hb.servers.iter().map(|n| n.name.clone()).collect();
    for n in &hb.servers {
        sqlx::query(
            "INSERT INTO pyo.servers (id, cluster_id, name, report) VALUES ($1, $2, $3, $4)
             ON CONFLICT (cluster_id, name) DO UPDATE SET report = excluded.report, updated_at = now()",
        )
        .bind(new_id("srv"))
        .bind(&cluster.id)
        .bind(&n.name)
        .bind(serde_json::to_value(n).map_err(anyhow::Error::from)?)
        .execute(&mut *tx)
        .await?;
    }
    sqlx::query("DELETE FROM pyo.servers WHERE cluster_id = $1 AND NOT (name = ANY($2))")
        .bind(&cluster.id)
        .bind(&names)
        .execute(&mut *tx)
        .await?;
    // Where each server is, from its public address (GeoIP for the city,
    // else the country the agent found). A location set by hand is kept.
    for n in &hb.servers {
        let ip = n
            .public_ip
            .as_deref()
            .or(n.external_ip.as_deref())
            .filter(|ip| ip.parse::<std::net::IpAddr>().is_ok_and(|a| public_ip(&a)));
        let (mut cc, _, city) = ip.map(|ip| s.observability.locate(ip)).unwrap_or_default();
        if cc.len() != 2 {
            cc = n.public_country.clone().unwrap_or_default();
        }
        if cc.len() == 2 {
            let location = if city.is_empty() {
                cc
            } else {
                format!("{city}, {cc}")
            };
            sqlx::query("UPDATE pyo.servers SET location = $3, location_source = 'geoip' WHERE cluster_id = $1 AND name = $2 AND coalesce(location_source, 'geoip') = 'geoip' AND location IS DISTINCT FROM $3")
                .bind(&cluster.id)
                .bind(&n.name)
                .bind(&location)
                .execute(&mut *tx)
                .await?;
        }
    }

    // Services: runtime numbers always; status only when no rollout owns it.
    for a in &hb.services {
        crate::cronruns::record(&mut tx, &cluster.id, &a.service_id, &a.runs).await?;
        let runtime = json!({
            "cronJobs": a.cron_jobs, "lastRun": a.last_run, "desired": a.desired, "readyReplicas": a.ready_replicas, "cpuMillis": a.cpu_millis, "memoryMi": a.memory_mi,
            "restarts": a.restarts, "since": a.since, "replicas": a.replicas, "clusterIp": a.cluster_ip,
            // Null from older agents: the dashboard shows "—".
            "storageMi": a.storage_mi, "netRxBps": a.net_rx_bps, "netTxBps": a.net_tx_bps,
        });
        sqlx::query(SERVICE_STATUS)
            .bind(&a.service_id)
            .bind(&cluster.id)
            .bind(&runtime)
            .bind(a.desired)
            .bind(a.ready_replicas)
            .execute(&mut *tx)
            .await?;
    }
    // HTTPS certificates: issued, still pending, or failing (with why).
    for c in &hb.certificates {
        let failing = !c.ready && c.message.to_lowercase().contains("fail");
        let tls = if c.ready {
            "issued"
        } else if failing {
            "failed"
        } else {
            "pending"
        };
        let error = if failing {
            Some(c.message.chars().take(300).collect::<String>())
        } else {
            None
        };
        sqlx::query(
            "UPDATE pyo.services SET spec = jsonb_set(spec, '{domains}', (
               SELECT coalesce(jsonb_agg(CASE WHEN d->>'hostname' = $3 AND coalesce(d->>'tlsMode', 'auto') <> 'off'
                                              THEN (d - 'tlsError') || jsonb_strip_nulls(jsonb_build_object('tls', $4::text, 'tlsError', $5::text))
                                              ELSE d END), '[]')
                 FROM jsonb_array_elements(spec->'domains') d))
              WHERE id = $1 AND cluster_id = $2 AND NOT deleting
                AND EXISTS (SELECT 1 FROM jsonb_array_elements(spec->'domains') d
                             WHERE d->>'hostname' = $3 AND (coalesce(d->>'tls', '') <> $4 OR coalesce(d->>'tlsError', '') <> coalesce($5, '')))",
        )
        .bind(&c.service_id)
        .bind(&cluster.id)
        .bind(&c.hostname)
        .bind(tls)
        .bind(&error)
        .execute(&mut *tx)
        .await?;
    }
    // Volumes: their state, deletions confirmed, claims Pyo has no volume for.
    crate::volumes::reported(&mut tx, &cluster.id, &hb.volumes).await?;
    tx.commit().await?;
    if refresh_sent || builder_added {
        s.wake(&cluster.id).await;
    }
    let (observability, team, id) = (
        s.observability.clone(),
        cluster.team_id.clone(),
        cluster.id.clone(),
    );
    tokio::spawn(async move {
        observability.record_metrics(&team, &id, &hb).await;
        observability.record_database_metrics(&id, &hb).await;
    });
    // Each server's location, for its topology labels (the cluster's when it has none of its own).
    let places: Vec<(String, String)> = sqlx::query_as(
        "SELECT n.name, coalesce(n.location, c.location) FROM pyo.servers n JOIN pyo.clusters c ON c.id = n.cluster_id WHERE n.cluster_id = $1",
    )
    .bind(&cluster.id)
    .fetch_all(&s.db)
    .await?;
    let server_locations: Vec<pyo_proto::ServerLocation> = places
        .into_iter()
        .filter_map(|(server, loc)| {
            pyo_proto::location_labels(&loc).map(|(region, zone)| pyo_proto::ServerLocation {
                server,
                region,
                zone,
            })
        })
        .collect();
    // The gateway is published on the servers or not (the agent places it).
    let public_traffic: bool =
        sqlx::query_scalar("SELECT public_traffic FROM pyo.clusters WHERE id = $1")
            .bind(&cluster.id)
            .fetch_one(&s.db)
            .await?;
    let tagged: Vec<(String, Vec<String>)> = sqlx::query_as(
        "SELECT n.name, coalesce(t.tags, '{}') FROM pyo.servers n LEFT JOIN pyo.server_tags t ON t.cluster_id = n.cluster_id AND t.name = n.name WHERE n.cluster_id = $1",
    )
    .bind(&cluster.id)
    .fetch_all(&s.db)
    .await?;
    let server_tags: Vec<pyo_proto::ServerTags> = tagged
        .into_iter()
        .map(|(server, tags)| pyo_proto::ServerTags { server, tags })
        .collect();
    let gpu_policies = gpu_policies(&s.db, &cluster.id).await?;
    // Each project namespace's team, so the agent labels it: the team's
    // databases let only its own projects in.
    let teams: Vec<(String, String)> = sqlx::query_as(
        "SELECT DISTINCT p.namespace, p.team_id FROM pyo.services a JOIN pyo.projects p ON p.id = a.project_id WHERE a.cluster_id = $1 AND p.namespace <> ''",
    )
    .bind(&cluster.id)
    .fetch_all(&s.db)
    .await?;
    let project_teams: Vec<pyo_proto::ProjectTeam> = teams
        .into_iter()
        .map(|(namespace, team_id)| pyo_proto::ProjectTeam { namespace, team_id })
        .collect();
    // Vector (and on Pyo's own cluster, ClickHouse) runs while it's on.
    let observability = s.observability.enabled().await;
    Ok(Json(
        json!({ "ok": true, "clusterId": cluster.id, "serverLocations": server_locations, "serverTags": server_tags, "gpuPolicies": gpu_policies, "publicTraffic": public_traffic, "projectTeams": project_teams, "observability": observability }),
    ))
}

/// Which GPU servers are kept for GPU work. A server's own setting wins;
/// by default one is, unless it's a control-plane server or the cluster has
/// no other ready server for everything else (that would leave regular
/// services nowhere to run).
pub async fn gpu_policies(
    db: &sqlx::PgPool,
    cluster_id: &str,
) -> ApiResult<Vec<pyo_proto::GpuPolicy>> {
    let rows: Vec<(String, Value, Option<bool>)> = sqlx::query_as(
        "SELECT n.name, n.report, t.regular_services FROM pyo.servers n
           LEFT JOIN pyo.server_tags t ON t.cluster_id = n.cluster_id AND t.name = n.name WHERE n.cluster_id = $1",
    )
    .bind(cluster_id)
    .fetch_all(db)
    .await?;
    Ok(decide_gpu_policies(&rows))
}

fn decide_gpu_policies(rows: &[(String, Value, Option<bool>)]) -> Vec<pyo_proto::GpuPolicy> {
    let gpu = |r: &Value| r["gpuDetected"] == json!(true);
    let others = rows.iter().any(|(_, r, _)| {
        !gpu(r) && r["status"] == json!("ready") && r["schedulable"] == json!(true)
    });
    rows.iter()
        .filter(|(_, r, _)| gpu(r))
        .map(|(node, r, regular)| {
            let control_plane = r["roles"]
                .as_array()
                .is_some_and(|rs| rs.iter().any(|x| x == "control-plane"));
            let dedicated = match regular {
                Some(regular) => !regular,
                None => others && !control_plane,
            };
            pyo_proto::GpuPolicy {
                server: node.clone(),
                dedicated,
            }
        })
        .collect()
}

#[derive(Deserialize)]
struct PollQuery {
    /// Seconds to hold the request open when there's nothing to do.
    wait: Option<u64>,
}

async fn poll(
    State(s): State<AppState>,
    cluster: AgentCluster,
    headers: axum::http::HeaderMap,
    Query(q): Query<PollQuery>,
) -> ApiResult<Json<Vec<CommandEnvelope>>> {
    let wait = Duration::from_secs(q.wait.unwrap_or(25).min(55));
    let instance = headers
        .get(pyo_proto::AGENT_INSTANCE_HEADER)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .chars()
        .take(64)
        .collect::<String>();
    // A rollout handed to an earlier process of this agent that is still
    // running here was orphaned by a restart: hand it out again.
    if !instance.is_empty() {
        sqlx::query(
            "UPDATE pyo.commands c SET status = 'pending', sent_to = NULL
              WHERE c.cluster_id = $1 AND c.status = 'sent' AND c.sent_to IS DISTINCT FROM $2
                AND c.command->>'kind' = 'deploy'
                AND EXISTS (SELECT 1 FROM pyo.deployments d
                             WHERE d.id = c.command->>'deploymentId' AND d.status NOT IN ('succeeded', 'failed', 'cancelled'))",
        )
        .bind(&cluster.id)
        .bind(&instance)
        .execute(&s.db)
        .await?;
    }
    let deadline = tokio::time::Instant::now() + wait;
    let waker = s.waker(&cluster.id).await;
    loop {
        // Register interest before checking, so a notify in between isn't lost.
        let notified = waker.notified();
        let mut rows: Vec<(String, Value, i64)> = sqlx::query_as(
            "UPDATE pyo.commands SET status = 'sent', sent_at = now(), sent_to = nullif($2, '')
              WHERE id IN (SELECT id FROM pyo.commands WHERE cluster_id = $1 AND status = 'pending' ORDER BY created_at LIMIT 20 FOR UPDATE SKIP LOCKED)
              RETURNING id, command, (extract(epoch FROM created_at) * 1000000)::bigint",
        )
        .bind(&cluster.id)
        .bind(&instance)
        .fetch_all(&s.db)
        .await?;
        // In the order they were queued (RETURNING keeps none): the agent
        // runs commands on the same object in the order it gets them.
        rows.sort_by_key(|r| r.2);
        if !rows.is_empty() {
            let mut out = Vec::with_capacity(rows.len());
            for (id, mut command, _) in rows {
                let opened = async {
                    crate::vault::get().open_env(&mut command).await?;
                    crate::teambackup::open_command(&mut command).await?;
                    // The service's domains as they are now (routes.rs).
                    crate::routes::refresh(&s.db, &mut command).await
                };
                if let Err(e) = opened.await {
                    tracing::error!(%id, error = %e, "can't open a command's sealed values");
                    undeliverable(
                        &s,
                        &id,
                        &command,
                        &format!("Couldn't open its sealed values: {e}"),
                    )
                    .await;
                    continue;
                }
                match serde_json::from_value(command.clone()) {
                    Ok(command) => out.push(CommandEnvelope { id, command }),
                    Err(e) => {
                        tracing::error!(%id, error = %e, "unreadable command");
                        undeliverable(&s, &id, &command, &format!("Pyo couldn't read it: {e}"))
                            .await;
                    }
                }
            }
            return Ok(Json(out));
        }
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        // Shutting down: answer now, so the agent reconnects to another replica.
        if left.is_zero() || s.stopping() {
            return Ok(Json(vec![]));
        }
        // Wake on new work, or re-check every few seconds in case another replica queued it.
        let _ = tokio::time::timeout(left.min(Duration::from_secs(5)), notified).await;
    }
}

async fn command_result(
    State(s): State<AppState>,
    cluster: AgentCluster,
    Path(id): Path<String>,
    Json(r): Json<CommandResult>,
) -> ApiResult<Json<Value>> {
    let mut tx = s.db.begin().await?;
    let command: Option<Value> =
        sqlx::query_scalar("SELECT command FROM pyo.commands WHERE id = $1 AND cluster_id = $2")
            .bind(&id)
            .bind(&cluster.id)
            .fetch_optional(&mut *tx)
            .await?;
    let command = command.ok_or_else(|| ApiError::not_found("Command"))?;
    sqlx::query("UPDATE pyo.commands SET status = $3, result = $4, done_at = now() WHERE id = $1 AND cluster_id = $2")
        .bind(&id)
        .bind(&cluster.id)
        .bind(if r.ok { "done" } else { "failed" })
        .bind(serde_json::to_value(&r).map_err(anyhow::Error::from)?)
        .execute(&mut *tx)
        .await?;
    if command["kind"] == json!("backup") {
        crate::teambackup::finished(&mut tx, &command, &r).await?;
    }
    crate::bundles::finished(
        &mut tx,
        &cluster.id,
        &id,
        &command,
        r.ok,
        r.message.as_deref(),
    )
    .await?;
    // Databases set up (or updated): the databases waiting for them go out.
    if r.ok
        && command["kind"] == json!("apply_bundle")
        && command["bundle"]["id"] == json!(pyo_proto::bundles::DATABASES_ID)
    {
        crate::databases::resend(&s, &mut tx, &cluster.id).await?;
    }
    crate::databases::finished(&mut tx, &cluster.id, &command, r.ok, r.message.as_deref()).await?;
    if command["kind"] == json!("set_origin_pull") {
        crate::certs::origin_pull_finished(
            &mut tx,
            &cluster.id,
            &command,
            r.ok,
            r.message.as_deref(),
        )
        .await?;
        if !r.ok {
            crate::ops::log_activity(
                &mut tx,
                &cluster.team_id,
                "Pyo",
                "domain.certificate_expiring",
                "warning",
                &format!(
                    "Couldn't change mTLS: {}",
                    r.message.as_deref().unwrap_or("the agent failed")
                ),
                json!({ "kind": "cluster", "id": cluster.id, "label": cluster.id }),
                json!({ "clusterId": cluster.id }),
            )
            .await?;
        }
    }
    crate::volumes::finished(&mut tx, &cluster.id, &command, r.ok, r.message.as_deref()).await?;
    crate::routes::finished(
        &mut tx,
        &cluster.team_id,
        &command,
        r.ok,
        r.message.as_deref(),
    )
    .await?;
    // Volumes deleted with a service go once its objects are gone (or stay, if that failed).
    if command["kind"] == json!("delete_service") {
        crate::volumes::service_deleted(
            &mut tx,
            &cluster.id,
            command["serviceId"].as_str().unwrap_or(""),
            r.ok,
        )
        .await?;
    }
    // A finished delete removes the service's row (and its deployments).
    if command["kind"] == json!("delete_service") && r.ok {
        sqlx::query("DELETE FROM pyo.services WHERE id = $1 AND deleting")
            .bind(command["serviceId"].as_str().unwrap_or(""))
            .execute(&mut *tx)
            .await?;
    }
    if command["kind"] == json!("remove_server") {
        let node = command["server"].as_str().unwrap_or("");
        let force = command["force"].as_bool().unwrap_or(false);
        if !r.ok {
            sqlx::query("UPDATE pyo.servers SET removal = removal || jsonb_build_object('state', 'failed', 'message', $3::text) WHERE cluster_id = $1 AND name = $2")
                .bind(&cluster.id)
                .bind(node)
                .bind(r.message.as_deref().unwrap_or("The removal failed."))
                .execute(&mut *tx)
                .await?;
        } else if force {
            sqlx::query("DELETE FROM pyo.servers WHERE cluster_id = $1 AND name = $2")
                .bind(&cluster.id)
                .bind(node)
                .execute(&mut *tx)
                .await?;
            crate::ops::log_activity(
                &mut tx,
                &cluster.team_id,
                "pyo",
                "server.removed",
                "info",
                &format!("Removed server {node}"),
                json!({ "kind": "cluster", "id": cluster.id, "label": node }),
                json!({}),
            )
            .await?;
        } else {
            // Drained: it stays until the machine is uninstalled and goes offline.
            sqlx::query("UPDATE pyo.servers SET removal = removal || jsonb_build_object('state', 'drained', 'drainedAt', $3::bigint, 'message', $4::text) WHERE cluster_id = $1 AND name = $2")
                .bind(&cluster.id)
                .bind(node)
                .bind(pyo_proto::now_ms())
                .bind(r.message.as_deref().unwrap_or(""))
                .execute(&mut *tx)
                .await?;
        }
    }
    if command["kind"] == json!("delete_service") && !r.ok {
        sqlx::query("UPDATE pyo.services SET deleting = false WHERE id = $1")
            .bind(command["serviceId"].as_str().unwrap_or(""))
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(Json(json!({ "ok": true })))
}

async fn progress(
    State(s): State<AppState>,
    cluster: AgentCluster,
    Path(id): Path<String>,
    Json(p): Json<DeploymentProgress>,
) -> ApiResult<Json<Value>> {
    deploy::apply_progress(&s, &cluster.id, &id, p).await?;
    Ok(Json(json!({ "ok": true })))
}

/// The gateway's access log lines from the cluster's Vector, one JSON object per
/// line (or a JSON array). See observability.rs for what's kept.
async fn access_logs(
    State(s): State<AppState>,
    cluster: AgentCluster,
    body: String,
) -> ApiResult<Json<Value>> {
    let lines: Vec<Value> = if body.trim_start().starts_with('[') {
        serde_json::from_str(&body).map_err(|_| ApiError::bad_request("Expected JSON lines."))?
    } else {
        body.lines()
            .filter(|l| !l.trim().is_empty())
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect()
    };
    let received = lines.len();
    let stored = s.observability.ingest(&cluster.id, lines).await?;
    Ok(Json(json!({ "received": received, "stored": stored })))
}

/// Each server's load, disk I/O and network from the cluster's Vector
/// (host_metrics), as JSON lines (or an array). Kept rows: see observability.rs.
async fn host_metrics(
    State(s): State<AppState>,
    cluster: AgentCluster,
    body: String,
) -> ApiResult<Json<Value>> {
    let events: Vec<Value> = if body.trim_start().starts_with('[') {
        serde_json::from_str(&body).map_err(|_| ApiError::bad_request("Expected JSON lines."))?
    } else {
        body.lines()
            .filter(|l| !l.trim().is_empty())
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect()
    };
    let stored = s
        .observability
        .ingest_host_metrics(&cluster.team_id, &cluster.id, &events)
        .await?;
    Ok(Json(json!({ "received": events.len(), "stored": stored })))
}

/// Routable on the internet (not private, tailnet/CGNAT, loopback or link-local).
pub(crate) fn public_ip(ip: &std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(v) => {
            let [a, b, ..] = v.octets();
            !(v.is_private()
                || v.is_loopback()
                || v.is_link_local()
                || v.is_unspecified()
                || (a == 100 && (64..=127).contains(&b)))
        }
        std::net::IpAddr::V6(v) => {
            !(v.is_loopback()
                || v.is_unspecified()
                || (v.segments()[0] & 0xfe00) == 0xfc00
                || (v.segments()[0] & 0xffc0) == 0xfe80)
        }
    }
}

/// A command that can't reach the agent fails now, not at a timeout, and so
/// does what it was for (a backup shows the error).
async fn undeliverable(s: &AppState, id: &str, command: &Value, why: &str) {
    let r = CommandResult {
        ok: false,
        message: Some(why.to_owned()),
        logs: vec![],
        data: None,
    };
    let _ = sqlx::query(
        "UPDATE pyo.commands SET status = 'failed', result = $2, done_at = now() WHERE id = $1",
    )
    .bind(id)
    .bind(serde_json::to_value(&r).unwrap_or(Value::Null))
    .execute(&s.db)
    .await;
    if command["kind"] == json!("set_origin_pull")
        && let Ok(cluster) =
            sqlx::query_scalar::<_, String>("SELECT cluster_id FROM pyo.commands WHERE id = $1")
                .bind(id)
                .fetch_one(&s.db)
                .await
        && let Ok(mut tx) = s.db.begin().await
        && crate::certs::origin_pull_finished(&mut tx, &cluster, command, false, Some(why))
            .await
            .is_ok()
    {
        let _ = tx.commit().await;
    }
    if command["kind"] == json!("backup")
        && let Ok(mut tx) = s.db.begin().await
        && crate::teambackup::finished(&mut tx, command, &r)
            .await
            .is_ok()
    {
        let _ = tx.commit().await;
    }
    // A volume's restore, move or growth shows why it didn't happen.
    if matches!(
        command["kind"].as_str(),
        Some("restore_volume" | "move_volume" | "resize_volume")
    ) && let Ok(cluster) =
        sqlx::query_scalar::<_, String>("SELECT cluster_id FROM pyo.commands WHERE id = $1")
            .bind(id)
            .fetch_one(&s.db)
            .await
        && let Ok(mut tx) = s.db.begin().await
        && crate::volumes::finished(&mut tx, &cluster, command, false, Some(why))
            .await
            .is_ok()
    {
        let _ = tx.commit().await;
    }
}

/// A cluster added without a name is named after its server (the control
/// plane's hostname) the first time its agent reports one: made a valid
/// cluster name, and numbered if the team has that name already.
async fn name_from_host(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    cluster_id: &str,
    team_id: &str,
    nodes: &[pyo_proto::ServerReport],
) -> ApiResult<()> {
    let wanted: bool = sqlx::query_scalar("SELECT name_from_host FROM pyo.clusters WHERE id = $1")
        .bind(cluster_id)
        .fetch_one(&mut **tx)
        .await?;
    if !wanted {
        return Ok(());
    }
    let Some(host) = nodes
        .iter()
        .find(|n| n.roles.iter().any(|r| r == "control-plane"))
        .or(nodes.first())
        .map(|n| n.name.as_str())
    else {
        return Ok(());
    };
    let Some(base) = host_label(host) else {
        return Ok(());
    };
    let taken: Vec<String> =
        sqlx::query_scalar("SELECT name FROM pyo.clusters WHERE team_id = $1 AND id <> $2")
            .bind(team_id)
            .bind(cluster_id)
            .fetch_all(&mut **tx)
            .await?;
    let name = (1..)
        .map(|i| {
            if i == 1 {
                base.clone()
            } else {
                format!("{}-{i}", &base[..base.len().min(59)])
            }
        })
        .find(|n| !taken.contains(n))
        .unwrap_or(base);
    sqlx::query("UPDATE pyo.clusters SET name = $2, name_from_host = false WHERE id = $1")
        .bind(cluster_id)
        .bind(&name)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

/// A hostname as a cluster name: lowercase letters, digits and dashes, from
/// its first label, at most 63 long. None when nothing usable is left or the
/// name belongs to a page.
pub(crate) fn host_label(host: &str) -> Option<String> {
    let first = host.split('.').next().unwrap_or("");
    let mut out = String::new();
    for c in first.chars().flat_map(char::to_lowercase) {
        if c.is_ascii_lowercase() || c.is_ascii_digit() {
            out.push(c);
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    let out: String = out
        .trim_matches('-')
        .chars()
        .take(63)
        .collect::<String>()
        .trim_end_matches('-')
        .to_owned();
    if out.is_empty()
        || ["new", "workloads", "networking", "storage"].contains(&out.as_str())
        || !crate::util::dns_label(&out)
    {
        return None;
    }
    Some(out)
}

#[cfg(test)]
mod name_tests {
    use super::{decide_gpu_policies, host_label};
    use serde_json::json;

    #[test]
    fn gpu_servers_are_kept_for_gpu_work_when_there_are_others() {
        let node = |gpu: bool, roles: &[&str]| json!({ "gpuDetected": gpu, "status": "ready", "schedulable": true, "roles": roles });
        // A GPU worker beside a regular one: kept for GPU work.
        let p = decide_gpu_policies(&[
            ("cp".into(), node(false, &["control-plane", "worker"]), None),
            ("g".into(), node(true, &["worker"]), None),
        ]);
        assert_eq!(
            p,
            vec![pyo_proto::GpuPolicy {
                server: "g".into(),
                dedicated: true
            }]
        );
        // The only server, or the control plane: runs everything.
        assert!(!decide_gpu_policies(&[("g".into(), node(true, &["worker"]), None)])[0].dedicated);
        assert!(
            !decide_gpu_policies(&[
                ("w".into(), node(false, &["worker"]), None),
                ("g".into(), node(true, &["control-plane", "worker"]), None)
            ])[0]
                .dedicated
        );
        // The server's own setting wins either way.
        assert!(
            !decide_gpu_policies(&[
                ("w".into(), node(false, &["worker"]), None),
                ("g".into(), node(true, &["worker"]), Some(true))
            ])[0]
                .dedicated
        );
        assert!(
            decide_gpu_policies(&[("g".into(), node(true, &["worker"]), Some(false))])[0].dedicated
        );
    }

    #[test]
    fn hostnames_become_cluster_names() {
        assert_eq!(host_label("server-1").as_deref(), Some("server-1"));
        assert_eq!(
            host_label("Prod_Box.example.com").as_deref(),
            Some("prod-box")
        );
        assert_eq!(host_label("--web--01--").as_deref(), Some("web-01"));
        assert_eq!(host_label("new"), None);
        assert_eq!(host_label(""), None);
    }

    #[test]
    fn only_the_hosts_key_proves_pyos_cluster() {
        let key = "k".repeat(64);
        let hash = crate::ids::sha256_hex("agent-token");
        let good = pyo_proto::host_proof(&key, &hash);
        assert!(super::proves_host(Some(&key), Some(&good), &hash));
        assert!(
            !super::proves_host(Some(&key), None, &hash),
            "saying so isn't enough"
        );
        assert!(
            !super::proves_host(
                Some(&key),
                Some(&good),
                &crate::ids::sha256_hex("another token")
            ),
            "bound to the token"
        );
        assert!(
            !super::proves_host(Some(&"x".repeat(64)), Some(&good), &hash),
            "another key"
        );
        assert!(
            !super::proves_host(None, Some(&good), &hash),
            "no key here: nobody proves it"
        );
        assert!(
            !super::proves_host(
                Some("short"),
                Some(&pyo_proto::host_proof("short", &hash)),
                &hash
            ),
            "a weak key doesn't count"
        );
    }

    /// Against a real Postgres (DATABASE_URL, a throwaway one): a service
    /// held by a volume's restore shows "restoring", not "failed", while it
    /// starts again; a rollout's status stays.
    #[tokio::test]
    #[ignore = "needs DATABASE_URL (a throwaway Postgres)"]
    async fn a_restored_service_starting_is_restoring() {
        let pool = sqlx::PgPool::connect(&std::env::var("DATABASE_URL").unwrap())
            .await
            .unwrap();
        crate::migrate::run(&pool).await.unwrap();
        let run = crate::ids::new_id("t");
        let (team, cluster, project, service, volume) = (
            format!("team_{run}"),
            format!("cl_{run}"),
            format!("prj_{run}"),
            format!("svc_{run}"),
            format!("vol_{run}"),
        );
        sqlx::query("INSERT INTO pyo.teams (id, name, slug) VALUES ($1, $1, $1)")
            .bind(&team)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO pyo.clusters (id, team_id, name, agent_token_hash) VALUES ($1, $2, $1, $1)").bind(&cluster).bind(&team).execute(&pool).await.unwrap();
        sqlx::query(
            "INSERT INTO pyo.projects (id, team_id, name, namespace) VALUES ($1, $2, 'shop', $1)",
        )
        .bind(&project)
        .bind(&team)
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query("INSERT INTO pyo.services (id, team_id, project_id, cluster_id, name, spec, status) VALUES ($1, $2, $3, $4, 'web', '{}', 'running')")
            .bind(&service)
            .bind(&team)
            .bind(&project)
            .bind(&cluster)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO pyo.volumes (id, team_id, cluster_id, project_id, service_id, data) VALUES ($1, $2, $3, $4, $5, '{\"name\": \"web-data\", \"restore\": {\"id\": \"rst_1\"}}')")
            .bind(&volume)
            .bind(&team)
            .bind(&cluster)
            .bind(&project)
            .bind(&service)
            .execute(&pool)
            .await
            .unwrap();
        let beat = |desired: i32, ready: i32| {
            sqlx::query(super::SERVICE_STATUS)
                .bind(&service)
                .bind(&cluster)
                .bind(json!({}))
                .bind(desired)
                .bind(ready)
        };
        let status = || {
            sqlx::query_scalar::<_, String>("SELECT status FROM pyo.services WHERE id = $1")
                .bind(&service)
        };
        for (desired, ready) in [(0, 0), (1, 0)] {
            beat(desired, ready).execute(&pool).await.unwrap();
            assert_eq!(
                status().fetch_one(&pool).await.unwrap(),
                "restoring",
                "{desired} {ready}"
            );
        }
        // Restored: as the cluster says again.
        sqlx::query("UPDATE pyo.volumes SET data = data - 'restore' WHERE id = $1")
            .bind(&volume)
            .execute(&pool)
            .await
            .unwrap();
        beat(1, 0).execute(&pool).await.unwrap();
        assert_eq!(status().fetch_one(&pool).await.unwrap(), "failed");
        beat(1, 1).execute(&pool).await.unwrap();
        assert_eq!(status().fetch_one(&pool).await.unwrap(), "running");
        sqlx::query("DELETE FROM pyo.teams WHERE id = $1")
            .bind(&team)
            .execute(&pool)
            .await
            .unwrap();
    }

    /// Against a real Postgres (DATABASE_URL, a throwaway one): the server
    /// Pyo runs on, "local" while it connects, takes its hostname; without a
    /// usable one it stays "local".
    #[tokio::test]
    #[ignore = "needs DATABASE_URL (a throwaway Postgres)"]
    async fn this_server_is_named_after_its_hostname() {
        use crate::builders::first_builder_tests::node;
        let pool = sqlx::PgPool::connect(&std::env::var("DATABASE_URL").unwrap())
            .await
            .unwrap();
        crate::migrate::run(&pool).await.unwrap();
        let run = crate::ids::new_id("t");
        let team = format!("team_{run}");
        sqlx::query("INSERT INTO pyo.teams (id, name, slug) VALUES ($1, $1, $1)")
            .bind(&team)
            .execute(&pool)
            .await
            .unwrap();
        for (id, host, want) in [("a", "Server-1.lan", "server-1"), ("b", "new", "local")] {
            let cluster = format!("cl_{id}{run}");
            sqlx::query("DELETE FROM pyo.clusters WHERE team_id = $1")
                .bind(&team)
                .execute(&pool)
                .await
                .unwrap();
            sqlx::query("INSERT INTO pyo.clusters (id, team_id, name, agent_token_hash, name_from_host, from_host_setup) VALUES ($1, $2, 'local', $1, true, true)")
                .bind(&cluster)
                .bind(&team)
                .execute(&pool)
                .await
                .unwrap();
            let mut tx = pool.begin().await.unwrap();
            super::name_from_host(
                &mut tx,
                &cluster,
                &team,
                &[node(host, &["control-plane"], "ready")],
            )
            .await
            .unwrap();
            tx.commit().await.unwrap();
            let name: String = sqlx::query_scalar("SELECT name FROM pyo.clusters WHERE id = $1")
                .bind(&cluster)
                .fetch_one(&pool)
                .await
                .unwrap();
            assert_eq!(name, want, "{host}");
        }
        sqlx::query("DELETE FROM pyo.teams WHERE id = $1")
            .bind(&team)
            .execute(&pool)
            .await
            .unwrap();
    }
}
