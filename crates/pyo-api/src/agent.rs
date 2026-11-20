//! Endpoints the in-cluster agent dials out to: heartbeats with what it sees,
//! a long-poll for commands, and reports on commands and rollouts.

use crate::error::Json;
use crate::error::Query;
use std::time::Duration;

use axum::{
    Router,
    extract::{FromRequestParts, Path, State},
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
        .route("/traffic-totals", post(traffic_totals))
        .route("/traffic-context", get(traffic_context))
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
        // A signed request (agent_auth.rs): the cluster its key belongs to,
        // when it's the agent's own key (a builder's or pyo-static's isn't).
        if let Some(v) = parts.extensions.get::<crate::agent_auth::VerifiedAgent>() {
            if !v.is_agent() {
                return Err(ApiError::new(StatusCode::FORBIDDEN, "This key isn't the agent's."));
            }
            return Ok(AgentCluster { id: v.cluster_id.clone(), team_id: v.team_id.clone() });
        }
        // Over the mTLS endpoint: the cluster its certificate names, while
        // that certificate is the cluster's current (or the one before it,
        // through a renewal). Never from the request.
        if let Some(peer) = parts.extensions.get::<crate::agent_tls::AgentPeer>()
            && let Some(cert) = &peer.cert
        {
            let id = crate::agent_tls::identity(cert)
                .ok_or_else(|| ApiError::new(StatusCode::UNAUTHORIZED, "This certificate names no cluster."))?;
            let ok: bool = sqlx::query_scalar(
                "SELECT EXISTS (SELECT 1 FROM pyo.clusters WHERE id = $1 AND team_id = $2 AND $3 IN (agent_cert_serial, agent_cert_prev_serial))",
            )
            .bind(&id.cluster)
            .bind(&id.team)
            .bind(&id.serial)
            .fetch_one(&state.db)
            .await?;
            if !ok {
                return Err(ApiError::new(
                    StatusCode::UNAUTHORIZED,
                    "This certificate was replaced or revoked. Run the cluster's connect command again.",
                ));
            }
            return Ok(AgentCluster { id: id.cluster, team_id: id.team });
        }
        if crate::agent_auth::refuse_tokens() {
            return Err(ApiError::new(
                StatusCode::UNAUTHORIZED,
                match crate::agent_auth::mode() {
                    crate::agent_auth::Mode::Signed => "Agents sign their requests here.",
                    _ => "Agents connect with a client certificate here.",
                },
            ));
        }
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

/// An uploaded certificate a domain of a service on this cluster uses, with
/// its key. Its team has to own the cluster too.
const CLUSTER_CERTIFICATE: &str = "SELECT c.chain_pem, c.key_sealed FROM pyo.certificates c
      WHERE c.id = $1 AND c.team_id = $2
        AND EXISTS (SELECT 1 FROM pyo.clusters k WHERE k.id = $3 AND k.team_id = c.team_id)
        AND EXISTS (SELECT 1 FROM pyo.services a, jsonb_array_elements(coalesce(a.spec->'domains', '[]')) d
                     WHERE a.cluster_id = $3 AND a.team_id = c.team_id AND d->>'certificateId' = c.id)";

/// A registry login a deployment on this cluster pulls with, with its
/// password. Its team has to own the cluster too.
const CLUSTER_REGISTRY_LOGIN: &str = "UPDATE pyo.registry_logins l SET last_used_at = now()
      WHERE l.id = $1 AND l.team_id = $2
        AND EXISTS (SELECT 1 FROM pyo.clusters k WHERE k.id = $3 AND k.team_id = l.team_id)
        AND EXISTS (SELECT 1 FROM pyo.deployments d
                     WHERE d.cluster_id = $3 AND d.team_id = l.team_id AND d.spec->>'pullLoginId' = l.id)
      RETURNING l.host, l.username, l.secret_sealed";

/// An uploaded certificate with its key, for a cluster that uses it.
async fn certificate(
    State(s): State<AppState>,
    cluster: AgentCluster,
    headers: axum::http::HeaderMap,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    let row: Option<(String, String)> = sqlx::query_as(CLUSTER_CERTIFICATE)
        .bind(&id)
        .bind(&cluster.team_id)
        .bind(&cluster.id)
        .fetch_optional(&s.db)
        .await?;
    let (certificate, sealed) = row.ok_or_else(|| ApiError::not_found("Certificate"))?;
    let private_key = crate::vault::get()
        .open_credential(&s.sealer, &sealed)
        .await?;
    let reply = json!(pyo_proto::TlsCertificate {
        certificate,
        private_key,
    });
    Ok(Json(signed_reply(&cluster.id, &headers, reply).await))
}

/// A registry login with its password, for a cluster that pulls with it.
async fn registry_login(
    State(s): State<AppState>,
    cluster: AgentCluster,
    headers: axum::http::HeaderMap,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    let row: Option<(String, String, String)> = sqlx::query_as(CLUSTER_REGISTRY_LOGIN)
        .bind(&id)
        .bind(&cluster.team_id)
        .bind(&cluster.id)
        .fetch_optional(&s.db)
        .await?;
    let (host, username, sealed) = row.ok_or_else(|| ApiError::not_found("Registry login"))?;
    let password = crate::vault::get()
        .open_credential(&s.sealer, &sealed)
        .await?;
    let reply = json!(pyo_proto::RegistryLogin {
        host,
        username,
        password,
    });
    Ok(Json(signed_reply(&cluster.id, &headers, reply).await))
}

/// A reply the agent acts on, with the signer's signature when this Pyo
/// has a signer (pyo-proto `signing`, `pyoreply1`): bound to the cluster
/// and to the nonce the agent sent with its request. The plain fields stay
/// beside it. When it can't be signed (no nonce, the signer down) it goes
/// out as it is, and an agent that requires signatures acts on nothing in it.
pub(crate) async fn signed_reply(
    cluster_id: &str,
    headers: &axum::http::HeaderMap,
    reply: Value,
) -> Value {
    match crate::command_signer() {
        Some(signer) => sign_reply_with(signer.as_ref(), cluster_id, headers, reply).await,
        None => reply,
    }
}

async fn sign_reply_with(
    signer: &dyn crate::CommandSigner,
    cluster_id: &str,
    headers: &axum::http::HeaderMap,
    mut reply: Value,
) -> Value {
    let Some(nonce) = headers
        .get(pyo_proto::REPLY_NONCE_HEADER)
        .and_then(|v| v.to_str().ok())
        .filter(|n| pyo_proto::valid_reply_nonce(n))
    else {
        return reply;
    };
    let json = reply.to_string();
    let item = crate::ReplyToSign {
        nonce: nonce.to_owned(),
        sha256: sha256_hex(&json),
    };
    match signer.sign_reply(cluster_id, &item).await {
        Ok(sig) if sig.refused.is_none() => {
            reply["signed"] = json!(pyo_proto::SignedReplyWire {
                claims: sig.claims,
                sig: sig.sig,
                kid: sig.kid,
                reply: json,
                delegation: sig.delegation,
            });
        }
        Ok(sig) => {
            tracing::warn!(cluster = %cluster_id, why = sig.refused.unwrap_or_default(), "the signer refused a reply")
        }
        Err(e) => {
            tracing::warn!(cluster = %cluster_id, error = %format!("{e:#}"), "couldn't sign a reply")
        }
    }
    reply
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
    headers: axum::http::HeaderMap,
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
    let (agent_install, refresh_sent) = crate::install::sync(
        &mut tx,
        &cluster.id,
        hb.scope.as_ref(),
        &hb.agent_version,
        same_cluster,
    )
    .await?;
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
        // Where the host publishes websites on its Tailscale address (freedomain.rs).
        "tailnetWebIps": caps.tailnet_web_ips,
        // Services whose WAF policy the gateway refused (bundles.rs `waf_shown`).
        "wafRejected": caps.waf_rejected,
        // And those whose WAF policy it accepted (bundles.rs `waf_shown`).
        "wafActive": caps.waf_active,
        // Whether this agent can update Pyo and itself (updates.rs).
        "updates": hb.updates,
        // Its builders and pyo-static sign their requests (agent_auth.rs).
        "componentKeys": hb.component_keys,
        // What the agent may do there, and the cluster owner's switches.
        "agentScope": hb.scope,
        // Whether its install is this Pyo's (install.rs).
        "agentInstall": agent_install,
        // Each server's Tailscale (tailscale.rs).
        "tailnet": hb.tailnet,
    });
    let workloads = serde_json::to_value(&hb.workloads).map_err(anyhow::Error::from)?;
    let was: String =
        sqlx::query_scalar("SELECT status FROM pyo.clusters WHERE id = $1 FOR UPDATE")
            .bind(&cluster.id)
            .fetch_one(&mut *tx)
            .await?;
    crate::install::agent_reported(&mut tx, &cluster.id).await?;
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
    // first builder goes there. Where every cluster keeps its own images
    // (Pyo Cloud), a team's first cluster to connect gets it.
    let builder_added = was == "pending"
        && (same_cluster || crate::deploy::in_cluster_registries())
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
                "cluster.reachable",
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

    // Servers: upsert what's reported, drop what's gone. One that stops
    // being ready (or is ready again), or is low on disk or memory (or no
    // longer), is in the activity, and alerts.
    let before: Vec<(String, String, Value)> = sqlx::query_as(
        "SELECT name, coalesce(report->>'status', ''), coalesce(report->'conditions', '[]'::jsonb) FROM pyo.servers WHERE cluster_id = $1 AND removal IS NULL",
    )
    .bind(&cluster.id)
    .fetch_all(&mut *tx)
    .await?;
    crate::pressure::record(&mut tx, &cluster.team_id, &cluster.id, &hb.servers, &before).await?;
    for n in &hb.servers {
        let Some((_, was, _)) = before.iter().find(|(name, _, _)| *name == n.name) else {
            continue;
        };
        let (kind, status, title) = match (was == "ready", n.status == "ready") {
            (true, false) => (
                "server.offline",
                "failure",
                format!("{} is offline", n.name),
            ),
            // Back only after it was said to be offline (a new server
            // isn't ready at first).
            (false, true) if offline_logged(&mut tx, &cluster, &n.name).await? => (
                "server.online",
                "success",
                format!("{} is back online", n.name),
            ),
            _ => continue,
        };
        log_activity(
            &mut tx,
            &cluster.team_id,
            "pyo",
            kind,
            status,
            &title,
            json!({ "kind": "server", "id": n.name, "label": n.name }),
            json!({ "clusterId": cluster.id }),
        )
        .await?;
    }
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
    // HTTPS certificates: issued, still pending, or failing, with why in
    // one plain sentence. A try that's been going for too long is failing
    // too: Let's Encrypt can't check the domain.
    let behind_nat = if hb.certificates.iter().any(|c| !c.ready) {
        crate::freedomain::load(&mut tx, &cluster.id)
            .await?
            .is_some_and(|(_, c)| c.behind_nat())
    } else {
        false
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    for c in &hb.certificates {
        let error = if c.ready {
            None
        } else {
            let dns_pending: bool = sqlx::query_scalar(
                "SELECT EXISTS (SELECT 1 FROM pyo.services s, jsonb_array_elements(s.spec->'domains') d
                  WHERE s.id = $1 AND d->>'hostname' = $2 AND d->>'status' = 'pending_dns')",
            )
            .bind(&c.service_id)
            .bind(&c.hostname)
            .fetch_one(&mut *tx)
            .await?;
            tls_problem(
                c,
                &TlsContext {
                    stuck: c.issuing_since.is_some_and(|t| now - t > TLS_STUCK_SECS),
                    dns_pending,
                    behind_nat,
                },
            )
        };
        let tls = if c.ready {
            "issued"
        } else if error.is_some() {
            "failed"
        } else {
            "pending"
        };
        // One that was issued and now fails: it couldn't be renewed.
        let lost: Option<(String, String)> = if tls == "failed" {
            sqlx::query_as(
                "SELECT d->>'id', s.project_id FROM pyo.services s, jsonb_array_elements(s.spec->'domains') d
                  WHERE s.id = $1 AND s.cluster_id = $2 AND NOT s.deleting AND d->>'hostname' = $3 AND d->>'tls' = 'issued'
                  LIMIT 1",
            )
            .bind(&c.service_id)
            .bind(&cluster.id)
            .bind(&c.hostname)
            .fetch_optional(&mut *tx)
            .await?
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
        if let Some((domain_id, project_id)) = lost {
            log_activity(
                &mut tx,
                &cluster.team_id,
                "pyo",
                "domain.certificate_failed",
                "failure",
                &format!(
                    "Couldn't renew the certificate for {}. {}",
                    c.hostname,
                    error.as_deref().unwrap_or("")
                ),
                json!({ "kind": "domain", "id": domain_id, "label": c.hostname }),
                json!({ "serviceId": c.service_id, "projectId": project_id, "clusterId": cluster.id }),
            )
            .await?;
        }
    }
    // Volumes: their state, deletions confirmed, claims Pyo has no volume for.
    crate::volumes::reported(&mut tx, &cluster.id, &hb.volumes).await?;
    // Let's Encrypt's word on Pyo's dashboard on the public IP: issued
    // there, the IP behind the cloud's NAT is reached from outside, and its
    // free domains may go there (freedomain.rs).
    if same_cluster {
        crate::publicdash::reported(&mut tx, hb.public_dashboard.as_ref()).await?;
    }
    // Free domains on another of its addresses (a private one, one it no
    // longer has, the other mode's) follow its free-domain address, live.
    let rehomed = crate::freedomain::rehome(&mut tx, &cluster.id).await?;
    // Where they point: on "tailscale" the agent publishes the websites on
    // the servers' Tailscale addresses.
    let free_domain_address = crate::freedomain::load(&mut tx, &cluster.id)
        .await?
        .map(|(_, c)| c.mode().as_str());
    tx.commit().await?;
    if refresh_sent || builder_added || rehomed {
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
    // The names its servers host on the tailnet.
    let tailnet = crate::tailscale::wanted_for(&s.db, &cluster.id, same_cluster).await?;
    // Pyo's dashboard on the public IP, while it's open.
    let public_dashboard = crate::publicdash::wanted_for(&s.db, same_cluster).await?;
    let reply = json!({ "ok": true, "clusterId": cluster.id, "serverLocations": server_locations, "serverTags": server_tags, "gpuPolicies": gpu_policies, "publicTraffic": public_traffic, "freeDomainAddress": free_domain_address, "projectTeams": project_teams, "observability": observability, "tailnet": tailnet, "publicDashboard": public_dashboard });
    Ok(Json(signed_reply(&cluster.id, &headers, reply).await))
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

/// Commands sent to an earlier process of the agent ($2 is the polling
/// one's instance) that it can't have finished, back to pending: a
/// rollout still under way, and the cluster's latest install refresh (it
/// changes nothing that's done already; an older one would put back an
/// older install).
const REQUEUE_ORPHANED: &str = "UPDATE pyo.commands c SET status = 'pending', sent_to = NULL
  WHERE c.cluster_id = $1 AND c.status = 'sent' AND c.sent_to IS DISTINCT FROM $2
    AND ((c.command->>'kind' = 'deploy'
          AND EXISTS (SELECT 1 FROM pyo.deployments d
                       WHERE d.id = c.command->>'deploymentId' AND d.status NOT IN ('succeeded', 'failed', 'cancelled')))
      OR (c.command->>'kind' = 'refresh_install'
          AND NOT EXISTS (SELECT 1 FROM pyo.commands n
                           WHERE n.cluster_id = c.cluster_id AND n.command->>'kind' = 'refresh_install'
                             AND n.created_at > c.created_at)))";

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
    // Commands handed to an earlier process of this agent were lost with
    // it when it restarted: hand them out again.
    if !instance.is_empty() {
        sqlx::query(REQUEUE_ORPHANED)
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
            // A hosted team's plan paused or ended: only reads and
            // removals go out, the rest fail unsigned (gate.rs).
            let held = crate::gate::held_back(&s.db, &cluster.team_id).await?;
            let mut out = Vec::with_capacity(rows.len());
            for (id, mut command, _) in rows {
                if let Some(why) = &held
                    && !crate::gate::allowed(command["kind"].as_str().unwrap_or(""))
                {
                    undeliverable(&s, &id, &command, why).await;
                    continue;
                }
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
                    Ok(command) => out.push(CommandEnvelope { id, command, signed: None }),
                    Err(e) => {
                        tracing::error!(%id, error = %e, "unreadable command");
                        undeliverable(&s, &id, &command, &format!("Pyo couldn't read it: {e}"))
                            .await;
                    }
                }
            }
            if let Some(signer) = crate::command_signer() {
                out = sign_all(&s, signer.as_ref(), &cluster.id, out).await?;
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

/// Has the signer sign each command for this cluster, in order. When it
/// can't, the commands go back to the queue and the agent asks again.
/// A command the signer refuses fails, with its reason; the rest go out.
async fn sign_all(
    s: &AppState,
    signer: &dyn crate::CommandSigner,
    cluster_id: &str,
    out: Vec<CommandEnvelope>,
) -> ApiResult<Vec<CommandEnvelope>> {
    if out.is_empty() {
        return Ok(out);
    }
    let mut items = Vec::with_capacity(out.len());
    let mut jsons = Vec::with_capacity(out.len());
    for e in out.iter() {
        let json = serde_json::to_string(&e.command).map_err(anyhow::Error::from)?;
        let kind = serde_json::from_str::<Value>(&json)
            .ok()
            .and_then(|v| v["kind"].as_str().map(str::to_owned))
            .unwrap_or_default();
        let command = crate::SIGNER_CHECKS.contains(&kind.as_str()).then(|| json.clone());
        items.push(crate::ToSign {
            id: e.id.clone(),
            kind,
            sha256: sha256_hex(&json),
            command,
        });
        jsons.push(json);
    }
    match signer.sign(cluster_id, &items).await {
        Ok(sigs) if sigs.len() == out.len() => {
            let mut signed = Vec::with_capacity(out.len());
            for ((mut e, sig), json) in out.into_iter().zip(sigs).zip(jsons) {
                if let Some(why) = sig.refused {
                    tracing::warn!(cluster = %cluster_id, id = %e.id, %why, "the signer refused a command");
                    let command: Value = serde_json::from_str(&json).unwrap_or(Value::Null);
                    undeliverable(s, &e.id, &command, &format!("Pyo's signer refused it: {why}")).await;
                    continue;
                }
                e.signed = Some(pyo_proto::SignedCommandWire {
                    claims: sig.claims,
                    sig: sig.sig,
                    kid: sig.kid,
                    command: json,
                    delegation: sig.delegation,
                });
                signed.push(e);
            }
            Ok(signed)
        }
        other => {
            let why = match other {
                Ok(_) => "the signer signed a different number of commands".to_owned(),
                Err(e) => format!("{e:#}"),
            };
            tracing::error!(cluster = %cluster_id, error = %why, "couldn't sign commands; they go back to the queue");
            let ids: Vec<&str> = out.iter().map(|e| e.id.as_str()).collect();
            sqlx::query("UPDATE pyo.commands SET status = 'pending', sent_at = NULL, sent_to = NULL WHERE cluster_id = $1 AND id = ANY($2) AND status = 'sent'")
                .bind(cluster_id)
                .bind(&ids)
                .execute(&s.db)
                .await?;
            Err(ApiError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "Commands can't be signed right now.",
            ))
        }
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
    // A Tailscale token or client secret, or a sandbox call's token: no
    // more use for it, so it isn't kept, even sealed.
    let mut wiped = command.clone();
    if crate::teambackup::wipe_command(&mut wiped) {
        sqlx::query("UPDATE pyo.commands SET command = $3 WHERE id = $1 AND cluster_id = $2")
            .bind(&id)
            .bind(&cluster.id)
            .bind(&wiped)
            .execute(&mut *tx)
            .await?;
    }
    if command["kind"] == json!("tailscale_sync") {
        crate::tailscale::synced(&mut tx, &cluster.id, &id, &r).await?;
    }
    if command["kind"] == json!("refresh_install") {
        crate::install::answered(&mut tx, &id, &cluster.id).await?;
    }
    // Why it failed, as the team reads it (the command keeps the agent's words).
    let install = if r.ok {
        String::new()
    } else {
        crate::install::state_of(&mut *tx, &cluster.id).await?
    };
    let shown = crate::install::shown(r.message.as_deref(), &install);
    crate::bundles::finished(&mut tx, &cluster.id, &id, &command, r.ok, shown.as_deref()).await?;
    // Databases set up (or updated): the databases waiting for them go out.
    if r.ok
        && command["kind"] == json!("apply_bundle")
        && command["bundle"]["id"] == json!(pyo_proto::bundles::DATABASES_ID)
    {
        crate::databases::resend(&s, &mut tx, &cluster.id).await?;
    }
    crate::databases::finished(&mut tx, &cluster.id, &command, r.ok, shown.as_deref()).await?;
    if command["kind"] == json!("set_origin_pull") {
        crate::certs::origin_pull_finished(&mut tx, &cluster.id, &command, r.ok, shown.as_deref())
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
                    shown.as_deref().unwrap_or("the agent failed")
                ),
                json!({ "kind": "cluster", "id": cluster.id, "label": cluster.id }),
                json!({ "clusterId": cluster.id }),
            )
            .await?;
        }
    }
    crate::volumes::finished(&mut tx, &cluster.id, &command, r.ok, shown.as_deref()).await?;
    crate::routes::finished(&mut tx, &cluster.team_id, &command, r.ok, shown.as_deref()).await?;
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

/// A cluster's traffic totals, from its traffic store through its agent
/// (totals.rs): finished minutes per host and proxied port. Raw requests
/// stay in the cluster.
async fn traffic_totals(
    State(s): State<AppState>,
    cluster: AgentCluster,
    Json(batch): Json<pyo_proto::traffic::TotalsBatch>,
) -> ApiResult<Json<Value>> {
    let stored = s.observability.ingest_totals(&cluster.id, &batch).await?;
    Ok(Json(json!({ "stored": stored })))
}

/// What the cluster's traffic store needs from Pyo: its services' hosts
/// and ports, their firewalls, the IP setting and the team's limits.
/// Signed like every answer the agent acts on.
async fn traffic_context(
    State(s): State<AppState>,
    cluster: AgentCluster,
    headers: axum::http::HeaderMap,
) -> ApiResult<Json<Value>> {
    let ctx = crate::totals::context(&s.db, &cluster.id, &cluster.team_id).await?;
    let reply = serde_json::to_value(ctx).map_err(anyhow::Error::from)?;
    Ok(Json(signed_reply(&cluster.id, &headers, reply).await))
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
    // A deployment shows why it didn't happen.
    if command["kind"] == json!("deploy")
        && let Some(deployment) = command["deploymentId"].as_str()
        && let Ok(cluster) =
            sqlx::query_scalar::<_, String>("SELECT cluster_id FROM pyo.commands WHERE id = $1")
                .bind(id)
                .fetch_one(&s.db)
                .await
    {
        let p = DeploymentProgress {
            status: "failed".into(),
            failure: Some(pyo_proto::Failure {
                stage: "applying".into(),
                reason: "NotSent".into(),
                message: why.to_owned(),
            }),
            ..Default::default()
        };
        let _ = deploy::apply_progress(s, &cluster, deployment, p).await;
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
/// Whether a server's last health entry in the activity says it's offline.
async fn offline_logged(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    cluster: &AgentCluster,
    server: &str,
) -> ApiResult<bool> {
    let last: Option<String> = sqlx::query_scalar(
        "SELECT data->>'type' FROM pyo.activity
          WHERE team_id = $1 AND data->>'type' IN ('server.offline', 'server.online')
            AND data->>'clusterId' = $2 AND data->'target'->>'id' = $3
          ORDER BY at DESC LIMIT 1",
    )
    .bind(&cluster.team_id)
    .bind(&cluster.id)
    .bind(server)
    .fetch_optional(&mut **tx)
    .await?;
    Ok(last.as_deref() == Some("server.offline"))
}

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

/// How long a certificate may take before it's shown as stuck (10 min).
const TLS_STUCK_SECS: i64 = 600;

/// What the API knows about a certificate's domain beyond cert-manager.
struct TlsContext {
    /// Still not issued after [`TLS_STUCK_SECS`].
    stuck: bool,
    /// Its DNS doesn't point at the cluster yet.
    dns_pending: bool,
    /// The cluster is behind NAT (a home router).
    behind_nat: bool,
}

/// Why a certificate isn't issued, as one plain sentence with what to do,
/// or None while it's on its way. cert-manager's and Let's Encrypt's own
/// text (ACME error URNs) is never shown.
fn tls_problem(c: &pyo_proto::CertificateReport, ctx: &TlsContext) -> Option<String> {
    let unreachable = || {
        if ctx.behind_nat {
            "Let's Encrypt can't reach this server on port 80. Forward ports 80 and 443 on your router to it."
        } else {
            "Let's Encrypt can't reach this server on port 80. Check that its firewall lets port 80 in."
        }
        .to_owned()
    };
    let dns = || "The domain doesn't point at this server yet. Check its DNS record.".to_owned();
    // An agent that only sends the Ready message says "failed" in it.
    let failed = c.failed || c.message.to_lowercase().contains("fail");
    if failed {
        let why = format!("{} {}", c.failure, c.message).to_lowercase();
        let has = |words: &[&str]| words.iter().any(|w| why.contains(w));
        return Some(if has(&["ratelimited", "rate limit", "too many"]) {
            if c.hostname.ends_with(".nip.io") {
                // nip.io isn't a public suffix, so every nip.io user shares
                // one weekly limit at Let's Encrypt. cert-manager retries
                // with a backoff, and slots come back over the week.
                "Let's Encrypt is busy for free nip.io addresses this week. Pyo tries again on its own. For HTTPS now, add your own domain.".to_owned()
            } else {
                "Let's Encrypt's limit for this domain is reached for now. Pyo tries again on its own.".to_owned()
            }
        } else if has(&["caa"]) {
            "The domain's CAA record doesn't allow Let's Encrypt. Add letsencrypt.org to it."
                .to_owned()
        } else if has(&["nxdomain", "dns problem", "no valid a records", "servfail"])
            || ctx.dns_pending
        {
            dns()
        } else if has(&[
            "connection",
            "timeout",
            "timed out",
            "unauthorized",
            "invalid response",
            "incorrect",
            "refused",
        ]) {
            unreachable()
        } else {
            "Let's Encrypt didn't issue a certificate. Pyo tries again on its own.".to_owned()
        });
    }
    if ctx.stuck {
        let why = if ctx.dns_pending {
            dns()
        } else {
            unreachable()
        };
        return Some(format!("No certificate after 10 minutes. {why}"));
    }
    None
}

#[cfg(test)]
mod tls_tests {
    use super::{TlsContext, tls_problem};
    use pyo_proto::CertificateReport;

    fn cert(failed: bool, failure: &str, host: &str) -> CertificateReport {
        CertificateReport {
            service_id: "svc".into(),
            hostname: host.into(),
            ready: false,
            message: "Issuing certificate as Secret does not exist".into(),
            failed,
            failure: failure.into(),
            issuing_since: Some(0),
        }
    }

    const CALM: TlsContext = TlsContext {
        stuck: false,
        dns_pending: false,
        behind_nat: false,
    };

    #[test]
    fn pending_is_quiet_until_stuck() {
        assert_eq!(tls_problem(&cert(false, "", "a.example.com"), &CALM), None);
        let stuck = tls_problem(
            &cert(false, "", "a.example.com"),
            &TlsContext {
                stuck: true,
                dns_pending: false,
                behind_nat: true,
            },
        )
        .unwrap();
        assert!(
            stuck.starts_with("No certificate after 10 minutes."),
            "{stuck}"
        );
        assert!(stuck.contains("router"), "{stuck}");
        let dns = tls_problem(
            &cert(false, "", "a.example.com"),
            &TlsContext {
                stuck: true,
                dns_pending: true,
                behind_nat: false,
            },
        )
        .unwrap();
        assert!(dns.contains("DNS record"), "{dns}");
    }

    #[test]
    fn acme_errors_become_plain_sentences() {
        let cases = [
            (
                "429 urn:ietf:params:acme:error:rateLimited: too many certificates (250000) already issued for \"nip.io\" in the last 168h0m0s",
                "a.1.2.3.4.nip.io",
                "busy for free nip.io",
            ),
            (
                "429 urn:ietf:params:acme:error:rateLimited: too many certificates",
                "shop.example.com",
                "limit for this domain",
            ),
            (
                "urn:ietf:params:acme:error:caa: CAA record prevents issuance",
                "a.example.com",
                "CAA record",
            ),
            (
                "urn:ietf:params:acme:error:dns: DNS problem: NXDOMAIN looking up A",
                "a.example.com",
                "DNS record",
            ),
            (
                "urn:ietf:params:acme:error:connection: Timeout during connect",
                "a.example.com",
                "port 80",
            ),
            ("something new", "a.example.com", "tries again on its own"),
        ];
        for (raw, host, want) in cases {
            let got = tls_problem(&cert(true, raw, host), &CALM).unwrap();
            assert!(got.contains(want), "{raw}: {got}");
            assert!(!got.contains("urn:") && !got.contains(';'), "{got}");
        }
    }

    #[test]
    fn an_older_agent_s_message_still_counts() {
        let mut c = cert(false, "", "a.example.com");
        c.message = "The certificate request has failed to complete".into();
        assert!(tls_problem(&c, &CALM).is_some());
    }
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

    /// Against a real Postgres (DATABASE_URL, a throwaway one): when the
    /// agent restarted, the install refresh its old process got is handed
    /// out again (the latest only), and so is a rollout under way; other
    /// commands, and those the polling process has, stay.
    #[tokio::test]
    #[ignore = "needs DATABASE_URL (a throwaway Postgres)"]
    async fn commands_lost_with_a_restarted_agent_are_sent_again() {
        let pool = sqlx::PgPool::connect(&std::env::var("DATABASE_URL").unwrap())
            .await
            .unwrap();
        crate::migrate::run(&pool).await.unwrap();
        let run = crate::ids::new_id("t");
        let (team, cluster) = (format!("team_{run}"), format!("cl_{run}"));
        sqlx::query("INSERT INTO pyo.teams (id, name, slug) VALUES ($1, $1, $1)")
            .bind(&team)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO pyo.clusters (id, team_id, name, agent_token_hash) VALUES ($1, $2, $1, $1)").bind(&cluster).bind(&team).execute(&pool).await.unwrap();
        let mut ids = vec![];
        for (i, (command, sent_to)) in [
            (json!({ "kind": "refresh_install", "hash": "old" }), "gone"),
            (json!({ "kind": "refresh_install", "hash": "new" }), "gone"),
            (json!({ "kind": "update_agent", "image": "a" }), "gone"),
            (
                json!({ "kind": "deploy", "deploymentId": format!("dep_{run}") }),
                "gone",
            ),
            (json!({ "kind": "refresh_install", "hash": "new" }), "me"),
        ]
        .into_iter()
        .enumerate()
        {
            let id = format!("cmd_{run}_{i}");
            sqlx::query("INSERT INTO pyo.commands (id, cluster_id, command, status, sent_at, sent_to, created_at) VALUES ($1, $2, $3, 'sent', now(), $4, now() + make_interval(secs => $5))")
                .bind(&id)
                .bind(&cluster)
                .bind(command)
                .bind(sent_to)
                .bind(i as f64)
                .execute(&pool)
                .await
                .unwrap();
            ids.push(id);
        }
        let statuses = || {
            sqlx::query_scalar::<_, String>(
                "SELECT status FROM pyo.commands WHERE cluster_id = $1 ORDER BY created_at",
            )
            .bind(&cluster)
            .fetch_all(&pool)
        };
        let requeue = || {
            sqlx::query(super::REQUEUE_ORPHANED)
                .bind(&cluster)
                .bind("me")
                .execute(&pool)
        };
        // The newest refresh is this process's: none of the old one's goes again.
        requeue().await.unwrap();
        assert_eq!(statuses().await.unwrap(), ["sent"; 5]);
        // Without it, the newest the old process got does (the rollout
        // has no deployment under way here).
        sqlx::query("DELETE FROM pyo.commands WHERE id = $1")
            .bind(&ids[4])
            .execute(&pool)
            .await
            .unwrap();
        requeue().await.unwrap();
        assert_eq!(
            statuses().await.unwrap(),
            ["sent", "pending", "sent", "sent"]
        );
        // Handed out to the new process: left with it.
        sqlx::query("UPDATE pyo.commands SET status = 'sent', sent_to = 'me' WHERE id = $1")
            .bind(&ids[1])
            .execute(&pool)
            .await
            .unwrap();
        requeue().await.unwrap();
        assert_eq!(statuses().await.unwrap(), ["sent"; 4]);
        sqlx::query("DELETE FROM pyo.teams WHERE id = $1")
            .bind(&team)
            .execute(&pool)
            .await
            .unwrap();
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

#[cfg(test)]
mod lookup_tests {
    use super::*;

    /// Against a throwaway Postgres: a cluster gets a certificate only when
    /// a domain of a service on it uses it, and a registry login only when
    /// a deployment on it pulls with it. Another cluster of the same team
    /// gets neither, and another team's cluster even less.
    #[tokio::test]
    #[ignore = "needs DATABASE_URL (a throwaway Postgres)"]
    async fn certificates_and_registry_logins_stay_with_their_cluster() {
        let db = sqlx::PgPool::connect(&std::env::var("DATABASE_URL").unwrap())
            .await
            .unwrap();
        crate::migrate::run(&db).await.unwrap();
        let run = crate::ids::random_string(8);
        let (team, other) = (format!("team_l{run}"), format!("team_o{run}"));
        let (here, there, theirs) = (
            format!("cl_h{run}"),
            format!("cl_t{run}"),
            format!("cl_o{run}"),
        );
        let (crt, rgl) = (format!("crt_{run}"), format!("rgl_{run}"));
        let q = |sql: &'static str| sqlx::query(sql);
        for t in [&team, &other] {
            q("INSERT INTO pyo.teams (id, name, slug) VALUES ($1, $1, $1)")
                .bind(t)
                .execute(&db)
                .await
                .unwrap();
        }
        for (c, t) in [(&here, &team), (&there, &team), (&theirs, &other)] {
            q("INSERT INTO pyo.clusters (id, team_id, name, agent_token_hash) VALUES ($1, $2, $1, $1)").bind(c).bind(t).execute(&db).await.unwrap();
        }
        q("INSERT INTO pyo.projects (id, team_id, name, namespace) VALUES ($1, $2, 'p', $1)")
            .bind(format!("prj_{run}"))
            .bind(&team)
            .execute(&db)
            .await
            .unwrap();
        let spec = json!({ "domains": [{ "id": "dom_1", "hostname": "shop.example.com", "tlsMode": "custom", "certificateId": crt }] });
        q("INSERT INTO pyo.services (id, team_id, project_id, cluster_id, name, spec) VALUES ($1, $2, $3, $4, 'web', $5)").bind(format!("svc_{run}")).bind(&team).bind(format!("prj_{run}")).bind(&here).bind(&spec).execute(&db).await.unwrap();
        q("INSERT INTO pyo.deployments (id, team_id, project_id, service_id, cluster_id, status, data, spec) VALUES ($1, $2, $3, $4, $5, 'succeeded', '{}', $6)").bind(format!("dpl_{run}")).bind(&team).bind(format!("prj_{run}")).bind(format!("svc_{run}")).bind(&here).bind(json!({ "pullLoginId": rgl })).execute(&db).await.unwrap();
        q("INSERT INTO pyo.certificates (id, team_id, name, hostnames, issuer, not_before, not_after, fingerprint, chain_pem, key_sealed) VALUES ($1, $2, $1, '{}', 'x', now(), now(), 'f', 'pem', 'key')").bind(&crt).bind(&team).execute(&db).await.unwrap();
        q("INSERT INTO pyo.registry_logins (id, team_id, host, username, secret_sealed) VALUES ($1, $2, 'ghcr.io', 'u', 'sealed')").bind(&rgl).bind(&team).execute(&db).await.unwrap();

        let gets = |cluster: &str, cluster_team: &str| {
            let (db, cluster, cluster_team, crt, rgl) = (
                db.clone(),
                cluster.to_owned(),
                cluster_team.to_owned(),
                crt.clone(),
                rgl.clone(),
            );
            async move {
                let cert: Option<(String, String)> = sqlx::query_as(CLUSTER_CERTIFICATE)
                    .bind(&crt)
                    .bind(&cluster_team)
                    .bind(&cluster)
                    .fetch_optional(&db)
                    .await
                    .unwrap();
                let login: Option<(String, String, String)> =
                    sqlx::query_as(CLUSTER_REGISTRY_LOGIN)
                        .bind(&rgl)
                        .bind(&cluster_team)
                        .bind(&cluster)
                        .fetch_optional(&db)
                        .await
                        .unwrap();
                (cert.is_some(), login.is_some())
            }
        };
        assert_eq!(gets(&here, &team).await, (true, true));
        assert_eq!(
            gets(&there, &team).await,
            (false, false),
            "another cluster of the team"
        );
        assert_eq!(
            gets(&theirs, &other).await,
            (false, false),
            "another team's cluster"
        );
        assert_eq!(
            gets(&theirs, &team).await,
            (false, false),
            "a cluster the team doesn't own"
        );

        for t in [&team, &other] {
            sqlx::query("DELETE FROM pyo.teams WHERE id = $1")
                .bind(t)
                .execute(&db)
                .await
                .unwrap();
        }
    }
}

#[cfg(test)]
mod reply_tests {
    use super::*;
    use pyo_proto::signing::{REPLY_TTL_MS, ReplyClaims, Signer, TrustedKeys};

    /// A signer here, as cloud-signer signs replies.
    struct Local(Signer);

    impl crate::CommandSigner for Local {
        fn sign<'a>(&'a self, _: &'a str, _: &'a [crate::ToSign]) -> futures::future::BoxFuture<'a, anyhow::Result<Vec<crate::Signature>>> {
            Box::pin(async { Ok(vec![]) })
        }
        fn sign_reply<'a>(&'a self, cluster_id: &'a str, r: &'a crate::ReplyToSign) -> futures::future::BoxFuture<'a, anyhow::Result<crate::Signature>> {
            Box::pin(async move {
                let now = pyo_proto::now_ms() as u64;
                let claims = ReplyClaims { v: 1, cluster: cluster_id.into(), nonce: r.nonce.clone(), iat: now, exp: now + REPLY_TTL_MS, sha256: r.sha256.clone() };
                let (claims, sig) = self.0.sign_reply(&claims);
                Ok(crate::Signature { claims, sig, kid: self.0.kid().into(), refused: None, delegation: None })
            })
        }
        fn public_keys(&self) -> String {
            self.0.public_key()
        }
    }

    fn headers(nonce: &str) -> axum::http::HeaderMap {
        let mut h = axum::http::HeaderMap::new();
        h.insert(pyo_proto::REPLY_NONCE_HEADER, nonce.parse().unwrap());
        h
    }

    #[tokio::test]
    async fn replies_are_signed_for_the_cluster_and_the_request() {
        let signer = Local(Signer::from_pkcs8(&Signer::generate_pkcs8().unwrap()).unwrap());
        let keys = TrustedKeys::parse(&crate::CommandSigner::public_keys(&signer)).unwrap();
        let reply = json!({ "ok": true, "publicTraffic": true, "serverTags": [{ "server": "s1", "tags": ["web"] }] });
        let out = sign_reply_with(&signer, "cl_a", &headers("nonce-1234"), reply.clone()).await;
        // The plain fields stay, for agents that don't check.
        assert_eq!(out["publicTraffic"], json!(true));
        let wire: pyo_proto::SignedReplyWire = serde_json::from_value(out["signed"].clone()).unwrap();
        let now = pyo_proto::now_ms() as u64;
        let signed = keys.verify_reply(Some(&wire), "cl_a", "nonce-1234", now).unwrap();
        assert_eq!(serde_json::from_str::<Value>(signed).unwrap(), reply);
        assert!(keys.verify_reply(Some(&wire), "cl_b", "nonce-1234", now).is_err());
        assert!(keys.verify_reply(Some(&wire), "cl_a", "nonce-9999", now).is_err());
        // No nonce, or not one an agent makes: nothing to bind it to, so unsigned.
        let plain = sign_reply_with(&signer, "cl_a", &axum::http::HeaderMap::new(), reply.clone()).await;
        assert!(plain.get("signed").is_none());
        let odd = sign_reply_with(&signer, "cl_a", &headers("short"), reply.clone()).await;
        assert!(odd.get("signed").is_none());
    }
}

#[cfg(test)]
mod gate_tests {
    use super::*;
    use pyo_proto::Command;

    /// A hosted build whose teams named `team_ended_…` have a paused or
    /// ended plan: deploys are refused, so commands are too (the default).
    struct Ended;

    impl crate::Entitlements for Ended {
        fn can_add_server<'a>(
            &'a self,
            _: &'a mut sqlx::PgConnection,
            _: &'a str,
        ) -> futures::future::BoxFuture<'a, Result<(), String>> {
            Box::pin(async { Ok(()) })
        }
        fn can_deploy<'a>(
            &'a self,
            _: &'a mut sqlx::PgConnection,
            team_id: &'a str,
        ) -> futures::future::BoxFuture<'a, Result<(), String>> {
            Box::pin(async move {
                if team_id.starts_with("team_ended_") {
                    Err("This team's subscription ended.".into())
                } else {
                    Ok(())
                }
            })
        }
    }

    /// Against a throwaway Postgres: a person's request for a change to an
    /// ended team's cluster is refused with the plain message, a removal
    /// isn't. The agent then gets only the removal: what Pyo queued itself
    /// fails unsigned, with the same message. Other teams are untouched.
    #[tokio::test]
    #[ignore = "needs DATABASE_URL (a throwaway Postgres)"]
    async fn an_ended_teams_clusters_get_only_reads_and_removals() {
        let db = sqlx::PgPool::connect(&std::env::var("DATABASE_URL").unwrap())
            .await
            .unwrap();
        crate::migrate::run(&db).await.unwrap();
        crate::vault::init(crate::vault::Vault::new(
            db.clone(),
            crate::vault::Provider::from_env(),
        ));
        let _ = crate::ENTITLEMENTS.set(std::sync::Arc::new(Ended));
        let run = crate::ids::random_string(8).to_lowercase();
        let s = crate::rls::tests::state(db.clone());
        let mut teams = vec![];
        for (t, ended) in [("a", true), ("b", false)] {
            let team = if ended {
                format!("team_ended_{run}")
            } else {
                format!("team_ok_{run}")
            };
            let (user, cluster, project, svc) = (
                format!("usr_{t}{run}"),
                format!("cl_{t}{run}"),
                format!("prj_{t}{run}"),
                format!("svc_{t}{run}"),
            );
            let q = |sql: &'static str| sqlx::query(sql);
            q("INSERT INTO pyo.users (id, email, name, password_hash) VALUES ($1, $1 || '@gate.test', $1, '')").bind(&user).execute(&db).await.unwrap();
            q("INSERT INTO pyo.teams (id, name, slug) VALUES ($1, $1, $1)")
                .bind(&team)
                .execute(&db)
                .await
                .unwrap();
            q("INSERT INTO pyo.members (id, team_id, user_id, email, role, status) VALUES ($1, $2, $3, $3 || '@gate.test', 'admin', 'active')").bind(format!("mem_{t}{run}")).bind(&team).bind(&user).execute(&db).await.unwrap();
            q("INSERT INTO pyo.clusters (id, team_id, name, agent_token_hash, status, last_seen_at) VALUES ($1, $2, $1, $1, 'connected', now())").bind(&cluster).bind(&team).execute(&db).await.unwrap();
            q("INSERT INTO pyo.projects (id, team_id, name, namespace) VALUES ($1, $2, 'shop', $1)").bind(&project).bind(&team).execute(&db).await.unwrap();
            q("INSERT INTO pyo.services (id, team_id, project_id, cluster_id, name, spec, status) VALUES ($1, $2, $3, $4, 'web', '{}', 'running')").bind(&svc).bind(&team).bind(&project).bind(&cluster).execute(&db).await.unwrap();
            let person = crate::auth::User {
                id: user,
                name: t.into(),
                email: format!("{t}@gate.test"),
                timezone: "UTC".into(),
                is_admin: false,
                must_set_up: false,
                key: None,
            };
            let service = pyo_proto::ServiceRef {
                service_id: svc,
                name: "web".into(),
                namespace: project,
                workload_kind: "Deployment".into(),
            };
            teams.push((team, cluster, person, service, ended));
        }
        for (team, cluster, person, service, ended) in &teams {
            // A person's restart: refused for the ended team, with why.
            let mut tx = crate::rls::begin(&db, person).await.unwrap();
            let restart =
                deploy::enqueue(&mut tx, cluster, Command::Restart(service.clone())).await;
            if *ended {
                let e = restart.expect_err("refused");
                assert_eq!(e.status, StatusCode::PAYMENT_REQUIRED);
                assert_eq!(e.message, crate::gate::STOPPED);
            } else {
                restart.unwrap();
            }
            // Removals and their follow-ons are queued.
            deploy::enqueue(&mut tx, cluster, Command::DeleteService(service.clone()))
                .await
                .unwrap();
            deploy::enqueue(
                &mut tx,
                cluster,
                Command::SyncCronJobs {
                    service: service.clone(),
                    jobs: vec![],
                },
            )
            .await
            .unwrap();
            tx.commit().await.unwrap();
            // Pyo's own, queued outside a request.
            let mut tx = db.begin().await.unwrap();
            deploy::enqueue(&mut tx, cluster, Command::Stop(service.clone()))
                .await
                .unwrap();
            tx.commit().await.unwrap();

            let Json(got) = poll(
                State(s.clone()),
                AgentCluster {
                    id: cluster.clone(),
                    team_id: team.clone(),
                },
                axum::http::HeaderMap::new(),
                Query(PollQuery { wait: Some(0) }),
            )
            .await
            .unwrap();
            let kinds: Vec<String> = got.iter().map(|e| command_kind(&e.command)).collect();
            let rows: Vec<(String, String, Option<String>)> = sqlx::query_as(
                "SELECT command->>'kind', status, result->>'message' FROM pyo.commands WHERE cluster_id = $1 ORDER BY created_at",
            )
            .bind(cluster)
            .fetch_all(&db)
            .await
            .unwrap();
            if *ended {
                assert_eq!(kinds, ["delete_service"]);
                for (kind, status, message) in &rows {
                    if kind != "delete_service" {
                        assert_eq!(
                            (status.as_str(), message.as_deref()),
                            ("failed", Some(crate::gate::STOPPED)),
                            "{kind}"
                        );
                    }
                }
            } else {
                assert_eq!(
                    kinds,
                    ["restart", "delete_service", "sync_cron_jobs", "stop"]
                );
                assert!(rows.iter().all(|(_, status, _)| status == "sent"));
            }
        }
        for (team, ..) in &teams {
            sqlx::query("DELETE FROM pyo.teams WHERE id = $1")
                .bind(team)
                .execute(&db)
                .await
                .unwrap();
        }
    }

    fn command_kind(c: &Command) -> String {
        serde_json::to_value(c).unwrap()["kind"]
            .as_str()
            .unwrap()
            .to_owned()
    }
}
