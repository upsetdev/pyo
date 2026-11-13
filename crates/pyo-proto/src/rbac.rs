//! What the agent may do in a cluster: the ServiceAccount, roles and
//! bindings of its install manifest (pyo-api's install.rs), plus the
//! admission policy that holds it to them.
//!
//! - It reads what it reports on, cluster-wide (never Secrets).
//! - It writes only in namespaces labelled `pyo.dev/managed=true`: its own
//!   (pyo-system) and the projects' it creates. Each gets a RoleBinding to
//!   `pyo-agent-write`, the only role the agent may bind.
//! - A few named objects elsewhere: the gateway's objects in
//!   `pyo-gateway`, the GPU device plugin, join tokens, CoreDNS's replica
//!   count and disruption budget, two certificate issuers, the proxy's
//!   GatewayClass and a RuntimeClass.
//! - Servers (labels, taints, cordon, drain, removal) in a role of their own
//!   that can be left out.
//! - The customer's `pyo-system/pyo-policy` ConfigMap: read, never written.
//!
//! RBAC can't say "only these names" for creating, or "only namespaces with
//! this label"; the ValidatingAdmissionPolicy does, for requests made by the
//! agent's ServiceAccount.

use serde_json::{Value, json};

use crate::{
    bundles,
    system::{self, cel_literal},
};

pub const NS: &str = "pyo-system";
pub const SA: &str = "pyo-agent";
/// The agent's user name in the API server's eyes.
pub const AGENT_USER: &str = "system:serviceaccount:pyo-system:pyo-agent";
/// Namespaces the agent may write in carry this label, set to "true".
pub const MANAGED_LABEL: &str = "pyo.dev/managed";
/// The ClusterRole bound in each managed namespace.
pub const WRITE_ROLE: &str = "pyo-agent-write";
/// The RoleBinding to it in each managed namespace.
pub const WRITE_BINDING: &str = "pyo-agent";
/// The customer's switches (see `Policy`), only ever read.
pub const POLICY_CONFIGMAP: &str = "pyo-policy";
/// The admission policy and its binding.
pub const ADMISSION_POLICY: &str = "pyo-agent";
/// Server operations: optional.
pub const NODES_ROLE: &str = "pyo-agent-nodes";
/// Disk and network figures through the kubelet: optional.
pub const STATS_ROLE: &str = "pyo-agent-stats";
/// The tailnet listeners' policy: its servers' rule's addresses, in CEL.
const TAILNET_SERVERS: &str = "object.spec.authorization.rules[1].principal.clientCIDRs";
/// The Role the upgrader Job runs with, on the cluster Pyo itself runs on.
pub const UPGRADER: &str = "pyo-upgrade";

const RBAC: &str = "rbac.authorization.k8s.io/v1";

fn labels() -> Value {
    json!({ "app.kubernetes.io/managed-by": "pyo", "app.kubernetes.io/part-of": "pyo-agent" })
}

fn cluster_role(name: &str, rules: Value) -> Value {
    json!({ "apiVersion": RBAC, "kind": "ClusterRole", "metadata": { "name": name, "labels": labels() }, "rules": rules })
}

fn cluster_binding(name: &str) -> Value {
    json!({
        "apiVersion": RBAC, "kind": "ClusterRoleBinding", "metadata": { "name": name, "labels": labels() },
        "roleRef": { "apiGroup": "rbac.authorization.k8s.io", "kind": "ClusterRole", "name": name },
        "subjects": [{ "kind": "ServiceAccount", "name": SA, "namespace": NS }],
    })
}

fn role(ns: &str, name: &str, rules: Value) -> Value {
    json!({ "apiVersion": RBAC, "kind": "Role", "metadata": { "name": name, "namespace": ns, "labels": labels() }, "rules": rules })
}

fn role_binding(ns: &str, name: &str, kind: &str, role: &str) -> Value {
    json!({
        "apiVersion": RBAC, "kind": "RoleBinding", "metadata": { "name": name, "namespace": ns, "labels": labels() },
        "roleRef": { "apiGroup": "rbac.authorization.k8s.io", "kind": kind, "name": role },
        "subjects": [{ "kind": "ServiceAccount", "name": SA, "namespace": NS }],
    })
}

/// The binding that lets the agent write in a managed namespace.
pub fn write_binding(ns: &str) -> Value {
    role_binding(ns, WRITE_BINDING, "ClusterRole", WRITE_ROLE)
}

/// A project's id, on its namespace (rollouts set it).
pub const PROJECT_LABEL: &str = "pyo/project-id";
/// A volume's id, on the claim the agent made for it.
pub const VOLUME_LABEL: &str = "pyo/volume-id";
/// Namespaces never deleted for a project, whatever their labels: the
/// cluster's and Pyo's own. Team databases' (`pyo-db-<team>`) too, by prefix.
pub const KEPT_NAMESPACES: [&str; 6] = [
    "default",
    NS,
    system::GATEWAY_NS,
    system::WAF_NS,
    "pyo-databases",
    "cert-manager",
];
const KEPT_PREFIXES: [&str; 2] = ["kube-", "pyo-db-"];

/// Why namespace `name` (with `labels`) isn't deleted for deleted project
/// `project_id`; None when it may be. Only a namespace Pyo manages for that
/// very project: never the cluster's, Pyo's own, a system bundle's or a
/// team's databases'. The admission policy holds the agent to the same.
pub fn project_namespace_refusal(
    name: &str,
    labels: &std::collections::BTreeMap<String, String>,
    project_id: &str,
) -> Option<String> {
    let label = |k: &str| labels.get(k).map(String::as_str).unwrap_or("");
    if KEPT_NAMESPACES.contains(&name) || KEPT_PREFIXES.iter().any(|p| name.starts_with(p)) {
        return Some(format!(
            "{name} is a system namespace and is never deleted."
        ));
    }
    if label(MANAGED_LABEL) != "true" || !label(bundles::BUNDLE_LABEL).is_empty() {
        return Some(format!("{name} isn't a project namespace Pyo manages."));
    }
    if project_id.is_empty() || label(PROJECT_LABEL) != project_id {
        return Some(format!("{name} belongs to another project, or none."));
    }
    None
}

/// The labels a namespace the agent manages carries.
pub fn managed_namespace(ns: &str) -> Value {
    let mut labels = json!({ MANAGED_LABEL: "true" });
    // A team's databases (CloudNativePG, the MySQL operator, KV): Pod
    // Security's baseline enforced, as in projects' namespaces.
    if ns.starts_with("pyo-db-") {
        labels[PSA_ENFORCE] = json!("baseline");
        labels["pod-security.kubernetes.io/enforce-version"] = json!("latest");
    }
    json!({ "apiVersion": "v1", "kind": "Namespace", "metadata": { "name": ns, "labels": labels } })
}

/// Pod Security Admission's enforced level on a namespace.
pub const PSA_ENFORCE: &str = "pod-security.kubernetes.io/enforce";

/// The sysctls Pod Security's baseline allows ("safe" ones).
const SAFE_SYSCTLS: [&str; 10] = [
    "kernel.shm_rmid_forced",
    "net.ipv4.ip_local_port_range",
    "net.ipv4.ip_unprivileged_port_start",
    "net.ipv4.tcp_syncookies",
    "net.ipv4.ping_group_range",
    "net.ipv4.ip_local_reserved_ports",
    "net.ipv4.tcp_keepalive_time",
    "net.ipv4.tcp_fin_timeout",
    "net.ipv4.tcp_keepalive_intvl",
    "net.ipv4.tcp_keepalive_probes",
];

const READ: [&str; 3] = ["get", "list", "watch"];
const ALL: [&str; 7] = [
    "get", "list", "watch", "create", "update", "patch", "delete",
];

/// The agent's ServiceAccount, roles and bindings. `upgrades`: on the
/// cluster Pyo itself runs on, where the agent starts Pyo's upgrader.
pub fn objects(upgrades: bool) -> Vec<Value> {
    let mut out = vec![
        json!({ "apiVersion": "v1", "kind": "Namespace", "metadata": { "name": NS, "labels": { MANAGED_LABEL: "true" } } }),
        json!({ "apiVersion": "v1", "kind": "ServiceAccount", "metadata": { "name": SA, "namespace": NS, "labels": labels() } }),
        // What it reports on: servers, workloads, volumes, storage, HTTPS.
        cluster_role(
            "pyo-agent-read",
            json!([
                { "apiGroups": [""], "resources": ["nodes", "namespaces", "pods", "services", "events", "persistentvolumeclaims", "persistentvolumes"], "verbs": READ },
                { "apiGroups": ["apps"], "resources": ["deployments", "statefulsets", "daemonsets", "replicasets"], "verbs": READ },
                { "apiGroups": ["batch"], "resources": ["jobs", "cronjobs"], "verbs": READ },
                { "apiGroups": ["autoscaling"], "resources": ["horizontalpodautoscalers"], "verbs": READ },
                { "apiGroups": ["networking.k8s.io"], "resources": ["networkpolicies"], "verbs": READ },
                { "apiGroups": ["storage.k8s.io"], "resources": ["storageclasses"], "verbs": READ },
                { "apiGroups": ["node.k8s.io"], "resources": ["runtimeclasses"], "verbs": READ },
                { "apiGroups": ["metrics.k8s.io"], "resources": ["nodes", "pods"], "verbs": ["get", "list"] },
                { "apiGroups": ["cert-manager.io"], "resources": ["certificates", "clusterissuers"], "verbs": READ },
                { "apiGroups": ["gateway.networking.k8s.io"], "resources": ["gatewayclasses", "gateways", "httproutes"], "verbs": READ },
                { "apiGroups": ["admissionregistration.k8s.io"], "resources": ["validatingadmissionpolicies", "validatingadmissionpolicybindings"], "verbs": ["get"], "resourceNames": [ADMISSION_POLICY] },
                // To check what the gateway's controller may read (asks only).
                { "apiGroups": ["authorization.k8s.io"], "resources": ["subjectaccessreviews"], "verbs": ["create"] },
            ]),
        ),
        cluster_binding("pyo-agent-read"),
        // Project namespaces: create them, label them, and bind the write
        // role in them (no other role).
        cluster_role(
            "pyo-agent-namespaces",
            json!([
                // Delete: a deleted project's (the admission policy says which).
                { "apiGroups": [""], "resources": ["namespaces"], "verbs": ["create", "patch", "delete"] },
                { "apiGroups": ["rbac.authorization.k8s.io"], "resources": ["rolebindings"], "verbs": ["get", "create", "patch"] },
                { "apiGroups": ["rbac.authorization.k8s.io"], "resources": ["clusterroles"], "verbs": ["bind"], "resourceNames": [WRITE_ROLE] },
            ]),
        ),
        cluster_binding("pyo-agent-namespaces"),
        // Inside a managed namespace: the services it deploys. No
        // ConfigMaps: nothing Pyo deploys needs one, and so the customer's
        // policy ConfigMap stays out of reach.
        cluster_role(
            WRITE_ROLE,
            json!([
                { "apiGroups": [""], "resources": ["pods"], "verbs": ["get", "list", "watch", "create", "delete"] },
                { "apiGroups": [""], "resources": ["pods/log"], "verbs": ["get"] },
                { "apiGroups": [""], "resources": ["pods/exec"], "verbs": ["get", "create"] },
                { "apiGroups": [""], "resources": ["services", "secrets", "serviceaccounts", "persistentvolumeclaims"], "verbs": ALL },
                { "apiGroups": [""], "resources": ["events"], "verbs": READ },
                { "apiGroups": ["apps"], "resources": ["deployments", "statefulsets", "daemonsets"], "verbs": ALL },
                { "apiGroups": ["batch"], "resources": ["jobs", "cronjobs"], "verbs": ALL },
                { "apiGroups": ["autoscaling"], "resources": ["horizontalpodautoscalers"], "verbs": ALL },
                { "apiGroups": ["networking.k8s.io"], "resources": ["networkpolicies"], "verbs": ALL },
                { "apiGroups": ["policy"], "resources": ["poddisruptionbudgets"], "verbs": ALL },
                // Databases (pyo-db-controller's), and nothing else of the system.
                { "apiGroups": ["pyo.dev"], "resources": ["databases"], "verbs": ALL },
            ]),
        ),
        write_binding(NS),
        // The customer's switches: read only.
        role(
            NS,
            "pyo-agent-policy",
            json!([
                { "apiGroups": [""], "resources": ["configmaps"], "verbs": ["get"], "resourceNames": [POLICY_CONFIGMAP] },
            ]),
        ),
        role_binding(NS, "pyo-agent-policy", "Role", "pyo-agent-policy"),
        // kube-system: only these objects. Creating can't be limited to a
        // name here; the admission policy does that.
        role(
            "kube-system",
            "pyo-agent",
            json!([
                // Join tokens: created, then deleted by name.
                { "apiGroups": [""], "resources": ["secrets"], "verbs": ["create", "delete"] },
                { "apiGroups": ["apps"], "resources": ["daemonsets"], "verbs": ["create"] },
                { "apiGroups": ["apps"], "resources": ["daemonsets"], "verbs": ["get", "patch"], "resourceNames": ["nvidia-device-plugin"] },
                // Cluster DNS on more than one server: k3s's CoreDNS scaled
                // (its replica count only) and a disruption budget for it.
                { "apiGroups": ["apps"], "resources": ["deployments/scale"], "verbs": ["get", "patch"], "resourceNames": [system::COREDNS] },
                { "apiGroups": ["policy"], "resources": ["poddisruptionbudgets"], "verbs": ["create"] },
                { "apiGroups": ["policy"], "resources": ["poddisruptionbudgets"], "verbs": ["get", "patch", "delete"], "resourceNames": [system::COREDNS_BUDGET] },
            ]),
        ),
        role_binding("kube-system", "pyo-agent", "Role", "pyo-agent"),
        // The gateway's namespace: routes, Gateways, Backends,
        // certificates and client policies. Its Secrets (certificates, and
        // the proxy's own): written by name, never read or listed.
        json!({ "apiVersion": "v1", "kind": "Namespace", "metadata": { "name": system::GATEWAY_NS } }),
        role(
            system::GATEWAY_NS,
            "pyo-agent",
            json!([
                { "apiGroups": ["gateway.networking.k8s.io"], "resources": ["gateways", "httproutes", "tcproutes", "udproutes"], "verbs": ALL },
                { "apiGroups": ["gateway.envoyproxy.io"], "resources": ["backends", "clienttrafficpolicies", "securitypolicies", "backendtrafficpolicies", "envoyextensionpolicies", "envoyproxies"], "verbs": ALL },
                { "apiGroups": ["cert-manager.io"], "resources": ["certificates"], "verbs": ALL },
                { "apiGroups": [""], "resources": ["secrets"], "verbs": ["create", "patch", "delete"] },
                { "apiGroups": [""], "resources": ["configmaps"], "verbs": ["create"] },
                { "apiGroups": [""], "resources": ["configmaps"], "verbs": ["get", "patch", "delete"], "resourceNames": [system::ORIGIN_PULL_SECRET] },
                // The firewall for UDP ports: a network policy on the proxy's pods.
                { "apiGroups": ["networking.k8s.io"], "resources": ["networkpolicies"], "verbs": ["create"] },
                { "apiGroups": ["networking.k8s.io"], "resources": ["networkpolicies"], "verbs": ["get", "patch", "delete"], "resourceNames": [system::FIREWALL_POLICY] },
                // Websites on the servers' Tailscale addresses: its own Service.
                { "apiGroups": [""], "resources": ["services"], "verbs": ["create"] },
                { "apiGroups": [""], "resources": ["services"], "verbs": ["get", "patch", "delete"], "resourceNames": [system::TAILNET_WEB_SERVICE] },
            ]),
        ),
        role_binding(system::GATEWAY_NS, "pyo-agent", "Role", "pyo-agent"),
        // Cluster-wide objects it owns, by name.
        cluster_role(
            "pyo-agent-cluster",
            json!([
                { "apiGroups": ["cert-manager.io"], "resources": ["clusterissuers"], "verbs": ["create"] },
                { "apiGroups": ["cert-manager.io"], "resources": ["clusterissuers"], "verbs": ["patch"], "resourceNames": ["pyo-letsencrypt", "pyo-selfsigned"] },
                { "apiGroups": ["node.k8s.io"], "resources": ["runtimeclasses"], "verbs": ["create"] },
                { "apiGroups": ["node.k8s.io"], "resources": ["runtimeclasses"], "verbs": ["patch"], "resourceNames": ["nvidia"] },
                { "apiGroups": ["gateway.networking.k8s.io"], "resources": ["gatewayclasses"], "verbs": ["create"] },
                { "apiGroups": ["gateway.networking.k8s.io"], "resources": ["gatewayclasses"], "verbs": ["patch"], "resourceNames": ["pyo"] },
                // A volume moved to another project: its data kept (Retain)
                // and bound to its new claim. Never made or deleted.
                { "apiGroups": [""], "resources": ["persistentvolumes"], "verbs": ["patch"] },
            ]),
        ),
        cluster_binding("pyo-agent-cluster"),
        // System bundles (bundles.rs): their cluster-wide objects by name, and in
        // their own namespaces only what a bundle has (bound there when one
        // is installed). The admission policy holds both to the bundles.
        // (Names only, so any images do.)
        cluster_role(
            bundles::CLUSTER_ROLE,
            Value::Array(bundles::cluster_rules(&bundles::policy_bundles(
                system::WAF_IMAGE_DEFAULT,
                system::GEOIP_IMAGE_DEFAULT,
            ))),
        ),
        cluster_binding(bundles::CLUSTER_ROLE),
        cluster_role(
            bundles::WRITE_ROLE,
            Value::Array(bundles::write_rules(&bundles::policy_bundles(
                system::WAF_IMAGE_DEFAULT,
                system::GEOIP_IMAGE_DEFAULT,
            ))),
        ),
        // Optional: servers. Labels, taints and regions; pausing, draining
        // and removing them (evicting or deleting their pods anywhere).
        // Without it those actions fail and the rest works.
        cluster_role(
            NODES_ROLE,
            json!([
                { "apiGroups": [""], "resources": ["nodes"], "verbs": ["patch", "delete"] },
                { "apiGroups": [""], "resources": ["pods/eviction"], "verbs": ["create"] },
                { "apiGroups": [""], "resources": ["pods"], "verbs": ["delete"] },
            ]),
        ),
        cluster_binding(NODES_ROLE),
        // Optional: disk and network figures from each server's kubelet.
        cluster_role(
            STATS_ROLE,
            json!([
                { "apiGroups": [""], "resources": ["nodes/proxy"], "verbs": ["get"] },
            ]),
        ),
        cluster_binding(STATS_ROLE),
        // HTTPS and the gateway: installed with the agent, not by it.
        system::cert_manager_chart(),
        system::envoy_gateway_chart(),
    ];
    if upgrades {
        // Pyo's upgrader: the agent writes its Role (whatever a release
        // needs in pyo-system) and binds it to the upgrader's account.
        out.push(role(NS, "pyo-agent-upgrade", json!([
            { "apiGroups": ["rbac.authorization.k8s.io"], "resources": ["roles", "rolebindings"], "verbs": ["create"] },
            { "apiGroups": ["rbac.authorization.k8s.io"], "resources": ["roles", "rolebindings"], "verbs": ["get", "patch"], "resourceNames": [UPGRADER] },
            { "apiGroups": ["rbac.authorization.k8s.io"], "resources": ["roles"], "verbs": ["escalate", "bind"], "resourceNames": [UPGRADER] },
        ])));
        out.push(role_binding(
            NS,
            "pyo-agent-upgrade",
            "Role",
            "pyo-agent-upgrade",
        ));
    }
    out
}

/// What the admission policy pins of a route's WAF call: all but where it
/// sends (checked on its own: the API server fills in a weight).
fn waf_pinned_for(block: bool) -> serde_json::Map<String, Value> {
    let mut m = system::waf_ext_proc(block)
        .as_object()
        .cloned()
        .unwrap_or_default();
    m.remove("backendRefs");
    m
}

/// The CEL behind the admission policy, for requests by the agent.
/// `waf_image` and `geoip_image`: the installation's images for the WAF's
/// processor and the proxies' GeoIP database, the only ones they may use.
pub fn admission(waf_image: &str, geoip_image: &str) -> Vec<Value> {
    let bundles = bundles::policy_bundles(waf_image, geoip_image);
    // The pod spec of whatever the request makes pods from.
    let pod_spec = "object.kind == 'Pod' ? object.spec : object.kind == 'CronJob' ? object.spec.jobTemplate.spec.template.spec : object.spec.template.spec";
    let validations = json!([
        // Only namespaces it manages, and the named objects of kube-system.
        {
            "expression": "variables.ns == '' || request.resource.resource == 'namespaces' || variables.managed || variables.kubeSystemAllowed || variables.gatewayAllowed || variables.drain || variables.bundleAllowed",
            "messageExpression": "variables.ns in ['kube-system', 'pyo-gateway'] ? 'In ' + variables.ns + ', Pyo\\'s agent only writes its own objects, with exactly the content this release defines.' : 'Pyo\\'s agent only changes namespaces labelled pyo.dev/managed=true; ' + variables.ns + ' isn\\'t one.'",
        },
        // Bundles' cluster-wide objects and charts: only as a bundle has them.
        {
            "expression": format!("!(request.resource.resource in {}) || variables.bundleAllowed", cel_literal(&json!(bundles::guarded_resources()))),
            "message": "Pyo's agent only writes its components' own objects, as this release defines them.",
        },
        // In a bundle's namespace, only the bundle's own containers.
        {
            "expression": "variables.bundlePods",
            "message": "In a component's namespace, Pyo's agent only runs that component's own containers.",
        },
        {
            "expression": "variables.clusterWideAllowed",
            "message": "Pyo's agent only writes its own issuers, gateway class and GPU runtime, as this release defines them.",
        },
        // Namespaces: new ones it creates, or its own; never kube-*/default.
        {
            "expression": "request.resource.resource != 'namespaces' || request.operation == 'DELETE' || (!(variables.reqName in ['default', 'kube-system', 'kube-public', 'kube-node-lease']) && (request.operation == 'CREATE' || (has(oldObject.metadata.labels) && (oldObject.metadata.labels[?'pyo.dev/managed'].orValue('') == 'true' || oldObject.metadata.labels[?'app.kubernetes.io/managed-by'].orValue('') == 'pyo'))))",
            "message": "Pyo's agent only creates namespaces or changes its own.",
        },
        // Deleting one: a system bundle's own, or a project's (managed, with
        // its project's label); never the cluster's, Pyo's or a team's
        // databases' (project_namespace_refusal).
        {
            "expression": format!("request.resource.resource != 'namespaces' || request.operation != 'DELETE' || (!(variables.reqName in {kept}) && !({prefixes}.exists(p, variables.reqName.startsWith(p))) && has(oldObject.metadata.labels) && (\
                (variables.reqName in {bundle_ns} && oldObject.metadata.labels[?'{bundle}'].orValue('') != '') || \
                (!(variables.reqName in {bundle_ns}) && oldObject.metadata.labels[?'{managed}'].orValue('') == 'true' && oldObject.metadata.labels[?'{project}'].orValue('') != '' && oldObject.metadata.labels[?'{bundle}'].orValue('') == '')))",
                kept = cel_literal(&json!(KEPT_NAMESPACES.iter().filter(|n| !bundles.iter().any(|b| b.namespace == **n)).collect::<Vec<_>>())),
                prefixes = cel_literal(&json!(KEPT_PREFIXES)),
                bundle_ns = cel_literal(&json!(bundles.iter().map(|b| b.namespace.as_str()).collect::<Vec<_>>())),
                bundle = bundles::BUNDLE_LABEL,
                managed = MANAGED_LABEL,
                project = PROJECT_LABEL),
            "message": "Pyo's agent only deletes a deleted project's namespace, or a component's own.",
        },
        // Volume claims: deleted only when Pyo made them for a volume
        // (labelled with its id) in a project's namespace; that label is
        // set when a claim is made and never added or changed after.
        {
            "expression": format!("request.resource.resource != 'persistentvolumeclaims' || variables.sub != '' || request.operation == 'CREATE' || variables.bundleAllowed || (request.operation == 'DELETE' ? \
                (variables.managed && namespaceObject.metadata.labels[?'{project}'].orValue('') != '' && namespaceObject.metadata.labels[?'{bundle}'].orValue('') == '' && has(oldObject.metadata.labels) && oldObject.metadata.labels[?'{volume}'].orValue('') != '' && oldObject.metadata.labels[?'app.kubernetes.io/managed-by'].orValue('') == 'pyo') : \
                ((has(oldObject.metadata.labels) ? oldObject.metadata.labels[?'{volume}'].orValue('') : '') == (has(object.metadata.labels) ? object.metadata.labels[?'{volume}'].orValue('') : '')))",
                project = PROJECT_LABEL,
                bundle = bundles::BUNDLE_LABEL,
                volume = VOLUME_LABEL),
            "message": "Pyo's agent only deletes the claims it made for a volume, in a project's namespace.",
        },
        // Volumes behind claims: only rebinding one (its claim and reclaim
        // policy), once it's labelled as a Pyo volume's; that label is set
        // once, never changed. What it holds, its size, class and server
        // stay as they are.
        {
            "expression": format!("request.resource.resource != 'persistentvolumes' || (request.operation == 'UPDATE' && variables.sub == '' && \
                (has(object.metadata.labels) ? object.metadata.labels[?'{volume}'].orValue('') : '') != '' && \
                ((has(oldObject.metadata.labels) ? oldObject.metadata.labels[?'{volume}'].orValue('') : '') in ['', (has(object.metadata.labels) ? object.metadata.labels[?'{volume}'].orValue('') : '')]) && \
                object.spec.capacity == oldObject.spec.capacity && \
                (has(object.spec.accessModes) ? object.spec.accessModes : []) == (has(oldObject.spec.accessModes) ? oldObject.spec.accessModes : []) && \
                (has(object.spec.storageClassName) ? object.spec.storageClassName : '') == (has(oldObject.spec.storageClassName) ? oldObject.spec.storageClassName : '') && \
                has(object.spec.nodeAffinity) == has(oldObject.spec.nodeAffinity) && (!has(object.spec.nodeAffinity) || object.spec.nodeAffinity == oldObject.spec.nodeAffinity))",
                volume = VOLUME_LABEL),
            "message": "Pyo's agent only rebinds a volume to a claim. It never changes what a volume holds.",
        },
        // RoleBindings: the write role, to the agent; the upgrader's, to the upgrader.
        {
            "expression": "request.resource.resource != 'rolebindings' || request.operation == 'DELETE' || (object.roleRef.kind == 'ClusterRole' && object.roleRef.name == 'pyo-agent-write' && object.subjects.all(s, s.kind == 'ServiceAccount' && s.name == 'pyo-agent' && s.namespace == 'pyo-system')) || (variables.ns == 'pyo-system' && object.metadata.name == 'pyo-upgrade' && object.roleRef.kind == 'Role' && object.roleRef.name == 'pyo-upgrade' && object.subjects.all(s, s.kind == 'ServiceAccount' && s.name == 'pyo-upgrade' && s.namespace == 'pyo-system')) || variables.bundleAllowed",
            "message": "Pyo's agent may only bind its own write role to itself.",
        },
        {
            "expression": "request.resource.resource != 'roles' || (variables.ns == 'pyo-system' && variables.reqName == 'pyo-upgrade') || variables.bundleAllowed",
            "message": "Pyo's agent may only write the upgrader's role, and its components' own.",
        },
        // The customer's policy is theirs.
        {
            "expression": "!(request.resource.resource == 'configmaps' && variables.ns == 'pyo-system' && variables.reqName == 'pyo-policy')",
            "message": "pyo-system/pyo-policy belongs to you: Pyo's agent only reads it.",
        },
        // No privileged containers, host namespaces or host paths, except
        // Pyo's own components that need them.
        {
            "expression": "!variables.hasPods || variables.systemException || (!(has(variables.spec.hostNetwork) && variables.spec.hostNetwork) && !(has(variables.spec.hostPID) && variables.spec.hostPID) && !(has(variables.spec.hostIPC) && variables.spec.hostIPC) && !(has(variables.spec.volumes) && variables.spec.volumes.exists(v, has(v.hostPath))) && !variables.containers.exists(c, has(c.securityContext) && has(c.securityContext.privileged) && c.securityContext.privileged))",
            "message": "Pyo's agent doesn't run privileged containers or use the host's network, processes or files.",
        },
        // The rest of Pod Security's baseline, for every pod it makes (Pyo's
        // own components too): capabilities only dropped, never added; no
        // unconfined seccomp or AppArmor; no host ports; the default /proc;
        // only safe sysctls.
        {
            "expression": "!variables.hasPods || (\
                variables.containers.all(c, !has(c.securityContext) || ((!has(c.securityContext.capabilities) || !has(c.securityContext.capabilities.add) || size(c.securityContext.capabilities.add) == 0) && \
                    (!has(c.securityContext.seccompProfile) || c.securityContext.seccompProfile.type != 'Unconfined') && \
                    (!has(c.securityContext.appArmorProfile) || c.securityContext.appArmorProfile.type != 'Unconfined') && \
                    (!has(c.securityContext.procMount) || c.securityContext.procMount == 'Default'))) && \
                variables.containers.all(c, !has(c.ports) || c.ports.all(p, !has(p.hostPort) || p.hostPort == 0)) && \
                (!has(variables.spec.securityContext) || ((!has(variables.spec.securityContext.seccompProfile) || variables.spec.securityContext.seccompProfile.type != 'Unconfined') && \
                    (!has(variables.spec.securityContext.appArmorProfile) || variables.spec.securityContext.appArmorProfile.type != 'Unconfined') && \
                    (!has(variables.spec.securityContext.sysctls) || variables.spec.securityContext.sysctls.all(x, x.name in variables.safeSysctls)))) && \
                !variables.podAnnotations.exists(k, k.startsWith('container.apparmor.security.beta.kubernetes.io/') && variables.podAnnotations[k] == 'unconfined'))",
            "message": "Pyo's agent doesn't add Linux capabilities, turn off seccomp or AppArmor, use host ports, unmask /proc or set unsafe sysctls.",
        },
        // Pod Security levels: set only by the namespace's owner or loosened
        // by nobody. Pyo's agent may make a namespace stricter, never laxer;
        // and a project's or a team's databases' namespace it creates is
        // baseline at least.
        {
            "expression": "request.resource.resource != 'namespaces' || request.operation == 'DELETE' || variables.sub != '' || \
                (request.operation == 'UPDATE' ? variables.psaRank[?variables.psaNew].orValue(0) >= variables.psaRank[?variables.psaOld].orValue(0) : \
                    (!(variables.reqName.startsWith('pyo-db-') || (has(object.metadata.labels) && object.metadata.labels[?'pyo/project-id'].orValue('') != '')) || variables.psaRank[?variables.psaNew].orValue(0) >= 1))",
            "message": "Pyo's agent doesn't loosen a namespace's Pod Security level, and projects' and databases' namespaces are baseline at least.",
        },
    ]);
    let variables = json!([
        // Fields a request may leave out: cluster-wide objects have no
        // namespace, most requests no subresource.
        { "name": "ns", "expression": "has(request.namespace) ? request.namespace : ''" },
        { "name": "sub", "expression": "has(request.subResource) ? request.subResource : ''" },
        { "name": "reqName", "expression": "has(request.name) ? request.name : ''" },
        { "name": "managed", "expression": "namespaceObject != null && has(namespaceObject.metadata.labels) && namespaceObject.metadata.labels[?'pyo.dev/managed'].orValue('') == 'true'" },
        { "name": "objName", "expression": "variables.reqName != '' ? variables.reqName : (has(object.metadata.name) ? object.metadata.name : '')" },
        // kube-system: join tokens, the GPU device plugin, and CoreDNS's
        // replica count (1 or 2) and disruption budget.
        { "name": "kubeSystemAllowed", "expression": format!("variables.ns == 'kube-system' && (\
            (request.resource.resource == 'deployments' && variables.sub == 'scale' && variables.objName == '{dns}' && request.operation == 'UPDATE' && object.spec.replicas in [1, 2]) || \
            (request.resource.resource == 'poddisruptionbudgets' && variables.sub == '' && variables.objName == '{dns_budget}' && (request.operation == 'DELETE' || object.spec == {dns_budget_spec})) || \
            (request.resource.resource == 'secrets' && variables.objName.startsWith('bootstrap-token-') && (request.operation == 'DELETE' ? (oldObject.type == 'bootstrap.kubernetes.io/token' && has(oldObject.metadata.labels) && oldObject.metadata.labels[?'pyo/join-token'].orValue('') == 'true') : (object.type == 'bootstrap.kubernetes.io/token' && has(object.metadata.labels) && object.metadata.labels[?'pyo/join-token'].orValue('') == 'true'))) || \
            (request.resource.resource == 'daemonsets' && variables.objName == 'nvidia-device-plugin' && request.operation != 'DELETE' && size(object.spec.template.spec.containers) == 1 && object.spec.template.spec.containers[0].image == '{plugin}' && !has(object.spec.template.spec.initContainers) && !has(object.spec.template.spec.containers[0].command) && !has(object.spec.template.spec.containers[0].args)))",
            plugin = system::DEVICE_PLUGIN_IMAGE,
            dns = system::COREDNS,
            dns_budget = system::COREDNS_BUDGET,
            dns_budget_spec = cel_literal(&system::coredns_budget()["spec"])) },
        // A proxied port's policies: a service's own (its labels), on its
        // port route (`<namespace>-<service>-tcp-<port>`).
        { "name": "portPolicy", "expression": "request.operation != 'DELETE' && has(object.metadata.labels) && object.metadata.labels[?'app.kubernetes.io/managed-by'].orValue('') == 'pyo' && object.metadata.labels[?'pyo/service-id'].orValue('') != ''" },
        { "name": "toPortRoute", "expression": "request.operation != 'DELETE' && has(object.spec.targetRefs) && object.spec.targetRefs.all(t, t.group == 'gateway.networking.k8s.io' && t.kind == 'TCPRoute' && !has(t.sectionName))" },
        { "name": "toWebRoute", "expression": "request.operation != 'DELETE' && has(object.spec.targetRefs) && object.spec.targetRefs.all(t, t.group == 'gateway.networking.k8s.io' && t.kind == 'HTTPRoute' && !has(t.sectionName))" },
        // A service's firewall and WAF policies: each on the one route it's
        // named after, so it never reaches another service's routes.
        { "name": "ownRoute", "expression": "request.operation != 'DELETE' && size(object.spec.targetRefs) == 1 && object.spec.targetRefs[0].name == variables.objName" },
        // The gateway's namespace: the fixed objects exactly as this
        // release defines them; a service's routes only to its Backends, by
        // cluster DNS name; certificates only from Pyo's issuers, into
        // pyo-tls-* Secrets; nothing of the proxy's own.
        { "name": "gatewayAllowed", "expression": format!("variables.ns == '{gw}' && (\
            (request.resource.resource == 'envoyproxies' && variables.objName == 'pyo' && request.operation != 'DELETE' && {proxies}.exists(p, {proxy_keys}.all(k, k in object.spec && object.spec[k] == p[k])) && object.spec.all(k, k in {proxy_keys})) || \
            (request.resource.resource == 'clienttrafficpolicies' && (variables.objName in ['pyo-http', 'pyo-https'] ? (request.operation != 'DELETE' && ((variables.objName == 'pyo-http' && object.spec in {ctp_http}) || (variables.objName == 'pyo-https' && object.spec in {ctp_https}))) : (request.operation == 'DELETE' || variables.portPolicy && \
                object.spec.all(k, k in ['targetRefs', 'connection', 'proxyProtocol']) && object.spec.targetRefs.all(t, t.group == 'gateway.networking.k8s.io' && t.kind == 'Gateway' && t.name.endsWith('-ports') && has(t.sectionName)) && \
                (!has(object.spec.connection) || (object.spec.connection.all(k, k in ['connectionLimit', 'maxAcceptPerSocketEvent']) && (!has(object.spec.connection.maxAcceptPerSocketEvent) || object.spec.connection.maxAcceptPerSocketEvent == 1) && (!has(object.spec.connection.connectionLimit) || (object.spec.connection.connectionLimit.all(k, k == 'value') && object.spec.connection.connectionLimit.value >= 1)))) && \
                (!has(object.spec.proxyProtocol) || object.spec.proxyProtocol == {pp_optional})))) || \
            (request.resource.resource == 'securitypolicies' && variables.objName != '{tailnet_gw}' && (request.operation == 'DELETE' || variables.portPolicy && (variables.toPortRoute || variables.toWebRoute) && variables.ownRoute && \
                object.spec.all(k, k in ['targetRefs', 'authorization']) && object.spec.authorization.all(k, k in ['defaultAction', 'rules']) && \
                (!has(object.spec.authorization.rules) || object.spec.authorization.rules.all(r, r.all(k, k in ['name', 'action', 'principal']) && r.principal.all(k, k == 'clientCIDRs' || (k == 'clientIPGeoLocations' && variables.toWebRoute)) && \
                    (!has(r.principal.clientIPGeoLocations) || r.principal.clientIPGeoLocations.all(g, g.all(k, k == 'country'))))))) || \
            (request.resource.resource == 'backendtrafficpolicies' && (request.operation == 'DELETE' || variables.portPolicy && \
                ((variables.toPortRoute && object.spec.all(k, k in ['targetRefs', 'proxyProtocol']) && object.spec.proxyProtocol.all(k, k == 'version') && object.spec.proxyProtocol.version in ['V1', 'V2']) || \
                 (variables.toWebRoute && object.spec.all(k, k in ['targetRefs', 'rateLimit', 'responseOverride']) && (has(object.spec.rateLimit) || has(object.spec.responseOverride)) && \
                    (!has(object.spec.responseOverride) || object.spec.responseOverride == {fw_override}) && \
                    (!has(object.spec.rateLimit) || (object.spec.rateLimit.all(k, k in ['type', 'local']) && object.spec.rateLimit.type == 'Local' && \
                    object.spec.rateLimit.local.all(k, k == 'rules') && size(object.spec.rateLimit.local.rules) >= 1 && size(object.spec.rateLimit.local.rules) <= 16 && object.spec.rateLimit.local.rules.all(r, r.all(k, k in ['clientSelectors', 'limit']) && \
                    size(r.clientSelectors) == 1 && r.clientSelectors.all(c, c.all(k, k == 'sourceCIDR') && c.sourceCIDR.all(k, k in ['type', 'value', 'invert']) && c.sourceCIDR.type == 'Distinct' && c.sourceCIDR.value.matches('^[0-9a-fA-F:.]+/[0-9]{{1,3}}$')) && \
                    r.limit.all(k, k in ['requests', 'unit']) && r.limit.unit in ['Second', 'Minute'] && r.limit.requests >= 1))))))) || \
            (request.resource.resource == 'envoyextensionpolicies' && (request.operation == 'DELETE' || variables.portPolicy && variables.toWebRoute && variables.ownRoute && \
                has(object.metadata.annotations) && object.metadata.annotations[?'{waf_annotation}'].orValue('') != '' && \
                object.spec.all(k, k in ['targetRefs', 'extProc']) && size(object.spec.extProc) == 1 && object.spec.extProc[0].all(k, k in {waf_keys}) && \
                size(object.spec.extProc[0].backendRefs) == 1 && object.spec.extProc[0].backendRefs.all(b, b.name == '{waf}' && b.kind == 'Backend' && b.group == 'gateway.envoyproxy.io' && (!has(b.namespace) || b.namespace == '{gw}') && !has(b.port)) && \
                {waf_pinned}.all(k, k in object.spec.extProc[0] && object.spec.extProc[0][k] == ((has(object.metadata.annotations) && object.metadata.annotations[?'{waf_annotation}'].orValue('').contains({waf_mark})) ? {waf_block}[k] : {waf_pass}[k])))) || \
            (request.resource.resource == 'networkpolicies' && variables.objName == '{fw_policy}' && (request.operation == 'DELETE' || \
                (object.spec.all(k, k in ['podSelector', 'policyTypes', 'ingress']) && object.spec.podSelector == {proxy_pods} && object.spec.policyTypes == ['Ingress']))) || \
            (request.resource.resource == 'gateways' && variables.objName == '{tailnet_gw}' && (request.operation == 'DELETE' || object.spec == {tailnet_spec})) || \
            (request.resource.resource == 'securitypolicies' && variables.objName == '{tailnet_gw}' && (request.operation == 'DELETE' || object.spec == {tailnet_policy} || \
                (has(object.spec.authorization) && has(object.spec.authorization.rules) && size(object.spec.authorization.rules) == 2 && object.spec == {tailnet_policy_servers} && \
                    size({servers}) >= 1 && size({servers}) <= 512 && {servers}.all(c, c.matches({server_v4}) || c.matches({server_v6}))))) || \
            {tailnet_routes} || \
            (request.resource.resource == 'services' && variables.objName == '{tw_svc}' && variables.sub == '' && (request.operation == 'DELETE' || (object.spec.all(k, k in {tw_keys}) && \
                object.spec.type == 'ClusterIP' && object.spec.externalTrafficPolicy == 'Local' && object.spec.selector == {tw_selector} && object.spec.ports == {tw_ports} && \
                size(object.spec.externalIPs) >= 1 && size(object.spec.externalIPs) <= 512 && object.spec.externalIPs.all(ip, ip.matches({tw_v4}) || ip.matches({tw_v6}))))) || \
            (request.resource.resource == 'gateways' && variables.objName != '{tailnet_gw}' && (request.operation == 'DELETE' ? variables.objName != '{http_gw}' : (variables.objName == '{http_gw}' ? object.spec == {http_spec} : \
                (object.metadata.labels[?'{label}'].orValue('') == 'https' && object.spec.gatewayClassName == 'pyo' && object.spec.listeners.all(l, l.port == 443 && l.protocol == 'HTTPS' && has(l.hostname) && l.tls.mode == 'Terminate' && l.tls.certificateRefs.all(c, c.name.startsWith('{tls}') && (!has(c.namespace) || c.namespace == '{gw}') && c.kind == 'Secret') && has(l.allowedRoutes) && l.allowedRoutes.namespaces.from == 'Same')) || \
                (object.metadata.labels[?'{label}'].orValue('') == '{ports_label}' && variables.objName.endsWith('-ports') && object.spec.gatewayClassName == 'pyo' && object.spec.listeners.all(l, l.all(k, k in ['name', 'port', 'protocol', 'allowedRoutes']) && \
                    ((l.protocol == 'UDP' && !(l.port in {udp_reserved})) || (l.protocol == 'TCP' && !(l.port in {tcp_reserved}) && !(l.port >= 19000 && l.port <= 19003))) && l.allowedRoutes == {same_ns}))))) || \
            (request.resource.resource == 'httproutes' && !(variables.objName in {tailnet_names}) && (request.operation == 'DELETE' || (object.spec.parentRefs.all(p, (!has(p.namespace) || p.namespace == '{gw}') && p.name != '{tailnet_gw}') && object.spec.rules.all(r, (!has(r.backendRefs) || r.backendRefs.all(b, b.kind == 'Backend' && b.group == 'gateway.envoyproxy.io' && (!has(b.namespace) || b.namespace == '{gw}'))) && (!has(r.filters) || r.filters.all(f, f.type == 'RequestRedirect' && f.requestRedirect.scheme == 'https')))))) || \
            (request.resource.resource in ['tcproutes', 'udproutes'] && (request.operation == 'DELETE' || (object.spec.all(k, k in ['parentRefs', 'rules']) && object.spec.parentRefs.all(p, (!has(p.namespace) || p.namespace == '{gw}') && p.name.endsWith('-ports')) && size(object.spec.rules) == 1 && \
                object.spec.rules.all(r, r.all(k, k == 'backendRefs') && r.backendRefs.all(b, b.kind == 'Backend' && b.group == 'gateway.envoyproxy.io' && (!has(b.namespace) || b.namespace == '{gw}')))))) || \
            (request.resource.resource == 'backends' && variables.objName == '{waf}' && (request.operation == 'DELETE' || object.spec == {waf_backend})) || \
            (request.resource.resource == 'backends' && !(variables.objName in {tailnet_names}) && variables.objName != '{waf}' && (request.operation == 'DELETE' || object.spec.endpoints.all(e, has(e.fqdn) && e.fqdn.hostname.endsWith('.svc.cluster.local') && size(e.fqdn.hostname.split('.')) == 5 && !(e.fqdn.hostname.split('.')[1] in {system_ns})))) || \
            (request.resource.resource == 'certificates' && variables.objName.startsWith('{tls}') && (request.operation == 'DELETE' || (object.spec.secretName == variables.objName && object.spec.issuerRef.kind == 'ClusterIssuer' && object.spec.issuerRef.name in ['pyo-letsencrypt', 'pyo-selfsigned']))) || \
            (request.resource.resource == 'secrets' && variables.objName.startsWith('{tls}') && (request.operation == 'DELETE' || object.type == 'kubernetes.io/tls')) || \
            (request.resource.resource == 'configmaps' && variables.objName == '{pull}' && (request.operation == 'DELETE' || (has(object.data) && object.data.all(k, k == 'ca.crt')))))",
            gw = system::GATEWAY_NS,
            http_gw = system::HTTP_GATEWAY,
            // Pyo's own HTTPS listeners on the tailnet (system.rs): these
            // exact objects only, nothing else may attach to their Gateway.
            tailnet_gw = system::TAILNET_GATEWAY,
            tailnet_spec = cel_literal(&system::tailnet_gateway_spec()),
            tailnet_policy = cel_literal(&system::tailnet_policy_spec(&[])),
            // With the servers' own rule: the spec as written, its addresses
            // only the servers' first /31s (/127s) (system.rs
            // `tailnet_server_cidrs`).
            tailnet_policy_servers = cel_literal(&system::tailnet_policy_spec(&["SERVERS".into()])).replace("dyn([dyn('SERVERS')])", &format!("dyn({TAILNET_SERVERS})")),
            servers = TAILNET_SERVERS,
            server_v4 = cel_literal(&json!(system::TAILNET_SERVER_CIDR_V4)),
            server_v6 = cel_literal(&json!(system::TAILNET_SERVER_CIDR_V6)),
            tailnet_names = cel_literal(&json!(system::TAILNET_ROUTES.iter().map(|r| r.0).collect::<Vec<_>>())),
            tailnet_routes = system::TAILNET_ROUTES.iter().map(|(name, section, host, port)| format!(
                "(request.resource.resource == 'backends' && variables.objName == '{name}' && (request.operation == 'DELETE' || object.spec == {spec})) || \
                 (request.resource.resource == 'httproutes' && variables.objName == '{name}' && (request.operation == 'DELETE' || (size(object.spec.parentRefs) == 1 && object.spec.parentRefs[0].name == '{gw}' && object.spec.parentRefs[0].sectionName == '{section}' && (!has(object.spec.parentRefs[0].namespace) || object.spec.parentRefs[0].namespace == '{ns}') && \
                    size(object.spec.rules) == 1 && object.spec.rules.all(r, !has(r.filters) && size(r.backendRefs) == 1 && r.backendRefs.all(b, b.kind == 'Backend' && b.group == 'gateway.envoyproxy.io' && b.name == '{name}' && (!has(b.namespace) || b.namespace == '{ns}'))))))",
                spec = cel_literal(&system::tailnet_backend_spec(host, *port)), gw = system::TAILNET_GATEWAY, ns = system::GATEWAY_NS)).collect::<Vec<_>>().join(" || "),
            // Websites on the servers' Tailscale addresses (system.rs): this
            // Service only, onto the proxies, the websites' ports, and
            // Tailscale's addresses alone.
            tw_svc = system::TAILNET_WEB_SERVICE,
            tw_keys = cel_literal(&json!(["type", "externalIPs", "externalTrafficPolicy", "selector", "ports"].iter().chain(system::TAILNET_WEB_DEFAULTED.iter()).collect::<Vec<_>>())),
            tw_selector = cel_literal(&system::proxy_pods()["matchLabels"]),
            tw_ports = cel_literal(&system::tailnet_web_ports()),
            tw_v4 = cel_literal(&json!(system::TAILNET_IPV4_RE)),
            tw_v6 = cel_literal(&json!(system::TAILNET_IPV6_RE)),
            label = system::GATEWAY_LABEL,
            tls = system::TLS_SECRET_PREFIX,
            pull = system::ORIGIN_PULL_SECRET,
            // Published on the servers or not (the cluster's public traffic).
            proxies = cel_literal(&json!([system::envoy_proxy(geoip_image, true)["spec"], system::envoy_proxy(geoip_image, false)["spec"]])),
            proxy_keys = cel_literal(&json!(system::envoy_proxy(geoip_image, true)["spec"].as_object().map(|m| m.keys().cloned().collect::<Vec<_>>()).unwrap_or_default())),
            ctp_http = cel_literal(&json!(system::client_policy_variants(false))),
            ctp_https = cel_literal(&json!(system::client_policy_variants(true))),
            ports_label = system::L4_GATEWAY,
            pp_optional = cel_literal(&json!({ "optional": true })),
            fw_policy = system::FIREWALL_POLICY,
            fw_override = cel_literal(&system::firewall_response_override()),
            waf = system::WAF_NAME,
            waf_backend = cel_literal(&system::waf_backend_spec()),
            waf_keys = cel_literal(&json!(system::waf_ext_proc(true).as_object().map(|m| m.keys().cloned().collect::<Vec<_>>()).unwrap_or_default())),
            // The rest of the WAF's call exactly, failing closed (and
            // waiting longer) for a block-mode service, open for any other:
            // as the mode its annotation gives the WAF says.
            waf_pinned = cel_literal(&json!(waf_pinned_for(true).keys().cloned().collect::<Vec<_>>())),
            waf_block = cel_literal(&Value::Object(waf_pinned_for(true))),
            waf_pass = cel_literal(&Value::Object(waf_pinned_for(false))),
            waf_annotation = system::WAF_ANNOTATION,
            waf_mark = cel_literal(&json!(system::WAF_BLOCK_MARK)),
            proxy_pods = cel_literal(&system::proxy_pods()),
            udp_reserved = format!("{:?}", crate::SERVER_UDP_PORTS),
            tcp_reserved = format!("{:?}", [&[80, 443, 10080, 10443][..], &crate::SERVER_TCP_PORTS[..], &system::TAILNET_PORTS[..]].concat()),
            same_ns = cel_literal(&json!({ "namespaces": { "from": "Same" } })),
            system_ns = cel_literal(&json!(["kube-system", "kube-public", "kube-node-lease", "default", "pyo-system", system::GATEWAY_NS, "cert-manager"])),
            http_spec = cel_literal(&system::http_gateway_spec())) },
        // Cluster-wide objects it writes, with exactly Pyo's content.
        { "name": "clusterWideAllowed", "expression": format!("variables.ns != '' || !(request.resource.resource in ['clusterissuers', 'runtimeclasses', 'gatewayclasses']) || request.operation == 'DELETE' || \
            (request.resource.resource == 'clusterissuers' && ({issuers})) || \
            (request.resource.resource == 'gatewayclasses' && variables.objName == 'pyo' && object.spec == {class}) || \
            (request.resource.resource == 'runtimeclasses' && variables.objName == 'nvidia' && object.handler == 'nvidia' && object.scheduling == {scheduling} && !has(object.overhead))",
            issuers = system::cluster_issuers().iter().map(|(n, spec)| format!("(variables.objName == '{n}' && object.spec == {})", cel_literal(spec))).collect::<Vec<_>>().join(" || "),
            class = cel_literal(&system::gateway_class()["spec"]),
            scheduling = cel_literal(&system::gpu_runtime_scheduling())) },
        // Draining a server: evicting or deleting its pods, wherever they are.
        { "name": "drain", "expression": "(request.resource.resource == 'pods' && variables.sub == 'eviction') || (request.resource.resource == 'pods' && variables.sub == '' && request.operation == 'DELETE')" },
        { "name": "hasPods", "expression": "request.operation != 'DELETE' && variables.sub == '' && object.kind in ['Pod', 'Deployment', 'StatefulSet', 'DaemonSet', 'ReplicaSet', 'Job', 'CronJob']" },
        { "name": "spec", "expression": format!("variables.hasPods ? ({pod_spec}) : {{}}") },
        { "name": "containers", "expression": "variables.hasPods ? (has(variables.spec.initContainers) ? variables.spec.initContainers : []) + variables.spec.containers : []" },
        // The pod template's annotations (AppArmor's old way).
        { "name": "podAnnotations", "expression": "!variables.hasPods ? {} : object.kind == 'Pod' ? (has(object.metadata.annotations) ? object.metadata.annotations : {}) : object.kind == 'CronJob' ? (has(object.spec.jobTemplate.spec.template.metadata) && has(object.spec.jobTemplate.spec.template.metadata.annotations) ? object.spec.jobTemplate.spec.template.metadata.annotations : {}) : (has(object.spec.template.metadata) && has(object.spec.template.metadata.annotations) ? object.spec.template.metadata.annotations : {})" },
        { "name": "safeSysctls", "expression": cel_literal(&json!(SAFE_SYSCTLS)) },
        // A namespace's Pod Security level before and after, and their order.
        { "name": "psaNew", "expression": format!("request.resource.resource == 'namespaces' && request.operation != 'DELETE' && has(object.metadata.labels) ? object.metadata.labels[?'{PSA_ENFORCE}'].orValue('') : ''") },
        { "name": "psaOld", "expression": format!("request.resource.resource == 'namespaces' && request.operation == 'UPDATE' && has(oldObject.metadata.labels) ? oldObject.metadata.labels[?'{PSA_ENFORCE}'].orValue('') : ''") },
        { "name": "psaRank", "expression": "{'baseline': 1, 'restricted': 2}" },
        // System bundles: one of a bundle's objects, as written; and their pods.
        { "name": "bundleAllowed", "expression": bundles::allowed_cel(&bundles) },
        { "name": "bundlePods", "expression": bundles::pods_cel(&bundles) },
        // Pyo's own: BuildKit (privileged, its cache on the host), Vector
        // (the host's logs and figures, read only), the address check (the
        // host's network) and the GPU device plugin (the kubelet's socket).
        { "name": "systemException", "expression": "variables.hasPods && (\
            (variables.ns == 'pyo-system' && object.kind == 'Deployment' && variables.objName.startsWith('pyo-builder-') && (!has(variables.spec.volumes) || variables.spec.volumes.all(v, !has(v.hostPath) || v.hostPath.path.startsWith('/var/lib/pyo-builder-'))) && variables.containers.all(c, c.name == 'buildkitd' || !has(c.securityContext) || !has(c.securityContext.privileged) || !c.securityContext.privileged) && !has(variables.spec.hostNetwork) && !has(variables.spec.hostPID)) || \
            (variables.ns == 'pyo-system' && object.kind == 'DaemonSet' && variables.objName == 'pyo-vector' && (!has(variables.spec.volumes) || variables.spec.volumes.all(v, !has(v.hostPath) || v.hostPath.path in ['/var/lib/pyo-vector', '/var/log', '/proc', '/sys'])) && variables.containers.all(c, !has(c.securityContext) || !has(c.securityContext.privileged) || !c.securityContext.privileged) && variables.containers.all(c, !has(c.volumeMounts) || c.volumeMounts.all(m, !(m.name in ['var-log', 'host-proc', 'host-sys']) || (has(m.readOnly) && m.readOnly))) && !has(variables.spec.hostNetwork)) || \
            (variables.ns == 'pyo-system' && object.kind == 'Pod' && variables.objName.startsWith('pyo-locate-') && (!has(variables.spec.volumes) || variables.spec.volumes.all(v, !has(v.hostPath))) && variables.containers.all(c, !has(c.securityContext) || !has(c.securityContext.privileged) || !c.securityContext.privileged) && !has(variables.spec.hostPID)) || \
            (variables.ns == 'kube-system' && object.kind == 'DaemonSet' && variables.objName == 'nvidia-device-plugin' && (!has(variables.spec.volumes) || variables.spec.volumes.all(v, !has(v.hostPath) || v.hostPath.path == '/var/lib/kubelet/device-plugins')) && variables.containers.all(c, !has(c.securityContext) || !has(c.securityContext.privileged) || !c.securityContext.privileged) && !has(variables.spec.hostNetwork)))" },
    ]);
    vec![
        json!({
            "apiVersion": "admissionregistration.k8s.io/v1", "kind": "ValidatingAdmissionPolicy",
            "metadata": { "name": ADMISSION_POLICY, "labels": labels() },
            "spec": {
                "failurePolicy": "Fail",
                "matchConstraints": { "resourceRules": [{ "apiGroups": ["*"], "apiVersions": ["*"], "operations": ["CREATE", "UPDATE", "DELETE"], "resources": ["*/*"] }] },
                "matchConditions": [{ "name": "pyo-agent", "expression": format!("request.userInfo.username == '{AGENT_USER}'") }],
                "variables": variables,
                "validations": validations,
            },
        }),
        json!({
            "apiVersion": "admissionregistration.k8s.io/v1", "kind": "ValidatingAdmissionPolicyBinding",
            "metadata": { "name": ADMISSION_POLICY, "labels": labels() },
            "spec": { "policyName": ADMISSION_POLICY, "validationActions": ["Deny"] },
        }),
    ]
}

// ── The install, and refreshing it ─────────────────────────────────────────
//
// What `/install/agent.yaml` sets up that bounds the agent: its roles
// (`objects`), its admission policy (`admission`, which pins the system
// images) and the image pins in its Deployment's env. Their hash is stamped
// on the admission policy and on the Deployment ([`INSTALL_ANNOTATION`]);
// the agent reports it, so Pyo knows when a cluster's install is older than
// its own.
//
// With the optional [`SELF_UPDATE_ROLE`] bound, the agent brings the
// admission policy and its image pins up to date itself (`RefreshInstall`):
// only when its roles are already as Pyo's are (it may read them, never
// write them). Without it, the cluster's connect command does.

/// On the admission policy and the agent's Deployment: the hash of the
/// install they're from.
pub const INSTALL_ANNOTATION: &str = "pyo.dev/install";
/// Optional: the agent may rewrite its own admission policy (and read its
/// roles, to know they're current). Deleting its binding takes that away.
pub const SELF_UPDATE_ROLE: &str = "pyo-agent-self-update";
/// The agent Deployment's env that pins system images: the only part of
/// the Deployment a refresh changes.
pub const PINNED_ENV: [&str; 9] = [
    "PYO_WAF_IMAGE",
    "PYO_GEOIP_IMAGE",
    "PYO_DB_CONTROLLER_IMAGE",
    "PYO_KV_IMAGE",
    "PYO_POSTGRES_IMAGE_16",
    "PYO_POSTGRES_IMAGE_17",
    "PYO_POSTGRES_IMAGE_18",
    "PYO_MYSQL_OPERATOR_IMAGE",
    "PYO_MYSQL_BACKUP_IMAGE",
];

/// The optional role (and its binding) that lets the agent refresh its
/// admission policy: update only that policy and its binding, by name; read
/// only its own roles and the charts installed with it, by name.
pub fn self_update_objects() -> Vec<Value> {
    let own = objects(true);
    let names = |kinds: &[&str], namespaced: bool| -> Vec<String> {
        let mut v: Vec<String> = own
            .iter()
            .filter(|o| {
                kinds.contains(&o["kind"].as_str().unwrap_or(""))
                    && o["metadata"]["namespace"].is_string() == namespaced
            })
            .filter_map(|o| o["metadata"]["name"].as_str().map(str::to_owned))
            .collect();
        if kinds.contains(&"ClusterRole") {
            v.push(SELF_UPDATE_ROLE.to_owned());
        }
        v.sort();
        v.dedup();
        v
    };
    vec![
        cluster_role(
            SELF_UPDATE_ROLE,
            json!([
                { "apiGroups": ["admissionregistration.k8s.io"], "resources": ["validatingadmissionpolicies", "validatingadmissionpolicybindings"], "verbs": ["get", "update"], "resourceNames": [ADMISSION_POLICY] },
                { "apiGroups": ["rbac.authorization.k8s.io"], "resources": ["clusterroles", "clusterrolebindings"], "verbs": ["get"], "resourceNames": names(&["ClusterRole", "ClusterRoleBinding"], false) },
                { "apiGroups": ["rbac.authorization.k8s.io"], "resources": ["roles", "rolebindings"], "verbs": ["get"], "resourceNames": names(&["Role", "RoleBinding"], true) },
                { "apiGroups": ["helm.cattle.io"], "resources": ["helmcharts"], "verbs": ["get"], "resourceNames": names(&["HelmChart"], true) },
            ]),
        ),
        cluster_binding(SELF_UPDATE_ROLE),
    ]
}

/// What the install sets up that bounds the agent, and its hash.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InstallParts {
    /// Hex, stamped as [`INSTALL_ANNOTATION`].
    pub hash: String,
    /// Its roles and bindings (and the charts installed with it): only
    /// read by a refresh, never written.
    pub access: Vec<Value>,
    /// The admission policy (stamped) and its binding.
    pub admission: Vec<Value>,
    /// The Deployment's [`PINNED_ENV`] values.
    pub pins: std::collections::BTreeMap<String, String>,
}

impl InstallParts {
    /// `upgrades`: the cluster Pyo runs on. `self_update`: with the
    /// optional [`SELF_UPDATE_ROLE`] bound.
    pub fn new(
        upgrades: bool,
        self_update: bool,
        waf_image: &str,
        geoip_image: &str,
        db: &system::DbImages,
    ) -> Self {
        let mut access = objects(upgrades);
        if self_update {
            access.extend(self_update_objects());
        }
        let mut admission = admission(waf_image, geoip_image);
        let [pg16, pg17, pg18] = db.postgres.clone();
        let pins: std::collections::BTreeMap<String, String> = PINNED_ENV
            .iter()
            .map(|k| (*k).to_owned())
            .zip([
                waf_image.to_owned(),
                geoip_image.to_owned(),
                db.controller.clone(),
                db.kv.clone(),
                pg16,
                pg17,
                pg18,
                db.mysql_operator.clone(),
                db.mysql_backup.clone(),
            ])
            .collect();
        let hash = install_hash(&access, &admission, &pins);
        admission[0]["metadata"]["annotations"] = json!({ INSTALL_ANNOTATION: hash });
        InstallParts {
            hash,
            access,
            admission,
            pins,
        }
    }
}

/// A short hash (16 hex digits) of what bounds the agent: any change to its
/// roles, its admission policy or its image pins changes it.
pub fn install_hash(
    access: &[Value],
    admission: &[Value],
    pins: &std::collections::BTreeMap<String, String>,
) -> String {
    use sha2::{Digest, Sha256};
    let doc = json!([access, admission, pins]).to_string();
    Sha256::digest(doc.as_bytes())
        .iter()
        .take(8)
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Whether a cluster's install, as its agent reports it, is Pyo's.
pub fn install_current(scope: &AgentScope, expected: &str) -> bool {
    match scope.admission.as_str() {
        "enforced" => scope.install == expected,
        // The cluster has admission policies but not the agent's: only its
        // connect command puts it back.
        "missing" => false,
        // Too old for one: nothing pins the system images.
        _ => true,
    }
}

/// The customer's switches in `pyo-system/pyo-policy`, as the agent read
/// them. Missing keys are off.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Policy {
    /// `terminal: "off"`: no terminals into services.
    #[serde(default)]
    pub no_terminal: bool,
    /// `forceRemoveServers: "off"`: servers are only removed after their
    /// services moved away.
    #[serde(default)]
    pub no_force_remove: bool,
    /// `readOnly: "true"`: the agent reports, and changes nothing.
    #[serde(default)]
    pub read_only: bool,
}

impl Policy {
    /// From the ConfigMap's data.
    pub fn from_data(data: &std::collections::BTreeMap<String, String>) -> Self {
        let is = |k: &str, v: &[&str]| {
            data.get(k)
                .is_some_and(|x| v.contains(&x.trim().to_ascii_lowercase().as_str()))
        };
        Policy {
            no_terminal: is("terminal", &["off", "false", "no", "disabled"]),
            no_force_remove: is("forceRemoveServers", &["off", "false", "no", "disabled"]),
            read_only: is("readOnly", &["true", "on", "yes"]),
        }
    }
}

/// What the agent may do in its cluster, as it found out: sent with each
/// heartbeat.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentScope {
    /// "limited" (the roles above) or "full" (cluster-admin).
    pub access: String,
    /// Whether the admission policy holds it to them: "enforced",
    /// "missing" (not installed) or "unsupported" (the cluster is too old).
    pub admission: String,
    /// It may manage servers (the optional role is bound).
    #[serde(default)]
    pub servers: bool,
    /// The customer's switches.
    #[serde(default)]
    pub policy: Policy,
    /// The gateway's reach, when it runs: none until reported.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proxy: Option<ProxyScope>,
    /// The install its admission policy and Deployment are from
    /// ([`INSTALL_ANNOTATION`]); empty when they aren't stamped alike.
    #[serde(default)]
    pub install: String,
    /// It may refresh its admission policy ([`SELF_UPDATE_ROLE`] is bound).
    #[serde(default)]
    pub self_update: bool,
}

/// What the gateway could get at if it were broken into.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProxyScope {
    /// Its controller may read Secrets outside its own namespace.
    pub controller_reads_other_secrets: bool,
    /// The proxies (facing the internet) carry cluster credentials.
    pub proxies_have_credentials: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pods_are_held_to_pod_security_baseline() {
        let policy = admission(system::WAF_IMAGE_DEFAULT, system::GEOIP_IMAGE_DEFAULT);
        let text = policy[0]["spec"]["validations"].to_string();
        for field in [
            "capabilities.add",
            "seccompProfile.type != 'Unconfined'",
            "appArmorProfile.type != 'Unconfined'",
            "procMount == 'Default'",
            "hostPort == 0",
            "sysctls.all",
            "container.apparmor.security.beta.kubernetes.io/",
        ] {
            assert!(text.contains(field), "the pod check covers {field}");
        }
        // Pod Security labels: never loosened.
        assert!(text.contains("psaRank[?variables.psaNew].orValue(0) >= variables.psaRank[?variables.psaOld].orValue(0)"));
        // Teams' databases' namespaces are baseline; projects' get it from the rollout.
        assert_eq!(
            managed_namespace("pyo-db-abc")["metadata"]["labels"][PSA_ENFORCE],
            "baseline"
        );
        assert!(
            managed_namespace("pyo-system")["metadata"]["labels"]
                .get(PSA_ENFORCE)
                .is_none(),
            "Pyo's own runs BuildKit and Vector"
        );
    }

    #[test]
    fn cluster_dns_only_its_scale_and_budget() {
        let objects = objects(false);
        let ks = objects
            .iter()
            .find(|o| o["kind"] == "Role" && o["metadata"]["namespace"] == "kube-system")
            .unwrap();
        let rules = ks["rules"].as_array().unwrap();
        // Deployments: only CoreDNS's scale, by name; nothing of its pod template.
        let deployments: Vec<&Value> = rules
            .iter()
            .filter(|r| {
                r["resources"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|x| x.as_str().unwrap().starts_with("deployments"))
            })
            .collect();
        assert_eq!(deployments.len(), 1);
        assert_eq!(deployments[0]["resources"], json!(["deployments/scale"]));
        assert_eq!(deployments[0]["resourceNames"], json!(["coredns"]));
        assert!(
            rules
                .iter()
                .any(|r| r["resources"] == json!(["poddisruptionbudgets"])
                    && r["verbs"] == json!(["get", "patch", "delete"])
                    && r["resourceNames"] == json!(["pyo-coredns"]))
        );
        let policy = admission("w", "g")[0].to_string();
        assert!(policy.contains("variables.sub == 'scale' && variables.objName == 'coredns' && request.operation == 'UPDATE' && object.spec.replicas in [1, 2]"));
        assert!(policy.contains("variables.objName == 'pyo-coredns' && (request.operation == 'DELETE' || object.spec == {'minAvailable': dyn(1)"));
    }

    #[test]
    fn roles_never_reach_secrets_or_configmaps_cluster_wide() {
        for o in objects(false) {
            // The write roles are bound only in managed (or bundle) namespaces.
            if o["kind"] != "ClusterRole"
                || o["metadata"]["name"] == WRITE_ROLE
                || o["metadata"]["name"] == bundles::WRITE_ROLE
            {
                continue;
            }
            for r in o["rules"].as_array().unwrap() {
                let res: Vec<&str> = r["resources"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|x| x.as_str().unwrap())
                    .collect();
                assert!(
                    !res.contains(&"secrets")
                        && !res.contains(&"configmaps")
                        && !res.contains(&"*"),
                    "{}: {res:?}",
                    o["metadata"]["name"]
                );
            }
        }
        // The write role, bound only in managed namespaces, has no ConfigMaps.
        let write = objects(false)
            .into_iter()
            .find(|o| o["metadata"]["name"] == WRITE_ROLE)
            .unwrap();
        assert!(!write.to_string().contains("configmaps"));
        assert!(
            !objects(false)
                .iter()
                .any(|o| o.to_string().contains("cluster-admin"))
        );
        // bind and escalate only on named roles.
        for o in objects(true) {
            for r in o["rules"].as_array().into_iter().flatten() {
                let verbs = r["verbs"].to_string();
                if verbs.contains("bind") || verbs.contains("escalate") {
                    assert!(r["resourceNames"].is_array(), "{r}");
                }
            }
        }
    }

    #[test]
    fn only_a_deleted_projects_namespace_is_deleted() {
        let labels = |pairs: &[(&str, &str)]| {
            pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect::<std::collections::BTreeMap<_, _>>()
        };
        let project = labels(&[
            (MANAGED_LABEL, "true"),
            (PROJECT_LABEL, "prj_1"),
            ("app.kubernetes.io/managed-by", "pyo"),
        ]);
        assert_eq!(project_namespace_refusal("e2e", &project, "prj_1"), None);
        // Another project's, or none's.
        assert!(project_namespace_refusal("e2e", &project, "prj_2").is_some());
        assert!(project_namespace_refusal("e2e", &project, "").is_some());
        assert!(
            project_namespace_refusal("e2e", &labels(&[(MANAGED_LABEL, "true")]), "prj_1")
                .is_some()
        );
        // Not Pyo's to manage, or a system bundle's.
        assert!(
            project_namespace_refusal("e2e", &labels(&[(PROJECT_LABEL, "prj_1")]), "prj_1")
                .is_some()
        );
        let mut bundle = project.clone();
        bundle.insert(bundles::BUNDLE_LABEL.into(), "waf".into());
        assert!(project_namespace_refusal("e2e", &bundle, "prj_1").is_some());
        // The cluster's and Pyo's own, whatever their labels say.
        for ns in [
            "default",
            "kube-system",
            "kube-anything",
            "pyo-system",
            "pyo-gateway",
            "pyo-waf",
            "pyo-databases",
            "pyo-db-q9pnllqh1p5f",
            "cert-manager",
        ] {
            assert!(
                project_namespace_refusal(ns, &project, "prj_1").is_some(),
                "{ns}"
            );
        }
        // The agent may delete namespaces; the policy says which.
        let ns_role = objects(false)
            .into_iter()
            .find(|o| o["metadata"]["name"] == "pyo-agent-namespaces")
            .unwrap();
        assert!(
            ns_role["rules"][0]["verbs"]
                .as_array()
                .unwrap()
                .contains(&json!("delete"))
        );
        let policy = admission("w", "g")[0].to_string();
        assert!(policy.contains("request.operation != 'DELETE' || (!(variables.reqName in [dyn('default'), dyn('pyo-system'), dyn('pyo-gateway'), dyn('cert-manager')])"));
        assert!(
            policy.contains(
                "[dyn('kube-'), dyn('pyo-db-')].exists(p, variables.reqName.startsWith(p))"
            )
        );
        assert!(policy.contains("(variables.reqName in [dyn('pyo-waf'), dyn('pyo-databases')] && oldObject.metadata.labels[?'pyo.dev/bundle'].orValue('') != '')"));
        assert!(policy.contains("oldObject.metadata.labels[?'pyo/project-id'].orValue('') != ''"));
    }

    #[test]
    fn volume_claims_are_deleted_only_when_labelled_in_a_project() {
        let write = objects(false)
            .into_iter()
            .find(|o| o["metadata"]["name"] == WRITE_ROLE)
            .unwrap();
        assert!(write["rules"].as_array().unwrap().iter().any(|r| {
            r["resources"]
                .as_array()
                .unwrap()
                .contains(&json!("persistentvolumeclaims"))
                && r["verbs"].as_array().unwrap().contains(&json!("delete"))
        }));
        let policy = admission("w", "g")[0].to_string();
        assert!(policy.contains("request.resource.resource != 'persistentvolumeclaims'"));
        assert!(policy.contains("oldObject.metadata.labels[?'pyo/volume-id'].orValue('') != ''"));
        assert!(
            policy.contains("namespaceObject.metadata.labels[?'pyo/project-id'].orValue('') != ''")
        );
    }

    #[test]
    fn the_waf_image_is_the_installations() {
        let img = "registry.example.com/pyo/waf:7@sha256:aa";
        let policy = admission(img, "g")[0].to_string();
        assert!(
            policy.contains(&format!("containers[0].image == '{img}'")),
            "the configured image is pinned"
        );
        assert!(
            !policy.contains("ghcr.io/upsetdev/pyo/waf"),
            "not a built-in one"
        );
        assert_eq!(
            system::waf_daemonset(img, "g")["spec"]["template"]["spec"]["containers"][0]["image"],
            img
        );
        // A quote can't end the CEL string.
        assert!(
            !admission("x' || true || 'y", "g")[0]
                .to_string()
                .contains("x' ||")
        );
        // In the gateway's namespace, the agent writes the routes' WAF
        // policies and nothing of the WAF's own (that's the bundle's).
        let gw = objects(false)
            .into_iter()
            .find(|o| o["kind"] == "Role" && o["metadata"]["namespace"] == system::GATEWAY_NS)
            .unwrap();
        let verbs = |res: &str| -> Vec<String> {
            gw["rules"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|r| r["resources"].as_array().unwrap().iter().any(|x| x == res))
                .flat_map(|r| {
                    r["verbs"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|v| v.as_str().unwrap().to_owned())
                })
                .collect()
        };
        assert!(verbs("envoyextensionpolicies").contains(&"patch".to_owned()));
        assert!(
            verbs("daemonsets").is_empty()
                && verbs("deployments").is_empty()
                && verbs("poddisruptionbudgets").is_empty()
        );
        // Its own namespace: the DaemonSet pinned, with the proxies' placement;
        // routes send to it through the Backend in the gateway's namespace.
        assert!(policy.contains("variables.ns == 'pyo-waf' && variables.objName == 'pyo-waf'"));
        assert!(policy.contains(
            "'key': dyn('pyo/builder'), 'operator': dyn('NotIn'), 'values': dyn([dyn('true')])"
        ));
        assert!(policy.contains("b.name == 'pyo-waf' && b.kind == 'Backend'"));
        assert!(policy.contains("'hostname': dyn('pyo-waf.pyo-waf.svc.cluster.local')"));
    }

    /// A service's firewall and WAF policies reach only the route they're
    /// named after (its own), and a WAF policy carries the service's WAF.
    #[test]
    fn service_policies_stay_on_their_own_route() {
        let all = admission(system::WAF_IMAGE_DEFAULT, system::GEOIP_IMAGE_DEFAULT)[0].to_string();
        assert!(all.contains("size(object.spec.targetRefs) == 1 && object.spec.targetRefs[0].name == variables.objName"));
        let clause = |res: &str| -> String {
            let start = all
                .find(&format!(
                    "request.resource.resource == '{res}' && variables.objName != "
                ))
                .or_else(|| {
                    all.find(&format!(
                        "request.resource.resource == '{res}' && (request.operation"
                    ))
                })
                .unwrap();
            all[start..start + 400].to_owned()
        };
        assert!(clause("securitypolicies").contains("variables.ownRoute"));
        let eep = clause("envoyextensionpolicies");
        assert!(
            eep.contains("variables.toWebRoute && variables.ownRoute"),
            "{eep}"
        );
        assert!(
            eep.contains("annotations[?'pyo.dev/waf'].orValue('') != ''"),
            "{eep}"
        );
    }

    /// Websites on the servers' Tailscale addresses: the agent writes one
    /// Service in pyo-gateway, by name, pinned to the proxies, the
    /// websites' ports and Tailscale's addresses; no other Service anywhere
    /// outside managed namespaces.
    #[test]
    fn the_tailnet_websites_service_is_pinned() {
        let objects = objects(false);
        let gw = objects
            .iter()
            .find(|o| o["kind"] == "Role" && o["metadata"]["namespace"] == system::GATEWAY_NS)
            .unwrap();
        let svc: Vec<&Value> = gw["rules"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|r| r["resources"] == json!(["services"]))
            .collect();
        assert_eq!(
            svc,
            [
                &json!({ "apiGroups": [""], "resources": ["services"], "verbs": ["create"] }),
                &json!({ "apiGroups": [""], "resources": ["services"], "verbs": ["get", "patch", "delete"], "resourceNames": [system::TAILNET_WEB_SERVICE] }),
            ]
        );
        let policy = admission("w", "g");
        let all = policy[0]["spec"]["variables"]
            .as_array()
            .unwrap()
            .iter()
            .find(|v| v["name"] == "gatewayAllowed")
            .unwrap()["expression"]
            .as_str()
            .unwrap();
        // Services appear in no other variable or validation.
        assert!(
            !policy[0]["spec"]["validations"]
                .to_string()
                .contains("'services'")
        );
        let start = all
            .find("(request.resource.resource == 'services'")
            .expect("the Service's clause");
        let clause = &all[start..start + all[start..].find(")))) || ").unwrap()];
        // Only in the gateway's namespace, and only this name.
        assert!(all.starts_with("variables.ns == 'pyo-gateway' && ("));
        for part in [
            "variables.objName == 'pyo-tailnet-web' && variables.sub == ''".to_owned(),
            "object.spec.type == 'ClusterIP' && object.spec.externalTrafficPolicy == 'Local'"
                .to_owned(),
            format!(
                "object.spec.selector == {}",
                cel_literal(&system::proxy_pods()["matchLabels"])
            ),
            format!(
                "object.spec.ports == {}",
                cel_literal(&system::tailnet_web_ports())
            ),
            "size(object.spec.externalIPs) >= 1".to_owned(),
            format!(
                "ip.matches({}) || ip.matches({})",
                cel_literal(&json!(system::TAILNET_IPV4_RE)),
                cel_literal(&json!(system::TAILNET_IPV6_RE))
            ),
        ] {
            assert!(clause.contains(&part), "{part}\n{clause}");
        }
        // Fields beyond these (load balancer, node ports, session settings) are refused.
        assert!(clause.contains("object.spec.all(k, k in [dyn('type'), dyn('externalIPs'), dyn('externalTrafficPolicy'), dyn('selector'), dyn('ports'), dyn('clusterIP')"));
        for k in [
            "loadBalancer",
            "nodePort",
            "sessionAffinityConfig",
            "healthCheckNodePort",
            "allocateLoadBalancerNodePorts",
        ] {
            assert!(!clause.contains(k), "{k}");
        }
    }

    #[test]
    fn the_geoip_image_is_the_installations() {
        let img = "registry.example.com/pyo/geoip:7@sha256:bb";
        let policy = admission("w", img)[0].to_string();
        assert!(
            policy.contains(&format!("'reference': dyn('{img}')")),
            "the configured image is pinned"
        );
        assert!(
            !policy.contains("ghcr.io/upsetdev/pyo/geoip"),
            "not a built-in one"
        );
        assert_eq!(
            system::envoy_proxy(img, true)["spec"]["provider"]["kubernetes"]["envoyDaemonSet"]["pod"]
                ["volumes"][0]["image"]["reference"],
            img
        );
        // The WAF mounts the same image, and only that one.
        assert!(policy.contains(&format!(
            "variables.spec.volumes[2].image.reference == '{img}'"
        )));
        // A quote can't end the CEL string.
        assert!(
            !admission("w", "x' || true || 'y")[0]
                .to_string()
                .contains("x' ||")
        );
    }

    fn db(controller: &str) -> system::DbImages {
        system::DbImages {
            controller: controller.into(),
            kv: "kv".into(),
            postgres: ["p16".into(), "p17".into(), "p18".into()],
            mysql_operator: "mo".into(),
            mysql_backup: "mb".into(),
        }
    }

    #[test]
    fn install_hash_follows_what_bounds_the_agent() {
        let a = InstallParts::new(false, false, "w", "g", &db("c"));
        // Stable: the same install, the same hash.
        assert_eq!(a, InstallParts::new(false, false, "w", "g", &db("c")));
        assert_eq!(a.hash.len(), 16);
        assert!(a.hash.bytes().all(|b| b.is_ascii_hexdigit()));
        // Stamped on the admission policy, not on its binding.
        assert_eq!(a.admission[0]["kind"], "ValidatingAdmissionPolicy");
        assert_eq!(
            a.admission[0]["metadata"]["annotations"][INSTALL_ANNOTATION],
            json!(a.hash)
        );
        assert!(a.admission[1]["metadata"]["annotations"].is_null());
        // Every input counts: an image pin, the upgrader's role, the self-update role.
        let others = [
            InstallParts::new(false, false, "w2", "g", &db("c")),
            InstallParts::new(false, false, "w", "g2", &db("c")),
            InstallParts::new(false, false, "w", "g", &db("c2")),
            InstallParts::new(true, false, "w", "g", &db("c")),
            InstallParts::new(false, true, "w", "g", &db("c")),
        ];
        for o in &others {
            assert_ne!(o.hash, a.hash);
        }
        assert_eq!(a.pins.len(), PINNED_ENV.len());
        assert_eq!(a.pins["PYO_DB_CONTROLLER_IMAGE"], "c");
        assert_eq!(a.pins["PYO_POSTGRES_IMAGE_18"], "p18");
        assert_eq!(others[2].pins["PYO_DB_CONTROLLER_IMAGE"], "c2");
    }

    #[test]
    fn install_current_only_counts_an_enforced_policy() {
        let scope = |admission: &str, install: &str| AgentScope {
            admission: admission.into(),
            install: install.into(),
            ..Default::default()
        };
        assert!(install_current(&scope("enforced", "abc"), "abc"));
        assert!(!install_current(&scope("enforced", "old"), "abc"));
        // Not stamped (or stamped differently on the policy and the Deployment).
        assert!(!install_current(&scope("enforced", ""), "abc"));
        // A missing policy needs the connect command; a cluster too old for
        // one has nothing to bring up to date.
        assert!(!install_current(&scope("missing", ""), "abc"));
        assert!(install_current(&scope("unsupported", ""), "abc"));
    }

    #[test]
    fn self_update_is_narrow() {
        let objs = self_update_objects();
        assert_eq!(objs.len(), 2);
        let rules = objs[0]["rules"].as_array().unwrap();
        for r in rules {
            // Everything by name; writes only to the admission policy, never create/patch/escalate/bind.
            assert!(
                r["resourceNames"].as_array().is_some_and(|n| !n.is_empty()),
                "{r}"
            );
            let verbs: Vec<&str> = r["verbs"]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_str().unwrap())
                .collect();
            if r["apiGroups"] == json!(["admissionregistration.k8s.io"]) {
                assert_eq!(verbs, ["get", "update"]);
                assert_eq!(r["resourceNames"], json!([ADMISSION_POLICY]));
            } else {
                assert_eq!(verbs, ["get"], "{r}");
            }
        }
        // It may read its own roles, the self-update one included.
        let cluster = rules
            .iter()
            .find(|r| r["resources"] == json!(["clusterroles", "clusterrolebindings"]))
            .unwrap();
        for n in [
            "pyo-agent-read",
            WRITE_ROLE,
            NODES_ROLE,
            SELF_UPDATE_ROLE,
            bundles::CLUSTER_ROLE,
        ] {
            assert!(
                cluster["resourceNames"]
                    .as_array()
                    .unwrap()
                    .contains(&json!(n)),
                "{n}"
            );
        }
        let ns = rules
            .iter()
            .find(|r| r["resources"] == json!(["roles", "rolebindings"]))
            .unwrap();
        assert!(
            ns["resourceNames"]
                .as_array()
                .unwrap()
                .contains(&json!("pyo-agent-upgrade"))
        );
        // Not part of the default roles.
        assert!(
            !objects(true)
                .iter()
                .any(|o| o["metadata"]["name"] == SELF_UPDATE_ROLE)
        );
    }

    #[test]
    fn policy_switches() {
        let d = |pairs: &[(&str, &str)]| {
            pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect()
        };
        assert_eq!(Policy::from_data(&d(&[])), Policy::default());
        let p = Policy::from_data(&d(&[
            ("terminal", "off"),
            ("forceRemoveServers", "Off"),
            ("readOnly", "true"),
        ]));
        assert!(p.no_terminal && p.no_force_remove && p.read_only);
        assert!(!Policy::from_data(&d(&[("terminal", "on"), ("readOnly", "false")])).read_only);
    }
}
