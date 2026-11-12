//! NVIDIA GPUs. A server's GPU is usable by containers once it has the
//! driver and the NVIDIA Container Toolkit (connect.sh/join.sh check, and
//! install the toolkit where that's standard); k3s then has an `nvidia`
//! containerd runtime. This agent:
//!
//! - labels such servers `pyo/gpu=nvidia` (the scripts do too; here it's
//!   from the node's runtime handlers, for servers set up by hand);
//! - when the cluster has one, keeps the `nvidia` RuntimeClass and NVIDIA's
//!   device plugin (a DaemonSet on those servers only), which makes
//!   `nvidia.com/gpu` a resource pods can ask for;
//! - reports GPUs per server and the cluster's GPU support.

use std::time::Duration;

use k8s_openapi::api::{apps::v1::DaemonSet, core::v1::Node, node::v1::RuntimeClass};
use kube::{
    Api, Client, ResourceExt,
    api::{ListParams, Patch, PatchParams},
};
use pyo_proto::{
    GPU_LABEL, GPU_MEMORY_LABEL, GPU_MODEL_LABEL, GPU_RESOURCE, GPU_RUNTIME_CLASS, GPU_TAINT,
    GpuPolicy, GpuReport,
};
use serde_json::{Value, json};

use crate::MANAGER;

/// NVIDIA's k8s-device-plugin, pinned.
pub use pyo_proto::system::DEVICE_PLUGIN_IMAGE;
const DEVICE_PLUGIN: &str = "nvidia-device-plugin";
const NS: &str = "kube-system";

/// GPUs Kubernetes can hand out on a server, whether it's set up for them,
/// and its model.
pub fn node_gpu(n: &Node) -> (u32, bool, Option<String>) {
    let count = n
        .status
        .as_ref()
        .and_then(|s| s.allocatable.as_ref())
        .and_then(|a| a.get(GPU_RESOURCE))
        .and_then(|q| q.0.parse::<u32>().ok())
        .unwrap_or(0);
    let model = n
        .labels()
        .get(GPU_MODEL_LABEL)
        .map(|m| m.replace(['-', '_'], " "))
        .filter(|m| !m.trim().is_empty());
    (count, count > 0 || has_gpu(n), model)
}

/// What's in the way of using a server's GPU: "" (none there), "ready",
/// "starting", "no-driver", "no-toolkit" or "no-runtime".
pub fn status(n: &Node) -> &'static str {
    let (count, detected, _) = node_gpu(n);
    let label = n.labels().get(GPU_LABEL).map(String::as_str);
    match label {
        Some("no-driver") => return "no-driver",
        Some("no-toolkit") => return "no-toolkit",
        _ => {}
    }
    if !detected {
        return "";
    }
    if count > 0 {
        return "ready";
    }
    // Set up by the script, but k3s has no nvidia runtime: it started
    // before the toolkit was installed. (Only when the node lists its
    // runtimes at all.)
    let listed = n
        .status
        .as_ref()
        .and_then(|s| s.runtime_handlers.as_ref())
        .is_some_and(|h| !h.is_empty());
    if label == Some("nvidia") && listed && !has_nvidia_runtime(n) {
        "no-runtime"
    } else {
        "starting"
    }
}

pub fn memory_mib(n: &Node) -> Option<u32> {
    n.labels()
        .get(GPU_MEMORY_LABEL)
        .and_then(|m| m.parse().ok())
}

/// GPUs the pods running on each server ask for.
pub fn used_per_node(
    pods: &[k8s_openapi::api::core::v1::Pod],
) -> std::collections::HashMap<String, u32> {
    let mut out = std::collections::HashMap::new();
    for p in pods {
        let phase = p
            .status
            .as_ref()
            .and_then(|s| s.phase.as_deref())
            .unwrap_or("");
        let (Some(node), false) = (
            p.spec.as_ref().and_then(|s| s.node_name.clone()),
            phase == "Succeeded" || phase == "Failed",
        ) else {
            continue;
        };
        let gpus: u32 = p
            .spec
            .iter()
            .flat_map(|s| s.containers.iter())
            .filter_map(|c| {
                c.resources
                    .as_ref()?
                    .limits
                    .as_ref()?
                    .get(GPU_RESOURCE)?
                    .0
                    .parse::<u32>()
                    .ok()
            })
            .sum();
        if gpus > 0 {
            *out.entry(node).or_default() += gpus;
        }
    }
    out
}

/// Kept for GPU work: it has Pyo's taint.
pub fn dedicated(n: &Node) -> bool {
    n.spec
        .as_ref()
        .and_then(|s| s.taints.as_ref())
        .is_some_and(|ts| {
            ts.iter()
                .any(|t| t.key == GPU_TAINT && t.effect == "NoSchedule")
        })
}

/// Taint GPU servers kept for GPU work, untaint the others, as Pyo says.
/// Only Pyo's own taint is touched. Pods already running aren't moved.
pub async fn apply_policies(client: &Client, policies: &[GpuPolicy]) {
    let nodes: Api<Node> = Api::all(client.clone());
    for p in policies {
        let Ok(n) = nodes.get(&p.server).await else {
            continue;
        };
        if dedicated(&n) == p.dedicated {
            continue;
        }
        let mut taints: Vec<Value> = n
            .spec
            .as_ref()
            .and_then(|s| s.taints.clone())
            .unwrap_or_default()
            .into_iter()
            .filter(|t| t.key != GPU_TAINT)
            .map(|t| serde_json::to_value(t).unwrap_or_default())
            .collect();
        if p.dedicated {
            taints.push(json!({ "key": GPU_TAINT, "value": "present", "effect": "NoSchedule" }));
        }
        // A merge patch replaces the list: the others are kept as they were.
        match nodes
            .patch(
                &p.server,
                &PatchParams::default(),
                &Patch::Merge(json!({ "spec": { "taints": taints } })),
            )
            .await
        {
            Ok(_) => tracing::info!(node = %p.server, dedicated = p.dedicated, "GPU server policy"),
            Err(e) => {
                tracing::warn!(node = %p.server, error = %e, "couldn't set GPU server policy")
            }
        }
    }
}

/// Set up for NVIDIA GPUs: labelled so, or k3s found the nvidia runtime.
pub fn has_gpu(n: &Node) -> bool {
    n.labels().get(GPU_LABEL).map(String::as_str) == Some("nvidia") || has_nvidia_runtime(n)
}

fn has_nvidia_runtime(n: &Node) -> bool {
    n.status
        .as_ref()
        .and_then(|s| s.runtime_handlers.as_ref())
        .is_some_and(|hs| {
            hs.iter()
                .any(|h| h.name.as_deref() == Some(GPU_RUNTIME_CLASS))
        })
}

pub fn runtime_class() -> Value {
    json!({
        "apiVersion": "node.k8s.io/v1", "kind": "RuntimeClass",
        "metadata": { "name": GPU_RUNTIME_CLASS, "labels": { "app.kubernetes.io/managed-by": MANAGER } },
        "handler": GPU_RUNTIME_CLASS,
        // Pods asking for it only go where the runtime is.
        "scheduling": pyo_proto::system::gpu_runtime_scheduling(),
    })
}

/// NVIDIA's static manifest (deployments/static/nvidia-device-plugin.yml),
/// on GPU servers only and with the nvidia runtime, so it sees the GPUs.
pub fn device_plugin() -> Value {
    json!({
        "apiVersion": "apps/v1", "kind": "DaemonSet",
        "metadata": { "name": DEVICE_PLUGIN, "namespace": NS, "labels": { "app.kubernetes.io/managed-by": MANAGER, "app.kubernetes.io/name": DEVICE_PLUGIN } },
        "spec": {
            "selector": { "matchLabels": { "app.kubernetes.io/name": DEVICE_PLUGIN } },
            "updateStrategy": { "type": "RollingUpdate" },
            "template": {
                "metadata": { "labels": { "app.kubernetes.io/name": DEVICE_PLUGIN } },
                "spec": {
                    "runtimeClassName": GPU_RUNTIME_CLASS,
                    "nodeSelector": { GPU_LABEL: "nvidia" },
                    "tolerations": [{ "key": GPU_RESOURCE, "operator": "Exists", "effect": "NoSchedule" }],
                    "priorityClassName": "system-node-critical",
                    "containers": [{
                        "name": "nvidia-device-plugin-ctr",
                        "image": DEVICE_PLUGIN_IMAGE,
                        // A server whose GPU isn't ready yet keeps the plugin
                        // running (and reporting 0) instead of crash-looping.
                        "env": [{ "name": "FAIL_ON_INIT_ERROR", "value": "false" }],
                        "securityContext": { "allowPrivilegeEscalation": false, "capabilities": { "drop": ["ALL"] } },
                        "resources": { "requests": { "cpu": "10m", "memory": "20Mi" }, "limits": { "memory": "128Mi" } },
                        "volumeMounts": [{ "name": "kubelet-device-plugins-dir", "mountPath": "/var/lib/kubelet/device-plugins" }],
                    }],
                    "volumes": [{ "name": "kubelet-device-plugins-dir", "hostPath": { "path": "/var/lib/kubelet/device-plugins", "type": "Directory" } }],
                },
            },
        },
    })
}

/// Every minute: label GPU servers, and keep the RuntimeClass and device
/// plugin while there are any. Nothing is installed on a cluster without one.
pub async fn run(client: Client) {
    loop {
        if let Err(e) = ensure(&client).await {
            tracing::warn!(error = %format!("{e:#}"), "GPU setup");
        }
        tokio::time::sleep(Duration::from_secs(60)).await;
    }
}

pub async fn ensure(client: &Client) -> anyhow::Result<bool> {
    let nodes: Api<Node> = Api::all(client.clone());
    let list = nodes.list(&ListParams::default()).await?.items;
    for n in list
        .iter()
        .filter(|n| has_nvidia_runtime(n) && n.labels().get(GPU_LABEL).is_none())
    {
        let patch = json!({ "metadata": { "labels": { GPU_LABEL: "nvidia" } } });
        nodes
            .patch(
                &n.name_any(),
                &PatchParams::default(),
                &Patch::Merge(&patch),
            )
            .await?;
        tracing::info!(node = %n.name_any(), "labelled GPU server");
    }
    if !list.iter().any(has_gpu) {
        return Ok(false);
    }
    let pp = PatchParams::apply(MANAGER).force();
    Api::<RuntimeClass>::all(client.clone())
        .patch(GPU_RUNTIME_CLASS, &pp, &Patch::Apply(runtime_class()))
        .await?;
    Api::<DaemonSet>::namespaced(client.clone(), NS)
        .patch(DEVICE_PLUGIN, &pp, &Patch::Apply(device_plugin()))
        .await?;
    Ok(true)
}

/// The cluster's GPU support, for the heartbeat.
pub async fn report(
    client: &Client,
    nodes: &[Node],
    used: &std::collections::HashMap<String, u32>,
) -> GpuReport {
    let gpu_nodes: Vec<&Node> = nodes.iter().filter(|n| node_gpu(n).1).collect();
    let counts: Vec<u32> = gpu_nodes.iter().map(|n| node_gpu(n).0).collect();
    let mut models: Vec<String> = gpu_nodes.iter().filter_map(|n| node_gpu(n).2).collect();
    models.sort();
    models.dedup();
    let mut r = GpuReport {
        servers: gpu_nodes.len() as u32,
        total: counts.iter().sum(),
        max_per_server: counts.iter().copied().max().unwrap_or(0),
        used: gpu_nodes
            .iter()
            .map(|n| used.get(&n.name_any()).copied().unwrap_or(0))
            .sum(),
        models,
        runtime_class: false,
        device_plugin: "none".into(),
    };
    if gpu_nodes.is_empty() {
        return r;
    }
    r.runtime_class = Api::<RuntimeClass>::all(client.clone())
        .get_opt(GPU_RUNTIME_CLASS)
        .await
        .ok()
        .flatten()
        .is_some();
    r.device_plugin = match Api::<DaemonSet>::namespaced(client.clone(), NS)
        .get_opt(DEVICE_PLUGIN)
        .await
    {
        Ok(Some(ds)) => {
            let st = ds.status.unwrap_or_default();
            if st.desired_number_scheduled > 0 && st.number_ready >= st.desired_number_scheduled {
                "ready".into()
            } else {
                "starting".into()
            }
        }
        Ok(None) => "starting".into(),
        Err(e) => format!("can't be read: {e}"),
    };
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(v: Value) -> Node {
        serde_json::from_value(v).unwrap()
    }

    #[test]
    fn servers_with_gpus() {
        let plain = node(
            json!({ "metadata": { "name": "a" }, "status": { "allocatable": { "cpu": "8" } } }),
        );
        assert_eq!(node_gpu(&plain), (0, false, None));
        // Labelled by join.sh; the device plugin hasn't counted it yet.
        let labelled = node(
            json!({ "metadata": { "name": "b", "labels": { "pyo/gpu": "nvidia", "pyo/gpu-model": "NVIDIA-GeForce-RTX-4090" } } }),
        );
        assert_eq!(
            node_gpu(&labelled),
            (0, true, Some("NVIDIA GeForce RTX 4090".into()))
        );
        // Counted by the device plugin.
        let counted = node(
            json!({ "metadata": { "name": "c", "labels": { "pyo/gpu": "nvidia" } }, "status": { "allocatable": { "nvidia.com/gpu": "2" } } }),
        );
        assert_eq!(node_gpu(&counted).0, 2);
        // Set up by hand: k3s found the nvidia runtime.
        let by_hand = node(
            json!({ "metadata": { "name": "d" }, "status": { "runtimeHandlers": [{ "name": "" }, { "name": "nvidia" }] } }),
        );
        assert!(has_gpu(&by_hand) && has_nvidia_runtime(&by_hand));
        // What's missing, said plainly.
        assert_eq!(status(&plain), "");
        assert_eq!(status(&counted), "ready");
        assert_eq!(status(&by_hand), "starting");
        assert_eq!(
            status(&labelled),
            "starting",
            "runtimes not listed: can't tell"
        );
        let no_runtime = node(
            json!({ "metadata": { "name": "e", "labels": { "pyo/gpu": "nvidia" } }, "status": { "runtimeHandlers": [{ "name": "" }, { "name": "runc" }] } }),
        );
        assert_eq!(status(&no_runtime), "no-runtime");
        let no_driver =
            node(json!({ "metadata": { "name": "f", "labels": { "pyo/gpu": "no-driver" } } }));
        assert_eq!(
            (status(&no_driver), has_gpu(&no_driver)),
            ("no-driver", false)
        );
        let mem = node(
            json!({ "metadata": { "name": "m", "labels": { "pyo/gpu-memory-mib": "24564" } } }),
        );
        assert_eq!(memory_mib(&mem), Some(24564));
    }

    #[test]
    fn gpus_in_use_per_server() {
        let pod = |node: &str, gpus: &str, phase: &str| -> k8s_openapi::api::core::v1::Pod {
            serde_json::from_value(json!({
                "metadata": { "name": "p" },
                "spec": { "nodeName": node, "containers": [{ "name": "c", "resources": { "limits": { "nvidia.com/gpu": gpus } } }, { "name": "side" }] },
                "status": { "phase": phase },
            }))
            .unwrap()
        };
        let used = used_per_node(&[
            pod("a", "1", "Running"),
            pod("a", "2", "Pending"),
            pod("a", "1", "Succeeded"),
            pod("b", "0", "Running"),
        ]);
        assert_eq!(used.get("a"), Some(&3));
        assert_eq!(used.get("b"), None);
        let tainted = node(
            json!({ "metadata": { "name": "g" }, "spec": { "taints": [{ "key": "nvidia.com/gpu", "value": "present", "effect": "NoSchedule" }] } }),
        );
        assert!(dedicated(&tainted));
        assert!(!dedicated(&node(json!({ "metadata": { "name": "h" } }))));
    }

    #[test]
    fn device_plugin_only_on_gpu_servers() {
        let ds: DaemonSet = serde_json::from_value(device_plugin()).unwrap();
        let spec = ds.spec.unwrap().template.spec.unwrap();
        assert_eq!(spec.runtime_class_name.as_deref(), Some("nvidia"));
        assert_eq!(
            spec.node_selector
                .unwrap()
                .get(GPU_LABEL)
                .map(String::as_str),
            Some("nvidia")
        );
        assert_eq!(
            spec.containers[0].image.as_deref(),
            Some(DEVICE_PLUGIN_IMAGE)
        );
        let rc: RuntimeClass = serde_json::from_value(runtime_class()).unwrap();
        assert_eq!(rc.handler, "nvidia");
    }

    /// Against a disposable Kubernetes API server (no kubelet), with nodes
    /// made by hand: PYO_TEST_KUBECONFIG=<throwaway> cargo test -p pyo-agent -- --ignored gpu_setup_against
    #[tokio::test]
    #[ignore = "needs a throwaway Kubernetes API server"]
    async fn gpu_setup_against_a_cluster() {
        let Ok(path) = std::env::var("PYO_TEST_KUBECONFIG") else {
            panic!("set PYO_TEST_KUBECONFIG to a throwaway cluster's kubeconfig")
        };
        let kc = kube::config::Kubeconfig::read_from(&path).unwrap();
        let client = Client::try_from(
            kube::Config::from_custom_kubeconfig(kc, &Default::default())
                .await
                .unwrap(),
        )
        .unwrap();
        let nodes: Api<Node> = Api::all(client.clone());
        let rcs: Api<RuntimeClass> = Api::all(client.clone());
        let ds: Api<DaemonSet> = Api::namespaced(client.clone(), NS);
        let pp = PatchParams::apply("test").force();
        // No GPU server: nothing is installed.
        nodes
            .patch(
                "plain",
                &pp,
                &Patch::Apply(
                    json!({ "apiVersion": "v1", "kind": "Node", "metadata": { "name": "plain" } }),
                ),
            )
            .await
            .unwrap();
        assert!(!ensure(&client).await.unwrap());
        assert!(rcs.get_opt("nvidia").await.unwrap().is_none());
        // A server set up by hand: k3s lists the nvidia runtime.
        nodes
            .patch(
                "gpu-1",
                &pp,
                &Patch::Apply(
                    json!({ "apiVersion": "v1", "kind": "Node", "metadata": { "name": "gpu-1" } }),
                ),
            )
            .await
            .unwrap();
        nodes
            .patch_status("gpu-1", &PatchParams::default(), &Patch::Merge(json!({ "status": { "runtimeHandlers": [{ "name": "" }, { "name": "nvidia" }], "allocatable": { "nvidia.com/gpu": "2" }, "capacity": { "nvidia.com/gpu": "2" } } })))
            .await
            .unwrap();
        assert!(ensure(&client).await.unwrap());
        assert!(ensure(&client).await.unwrap(), "it's idempotent");
        let n = nodes.get("gpu-1").await.unwrap();
        assert_eq!(
            n.labels().get(GPU_LABEL).map(String::as_str),
            Some("nvidia")
        );
        assert_eq!(node_gpu(&n).0, 2);
        assert_eq!(rcs.get("nvidia").await.unwrap().handler, "nvidia");
        let plugin = ds.get(DEVICE_PLUGIN).await.unwrap();
        assert_eq!(
            plugin
                .spec
                .unwrap()
                .template
                .spec
                .unwrap()
                .node_selector
                .unwrap()
                .get(GPU_LABEL)
                .map(String::as_str),
            Some("nvidia")
        );
        let r = report(
            &client,
            &nodes.list(&ListParams::default()).await.unwrap().items,
            &Default::default(),
        )
        .await;
        assert_eq!(
            (r.servers, r.total, r.max_per_server, r.runtime_class),
            (1, 2, 2, true)
        );
        // Kept for GPU work, then not: only Pyo's taint is touched.
        nodes.patch("gpu-1", &PatchParams::default(), &Patch::Merge(json!({ "spec": { "taints": [{ "key": "other", "value": "x", "effect": "NoSchedule" }] } }))).await.unwrap();
        apply_policies(
            &client,
            &[GpuPolicy {
                server: "gpu-1".into(),
                dedicated: true,
            }],
        )
        .await;
        let n = nodes.get("gpu-1").await.unwrap();
        assert!(dedicated(&n));
        assert_eq!(n.spec.as_ref().unwrap().taints.as_ref().unwrap().len(), 2);
        apply_policies(
            &client,
            &[GpuPolicy {
                server: "gpu-1".into(),
                dedicated: false,
            }],
        )
        .await;
        let n = nodes.get("gpu-1").await.unwrap();
        assert!(!dedicated(&n));
        assert_eq!(n.spec.unwrap().taints.unwrap()[0].key, "other");
    }
}
