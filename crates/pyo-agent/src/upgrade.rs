//! Updating Pyo from the dashboard.
//!
//! On the cluster Pyo runs on, `UpgradePyo` makes this agent run the new
//! release's upgrader: a Job in pyo-system, from the new agent image (pinned
//! by digest), with a ServiceAccount that may only change pyo-system. The
//! upgrader (`pyo-agent upgrade <release.json>`) does what
//! `scripts/pyo-up.sh --release` does: makes sure the generated secrets
//! exist, applies platform.yaml then pyo.yaml (server-side, as pyo-up.sh
//! does), and waits for every rollout. Pyo's Deployments roll with
//! maxUnavailable 0, so it keeps serving throughout. If anything doesn't
//! become ready, it puts back the pod templates it found and waits for
//! those: the old version keeps running.
//!
//! `UpdateAgent` moves an agent to a new image: it first runs the image once
//! (`pyo-agent version`) to be sure this cluster can pull and start it, then
//! changes its own Deployment and restarts on it.

use std::time::{Duration, Instant};

use k8s_openapi::api::{
    apps::v1::{Deployment, StatefulSet},
    batch::v1::Job,
    core::v1::{Pod, Secret},
};
use kube::{
    Api, Client,
    api::{DeleteParams, DynamicObject, ListParams, LogParams, Patch, PatchParams, PostParams},
    core::{ApiResource, GroupVersionKind},
};
use pyo_proto::{CommandResult, Release, UpgradeSpec};
use serde_json::{Value, json};

use crate::{Agent, MANAGER, failed};

pub const NS: &str = "pyo-system";
/// Field manager of Pyo's own objects, shared with scripts/pyo-up.sh
/// (`kubectl apply --server-side --field-manager=pyo-install`).
pub const INSTALL_MANAGER: &str = "pyo-install";
const UPGRADER: &str = "pyo-upgrade";
/// The line the upgrader ends its log with, read back by the agent.
const OUTCOME: &str = "pyoUpgradeOutcome";
/// How long a rollout may take before it counts as failed
/// (PYO_UPGRADE_ROLLOUT_SECS changes it, for tests).
fn rollout_timeout() -> Duration {
    Duration::from_secs(
        std::env::var("PYO_UPGRADE_ROLLOUT_SECS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(600),
    )
}
/// A container that can't pull or keeps crashing this long has failed.
const STUCK: Duration = Duration::from_secs(150);

/// Kinds a release may hold, with their API paths. Anything else is refused
/// before anything is applied (and the upgrader's role allows only these).
const KINDS: &[(&str, &str, &str, &str)] = &[
    ("", "v1", "Service", "services"),
    ("", "v1", "ConfigMap", "configmaps"),
    ("", "v1", "PersistentVolumeClaim", "persistentvolumeclaims"),
    ("", "v1", "ServiceAccount", "serviceaccounts"),
    ("apps", "v1", "Deployment", "deployments"),
    ("apps", "v1", "StatefulSet", "statefulsets"),
    (
        "policy",
        "v1",
        "PodDisruptionBudget",
        "poddisruptionbudgets",
    ),
    (
        "networking.k8s.io",
        "v1",
        "NetworkPolicy",
        "networkpolicies",
    ),
];

fn resource(o: &Value) -> anyhow::Result<ApiResource> {
    let api_version = o["apiVersion"].as_str().unwrap_or("");
    let kind = o["kind"].as_str().unwrap_or("");
    let (group, version) = api_version.split_once('/').unwrap_or(("", api_version));
    let (_, _, _, plural) = KINDS
        .iter()
        .find(|(g, v, k, _)| *g == group && *v == version && *k == kind)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "the release has a {api_version} {kind}, which the upgrader can't apply"
            )
        })?;
    Ok(ApiResource::from_gvk_with_plural(
        &GroupVersionKind::gvk(group, version, kind),
        plural,
    ))
}

fn job_name(update_id: &str) -> String {
    let id: String = update_id
        .to_ascii_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    format!("{UPGRADER}-{id}")
        .chars()
        .take(52)
        .collect::<String>()
        .trim_end_matches('-')
        .to_owned()
}

/// The upgrader's ServiceAccount, Role and RoleBinding: only pyo-system,
/// only what a release holds, and Secrets it may create but never list or
/// change.
pub fn upgrader_rbac(labels: &Value) -> [Value; 3] {
    [
        json!({ "apiVersion": "v1", "kind": "ServiceAccount", "metadata": { "name": UPGRADER, "namespace": NS, "labels": labels } }),
        json!({
            "apiVersion": "rbac.authorization.k8s.io/v1", "kind": "Role",
            "metadata": { "name": UPGRADER, "namespace": NS, "labels": labels },
            "rules": [
                { "apiGroups": ["apps"], "resources": ["deployments", "statefulsets"], "verbs": ["get", "list", "watch", "create", "patch"] },
                { "apiGroups": [""], "resources": ["services", "configmaps", "persistentvolumeclaims", "serviceaccounts"], "verbs": ["get", "create", "patch"] },
                { "apiGroups": [""], "resources": ["secrets"], "verbs": ["get", "create"] },
                { "apiGroups": [""], "resources": ["pods"], "verbs": ["get", "list", "delete"] },
                { "apiGroups": ["policy"], "resources": ["poddisruptionbudgets"], "verbs": ["get", "create", "patch"] },
                { "apiGroups": ["networking.k8s.io"], "resources": ["networkpolicies"], "verbs": ["get", "create", "patch"] },
            ],
        }),
        json!({
            "apiVersion": "rbac.authorization.k8s.io/v1", "kind": "RoleBinding",
            "metadata": { "name": UPGRADER, "namespace": NS, "labels": labels },
            "roleRef": { "apiGroup": "rbac.authorization.k8s.io", "kind": "Role", "name": UPGRADER },
            "subjects": [{ "kind": "ServiceAccount", "name": UPGRADER, "namespace": NS }],
        }),
    ]
}

// ── In the agent: run the upgrader and report ──────────────────────────────

pub async fn start(agent: &Agent, spec: UpgradeSpec) -> CommandResult {
    if !agent.api.same_cluster() {
        return failed("Only the agent on the cluster Pyo runs on can update Pyo.");
    }
    let problems = spec.release.problems();
    if !problems.is_empty() {
        return failed(format!(
            "Pyo {} can't be applied: {}",
            spec.release.version,
            problems.join(" ")
        ));
    }
    if !pyo_proto::pinned_image(&spec.image) {
        return failed("The upgrader's image isn't pinned by digest.");
    }
    match run_upgrader(&agent.kube, &spec).await {
        Ok(outcome) => CommandResult {
            ok: outcome["ok"].as_bool().unwrap_or(false),
            message: outcome["message"].as_str().map(str::to_owned),
            logs: vec![],
            data: Some(outcome),
        },
        Err(e) => failed(format!("{e:#}")),
    }
}

async fn run_upgrader(client: &Client, spec: &UpgradeSpec) -> anyhow::Result<Value> {
    let name = job_name(&spec.update_id);
    let pp = PatchParams::apply(MANAGER).force();
    let labels = json!({ "app.kubernetes.io/managed-by": MANAGER, "app.kubernetes.io/name": UPGRADER, "pyo/update-id": spec.update_id });

    let objects = upgrader_rbac(&labels);
    for o in &objects {
        let (group, version) = o["apiVersion"]
            .as_str()
            .unwrap_or("")
            .split_once('/')
            .unwrap_or(("", "v1"));
        let kind = o["kind"].as_str().unwrap_or("");
        let plural = match kind {
            "ServiceAccount" => "serviceaccounts",
            "Role" => "roles",
            _ => "rolebindings",
        };
        let ar =
            ApiResource::from_gvk_with_plural(&GroupVersionKind::gvk(group, version, kind), plural);
        Api::<DynamicObject>::namespaced_with(client.clone(), NS, &ar)
            .patch(UPGRADER, &pp, &Patch::Apply(o))
            .await?;
    }
    // A Secret, though the release isn't one: the agent has no ConfigMaps
    // to write (rbac.rs).
    let cms = Api::<Secret>::namespaced(client.clone(), NS);
    cms.patch(
        &name,
        &pp,
        &Patch::Apply(json!({
            "apiVersion": "v1", "kind": "Secret",
            "metadata": { "name": name, "labels": labels },
            "stringData": { "release.json": serde_json::to_string(&spec.release)? },
        })),
    )
    .await?;

    let job = json!({
        "apiVersion": "batch/v1", "kind": "Job",
        "metadata": { "name": name, "labels": labels },
        "spec": {
            "backoffLimit": 0,
            "activeDeadlineSeconds": 1800,
            "ttlSecondsAfterFinished": 86_400,
            "template": {
                "metadata": { "labels": labels },
                "spec": {
                    "serviceAccountName": UPGRADER,
                    "restartPolicy": "Never",
                    "containers": [{
                        "name": "upgrade",
                        "image": spec.image,
                        "args": ["upgrade", "/release/release.json"],
                        "env": [{ "name": "RUST_LOG", "value": "pyo_agent=info" }],
                        "volumeMounts": [{ "name": "release", "mountPath": "/release", "readOnly": true }],
                        "resources": { "requests": { "cpu": "20m", "memory": "32Mi" }, "limits": { "memory": "256Mi" } },
                        "securityContext": { "runAsNonRoot": true, "runAsUser": 65532, "allowPrivilegeEscalation": false, "readOnlyRootFilesystem": true, "capabilities": { "drop": ["ALL"] } },
                    }],
                    "volumes": [{ "name": "release", "secret": { "secretName": name } }],
                },
            },
        },
    });
    let jobs = Api::<Job>::namespaced(client.clone(), NS);
    // The same update again (it was retried): start over.
    if jobs.get_opt(&name).await?.is_some() {
        jobs.delete(&name, &DeleteParams::background()).await?;
        for _ in 0..30 {
            if jobs.get_opt(&name).await?.is_none() {
                break;
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    }
    jobs.create(&PostParams::default(), &serde_json::from_value(job)?)
        .await?;
    tracing::info!(job = %name, version = %spec.release.version, "updating Pyo");

    let pods = Api::<Pod>::namespaced(client.clone(), NS);
    let selector = format!("pyo/update-id={}", spec.update_id);
    let started = Instant::now();
    let mut stuck_since: Option<Instant> = None;
    let finished = loop {
        tokio::time::sleep(Duration::from_secs(5)).await;
        if let Some(j) = jobs.get_opt(&name).await? {
            let st = j.status.unwrap_or_default();
            if st.succeeded.unwrap_or(0) > 0 || st.failed.unwrap_or(0) > 0 {
                break true;
            }
        }
        // The new image can't be pulled or started: nothing has changed yet.
        match stuck_reason(&pods, &selector).await {
            Some(why) if stuck_since.get_or_insert_with(Instant::now).elapsed() > STUCK => {
                let _ = jobs.delete(&name, &DeleteParams::background()).await;
                let _ = cms.delete(&name, &DeleteParams::default()).await;
                anyhow::bail!(
                    "The upgrader couldn't start ({why}), so nothing was changed. Can this cluster pull {}?",
                    spec.image
                );
            }
            Some(_) => {}
            None => stuck_since = None,
        }
        if started.elapsed() > Duration::from_secs(1900) {
            break false;
        }
    };
    let logs = job_logs(&pods, &selector).await;
    let _ = cms.delete(&name, &DeleteParams::default()).await;
    if let Some(outcome) = logs
        .lines()
        .rev()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .find_map(|v| v.get(OUTCOME).cloned())
    {
        return Ok(outcome);
    }
    let tail: Vec<&str> = logs.lines().filter(|l| !l.trim().is_empty()).collect();
    let tail = tail[tail.len().saturating_sub(5)..].join(" | ");
    Ok(json!({
        "ok": false,
        "message": if finished { format!("The upgrader stopped without saying how it went. Its last lines: {tail}") } else { format!("The upgrader didn't finish in time (job {NS}/{name}). Its last lines: {tail}") },
    }))
}

/// A container of these pods that can't be pulled or keeps crashing.
async fn stuck_reason(pods: &Api<Pod>, selector: &str) -> Option<String> {
    let list = pods
        .list(&ListParams::default().labels(selector))
        .await
        .ok()?;
    for p in list.items {
        for cs in p
            .status
            .and_then(|s| s.container_statuses)
            .unwrap_or_default()
        {
            let waiting = cs.state.and_then(|s| s.waiting);
            if let Some(w) = waiting
                && let Some(reason) = w.reason.filter(|r| {
                    matches!(
                        r.as_str(),
                        "ErrImagePull"
                            | "ImagePullBackOff"
                            | "InvalidImageName"
                            | "CrashLoopBackOff"
                            | "CreateContainerConfigError"
                            | "CreateContainerError"
                    )
                })
            {
                return Some(match w.message {
                    Some(m) if !m.is_empty() => {
                        format!("{reason}: {}", m.chars().take(300).collect::<String>())
                    }
                    _ => reason,
                });
            }
        }
    }
    None
}

async fn job_logs(pods: &Api<Pod>, selector: &str) -> String {
    let Ok(list) = pods.list(&ListParams::default().labels(selector)).await else {
        return String::new();
    };
    let mut out = String::new();
    for p in list.items {
        if let Some(name) = p.metadata.name
            && let Ok(l) = pods
                .logs(
                    &name,
                    &LogParams {
                        tail_lines: Some(400),
                        ..Default::default()
                    },
                )
                .await
        {
            out.push_str(&l);
        }
    }
    out
}

// ── In the agent: update itself ────────────────────────────────────────────

/// Check that the new image runs here, then switch this agent's Deployment
/// to it. Returns once the change is checked; `switch` makes it.
pub async fn check_agent_image(client: &Client, image: &str) -> anyhow::Result<String> {
    if !pyo_proto::pinned_image(image) {
        anyhow::bail!("{image} isn't pinned by digest");
    }
    let deployments = Api::<Deployment>::namespaced(client.clone(), NS);
    let me = deployments.get_opt("pyo-agent").await?.ok_or_else(|| {
        anyhow::anyhow!("this agent isn't the {NS}/pyo-agent Deployment, so it can't update itself")
    })?;
    let spec = me.spec.and_then(|s| s.template.spec).unwrap_or_default();
    if !spec.containers.iter().any(|c| c.name == "agent") {
        anyhow::bail!("{NS}/pyo-agent has no container named agent");
    }
    // Run it once: proves the cluster can pull and start it, and says its version.
    let digest = image.rsplit_once("@sha256:").map_or("", |(_, d)| d);
    let name = format!("pyo-agent-check-{}", &digest[..12]);
    let pods = Api::<Pod>::namespaced(client.clone(), NS);
    let _ = pods.delete(&name, &DeleteParams::default()).await;
    for _ in 0..30 {
        if pods.get_opt(&name).await?.is_none() {
            break;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    let pod = json!({
        "apiVersion": "v1", "kind": "Pod",
        "metadata": { "name": name, "labels": { "app.kubernetes.io/managed-by": MANAGER, "pyo/agent-check": "true" } },
        "spec": {
            "restartPolicy": "Never",
            "automountServiceAccountToken": false,
            "containers": [{
                "name": "check", "image": image, "args": ["version"],
                "resources": { "requests": { "cpu": "10m", "memory": "16Mi" }, "limits": { "memory": "64Mi" } },
                "securityContext": { "runAsNonRoot": true, "runAsUser": 65532, "allowPrivilegeEscalation": false, "readOnlyRootFilesystem": true, "capabilities": { "drop": ["ALL"] } },
            }],
        },
    });
    pods.create(&PostParams::default(), &serde_json::from_value(pod)?)
        .await?;
    let started = Instant::now();
    let outcome = loop {
        tokio::time::sleep(Duration::from_secs(3)).await;
        let p = pods.get(&name).await?;
        match p.status.as_ref().and_then(|s| s.phase.as_deref()) {
            Some("Succeeded") => {
                break Ok(pods
                    .logs(&name, &LogParams::default())
                    .await
                    .unwrap_or_default()
                    .trim()
                    .to_owned());
            }
            Some("Failed") => {
                break Err(anyhow::anyhow!("the new agent image exited with an error"));
            }
            _ => {}
        }
        let waiting = p
            .status
            .and_then(|s| s.container_statuses)
            .unwrap_or_default()
            .into_iter()
            .find_map(|c| c.state.and_then(|s| s.waiting))
            .and_then(|w| w.reason);
        if started.elapsed() > STUCK {
            break Err(anyhow::anyhow!(
                "the new agent image didn't start ({}), so this agent stays as it is",
                waiting.unwrap_or_else(|| "still waiting".into())
            ));
        }
    };
    let _ = pods.delete(&name, &DeleteParams::default()).await;
    let version = outcome?;
    // Checked by the API server without changing anything.
    deployments
        .patch(
            "pyo-agent",
            &PatchParams {
                dry_run: true,
                ..Default::default()
            },
            &Patch::Strategic(agent_image_patch(image)),
        )
        .await?;
    Ok(version)
}

fn agent_image_patch(image: &str) -> Value {
    json!({ "spec": { "template": { "spec": { "containers": [{ "name": "agent", "image": image }] } } } })
}

/// Restart this agent on `image`. Its pod stops soon after.
pub async fn switch(client: &Client, image: &str) -> anyhow::Result<()> {
    Api::<Deployment>::namespaced(client.clone(), NS)
        .patch(
            "pyo-agent",
            &PatchParams::default(),
            &Patch::Strategic(agent_image_patch(image)),
        )
        .await?;
    Ok(())
}

// ── In the Job: the upgrader ───────────────────────────────────────────────

/// `pyo-agent check-release <release.json>`: fails with what the upgrader
/// would refuse in it.
pub fn check_release(path: &str) -> anyhow::Result<()> {
    let release: Release = serde_json::from_slice(&std::fs::read(path)?)?;
    let mut problems = release.problems();
    problems.extend(access_problem(&release));
    if !problems.is_empty() {
        anyhow::bail!(
            "Pyo {} can't be applied: {}",
            release.version,
            problems.join(" ")
        );
    }
    println!("Pyo {} can be applied.", release.version);
    Ok(())
}

/// A release must say which roles its agent has (this agent's: it's the
/// release's own), so installations on other roles are told to run the
/// connect command after updating.
fn access_problem(release: &Release) -> Option<String> {
    let ours = pyo_proto::rbac::access_hash();
    match release.agent_access.as_deref() {
        Some(h) if h == ours => None,
        Some(h) => Some(format!(
            "agentAccess is {h}, but this release's agent has {ours} (pyo-agent access-hash)."
        )),
        None => Some(format!(
            "It doesn't say which roles its agent has: agentAccess {ours} (pyo-agent access-hash)."
        )),
    }
}

/// `pyo-agent upgrade <release.json>`. Ends its output with one JSON line
/// `{"pyoUpgradeOutcome": {"ok", "message", "rolledBack"}}`.
pub async fn run_job(path: &str) -> anyhow::Result<()> {
    let client = Client::try_default().await?;
    let release: Release = serde_json::from_slice(&std::fs::read(path)?)?;
    let outcome = match upgrade(&client, &release).await {
        Ok(()) => {
            json!({ "ok": true, "message": format!("Pyo is on {}.", release.version), "rolledBack": false })
        }
        Err(Failure {
            message,
            rolled_back,
        }) => json!({ "ok": false, "message": message, "rolledBack": rolled_back }),
    };
    println!("{}", json!({ OUTCOME: outcome }));
    if outcome["ok"] == json!(true) {
        Ok(())
    } else {
        std::process::exit(1)
    }
}

#[derive(Debug)]
pub struct Failure {
    pub message: String,
    pub rolled_back: bool,
}

impl<E: std::fmt::Display> From<E> for Failure {
    fn from(e: E) -> Self {
        Failure {
            message: e.to_string(),
            rolled_back: false,
        }
    }
}

/// What an installation fills in itself (pyo-up.sh works these out on the host).
#[derive(Debug, Default, PartialEq)]
pub struct InstallValues {
    pub tailnet_ip: String,
    pub registry_push: String,
    pub tailnet_api: String,
    /// The host's tailnet name (Self.DNSName, "<host>.<tailnet>.ts.net"), when
    /// its tailnet has MagicDNS and HTTPS certificates; else empty.
    pub tailnet_name: String,
}

impl InstallValues {
    /// Put them in a release's objects. Values are JSON-escaped.
    pub fn fill(&self, objects: &[Value]) -> anyhow::Result<Vec<Value>> {
        let esc = |s: &str| serde_json::to_string(s).map(|q| q[1..q.len() - 1].to_owned());
        let text = serde_json::to_string(objects)?
            .replace("__TAILNET_IP__", &esc(&self.tailnet_ip)?)
            .replace("__REGISTRY_PUSH__", &esc(&self.registry_push)?)
            .replace("__TAILNET_API__", &esc(&self.tailnet_api)?)
            .replace("__TAILNET_NAME__", &esc(&self.tailnet_name)?);
        Ok(serde_json::from_str(&text)?)
    }

    /// install.sh knows them from the host itself (its Tailscale IP), so its
    /// values win: PYO_INSTALL_TAILNET_IP, _REGISTRY_PUSH, _TAILNET_API.
    pub fn override_from_env(&mut self) {
        let get = |k: &str| std::env::var(k).ok();
        if let Some(ip) = get("PYO_INSTALL_TAILNET_IP") {
            self.tailnet_ip = ip;
        }
        if let Some(r) = get("PYO_INSTALL_REGISTRY_PUSH").filter(|r| !r.is_empty()) {
            self.registry_push = r;
        }
        if let Some(a) = get("PYO_INSTALL_TAILNET_API") {
            self.tailnet_api = a;
        }
        if let Some(h) = get("PYO_INSTALL_TAILNET_NAME") {
            self.tailnet_name = h;
        }
    }

    /// From the running pyo-host (its tailnet IP) and Pyo Deployments.
    pub fn read(host: Option<&Deployment>, pyo: Option<&Deployment>) -> Self {
        let mut v = InstallValues::default();
        let containers = |d: Option<&Deployment>| {
            d.and_then(|d| d.spec.as_ref())
                .and_then(|s| s.template.spec.as_ref())
                .map(|s| s.containers.clone())
                .unwrap_or_default()
        };
        for c in containers(host).iter().filter(|c| c.name == "forward") {
            let script = c.args.clone().unwrap_or_default().join("\n");
            if let Some(rest) = script.split("IP=\"").nth(1) {
                v.tailnet_ip = rest.split('"').next().unwrap_or("").to_owned();
            }
        }
        for c in containers(pyo).iter().filter(|c| c.name == "pyo") {
            for e in c.env.clone().unwrap_or_default() {
                match e.name.as_str() {
                    "PYO_REGISTRY_PUSH" => v.registry_push = e.value.unwrap_or_default(),
                    "PYO_TAILNET_API_URL" => v.tailnet_api = e.value.unwrap_or_default(),
                    "PYO_TAILNET_NAME" => v.tailnet_name = e.value.unwrap_or_default(),
                    _ => {}
                }
            }
        }
        // The registry door on the tailnet (pyo-host); none without one.
        if v.registry_push.is_empty() && !v.tailnet_ip.is_empty() {
            v.registry_push = format!("{}:5005", v.tailnet_ip);
        }
        v
    }
}

/// A release's objects for pyo-system, checked before anything is applied:
/// known kinds, in pyo-system, no Namespace (it exists).
fn prepare(objects: &[Value]) -> anyhow::Result<Vec<(ApiResource, String, Value)>> {
    let mut out = vec![];
    for o in objects {
        if o["kind"] == json!("Namespace") {
            continue;
        }
        let mut o = o.clone();
        let ns = o["metadata"]["namespace"].as_str().unwrap_or(NS);
        if ns != NS {
            anyhow::bail!(
                "the release has an object in namespace {ns}; the upgrader only changes {NS}"
            );
        }
        o["metadata"]["namespace"] = json!(NS);
        let name = o["metadata"]["name"]
            .as_str()
            .filter(|n| !n.is_empty())
            .ok_or_else(|| anyhow::anyhow!("an object in the release has no name"))?
            .to_owned();
        out.push((resource(&o)?, name, o));
    }
    Ok(out)
}

pub async fn upgrade(client: &Client, release: &Release) -> Result<(), Failure> {
    let problems = release.problems();
    if !problems.is_empty() {
        return Err(format!("This release can't be applied: {}", problems.join(" ")).into());
    }
    let deployments = Api::<Deployment>::namespaced(client.clone(), NS);
    let statefulsets = Api::<StatefulSet>::namespaced(client.clone(), NS);
    let mut values = InstallValues::read(
        deployments.get_opt("pyo-host").await?.as_ref(),
        deployments.get_opt("pyo").await?.as_ref(),
    );
    values.override_from_env();
    log(&format!(
        "Updating Pyo to {} (registry push {}, tailnet IP {})",
        release.version,
        values.registry_push,
        if values.tailnet_ip.is_empty() {
            "none"
        } else {
            &values.tailnet_ip
        }
    ));
    let platform = prepare(&values.fill(&release.manifests.platform)?)?;
    let pyo = prepare(&values.fill(&release.manifests.pyo)?)?;

    // What's running now, to put back if the new version doesn't come up.
    let mut saved: Vec<(String, String, Value)> = vec![];
    for (ar, name, _) in platform.iter().chain(&pyo) {
        let template = match ar.kind.as_str() {
            "Deployment" => deployments
                .get_opt(name)
                .await?
                .and_then(|d| d.spec)
                .map(|s| serde_json::to_value(s.template)),
            "StatefulSet" => statefulsets
                .get_opt(name)
                .await?
                .and_then(|d| d.spec)
                .map(|s| serde_json::to_value(s.template)),
            _ => None,
        };
        if let Some(t) = template {
            saved.push((ar.kind.clone(), name.clone(), t?));
        }
    }

    ensure_secrets(client).await?;
    let pp = PatchParams::apply(INSTALL_MANAGER).force();
    for (step, objects) in [("platform services", &platform), ("Pyo", &pyo)] {
        log(&format!("Applying {step}"));
        for (ar, name, o) in objects.iter() {
            // Volume claims are only created: their spec can't change once
            // bound, and claims made by an older client-side apply would
            // otherwise have their bound volume cleared.
            if ar.kind == "PersistentVolumeClaim"
                && Api::<DynamicObject>::namespaced_with(client.clone(), NS, ar)
                    .get_opt(name)
                    .await?
                    .is_some()
            {
                continue;
            }
            created_stopped(client, ar, name, o)
                .await
                .map_err(|e| format!("Couldn't create {} {name}: {e}", ar.kind))?;
            Api::<DynamicObject>::namespaced_with(client.clone(), NS, ar)
                .patch(name, &pp, &Patch::Apply(o))
                .await
                .map_err(|e| format!("Couldn't apply {} {name}: {e}", ar.kind))?;
        }
        for (ar, name, _) in objects.iter() {
            let waited = match ar.kind.as_str() {
                "Deployment" => wait_deployment(&deployments, client, name).await,
                "StatefulSet" => wait_statefulset(&statefulsets, client, name).await,
                _ => Ok(()),
            };
            if let Err(e) = waited {
                log(&format!(
                    "{} {name} didn't become ready: {e}; putting back what was running",
                    ar.kind
                ));
                let restored = restore(client, &saved).await;
                return Err(Failure {
                    message: match restored {
                        Ok(()) => format!(
                            "{} {name} of Pyo {} didn't become ready ({e}). Put back what was running before, and nothing else changed.",
                            ar.kind, release.version
                        ),
                        Err(r) => format!(
                            "{} {name} of Pyo {} didn't become ready ({e}), and putting back the previous version didn't finish either ({r}). Check pyo-system.",
                            ar.kind, release.version
                        ),
                    },
                    rolled_back: true,
                });
            }
            if matches!(ar.kind.as_str(), "Deployment" | "StatefulSet") {
                log(&format!("{} {name} is ready", ar.kind));
            }
        }
    }
    log(&format!("Pyo is on {}", release.version));
    Ok(())
}

fn log(msg: &str) {
    println!("{msg}");
}

/// A new install on a small server (pyo-proto's memory.rs, as Pyo counts it
/// for observability's default) starts with observability off: its
/// ClickHouse is created stopped, so it's never pulled or started. Created,
/// not applied, so the replicas belong to the agent's field manager (which
/// sets them from then on, observability.rs) and not to the apply after.
async fn created_stopped(
    client: &Client,
    ar: &ApiResource,
    name: &str,
    o: &Value,
) -> anyhow::Result<()> {
    if ar.kind != "StatefulSet"
        || name != "pyo-clickhouse"
        || pyo_proto::memory::server_tier() != pyo_proto::memory::Tier::Small
    {
        return Ok(());
    }
    let api = Api::<DynamicObject>::namespaced_with(client.clone(), NS, ar);
    if api.get_opt(name).await?.is_some() {
        return Ok(());
    }
    let mut o = o.clone();
    o["spec"]["replicas"] = json!(0);
    let pp = PostParams {
        field_manager: Some(MANAGER.into()),
        ..Default::default()
    };
    api.create(&pp, &serde_json::from_value(o)?).await?;
    log("A small server: observability starts off, ClickHouse stopped");
    Ok(())
}

/// The secrets pyo-up.sh creates, when a newer release needs one an older
/// install didn't make. Existing ones are never read or changed.
async fn ensure_secrets(client: &Client) -> anyhow::Result<()> {
    let secrets = Api::<Secret>::namespaced(client.clone(), NS);
    let hex = |n: usize| -> anyhow::Result<String> {
        let mut b = vec![0u8; n];
        use std::io::Read;
        std::fs::File::open("/dev/urandom")?.read_exact(&mut b)?;
        Ok(b.iter().map(|x| format!("{x:02x}")).collect())
    };
    let pg = hex(16)?;
    let wanted: Vec<(&str, Value)> = vec![
        // Only on a new install (install.sh): an existing one keeps its own.
        (
            "pyo-postgres",
            json!({ "password": pg, "url": format!("postgres://pyo:{pg}@pyo-postgres.{NS}.svc:5432/pyo") }),
        ),
        ("pyo-clickhouse", json!({ "password": hex(16)? })),
        ("pyo-secrets", json!({ "secret-key": hex(32)? })),
        // The backup store (Garage): its RPC secret and admin token, and
        // Pyo's own S3 key, imported at start (teams get keys of their own).
        (
            "pyo-backup-store",
            json!({ "rpc-secret": hex(32)?, "admin-token": hex(32)?, "access-key": format!("GK{}", hex(12)?), "secret-key": hex(32)? }),
        ),
    ];
    for (name, data) in wanted {
        match secrets.get_opt(name).await {
            Ok(Some(_)) => {}
            Ok(None) => {
                log(&format!("Creating the {name} secret"));
                let s = json!({ "apiVersion": "v1", "kind": "Secret", "metadata": { "name": name, "namespace": NS }, "type": "Opaque", "stringData": data });
                secrets
                    .create(&PostParams::default(), &serde_json::from_value(s)?)
                    .await?;
            }
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}

async fn pods_stuck(
    client: &Client,
    selector: &std::collections::BTreeMap<String, String>,
) -> Option<String> {
    let sel = selector
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join(",");
    stuck_reason(&Api::<Pod>::namespaced(client.clone(), NS), &sel).await
}

/// Like `kubectl rollout status`, failing early on a container that can't
/// pull or keeps crashing.
async fn wait_deployment(api: &Api<Deployment>, client: &Client, name: &str) -> anyhow::Result<()> {
    let started = Instant::now();
    let mut stuck_since: Option<Instant> = None;
    loop {
        let d = api.get(name).await?;
        let want = d.spec.as_ref().and_then(|s| s.replicas).unwrap_or(1);
        let generation = d.metadata.generation.unwrap_or(0);
        let st = d.status.clone().unwrap_or_default();
        if st.observed_generation.unwrap_or(0) >= generation
            && st.updated_replicas.unwrap_or(0) >= want
            && st.replicas.unwrap_or(0) == st.updated_replicas.unwrap_or(0)
            && st.available_replicas.unwrap_or(0) >= want
        {
            return Ok(());
        }
        if let Some(c) = st.conditions.unwrap_or_default().iter().find(|c| {
            c.type_ == "Progressing" && c.reason.as_deref() == Some("ProgressDeadlineExceeded")
        }) {
            anyhow::bail!(
                "{}",
                c.message
                    .clone()
                    .unwrap_or_else(|| "it stopped making progress".into())
            );
        }
        let selector = d
            .spec
            .and_then(|s| s.selector.match_labels)
            .unwrap_or_default();
        match pods_stuck(client, &selector).await {
            Some(why) if stuck_since.get_or_insert_with(Instant::now).elapsed() > STUCK => {
                anyhow::bail!("{why}")
            }
            Some(_) => {}
            None => stuck_since = None,
        }
        let limit = rollout_timeout();
        if started.elapsed() > limit {
            anyhow::bail!("not ready after {} seconds", limit.as_secs());
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
}

async fn wait_statefulset(
    api: &Api<StatefulSet>,
    client: &Client,
    name: &str,
) -> anyhow::Result<()> {
    let started = Instant::now();
    let mut stuck_since: Option<Instant> = None;
    loop {
        let s = api.get(name).await?;
        let want = s.spec.as_ref().and_then(|s| s.replicas).unwrap_or(1);
        let generation = s.metadata.generation.unwrap_or(0);
        let st = s.status.clone().unwrap_or_default();
        let revisions_match =
            st.update_revision.is_none() || st.update_revision == st.current_revision;
        if st.observed_generation.unwrap_or(0) >= generation
            && st.ready_replicas.unwrap_or(0) >= want
            && st.updated_replicas.unwrap_or(0) >= want
            && revisions_match
        {
            return Ok(());
        }
        let selector = s
            .spec
            .and_then(|s| s.selector.match_labels)
            .unwrap_or_default();
        match pods_stuck(client, &selector).await {
            Some(why) if stuck_since.get_or_insert_with(Instant::now).elapsed() > STUCK => {
                anyhow::bail!("{why}")
            }
            Some(_) => {}
            None => stuck_since = None,
        }
        let limit = rollout_timeout();
        if started.elapsed() > limit {
            anyhow::bail!("not ready after {} seconds", limit.as_secs());
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
}

/// Put the saved pod templates back and wait for them. A StatefulSet stuck on
/// a broken pod doesn't replace it by itself, so its unready pods go.
async fn restore(client: &Client, saved: &[(String, String, Value)]) -> anyhow::Result<()> {
    let deployments = Api::<Deployment>::namespaced(client.clone(), NS);
    let statefulsets = Api::<StatefulSet>::namespaced(client.clone(), NS);
    let patch = |t: &Value| {
        Patch::<Value>::Json(
            serde_json::from_value(
                json!([{ "op": "replace", "path": "/spec/template", "value": t }]),
            )
            .expect("a JSON patch"),
        )
    };
    for (kind, name, template) in saved {
        match kind.as_str() {
            "Deployment" => {
                deployments
                    .patch(name, &PatchParams::default(), &patch(template))
                    .await?;
            }
            _ => {
                statefulsets
                    .patch(name, &PatchParams::default(), &patch(template))
                    .await?;
                tokio::time::sleep(Duration::from_secs(5)).await;
                let s = statefulsets.get(name).await?;
                let selector = s
                    .spec
                    .and_then(|s| s.selector.match_labels)
                    .unwrap_or_default();
                let sel = selector
                    .iter()
                    .map(|(k, v)| format!("{k}={v}"))
                    .collect::<Vec<_>>()
                    .join(",");
                let pods = Api::<Pod>::namespaced(client.clone(), NS);
                for p in pods.list(&ListParams::default().labels(&sel)).await?.items {
                    let ready = p
                        .status
                        .as_ref()
                        .and_then(|s| s.conditions.as_ref())
                        .is_some_and(|c| {
                            c.iter().any(|c| c.type_ == "Ready" && c.status == "True")
                        });
                    if !ready && let Some(n) = p.metadata.name {
                        let _ = pods.delete(&n, &DeleteParams::default()).await;
                    }
                }
            }
        }
    }
    for (kind, name, _) in saved {
        match kind.as_str() {
            "Deployment" => wait_deployment(&deployments, client, name).await?,
            _ => wait_statefulset(&statefulsets, client, name).await?,
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The release workflow can't publish a release.json that doesn't say
    /// (rightly) which roles its agent has.
    #[test]
    fn a_release_says_which_roles_its_agent_has() {
        let with = |h: Option<String>| Release {
            agent_access: h,
            ..Default::default()
        };
        assert_eq!(
            access_problem(&with(Some(pyo_proto::rbac::access_hash()))),
            None
        );
        assert!(access_problem(&with(None)).unwrap().contains("doesn't say"));
        assert!(
            access_problem(&with(Some("0000000000000000".into())))
                .unwrap()
                .contains("agentAccess is 0000000000000000")
        );
    }

    #[test]
    fn job_names_are_dns_labels() {
        assert_eq!(job_name("upd_AbC123"), "pyo-upgrade-upd-abc123");
        assert!(job_name(&"x".repeat(80)).len() <= 52);
    }

    #[test]
    fn install_values_come_from_whats_running() {
        let registry: Deployment = serde_json::from_value(json!({
            "metadata": { "name": "pyo-host" },
            "spec": { "selector": {}, "template": { "spec": { "containers": [
                { "name": "forward", "image": "alpine/socat", "args": ["IP=\"100.64.0.7\"\nsocat TCP-LISTEN:8000 &\nwait\n"] },
            ] } } },
        }))
        .unwrap();
        let pyo: Deployment = serde_json::from_value(json!({
            "metadata": { "name": "pyo" },
            "spec": { "selector": {}, "template": { "spec": { "containers": [{ "name": "pyo", "env": [
                { "name": "PYO_REGISTRY_PUSH", "value": "100.64.0.7:5005" },
                { "name": "PYO_TAILNET_API_URL", "value": "http://100.64.0.7:8000" },
            ] }] } } },
        }))
        .unwrap();
        let v = InstallValues::read(Some(&registry), Some(&pyo));
        assert_eq!(
            v,
            InstallValues {
                tailnet_ip: "100.64.0.7".into(),
                registry_push: "100.64.0.7:5005".into(),
                tailnet_api: "http://100.64.0.7:8000".into(),
                tailnet_name: String::new()
            }
        );
        // No tailnet, nothing running yet: what pyo-up.sh would fill in.
        assert_eq!(
            InstallValues::read(None, None).registry_push,
            "",
            "no door without a tailnet"
        );

        let filled = v.fill(&[json!({ "a": "IP=\"__TAILNET_IP__\"", "b": "__REGISTRY_PUSH__", "c": "__TAILNET_API__" })]).unwrap();
        assert_eq!(
            filled[0],
            json!({ "a": "IP=\"100.64.0.7\"", "b": "100.64.0.7:5005", "c": "http://100.64.0.7:8000" })
        );
        // A value can't break out of its string.
        let evil = InstallValues {
            tailnet_ip: "\", \"x\": \"y".into(),
            ..Default::default()
        };
        let filled = evil.fill(&[json!({ "a": "__TAILNET_IP__" })]).unwrap();
        assert_eq!(filled[0].as_object().unwrap().len(), 1);
    }

    /// The upgrader's Role may get, create and patch every kind a release
    /// may hold.
    #[test]
    fn upgrader_role_covers_every_kind() {
        let [_, role, _] = upgrader_rbac(&json!({}));
        for (group, _, kind, plural) in KINDS {
            let ok = role["rules"].as_array().unwrap().iter().any(|r| {
                let has = |k: &str, v: &str| r[k].as_array().unwrap().contains(&json!(v));
                has("apiGroups", group)
                    && has("resources", plural)
                    && ["get", "create", "patch"].iter().all(|v| has("verbs", v))
            });
            assert!(ok, "the upgrader's Role can't apply a {kind}");
        }
    }

    /// install.sh runs the upgrader as the agent does: with the same
    /// ServiceAccount, Role and RoleBinding (its `pyo:upgrader-rbac` block).
    #[test]
    fn install_sh_gives_the_upgrader_the_same_rbac() {
        let script = include_str!("../../../deploy/install.sh");
        let block = script
            .split("# >>> pyo:upgrader-rbac ")
            .nth(1)
            .and_then(|r| r.split("# <<< pyo:upgrader-rbac\n").next())
            .expect("deploy/install.sh has no pyo:upgrader-rbac block");
        let body = block
            .split("<<'JSON'\n")
            .nth(1)
            .and_then(|r| r.split("\nJSON\n").next())
            .expect("install.sh's pyo:upgrader-rbac block has no JSON");
        let list: Value = serde_json::from_str(body).expect("install.sh's upgrader RBAC is JSON");
        assert_eq!(list["kind"], "List");
        let expected = upgrader_rbac(&json!({ "app.kubernetes.io/managed-by": MANAGER }));
        assert_eq!(
            list["items"].as_array().unwrap().as_slice(),
            expected.as_slice(),
            "deploy/install.sh's upgrader RBAC differs from upgrader_rbac(); it should be:\n{}",
            expected
                .iter()
                .map(|o| o.to_string())
                .collect::<Vec<_>>()
                .join("\n")
        );
        // Its Job: the same account, image arguments and limits.
        for part in [
            "serviceAccountName: pyo-upgrade",
            "args: [upgrade, /release/release.json]",
            "resources: { requests: { cpu: 20m, memory: 32Mi }, limits: { memory: 256Mi } }",
            "securityContext: { runAsNonRoot: true, runAsUser: 65532, allowPrivilegeEscalation: false, readOnlyRootFilesystem: true, capabilities: { drop: [ALL] } }",
            "activeDeadlineSeconds: 1800",
        ] {
            assert!(
                script.contains(part),
                "install.sh's upgrader Job has no {part}"
            );
        }
        // pyo-system as platform.yaml has it (the upgrader skips Namespaces).
        let platform = include_str!("../../../deploy/k8s/platform.yaml");
        assert!(
            platform.contains(
                "kind: Namespace\nmetadata:\n  name: pyo-system\n  labels:\n    app.kubernetes.io/part-of: pyo\n---"
            ),
            "platform.yaml's pyo-system changed: update install.sh's run_upgrader and this test"
        );
        assert!(
            script.contains("metadata: { name: $NS, labels: { app.kubernetes.io/part-of: pyo } }")
        );
    }

    #[test]
    fn only_pyo_system_and_known_kinds() {
        let ok = prepare(&[
            json!({ "apiVersion": "v1", "kind": "Namespace", "metadata": { "name": "pyo-system" } }),
            json!({ "apiVersion": "apps/v1", "kind": "Deployment", "metadata": { "name": "pyo", "namespace": "pyo-system" } }),
            json!({ "apiVersion": "policy/v1", "kind": "PodDisruptionBudget", "metadata": { "name": "pyo" } }),
        ])
        .unwrap();
        assert_eq!(ok.len(), 2);
        assert_eq!(ok[1].0.plural, "poddisruptionbudgets");
        assert_eq!(ok[1].2["metadata"]["namespace"], "pyo-system");
        assert!(prepare(&[json!({ "apiVersion": "apps/v1", "kind": "Deployment", "metadata": { "name": "x", "namespace": "kube-system" } })]).is_err());
        assert!(prepare(&[json!({ "apiVersion": "rbac.authorization.k8s.io/v1", "kind": "ClusterRoleBinding", "metadata": { "name": "x" } })]).is_err());
        assert!(
            prepare(&[json!({ "apiVersion": "v1", "kind": "Service", "metadata": {} })]).is_err()
        );
    }

    /// Against a disposable Kubernetes API server (no kubelet), as the
    /// upgrader's own ServiceAccount: applies an old release as pyo-up.sh
    /// would, then the new one. Pods are marked ready by hand where noted.
    ///
    /// PYO_TEST_KUBECONFIG=<throwaway kubeconfig> PYO_TEST_RELEASE_OLD=<release.json>
    /// PYO_TEST_RELEASE_NEW=<release.json> cargo test -p pyo-agent -- --ignored upgrader_against
    #[tokio::test]
    #[ignore = "needs a throwaway Kubernetes API server"]
    async fn upgrader_against_a_cluster() {
        use k8s_openapi::api::{
            authentication::v1::TokenRequest,
            core::v1::{Namespace, ServiceAccount},
        };
        let Ok(path) = std::env::var("PYO_TEST_KUBECONFIG") else {
            panic!("set PYO_TEST_KUBECONFIG to a throwaway cluster's kubeconfig")
        };
        let kc = kube::config::Kubeconfig::read_from(&path).unwrap();
        let config = kube::Config::from_custom_kubeconfig(kc, &Default::default())
            .await
            .unwrap();
        let admin = Client::try_from(config.clone()).unwrap();
        let read = |k: &str| -> Release {
            serde_json::from_slice(&std::fs::read(std::env::var(k).unwrap()).unwrap()).unwrap()
        };
        let (old, new) = (read("PYO_TEST_RELEASE_OLD"), read("PYO_TEST_RELEASE_NEW"));
        let pp = PatchParams::apply(INSTALL_MANAGER).force();

        // The namespace and the upgrader's access, as the agent makes them.
        Api::<Namespace>::all(admin.clone())
            .patch(
                NS,
                &pp,
                &Patch::Apply(
                    json!({ "apiVersion": "v1", "kind": "Namespace", "metadata": { "name": NS } }),
                ),
            )
            .await
            .unwrap();
        for o in upgrader_rbac(&json!({ "app.kubernetes.io/managed-by": MANAGER })) {
            let kind = o["kind"].as_str().unwrap();
            let (group, version, plural) = match kind {
                "ServiceAccount" => ("", "v1", "serviceaccounts"),
                "Role" => ("rbac.authorization.k8s.io", "v1", "roles"),
                _ => ("rbac.authorization.k8s.io", "v1", "rolebindings"),
            };
            let ar = ApiResource::from_gvk_with_plural(
                &GroupVersionKind::gvk(group, version, kind),
                plural,
            );
            Api::<DynamicObject>::namespaced_with(admin.clone(), NS, &ar)
                .patch(
                    UPGRADER,
                    &PatchParams::apply(MANAGER).force(),
                    &Patch::Apply(&o),
                )
                .await
                .unwrap();
        }
        // What pyo-up.sh would have running: the old release, and the Postgres secret.
        let values = InstallValues {
            tailnet_ip: "100.64.0.9".into(),
            registry_push: "100.64.0.9:5005".into(),
            tailnet_api: "http://100.64.0.9:8000".into(),
            tailnet_name: String::new(),
        };
        for (ar, name, o) in prepare(&values.fill(&old.manifests.platform).unwrap())
            .unwrap()
            .into_iter()
            .chain(prepare(&values.fill(&old.manifests.pyo).unwrap()).unwrap())
        {
            Api::<DynamicObject>::namespaced_with(admin.clone(), NS, &ar)
                .patch(&name, &pp, &Patch::Apply(&o))
                .await
                .unwrap();
        }
        let secrets = Api::<Secret>::namespaced(admin.clone(), NS);
        let _ = secrets.create(&PostParams::default(), &serde_json::from_value(json!({ "metadata": { "name": "pyo-postgres" }, "stringData": { "url": "postgres://x" } })).unwrap()).await;

        // From here on, only the upgrader's ServiceAccount.
        let token = Api::<ServiceAccount>::namespaced(admin.clone(), NS)
            .create_token_request(
                UPGRADER,
                &PostParams::default(),
                &serde_json::from_value::<TokenRequest>(
                    json!({ "spec": { "audiences": [], "expirationSeconds": 3600 } }),
                )
                .unwrap(),
            )
            .await
            .unwrap()
            .status
            .unwrap()
            .token
            .unwrap();
        let mut sa_config = config.clone();
        sa_config.auth_info = kube::config::AuthInfo {
            token: Some(token.into()),
            ..Default::default()
        };
        let sa = Client::try_from(sa_config).unwrap();
        // It can't reach outside pyo-system.
        assert!(
            Api::<Secret>::namespaced(sa.clone(), "default")
                .list(&ListParams::default())
                .await
                .is_err()
        );
        assert!(
            Api::<Secret>::namespaced(sa.clone(), NS)
                .list(&ListParams::default())
                .await
                .is_err(),
            "it may not list secrets"
        );

        let deployments = Api::<Deployment>::namespaced(admin.clone(), NS);
        let image_of = |d: &Deployment| {
            d.spec
                .as_ref()
                .unwrap()
                .template
                .spec
                .as_ref()
                .unwrap()
                .containers[0]
                .image
                .clone()
                .unwrap()
        };
        let old_image = image_of(&deployments.get("pyo").await.unwrap());
        assert_eq!(old_image, old.images.pyo);

        // 1. Nothing becomes ready (no kubelet): it fails, and puts back the old version.
        unsafe { std::env::set_var("PYO_UPGRADE_ROLLOUT_SECS", "8") };
        let err = upgrade(&sa, &new)
            .await
            .expect_err("nothing can become ready here");
        assert!(err.rolled_back, "{}", err.message);
        println!("failure: {}", err.message);
        assert_eq!(
            image_of(&deployments.get("pyo").await.unwrap()),
            old_image,
            "the old version is back"
        );
        // The secrets a newer release needs were made; the one that existed wasn't touched.
        for name in ["pyo-clickhouse", "pyo-secrets", "pyo-backup-store"] {
            assert!(secrets.get_opt(name).await.unwrap().is_some(), "{name}");
        }
        assert!(
            secrets
                .get("pyo-postgres")
                .await
                .unwrap()
                .data
                .unwrap()
                .contains_key("url")
        );
        // Fields filled in from what was running.
        let host = deployments.get("pyo-host").await.unwrap();
        assert!(
            serde_json::to_string(&host.spec)
                .unwrap()
                .contains("IP=\\\"100.64.0.9\\\"")
        );

        // 2. With every pod marked ready (standing in for the kubelet), it succeeds.
        let marker = tokio::spawn(mark_pods_ready(admin.clone()));
        unsafe { std::env::set_var("PYO_UPGRADE_ROLLOUT_SECS", "90") };
        let res = upgrade(&sa, &new).await;
        marker.abort();
        res.unwrap_or_else(|e| panic!("{}", e.message));
        let d = deployments.get("pyo").await.unwrap();
        assert_eq!(image_of(&d), new.images.pyo);
        assert_eq!(
            d.spec
                .unwrap()
                .strategy
                .unwrap()
                .rolling_update
                .unwrap()
                .max_unavailable,
            Some(k8s_openapi::apimachinery::pkg::util::intstr::IntOrString::Int(0))
        );
        let env = serde_json::to_string(&deployments.get("pyo").await.unwrap().spec).unwrap();
        assert!(
            env.contains("100.64.0.9:5005") && env.contains(&new.images.agent),
            "install values kept, agent image moved"
        );
    }

    /// Mark every pod in pyo-system running and ready, like a kubelet would.
    async fn mark_pods_ready(client: Client) {
        let pods = Api::<Pod>::namespaced(client, NS);
        loop {
            if let Ok(list) = pods.list(&ListParams::default()).await {
                for p in list.items {
                    let ready = p
                        .status
                        .as_ref()
                        .and_then(|s| s.conditions.as_ref())
                        .is_some_and(|c| {
                            c.iter().any(|c| c.type_ == "Ready" && c.status == "True")
                        });
                    if ready {
                        continue;
                    }
                    let name = p.metadata.name.clone().unwrap();
                    let now = k8s_openapi::jiff::Timestamp::now().to_string();
                    let containers: Vec<Value> = p.spec.as_ref().unwrap().containers.iter().map(|c| json!({
                        "name": c.name, "image": c.image, "imageID": "", "ready": true, "restartCount": 0, "started": true,
                        "state": { "running": { "startedAt": now } },
                    })).collect();
                    let status = json!({ "status": {
                        "phase": "Running",
                        "conditions": [{ "type": "Ready", "status": "True", "lastTransitionTime": "2020-01-01T00:00:00Z" }, { "type": "ContainersReady", "status": "True", "lastTransitionTime": "2020-01-01T00:00:00Z" }],
                        "containerStatuses": containers,
                    } });
                    let _ = pods
                        .patch_status(&name, &PatchParams::default(), &Patch::Merge(&status))
                        .await;
                }
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }
}
