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
//!
//! An owner with more than [`STORM`] Failed pods is caught in a loop (a
//! DaemonSet's pod refused by its server over and over, a few a second):
//! its pods past the newest [`KEEP`] go without waiting out the grace
//! period, and the next run comes after [`SOON`] instead of minutes, until
//! no owner is over. Pyo's own DaemonSets are critical (pyo-proto's
//! `system::SERVER_CRITICAL`) so they don't loop: this is for what else may.

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
/// More Failed pods than this for one owner: a loop.
pub const STORM: usize = 50;
const EVERY: Duration = Duration::from_secs(5 * 60);
/// The next run, while some owner is over [`STORM`].
pub const SOON: Duration = Duration::from_secs(30);
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

/// What a run deletes, and whether some owner is in a loop.
#[derive(Debug, Default, PartialEq)]
pub struct Choice {
    /// Oldest first.
    pub pods: Vec<Dead>,
    /// An owner has more than [`STORM`] Failed pods.
    pub storm: bool,
}

/// The pods to delete, oldest first: not among the newest [`KEEP`] of their
/// owner (in their namespace), and past the grace period unless their owner
/// is over [`STORM`]. At most `max`. Pods with no owner are never a loop.
pub fn choose(dead: Vec<Dead>, now: i64, max: usize) -> Choice {
    let mut by_owner: BTreeMap<(String, String), Vec<Dead>> = BTreeMap::new();
    for d in dead {
        by_owner
            .entry((d.namespace.clone(), d.owner.clone()))
            .or_default()
            .push(d);
    }
    let mut storm = false;
    let mut out: Vec<Dead> = vec![];
    for ((_, owner), mut pods) in by_owner {
        let looping = !owner.is_empty() && pods.len() > STORM;
        storm |= looping;
        pods.sort_by_key(|d| std::cmp::Reverse(d.ended_at));
        out.extend(
            pods.into_iter()
                .skip(KEEP)
                .filter(|d| looping || now - d.ended_at >= GRACE_MS),
        );
    }
    out.sort_by_key(|d| d.ended_at);
    out.truncate(max);
    Choice { pods: out, storm }
}

/// Every few minutes, for good, or [`SOON`] again while pods loop. Not while
/// the cluster's policy is read only.
pub async fn run(agent: Agent) {
    tokio::time::sleep(FIRST).await;
    loop {
        let read_only = agent
            .policy
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .read_only;
        let mut storm = false;
        if !read_only {
            match once(&agent.kube, PAUSE).await {
                Ok(r) => {
                    storm = r.storm;
                    if r.storm {
                        tracing::warn!(
                            deleted = r.deleted,
                            "pods are failing over and over, cleaning up again soon"
                        );
                    } else if r.deleted > 0 {
                        tracing::info!(deleted = r.deleted, "cleaned up failed pods");
                    }
                }
                Err(e) => tracing::warn!(error = %e, "couldn't clean up failed pods"),
            }
        }
        tokio::time::sleep(if storm { SOON } else { EVERY }).await;
    }
}

/// What a run did.
#[derive(Debug, Default, PartialEq)]
struct Run {
    deleted: usize,
    storm: bool,
}

/// One run, with `pause` between batches.
async fn once(client: &kube::Client, pause: Duration) -> kube::Result<Run> {
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
    let Choice {
        pods: chosen,
        storm,
    } = choose(dead, pyo_proto::now_ms(), MAX_PER_RUN);
    let mut deleted = 0;
    // Namespaces where the agent's roles don't allow deleting.
    let mut refused: Vec<String> = vec![];
    for (i, d) in chosen.iter().enumerate() {
        if refused.contains(&d.namespace) {
            continue;
        }
        if i > 0 && i % BATCH == 0 {
            tokio::time::sleep(pause).await;
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
    Ok(Run { deleted, storm })
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
        assert_eq!(names(&choose(pods, NOW, 100).pods), ["p5", "p4", "p3"]);
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
        assert_eq!(names(&choose(pods, NOW, 100).pods), ["old"]);
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
        assert_eq!(names(&choose(pods, NOW, 100).pods), ["bare4"]);
    }

    #[test]
    fn at_most_max_per_run_oldest_first() {
        let rs = "ReplicaSet/envoy-pyo-1";
        let pods = (0..4700)
            .map(|i| d("pyo-gateway", rs, &format!("p{i}"), 20 + i))
            .collect();
        let chosen = choose(pods, NOW, MAX_PER_RUN).pods;
        assert_eq!(chosen.len(), MAX_PER_RUN);
        assert_eq!(chosen[0].name, "p4699");
        assert!(
            chosen
                .iter()
                .all(|d| d.name != "p0" && d.name != "p1" && d.name != "p2")
        );
    }

    /// A DaemonSet's pod refused over and over: its young pods go too
    /// (all but the newest three), and the next run comes soon.
    #[test]
    fn a_loop_skips_the_grace_period() {
        let ds = "DaemonSet/envoy-pyo-1";
        let pods: Vec<Dead> = (0..=STORM as i64)
            .map(|i| d("pyo-gateway", ds, &format!("p{i}"), i % 5))
            .collect();
        let c = choose(pods.clone(), NOW, MAX_PER_RUN);
        assert!(c.storm);
        assert_eq!(c.pods.len(), STORM + 1 - KEEP);
        // At its limit, not yet a loop: the grace period holds.
        let c = choose(pods[..STORM].to_vec(), NOW, MAX_PER_RUN);
        assert!(!c.storm);
        assert!(c.pods.is_empty(), "{:?}", names(&c.pods));
    }

    /// Only the looping owner skips the grace period, and many pods with no
    /// owner aren't a loop.
    #[test]
    fn a_loop_is_one_owners() {
        let mut pods: Vec<Dead> = (0..60)
            .map(|i| d("pyo-system", "DaemonSet/pyo-vector", &format!("v{i}"), 1))
            .collect();
        pods.extend((0..5).map(|i| d("pyo-system", "ReplicaSet/pyo-agent-1", &format!("a{i}"), 1)));
        pods.extend((0..5).map(|i| d("shop", "DaemonSet/pyo-vector", &format!("s{i}"), 1)));
        let c = choose(pods, NOW, MAX_PER_RUN);
        assert!(c.storm);
        assert_eq!(c.pods.len(), 57);
        assert!(
            c.pods.iter().all(|p| p.name.starts_with('v')),
            "{:?}",
            names(&c.pods)
        );

        let bare: Vec<Dead> = (0..80)
            .map(|i| d("shop", "", &format!("b{i}"), 1))
            .collect();
        let c = choose(bare, NOW, MAX_PER_RUN);
        assert!(!c.storm);
        assert!(c.pods.is_empty());
    }

    fn pod(v: serde_json::Value) -> Pod {
        serde_json::from_value(v).unwrap()
    }

    /// A fake Kubernetes API: namespaces (name, labels), Failed pods per
    /// namespace, and the deletes it got (namespace, name, uid precondition).
    mod fake {
        use std::sync::{Arc, Mutex};

        use axum::{
            Json, Router,
            body::Bytes,
            extract::State,
            http::{Method, StatusCode, Uri},
            response::{IntoResponse, Response},
        };
        use serde_json::{Value, json};

        #[derive(Default)]
        pub struct Cluster {
            pub namespaces: Vec<Value>,
            pub pods: Vec<Value>,
            pub deleted: Vec<(String, String, String)>,
            pub lists: Vec<String>,
        }

        pub type Shared = Arc<Mutex<Cluster>>;

        async fn handle(
            State(st): State<Shared>,
            method: Method,
            uri: Uri,
            body: Bytes,
        ) -> Response {
            let mut c = st.lock().unwrap();
            let path = uri.path().to_owned();
            let parts: Vec<&str> = path.trim_start_matches("/api/v1/").split('/').collect();
            match (method.as_str(), parts.as_slice()) {
                ("GET", ["namespaces"]) => Json(json!({
                    "apiVersion": "v1", "kind": "NamespaceList", "metadata": {}, "items": c.namespaces,
                }))
                .into_response(),
                ("GET", ["namespaces", ns, "pods"]) => {
                    c.lists.push(format!("{ns}?{}", uri.query().unwrap_or_default()));
                    let items: Vec<Value> = c
                        .pods
                        .iter()
                        .filter(|p| p["metadata"]["namespace"] == *ns)
                        .cloned()
                        .collect();
                    Json(json!({ "apiVersion": "v1", "kind": "PodList", "metadata": {}, "items": items }))
                        .into_response()
                }
                ("DELETE", ["namespaces", ns, "pods", name]) => {
                    let opts: Value = serde_json::from_slice(&body).unwrap_or_default();
                    let uid = opts["preconditions"]["uid"].as_str().unwrap_or_default().to_owned();
                    c.deleted.push(((*ns).to_owned(), (*name).to_owned(), uid));
                    let at = c.pods.iter().position(|p| {
                        p["metadata"]["namespace"] == *ns && p["metadata"]["name"] == *name
                    });
                    match at {
                        Some(i) => Json(c.pods.remove(i)).into_response(),
                        None => (StatusCode::NOT_FOUND, Json(json!({ "kind": "Status", "apiVersion": "v1", "status": "Failure", "reason": "NotFound", "code": 404, "message": "not found" }))).into_response(),
                    }
                }
                _ => (StatusCode::NOT_FOUND, Json(json!({ "kind": "Status", "apiVersion": "v1", "status": "Failure", "code": 404, "message": "no such path" }))).into_response(),
            }
        }

        pub async fn serve(cluster: Cluster) -> (kube::Client, Shared) {
            let st: Shared = Arc::new(Mutex::new(cluster));
            let app = Router::new().fallback(handle).with_state(st.clone());
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
            let config = kube::Config::new(format!("http://{addr}").parse().unwrap());
            (kube::Client::try_from(config).unwrap(), st)
        }
    }

    fn failed_pod(
        ns: &str,
        name: &str,
        owner: Option<(&str, &str)>,
        ended: &str,
    ) -> serde_json::Value {
        let owners: Vec<serde_json::Value> = owner
            .map(|(kind, n)| json!({ "apiVersion": "apps/v1", "kind": kind, "name": n, "uid": "o", "controller": true }))
            .into_iter()
            .collect();
        json!({
            "apiVersion": "v1", "kind": "Pod",
            "metadata": { "name": name, "namespace": ns, "uid": format!("uid-{name}"), "ownerReferences": owners },
            "status": { "phase": "Failed", "reason": "Evicted", "message": "The node had condition: [DiskPressure]. ",
                "conditions": [{ "type": "Ready", "status": "False", "lastTransitionTime": ended }] },
        })
    }

    /// A run against a cluster where the proxy's DaemonSet loops on a
    /// server short of disk: its pods go at once, each by its uid, but
    /// only in Pyo's namespaces, never another's, and nothing young that
    /// isn't looping.
    #[tokio::test]
    async fn a_run_cleans_up_a_loop_in_pyos_namespaces_only() {
        use k8s_openapi::jiff::Timestamp;
        let now = Timestamp::from_second(Timestamp::now().as_second())
            .unwrap()
            .to_string();
        let ns = |name: &str, labels: serde_json::Value| json!({ "metadata": { "name": name, "labels": labels } });
        let mut cluster = fake::Cluster {
            namespaces: vec![
                ns("pyo-gateway", json!({})),
                ns("pyo-system", json!({})),
                ns("theirs", json!({})),
                ns(
                    "pyo-waf",
                    json!({ "pyo.dev/bundle": "waf", "pyo.dev/managed": "true" }),
                ),
            ],
            ..Default::default()
        };
        let ds = Some(("DaemonSet", "envoy-pyo-1"));
        for i in 0..60 {
            cluster
                .pods
                .push(failed_pod("pyo-gateway", &format!("envoy-{i}"), ds, &now));
            cluster.pods.push(failed_pod(
                "theirs",
                &format!("theirs-{i}"),
                Some(("DaemonSet", "theirs")),
                &now,
            ));
            cluster.pods.push(failed_pod(
                "pyo-waf",
                &format!("waf-{i}"),
                Some(("DaemonSet", "pyo-waf")),
                &now,
            ));
        }
        // Just evicted, not looping: kept for now.
        for i in 0..5 {
            cluster.pods.push(failed_pod(
                "pyo-system",
                &format!("vector-{i}"),
                Some(("DaemonSet", "pyo-vector")),
                &now,
            ));
        }
        let (client, st) = fake::serve(cluster).await;

        let run = once(&client, Duration::ZERO).await.unwrap();
        assert_eq!(
            run,
            Run {
                deleted: 57,
                storm: true
            }
        );
        let c = st.lock().unwrap();
        assert_eq!(c.deleted.len(), 57);
        assert!(
            c.deleted.iter().all(|(ns, name, uid)| ns == "pyo-gateway"
                && name.starts_with("envoy-")
                && *uid == format!("uid-{name}")),
            "{:?}",
            c.deleted
        );
        // The newest three of the loop stay, and everything elsewhere.
        assert_eq!(
            c.pods
                .iter()
                .filter(|p| p["metadata"]["namespace"] == "pyo-gateway")
                .count(),
            KEEP
        );
        assert_eq!(c.pods.len(), KEEP + 60 + 60 + 5);
        // Only Pyo's namespaces are listed, Failed pods only, a page at a time.
        let listed: Vec<&str> = c
            .lists
            .iter()
            .map(|l| l.split('?').next().unwrap())
            .collect();
        assert_eq!(listed, ["pyo-gateway", "pyo-system"]);
        assert!(
            c.lists
                .iter()
                .all(|l| l.contains("fieldSelector=status.phase%3DFailed") && l.contains("limit=")),
            "{:?}",
            c.lists
        );
        drop(c);

        // Nothing looping any more: the next run is in minutes.
        let run = once(&client, Duration::ZERO).await.unwrap();
        assert_eq!(
            run,
            Run {
                deleted: 0,
                storm: false
            }
        );
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
