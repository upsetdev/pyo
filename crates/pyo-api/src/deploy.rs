//! Deployments: from a service's saved configuration to a DeploySpec the agent
//! applies, then the stages it reports until the rollout settles.

use std::time::Duration;

use pyo_proto::{
    BuildResult, Command, DeploySpec, DeploymentProgress, EnvVar, HealthCheck, PortSpec, Resources,
    ServiceRef, VolumeSpec, now_ms,
};
use serde_json::{Value, json};
use sqlx::{Postgres, Transaction};

use crate::{
    AppState,
    error::{ApiError, ApiResult},
    ids::new_id,
    ops::log_activity,
};

/// A rollout that hasn't settled after this long is reported failed.
pub const ROLLOUT_TIMEOUT_SECS: u64 = 300;

/// Stages an image deploy goes through, in order.
const STAGES: [&str; 6] = [
    "queued",
    "applying",
    "scheduling",
    "pulling",
    "starting",
    "waiting_ready",
];
/// A Git deploy builds and pushes the image first.
const GIT_STAGES: [&str; 8] = [
    "queued",
    "building",
    "pushing",
    "applying",
    "scheduling",
    "pulling",
    "starting",
    "waiting_ready",
];

/// Pyo's registry, by who reaches it how (all host:port, plain HTTP):
/// - `push`: its door on the tailnet (pyo-host, `<tailnet IP>:5005`), where
///   other machines' builders push and their containerd pulls, with a login.
///   Empty without a tailnet.
/// - `pull`: the name every node pulls by: on Pyo's node, the registry
///   itself through pyo-host; elsewhere mirrored to the door (registries.yaml).
/// - `internal`: the registry itself, in the cluster only (Pyo's own calls).
pub struct Registry {
    pub push: String,
    pub pull: String,
    pub internal: String,
}
pub static REGISTRY: std::sync::OnceLock<Registry> = std::sync::OnceLock::new();
pub const PULL_DEFAULT: &str = "127.0.0.1:5005";
pub const INTERNAL_DEFAULT: &str = "pyo-registry.pyo-system.svc.cluster.local:5000";
/// The door in the cluster, for builders on Pyo's own cluster.
pub const DOOR_IN_CLUSTER: &str = "pyo-registry-door.pyo-system.svc.cluster.local:5006";

pub fn registry() -> &'static Registry {
    REGISTRY.get_or_init(|| Registry {
        push: String::new(),
        pull: PULL_DEFAULT.into(),
        internal: INTERNAL_DEFAULT.into(),
    })
}

/// Where a builder on this cluster pushes: the door in the cluster on Pyo's
/// own, its tailnet address elsewhere (None without one).
pub fn builder_registry(on_pyos_cluster: bool) -> Option<String> {
    if on_pyos_cluster {
        return Some(DOOR_IN_CLUSTER.into());
    }
    let push = &registry().push;
    (!push.is_empty()).then(|| push.clone())
}

/// A build job for a deployment's Git source.
fn build_job(
    id: &str,
    deployment_id: &str,
    service_id: &str,
    source: &Value,
) -> pyo_proto::BuildJob {
    let r = registry();
    pyo_proto::BuildJob {
        id: id.to_owned(),
        deployment_id: deployment_id.to_owned(),
        repo_url: str_at(source, &["repoUrl"]).trim().to_owned(),
        branch: match str_at(source, &["branch"]).trim() {
            "" => "main".to_owned(),
            b => b.to_owned(),
        },
        commit: str_at(source, &["commit"]).trim().to_owned(),
        dockerfile_path: match str_at(source, &["dockerfilePath"]).trim() {
            "" => "Dockerfile".to_owned(),
            d => d.trim_start_matches("./").to_owned(),
        },
        build_context: match str_at(source, &["buildContext"]).trim() {
            "" => ".".to_owned(),
            c => c.to_owned(),
        },
        build_args: source["buildArgs"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter(|x| !str_at(x, &["key"]).is_empty())
                    .map(|x| EnvVar {
                        key: str_at(x, &["key"]).to_owned(),
                        value: str_at(x, &["value"]).to_owned(),
                    })
                    .collect()
            })
            .unwrap_or_default(),
        // The builder that takes it pushes to its own address for the
        // registry (builders.rs `poll`).
        push_repository: format!("{}/pyo/services/{service_id}", r.pull),
        registry_host: r.pull.clone(),
        timeout_seconds: 30 * 60,
        github_installation_id: source["installationId"].as_i64(),
        clone_token: None,
    }
}
const FINAL: [&str; 3] = ["succeeded", "failed", "cancelled"];

pub fn is_final(status: &str) -> bool {
    FINAL.contains(&status)
}

fn str_at<'a>(v: &'a Value, path: &[&str]) -> &'a str {
    let mut cur = v;
    for p in path {
        cur = &cur[*p];
    }
    cur.as_str().unwrap_or("")
}

fn int_at(v: &Value, path: &[&str], default: i64) -> i64 {
    let mut cur = v;
    for p in path {
        cur = &cur[*p];
    }
    cur.as_i64()
        .or_else(|| cur.as_f64().map(|f| f as i64))
        .unwrap_or(default)
}

/// "nginx" + "1.27" -> "nginx:1.27"; digests use "@".
pub fn image_ref(source: &Value) -> ApiResult<String> {
    match str_at(source, &["type"]) {
        "image" | "template" => {
            let image = str_at(source, &["image"]).trim();
            let reference = str_at(source, &["reference"]).trim();
            if image.is_empty() {
                return Err(ApiError::bad_request("The service has no image."));
            }
            Ok(match reference {
                "" => image.to_owned(),
                r if r.starts_with("sha256:") => format!("{image}@{r}"),
                r => format!("{image}:{r}"),
            })
        }
        // Built by a builder; the image is known once the build finishes.
        "git" => {
            let url = str_at(source, &["repoUrl"]).trim();
            if !(url.starts_with("https://") || url.starts_with("http://"))
                || url.contains(char::is_whitespace)
            {
                return Err(ApiError::bad_request(
                    "Use the repository's HTTPS URL, e.g. https://github.com/owner/repo.",
                ));
            }
            for (k, what) in [
                ("dockerfilePath", "Dockerfile path"),
                ("buildContext", "Build context"),
                ("branch", "Branch"),
            ] {
                let v = str_at(source, &[k]);
                if v.contains("..") || v.contains(['#', ':', ' ']) {
                    return Err(ApiError::bad_request(format!(
                        "{what} can't contain '..', '#', ':' or spaces."
                    )));
                }
            }
            Ok(String::new())
        }
        _ => Err(ApiError::bad_request("Unknown service source.")),
    }
}

/// A port as the agent gets it, from a saved one. A public TCP or UDP port
/// goes through the gateway unless `proxy` is false; its options
/// (all off unless set) only apply then.
pub fn port_spec(p: &Value) -> Option<PortSpec> {
    let exposure = str_at(p, &["exposure"]).to_owned();
    let protocol = str_at(p, &["protocol"]).to_owned();
    let proxy =
        (exposure == "loadbalancer" && p["proxy"] != json!(false)).then(|| pyo_proto::PortProxy {
            ip_filter: p["ipFilter"].is_object().then(|| pyo_proto::IpFilter {
                mode: str_at(p, &["ipFilter", "mode"]).to_owned(),
                cidrs: p["ipFilter"]["cidrs"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|c| c.as_str().and_then(normalize_cidr))
                    .collect(),
            }),
            connection_limit: p["connectionLimit"]
                .as_u64()
                .map(|n| n.min(u32::MAX as u64) as u32),
            send_proxy_protocol: str_at(p, &["sendProxyProtocol"]).to_owned(),
            accept_proxy_protocol: p["acceptProxyProtocol"].as_bool().unwrap_or(false),
        });
    Some(PortSpec {
        name: str_at(p, &["name"]).to_owned(),
        container_port: int_at(p, &["containerPort"], 0) as i32,
        protocol,
        exposure,
        public_port: p["publicPort"].as_i64().map(|x| x as i32),
        proxy,
    })
}

/// An address or range as a range ("203.0.113.7" is "203.0.113.7/32"),
/// with the host bits cleared; none when it's neither.
pub fn normalize_cidr(s: &str) -> Option<String> {
    let s = s.trim();
    let (addr, bits) = match s.split_once('/') {
        Some((a, b)) => (a, Some(b.parse::<u8>().ok()?)),
        None => (s, None),
    };
    match addr.parse::<std::net::IpAddr>().ok()? {
        std::net::IpAddr::V4(v) => {
            let bits = bits.unwrap_or(32);
            if bits > 32 {
                return None;
            }
            let mask = if bits == 0 {
                0
            } else {
                u32::MAX << (32 - bits)
            };
            Some(format!(
                "{}/{bits}",
                std::net::Ipv4Addr::from(u32::from(v) & mask)
            ))
        }
        std::net::IpAddr::V6(v) => {
            let bits = bits.unwrap_or(128);
            if bits > 128 {
                return None;
            }
            let mask = if bits == 0 {
                0
            } else {
                u128::MAX << (128 - bits)
            };
            Some(format!(
                "{}/{bits}",
                std::net::Ipv6Addr::from(u128::from(v) & mask)
            ))
        }
    }
}

/// The spec the agent applies, from the service's saved configuration.
/// Which issuer signs a domain's certificate: Let's Encrypt for public
/// domains; self-signed for ones only a tailnet or LAN can reach, which Let's
/// Encrypt can't validate; none when HTTPS is off.
pub fn tls_issuer(mode: &str, hostname: &str) -> &'static str {
    match mode {
        "off" => "",
        "letsencrypt" => "letsencrypt",
        "selfsigned" => "selfsigned",
        "custom" => "custom",
        _ if private_host(hostname) => "selfsigned",
        _ => "letsencrypt",
    }
}

/// Reachable only privately: local names, or nip.io/sslip.io names of a
/// private, tailnet or loopback address.
pub fn private_host(host: &str) -> bool {
    let h = host.to_lowercase();
    if [
        ".localhost",
        ".local",
        ".internal",
        ".lan",
        ".home.arpa",
        ".test",
    ]
    .iter()
    .any(|s| h.ends_with(s))
        || h == "localhost"
    {
        return true;
    }
    let Some(rest) = h
        .strip_suffix(".nip.io")
        .or_else(|| h.strip_suffix(".sslip.io"))
    else {
        return false;
    };
    let ip = rest.rsplit('.').next().unwrap_or("").replace('-', ".");
    let octets: Vec<u8> = ip.split('.').filter_map(|o| o.parse().ok()).collect();
    match octets[..] {
        [10, ..] | [127, ..] | [192, 168, ..] => true,
        [172, b, ..] => (16..=31).contains(&b),
        [100, b, ..] => (64..=127).contains(&b),
        _ => false,
    }
}

/// `service_id` is the label id its Kubernetes objects carry.
/// A service's autoscaling, unless it can't run several replicas side by side.
pub fn autoscale_for(spec: &Value, deploy: &DeploySpec) -> Option<pyo_proto::Autoscale> {
    let single_writer = deploy
        .volumes
        .iter()
        .any(|v| v.access_mode == "ReadWriteOnce");
    if deploy.service.workload_kind != "Deployment" || single_writer {
        return None;
    }
    serde_json::from_value(spec["autoscale"].clone()).ok()
}

pub fn build_spec(
    deployment_id: &str,
    service_id: &str,
    project_id: &str,
    namespace: &str,
    spec: &Value,
    volumes: &[Value],
) -> ApiResult<DeploySpec> {
    let name = str_at(spec, &["name"]).to_owned();
    let workload_kind = match str_at(spec, &["workloadKind"]) {
        "StatefulSet" => "StatefulSet",
        "CronJob" => "CronJob",
        "Deployment" | "" => "Deployment",
        other => {
            return Err(ApiError::bad_request(format!(
                "{other} workloads aren't supported yet."
            )));
        }
    };
    let ports: Vec<PortSpec> = spec["ports"]
        .as_array()
        .map(|ps| ps.iter().filter_map(port_spec).collect())
        .unwrap_or_default();
    let domains = crate::routes::domain_specs(&spec["domains"]);
    let mut env: Vec<EnvVar> = spec["env"]
        .as_array()
        .map(|es| {
            es.iter()
                .filter(|e| !str_at(e, &["key"]).is_empty())
                .map(|e| EnvVar {
                    key: str_at(e, &["key"]).to_owned(),
                    value: str_at(e, &["value"]).to_owned(),
                })
                .collect()
        })
        .unwrap_or_default();
    // Most frameworks listen on $PORT, so zero-config builds serve on the
    // service's port without any setup. A PORT the service sets wins.
    if let Some(p) = ports.first()
        && !env.iter().any(|e| e.key == "PORT")
    {
        env.push(EnvVar {
            key: "PORT".into(),
            value: p.container_port.to_string(),
        });
    }
    let r = &spec["resources"];
    let h = &spec["healthCheck"];
    Ok(DeploySpec {
        fallback: None,
        pull_login_id: String::new(),
        network_isolation: false,
        firewall: None,
        waf: None,
        server_names: spec["servers"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|n| n.as_str())
            .filter(|n| !n.is_empty())
            .map(str::to_owned)
            .collect(),
        node_selector: {
            let mut m: std::collections::BTreeMap<String, String> = Default::default();
            if let Some((region, zone)) = pyo_proto::location_labels(str_at(spec, &["location"])) {
                m.insert(pyo_proto::REGION_LABEL.to_owned(), region);
                if let Some(z) = zone {
                    m.insert(pyo_proto::ZONE_LABEL.to_owned(), z);
                }
            }
            // Placed by tag: only on servers carrying every one of them.
            for t in spec["serverTags"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|t| t.as_str().and_then(pyo_proto::valid_tag))
            {
                m.insert(format!("{}{t}", pyo_proto::TAG_LABEL_PREFIX), "true".into());
            }
            m
        },
        deployment_id: deployment_id.to_owned(),
        service: ServiceRef {
            service_id: service_id.to_owned(),
            name,
            namespace: namespace.to_owned(),
            workload_kind: workload_kind.to_owned(),
        },
        project_id: project_id.to_owned(),
        team_id: String::new(),
        image: image_ref(&spec["source"])?,
        replicas: int_at(spec, &["replicas"], 1).clamp(0, 50) as i32,
        schedule: str_at(spec, &["schedule"]).trim().to_owned(),
        command: str_at(spec, &["command"]).trim().to_owned(),
        container: serde_json::from_value::<pyo_proto::ContainerOverrides>(
            spec["container"].clone(),
        )
        .ok()
        .filter(|c| !c.is_empty()),
        cron_jobs: vec![],
        traffic_mode: String::new(),
        autoscale: None,
        env,
        ports,
        domains,
        routes_revision: 0,
        resources: Resources {
            cpu_request: int_at(r, &["cpuRequest"], 100),
            cpu_limit: int_at(r, &["cpuLimit"], 500),
            memory_request: int_at(r, &["memoryRequest"], 128),
            memory_limit: int_at(r, &["memoryLimit"], 512),
            gpu: int_at(r, &["gpu"], 0).clamp(0, 8) as u32,
        },
        health_check: HealthCheck {
            kind: match str_at(h, &["type"]) {
                "" => "none".into(),
                t => t.to_owned(),
            },
            path: str_at(h, &["path"]).to_owned(),
            port: int_at(h, &["port"], 0) as i32,
            command: str_at(h, &["command"]).to_owned(),
            initial_delay_seconds: int_at(h, &["initialDelaySeconds"], 5) as i32,
            period_seconds: int_at(h, &["periodSeconds"], 10) as i32,
        },
        volumes: volume_specs(volumes),
        timeout_seconds: ROLLOUT_TIMEOUT_SECS,
    })
}

/// The volumes a rollout mounts, from their rows (with `id`).
pub fn volume_specs(volumes: &[Value]) -> Vec<VolumeSpec> {
    volumes
        .iter()
        .filter(|v| !str_at(v, &["mountPath"]).is_empty())
        .map(|v| VolumeSpec {
            volume_id: str_at(v, &["id"]).to_owned(),
            name: str_at(v, &["name"]).to_owned(),
            mount_path: str_at(v, &["mountPath"]).to_owned(),
            size_gi: int_at(v, &["sizeGi"], 1),
            storage_class: str_at(v, &["storageClass"]).to_owned(),
            access_mode: match str_at(v, &["accessMode"]) {
                "" => "ReadWriteOnce".into(),
                m => m.to_owned(),
            },
        })
        .collect()
}

/// A service's volumes as they are now (attached, not being deleted).
async fn attached_volumes(
    tx: &mut Transaction<'_, Postgres>,
    service_id: &str,
) -> ApiResult<Vec<Value>> {
    Ok(sqlx::query_scalar("SELECT data || jsonb_build_object('id', id) FROM pyo.volumes WHERE service_id = $1 AND NOT deleting ORDER BY created_at")
        .bind(service_id)
        .fetch_all(&mut **tx)
        .await?)
}

pub struct DeployRequest<'a> {
    pub deployment_id: &'a str,
    pub service_id: &'a str,
    pub trigger: &'a str,
    pub actor: &'a str,
    pub rollback_of: Option<&'a str>,
    pub retry_of: Option<&'a str>,
    /// Build this commit instead of the source's pin or branch head (a push).
    pub commit: Option<&'a str>,
}

/// What a deployment needs of its service: team, project, cluster, spec,
/// namespace, status, current deployment.
type ServiceRow = (
    String,
    String,
    String,
    Value,
    String,
    String,
    Option<String>,
);

/// Queue a deployment for a service and hand it to the cluster's agent.
pub async fn create(
    tx: &mut Transaction<'_, Postgres>,
    req: DeployRequest<'_>,
) -> ApiResult<String> {
    let row: Option<ServiceRow> = sqlx::query_as(
        "SELECT a.team_id, a.project_id, a.cluster_id, a.spec, p.namespace, a.status, a.current_deployment_id
           FROM pyo.services a JOIN pyo.projects p ON p.id = a.project_id
          WHERE a.id = $1 AND NOT a.deleting FOR UPDATE OF a",
    )
    .bind(req.service_id)
    .fetch_optional(&mut **tx)
    .await?;
    let (team_id, project_id, cluster_id, mut spec, namespace, prev_status, live_id) =
        row.ok_or_else(|| ApiError::not_found("Service"))?;
    crate::check_deploy(tx, &team_id).await?;
    crate::volumes::not_restoring(tx, req.service_id).await?;
    // Values are sealed at rest (vault.rs); the deploy needs them in the clear.
    crate::vault::get().open_env(&mut spec).await?;

    let in_flight: Option<String> = sqlx::query_scalar("SELECT id FROM pyo.deployments WHERE service_id = $1 AND status NOT IN ('succeeded', 'failed', 'cancelled') LIMIT 1")
        .bind(req.service_id)
        .fetch_optional(&mut **tx)
        .await?;
    if in_flight.is_some() {
        return Err(ApiError::conflict(
            "A deployment of this service is already in progress. Wait for it or cancel it first.",
        ));
    }

    // Rollbacks and retries reuse the exact spec of an earlier deployment.
    let earlier = req.rollback_of.or(req.retry_of);
    let (mut deploy_spec, source) = if let Some(prev_id) = earlier {
        let (prev_spec, prev_data): (Value, Value) = sqlx::query_as(
            "SELECT spec, data FROM pyo.deployments WHERE id = $1 AND service_id = $2",
        )
        .bind(prev_id)
        .bind(req.service_id)
        .fetch_optional(&mut **tx)
        .await?
        .ok_or_else(|| ApiError::not_found("Deployment"))?;
        if req.rollback_of.is_some() && prev_data["imageExpired"] == json!(true) {
            return Err(ApiError::conflict(
                "That version's image was cleaned up to save space, so it can't be rolled back to. Pick a newer deployment.",
            ));
        }
        let mut prev_spec = prev_spec;
        crate::vault::get().open_env(&mut prev_spec).await?;
        let mut s: DeploySpec = serde_json::from_value(prev_spec).map_err(anyhow::Error::from)?;
        s.deployment_id = req.deployment_id.to_owned();
        // The exact image that ran then, even if its tag has moved since.
        if req.rollback_of.is_some()
            && let Some(img) = prev_data["pinnedImage"].as_str()
        {
            s.image = img.to_owned();
        }
        (s, prev_data["source"].clone())
    } else {
        let volumes = attached_volumes(tx, req.service_id).await?;
        let mut built = build_spec(
            req.deployment_id,
            req.service_id,
            &project_id,
            &namespace,
            &spec,
            &volumes,
        )?;
        crate::secretstores::import_env(tx, &team_id, &spec, &mut built.env).await?;
        // `${{postgres.DATABASE_URL}}` and the like, from the project's services.
        crate::refs::resolve(tx, &project_id, &namespace, &mut built.env).await?;
        (built, spec["source"].clone())
    };
    // Its volumes as they are now, rollbacks included: a volume detached
    // (or attached) since isn't mounted (or is) by an older version.
    deploy_spec.volumes = volume_specs(&attached_volumes(tx, req.service_id).await?);
    // The cluster's traffic mode now, rollbacks included.
    deploy_spec.traffic_mode = sqlx::query_scalar::<_, Option<String>>(
        "SELECT traffic_mode FROM pyo.clusters WHERE id = $1",
    )
    .bind(&cluster_id)
    .fetch_one(&mut **tx)
    .await?
    .unwrap_or_else(|| pyo_proto::TRAFFIC_REGION.into());

    // A private registry's login, matched by the image's host (built images
    // come from Pyo's own registry and need none).
    // The service's setting today, even when rolling back to an older spec.
    deploy_spec.autoscale = autoscale_for(&spec, &deploy_spec);
    // And its cron jobs, which are managed apart from deploys.
    deploy_spec.cron_jobs = serde_json::from_value(spec["cronJobs"].clone()).unwrap_or_default();
    deploy_spec.network_isolation =
        sqlx::query_scalar("SELECT network_isolation FROM pyo.projects WHERE id = $1")
            .bind(&project_id)
            .fetch_one(&mut **tx)
            .await?;
    // Its namespace's team label, which lets it reach the team's databases.
    deploy_spec.team_id = team_id.clone();
    // Its firewall as it is now, rollbacks included.
    crate::firewall::fill(tx, &cluster_id, req.service_id, &mut deploy_spec).await?;
    // An image in Pyo registry, by whatever name it's given (the door's,
    // the tailnet's, localhost's...): as servers pull it. Then, whoever's it
    // is: a build of one of the team's services, or a repository the team
    // pushed first (names stay put when a team, project or service is
    // renamed). Nothing else of Pyo registry's, and nothing else on this
    // machine's own addresses.
    match pyo_registry_image(&deploy_spec.image) {
        Some(Ok(pulled)) => deploy_spec.image = pulled,
        Some(Err(why)) => return Err(ApiError::forbidden(why)),
        None => {}
    }
    if let Some(rest) = deploy_spec
        .image
        .strip_prefix(&format!("{}/", registry().pull))
    {
        let repo = rest.split('@').next().unwrap_or(rest);
        let repo = repo
            .rsplit_once(':')
            .filter(|(_, t)| !t.contains('/'))
            .map_or(repo, |(r, _)| r);
        let ours = match repo.strip_prefix("pyo/services/") {
            Some(service) => {
                sqlx::query_scalar::<_, String>("SELECT team_id FROM pyo.services WHERE id = $1")
                    .bind(service)
                    .fetch_optional(&mut **tx)
                    .await?
            }
            None if repo.starts_with("pyo/") => None,
            None => {
                sqlx::query_scalar("SELECT team_id FROM pyo.registry_repos WHERE repo = $1")
                    .bind(repo)
                    .fetch_optional(&mut **tx)
                    .await?
            }
        };
        if ours.as_deref() != Some(team_id.as_str()) {
            return Err(ApiError::forbidden(
                "That image isn't one of your team's in Pyo registry.",
            ));
        }
    }
    if !deploy_spec.image.is_empty() {
        deploy_spec.pull_login_id = sqlx::query_scalar(
            "SELECT id FROM pyo.registry_logins WHERE team_id = $1 AND host = $2",
        )
        .bind(&team_id)
        .bind(crate::registry::image_host(&deploy_spec.image))
        .fetch_optional(&mut **tx)
        .await?
        .unwrap_or_default();
    }
    let mut source = source;
    if let Some(c) = req.commit.filter(|c| !c.is_empty()) {
        source["commit"] = json!(c);
    }
    // No image yet: a Git source that has to be built first.
    let needs_build = deploy_spec.image.is_empty();
    if needs_build {
        let ready: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM pyo.builders WHERE team_id = $1 AND status = 'ready')",
        )
        .bind(&team_id)
        .fetch_one(&mut **tx)
        .await?;
        if !ready {
            return Err(ApiError::conflict(
                "Git deploys need a builder. Add one in Builders, then deploy again.",
            ));
        }
    }
    let now = now_ms();
    let revision_label = if needs_build {
        match str_at(&source, &["commit"]) {
            "" => str_at(&source, &["branch"]).to_owned(),
            c => c.chars().take(7).collect(),
        }
    } else if source["type"] == json!("git") {
        // A rollback or retry of a build: same image, same commit.
        deploy_spec
            .image
            .rsplit('@')
            .next()
            .map(|d| d.trim_start_matches("sha256:").chars().take(7).collect())
            .unwrap_or_default()
    } else {
        match str_at(&source, &["reference"]) {
            "" => "latest".to_owned(),
            r => r.chars().take(19).collect(),
        }
    };
    let stages: &[&str] = if needs_build { &GIT_STAGES } else { &STAGES };
    let data = json!({
        "source": source,
        "revision": deploy_spec.image,
        "revisionLabel": revision_label,
        "trigger": req.trigger,
        "actor": req.actor,
        "stages": stages.iter().map(|s| if *s == "queued" { json!({ "stage": s, "startedAt": now }) } else { json!({ "stage": s }) }).collect::<Vec<_>>(),
        "stageStartedAt": now,
        "events": [],
        "rollbackOf": req.rollback_of,
        // What was live when this rollback started, for "Undo".
        "rollbackFrom": if req.rollback_of.is_some() { json!(live_id) } else { json!(null) },
        "retryOf": req.retry_of,
        "prevServiceStatus": prev_status,
        "config": {
            "replicas": deploy_spec.replicas,
            "resources": deploy_spec.resources,
            "envKeys": deploy_spec.env.iter().map(|e| e.key.clone()).collect::<Vec<_>>(),
        },
    });
    sqlx::query("INSERT INTO pyo.deployments (id, team_id, project_id, service_id, cluster_id, status, data, spec) VALUES ($1, $2, $3, $4, $5, 'queued', $6, $7)")
        .bind(req.deployment_id)
        .bind(&team_id)
        .bind(&project_id)
        .bind(req.service_id)
        .bind(&cluster_id)
        .bind(&data)
        .bind({
            let mut stored = serde_json::to_value(&deploy_spec).map_err(anyhow::Error::from)?;
            crate::vault::get().seal_env(&team_id, &mut stored).await?;
            stored
        })
        .execute(&mut **tx)
        .await?;
    sqlx::query("UPDATE pyo.services SET status = 'deploying', pending_changes = false, updated_at = now() WHERE id = $1")
        .bind(req.service_id)
        .execute(&mut **tx)
        .await?;
    let label = deploy_spec.service.name.clone();
    if needs_build {
        let job_id = new_id("bld");
        let job = build_job(&job_id, req.deployment_id, req.service_id, &source);
        sqlx::query(
            "INSERT INTO pyo.build_jobs (id, team_id, deployment_id, job) VALUES ($1, $2, $3, $4)",
        )
        .bind(&job_id)
        .bind(&team_id)
        .bind(req.deployment_id)
        .bind(serde_json::to_value(&job).map_err(anyhow::Error::from)?)
        .execute(&mut **tx)
        .await?;
    } else {
        send_or_hold(tx, &cluster_id, req.service_id, &prev_status, deploy_spec).await?;
    }
    log_activity(
        tx,
        &team_id,
        req.actor,
        "deployment.started",
        "info",
        &format!("Deploying {label}"),
        json!({ "kind": "deployment", "id": req.deployment_id, "label": req.deployment_id }),
        json!({ "projectId": project_id, "serviceId": req.service_id, "clusterId": cluster_id }),
    )
    .await?;
    Ok(cluster_id)
}

/// How long a rollout waits for its service's databases before it fails.
pub const DATABASE_WAIT_SECS: i64 = 15 * 60;

/// The service's connected databases that don't serve yet, by name. A
/// degraded one serves (some of its instances are still coming up).
async fn unready_databases(
    tx: &mut Transaction<'_, Postgres>,
    service_id: &str,
) -> ApiResult<Vec<String>> {
    Ok(sqlx::query_scalar(
        "SELECT m.name FROM pyo.database_links l JOIN pyo.databases m ON m.id = l.database_id
          WHERE l.service_id = $1 AND m.state = 'active' AND coalesce(m.status->>'phase', '') NOT IN ('Ready', 'Degraded')
          ORDER BY m.name",
    )
    .bind(service_id)
    .fetch_all(&mut **tx)
    .await?)
}

/// Send a rollout to the cluster now, or, while a database the service is
/// connected to isn't ready yet (one just made with it), hold it at
/// "waiting_database" until it is (`release_waiting`). A service whose
/// databases all serve goes straight on.
async fn send_or_hold(
    tx: &mut Transaction<'_, Postgres>,
    cluster_id: &str,
    service_id: &str,
    prev_status: &str,
    spec: DeploySpec,
) -> ApiResult<()> {
    let waiting = unready_databases(tx, service_id).await?;
    if waiting.is_empty() {
        let spec = with_fallback(tx, service_id, prev_status, spec).await?;
        enqueue(tx, cluster_id, Command::Deploy(Box::new(spec))).await?;
        return Ok(());
    }
    let deployment_id = spec.deployment_id;
    let mut data: Value = sqlx::query_scalar("SELECT data FROM pyo.deployments WHERE id = $1")
        .bind(&deployment_id)
        .fetch_one(&mut **tx)
        .await?;
    let now = now_ms();
    if let Some(stages) = data["stages"].as_array_mut() {
        for s in stages.iter_mut() {
            if s["startedAt"].is_number() && !s["finishedAt"].is_number() {
                s["finishedAt"] = json!(now);
            }
        }
        let at = stages
            .iter()
            .position(|s| s["stage"] == json!("applying"))
            .unwrap_or(stages.len());
        stages.insert(at, json!({ "stage": "waiting_database", "startedAt": now }));
    }
    data["stageStartedAt"] = json!(now);
    data["waitingSince"] = json!(now);
    sqlx::query("UPDATE pyo.deployments SET status = 'waiting_database', data = $2 WHERE id = $1")
        .bind(&deployment_id)
        .bind(&data)
        .execute(&mut **tx)
        .await?;
    for name in &waiting {
        sqlx::query("INSERT INTO pyo.deployment_logs (deployment_id, at, level, source, message) VALUES ($1, now(), 'info', 'pyo', $2)")
            .bind(&deployment_id)
            .bind(format!("Waiting for database {name}"))
            .execute(&mut **tx)
            .await?;
    }
    Ok(())
}

/// Rollouts held for their databases: sent once those are ready, failed
/// after `DATABASE_WAIT_SECS`.
async fn release_waiting(state: &AppState) -> ApiResult<()> {
    let held: Vec<String> =
        sqlx::query_scalar("SELECT id FROM pyo.deployments WHERE status = 'waiting_database'")
            .fetch_all(&state.db)
            .await?;
    for id in held {
        let mut tx = state.db.begin().await?;
        // Still held, and nobody asked to cancel it (the agent settles that).
        let row: Option<(String, String, Value, Value)> = sqlx::query_as(
            "SELECT cluster_id, service_id, spec, data FROM pyo.deployments d
              WHERE id = $1 AND status = 'waiting_database'
                AND NOT EXISTS (SELECT 1 FROM pyo.commands c WHERE c.command->>'kind' = 'cancel' AND c.command->>'deployment_id' = d.id)
              FOR UPDATE",
        )
        .bind(&id)
        .fetch_optional(&mut *tx)
        .await?;
        let Some((cluster_id, service_id, mut spec, mut data)) = row else {
            continue;
        };
        let waiting = unready_databases(&mut tx, &service_id).await?;
        if waiting.is_empty() {
            crate::vault::get().open_env(&mut spec).await?;
            let deploy: DeploySpec = serde_json::from_value(spec).map_err(anyhow::Error::from)?;
            // The rollout's own timeout starts now (see `sweep`).
            data["sentAt"] = json!(now_ms());
            sqlx::query("UPDATE pyo.deployments SET data = $2 WHERE id = $1")
                .bind(&id)
                .bind(&data)
                .execute(&mut *tx)
                .await?;
            send_or_hold(
                &mut tx,
                &cluster_id,
                &service_id,
                data["prevServiceStatus"].as_str().unwrap_or(""),
                deploy,
            )
            .await?;
            tx.commit().await?;
            state.wake(&cluster_id).await;
            continue;
        }
        let since = data["waitingSince"].as_i64().unwrap_or(0);
        tx.commit().await?;
        if now_ms() - since > DATABASE_WAIT_SECS * 1000 {
            let p = DeploymentProgress {
                status: "failed".into(),
                failure: Some(pyo_proto::Failure {
                    stage: "waiting_database".into(),
                    reason: "DatabaseNotReady".into(),
                    message: not_ready_message(&waiting),
                }),
                ..Default::default()
            };
            apply_progress(state, &cluster_id, &id, p).await?;
        }
    }
    Ok(())
}

fn not_ready_message(names: &[String]) -> String {
    let mins = DATABASE_WAIT_SECS / 60;
    match names {
        [one] => format!("Database {one} wasn't ready after {mins} minutes."),
        many => format!(
            "Databases {} weren't ready after {mins} minutes.",
            many.join(", ")
        ),
    }
}

/// Attach the live version to a rollout so the agent can put it back if the
/// rollout fails or is cancelled. Not for stopped services (that would start
/// them) or services that opted out.
async fn with_fallback(
    tx: &mut Transaction<'_, Postgres>,
    service_id: &str,
    service_status: &str,
    mut spec: DeploySpec,
) -> ApiResult<DeploySpec> {
    if service_status == "stopped" {
        return Ok(spec);
    }
    let live: Option<(Value, Value)> = sqlx::query_as(
        "SELECT coalesce(a.spec->'autoRollback', 'true'), d.spec FROM pyo.services a JOIN pyo.deployments d ON d.id = a.current_deployment_id
          WHERE a.id = $1 AND d.status = 'succeeded' AND d.id <> $2",
    )
    .bind(service_id)
    .bind(&spec.deployment_id)
    .fetch_optional(&mut **tx)
    .await?;
    let live = match live {
        Some((auto, mut prev)) => {
            crate::vault::get().open_env(&mut prev).await?;
            Some((auto, prev))
        }
        None => None,
    };
    if let Some((auto, prev)) = live
        && auto != json!(false)
        && let Ok(mut prev) = serde_json::from_value::<DeploySpec>(prev)
    {
        prev.fallback = None;
        // The volumes as they are now, as the rollout has them.
        prev.volumes = spec.volumes.clone();
        spec.fallback = Some(Box::new(prev));
    }
    Ok(spec)
}

pub async fn enqueue(
    tx: &mut Transaction<'_, Postgres>,
    cluster_id: &str,
    command: Command,
) -> ApiResult<String> {
    let id = new_id("cmd");
    // Queued commands keep env values sealed; the agent gets them opened.
    let mut stored = serde_json::to_value(&command).map_err(anyhow::Error::from)?;
    let team: String = sqlx::query_scalar("SELECT team_id FROM pyo.clusters WHERE id = $1")
        .bind(cluster_id)
        .fetch_one(&mut **tx)
        .await?;
    crate::vault::get().seal_env(&team, &mut stored).await?;
    crate::teambackup::seal_command(&team, &mut stored).await?;
    // clock_timestamp(), not now(): commands queued in one transaction keep
    // their order (the WAF's install, then the routes that use it).
    sqlx::query("INSERT INTO pyo.commands (id, cluster_id, command, created_at) VALUES ($1, $2, $3, clock_timestamp())")
        .bind(&id)
        .bind(cluster_id)
        .bind(stored)
        .execute(&mut **tx)
        .await?;
    Ok(id)
}

/// Record a step the agent reports. Final statuses settle the service.
pub async fn apply_progress(
    state: &AppState,
    cluster_id: &str,
    deployment_id: &str,
    p: DeploymentProgress,
) -> ApiResult<()> {
    let mut tx = state.db.begin().await?;
    let row: Option<(String, String, String, Value)> = sqlx::query_as("SELECT service_id, team_id, status, data FROM pyo.deployments WHERE id = $1 AND cluster_id = $2 FOR UPDATE")
        .bind(deployment_id)
        .bind(cluster_id)
        .fetch_optional(&mut *tx)
        .await?;
    let Some((service_id, team_id, status, mut data)) = row else {
        return Err(ApiError::not_found("Deployment"));
    };

    for l in &p.logs {
        sqlx::query("INSERT INTO pyo.deployment_logs (deployment_id, at, level, source, message) VALUES ($1, to_timestamp($2 / 1000.0), $3, $4, $5)")
            .bind(deployment_id)
            .bind(l.at as f64)
            .bind(&l.level)
            .bind(&l.source)
            .bind(l.message.chars().take(4000).collect::<String>())
            .execute(&mut *tx)
            .await?;
    }
    if !p.events.is_empty() {
        let events = data["events"]
            .as_array_mut()
            .map(std::mem::take)
            .unwrap_or_default();
        let mut all: Vec<Value> = events;
        all.extend(
            p.events
                .iter()
                .map(|e| serde_json::to_value(e).unwrap_or_default()),
        );
        let skip = all.len().saturating_sub(200);
        data["events"] = Value::Array(all.into_iter().skip(skip).collect());
    }
    // What the node actually ran; a tag gets pinned to it on success.
    if let Some(rev) = &p.revision {
        data["imageId"] = json!(rev);
    }
    if let Some(rb) = p.rolled_back {
        data["autoRolledBack"] = json!(rb);
    }

    // A late report for a rollout that already settled only adds logs.
    if is_final(&status) || p.status.is_empty() || p.status == status {
        sqlx::query("UPDATE pyo.deployments SET data = $2 WHERE id = $1")
            .bind(deployment_id)
            .bind(&data)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        return Ok(());
    }

    let now = now_ms();
    if let Some(stages) = data["stages"].as_array_mut() {
        // Rolling back isn't planned up front; it's added when it happens.
        if !is_final(&p.status) && !stages.iter().any(|s| s["stage"] == json!(p.status)) {
            stages.push(json!({ "stage": p.status }));
        }
        for s in stages.iter_mut() {
            if s["startedAt"].is_number()
                && !s["finishedAt"].is_number()
                && s["stage"] != json!(p.status)
            {
                s["finishedAt"] = json!(now);
            }
            if s["stage"] == json!(p.status) && !s["startedAt"].is_number() {
                s["startedAt"] = json!(now);
            }
        }
    }
    data["stageStartedAt"] = json!(now);
    if let Some(f) = &p.failure {
        data["failure"] = serde_json::to_value(f).unwrap_or_default();
    }

    let finished = is_final(&p.status);
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "UPDATE pyo.deployments SET status = $2, data = $3{} WHERE id = $1",
        if finished {
            ", finished_at = now()"
        } else {
            ""
        }
    )))
    .bind(deployment_id)
    .bind(&p.status)
    .bind(&data)
    .execute(&mut *tx)
    .await?;

    if finished {
        // Follow-ups as jobs in this transaction: they happen even if this
        // process stops right after the commit.
        for (repo, digest) in settle_service(
            &mut tx,
            &service_id,
            &team_id,
            deployment_id,
            &p.status,
            &data,
        )
        .await?
        {
            crate::jobs::enqueue(
                &mut *tx,
                "registry.delete",
                json!({ "repo": repo, "digest": digest }),
            )
            .await?;
        }
        if data["source"]["installationId"].is_i64() {
            let msg = data["failure"]["message"].as_str().unwrap_or("");
            let (gh, text) = match p.status.as_str() {
                "succeeded" => ("success", "Deployed"),
                "cancelled" => ("error", "Cancelled"),
                _ => (
                    "failure",
                    if msg.is_empty() {
                        "The deploy failed"
                    } else {
                        msg
                    },
                ),
            };
            crate::jobs::enqueue(
                &mut *tx,
                "github.status",
                crate::jobs::github_status(deployment_id, gh, text),
            )
            .await?;
        }
    }
    tx.commit().await?;
    Ok(())
}

/// After a rollout ends: which deployment is live and what the service's status
/// is. Returns registry images that are no longer kept, to delete.
async fn settle_service(
    tx: &mut Transaction<'_, Postgres>,
    service_id: &str,
    team_id: &str,
    deployment_id: &str,
    outcome: &str,
    data: &Value,
) -> ApiResult<Vec<(String, String)>> {
    let (name, project_id, cluster_id, current): (String, String, String, Option<String>) =
        sqlx::query_as("SELECT name, project_id, cluster_id, current_deployment_id FROM pyo.services WHERE id = $1")
            .bind(service_id)
            .fetch_one(&mut **tx)
            .await?;
    let ctx = json!({ "projectId": project_id, "serviceId": service_id, "clusterId": cluster_id });
    let target = json!({ "kind": "deployment", "id": deployment_id, "label": deployment_id });
    match outcome {
        "succeeded" => {
            sqlx::query("UPDATE pyo.services SET status = 'running', current_deployment_id = $2, updated_at = now() WHERE id = $1")
                .bind(service_id)
                .bind(deployment_id)
                .execute(&mut **tx)
                .await?;
            // Its domains route now (TLS status from the agent's reports).
            crate::routes::mark_routed(tx, service_id, None).await?;
            pin_image(tx, deployment_id, data).await?;
            let rollback = data["rollbackOf"].is_string();
            let kind = if rollback {
                "deployment.rollback"
            } else {
                "deployment.succeeded"
            };
            let title = if rollback {
                format!("Rolled back {name}")
            } else {
                format!("Deployed {name}")
            };
            log_activity(tx, team_id, "pyo", kind, "success", &title, target, ctx).await?;
            return prune_images(tx, service_id, team_id).await;
        }
        "failed" => {
            let rolled_back = data["autoRolledBack"] == json!(true);
            // Rolled back: the previous version serves again. Otherwise a
            // failed rolling update still keeps the previous pods serving.
            let status = if current.is_none() {
                "failed"
            } else if rolled_back {
                "running"
            } else {
                "degraded"
            };
            sqlx::query("UPDATE pyo.services SET status = $2, updated_at = now() WHERE id = $1")
                .bind(service_id)
                .bind(status)
                .execute(&mut **tx)
                .await?;
            let reason = data["failure"]["message"]
                .as_str()
                .unwrap_or("Rollout failed");
            let title = if rolled_back {
                format!("{name} deploy failed and was rolled back: {reason}")
            } else {
                format!("{name} deploy failed: {reason}")
            };
            log_activity(
                tx,
                team_id,
                "pyo",
                "deployment.failed",
                "failure",
                &title,
                target,
                ctx,
            )
            .await?;
        }
        _ => {
            let prev = data["prevServiceStatus"].as_str().unwrap_or("stopped");
            let status = if data["autoRolledBack"] == json!(true) {
                "running"
            } else if prev == "deploying" {
                if current.is_some() {
                    "running"
                } else {
                    "stopped"
                }
            } else {
                prev
            };
            sqlx::query("UPDATE pyo.services SET status = $2, updated_at = now() WHERE id = $1")
                .bind(service_id)
                .bind(status)
                .execute(&mut **tx)
                .await?;
            log_activity(
                tx,
                team_id,
                "pyo",
                "deployment.cancelled",
                "warning",
                &format!("Cancelled deploy of {name}"),
                target,
                ctx,
            )
            .await?;
        }
    }
    Ok(vec![])
}

/// "nginx:1.27-alpine" + "docker.io/library/nginx@sha256:ab…" ->
/// "nginx@sha256:ab…": the same name, pinned to what actually ran, so a
/// rollback gets exactly these bytes even if the tag moves later.
fn pinned(image: &str, image_id: &str) -> Option<String> {
    if image.contains('@') {
        return None;
    }
    let digest = image_id.rsplit_once('@')?.1;
    if !digest.starts_with("sha256:") || digest.len() != 71 {
        return None;
    }
    let (dir, last) = image
        .rsplit_once('/')
        .map(|(d, l)| (Some(d), l))
        .unwrap_or((None, image));
    let repo = last.split(':').next().unwrap_or(last);
    Some(match dir {
        Some(d) => format!("{d}/{repo}@{digest}"),
        None => format!("{repo}@{digest}"),
    })
}

/// Record the pinned image next to the spec. The spec stays as applied, so
/// putting this version back after a failed rollout matches its running
/// pods exactly; a rollback started later uses the pinned image.
async fn pin_image(
    tx: &mut Transaction<'_, Postgres>,
    deployment_id: &str,
    data: &Value,
) -> ApiResult<()> {
    let image: String = sqlx::query_scalar(
        "SELECT coalesce(spec->>'image', '') FROM pyo.deployments WHERE id = $1",
    )
    .bind(deployment_id)
    .fetch_one(&mut **tx)
    .await?;
    if let Some(p) = pinned(&image, data["imageId"].as_str().unwrap_or("")) {
        sqlx::query("UPDATE pyo.deployments SET data = jsonb_set(data, '{pinnedImage}', to_jsonb($2::text)) WHERE id = $1").bind(deployment_id).bind(&p).execute(&mut **tx).await?;
    }
    Ok(())
}

/// Keep the images of the team's `retainDeployments` newest successful
/// deployments of a service (and the live one); older images Pyo built are
/// marked expired and returned as (repository, digest) to delete, unless
/// something else still runs or keeps the same image.
async fn prune_images(
    tx: &mut Transaction<'_, Postgres>,
    service_id: &str,
    team_id: &str,
) -> ApiResult<Vec<(String, String)>> {
    let keep: i64 = sqlx::query_scalar("SELECT coalesce((preferences->>'retainDeployments')::bigint, 20) FROM pyo.teams WHERE id = $1").bind(team_id).fetch_one(&mut **tx).await?;
    let rows: Vec<(String, String, bool)> = sqlx::query_as(
        "SELECT d.id, coalesce(d.spec->>'image', ''), coalesce(d.id = a.current_deployment_id, false) OR coalesce((d.data->>'imageExpired')::bool, false)
           FROM pyo.deployments d JOIN pyo.services a ON a.id = d.service_id
          WHERE d.service_id = $1 AND d.status = 'succeeded' ORDER BY d.created_at DESC, d.id DESC",
    )
    .bind(service_id)
    .fetch_all(&mut **tx)
    .await?;
    let keep = keep.max(1) as usize;
    let pull = &registry().pull;
    // The same image elsewhere: another service's live or kept deployment,
    // or any rollout still going.
    let candidates: Vec<String> = rows
        .iter()
        .skip(keep)
        .filter(|r| !r.2)
        .map(|r| r.1.clone())
        .collect();
    let elsewhere: Vec<String> = if candidates.is_empty() {
        vec![]
    } else {
        sqlx::query_scalar(
            "SELECT DISTINCT d.spec->>'image' FROM pyo.deployments d
              WHERE d.spec->>'image' = ANY($2)
                AND (d.status NOT IN ('succeeded', 'failed', 'cancelled')
                     OR (d.service_id <> $1 AND d.status = 'succeeded' AND NOT coalesce((d.data->>'imageExpired')::bool, false)))",
        )
        .bind(service_id)
        .bind(&candidates)
        .fetch_all(&mut **tx)
        .await?
    };
    let elsewhere = elsewhere
        .iter()
        .filter_map(|i| built_image(pull, i))
        .collect();
    let (expired, delete) = retention(&rows, keep, pull, &elsewhere);
    for id in expired {
        sqlx::query(
            "UPDATE pyo.deployments SET data = data || '{\"imageExpired\": true}' WHERE id = $1",
        )
        .bind(id)
        .execute(&mut **tx)
        .await?;
    }
    Ok(delete)
}

/// An image Pyo built, as (repository, digest): "<pull>/pyo/services/<id>@sha256:…".
fn built_image(pull: &str, image: &str) -> Option<(String, String)> {
    let (repo, digest) = image
        .strip_prefix(pull)?
        .strip_prefix('/')?
        .split_once('@')?;
    (repo.starts_with("pyo/services/") && digest.starts_with("sha256:"))
        .then(|| (repo.to_owned(), digest.to_owned()))
}

/// Which of a service's succeeded deployments (id, image, live or already
/// expired; newest first) lose their image, and the images to delete. The
/// newest `keep`, the live one and those already expired keep theirs; an
/// older one whose image is still in use (by one of those, or `elsewhere`)
/// keeps it too, so it can still be rolled back to. Registry deletes are by
/// repository and digest: one image deployed twice is deleted once.
fn retention<'a>(
    rows: &'a [(String, String, bool)],
    keep: usize,
    pull: &str,
    elsewhere: &std::collections::HashSet<(String, String)>,
) -> (Vec<&'a str>, Vec<(String, String)>) {
    let (kept, older) = rows.split_at(keep.min(rows.len()));
    let mut in_use: std::collections::HashSet<(String, String)> = elsewhere.clone();
    in_use.extend(
        kept.iter()
            .chain(older.iter().filter(|r| r.2))
            .filter_map(|r| built_image(pull, &r.1)),
    );
    let mut expired = vec![];
    let mut delete: Vec<(String, String)> = vec![];
    for (id, image, skip) in older {
        let Some(key) = (!*skip).then(|| built_image(pull, image)).flatten() else {
            continue;
        };
        if in_use.contains(&key) {
            continue;
        }
        expired.push(id.as_str());
        if !delete.contains(&key) {
            delete.push(key);
        }
    }
    (expired, delete)
}

/// An image in Pyo registry, named by any of its addresses (the door's on
/// the tailnet or in the cluster, the registry's own, or this machine's
/// loopback on the pull port), as servers pull it: Some(Ok(name)). An image
/// on another port of this machine's own addresses: Some(Err(why)). None
/// for any other registry.
pub fn pyo_registry_image(image: &str) -> Option<Result<String, &'static str>> {
    let r = registry();
    if let Some(p) = crate::registrydoor::as_pulled(image) {
        return Some(Ok(p));
    }
    let (host, rest) = image.split_once('/')?;
    // Docker Hub's short names ("library/nginx") have no host.
    if !(host.contains('.') || host.contains(':') || host.eq_ignore_ascii_case("localhost")) {
        return None;
    }
    let host = host.to_ascii_lowercase();
    let pulled = || Some(Ok(format!("{}/{rest}", r.pull)));
    if host == r.pull
        || host == r.internal
        || host == DOOR_IN_CLUSTER
        || (!r.push.is_empty() && host == r.push.to_ascii_lowercase())
    {
        return pulled();
    }
    let (name, port) = split_port(&host);
    // The registry and its door by any of their in-cluster names.
    if name.starts_with("pyo-registry")
        && (name.ends_with(".pyo-system") || name.contains(".pyo-system.svc"))
    {
        return pulled();
    }
    if loopback(name) {
        let pull_port = split_port(&r.pull).1;
        return if port == pull_port {
            pulled()
        } else {
            Some(Err(
                "Pyo doesn't pull images from its own machine's addresses.",
            ))
        };
    }
    None
}

/// "host:port" (or "[v6]:port") → (host, port).
fn split_port(host: &str) -> (&str, Option<&str>) {
    if let Some(v6) = host.strip_prefix('[') {
        let (h, rest) = v6.split_once(']').unwrap_or((v6, ""));
        return (h, rest.strip_prefix(':'));
    }
    match host.rsplit_once(':') {
        Some((h, p)) if !h.contains(':') => (h, Some(p)),
        _ => (host, None),
    }
}

/// This machine, by any name: localhost, the loopback ranges, "any".
fn loopback(host: &str) -> bool {
    if host == "localhost" || host.ends_with(".localhost") {
        return true;
    }
    match host.parse::<std::net::IpAddr>() {
        Ok(std::net::IpAddr::V4(v4)) => v4.is_loopback() || v4.is_unspecified(),
        Ok(std::net::IpAddr::V6(v6)) => {
            v6.is_loopback()
                || v6.is_unspecified()
                || v6
                    .to_ipv4_mapped()
                    .is_some_and(|v4| v4.is_loopback() || v4.is_unspecified())
        }
        Err(_) => false,
    }
}

/// Remove a manifest from Pyo's registry (a job). Its layers are freed by
/// the registry's garbage collection.
pub async fn delete_image(repo: &str, digest: &str) -> anyhow::Result<()> {
    let url = format!(
        "http://{}/v2/{repo}/manifests/{digest}",
        registry().internal
    );
    let r = reqwest::Client::new()
        .delete(&url)
        .timeout(Duration::from_secs(20))
        .send()
        .await?;
    if r.status().is_success() || r.status() == reqwest::StatusCode::NOT_FOUND {
        tracing::info!(%repo, %digest, "deleted old image");
        Ok(())
    } else {
        anyhow::bail!(
            "the registry answered {} deleting {repo}@{digest}",
            r.status()
        )
    }
}

/// Marks clusters whose agent went quiet as unreachable, and fails rollouts
/// nobody is reporting on any more.
pub async fn sweeper(state: AppState) {
    let mut tick = tokio::time::interval(Duration::from_secs(15));
    loop {
        tick.tick().await;
        let Some(r) = crate::bg::as_leader(&state.db, crate::bg::LOCK_SWEEPER, || async {
            let _ = crate::jobs::prune(&state).await;
            let _ = crate::certs::warn_expiring(&state).await;
            if let Err(e) = release_waiting(&state).await {
                tracing::warn!(error = ?e.message, "releasing rollouts held for databases failed");
            }
            sweep(&state).await
        })
        .await
        else {
            continue;
        };
        if let Err(e) = r {
            tracing::warn!(error = ?e.message, "sweep failed");
        }
    }
}

async fn sweep(state: &AppState) -> ApiResult<()> {
    // A drained server whose machine was uninstalled (it went offline) is
    // deleted from its cluster, finishing its removal.
    let gone: Vec<(String, String)> = sqlx::query_as(
        "UPDATE pyo.servers SET removal = removal || '{\"state\": \"removing\"}'
          WHERE removal->>'state' = 'drained' AND report->>'status' <> 'ready'
          RETURNING cluster_id, name",
    )
    .fetch_all(&state.db)
    .await?;
    for (cluster_id, node) in gone {
        let mut tx = state.db.begin().await?;
        enqueue(
            &mut tx,
            &cluster_id,
            Command::RemoveServer {
                server: node,
                force: true,
            },
        )
        .await?;
        tx.commit().await?;
        state.wake(&cluster_id).await;
    }
    sqlx::query("UPDATE pyo.clusters SET status = 'unreachable' WHERE status IN ('connected', 'degraded') AND last_seen_at < now() - interval '60 seconds'")
        .execute(&state.db)
        .await?;
    // Clusters Add cluster made without a name that never connected: the
    // page opens with one ready, so a visit that goes nowhere leaves it.
    // Gone after a day (a named one stays until it's removed).
    sqlx::query(
        "WITH gone AS (
           DELETE FROM pyo.clusters c
            WHERE c.status = 'pending' AND c.name_from_host AND c.created_at < now() - interval '1 day'
              AND NOT EXISTS (SELECT 1 FROM pyo.services a WHERE a.cluster_id = c.id)
              AND NOT EXISTS (SELECT 1 FROM pyo.builders b WHERE b.cluster_id = c.id)
           RETURNING c.id)
         UPDATE pyo.teams SET default_cluster_id = NULL WHERE default_cluster_id IN (SELECT id FROM gone)",
    )
    .execute(&state.db)
    .await?;
    let stale: Vec<(String, String)> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT id, cluster_id FROM pyo.deployments
          WHERE status NOT IN ('succeeded', 'failed', 'cancelled', 'waiting_database')
            AND coalesce(to_timestamp((data->>'sentAt')::bigint / 1000.0), created_at) < now() - interval '{} seconds'",
        ROLLOUT_TIMEOUT_SECS + 120
    )))
    .fetch_all(&state.db)
    .await?;
    for (id, cluster_id) in stale {
        let p = DeploymentProgress {
            status: "failed".into(),
            failure: Some(pyo_proto::Failure {
                stage: "waiting_ready".into(),
                reason: "AgentTimeout".into(),
                message: "The cluster's agent stopped reporting on this rollout.".into(),
            }),
            ..Default::default()
        };
        apply_progress(state, &cluster_id, &id, p).await?;
    }
    Ok(())
}

/// A builder finished a job: on success the image (pinned by digest) goes to
/// the cluster's agent; on failure the deployment fails with the reason.
pub async fn finish_build(
    state: &AppState,
    job_id: &str,
    builder_id: &str,
    r: BuildResult,
) -> ApiResult<Option<String>> {
    let mut tx = state.db.begin().await?;
    let row: Option<(String, String)> = sqlx::query_as(
        "UPDATE pyo.build_jobs SET status = $3, result = $4, finished_at = now()
          WHERE id = $1 AND builder_id = $2 AND status = 'running' RETURNING deployment_id, team_id",
    )
    .bind(job_id)
    .bind(builder_id)
    .bind(if r.ok { "succeeded" } else { "failed" })
    .bind(serde_json::to_value(&r).map_err(anyhow::Error::from)?)
    .fetch_optional(&mut *tx)
    .await?;
    // Cancelled meanwhile (or not this builder's): nothing to do.
    let Some((deployment_id, _team)) = row else {
        tx.commit().await?;
        return Ok(None);
    };
    // The service's id, not the label in the spec: the builder pushed to
    // pyo/services/<id> (build_job).
    let (cluster_id, service_id, status, mut spec, mut data): (String, String, String, Value, Value) =
        sqlx::query_as("SELECT cluster_id, service_id, status, spec, data FROM pyo.deployments WHERE id = $1 FOR UPDATE").bind(&deployment_id).fetch_one(&mut *tx).await?;
    if is_final(&status) {
        tx.commit().await?;
        return Ok(None);
    }
    match (&r.digest, r.ok) {
        (Some(digest), true) => {
            let image = format!("{}/pyo/services/{service_id}@{digest}", registry().pull);
            // First deploy of a service whose single port is still the wizard's
            // guess: use the port the Dockerfile EXPOSEs instead.
            if let Some(&port) = r.exposed_ports.first() {
                let (current, service_spec): (Option<String>, Value) = sqlx::query_as(
                    "SELECT current_deployment_id, spec FROM pyo.services WHERE id = $1",
                )
                .bind(&service_id)
                .fetch_one(&mut *tx)
                .await?;
                let ports = service_spec["ports"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default();
                let guessed = ports.len() == 1
                    && ports[0]["containerPort"] == json!(8080)
                    && !r.exposed_ports.contains(&8080);
                if current.is_none() && guessed {
                    sqlx::query(
                        "UPDATE pyo.services SET spec = jsonb_set(jsonb_set(spec, '{ports,0,containerPort}', to_jsonb($2::int)), '{healthCheck,port}', to_jsonb($2::int)) WHERE id = $1",
                    )
                    .bind(&service_id)
                    .bind(port as i32)
                    .execute(&mut *tx)
                    .await?;
                    spec["ports"][0]["containerPort"] = json!(port);
                    spec["healthCheck"]["port"] = json!(port);
                    sqlx::query("INSERT INTO pyo.deployment_logs (deployment_id, at, level, source, message) VALUES ($1, now(), 'info', 'pyo', $2)")
                        .bind(&deployment_id)
                        .bind(format!("Using port {port} from the Dockerfile's EXPOSE (the service was set to 8080)."))
                        .execute(&mut *tx)
                        .await?;
                }
            }
            spec["image"] = json!(image);
            data["revision"] = json!(r.commit);
            data["revisionLabel"] = json!(r.commit.chars().take(7).collect::<String>());
            data["source"]["commit"] = json!(r.commit);
            // "Dockerfile" or "Containerfile", or the framework Railpack detected.
            if let Some(how) = &r.detected {
                data["source"]["builder"] =
                    json!(if matches!(how.as_str(), "Dockerfile" | "Containerfile") {
                        "dockerfile"
                    } else {
                        "railpack"
                    });
                data["source"]["buildpackLanguage"] = json!(how);
            }
            sqlx::query("UPDATE pyo.deployments SET spec = $2, data = $3 WHERE id = $1")
                .bind(&deployment_id)
                .bind(&spec)
                .bind(&data)
                .execute(&mut *tx)
                .await?;
            crate::vault::get().open_env(&mut spec).await?;
            let deploy: DeploySpec = serde_json::from_value(spec).map_err(anyhow::Error::from)?;
            send_or_hold(
                &mut tx,
                &cluster_id,
                &service_id,
                data["prevServiceStatus"].as_str().unwrap_or(""),
                deploy,
            )
            .await?;
            tx.commit().await?;
            Ok(Some(cluster_id))
        }
        _ => {
            tx.commit().await?;
            let message = r.message.unwrap_or_else(|| "The build failed.".into());
            let p = DeploymentProgress {
                status: "failed".into(),
                failure: Some(pyo_proto::Failure {
                    stage: "building".into(),
                    reason: "BuildFailed".into(),
                    message: message.clone(),
                }),
                logs: vec![pyo_proto::LogLine {
                    at: now_ms(),
                    level: "error".into(),
                    source: "builder".into(),
                    message,
                }],
                ..Default::default()
            };
            apply_progress(state, &cluster_id, &deployment_id, p).await?;
            Ok(None)
        }
    }
}

#[cfg(test)]
mod retention_tests {
    use super::{built_image, retention};
    use std::collections::HashSet;

    const PULL: &str = "127.0.0.1:5005";

    fn img(service: &str, d: char) -> String {
        format!(
            "{PULL}/pyo/services/{service}@sha256:{}",
            d.to_string().repeat(64)
        )
    }
    fn key(service: &str, d: char) -> (String, String) {
        built_image(PULL, &img(service, d)).unwrap()
    }
    fn row(id: &str, image: String, skip: bool) -> (String, String, bool) {
        (id.into(), image, skip)
    }

    #[test]
    fn reads_built_images_only() {
        assert_eq!(
            key("svc_x", 'a'),
            (
                "pyo/services/svc_x".into(),
                format!("sha256:{}", "a".repeat(64))
            )
        );
        assert_eq!(built_image(PULL, "docker.io/library/nginx:1.27"), None);
        assert_eq!(built_image(PULL, &format!("{PULL}/acme/shop/web:v1")), None);
        assert_eq!(
            built_image(PULL, &format!("{PULL}/pyo/services/svc_x:abc")),
            None
        );
        assert_eq!(
            built_image(
                PULL,
                &format!("{PULL}2/pyo/services/svc_x@sha256:{}", "a".repeat(64))
            ),
            None
        );
    }

    #[test]
    fn keeps_the_newest_and_deletes_older() {
        let rows = vec![
            row("d4", img("svc_x", '4'), false),
            row("d3", img("svc_x", '3'), false),
            row("d2", img("svc_x", '2'), false),
            row("d1", img("svc_x", '1'), false),
        ];
        let (expired, delete) = retention(&rows, 2, PULL, &HashSet::new());
        assert_eq!(expired, vec!["d2", "d1"]);
        assert_eq!(delete, vec![key("svc_x", '2'), key("svc_x", '1')]);
    }

    #[test]
    fn never_deletes_the_live_image() {
        // Rolled back to d1: it's live though it's the oldest.
        let rows = vec![
            row("d3", img("svc_x", '3'), false),
            row("d2", img("svc_x", '2'), false),
            row("d1", img("svc_x", '1'), true),
        ];
        let (expired, delete) = retention(&rows, 1, PULL, &HashSet::new());
        assert_eq!(expired, vec!["d2"]);
        assert_eq!(delete, vec![key("svc_x", '2')]);
    }

    #[test]
    fn keeps_a_digest_a_kept_deployment_shares() {
        // A redeploy and a rollback reuse an older deployment's image.
        let rows = vec![
            row("d3", img("svc_x", 'a'), false),
            row("d2", img("svc_x", 'b'), false),
            row("d1", img("svc_x", 'a'), false),
        ];
        let (expired, delete) = retention(&rows, 1, PULL, &HashSet::new());
        assert_eq!(
            expired,
            vec!["d2"],
            "d1 runs what d3 runs: it can still be rolled back to"
        );
        assert_eq!(delete, vec![key("svc_x", 'b')]);
    }

    #[test]
    fn keeps_what_the_live_one_shares() {
        let rows = vec![
            row("d3", img("svc_x", 'c'), false),
            row("d2", img("svc_x", 'a'), true),
            row("d1", img("svc_x", 'a'), false),
        ];
        let (expired, delete) = retention(&rows, 1, PULL, &HashSet::new());
        assert!(expired.is_empty() && delete.is_empty());
    }

    #[test]
    fn keeps_what_another_service_uses() {
        let rows = vec![
            row("d2", img("svc_x", '2'), false),
            row("d1", img("svc_x", '1'), false),
        ];
        let elsewhere: HashSet<_> = [key("svc_x", '1')].into();
        let (expired, delete) = retention(&rows, 1, PULL, &elsewhere);
        assert!(expired.is_empty() && delete.is_empty());
    }

    #[test]
    fn deletes_a_shared_digest_once() {
        // The same bytes deployed three times.
        let d = 'e';
        let rows = vec![
            row("d4", img("svc_x", 'f'), false),
            row("d3", img("svc_x", d), false),
            row("d2", img("svc_x", d), false),
            row("d1", img("svc_x", d), false),
        ];
        let (expired, delete) = retention(&rows, 1, PULL, &HashSet::new());
        assert_eq!(expired, vec!["d3", "d2", "d1"]);
        assert_eq!(delete, vec![key("svc_x", d)]);
    }

    #[test]
    fn leaves_images_pyo_did_not_build_and_expired_ones() {
        let rows = vec![
            row("d4", img("svc_x", '4'), false),
            row("d3", "docker.io/library/nginx:1.27".into(), false),
            row(
                "d2",
                format!("{PULL}/acme/shop/web@sha256:{}", "2".repeat(64)),
                false,
            ),
            row("d1", img("svc_x", '1'), true),
        ];
        let (expired, delete) = retention(&rows, 1, PULL, &HashSet::new());
        assert!(expired.is_empty() && delete.is_empty());
    }

    #[test]
    fn keeps_at_least_everything_when_there_are_few() {
        let rows = vec![row("d1", img("svc_x", '1'), false)];
        assert_eq!(
            retention(&rows, 20, PULL, &HashSet::new()),
            (vec![], vec![])
        );
    }
}

#[cfg(test)]
mod registry_name_tests {
    use super::pyo_registry_image;

    #[test]
    fn every_name_of_pyo_registry_is_its_pull_name() {
        let p = |i: &str| pyo_registry_image(i);
        let ok = |rest: &str| Some(Ok(format!("127.0.0.1:5005/{rest}")));
        assert_eq!(p("127.0.0.1:5005/acme/shop/web:1"), ok("acme/shop/web:1"));
        assert_eq!(
            p("localhost:5005/acme/shop/web:1"),
            ok("acme/shop/web:1"),
            "localhost is the same registry"
        );
        assert_eq!(
            p("LOCALHOST:5005/pyo/services/svc_x@sha256:aa"),
            ok("pyo/services/svc_x@sha256:aa")
        );
        assert_eq!(
            p("127.0.0.2:5005/a/b/c"),
            ok("a/b/c"),
            "the whole loopback range"
        );
        assert_eq!(p("[::1]:5005/a/b/c"), ok("a/b/c"));
        assert_eq!(p("0.0.0.0:5005/a/b/c"), ok("a/b/c"));
        assert_eq!(p("[::ffff:127.0.0.1]:5005/a/b/c"), ok("a/b/c"));
        assert_eq!(
            p("pyo-registry.pyo-system.svc.cluster.local:5000/a/b/c"),
            ok("a/b/c")
        );
        assert_eq!(
            p("pyo-registry-door.pyo-system.svc.cluster.local:5006/a/b/c"),
            ok("a/b/c")
        );
        assert_eq!(p("pyo-registry.pyo-system:5000/a/b/c"), ok("a/b/c"));
        assert!(
            matches!(p("localhost:8000/x"), Some(Err(_))),
            "another port on this machine"
        );
        assert!(matches!(p("127.0.0.1:2375/x"), Some(Err(_))));
        assert_eq!(p("docker.io/library/nginx:1"), None);
        assert_eq!(p("ghcr.io/acme/web:1"), None);
        assert_eq!(p("library/nginx"), None);
        assert_eq!(p("nginx:1.27"), None);
        assert_eq!(p("registry.example.com:5005/x"), None);
    }
}

#[cfg(test)]
mod pin_tests {
    use super::pinned;

    #[test]
    fn pins_tags_to_what_ran() {
        let d = format!("sha256:{}", "a".repeat(64));
        assert_eq!(
            pinned("nginx:1.27-alpine", &format!("docker.io/library/nginx@{d}")),
            Some(format!("nginx@{d}"))
        );
        assert_eq!(
            pinned("ghcr.io/acme/web", &format!("ghcr.io/acme/web@{d}")),
            Some(format!("ghcr.io/acme/web@{d}"))
        );
        assert_eq!(
            pinned("localhost:5000/x:v2", &format!("localhost:5000/x@{d}")),
            Some(format!("localhost:5000/x@{d}"))
        );
        assert_eq!(
            pinned(
                &format!("nginx@{d}"),
                &format!("docker.io/library/nginx@{d}")
            ),
            None
        );
        assert_eq!(pinned("nginx:1", "sha256:short"), None);
    }
}

/// The addresses a hostname resolves to; empty when it doesn't (yet).
pub async fn resolve(host: &str) -> Vec<String> {
    tokio::time::timeout(
        Duration::from_secs(5),
        tokio::net::lookup_host(format!("{host}:80")),
    )
    .await
    .ok()
    .and_then(|r| r.ok())
    .map(|a| a.map(|x| x.ip().to_string()).collect())
    .unwrap_or_default()
}

/// Record a domain's DNS check: active once it resolves, else still waiting.
pub async fn set_domain_dns(
    db: &sqlx::PgPool,
    service_id: &str,
    domain_id: &str,
    resolves: bool,
    host: &str,
) -> ApiResult<()> {
    let (status, error) = if resolves {
        ("active", None)
    } else {
        (
            "pending_dns",
            Some(format!(
                "{host} doesn't resolve yet. Point it at the cluster's ingress."
            )),
        )
    };
    sqlx::query(
        "UPDATE pyo.services SET spec = jsonb_set(spec, '{domains}', (
           SELECT coalesce(jsonb_agg(CASE WHEN d->>'id' = $2
                    THEN (d - 'error') || jsonb_strip_nulls(jsonb_build_object('status', $3::text, 'error', $4::text, 'dnsCheckedAt', pyo.ms(now())))
                    ELSE d END), '[]')
             FROM jsonb_array_elements(spec->'domains') d)) WHERE id = $1",
    )
    .bind(service_id)
    .bind(domain_id)
    .bind(status)
    .bind(&error)
    .execute(db)
    .await?;
    Ok(())
}

/// Checks the DNS of domains still waiting for it, so nobody has to press a
/// button: every 30 seconds for an hour after a domain is added, then every
/// 5 minutes for a day, then every 30 minutes.
pub async fn dns_watcher(state: AppState) {
    let mut tick = tokio::time::interval(Duration::from_secs(15));
    loop {
        tick.tick().await;
        if let Some(Err(e)) =
            crate::bg::as_leader(&state.db, crate::bg::LOCK_DNS, || check_pending_dns(&state)).await
        {
            tracing::warn!(error = ?e.message, "DNS check failed");
        }
    }
}

async fn check_pending_dns(state: &AppState) -> ApiResult<()> {
    let due: Vec<(String, String, String, String)> = sqlx::query_as(
        "SELECT a.id, a.team_id, d->>'id', d->>'hostname'
           FROM pyo.services a, jsonb_array_elements(a.spec->'domains') d
          WHERE NOT a.deleting AND d->>'status' IN ('pending_dns', 'verifying')
            AND pyo.ms(now()) - coalesce((d->>'dnsCheckedAt')::bigint, 0) >=
                CASE WHEN pyo.ms(now()) - coalesce((d->>'createdAt')::bigint, 0) < 3600000 THEN 30000
                     WHEN pyo.ms(now()) - coalesce((d->>'createdAt')::bigint, 0) < 86400000 THEN 300000
                     ELSE 1800000 END
          LIMIT 200",
    )
    .fetch_all(&state.db)
    .await?;
    for (service_id, team_id, domain_id, host) in due {
        let resolves = !resolve(&host).await.is_empty();
        set_domain_dns(&state.db, &service_id, &domain_id, resolves, &host).await?;
        if resolves {
            let mut tx = state.db.begin().await?;
            crate::ops::log_activity(
                &mut tx,
                &team_id,
                "Pyo",
                "domain.verified",
                "success",
                &format!("{host} points to the cluster now"),
                json!({ "kind": "service", "id": service_id, "label": host }),
                json!({ "serviceId": service_id }),
            )
            .await?;
            tx.commit().await?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tls_tests {
    use super::tls_issuer;

    #[test]
    fn picks_issuers() {
        assert_eq!(tls_issuer("auto", "shop.example.com"), "letsencrypt");
        assert_eq!(tls_issuer("", "app.203-0-113-10.nip.io"), "letsencrypt");
        assert_eq!(tls_issuer("auto", "app.100-95-104-0.nip.io"), "selfsigned");
        assert_eq!(tls_issuer("auto", "app.192-168-1-48.nip.io"), "selfsigned");
        assert_eq!(tls_issuer("auto", "hello.home.localhost"), "selfsigned");
        assert_eq!(tls_issuer("off", "shop.example.com"), "");
        assert_eq!(tls_issuer("selfsigned", "shop.example.com"), "selfsigned");
        assert_eq!(tls_issuer("custom", "hello.home.localhost"), "custom");
    }
}
