//! What the agent reports every heartbeat: servers, workloads, the health of
//! services Pyo manages, volume binding, and what the cluster can do.

use std::collections::{BTreeMap, HashMap};

use k8s_openapi::api::{
    apps::v1::{DaemonSet, Deployment, StatefulSet},
    batch::v1::{CronJob, Job},
    core::v1::{Namespace, Node, PersistentVolumeClaim, Pod, Service},
    networking::v1::NetworkPolicy,
    storage::v1::StorageClass,
};
use kube::{
    Api, Client, ResourceExt,
    api::{DynamicObject, ListParams},
    core::{ApiResource, GroupVersionKind},
};
use pyo_proto::{
    Capabilities, Heartbeat, PROTOCOL_VERSION, ServerCapacity, ServerCondition, ServerReport,
    ServerRoom, ServerUsage, ServiceRuntime, StorageClassReport, VolumeReport, WorkloadReport,
};

pub const SERVICE_LABEL: &str = "pyo/service-id";
pub use pyo_proto::rbac::VOLUME_LABEL;
pub const MAINTENANCE_ANNOTATION: &str = "pyo/maintenance";
const SYSTEM_NAMESPACES: [&str; 5] = [
    "kube-system",
    "kube-public",
    "pyo-system",
    "pyo-gateway",
    "cert-manager",
];

fn ms(t: &k8s_openapi::apimachinery::pkg::apis::meta::v1::Time) -> i64 {
    t.0.as_millisecond()
}

/// "250m" -> 0.25 cores; "2" -> 2.0; "1500000n" -> 0.0015.
pub fn cpu_cores(q: &str) -> f64 {
    let (num, mult) = if let Some(n) = q.strip_suffix('n') {
        (n, 1e-9)
    } else if let Some(u) = q.strip_suffix('u') {
        (u, 1e-6)
    } else if let Some(m) = q.strip_suffix('m') {
        (m, 1e-3)
    } else {
        (q, 1.0)
    };
    num.parse::<f64>().unwrap_or(0.0) * mult
}

/// "512Mi" -> bytes; supports binary and decimal suffixes.
pub fn bytes(q: &str) -> f64 {
    const SUFFIXES: [(&str, f64); 12] = [
        ("Ki", 1024.0),
        ("Mi", 1048576.0),
        ("Gi", 1073741824.0),
        ("Ti", 1099511627776.0),
        ("Pi", 1125899906842624.0),
        ("Ei", 1152921504606846976.0),
        ("k", 1e3),
        ("M", 1e6),
        ("G", 1e9),
        ("T", 1e12),
        ("P", 1e15),
        ("E", 1e18),
    ];
    for (s, m) in SUFFIXES {
        if let Some(n) = q.strip_suffix(s) {
            return n.parse::<f64>().unwrap_or(0.0) * m;
        }
    }
    q.parse::<f64>().unwrap_or(0.0)
}

const GIB: f64 = 1073741824.0;

fn round1(x: f64) -> f64 {
    (x * 10.0).round() / 10.0
}

fn round2(x: f64) -> f64 {
    (x * 100.0).round() / 100.0
}
const MIB: f64 = 1048576.0;

/// CPU (cores) and memory (bytes) a pod asks for, as the scheduler counts
/// it: its containers together, or its largest init container when that's
/// more, plus the runtime's overhead.
pub(crate) fn pod_requests(p: &Pod) -> (f64, f64) {
    let Some(spec) = p.spec.as_ref() else {
        return (0.0, 0.0);
    };
    let of = |c: &k8s_openapi::api::core::v1::Container| {
        let r = c.resources.as_ref().and_then(|r| r.requests.as_ref());
        (
            r.and_then(|r| r.get("cpu"))
                .map(|q| cpu_cores(&q.0))
                .unwrap_or(0.0),
            r.and_then(|r| r.get("memory"))
                .map(|q| bytes(&q.0))
                .unwrap_or(0.0),
        )
    };
    let (mut cpu, mut mem) = spec
        .containers
        .iter()
        .map(of)
        .fold((0.0, 0.0), |(a, b), (c, m)| (a + c, b + m));
    for (c, m) in spec.init_containers.iter().flatten().map(of) {
        cpu = f64::max(cpu, c);
        mem = f64::max(mem, m);
    }
    if let Some(o) = spec.overhead.as_ref() {
        cpu += o.get("cpu").map(|q| cpu_cores(&q.0)).unwrap_or(0.0);
        mem += o.get("memory").map(|q| bytes(&q.0)).unwrap_or(0.0);
    }
    (cpu, mem)
}

/// A pod no server has room for: the scheduler found too little free CPU,
/// memory or GPU. It starts when there's room.
fn waiting_for_room(p: &Pod) -> bool {
    p.metadata.deletion_timestamp.is_none()
        && p.spec.as_ref().and_then(|s| s.node_name.as_ref()).is_none()
        && p.status
            .as_ref()
            .and_then(|s| s.conditions.as_ref())
            .is_some_and(|cs| {
                cs.iter().any(|c| {
                    c.type_ == "PodScheduled"
                        && c.status == "False"
                        && c.reason.as_deref() == Some("Unschedulable")
                        && c.message
                            .as_deref()
                            .is_some_and(|m| m.contains("Insufficient"))
                })
            })
}

/// What the pods placed on each server ask for (cores, bytes); finished
/// pods hold nothing.
fn requested_per_node(pods: &[Pod]) -> HashMap<String, (f64, f64)> {
    let mut out: HashMap<String, (f64, f64)> = HashMap::new();
    for p in pods {
        let phase = p
            .status
            .as_ref()
            .and_then(|s| s.phase.as_deref())
            .unwrap_or("");
        let Some(node) = p.spec.as_ref().and_then(|s| s.node_name.clone()) else {
            continue;
        };
        if phase == "Succeeded" || phase == "Failed" {
            continue;
        }
        let (c, m) = pod_requests(p);
        let e = out.entry(node).or_default();
        e.0 += c;
        e.1 += m;
    }
    out
}

/// A claim's requested size, in GiB.
fn claim_gi(p: &PersistentVolumeClaim) -> Option<f64> {
    p.spec
        .as_ref()?
        .resources
        .as_ref()?
        .requests
        .as_ref()?
        .get("storage")
        .map(|q| bytes(&q.0) / GIB)
}

/// A claim's size as provisioned (its status), in GiB.
fn claim_capacity_gi(p: &PersistentVolumeClaim) -> Option<f64> {
    p.status
        .as_ref()?
        .capacity
        .as_ref()?
        .get("storage")
        .map(|q| bytes(&q.0) / GIB)
}

/// Pod and node usage from metrics-server, when it's installed.
struct Metrics {
    nodes: HashMap<String, (f64, f64)>,
    /// (namespace, pod) -> (cores, bytes)
    pods: HashMap<(String, String), (f64, f64)>,
    available: bool,
}

async fn metrics(client: &Client) -> Metrics {
    let gvk = |kind: &str| GroupVersionKind::gvk("metrics.k8s.io", "v1beta1", kind);
    let node_ar = ApiResource::from_gvk_with_plural(&gvk("NodeMetrics"), "nodes");
    let pod_ar = ApiResource::from_gvk_with_plural(&gvk("PodMetrics"), "pods");
    let mut out = Metrics {
        nodes: HashMap::new(),
        pods: HashMap::new(),
        available: false,
    };
    if let Ok(list) = Api::<DynamicObject>::all_with(client.clone(), &node_ar)
        .list(&ListParams::default())
        .await
    {
        out.available = true;
        for n in list.items {
            let u = &n.data["usage"];
            out.nodes.insert(
                n.name_any(),
                (
                    cpu_cores(u["cpu"].as_str().unwrap_or("0")),
                    bytes(u["memory"].as_str().unwrap_or("0")),
                ),
            );
        }
    }
    if let Ok(list) = Api::<DynamicObject>::all_with(client.clone(), &pod_ar)
        .list(&ListParams::default())
        .await
    {
        for p in list.items {
            let (mut cpu, mut mem) = (0.0, 0.0);
            for c in p.data["containers"].as_array().into_iter().flatten() {
                cpu += cpu_cores(c["usage"]["cpu"].as_str().unwrap_or("0"));
                mem += bytes(c["usage"]["memory"].as_str().unwrap_or("0"));
            }
            out.pods.insert(
                (p.namespace().unwrap_or_default(), p.name_any()),
                (cpu, mem),
            );
        }
    }
    out
}

/// The newest of a service's Jobs, and how it went.
fn last_run(jobs: &[Job], label: &str, id: &str) -> Option<pyo_proto::CronRun> {
    let job = jobs
        .iter()
        .filter(|j| j.labels().get(label).is_some_and(|v| v == id))
        .max_by_key(|j| j.metadata.creation_timestamp.as_ref().map(ms).unwrap_or(0))?;
    let st = job.status.as_ref();
    let done = |n: Option<i32>| n.unwrap_or(0) > 0;
    let succeeded = if done(st.and_then(|s| s.succeeded)) {
        Some(true)
    } else if done(st.and_then(|s| s.failed)) {
        Some(false)
    } else {
        None
    };
    Some(pyo_proto::CronRun {
        started_at: job
            .metadata
            .creation_timestamp
            .as_ref()
            .map(ms)
            .unwrap_or(0),
        succeeded,
    })
}

fn matches(selector: &Option<BTreeMap<String, String>>, labels: &BTreeMap<String, String>) -> bool {
    match selector {
        Some(sel) if !sel.is_empty() => sel.iter().all(|(k, v)| labels.get(k) == Some(v)),
        _ => false,
    }
}

/// The pods the heartbeat reads: those that haven't ended, and the ended
/// ones of Pyo's Jobs (a cron run's outcome is in its pod). Never the rest
/// of the ended pods, however many a cluster keeps.
async fn live_pods(client: &Client) -> kube::Result<Vec<Pod>> {
    let api = Api::<Pod>::all(client.clone());
    let mut pods =
        crate::pods::list_pods(&api, &ListParams::default().fields(crate::pods::LIVE)).await?;
    for owner in [crate::cron::CRON_SERVICE_LABEL, SERVICE_LABEL] {
        let lp = ListParams::default()
            .labels(&format!("{owner},{}", crate::runs::JOB_LABEL))
            .fields(crate::pods::ENDED);
        pods.extend(crate::pods::list_pods(&api, &lp).await?);
    }
    Ok(pods)
}

/// Pyo's Jobs (a service's cron runs and its cron tasks'), without their
/// pod templates.
async fn pyo_jobs(client: &Client) -> kube::Result<Vec<Job>> {
    let api = Api::<Job>::all(client.clone());
    let mut jobs = vec![];
    for owner in [crate::cron::CRON_SERVICE_LABEL, SERVICE_LABEL] {
        jobs.extend(
            crate::pods::list(&api, &ListParams::default().labels(owner), |mut j: Job| {
                j.metadata.managed_fields = None;
                j.spec = None;
                Some(j)
            })
            .await?,
        );
    }
    Ok(jobs)
}

pub async fn heartbeat(client: &Client) -> anyhow::Result<Heartbeat> {
    let version = client.apiserver_version().await?.git_version;
    // Paged and cut down (pods.rs): memory follows what runs, not how many
    // pods ended.
    let lp = ListParams::default();
    let nodes = crate::pods::list(&Api::<Node>::all(client.clone()), &lp, |mut n: Node| {
        n.metadata.managed_fields = None;
        // Every image on the server: long, and never read.
        if let Some(s) = n.status.as_mut() {
            s.images = None;
        }
        Some(n)
    })
    .await?;
    let pods = live_pods(client).await?;
    let deployments = crate::pods::list(
        &Api::<Deployment>::all(client.clone()),
        &lp,
        |mut d: Deployment| {
            d.metadata.managed_fields = None;
            if let Some(s) = d.spec.as_mut() {
                s.template = Default::default();
            }
            Some(d)
        },
    )
    .await?;
    let statefulsets = crate::pods::list(
        &Api::<StatefulSet>::all(client.clone()),
        &lp,
        |mut d: StatefulSet| {
            d.metadata.managed_fields = None;
            if let Some(s) = d.spec.as_mut() {
                s.template = Default::default();
                s.volume_claim_templates = None;
            }
            Some(d)
        },
    )
    .await?;
    let daemonsets = crate::pods::list(
        &Api::<DaemonSet>::all(client.clone()),
        &lp,
        |mut d: DaemonSet| {
            d.metadata.managed_fields = None;
            if let Some(s) = d.spec.as_mut() {
                s.template = Default::default();
            }
            Some(d)
        },
    )
    .await?;
    let cronjobs = crate::pods::list(
        &Api::<CronJob>::all(client.clone()),
        &lp,
        |mut c: CronJob| {
            c.metadata.managed_fields = None;
            c.spec.job_template = Default::default();
            Some(c)
        },
    )
    .await?;
    let jobs = pyo_jobs(client).await?;
    let bundle_namespaces = Api::<Namespace>::all(client.clone())
        .list(&ListParams::default().labels(pyo_proto::bundles::BUNDLE_LABEL))
        .await
        .map(|l| l.items)
        .unwrap_or_default();
    let bundle_ns = crate::bundles::bundle_namespaces(&bundle_namespaces);
    // CPU and memory from metrics-server; disk and network from the kubelets,
    // fetched together.
    let names: Vec<String> = nodes.iter().map(|n| n.name_any()).collect();
    // Claims' sizes tell a volume's own space from its server's disk.
    let claims =
        crate::pods::list_slim(&Api::<PersistentVolumeClaim>::all(client.clone()), &lp).await?;
    let sizes: HashMap<(String, String), f64> = claims
        .iter()
        .filter_map(|p| Some(((p.namespace()?, p.name_any()), claim_gi(p)?)))
        .collect();
    let (m, k) = futures::join!(
        metrics(client),
        crate::kubelet::collect(client, &names, &sizes)
    );

    // ── Servers ──
    let gpus_used = crate::gpu::used_per_node(&pods);
    let requested = requested_per_node(&pods);
    let mut pods_per_node: HashMap<String, u32> = HashMap::new();
    for p in &pods {
        if let Some(n) = p.spec.as_ref().and_then(|s| s.node_name.clone()) {
            let phase = p
                .status
                .as_ref()
                .and_then(|s| s.phase.as_deref())
                .unwrap_or("");
            if phase != "Succeeded" && phase != "Failed" {
                *pods_per_node.entry(n).or_default() += 1;
            }
        }
    }
    let node_reports = nodes
        .iter()
        .map(|n| {
            let name = n.name_any();
            let labels = n.labels();
            let mut roles: Vec<String> = vec![];
            if labels.contains_key("node-role.kubernetes.io/control-plane")
                || labels.contains_key("node-role.kubernetes.io/master")
            {
                roles.push("control-plane".into());
            }
            if labels.contains_key("node-role.kubernetes.io/etcd") {
                roles.push("etcd".into());
            }
            // k3s servers run workloads unless tainted.
            let tainted = n
                .spec
                .as_ref()
                .and_then(|s| s.taints.as_ref())
                .is_some_and(|ts| {
                    ts.iter()
                        .any(|t| t.effect == "NoSchedule" && t.key.contains("control-plane"))
                });
            if roles.is_empty() || !tainted {
                roles.push("worker".into());
            }
            let status = n.status.clone().unwrap_or_default();
            let ready = status
                .conditions
                .as_ref()
                .and_then(|cs| cs.iter().find(|c| c.type_ == "Ready"))
                .map(|c| c.status.as_str());
            let info = status.node_info.clone().unwrap_or_default();
            let cap = status.capacity.clone().unwrap_or_default();
            let alloc = status.allocatable.clone().unwrap_or_default();
            let (rcpu, rmem) = requested.get(&name).copied().unwrap_or((0.0, 0.0));
            let addr = |kind: &str| {
                status
                    .addresses
                    .as_ref()
                    .and_then(|a| a.iter().find(|x| x.type_ == kind))
                    .map(|x| x.address.clone())
            };
            let (ucpu, umem) = m.nodes.get(&name).copied().unwrap_or((0.0, 0.0));
            let (gpus, gpu_detected, gpu_model) = crate::gpu::node_gpu(n);
            ServerReport {
                gpus,
                gpu_detected,
                gpu_model,
                gpus_used: gpus_used.get(&name).copied().unwrap_or(0),
                builder: labels
                    .get(pyo_proto::BUILDER_NODE_LABEL)
                    .is_some_and(|v| v == "true"),
                gpu_dedicated: crate::gpu::dedicated(n),
                gpu_memory_mib: crate::gpu::memory_mib(n),
                gpu_status: crate::gpu::status(n).into(),
                roles,
                status: match ready {
                    Some("True") => "ready",
                    Some("False") => "not_ready",
                    _ => "unknown",
                }
                .into(),
                schedulable: !n
                    .spec
                    .as_ref()
                    .and_then(|s| s.unschedulable)
                    .unwrap_or(false),
                maintenance: n.annotations().contains_key(MAINTENANCE_ANNOTATION),
                kubelet_version: info.kubelet_version,
                os_image: info.os_image,
                arch: info.architecture,
                container_runtime: info.container_runtime_version,
                internal_ip: addr("InternalIP").unwrap_or_default(),
                external_ip: addr("ExternalIP"),
                capacity: ServerCapacity {
                    cpu: cap.get("cpu").map(|q| cpu_cores(&q.0)).unwrap_or(0.0),
                    memory_gi: cap
                        .get("memory")
                        .map(|q| round1(bytes(&q.0) / GIB))
                        .unwrap_or(0.0),
                    pods: cap.get("pods").and_then(|q| q.0.parse().ok()).unwrap_or(0),
                },
                usage: {
                    let ks = k.nodes.get(&name).cloned().unwrap_or_default();
                    ServerUsage {
                        cpu: round2(ucpu),
                        memory_gi: round2(umem / GIB),
                        root_fs: ks.root_fs,
                        image_fs: ks.image_fs,
                        net_rx_bps: ks.net_rx_bps,
                        net_tx_bps: ks.net_tx_bps,
                    }
                },
                allocatable: ServerRoom {
                    cpu: round2(alloc.get("cpu").map(|q| cpu_cores(&q.0)).unwrap_or(0.0)),
                    memory_gi: round2(
                        alloc.get("memory").map(|q| bytes(&q.0)).unwrap_or(0.0) / GIB,
                    ),
                },
                requested: ServerRoom {
                    cpu: round2(rcpu),
                    memory_gi: round2(rmem / GIB),
                },
                pod_count: pods_per_node.get(&name).copied().unwrap_or(0),
                conditions: status
                    .conditions
                    .unwrap_or_default()
                    .into_iter()
                    .map(|c| ServerCondition {
                        kind: c.type_,
                        status: c.status,
                        message: c.message.unwrap_or_default(),
                    })
                    .collect(),
                created_at: n.metadata.creation_timestamp.as_ref().map(ms).unwrap_or(0),
                public_ip: n
                    .annotations()
                    .get(pyo_proto::PUBLIC_IP_ANNOTATION)
                    .cloned(),
                public_ipv4: n
                    .annotations()
                    .get(pyo_proto::PUBLIC_IPV4_ANNOTATION)
                    .cloned(),
                public_ipv6: n
                    .annotations()
                    .get(pyo_proto::PUBLIC_IPV6_ANNOTATION)
                    .cloned(),
                addresses: status
                    .addresses
                    .iter()
                    .flatten()
                    .filter(|a| a.type_ == "InternalIP" || a.type_ == "ExternalIP")
                    .map(|a| a.address.clone())
                    .collect(),
                public_country: n
                    .annotations()
                    .get(pyo_proto::PUBLIC_COUNTRY_ANNOTATION)
                    .cloned(),
                cloud_public_ip: n
                    .annotations()
                    .get(pyo_proto::CLOUD_PUBLIC_IPV4_ANNOTATION)
                    .filter(|ip| !ip.is_empty())
                    .cloned(),
                nic_public_ips: n
                    .annotations()
                    .get(pyo_proto::NIC_PUBLIC_IPV4_ANNOTATION)
                    .map(|v| {
                        v.split(',')
                            .map(str::trim)
                            .filter(|ip| !ip.is_empty())
                            .map(str::to_owned)
                            .collect()
                    })
                    .unwrap_or_default(),
                // What the server's own interfaces have (locate.rs), else a
                // node address on the tailnet (a server joined over it).
                tailnet_ip: n
                    .annotations()
                    .get(pyo_proto::TAILNET_IPV4_ANNOTATION)
                    .filter(|ip| !ip.is_empty())
                    .cloned()
                    .or_else(|| {
                        status
                            .addresses
                            .iter()
                            .flatten()
                            .map(|a| a.address.as_str())
                            .find(|a| crate::locate::tailnet_v4(a))
                            .map(str::to_owned)
                    }),
                name,
            }
        })
        .collect();

    // ── Workloads, and service health for the ones Pyo manages ──
    // Each Pyo service's internal address, by its service label.
    let cluster_ips: HashMap<String, String> = Api::<Service>::all(client.clone())
        .list(
            &ListParams::default()
                .labels(&format!("app.kubernetes.io/managed-by={}", crate::MANAGER)),
        )
        .await
        .map(|l| l.items)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|s| {
            let id = s.labels().get(SERVICE_LABEL)?.clone();
            let ip = s
                .spec?
                .cluster_ip
                .filter(|ip| !ip.is_empty() && ip != "None")?;
            Some((id, ip))
        })
        .collect();
    let mut workloads = vec![];
    let mut services = vec![];
    let mut add = |kind: &str,
                   meta: &kube::core::ObjectMeta,
                   selector: Option<BTreeMap<String, String>>,
                   desired: i32,
                   ready: i32| {
        let ns = meta.namespace.clone().unwrap_or_default();
        let own: Vec<&Pod> = pods
            .iter()
            .filter(|p| {
                p.namespace().as_deref() == Some(ns.as_str()) && matches(&selector, p.labels())
            })
            .collect();
        let (mut cpu, mut mem) = (0.0, 0.0);
        let mut restarts = 0;
        let mut since: Option<i64> = None;
        let mut on_nodes: Vec<String> = vec![];
        let mut replicas: Vec<pyo_proto::ReplicaReport> = vec![];
        let mut waiting = 0;
        // Storage and network: summed over replicas the kubelet reported on.
        let (mut storage, mut rx, mut tx, mut seen) = (0.0, None::<f64>, None::<f64>, false);
        for p in &own {
            let ks = k.pods.get(&(ns.clone(), p.name_any()));
            if let Some(ks) = ks {
                seen = true;
                storage += ks.ephemeral_mi.unwrap_or(0.0) + ks.volumes_mi;
                if let Some(r) = ks.net_rx_bps {
                    rx = Some(rx.unwrap_or(0.0) + r);
                }
                if let Some(t) = ks.net_tx_bps {
                    tx = Some(tx.unwrap_or(0.0) + t);
                }
            }
            let no_room = waiting_for_room(p);
            waiting += i32::from(no_room);
            replicas.push(pyo_proto::ReplicaReport {
                waiting: no_room,
                ephemeral_mi: ks.and_then(|x| x.ephemeral_mi),
                net_rx_bps: ks.and_then(|x| x.net_rx_bps),
                net_tx_bps: ks.and_then(|x| x.net_tx_bps),
                name: p.name_any(),
                server: p
                    .spec
                    .as_ref()
                    .and_then(|s| s.node_name.clone())
                    .unwrap_or_default(),
                ready: p.metadata.deletion_timestamp.is_none()
                    && p.status
                        .as_ref()
                        .and_then(|s| s.conditions.as_ref())
                        .is_some_and(|cs| {
                            cs.iter().any(|c| c.type_ == "Ready" && c.status == "True")
                        }),
                started_at: p
                    .status
                    .as_ref()
                    .and_then(|s| s.start_time.as_ref())
                    .map(ms),
            });
            if let Some(n) = p.spec.as_ref().and_then(|s| s.node_name.clone())
                && !on_nodes.contains(&n)
            {
                on_nodes.push(n);
            }
            if let Some((c, b)) = m.pods.get(&(ns.clone(), p.name_any())) {
                cpu += c;
                mem += b;
            }
            if let Some(st) = &p.status {
                for cs in st.container_statuses.iter().flatten() {
                    restarts += cs.restart_count;
                }
                if let Some(t) = st.start_time.as_ref().map(ms) {
                    since = Some(since.map_or(t, |s| s.min(t)));
                }
            }
        }
        let service_id = meta
            .labels
            .as_ref()
            .and_then(|l| l.get(SERVICE_LABEL))
            .cloned();
        let status = if desired == 0 {
            "stopped"
        } else if ready >= desired {
            "healthy"
        } else if ready == 0 {
            "failed"
        } else {
            "degraded"
        };
        if let Some(id) = &service_id {
            services.push(ServiceRuntime {
                cluster_ip: cluster_ips.get(id.as_str()).cloned(),
                service_id: id.clone(),
                desired,
                last_run: None,
                cron_jobs: vec![],
                runs: vec![],
                ready_replicas: ready,
                restarts,
                cpu_millis: cpu * 1000.0,
                memory_mi: mem / MIB,
                since,
                replicas,
                waiting,
                storage_mi: seen.then_some((storage * 10.0).round() / 10.0),
                net_rx_bps: rx,
                net_tx_bps: tx,
            });
        }
        workloads.push(WorkloadReport {
            name: meta.name.clone().unwrap_or_default(),
            system: SYSTEM_NAMESPACES.contains(&ns.as_str()) || bundle_ns.contains(&ns),
            namespace: ns,
            kind: kind.into(),
            status: status.into(),
            desired,
            ready,
            cpu_millis: cpu * 1000.0,
            memory_mi: mem / MIB,
            service_id,
            servers: on_nodes,
            created_at: meta.creation_timestamp.as_ref().map(ms).unwrap_or(0),
        });
    };
    for d in &deployments {
        let spec = d.spec.clone().unwrap_or_default();
        let st = d.status.clone().unwrap_or_default();
        add(
            "Deployment",
            &d.metadata,
            spec.selector.match_labels,
            spec.replicas.unwrap_or(1),
            st.ready_replicas.unwrap_or(0),
        );
    }
    for s in &statefulsets {
        let spec = s.spec.clone().unwrap_or_default();
        let st = s.status.clone().unwrap_or_default();
        add(
            "StatefulSet",
            &s.metadata,
            spec.selector.match_labels,
            spec.replicas.unwrap_or(1),
            st.ready_replicas.unwrap_or(0),
        );
    }
    for d in &daemonsets {
        let spec = d.spec.clone().unwrap_or_default();
        let st = d.status.clone().unwrap_or_default();
        add(
            "DaemonSet",
            &d.metadata,
            spec.selector.match_labels,
            st.desired_number_scheduled,
            st.number_ready,
        );
    }
    // Nothing runs between schedules, so "healthy" means it's scheduled.
    for c in cronjobs
        .iter()
        .filter(|c| !c.labels().contains_key(crate::cron::CRON_LABEL))
    {
        let scheduled = i32::from(!c.spec.suspend.unwrap_or(false));
        let selector = c
            .metadata
            .labels
            .as_ref()
            .and_then(|l| l.get(SERVICE_LABEL))
            .map(|id| BTreeMap::from([(SERVICE_LABEL.to_owned(), id.clone())]));
        add("CronJob", &c.metadata, selector, scheduled, scheduled);
    }
    let mut runs = crate::runs::collect(client, &jobs, &pods, pyo_proto::now_ms()).await;
    for a in &mut services {
        a.runs = runs.remove(&a.service_id).unwrap_or_default();
        a.last_run = last_run(&jobs, SERVICE_LABEL, &a.service_id);
        a.cron_jobs = cronjobs
            .iter()
            .filter(|c| c.labels().get(crate::cron::CRON_SERVICE_LABEL) == Some(&a.service_id))
            .filter_map(|c| {
                let id = c.labels().get(crate::cron::CRON_LABEL)?.clone();
                Some(pyo_proto::CronTaskReport {
                    suspended: c.spec.suspend.unwrap_or(false),
                    last_run: last_run(&jobs, crate::cron::CRON_LABEL, &id),
                    id,
                })
            })
            .collect();
    }

    // ── Volumes Pyo created ──
    let volumes = claims
        .iter()
        .filter_map(|p| {
            let id = p.labels().get(VOLUME_LABEL)?.clone();
            let phase = p
                .status
                .as_ref()
                .and_then(|s| s.phase.clone())
                .unwrap_or_default();
            let (status, message) = match phase.as_str() {
                "Bound" => ("bound", None),
                "Lost" => (
                    "error",
                    Some("The volume behind this claim is gone.".to_owned()),
                ),
                _ => (
                    "pending",
                    Some("Waiting for a pod to use it (local-path binds on first use).".to_owned()),
                ),
            };
            let namespace = p.namespace().unwrap_or_default();
            let usage = k.volumes.get(&(namespace.clone(), p.name_any())).copied();
            let size_gi = claim_gi(p).map(|g| g.ceil() as i64).filter(|g| *g > 0);
            let node = p
                .annotations()
                .get("volume.kubernetes.io/selected-node")
                .cloned()
                .unwrap_or_default();
            let capacity_gi = claim_capacity_gi(p)
                .map(|g| g.ceil() as i64)
                .filter(|g| *g > 0);
            // Grown, but its filesystem only once a pod mounts it again.
            let message = message.or_else(|| {
                let pending = p
                    .status
                    .as_ref()
                    .and_then(|s| s.conditions.as_ref())
                    .is_some_and(|c| {
                        c.iter()
                            .any(|c| c.type_ == "FileSystemResizePending" && c.status == "True")
                    });
                pending.then(|| "Grows when its service starts again.".to_owned())
            });
            Some(VolumeReport {
                volume_id: id,
                status: status.into(),
                message,
                usage,
                namespace,
                name: p.name_any(),
                size_gi,
                capacity_gi,
                server: node,
            })
        })
        .collect();

    let (databases, databases_listed) = crate::databases::report(client).await;
    Ok(Heartbeat {
        protocol: PROTOCOL_VERSION,
        agent_version: crate::AGENT_VERSION.into(),
        kubernetes_version: version,
        servers: node_reports,
        workloads,
        services,
        volumes,
        capabilities: capabilities(
            client,
            &nodes,
            &daemonsets,
            &deployments,
            &pods,
            m.available,
            &gpus_used,
        )
        .await,
        certificates: crate::tls::report(client).await,
        public_dashboard: crate::publicdash::report(client).await,
        host_proof: None,
        updates: pyo_proto::UPDATES_VERSION,
        component_keys: true,
        scope: None,
        bundles: crate::bundles::report(client, &bundle_namespaces).await,
        databases,
        databases_listed,
        database_metrics: crate::dbmetrics::report(&pods, &m.pods, &k.volumes),
        tailnet: Vec::new(),
        routes_version: crate::gateway::routes_version().into(),
    })
}

async fn capabilities(
    client: &Client,
    nodes: &[Node],
    daemonsets: &[DaemonSet],
    deployments: &[Deployment],
    pods: &[Pod],
    metrics_server: bool,
    gpus_used: &HashMap<String, u32>,
) -> Capabilities {
    let lp = ListParams::default();
    // The gateway's Service (Envoy Gateway's, in pyo-gateway).
    let proxy_service =
        Api::<Service>::namespaced(client.clone(), pyo_proto::system::GATEWAY_NS)
            .list(&ListParams::default().labels(
                "app.kubernetes.io/component=proxy,app.kubernetes.io/managed-by=envoy-gateway",
            ))
            .await
            .ok()
            .and_then(|l| {
                l.items.into_iter().find(|s| {
                    matches!(
                        s.spec.as_ref().and_then(|s| s.type_.as_deref()),
                        Some("LoadBalancer" | "ClusterIP")
                    )
                })
            });
    let ingress_controller = if proxy_service.is_some() {
        "envoy"
    } else {
        "none"
    };
    let gateway_error = if proxy_service.is_some() {
        None
    } else {
        crate::gateway::install_problem(client).await
    };
    let (waf_rejected, waf_active) = waf_policies(client).await;
    let servicelb = daemonsets
        .iter()
        .any(|d| d.name_any().starts_with("svclb-"));
    let metallb = deployments
        .iter()
        .any(|d| d.namespace().as_deref() == Some("metallb-system"));
    let lb_ips = proxy_service
        .and_then(|s| s.status)
        .and_then(|s| s.load_balancer)
        .and_then(|lb| lb.ingress)
        .map(|ing| {
            ing.into_iter()
                .filter_map(|i| i.ip.or(i.hostname))
                .collect()
        })
        .unwrap_or_default();
    let storage_classes = Api::<StorageClass>::all(client.clone())
        .list(&lp)
        .await
        .map(|l| l.items)
        .unwrap_or_default()
        .into_iter()
        .map(|sc| StorageClassReport {
            is_default: sc
                .annotations()
                .get("storageclass.kubernetes.io/is-default-class")
                .map(|v| v == "true")
                .unwrap_or(false),
            name: sc.name_any(),
            provisioner: sc.provisioner,
            reclaim_policy: sc.reclaim_policy.unwrap_or_else(|| "Delete".into()),
            volume_binding_mode: sc.volume_binding_mode.unwrap_or_else(|| "Immediate".into()),
            allow_volume_expansion: sc.allow_volume_expansion.unwrap_or(false),
        })
        .collect();
    Capabilities {
        ingress_controller: ingress_controller.into(),
        gateway_error,
        gateway_ports_taken: gateway_ports_taken(client, pods).await,
        load_balancer: if metallb {
            "metallb"
        } else if servicelb {
            "servicelb"
        } else {
            "none"
        }
        .into(),
        load_balancer_ips: lb_ips,
        cert_manager: deployments.iter().any(|d| d.name_any() == "cert-manager"),
        metrics_server,
        storage_classes,
        pod_cidr: nodes
            .first()
            .and_then(|n| n.spec.as_ref())
            .and_then(|s| s.pod_cidr.clone())
            .unwrap_or_default(),
        service_cidr: String::new(),
        security: security(client, nodes).await,
        gpu: crate::gpu::report(client, nodes, gpus_used).await,
        regions: true,
        traffic_modes: true,
        traffic_local: true,
        waf_rejected,
        waf_active,
        tailnet_web_ips: tailnet_web_ips(client).await,
    }
}

/// k3s ServiceLB's labels on the pods that publish a LoadBalancer Service
/// on the servers' host ports: the Service's namespace and name.
const SVCLB_NS_LABEL: &str = "svccontroller.k3s.cattle.io/svcnamespace";
const SVCLB_NAME_LABEL: &str = "svccontroller.k3s.cattle.io/svcname";

/// Where the gateway can't get its ports: its ServiceLB pods that wait
/// because another pod on their server has those host ports (k3s's
/// Traefik, ingress-nginx…). Said in the log when it changes.
async fn gateway_ports_taken(client: &Client, live: &[Pod]) -> Vec<pyo_proto::GatewayPortsTaken> {
    let lp = ListParams::default()
        .labels(&format!("{SVCLB_NS_LABEL}={}", pyo_proto::system::GATEWAY_NS))
        .fields("status.phase=Pending");
    let Ok(waiting) = Api::<Pod>::all(client.clone()).list(&lp).await else {
        return vec![];
    };
    let taken = ports_taken(&waiting.items, live);
    if let Ok(mut last) = PORTS_TAKEN.lock()
        && *last != taken
    {
        for t in &taken {
            tracing::warn!(server = %t.server, ports = ?t.ports, by = %t.by, "the gateway can't get its ports there: another program in the cluster has them");
        }
        if taken.is_empty() {
            tracing::info!("the gateway has its ports on every server");
        }
        *last = taken.clone();
    }
    taken
}

/// What the last heartbeat found of [`gateway_ports_taken`].
static PORTS_TAKEN: std::sync::Mutex<Vec<pyo_proto::GatewayPortsTaken>> =
    std::sync::Mutex::new(Vec::new());

/// The gateway's ServiceLB pods that `waiting` holds, unscheduled for lack of
/// free ports: each one's server (the one its DaemonSet aims it at) and the
/// ports another pod there in `live` has, with what that is. Every host
/// port it asks for when none is found there. By server, once each.
pub fn ports_taken(waiting: &[Pod], live: &[Pod]) -> Vec<pyo_proto::GatewayPortsTaken> {
    let host_ports = |p: &Pod| -> Vec<(u16, String)> {
        p.spec
            .iter()
            .flat_map(|s| s.containers.iter())
            .flat_map(|c| c.ports.iter().flatten())
            .filter_map(|cp| {
                let port = u16::try_from(cp.host_port?).ok()?;
                Some((port, cp.protocol.clone().unwrap_or_else(|| "TCP".into())))
            })
            .collect()
    };
    let ours = |p: &Pod| {
        p.labels().get(SVCLB_NS_LABEL).map(String::as_str) == Some(pyo_proto::system::GATEWAY_NS)
    };
    let mut out: BTreeMap<String, (std::collections::BTreeSet<u16>, String)> = BTreeMap::new();
    for pod in waiting.iter().filter(|p| ours(p)) {
        let no_ports = pod
            .status
            .as_ref()
            .and_then(|s| s.conditions.as_ref())
            .into_iter()
            .flatten()
            .any(|c| {
                c.type_ == "PodScheduled"
                    && c.status == "False"
                    && c.message.as_deref().is_some_and(|m| m.contains("free ports"))
            });
        let Some(server) = target_server(pod) else {
            continue;
        };
        if !no_ports {
            continue;
        }
        let wanted = host_ports(pod);
        let entry = out.entry(server.clone()).or_default();
        let mut found = false;
        for other in live.iter().filter(|o| {
            !ours(o)
                && o.spec.as_ref().and_then(|s| s.node_name.as_deref()) == Some(server.as_str())
        }) {
            let theirs = host_ports(other);
            for (port, proto) in &wanted {
                if theirs.iter().any(|(p, pr)| p == port && pr == proto) {
                    found = true;
                    entry.0.insert(*port);
                    if entry.1.is_empty() {
                        entry.1 = holder_name(other);
                    }
                }
            }
        }
        if !found {
            entry.0.extend(wanted.iter().map(|(p, _)| *p));
        }
    }
    out.into_iter()
        .map(|(server, (ports, by))| pyo_proto::GatewayPortsTaken {
            server,
            ports: ports.into_iter().collect(),
            by,
        })
        .collect()
}

/// The server a DaemonSet's pod is for: its node affinity on the server's
/// name (set before it's scheduled), else where it runs.
fn target_server(pod: &Pod) -> Option<String> {
    let spec = pod.spec.as_ref()?;
    spec.affinity
        .as_ref()
        .and_then(|a| a.node_affinity.as_ref())
        .and_then(|n| n.required_during_scheduling_ignored_during_execution.as_ref())
        .into_iter()
        .flat_map(|r| r.node_selector_terms.iter())
        .flat_map(|t| t.match_fields.iter().flatten())
        .find(|f| f.key == "metadata.name" && f.operator == "In")
        .and_then(|f| f.values.as_ref()?.first().cloned())
        .or_else(|| spec.node_name.clone())
}

/// What a pod holding host ports is, in words people know: "Traefik",
/// "ingress-nginx", else the Service ServiceLB publishes, else its name.
fn holder_name(pod: &Pod) -> String {
    let name = pod
        .labels()
        .get(SVCLB_NAME_LABEL)
        .cloned()
        .unwrap_or_else(|| pod.name_any());
    let ns = pod.namespace().unwrap_or_default();
    if name.contains("traefik") {
        "Traefik".into()
    } else if ns == "ingress-nginx" || name.contains("ingress-nginx") {
        "ingress-nginx".into()
    } else {
        name
    }
}

/// The Tailscale addresses that take websites' connections (ports 80 and
/// 443) whatever the cluster's public traffic: where the agent published
/// them (gateway.rs `set_tailnet_web`).
async fn tailnet_web_ips(client: &Client) -> Vec<String> {
    let Ok(svc) = Api::<Service>::namespaced(client.clone(), pyo_proto::system::GATEWAY_NS)
        .get_opt(pyo_proto::system::TAILNET_WEB_SERVICE)
        .await
    else {
        return vec![];
    };
    svc.and_then(|s| s.spec)
        .and_then(|s| s.external_ips)
        .unwrap_or_default()
        .into_iter()
        .filter(|ip| crate::locate::tailnet_v4(ip))
        .collect()
}

/// Services (by id) with a WAF policy the gateway refused (Accepted
/// false): those sites pass unfiltered; and those whose every WAF policy
/// it accepted: their WAF is in effect.
async fn waf_policies(client: &Client) -> (Vec<String>, Vec<String>) {
    use std::collections::BTreeSet;
    let api = Api::<DynamicObject>::namespaced_with(
        client.clone(),
        pyo_proto::system::GATEWAY_NS,
        &crate::gateway::extension_policies(),
    );
    let Ok(list) = api.list(&ListParams::default()).await else {
        return (vec![], vec![]);
    };
    let waf = || {
        list.items
            .iter()
            .filter(|p| p.annotations().contains_key(crate::gateway::WAF_ANNOTATION))
    };
    let service = |p: &DynamicObject| p.labels().get(SERVICE_LABEL).cloned();
    let rejected: BTreeSet<String> = waf()
        .filter(|p| refused(&p.data["status"]))
        .filter_map(service)
        .collect();
    let waiting: BTreeSet<String> = waf()
        .filter(|p| !accepted(&p.data["status"]))
        .filter_map(service)
        .collect();
    let active: BTreeSet<String> = waf()
        .filter_map(service)
        .filter(|s| !waiting.contains(s))
        .collect();
    (rejected.into_iter().collect(), active.into_iter().collect())
}

fn conditions(status: &serde_json::Value) -> impl Iterator<Item = &serde_json::Value> {
    status["ancestors"]
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|a| a["conditions"].as_array().into_iter().flatten())
}

/// A policy's status says one of its Gateways didn't accept it.
fn refused(status: &serde_json::Value) -> bool {
    conditions(status).any(|c| c["type"] == "Accepted" && c["status"] == "False")
}

/// The gateway took the policy: it says so, and nothing refused it.
fn accepted(status: &serde_json::Value) -> bool {
    conditions(status).any(|c| c["type"] == "Accepted" && c["status"] == "True") && !refused(status)
}

/// k3s records each server's flags on its node; namespaces and policies
/// say the rest.
async fn security(client: &Client, nodes: &[Node]) -> pyo_proto::SecurityReport {
    let secrets_encryption = nodes.iter().find_map(|n| {
        let args: Vec<String> =
            serde_json::from_str(n.annotations().get("k3s.io/node-args")?).ok()?;
        if args.first().map(String::as_str) != Some("server") {
            return None;
        }
        let flag = |name: &str| {
            args.iter()
                .position(|a| a == name || a.starts_with(&format!("{name}=")))
                .map(|i| {
                    args[i]
                        .split_once('=')
                        .map(|(_, v)| v.to_owned())
                        .or_else(|| args.get(i + 1).filter(|v| !v.starts_with("--")).cloned())
                        .unwrap_or_else(|| "true".into())
                })
        };
        Some(match flag("--secrets-encryption").as_deref() {
            Some("true") => {
                flag("--secrets-encryption-provider").unwrap_or_else(|| "aescbc".into())
            }
            _ => String::new(),
        })
    });
    let lp =
        ListParams::default().labels(&format!("app.kubernetes.io/managed-by={}", crate::MANAGER));
    let namespaces = Api::<Namespace>::all(client.clone())
        .list(&lp)
        .await
        .map(|l| l.items)
        .unwrap_or_default();
    let policies = Api::<NetworkPolicy>::all(client.clone())
        .list(&ListParams::default())
        .await
        .map(|l| l.items)
        .unwrap_or_default();
    let pods = crate::pods::list_pods(
        &Api::<Pod>::all(client.clone()),
        &lp.clone().fields(crate::pods::LIVE),
    )
    .await
    .unwrap_or_default();
    let mut report = pyo_proto::SecurityReport {
        secrets_encryption,
        namespaces: namespaces.len() as u32,
        ..Default::default()
    };
    for ns in &namespaces {
        let name = ns.name_any();
        if matches!(
            ns.labels()
                .get("pod-security.kubernetes.io/enforce")
                .map(String::as_str),
            Some("baseline" | "restricted")
        ) {
            report.pod_security_enforced += 1;
        }
        if pods
            .iter()
            .filter(|p| p.namespace().as_deref() == Some(&name))
            .all(restricted)
        {
            report.pod_security_restricted += 1;
        }
        if policies.iter().any(|p| {
            p.namespace().as_deref() == Some(&name) && p.name_any() == pyo_proto::ISOLATION_POLICY
        }) {
            report.network_isolated += 1;
        }
    }
    report
}

/// The restricted Pod Security Standard's main asks: not root, no privilege
/// escalation, every capability dropped, a seccomp profile.
fn restricted(pod: &Pod) -> bool {
    let Some(spec) = &pod.spec else { return true };
    let pod_sc = spec.security_context.as_ref();
    let pod_non_root = pod_sc.and_then(|s| s.run_as_non_root).unwrap_or(false);
    let pod_seccomp = pod_sc.and_then(|s| s.seccomp_profile.as_ref()).is_some();
    spec.containers.iter().all(|c| {
        let sc = c.security_context.as_ref();
        sc.and_then(|s| s.run_as_non_root).unwrap_or(pod_non_root)
            && sc.and_then(|s| s.allow_privilege_escalation) == Some(false)
            && sc
                .and_then(|s| s.capabilities.as_ref())
                .and_then(|c| c.drop.as_ref())
                .is_some_and(|d| d.iter().any(|x| x == "ALL"))
            && (pod_seccomp || sc.and_then(|s| s.seccomp_profile.as_ref()).is_some())
    })
}

/// Keep each node's standard topology labels on the location Pyo has for
/// it (only patched when they differ). They let services be pinned to a place.
pub async fn label_locations(client: &Client, wanted: &[pyo_proto::ServerLocation]) {
    use pyo_proto::{REGION_LABEL, ZONE_LABEL};
    let nodes: Api<Node> = Api::all(client.clone());
    for w in wanted {
        let Ok(n) = nodes.get(&w.server).await else {
            continue;
        };
        let labels = n.labels();
        if labels.get(REGION_LABEL) == Some(&w.region) && labels.get(ZONE_LABEL) == w.zone.as_ref()
        {
            continue;
        }
        let patch = serde_json::json!({ "metadata": { "labels": { REGION_LABEL: w.region, ZONE_LABEL: w.zone } } });
        match nodes
            .patch(
                &w.server,
                &kube::api::PatchParams::default(),
                &kube::api::Patch::Merge(&patch),
            )
            .await
        {
            Ok(_) => {
                tracing::info!(node = %w.server, region = %w.region, zone = ?w.zone, "labelled node location")
            }
            Err(e) => tracing::warn!(node = %w.server, error = %e, "couldn't label node location"),
        }
    }
}

/// Keep each server's `pyo/tag-<tag>` labels on the tags Pyo has
/// for it, removing ones it no longer has (only patched when they differ).
pub async fn label_tags(client: &Client, wanted: &[pyo_proto::ServerTags]) {
    use pyo_proto::TAG_LABEL_PREFIX;
    let nodes: Api<Node> = Api::all(client.clone());
    for w in wanted {
        let Ok(n) = nodes.get(&w.server).await else {
            continue;
        };
        let want: std::collections::BTreeSet<String> = w
            .tags
            .iter()
            .filter_map(|t| pyo_proto::valid_tag(t))
            .map(|t| format!("{TAG_LABEL_PREFIX}{t}"))
            .collect();
        let have: std::collections::BTreeSet<String> = n
            .labels()
            .keys()
            .filter(|k| k.starts_with(TAG_LABEL_PREFIX))
            .cloned()
            .collect();
        if want == have {
            continue;
        }
        let mut labels = serde_json::Map::new();
        for k in have.difference(&want) {
            labels.insert(k.clone(), serde_json::Value::Null);
        }
        for k in &want {
            labels.insert(k.clone(), serde_json::json!("true"));
        }
        let patch = serde_json::json!({ "metadata": { "labels": labels } });
        match nodes
            .patch(
                &w.server,
                &kube::api::PatchParams::default(),
                &kube::api::Patch::Merge(&patch),
            )
            .await
        {
            Ok(_) => tracing::info!(node = %w.server, tags = ?w.tags, "labelled node tags"),
            Err(e) => tracing::warn!(node = %w.server, error = %e, "couldn't label node tags"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A ServiceLB pod as k3s makes it for `svc` in `ns`, aimed at `server`:
    /// pending without free ports, or running there.
    fn svclb(ns: &str, svc: &str, server: &str, ports: &[i32], pending: bool) -> Pod {
        let containers: Vec<_> = ports
            .iter()
            .map(|p| serde_json::json!({ "name": format!("lb-tcp-{p}"), "ports": [{ "containerPort": p, "hostPort": p, "protocol": "TCP" }] }))
            .collect();
        let mut pod = serde_json::json!({
            "metadata": { "name": format!("svclb-{svc}-1a2b3c4d-x{server}"), "namespace": "kube-system",
                "labels": { SVCLB_NS_LABEL: ns, SVCLB_NAME_LABEL: svc } },
            "spec": { "containers": containers, "affinity": { "nodeAffinity": { "requiredDuringSchedulingIgnoredDuringExecution": {
                "nodeSelectorTerms": [{ "matchFields": [{ "key": "metadata.name", "operator": "In", "values": [server] }] }] } } } },
            "status": { "phase": "Running" }
        });
        if pending {
            pod["status"] = serde_json::json!({ "phase": "Pending", "conditions": [{ "type": "PodScheduled", "status": "False", "reason": "Unschedulable",
                "message": "0/2 nodes are available: 1 node(s) didn't have free ports for the requested pod ports, 1 node(s) didn't match Pod's node affinity/selector." }] });
        } else {
            pod["spec"]["nodeName"] = server.into();
        }
        serde_json::from_value(pod).unwrap()
    }

    #[test]
    fn the_gateway_sees_where_its_ports_are_taken() {
        let gw = |server: &str, pending: bool| svclb("pyo-gateway", "envoy-pyo-695a31a2", server, &[80, 443], pending);
        let traefik = svclb("kube-system", "traefik", "misc-ten-sin4", &[80, 443], false);
        // As on misc-ten-sin4: k3s's Traefik has 80 and 443 there.
        let live = [traefik.clone(), gw("other", false)];
        assert_eq!(
            ports_taken(&[gw("misc-ten-sin4", true)], &live),
            vec![pyo_proto::GatewayPortsTaken { server: "misc-ten-sin4".into(), ports: vec![80, 443], by: "Traefik".into() }]
        );
        assert_eq!(ports_taken(&[gw("misc-ten-sin4", true)], &live)[0].text(), "Ports 80 and 443 are in use on misc-ten-sin4");
        // ingress-nginx on host ports, 443 only.
        let nginx: Pod = serde_json::from_value(serde_json::json!({
            "metadata": { "name": "ingress-nginx-controller-5c9", "namespace": "ingress-nginx" },
            "spec": { "nodeName": "web-2", "containers": [{ "name": "c", "ports": [{ "containerPort": 443, "hostPort": 443 }, { "containerPort": 8443 }] }] }
        }))
        .unwrap();
        assert_eq!(
            ports_taken(&[gw("web-2", true)], &[nginx.clone()]),
            vec![pyo_proto::GatewayPortsTaken { server: "web-2".into(), ports: vec![443], by: "ingress-nginx".into() }]
        );
        // Another server's holder doesn't count; nothing found: every port it wants.
        assert_eq!(
            ports_taken(&[gw("web-3", true)], &[traefik.clone(), nginx]),
            vec![pyo_proto::GatewayPortsTaken { server: "web-3".into(), ports: vec![80, 443], by: String::new() }]
        );
        // Pending for another reason (a server not ready), someone else's
        // pending pod, or nothing pending: nothing to say.
        let mut other_reason = gw("misc-ten-sin4", true);
        other_reason.status.as_mut().unwrap().conditions.as_mut().unwrap()[0].message =
            Some("0/1 nodes are available: 1 node(s) had untolerated taint {node.kubernetes.io/not-ready: }.".into());
        assert!(ports_taken(&[other_reason], &live).is_empty());
        assert!(ports_taken(&[svclb("shop", "lb", "misc-ten-sin4", &[80], true)], &live).is_empty());
        assert!(ports_taken(&[], &live).is_empty());
        // Two of the gateway's Services on one server: once, ports merged.
        let tcp = svclb("pyo-gateway", "envoy-pyo-tcp", "misc-ten-sin4", &[5432], true);
        let pg: Pod = serde_json::from_value(serde_json::json!({
            "metadata": { "name": "postgres-0", "namespace": "db" },
            "spec": { "nodeName": "misc-ten-sin4", "hostNetwork": true, "containers": [{ "name": "pg", "ports": [{ "containerPort": 5432, "hostPort": 5432 }] }] }
        }))
        .unwrap();
        let both = ports_taken(&[gw("misc-ten-sin4", true), tcp], &[traefik, pg]);
        assert_eq!(both.len(), 1);
        assert_eq!(both[0].ports, vec![80, 443, 5432]);
        assert_eq!(both[0].by, "Traefik");
    }

    #[test]
    fn a_refused_waf_policy_is_seen() {
        // As Envoy Gateway reported the policies that named the WAF's Service.
        let refused_one = serde_json::json!({ "ancestors": [{ "conditions": [{ "type": "Accepted", "status": "False", "reason": "Invalid", "message": "ExtProc: service pyo-waf/pyo-waf not found." }] }] });
        assert!(refused(&refused_one));
        let accepted = serde_json::json!({ "ancestors": [{ "conditions": [{ "type": "Accepted", "status": "True" }] }] });
        assert!(!refused(&accepted));
        assert!(!refused(&serde_json::Value::Null), "no status yet");
    }

    #[test]
    fn quantities() {
        assert_eq!(cpu_cores("250m"), 0.25);
        assert_eq!(cpu_cores("2"), 2.0);
        assert!((cpu_cores("1500000n") - 0.0015).abs() < 1e-12);
        assert_eq!(bytes("512Mi"), 512.0 * MIB);
        assert_eq!(bytes("1G"), 1e9);
        assert_eq!(bytes("1024"), 1024.0);
    }

    #[test]
    fn requests_are_counted_per_server_as_the_scheduler_does() {
        let pod = |node: Option<&str>, phase: &str, cpu: &str, mem: &str| -> Pod {
            serde_json::from_value(serde_json::json!({
                "metadata": { "name": "p" },
                "spec": {
                    "nodeName": node,
                    "initContainers": [{ "name": "init", "resources": { "requests": { "cpu": "2", "memory": "64Mi" } } }],
                    "containers": [
                        { "name": "a", "resources": { "requests": { "cpu": cpu, "memory": mem } } },
                        { "name": "b" },
                    ],
                },
                "status": { "phase": phase },
            }))
            .unwrap()
        };
        let pods = [
            pod(Some("s1"), "Running", "500m", "256Mi"),
            pod(Some("s1"), "Pending", "250m", "128Mi"),
            pod(Some("s1"), "Succeeded", "4", "1Gi"),
            pod(None, "Pending", "1", "1Gi"),
        ];
        let r = requested_per_node(&pods);
        assert_eq!(r.len(), 1, "an unplaced pod holds nothing anywhere");
        let (cpu, mem) = r["s1"];
        // The init container's 2 CPU wins over 0.5 and 0.25 each; memory
        // is the containers' 256 + 128 MiB.
        assert_eq!(cpu, 4.0);
        assert_eq!(mem, 384.0 * MIB);
    }

    #[test]
    fn a_pod_without_room_is_waiting() {
        let pod = |message: &str| -> Pod {
            serde_json::from_value(serde_json::json!({
                "metadata": { "name": "p" },
                "spec": { "containers": [] },
                "status": { "phase": "Pending", "conditions": [{ "type": "PodScheduled", "status": "False", "reason": "Unschedulable", "message": message }] },
            }))
            .unwrap()
        };
        assert!(waiting_for_room(&pod(
            "0/2 nodes are available: 2 Insufficient cpu."
        )));
        // Can't run anywhere it may: not a matter of room.
        assert!(!waiting_for_room(&pod(
            "0/2 nodes are available: 2 node(s) didn't match Pod's node affinity/selector."
        )));
    }
}
