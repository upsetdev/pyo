//! Why a deploy didn't fit, in figures: when the scheduler finds too little
//! free memory or CPU for a new pod, what it needs, what's free on the
//! server with the most room it may run on, and what takes the room there
//! (the biggest requests). And when a new container ran out of memory, at
//! what limit.

use std::collections::BTreeMap;

use k8s_openapi::api::core::v1::{Node, Pod, Toleration};
use kube::{Api, Client, ResourceExt, api::ListParams};

use crate::observe::{bytes, cpu_cores, pod_requests};

const MIB: f64 = 1048576.0;

/// "128 MiB", "1.5 GiB".
pub fn memory(b: f64) -> String {
    let mib = (b / MIB).round() as u64;
    if mib >= 1024 {
        let gib = format!("{:.1}", mib as f64 / 1024.0);
        format!("{} GiB", gib.trim_end_matches(".0"))
    } else {
        format!("{mib} MiB")
    }
}

/// "0.25 CPU", "2 CPU".
fn cpu(c: f64) -> String {
    let s = format!("{:.2}", c);
    format!("{} CPU", s.trim_end_matches('0').trim_end_matches('.'))
}

/// A server a pod may run on: its allocatable (cores, bytes) and what the
/// pods on it ask for, by what they're called.
#[derive(Debug, Clone, Default)]
pub struct Server {
    pub name: String,
    pub cpu: f64,
    pub memory: f64,
    /// (name, cores, bytes), one per pod.
    pub pods: Vec<(String, f64, f64)>,
}

impl Server {
    fn free(&self, mem: bool) -> f64 {
        let used: f64 = self
            .pods
            .iter()
            .map(|(_, c, m)| if mem { *m } else { *c })
            .sum();
        (if mem { self.memory } else { self.cpu } - used).max(0.0)
    }
}

/// The message for a pod that needs (cores, bytes) where `servers` are the
/// ones it may run on; `memory_short` and `cpu_short` as the scheduler said.
/// None when there's no server to speak of.
pub fn explain(
    need: (f64, f64),
    memory_short: bool,
    cpu_short: bool,
    servers: &[Server],
) -> Option<String> {
    if servers.is_empty() || !(memory_short || cpu_short) {
        return None;
    }
    let mut out = vec![];
    for (mem, short) in [(true, memory_short), (false, cpu_short)] {
        if !short {
            continue;
        }
        let fmt = |v: f64| if mem { memory(v) } else { cpu(v) };
        let best = servers
            .iter()
            .max_by(|a, b| a.free(mem).total_cmp(&b.free(mem)))?;
        let what = if mem { "of memory" } else { "" };
        let needs = if mem { need.1 } else { need.0 };
        let mut s = format!(
            "Needs {}{}{}, {} is free",
            fmt(needs),
            if what.is_empty() { "" } else { " " },
            what,
            fmt(best.free(mem))
        );
        if servers.len() > 1 {
            s.push_str(&format!(" on the server with the most ({})", best.name));
        }
        s.push('.');
        // What takes the room there, by name (replicas together).
        let mut by_name: BTreeMap<&str, f64> = BTreeMap::new();
        for (name, c, m) in &best.pods {
            *by_name.entry(name).or_default() += if mem { *m } else { *c };
        }
        let mut biggest: Vec<(&str, f64)> = by_name.into_iter().filter(|(_, v)| *v > 0.0).collect();
        biggest.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(b.0)));
        biggest.truncate(3);
        let list: Vec<String> = biggest
            .iter()
            .map(|(n, v)| format!("{n} ({})", fmt(*v)))
            .collect();
        let kind = if mem { "" } else { "CPU " };
        match list.len() {
            0 => {}
            1 => s.push_str(&format!(" The biggest {kind}request is {}.", list[0])),
            n => s.push_str(&format!(
                " The biggest {kind}requests are {} and {}.",
                list[..n - 1].join(", "),
                list[n - 1]
            )),
        }
        out.push(s);
    }
    (!out.is_empty()).then(|| out.join(" "))
}

/// What a pod is called in a message: its database, service or app, else
/// its workload's name.
pub fn display_name(p: &Pod) -> String {
    let l = p.labels();
    for k in [
        "pyo.dev/database",
        "app.kubernetes.io/name",
        "cnpg.io/cluster",
        "app.kubernetes.io/instance",
        "k8s-app",
        "app",
    ] {
        if let Some(v) = l.get(k).filter(|v| !v.is_empty()) {
            return v.clone();
        }
    }
    if let Some(o) = p.owner_references().first() {
        if o.kind == "ReplicaSet"
            && let Some((base, _)) = o.name.rsplit_once('-')
        {
            return base.to_owned();
        }
        return o.name.clone();
    }
    p.name_any()
}

fn tolerates(tolerations: &[Toleration], key: &str, value: &str, effect: &str) -> bool {
    tolerations.iter().any(|t| {
        let effect_ok = t
            .effect
            .as_deref()
            .is_none_or(|e| e.is_empty() || e == effect);
        let key_ok = t.key.as_deref().is_none_or(|k| k.is_empty() || k == key);
        let op = t.operator.as_deref().unwrap_or("Equal");
        let value_ok = op == "Exists" || t.value.as_deref().unwrap_or("") == value;
        effect_ok && key_ok && value_ok && (t.key.is_some() || op == "Exists")
    })
}

/// Whether the pod may run on the node at all: it's ready and takes work,
/// its taints are tolerated, and its labels match the pod's selector and
/// required node affinity.
pub fn may_run_on(p: &Pod, n: &Node) -> bool {
    let Some(spec) = p.spec.as_ref() else {
        return false;
    };
    if n.spec
        .as_ref()
        .and_then(|s| s.unschedulable)
        .unwrap_or(false)
    {
        return false;
    }
    let ready = n
        .status
        .as_ref()
        .and_then(|s| s.conditions.as_ref())
        .is_some_and(|cs| cs.iter().any(|c| c.type_ == "Ready" && c.status == "True"));
    if !ready {
        return false;
    }
    let tolerations = spec.tolerations.clone().unwrap_or_default();
    for t in n
        .spec
        .as_ref()
        .and_then(|s| s.taints.as_ref())
        .into_iter()
        .flatten()
    {
        if (t.effect == "NoSchedule" || t.effect == "NoExecute")
            && !tolerates(
                &tolerations,
                &t.key,
                t.value.as_deref().unwrap_or(""),
                &t.effect,
            )
        {
            return false;
        }
    }
    let labels = n.labels();
    if let Some(sel) = &spec.node_selector
        && sel.iter().any(|(k, v)| labels.get(k) != Some(v))
    {
        return false;
    }
    let terms = spec
        .affinity
        .as_ref()
        .and_then(|a| a.node_affinity.as_ref())
        .and_then(|a| {
            a.required_during_scheduling_ignored_during_execution
                .as_ref()
        })
        .map(|r| r.node_selector_terms.clone())
        .unwrap_or_default();
    if terms.is_empty() {
        return true;
    }
    // Any term, every expression in it.
    terms.iter().any(|term| {
        term.match_expressions.iter().flatten().all(|e| {
            let v = labels.get(&e.key);
            let values = e.values.clone().unwrap_or_default();
            match e.operator.as_str() {
                "In" => v.is_some_and(|v| values.contains(v)),
                "NotIn" => !v.is_some_and(|v| values.contains(v)),
                "Exists" => v.is_some(),
                "DoesNotExist" => v.is_none(),
                _ => true,
            }
        })
    })
}

/// The servers `pod` may run on, with what the pods there ask for.
pub fn servers_for(pod: &Pod, nodes: &[Node], pods: &[Pod]) -> Vec<Server> {
    let mut out: Vec<Server> = nodes
        .iter()
        .filter(|n| may_run_on(pod, n))
        .map(|n| {
            let a = n
                .status
                .as_ref()
                .and_then(|s| s.allocatable.clone())
                .unwrap_or_default();
            Server {
                name: n.name_any(),
                cpu: a.get("cpu").map(|q| cpu_cores(&q.0)).unwrap_or(0.0),
                memory: a.get("memory").map(|q| bytes(&q.0)).unwrap_or(0.0),
                pods: vec![],
            }
        })
        .collect();
    for p in pods {
        let phase = p
            .status
            .as_ref()
            .and_then(|s| s.phase.as_deref())
            .unwrap_or("");
        if phase == "Succeeded" || phase == "Failed" {
            continue;
        }
        let Some(node) = p.spec.as_ref().and_then(|s| s.node_name.as_deref()) else {
            continue;
        };
        if let Some(s) = out.iter_mut().find(|s| s.name == node) {
            let (c, m) = pod_requests(p);
            s.pods.push((display_name(p), c, m));
        }
    }
    out
}

/// For a pod the scheduler found no room for (its `message`), what it
/// needs, what's free and what takes the room. None when it isn't short of
/// memory or CPU, or the cluster can't be read.
pub async fn no_room(client: &Client, pod: &Pod, message: &str) -> Option<String> {
    let memory_short = message.contains("Insufficient memory");
    let cpu_short = message.contains("Insufficient cpu");
    if !memory_short && !cpu_short {
        return None;
    }
    let nodes = Api::<Node>::all(client.clone())
        .list(&ListParams::default())
        .await
        .ok()?
        .items;
    let pods = Api::<Pod>::all(client.clone())
        .list(&ListParams::default())
        .await
        .ok()?
        .items;
    explain(
        pod_requests(pod),
        memory_short,
        cpu_short,
        &servers_for(pod, &nodes, &pods),
    )
}

/// A container of the pod that was stopped for running out of memory: its
/// message, at its limit.
pub fn out_of_memory(p: &Pod) -> Option<String> {
    let st = p.status.as_ref()?;
    let cs = st.container_statuses.iter().flatten().find(|cs| {
        [cs.state.as_ref(), cs.last_state.as_ref()]
            .into_iter()
            .flatten()
            .any(|s| {
                s.terminated
                    .as_ref()
                    .is_some_and(|t| t.reason.as_deref() == Some("OOMKilled"))
            })
    })?;
    let limit = p
        .spec
        .as_ref()?
        .containers
        .iter()
        .find(|c| c.name == cs.name)
        .and_then(|c| {
            c.resources
                .as_ref()?
                .limits
                .as_ref()?
                .get("memory")
                .map(|q| bytes(&q.0))
        });
    Some(match limit {
        Some(l) if l > 0.0 => format!(
            "It ran out of memory at its {} limit. Give it more memory in its resources, or make it use less.",
            memory(l)
        ),
        _ => "It ran out of memory. Give it more memory in its resources, or make it use less."
            .into(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn mi(n: f64) -> f64 {
        n * MIB
    }

    fn one_server() -> Server {
        Server {
            name: "vps".into(),
            cpu: 2.0,
            memory: mi(1460.0),
            pods: vec![
                ("umami-db".into(), 0.25, mi(512.0)),
                ("pyo-clickhouse".into(), 0.05, mi(144.0)),
                ("web".into(), 0.1, mi(128.0)),
                ("web".into(), 0.1, mi(128.0)),
                ("pyo".into(), 0.02, mi(64.0)),
                ("rest".into(), 0.5, mi(364.0)),
            ],
        }
    }

    #[test]
    fn short_of_memory_names_the_need_the_room_and_the_biggest_requests() {
        assert_eq!(
            explain((0.1, mi(256.0)), true, false, &[one_server()]).unwrap(),
            "Needs 256 MiB of memory, 120 MiB is free. The biggest requests are umami-db (512 MiB), rest (364 MiB) and web (256 MiB)."
        );
    }

    #[test]
    fn short_of_cpu_too() {
        let s = explain((2.0, mi(256.0)), true, true, &[one_server()]).unwrap();
        assert!(s.ends_with("Needs 2 CPU, 0.98 CPU is free. The biggest CPU requests are rest (0.5 CPU), umami-db (0.25 CPU) and web (0.2 CPU)."), "{s}");
    }

    #[test]
    fn with_several_servers_the_one_with_the_most_room() {
        let mut other = one_server();
        other.name = "big".into();
        other.memory = mi(4096.0);
        other.pods = vec![("cache".into(), 0.1, mi(4000.0))];
        assert_eq!(
            explain((0.1, mi(1024.0)), true, false, &[one_server(), other]).unwrap(),
            "Needs 1 GiB of memory, 120 MiB is free on the server with the most (vps). The biggest requests are umami-db (512 MiB), rest (364 MiB) and web (256 MiB)."
        );
    }

    #[test]
    fn nothing_to_say_without_a_server_or_a_shortage() {
        assert_eq!(explain((0.1, mi(1.0)), true, false, &[]), None);
        assert_eq!(explain((0.1, mi(1.0)), false, false, &[one_server()]), None);
        let empty = Server {
            name: "s".into(),
            cpu: 1.0,
            memory: mi(100.0),
            pods: vec![],
        };
        assert_eq!(
            explain((0.1, mi(200.0)), true, false, &[empty]).unwrap(),
            "Needs 200 MiB of memory, 100 MiB is free."
        );
    }

    fn node(name: &str, labels: serde_json::Value, taints: serde_json::Value, ready: bool) -> Node {
        serde_json::from_value(json!({
            "metadata": { "name": name, "labels": labels },
            "spec": { "taints": taints },
            "status": {
                "allocatable": { "cpu": "2", "memory": "1460Mi" },
                "conditions": [{ "type": "Ready", "status": if ready { "True" } else { "False" } }],
            },
        }))
        .unwrap()
    }

    fn pod(spec: serde_json::Value, labels: serde_json::Value) -> Pod {
        serde_json::from_value(
            json!({ "metadata": { "name": "web-7d9f-abcde", "labels": labels }, "spec": spec }),
        )
        .unwrap()
    }

    #[test]
    fn only_servers_it_may_run_on_count() {
        let p = pod(
            json!({ "containers": [{ "name": "app", "resources": { "requests": { "memory": "128Mi" } } }], "nodeSelector": { "pyo/region": "eu" } }),
            json!({}),
        );
        let eu = node("eu1", json!({ "pyo/region": "eu" }), json!([]), true);
        let us = node("us1", json!({ "pyo/region": "us" }), json!([]), true);
        let down = node("eu2", json!({ "pyo/region": "eu" }), json!([]), false);
        let builder = node(
            "eu3",
            json!({ "pyo/region": "eu" }),
            json!([{ "key": "pyo/builder", "value": "true", "effect": "NoSchedule" }]),
            true,
        );
        let running = pod(
            json!({ "nodeName": "eu1", "containers": [{ "name": "db", "resources": { "requests": { "memory": "512Mi", "cpu": "250m" } } }] }),
            json!({ "pyo.dev/database": "umami-db" }),
        );
        let servers = servers_for(&p, &[eu, us, down, builder], &[running]);
        assert_eq!(
            servers.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(),
            ["eu1"]
        );
        assert_eq!(
            servers[0].pods,
            vec![("umami-db".to_string(), 0.25, mi(512.0))]
        );
        // A toleration lets it onto a tainted one.
        let tolerant = pod(
            json!({ "containers": [], "tolerations": [{ "key": "pyo/builder", "operator": "Exists", "effect": "NoSchedule" }] }),
            json!({}),
        );
        let builder = node(
            "b",
            json!({}),
            json!([{ "key": "pyo/builder", "value": "true", "effect": "NoSchedule" }]),
            true,
        );
        assert!(may_run_on(&tolerant, &builder));
    }

    #[test]
    fn pods_are_called_by_their_database_service_or_workload() {
        assert_eq!(
            display_name(&pod(
                json!({ "containers": [] }),
                json!({ "app.kubernetes.io/name": "web" })
            )),
            "web"
        );
        assert_eq!(
            display_name(&pod(
                json!({ "containers": [] }),
                json!({ "k8s-app": "kube-dns" })
            )),
            "kube-dns"
        );
        let owned: Pod = serde_json::from_value(json!({
            "metadata": { "name": "x-5d8-abcde", "ownerReferences": [{ "apiVersion": "apps/v1", "kind": "ReplicaSet", "name": "x-5d8", "uid": "u" }] },
        }))
        .unwrap();
        assert_eq!(display_name(&owned), "x");
    }

    #[test]
    fn running_out_of_memory_names_the_limit() {
        let p: Pod = serde_json::from_value(json!({
            "metadata": { "name": "site" },
            "spec": { "containers": [{ "name": "app", "resources": { "limits": { "memory": "64Mi" } } }] },
            "status": { "containerStatuses": [{
                "name": "app", "image": "i", "imageID": "", "ready": false, "restartCount": 2,
                "state": { "waiting": { "reason": "CrashLoopBackOff" } },
                "lastState": { "terminated": { "reason": "OOMKilled", "exitCode": 137 } },
            }] },
        }))
        .unwrap();
        assert_eq!(
            out_of_memory(&p).unwrap(),
            "It ran out of memory at its 64 MiB limit. Give it more memory in its resources, or make it use less."
        );
        let mut fine = p.clone();
        fine.status
            .as_mut()
            .unwrap()
            .container_statuses
            .as_mut()
            .unwrap()[0]
            .last_state = None;
        assert_eq!(out_of_memory(&fine), None);
        assert_eq!(memory(mi(1536.0)), "1.5 GiB");
        assert_eq!(memory(mi(2048.0)), "2 GiB");
    }

    /// Against a throwaway cluster (PYO_TEST_KUBECONFIG): a pod that asks
    /// for more memory than any server has gets the figures, and one that
    /// runs out of memory names its limit. Don't point it at one you care
    /// about.
    #[tokio::test]
    #[ignore]
    async fn on_a_cluster() {
        use k8s_openapi::api::core::v1::Namespace;
        use kube::api::{DeleteParams, Patch, PatchParams, PostParams};
        let path = std::env::var("PYO_TEST_KUBECONFIG").expect("PYO_TEST_KUBECONFIG");
        let kc = kube::config::Kubeconfig::read_from(&path).unwrap();
        let client = Client::try_from(
            kube::Config::from_custom_kubeconfig(kc, &Default::default())
                .await
                .unwrap(),
        )
        .unwrap();
        let ns = "pyo-room-test";
        let namespaces: Api<Namespace> = Api::all(client.clone());
        namespaces
            .patch(
                ns,
                &PatchParams::apply("test").force(),
                &Patch::Apply(
                    json!({ "apiVersion": "v1", "kind": "Namespace", "metadata": { "name": ns } }),
                ),
            )
            .await
            .unwrap();
        let pods: Api<Pod> = Api::namespaced(client.clone(), ns);
        let big: Pod = serde_json::from_value(json!({
            "metadata": { "name": "big", "labels": { "app.kubernetes.io/name": "big" } },
            "spec": { "containers": [{ "name": "app", "image": "busybox:1.37", "command": ["sleep", "3600"],
                "resources": { "requests": { "memory": "200Gi" } } }] },
        }))
        .unwrap();
        let oom: Pod = serde_json::from_value(json!({
            "metadata": { "name": "oom" },
            "spec": { "containers": [{ "name": "app", "image": "busybox:1.37", "command": ["sh", "-c", "tail /dev/zero"],
                "resources": { "requests": { "memory": "16Mi" }, "limits": { "memory": "16Mi" } } }] },
        }))
        .unwrap();
        pods.create(&PostParams::default(), &big).await.unwrap();
        pods.create(&PostParams::default(), &oom).await.unwrap();
        let mut room = None;
        let mut ran_out = None;
        for _ in 0..120 {
            let b = pods.get("big").await.unwrap();
            if room.is_none()
                && let Some(c) = b
                    .status
                    .as_ref()
                    .and_then(|s| s.conditions.as_ref())
                    .and_then(|cs| {
                        cs.iter()
                            .find(|c| c.type_ == "PodScheduled" && c.status == "False")
                    })
            {
                room = no_room(&client, &b, c.message.as_deref().unwrap_or("")).await;
            }
            if ran_out.is_none() {
                ran_out = out_of_memory(&pods.get("oom").await.unwrap());
            }
            if room.is_some() && ran_out.is_some() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        }
        let _ = namespaces.delete(ns, &DeleteParams::default()).await;
        let room = room.expect("the scheduler's figures");
        eprintln!("{room}");
        assert!(room.starts_with("Needs 200 GiB of memory, "), "{room}");
        assert!(room.contains(" is free"), "{room}");
        assert!(room.contains("The biggest request"), "{room}");
        let ran_out = ran_out.expect("an out-of-memory stop");
        eprintln!("{ran_out}");
        assert!(
            ran_out.starts_with("It ran out of memory at its 16 MiB limit."),
            "{ran_out}"
        );
    }
}
