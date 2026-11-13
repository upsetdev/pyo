//! The gateway: Envoy Gateway (pyo-proto's system.rs installs and pins
//! it). How a service's domains become its routes, all in `pyo-gateway`:
//!
//! - **Backends**: one per port its domains use, by the Service's cluster DNS
//!   name. The proxy reaches the service through its Service (as with
//!   `routingType: Service`), so "keep traffic in its region" works as
//!   before: the Service's trafficDistribution picks a replica close by.
//!   The controller only ever reads `pyo-gateway`, never a project's
//!   namespace, so it can't read the project's Secrets.
//! - **HTTPS**: a Gateway of the service's own (`<namespace>-<service>`),
//!   with a listener per domain (its hostname, its certificate). Gateways
//!   are merged onto one proxy fleet, so this costs nothing at runtime, and
//!   the 64-listener limit is per service, not per cluster.
//! - **Certificates**: cert-manager Certificates in `pyo-gateway`
//!   (`pyo-tls-<namespace>-<service>-<hash>`); uploaded ones are written there
//!   from the API.
//! - **HTTP**: routes on the shared Gateway's :80 listener: a redirect to
//!   HTTPS, or the service itself for plain HTTP. Let's Encrypt's challenge
//!   routes (cert-manager's) are more specific, so they still answer.
//!
//! A hostname can't be claimed by two services: listeners and routes are
//! per hostname, but they all live in one namespace, so the Gateway API
//! can't tell projects apart. The API refuses a domain another service has
//! (one per installation), and only the agent writes here.

use std::{
    collections::BTreeMap,
    sync::atomic::{AtomicU8, Ordering},
};

use k8s_openapi::api::core::v1::Secret;
use kube::{
    Api, Client, ResourceExt,
    api::{DeleteParams, DynamicObject, ListParams, Patch, PatchParams},
    core::{ApiResource, GroupVersionKind},
};
use pyo_proto::{
    DeploySpec, DomainSpec, PortSpec,
    firewall::Firewall,
    system::{self, GATEWAY_LABEL, GATEWAY_NS, HTTP_GATEWAY, TLS_SECRET_PREFIX},
};
use serde_json::{Value, json};

use crate::{MANAGER, api::PyoApi, observe::SERVICE_LABEL};

/// The certificate Secrets a service's HTTPS Gateway uses, so stale ones go.
const SECRETS_ANNOTATION: &str = "pyo.dev/tls-secrets";

pub fn resource(group: &str, version: &str, kind: &str, plural: &str) -> ApiResource {
    ApiResource::from_gvk_with_plural(&GroupVersionKind::gvk(group, version, kind), plural)
}

pub fn gateways() -> ApiResource {
    resource("gateway.networking.k8s.io", "v1", "Gateway", "gateways")
}
pub fn routes() -> ApiResource {
    resource("gateway.networking.k8s.io", "v1", "HTTPRoute", "httproutes")
}
pub fn backends() -> ApiResource {
    resource("gateway.envoyproxy.io", "v1alpha1", "Backend", "backends")
}
pub fn client_policies() -> ApiResource {
    resource(
        "gateway.envoyproxy.io",
        "v1alpha1",
        "ClientTrafficPolicy",
        "clienttrafficpolicies",
    )
}
pub fn tcp_routes() -> ApiResource {
    resource("gateway.networking.k8s.io", "v1", "TCPRoute", "tcproutes")
}
pub fn udp_routes() -> ApiResource {
    resource("gateway.networking.k8s.io", "v1", "UDPRoute", "udproutes")
}
pub fn security_policies() -> ApiResource {
    resource(
        "gateway.envoyproxy.io",
        "v1alpha1",
        "SecurityPolicy",
        "securitypolicies",
    )
}
pub fn backend_policies() -> ApiResource {
    resource(
        "gateway.envoyproxy.io",
        "v1alpha1",
        "BackendTrafficPolicy",
        "backendtrafficpolicies",
    )
}
pub fn extension_policies() -> ApiResource {
    resource(
        "gateway.envoyproxy.io",
        "v1alpha1",
        "EnvoyExtensionPolicy",
        "envoyextensionpolicies",
    )
}
fn certificates() -> ApiResource {
    resource("cert-manager.io", "v1", "Certificate", "certificates")
}

fn pp() -> PatchParams {
    PatchParams::apply(MANAGER).force()
}

/// FNV-1a of a hostname: stable names per domain.
pub fn host_hash(hostname: &str) -> String {
    let mut h: u32 = 2166136261;
    for b in hostname.bytes() {
        h = (h ^ b as u32).wrapping_mul(16777619);
    }
    format!("{h:08x}")
}

/// What a service's objects are named after.
pub fn base_name(spec: &DeploySpec) -> String {
    format!("{}-{}", spec.service.namespace, spec.service.name)
}

/// A domain's certificate Secret in `pyo-gateway`.
pub fn tls_secret(spec: &DeploySpec, hostname: &str) -> String {
    format!(
        "{TLS_SECRET_PREFIX}{}-{}",
        base_name(spec),
        host_hash(hostname)
    )
}

/// A service's routes, as objects to apply.
#[derive(Debug, Default)]
pub struct Routes {
    pub objects: Vec<Value>,
    /// Uploaded certificates to write: (Secret, certificate id).
    pub uploaded: Vec<(String, String)>,
    /// Every certificate Secret its HTTPS Gateway uses.
    pub secrets: Vec<String>,
}

/// The service port a domain's port is reached on.
fn service_port(spec: &DeploySpec, port_name: &str) -> Option<i32> {
    spec.ports.iter().find(|p| p.name == port_name).map(|p| {
        if p.exposure == "loadbalancer" {
            p.public_port.unwrap_or(p.container_port)
        } else {
            p.container_port
        }
    })
}

/// A service's Gateway for its public TCP and UDP ports.
pub fn ports_gateway(spec: &DeploySpec) -> String {
    format!("{}-ports", base_name(spec))
}

/// A proxied port's listener on that Gateway: `tcp-5432`, `udp-27015`.
pub fn listener_name(p: &PortSpec) -> String {
    format!("{}-{}", p.transport().to_lowercase(), p.public())
}

/// A proxied port's route (and its policies): `<namespace>-<service>-tcp-5432`.
/// The access log names it (`tcproute/pyo-gateway/<route>/rule/0`), which is
/// how analytics finds the service and port.
pub fn port_route_name(spec: &DeploySpec, p: &PortSpec) -> String {
    format!("{}-{}", base_name(spec), listener_name(p))
}

/// The objects for a service's domains and proxied TCP and UDP ports (none
/// without either).
pub fn route_objects(spec: &DeploySpec) -> Routes {
    let base = base_name(spec);
    let labels = json!({ "app.kubernetes.io/managed-by": MANAGER, SERVICE_LABEL: spec.service.service_id, "pyo/project-namespace": spec.service.namespace });
    let mut out = Routes::default();
    let meta = |name: &str| json!({ "name": name, "namespace": GATEWAY_NS, "labels": labels });
    let backend_name = |port: &str| format!("{base}-{port}");
    let backend_ref = |port: &str| json!({ "group": "gateway.envoyproxy.io", "kind": "Backend", "name": backend_name(port) });
    let backend = |p: &str| {
        json!({
            "apiVersion": "gateway.envoyproxy.io/v1alpha1", "kind": "Backend", "metadata": meta(&backend_name(p)),
            "spec": { "endpoints": [{ "fqdn": { "hostname": format!("{}.{}.svc.cluster.local", spec.service.name, spec.service.namespace), "port": service_port(spec, p) } }] },
        })
    };

    // Public TCP and UDP ports through the proxy: a Gateway with a listener
    // per port, a route per port to the port's Backend, and the port's options.
    let proxied: Vec<&PortSpec> = spec.ports.iter().filter(|p| p.proxied()).collect();
    if !proxied.is_empty() {
        let gw = ports_gateway(spec);
        let mut gw_meta = meta(&gw);
        gw_meta["labels"][GATEWAY_LABEL] = json!(system::L4_GATEWAY);
        let listeners: Vec<Value> = proxied
            .iter()
            .map(|p| json!({ "name": listener_name(p), "port": p.public(), "protocol": p.transport(), "allowedRoutes": { "namespaces": { "from": "Same" } } }))
            .collect();
        out.objects.push(json!({
            "apiVersion": "gateway.networking.k8s.io/v1", "kind": "Gateway", "metadata": gw_meta,
            "spec": { "gatewayClassName": "pyo", "listeners": listeners },
        }));
        for p in &proxied {
            out.objects.push(backend(&p.name));
            let route = port_route_name(spec, p);
            let kind = if p.transport() == "UDP" {
                "UDPRoute"
            } else {
                "TCPRoute"
            };
            out.objects.push(json!({
                "apiVersion": "gateway.networking.k8s.io/v1", "kind": kind, "metadata": meta(&route),
                "spec": { "parentRefs": [{ "name": gw, "sectionName": listener_name(p) }], "rules": [{ "backendRefs": [backend_ref(&p.name)] }] },
            }));
            // UDP: the firewall's address rules, as a network policy on the
            // proxy's pods (firewall.rs); Envoy Gateway does none of the
            // options for UDP (the API refuses them).
            if kind == "UDPRoute" {
                if let Some(o) = out.objects.last_mut() {
                    o["metadata"]["annotations"] = json!({
                        FIREWALL_ANNOTATION: ip_blocks_json(&fw_layers(spec, None)),
                        LISTEN_ANNOTATION: pyo_proto::proxy_listen_port(p.public()).to_string(),
                    });
                }
                continue;
            }
            let Some(opts) = p.proxy.as_ref() else {
                continue;
            };
            let route_ref = json!([{ "group": "gateway.networking.k8s.io", "kind": "TCPRoute", "name": route }]);
            // The port's own address rule, then the service's.
            let port_level = opts
                .ip_filter
                .as_ref()
                .filter(|f| !f.cidrs.is_empty())
                .map(pyo_proto::firewall::port_level);
            let layers = fw_layers(spec, port_level.as_ref());
            if let Some(authorization) =
                pyo_proto::firewall::authorization(&layers.iter().collect::<Vec<_>>(), false)
            {
                out.objects.push(json!({
                    "apiVersion": "gateway.envoyproxy.io/v1alpha1", "kind": "SecurityPolicy", "metadata": meta(&route),
                    "spec": { "targetRefs": route_ref, "authorization": authorization },
                }));
            }
            if opts.connection_limit.is_some() || opts.accept_proxy_protocol {
                let mut s = json!({ "targetRefs": [{ "group": "gateway.networking.k8s.io", "kind": "Gateway", "name": gw, "sectionName": listener_name(p) }] });
                if let Some(n) = opts.connection_limit {
                    s["connection"] = json!({ "connectionLimit": { "value": n } });
                }
                if opts.accept_proxy_protocol {
                    s["proxyProtocol"] = json!({ "optional": true });
                }
                out.objects.push(json!({ "apiVersion": "gateway.envoyproxy.io/v1alpha1", "kind": "ClientTrafficPolicy", "metadata": meta(&route), "spec": s }));
            }
            if matches!(opts.send_proxy_protocol.as_str(), "V1" | "V2") {
                out.objects.push(json!({
                    "apiVersion": "gateway.envoyproxy.io/v1alpha1", "kind": "BackendTrafficPolicy", "metadata": meta(&route),
                    "spec": { "targetRefs": route_ref, "proxyProtocol": { "version": opts.send_proxy_protocol } },
                }));
            }
        }
    }
    if spec.domains.is_empty() {
        return out;
    }

    // Backends: the ports the domains reach.
    let mut ports: Vec<&str> = spec.domains.iter().map(|d| d.port_name.as_str()).collect();
    ports.sort();
    ports.dedup();
    let ports: Vec<&str> = ports
        .into_iter()
        .filter(|p| service_port(spec, p).is_some())
        .collect();
    for p in &ports {
        out.objects.push(backend(p));
    }
    let reachable = |d: &&DomainSpec| ports.contains(&d.port_name.as_str());

    // HTTPS: a Gateway with a listener per domain, a route per port.
    let secure: Vec<&DomainSpec> = spec
        .domains
        .iter()
        .filter(|d| !d.tls.is_empty())
        .filter(reachable)
        .collect();
    if !secure.is_empty() {
        let mut listeners = vec![];
        for d in &secure {
            let secret = tls_secret(spec, &d.hostname);
            listeners.push(json!({
                "name": format!("h-{}", host_hash(&d.hostname)), "port": 443, "protocol": "HTTPS", "hostname": d.hostname,
                "tls": { "mode": "Terminate", "certificateRefs": [{ "kind": "Secret", "name": secret }] },
                "allowedRoutes": { "namespaces": { "from": "Same" } },
            }));
            if d.tls == "custom" {
                out.uploaded
                    .push((secret.clone(), d.certificate_id.clone()));
            } else {
                out.objects.push(json!({
                    "apiVersion": "cert-manager.io/v1", "kind": "Certificate", "metadata": meta(&secret),
                    "spec": { "secretName": secret, "dnsNames": [d.hostname], "issuerRef": { "name": format!("pyo-{}", d.tls), "kind": "ClusterIssuer" } },
                }));
            }
            out.secrets.push(secret);
        }
        let mut gw_meta = meta(&base);
        gw_meta["labels"][GATEWAY_LABEL] = json!("https");
        gw_meta["annotations"] = json!({ SECRETS_ANNOTATION: out.secrets.join(",") });
        out.objects.push(json!({
            "apiVersion": "gateway.networking.k8s.io/v1", "kind": "Gateway", "metadata": gw_meta,
            "spec": { "gatewayClassName": "pyo", "listeners": listeners },
        }));
        for p in &ports {
            let hosts: Vec<&str> = secure
                .iter()
                .filter(|d| d.port_name == *p)
                .map(|d| d.hostname.as_str())
                .collect();
            if !hosts.is_empty() {
                out.objects.push(json!({
                    "apiVersion": "gateway.networking.k8s.io/v1", "kind": "HTTPRoute", "metadata": meta(&format!("{base}-https-{p}")),
                    "spec": { "parentRefs": [{ "name": base }], "hostnames": hosts, "rules": [{ "backendRefs": [backend_ref(p)] }] },
                }));
            }
        }
    }

    // HTTP, on the shared :80 listener: redirects, then plain HTTP.
    let http_parent = json!([{ "name": HTTP_GATEWAY, "sectionName": "http" }]);
    let redirect: Vec<&str> = secure
        .iter()
        .filter(|d| d.redirect_https)
        .map(|d| d.hostname.as_str())
        .collect();
    if !redirect.is_empty() {
        out.objects.push(json!({
            "apiVersion": "gateway.networking.k8s.io/v1", "kind": "HTTPRoute", "metadata": meta(&format!("{base}-redirect")),
            "spec": { "parentRefs": http_parent, "hostnames": redirect, "rules": [{ "filters": [{ "type": "RequestRedirect", "requestRedirect": { "scheme": "https", "statusCode": 301 } }] }] },
        }));
    }
    for p in &ports {
        let hosts: Vec<&str> = spec
            .domains
            .iter()
            .filter(reachable)
            .filter(|d| d.port_name == *p && !(!d.tls.is_empty() && d.redirect_https))
            .map(|d| d.hostname.as_str())
            .collect();
        if !hosts.is_empty() {
            out.objects.push(json!({
                "apiVersion": "gateway.networking.k8s.io/v1", "kind": "HTTPRoute", "metadata": meta(&format!("{base}-http-{p}")),
                "spec": { "parentRefs": http_parent, "hostnames": hosts, "rules": [{ "backendRefs": [backend_ref(p)] }] },
            }));
        }
    }

    // The service's firewall on each of its routes (not the redirect to
    // HTTPS): addresses and countries, and its rate limits.
    let layers = fw_layers(spec, None);
    let authorization =
        pyo_proto::firewall::authorization(&layers.iter().collect::<Vec<_>>(), true);
    let rate = spec
        .firewall
        .as_ref()
        .and_then(pyo_proto::firewall::rate_limit);
    let names: Vec<String> = out
        .objects
        .iter()
        .filter(|o| o["kind"] == "HTTPRoute")
        .filter_map(|o| o["metadata"]["name"].as_str())
        .filter(|n| !n.ends_with("-redirect"))
        .map(str::to_owned)
        .collect();
    for name in names {
        let route_ref =
            json!([{ "group": "gateway.networking.k8s.io", "kind": "HTTPRoute", "name": name }]);
        if let Some(a) = &authorization {
            out.objects.push(json!({
                "apiVersion": "gateway.envoyproxy.io/v1alpha1", "kind": "SecurityPolicy", "metadata": meta(&name),
                "spec": { "targetRefs": route_ref, "authorization": a },
            }));
        }
        // Its rate limits, and the message a refused visitor gets.
        if rate.is_some() || authorization.is_some() {
            let mut spec = json!({ "targetRefs": route_ref });
            if let Some(r) = &rate {
                spec["rateLimit"] = r.clone();
            }
            if authorization.is_some() {
                spec["responseOverride"] = system::firewall_response_override();
            }
            out.objects.push(json!({ "apiVersion": "gateway.envoyproxy.io/v1alpha1", "kind": "BackendTrafficPolicy", "metadata": meta(&name), "spec": spec }));
        }
    }

    // The WAF: with the service's WAF on, its routes go through the
    // processor first (its custom rules, then the OWASP rules), failing
    // closed in block mode. The service's WAF rides on the policy, and the
    // processor's settings are rebuilt from every policy's (waf.rs).
    if let Some(waf) = spec.waf.as_ref().filter(|w| w.on()) {
        let routes: Vec<(String, Vec<String>)> = out
            .objects
            .iter()
            .filter(|o| o["kind"] == "HTTPRoute")
            .filter_map(|o| {
                Some((
                    o["metadata"]["name"].as_str()?.to_owned(),
                    o["spec"]["hostnames"]
                        .as_array()?
                        .iter()
                        .filter_map(|h| h.as_str().map(str::to_owned))
                        .collect(),
                ))
            })
            .filter(|(n, _)| !n.ends_with("-redirect"))
            .collect();
        for (name, hosts) in routes {
            let route = system::WafRoute {
                hosts,
                settings: waf.settings.clone(),
                rules: waf.rules.clone(),
            };
            let mut m = meta(&name);
            m["annotations"] =
                json!({ WAF_ANNOTATION: serde_json::to_string(&route).unwrap_or_default() });
            out.objects.push(json!({
                "apiVersion": "gateway.envoyproxy.io/v1alpha1", "kind": "EnvoyExtensionPolicy", "metadata": m,
                "spec": { "targetRefs": [{ "group": "gateway.networking.k8s.io", "kind": "HTTPRoute", "name": name }], "extProc": [system::waf_ext_proc(system::waf_fails_closed(&route))] },
            }));
        }
    }
    out
}

pub use system::WAF_ANNOTATION;

/// Marks a UDP route with the address ranges its port lets in, for the
/// network policy on the proxy's pods ("" when open), and with where the
/// proxy listens for it.
pub const FIREWALL_ANNOTATION: &str = "pyo.dev/firewall";
pub const LISTEN_ANNOTATION: &str = "pyo.dev/listen-port";

/// The firewall's layers for a service, in the order their rules are
/// checked: a port's own IP rule (block rules only, so it can only narrow),
/// then the service's.
pub fn fw_layers(spec: &DeploySpec, port: Option<&Firewall>) -> Vec<Firewall> {
    port.into_iter()
        .chain(spec.firewall.iter())
        .cloned()
        .collect()
}

pub fn ip_blocks_json(layers: &[Firewall]) -> String {
    pyo_proto::firewall::ip_blocks(&layers.iter().collect::<Vec<_>>())
        .map(|b| serde_json::to_string(&b).unwrap_or_default())
        .unwrap_or_default()
}

fn api_of(client: &Client, kind: &str) -> Api<DynamicObject> {
    let ar = match kind {
        "Gateway" => gateways(),
        "HTTPRoute" => routes(),
        "TCPRoute" => tcp_routes(),
        "UDPRoute" => udp_routes(),
        "Backend" => backends(),
        "Certificate" => certificates(),
        "SecurityPolicy" => security_policies(),
        "BackendTrafficPolicy" => backend_policies(),
        "EnvoyExtensionPolicy" => extension_policies(),
        _ => client_policies(),
    };
    Api::namespaced_with(client.clone(), GATEWAY_NS, &ar)
}

/// Whether the proxy's APIs are there yet: Envoy Gateway's own and the
/// Gateway API's.
pub async fn installed(client: &Client) -> bool {
    api_of(client, "Backend")
        .list(&ListParams::default().limit(1))
        .await
        .is_ok()
        && api_of(client, "Gateway")
            .list(&ListParams::default().limit(1))
            .await
            .is_ok()
}

/// The Job k3s's helm controller runs to install Envoy Gateway's chart
/// (`system::envoy_gateway_chart`).
const CHART_JOB: &str = "helm-install-envoy-gateway";
/// Failed runs of it before the install counts as failed, not under way.
const CHART_FAILURES: i32 = 3;

/// Why the gateway isn't coming up, once that's clear: its chart's install
/// keeps failing (Helm's error is in its job's log, which the agent can't
/// read), or the chart is in but its APIs aren't. None while it's on its
/// way, or without its chart.
pub async fn install_problem(client: &Client) -> Option<String> {
    use k8s_openapi::api::{batch::v1::Job, core::v1::Pod};
    let job = Api::<Job>::namespaced(client.clone(), "kube-system")
        .get_opt(CHART_JOB)
        .await
        .ok()??;
    let status = job.status.unwrap_or_default();
    let condition = |t: &str| {
        status
            .conditions
            .iter()
            .flatten()
            .any(|c| c.type_ == t && c.status == "True")
    };
    if condition("Complete") {
        let lp = ListParams::default().limit(1);
        return missing_apis(
            api_of(client, "Gateway").list(&lp).await.is_ok(),
            api_of(client, "Backend").list(&lp).await.is_ok(),
        );
    }
    let pods = Api::<Pod>::namespaced(client.clone(), "kube-system")
        .list(&ListParams::default().labels(&format!("job-name={CHART_JOB}")))
        .await
        .map(|l| l.items)
        .unwrap_or_default();
    let restarts: i32 = pods
        .iter()
        .flat_map(|p| {
            p.status
                .iter()
                .flat_map(|s| s.container_statuses.iter().flatten())
        })
        .map(|c| c.restart_count)
        .sum();
    chart_failed(condition("Failed"), restarts + status.failed.unwrap_or(0))
}

/// The chart's install counts as failed once its job says so, or after
/// `CHART_FAILURES` failed runs.
fn chart_failed(failed: bool, failures: i32) -> Option<String> {
    (failed || failures >= CHART_FAILURES).then(|| {
        format!(
            "Gateway failed to install. See `sudo k3s kubectl -n kube-system logs job/{CHART_JOB}`."
        )
    })
}

/// What's missing once the chart is in: the Gateway API (the cluster's,
/// or the copy connect.sh applies) or Envoy Gateway's own CRDs.
fn missing_apis(gateway_api: bool, envoy_api: bool) -> Option<String> {
    if !gateway_api {
        Some("Gateway failed to install: this cluster has no Gateway API. Run the cluster's connect command again.".into())
    } else if !envoy_api {
        Some("Gateway failed to install: Envoy Gateway's CRDs are missing. Run the cluster's connect command again.".into())
    } else {
        None
    }
}

/// Apply a service's routes and remove what it no longer has. Notes for the
/// deployment log come back.
pub async fn apply(
    client: &Client,
    pyo: &PyoApi,
    spec: &DeploySpec,
) -> anyhow::Result<Vec<String>> {
    let mut notes = vec![];
    let exposed = !spec.domains.is_empty() || spec.ports.iter().any(|p| p.proxied());
    let problem = if exposed {
        install_problem(client).await
    } else {
        None
    };
    if !installed(client).await {
        if exposed {
            notes.push(match &problem {
                Some(p) => p.clone(),
                None => "The gateway is still being installed on this cluster. Its domains and proxied ports start answering with a later deploy.".into(),
            });
        }
        return Ok(notes);
    }
    if let Some(p) = &problem {
        notes.push(p.clone());
    }
    let mut r = route_objects(spec);
    // New public TCP and UDP ports wait for the servers to free them (a port
    // that just stopped being published straight from them), and stay off
    // the proxy while something else on the cluster publishes them: one
    // server port can't serve both, and the proxy's websites would stop.
    let gw = ports_gateway(spec);
    if let Some(o) = r
        .objects
        .iter_mut()
        .find(|o| o["kind"] == "Gateway" && o["metadata"]["name"] == gw.as_str())
    {
        let current: Vec<String> = api_of(client, "Gateway")
            .get_opt(&gw)
            .await?
            .map(|g| {
                g.data["spec"]["listeners"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|l| l["name"].as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default();
        let mut keep = vec![];
        for l in o["spec"]["listeners"]
            .as_array()
            .cloned()
            .unwrap_or_default()
        {
            let name = l["name"].as_str().unwrap_or("").to_owned();
            let (proto, port) = (
                l["protocol"].as_str().unwrap_or("TCP").to_owned(),
                l["port"].as_i64().unwrap_or(0) as i32,
            );
            if current.contains(&name) {
                keep.push(l);
                continue;
            }
            match port_free(client, &proto, port).await {
                Ok(()) => keep.push(l),
                Err(why) => notes.push(format!(
                    "Port {port}/{proto} isn't going through the gateway yet: {why}"
                )),
            }
        }
        if keep.is_empty() {
            let gw_name = gw.clone();
            r.objects
                .retain(|o| !(o["kind"] == "Gateway" && o["metadata"]["name"] == gw_name.as_str()));
        } else {
            o["spec"]["listeners"] = json!(keep);
        }
    }
    let secrets: Api<Secret> = Api::namespaced(client.clone(), GATEWAY_NS);
    for (secret, cert_id) in &r.uploaded {
        // A domain that had a cert-manager certificate under this name:
        // that one goes first, with its Secret (which it owns), so neither
        // cert-manager nor the owner's deletion takes the uploaded one.
        retire_certificate(client, secret).await?;
        let host = spec
            .domains
            .iter()
            .find(|d| tls_secret(spec, &d.hostname) == *secret)
            .map(|d| d.hostname.clone())
            .unwrap_or_default();
        let cert = pyo
            .certificate(cert_id)
            .await
            .map_err(|e| anyhow::anyhow!("Couldn't get the certificate for {host}: {e}"))?;
        let obj = json!({
            "apiVersion": "v1", "kind": "Secret", "type": "kubernetes.io/tls",
            "metadata": { "name": secret, "namespace": GATEWAY_NS, "labels": { "app.kubernetes.io/managed-by": MANAGER, SERVICE_LABEL: spec.service.service_id, "pyo/tls": "custom" } },
            "stringData": { "tls.crt": cert.certificate, "tls.key": cert.private_key },
        });
        secrets.patch(secret, &pp(), &Patch::Apply(&obj)).await?;
    }
    let mut wanted: BTreeMap<(String, String), ()> = BTreeMap::new();
    for o in &r.objects {
        let kind = o["kind"].as_str().unwrap_or("").to_owned();
        let name = o["metadata"]["name"].as_str().unwrap_or("").to_owned();
        let api = api_of(client, &kind);
        match api.patch(&name, &pp(), &Patch::Apply(o)).await {
            Ok(_) => {}
            Err(kube::Error::Api(e)) if e.code == 404 && kind == "Certificate" => {
                notes.push("cert-manager is still being installed on this cluster. HTTPS starts with a later deploy.".into());
            }
            // An API not there (yet): routing waits for a later deploy.
            Err(kube::Error::Api(e)) if e.code == 404 => {
                notes.push(format!("The gateway isn't ready on this cluster yet ({kind}). Its domains start answering with a later deploy."));
                return Ok(notes);
            }
            Err(e) => return Err(anyhow::anyhow!("{kind} {name}: {e}")),
        }
        wanted.insert((kind, name), ());
    }
    // Secrets the Gateway used before and no longer does.
    let before: Vec<String> = api_of(client, "Gateway")
        .get_opt(&base_name(spec))
        .await
        .ok()
        .flatten()
        .and_then(|g| g.annotations().get(SECRETS_ANNOTATION).cloned())
        .map(|s| {
            s.split(',')
                .filter(|x| !x.is_empty())
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default();
    for old in before.iter().filter(|s| !r.secrets.contains(s)) {
        let _ = secrets.delete(old, &DeleteParams::default()).await;
    }
    remove_stale(client, &spec.service.service_id, &wanted, &r.secrets).await;
    // The firewall on UDP ports: the proxy pods' network policy.
    // (The details go to the agent's log; the deploy's log says it plainly.)
    if let Err(e) = crate::firewall::proxy_policy(client).await {
        tracing::warn!(error = %format!("{e:#}"), "couldn't apply the firewall for UDP ports");
        notes.push(
            "The firewall for UDP ports couldn't be updated. It keeps its previous rules.".into(),
        );
    }
    // The WAF's settings, from every route's: only while it's on in this
    // cluster (the deploy carries its WAF then) and installed.
    if let Err(e) = crate::waf::settings(client, spec.waf.is_some()).await {
        tracing::warn!(error = %format!("{e:#}"), "couldn't update the WAF's settings");
        notes
            .push("The WAF's settings couldn't be updated. It keeps its previous settings.".into());
    }
    Ok(notes)
}

/// The newest domains this agent has seen per service (by their
/// `routes_revision`), and a lock per service: a deploy, its rollback, a
/// firewall sync and a routes sync each apply a service's routes, one at a
/// time, with the newest domains whichever of them carried those. So a
/// rollback, or a deploy handed out before a domain changed, never brings
/// back a removed domain or drops a new one.
#[derive(Clone, Default)]
pub struct RouteState(std::sync::Arc<RouteStateInner>);

#[derive(Default)]
pub struct RouteStateInner {
    newest: std::sync::Mutex<std::collections::HashMap<String, (i64, Vec<DomainSpec>)>>,
    locks:
        std::sync::Mutex<std::collections::HashMap<String, std::sync::Arc<tokio::sync::Mutex<()>>>>,
}

impl RouteState {
    fn lock_for(&self, service_id: &str) -> std::sync::Arc<tokio::sync::Mutex<()>> {
        self.0
            .locks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(service_id.to_owned())
            .or_default()
            .clone()
    }

    /// The spec's domains when they're the newest seen (remembered then),
    /// else the newest instead.
    pub fn settle(&self, spec: &mut DeploySpec) {
        let mut newest = self.0.newest.lock().unwrap_or_else(|e| e.into_inner());
        match newest.get(&spec.service.service_id) {
            Some((revision, domains)) if *revision > spec.routes_revision => {
                spec.domains = domains.clone();
                spec.routes_revision = *revision;
            }
            _ => {
                newest.insert(
                    spec.service.service_id.clone(),
                    (spec.routes_revision, spec.domains.clone()),
                );
            }
        }
    }
}

/// A service's routes as a spec to apply: only what routes are made of
/// (never its variables). Its WAF only while the cluster runs the WAF's
/// engine (`waf`).
pub fn target_spec(t: &pyo_proto::FirewallTarget, waf: bool) -> anyhow::Result<DeploySpec> {
    Ok(serde_json::from_value(json!({
        "deploymentId": "", "service": t.service, "projectId": t.project_id, "image": "", "replicas": 0, "env": [],
        "ports": t.ports, "domains": t.domains, "routesRevision": t.routes_revision,
        "resources": { "cpuRequest": 0, "cpuLimit": 0, "memoryRequest": 0, "memoryLimit": 0 },
        "healthCheck": { "type": "none", "path": "", "port": 0, "command": "", "initialDelaySeconds": 0, "periodSeconds": 0 },
        "volumes": [], "timeoutSeconds": 0, "networkIsolation": t.network_isolation, "firewall": t.firewall,
        "waf": t.waf.as_ref().filter(|_| waf),
    }))?)
}

/// Apply a service's routes ([`apply`]) with the newest domains seen
/// ([`RouteState`]), one apply per service at a time: what every rollout,
/// rollback, firewall sync and routes sync goes through. With
/// `only_if_running`, a service without its workload here (deleted, or
/// its first rollout not that far yet) only has its domains remembered,
/// and None comes back. Else the notes and the domains applied.
pub async fn apply_routes(
    agent: &crate::Agent,
    spec: &DeploySpec,
    only_if_running: bool,
) -> anyhow::Result<Option<(Vec<String>, Vec<DomainSpec>)>> {
    let lock = agent.routes.lock_for(&spec.service.service_id);
    let _held = lock.lock().await;
    let mut spec = spec.clone();
    agent.routes.settle(&mut spec);
    if only_if_running && !workload_exists(&agent.kube, &spec.service).await? {
        return Ok(None);
    }
    let notes = apply(&agent.kube, &agent.api, &spec).await?;
    Ok(Some((notes, spec.domains)))
}

/// `SyncRoutes`: a service's domains changed; its routes follow, in place.
/// Only gateway objects change (in `pyo-gateway`): its pods aren't touched,
/// and the proxies update their routes without dropping connections.
pub async fn sync_routes(
    agent: &crate::Agent,
    target: &pyo_proto::FirewallTarget,
) -> anyhow::Result<String> {
    let spec = target_spec(target, true)?;
    match apply_routes(agent, &spec, true).await? {
        // Its first rollout applies them (with these domains, or newer).
        None => Ok(format!(
            "{} isn't running here yet. Its rollout applies its domains",
            target.service.name
        )),
        Some((notes, domains)) if notes.is_empty() => Ok(format!(
            "{} domain(s) of {} in place",
            domains.len(),
            target.service.name
        )),
        Some((notes, _)) => Ok(notes.join(" ")),
    }
}

/// Whether a server port is free for the gateway: no other public
/// Service on the cluster has it, and no ServiceLB pod (other than the
/// proxy's) still holds it on a server. One that's on its way out (a port
/// that just stopped bypassing the proxy) is waited for, up to 90 seconds.
async fn port_free(client: &Client, protocol: &str, port: i32) -> Result<(), String> {
    use k8s_openapi::api::core::v1::{Pod, Service};
    let services = Api::<Service>::all(client.clone())
        .list(&ListParams::default())
        .await
        .map_err(|e| e.to_string())?;
    for svc in services.items {
        let ns = svc.namespace().unwrap_or_default();
        // A public port held while the cluster's public traffic is off too.
        let held = crate::rollout::Published::of(&svc).map(|p| p.kind);
        let Some(spec) = svc.spec.as_ref().filter(|s| {
            s.type_.as_deref() == Some("LoadBalancer") || held.as_deref() == Some("LoadBalancer")
        }) else {
            continue;
        };
        if ns == GATEWAY_NS {
            continue;
        }
        if spec
            .ports
            .iter()
            .flatten()
            .any(|p| p.port == port && p.protocol.as_deref().unwrap_or("TCP") == protocol)
        {
            return Err(format!(
                "{ns}/{} publishes it on the servers. Free the port there, then deploy again.",
                svc.name_any()
            ));
        }
    }
    let pods: Api<Pod> = Api::namespaced(client.clone(), "kube-system");
    for _ in 0..45 {
        let held = pods
            .list(&ListParams::default().labels("svccontroller.k3s.cattle.io/svcnamespace"))
            .await
            .map_err(|e| e.to_string())?
            .items
            .into_iter()
            .filter(|p| {
                p.labels()
                    .get("svccontroller.k3s.cattle.io/svcnamespace")
                    .map(String::as_str)
                    != Some(GATEWAY_NS)
            })
            .filter(|p| {
                p.spec.as_ref().is_some_and(|s| s.node_name.is_some())
                    && !matches!(
                        p.status.as_ref().and_then(|s| s.phase.as_deref()),
                        Some("Succeeded" | "Failed")
                    )
            })
            .any(|p| {
                p.spec
                    .iter()
                    .flat_map(|s| &s.containers)
                    .flat_map(|c| c.ports.iter().flatten())
                    .any(|cp| {
                        cp.host_port == Some(port)
                            && cp.protocol.as_deref().unwrap_or("TCP") == protocol
                    })
            });
        if !held {
            return Ok(());
        }
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    }
    Err("the servers still hold it for a service that published it directly. Deploy again in a minute.".into())
}

/// Before a deploy changes the Services: listeners of ports that no longer
/// go through the gateway come off its servers first, so a port moving
/// to its own public address finds it free. Notes for the log come back.
pub async fn release_ports(client: &Client, spec: &DeploySpec) -> anyhow::Result<Vec<String>> {
    if !installed(client).await {
        return Ok(vec![]);
    }
    let api = api_of(client, "Gateway");
    let gw = ports_gateway(spec);
    let Some(existing) = api.get_opt(&gw).await? else {
        return Ok(vec![]);
    };
    let wanted: Vec<String> = spec
        .ports
        .iter()
        .filter(|p| p.proxied())
        .map(listener_name)
        .collect();
    let listeners = existing.data["spec"]["listeners"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let keep: Vec<Value> = listeners
        .iter()
        .filter(|l| {
            l["name"]
                .as_str()
                .is_some_and(|n| wanted.iter().any(|w| w == n))
        })
        .cloned()
        .collect();
    if keep.len() == listeners.len() {
        return Ok(vec![]);
    }
    let gone: Vec<String> = listeners
        .iter()
        .filter(|l| !keep.contains(l))
        .map(|l| format!("{}/{}", l["port"], l["protocol"].as_str().unwrap_or("")))
        .collect();
    if keep.is_empty() {
        api.delete(&gw, &DeleteParams::default()).await?;
    } else {
        let obj = json!({
            "apiVersion": "gateway.networking.k8s.io/v1", "kind": "Gateway",
            "metadata": { "name": gw, "namespace": GATEWAY_NS, "labels": existing.labels() },
            "spec": { "gatewayClassName": "pyo", "listeners": keep },
        });
        api.patch(&gw, &pp(), &Patch::Apply(&obj)).await?;
    }
    Ok(vec![format!(
        "Port(s) {} no longer go through the gateway",
        gone.join(", ")
    )])
}

/// The kinds of a service's objects in `pyo-gateway` (labelled with its id).
const SERVICE_KINDS: [&str; 10] = [
    "HTTPRoute",
    "TCPRoute",
    "UDPRoute",
    "SecurityPolicy",
    "ClientTrafficPolicy",
    "BackendTrafficPolicy",
    "EnvoyExtensionPolicy",
    "Gateway",
    "Backend",
    "Certificate",
];

/// A cert-manager Certificate whose Secret name is about to hold an
/// uploaded certificate: deleted, its dependents first (the Secret, which
/// cert-manager makes it own), and waited for; then the Secret by name, in
/// case it was made before Certificates owned theirs.
async fn retire_certificate(client: &Client, name: &str) -> anyhow::Result<()> {
    let certs = api_of(client, "Certificate");
    if certs.get_opt(name).await?.is_none() {
        return Ok(());
    }
    match certs.delete(name, &DeleteParams::foreground()).await {
        Ok(_) => {}
        Err(kube::Error::Api(e)) if e.code == 404 => {}
        Err(e) => return Err(e.into()),
    }
    for _ in 0..30 {
        if certs.get_opt(name).await?.is_none() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }
    match Api::<Secret>::namespaced(client.clone(), GATEWAY_NS)
        .delete(name, &DeleteParams::default())
        .await
    {
        Ok(_) => Ok(()),
        Err(kube::Error::Api(e)) if e.code == 404 => Ok(()),
        Err(e) => Err(e.into()),
    }
}

/// Remove a service's objects that aren't in `wanted` (everything, when
/// empty), and the Secrets of the certificates removed (cert-manager keeps
/// those otherwise), unless one of `keep` (Secrets its Gateway still uses).
async fn remove_stale(
    client: &Client,
    service_id: &str,
    wanted: &BTreeMap<(String, String), ()>,
    keep: &[String],
) {
    let secrets: Api<Secret> = Api::namespaced(client.clone(), GATEWAY_NS);
    for kind in SERVICE_KINDS {
        let api = api_of(client, kind);
        let Ok(list) = api
            .list(&ListParams::default().labels(&format!("{SERVICE_LABEL}={service_id}")))
            .await
        else {
            continue;
        };
        for o in list.items {
            let name = o.name_any();
            if wanted.contains_key(&(kind.to_owned(), name.clone())) {
                continue;
            }
            if kind == "Gateway" {
                for s in o
                    .annotations()
                    .get(SECRETS_ANNOTATION)
                    .map(|s| s.split(',').map(str::to_owned).collect::<Vec<_>>())
                    .unwrap_or_default()
                {
                    if !keep.contains(&s) {
                        let _ = secrets.delete(&s, &DeleteParams::default()).await;
                    }
                }
            }
            let _ = api.delete(&name, &DeleteParams::default()).await;
            // A Certificate's Secret is named after it (the admission policy
            // holds it to that).
            if kind == "Certificate" && !keep.contains(&name) {
                let _ = secrets.delete(&name, &DeleteParams::default()).await;
            }
        }
    }
}

/// Whether a service's workload is on the cluster (and not on its way
/// out): a firewall sync handed out before the service was deleted must not
/// put its routes back.
pub async fn workload_exists(
    client: &Client,
    service: &pyo_proto::ServiceRef,
) -> anyhow::Result<bool> {
    use k8s_openapi::api::{
        apps::v1::{Deployment, StatefulSet},
        batch::v1::CronJob,
    };
    let ns = service.namespace.as_str();
    let meta = match service.workload_kind.as_str() {
        "StatefulSet" => Api::<StatefulSet>::namespaced(client.clone(), ns)
            .get_opt(&service.name)
            .await?
            .map(|w| w.metadata),
        "CronJob" => Api::<CronJob>::namespaced(client.clone(), ns)
            .get_opt(&service.name)
            .await?
            .map(|w| w.metadata),
        _ => Api::<Deployment>::namespaced(client.clone(), ns)
            .get_opt(&service.name)
            .await?
            .map(|w| w.metadata),
    };
    Ok(meta.is_some_and(|m| {
        m.deletion_timestamp.is_none()
            && m.labels.as_ref().and_then(|l| l.get(SERVICE_LABEL)) == Some(&service.service_id)
    }))
}

/// How old a service's gateway objects must all be before they're
/// collected without a workload: a first deploy writes its routes before
/// its workload is up.
const ORPHAN_AFTER_SECS: i64 = 10 * 60;

/// Service ids in `pyo-gateway` with nothing behind them: no workload
/// (`live`), no rollout under way (`deploying`), and every object (its
/// newest creation time, in seconds) at least `min_age` old.
pub fn orphans(
    found: &BTreeMap<String, i64>,
    live: &std::collections::BTreeSet<String>,
    deploying: impl Fn(&str) -> bool,
    now: i64,
    min_age: i64,
) -> Vec<String> {
    found
        .iter()
        .filter(|(id, newest)| {
            !live.contains(*id) && now.saturating_sub(**newest) >= min_age && !deploying(id)
        })
        .map(|(id, _)| id.clone())
        .collect()
}

/// Every service id that has a workload on the cluster.
async fn live_services(client: &Client) -> anyhow::Result<std::collections::BTreeSet<String>> {
    use k8s_openapi::api::{
        apps::v1::{Deployment, StatefulSet},
        batch::v1::CronJob,
    };
    let lp = ListParams::default().labels(SERVICE_LABEL);
    let mut live = std::collections::BTreeSet::new();
    let id = |m: &kube::api::ObjectMeta| {
        m.labels
            .as_ref()
            .and_then(|l| l.get(SERVICE_LABEL))
            .cloned()
    };
    live.extend(
        Api::<Deployment>::all(client.clone())
            .list(&lp)
            .await?
            .items
            .iter()
            .filter_map(|w| id(&w.metadata)),
    );
    live.extend(
        Api::<StatefulSet>::all(client.clone())
            .list(&lp)
            .await?
            .items
            .iter()
            .filter_map(|w| id(&w.metadata)),
    );
    live.extend(
        Api::<CronJob>::all(client.clone())
            .list(&lp)
            .await?
            .items
            .iter()
            .filter_map(|w| id(&w.metadata)),
    );
    Ok(live)
}

/// Garbage collection: removes the gateway objects (routes, Gateways,
/// Backends, policies, certificates and their Secrets) of services with no
/// workload on the cluster, such as a deleted service's that a firewall
/// sync put back. Any error stops it before it removes anything. The
/// services removed come back.
pub async fn collect(agent: &crate::Agent) -> anyhow::Result<Vec<String>> {
    collect_older_than(&agent.kube, &agent.deploying, ORPHAN_AFTER_SECS).await
}

async fn collect_older_than(
    client: &Client,
    deploying: &crate::Deploying,
    min_age: i64,
) -> anyhow::Result<Vec<String>> {
    if !installed(client).await {
        return Ok(vec![]);
    }
    // Each service's newest object first, its workloads after: a rollout
    // that starts in between is in `deploying` by the time it's read.
    let mut found: BTreeMap<String, i64> = BTreeMap::new();
    for kind in SERVICE_KINDS {
        let list = match api_of(client, kind)
            .list(&ListParams::default().labels(SERVICE_LABEL))
            .await
        {
            Ok(l) => l.items,
            // Not installed (cert-manager still coming): none of those.
            Err(kube::Error::Api(e)) if e.code == 404 => vec![],
            Err(e) => return Err(e.into()),
        };
        for o in list {
            let Some(id) = o
                .labels()
                .get(SERVICE_LABEL)
                .filter(|id| !id.is_empty())
                .cloned()
            else {
                continue;
            };
            let created = o
                .metadata
                .creation_timestamp
                .as_ref()
                .map(|t| t.0.as_second())
                .unwrap_or(i64::MAX);
            let newest = found.entry(id).or_insert(i64::MIN);
            *newest = (*newest).max(created);
        }
    }
    if found.is_empty() {
        return Ok(vec![]);
    }
    let live = live_services(client).await?;
    let now = k8s_openapi::jiff::Timestamp::now().as_second();
    let gone = orphans(&found, &live, |id| deploying.contains(id), now, min_age);
    for id in &gone {
        tracing::info!(service_id = %id, "removing the gateway objects of a service with no workload");
        remove_stale(client, id, &BTreeMap::new(), &[]).await;
    }
    if !gone.is_empty() {
        let _ = crate::firewall::proxy_policy(client).await;
    }
    Ok(gone)
}

/// [`collect`] every 10 minutes (the first a few minutes after the agent
/// starts), unless the cluster's policy is read only.
pub async fn collect_loop(agent: crate::Agent) {
    tokio::time::sleep(std::time::Duration::from_secs(3 * 60)).await;
    let mut tick = tokio::time::interval(std::time::Duration::from_secs(10 * 60));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tick.tick().await;
        if agent
            .policy
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .read_only
        {
            continue;
        }
        if let Err(e) = collect(&agent).await {
            tracing::warn!(error = %format!("{e:#}"), "couldn't collect the gateway's leftover objects");
        }
    }
}

/// A deleted service: all its routes, Gateway, Backends and certificates.
pub async fn remove(client: &Client, service_id: &str) {
    if installed(client).await {
        remove_stale(client, service_id, &BTreeMap::new(), &[]).await;
        let _ = crate::firewall::proxy_policy(client).await;
    }
}

/// The fixed objects of the proxy: its settings, class, shared Gateway and
/// client policies (with origin pull as last set). Retried until the APIs
/// exist (Envoy Gateway installs a minute or so after the manifest). Run at
/// each start, so a new GeoIP image (`PYO_GEOIP_IMAGE`, from the install
/// manifest that also allows it) reaches the proxies, which then roll.
/// `pyo_host`: this is the cluster Pyo runs on, so Pyo's own HTTPS listeners
/// on the tailnet are applied too (system.rs `tailnet_objects`; without the
/// host's certificate they just stay unready, and only admit the tailnet).
/// Their policy then follows the servers as they join and leave.
pub async fn ensure(client: Client, pyo_host: bool) {
    let mut servers = loop {
        let r = match ensure_once(&client).await {
            Ok(()) if pyo_host => ensure_tailnet(&client).await,
            Ok(()) => Ok(Vec::new()),
            Err(e) => Err(e),
        };
        match r {
            Ok(servers) => {
                tracing::info!("gateway ready");
                break servers;
            }
            Err(e) => {
                tracing::info!(error = %format!("{e:#}"), "gateway not ready yet; retrying");
                tokio::time::sleep(std::time::Duration::from_secs(30)).await;
            }
        }
    };
    if !pyo_host {
        return;
    }
    let mut tick = tokio::time::interval(std::time::Duration::from_secs(60));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tick.tick().await;
        match server_cidrs(&client).await {
            Ok(now) if now != servers => match api_of(&client, "SecurityPolicy")
                .patch(
                    system::TAILNET_GATEWAY,
                    &pp(),
                    &Patch::Apply(system::tailnet_policy(&now)),
                )
                .await
            {
                Ok(_) => servers = now,
                Err(e) => {
                    tracing::warn!(error = %e, "couldn't update the tailnet listeners' policy")
                }
            },
            Ok(_) => {}
            Err(e) => tracing::warn!(error = %e, "couldn't list the servers"),
        }
    }
}

/// The servers' own addresses on the pod network, as the tailnet listeners
/// admit them (system.rs `tailnet_server_cidrs`).
async fn server_cidrs(client: &Client) -> kube::Result<Vec<String>> {
    let nodes = Api::<k8s_openapi::api::core::v1::Node>::all(client.clone())
        .list(&ListParams::default())
        .await?
        .items;
    let ranges: Vec<String> = nodes
        .into_iter()
        .filter_map(|n| n.spec)
        .flat_map(|s| {
            s.pod_cidrs
                .filter(|c| !c.is_empty())
                .or(s.pod_cidr.map(|c| vec![c]))
                .unwrap_or_default()
        })
        .collect();
    Ok(system::tailnet_server_cidrs(
        ranges.iter().map(String::as_str),
    ))
}

/// Pyo's dashboard and registry door over HTTPS, on the tailnet only (and
/// the servers themselves): the servers' addresses it admits.
async fn ensure_tailnet(client: &Client) -> anyhow::Result<Vec<String>> {
    let servers = server_cidrs(client).await?;
    for o in system::tailnet_objects(&servers) {
        let kind = o["kind"].as_str().unwrap_or("");
        let name = o["metadata"]["name"].as_str().unwrap_or("");
        api_of(client, kind)
            .patch(name, &pp(), &Patch::Apply(&o))
            .await
            .map_err(|e| anyhow::anyhow!("{kind} {name}: {e}"))?;
    }
    Ok(servers)
}

/// The cluster's public traffic as the API last said: 0 not heard yet,
/// 1 on, 2 off.
static PUBLIC: AtomicU8 = AtomicU8::new(0);

fn heard_public() -> Option<bool> {
    match PUBLIC.load(Ordering::Relaxed) {
        1 => Some(true),
        2 => Some(false),
        _ => None,
    }
}

/// Whether the proxies are published on the servers: as the API last said,
/// else as they are, else (a new cluster) yes.
pub fn wanted_public(heard: Option<bool>, current: Option<bool>) -> bool {
    heard.or(current).unwrap_or(true)
}

/// Whether the cluster takes public traffic now: as the API last said, else
/// as the proxies are published, else yes.
pub async fn public_now(client: &Client) -> bool {
    let current = envoy_proxies(client)
        .get_opt("pyo")
        .await
        .ok()
        .flatten()
        .map(|p| system::envoy_proxy_public(&p.data["spec"]));
    wanted_public(heard_public(), current)
}

fn envoy_proxies(client: &Client) -> Api<DynamicObject> {
    Api::namespaced_with(
        client.clone(),
        GATEWAY_NS,
        &resource(
            "gateway.envoyproxy.io",
            "v1alpha1",
            "EnvoyProxy",
            "envoyproxies",
        ),
    )
}

/// The proxies are published on the servers or not, as the cluster's
/// public traffic setting says (from each heartbeat's reply; patched only
/// when it differs). Before the gateway is installed it's kept for
/// [`ensure_once`].
pub async fn set_public_traffic(client: &Client, on: bool) {
    PUBLIC.store(if on { 1 } else { 2 }, Ordering::Relaxed);
    let proxies = envoy_proxies(client);
    let Ok(Some(current)) = proxies.get_opt("pyo").await else {
        return;
    };
    if system::envoy_proxy_public(&current.data["spec"]) == on {
        return;
    }
    match proxies
        .patch(
            "pyo",
            &pp(),
            &Patch::Apply(system::envoy_proxy(&system::geoip_image_from_env(), on)),
        )
        .await
    {
        Ok(_) => tracing::info!(on, "public traffic"),
        Err(e) => tracing::warn!(error = %e, "couldn't change public traffic"),
    }
}

/// The Tailscale addresses the websites are published on: every server's
/// that runs a proxy (all but build-only ones), from what locate.rs found,
/// else a node address on the tailnet. Sorted, once each.
pub fn tailnet_web_ips(nodes: &[k8s_openapi::api::core::v1::Node]) -> Vec<String> {
    let mut ips: Vec<String> = nodes
        .iter()
        .filter(|n| {
            n.labels()
                .get(pyo_proto::BUILDER_NODE_LABEL)
                .map(String::as_str)
                != Some("true")
        })
        .filter_map(|n| {
            n.annotations()
                .get(pyo_proto::TAILNET_IPV4_ANNOTATION)
                .filter(|ip| crate::locate::tailnet_v4(ip))
                .cloned()
                .or_else(|| {
                    n.status
                        .as_ref()?
                        .addresses
                        .iter()
                        .flatten()
                        .map(|a| a.address.as_str())
                        .find(|a| crate::locate::tailnet_v4(a))
                        .map(str::to_owned)
                })
        })
        .collect();
    ips.sort();
    ips.dedup();
    ips
}

/// What to do about the websites' Service on the Tailscale addresses:
/// None when it's as it should be, Some(None) to remove it, Some(Some(ips))
/// to write it on those addresses. `current`: its addresses and ports now.
pub fn tailnet_web_change(
    on: bool,
    wanted: Vec<String>,
    current: Option<(Vec<String>, Value)>,
) -> Option<Option<Vec<String>>> {
    match (on && !wanted.is_empty(), current) {
        (false, None) => None,
        (false, Some(_)) => Some(None),
        (true, Some((ips, ports))) if ips == wanted && ports == system::tailnet_web_ports() => None,
        (true, _) => Some(Some(wanted)),
    }
}

/// The tailnet websites' Service was refused (said once).
static TAILNET_WEB_REFUSED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Websites on the servers' Tailscale addresses (system.rs), while the
/// cluster's free domains point at its Tailscale IP (from each heartbeat's
/// reply): the Service written or removed when that differs.
pub async fn set_tailnet_web(client: &Client, on: bool) {
    let services: Api<k8s_openapi::api::core::v1::Service> =
        Api::namespaced(client.clone(), GATEWAY_NS);
    let wanted = if on {
        match Api::<k8s_openapi::api::core::v1::Node>::all(client.clone())
            .list(&ListParams::default())
            .await
        {
            Ok(l) => tailnet_web_ips(&l.items),
            Err(e) => {
                tracing::warn!(error = %e, "couldn't list servers for the tailnet websites");
                return;
            }
        }
    } else {
        vec![]
    };
    let current = match services.get_opt(system::TAILNET_WEB_SERVICE).await {
        Ok(s) => s.map(|s| {
            let spec = s.spec.unwrap_or_default();
            let mut ips = spec.external_ips.unwrap_or_default();
            ips.sort();
            let ports = serde_json::to_value(spec.ports.unwrap_or_default()).unwrap_or_default();
            (ips, ports)
        }),
        Err(e) => {
            tracing::warn!(error = %e, "couldn't read the tailnet websites' Service");
            return;
        }
    };
    match tailnet_web_change(on, wanted, current) {
        None => {}
        Some(None) => match services
            .delete(system::TAILNET_WEB_SERVICE, &DeleteParams::default())
            .await
        {
            Ok(_) => tracing::info!("websites off the Tailscale addresses"),
            Err(e) => tracing::warn!(error = %e, "couldn't remove the tailnet websites' Service"),
        },
        Some(Some(ips)) => match services
            .patch(
                system::TAILNET_WEB_SERVICE,
                &pp(),
                &Patch::Apply(system::tailnet_web_service(&ips)),
            )
            .await
        {
            Ok(_) => {
                TAILNET_WEB_REFUSED.store(false, Ordering::Relaxed);
                tracing::info!(?ips, "websites on the Tailscale addresses")
            }
            // Refused (an install from before this Service: its connect
            // command brings it up to date): said once, not every heartbeat.
            Err(kube::Error::Api(e)) if e.code == 403 => {
                if !TAILNET_WEB_REFUSED.swap(true, Ordering::Relaxed) {
                    tracing::warn!(error = %e, "couldn't publish websites on the Tailscale addresses (run the cluster's connect command)");
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "couldn't publish websites on the Tailscale addresses")
            }
        },
    }
}

async fn ensure_once(client: &Client) -> anyhow::Result<()> {
    let proxies = envoy_proxies(client);
    let current = proxies
        .get_opt("pyo")
        .await?
        .map(|p| system::envoy_proxy_public(&p.data["spec"]));
    let public = wanted_public(heard_public(), current);
    proxies
        .patch(
            "pyo",
            &pp(),
            &Patch::Apply(system::envoy_proxy(&system::geoip_image_from_env(), public)),
        )
        .await?;
    let classes = Api::<DynamicObject>::all_with(
        client.clone(),
        &resource(
            "gateway.networking.k8s.io",
            "v1",
            "GatewayClass",
            "gatewayclasses",
        ),
    );
    classes
        .patch("pyo", &pp(), &Patch::Apply(system::gateway_class()))
        .await?;
    api_of(client, "Gateway")
        .patch(HTTP_GATEWAY, &pp(), &Patch::Apply(system::http_gateway()))
        .await?;
    // Where the routes' WAF policies send (whether or not the WAF is on).
    api_of(client, "Backend")
        .patch(
            system::WAF_NAME,
            &pp(),
            &Patch::Apply(system::waf_backend()),
        )
        .await?;
    // Origin pull and PROXY protocol stay as set; visitors are placed by
    // their connection (an older install's X-Forwarded-For goes).
    write_web(client, web_settings(client).await?).await
}

/// The websites' settings as they are.
async fn web_settings(client: &Client) -> anyhow::Result<system::WebSettings> {
    let https = api_of(client, "ClientTrafficPolicy")
        .get_opt("pyo-https")
        .await?;
    let spec = https
        .as_ref()
        .map(|p| p.data["spec"].clone())
        .unwrap_or_default();
    Ok(system::WebSettings {
        origin_pull: spec["tls"]["clientValidation"].is_object(),
        proxy_protocol: spec["proxyProtocol"].is_object(),
    })
}

async fn write_web(client: &Client, w: system::WebSettings) -> anyhow::Result<()> {
    let policies = api_of(client, "ClientTrafficPolicy");
    policies
        .patch(
            "pyo-http",
            &pp(),
            &Patch::Apply(system::client_policy(
                false,
                system::WebSettings {
                    origin_pull: false,
                    ..w
                },
            )),
        )
        .await?;
    policies
        .patch(
            "pyo-https",
            &pp(),
            &Patch::Apply(system::client_policy(true, w)),
        )
        .await?;
    Ok(())
}

/// Read the visitor's address from a PROXY protocol header on ports 80 and
/// 443 (optional: plain connections still work), or stop.
pub async fn set_web_proxy_protocol(client: &Client, on: bool) -> anyhow::Result<String> {
    if !installed(client).await {
        anyhow::bail!("The gateway isn't installed on this cluster yet. Try again in a minute.");
    }
    let w = web_settings(client).await?;
    write_web(
        client,
        system::WebSettings {
            proxy_protocol: on,
            ..w
        },
    )
    .await?;
    Ok(if on {
        "Websites now accept PROXY protocol from a load balancer in front".into()
    } else {
        "Websites no longer read PROXY protocol".into()
    })
}

/// Require a client certificate from these CAs on this cluster's HTTPS, or
/// stop requiring one when there are none. The CAs go in first and leave
/// last, so the policy never points at nothing.
pub async fn set_origin_pull(client: &Client, ca_pem: &str) -> anyhow::Result<String> {
    if !installed(client).await {
        anyhow::bail!("The gateway isn't installed on this cluster yet. Try again in a minute.");
    }
    let maps: Api<k8s_openapi::api::core::v1::ConfigMap> =
        Api::namespaced(client.clone(), GATEWAY_NS);
    let policies = api_of(client, "ClientTrafficPolicy");
    let w = web_settings(client).await?;
    if ca_pem.is_empty() {
        policies
            .patch(
                "pyo-https",
                &pp(),
                &Patch::Apply(system::client_policy(
                    true,
                    system::WebSettings {
                        origin_pull: false,
                        ..w
                    },
                )),
            )
            .await?;
        match maps
            .delete(system::ORIGIN_PULL_SECRET, &DeleteParams::default())
            .await
        {
            Err(kube::Error::Api(e)) if e.code != 404 => return Err(kube::Error::Api(e).into()),
            Err(e) if !matches!(e, kube::Error::Api(_)) => return Err(e.into()),
            _ => {}
        }
        return Ok("HTTPS no longer requires a client certificate".into());
    }
    let map = json!({
        "apiVersion": "v1", "kind": "ConfigMap",
        "metadata": { "name": system::ORIGIN_PULL_SECRET, "namespace": GATEWAY_NS, "labels": { "app.kubernetes.io/managed-by": MANAGER } },
        "data": { "ca.crt": ca_pem },
    });
    maps.patch(system::ORIGIN_PULL_SECRET, &pp(), &Patch::Apply(&map))
        .await?;
    policies
        .patch(
            "pyo-https",
            &pp(),
            &Patch::Apply(system::client_policy(
                true,
                system::WebSettings {
                    origin_pull: true,
                    ..w
                },
            )),
        )
        .await?;
    Ok("HTTPS now requires your CDN's client certificate".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use pyo_proto::{PortSpec, ServiceRef};

    /// Websites on the Tailscale addresses: every server's that runs a
    /// proxy (not build-only ones), what locate.rs found first, else a node
    /// address on the tailnet; written only when it differs, removed when
    /// the free domains don't point there (or no server has one).
    #[test]
    fn tailnet_websites_follow_the_servers_and_the_mode() {
        let node = |name: &str, labels: Value, annotations: Value, addresses: Value| {
            serde_json::from_value::<k8s_openapi::api::core::v1::Node>(json!({
                "metadata": { "name": name, "labels": labels, "annotations": annotations },
                "status": { "addresses": addresses },
            }))
            .unwrap()
        };
        let nodes = [
            node(
                "a",
                json!({}),
                json!({ "pyo/tailnet-ipv4": "100.101.67.2" }),
                json!([{ "type": "InternalIP", "address": "10.3.8.13" }]),
            ),
            // Joined over Tailscale, not located yet.
            node(
                "b",
                json!({}),
                json!({}),
                json!([{ "type": "InternalIP", "address": "100.64.0.7" }]),
            ),
            // Build-only: no proxy there.
            node(
                "c",
                json!({ "pyo/builder": "true" }),
                json!({ "pyo/tailnet-ipv4": "100.64.0.8" }),
                json!([]),
            ),
            // Not on a tailnet; an annotation that isn't a tailnet address.
            node(
                "d",
                json!({}),
                json!({ "pyo/tailnet-ipv4": "" }),
                json!([{ "type": "InternalIP", "address": "192.168.1.4" }]),
            ),
            node(
                "e",
                json!({}),
                json!({ "pyo/tailnet-ipv4": "8.8.8.8" }),
                json!([]),
            ),
            node(
                "f",
                json!({}),
                json!({ "pyo/tailnet-ipv4": "100.101.67.2" }),
                json!([]),
            ),
        ];
        let ips = tailnet_web_ips(&nodes);
        assert_eq!(ips, ["100.101.67.2", "100.64.0.7"]);
        let current = |ips: &[&str]| {
            Some((
                ips.iter().map(|s| s.to_string()).collect(),
                system::tailnet_web_ports(),
            ))
        };
        // Tailscale: written, then left alone.
        assert_eq!(
            tailnet_web_change(true, ips.clone(), None),
            Some(Some(ips.clone()))
        );
        assert_eq!(
            tailnet_web_change(true, ips.clone(), current(&["100.101.67.2", "100.64.0.7"])),
            None
        );
        // A server more, or its ports changed by hand: written again.
        assert_eq!(
            tailnet_web_change(true, ips.clone(), current(&["100.101.67.2"])),
            Some(Some(ips.clone()))
        );
        assert_eq!(
            tailnet_web_change(
                true,
                ips.clone(),
                Some((ips.clone(), json!([{ "port": 80 }])))
            ),
            Some(Some(ips.clone()))
        );
        // Public IP, or no Tailscale address at all: removed (if there).
        assert_eq!(
            tailnet_web_change(false, vec![], current(&["100.101.67.2"])),
            Some(None)
        );
        assert_eq!(
            tailnet_web_change(true, vec![], current(&["100.101.67.2"])),
            Some(None)
        );
        assert_eq!(tailnet_web_change(false, vec![], None), None);
        assert_eq!(tailnet_web_change(true, vec![], None), None);
    }

    #[test]
    fn a_failed_chart_points_at_its_log() {
        assert_eq!(chart_failed(false, 2), None);
        let e = chart_failed(false, 3).unwrap();
        assert_eq!(
            e,
            "Gateway failed to install. See `sudo k3s kubectl -n kube-system logs job/helm-install-envoy-gateway`."
        );
        assert_eq!(chart_failed(true, 0), Some(e));
    }

    #[test]
    fn an_installed_chart_without_its_apis_says_which() {
        assert_eq!(missing_apis(true, true), None);
        assert!(
            missing_apis(false, true)
                .unwrap()
                .contains("no Gateway API")
        );
        assert!(
            missing_apis(true, false)
                .unwrap()
                .contains("Envoy Gateway's CRDs")
        );
    }

    fn spec(domains: Vec<DomainSpec>) -> DeploySpec {
        let mut s: DeploySpec = serde_json::from_value(json!({
            "deploymentId": "dpl_1", "service": { "serviceId": "svc_web", "name": "web", "namespace": "shop", "workloadKind": "Deployment" },
            "projectId": "prj_1", "image": "nginx", "replicas": 1, "env": [], "ports": [], "domains": [],
            "resources": { "cpuRequest": 100, "cpuLimit": 500, "memoryRequest": 128, "memoryLimit": 256 },
            "healthCheck": { "type": "none", "path": "", "port": 0, "command": "", "initialDelaySeconds": 1, "periodSeconds": 1 },
            "volumes": [], "timeoutSeconds": 60,
        }))
        .unwrap();
        s.service = ServiceRef {
            service_id: "svc_web".into(),
            name: "web".into(),
            namespace: "shop".into(),
            workload_kind: "Deployment".into(),
        };
        s.ports = vec![
            PortSpec {
                name: "http".into(),
                container_port: 8080,
                protocol: "HTTP".into(),
                exposure: "http".into(),
                public_port: None,
                proxy: None,
            },
            PortSpec {
                name: "admin".into(),
                container_port: 9000,
                protocol: "HTTP".into(),
                exposure: "http".into(),
                public_port: None,
                proxy: None,
            },
        ];
        s.domains = domains;
        s
    }

    fn domain(host: &str, port: &str, tls: &str, redirect: bool) -> DomainSpec {
        DomainSpec {
            hostname: host.into(),
            port_name: port.into(),
            tls: tls.into(),
            certificate_id: if tls == "custom" {
                "crt_1".into()
            } else {
                String::new()
            },
            redirect_https: redirect,
        }
    }

    #[test]
    fn the_newest_domains_win_whoever_applies_last() {
        let state = RouteState::default();
        let hosts = |s: &DeploySpec| {
            s.domains
                .iter()
                .map(|d| d.hostname.clone())
                .collect::<Vec<_>>()
        };
        // A deploy handed out with revision 3.
        let mut deploy = spec(vec![domain("a.example.com", "http", "", false)]);
        deploy.routes_revision = 3;
        // A domain added since (revision 4) is synced first.
        let mut sync = spec(vec![
            domain("a.example.com", "http", "", false),
            domain("b.example.com", "http", "", false),
        ]);
        sync.routes_revision = 4;
        state.settle(&mut sync);
        assert_eq!(hosts(&sync), ["a.example.com", "b.example.com"]);
        // The deploy (or its rollback) applying after it keeps b.
        state.settle(&mut deploy);
        assert_eq!(
            (hosts(&deploy), deploy.routes_revision),
            (
                vec!["a.example.com".to_owned(), "b.example.com".to_owned()],
                4
            )
        );
        // Removing both (revision 5) wins over anything older.
        let mut removed = spec(vec![]);
        removed.routes_revision = 5;
        state.settle(&mut removed);
        let mut stale = spec(vec![domain("a.example.com", "http", "", false)]);
        stale.routes_revision = 4;
        state.settle(&mut stale);
        assert!(stale.domains.is_empty());
        // Another service's are its own.
        let mut other = spec(vec![domain("c.example.com", "http", "", false)]);
        other.service.service_id = "svc_api".into();
        state.settle(&mut other);
        assert_eq!(hosts(&other), ["c.example.com"]);
    }

    fn find<'a>(r: &'a Routes, kind: &str, name: &str) -> &'a Value {
        r.objects
            .iter()
            .find(|o| o["kind"] == kind && o["metadata"]["name"] == name)
            .unwrap_or_else(|| {
                panic!(
                    "no {kind} {name}: {:?}",
                    r.objects
                        .iter()
                        .map(|o| format!("{} {}", o["kind"], o["metadata"]["name"]))
                        .collect::<Vec<_>>()
                )
            })
    }

    #[test]
    fn no_domains_no_routes() {
        assert!(route_objects(&spec(vec![])).objects.is_empty());
    }

    #[test]
    fn https_with_redirect() {
        let r = route_objects(&spec(vec![
            domain("shop.example.com", "http", "letsencrypt", true),
            domain("www.shop.example.com", "http", "letsencrypt", true),
        ]));
        let backend = find(&r, "Backend", "shop-web-http");
        assert_eq!(
            backend["spec"]["endpoints"][0]["fqdn"],
            json!({ "hostname": "web.shop.svc.cluster.local", "port": 8080 }),
            "through the Service"
        );
        let gw = find(&r, "Gateway", "shop-web");
        assert_eq!(gw["metadata"]["namespace"], "pyo-gateway");
        assert_eq!(gw["metadata"]["labels"]["pyo.dev/gateway"], "https");
        let listeners = gw["spec"]["listeners"].as_array().unwrap();
        assert_eq!(listeners.len(), 2, "a listener per domain");
        assert_eq!(listeners[0]["hostname"], "shop.example.com");
        assert_eq!(listeners[0]["port"], 443);
        assert_eq!(
            listeners[0]["tls"]["certificateRefs"][0]["name"],
            tls_secret(&spec(vec![]), "shop.example.com")
        );
        assert!(listeners[0]["name"].as_str().unwrap().starts_with("h-"));
        let cert = find(
            &r,
            "Certificate",
            &tls_secret(&spec(vec![]), "shop.example.com"),
        );
        assert_eq!(cert["spec"]["issuerRef"]["name"], "pyo-letsencrypt");
        assert_eq!(cert["spec"]["dnsNames"], json!(["shop.example.com"]));
        let https = find(&r, "HTTPRoute", "shop-web-https-http");
        assert_eq!(https["spec"]["parentRefs"][0]["name"], "shop-web");
        assert_eq!(
            https["spec"]["rules"][0]["backendRefs"][0]["kind"],
            "Backend"
        );
        let redirect = find(&r, "HTTPRoute", "shop-web-redirect");
        assert_eq!(
            redirect["spec"]["parentRefs"][0],
            json!({ "name": "pyo", "sectionName": "http" })
        );
        assert_eq!(
            redirect["spec"]["rules"][0]["filters"][0]["requestRedirect"],
            json!({ "scheme": "https", "statusCode": 301 })
        );
        assert!(
            !r.objects
                .iter()
                .any(|o| o["metadata"]["name"] == "shop-web-http-http"),
            "no plain HTTP route"
        );
        assert_eq!(r.secrets.len(), 2);
        assert!(
            r.objects
                .iter()
                .all(|o| o["metadata"]["namespace"] == "pyo-gateway"
                    && o["metadata"]["labels"]["pyo/service-id"] == "svc_web")
        );
    }

    #[test]
    fn plain_http_custom_and_private() {
        let r = route_objects(&spec(vec![
            domain("plain.example.com", "http", "", false),
            domain("own.example.com", "http", "custom", false),
            domain("admin.shop.localhost", "admin", "selfsigned", true),
        ]));
        let plain = find(&r, "HTTPRoute", "shop-web-http-http");
        assert_eq!(
            plain["spec"]["hostnames"],
            json!(["plain.example.com", "own.example.com"]),
            "plain, and HTTPS without the redirect, answer on :80"
        );
        assert_eq!(
            r.uploaded,
            vec![(
                tls_secret(&spec(vec![]), "own.example.com"),
                "crt_1".to_owned()
            )]
        );
        assert!(
            !r.objects.iter().any(|o| o["kind"] == "Certificate"
                && o["metadata"]["name"] == tls_secret(&spec(vec![]), "own.example.com")),
            "uploaded: no Certificate"
        );
        assert_eq!(
            find(&r, "Backend", "shop-web-admin")["spec"]["endpoints"][0]["fqdn"]["port"],
            9000
        );
        assert_eq!(
            find(
                &r,
                "Certificate",
                &tls_secret(&spec(vec![]), "admin.shop.localhost")
            )["spec"]["issuerRef"]["name"],
            "pyo-selfsigned"
        );
        assert_eq!(
            find(&r, "HTTPRoute", "shop-web-redirect")["spec"]["hostnames"],
            json!(["admin.shop.localhost"])
        );
        assert_eq!(
            find(&r, "HTTPRoute", "shop-web-https-admin")["spec"]["hostnames"],
            json!(["admin.shop.localhost"])
        );
    }

    #[test]
    fn proxied_ports() {
        use pyo_proto::{IpFilter, PortProxy};
        let mut s = spec(vec![]);
        let port = |name: &str, n: i32, protocol: &str, proxy: Option<PortProxy>| PortSpec {
            name: name.into(),
            container_port: n,
            protocol: protocol.into(),
            exposure: "loadbalancer".into(),
            public_port: Some(n),
            proxy,
        };
        s.ports = vec![
            port(
                "redis",
                6379,
                "TCP",
                Some(PortProxy {
                    ip_filter: Some(IpFilter {
                        mode: "deny".into(),
                        cidrs: vec!["203.0.113.0/24".into()],
                    }),
                    connection_limit: Some(100),
                    send_proxy_protocol: "V2".into(),
                    accept_proxy_protocol: true,
                }),
            ),
            port("dns", 53, "UDP", Some(PortProxy::default())),
            port("game", 27015, "UDP", None),
        ];
        let r = route_objects(&s);
        let gw = find(&r, "Gateway", "shop-web-ports");
        assert_eq!(gw["metadata"]["labels"]["pyo.dev/gateway"], "ports");
        assert_eq!(
            gw["spec"]["listeners"],
            json!([
                { "name": "tcp-6379", "port": 6379, "protocol": "TCP", "allowedRoutes": { "namespaces": { "from": "Same" } } },
                { "name": "udp-53", "port": 53, "protocol": "UDP", "allowedRoutes": { "namespaces": { "from": "Same" } } },
            ]),
            "the direct port isn't on the proxy"
        );
        assert_eq!(
            find(&r, "Backend", "shop-web-redis")["spec"]["endpoints"][0]["fqdn"],
            json!({ "hostname": "web.shop.svc.cluster.local", "port": 6379 })
        );
        let tcp = find(&r, "TCPRoute", "shop-web-tcp-6379");
        assert_eq!(
            tcp["spec"]["parentRefs"],
            json!([{ "name": "shop-web-ports", "sectionName": "tcp-6379" }])
        );
        assert_eq!(
            tcp["spec"]["rules"][0]["backendRefs"][0]["name"],
            "shop-web-redis"
        );
        assert_eq!(
            find(&r, "UDPRoute", "shop-web-udp-53")["spec"]["parentRefs"][0]["sectionName"],
            "udp-53"
        );
        let sp = find(&r, "SecurityPolicy", "shop-web-tcp-6379");
        assert_eq!(
            sp["spec"]["authorization"],
            json!({ "defaultAction": "Allow", "rules": [{ "name": "port", "action": "Deny", "principal": { "clientCIDRs": ["203.0.113.0/24"] } }] })
        );
        let ctp = find(&r, "ClientTrafficPolicy", "shop-web-tcp-6379");
        assert_eq!(ctp["spec"]["targetRefs"][0]["sectionName"], "tcp-6379");
        assert_eq!(
            ctp["spec"]["connection"],
            json!({ "connectionLimit": { "value": 100 } })
        );
        assert_eq!(ctp["spec"]["proxyProtocol"], json!({ "optional": true }));
        assert_eq!(
            find(&r, "BackendTrafficPolicy", "shop-web-tcp-6379")["spec"]["proxyProtocol"],
            json!({ "version": "V2" })
        );
        assert!(
            !r.objects.iter().any(
                |o| o["metadata"]["name"].as_str().unwrap().contains("udp-53")
                    && o["kind"].as_str().unwrap().ends_with("Policy")
            ),
            "nothing for UDP"
        );
        assert!(
            !r.objects
                .iter()
                .any(|o| o["metadata"]["name"].as_str().unwrap().contains("27015"))
        );
    }

    #[test]
    fn firewall_on_routes() {
        use pyo_proto::firewall::FirewallRule;
        let rule = |id: &str, action: &str, source: &str| FirewallRule {
            id: id.into(),
            action: action.into(),
            operator: "in".into(),
            sources: vec![source.into()],
            limit: None,
        };
        let mut s = spec(vec![domain(
            "shop.example.com",
            "http",
            "letsencrypt",
            true,
        )]);
        s.ports.push(PortSpec {
            name: "dns".into(),
            container_port: 53,
            protocol: "UDP".into(),
            exposure: "loadbalancer".into(),
            public_port: Some(5353),
            proxy: Some(Default::default()),
        });
        s.ports.push(PortSpec {
            name: "db".into(),
            container_port: 5432,
            protocol: "TCP".into(),
            exposure: "loadbalancer".into(),
            public_port: Some(5432),
            proxy: Some(Default::default()),
        });
        let limit = FirewallRule {
            id: "fwr_rl".into(),
            action: "rateLimit".into(),
            operator: "in".into(),
            sources: vec![],
            limit: Some(pyo_proto::firewall::RateLimit {
                requests: 120,
                per: "minute".into(),
            }),
        };
        s.firewall = Some(Firewall {
            rules: vec![
                rule("fwr_ru", "block", "RU"),
                rule("fwr_bad", "block", "203.0.113.0/24"),
                limit,
            ],
        });
        let r = route_objects(&s);
        let sp = find(&r, "SecurityPolicy", "shop-web-https-http");
        assert_eq!(sp["spec"]["targetRefs"][0]["kind"], "HTTPRoute");
        assert_eq!(
            sp["spec"]["authorization"]["rules"]
                .as_array()
                .unwrap()
                .len(),
            2,
            "the country and the range"
        );
        assert!(
            !r.objects
                .iter()
                .any(|o| o["kind"] == "SecurityPolicy"
                    && o["metadata"]["name"] == "shop-web-redirect"),
            "the redirect to HTTPS stays open"
        );
        assert_eq!(
            find(&r, "BackendTrafficPolicy", "shop-web-https-http")["spec"]["rateLimit"],
            pyo_proto::firewall::rate_limit(s.firewall.as_ref().unwrap()).unwrap(),
            "the service's limit"
        );
        // A refused visitor gets a short message, not Envoy's.
        assert_eq!(
            find(&r, "BackendTrafficPolicy", "shop-web-https-http")["spec"]["responseOverride"],
            system::firewall_response_override()
        );
        assert!(
            !r.objects.iter().any(|o| o["kind"] == "BackendTrafficPolicy"
                && o["metadata"]["name"] == "shop-web-tcp-5432"),
            "websites only"
        );
        // Every policy is named after the one route it targets (the
        // admission policy holds it to that).
        for o in r
            .objects
            .iter()
            .filter(|o| o["kind"] == "SecurityPolicy" || o["kind"] == "EnvoyExtensionPolicy")
        {
            assert_eq!(o["spec"]["targetRefs"].as_array().unwrap().len(), 1);
            assert_eq!(o["spec"]["targetRefs"][0]["name"], o["metadata"]["name"]);
        }
        let tcp = find(&r, "SecurityPolicy", "shop-web-tcp-5432");
        assert_eq!(
            tcp["spec"]["authorization"]["rules"]
                .as_array()
                .unwrap()
                .len(),
            1,
            "TCP: the range, not the country"
        );
        let udp = find(&r, "UDPRoute", "shop-web-udp-5353");
        assert_eq!(udp["metadata"]["annotations"][LISTEN_ANNOTATION], "5353");
        let blocks: Vec<(String, Vec<String>)> = serde_json::from_str(
            udp["metadata"]["annotations"][FIREWALL_ANNOTATION]
                .as_str()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(
            blocks[0],
            ("0.0.0.0/0".to_owned(), vec!["203.0.113.0/24".to_owned()])
        );
        // Access rules alone: the message, no rate limit.
        s.firewall = Some(Firewall {
            rules: vec![rule("fwr_bad", "block", "203.0.113.0/24")],
        });
        let r = route_objects(&s);
        let btp = find(&r, "BackendTrafficPolicy", "shop-web-https-http");
        assert!(
            btp["spec"].get("rateLimit").is_none()
                && btp["spec"]["responseOverride"] == system::firewall_response_override()
        );
        // No firewall: no policies.
        s.firewall = None;
        let r = route_objects(&s);
        assert!(!r.objects.iter().any(|o| o["kind"] == "SecurityPolicy"
            || (o["kind"] == "BackendTrafficPolicy"
                && !o["metadata"]["name"].as_str().unwrap().contains("tcp"))));
    }

    #[test]
    fn waf_on_routes() {
        use pyo_proto::{Waf, WafCondition, WafRule, WafSettings};
        use system::WafRoute;
        let d = |mode: &str, level: u8, ex: Vec<u32>| WafSettings {
            mode: mode.into(),
            level,
            exceptions: ex,
            ..Default::default()
        };
        let rule = |id: u32| WafRule {
            id,
            name: format!("r{id}"),
            matching: "all".into(),
            conditions: vec![WafCondition {
                field: "path".into(),
                op: "startsWith".into(),
                values: vec!["/wp-admin".into()],
                ..Default::default()
            }],
            action: "block".into(),
        };
        let mut s = spec(vec![
            domain("shop.example.com", "http", "letsencrypt", true),
            domain("www.shop.example.com", "http", "letsencrypt", true),
            domain("admin.example.com", "admin", "letsencrypt", false),
        ]);
        s.waf = Some(Waf {
            settings: d("block", 2, vec![942100]),
            rules: vec![rule(1_000_001)],
        });
        let r = route_objects(&s);
        let eep = find(&r, "EnvoyExtensionPolicy", "shop-web-https-http");
        assert_eq!(eep["spec"]["targetRefs"][0]["kind"], "HTTPRoute");
        assert_eq!(eep["spec"]["extProc"][0], system::waf_ext_proc(true));
        assert_eq!(
            eep["spec"]["extProc"][0]["failOpen"], false,
            "block mode: refused when the WAF is unavailable"
        );
        let on: WafRoute = serde_json::from_str(
            eep["metadata"]["annotations"][WAF_ANNOTATION]
                .as_str()
                .unwrap(),
        )
        .unwrap();
        assert!(
            on.hosts.contains(&"shop.example.com".to_owned())
                && on.settings.mode == "block"
                && on.rules.len() == 1
        );
        assert!(
            eep["metadata"]["annotations"][WAF_ANNOTATION]
                .as_str()
                .unwrap()
                .contains(system::WAF_BLOCK_MARK)
        );
        // Log only: passed uninspected when the WAF is unavailable.
        s.waf = Some(Waf {
            settings: d("log", 2, vec![]),
            rules: vec![rule(1_000_001)],
        });
        let r = route_objects(&s);
        let eep = find(&r, "EnvoyExtensionPolicy", "shop-web-https-http");
        assert_eq!(eep["spec"]["extProc"][0], system::waf_ext_proc(false));
        assert_eq!(eep["spec"]["extProc"][0]["failOpen"], true);
        assert!(
            !eep["metadata"]["annotations"][WAF_ANNOTATION]
                .as_str()
                .unwrap()
                .contains(system::WAF_BLOCK_MARK)
        );
        assert!(
            r.objects.iter().any(|o| o["kind"] == "EnvoyExtensionPolicy"
                && o["metadata"]["name"].as_str().unwrap().contains("admin")),
            "every domain of the service"
        );
        assert!(
            !r.objects.iter().any(|o| o["kind"] == "EnvoyExtensionPolicy"
                && o["metadata"]["name"] == "shop-web-redirect")
        );
        // Off (custom rules or not), or no WAF: no policies, so the service's
        // routes never wait on the engine.
        s.waf = Some(Waf {
            settings: d("off", 1, vec![]),
            rules: vec![rule(1_000_001)],
        });
        assert!(
            !route_objects(&s)
                .objects
                .iter()
                .any(|o| o["kind"] == "EnvoyExtensionPolicy")
        );
        s.waf = Some(Waf {
            settings: d("off", 1, vec![]),
            ..Default::default()
        });
        assert!(
            !route_objects(&s)
                .objects
                .iter()
                .any(|o| o["kind"] == "EnvoyExtensionPolicy")
        );
        s.waf = None;
        assert!(
            !route_objects(&s)
                .objects
                .iter()
                .any(|o| o["kind"] == "EnvoyExtensionPolicy")
        );
        // Its settings: each distinct combination once.
        let route = |h: &str, set: WafSettings, rules: Vec<WafRule>| WafRoute {
            hosts: vec![h.to_owned()],
            settings: set,
            rules,
        };
        let cfg = system::waf_config(&[
            route(
                "a.example.com",
                d("block", 1, vec![]),
                vec![rule(1_000_001)],
            ),
            route(
                "b.example.com",
                d("block", 1, vec![]),
                vec![rule(1_000_001)],
            ),
            route("c.example.com", d("log", 3, vec![941100]), vec![]),
            route("d.example.com", d("off", 1, vec![]), vec![rule(1_000_002)]),
        ]);
        assert_eq!(cfg["sets"].as_object().unwrap().len(), 2);
        assert_eq!(cfg["hosts"]["a.example.com"], cfg["hosts"]["b.example.com"]);
        assert_eq!(
            cfg["sets"][cfg["hosts"]["c.example.com"].as_str().unwrap()],
            json!({ "engine": "DetectionOnly", "level": 3, "exceptions": [941100], "threshold": 0 })
        );
        assert!(
            cfg["hosts"].get("d.example.com").is_none(),
            "no OWASP rules"
        );
        assert_eq!(cfg["lists"].as_object().unwrap().len(), 2);
        assert_eq!(
            cfg["hostRules"]["a.example.com"],
            cfg["hostRules"]["b.example.com"]
        );
        assert_eq!(
            cfg["lists"][cfg["hostRules"]["d.example.com"].as_str().unwrap()][0]["id"],
            1_000_002
        );
        assert!(cfg["hostRules"].get("c.example.com").is_none());
        // The threshold is part of the set.
        let mut b = d("block", 1, vec![]);
        b.threshold = 20;
        let cfg = system::waf_config(&[
            route("b.example.com", b, vec![]),
            route("c.example.com", d("block", 1, vec![]), vec![]),
        ]);
        assert_ne!(cfg["hosts"]["b.example.com"], cfg["hosts"]["c.example.com"]);
        assert_eq!(
            cfg["sets"][cfg["hosts"]["b.example.com"].as_str().unwrap()]["threshold"],
            20
        );
        // One copy per proxy server, and only ever the local one.
        let ds = system::waf_daemonset("img", "g");
        assert_eq!(
            ds["spec"]["template"]["spec"]["affinity"],
            system::envoy_proxy("g", true)["spec"]["provider"]["kubernetes"]["envoyDaemonSet"]["pod"]
                ["affinity"]
        );
        assert_eq!(
            ds["spec"]["updateStrategy"]["rollingUpdate"],
            json!({ "maxSurge": 1, "maxUnavailable": 0 })
        );
        assert_eq!(
            system::waf_service()["spec"]["internalTrafficPolicy"],
            "Local"
        );
    }

    #[test]
    fn public_traffic_places_and_publishes_the_proxies() {
        // What the API last said wins; before it has, the proxies stay as
        // they are; a new cluster's are published.
        assert!(!wanted_public(Some(false), Some(true)));
        assert!(wanted_public(Some(true), Some(false)));
        assert!(!wanted_public(None, Some(false)));
        assert!(wanted_public(None, None));
        // On: a LoadBalancer Service, so ServiceLB publishes them; off: only
        // inside the cluster. Either way, on every server but build-only ones.
        for on in [true, false] {
            let p = system::envoy_proxy("g", on);
            let k = &p["spec"]["provider"]["kubernetes"];
            assert_eq!(
                k["envoyService"]["type"],
                if on { "LoadBalancer" } else { "ClusterIP" }
            );
            assert_eq!(system::envoy_proxy_public(&p["spec"]), on);
            assert_eq!(
                k["envoyDaemonSet"]["pod"]["affinity"]["nodeAffinity"]["requiredDuringSchedulingIgnoredDuringExecution"]
                    ["nodeSelectorTerms"],
                json!([{ "matchExpressions": [{ "key": pyo_proto::BUILDER_NODE_LABEL, "operator": "NotIn", "values": ["true"] }] }])
            );
            // GPU servers kept for GPU work still run one.
            assert_eq!(
                k["envoyDaemonSet"]["pod"]["tolerations"],
                json!([pyo_proto::gpu_toleration()])
            );
        }
    }

    #[test]
    fn a_domain_on_a_missing_port_is_skipped() {
        let r = route_objects(&spec(vec![domain(
            "x.example.com",
            "nope",
            "letsencrypt",
            true,
        )]));
        assert!(r.objects.is_empty());
    }

    #[test]
    fn names_stay_dns_safe_and_stable() {
        assert_eq!(host_hash("shop.example.com"), host_hash("shop.example.com"));
        assert_ne!(host_hash("a.example.com"), host_hash("b.example.com"));
        let s = tls_secret(&spec(vec![]), "shop.example.com");
        assert!(s.starts_with("pyo-tls-shop-web-") && s.len() <= 63, "{s}");
    }

    /// Garbage collection takes a service's objects only when nothing is
    /// behind them: no workload, no rollout under way, and none of them new.
    #[test]
    fn orphans_have_no_workload_no_rollout_and_are_not_new() {
        let now = 1_000_000;
        let found: BTreeMap<String, i64> = [
            ("svc_live", now - 3600),
            ("svc_gone", now - 3600),
            ("svc_deploying", now - 3600),
            ("svc_new", now - 60),
            ("svc_edge", now - ORPHAN_AFTER_SECS),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_owned(), v))
        .collect();
        let live = ["svc_live".to_owned()].into();
        let gone = orphans(
            &found,
            &live,
            |id| id == "svc_deploying",
            now,
            ORPHAN_AFTER_SECS,
        );
        assert_eq!(gone, ["svc_edge", "svc_gone"]);
        // An object without a creation time never counts as old.
        let found: BTreeMap<String, i64> = [("svc_x".to_owned(), i64::MAX)].into();
        assert!(orphans(&found, &Default::default(), |_| false, now, 0).is_empty());
    }

    #[test]
    fn deploying_lasts_as_long_as_its_rollouts() {
        let d = crate::Deploying::default();
        let a = d.start("svc_1");
        let b = d.start("svc_1");
        assert!(d.contains("svc_1") && !d.contains("svc_2"));
        drop(a);
        assert!(d.contains("svc_1"), "a second rollout still runs");
        drop(b);
        assert!(!d.contains("svc_1"));
    }

    /// Against a throwaway cluster (1.30+) with the Gateway API's and Envoy
    /// Gateway's CRDs, as the agent's own ServiceAccount: a service's
    /// firewall and WAF policies are admitted on the route they're named
    /// after, and refused on another route or (a WAF policy) without the
    /// service's WAF.
    ///
    /// PYO_TEST_KUBECONFIG=<throwaway kubeconfig> cargo test -p pyo-agent -- --ignored service_policies_against
    #[tokio::test]
    #[ignore = "needs a throwaway Kubernetes API server"]
    async fn service_policies_against_a_cluster() {
        use k8s_openapi::api::{
            authentication::v1::TokenRequest,
            core::v1::{Namespace, ServiceAccount},
        };
        use kube::api::PostParams;
        use pyo_proto::{Waf, WafSettings, firewall::FirewallRule, rbac};
        let Ok(path) = std::env::var("PYO_TEST_KUBECONFIG") else {
            panic!("set PYO_TEST_KUBECONFIG to a throwaway cluster's kubeconfig")
        };
        let kc = kube::config::Kubeconfig::read_from(&path).unwrap();
        let config = kube::Config::from_custom_kubeconfig(kc, &Default::default())
            .await
            .unwrap();
        let admin = Client::try_from(config.clone()).unwrap();
        let _ = Api::<Namespace>::all(admin.clone())
            .create(
                &PostParams::default(),
                &serde_json::from_value(json!({ "metadata": { "name": GATEWAY_NS } })).unwrap(),
            )
            .await;
        let db = system::DbImages {
            controller: "c".into(),
            kv: "k".into(),
            postgres: ["p16".into(), "p17".into(), "p18".into()],
            mysql_operator: "mo".into(),
            mysql_backup: "mb".into(),
        };
        let install = rbac::InstallParts::new(
            false,
            true,
            system::WAF_IMAGE_DEFAULT,
            system::GEOIP_IMAGE_DEFAULT,
            &db,
        );
        let pp = PatchParams::apply("pyo-test").force();
        for o in install
            .access
            .iter()
            .chain(install.admission.iter())
            .filter(|o| o["kind"] != "HelmChart")
        {
            let r = pyo_proto::bundles::ObjectRef::of(o);
            crate::bundles::api_for(&admin, &r)
                .patch(&r.name, &pp, &Patch::Apply(o))
                .await
                .unwrap_or_else(|e| panic!("{}: {e}", r.name));
        }
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        let token = Api::<ServiceAccount>::namespaced(admin.clone(), rbac::NS)
            .create_token_request(
                rbac::SA,
                &PostParams::default(),
                &serde_json::from_value::<TokenRequest>(
                    json!({ "spec": { "audiences": [], "expirationSeconds": 3600 } }),
                )
                .unwrap(),
            )
            .await
            .unwrap()
            .status
            .unwrap()
            .token
            .unwrap();
        let mut sa_config = config.clone();
        sa_config.auth_info = kube::config::AuthInfo {
            token: Some(token.into()),
            ..Default::default()
        };
        let sa = Client::try_from(sa_config).unwrap();

        let mut s = spec(vec![domain("shop.example.com", "http", "", false)]);
        let limit = |id: &str, operator: &str, sources: &[&str], per: &str| FirewallRule {
            id: id.into(),
            action: "rateLimit".into(),
            operator: operator.into(),
            sources: sources.iter().map(|x| x.to_string()).collect(),
            limit: Some(pyo_proto::firewall::RateLimit {
                requests: 10,
                per: per.into(),
            }),
        };
        s.firewall = Some(Firewall {
            rules: vec![
                FirewallRule {
                    id: "fwr_x".into(),
                    action: "block".into(),
                    operator: "notIn".into(),
                    sources: vec!["203.0.113.0/24".into()],
                    limit: None,
                },
                limit("fwr_a", "in", &[], "minute"),
                limit("fwr_b", "notIn", &["10.0.0.0/8"], "second"),
                limit(
                    "fwr_c",
                    "in",
                    &["198.51.100.0/24", "2001:db8::/32"],
                    "minute",
                ),
            ],
        });
        s.waf = Some(Waf {
            settings: WafSettings {
                mode: "block".into(),
                level: 1,
                ..Default::default()
            },
            rules: vec![],
        });
        let r = route_objects(&s);
        let apply = |o: Value| {
            let sa = sa.clone();
            async move {
                let kind = o["kind"].as_str().unwrap().to_owned();
                let name = o["metadata"]["name"].as_str().unwrap().to_owned();
                api_of(&sa, &kind)
                    .patch(
                        &name,
                        &PatchParams::apply(MANAGER).force(),
                        &Patch::Apply(&o),
                    )
                    .await
            }
        };
        for kind in ["SecurityPolicy", "EnvoyExtensionPolicy"] {
            let own = find(&r, kind, "shop-web-http-http").clone();
            apply(own.clone())
                .await
                .unwrap_or_else(|e| panic!("{kind} on its own route: {e}"));
            // Aimed at another service's route.
            let mut other = own.clone();
            other["spec"]["targetRefs"][0]["name"] = json!("other-web-http-http");
            assert!(apply(other).await.is_err(), "{kind} on another route");
            // Its name, but two routes.
            let mut two = own.clone();
            two["spec"]["targetRefs"] = json!([own["spec"]["targetRefs"][0], { "group": "gateway.networking.k8s.io", "kind": "HTTPRoute", "name": "other-web-http-http" }]);
            assert!(apply(two).await.is_err(), "{kind} on two routes");
            api_of(&sa, kind)
                .delete("shop-web-http-http", &Default::default())
                .await
                .unwrap();
        }
        // Rate limits: the service's, but nothing else in them.
        let btp = find(&r, "BackendTrafficPolicy", "shop-web-http-http").clone();
        apply(btp.clone())
            .await
            .unwrap_or_else(|e| panic!("rate limits: {e}"));
        let mut hourly = btp.clone();
        hourly["spec"]["rateLimit"]["local"]["rules"][0]["limit"]["unit"] = json!("Hour");
        assert!(apply(hourly).await.is_err(), "another unit");
        let mut header = btp.clone();
        header["spec"]["rateLimit"]["local"]["rules"][0]["clientSelectors"] =
            json!([{ "headers": [{ "name": "x", "type": "Distinct" }] }]);
        assert!(apply(header).await.is_err(), "another selector");
        let mut message = btp.clone();
        message["spec"]["responseOverride"][0]["response"]["body"]["inline"] =
            json!("<script>…</script>");
        assert!(apply(message).await.is_err(), "another message");
        let mut upstream = btp.clone();
        upstream["spec"]["responseOverride"][0]["source"] = json!("All");
        assert!(
            apply(upstream).await.is_err(),
            "the service's own responses"
        );
        let mut only_message = btp.clone();
        only_message["spec"]
            .as_object_mut()
            .unwrap()
            .remove("rateLimit");
        apply(only_message.clone())
            .await
            .unwrap_or_else(|e| panic!("the message alone: {e}"));
        only_message["spec"]
            .as_object_mut()
            .unwrap()
            .remove("responseOverride");
        assert!(apply(only_message).await.is_err(), "neither");
        api_of(&sa, "BackendTrafficPolicy")
            .delete("shop-web-http-http", &Default::default())
            .await
            .unwrap();
        let mut bare = find(&r, "EnvoyExtensionPolicy", "shop-web-http-http").clone();
        bare["metadata"]["annotations"] = json!({});
        assert!(
            apply(bare).await.is_err(),
            "a WAF policy without the service's WAF"
        );
    }

    /// The e2e leftovers on a cluster: a deleted service's routes, Backend
    /// and certificate (with its Secret) go; a live service's and a
    /// deploying one's stay. Needs a throwaway cluster with Envoy Gateway's
    /// and cert-manager's CRDs (cluster-admin):
    /// KUBECONFIG=… cargo test -p pyo-agent -- --ignored gateway::tests::collects
    #[tokio::test]
    #[ignore = "needs a throwaway Kubernetes API server"]
    async fn collects_a_deleted_services_leftovers_on_a_cluster() {
        use k8s_openapi::api::{apps::v1::Deployment, core::v1::Namespace};
        use kube::api::PostParams;
        let client = Client::try_default().await.unwrap();
        let _ = Api::<Namespace>::all(client.clone())
            .create(
                &PostParams::default(),
                &serde_json::from_value(json!({ "metadata": { "name": GATEWAY_NS } })).unwrap(),
            )
            .await;
        let _ = Api::<Namespace>::all(client.clone())
            .create(
                &PostParams::default(),
                &serde_json::from_value(json!({ "metadata": { "name": "pyo-t-gc" } })).unwrap(),
            )
            .await;
        let deployment: Deployment = serde_json::from_value(json!({
            "metadata": { "name": "web", "labels": { SERVICE_LABEL: "svc_t_live" } },
            "spec": { "replicas": 0, "selector": { "matchLabels": { SERVICE_LABEL: "svc_t_live" } }, "template": {
                "metadata": { "labels": { SERVICE_LABEL: "svc_t_live" } }, "spec": { "containers": [{ "name": "web", "image": "busybox:musl" }] } } },
        }))
        .unwrap();
        let _ = Api::<Deployment>::namespaced(client.clone(), "pyo-t-gc")
            .create(&PostParams::default(), &deployment)
            .await;
        let secrets: Api<Secret> = Api::namespaced(client.clone(), GATEWAY_NS);
        for id in ["svc_t_live", "svc_t_gone", "svc_t_deploying"] {
            let name = format!("{TLS_SECRET_PREFIX}t-{}", id.replace('_', "-"));
            let meta = json!({ "name": name, "namespace": GATEWAY_NS, "labels": { "app.kubernetes.io/managed-by": MANAGER, SERVICE_LABEL: id } });
            for o in [
                json!({ "apiVersion": "gateway.envoyproxy.io/v1alpha1", "kind": "Backend", "metadata": meta, "spec": { "endpoints": [{ "fqdn": { "hostname": "web.pyo-t-gc.svc.cluster.local", "port": 80 } }] } }),
                json!({ "apiVersion": "gateway.networking.k8s.io/v1", "kind": "HTTPRoute", "metadata": meta, "spec": { "parentRefs": [{ "name": HTTP_GATEWAY }], "rules": [{ "backendRefs": [{ "group": "gateway.envoyproxy.io", "kind": "Backend", "name": name }] }] } }),
                json!({ "apiVersion": "cert-manager.io/v1", "kind": "Certificate", "metadata": meta, "spec": { "secretName": name, "dnsNames": ["t.example.com"], "issuerRef": { "name": "pyo-selfsigned", "kind": "ClusterIssuer" } } }),
            ] {
                let kind = o["kind"].as_str().unwrap().to_owned();
                api_of(&client, &kind)
                    .patch(&name, &pp(), &Patch::Apply(&o))
                    .await
                    .unwrap_or_else(|e| panic!("{kind}: {e}"));
            }
            let secret = json!({ "apiVersion": "v1", "kind": "Secret", "type": "kubernetes.io/tls", "metadata": { "name": name, "namespace": GATEWAY_NS },
                "stringData": { "tls.crt": "", "tls.key": "" } });
            secrets
                .patch(&name, &pp(), &Patch::Apply(&secret))
                .await
                .unwrap();
        }
        let deploying = crate::Deploying::default();
        let _rollout = deploying.start("svc_t_deploying");
        // New objects aren't collected with the usual age.
        assert!(
            collect_older_than(&client, &deploying, ORPHAN_AFTER_SECS)
                .await
                .unwrap()
                .iter()
                .all(|id| !id.starts_with("svc_t_"))
        );
        let gone = collect_older_than(&client, &deploying, 0).await.unwrap();
        assert!(
            gone.contains(&"svc_t_gone".to_owned())
                && !gone
                    .iter()
                    .any(|id| id == "svc_t_live" || id == "svc_t_deploying"),
            "{gone:?}"
        );
        let left = |kind: &str, id: &str| {
            let api = api_of(&client, kind);
            let name = format!("{TLS_SECRET_PREFIX}t-{}", id.replace('_', "-"));
            async move { api.get_opt(&name).await.unwrap().is_some() }
        };
        for kind in ["Backend", "HTTPRoute", "Certificate"] {
            assert!(!left(kind, "svc_t_gone").await, "{kind}");
            assert!(
                left(kind, "svc_t_live").await && left(kind, "svc_t_deploying").await,
                "{kind}"
            );
        }
        assert!(
            secrets
                .get_opt("pyo-tls-t-svc-t-gone")
                .await
                .unwrap()
                .is_none(),
            "its certificate's Secret"
        );
        assert!(
            secrets
                .get_opt("pyo-tls-t-svc-t-live")
                .await
                .unwrap()
                .is_some()
        );
        drop(_rollout);
        // The workload going makes the live one's leftovers too.
        Api::<Deployment>::namespaced(client.clone(), "pyo-t-gc")
            .delete("web", &DeleteParams::default())
            .await
            .unwrap();
        let gone = collect_older_than(&client, &deploying, 0).await.unwrap();
        assert!(
            gone.contains(&"svc_t_live".to_owned()) && gone.contains(&"svc_t_deploying".to_owned()),
            "{gone:?}"
        );
        let _ = Api::<k8s_openapi::api::core::v1::Namespace>::all(client.clone())
            .delete("pyo-t-gc", &DeleteParams::default())
            .await;
    }

    /// A domain switched from a cert-manager certificate to an uploaded
    /// one (the same Secret name): the Certificate and the Secret it owns go
    /// first, so the uploaded Secret isn't taken with them. Needs a
    /// throwaway cluster with cert-manager and Pyo's issuers:
    /// KUBECONFIG=… cargo test -p pyo-agent -- --ignored gateway::tests::retires
    #[tokio::test]
    #[ignore = "needs a throwaway Kubernetes API server"]
    async fn retires_a_certificate_before_an_upload_takes_its_name_on_a_cluster() {
        let client = Client::try_default().await.unwrap();
        let name = format!("{TLS_SECRET_PREFIX}t-upload");
        let cert = json!({ "apiVersion": "cert-manager.io/v1", "kind": "Certificate", "metadata": { "name": name, "namespace": GATEWAY_NS },
            "spec": { "secretName": name, "dnsNames": ["upload.example.com"], "issuerRef": { "name": "pyo-selfsigned", "kind": "ClusterIssuer" } } });
        api_of(&client, "Certificate")
            .patch(&name, &pp(), &Patch::Apply(&cert))
            .await
            .unwrap();
        let secrets: Api<Secret> = Api::namespaced(client.clone(), GATEWAY_NS);
        for _ in 0..60 {
            if secrets
                .get_opt(&name)
                .await
                .unwrap()
                .is_some_and(|s| s.metadata.owner_references.is_some())
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        }
        assert!(
            secrets
                .get_opt(&name)
                .await
                .unwrap()
                .is_some_and(|s| s.metadata.owner_references.is_some()),
            "cert-manager made the Secret, owned"
        );
        retire_certificate(&client, &name).await.unwrap();
        assert!(
            api_of(&client, "Certificate")
                .get_opt(&name)
                .await
                .unwrap()
                .is_none()
        );
        assert!(secrets.get_opt(&name).await.unwrap().is_none());
        let uploaded = json!({ "apiVersion": "v1", "kind": "Secret", "type": "kubernetes.io/tls", "metadata": { "name": name, "namespace": GATEWAY_NS },
            "stringData": { "tls.crt": "", "tls.key": "" } });
        secrets
            .patch(&name, &pp(), &Patch::Apply(&uploaded))
            .await
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
        let s = secrets
            .get_opt(&name)
            .await
            .unwrap()
            .expect("the uploaded Secret stays");
        assert!(s.metadata.owner_references.unwrap_or_default().is_empty());
        // Nothing to retire: fine.
        retire_certificate(&client, "pyo-tls-t-none").await.unwrap();
        let _ = secrets.delete(&name, &DeleteParams::default()).await;
    }

    /// Domains go live in place, end to end, as the agent's own
    /// ServiceAccount (its roles and admission policy) on a throwaway k3s
    /// cluster with Envoy Gateway's CRDs applied (connect.sh's) and its
    /// ports 80 and 443 at PYO_TEST_HTTP and PYO_TEST_HTTPS: the install's
    /// charts (Envoy Gateway, cert-manager) come up, a service deploys with
    /// one domain; a second (HTTPS) is added and removed with `SyncRoutes`
    /// while its pods and an open download carry on; its route, certificate
    /// and Secret go with it; a rollback, and deploys racing syncs, end on
    /// the newest domains.
    ///
    /// PYO_TEST_KUBECONFIG=… PYO_TEST_HTTP=127.0.0.1:18480 PYO_TEST_HTTPS=127.0.0.1:18443 \
    ///   cargo test -p pyo-agent -- --ignored --nocapture gateway::tests::domains_go_live
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "needs a throwaway Kubernetes cluster"]
    async fn domains_go_live_in_place_on_a_cluster() {
        use k8s_openapi::api::{
            apps::v1::Deployment,
            authentication::v1::TokenRequest,
            core::v1::{Pod, ServiceAccount},
        };
        use kube::api::PostParams;
        use pyo_proto::rbac;
        use std::time::Duration;
        let env = |k: &str| std::env::var(k).unwrap_or_else(|_| panic!("set {k}"));
        let (http, https) = (env("PYO_TEST_HTTP"), env("PYO_TEST_HTTPS"));
        let kc = kube::config::Kubeconfig::read_from(env("PYO_TEST_KUBECONFIG")).unwrap();
        let config = kube::Config::from_custom_kubeconfig(kc, &Default::default())
            .await
            .unwrap();
        let admin = Client::try_from(config.clone()).unwrap();
        // The install, as its manifest has it (its charts included).
        let db = system::DbImages {
            controller: "c".into(),
            kv: "k".into(),
            postgres: ["p16".into(), "p17".into(), "p18".into()],
            mysql_operator: "mo".into(),
            mysql_backup: "mb".into(),
        };
        let install = rbac::InstallParts::new(
            false,
            true,
            system::WAF_IMAGE_DEFAULT,
            system::GEOIP_IMAGE_DEFAULT,
            &db,
        );
        for o in install.access.iter().chain(install.admission.iter()) {
            let r = pyo_proto::bundles::ObjectRef::of(o);
            crate::bundles::api_for(&admin, &r)
                .patch(
                    &r.name,
                    &PatchParams::apply("pyo-test").force(),
                    &Patch::Apply(o),
                )
                .await
                .unwrap_or_else(|e| panic!("{}: {e}", r.name));
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
        let token = Api::<ServiceAccount>::namespaced(admin.clone(), rbac::NS)
            .create_token_request(
                rbac::SA,
                &PostParams::default(),
                &serde_json::from_value::<TokenRequest>(
                    json!({ "spec": { "audiences": [], "expirationSeconds": 7200 } }),
                )
                .unwrap(),
            )
            .await
            .unwrap()
            .status
            .unwrap()
            .token
            .unwrap();
        let mut sa_config = config.clone();
        sa_config.auth_info = kube::config::AuthInfo {
            token: Some(token.into()),
            ..Default::default()
        };
        let sa = Client::try_from(sa_config).unwrap();
        // The agent's start: issuers and the gateway's fixed objects.
        tokio::time::timeout(Duration::from_secs(900), crate::tls::ensure(sa.clone()))
            .await
            .expect("cert-manager");
        tokio::time::timeout(Duration::from_secs(900), ensure(sa.clone(), false))
            .await
            .expect("the gateway");
        eprintln!("installed");

        // Pyo's API, as far as a rollout needs it: progress reports.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let api_url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 1 << 20];
                    let mut have = 0;
                    loop {
                        let Ok(n) = sock.read(&mut buf[have..]).await else {
                            return;
                        };
                        if n == 0 {
                            return;
                        }
                        have += n;
                        let text = String::from_utf8_lossy(&buf[..have]).to_string();
                        let Some(end) = text.find("\r\n\r\n") else {
                            continue;
                        };
                        let len = text[..end]
                            .lines()
                            .find_map(|l| {
                                l.to_ascii_lowercase()
                                    .strip_prefix("content-length:")
                                    .map(|v| v.trim().parse::<usize>().unwrap_or(0))
                            })
                            .unwrap_or(0);
                        if have < end + 4 + len {
                            continue;
                        }
                        let _ = sock.write_all(b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 2\r\n\r\n{}").await;
                        have = 0;
                    }
                });
            }
        });
        let agent = crate::Agent {
            kube: sa.clone(),
            api: PyoApi::new(&api_url, "t").unwrap(),
            cancels: Default::default(),
            deploying: Default::default(),
            routes: Default::default(),
            policy: std::sync::Arc::new(std::sync::RwLock::new(Default::default())),
        };

        let ns = "pyo-e2e-routes";
        let id = "svc_e2eroutes";
        let (a, b, c) = ("a.e2e.test", "b.e2e.test", "c.e2e.test");
        let deploy = |n: u32, image: &str, revision: i64, domains: Vec<DomainSpec>| {
            let mut s: DeploySpec = serde_json::from_value(json!({
                "deploymentId": format!("dpl_e2e{n}"), "service": { "serviceId": id, "name": "whoami", "namespace": ns, "workloadKind": "Deployment" },
                "projectId": "prj_e2e", "image": image, "replicas": 1, "env": [],
                "ports": [{ "name": "http", "containerPort": 80, "protocol": "HTTP", "exposure": "http", "publicPort": null }],
                "domains": [], "resources": { "cpuRequest": 10, "cpuLimit": 200, "memoryRequest": 16, "memoryLimit": 64 },
                "healthCheck": { "type": "http", "path": "/health", "port": 80, "command": "", "initialDelaySeconds": 1, "periodSeconds": 2 },
                "volumes": [], "timeoutSeconds": 120,
            }))
            .unwrap();
            s.domains = domains;
            s.routes_revision = revision;
            s
        };
        let plain = |h: &str| domain(h, "http", "", false);
        let secure = |h: &str| domain(h, "http", "selfsigned", false);
        let target = |revision: i64, domains: Vec<DomainSpec>| {
            let s = deploy(0, "", revision, domains);
            pyo_proto::FirewallTarget {
                service: s.service,
                project_id: s.project_id,
                ports: s.ports,
                domains: s.domains,
                routes_revision: revision,
                network_isolation: false,
                firewall: Default::default(),
                waf: None,
            }
        };
        let hostnames = || async {
            let mut h: Vec<String> = api_of(&sa, "HTTPRoute")
                .list(&ListParams::default().labels(&format!("{SERVICE_LABEL}={id}")))
                .await
                .unwrap()
                .items
                .iter()
                .flat_map(|r| {
                    r.data["spec"]["hostnames"]
                        .as_array()
                        .cloned()
                        .unwrap_or_default()
                })
                .filter_map(|h| h.as_str().map(str::to_owned))
                .collect();
            h.sort();
            h.dedup();
            h
        };
        let pods = || async {
            let mut p: Vec<(String, String, i32)> = Api::<Pod>::namespaced(sa.clone(), ns)
                .list(&ListParams::default().labels(&format!("{SERVICE_LABEL}={id}")))
                .await
                .unwrap()
                .items
                .into_iter()
                .filter(|p| p.metadata.deletion_timestamp.is_none())
                .map(|p| {
                    (
                        p.name_any(),
                        p.uid().unwrap_or_default(),
                        p.status
                            .and_then(|s| s.container_statuses)
                            .map(|c| c.iter().map(|c| c.restart_count).sum())
                            .unwrap_or(0),
                    )
                })
                .collect();
            p.sort();
            p
        };
        let web = reqwest::Client::builder()
            .danger_accept_invalid_certs(true)
            .resolve(b, https.parse().unwrap())
            .timeout(Duration::from_secs(10))
            .build()
            .unwrap();
        let get = |host: &'static str| {
            let (web, http) = (web.clone(), http.clone());
            async move {
                web.get(format!("http://{http}/"))
                    .header("host", host)
                    .send()
                    .await
                    .map(|r| r.status().as_u16())
                    .unwrap_or(0)
            }
        };
        let get_https = |host: &'static str| {
            let (web, https) = (web.clone(), https.clone());
            let port = https.rsplit(':').next().unwrap().to_owned();
            async move {
                web.get(format!("https://{host}:{port}/"))
                    .send()
                    .await
                    .map(|r| r.status().as_u16())
                    .unwrap_or(0)
            }
        };
        async fn until<F: std::future::Future<Output = bool>>(
            what: &str,
            secs: u64,
            mut f: impl FnMut() -> F,
        ) {
            for _ in 0..secs {
                if f().await {
                    return;
                }
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
            panic!("{what} didn't happen in {secs}s");
        }

        // 1. Deployed with a.
        let image = "traefik/whoami:v1.10.3";
        let r = crate::rollout::run(&agent, deploy(1, image, 1, vec![plain(a)])).await;
        assert!(r.ok, "{r:?}");
        until("a answers", 300, || async { get(a).await == 200 }).await;
        let before = pods().await;
        assert_eq!(before.len(), 1);
        eprintln!("deployed: {before:?}");

        // 2. b (HTTPS) added in place, while a download from a carries on.
        let download = {
            let (web, http) = (web.clone(), http.clone());
            tokio::spawn(async move {
                let mut res = web
                    .get(format!("http://{http}/data?size=6&unit=MB"))
                    .header("host", a)
                    .timeout(Duration::from_secs(120))
                    .send()
                    .await
                    .unwrap();
                let mut got = 0usize;
                while let Some(chunk) = res.chunk().await.unwrap() {
                    got += chunk.len();
                    tokio::time::sleep(Duration::from_millis(40)).await;
                }
                (res.status().as_u16(), got)
            })
        };
        tokio::time::sleep(Duration::from_secs(2)).await;
        let msg = sync_routes(&agent, &target(2, vec![plain(a), secure(b)]))
            .await
            .unwrap();
        eprintln!("sync 2: {msg}");
        assert_eq!(hostnames().await, [a, b]);
        let secrets: Api<Secret> = Api::namespaced(admin.clone(), GATEWAY_NS);
        let spec_b = deploy(0, "", 0, vec![]);
        let b_secret = tls_secret(&spec_b, b);
        until("b's certificate", 180, || async {
            secrets.get_opt(&b_secret).await.unwrap().is_some()
        })
        .await;
        until("b answers over HTTPS", 180, || async {
            get_https(b).await == 200
        })
        .await;
        assert!(!download.is_finished(), "the download outlasts the sync");
        let (status, got) = download.await.unwrap();
        assert_eq!(
            (status, got),
            (200, 6 << 20),
            "the open download carried on"
        );
        assert_eq!(pods().await, before, "its pods weren't touched");

        // 3. b removed: its route, certificate and Secret go.
        sync_routes(&agent, &target(3, vec![plain(a)]))
            .await
            .unwrap();
        assert_eq!(hostnames().await, [a]);
        assert!(
            api_of(&sa, "Certificate")
                .get_opt(&b_secret)
                .await
                .unwrap()
                .is_none(),
            "b's Certificate"
        );
        until("b's Secret goes", 30, || async {
            secrets.get_opt(&b_secret).await.unwrap().is_none()
        })
        .await;
        assert!(
            api_of(&sa, "Gateway")
                .get_opt(&base_name(&spec_b))
                .await
                .unwrap()
                .is_none(),
            "no HTTPS Gateway without HTTPS domains"
        );
        until("b stops answering", 60, || async { get(b).await == 404 }).await;
        assert_eq!(get(a).await, 200);
        assert_eq!(pods().await, before, "its pods weren't touched");

        // 4. A failed deploy handed out before b was removed, rolled back
        // to a version that had b: the domains of now stay.
        let r = crate::rollout::run(&agent, {
            let mut s = deploy(
                4,
                "traefik/whoami:pyo-e2e-no-such-tag",
                2,
                vec![plain(a), secure(b)],
            );
            s.timeout_seconds = 90;
            s.fallback = Some(Box::new(deploy(1, image, 2, vec![plain(a), secure(b)])));
            s
        })
        .await;
        assert!(!r.ok, "{r:?}");
        assert_eq!(hostnames().await, [a], "the rollback kept today's domains");
        assert!(secrets.get_opt(&b_secret).await.unwrap().is_none());
        until("a answers after the rollback", 120, || async {
            get(a).await == 200
        })
        .await;

        // 5. A deploy and a sync at once: the newest domains win, whichever
        // applies last.
        let (r, synced) = tokio::join!(
            crate::rollout::run(&agent, deploy(5, image, 4, vec![plain(a)])),
            async {
                tokio::time::sleep(Duration::from_millis(300)).await;
                sync_routes(&agent, &target(5, vec![plain(a), plain(c)])).await
            }
        );
        assert!(r.ok && synced.is_ok(), "{r:?} {synced:?}");
        assert_eq!(hostnames().await, [a, c]);
        let only_c = target(6, vec![plain(c)]);
        let (synced, r) = tokio::join!(sync_routes(&agent, &only_c), async {
            tokio::time::sleep(Duration::from_millis(300)).await;
            crate::rollout::run(&agent, deploy(6, image, 5, vec![plain(a), plain(c)])).await
        });
        assert!(r.ok && synced.is_ok(), "{r:?} {synced:?}");
        assert_eq!(
            hostnames().await,
            [c],
            "a stale deploy doesn't bring a back"
        );
        until("c answers, a doesn't", 60, || async {
            get(c).await == 200 && get(a).await == 404
        })
        .await;

        remove(&sa, id).await;
        let _ = Api::<Deployment>::namespaced(admin.clone(), ns)
            .delete("whoami", &DeleteParams::default())
            .await;
    }
}
