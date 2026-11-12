//! Cluster DNS that outlives a server: k3s runs CoreDNS as one pod, so when
//! its server freezes, every name in the cluster stops resolving.
//!
//! Every heartbeat, on k3s's own CoreDNS (`kube-system/coredns`):
//! - two or more servers it can run on (ready, schedulable, no taint it
//!   doesn't tolerate): at least 2 replicas;
//! - down to one server in the cluster: back to 1, if it's at Pyo's 2;
//! - 2 or more replicas: a disruption budget keeps one up through a drain
//!   (`system::coredns_budget`); fewer: none, so a drain isn't held up.
//!
//! Only the replica count is changed (the scale subresource): k3s's
//! manifest already spreads its pods one per server (a topology spread
//! constraint on the hostname) and doesn't set a count, so k3s re-applying
//! it on a restart or an upgrade keeps ours. Fewer servers for a while (one
//! frozen) changes nothing: the pod there is replaced once Kubernetes gives
//! up on it.

use k8s_openapi::api::{
    apps::v1::Deployment,
    core::v1::{Node, Taint, Toleration},
    policy::v1::PodDisruptionBudget,
};
use kube::{
    Api, Client,
    api::{DeleteParams, ListParams, Patch, PatchParams},
};
use pyo_proto::system::{COREDNS, COREDNS_BUDGET, coredns_budget};
use serde_json::json;

const NS: &str = "kube-system";

/// Whether the pod's tolerations let it onto a node with this taint.
fn tolerated(taint: &Taint, tolerations: &[Toleration]) -> bool {
    tolerations.iter().any(|t| {
        let key = t.key.as_deref().unwrap_or("");
        let key_ok =
            (key.is_empty() && t.operator.as_deref() == Some("Exists")) || key == taint.key;
        let value_ok = t.operator.as_deref() == Some("Exists")
            || t.value.as_deref().unwrap_or("") == taint.value.as_deref().unwrap_or("");
        let effect_ok = t
            .effect
            .as_deref()
            .is_none_or(|e| e.is_empty() || e == taint.effect);
        key_ok && value_ok && effect_ok
    })
}

/// Whether CoreDNS can run on this node now.
fn usable(node: &Node, tolerations: &[Toleration]) -> bool {
    let ready = node
        .status
        .as_ref()
        .and_then(|s| s.conditions.as_ref())
        .is_some_and(|c| c.iter().any(|c| c.type_ == "Ready" && c.status == "True"));
    let spec = node.spec.as_ref();
    let schedulable = !spec.and_then(|s| s.unschedulable).unwrap_or(false);
    let taints_ok = spec.and_then(|s| s.taints.as_ref()).is_none_or(|ts| {
        ts.iter()
            .filter(|t| t.effect != "PreferNoSchedule")
            .all(|t| tolerated(t, tolerations))
    });
    ready && schedulable && taints_ok
}

/// The replica count to set, if any: `servers` in the cluster, `usable` of
/// them for CoreDNS, `current` replicas.
fn wanted(servers: usize, usable: usize, current: i32) -> Option<i32> {
    if usable >= 2 && current < 2 {
        Some(2)
    } else if servers == 1 && current == 2 {
        Some(1)
    } else {
        None
    }
}

/// Whether this is k3s's CoreDNS (its deploy controller's), not one the
/// cluster's owner runs some other way.
fn k3s_managed(d: &Deployment) -> bool {
    d.metadata
        .annotations
        .as_ref()
        .and_then(|a| a.get("objectset.rio.cattle.io/owner-gvk"))
        .is_some_and(|g| g.starts_with("k3s.cattle.io/"))
}

pub async fn ensure(client: &Client) {
    if let Err(e) = run(client).await {
        tracing::warn!(error = %format!("{e:#}"), "couldn't check cluster DNS's replicas");
    }
}

async fn run(client: &Client) -> anyhow::Result<()> {
    let deployments: Api<Deployment> = Api::namespaced(client.clone(), NS);
    let Some(dns) = deployments.get_opt(COREDNS).await? else {
        return Ok(());
    };
    if !k3s_managed(&dns) {
        return Ok(());
    }
    let tolerations = dns
        .spec
        .as_ref()
        .and_then(|s| s.template.spec.as_ref())
        .and_then(|s| s.tolerations.clone())
        .unwrap_or_default();
    let nodes = Api::<Node>::all(client.clone())
        .list(&ListParams::default())
        .await?
        .items;
    let fit = nodes.iter().filter(|n| usable(n, &tolerations)).count();
    let mut replicas = dns.spec.as_ref().and_then(|s| s.replicas).unwrap_or(1);
    if let Some(n) = wanted(nodes.len(), fit, replicas) {
        deployments
            .patch_scale(
                COREDNS,
                &PatchParams::default(),
                &Patch::Merge(json!({ "spec": { "replicas": n } })),
            )
            .await?;
        tracing::info!(
            from = replicas,
            to = n,
            servers = nodes.len(),
            usable = fit,
            "scaled cluster DNS"
        );
        replicas = n;
    }
    let budgets: Api<PodDisruptionBudget> = Api::namespaced(client.clone(), NS);
    let have = budgets.get_opt(COREDNS_BUDGET).await?;
    if replicas >= 2 {
        let want = coredns_budget();
        let same = have
            .as_ref()
            .and_then(|b| serde_json::to_value(&b.spec).ok())
            .is_some_and(|s| s == want["spec"]);
        if !same {
            budgets
                .patch(
                    COREDNS_BUDGET,
                    &PatchParams::apply(crate::MANAGER).force(),
                    &Patch::Apply(&want),
                )
                .await?;
        }
    } else if have.is_some() {
        budgets
            .delete(COREDNS_BUDGET, &DeleteParams::default())
            .await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replica_counts() {
        // Two servers it can run on: 2.
        assert_eq!(wanted(2, 2, 1), Some(2));
        assert_eq!(wanted(4, 3, 1), Some(2));
        // Already 2, or more set by someone: left alone.
        assert_eq!(wanted(3, 3, 2), None);
        assert_eq!(wanted(3, 3, 3), None);
        // One frozen or paused for a while: unchanged.
        assert_eq!(wanted(2, 1, 2), None);
        assert_eq!(wanted(2, 1, 1), None);
        // Down to one server: back to 1, only from Pyo's 2.
        assert_eq!(wanted(1, 1, 2), Some(1));
        assert_eq!(wanted(1, 1, 3), None);
        assert_eq!(wanted(1, 1, 1), None);
    }

    /// One pass, as the agent's heartbeat runs it, on a throwaway cluster
    /// (PYO_TEST_KUBECONFIG: best the agent's own account's, so its roles
    /// and admission policy are what's tested).
    #[tokio::test]
    #[ignore = "needs a throwaway Kubernetes API server"]
    async fn against_a_cluster() {
        let Ok(path) = std::env::var("PYO_TEST_KUBECONFIG") else {
            panic!("set PYO_TEST_KUBECONFIG to a throwaway cluster's kubeconfig")
        };
        let kc = kube::config::Kubeconfig::read_from(&path).unwrap();
        let cfg = kube::Config::from_custom_kubeconfig(kc, &Default::default())
            .await
            .unwrap();
        run(&kube::Client::try_from(cfg).unwrap()).await.unwrap();
    }

    #[test]
    fn tainted_servers_dont_count() {
        let tolerations: Vec<Toleration> = serde_json::from_value(json!([
            { "key": "CriticalAddonsOnly", "operator": "Exists" },
            { "key": "node-role.kubernetes.io/control-plane", "operator": "Exists", "effect": "NoSchedule" },
        ]))
        .unwrap();
        let node = |taints: serde_json::Value, unschedulable: bool, ready: &str| -> Node {
            serde_json::from_value(json!({
                "metadata": { "name": "a" },
                "spec": { "taints": taints, "unschedulable": unschedulable },
                "status": { "conditions": [{ "type": "Ready", "status": ready }] },
            }))
            .unwrap()
        };
        assert!(usable(&node(json!([]), false, "True"), &tolerations));
        assert!(usable(
            &node(
                json!([{ "key": "node-role.kubernetes.io/control-plane", "effect": "NoSchedule" }]),
                false,
                "True"
            ),
            &tolerations
        ));
        assert!(usable(
            &node(
                json!([{ "key": "x", "effect": "PreferNoSchedule" }]),
                false,
                "True"
            ),
            &tolerations
        ));
        assert!(!usable(
            &node(
                json!([{ "key": "pyo/builder", "value": "true", "effect": "NoSchedule" }]),
                false,
                "True"
            ),
            &tolerations
        ));
        assert!(!usable(&node(json!([]), true, "True"), &tolerations));
        assert!(!usable(&node(json!([]), false, "Unknown"), &tolerations));
    }
}
