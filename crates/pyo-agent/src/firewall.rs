//! The firewall where the gateway can't apply it: UDP ports through
//! the proxy (a network policy on the proxy's own pods, which see the
//! visitor's address), and, in rollout.rs, ports that bypass it (a network
//! policy on the service's pods). Websites and TCP ports through the proxy
//! get theirs from gateway.rs, in each route's SecurityPolicy.

use std::collections::BTreeSet;

use k8s_openapi::api::{core::v1::Service, networking::v1::NetworkPolicy};
use kube::{
    Api, Client, ResourceExt,
    api::{DeleteParams, ListParams, Patch, PatchParams},
};
use pyo_proto::{FirewallSync, system::GATEWAY_NS};
use serde_json::{Value, json};

use crate::{
    MANAGER,
    gateway::{self, FIREWALL_ANNOTATION, LISTEN_ANNOTATION},
};

/// Address ranges a port lets in: (range, except).
type Blocks = Vec<(String, Vec<String>)>;

/// The network policy on the proxy's pods.
pub const PROXY_POLICY: &str = pyo_proto::system::FIREWALL_POLICY;
use pyo_proto::system::proxy_pods;

/// `(range, except)` pairs as a network policy's `from`.
pub fn from_blocks(blocks: &[(String, Vec<String>)]) -> Vec<Value> {
    blocks
        .iter()
        .map(|(cidr, except)| {
            if except.is_empty() {
                json!({ "ipBlock": { "cidr": cidr } })
            } else {
                json!({ "ipBlock": { "cidr": cidr, "except": except } })
            }
        })
        .collect()
}

pub fn anywhere() -> Vec<Value> {
    vec![
        json!({ "ipBlock": { "cidr": "0.0.0.0/0" } }),
        json!({ "ipBlock": { "cidr": "::/0" } }),
    ]
}

/// Rebuild the proxy pods' policy from every UDP route's firewall: those
/// ports only from their ranges, everything else the proxy listens on from
/// anywhere, and the cluster's own pods to any port. No restricted UDP
/// port: no policy (everything gets in).
pub async fn proxy_policy(client: &Client) -> anyhow::Result<()> {
    let api: Api<NetworkPolicy> = Api::namespaced(client.clone(), GATEWAY_NS);
    let routes = Api::<kube::api::DynamicObject>::namespaced_with(
        client.clone(),
        GATEWAY_NS,
        &gateway::udp_routes(),
    )
    .list(&ListParams::default())
    .await?;
    let mut restricted: Vec<(i32, Blocks)> = vec![];
    for r in routes.items {
        let a = r.annotations();
        let (Some(fw), Some(port)) = (
            a.get(FIREWALL_ANNOTATION).filter(|f| !f.is_empty()),
            a.get(LISTEN_ANNOTATION).and_then(|p| p.parse::<i32>().ok()),
        ) else {
            continue;
        };
        let blocks: Blocks = serde_json::from_str(fw).unwrap_or_default();
        restricted.push((port, blocks));
    }
    if restricted.is_empty() {
        return match api.delete(PROXY_POLICY, &DeleteParams::default()).await {
            Err(kube::Error::Api(e)) if e.code != 404 => Err(kube::Error::Api(e).into()),
            Err(e) if !matches!(e, kube::Error::Api(_)) => Err(e.into()),
            _ => Ok(()),
        };
    }
    // Everything else the proxy listens on: its Service's ports, and the
    // websites' always.
    let mut open: BTreeSet<(String, i32)> =
        [("TCP".to_owned(), 10080), ("TCP".to_owned(), 10443)].into();
    let services =
        Api::<Service>::namespaced(client.clone(), GATEWAY_NS)
            .list(&ListParams::default().labels(
                "app.kubernetes.io/component=proxy,app.kubernetes.io/managed-by=envoy-gateway",
            ))
            .await?;
    for p in services
        .items
        .iter()
        .filter_map(|s| s.spec.as_ref())
        .flat_map(|s| s.ports.iter().flatten())
    {
        let target = match &p.target_port {
            Some(k8s_openapi::apimachinery::pkg::util::intstr::IntOrString::Int(n)) => *n,
            _ => p.port,
        };
        open.insert((p.protocol.clone().unwrap_or_else(|| "TCP".into()), target));
    }
    for (port, _) in &restricted {
        open.remove(&("UDP".to_owned(), *port));
    }
    let mut ingress: Vec<Value> = restricted
        .iter()
        .filter(|(_, blocks)| !blocks.is_empty())
        .map(|(port, blocks)| json!({ "from": from_blocks(blocks), "ports": [{ "protocol": "UDP", "port": port }] }))
        .collect();
    ingress.push(json!({ "from": anywhere(), "ports": open.iter().map(|(proto, port)| json!({ "protocol": proto, "port": port })).collect::<Vec<_>>() }));
    ingress.push(json!({ "from": [{ "namespaceSelector": {} }] }));
    let obj = json!({
        "apiVersion": "networking.k8s.io/v1", "kind": "NetworkPolicy",
        "metadata": { "name": PROXY_POLICY, "namespace": GATEWAY_NS, "labels": { "app.kubernetes.io/managed-by": MANAGER } },
        "spec": { "podSelector": proxy_pods(), "policyTypes": ["Ingress"], "ingress": ingress },
    });
    api.patch(
        PROXY_POLICY,
        &PatchParams::apply(MANAGER).force(),
        &Patch::Apply(&obj),
    )
    .await?;
    Ok(())
}

/// Apply the firewall to every running service, in place: each one's routes
/// (gateway.rs), its public Service's ranges and network policies, then the
/// proxy pods' policy.
pub async fn sync(agent: &crate::Agent, sync: FirewallSync) -> anyhow::Result<String> {
    let client = &agent.kube;
    let mut n = 0;
    let installed = gateway::installed(client).await;
    // The WAF's engine (the API installs it while a service here has its
    // WAF on): its processor before routes point at it.
    let waf_ready = installed && sync.waf && crate::waf::wait_ready(client).await;
    if installed && sync.waf && !waf_ready {
        tracing::warn!(
            "the WAF's processor isn't ready yet; until it is, block-mode services' routes are refused and the rest pass uninspected"
        );
    }
    let mut skipped = 0;
    // One service that fails doesn't keep the others from theirs.
    let mut failed: Vec<String> = vec![];
    for t in &sync.targets {
        match apply_target(agent, t, sync.waf).await {
            Ok(true) => n += 1,
            Ok(false) => skipped += 1,
            Err(e) => {
                tracing::warn!(service = %t.service.name, error = %format!("{e:#}"), "couldn't apply the firewall to a service");
                failed.push(format!("{}: {e:#}", t.service.name));
            }
        }
    }
    // No service wants it: the routes no longer point at it (above); the
    // API removes it.
    if installed && !sync.waf {
        crate::waf::remove_policies(client).await?;
    }
    proxy_policy(client).await?;
    // And whatever a service left behind.
    if let Err(e) = gateway::collect(agent).await {
        tracing::warn!(error = %format!("{e:#}"), "couldn't collect the gateway's leftover objects");
    }
    let done = match skipped {
        0 => format!("Firewall applied to {n} service(s)"),
        _ => format!("Firewall applied to {n} service(s); {skipped} no longer here"),
    };
    if !failed.is_empty() {
        anyhow::bail!("{done}. It couldn't be applied to {}", failed.join(". "));
    }
    Ok(done)
}

/// Apply the firewall to one running service: its routes, its public
/// Service's ranges and its network policies. False when its workload is
/// gone (a delete that ran first: commands on a service take turns, main.rs
/// `ordered_keys`), and its routes stay gone.
async fn apply_target(
    agent: &crate::Agent,
    t: &pyo_proto::FirewallTarget,
    waf: bool,
) -> anyhow::Result<bool> {
    let client = &agent.kube;
    let spec = gateway::target_spec(t, waf)?;
    let Some((notes, _)) = gateway::apply_routes(agent, &spec, true).await? else {
        return Ok(false);
    };
    for note in notes {
        tracing::warn!(service = %t.service.name, "{note}");
    }
    // Ports that bypass the gateway: their Service carries the ranges the
    // network policy lets in (rollout.rs's set_isolation writes it).
    let services: Api<Service> = Api::namespaced(client.clone(), &t.service.namespace);
    let public = crate::rollout::public_service_name(&t.service.name);
    if services.get_opt(&public).await?.is_some() {
        let patch = json!({ "metadata": { "annotations": { FIREWALL_ANNOTATION: gateway::ip_blocks_json(&gateway::fw_layers(&spec, None)) } } });
        services
            .patch(&public, &PatchParams::default(), &Patch::Merge(&patch))
            .await?;
    }
    crate::rollout::set_isolation(client, &t.service.namespace, t.network_isolation).await?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A firewall sync goes on past a service whose routes fail: the
    /// services after it still get theirs, and the sync says which failed.
    #[tokio::test]
    async fn one_failing_service_doesnt_stop_the_others() {
        let (kube, client) = crate::fakekube::FakeKube::start().await;
        let target = |name: &str| -> pyo_proto::FirewallTarget {
            serde_json::from_value(json!({
                "service": { "serviceId": format!("svc_{name}"), "name": name, "namespace": format!("ns-{name}"), "workloadKind": "Deployment" },
                "projectId": "prj_1", "routesRevision": 1, "firewall": {},
                "ports": [{ "name": "http", "containerPort": 8080, "protocol": "HTTP", "exposure": "http", "publicPort": null }],
                "domains": [{ "hostname": format!("{name}.example.com"), "portName": "http" }],
            }))
            .unwrap()
        };
        let mut targets = vec![];
        for name in ["a", "b", "c"] {
            kube.put(
                &format!("apis/apps/v1/namespaces/ns-{name}/deployments/{name}"),
                json!({ "apiVersion": "apps/v1", "kind": "Deployment", "metadata": { "labels": { crate::observe::SERVICE_LABEL: format!("svc_{name}") } } }),
            );
            targets.push(target(name));
        }
        // The first service's routes can't be written.
        kube.admit(|w| w.body.to_string().contains("svc_a").then(|| (500, "etcd timeout".into())));
        let agent = crate::Agent {
            kube: client,
            api: crate::api::PyoApi::new("http://127.0.0.1:9", "t").unwrap(),
            cancels: Default::default(),
            deploying: Default::default(),
            routes: Default::default(),
            policy: std::sync::Arc::new(std::sync::RwLock::new(Default::default())),
        };
        let e = sync(&agent, FirewallSync { targets, waf: false })
            .await
            .expect_err("the first service failed");
        let e = format!("{e:#}");
        assert!(e.starts_with("Firewall applied to 2 service(s)."), "{e}");
        assert!(e.contains("a: ") && e.contains("etcd timeout"), "{e}");
        assert!(!e.contains("b: ") && !e.contains("c: "), "{e}");
        let route = |name: &str| {
            kube.get(&format!(
                "apis/gateway.networking.k8s.io/v1/namespaces/{GATEWAY_NS}/httproutes/ns-{name}-{name}-http-http"
            ))
        };
        assert!(route("a").is_none());
        assert!(route("b").is_some(), "the service after it");
        assert!(route("c").is_some(), "and the last one");
    }
}
