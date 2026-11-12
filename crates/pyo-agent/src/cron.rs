//! A service's cron jobs: commands run on a schedule with the service's own
//! image, variables and resources, read from its live pods. So each deploy
//! carries them along, and changing one needs no redeploy.
//!
//! Their pods carry labels of their own, never the service's: those would
//! put them behind its Service and send them its traffic.

use k8s_openapi::api::{
    apps::v1::{Deployment, StatefulSet},
    batch::v1::CronJob,
};
use kube::{
    Api, Client, ResourceExt,
    api::{DeleteParams, ListParams, Patch, PatchParams},
};
use pyo_proto::{CronTask, ServiceRef};
use serde_json::{Value, json};

use crate::MANAGER;

/// On a cron job, its pods and runs: which one.
pub const CRON_LABEL: &str = "pyo/cron-id";
/// On a cron job, its pods and runs: whose.
pub const CRON_SERVICE_LABEL: &str = "pyo/cron-service";

/// The cron job's object name: the service's, then the job's id, within the
/// 52 characters a CronJob's name may have.
pub fn object_name(service: &ServiceRef, id: &str) -> String {
    let suffix: String = id
        .rsplit('_')
        .next()
        .unwrap_or(id)
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .take(12)
        .collect::<String>()
        .to_ascii_lowercase();
    let stem: String = service.name.chars().take(38).collect();
    format!("{}-{suffix}", stem.trim_end_matches('-'))
}

fn labels(service: &ServiceRef, id: &str) -> Value {
    json!({ CRON_LABEL: id, CRON_SERVICE_LABEL: service.service_id, "app.kubernetes.io/managed-by": MANAGER })
}

/// The service's pod, made into one that runs a command once.
pub fn manifest(service: &ServiceRef, task: &CronTask, pod: &Value) -> Value {
    let mut template = pod.clone();
    template["metadata"] = json!({ "labels": labels(service, &task.id) });
    let spec = &mut template["spec"];
    spec["restartPolicy"] = json!("Never");
    if let Some(containers) = spec["containers"].as_array_mut() {
        for c in containers.iter_mut() {
            if let Some(o) = c.as_object_mut() {
                for field in [
                    "readinessProbe",
                    "livenessProbe",
                    "startupProbe",
                    "lifecycle",
                    "ports",
                ] {
                    o.remove(field);
                }
            }
        }
        if let Some(first) = containers.first_mut() {
            first["command"] = json!(["sh", "-c", task.command]);
            first.as_object_mut().map(|o| o.remove("args"));
        }
    }
    json!({
        "apiVersion": "batch/v1", "kind": "CronJob",
        "metadata": { "name": object_name(service, &task.id), "namespace": service.namespace, "labels": labels(service, &task.id) },
        "spec": {
            "schedule": task.schedule,
            "timeZone": "Etc/UTC",
            "concurrencyPolicy": "Forbid",
            "startingDeadlineSeconds": 300,
            "successfulJobsHistoryLimit": 3,
            "failedJobsHistoryLimit": 3,
            "suspend": task.paused,
            "jobTemplate": { "metadata": { "labels": labels(service, &task.id) }, "spec": { "backoffLimit": 0, "template": template } },
        },
    })
}

/// The service's pod template, as it runs now.
async fn live_pod(client: &Client, service: &ServiceRef) -> anyhow::Result<Value> {
    let template = if service.workload_kind == "StatefulSet" {
        Api::<StatefulSet>::namespaced(client.clone(), &service.namespace)
            .get_opt(&service.name)
            .await?
            .and_then(|s| s.spec)
            .map(|s| s.template)
    } else {
        Api::<Deployment>::namespaced(client.clone(), &service.namespace)
            .get_opt(&service.name)
            .await?
            .and_then(|d| d.spec)
            .map(|s| s.template)
    };
    let template = template.ok_or_else(|| {
        anyhow::anyhow!("{} isn't running here yet. Deploy it first.", service.name)
    })?;
    Ok(serde_json::to_value(template)?)
}

/// Make the service's cron jobs match `jobs`. Returns how many there are.
pub async fn sync(
    client: &Client,
    service: &ServiceRef,
    jobs: &[CronTask],
) -> anyhow::Result<usize> {
    sync_list(client, service, jobs).await?;
    Ok(jobs.len())
}

pub async fn sync_list(
    client: &Client,
    service: &ServiceRef,
    jobs: &[CronTask],
) -> anyhow::Result<()> {
    let api = Api::<CronJob>::namespaced(client.clone(), &service.namespace);
    if !jobs.is_empty() {
        let pod = live_pod(client, service).await?;
        for task in jobs {
            let body: CronJob = serde_json::from_value(manifest(service, task, &pod))?;
            api.patch(
                &object_name(service, &task.id),
                &PatchParams::apply(MANAGER).force(),
                &Patch::Apply(&body),
            )
            .await?;
        }
    }
    let wanted: Vec<String> = jobs.iter().map(|t| object_name(service, &t.id)).collect();
    let existing = api
        .list(
            &ListParams::default().labels(&format!("{CRON_SERVICE_LABEL}={}", service.service_id)),
        )
        .await?
        .items;
    for old in existing.iter().filter(|c| !wanted.contains(&c.name_any())) {
        match api
            .delete(&old.name_any(), &DeleteParams::background())
            .await
        {
            Err(kube::Error::Api(e)) if e.code != 404 => return Err(kube::Error::Api(e).into()),
            Err(e @ kube::Error::Service(_)) => return Err(e.into()),
            _ => {}
        }
    }
    Ok(())
}

pub async fn run(client: &Client, service: &ServiceRef, id: &str) -> anyhow::Result<String> {
    crate::actions::run_job(client, &service.namespace, &object_name(service, id)).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::observe::SERVICE_LABEL;

    fn service() -> ServiceRef {
        ServiceRef {
            service_id: "svc_web".into(),
            name: "web".into(),
            namespace: "shop".into(),
            workload_kind: "Deployment".into(),
        }
    }

    fn task() -> CronTask {
        CronTask {
            id: "crn_abc123def456".into(),
            name: "Cleanup".into(),
            schedule: "0 3 * * *".into(),
            command: "npm run cleanup".into(),
            paused: false,
        }
    }

    fn pod() -> Value {
        json!({
            "metadata": { "labels": { SERVICE_LABEL: "svc_web", "app.kubernetes.io/name": "web" } },
            "spec": { "containers": [{ "name": "app", "image": "shop/web@sha256:1", "args": ["serve"], "envFrom": [{ "secretRef": { "name": "web-env-dpl" } }],
              "ports": [{ "containerPort": 3000 }], "readinessProbe": {}, "livenessProbe": {}, "lifecycle": {} }] },
        })
    }

    #[test]
    fn it_runs_the_services_image_and_variables() {
        let m = manifest(&service(), &task(), &pod());
        let c = &m["spec"]["jobTemplate"]["spec"]["template"]["spec"]["containers"][0];
        assert_eq!(c["image"], "shop/web@sha256:1");
        assert_eq!(c["envFrom"][0]["secretRef"]["name"], "web-env-dpl");
        assert_eq!(c["command"], json!(["sh", "-c", "npm run cleanup"]));
        assert!(
            c.get("args").is_none()
                && c.get("ports").is_none()
                && c.get("readinessProbe").is_none()
        );
        assert_eq!(
            m["spec"]["jobTemplate"]["spec"]["template"]["spec"]["restartPolicy"],
            "Never"
        );
    }

    /// The Service selects the service label: a cron pod with it would get traffic.
    #[test]
    fn its_pods_never_carry_the_services_label() {
        let m = manifest(&service(), &task(), &pod());
        let labels = &m["spec"]["jobTemplate"]["spec"]["template"]["metadata"]["labels"];
        assert!(labels.get(SERVICE_LABEL).is_none());
        assert_eq!(labels[CRON_LABEL], "crn_abc123def456");
        assert_eq!(labels[CRON_SERVICE_LABEL], "svc_web");
    }

    #[test]
    fn names_fit_and_stay_valid() {
        let long = ServiceRef {
            name: "a".repeat(63),
            ..service()
        };
        let name = object_name(&long, "crn_ABC123def456");
        assert!(name.len() <= 52, "{name}");
        assert!(
            name.chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        );
        assert_eq!(
            object_name(&service(), "crn_abc123def456"),
            "web-abc123def456"
        );
    }

    #[test]
    fn paused_means_suspended() {
        let m = manifest(
            &service(),
            &CronTask {
                paused: true,
                ..task()
            },
            &pod(),
        );
        assert_eq!(m["spec"]["suspend"], true);
    }

    /// Needs a cluster: KUBECONFIG=… cargo test -p pyo-agent -- --ignored cron::tests
    #[tokio::test]
    #[ignore]
    async fn service_cron_on_a_cluster() {
        use k8s_openapi::api::{batch::v1::Job, core::v1::Service};
        let client = Client::try_default().await.unwrap();
        let ns = "default";
        let service = ServiceRef {
            namespace: ns.into(),
            ..service()
        };
        let deployments = Api::<Deployment>::namespaced(client.clone(), ns);
        let body: Deployment = serde_json::from_value(json!({
            "apiVersion": "apps/v1", "kind": "Deployment", "metadata": { "name": "web", "labels": { SERVICE_LABEL: "svc_web" } },
            "spec": { "selector": { "matchLabels": { SERVICE_LABEL: "svc_web" } }, "template": {
                "metadata": { "labels": { SERVICE_LABEL: "svc_web" } },
                "spec": { "containers": [{ "name": "app", "image": "shop/web:1", "ports": [{ "containerPort": 3000 }] }] } } },
        })).unwrap();
        deployments
            .patch(
                "web",
                &PatchParams::apply("test").force(),
                &Patch::Apply(&body),
            )
            .await
            .unwrap();
        let svc: Service = serde_json::from_value(
            json!({ "apiVersion": "v1", "kind": "Service", "metadata": { "name": "web" },
            "spec": { "selector": { SERVICE_LABEL: "svc_web" }, "ports": [{ "port": 3000 }] } }),
        )
        .unwrap();
        Api::<Service>::namespaced(client.clone(), ns)
            .patch(
                "web",
                &PatchParams::apply("test").force(),
                &Patch::Apply(&svc),
            )
            .await
            .unwrap();

        let a = task();
        let b = CronTask {
            id: "crn_second000001".into(),
            name: "Report".into(),
            schedule: "*/10 * * * *".into(),
            command: "echo report".into(),
            paused: true,
        };
        assert_eq!(
            sync(&client, &service, &[a.clone(), b.clone()])
                .await
                .unwrap(),
            2
        );
        let crons = Api::<CronJob>::namespaced(client.clone(), ns);
        let live = crons.get(&object_name(&service, &a.id)).await.unwrap();
        assert_eq!(
            live.spec
                .job_template
                .spec
                .as_ref()
                .unwrap()
                .template
                .spec
                .as_ref()
                .unwrap()
                .containers[0]
                .image
                .as_deref(),
            Some("shop/web:1")
        );
        assert_eq!(
            crons
                .get(&object_name(&service, &b.id))
                .await
                .unwrap()
                .spec
                .suspend,
            Some(true)
        );

        // A deploy changes the image: the next sync carries it to the cron jobs.
        let mut newer = serde_json::to_value(&body).unwrap();
        newer["spec"]["template"]["spec"]["containers"][0]["image"] = json!("shop/web:2");
        let newer: Deployment = serde_json::from_value(newer).unwrap();
        deployments
            .patch(
                "web",
                &PatchParams::apply("test").force(),
                &Patch::Apply(&newer),
            )
            .await
            .unwrap();
        sync(&client, &service, std::slice::from_ref(&a))
            .await
            .unwrap();
        let live = crons.get(&object_name(&service, &a.id)).await.unwrap();
        assert_eq!(
            live.spec
                .job_template
                .spec
                .as_ref()
                .unwrap()
                .template
                .spec
                .as_ref()
                .unwrap()
                .containers[0]
                .image
                .as_deref(),
            Some("shop/web:2")
        );
        assert!(
            crons
                .get_opt(&object_name(&service, &b.id))
                .await
                .unwrap()
                .is_none(),
            "removed from the list, removed from the cluster"
        );

        let name = run(&client, &service, &a.id).await.unwrap();
        let job = Api::<Job>::namespaced(client.clone(), ns)
            .get(&name)
            .await
            .unwrap();
        let pod_labels = job.spec.unwrap().template.metadata.unwrap().labels.unwrap();
        assert!(
            !pod_labels.contains_key(SERVICE_LABEL),
            "a run's pods must not join the service"
        );

        sync(&client, &service, &[]).await.unwrap();
        assert!(
            crons
                .list(&ListParams::default().labels(&format!("{CRON_SERVICE_LABEL}=svc_web")))
                .await
                .unwrap()
                .items
                .is_empty()
        );
        let _ = deployments.delete("web", &DeleteParams::background()).await;
        let _ = Api::<Service>::namespaced(client.clone(), ns)
            .delete("web", &DeleteParams::background())
            .await;
    }
}
