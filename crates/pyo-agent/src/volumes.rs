//! A volume's restore (its service stopped around it), growth, and move to
//! another project's namespace.
//!
//! A move keeps the data where it is: the volume behind the claim is kept
//! (its reclaim policy held at Retain), the old claim deleted, and the
//! volume bound to a claim of the same name in the new namespace. Its
//! reclaim policy goes back to what it was once it's bound there; if
//! anything fails it's bound back to where it was, or left at Retain so
//! the data is never lost.

use std::time::Duration;

use k8s_openapi::api::{
    apps::v1::{Deployment, StatefulSet},
    core::v1::{Namespace, PersistentVolume, PersistentVolumeClaim, Pod},
};
use kube::{
    Api, ResourceExt,
    api::{DeleteParams, ListParams, Patch, PatchParams},
};
use pyo_proto::{CommandResult, MoveVolumeSpec, RestoreSpec, ServiceRef};
use serde_json::{Value, json};

use crate::{
    MANAGER,
    actions::{claim_users, volume_claim_refusal, wait_let_go},
    failed,
    observe::{SERVICE_LABEL, VOLUME_LABEL},
};

/// How long a stopped service's pods get to let go of the volume.
const LET_GO: Duration = Duration::from_secs(180);
/// How long a move waits for stopping pods to let go of the volume.
const LET_GO_MOVE: Duration = Duration::from_secs(120);

/// The replicas a Deployment or StatefulSet asks for; None when it isn't
/// there (never deployed) or isn't one.
async fn replicas_of(client: &kube::Client, s: &ServiceRef) -> anyhow::Result<Option<i32>> {
    Ok(match s.workload_kind.as_str() {
        "StatefulSet" => Api::<StatefulSet>::namespaced(client.clone(), &s.namespace)
            .get_opt(&s.name)
            .await?
            .map(|w| w.spec.and_then(|s| s.replicas).unwrap_or(1)),
        "Deployment" | "" => Api::<Deployment>::namespaced(client.clone(), &s.namespace)
            .get_opt(&s.name)
            .await?
            .map(|w| w.spec.and_then(|s| s.replicas).unwrap_or(1)),
        _ => None,
    })
}

async fn set_replicas(client: &kube::Client, s: &ServiceRef, n: i32) -> anyhow::Result<()> {
    let body = Patch::Merge(json!({ "spec": { "replicas": n } }));
    if s.workload_kind == "StatefulSet" {
        Api::<StatefulSet>::namespaced(client.clone(), &s.namespace)
            .patch(&s.name, &PatchParams::default(), &body)
            .await?;
    } else {
        Api::<Deployment>::namespaced(client.clone(), &s.namespace)
            .patch(&s.name, &PatchParams::default(), &body)
            .await?;
    }
    Ok(())
}

/// Waits until no pod mounts `claim` in `ns`. Ones not stopping after
/// `wait` fail it.
async fn wait_unmounted(
    client: &kube::Client,
    ns: &str,
    claim: &str,
    wait: Duration,
) -> anyhow::Result<()> {
    let pods = Api::<Pod>::namespaced(client.clone(), ns);
    let deadline = tokio::time::Instant::now() + wait;
    loop {
        let users = claim_users(&pods.list(&ListParams::default()).await?.items, claim);
        if users.is_empty() {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            let names = users
                .iter()
                .map(|(n, _)| n.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            anyhow::bail!("{claim} is still in use by {names}.");
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

/// How long a restore waits for its service to be up again.
const STARTING: Duration = Duration::from_secs(180);

/// Whether the service has `n` ready replicas within `wait`.
async fn wait_ready(client: &kube::Client, s: &ServiceRef, n: i32, wait: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + wait;
    loop {
        let ready = match s.workload_kind.as_str() {
            "StatefulSet" => Api::<StatefulSet>::namespaced(client.clone(), &s.namespace)
                .get_opt(&s.name)
                .await
                .ok()
                .flatten()
                .and_then(|w| w.status?.ready_replicas),
            _ => Api::<Deployment>::namespaced(client.clone(), &s.namespace)
                .get_opt(&s.name)
                .await
                .ok()
                .flatten()
                .and_then(|w| w.status?.ready_replicas),
        };
        if ready.unwrap_or(0) >= n {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

/// RestoreVolume: stop the service, restore, start it again at the size it
/// had (also when the restore failed), and wait a while for it to be up.
pub async fn restore(client: &kube::Client, spec: RestoreSpec) -> CommandResult {
    let mut stopped: Option<(ServiceRef, i32)> = None;
    if let Some(s) = &spec.service {
        match replicas_of(client, s).await {
            Ok(Some(n)) => {
                if n > 0
                    && let Err(e) = set_replicas(client, s, 0).await
                {
                    return failed(format!("Couldn't stop {}: {e}", s.name));
                }
                stopped = Some((s.clone(), n));
            }
            Ok(None) => {}
            Err(e) => return failed(format!("Couldn't read {}: {e}", s.name)),
        }
    }
    let restored = async {
        wait_unmounted(client, &spec.namespace, &spec.claim, LET_GO).await?;
        crate::backup::restore_job(client, &spec).await
    }
    .await;
    let mut started = Ok(());
    let mut up = true;
    if let Some((s, n)) = &stopped
        && *n > 0
    {
        started = set_replicas(client, s, *n)
            .await
            .map_err(|e| format!("Couldn't start {} again: {e}", s.name));
        // Done once it's up again: until then the service shows as restoring.
        if started.is_ok() && restored.is_ok() {
            up = wait_ready(client, s, *n, STARTING).await;
        }
    }
    match (restored, started) {
        (Ok(()), Ok(())) if !up => crate::ok(&format!(
            "Restored; {} is still starting",
            stopped.map(|(s, _)| s.name).unwrap_or_default()
        )),
        (Ok(()), Ok(())) => crate::ok("Restored"),
        (Ok(()), Err(e)) => failed(format!("Restored, but: {e}")),
        (Err(e), Ok(())) => failed(format!("{e:#}")),
        (Err(e), Err(s)) => failed(format!("{e:#}. {s}")),
    }
}

/// ResizeVolume: ask for a bigger claim (never smaller).
pub async fn resize(
    client: &kube::Client,
    ns: &str,
    name: &str,
    volume_id: &str,
    size_gi: i64,
) -> anyhow::Result<String> {
    let claims = Api::<PersistentVolumeClaim>::namespaced(client.clone(), ns);
    let claim = claims.get(name).await?;
    if let Some(why) = volume_claim_refusal(claim.labels(), volume_id) {
        anyhow::bail!("Didn't resize {name}: {why}");
    }
    let now = claim
        .spec
        .as_ref()
        .and_then(|s| s.resources.as_ref())
        .and_then(|r| r.requests.as_ref())
        .and_then(|r| r.get("storage"))
        .map(|q| q.0.clone())
        .unwrap_or_default();
    if gib_of(&now).is_some_and(|g| g >= size_gi as f64) {
        return Ok(format!("{name} is already {now}"));
    }
    claims.patch(name, &PatchParams::default(), &Patch::Merge(json!({ "spec": { "resources": { "requests": { "storage": format!("{size_gi}Gi") } } } }))).await?;
    Ok(format!("Growing {name} to {size_gi} GiB"))
}

/// "5Gi", "5368709120", "5G": in GiB.
fn gib_of(q: &str) -> Option<f64> {
    let q = q.trim();
    let split = q.find(|c: char| c.is_ascii_alphabetic()).unwrap_or(q.len());
    let (n, unit) = q.split_at(split);
    let n: f64 = n.parse().ok()?;
    let mult = match unit {
        "" => 1.0,
        "Ki" => 1024.0,
        "Mi" => 1024.0 * 1024.0,
        "Gi" => 1024.0 * 1024.0 * 1024.0,
        "Ti" => 1024.0_f64.powi(4),
        "k" => 1e3,
        "M" => 1e6,
        "G" => 1e9,
        "T" => 1e12,
        _ => return None,
    };
    Some(n * mult / 1024.0_f64.powi(3))
}

/// The claim a moved volume gets: the old one's spec, bound to `pv`.
fn moved_claim(
    spec: &MoveVolumeSpec,
    old: &PersistentVolumeClaim,
    pv: Option<&PersistentVolume>,
) -> Value {
    let old_spec = old.spec.clone().unwrap_or_default();
    let storage = pv
        .and_then(|p| {
            p.spec
                .as_ref()?
                .capacity
                .as_ref()?
                .get("storage")
                .map(|q| q.0.clone())
        })
        .or_else(|| {
            old_spec
                .resources
                .as_ref()?
                .requests
                .as_ref()?
                .get("storage")
                .map(|q| q.0.clone())
        })
        .unwrap_or_else(|| "1Gi".into());
    let mut body = json!({
        "apiVersion": "v1", "kind": "PersistentVolumeClaim",
        "metadata": { "name": spec.name, "labels": { "app.kubernetes.io/managed-by": MANAGER, VOLUME_LABEL: spec.volume_id, SERVICE_LABEL: spec.service_id } },
        "spec": { "accessModes": old_spec.access_modes.unwrap_or_else(|| vec!["ReadWriteOnce".into()]), "resources": { "requests": { "storage": storage } } },
    });
    if let Some(c) = pv
        .and_then(|p| p.spec.as_ref()?.storage_class_name.clone())
        .or(old_spec.storage_class_name)
    {
        body["spec"]["storageClassName"] = json!(c);
    }
    if let Some(m) = old_spec.volume_mode {
        body["spec"]["volumeMode"] = json!(m);
    }
    if let Some(p) = pv {
        body["spec"]["volumeName"] = json!(p.name_any());
    }
    body
}

async fn wait_gone(
    claims: &Api<PersistentVolumeClaim>,
    name: &str,
    uid: Option<String>,
) -> anyhow::Result<()> {
    for _ in 0..60 {
        if claims.get_opt(name).await?.is_none_or(|c| c.uid() != uid) {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    anyhow::bail!("{name} is still being removed.")
}

async fn wait_bound(claims: &Api<PersistentVolumeClaim>, name: &str) -> anyhow::Result<()> {
    for _ in 0..60 {
        if claims
            .get_opt(name)
            .await?
            .and_then(|c| c.status)
            .and_then(|s| s.phase)
            .as_deref()
            == Some("Bound")
        {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    anyhow::bail!("{name} didn't take its data in time.")
}

/// Hold the volume to `claim` in `ns`: pre-bound, so only that claim gets it.
async fn bind_to(
    pvs: &Api<PersistentVolume>,
    pv: &str,
    ns: &str,
    claim: &str,
) -> anyhow::Result<()> {
    let body = json!({ "spec": { "claimRef": { "apiVersion": "v1", "kind": "PersistentVolumeClaim", "namespace": ns, "name": claim, "uid": null, "resourceVersion": null } } });
    pvs.patch(pv, &PatchParams::default(), &Patch::Merge(body))
        .await?;
    Ok(())
}

async fn set_policy(
    pvs: &Api<PersistentVolume>,
    pv: &str,
    volume_id: &str,
    policy: &str,
) -> anyhow::Result<()> {
    let body = json!({ "metadata": { "labels": { VOLUME_LABEL: volume_id } }, "spec": { "persistentVolumeReclaimPolicy": policy } });
    pvs.patch(pv, &PatchParams::default(), &Patch::Merge(body))
        .await?;
    Ok(())
}

/// MoveVolume.
pub async fn move_claim(client: &kube::Client, spec: &MoveVolumeSpec) -> anyhow::Result<String> {
    let name = spec.name.as_str();
    let from = Api::<PersistentVolumeClaim>::namespaced(client.clone(), &spec.from_namespace);
    let to = Api::<PersistentVolumeClaim>::namespaced(client.clone(), &spec.to_namespace);
    if spec.from_namespace == spec.to_namespace {
        return Ok(format!("{name} is already in {}", spec.to_namespace));
    }
    let old = from.get_opt(name).await?;
    if let Some(there) = to.get_opt(name).await? {
        // Asked again after it moved.
        if there.labels().get(VOLUME_LABEL) == Some(&spec.volume_id) && old.is_none() {
            return Ok(format!("{name} is already in {}", spec.to_namespace));
        }
        anyhow::bail!("{} already has a volume named {name}.", spec.to_namespace);
    }
    let Some(old) = old else {
        anyhow::bail!("{name} isn't in {} any more.", spec.from_namespace)
    };
    if let Some(why) = volume_claim_refusal(old.labels(), &spec.volume_id) {
        anyhow::bail!("Didn't move {name}: {why}");
    }
    // A replica stopping after a deploy (detached from it) is waited for.
    if let Some(busy) = wait_let_go(client, &spec.from_namespace, name, LET_GO_MOVE).await? {
        anyhow::bail!(
            "{}",
            busy.message(
                name,
                "Deploy the service that used it so it lets go, then try again."
            )
        );
    }
    // The new project's namespace, as a deploy makes it.
    let ns_body =
        crate::rollout::project_namespace(&spec.to_namespace, &spec.project_id, &spec.team_id);
    Api::<Namespace>::all(client.clone())
        .patch(
            &spec.to_namespace,
            &PatchParams::apply(MANAGER).force(),
            &Patch::Apply(&ns_body),
        )
        .await?;
    crate::scope::ensure_managed(client, &spec.to_namespace).await?;

    let apply = PatchParams::apply(MANAGER).force();
    let bound = old.status.as_ref().and_then(|s| s.phase.as_deref()) == Some("Bound");
    let pv_name = old
        .spec
        .as_ref()
        .and_then(|s| s.volume_name.clone())
        .filter(|n| bound && !n.is_empty());
    let Some(pv_name) = pv_name else {
        // Never used: no data yet. A new claim there; the old one goes.
        to.patch(name, &apply, &Patch::Apply(moved_claim(spec, &old, None)))
            .await?;
        let dp = DeleteParams {
            preconditions: Some(kube::api::Preconditions {
                uid: old.uid(),
                resource_version: None,
            }),
            ..DeleteParams::background()
        };
        let _ = from.delete(name, &dp).await;
        return Ok(format!("Moved {name} to {}", spec.to_namespace));
    };
    let pvs = Api::<PersistentVolume>::all(client.clone());
    let pv = pvs.get(&pv_name).await?;
    let policy = pv
        .spec
        .as_ref()
        .and_then(|s| s.persistent_volume_reclaim_policy.clone())
        .unwrap_or_else(|| "Delete".into());
    // Its data stays when the old claim goes.
    set_policy(&pvs, &pv_name, &spec.volume_id, "Retain").await?;
    let moved = async {
        let dp = DeleteParams {
            preconditions: Some(kube::api::Preconditions {
                uid: old.uid(),
                resource_version: None,
            }),
            ..DeleteParams::background()
        };
        from.delete(name, &dp).await?;
        wait_gone(&from, name, old.uid()).await?;
        bind_to(&pvs, &pv_name, &spec.to_namespace, name).await?;
        to.patch(
            name,
            &apply,
            &Patch::Apply(moved_claim(spec, &old, Some(&pv))),
        )
        .await?;
        wait_bound(&to, name).await
    }
    .await;
    match moved {
        Ok(()) => {
            if let Err(e) = set_policy(&pvs, &pv_name, &spec.volume_id, &policy).await {
                tracing::warn!(volume = %spec.volume_id, error = %e, "moved a volume; couldn't put its reclaim policy back");
            }
            Ok(format!("Moved {name} to {}", spec.to_namespace))
        }
        Err(e) => {
            // Back where it was, if it can be; the data is kept either way.
            let back = async {
                if let Some(made) = to
                    .get_opt(name)
                    .await?
                    .filter(|c| c.labels().get(VOLUME_LABEL) == Some(&spec.volume_id))
                {
                    let _ = to.delete(name, &DeleteParams::background()).await;
                    wait_gone(&to, name, made.uid()).await?;
                }
                if from.get_opt(name).await?.is_none() {
                    bind_to(&pvs, &pv_name, &spec.from_namespace, name).await?;
                    let mut back = spec.clone();
                    back.service_id = old.labels().get(SERVICE_LABEL).cloned().unwrap_or_default();
                    from.patch(
                        name,
                        &apply,
                        &Patch::Apply(moved_claim(&back, &old, Some(&pv))),
                    )
                    .await?;
                    wait_bound(&from, name).await?;
                }
                set_policy(&pvs, &pv_name, &spec.volume_id, &policy).await
            }
            .await;
            match back {
                Ok(()) => Err(anyhow::anyhow!(
                    "Couldn't move {name}: {e:#}. It's still in {}.",
                    spec.from_namespace
                )),
                Err(b) => Err(anyhow::anyhow!(
                    "Couldn't move {name}: {e:#}. Its data is kept (volume {pv_name}), but it couldn't be put back: {b:#}"
                )),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> MoveVolumeSpec {
        MoveVolumeSpec {
            volume_id: "vol_1".into(),
            name: "web-data".into(),
            from_namespace: "a".into(),
            to_namespace: "b".into(),
            project_id: "prj_b".into(),
            team_id: "t".into(),
            service_id: "svc_b".into(),
        }
    }

    #[test]
    fn a_moved_claim_takes_its_volume() {
        let old: PersistentVolumeClaim = serde_json::from_value(json!({
            "metadata": { "name": "web-data", "namespace": "a", "labels": { VOLUME_LABEL: "vol_1", SERVICE_LABEL: "svc_a" } },
            "spec": { "accessModes": ["ReadWriteOnce"], "storageClassName": "local-path", "resources": { "requests": { "storage": "5Gi" } }, "volumeName": "pvc-1" },
        }))
        .unwrap();
        let pv: PersistentVolume = serde_json::from_value(json!({ "metadata": { "name": "pvc-1" }, "spec": { "capacity": { "storage": "6Gi" }, "storageClassName": "local-path" } })).unwrap();
        let c = moved_claim(&spec(), &old, Some(&pv));
        assert_eq!(c["spec"]["volumeName"], "pvc-1");
        assert_eq!(
            c["spec"]["resources"]["requests"]["storage"], "6Gi",
            "as big as the volume"
        );
        assert_eq!(c["metadata"]["labels"][SERVICE_LABEL], "svc_b");
        assert_eq!(c["metadata"]["labels"][VOLUME_LABEL], "vol_1");
        let fresh = moved_claim(&spec(), &old, None);
        assert!(fresh["spec"].get("volumeName").is_none());
        assert_eq!(fresh["spec"]["resources"]["requests"]["storage"], "5Gi");
    }

    #[test]
    fn sizes() {
        assert_eq!(gib_of("5Gi"), Some(5.0));
        assert_eq!(gib_of("1024Mi"), Some(1.0));
        assert!(gib_of("5G").unwrap() < 5.0);
        assert_eq!(gib_of("x"), None);
    }

    /// Against a throwaway cluster with local-path (k3s), as the agent's own
    /// ServiceAccount under its roles and admission policy: back a volume
    /// up, change it, restore it with its service stopped and started again;
    /// growing a local-path volume is refused; move it to another project's
    /// namespace with its data, and the policy keeps the agent from changing
    /// what a volume holds. Needs a restic S3 repository the cluster reaches.
    ///
    /// PYO_TEST_KUBECONFIG=<throwaway> PYO_TEST_RESTIC_REPO=s3:http://…/bucket PYO_TEST_S3_KEY=… PYO_TEST_S3_SECRET=…
    ///   [PYO_TEST_S3_REGION=garage]
    ///   cargo test -p pyo-agent -- --ignored volumes_against --nocapture
    #[tokio::test]
    #[ignore = "needs a throwaway Kubernetes API server and an S3 bucket"]
    async fn volumes_against_a_cluster() {
        use k8s_openapi::api::{
            authentication::v1::TokenRequest, batch::v1::Job, core::v1::ServiceAccount,
        };
        use kube::api::PostParams;
        use pyo_proto::rbac;
        let env = |k: &str| std::env::var(k).unwrap_or_else(|_| panic!("set {k}"));
        let kc = kube::config::Kubeconfig::read_from(env("PYO_TEST_KUBECONFIG")).unwrap();
        let config = kube::Config::from_custom_kubeconfig(kc, &Default::default())
            .await
            .unwrap();
        let admin = kube::Client::try_from(config.clone()).unwrap();
        let db = pyo_proto::system::DbImages {
            controller: "c".into(),
            kv: "k".into(),
            postgres: ["p16".into(), "p17".into(), "p18".into()],
            mysql_operator: "mo".into(),
            mysql_backup: "mb".into(),
        };
        let parts = rbac::InstallParts::new(
            false,
            true,
            "127.0.0.1:5005/pyo/waf:dev",
            "127.0.0.1:5005/pyo/geoip:dev",
            &db,
        );
        let pp = PatchParams::apply("pyo-test").force();
        for o in parts
            .access
            .iter()
            .chain(parts.admission.iter())
            .filter(|o| o["kind"] != "HelmChart")
        {
            let (api, name) = crate::scope::api_for(&admin, o);
            api.patch(&name, &pp, &Patch::Apply(o))
                .await
                .unwrap_or_else(|e| panic!("{name}: {e}"));
        }
        let vaps = crate::scope::api_for(&admin, &parts.admission[0]).0;
        for _ in 0..30 {
            let v = vaps.get(rbac::ADMISSION_POLICY).await.unwrap();
            if v.data["status"]["observedGeneration"].as_i64().is_some() {
                assert!(
                    v.data["status"]["typeChecking"]["expressionWarnings"]
                        .as_array()
                        .is_none_or(|w| w.is_empty()),
                    "{}",
                    v.data["status"]
                );
                break;
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
        let token = Api::<ServiceAccount>::namespaced(admin.clone(), rbac::NS)
            .create_token_request(
                rbac::SA,
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
        let sa = kube::Client::try_from(sa_config).unwrap();
        let region = std::env::var("PYO_TEST_S3_REGION").unwrap_or_else(|_| "us-east-1".into());
        let repo = pyo_proto::ResticRepo {
            repository: env("PYO_TEST_RESTIC_REPO"),
            password: "pyo-test".into(),
            access_key: env("PYO_TEST_S3_KEY"),
            secret_key: env("PYO_TEST_S3_SECRET"),
            region,
        };

        // Two projects' namespaces, as deploys make them.
        let (a, b, vol) = ("pyo-vt-a", "pyo-vt-b", "vol_vt1");
        for (ns, prj) in [(a, "prj_vta"), (b, "prj_vtb")] {
            let _ = Api::<Namespace>::all(admin.clone())
                .delete(ns, &Default::default())
                .await;
            while Api::<Namespace>::all(admin.clone())
                .get_opt(ns)
                .await
                .unwrap()
                .is_some()
            {
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
            Api::<Namespace>::all(sa.clone())
                .patch(
                    ns,
                    &PatchParams::apply(MANAGER).force(),
                    &Patch::Apply(crate::rollout::project_namespace(ns, prj, "team_t")),
                )
                .await
                .unwrap();
            crate::scope::ensure_managed(&sa, ns).await.unwrap();
        }
        let claims = Api::<PersistentVolumeClaim>::namespaced(sa.clone(), a);
        claims
            .patch("web-data", &PatchParams::apply(MANAGER).force(), &Patch::Apply(json!({
                "apiVersion": "v1", "kind": "PersistentVolumeClaim",
                "metadata": { "name": "web-data", "labels": { "app.kubernetes.io/managed-by": MANAGER, VOLUME_LABEL: vol, SERVICE_LABEL: "svc_web" } },
                "spec": { "accessModes": ["ReadWriteOnce"], "resources": { "requests": { "storage": "1Gi" } } },
            })))
            .await
            .unwrap();
        // A short Job on the volume; its output.
        let run = |ns: &'static str, name: &'static str, script: &'static str| {
            let jobs = Api::<Job>::namespaced(sa.clone(), ns);
            let pods = Api::<Pod>::namespaced(sa.clone(), ns);
            async move {
                let job: Job = serde_json::from_value(json!({ "metadata": { "name": name }, "spec": { "backoffLimit": 0, "template": { "metadata": { "labels": { "pyo-test": name } }, "spec": {
                    "restartPolicy": "Never",
                    "volumes": [{ "name": "d", "persistentVolumeClaim": { "claimName": "web-data" } }],
                    "containers": [{ "name": "c", "image": "busybox:1.37", "command": ["sh", "-c", script], "volumeMounts": [{ "name": "d", "mountPath": "/data" }] }],
                } } } }))
                .unwrap();
                jobs.create(&PostParams::default(), &job).await.unwrap();
                for _ in 0..120 {
                    let st = jobs.get(name).await.unwrap().status.unwrap_or_default();
                    if st.succeeded.unwrap_or(0) > 0 {
                        break;
                    }
                    assert_eq!(st.failed.unwrap_or(0), 0, "{name} failed");
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
                let pod = pods
                    .list(&ListParams::default().labels(&format!("pyo-test={name}")))
                    .await
                    .unwrap()
                    .items
                    .remove(0);
                let out = pods
                    .logs(&pod.name_any(), &Default::default())
                    .await
                    .unwrap();
                jobs.delete(name, &DeleteParams::background())
                    .await
                    .unwrap();
                for _ in 0..60 {
                    if pods
                        .list(&ListParams::default().labels(&format!("pyo-test={name}")))
                        .await
                        .unwrap()
                        .items
                        .is_empty()
                    {
                        break;
                    }
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
                out
            }
        };
        run(a, "write-one", "echo one > /data/hello && mkdir -p /data/sub && echo 1 > /data/sub/x && ln -s hello /data/link").await;

        // Backed up as the agent does it, then changed.
        let spec = pyo_proto::BackupSpec {
            backup_id: "bkp_vt1".into(),
            service: ServiceRef {
                service_id: String::new(),
                name: String::new(),
                namespace: a.into(),
                workload_kind: String::new(),
            },
            kind: "volume".into(),
            claim: "web-data".into(),
            engine: String::new(),
            tags: vec![format!("volume={vol}")],
            keep: 3,
            repository: repo.repository.clone(),
            password: repo.password.clone(),
            access_key: repo.access_key.clone(),
            secret_key: repo.secret_key.clone(),
            region: repo.region.clone(),
            connection: None,
        };
        let summary = crate::backup::backup(&sa, &spec).await.unwrap();
        let snapshot = summary["snapshotId"].as_str().unwrap().to_owned();
        run(
            a,
            "write-two",
            "echo two > /data/hello && echo new > /data/extra && rm -r /data/sub",
        )
        .await;

        // Listed, newest first.
        let listed = crate::backup::snapshots(
            &sa,
            pyo_proto::SnapshotsSpec {
                request_id: "snp_vt1".into(),
                namespace: a.into(),
                volume_id: vol.into(),
                repo: repo.clone(),
            },
        )
        .await;
        assert!(listed.ok, "{:?}", listed.message);
        let list = listed.data.unwrap()["snapshots"]
            .as_array()
            .unwrap()
            .clone();
        assert_eq!(list[0]["id"], json!(snapshot));
        assert!(
            list[0]["time"].as_i64().unwrap() > 0 && list[0]["sizeBytes"].as_i64().unwrap() > 0
        );

        // Another backup, then deleted: the list shrinks back; deleted again is done.
        let second = crate::backup::backup(
            &sa,
            &pyo_proto::BackupSpec {
                backup_id: "bkp_vt2".into(),
                ..spec.clone()
            },
        )
        .await
        .unwrap()["snapshotId"]
            .as_str()
            .unwrap()
            .to_owned();
        let list_now = |id: &'static str| {
            let (sa, repo) = (sa.clone(), repo.clone());
            async move {
                let r = crate::backup::snapshots(
                    &sa,
                    pyo_proto::SnapshotsSpec {
                        request_id: id.into(),
                        namespace: a.into(),
                        volume_id: vol.into(),
                        repo,
                    },
                )
                .await;
                assert!(r.ok, "{:?}", r.message);
                r.data.unwrap()["snapshots"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|s| s["id"].as_str().unwrap().to_owned())
                    .collect::<Vec<_>>()
            }
        };
        assert_eq!(list_now("snp-vt2").await.len(), 2);
        let forget = |id: &'static str, snapshot: String| {
            crate::backup::forget(
                &sa,
                pyo_proto::ForgetSpec {
                    request_id: id.into(),
                    namespace: a.into(),
                    volume_id: vol.into(),
                    snapshot_id: snapshot,
                    repo: repo.clone(),
                },
            )
        };
        // Not this volume's: nothing deleted.
        let other = crate::backup::forget(
            &sa,
            pyo_proto::ForgetSpec {
                request_id: "fgt_vt0".into(),
                namespace: a.into(),
                volume_id: "vol_other".into(),
                snapshot_id: second.clone(),
                repo: repo.clone(),
            },
        )
        .await;
        assert!(
            other.ok
                && other
                    .message
                    .as_deref()
                    .unwrap_or("")
                    .contains("already gone"),
            "{:?}",
            other.message
        );
        let gone = forget("fgt_vt1", second[..8].to_owned()).await;
        assert!(
            gone.ok && gone.message.as_deref().unwrap_or("").starts_with("Deleted"),
            "{:?}",
            gone.message
        );
        assert_eq!(list_now("snp-vt3").await, std::slice::from_ref(&snapshot));
        assert!(
            forget("fgt_vt2", second.clone())
                .await
                .message
                .unwrap_or_default()
                .contains("already gone")
        );

        // Its service runs; the restore stops it and starts it again.
        let web = ServiceRef {
            service_id: "svc_web".into(),
            name: "web".into(),
            namespace: a.into(),
            workload_kind: "Deployment".into(),
        };
        let deployments = Api::<Deployment>::namespaced(sa.clone(), a);
        deployments
            .patch("web", &PatchParams::apply(MANAGER).force(), &Patch::Apply(json!({
                "apiVersion": "apps/v1", "kind": "Deployment", "metadata": { "name": "web" },
                "spec": { "replicas": 1, "strategy": { "type": "Recreate" }, "selector": { "matchLabels": { SERVICE_LABEL: "svc_web" } }, "template": { "metadata": { "labels": { SERVICE_LABEL: "svc_web" } }, "spec": {
                    "volumes": [{ "name": "d", "persistentVolumeClaim": { "claimName": "web-data" } }],
                    "containers": [{ "name": "web", "image": "busybox:1.37", "command": ["sleep", "100000"], "volumeMounts": [{ "name": "d", "mountPath": "/data" }] }],
                } } },
            })))
            .await
            .unwrap();
        let ready = |n: i32| {
            let deployments = deployments.clone();
            async move {
                for _ in 0..120 {
                    if deployments
                        .get("web")
                        .await
                        .unwrap()
                        .status
                        .and_then(|s| s.ready_replicas)
                        .unwrap_or(0)
                        == n
                    {
                        return;
                    }
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
                panic!("web never had {n} ready");
            }
        };
        ready(1).await;
        // Not one of this volume's snapshots: refused, nothing touched, web back.
        let wrong = restore(
            &sa,
            RestoreSpec {
                restore_id: "rst_vt0".into(),
                namespace: a.into(),
                claim: "web-data".into(),
                volume_id: "vol_other".into(),
                snapshot_id: snapshot.clone(),
                service: Some(web.clone()),
                repo: repo.clone(),
            },
        )
        .await;
        assert!(
            !wrong.ok
                && wrong
                    .message
                    .as_deref()
                    .unwrap_or("")
                    .contains("isn't one of this volume's"),
            "{:?}",
            wrong.message
        );
        ready(1).await;
        assert_eq!(
            run(a, "read-zero", "cat /data/hello").await.trim(),
            "two",
            "a refused restore leaves the data"
        );
        let r = restore(
            &sa,
            RestoreSpec {
                restore_id: "rst_vt1".into(),
                namespace: a.into(),
                claim: "web-data".into(),
                volume_id: vol.into(),
                snapshot_id: snapshot.clone(),
                service: Some(web.clone()),
                repo: repo.clone(),
            },
        )
        .await;
        assert!(r.ok, "{:?}", r.message);
        assert_eq!(
            deployments.get("web").await.unwrap().spec.unwrap().replicas,
            Some(1),
            "started again"
        );
        ready(1).await;
        let after = run(
            a,
            "read-one",
            "cat /data/hello /data/sub/x; readlink /data/link; ls -A /data",
        )
        .await;
        let lines: Vec<&str> = after.lines().collect();
        assert_eq!(&lines[..3], ["one", "1", "hello"], "{after}");
        assert!(
            !after.contains("extra") && !after.contains(".pyo-"),
            "only the snapshot's files: {after}"
        );

        // local-path can't grow a volume: the cluster says so.
        let grow = resize(&sa, a, "web-data", vol, 5).await;
        assert!(grow.is_err(), "{grow:?}");
        println!("resize on local-path: {}", grow.unwrap_err());

        // Moved to project b: refused while web still mounts it.
        let mv = MoveVolumeSpec {
            volume_id: vol.into(),
            name: "web-data".into(),
            from_namespace: a.into(),
            to_namespace: b.into(),
            project_id: "prj_vtb".into(),
            team_id: "team_t".into(),
            service_id: "svc_api".into(),
        };
        let busy = move_claim(&sa, &mv).await.unwrap_err();
        assert!(busy.to_string().contains("still in use"), "{busy}");
        // Its replica stopping (as after a deploy without it; busybox's sleep
        // ignores SIGTERM, so it takes its whole grace period): the move
        // waits for it rather than failing.
        let pods = Api::<Pod>::namespaced(sa.clone(), a);
        let web_pod = claim_users(
            &pods.list(&ListParams::default()).await.unwrap().items,
            "web-data",
        )
        .remove(0)
        .0;
        deployments
            .delete("web", &DeleteParams::background())
            .await
            .unwrap();
        while pods
            .get(&web_pod)
            .await
            .unwrap()
            .metadata
            .deletion_timestamp
            .is_none()
        {
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        let pv_name = claims
            .get("web-data")
            .await
            .unwrap()
            .spec
            .unwrap()
            .volume_name
            .unwrap();
        move_claim(&sa, &mv).await.unwrap();
        assert!(
            pods.get_opt(&web_pod).await.unwrap().is_none(),
            "waited for the stopping replica"
        );
        assert!(
            claims.get_opt("web-data").await.unwrap().is_none(),
            "gone from a"
        );
        let moved = Api::<PersistentVolumeClaim>::namespaced(sa.clone(), b)
            .get("web-data")
            .await
            .unwrap();
        assert_eq!(
            moved.spec.as_ref().unwrap().volume_name.as_deref(),
            Some(pv_name.as_str()),
            "the same volume"
        );
        assert_eq!(
            moved.labels().get(SERVICE_LABEL).map(String::as_str),
            Some("svc_api")
        );
        let pv = Api::<PersistentVolume>::all(admin.clone())
            .get(&pv_name)
            .await
            .unwrap();
        assert_eq!(
            pv.spec
                .as_ref()
                .unwrap()
                .persistent_volume_reclaim_policy
                .as_deref(),
            Some("Delete"),
            "its policy back"
        );
        assert_eq!(pv.labels().get(VOLUME_LABEL).map(String::as_str), Some(vol));
        assert_eq!(
            run(b, "read-moved", "cat /data/hello").await.trim(),
            "one",
            "its data came along"
        );
        // Asked again: done already.
        assert!(move_claim(&sa, &mv).await.unwrap().contains("already"));

        // The policy: the agent can't change what a volume holds, or take one over by its label.
        let pvs = Api::<PersistentVolume>::all(sa.clone());
        let bigger = pvs
            .patch(
                &pv_name,
                &PatchParams::default(),
                &Patch::Merge(json!({ "spec": { "capacity": { "storage": "9Gi" } } })),
            )
            .await;
        assert!(
            bigger.is_err_and(|e| e.to_string().contains("rebinds")),
            "capacity"
        );
        let relabel = pvs
            .patch(
                &pv_name,
                &PatchParams::default(),
                &Patch::Merge(json!({ "metadata": { "labels": { VOLUME_LABEL: "vol_x" } } })),
            )
            .await;
        assert!(relabel.is_err(), "relabelled");
        let unlabel = pvs
            .patch(
                &pv_name,
                &PatchParams::default(),
                &Patch::Merge(json!({ "metadata": { "labels": { VOLUME_LABEL: null } } })),
            )
            .await;
        assert!(unlabel.is_err(), "label dropped");
        assert!(
            pvs.delete(&pv_name, &DeleteParams::default())
                .await
                .is_err(),
            "deleted"
        );

        // Never used: no data, a new claim there.
        claims
            .patch("fresh", &PatchParams::apply(MANAGER).force(), &Patch::Apply(json!({
                "apiVersion": "v1", "kind": "PersistentVolumeClaim",
                "metadata": { "name": "fresh", "labels": { "app.kubernetes.io/managed-by": MANAGER, VOLUME_LABEL: "vol_vt2" } },
                "spec": { "accessModes": ["ReadWriteOnce"], "resources": { "requests": { "storage": "1Gi" } } },
            })))
            .await
            .unwrap();
        let fresh = MoveVolumeSpec {
            volume_id: "vol_vt2".into(),
            name: "fresh".into(),
            ..mv.clone()
        };
        move_claim(&sa, &fresh).await.unwrap();
        assert!(
            Api::<PersistentVolumeClaim>::namespaced(sa.clone(), b)
                .get_opt("fresh")
                .await
                .unwrap()
                .is_some()
        );

        for ns in [a, b] {
            Api::<Namespace>::all(admin.clone())
                .delete(ns, &Default::default())
                .await
                .unwrap();
        }
    }
}
