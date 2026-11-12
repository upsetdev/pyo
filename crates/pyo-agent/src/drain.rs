//! Taking a server out of the cluster.
//!
//! A server that's still running is drained: cordoned, then every pod on it
//! is evicted so its controller starts it on another server. The machine's
//! k3s would register the server again if it were deleted now, so that's
//! left until the machine is uninstalled and goes offline; then (or for a
//! machine that's already gone) a forced removal deletes it and its pods.

use std::time::Duration;

use anyhow::{Context, bail};
use k8s_openapi::api::core::v1::{Node, PersistentVolume, Pod};
use kube::{
    Api, ResourceExt,
    api::{DeleteParams, EvictParams, ListParams, Patch, PatchParams},
};
use serde_json::json;

use crate::Agent;

/// How long evicted pods get to leave before the drain gives up.
const DRAIN_TIMEOUT: Duration = Duration::from_secs(600);

pub async fn remove(agent: &Agent, node: &str, force: bool) -> anyhow::Result<String> {
    if force {
        delete(agent, node).await
    } else {
        drain(agent, node).await
    }
}

/// The pods scheduled on a server.
async fn pods_on(agent: &Agent, node: &str) -> anyhow::Result<Vec<Pod>> {
    let lp = ListParams::default().fields(&format!("spec.nodeName={node}"));
    Ok(Api::<Pod>::all(agent.kube.clone()).list(&lp).await?.items)
}

/// Pods a drain moves: not DaemonSet pods (they run on every server and go
/// with it) and not static (mirror) pods.
fn movable(p: &Pod) -> bool {
    let daemon = p.owner_references().iter().any(|o| o.kind == "DaemonSet");
    let mirror = p.annotations().contains_key("kubernetes.io/config.mirror");
    !daemon && !mirror
}

/// Volumes whose data is on this server's disk (local-path and local PVs
/// pinned to it by node affinity), as "namespace/claim".
async fn local_volumes(agent: &Agent, node: &str) -> anyhow::Result<Vec<String>> {
    let pvs = Api::<PersistentVolume>::all(agent.kube.clone())
        .list(&ListParams::default())
        .await?;
    Ok(pvs
        .items
        .iter()
        .filter(|pv| {
            let terms = pv
                .spec
                .as_ref()
                .and_then(|s| s.node_affinity.as_ref())
                .and_then(|a| a.required.as_ref())
                .map(|r| r.node_selector_terms.as_slice())
                .unwrap_or_default();
            terms.iter().any(|t| {
                t.match_expressions.iter().flatten().any(|e| {
                    e.key == "kubernetes.io/hostname"
                        && e.operator == "In"
                        && e.values.iter().flatten().any(|v| v == node)
                })
            })
        })
        .map(
            |pv| match pv.spec.as_ref().and_then(|s| s.claim_ref.as_ref()) {
                Some(c) => format!(
                    "{}/{}",
                    c.namespace.as_deref().unwrap_or("default"),
                    c.name.as_deref().unwrap_or("?")
                ),
                None => pv.name_any(),
            },
        )
        .collect())
}

async fn drain(agent: &Agent, node: &str) -> anyhow::Result<String> {
    let nodes = Api::<Node>::all(agent.kube.clone());
    let n = nodes
        .get(node)
        .await
        .with_context(|| format!("{node} isn't in the cluster"))?;
    if n.labels()
        .contains_key("node-role.kubernetes.io/control-plane")
        || n.labels().contains_key("node-role.kubernetes.io/master")
    {
        bail!(
            "{node} runs the cluster itself (control plane), so it can't be removed. Delete the cluster instead."
        );
    }
    let local = local_volumes(agent, node).await?;
    if !local.is_empty() {
        bail!(
            "These volumes keep their data on {node}'s disk, so they can't move: {}. Move or delete the services using them first.",
            local.join(", ")
        );
    }
    nodes
        .patch(
            node,
            &PatchParams::default(),
            &Patch::Merge(json!({ "spec": { "unschedulable": true } })),
        )
        .await?;

    // The agent itself goes last, after this result is reported.
    let me = std::env::var("HOSTNAME").unwrap_or_default();
    let deadline = tokio::time::Instant::now() + DRAIN_TIMEOUT;
    let mut moved = 0usize;
    loop {
        let left: Vec<Pod> = pods_on(agent, node)
            .await?
            .into_iter()
            .filter(|p| movable(p) && p.name_any() != me)
            .collect();
        if left.is_empty() {
            break;
        }
        if tokio::time::Instant::now() > deadline {
            let names: Vec<String> = left
                .iter()
                .take(8)
                .map(|p| format!("{}/{}", p.namespace().unwrap_or_default(), p.name_any()))
                .collect();
            bail!(
                "Gave up after 10 minutes. Still on {node}: {}. It stays paused. Try again, or remove it by force.",
                names.join(", ")
            );
        }
        for p in left
            .iter()
            .filter(|p| p.metadata.deletion_timestamp.is_none())
        {
            let pods =
                Api::<Pod>::namespaced(agent.kube.clone(), &p.namespace().unwrap_or_default());
            // A disruption budget can refuse for now (429); it's retried.
            match pods.evict(&p.name_any(), &EvictParams::default()).await {
                Ok(_) => moved += 1,
                Err(kube::Error::Api(e)) if e.code == 429 || e.code == 404 => {}
                Err(e) => tracing::warn!(pod = %p.name_any(), error = %e, "eviction failed"),
            }
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }

    if pods_on(agent, node)
        .await?
        .iter()
        .any(|p| p.name_any() == me)
    {
        let kube = agent.kube.clone();
        let ns = std::env::var("POD_NAMESPACE").unwrap_or_else(|_| "pyo-system".into());
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(5)).await;
            if let Err(e) = Api::<Pod>::namespaced(kube, &ns)
                .evict(&me, &EvictParams::default())
                .await
            {
                tracing::warn!(error = %e, "couldn't move the agent off the drained server");
            }
        });
    }
    Ok(format!(
        "Drained {node}: moved {moved} pod(s) to other servers"
    ))
}

/// Delete a server that's gone: its pods first (so their controllers start
/// them elsewhere now rather than after the eviction timeout), then itself.
async fn delete(agent: &Agent, node: &str) -> anyhow::Result<String> {
    let now = DeleteParams {
        grace_period_seconds: Some(0),
        ..Default::default()
    };
    for p in pods_on(agent, node).await? {
        let pods = Api::<Pod>::namespaced(agent.kube.clone(), &p.namespace().unwrap_or_default());
        let _ = pods.delete(&p.name_any(), &now).await;
    }
    match Api::<Node>::all(agent.kube.clone())
        .delete(node, &DeleteParams::default())
        .await
    {
        Ok(_) => {}
        Err(kube::Error::Api(e)) if e.code == 404 => {}
        Err(e) => return Err(e.into()),
    }
    Ok(format!("Removed {node} from the cluster"))
}
