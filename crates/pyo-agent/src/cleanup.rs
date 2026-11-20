//! Failed pods in Pyo's namespaces, cleaned up.
//!
//! A server short of disk or memory evicts pods, and their controllers start
//! new ones, which can be evicted again: each try leaves a Failed pod behind.
//! Kubernetes only collects ended pods past 12,500 of them, so thousands can
//! pile up, and each one is more for the kubelet and every list of pods.
//!
//! Every few minutes the agent deletes Failed pods that ended more than
//! [`GRACE_MS`] ago, keeping the newest [`KEEP`] of each owner (a
//! ReplicaSet, a StatefulSet) to show what happened. Only in namespaces Pyo
//! manages ([`managed`]), never a Job's pods (the Job and its history keep
//! those, and a cron run's outcome is read from them), and a few at a time
//! ([`BATCH`], [`PAUSE`], [`MAX_PER_RUN`]) so neither the API server nor the
//! kubelets are flooded.

use std::{collections::BTreeMap, time::Duration};

use k8s_openapi::api::core::v1::{Namespace, Pod};
use kube::{
    Api, ResourceExt,
    api::{DeleteParams, ListParams, Preconditions},
};
use pyo_proto::{bundles::BUNDLE_LABEL, rbac::MANAGED_LABEL, system::GATEWAY_NS};

use crate::Agent;

/// How long a Failed pod is kept before it may go.
pub const GRACE_MS: i64 = 10 * 60_000;
/// Failed pods kept per owner, the newest, whatever their age.
pub const KEEP: usize = 3;
/// Deletions between pauses.
pub const BATCH: usize = 25;
pub const PAUSE: Duration = Duration::from_secs(5);
/// Deletions per run at most: the rest wait for the next run.
pub const MAX_PER_RUN: usize = 500;
const EVERY: Duration = Duration::from_secs(5 * 60);
/// After a start: soon, but after the first heartbeat.
const FIRST: Duration = Duration::from_secs(60);

/// A Failed pod, as much as choosing needs.
#[derive(Debug, Clone, PartialEq)]
pub struct Dead {
    pub namespace: String,
    pub name: String,
    pub uid: String,
    /// Its controller ("ReplicaSet/web-5d8c"), empty for none.
    pub owner: String,
    /// When it ended (unix ms).
    pub ended_at: i64,
}

/// Whether Pyo manages the namespace's pods: its own and the gateway's, and
/// those it writes in (`pyo.dev/managed=true`: projects', teams'
/// databases'). Not its components' (they're their operators' to run).
pub fn managed(ns: &Namespace) -> bool {
    let name = ns.name_any();
    let labels = ns.labels();
    if labels.contains_key(BUNDLE_LABEL) {
        return false;
    }
    name == pyo_proto::rbac::NS
        || name == GATEWAY_NS
        || labels.get(MANAGED_LABEL).is_some_and(|v| v == "true")
}

fn ms(t: &k8s_openapi::apimachinery::pkg::apis::meta::v1::Time) -> i64 {
    t.0.as_millisecond()
}

/// A Failed pod to choose from; None for one that isn't Failed, is being
/// deleted already, or is a Job's.
pub fn dead(p: &Pod) -> Option<Dead> {
    let st = p.status.as_ref()?;
    if st.phase.as_deref() != Some("Failed") || p.metadata.deletion_timestamp.is_some() {
        return None;
    }
    let owner = p
        .owner_references()
        .iter()
        .find(|o| o.controller == Some(true))
        .map(|o| (o.kind.clone(), o.name.clone()));
    if owner.as_ref().is_some_and(|(kind, _)| kind == "Job") {
        return None;
    }
    // The latest of its containers' ends and its conditions' changes (an
    // evicted pod's containers may have none), else when it started.
    let ended_at = st
        .container_statuses
        .iter()
        .flatten()
        .filter_map(|c| c.state.as_ref()?.terminated.as_ref()?.finished_at.as_ref())
        .map(ms)
        .chain(
            st.conditions
                .iter()
                .flatten()
                .filter_map(|c| c.last_transition_time.as_ref())
                .map(ms),
        )
        .max()
        .or_else(|| st.start_time.as_ref().map(ms))
        .or_else(|| p.metadata.creation_timestamp.as_ref().map(ms))
        .unwrap_or(0);
    Some(Dead {
        namespace: p.namespace().unwrap_or_default(),
        name: p.name_any(),
        uid: p.metadata.uid.clone().unwrap_or_default(),
        owner: owner.map(|(k, n)| format!("{k}/{n}")).unwrap_or_default(),
        ended_at,
    })
}

/// The pods to delete, oldest first: past the grace period and not among
/// the newest [`KEEP`] of their owner (in their namespace). At most `max`.
pub fn choose(dead: Vec<Dead>, now: i64, max: usize) -> Vec<Dead> {
    let mut by_owner: BTreeMap<(String, String), Vec<Dead>> = BTreeMap::new();
    for d in dead {
        by_owner
            .entry((d.namespace.clone(), d.owner.clone()))
            .or_default()
            .push(d);
    }
    let mut out: Vec<Dead> = by_owner
        .into_values()
        .flat_map(|mut pods| {
            pods.sort_by_key(|d| std::cmp::Reverse(d.ended_at));
            pods.into_iter()
                .skip(KEEP)
                .filter(|d| now - d.ended_at >= GRACE_MS)
        })
        .collect();
    out.sort_by_key(|d| d.ended_at);
    out.truncate(max);
    out
}

/// Every few minutes, for good. Not while the cluster's policy is read only.
pub async fn run(agent: Agent) {
    tokio::time::sleep(FIRST).await;
    loop {
        let read_only = agent
            .policy
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .read_only;
        if !read_only {
            match once(&agent.kube).await {
                Ok(0) => {}
                Ok(n) => tracing::info!(deleted = n, "cleaned up failed pods"),
                Err(e) => tracing::warn!(error = %e, "couldn't clean up failed pods"),
            }
        }
        tokio::time::sleep(EVERY).await;
    }
}

/// One run: how many pods it deleted.
async fn once(client: &kube::Client) -> kube::Result<usize> {
    let namespaces: Vec<String> = Api::<Namespace>::all(client.clone())
        .list(&ListParams::default())
        .await?
        .items
        .iter()
        .filter(|n| managed(n))
        .map(|n| n.name_any())
        .collect();
    let lp = ListParams::default().fields("status.phase=Failed");
    let mut dead = vec![];
    for ns in &namespaces {
        let api = Api::<Pod>::namespaced(client.clone(), ns);
        // Unreadable (gone since): the others still are.
        if let Ok(d) = crate::pods::list(&api, &lp, |p| dead_in(&p, ns)).await {
            dead.extend(d);
        }
    }
    let chosen = choose(dead, pyo_proto::now_ms(), MAX_PER_RUN);
    let mut deleted = 0;
    // Namespaces where the agent's roles don't allow deleting.
    let mut refused: Vec<String> = vec![];
    for (i, d) in chosen.iter().enumerate() {
        if refused.contains(&d.namespace) {
            continue;
        }
        if i > 0 && i % BATCH == 0 {
            tokio::time::sleep(PAUSE).await;
        }
        // Only that very pod: a StatefulSet's next one has the same name.
        let dp = DeleteParams {
            preconditions: Some(Preconditions {
                uid: Some(d.uid.clone()),
                resource_version: None,
            }),
            ..Default::default()
        };
        match Api::<Pod>::namespaced(client.clone(), &d.namespace)
            .delete(&d.name, &dp)
            .await
        {
            Ok(_) => deleted += 1,
            // Gone already, or not the same pod.
            Err(kube::Error::Api(e)) if e.code == 404 || e.code == 409 => {}
            Err(e) => {
                tracing::warn!(pod = %d.name, namespace = %d.namespace, error = %e, "couldn't delete a failed pod");
                if matches!(&e, kube::Error::Api(a) if a.code == 403) {
                    refused.push(d.namespace.clone());
                }
            }
        }
    }
    Ok(deleted)
}

/// [`dead`], for a pod listed in `ns` (only ever that namespace's).
fn dead_in(p: &Pod, ns: &str) -> Option<Dead> {
    dead(p).filter(|d| d.namespace == ns)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const NOW: i64 = 1_800_000_000_000;
    const MIN: i64 = 60_000;

    fn d(ns: &str, owner: &str, name: &str, ago_min: i64) -> Dead {
        Dead {
            namespace: ns.into(),
            name: name.into(),
            uid: format!("uid-{name}"),
            owner: owner.into(),
            ended_at: NOW - ago_min * MIN,
        }
    }

    fn names(v: &[Dead]) -> Vec<&str> {
        v.iter().map(|d| d.name.as_str()).collect()
    }

    #[test]
    fn keeps_the_newest_three_per_owner() {
        let rs = "ReplicaSet/envoy-pyo-1";
        let pods = (0..6)
            .map(|i| d("pyo-gateway", rs, &format!("p{i}"), 20 + i))
            .collect();
        // p0..p2 are the newest; p3..p5 go, oldest first.
        assert_eq!(names(&choose(pods, NOW, 100)), ["p5", "p4", "p3"]);
    }

    #[test]
    fn waits_out_the_grace_period() {
        let rs = "ReplicaSet/web-1";
        let pods = vec![
            d("shop", rs, "a", 1),
            d("shop", rs, "b", 2),
            d("shop", rs, "c", 3),
            d("shop", rs, "young", 9),
            d("shop", rs, "old", 11),
        ];
        assert_eq!(names(&choose(pods, NOW, 100)), ["old"]);
    }

    #[test]
    fn owners_and_namespaces_are_counted_apart() {
        let pods = vec![
            d("shop", "ReplicaSet/web-1", "w1", 30),
            d("shop", "ReplicaSet/web-1", "w2", 31),
            d("shop", "ReplicaSet/api-1", "a1", 30),
            d("shop", "ReplicaSet/api-1", "a2", 31),
            d("blog", "ReplicaSet/web-1", "b1", 30),
            d("blog", "ReplicaSet/web-1", "b2", 31),
            d("shop", "", "bare1", 40),
            d("shop", "", "bare2", 41),
            d("shop", "", "bare3", 42),
            d("shop", "", "bare4", 43),
        ];
        // Three or fewer each, except the bare pods (one owner: none).
        assert_eq!(names(&choose(pods, NOW, 100)), ["bare4"]);
    }

    #[test]
    fn at_most_max_per_run_oldest_first() {
        let rs = "ReplicaSet/envoy-pyo-1";
        let pods = (0..4700)
            .map(|i| d("pyo-gateway", rs, &format!("p{i}"), 20 + i))
            .collect();
        let chosen = choose(pods, NOW, MAX_PER_RUN);
        assert_eq!(chosen.len(), MAX_PER_RUN);
        assert_eq!(chosen[0].name, "p4699");
        assert!(
            chosen
                .iter()
                .all(|d| d.name != "p0" && d.name != "p1" && d.name != "p2")
        );
    }

    fn pod(v: serde_json::Value) -> Pod {
        serde_json::from_value(v).unwrap()
    }

    #[test]
    fn only_failed_pods_not_jobs_and_not_being_deleted() {
        let evicted = pod(json!({
            "metadata": { "name": "envoy-1", "namespace": "pyo-gateway", "uid": "u1",
                "ownerReferences": [{ "apiVersion": "apps/v1", "kind": "ReplicaSet", "name": "envoy-pyo-1", "uid": "r", "controller": true }] },
            "status": { "phase": "Failed", "reason": "Evicted", "startTime": "2026-11-20T01:00:00Z",
                "conditions": [{ "type": "Ready", "status": "False", "lastTransitionTime": "2026-11-20T01:05:00Z" }] },
        }));
        let got = dead(&evicted).unwrap();
        assert_eq!(got.owner, "ReplicaSet/envoy-pyo-1");
        assert_eq!(got.uid, "u1");
        assert_eq!(
            got.ended_at,
            "2026-11-20T01:05:00Z"
                .parse::<k8s_openapi::jiff::Timestamp>()
                .unwrap()
                .as_millisecond()
        );

        let job = pod(json!({
            "metadata": { "name": "run-1", "namespace": "shop",
                "ownerReferences": [{ "apiVersion": "batch/v1", "kind": "Job", "name": "run", "uid": "j", "controller": true }] },
            "status": { "phase": "Failed" },
        }));
        assert_eq!(dead(&job), None);
        let running =
            pod(json!({ "metadata": { "name": "web" }, "status": { "phase": "Running" } }));
        assert_eq!(dead(&running), None);
        let going = pod(json!({
            "metadata": { "name": "web", "deletionTimestamp": "2026-11-20T01:00:00Z" },
            "status": { "phase": "Failed" },
        }));
        assert_eq!(dead(&going), None);
        // A container's end counts too, the latest of all.
        let crashed = pod(json!({
            "metadata": { "name": "x" },
            "status": { "phase": "Failed", "conditions": [{ "type": "Ready", "status": "False", "lastTransitionTime": "2026-11-20T01:00:00Z" }],
                "containerStatuses": [{ "name": "app", "image": "x", "imageID": "", "ready": false, "restartCount": 0,
                    "state": { "terminated": { "exitCode": 137, "finishedAt": "2026-11-20T02:00:00Z" } } }] },
        }));
        assert_eq!(
            dead(&crashed).unwrap().ended_at,
            "2026-11-20T02:00:00Z"
                .parse::<k8s_openapi::jiff::Timestamp>()
                .unwrap()
                .as_millisecond()
        );
        // Listed in another namespace than asked: never.
        assert!(dead_in(&evicted, "shop").is_none());
        assert!(dead_in(&evicted, "pyo-gateway").is_some());
    }

    #[test]
    fn only_namespaces_pyo_manages() {
        let ns = |name: &str, labels: serde_json::Value| -> Namespace {
            serde_json::from_value(json!({ "metadata": { "name": name, "labels": labels } }))
                .unwrap()
        };
        assert!(managed(&ns("pyo-system", json!({}))));
        assert!(managed(&ns("pyo-gateway", json!({}))));
        assert!(managed(&ns(
            "shop-prod",
            json!({ "pyo.dev/managed": "true" })
        )));
        assert!(managed(&ns(
            "pyo-db-team1",
            json!({ "pyo.dev/managed": "true" })
        )));
        assert!(!managed(&ns(
            "pyo-databases",
            json!({ "pyo.dev/bundle": "databases" })
        )));
        assert!(!managed(&ns(
            "pyo-waf",
            json!({ "pyo.dev/bundle": "waf", "pyo.dev/managed": "true" })
        )));
        assert!(!managed(&ns("kube-system", json!({}))));
        assert!(!managed(&ns("default", json!({}))));
        assert!(!managed(&ns(
            "theirs",
            json!({ "pyo.dev/managed": "false" })
        )));
        assert!(!managed(&ns(
            "theirs",
            json!({ "app.kubernetes.io/managed-by": "pyo" })
        )));
    }
}
