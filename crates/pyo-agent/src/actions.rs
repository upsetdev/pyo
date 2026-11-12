//! Commands that aren't rollouts: restart, stop, delete, cordon, maintenance
//! and on-demand logs.

use k8s_openapi::api::batch::v1::{CronJob, Job};
use k8s_openapi::api::{
    apps::v1::{Deployment, StatefulSet},
    core::v1::{Namespace, Node, PersistentVolumeClaim, Pod, Secret, Service},
};
use kube::{
    Api, ResourceExt,
    api::{DeleteParams, ListParams, LogParams, Patch, PatchParams},
};
use pyo_proto::{Command, CommandResult, LogLine, ServiceRef, now_ms};
use serde_json::json;

use crate::{
    Agent, failed,
    observe::{MAINTENANCE_ANNOTATION, SERVICE_LABEL, VOLUME_LABEL},
    ok,
};

pub async fn run(agent: &Agent, command: Command) -> CommandResult {
    let result = match command {
        Command::Restart(service) => restart(agent, &service).await,
        Command::Stop(service) => stop(agent, &service).await,
        Command::Scale { service, replicas } => scale(agent, &service, replicas).await,
        Command::SyncCronJobs { service, jobs } => crate::cron::sync(&agent.kube, &service, &jobs)
            .await
            .map(|n| format!("{n} cron job(s) in place")),
        Command::RunCronJob { service, id } => crate::cron::run(&agent.kube, &service, &id)
            .await
            .map(|name| format!("Started {name}")),
        Command::Autoscale {
            service,
            autoscale,
            replicas,
        } => set_autoscale(agent, &service, autoscale.as_ref(), replicas).await,
        Command::DeleteService(service) => delete(agent, &service).await,
        Command::SetSchedulable {
            server,
            schedulable,
        } => cordon(agent, &server, !schedulable, None).await,
        Command::SetMaintenance { server, on } => cordon(agent, &server, on, Some(on)).await,
        Command::FetchLogs {
            service,
            tail_lines,
        } => return logs(agent, &service, tail_lines).await,
        Command::EnsureBuilder(spec) => crate::builder::ensure(agent, &spec).await,
        Command::RemoveBuilder { builder_id } => crate::builder::remove(agent, &builder_id).await,
        Command::RemoveServer { server, force } => {
            crate::drain::remove(agent, &server, force).await
        }
        Command::SetOriginPull { ca_pem, .. } => {
            crate::gateway::set_origin_pull(&agent.kube, &ca_pem).await
        }
        Command::SetWebProxyProtocol { on } => {
            crate::gateway::set_web_proxy_protocol(&agent.kube, on).await
        }
        Command::SyncFirewall(sync) => crate::firewall::sync(agent, *sync).await,
        Command::SyncRoutes(target) => crate::gateway::sync_routes(agent, &target).await,
        Command::ApplyBundle { bundle, prune } => {
            crate::bundles::apply(&agent.kube, &bundle, &prune).await
        }
        Command::RemoveBundle { id, objects } => {
            crate::bundles::remove(&agent.kube, &id, &objects).await
        }
        Command::ApplyDatabase(spec) => crate::databases::apply(&agent.kube, &spec).await,
        Command::DeleteDatabase { namespace, name } => {
            crate::databases::delete(&agent.kube, &namespace, &name).await
        }
        Command::FetchDatabaseLogs {
            namespace,
            name,
            tail_lines,
            since_seconds,
        } => {
            return crate::databases::logs(
                &agent.kube,
                &namespace,
                &name,
                tail_lines,
                since_seconds,
            )
            .await;
        }
        Command::CheckDatabaseImport(spec) => {
            return crate::dbimport::check(&agent.kube, *spec).await;
        }
        Command::SetTrafficMode { mode } => {
            crate::rollout::set_traffic_mode(&agent.kube, &mode).await
        }
        Command::SetIsolation { namespace, isolate } => {
            crate::rollout::set_isolation(&agent.kube, &namespace, isolate).await
        }
        Command::DeleteNamespace {
            namespace,
            project_id,
        } => delete_namespace(&agent.kube, &namespace, &project_id).await,
        Command::DeleteVolume {
            namespace,
            name,
            volume_id,
        } => {
            delete_volume(
                &agent.kube,
                &namespace,
                &name,
                &volume_id,
                std::time::Duration::from_secs(60),
            )
            .await
        }
        Command::CreateJoinToken {
            token_id,
            token_secret,
            ttl_seconds,
        } => join_token(agent, &token_id, &token_secret, ttl_seconds).await,
        Command::ResizeVolume {
            namespace,
            name,
            volume_id,
            size_gi,
        } => crate::volumes::resize(&agent.kube, &namespace, &name, &volume_id, size_gi).await,
        Command::MoveVolume(spec) => crate::volumes::move_claim(&agent.kube, &spec).await,
        Command::Deploy(_)
        | Command::Cancel { .. }
        | Command::OpenTerminal { .. }
        | Command::Backup(_)
        | Command::ListSnapshots(_)
        | Command::ForgetSnapshot(_)
        | Command::RestoreVolume(_)
        | Command::DbSetup { .. }
        | Command::UpgradePyo(_)
        | Command::UpdateAgent { .. }
        | Command::RefreshInstall(_) => {
            unreachable!("handled by the command loop")
        }
    };
    match result {
        Ok(msg) => ok(&msg),
        Err(e) => failed(format!("{e:#}")),
    }
}

fn merge(body: serde_json::Value) -> Patch<serde_json::Value> {
    Patch::Merge(body)
}

async fn patch_workload(
    agent: &Agent,
    service: &ServiceRef,
    body: serde_json::Value,
) -> anyhow::Result<()> {
    let pp = PatchParams::default();
    if service.workload_kind == "StatefulSet" {
        Api::<StatefulSet>::namespaced(agent.kube.clone(), &service.namespace)
            .patch(&service.name, &pp, &merge(body))
            .await?;
    } else {
        Api::<Deployment>::namespaced(agent.kube.clone(), &service.namespace)
            .patch(&service.name, &pp, &merge(body))
            .await?;
    }
    Ok(())
}

/// Pause a CronJob, or let it run again.
pub async fn set_suspended(
    client: &kube::Client,
    service: &ServiceRef,
    suspended: bool,
) -> anyhow::Result<()> {
    Api::<CronJob>::namespaced(client.clone(), &service.namespace)
        .patch(
            &service.name,
            &PatchParams::default(),
            &merge(json!({ "spec": { "suspend": suspended } })),
        )
        .await?;
    Ok(())
}

/// A run outside the schedule, made from the CronJob's own template and owned
/// by it, so it's cleaned up with it. Returns its name.
pub async fn run_now(client: &kube::Client, service: &ServiceRef) -> anyhow::Result<String> {
    run_job(client, &service.namespace, &service.name).await
}

/// A run of a CronJob outside its schedule, made from its template and owned
/// by it. Returns the Job's name.
pub async fn run_job(
    client: &kube::Client,
    namespace: &str,
    cron_name: &str,
) -> anyhow::Result<String> {
    let crons = Api::<CronJob>::namespaced(client.clone(), namespace);
    let cron = crons.get(cron_name).await?;
    let uid = cron.metadata.uid.clone().unwrap_or_default();
    let template = serde_json::to_value(&cron.spec.job_template)?;
    let stem: String = cron_name.chars().take(40).collect();
    let name = format!("{stem}-run-{}", now_ms() / 1000);
    let job = json!({
        "apiVersion": "batch/v1", "kind": "Job",
        "metadata": {
            "name": name, "labels": template["metadata"]["labels"],
            "ownerReferences": [{ "apiVersion": "batch/v1", "kind": "CronJob", "name": cron_name, "uid": uid, "blockOwnerDeletion": false }],
        },
        "spec": template["spec"],
    });
    let job: Job = serde_json::from_value(job)?;
    Api::<Job>::namespaced(client.clone(), namespace)
        .create(&Default::default(), &job)
        .await?;
    Ok(name)
}

async fn restart(agent: &Agent, service: &ServiceRef) -> anyhow::Result<String> {
    if service.workload_kind == "CronJob" {
        return run_now(&agent.kube, service)
            .await
            .map(|name| format!("Started {name}"));
    }
    // Same mechanism as `kubectl rollout restart`.
    let at = k8s_openapi::jiff::Timestamp::now().to_string();
    patch_workload(agent, service, json!({ "spec": { "template": { "metadata": { "annotations": { "kubectl.kubernetes.io/restartedAt": at } } } } })).await?;
    Ok(format!("Restarting {}", service.name))
}

async fn stop(agent: &Agent, service: &ServiceRef) -> anyhow::Result<String> {
    if service.workload_kind == "CronJob" {
        set_suspended(&agent.kube, service, true).await?;
        return Ok(format!("Paused {}", service.name));
    }
    patch_workload(agent, service, json!({ "spec": { "replicas": 0 } })).await?;
    Ok(format!("Scaled {} to 0", service.name))
}

async fn scale(agent: &Agent, service: &ServiceRef, replicas: i32) -> anyhow::Result<String> {
    if service.workload_kind == "CronJob" {
        set_suspended(&agent.kube, service, replicas == 0).await?;
        return Ok(format!("Scheduled {}", service.name));
    }
    patch_workload(
        agent,
        service,
        json!({ "spec": { "replicas": replicas.max(0) } }),
    )
    .await?;
    Ok(format!("Scaled {} to {replicas}", service.name))
}

async fn set_autoscale(
    agent: &Agent,
    service: &ServiceRef,
    autoscale: Option<&pyo_proto::Autoscale>,
    replicas: i32,
) -> anyhow::Result<String> {
    crate::rollout::apply_autoscaler(&agent.kube, service, autoscale).await?;
    match autoscale {
        Some(a) => Ok(format!(
            "Autoscaling {} between {} and {}",
            service.name, a.min, a.max
        )),
        None => {
            // The autoscaler is gone: hold the size it had reached.
            patch_workload(
                agent,
                service,
                json!({ "spec": { "replicas": replicas.max(0) } }),
            )
            .await?;
            Ok(format!(
                "Stopped autoscaling {}, at {replicas}",
                service.name
            ))
        }
    }
}

async fn delete_named<K>(api: Api<K>, name: &str) -> anyhow::Result<()>
where
    K: kube::Resource + Clone + serde::de::DeserializeOwned + std::fmt::Debug,
{
    match api.delete(name, &DeleteParams::background()).await {
        Ok(_) => Ok(()),
        Err(kube::Error::Api(e)) if e.code == 404 => Ok(()),
        Err(e) => Err(e.into()),
    }
}

/// Removes the workload, Service, routes and env Secrets. Volumes stay.
async fn delete(agent: &Agent, service: &ServiceRef) -> anyhow::Result<String> {
    let c = agent.kube.clone();
    let ns = service.namespace.as_str();
    delete_named(
        Api::<k8s_openapi::api::autoscaling::v2::HorizontalPodAutoscaler>::namespaced(
            c.clone(),
            ns,
        ),
        &service.name,
    )
    .await?;
    delete_named(Api::<CronJob>::namespaced(c.clone(), ns), &service.name).await?;
    crate::cron::sync_list(&c, service, &[]).await?;
    delete_named(Api::<Deployment>::namespaced(c.clone(), ns), &service.name).await?;
    delete_named(Api::<StatefulSet>::namespaced(c.clone(), ns), &service.name).await?;
    delete_named(Api::<Service>::namespaced(c.clone(), ns), &service.name).await?;
    let public = crate::rollout::public_service_name(&service.name);
    delete_named(Api::<Service>::namespaced(c.clone(), ns), &public).await?;
    for policy in [
        format!("pyo-public-{}", service.name),
        format!("pyo-public-{public}"),
    ] {
        delete_named(
            Api::<k8s_openapi::api::networking::v1::NetworkPolicy>::namespaced(c.clone(), ns),
            &policy,
        )
        .await?;
    }
    // Its routes, Gateway and certificates with the gateway.
    crate::gateway::remove(&c, &service.service_id).await;
    // Secrets: its pull secret and each deployment's env
    // (<service>-env-<deployment>).
    let secrets = Api::<Secret>::namespaced(c, ns);
    let _ = delete_named(secrets.clone(), &crate::rollout::pull_secret(&service.name)).await;
    let env = format!("{}-env", service.name);
    for s in secrets.list(&ListParams::default()).await?.items {
        let name = s.name_any();
        let ours = s.labels().get(SERVICE_LABEL) == Some(&service.service_id);
        if ours && (name == env || name.starts_with(&format!("{env}-"))) {
            delete_named(secrets.clone(), &name).await?;
        }
    }
    Ok(format!("Deleted {}", service.name))
}

/// A deleted project's namespace, with whatever is left in it. Only when it
/// is that project's (rbac's `project_namespace_refusal`), and kept while it
/// holds volumes: deleting a project keeps them. Already gone is done.
async fn delete_namespace(
    client: &kube::Client,
    ns: &str,
    project_id: &str,
) -> anyhow::Result<String> {
    let api = Api::<Namespace>::all(client.clone());
    let Some(have) = api.get_opt(ns).await? else {
        return Ok(format!("{ns} is already gone"));
    };
    if have.metadata.deletion_timestamp.is_some() {
        return Ok(format!("{ns} is being deleted"));
    }
    if let Some(why) = pyo_proto::rbac::project_namespace_refusal(ns, have.labels(), project_id) {
        anyhow::bail!("Didn't delete namespace {ns}: {why}");
    }
    let claims = Api::<PersistentVolumeClaim>::namespaced(client.clone(), ns)
        .list(&ListParams::default())
        .await?
        .items
        .len();
    if claims > 0 {
        return Ok(format!(
            "Kept namespace {ns}: it still has {claims} volume(s)"
        ));
    }
    match api.delete(ns, &DeleteParams::background()).await {
        Ok(_) => Ok(format!("Deleting namespace {ns}")),
        Err(kube::Error::Api(e)) if e.code == 404 => Ok(format!("{ns} is already gone")),
        Err(e) => Err(anyhow::anyhow!("Couldn't delete namespace {ns}: {e}")),
    }
}

/// Why the claim with these labels isn't deleted for volume `volume_id`;
/// None when it may be: only the claim Pyo made for that very volume.
pub fn volume_claim_refusal(
    labels: &std::collections::BTreeMap<String, String>,
    volume_id: &str,
) -> Option<String> {
    if labels
        .get("app.kubernetes.io/managed-by")
        .map(String::as_str)
        != Some(crate::MANAGER)
    {
        return Some("Pyo didn't make it.".into());
    }
    if volume_id.is_empty() || labels.get(VOLUME_LABEL).map(String::as_str) != Some(volume_id) {
        return Some(format!("it isn't volume {volume_id}'s."));
    }
    None
}

/// Pods that mount claim `claim` and haven't finished, by name, and whether
/// each is on its way out.
pub fn claim_users(pods: &[Pod], claim: &str) -> Vec<(String, bool)> {
    pods.iter()
        .filter(|p| {
            !matches!(
                p.status.as_ref().and_then(|s| s.phase.as_deref()),
                Some("Succeeded" | "Failed")
            )
        })
        .filter(|p| {
            p.spec
                .as_ref()
                .and_then(|s| s.volumes.as_ref())
                .is_some_and(|vs| {
                    vs.iter().any(|v| {
                        v.persistent_volume_claim
                            .as_ref()
                            .is_some_and(|c| c.claim_name == claim)
                    })
                })
        })
        .map(|p| (p.name_any(), p.metadata.deletion_timestamp.is_some()))
        .collect()
}

/// Who still holds a claim after [`wait_let_go`].
pub enum Busy {
    /// Pods running with it (their names).
    InUse(String),
    /// Pods still stopping when the wait ran out.
    Stopping(String),
}

impl Busy {
    pub fn message(&self, claim: &str, then: &str) -> String {
        match self {
            Busy::InUse(names) => format!("{claim} is still in use by {names}. {then}"),
            Busy::Stopping(names) => format!(
                "{claim} is still in use by {names}, which are stopping. Try again in a minute."
            ),
        }
    }
}

/// Waits for pods on their way out to let go of `claim` in `ns`, up to
/// `wait`. A pod running with it ends the wait at once.
pub async fn wait_let_go(
    client: &kube::Client,
    ns: &str,
    claim: &str,
    wait: std::time::Duration,
) -> anyhow::Result<Option<Busy>> {
    let pods = Api::<Pod>::namespaced(client.clone(), ns);
    let deadline = tokio::time::Instant::now() + wait;
    loop {
        let users = claim_users(&pods.list(&ListParams::default()).await?.items, claim);
        if users.is_empty() {
            return Ok(None);
        }
        let names = users
            .iter()
            .map(|(n, _)| n.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        if users.iter().any(|(_, leaving)| !leaving) {
            return Ok(Some(Busy::InUse(names)));
        }
        if tokio::time::Instant::now() >= deadline {
            return Ok(Some(Busy::Stopping(names)));
        }
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    }
}

/// A volume's claim, and its data with it: only the claim made for this
/// very volume, in a project's namespace, once no pod mounts it. Pods on
/// their way out (a deleted service's) are waited for, up to `wait`; so is
/// the claim's going. Already gone is done.
async fn delete_volume(
    client: &kube::Client,
    ns: &str,
    name: &str,
    volume_id: &str,
    wait: std::time::Duration,
) -> anyhow::Result<String> {
    let claims = Api::<PersistentVolumeClaim>::namespaced(client.clone(), ns);
    let Some(claim) = claims.get_opt(name).await? else {
        return Ok(format!("{name} is already gone"));
    };
    if let Some(why) = volume_claim_refusal(claim.labels(), volume_id) {
        anyhow::bail!("Didn't delete {name}: {why}");
    }
    let namespace = Api::<Namespace>::all(client.clone()).get(ns).await?;
    let label = |k: &str| namespace.labels().get(k).cloned().unwrap_or_default();
    if label(pyo_proto::rbac::MANAGED_LABEL) != "true"
        || label(pyo_proto::rbac::PROJECT_LABEL).is_empty()
        || !label(pyo_proto::bundles::BUNDLE_LABEL).is_empty()
    {
        anyhow::bail!("Didn't delete {name}: {ns} isn't a project's namespace.");
    }
    let deadline = tokio::time::Instant::now() + wait;
    if let Some(busy) = wait_let_go(client, ns, name, wait).await? {
        anyhow::bail!(
            "{}",
            busy.message(name, "Deploy the service that used it first.")
        );
    }
    if claim.metadata.deletion_timestamp.is_none() {
        // This claim, not one made again under its name since.
        let dp = DeleteParams {
            preconditions: Some(kube::api::Preconditions {
                uid: claim.uid(),
                resource_version: None,
            }),
            ..DeleteParams::background()
        };
        match claims.delete(name, &dp).await {
            Ok(_) => {}
            Err(kube::Error::Api(e)) if e.code == 404 => {
                return Ok(format!("{name} is already gone"));
            }
            Err(e) => anyhow::bail!("Couldn't delete {name}: {e}"),
        }
    }
    loop {
        if claims
            .get_opt(name)
            .await?
            .is_none_or(|c| c.uid() != claim.uid())
        {
            return Ok(format!("Deleted {name}"));
        }
        if tokio::time::Instant::now() >= deadline {
            anyhow::bail!("{name} is still being deleted.");
        }
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    }
}

/// Cordon or uncordon; with `maintenance`, also set or clear the annotation.
/// Pods aren't evicted: draining is left to the operator in this version.
async fn cordon(
    agent: &Agent,
    server: &str,
    unschedulable: bool,
    maintenance: Option<bool>,
) -> anyhow::Result<String> {
    let mut body = json!({ "spec": { "unschedulable": unschedulable } });
    if let Some(on) = maintenance {
        body["metadata"] = json!({ "annotations": { MAINTENANCE_ANNOTATION: if on { json!("true") } else { json!(null) } } });
    }
    Api::<Node>::all(agent.kube.clone())
        .patch(server, &PatchParams::default(), &merge(body))
        .await?;
    Ok(format!(
        "{server} {}",
        if unschedulable {
            "isn't taking new work"
        } else {
            "is taking new work"
        }
    ))
}

async fn logs(agent: &Agent, service: &ServiceRef, tail_lines: i64) -> CommandResult {
    let pods = Api::<Pod>::namespaced(agent.kube.clone(), &service.namespace);
    let list = match pods
        .list(&ListParams::default().labels(&format!("{SERVICE_LABEL}={}", service.service_id)))
        .await
    {
        Ok(l) => l.items,
        Err(e) => return failed(format!("Couldn't list pods: {e}")),
    };
    let mut lines: Vec<LogLine> = vec![];
    for p in &list {
        let name = p.name_any();
        let lp = LogParams {
            tail_lines: Some(tail_lines.clamp(1, 2000)),
            timestamps: true,
            ..Default::default()
        };
        match pods.logs(&name, &lp).await {
            Ok(out) => {
                for l in out.lines() {
                    // "2026-10-28T10:00:00.123456789Z message"
                    let (at, msg) = match l.split_once(' ') {
                        Some((ts, rest)) => match ts.parse::<k8s_openapi::jiff::Timestamp>() {
                            Ok(t) => (t.as_millisecond(), rest),
                            Err(_) => (now_ms(), l),
                        },
                        None => (now_ms(), l),
                    };
                    // Its words, colors aside; the colors stay for the dashboard.
                    let level = pyo_proto::ansi::level_of(msg);
                    lines.push(LogLine {
                        at,
                        level: level.into(),
                        source: name.clone(),
                        message: msg.chars().take(4000).collect(),
                    });
                }
            }
            Err(e) => lines.push(LogLine {
                at: now_ms(),
                level: "warn".into(),
                source: name.clone(),
                message: format!("No output yet: {e}"),
            }),
        }
    }
    lines.sort_by_key(|l| l.at);
    let skip = lines.len().saturating_sub(tail_lines as usize);
    CommandResult {
        ok: true,
        message: Some(format!("{} pod(s)", list.len())),
        logs: lines.into_iter().skip(skip).collect(),
        data: None,
    }
}

/// A bootstrap token (what `k3s token create` makes) for one new server to
/// join with, expiring after `ttl`. Earlier ones Pyo made that expired are
/// removed, by the names it keeps in pyo-system: it may delete kube-system's
/// Secrets, never read or list them.
async fn join_token(agent: &Agent, id: &str, secret: &str, ttl: u64) -> anyhow::Result<String> {
    const RECORD: &str = "pyo-join-tokens";
    let secrets = Api::<Secret>::namespaced(agent.kube.clone(), "kube-system");
    let records = Api::<Secret>::namespaced(agent.kube.clone(), "pyo-system");
    let now = k8s_openapi::jiff::Timestamp::now();
    let expires =
        k8s_openapi::jiff::Timestamp::from_millisecond(now.as_millisecond() + ttl as i64 * 1000)?;
    let mut known: std::collections::BTreeMap<String, String> = records
        .get_opt(RECORD)
        .await?
        .and_then(|s| s.data)
        .map(|d| {
            d.into_iter()
                .map(|(k, v)| (k, String::from_utf8_lossy(&v.0).into_owned()))
                .collect()
        })
        .unwrap_or_default();
    let expired: Vec<String> = known
        .iter()
        .filter(|(_, t)| {
            t.parse::<k8s_openapi::jiff::Timestamp>()
                .is_ok_and(|t| t < now)
        })
        .map(|(n, _)| n.clone())
        .collect();
    for name in expired {
        let gone = match secrets.delete(&name, &DeleteParams::default()).await {
            Ok(_) => true,
            Err(kube::Error::Api(e)) if e.code == 404 => true,
            Err(e) => {
                tracing::warn!(token = %name, error = %e, "couldn't remove an expired join token");
                false
            }
        };
        if gone {
            known.remove(&name);
        }
    }
    let body: Secret = serde_json::from_value(json!({
        "apiVersion": "v1", "kind": "Secret", "type": "bootstrap.kubernetes.io/token",
        "metadata": { "name": format!("bootstrap-token-{id}"), "namespace": "kube-system",
                      "labels": { "app.kubernetes.io/managed-by": "pyo", "pyo/join-token": "true" } },
        "stringData": {
            "description": "Pyo: join a server",
            "token-id": id, "token-secret": secret, "expiration": expires.to_string(),
            "usage-bootstrap-authentication": "true", "usage-bootstrap-signing": "true",
            "auth-extra-groups": "system:bootstrappers:k3s:default-node-token",
        },
    }))?;
    secrets
        .create(&kube::api::PostParams::default(), &body)
        .await?;
    known.insert(format!("bootstrap-token-{id}"), expires.to_string());
    let record = json!({
        "apiVersion": "v1", "kind": "Secret",
        "metadata": { "name": RECORD, "labels": { "app.kubernetes.io/managed-by": "pyo" } },
        "stringData": known,
    });
    records
        .patch(
            RECORD,
            &PatchParams::apply(crate::MANAGER).force(),
            &kube::api::Patch::Apply(&record),
        )
        .await?;
    Ok(format!("Join token {id} valid until {expires}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn labels(pairs: &[(&str, &str)]) -> std::collections::BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    #[test]
    fn only_a_volumes_own_claim_is_deleted() {
        let ours = labels(&[
            ("app.kubernetes.io/managed-by", "pyo"),
            (VOLUME_LABEL, "vol_a"),
        ]);
        assert!(volume_claim_refusal(&ours, "vol_a").is_none());
        assert!(
            volume_claim_refusal(&ours, "vol_b").is_some(),
            "another volume's"
        );
        assert!(volume_claim_refusal(&ours, "").is_some());
        assert!(
            volume_claim_refusal(&labels(&[(VOLUME_LABEL, "vol_a")]), "vol_a").is_some(),
            "not made by Pyo"
        );
        assert!(
            volume_claim_refusal(&labels(&[("app.kubernetes.io/managed-by", "pyo")]), "vol_a")
                .is_some(),
            "unlabelled"
        );
    }

    #[test]
    fn claim_users_are_running_pods_that_mount_it() {
        let pod = |name: &str, claim: &str, phase: &str, leaving: bool| -> Pod {
            let mut p: Pod = serde_json::from_value(json!({
                "metadata": { "name": name },
                "spec": { "containers": [{ "name": "c" }], "volumes": [{ "name": "v", "persistentVolumeClaim": { "claimName": claim } }] },
                "status": { "phase": phase },
            }))
            .unwrap();
            if leaving {
                p.metadata.deletion_timestamp =
                    Some(k8s_openapi::apimachinery::pkg::apis::meta::v1::Time(
                        k8s_openapi::jiff::Timestamp::now(),
                    ));
            }
            p
        };
        let pods = [
            pod("web-1", "web-data", "Running", false),
            pod("web-0", "web-data", "Running", true),
            pod("job", "web-data", "Succeeded", false),
            pod("api", "api-data", "Running", false),
        ];
        assert_eq!(
            claim_users(&pods, "web-data"),
            vec![("web-1".to_owned(), false), ("web-0".to_owned(), true)]
        );
        assert!(claim_users(&pods, "other").is_empty());
    }

    /// Needs a cluster: KUBECONFIG=… cargo test -p pyo-agent -- --ignored actions::tests
    #[tokio::test]
    #[ignore]
    async fn deletes_only_labelled_unmounted_volume_claims_on_a_cluster() {
        use kube::api::PostParams;
        let client = kube::Client::try_default().await.unwrap();
        let ns_api = Api::<Namespace>::all(client.clone());
        for (name, labels) in [
            (
                "pyo-t-vols",
                json!({ "pyo.dev/managed": "true", "pyo/project-id": "prj_t" }),
            ),
            ("pyo-t-plain", json!({})),
        ] {
            let ns: Namespace =
                serde_json::from_value(json!({ "metadata": { "name": name, "labels": labels } }))
                    .unwrap();
            let _ = ns_api.create(&PostParams::default(), &ns).await;
        }
        let claim = |ns: &str, name: &str, labels: serde_json::Value| {
            let api = Api::<PersistentVolumeClaim>::namespaced(client.clone(), ns);
            let c: PersistentVolumeClaim = serde_json::from_value(json!({ "metadata": { "name": name, "labels": labels },
                "spec": { "accessModes": ["ReadWriteOnce"], "resources": { "requests": { "storage": "1Gi" } } } })).unwrap();
            async move {
                let _ = api.create(&PostParams::default(), &c).await;
            }
        };
        let ours = |id: &str| json!({ "app.kubernetes.io/managed-by": "pyo", VOLUME_LABEL: id });
        claim("pyo-t-vols", "free", ours("vol_free")).await;
        claim("pyo-t-vols", "used", ours("vol_used")).await;
        claim("pyo-t-vols", "bare", json!({})).await;
        claim("pyo-t-plain", "elsewhere", ours("vol_else")).await;
        let pod: Pod = serde_json::from_value(json!({ "metadata": { "name": "user" }, "spec": {
            "containers": [{ "name": "c", "image": "busybox:1.36", "command": ["sleep", "3600"] }],
            "volumes": [{ "name": "v", "persistentVolumeClaim": { "claimName": "used" } }] } }))
        .unwrap();
        let _ = Api::<Pod>::namespaced(client.clone(), "pyo-t-vols")
            .create(&PostParams::default(), &pod)
            .await;
        let wait = std::time::Duration::from_secs(20);

        assert_eq!(
            delete_volume(&client, "pyo-t-vols", "free", "vol_free", wait)
                .await
                .unwrap(),
            "Deleted free"
        );
        assert!(
            delete_volume(&client, "pyo-t-vols", "free", "vol_free", wait)
                .await
                .unwrap()
                .contains("already gone")
        );
        // Another volume's id, or no label: refused.
        assert!(
            delete_volume(&client, "pyo-t-vols", "used", "vol_other", wait)
                .await
                .unwrap_err()
                .to_string()
                .contains("isn't volume")
        );
        assert!(
            delete_volume(&client, "pyo-t-vols", "bare", "vol_bare", wait)
                .await
                .is_err()
        );
        // Mounted by a running pod: refused, and kept.
        let e = delete_volume(&client, "pyo-t-vols", "used", "vol_used", wait)
            .await
            .unwrap_err()
            .to_string();
        assert!(e.contains("still in use by user"), "{e}");
        let pvcs = Api::<PersistentVolumeClaim>::namespaced(client.clone(), "pyo-t-vols");
        assert!(
            pvcs.get("used")
                .await
                .unwrap()
                .metadata
                .deletion_timestamp
                .is_none()
        );
        // Not a project's namespace.
        assert!(
            delete_volume(&client, "pyo-t-plain", "elsewhere", "vol_else", wait)
                .await
                .unwrap_err()
                .to_string()
                .contains("isn't a project's namespace")
        );

        for n in ["pyo-t-vols", "pyo-t-plain"] {
            let _ = ns_api.delete(n, &DeleteParams::background()).await;
        }
    }

    /// The admission policy, for requests as the agent: a claim labelled as
    /// a volume in a project's namespace may be deleted; an unlabelled one,
    /// one elsewhere, or a label added later, not. Applies the agent's roles
    /// and policy, and removes them after. Needs a cluster (cluster-admin):
    /// KUBECONFIG=… cargo test -p pyo-agent -- --ignored actions::tests
    #[tokio::test]
    #[ignore]
    async fn admission_policy_guards_volume_claims_on_a_cluster() {
        use kube::api::PostParams;
        use pyo_proto::rbac;
        let admin = kube::Client::try_default().await.unwrap();
        let installed: Vec<serde_json::Value> = rbac::objects(false)
            .into_iter()
            .chain(rbac::admission("w", "g"))
            .collect();
        for o in &installed {
            let (api, name) = crate::scope::api_for(&admin, o);
            api.patch(
                &name,
                &PatchParams::apply("pyo-test").force(),
                &Patch::Apply(o),
            )
            .await
            .unwrap_or_else(|e| panic!("{name}: {e}"));
        }
        let ns_api = Api::<Namespace>::all(admin.clone());
        for (name, labels) in [
            (
                "pyo-t-adm-project",
                json!({ "pyo.dev/managed": "true", "pyo/project-id": "prj_t" }),
            ),
            ("pyo-t-adm-managed", json!({ "pyo.dev/managed": "true" })),
        ] {
            let ns: Namespace =
                serde_json::from_value(json!({ "metadata": { "name": name, "labels": labels } }))
                    .unwrap();
            let _ = ns_api.create(&PostParams::default(), &ns).await;
            let b = rbac::write_binding(name);
            let (api, n) = crate::scope::api_for(&admin, &b);
            api.patch(
                &n,
                &PatchParams::apply("pyo-test").force(),
                &Patch::Apply(&b),
            )
            .await
            .unwrap();
        }
        let claim = |ns: &str, name: &str, labels: serde_json::Value| {
            let api = Api::<PersistentVolumeClaim>::namespaced(admin.clone(), ns);
            let c: PersistentVolumeClaim = serde_json::from_value(json!({ "metadata": { "name": name, "labels": labels },
                "spec": { "accessModes": ["ReadWriteOnce"], "resources": { "requests": { "storage": "1Gi" } } } })).unwrap();
            async move {
                let _ = api.create(&PostParams::default(), &c).await;
            }
        };
        let ours = json!({ "app.kubernetes.io/managed-by": "pyo", VOLUME_LABEL: "vol_t" });
        claim("pyo-t-adm-project", "ours", ours.clone()).await;
        claim(
            "pyo-t-adm-project",
            "bare",
            json!({ "app.kubernetes.io/managed-by": "pyo" }),
        )
        .await;
        claim("pyo-t-adm-managed", "ours", ours.clone()).await;
        // Policies take a moment to be enforced.
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;

        let mut config = kube::Config::infer().await.unwrap();
        config.auth_info.impersonate = Some(rbac::AGENT_USER.into());
        let agent = kube::Client::try_from(config).unwrap();
        let as_agent = |ns: &str| Api::<PersistentVolumeClaim>::namespaced(agent.clone(), ns);
        let denied = |r: Result<_, kube::Error>| match r {
            Err(kube::Error::Api(e)) => {
                assert!(
                    e.message
                        .contains("only deletes the claims it made for a volume"),
                    "{}",
                    e.message
                );
            }
            Ok(_) => panic!("allowed"),
            Err(e) => panic!("{e}"),
        };
        // Unlabelled, or in a namespace that isn't a project's: refused.
        denied(
            as_agent("pyo-t-adm-project")
                .delete("bare", &DeleteParams::default())
                .await
                .map(|_| ()),
        );
        denied(
            as_agent("pyo-t-adm-managed")
                .delete("ours", &DeleteParams::default())
                .await
                .map(|_| ()),
        );
        // Labelling one after the fact: refused.
        let label = Patch::Merge(json!({ "metadata": { "labels": { VOLUME_LABEL: "vol_x" } } }));
        denied(
            as_agent("pyo-t-adm-project")
                .patch("bare", &PatchParams::default(), &label)
                .await
                .map(|_| ()),
        );
        // A volume's own claim in a project's namespace: deleted.
        as_agent("pyo-t-adm-project")
            .delete("ours", &DeleteParams::default())
            .await
            .unwrap();

        for n in ["pyo-t-adm-project", "pyo-t-adm-managed"] {
            let _ = ns_api.delete(n, &DeleteParams::background()).await;
        }
        for o in installed.iter().rev() {
            if o["kind"] == "Namespace" {
                continue;
            }
            let (api, name) = crate::scope::api_for(&admin, o);
            let _ = api.delete(&name, &DeleteParams::default()).await;
        }
    }

    /// Needs a cluster: KUBECONFIG=… cargo test -p pyo-agent -- --ignored actions::tests
    #[tokio::test]
    #[ignore]
    async fn deletes_only_its_projects_namespace_on_a_cluster() {
        use kube::api::PostParams;
        let client = kube::Client::try_default().await.unwrap();
        let api = Api::<Namespace>::all(client.clone());
        let make = |name: &str, labels: serde_json::Value| {
            let api = api.clone();
            let ns: Namespace =
                serde_json::from_value(json!({ "metadata": { "name": name, "labels": labels } }))
                    .unwrap();
            async move {
                let _ = api.create(&PostParams::default(), &ns).await;
            }
        };
        let ours = json!({ "pyo.dev/managed": "true", "pyo/project-id": "prj_t" });
        make("pyo-t-gone", ours.clone()).await;
        make(
            "pyo-t-other",
            json!({ "pyo.dev/managed": "true", "pyo/project-id": "prj_other" }),
        )
        .await;
        make("pyo-t-vol", ours.clone()).await;
        let claim: PersistentVolumeClaim = serde_json::from_value(json!({ "metadata": { "name": "data" },
            "spec": { "accessModes": ["ReadWriteOnce"], "resources": { "requests": { "storage": "1Gi" } } } })).unwrap();
        Api::<PersistentVolumeClaim>::namespaced(client.clone(), "pyo-t-vol")
            .create(&PostParams::default(), &claim)
            .await
            .unwrap();

        assert_eq!(
            delete_namespace(&client, "pyo-t-gone", "prj_t")
                .await
                .unwrap(),
            "Deleting namespace pyo-t-gone"
        );
        assert!(
            api.get("pyo-t-gone")
                .await
                .unwrap()
                .metadata
                .deletion_timestamp
                .is_some()
        );
        // Again (a resend, or another cluster): nothing to do.
        assert!(
            delete_namespace(&client, "pyo-t-gone", "prj_t")
                .await
                .is_ok()
        );
        assert!(
            delete_namespace(&client, "pyo-t-never", "prj_t")
                .await
                .unwrap()
                .contains("already gone")
        );
        // Another project's: refused, and kept.
        assert!(
            delete_namespace(&client, "pyo-t-other", "prj_t")
                .await
                .is_err()
        );
        assert!(
            api.get("pyo-t-other")
                .await
                .unwrap()
                .metadata
                .deletion_timestamp
                .is_none()
        );
        // Volumes are kept, so is their namespace.
        assert!(
            delete_namespace(&client, "pyo-t-vol", "prj_t")
                .await
                .unwrap()
                .starts_with("Kept namespace pyo-t-vol")
        );
        assert!(
            api.get("pyo-t-vol")
                .await
                .unwrap()
                .metadata
                .deletion_timestamp
                .is_none()
        );
        // Never a system namespace, whatever its labels.
        assert!(
            delete_namespace(&client, "kube-system", "prj_t")
                .await
                .is_err()
        );

        for n in ["pyo-t-other", "pyo-t-vol"] {
            let _ = api.delete(n, &DeleteParams::background()).await;
        }
    }
}
