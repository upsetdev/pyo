//! A deploy: server-side apply of the service's objects, then watch its pods
//! until the rollout is ready, fails, times out or is cancelled. Each stage,
//! new Kubernetes event and failure is reported as it happens.
//!
//! When a rollout fails or is cancelled and the command carries the version
//! that was live before, that version is applied again. Its pods usually
//! still run (a rolling update keeps them until the new ones are ready), so
//! this mostly points the workload back at them.

use std::{collections::HashSet, time::Duration};

use k8s_openapi::api::autoscaling::v2::HorizontalPodAutoscaler;
use k8s_openapi::api::batch::v1::CronJob;
use k8s_openapi::api::{
    apps::v1::{Deployment, StatefulSet},
    core::v1::{Event, Namespace, PersistentVolumeClaim, Pod, Secret, Service},
    networking::v1::NetworkPolicy,
};
use kube::{
    Api, ResourceExt,
    api::{DeleteParams, ListParams, LogParams, Patch, PatchParams},
};
use pyo_proto::{
    CommandResult, DeploySpec, DeploymentProgress, Failure, KubeEvent, LogLine, now_ms,
};
use serde_json::{Value, json};
use tokio::sync::watch;

use crate::{
    Agent, MANAGER,
    observe::{SERVICE_LABEL, VOLUME_LABEL},
};

pub const DEPLOYMENT_ANNOTATION: &str = "pyo/deployment-id";

/// Container states that won't fix themselves.
const FATAL_WAITING: [&str; 5] = [
    "ErrImagePull",
    "ImagePullBackOff",
    "InvalidImageName",
    "CreateContainerConfigError",
    "CreateContainerError",
];

struct Reporter<'a> {
    agent: &'a Agent,
    deployment_id: String,
    stage: String,
    /// While rolling back, the deployment stays at "rolling_back" instead of
    /// following the previous version's pods through each stage.
    pinned: bool,
}

impl Reporter<'_> {
    async fn send(&self, p: DeploymentProgress) -> bool {
        match self.agent.api.progress(&self.deployment_id, &p).await {
            Ok(()) => true,
            Err(e) => {
                tracing::warn!(deployment = %self.deployment_id, error = %e, "progress not delivered");
                false
            }
        }
    }
    /// A stage not delivered is sent again with the next poll, so the
    /// deployment doesn't stay on an old one.
    async fn stage(&mut self, stage: &str, message: &str) {
        if self.stage == stage || self.pinned {
            return;
        }
        if self
            .send(DeploymentProgress {
                status: stage.into(),
                logs: vec![line("info", message)],
                ..Default::default()
            })
            .await
        {
            self.stage = stage.to_owned();
        }
    }
    async fn log(&self, level: &str, message: &str) {
        self.send(DeploymentProgress {
            logs: vec![line(level, message)],
            ..Default::default()
        })
        .await;
    }
    async fn events(&self, events: Vec<KubeEvent>) {
        if !events.is_empty() {
            self.send(DeploymentProgress {
                events,
                ..Default::default()
            })
            .await;
        }
    }
}

/// How a watched rollout ended.
enum Outcome {
    Ready {
        revision: Option<String>,
    },
    Failed {
        failure: Failure,
        logs: Vec<LogLine>,
    },
    Cancelled,
}

fn line(level: &str, message: &str) -> LogLine {
    LogLine {
        at: now_ms(),
        level: level.into(),
        source: "pyo-agent".into(),
        message: message.into(),
    }
}

pub async fn run(agent: &Agent, spec: DeploySpec) -> CommandResult {
    // Its gateway objects aren't leftovers while this runs (gateway.rs
    // `collect`), the rollback included.
    let _deploying = agent.deploying.start(&spec.service.service_id);
    let (tx, rx) = watch::channel(false);
    agent
        .cancels
        .lock()
        .unwrap()
        .insert(spec.deployment_id.clone(), tx);
    let mut r = Reporter {
        agent,
        deployment_id: spec.deployment_id.clone(),
        stage: "queued".into(),
        pinned: false,
    };
    let result = match apply_objects(agent, &spec, &mut r).await {
        // Nothing runs until its schedule says so, so there's nothing to watch.
        Ok(()) if spec.service.workload_kind == "CronJob" => Ok(Outcome::Ready { revision: None }),
        Ok(()) => watch_rollout(agent, &spec, &mut r, rx).await,
        Err(e) => Err(e),
    };
    agent.cancels.lock().unwrap().remove(&spec.deployment_id);

    let (status, failure, mut logs) = match result {
        Ok(Outcome::Ready { revision }) => {
            remove_old_env(agent, &spec).await;
            let msg = match &spec.autoscale {
                _ if spec.service.workload_kind == "CronJob" => {
                    format!("{} is scheduled: {} UTC", spec.service.name, spec.schedule)
                }
                Some(a) => format!(
                    "{} is live, autoscaling between {} and {}",
                    spec.service.name, a.min, a.max
                ),
                None => format!("{} is live: {} ready", spec.service.name, spec.replicas),
            };
            r.send(DeploymentProgress {
                status: "succeeded".into(),
                logs: vec![line("info", &msg)],
                revision,
                ..Default::default()
            })
            .await;
            return crate::ok("Rolled out");
        }
        Ok(Outcome::Cancelled) => ("cancelled", None, vec![line("warn", "Cancelled.")]),
        Ok(Outcome::Failed { failure, logs }) => ("failed", Some(failure), logs),
        Err(e) => {
            let msg = format!("{e:#}");
            (
                "failed",
                Some(Failure {
                    stage: r.stage.clone(),
                    reason: "ApplyFailed".into(),
                    message: msg.clone(),
                }),
                vec![line("error", &msg)],
            )
        }
    };

    // Report why it failed before putting the previous version back, so the
    // reason is on screen while that runs.
    let rolled_back = match spec.fallback.as_deref() {
        Some(prev) => {
            r.send(DeploymentProgress {
                status: "rolling_back".into(),
                failure: failure.clone(),
                logs: std::mem::take(&mut logs),
                ..Default::default()
            })
            .await;
            r.stage = "rolling_back".into();
            r.pinned = true;
            Some(roll_back(agent, prev, &mut r).await)
        }
        None => {
            if status == "cancelled" {
                logs.push(line("warn", "Objects already applied stay in place."));
            }
            None
        }
    };
    let message = failure
        .as_ref()
        .map(|f| f.message.clone())
        .unwrap_or_else(|| "Cancelled".into());
    r.send(DeploymentProgress {
        status: status.into(),
        failure,
        logs,
        rolled_back,
        ..Default::default()
    })
    .await;
    if status == "cancelled" {
        crate::ok(&message)
    } else {
        crate::failed(message)
    }
}

/// Apply the version that was live before and wait until it serves again.
async fn roll_back(agent: &Agent, prev: &DeploySpec, r: &mut Reporter<'_>) -> bool {
    r.log(
        "warn",
        &format!("Putting the previous version back ({})", prev.deployment_id),
    )
    .await;
    // Nothing cancels a rollback: it's what makes the service whole again.
    let (_keep, never) = watch::channel(false);
    let outcome = match apply_objects(agent, prev, r).await {
        Ok(()) => watch_rollout(agent, prev, r, never).await,
        Err(e) => Err(e),
    };
    match outcome {
        Ok(Outcome::Ready { .. }) => {
            remove_old_env(agent, prev).await;
            r.log(
                "info",
                &format!(
                    "Rolled back: {} is serving the previous version again",
                    prev.service.name
                ),
            )
            .await;
            true
        }
        Ok(Outcome::Failed { failure, .. }) => {
            r.log(
                "error",
                &format!(
                    "The previous version didn't come back either: {}",
                    failure.message
                ),
            )
            .await;
            false
        }
        Ok(Outcome::Cancelled) => false,
        Err(e) => {
            r.log(
                "error",
                &format!("Couldn't put the previous version back: {e:#}"),
            )
            .await;
            false
        }
    }
}

/// Each deployment reads its own Secret, so a new rollout can't change the
/// environment of pods already running, and a rollback brings its own back.
fn env_secret(spec: &DeploySpec) -> String {
    let id: String = spec
        .deployment_id
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    format!("{}-env-{id}", spec.service.name)
}

/// After a rollout settles, only the live deployment's Secret is needed
/// (older ones come back with their deployment's spec on a rollback).
async fn remove_old_env(agent: &Agent, spec: &DeploySpec) {
    let secrets = Api::<Secret>::namespaced(agent.kube.clone(), &spec.service.namespace);
    let keep = env_secret(spec);
    let prefix = format!("{}-env-", spec.service.name);
    let Ok(list) = secrets
        .list(
            &ListParams::default().labels(&format!("{SERVICE_LABEL}={}", spec.service.service_id)),
        )
        .await
    else {
        return;
    };
    for s in list.items {
        let name = s.name_any();
        if name != keep && name.starts_with(&prefix) {
            let _ = delete_if_exists(&secrets, &name).await;
        }
    }
}

fn labels(spec: &DeploySpec) -> Value {
    json!({
        "app.kubernetes.io/name": spec.service.name,
        "app.kubernetes.io/managed-by": MANAGER,
        SERVICE_LABEL: spec.service.service_id,
        "pyo/project-id": spec.project_id,
    })
}

fn probe(spec: &DeploySpec) -> Option<Value> {
    let h = &spec.health_check;
    let timing = json!({ "initialDelaySeconds": h.initial_delay_seconds.max(0), "periodSeconds": h.period_seconds.clamp(1, 300), "failureThreshold": 3 });
    let mut p = match h.kind.as_str() {
        "http" => {
            json!({ "httpGet": { "path": if h.path.is_empty() { "/" } else { &h.path }, "port": h.port } })
        }
        "tcp" => json!({ "tcpSocket": { "port": h.port } }),
        "exec" if !h.command.trim().is_empty() => {
            json!({ "exec": { "command": ["sh", "-c", h.command] } })
        }
        _ => return None,
    };
    if h.kind != "exec" && h.port <= 0 {
        return None;
    }
    p.as_object_mut()?.extend(timing.as_object()?.clone());
    Some(p)
}

/// Keeping traffic close: the gateway reaches a service through its
/// Service (gateway.rs's Backends), so kube-proxy on the server that
/// received the request chooses, and with PreferSameNode it picks a replica
/// on that server, else one in its zone (Pyo's region), else any. Older
/// clusters get PreferClose (same zone), or nothing.
///
/// Spread mode (no distribution): kube-proxy picks among every ready
/// replica, wherever they run.
fn service_manifest(
    name: &str,
    kind: &str,
    labels: Value,
    service_id: &str,
    ports: Vec<Value>,
    distribution: Option<&str>,
) -> Value {
    let mut s = json!({ "type": kind, "selector": { SERVICE_LABEL: service_id }, "ports": ports });
    if let Some(d) = distribution {
        s["trafficDistribution"] = json!(d);
    }
    json!({
        "apiVersion": "v1", "kind": "Service",
        "metadata": { "name": name, "labels": labels },
        "spec": s,
    })
}

/// The Service with a service's public TCP and UDP ports that bypass the
/// gateway (the API keeps service names from starting with its prefix).
pub fn public_service_name(name: &str) -> String {
    pyo_proto::public_service_name(name)
}

/// Spread mode leaves the distribution out, but a switch made in place
/// (`set_traffic_mode`) owns it outside of server-side apply, which then
/// wouldn't remove it: clear it explicitly.
async fn clear_distribution(
    services: &Api<Service>,
    name: &str,
    distribution: Option<&str>,
) -> anyhow::Result<()> {
    if distribution.is_none()
        && services
            .get_opt(name)
            .await?
            .and_then(|s| s.spec)
            .and_then(|s| s.traffic_distribution)
            .is_some()
    {
        services
            .patch(
                name,
                &PatchParams::default(),
                &kube::api::Patch::Merge(json!({ "spec": { "trafficDistribution": null } })),
            )
            .await?;
    }
    Ok(())
}

/// Switch every service Pyo runs on this cluster to a traffic mode, in
/// place: a merge patch of the distribution, so nothing restarts.
pub async fn set_traffic_mode(client: &kube::Client, mode: &str) -> anyhow::Result<String> {
    let distribution = traffic_distribution(client, mode).await;
    let patch = json!({ "spec": { "trafficDistribution": distribution } });
    let all = Api::<Service>::all(client.clone());
    let list = all
        .list(&kube::api::ListParams::default().labels(&format!(
            "app.kubernetes.io/managed-by={MANAGER},{SERVICE_LABEL}"
        )))
        .await?;
    let mut n = 0;
    for s in list.items {
        let (Some(ns), Some(name)) = (s.metadata.namespace.clone(), s.metadata.name.clone()) else {
            continue;
        };
        Api::<Service>::namespaced(client.clone(), &ns)
            .patch(
                &name,
                &PatchParams::default(),
                &kube::api::Patch::Merge(&patch),
            )
            .await?;
        n += 1;
    }
    let how = match mode {
        pyo_proto::TRAFFIC_SPREAD => "spread requests",
        pyo_proto::TRAFFIC_LOCAL => "keep requests on their server",
        _ => "keep requests in their region",
    };
    Ok(format!("{n} service(s) now {how}"))
}

/// On a Service published on the servers (NodePort or LoadBalancer): how
/// it's published, so it can be again after the cluster's public traffic
/// was off (it's a ClusterIP then).
pub const PUBLIC_ANNOTATION: &str = "pyo.dev/public";

#[derive(serde::Serialize, serde::Deserialize, Debug, Default, Clone, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Published {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external_traffic_policy: Option<String>,
    /// A NodePort Service's server ports, by port name.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub node_ports: std::collections::BTreeMap<String, i64>,
}

impl Published {
    pub fn of(svc: &Service) -> Option<Self> {
        svc.annotations()
            .get(PUBLIC_ANNOTATION)
            .and_then(|a| serde_json::from_str(a).ok())
    }
}

/// A Service manifest, written as published, made to match the cluster's
/// public traffic: unchanged but for the annotation when on, a ClusterIP
/// when off. `known`: the server ports it had (kept for ports without one
/// of their own, so turning public traffic back on doesn't move them).
fn publish(svc: &mut Value, on: bool, known: &std::collections::BTreeMap<String, i64>) {
    let kind = svc["spec"]["type"]
        .as_str()
        .unwrap_or("ClusterIP")
        .to_owned();
    if kind == "ClusterIP" {
        return;
    }
    let mut node_ports = std::collections::BTreeMap::new();
    if kind == "NodePort" {
        for p in svc["spec"]["ports"].as_array_mut().into_iter().flatten() {
            let name = p["name"].as_str().unwrap_or("").to_owned();
            if let Some(n) = p["nodePort"].as_i64().or_else(|| known.get(&name).copied()) {
                p["nodePort"] = json!(n);
                node_ports.insert(name, n);
            }
        }
    }
    let published = Published {
        kind,
        external_traffic_policy: svc["spec"]["externalTrafficPolicy"]
            .as_str()
            .map(str::to_owned),
        node_ports,
    };
    svc["metadata"]["annotations"][PUBLIC_ANNOTATION] =
        json!(serde_json::to_string(&published).unwrap_or_default());
    if !on {
        conceal(&mut svc["spec"]);
    }
}

/// A Service spec reachable only inside the cluster.
fn conceal(spec: &mut Value) {
    spec["type"] = json!("ClusterIP");
    if let Some(s) = spec.as_object_mut() {
        s.remove("externalTrafficPolicy");
    }
    for p in spec["ports"].as_array_mut().into_iter().flatten() {
        if let Some(p) = p.as_object_mut() {
            p.remove("nodePort");
        }
    }
}

/// The server ports a live Service has, or had before public traffic was
/// turned off.
fn node_ports_of(svc: Option<&Service>) -> std::collections::BTreeMap<String, i64> {
    let Some(svc) = svc else {
        return Default::default();
    };
    match svc.spec.as_ref().and_then(|s| s.type_.as_deref()) {
        Some("NodePort") => svc
            .spec
            .iter()
            .flat_map(|s| s.ports.iter().flatten())
            .filter_map(|p| Some((p.name.clone()?, p.node_port? as i64)))
            .collect(),
        _ => Published::of(svc).map(|p| p.node_ports).unwrap_or_default(),
    }
}

/// Apply a Service as the cluster's public traffic says. Kept server ports
/// that another Service took meanwhile are given up for new ones.
async fn apply_service(
    services: &Api<Service>,
    name: &str,
    mut svc: Value,
    on: bool,
) -> anyhow::Result<()> {
    let known = node_ports_of(services.get_opt(name).await?.as_ref());
    let plain = {
        let mut s = svc.clone();
        publish(&mut s, on, &Default::default());
        s
    };
    publish(&mut svc, on, &known);
    if svc != plain && apply(services, name, &svc).await.is_ok() {
        return Ok(());
    }
    apply(services, name, &plain).await
}

/// The live Service's spec changes that publish it on the servers or not.
/// None when it's already that way, or isn't Pyo's to change.
fn public_patch(svc: &Service, on: bool, node_ports: bool) -> Option<Value> {
    let mut published = Published::of(svc)?;
    let spec = svc.spec.as_ref()?;
    let current = spec.type_.as_deref().unwrap_or("ClusterIP");
    let wanted = if on {
        published.kind.as_str()
    } else {
        "ClusterIP"
    };
    if current == wanted {
        return None;
    }
    let mut ports = serde_json::to_value(spec.ports.clone().unwrap_or_default()).ok()?;
    if on {
        for p in ports.as_array_mut().into_iter().flatten() {
            let n = p["name"]
                .as_str()
                .and_then(|n| published.node_ports.get(n))
                .copied();
            match n {
                Some(n) if node_ports => p["nodePort"] = json!(n),
                _ => {
                    if let Some(p) = p.as_object_mut() {
                        p.remove("nodePort");
                    }
                }
            }
        }
        return Some(
            json!({ "spec": { "type": published.kind, "externalTrafficPolicy": published.external_traffic_policy, "ports": ports } }),
        );
    }
    // Off: the server ports it has now are kept for when it's on again.
    if current == "NodePort" {
        published.node_ports.extend(node_ports_of(Some(svc)));
    }
    let mut s = json!({ "type": "ClusterIP", "ports": ports });
    conceal(&mut s);
    s["externalTrafficPolicy"] = Value::Null;
    Some(json!({
        "metadata": { "annotations": { PUBLIC_ANNOTATION: serde_json::to_string(&published).unwrap_or_default() } },
        "spec": s,
    }))
}

/// Publish every service's own server ports (Port on every server, and
/// public ports that bypass the gateway) or stop, as the cluster's public
/// traffic says: in place, so nothing restarts. Only what differs changes;
/// it runs on each heartbeat.
pub async fn set_public_ports(client: &kube::Client, on: bool) -> anyhow::Result<()> {
    let all = Api::<Service>::all(client.clone());
    let list = all
        .list(&ListParams::default().labels(&format!(
            "app.kubernetes.io/managed-by={MANAGER},{SERVICE_LABEL}"
        )))
        .await?;
    let mut changed: Vec<String> = vec![];
    for svc in &list.items {
        let (Some(ns), Some(name)) = (svc.metadata.namespace.clone(), svc.metadata.name.clone())
        else {
            continue;
        };
        let Some(patch) = public_patch(svc, on, true) else {
            continue;
        };
        let api = Api::<Service>::namespaced(client.clone(), &ns);
        let pp = PatchParams {
            field_manager: Some(MANAGER.into()),
            ..Default::default()
        };
        if let Err(e) = api.patch(&name, &pp, &Patch::Merge(&patch)).await {
            // Its server ports were taken meanwhile: new ones.
            let Some(retry) = public_patch(svc, on, false).filter(|_| on) else {
                return Err(e.into());
            };
            api.patch(&name, &pp, &Patch::Merge(&retry)).await?;
        }
        tracing::info!(namespace = %ns, service = %name, on, "public ports");
        if !changed.contains(&ns) {
            changed.push(ns);
        }
    }
    // The namespaces' policies follow their public Services.
    for ns in changed {
        let isolated = Api::<NetworkPolicy>::namespaced(client.clone(), &ns)
            .get_opt(pyo_proto::ISOLATION_POLICY)
            .await?
            .is_some();
        set_isolation(client, &ns, isolated).await?;
    }
    Ok(())
}

/// A mode's traffic distribution, as far as this cluster's Kubernetes
/// supports it: local is PreferSameNode (1.34+, on by default there), else
/// PreferClose; region is PreferClose (1.31+, the same zone, which is the
/// server's region); spread is none.
async fn traffic_distribution(client: &kube::Client, mode: &str) -> Option<&'static str> {
    if mode == pyo_proto::TRAFFIC_SPREAD {
        return None;
    }
    let v = client.apiserver_version().await.ok()?;
    distribution_for(mode, &v.major, &v.minor)
}

fn distribution_for(mode: &str, major: &str, minor: &str) -> Option<&'static str> {
    let minor: u32 = minor.trim_end_matches('+').parse().ok()?;
    match (mode, major, minor) {
        (pyo_proto::TRAFFIC_SPREAD, ..) => None,
        (pyo_proto::TRAFFIC_LOCAL, "1", m) if m >= 34 => Some("PreferSameNode"),
        (_, "1", m) if m >= 31 => Some("PreferClose"),
        _ => None,
    }
}

/// How a Deployment rolls. A new pod normally starts before an old one
/// stops; a ReadWriteOnce volume can't be mounted by both, and a GPU service
/// would need a spare GPU, so those stop one old pod first.
fn deployment_strategy(single_writer: bool, gpu: u32) -> Value {
    if single_writer {
        json!({ "type": "Recreate", "rollingUpdate": null })
    } else if gpu > 0 {
        json!({ "type": "RollingUpdate", "rollingUpdate": { "maxUnavailable": 1, "maxSurge": 0 } })
    } else {
        json!({ "type": "RollingUpdate", "rollingUpdate": { "maxUnavailable": 0, "maxSurge": 1 } })
    }
}

fn pod_template(spec: &DeploySpec, secret_name: &str) -> Value {
    let ports: Vec<Value> = spec
        .ports
        .iter()
        .map(|p| json!({ "name": p.name, "containerPort": p.container_port, "protocol": if p.protocol == "UDP" { "UDP" } else { "TCP" } }))
        .collect();
    let mounts: Vec<Value> = spec
        .volumes
        .iter()
        .map(|v| json!({ "name": v.name, "mountPath": v.mount_path }))
        .collect();
    let volumes: Vec<Value> = spec
        .volumes
        .iter()
        .map(|v| json!({ "name": v.name, "persistentVolumeClaim": { "claimName": v.name } }))
        .collect();
    let r = &spec.resources;
    let mut container = json!({
        "name": "app",
        "image": spec.image,
        "envFrom": [{ "secretRef": { "name": secret_name } }],
        "ports": ports,
        "volumeMounts": mounts,
        "resources": {
            "requests": { "cpu": format!("{}m", r.cpu_request.max(1)), "memory": format!("{}Mi", r.memory_request.max(1)) },
            "limits": { "cpu": format!("{}m", r.cpu_limit.max(r.cpu_request).max(1)), "memory": format!("{}Mi", r.memory_limit.max(r.memory_request).max(1)) },
        },
    });
    if let Some(p) = probe(spec) {
        container["readinessProbe"] = p.clone();
        // Liveness waits longer so a slow start isn't killed in a loop.
        let mut live = p;
        live["initialDelaySeconds"] = json!(spec.health_check.initial_delay_seconds.max(0) + 30);
        container["livenessProbe"] = live;
    }
    // A stopping pod keeps serving for a moment while kube-proxy
    // drops it, so rollouts and rollbacks don't fail requests in flight.
    container["lifecycle"] = json!({ "preStop": { "sleep": { "seconds": 5 } } });
    // A Compose service's entrypoint, command, working directory and user.
    if let Some(o) = &spec.container {
        if !o.entrypoint.is_empty() {
            container["command"] = json!(o.entrypoint);
        }
        if !o.command.is_empty() {
            container["args"] = json!(o.command);
        }
        if !o.working_dir.is_empty() {
            container["workingDir"] = json!(o.working_dir);
        }
        if let Some(u) = o.run_as_user {
            container["securityContext"]["runAsUser"] = json!(u);
        }
        if let Some(g) = o.run_as_group {
            container["securityContext"]["runAsGroup"] = json!(g);
        }
    }
    let mut pod =
        json!({ "enableServiceLinks": false, "containers": [container], "volumes": volumes });
    if !spec.node_selector.is_empty() {
        pod["nodeSelector"] = json!(spec.node_selector);
    }
    // Picked servers: a selector can't say "one of these", affinity can.
    if !spec.server_names.is_empty() {
        pod["affinity"] = json!({ "nodeAffinity": { "requiredDuringSchedulingIgnoredDuringExecution": { "nodeSelectorTerms": [
            { "matchExpressions": [{ "key": "kubernetes.io/hostname", "operator": "In", "values": spec.server_names }] }
        ] } } });
    }
    if !spec.pull_login_id.is_empty() {
        pod["imagePullSecrets"] = json!([{ "name": pull_secret(&spec.service.name) }]);
    }
    // Replicas spread evenly across regions, then across the servers in each
    // (4 on two regions: 2+2). Soft: a full or offline region still gets
    // replicas elsewhere. Servers a pod can't use (placement, taints) don't
    // count. Regions are the zone label Pyo gives each server.
    let selector = json!({ "matchLabels": { SERVICE_LABEL: spec.service.service_id } });
    pod["topologySpreadConstraints"] = json!([
        { "maxSkew": 1, "topologyKey": pyo_proto::ZONE_LABEL, "whenUnsatisfiable": "ScheduleAnyway", "labelSelector": selector, "nodeAffinityPolicy": "Honor", "nodeTaintsPolicy": "Honor" },
        { "maxSkew": 1, "topologyKey": "kubernetes.io/hostname", "whenUnsatisfiable": "ScheduleAnyway", "labelSelector": selector, "nodeAffinityPolicy": "Honor", "nodeTaintsPolicy": "Honor" },
    ]);
    // Whole NVIDIA GPUs: the device plugin's resource, the nvidia runtime,
    // and only servers that have them.
    if r.gpu > 0 {
        pod["containers"][0]["resources"]["limits"][pyo_proto::GPU_RESOURCE] =
            json!(r.gpu.to_string());
        pod["runtimeClassName"] = json!(pyo_proto::GPU_RUNTIME_CLASS);
        pod["nodeSelector"][pyo_proto::GPU_LABEL] = json!("nvidia");
        // GPU servers may be kept for GPU work (tainted).
        pod["tolerations"] = json!([pyo_proto::gpu_toleration()]);
    }
    json!({
        "metadata": { "labels": labels(spec), "annotations": { DEPLOYMENT_ANNOTATION: spec.deployment_id } },
        "spec": pod,
    })
}

/// A pod that runs once: no probes or traffic, and it's not restarted in
/// place, so a failed run shows as failed.
fn cron_template(spec: &DeploySpec, secret_name: &str) -> Value {
    let mut template = pod_template(spec, secret_name);
    template["spec"]["restartPolicy"] = json!("Never");
    let container = &mut template["spec"]["containers"][0];
    if let Some(c) = container.as_object_mut() {
        for field in ["readinessProbe", "livenessProbe", "lifecycle", "ports"] {
            c.remove(field);
        }
    }
    if !spec.command.is_empty() {
        container["command"] = json!(["sh", "-c", spec.command]);
    }
    template
}

fn cron_manifest(spec: &DeploySpec, secret_name: &str) -> Value {
    json!({
        "apiVersion": "batch/v1", "kind": "CronJob",
        "metadata": { "name": spec.service.name, "labels": labels(spec) },
        "spec": {
            "schedule": spec.schedule,
            "timeZone": "Etc/UTC",
            // A slow run mustn't pile up on itself.
            "concurrencyPolicy": "Forbid",
            "startingDeadlineSeconds": 300,
            "successfulJobsHistoryLimit": 3,
            "failedJobsHistoryLimit": 3,
            "suspend": false,
            "jobTemplate": {
                "metadata": { "labels": labels(spec) },
                "spec": { "backoffLimit": 0, "template": cron_template(spec, secret_name) },
            },
        },
    })
}

/// How long a deploy waits for a deleted volume's claim of the same name to go.
const OLD_CLAIM_WAIT: Duration = Duration::from_secs(180);

/// Whether a claim is this volume's own, still in use (not on its way out).
fn claim_is(pvc: &PersistentVolumeClaim, volume_id: &str) -> bool {
    pvc.metadata.deletion_timestamp.is_none()
        && pvc.labels().get(VOLUME_LABEL).map(String::as_str) == Some(volume_id)
}

/// Waits until `old` (by uid) is gone: deleted volumes' claims go once their
/// pods have stopped. One that isn't being deleted is a clash, not a wait.
async fn wait_claim_gone(
    pvcs: &Api<PersistentVolumeClaim>,
    old: &PersistentVolumeClaim,
    timeout: Duration,
) -> anyhow::Result<()> {
    let name = old.name_any();
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        match pvcs.get_opt(&name).await? {
            Some(c) if c.metadata.uid == old.metadata.uid => {
                if tokio::time::Instant::now() > deadline {
                    let why = if c.metadata.deletion_timestamp.is_some() {
                        "is still being removed"
                    } else {
                        "belongs to another volume"
                    };
                    anyhow::bail!(
                        "The volume name {name} {why}. Try again in a minute, or use another name."
                    );
                }
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
            _ => return Ok(()),
        }
    }
}

async fn apply<K>(api: &Api<K>, name: &str, body: &Value) -> anyhow::Result<()>
where
    K: kube::Resource + Clone + serde::de::DeserializeOwned + std::fmt::Debug,
{
    api.patch(
        name,
        &PatchParams::apply(MANAGER).force(),
        &Patch::Apply(body),
    )
    .await?;
    Ok(())
}

/// How big to make an autoscaled service in a rollout: as big as it is now,
/// so a deploy during a traffic peak doesn't shrink it.
fn autoscaled_replicas(current: Option<i32>, a: &pyo_proto::Autoscale) -> i32 {
    current.unwrap_or(a.min).clamp(a.min, a.max)
}

fn autoscaler_manifest(service: &pyo_proto::ServiceRef, a: &pyo_proto::Autoscale) -> Value {
    let target = |name: &str, pct: i32| json!({ "type": "Resource", "resource": { "name": name, "target": { "type": "Utilization", "averageUtilization": pct } } });
    let mut metrics = vec![target("cpu", a.target_cpu)];
    if let Some(m) = a.target_memory {
        metrics.push(target("memory", m));
    }
    json!({
        "apiVersion": "autoscaling/v2", "kind": "HorizontalPodAutoscaler",
        "metadata": { "name": service.name, "labels": { SERVICE_LABEL: service.service_id } },
        "spec": {
            "scaleTargetRef": { "apiVersion": "apps/v1", "kind": "Deployment", "name": service.name },
            "minReplicas": a.min,
            "maxReplicas": a.max,
            "metrics": metrics,
        },
    })
}

/// Create, change or remove the service's autoscaler.
pub async fn apply_autoscaler(
    client: &kube::Client,
    service: &pyo_proto::ServiceRef,
    autoscale: Option<&pyo_proto::Autoscale>,
) -> anyhow::Result<()> {
    let api = Api::<HorizontalPodAutoscaler>::namespaced(client.clone(), &service.namespace);
    match autoscale {
        Some(a) => apply(&api, &service.name, &autoscaler_manifest(service, a)).await,
        None => delete_if_exists(&api, &service.name).await,
    }
}

async fn delete_if_exists<K>(api: &Api<K>, name: &str) -> anyhow::Result<()>
where
    K: kube::Resource + Clone + serde::de::DeserializeOwned + std::fmt::Debug,
{
    match api.delete(name, &DeleteParams::background()).await {
        Ok(_) => Ok(()),
        Err(kube::Error::Api(e)) if e.code == 404 => Ok(()),
        Err(e) => Err(e.into()),
    }
}

/// An isolated project's namespace denies ingress except from its own pods,
/// kube-system (ServiceLB) and pyo-gateway (the gateway). The node's own
/// traffic (kubelet probes) is always allowed by k3s's policy controller.
/// NodePort and LoadBalancer Services stay reachable from anywhere through a
/// policy each. Open namespaces have none of these: in Kubernetes, any
/// policy selecting a pod blocks what it doesn't allow.
///
/// The firewall: a public Service whose ports bypass the gateway
/// carries the address ranges they let in (gateway.rs's annotation). Its
/// policy then allows only those, isolated or not; in an open namespace it
/// also lets everything else in as before (the cluster's own pods, and its
/// other ports from anywhere), since the policy now selects the pods.
pub async fn set_isolation(
    client: &kube::Client,
    ns: &str,
    isolate: bool,
) -> anyhow::Result<String> {
    let api = Api::<NetworkPolicy>::namespaced(client.clone(), ns);
    let mut wanted: Vec<(String, Value)> = Vec::new();
    if isolate {
        let team = Api::<Namespace>::all(client.clone())
            .get_opt(ns)
            .await?
            .and_then(|n| n.labels().get(pyo_proto::TEAM_LABEL).cloned())
            .unwrap_or_default();
        wanted.push((
            pyo_proto::ISOLATION_POLICY.to_owned(),
            isolation_spec(&team),
        ));
    }
    let all = Api::<Service>::namespaced(client.clone(), ns)
        .list(&ListParams::default())
        .await?
        .items;
    let target = |p: &k8s_openapi::api::core::v1::ServicePort| json!({ "port": p.target_port.clone().map(|t| serde_json::to_value(t).unwrap_or(Value::Null)).unwrap_or(json!(p.port)), "protocol": p.protocol.clone().unwrap_or_else(|| "TCP".into()) });
    for svc in &all {
        let Some(spec) = &svc.spec else { continue };
        if !matches!(spec.type_.as_deref(), Some("NodePort" | "LoadBalancer")) {
            continue;
        }
        let blocks: Option<Vec<(String, Vec<String>)>> = svc
            .annotations()
            .get(crate::gateway::FIREWALL_ANNOTATION)
            .filter(|a| !a.is_empty())
            .and_then(|a| serde_json::from_str(a).ok());
        if !isolate && blocks.is_none() {
            continue;
        }
        let ports: Vec<Value> = spec.ports.iter().flatten().map(target).collect();
        let from = match &blocks {
            Some(b) => crate::firewall::from_blocks(b),
            None => crate::firewall::anywhere(),
        };
        let mut ingress = vec![];
        if !from.is_empty() {
            ingress.push(json!({ "from": from, "ports": ports }));
        }
        if !isolate {
            // Open namespace: the rest as before, now that a policy selects the pods.
            ingress.push(json!({ "from": [{ "namespaceSelector": {} }] }));
            let selector = spec.selector.clone().unwrap_or_default();
            let others: Vec<Value> = all
                .iter()
                .filter(|o| o.metadata.name != svc.metadata.name)
                .filter_map(|o| o.spec.as_ref())
                .filter(|o| o.selector.as_ref() == Some(&selector))
                .flat_map(|o| o.ports.iter().flatten().map(target))
                .filter(|p| !ports.contains(p))
                .collect();
            if !others.is_empty() {
                ingress.push(json!({ "from": crate::firewall::anywhere(), "ports": others }));
            }
        }
        wanted.push((
            format!("pyo-public-{}", svc.name_any()),
            json!({
                "podSelector": { "matchLabels": spec.selector.clone().unwrap_or_default() },
                "policyTypes": ["Ingress"],
                "ingress": ingress,
            }),
        ));
    }
    for (name, spec) in &wanted {
        apply(
            &api,
            name,
            &json!({
                "apiVersion": "networking.k8s.io/v1", "kind": "NetworkPolicy",
                "metadata": { "name": name, "labels": { "app.kubernetes.io/managed-by": MANAGER } },
                "spec": spec,
            }),
        )
        .await?;
    }
    // Pyo's policies no longer wanted (isolation off, or a port made private).
    for p in api.list(&ListParams::default()).await?.items {
        let name = p.name_any();
        let ours = name == pyo_proto::ISOLATION_POLICY || name.starts_with("pyo-public-");
        if ours && !wanted.iter().any(|(w, _)| *w == name) {
            delete_if_exists(&api, &name).await?;
        }
    }
    Ok(if isolate {
        format!("{ns} is isolated")
    } else {
        format!("{ns} is open to other namespaces")
    })
}

/// An isolated project's own policy (see `set_isolation`). Its team's
/// imports come in too: only their pods (pyo-proto's `dbimport`), from the
/// team's databases namespace, so the databases themselves don't.
pub fn isolation_spec(team_id: &str) -> Value {
    let mut from = vec![
        json!({ "podSelector": {} }),
        json!({ "namespaceSelector": { "matchLabels": { "kubernetes.io/metadata.name": "kube-system" } } }),
        // The gateway.
        json!({ "namespaceSelector": { "matchLabels": { "kubernetes.io/metadata.name": pyo_proto::system::GATEWAY_NS } } }),
    ];
    if !team_id.is_empty() {
        let databases = json!({ "kubernetes.io/metadata.name": pyo_proto::databases_namespace(team_id), pyo_proto::TEAM_LABEL: team_id });
        for (key, value) in [
            pyo_proto::dbimport::POD_LABEL,
            pyo_proto::dbimport::CNPG_POD_LABEL,
        ] {
            from.push(json!({ "namespaceSelector": { "matchLabels": databases }, "podSelector": { "matchLabels": { key: value } } }));
        }
    }
    json!({ "podSelector": {}, "policyTypes": ["Ingress"], "ingress": [{ "from": from }] })
}

/// A project's namespace, as a deploy (or a volume moved into it) makes it.
pub fn project_namespace(ns: &str, project_id: &str, team_id: &str) -> Value {
    let mut namespace = json!({
        "apiVersion": "v1", "kind": "Namespace",
        "metadata": { "name": ns, "labels": {
            "app.kubernetes.io/managed-by": MANAGER,
            "pyo/project-id": project_id,
            // Baseline is enforced (no privileged pods, host network or host
            // paths); restricted is warned about and audited, since many
            // images still start as root.
            "pod-security.kubernetes.io/enforce": "baseline",
            "pod-security.kubernetes.io/enforce-version": "latest",
            "pod-security.kubernetes.io/warn": "restricted",
            "pod-security.kubernetes.io/audit": "restricted",
        } },
    });
    // Its team: the team's databases let it in (databases.rs).
    if !team_id.is_empty() {
        namespace["metadata"]["labels"][pyo_proto::TEAM_LABEL] = json!(team_id);
    }
    namespace
}

async fn apply_objects(
    agent: &Agent,
    spec: &DeploySpec,
    r: &mut Reporter<'_>,
) -> anyhow::Result<()> {
    let client = agent.kube.clone();
    let ns = spec.service.namespace.as_str();
    let name = spec.service.name.as_str();
    let secret_name = env_secret(spec);
    r.stage("applying", &format!("Applying {name}'s settings"))
        .await;

    let namespace = project_namespace(ns, &spec.project_id, &spec.team_id);
    apply(&Api::<Namespace>::all(client.clone()), ns, &namespace).await?;
    // Labelled as Pyo's, with the agent's write role bound in it.
    crate::scope::ensure_managed(&client, ns).await?;

    let env: serde_json::Map<String, Value> = spec
        .env
        .iter()
        .map(|e| (e.key.clone(), json!(e.value)))
        .collect();
    apply(
        &Api::<Secret>::namespaced(client.clone(), ns),
        &secret_name,
        &json!({
            "apiVersion": "v1", "kind": "Secret",
            "metadata": { "name": secret_name, "labels": labels(spec) },
            "type": "Opaque",
            "stringData": env,
        }),
    )
    .await?;
    r.log(
        "info",
        &format!("{} environment variable(s)", spec.env.len()),
    )
    .await;

    let pvcs = Api::<PersistentVolumeClaim>::namespaced(client.clone(), ns);
    for v in &spec.volumes {
        // A claim's spec can't change after creation (except growing), so only
        // create it. A claim by that name from a deleted volume (another id,
        // or on its way out) is waited out first, or it would vanish under us.
        if let Some(old) = pvcs.get_opt(&v.name).await? {
            if claim_is(&old, &v.volume_id) {
                continue;
            }
            r.log(
                "info",
                &format!("Waiting for the old volume {} to be removed", v.name),
            )
            .await;
            wait_claim_gone(&pvcs, &old, OLD_CLAIM_WAIT).await?;
        }
        let mut body = json!({
            "apiVersion": "v1", "kind": "PersistentVolumeClaim",
            "metadata": { "name": v.name, "labels": { "app.kubernetes.io/managed-by": MANAGER, VOLUME_LABEL: v.volume_id, SERVICE_LABEL: spec.service.service_id } },
            "spec": { "accessModes": [v.access_mode], "resources": { "requests": { "storage": format!("{}Gi", v.size_gi.max(1)) } } },
        });
        if !v.storage_class.is_empty() {
            body["spec"]["storageClassName"] = json!(v.storage_class);
        }
        apply(&pvcs, &v.name, &body).await?;
        r.log(
            "info",
            &format!(
                "Created volume {} ({} GiB) at {}",
                v.name, v.size_gi, v.mount_path
            ),
        )
        .await;
    }

    // A private registry: the team's login, as the pull secret the pods use.
    if !spec.pull_login_id.is_empty() {
        let login = agent
            .api
            .registry_login(&spec.pull_login_id)
            .await
            .map_err(|e| anyhow::anyhow!("Couldn't get the login for the image's registry: {e}"))?;
        let server = if login.host == "docker.io" {
            "https://index.docker.io/v1/".to_owned()
        } else {
            login.host.clone()
        };
        let auth = base64::Engine::encode(
            &base64::engine::general_purpose::STANDARD,
            format!("{}:{}", login.username, login.password),
        );
        let config = json!({ "auths": { server: { "username": login.username, "password": login.password, "auth": auth } } });
        apply(
            &Api::<Secret>::namespaced(client.clone(), ns),
            &pull_secret(name),
            &json!({
                "apiVersion": "v1", "kind": "Secret", "type": "kubernetes.io/dockerconfigjson",
                "metadata": { "name": pull_secret(name), "labels": labels(spec) },
                "stringData": { ".dockerconfigjson": config.to_string() },
            }),
        )
        .await?;
        r.log(
            "info",
            &format!("Pulling with the team's login for {}", login.host),
        )
        .await;
    }

    let selector = json!({ "matchLabels": { SERVICE_LABEL: spec.service.service_id } });
    let template = pod_template(spec, &secret_name);
    let single_writer = spec
        .volumes
        .iter()
        .any(|v| v.access_mode == "ReadWriteOnce");
    if spec.service.workload_kind == "CronJob" {
        apply(
            &Api::<CronJob>::namespaced(client.clone(), ns),
            name,
            &cron_manifest(spec, &secret_name),
        )
        .await?;
    } else if spec.service.workload_kind == "StatefulSet" {
        apply(&Api::<StatefulSet>::namespaced(client.clone(), ns), name, &json!({
            "apiVersion": "apps/v1", "kind": "StatefulSet",
            "metadata": { "name": name, "labels": labels(spec) },
            "spec": { "replicas": spec.replicas, "serviceName": name, "selector": selector, "template": template },
        }))
        .await?;
    } else {
        let strategy = deployment_strategy(single_writer, spec.resources.gpu);
        let deployments = Api::<Deployment>::namespaced(client.clone(), ns);
        let replicas = match &spec.autoscale {
            Some(a) => autoscaled_replicas(
                deployments
                    .get_opt(name)
                    .await?
                    .and_then(|d| d.spec)
                    .and_then(|s| s.replicas),
                a,
            ),
            None => spec.replicas,
        };
        apply(&deployments, name, &json!({
            "apiVersion": "apps/v1", "kind": "Deployment",
            "metadata": { "name": name, "labels": labels(spec) },
            "spec": { "replicas": replicas, "revisionHistoryLimit": 5, "progressDeadlineSeconds": spec.timeout_seconds, "strategy": strategy, "selector": selector, "template": template },
        }))
        .await?;
    }
    apply_autoscaler(&client, &spec.service, spec.autoscale.as_ref()).await?;
    // The service's cron jobs follow it, to this version's image and variables.
    if spec.service.workload_kind != "CronJob" {
        crate::cron::sync_list(&client, &spec.service, &spec.cron_jobs).await?;
    }
    match &spec.autoscale {
        _ if spec.service.workload_kind == "CronJob" => {
            r.log(
                "info",
                &format!("{name}: runs {} (UTC), {}", spec.schedule, spec.image),
            )
            .await
        }
        Some(a) => {
            r.log(
                "info",
                &format!(
                    "{name}: {} to {} replicas of {}, autoscaling at {}% CPU",
                    a.min, a.max, spec.image, a.target_cpu
                ),
            )
            .await
        }
        None => {
            r.log(
                "info",
                &format!("{name}: {} replica(s) of {}", spec.replicas, spec.image),
            )
            .await
        }
    }

    // Ports going through the gateway that no longer do: their
    // listeners go first, so a port moving to its own public address is free.
    for note in crate::gateway::release_ports(&client, spec).await? {
        r.log("warn", &note).await;
    }

    // Services: the service's own (every port, at the port the cluster reaches
    // it on), and one with only the public TCP and UDP ports that bypass the
    // gateway. Kept apart so a proxied port is never also published
    // straight from the servers (around the proxy's IP rules, and on the
    // same server port as the proxy).
    let services = Api::<Service>::namespaced(client.clone(), ns);
    let public = public_service_name(name);
    let mode = if spec.traffic_mode.is_empty() {
        pyo_proto::TRAFFIC_REGION
    } else {
        spec.traffic_mode.as_str()
    };
    let distribution = traffic_distribution(&client, mode).await;
    // While the cluster's public traffic is off, nothing is published on
    // the servers: these Services are ClusterIPs then.
    let public_traffic = crate::gateway::public_now(&client).await;
    if spec.ports.is_empty() {
        delete_if_exists(&services, name).await?;
    } else {
        let kind = if spec.ports.iter().any(|p| p.exposure == "nodeport") {
            "NodePort"
        } else {
            "ClusterIP"
        };
        let ports: Vec<Value> = spec
            .ports
            .iter()
            .map(|p| {
                let mut port = json!({ "name": p.name, "port": if p.exposure == "loadbalancer" { p.public() } else { p.container_port }, "targetPort": p.name, "protocol": p.transport() });
                if p.exposure == "nodeport"
                    && let Some(np) = p.public_port {
                        port["nodePort"] = json!(np);
                    }
                port
            })
            .collect();
        apply_service(
            &services,
            name,
            service_manifest(
                name,
                kind,
                labels(spec),
                &spec.service.service_id,
                ports,
                distribution,
            ),
            public_traffic,
        )
        .await?;
        clear_distribution(&services, name, distribution).await?;
    }
    let direct: Vec<Value> = spec.ports.iter().filter(|p| p.direct()).map(|p| json!({ "name": p.name, "port": p.public(), "targetPort": p.name, "protocol": p.transport() })).collect();
    if direct.is_empty() {
        delete_if_exists(&services, &public).await?;
    } else {
        // Local: the servers hand connections to a replica on themselves
        // without rewriting the source, so the service sees the visitor's
        // own address (a server without a replica doesn't answer).
        let mut svc = service_manifest(
            &public,
            "LoadBalancer",
            labels(spec),
            &spec.service.service_id,
            direct,
            distribution,
        );
        svc["spec"]["externalTrafficPolicy"] = json!("Local");
        // The firewall's ranges for these ports (set_isolation's policy
        // lets only them in): "" when everyone may connect.
        svc["metadata"]["annotations"][crate::gateway::FIREWALL_ANNOTATION] = json!(
            crate::gateway::ip_blocks_json(&crate::gateway::fw_layers(spec, None))
        );
        apply_service(&services, &public, svc, public_traffic).await?;
        clear_distribution(&services, &public, distribution).await?;
    }
    // After the Service, so an isolated namespace keeps its public ports open.
    set_isolation(&client, ns, spec.network_isolation).await?;

    // Routes through the gateway (gateway.rs): its domains, with
    // their certificates and HTTPS redirects.
    // With the service's newest domains (a change synced since this was
    // handed out wins: gateway.rs `RouteState`).
    let (notes, domains) = crate::gateway::apply_routes(agent, spec, false)
        .await?
        .unwrap_or_default();
    for note in notes {
        r.log("warn", &note).await;
    }
    if !domains.is_empty() {
        let hosts: Vec<String> = domains
            .iter()
            .map(|d| {
                format!(
                    "{}://{}",
                    if d.tls.is_empty() { "http" } else { "https" },
                    d.hostname
                )
            })
            .collect();
        r.log("info", &format!("Serving at {}", hosts.join(", ")))
            .await;
    }

    Ok(())
}

/// Poll the service's pods and workload until the rollout settles.
async fn watch_rollout(
    agent: &Agent,
    spec: &DeploySpec,
    r: &mut Reporter<'_>,
    mut cancel: watch::Receiver<bool>,
) -> anyhow::Result<Outcome> {
    let client = agent.kube.clone();
    let ns = spec.service.namespace.as_str();
    let pods_api = Api::<Pod>::namespaced(client.clone(), ns);
    let events_api = Api::<Event>::namespaced(client.clone(), ns);
    let started = tokio::time::Instant::now();
    let deadline = started + Duration::from_secs(spec.timeout_seconds);
    let mut seen_events: HashSet<String> = HashSet::new();
    let since = now_ms() - 5_000;
    // A health check that keeps failing won't fix itself: give it the probe's
    // own delay plus six periods (at least a minute), then report why.
    let h = &spec.health_check;
    let probe_grace = Duration::from_secs(
        ((h.initial_delay_seconds.max(0) + 6 * h.period_seconds.max(1)) as u64).max(60),
    );
    let mut failing_probe: Option<(tokio::time::Instant, String)> = None;

    loop {
        if *cancel.borrow() {
            return Ok(Outcome::Cancelled);
        }

        let (pods, older): (Vec<Pod>, Vec<Pod>) = pods_api
            .list(
                &ListParams::default()
                    .labels(&format!("{SERVICE_LABEL}={}", spec.service.service_id)),
            )
            .await?
            .items
            .into_iter()
            .partition(|p| p.annotations().get(DEPLOYMENT_ANNOTATION) == Some(&spec.deployment_id));

        // New events about this service's objects.
        let mut fresh = vec![];
        if let Ok(list) = events_api.list(&ListParams::default()).await {
            for e in list.items {
                let obj = e.involved_object.name.clone().unwrap_or_default();
                if !obj.starts_with(&spec.service.name) {
                    continue;
                }
                let at = e
                    .last_timestamp
                    .as_ref()
                    .or(e.first_timestamp.as_ref())
                    .map(|t| t.0.as_millisecond())
                    .or_else(|| e.event_time.as_ref().map(|t| t.0.as_millisecond()))
                    .unwrap_or(0);
                let uid = format!(
                    "{}:{}",
                    e.metadata.uid.clone().unwrap_or_default(),
                    e.count.unwrap_or(1)
                );
                if at < since || !seen_events.insert(uid) {
                    continue;
                }
                let reason = e.reason.clone().unwrap_or_default();
                let mut message = e.message.clone().unwrap_or_default();
                if reason == "FailedScheduling" {
                    tracing::info!(deployment = %spec.deployment_id, object = %obj, %message, "failed scheduling");
                    message = scheduling_message(&message);
                }
                fresh.push(KubeEvent {
                    at,
                    kind: e.type_.clone().unwrap_or_else(|| "Normal".into()),
                    reason,
                    object: format!(
                        "{}/{obj}",
                        e.involved_object
                            .kind
                            .clone()
                            .unwrap_or_default()
                            .to_lowercase()
                    ),
                    message,
                });
            }
        }
        fresh.sort_by_key(|e| e.at);
        if let Some(e) = fresh
            .iter()
            .rev()
            .find(|e| e.reason == "Unhealthy" && e.message.contains("Readiness probe failed"))
        {
            let first = failing_probe
                .as_ref()
                .map(|(t, _)| *t)
                .unwrap_or_else(tokio::time::Instant::now);
            failing_probe = Some((first, e.message.clone()));
        }
        r.events(fresh).await;

        // Hard failures in any new pod.
        for p in &pods {
            let Some(st) = &p.status else { continue };
            for cs in st.container_statuses.iter().flatten() {
                if let Some(w) = cs.state.as_ref().and_then(|s| s.waiting.as_ref()) {
                    let reason = w.reason.clone().unwrap_or_default();
                    let crashing = reason == "CrashLoopBackOff" && cs.restart_count >= 2;
                    if FATAL_WAITING.contains(&reason.as_str()) || crashing {
                        let stage = if reason.contains("Image") {
                            "pulling"
                        } else {
                            "starting"
                        };
                        let mut logs = vec![line(
                            "error",
                            &format!("{}: {}", reason, w.message.clone().unwrap_or_default()),
                        )];
                        if crashing {
                            logs.extend(previous_logs(&pods_api, &p.name_any()).await);
                        }
                        let message = match reason.as_str() {
                            "ErrImagePull" | "ImagePullBackOff"
                                if refused(w.message.as_deref().unwrap_or("")) =>
                            {
                                if spec.pull_login_id.is_empty() {
                                    format!(
                                        "The registry refused to send {} without a login. If it's private, add a login for its registry in Integrations, then deploy again.",
                                        spec.image
                                    )
                                } else {
                                    format!(
                                        "The registry refused the team's login for {}. Check the username and token in Integrations, then deploy again.",
                                        spec.image
                                    )
                                }
                            }
                            "ErrImagePull" | "ImagePullBackOff" => format!(
                                "Couldn't pull {}. Check the image name and tag, and that the registry is reachable.",
                                spec.image
                            ),
                            "InvalidImageName" => {
                                format!("{} isn't a valid image reference.", spec.image)
                            }
                            "CrashLoopBackOff" => format!(
                                "The container keeps exiting (restarted {} times). See its last output below.",
                                cs.restart_count
                            ),
                            _ => w.message.clone().unwrap_or_else(|| reason.clone()),
                        };
                        r.stage(stage, "").await;
                        return Ok(Outcome::Failed {
                            failure: Failure {
                                stage: stage.into(),
                                reason: reason.clone(),
                                message,
                            },
                            logs,
                        });
                    }
                }
            }
            // Unschedulable for a while: not enough CPU or memory, or a volume can't bind.
            let unschedulable = st.conditions.iter().flatten().find(|c| {
                c.type_ == "PodScheduled"
                    && c.status == "False"
                    && c.reason.as_deref() == Some("Unschedulable")
            });
            if let Some(c) = unschedulable
                && started.elapsed() > Duration::from_secs(90)
            {
                if let Some(raw) = &c.message {
                    tracing::info!(deployment = %spec.deployment_id, message = %raw, "unschedulable");
                }
                let message = c
                    .message
                    .as_deref()
                    .map(scheduling_message)
                    .unwrap_or_else(|| "No server has room to run it.".into());
                let logs = vec![line("error", &message)];
                return Ok(Outcome::Failed {
                    failure: Failure {
                        stage: "scheduling".into(),
                        reason: "Unschedulable".into(),
                        message,
                    },
                    logs,
                });
            }
        }

        // Stage from the furthest-along new pod.
        let stage = pod_stage(&pods);
        let label = match stage {
            "scheduling" => "Finding a server to run it",
            "pulling" => "Downloading the image",
            "starting" => "Starting",
            _ => "Waiting for the health check to pass",
        };
        r.stage(stage, label).await;

        // Old replicas still stopping with a volume: not done until they've
        // let go of it (so it can be moved or attached elsewhere straight
        // after), unless that outlasts the rollout's time.
        let letting_go = older
            .iter()
            .any(|p| p.metadata.deletion_timestamp.is_some() && mounts_a_claim(p));
        if rollout_complete(agent, spec).await?
            && (!letting_go || tokio::time::Instant::now() > deadline)
        {
            let revision = pods
                .iter()
                .find_map(|p| {
                    Some(
                        p.status
                            .as_ref()?
                            .container_statuses
                            .as_ref()?
                            .first()?
                            .image_id
                            .clone(),
                    )
                })
                .filter(|id| !id.is_empty());
            r.stage("waiting_ready", "Waiting for the health check to pass")
                .await;
            return Ok(Outcome::Ready { revision });
        }

        if let Some((first, msg)) = &failing_probe
            && first.elapsed() > probe_grace
            && pods
                .iter()
                .any(|p| p.status.as_ref().and_then(|s| s.phase.as_deref()) == Some("Running"))
        {
            let what = match h.kind.as_str() {
                "http" => format!(
                    "HTTP GET {} on port {}",
                    if h.path.is_empty() { "/" } else { &h.path },
                    h.port
                ),
                "tcp" => format!("a connection to port {}", h.port),
                _ => "the health check command".to_owned(),
            };
            let message = format!(
                "The container runs, but its health check ({what}) keeps failing. Check the service's port and health check settings."
            );
            return Ok(Outcome::Failed {
                failure: Failure {
                    stage: "waiting_ready".into(),
                    reason: "HealthCheckFailing".into(),
                    message,
                },
                logs: vec![line("error", msg)],
            });
        }

        if tokio::time::Instant::now() > deadline {
            let message = format!(
                "The rollout didn't finish within {} seconds.",
                spec.timeout_seconds
            );
            let logs = vec![line("error", &message)];
            return Ok(Outcome::Failed {
                failure: Failure {
                    stage: r.stage.clone(),
                    reason: "ProgressDeadlineExceeded".into(),
                    message,
                },
                logs,
            });
        }
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_millis(1500)) => {}
            _ = cancel.changed() => {}
        }
    }
}

fn mounts_a_claim(p: &Pod) -> bool {
    p.spec
        .as_ref()
        .and_then(|s| s.volumes.as_ref())
        .is_some_and(|vs| vs.iter().any(|v| v.persistent_volume_claim.is_some()))
}

fn pod_stage(pods: &[Pod]) -> &'static str {
    let mut best = 0;
    for p in pods {
        let Some(st) = &p.status else { continue };
        let scheduled = st
            .conditions
            .iter()
            .flatten()
            .any(|c| c.type_ == "PodScheduled" && c.status == "True");
        let mut rank = if scheduled { 1 } else { 0 };
        for cs in st.container_statuses.iter().flatten() {
            let state = cs.state.clone().unwrap_or_default();
            if state.running.is_some() {
                // Started (past any startup probe) but not ready: the
                // readiness check is what it waits for now.
                rank = rank.max(if cs.ready || cs.started == Some(true) {
                    4
                } else {
                    3
                });
            } else if let Some(w) = state.waiting
                && (w.reason.as_deref() == Some("ContainerCreating")
                    || w.reason.as_deref() == Some("PodInitializing"))
            {
                rank = rank.max(2);
            }
        }
        best = best.max(rank);
    }
    match best {
        0 => "scheduling",
        1 | 2 => "pulling",
        3 => "starting",
        _ => "waiting_ready",
    }
}

async fn rollout_complete(agent: &Agent, spec: &DeploySpec) -> anyhow::Result<bool> {
    let ns = spec.service.namespace.as_str();
    let name = spec.service.name.as_str();
    if spec.service.workload_kind == "StatefulSet" {
        let Some(s) = Api::<StatefulSet>::namespaced(agent.kube.clone(), ns)
            .get_opt(name)
            .await?
        else {
            return Ok(false);
        };
        let gen_seen =
            s.status.as_ref().and_then(|st| st.observed_generation) >= s.metadata.generation;
        let st = s.status.unwrap_or_default();
        let want = s.spec.and_then(|sp| sp.replicas).unwrap_or(1);
        Ok(gen_seen
            && st.updated_replicas.unwrap_or(0) >= want
            && st.ready_replicas.unwrap_or(0) >= want
            && st.current_revision == st.update_revision)
    } else {
        let Some(d) = Api::<Deployment>::namespaced(agent.kube.clone(), ns)
            .get_opt(name)
            .await?
        else {
            return Ok(false);
        };
        let gen_seen =
            d.status.as_ref().and_then(|st| st.observed_generation) >= d.metadata.generation;
        let st = d.status.unwrap_or_default();
        let want = d.spec.and_then(|sp| sp.replicas).unwrap_or(1);
        // All replicas updated and available, and no old ones left.
        Ok(gen_seen
            && st.updated_replicas.unwrap_or(0) >= want
            && st.available_replicas.unwrap_or(0) >= want
            && st.replicas.unwrap_or(0) == want)
    }
}

/// The last output of a crashed container, for the failure report.
async fn previous_logs(pods: &Api<Pod>, pod: &str) -> Vec<LogLine> {
    let lp = LogParams {
        previous: true,
        tail_lines: Some(40),
        ..Default::default()
    };
    match pods.logs(pod, &lp).await {
        Ok(out) => out
            .lines()
            .map(|l| LogLine {
                at: now_ms(),
                level: "error".into(),
                source: pod.into(),
                message: l.into(),
            })
            .collect(),
        Err(_) => vec![],
    }
}

/// The pull secret of a service whose image is on a private registry.
pub fn pull_secret(service: &str) -> String {
    format!("{service}-pull")
}

/// A pull failed because the registry wants (other) credentials.
fn refused(message: &str) -> bool {
    let m = message.to_lowercase();
    [
        "401",
        "403",
        "unauthorized",
        "denied",
        "authorization failed",
        "failed to authorize",
        "authentication required",
    ]
    .iter()
    .any(|k| m.contains(k))
}

/// The scheduler's "0/3 nodes are available: …" in server terms, for the
/// deploy log. The raw text only goes to the agent's own log.
fn scheduling_message(raw: &str) -> String {
    let head = raw
        .split(" preemption:")
        .next()
        .unwrap_or(raw)
        .trim()
        .trim_end_matches('.');
    let Some((_, reasons)) = head.split_once("nodes are available: ") else {
        return raw.replace("node(s)", "server(s)");
    };
    let parts: Vec<String> = reasons
        .split(", ")
        .map(|r| match r.split_once(' ') {
            Some((n, what)) if n.parse::<u32>().is_ok() => format!(
                "{} ({n} {})",
                scheduling_reason(what),
                if n == "1" { "server" } else { "servers" }
            ),
            _ => scheduling_reason(r),
        })
        .collect();
    format!("No server can run it: {}.", parts.join(", "))
}

fn scheduling_reason(what: &str) -> String {
    let what = what.trim().trim_end_matches('.');
    if let Some(res) = what.strip_prefix("Insufficient ") {
        return match res {
            "cpu" => "not enough free CPU".into(),
            "memory" => "not enough free memory".into(),
            "ephemeral-storage" => "not enough free disk".into(),
            r if r.contains("gpu") => "no free GPU".into(),
            r => format!("not enough free {r}"),
        };
    }
    let known = [
        (
            "didn't match Pod's node affinity/selector",
            "not a server it may run on",
        ),
        (
            "didn't match pod anti-affinity rules",
            "already runs a replica",
        ),
        (
            "didn't match pod affinity rules",
            "doesn't run what it must run beside",
        ),
        (
            "didn't match pod topology spread constraints",
            "would put too many replicas on one server",
        ),
        ("were unschedulable", "scheduling is paused"),
        ("had untolerated taint", "not taking new work"),
        (
            "had volume node affinity conflict",
            "its volume is on another server",
        ),
        ("didn't have free ports", "its port is already taken"),
        (
            "didn't find available persistent volumes",
            "no volume it can use",
        ),
        (
            "unbound immediate PersistentVolumeClaims",
            "its volume isn't ready yet",
        ),
        ("Too many pods", "too many containers already"),
    ];
    match known.iter().find(|(k, _)| what.contains(k)) {
        Some((_, plain)) => (*plain).into(),
        None => what
            .replace("node(s)", "server(s)")
            .replace("Pod's node", "Pod's server"),
    }
}

#[cfg(test)]
mod scheduling_tests {
    use super::scheduling_message;

    #[test]
    fn scheduler_text_reads_as_servers() {
        assert_eq!(
            scheduling_message(
                "0/3 nodes are available: 1 node(s) had untolerated taint {node.kubernetes.io/unreachable: }, 2 Insufficient memory. preemption: 0/3 nodes are available: 3 No preemption victims found for incoming pod."
            ),
            "No server can run it: not taking new work (1 server), not enough free memory (2 servers)."
        );
        assert_eq!(
            scheduling_message(
                "0/1 nodes are available: pod has unbound immediate PersistentVolumeClaims. preemption: 0/1 nodes are available: 1 Preemption is not helpful for scheduling."
            ),
            "No server can run it: its volume isn't ready yet."
        );
        assert_eq!(
            scheduling_message("2 node(s) were busy"),
            "2 server(s) were busy"
        );
        assert!(
            !scheduling_message(
                "0/2 nodes are available: 2 node(s) didn't match Pod's node affinity/selector."
            )
            .contains("node")
        );
    }
}

#[cfg(test)]
mod autoscale_tests {
    use super::*;
    use pyo_proto::{Autoscale, ServiceRef};

    const A: Autoscale = Autoscale {
        min: 2,
        max: 10,
        target_cpu: 70,
        target_memory: None,
    };

    #[test]
    fn a_rollout_keeps_the_size_the_autoscaler_chose() {
        assert_eq!(autoscaled_replicas(Some(6), &A), 6);
        // New, stopped, or outside new limits.
        assert_eq!(autoscaled_replicas(None, &A), 2);
        assert_eq!(autoscaled_replicas(Some(0), &A), 2);
        assert_eq!(autoscaled_replicas(Some(14), &A), 10);
    }

    #[test]
    fn the_autoscaler_targets_the_deployment_by_cpu() {
        let service = ServiceRef {
            service_id: "svc_1".into(),
            name: "web".into(),
            namespace: "shop".into(),
            workload_kind: "Deployment".into(),
        };
        let m = autoscaler_manifest(&service, &A);
        assert_eq!(m["spec"]["scaleTargetRef"]["name"], "web");
        assert_eq!(m["spec"]["minReplicas"], 2);
        assert_eq!(m["spec"]["maxReplicas"], 10);
        assert_eq!(
            m["spec"]["metrics"][0]["resource"]["target"]["averageUtilization"],
            70
        );
        assert_eq!(
            m["spec"]["metrics"].as_array().unwrap().len(),
            1,
            "memory only when asked for"
        );
        let both = autoscaler_manifest(
            &service,
            &Autoscale {
                target_memory: Some(80),
                ..A
            },
        );
        assert_eq!(both["spec"]["metrics"][1]["resource"]["name"], "memory");
        assert_eq!(
            both["spec"]["metrics"][1]["resource"]["target"]["averageUtilization"],
            80
        );
    }

    /// Needs a cluster: KUBECONFIG=… cargo test -p pyo-agent -- --ignored
    /// `autoscaler_on_a_cluster`. Don't point it at one you care about.
    #[tokio::test]
    #[ignore]
    async fn autoscaler_on_a_cluster() {
        let client = kube::Client::try_default().await.unwrap();
        let ns = "default";
        let service = ServiceRef {
            service_id: "svc_test".into(),
            name: "autoscale-test".into(),
            namespace: ns.into(),
            workload_kind: "Deployment".into(),
        };
        let deployments = Api::<Deployment>::namespaced(client.clone(), ns);
        let hpas = Api::<HorizontalPodAutoscaler>::namespaced(client.clone(), ns);
        let body = json!({
            "apiVersion": "apps/v1", "kind": "Deployment", "metadata": { "name": service.name },
            "spec": { "replicas": 1, "selector": { "matchLabels": { "a": "t" } }, "template": { "metadata": { "labels": { "a": "t" } }, "spec": { "containers": [{ "name": "c", "image": "busybox", "resources": { "requests": { "cpu": "100m" } } }] } } },
        });
        apply(&deployments, &service.name, &body).await.unwrap();

        // On: Kubernetes itself brings it up to the minimum.
        let a = Autoscale {
            min: 3,
            max: 6,
            target_cpu: 70,
            target_memory: None,
        };
        apply_autoscaler(&client, &service, Some(&a)).await.unwrap();
        let mut size = 0;
        for _ in 0..60 {
            size = deployments
                .get(&service.name)
                .await
                .unwrap()
                .spec
                .unwrap()
                .replicas
                .unwrap();
            if size == 3 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        }
        assert_eq!(
            size, 3,
            "the autoscaler should have raised it to the minimum"
        );

        // A rollout applies with force, keeping that size and the autoscaler working.
        let mut body = body;
        body["spec"]["replicas"] = json!(autoscaled_replicas(Some(size), &a));
        apply(&deployments, &service.name, &body).await.unwrap();
        assert_eq!(
            deployments
                .get(&service.name)
                .await
                .unwrap()
                .spec
                .unwrap()
                .replicas,
            Some(3)
        );

        // Changed in place, then off.
        apply_autoscaler(
            &client,
            &service,
            Some(&Autoscale {
                min: 3,
                max: 9,
                target_cpu: 50,
                target_memory: Some(75),
            }),
        )
        .await
        .unwrap();
        assert_eq!(hpas.get(&service.name).await.unwrap().spec.max_replicas, 9);
        apply_autoscaler(&client, &service, None).await.unwrap();
        assert!(hpas.get_opt(&service.name).await.unwrap().is_none());
        apply_autoscaler(&client, &service, None).await.unwrap();
        delete_if_exists(&deployments, &service.name).await.unwrap();
    }
}

#[cfg(test)]
mod cron_tests {
    use super::*;
    use k8s_openapi::api::batch::v1::Job;
    use pyo_proto::{DeploySpec, ServiceRef};

    fn spec() -> DeploySpec {
        let mut d: DeploySpec = serde_json::from_value(json!({
            "deploymentId": "dpl_1", "image": "busybox:1.37", "replicas": 1, "env": [], "ports": [], "domains": [], "volumes": [],
            "service": { "serviceId": "svc_cron", "name": "cleanup", "namespace": "default", "workloadKind": "CronJob" },
            "projectId": "prj_1", "timeoutSeconds": 60,
            "resources": { "cpuRequest": 100, "cpuLimit": 500, "memoryRequest": 64, "memoryLimit": 128 },
            "healthCheck": { "type": "http", "path": "/", "port": 8080, "command": "", "initialDelaySeconds": 1, "periodSeconds": 1 },
        }))
        .unwrap();
        d.schedule = "*/5 * * * *".into();
        d.command = "npm run cleanup".into();
        d
    }

    #[test]
    fn a_cron_pod_runs_once_with_no_probes() {
        let m = cron_manifest(&spec(), "env");
        assert_eq!(m["spec"]["concurrencyPolicy"], "Forbid");
        assert_eq!(m["spec"]["jobTemplate"]["spec"]["backoffLimit"], 0);
        let pod = &m["spec"]["jobTemplate"]["spec"]["template"]["spec"];
        assert_eq!(pod["restartPolicy"], "Never");
        let c = &pod["containers"][0];
        assert_eq!(c["command"], json!(["sh", "-c", "npm run cleanup"]));
        assert!(
            c.get("readinessProbe").is_none()
                && c.get("livenessProbe").is_none()
                && c.get("lifecycle").is_none()
        );
    }

    #[test]
    fn no_command_keeps_the_images_own() {
        let mut d = spec();
        d.command.clear();
        assert!(cron_manifest(&d, "env")["spec"]["jobTemplate"]["spec"]["template"]["spec"]["containers"][0].get("command").is_none());
    }

    /// Needs a cluster: real CronJob and Job controllers.
    #[tokio::test]
    #[ignore]
    async fn cron_on_a_cluster() {
        let client = kube::Client::try_default().await.unwrap();
        let d = spec();
        let service: &ServiceRef = &d.service;
        let crons = Api::<CronJob>::namespaced(client.clone(), "default");
        let jobs = Api::<Job>::namespaced(client.clone(), "default");
        // The API server's own validation of everything Pyo sends.
        apply(&crons, &service.name, &cron_manifest(&d, "env"))
            .await
            .unwrap();
        let live = crons.get(&service.name).await.unwrap().spec;
        assert_eq!(live.schedule, "*/5 * * * *");
        assert_eq!(live.time_zone.as_deref(), Some("Etc/UTC"));
        let name = crate::actions::run_now(&client, service).await.unwrap();
        let job = jobs.get(&name).await.unwrap();
        assert_eq!(
            job.metadata
                .labels
                .as_ref()
                .unwrap()
                .get(SERVICE_LABEL)
                .map(String::as_str),
            Some("svc_cron")
        );
        assert_eq!(
            job.metadata.owner_references.as_ref().unwrap()[0].name,
            "cleanup"
        );
        // Every minute, to watch the real controller start one on its own.
        let mut quick = d.clone();
        quick.schedule = "* * * * *".into();
        apply(&crons, &service.name, &cron_manifest(&quick, "env"))
            .await
            .unwrap();
        crons
            .patch(
                &service.name,
                &PatchParams::default(),
                &Patch::Merge(&json!({ "spec": { "concurrencyPolicy": "Allow" } })),
            )
            .await
            .unwrap();
        let mut started = false;
        for _ in 0..80 {
            let all = jobs
                .list(&ListParams::default().labels(&format!("{SERVICE_LABEL}=svc_cron")))
                .await
                .unwrap()
                .items;
            if all
                .iter()
                .any(|j| j.metadata.name.as_deref() != Some(name.as_str()))
            {
                started = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        }
        assert!(started, "the schedule should have started a job by itself");
        crate::actions::set_suspended(&client, service, true)
            .await
            .unwrap();
        assert_eq!(
            crons.get(&service.name).await.unwrap().spec.suspend,
            Some(true)
        );
        delete_if_exists(&crons, &service.name).await.unwrap();
    }
}

#[cfg(test)]
mod gpu_tests {
    use super::*;
    use k8s_openapi::api::apps::v1::Deployment;
    use pyo_proto::DeploySpec;

    fn spec(gpu: u32) -> DeploySpec {
        serde_json::from_value(json!({
            "deploymentId": "dpl_1", "image": "ollama/ollama:0.12.0", "replicas": 1, "env": [], "ports": [], "domains": [], "volumes": [],
            "service": { "serviceId": "svc_llm", "name": "ollama", "namespace": "ai", "workloadKind": "Deployment" },
            "projectId": "prj_1", "timeoutSeconds": 600, "nodeSelector": { "pyo/tag-fast": "true" },
            "resources": { "cpuRequest": 1000, "cpuLimit": 4000, "memoryRequest": 4096, "memoryLimit": 16384, "gpu": gpu },
            "healthCheck": { "type": "none", "path": "/", "port": 11434, "command": "", "initialDelaySeconds": 1, "periodSeconds": 1 },
        }))
        .unwrap()
    }

    #[test]
    fn a_gpu_service_asks_for_whole_gpus_on_gpu_servers() {
        let t = pod_template(&spec(2), "env");
        let pod = &t["spec"];
        assert_eq!(
            pod["containers"][0]["resources"]["limits"]["nvidia.com/gpu"],
            "2"
        );
        assert_eq!(pod["runtimeClassName"], "nvidia");
        assert_eq!(
            pod["tolerations"],
            json!([{ "key": "nvidia.com/gpu", "operator": "Exists", "effect": "NoSchedule" }])
        );
        // The service's own server choice stays.
        assert_eq!(
            pod["nodeSelector"],
            json!({ "pyo/tag-fast": "true", "pyo/gpu": "nvidia" })
        );
        // And it's a valid Deployment.
        let d: Deployment = serde_json::from_value(json!({ "metadata": { "name": "x" }, "spec": { "selector": {}, "template": t, "strategy": deployment_strategy(false, 2) } })).unwrap();
        let st = d.spec.unwrap().strategy.unwrap().rolling_update.unwrap();
        assert_eq!(
            st.max_surge,
            Some(k8s_openapi::apimachinery::pkg::util::intstr::IntOrString::Int(0))
        );
    }

    #[test]
    fn traffic_stays_close() {
        assert_eq!(distribution_for("local", "1", "36"), Some("PreferSameNode"));
        assert_eq!(
            distribution_for("local", "1", "34+"),
            Some("PreferSameNode")
        );
        assert_eq!(
            distribution_for("local", "1", "32"),
            Some("PreferClose"),
            "the closest there is"
        );
        assert_eq!(distribution_for("local", "1", "29"), None);
        assert_eq!(distribution_for("region", "1", "36"), Some("PreferClose"));
        assert_eq!(distribution_for("region", "1", "31"), Some("PreferClose"));
        assert_eq!(distribution_for("region", "1", "30"), None);
        assert_eq!(distribution_for("spread", "1", "36"), None);
        let spread = service_manifest("web", "ClusterIP", json!({}), "svc_1", vec![], None);
        assert!(spread["spec"].get("trafficDistribution").is_none());
        let s = service_manifest(
            "web",
            "ClusterIP",
            json!({}),
            "svc_1",
            vec![],
            Some("PreferSameNode"),
        );
        assert_eq!(s["spec"]["trafficDistribution"], "PreferSameNode");
        let pod = &pod_template(&spec(0), "env")["spec"];
        assert_eq!(
            pod["topologySpreadConstraints"][0]["topologyKey"],
            "topology.kubernetes.io/zone"
        );
        assert_eq!(
            pod["topologySpreadConstraints"][1]["whenUnsatisfiable"],
            "ScheduleAnyway"
        );
    }

    #[test]
    fn isolated_projects_let_in_their_teams_imports_only() {
        let spec = super::isolation_spec("team_q9pnllqh1p5f");
        let from = spec["ingress"][0]["from"].as_array().unwrap();
        assert_eq!(from.len(), 5);
        let databases = json!({ "kubernetes.io/metadata.name": "pyo-db-q9pnllqh1p5f", "pyo/team-id": "team_q9pnllqh1p5f" });
        // From the team's databases namespace, only import pods: never a whole namespace.
        for f in &from[3..] {
            assert_eq!(f["namespaceSelector"]["matchLabels"], databases);
            assert_eq!(
                f["podSelector"]["matchLabels"].as_object().unwrap().len(),
                1
            );
        }
        assert_eq!(
            from[3]["podSelector"]["matchLabels"],
            json!({ "pyo.dev/component": "import" })
        );
        assert_eq!(
            from[4]["podSelector"]["matchLabels"],
            json!({ "cnpg.io/jobRole": "import" })
        );
        assert!(
            !from
                .iter()
                .any(|f| f.get("namespaceSelector") == Some(&json!({})))
        );
        // Without a team: as before.
        assert_eq!(
            super::isolation_spec("")["ingress"][0]["from"]
                .as_array()
                .unwrap()
                .len(),
            3
        );
    }

    #[test]
    fn no_gpu_no_change() {
        let pod = &pod_template(&spec(0), "env")["spec"];
        assert!(pod.get("runtimeClassName").is_none());
        assert!(
            pod["containers"][0]["resources"]["limits"]
                .get("nvidia.com/gpu")
                .is_none()
        );
        assert_eq!(pod["nodeSelector"], json!({ "pyo/tag-fast": "true" }));
        assert_eq!(
            deployment_strategy(false, 0)["rollingUpdate"]["maxSurge"],
            1
        );
        assert_eq!(deployment_strategy(true, 1)["type"], "Recreate");
    }

    /// The GPU Deployment as the API server takes it (no kubelet needed):
    /// PYO_TEST_KUBECONFIG=<throwaway> cargo test -p pyo-agent -- --ignored gpu_deployment_against
    #[tokio::test]
    #[ignore = "needs a throwaway Kubernetes API server"]
    async fn gpu_deployment_against_a_cluster() {
        let Ok(path) = std::env::var("PYO_TEST_KUBECONFIG") else {
            panic!("set PYO_TEST_KUBECONFIG to a throwaway cluster's kubeconfig")
        };
        let kc = kube::config::Kubeconfig::read_from(&path).unwrap();
        let client = kube::Client::try_from(
            kube::Config::from_custom_kubeconfig(kc, &Default::default())
                .await
                .unwrap(),
        )
        .unwrap();
        let s = spec(1);
        let body = json!({
            "apiVersion": "apps/v1", "kind": "Deployment", "metadata": { "name": "ollama", "namespace": "default" },
            "spec": { "replicas": 1, "strategy": deployment_strategy(false, 1), "selector": { "matchLabels": labels(&s) }, "template": pod_template(&s, "env") },
        });
        let api = Api::<Deployment>::namespaced(client.clone(), "default");
        let d = api
            .patch(
                "ollama",
                &PatchParams::apply("pyo").force(),
                &kube::api::Patch::Apply(&body),
            )
            .await
            .unwrap();
        let pod = d.spec.unwrap().template.spec.unwrap();
        assert_eq!(pod.runtime_class_name.as_deref(), Some("nvidia"));
        assert_eq!(
            pod.tolerations.unwrap()[0].key.as_deref(),
            Some("nvidia.com/gpu")
        );
        assert_eq!(
            pod.containers[0]
                .resources
                .as_ref()
                .unwrap()
                .limits
                .as_ref()
                .unwrap()["nvidia.com/gpu"]
                .0,
            "1"
        );
        api.delete("ollama", &Default::default()).await.unwrap();
    }

    /// A throwaway cluster's client, and a fresh namespace of the test's own
    /// so tests running in parallel don't touch each other's objects.
    async fn test_cluster(ns: &str) -> kube::Client {
        use k8s_openapi::api::core::v1::Namespace;
        use kube::api::{Patch, PatchParams};
        let Ok(path) = std::env::var("PYO_TEST_KUBECONFIG") else {
            panic!("set PYO_TEST_KUBECONFIG to a throwaway cluster's kubeconfig")
        };
        let kc = kube::config::Kubeconfig::read_from(&path).unwrap();
        let client = kube::Client::try_from(
            kube::Config::from_custom_kubeconfig(kc, &Default::default())
                .await
                .unwrap(),
        )
        .unwrap();
        let namespaces: Api<Namespace> = Api::all(client.clone());
        // Left over from an earlier run that stopped halfway: wait it out.
        for _ in 0..120 {
            match namespaces.get_opt(ns).await.unwrap() {
                Some(n) if n.metadata.deletion_timestamp.is_some() => {
                    tokio::time::sleep(Duration::from_millis(500)).await
                }
                _ => break,
            }
        }
        namespaces
            .patch(
                ns,
                &PatchParams::apply("test").force(),
                &Patch::Apply(
                    json!({ "apiVersion": "v1", "kind": "Namespace", "metadata": { "name": ns } }),
                ),
            )
            .await
            .unwrap();
        client
    }

    /// Spreading and region routing hints on a disposable Kubernetes API
    /// server: the scheduler is real, the servers are made by hand (no
    /// kubelet), and pods are marked running with an address.
    /// PYO_TEST_KUBECONFIG=<throwaway> cargo test -p pyo-agent -- --ignored regions_against --nocapture
    #[tokio::test]
    #[ignore = "needs a throwaway Kubernetes API server"]
    async fn regions_against_a_cluster() {
        use k8s_openapi::api::{
            core::v1::{Namespace, Node, Pod, Service},
            discovery::v1::EndpointSlice,
        };
        use kube::api::{DeleteParams, ListParams, Patch, PatchParams};
        let ns = "pyo-test-regions";
        let client = test_cluster(ns).await;
        let nodes: Api<Node> = Api::all(client.clone());
        // Two regions, two servers each, as the agent labels them.
        let servers = [
            ("pyo-test-sg-a", "singapore"),
            ("pyo-test-sg-b", "singapore"),
            ("pyo-test-us-a", "us-east"),
            ("pyo-test-us-b", "us-east"),
        ];
        let ready = |hb: &str| {
            json!({ "status": {
            "conditions": [{ "type": "Ready", "status": "True", "reason": "KubeletReady", "message": "", "lastHeartbeatTime": hb, "lastTransitionTime": hb }],
            "allocatable": { "cpu": "8", "memory": "16Gi", "pods": "110" }, "capacity": { "cpu": "8", "memory": "16Gi", "pods": "110" },
        } })
        };
        let now = || {
            k8s_openapi::jiff::Timestamp::now()
                .strftime("%Y-%m-%dT%H:%M:%SZ")
                .to_string()
        };
        for (n, zone) in servers {
            nodes.patch(n, &PatchParams::apply("test").force(), &Patch::Apply(json!({ "apiVersion": "v1", "kind": "Node", "metadata": { "name": n, "labels": {
                "kubernetes.io/hostname": n, "topology.kubernetes.io/zone": zone, "topology.kubernetes.io/region": "xx" } } }))).await.unwrap();
            nodes
                .patch_status(n, &PatchParams::default(), &Patch::Merge(ready(&now())))
                .await
                .unwrap();
        }
        // The scheduler only spreads over servers it sees as ready and
        // untainted (new ones start with the not-ready taint).
        for _ in 0..60 {
            let mut usable = 0;
            for (n, _) in servers {
                let node = nodes.get(n).await.unwrap();
                if node
                    .spec
                    .and_then(|s| s.taints)
                    .unwrap_or_default()
                    .is_empty()
                {
                    usable += 1;
                }
            }
            if usable == servers.len() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        let mut s = spec(0);
        s.node_selector.clear();
        s.replicas = 4;
        s.service.namespace = ns.into();
        s.service.name = "web-regions".into();
        s.service.service_id = "svc_regions".into();
        let name = s.service.name.clone();
        let pods: Api<Pod> = Api::namespaced(client.clone(), ns);
        let sel = format!("{SERVICE_LABEL}={}", s.service.service_id);
        let gone = DeleteParams {
            grace_period_seconds: Some(0),
            ..Default::default()
        };
        // A run that failed halfway leaves its replicas behind.
        let deployments = Api::<Deployment>::namespaced(client.clone(), ns);
        let sets = Api::<k8s_openapi::api::apps::v1::ReplicaSet>::namespaced(client.clone(), ns);
        for _ in 0..60 {
            let _ = deployments.delete(&name, &Default::default()).await;
            sets.delete_collection(&Default::default(), &ListParams::default().labels(&sel))
                .await
                .unwrap();
            pods.delete_collection(&gone, &ListParams::default().labels(&sel))
                .await
                .unwrap();
            if deployments.get_opt(&name).await.unwrap().is_none()
                && pods
                    .list(&ListParams::default().labels(&sel))
                    .await
                    .unwrap()
                    .items
                    .is_empty()
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        // Only the fake servers: the cluster's own may have no zone.
        let mut template = pod_template(&s, "env");
        template["spec"]["affinity"] = json!({ "nodeAffinity": { "requiredDuringSchedulingIgnoredDuringExecution": { "nodeSelectorTerms": [
            { "matchExpressions": [{ "key": "kubernetes.io/hostname", "operator": "In", "values": servers.map(|(n, _)| n) }] }] } } });
        let deploy = json!({ "apiVersion": "apps/v1", "kind": "Deployment", "metadata": { "name": name, "namespace": ns },
            "spec": { "replicas": 4, "selector": { "matchLabels": labels(&s) }, "template": template, "strategy": deployment_strategy(false, 0) } });
        deployments
            .patch(
                &name,
                &PatchParams::apply("pyo").force(),
                &Patch::Apply(&deploy),
            )
            .await
            .unwrap();
        // Without the managed-by label, so traffic_mode_against_a_cluster's
        // cluster-wide switch (set_traffic_mode) leaves this one alone.
        let mut svc_labels = labels(&s);
        svc_labels
            .as_object_mut()
            .unwrap()
            .remove("app.kubernetes.io/managed-by");
        let distribution = traffic_distribution(&client, pyo_proto::TRAFFIC_REGION).await;
        assert_eq!(
            distribution,
            Some("PreferClose"),
            "region keeps traffic in the zone (Pyo's region)"
        );
        let svc = service_manifest(
            &name,
            "ClusterIP",
            svc_labels,
            &s.service.service_id,
            vec![json!({ "name": "http", "port": 80, "targetPort": 80 })],
            distribution,
        );
        Api::<Service>::namespaced(client.clone(), ns)
            .patch(
                &name,
                &PatchParams::apply("pyo").force(),
                &Patch::Apply(&svc),
            )
            .await
            .unwrap();
        // Wait for the scheduler, then play kubelet.
        let mut placed = vec![];
        for _ in 0..60 {
            let list = pods
                .list(&ListParams::default().labels(&sel))
                .await
                .unwrap()
                .items;
            placed = list
                .iter()
                .filter_map(|p| {
                    p.spec
                        .as_ref()?
                        .node_name
                        .clone()
                        .map(|n| (p.metadata.name.clone().unwrap(), n))
                })
                .collect::<Vec<_>>();
            if placed.len() == 4 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        let mut per_node: std::collections::BTreeMap<String, usize> = Default::default();
        for (_, n) in &placed {
            *per_node.entry(n.clone()).or_default() += 1;
        }
        println!("spread: {per_node:?}");
        assert_eq!(per_node.len(), 4, "one replica per server: {per_node:?}");
        for (i, (pod, _)) in placed.iter().enumerate() {
            let t = now();
            pods.patch_status(pod, &PatchParams::default(), &Patch::Merge(json!({ "status": {
                "phase": "Running", "podIP": format!("10.42.0.{}", i + 10), "podIPs": [{ "ip": format!("10.42.0.{}", i + 10) }],
                "conditions": [{ "type": "Ready", "status": "True", "lastTransitionTime": t }, { "type": "ContainersReady", "status": "True", "lastTransitionTime": t }],
            } }))).await.unwrap();
        }
        let slices: Api<EndpointSlice> = Api::namespaced(client.clone(), ns);
        let mut hinted = vec![];
        for _ in 0..40 {
            let list = slices
                .list(&ListParams::default().labels(&format!("kubernetes.io/service-name={name}")))
                .await
                .unwrap()
                .items;
            hinted = list
                .iter()
                .flat_map(|s| s.endpoints.clone().unwrap_or_default())
                .filter(|e| e.hints.is_some())
                .collect();
            if hinted.len() == 4 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        for e in &hinted {
            let h = e.hints.as_ref().unwrap();
            println!(
                "endpoint on {:?} zone {:?}: hints nodes {:?} zones {:?}",
                e.node_name,
                e.zone,
                h.for_nodes
                    .as_ref()
                    .map(|v| v.iter().map(|x| x.name.clone()).collect::<Vec<_>>()),
                h.for_zones
                    .as_ref()
                    .map(|v| v.iter().map(|x| x.name.clone()).collect::<Vec<_>>())
            );
        }
        assert_eq!(
            hinted.len(),
            4,
            "every endpoint carries its region as a hint"
        );
        for e in &hinted {
            let zones = e
                .hints
                .as_ref()
                .unwrap()
                .for_zones
                .clone()
                .unwrap_or_default();
            assert_eq!(
                zones.iter().map(|z| z.name.clone()).collect::<Vec<_>>(),
                vec![e.zone.clone().unwrap()],
                "hinted for its own region"
            );
        }
        // Clean up: no kubelet finishes the pods, so they go at once.
        deployments
            .delete(&name, &Default::default())
            .await
            .unwrap();
        pods.delete_collection(&gone, &ListParams::default().labels(&sel))
            .await
            .unwrap();
        for (n, _) in servers {
            nodes.delete(n, &Default::default()).await.unwrap();
        }
        Api::<Namespace>::all(client.clone())
            .delete(ns, &Default::default())
            .await
            .unwrap();
    }

    /// Switching a cluster's traffic mode in place, on a throwaway API server:
    /// PYO_TEST_KUBECONFIG=<throwaway> cargo test -p pyo-agent -- --ignored traffic_mode_against
    #[tokio::test]
    #[ignore = "needs a throwaway Kubernetes API server"]
    async fn traffic_mode_against_a_cluster() {
        use k8s_openapi::api::core::v1::{Namespace, Service};
        use kube::api::{Patch, PatchParams};
        let ns = "pyo-test-traffic";
        let client = test_cluster(ns).await;
        let mut s = spec(0);
        s.service.namespace = ns.into();
        s.service.name = "web-traffic".into();
        s.service.service_id = "svc_traffic".into();
        let name = s.service.name.clone();
        let manifest = |d: Option<&str>| {
            service_manifest(
                &name,
                "ClusterIP",
                labels(&s),
                &s.service.service_id,
                vec![json!({ "name": "http", "port": 80, "targetPort": 80 })],
                d,
            )
        };
        let api = Api::<Service>::namespaced(client.clone(), ns);
        api.patch(
            &name,
            &PatchParams::apply("pyo").force(),
            &Patch::Apply(&manifest(
                traffic_distribution(&client, pyo_proto::TRAFFIC_REGION).await,
            )),
        )
        .await
        .unwrap();
        let read = || async {
            api.get(&name)
                .await
                .unwrap()
                .spec
                .unwrap()
                .traffic_distribution
        };
        // Region keeps traffic in the zone (Pyo's region).
        assert_eq!(read().await, Some("PreferClose".into()));
        println!(
            "{}",
            set_traffic_mode(&client, pyo_proto::TRAFFIC_SPREAD)
                .await
                .unwrap()
        );
        assert_eq!(read().await, None);
        println!(
            "{}",
            set_traffic_mode(&client, pyo_proto::TRAFFIC_REGION)
                .await
                .unwrap()
        );
        assert_eq!(read().await, Some("PreferClose".into()));
        // Local keeps it on the server, where Kubernetes can (1.34+).
        println!(
            "{}",
            set_traffic_mode(&client, pyo_proto::TRAFFIC_LOCAL)
                .await
                .unwrap()
        );
        let v = client.apiserver_version().await.unwrap();
        let local = if v.minor.trim_end_matches('+').parse::<u32>().unwrap() >= 34 {
            "PreferSameNode"
        } else {
            "PreferClose"
        };
        assert_eq!(read().await, Some(local.into()));
        // A later deploy in spread mode (agent offline when it was switched)
        // still ends up spread.
        api.patch(
            &name,
            &PatchParams::apply("pyo").force(),
            &Patch::Apply(&manifest(None)),
        )
        .await
        .unwrap();
        clear_distribution(&api, &name, None).await.unwrap();
        assert_eq!(read().await, None);
        api.delete(&name, &Default::default()).await.unwrap();
        Api::<Namespace>::all(client.clone())
            .delete(ns, &Default::default())
            .await
            .unwrap();
    }
}

#[cfg(test)]
mod claim_tests {
    use super::*;

    fn claim(volume_id: &str, deleting: bool) -> PersistentVolumeClaim {
        let mut c: PersistentVolumeClaim = serde_json::from_value(
            json!({ "metadata": { "name": "data", "labels": { VOLUME_LABEL: volume_id } } }),
        )
        .unwrap();
        if deleting {
            c.metadata.deletion_timestamp =
                Some(k8s_openapi::apimachinery::pkg::apis::meta::v1::Time(
                    k8s_openapi::jiff::Timestamp::now(),
                ));
        }
        c
    }

    #[test]
    fn only_the_volumes_own_live_claim_is_kept() {
        assert!(claim_is(&claim("v1", false), "v1"));
        assert!(
            !claim_is(&claim("v1", true), "v1"),
            "a claim on its way out is waited out"
        );
        assert!(
            !claim_is(&claim("v0", false), "v1"),
            "a deleted volume's claim of the same name isn't this one"
        );
        assert!(!claim_is(&PersistentVolumeClaim::default(), "v1"));
    }

    /// A claim held by a finalizer while being deleted is waited out; one of
    /// another volume that isn't going away fails with a clear message.
    /// PYO_TEST_KUBECONFIG=<throwaway> cargo test -p pyo-agent -- --ignored old_claim --nocapture
    #[tokio::test]
    #[ignore = "needs a throwaway Kubernetes API server"]
    async fn old_claim_against_a_cluster() {
        let Ok(path) = std::env::var("PYO_TEST_KUBECONFIG") else {
            panic!("set PYO_TEST_KUBECONFIG to a throwaway cluster's kubeconfig")
        };
        let kc = kube::config::Kubeconfig::read_from(&path).unwrap();
        let client = kube::Client::try_from(
            kube::Config::from_custom_kubeconfig(kc, &Default::default())
                .await
                .unwrap(),
        )
        .unwrap();
        let ns = "pyo-test-old-claim";
        let namespaces: Api<Namespace> = Api::all(client.clone());
        apply(
            &namespaces,
            ns,
            &json!({ "apiVersion": "v1", "kind": "Namespace", "metadata": { "name": ns } }),
        )
        .await
        .unwrap();
        let pvcs: Api<PersistentVolumeClaim> = Api::namespaced(client.clone(), ns);
        let body = |id: &str, finalizers: Value| {
            json!({
                "apiVersion": "v1", "kind": "PersistentVolumeClaim",
                "metadata": { "name": "data", "labels": { VOLUME_LABEL: id }, "finalizers": finalizers },
                "spec": { "accessModes": ["ReadWriteOnce"], "resources": { "requests": { "storage": "1Gi" } } },
            })
        };

        // Another volume's claim that stays: a clash, reported after the wait.
        apply(&pvcs, "data", &body("old", json!([]))).await.unwrap();
        let old = pvcs.get("data").await.unwrap();
        assert!(!claim_is(&old, "new"));
        let err = wait_claim_gone(&pvcs, &old, Duration::from_secs(3))
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("belongs to another volume"),
            "{err}"
        );

        // Being deleted but held (as a pod would hold it): waited out.
        apply(&pvcs, "data", &body("old", json!(["test.pyo/hold"])))
            .await
            .unwrap();
        pvcs.delete("data", &DeleteParams::default()).await.unwrap();
        let old = pvcs.get("data").await.unwrap();
        assert!(old.metadata.deletion_timestamp.is_some() && !claim_is(&old, "old"));
        let release = {
            let pvcs = pvcs.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_secs(3)).await;
                pvcs.patch(
                    "data",
                    &PatchParams::default(),
                    &Patch::Merge(json!({ "metadata": { "finalizers": null } })),
                )
                .await
                .unwrap();
            })
        };
        let started = tokio::time::Instant::now();
        wait_claim_gone(&pvcs, &old, Duration::from_secs(30))
            .await
            .unwrap();
        assert!(started.elapsed() >= Duration::from_secs(2));
        release.await.unwrap();
        assert!(pvcs.get_opt("data").await.unwrap().is_none());
        namespaces
            .delete(ns, &DeleteParams::default())
            .await
            .unwrap();
    }
}

#[cfg(test)]
mod stage_tests {
    use super::*;

    fn pod(scheduled: bool, container: Value) -> Pod {
        serde_json::from_value(json!({
            "metadata": { "name": "web-1" },
            "status": {
                "conditions": [{ "type": "PodScheduled", "status": if scheduled { "True" } else { "False" } }],
                "containerStatuses": [container],
            },
        }))
        .unwrap()
    }

    fn container(state: Value, ready: bool, started: bool) -> Value {
        json!({ "name": "web", "image": "x", "imageID": "", "restartCount": 0, "ready": ready, "started": started, "state": state })
    }

    #[test]
    fn a_running_pod_waits_for_its_health_check() {
        assert_eq!(pod_stage(&[]), "scheduling");
        assert_eq!(
            pod_stage(&[pod(
                false,
                container(json!({ "waiting": { "reason": "Pending" } }), false, false)
            )]),
            "scheduling"
        );
        assert_eq!(
            pod_stage(&[pod(
                true,
                container(
                    json!({ "waiting": { "reason": "ContainerCreating" } }),
                    false,
                    false
                )
            )]),
            "pulling"
        );
        let running = json!({ "running": { "startedAt": "2026-11-11T10:00:00Z" } });
        assert_eq!(
            pod_stage(&[pod(true, container(running.clone(), false, false))]),
            "starting",
            "its startup probe hasn't passed"
        );
        assert_eq!(
            pod_stage(&[pod(true, container(running.clone(), false, true))]),
            "waiting_ready",
            "started, not ready yet"
        );
        assert_eq!(
            pod_stage(&[pod(true, container(running, true, true))]),
            "waiting_ready"
        );
    }
}

#[cfg(test)]
mod public_tests {
    use super::*;
    use std::collections::BTreeMap;

    fn node_port_svc() -> Value {
        let ports = vec![
            json!({ "name": "game", "port": 7777, "targetPort": "game", "protocol": "UDP", "nodePort": 30777 }),
            json!({ "name": "admin", "port": 8080, "targetPort": "admin", "protocol": "TCP" }),
        ];
        service_manifest("web", "NodePort", json!({}), "svc1", ports, None)
    }

    fn direct_svc() -> Value {
        let mut s = service_manifest(
            "pyo-public-web",
            "LoadBalancer",
            json!({}),
            "svc1",
            vec![json!({ "name": "tcp", "port": 5432, "targetPort": "tcp", "protocol": "TCP" })],
            None,
        );
        s["spec"]["externalTrafficPolicy"] = json!("Local");
        s
    }

    fn published(s: &Value) -> Published {
        serde_json::from_str(
            s["metadata"]["annotations"][PUBLIC_ANNOTATION]
                .as_str()
                .unwrap(),
        )
        .unwrap()
    }

    #[test]
    fn on_publishes_as_written() {
        let known = BTreeMap::from([("admin".to_owned(), 31000), ("gone".to_owned(), 31001)]);
        let mut s = node_port_svc();
        publish(&mut s, true, &known);
        assert_eq!(s["spec"]["type"], "NodePort");
        assert_eq!(s["spec"]["ports"][0]["nodePort"], 30777);
        assert_eq!(
            s["spec"]["ports"][1]["nodePort"], 31000,
            "keeps the server port it had"
        );
        assert_eq!(
            published(&s).node_ports,
            BTreeMap::from([("game".to_owned(), 30777), ("admin".to_owned(), 31000)])
        );

        let mut d = direct_svc();
        publish(&mut d, true, &BTreeMap::new());
        assert_eq!(d["spec"]["type"], "LoadBalancer");
        assert_eq!(d["spec"]["externalTrafficPolicy"], "Local");
        assert_eq!(
            published(&d),
            Published {
                kind: "LoadBalancer".into(),
                external_traffic_policy: Some("Local".into()),
                node_ports: BTreeMap::new()
            }
        );
    }

    #[test]
    fn off_keeps_them_inside_the_cluster() {
        let mut s = node_port_svc();
        publish(&mut s, false, &BTreeMap::new());
        assert_eq!(s["spec"]["type"], "ClusterIP");
        assert!(
            s["spec"]["ports"]
                .as_array()
                .unwrap()
                .iter()
                .all(|p| p.get("nodePort").is_none())
        );
        assert_eq!(
            s["spec"]["ports"][0]["port"], 7777,
            "still reachable inside the cluster"
        );
        assert_eq!(published(&s).kind, "NodePort");
        assert_eq!(published(&s).node_ports["game"], 30777);

        let mut d = direct_svc();
        publish(&mut d, false, &BTreeMap::new());
        assert_eq!(d["spec"]["type"], "ClusterIP");
        assert!(d["spec"].get("externalTrafficPolicy").is_none());
        assert_eq!(published(&d).kind, "LoadBalancer");
    }

    #[test]
    fn a_cluster_ip_has_nothing_to_publish() {
        let mut s = service_manifest(
            "web",
            "ClusterIP",
            json!({}),
            "svc1",
            vec![json!({ "name": "http", "port": 80 })],
            None,
        );
        let before = s.clone();
        publish(&mut s, false, &BTreeMap::new());
        assert_eq!(s, before);
    }

    fn live(v: &Value) -> Service {
        serde_json::from_value(v.clone()).unwrap()
    }

    #[test]
    fn toggling_patches_only_what_differs() {
        // Live and published: off conceals it, keeping its server ports.
        let mut on = node_port_svc();
        publish(&mut on, true, &BTreeMap::new());
        on["spec"]["ports"][1]["nodePort"] = json!(31555);
        let svc = live(&on);
        assert!(public_patch(&svc, true, true).is_none());
        let off = public_patch(&svc, false, true).unwrap();
        assert_eq!(off["spec"]["type"], "ClusterIP");
        assert!(off["spec"]["externalTrafficPolicy"].is_null());
        assert!(
            off["spec"]["ports"]
                .as_array()
                .unwrap()
                .iter()
                .all(|p| p.get("nodePort").is_none())
        );
        let kept: Published = serde_json::from_str(
            off["metadata"]["annotations"][PUBLIC_ANNOTATION]
                .as_str()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(
            kept.node_ports["admin"], 31555,
            "an allocated server port is remembered"
        );

        // Concealed: on restores it as it was.
        let mut concealed = on.clone();
        concealed["metadata"]["annotations"][PUBLIC_ANNOTATION] =
            off["metadata"]["annotations"][PUBLIC_ANNOTATION].clone();
        conceal(&mut concealed["spec"]);
        let svc = live(&concealed);
        assert!(public_patch(&svc, false, true).is_none());
        let back = public_patch(&svc, true, true).unwrap();
        assert_eq!(back["spec"]["type"], "NodePort");
        assert_eq!(back["spec"]["ports"][0]["nodePort"], 30777);
        assert_eq!(back["spec"]["ports"][1]["nodePort"], 31555);
        let fresh = public_patch(&svc, true, false).unwrap();
        assert!(
            fresh["spec"]["ports"]
                .as_array()
                .unwrap()
                .iter()
                .all(|p| p.get("nodePort").is_none()),
            "new server ports when the old are taken"
        );

        // A direct port's Service gets its traffic policy back.
        let mut d = direct_svc();
        publish(&mut d, false, &BTreeMap::new());
        let back = public_patch(&live(&d), true, true).unwrap();
        assert_eq!(back["spec"]["type"], "LoadBalancer");
        assert_eq!(back["spec"]["externalTrafficPolicy"], "Local");

        // Not Pyo's to change.
        assert!(
            public_patch(
                &live(&service_manifest(
                    "x",
                    "NodePort",
                    json!({}),
                    "s",
                    vec![],
                    None
                )),
                false,
                true
            )
            .is_none()
        );
    }
}

/// Against a throwaway cluster (KUBECONFIG): PYO_PUBLIC=deploy-on or
/// deploy-off applies a service's Services as a deploy does; on or off
/// toggles the cluster's public traffic. Reachability is checked outside.
#[cfg(test)]
mod public_cluster_tests {
    use super::*;

    #[tokio::test]
    #[ignore = "needs a throwaway Kubernetes cluster"]
    async fn public_ports_follow_the_setting() {
        let client = kube::Client::try_default().await.unwrap();
        let step = std::env::var("PYO_PUBLIC").unwrap();
        let ns = "pyo-public-test";
        let labels = json!({ "app.kubernetes.io/managed-by": MANAGER, SERVICE_LABEL: "svc1" });
        match step.as_str() {
            "deploy-on" | "deploy-off" => {
                let on = step == "deploy-on";
                let services = Api::<Service>::namespaced(client.clone(), ns);
                let ports = vec![
                    json!({ "name": "web", "port": 8080, "targetPort": "web", "protocol": "TCP", "nodePort": 30880 }),
                    json!({ "name": "auto", "port": 8082, "targetPort": "web", "protocol": "TCP" }),
                    json!({ "name": "tcp", "port": 8081, "targetPort": "tcp", "protocol": "TCP" }),
                ];
                apply_service(
                    &services,
                    "web",
                    service_manifest("web", "NodePort", labels.clone(), "svc1", ports, None),
                    on,
                )
                .await
                .unwrap();
                let mut direct = service_manifest(
                    "pyo-public-web",
                    "LoadBalancer",
                    labels,
                    "svc1",
                    vec![
                        json!({ "name": "tcp", "port": 18081, "targetPort": "tcp", "protocol": "TCP" }),
                    ],
                    None,
                );
                direct["spec"]["externalTrafficPolicy"] = json!("Local");
                apply_service(&services, "pyo-public-web", direct, on)
                    .await
                    .unwrap();
                set_isolation(&client, ns, false).await.unwrap();
            }
            "on" | "off" => set_public_ports(&client, step == "on").await.unwrap(),
            other => panic!("{other}"),
        }
    }
}
