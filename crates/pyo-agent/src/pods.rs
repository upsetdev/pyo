//! Listing pods (and other objects) in pages, keeping only what's used.
//!
//! A cluster can hold thousands of pods that ended: a proxy evicted over
//! and over on a full disk leaves one Failed pod per try, and Kubernetes
//! only collects them past 12,500. One list of all of them, whole, is a
//! response of a hundred megabytes and more held at once, more than the
//! agent's memory. So lists here come a page at a time, ended pods are left
//! out where they aren't needed ([`LIVE`]), and what's kept of each object
//! is cut down to what the agent reads ([`slim_pod`]).

use std::fmt::Debug;

use k8s_openapi::api::core::v1::{Container, Pod, PodSpec, Volume};
use kube::{Api, api::ListParams};
use serde::de::DeserializeOwned;

/// Objects per page: a page of large pods is a few megabytes.
pub const PAGE: u32 = 250;
/// Pods that haven't ended (field selector).
pub const LIVE: &str = "status.phase!=Failed,status.phase!=Succeeded";
/// Pods that have ended, failed or not (field selector).
pub const ENDED: &str = "status.phase!=Pending,status.phase!=Running,status.phase!=Unknown";

/// Every object `lp` selects, a page at a time, each passed through `keep`
/// as its page comes in: only what it returns is held.
pub async fn list<K, T>(
    api: &Api<K>,
    lp: &ListParams,
    mut keep: impl FnMut(K) -> Option<T>,
) -> kube::Result<Vec<T>>
where
    K: Clone + DeserializeOwned + Debug,
{
    let mut out = vec![];
    let mut lp = lp.clone().limit(PAGE);
    loop {
        let page = api.list(&lp).await?;
        out.extend(page.items.into_iter().filter_map(&mut keep));
        match page.metadata.continue_ {
            Some(next) if !next.is_empty() => lp = lp.continue_token(&next),
            _ => return Ok(out),
        }
    }
}

/// Every object `lp` selects, without their managed fields (often a third of
/// an object), a page at a time.
pub async fn list_slim<K>(api: &Api<K>, lp: &ListParams) -> kube::Result<Vec<K>>
where
    K: kube::Resource + Clone + DeserializeOwned + Debug,
{
    list(api, lp, |mut o: K| {
        o.meta_mut().managed_fields = None;
        Some(o)
    })
    .await
}

/// Pods as [`slim_pod`] keeps them, a page at a time.
pub async fn list_pods(api: &Api<Pod>, lp: &ListParams) -> kube::Result<Vec<Pod>> {
    list(api, lp, |p| Some(slim_pod(p))).await
}

/// A pod with what the agent reads of it: its name, labels, owners and
/// times; where it runs; what its containers ask for, their ports and
/// security settings; the claims it mounts; and its status. Not its
/// managed fields, environment, commands, probes or other volumes.
pub fn slim_pod(mut p: Pod) -> Pod {
    p.metadata.managed_fields = None;
    p.spec = p.spec.map(|s| PodSpec {
        node_name: s.node_name,
        containers: s.containers.into_iter().map(slim_container).collect(),
        init_containers: s
            .init_containers
            .map(|cs| cs.into_iter().map(slim_container).collect()),
        volumes: s.volumes.map(|vs| {
            vs.into_iter()
                .filter(|v| v.persistent_volume_claim.is_some())
                .map(|v| Volume {
                    name: v.name,
                    persistent_volume_claim: v.persistent_volume_claim,
                    ..Default::default()
                })
                .collect()
        }),
        overhead: s.overhead,
        security_context: s.security_context,
        restart_policy: s.restart_policy,
        priority_class_name: s.priority_class_name,
        ..Default::default()
    });
    p
}

fn slim_container(c: Container) -> Container {
    Container {
        name: c.name,
        image: c.image,
        resources: c.resources,
        ports: c.ports,
        security_context: c.security_context,
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_slim_pod_keeps_what_is_read() {
        let p: Pod = serde_json::from_value(json!({
            "metadata": {
                "name": "web-1", "namespace": "shop", "labels": { "pyo/service-id": "svc_1" },
                "ownerReferences": [{ "apiVersion": "apps/v1", "kind": "ReplicaSet", "name": "web", "uid": "u1", "controller": true }],
                "managedFields": [{ "manager": "kubelet", "operation": "Update", "fieldsType": "FieldsV1", "fieldsV1": { "f:status": {} } }],
            },
            "spec": {
                "nodeName": "s1",
                "containers": [{
                    "name": "app", "image": "web:1",
                    "env": [{ "name": "SECRET_ISH", "value": "x".repeat(4000) }],
                    "args": ["--a"], "command": ["/web"],
                    "readinessProbe": { "httpGet": { "port": 80 } },
                    "ports": [{ "name": "metrics", "containerPort": 9187 }],
                    "resources": { "requests": { "cpu": "100m" }, "limits": { "nvidia.com/gpu": "1" } },
                    "securityContext": { "runAsNonRoot": true },
                }],
                "volumes": [
                    { "name": "data", "persistentVolumeClaim": { "claimName": "pg-1" } },
                    { "name": "kube-api-access", "projected": { "sources": [] } },
                ],
            },
            "status": { "phase": "Running", "podIP": "10.0.0.5", "containerStatuses": [{ "name": "app", "image": "web:1", "imageID": "", "ready": true, "restartCount": 2 }] },
        }))
        .unwrap();
        let before = serde_json::to_vec(&p).unwrap().len();
        let s = slim_pod(p);
        assert!(serde_json::to_vec(&s).unwrap().len() < before / 4);
        assert!(s.metadata.managed_fields.is_none());
        assert_eq!(
            s.metadata.owner_references.as_ref().unwrap()[0].kind,
            "ReplicaSet"
        );
        let spec = s.spec.as_ref().unwrap();
        assert_eq!(spec.node_name.as_deref(), Some("s1"));
        let c = &spec.containers[0];
        assert!(c.env.is_none() && c.args.is_none() && c.readiness_probe.is_none());
        assert_eq!(c.ports.as_ref().unwrap()[0].container_port, 9187);
        assert!(c.resources.as_ref().unwrap().limits.is_some());
        assert_eq!(
            c.security_context.as_ref().unwrap().run_as_non_root,
            Some(true)
        );
        let vols = spec.volumes.as_ref().unwrap();
        assert_eq!(vols.len(), 1);
        assert_eq!(
            vols[0].persistent_volume_claim.as_ref().unwrap().claim_name,
            "pg-1"
        );
        let st = s.status.as_ref().unwrap();
        assert_eq!(st.container_statuses.as_ref().unwrap()[0].restart_count, 2);
        assert_eq!(st.pod_ip.as_deref(), Some("10.0.0.5"));
    }
}
