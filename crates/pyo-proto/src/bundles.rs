//! System bundles: Pyo's own parts that run per cluster, installed by the
//! agent from a bundle. There are two, built in: the WAF ([`waf_bundle`],
//! there while a service on the cluster has its WAF on) and the databases
//! system ([`databases_bundle`],
//! set up with a cluster's first database and updated with Pyo).
//!
//! A bundle is its namespace, its Kubernetes objects (CRDs, cluster roles
//! and bindings, webhooks, its namespace's workloads and services), the
//! images they run, pinned, the workloads that must be ready, the kinds that
//! mean it's in use, and roles to bind in every project namespace.
//!
//! - The agent applies a bundle's objects (server-side apply, field manager
//!   [`FIELD_MANAGER`], every object labelled [`BUNDLE_LABEL`]`=<id>`), prunes
//!   the ones an update dropped, and deletes them all on removal.
//! - Its namespace isn't a managed one: the agent may write there only the
//!   bundle's own objects (the admission policy, rbac.rs), through
//!   [`WRITE_ROLE`], which has no pods or exec. Its workloads run only the
//!   bundle's containers (image, command, args and env as written).
//!   Its workloads' pods are pinned to the bundle's: every value it sets
//!   (containers, volumes, placement, security), and no fields it doesn't
//!   but the ones the cluster fills in.
//! - Cluster roles and bindings are pinned to their rules and subjects;
//!   webhooks to every value the bundle sets. CRDs are
//!   pinned by name only (their schemas are too big for the policy).
//! - `managedNamespaceBindings`: a RoleBinding of one of its cluster roles
//!   to one of its service accounts in each project namespace while it's
//!   installed (and in new ones as they're made), exactly that.

use std::sync::OnceLock;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// Every object of a bundle carries `pyo.dev/bundle=<id>`.
pub const BUNDLE_LABEL: &str = "pyo.dev/bundle";
/// Its namespace also carries its version.
pub const VERSION_LABEL: &str = "pyo.dev/bundle-version";
/// And, as JSON, the kinds that mean it's in use (the agent counts them).
pub const IN_USE_ANNOTATION: &str = "pyo.dev/bundle-in-use";
/// And, as JSON, its bindings in project namespaces (for new ones).
pub const BINDINGS_ANNOTATION: &str = "pyo.dev/bundle-bindings";
/// The field manager of the agent's applies.
pub const FIELD_MANAGER: &str = "pyo-agent";
/// What a bundle command's failure says when the agent's admission policy
/// refused one of its objects (the API words it for databases).
pub const POLICY_REFUSED: &str = "this cluster's agent policy refused it";
/// The ClusterRole the agent is bound to in a bundle's namespace.
pub const WRITE_ROLE: &str = "pyo-agent-bundle-write";
/// The ClusterRole of what it may do with bundles' cluster-wide objects.
pub const CLUSTER_ROLE: &str = "pyo-agent-bundles";
/// Namespaces that are never a bundle's own.
const SYSTEM_NAMESPACES: [&str; 7] = [
    "default",
    "kube-system",
    "kube-public",
    "kube-node-lease",
    crate::rbac::NS,
    crate::system::GATEWAY_NS,
    "cert-manager",
];
/// What a WAF bundle is called (it's built from the installation's images:
/// [`waf_bundle`]).
pub const WAF_ID: &str = "waf";
/// Workload kinds whose pods the admission policy holds to the bundle.
pub const WORKLOAD_KINDS: [&str; 3] = ["Deployment", "StatefulSet", "DaemonSet"];
/// Namespaced kinds a bundle may have in its namespace (the write role's).
/// Certificates and Issuers are cert-manager's (cert-manager.io/v1), which
/// Pyo installs on every cluster: an operator's webhook certificates.
pub const NAMESPACED_KINDS: [&str; 11] = [
    "ServiceAccount",
    "Service",
    "Secret",
    "ConfigMap",
    "Deployment",
    "StatefulSet",
    "DaemonSet",
    "Role",
    "RoleBinding",
    "Certificate",
    "Issuer",
];
/// Cluster-wide kinds a bundle may have.
pub const CLUSTER_KINDS: [&str; 7] = [
    "CustomResourceDefinition",
    "ClusterRole",
    "ClusterRoleBinding",
    "MutatingWebhookConfiguration",
    "ValidatingWebhookConfiguration",
    "ValidatingAdmissionPolicy",
    "ValidatingAdmissionPolicyBinding",
];
/// The API group of the kinds that need cert-manager.
pub const CERT_MANAGER_GROUP: &str = "cert-manager.io";

/// A bundle, as the agent installs it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SystemBundle {
    pub id: String,
    pub version: String,
    pub name: String,
    pub description: String,
    /// Its own namespace, made first and deleted last.
    pub namespace: String,
    /// Every image it runs, pinned.
    pub images: Vec<String>,
    /// Ready once these run.
    #[serde(default)]
    pub ready: Vec<ReadyCheck>,
    /// In use while any objects of these kinds exist.
    #[serde(default)]
    pub in_use: Vec<KindRef>,
    /// Bound in every project namespace while it's installed.
    #[serde(default)]
    pub managed_namespace_bindings: Vec<NamespaceBinding>,
    /// Its objects, as written (see [`SystemBundle::objects`] for what's applied).
    #[serde(default)]
    pub objects: Vec<Value>,
}

/// A workload that must be ready: "Deployment" or "DaemonSet" (or
/// "StatefulSet"), in the bundle's namespace unless it says.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReadyCheck {
    pub kind: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub namespace: Option<String>,
}

/// A RoleBinding `name` of its ClusterRole `cluster_role` to its
/// ServiceAccount `service_account` (in its namespace).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NamespaceBinding {
    pub name: String,
    pub cluster_role: String,
    pub service_account: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KindRef {
    pub api_version: String,
    pub kind: String,
}

/// One object, enough to find and delete it.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ObjectRef {
    pub api_version: String,
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub namespace: Option<String>,
    pub name: String,
}

impl ObjectRef {
    pub fn of(o: &Value) -> Self {
        ObjectRef {
            api_version: o["apiVersion"].as_str().unwrap_or("").to_owned(),
            kind: o["kind"].as_str().unwrap_or("").to_owned(),
            namespace: o["metadata"]["namespace"].as_str().map(str::to_owned),
            name: o["metadata"]["name"].as_str().unwrap_or("").to_owned(),
        }
    }

    /// (group, version, plural)
    pub fn resource(&self) -> (&str, &str, String) {
        let (group, version) = self
            .api_version
            .split_once('/')
            .unwrap_or(("", &self.api_version));
        (group, version, plural(&self.kind))
    }
}

/// What the agent reports of a bundle it finds installed (by its namespace).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BundleReport {
    pub id: String,
    pub version: String,
    /// How many objects of its in-use kinds exist.
    #[serde(default)]
    pub in_use: u32,
}

/// A kind's resource name: "Deployment" → "deployments", "NetworkPolicy" →
/// "networkpolicies", "Ingress" → "ingresses".
pub fn plural(kind: &str) -> String {
    let k = kind.to_ascii_lowercase();
    if k.ends_with('s') || k.ends_with('x') {
        format!("{k}es")
    } else if let Some(stem) = k
        .strip_suffix('y')
        .filter(|s| !s.ends_with(['a', 'e', 'o', 'u']))
    {
        format!("{stem}ies")
    } else {
        format!("{k}s")
    }
}

impl SystemBundle {
    /// Its namespace object, with its version, in-use kinds and bindings.
    pub fn namespace_object(&self) -> Value {
        json!({
            "apiVersion": "v1", "kind": "Namespace",
            "metadata": {
                "name": self.namespace,
                "labels": { BUNDLE_LABEL: self.id, VERSION_LABEL: self.version, "app.kubernetes.io/managed-by": "pyo" },
                "annotations": {
                    IN_USE_ANNOTATION: serde_json::to_string(&self.in_use).unwrap_or_default(),
                    BINDINGS_ANNOTATION: serde_json::to_string(&self.managed_namespace_bindings).unwrap_or_default(),
                },
            },
        })
    }

    /// The agent's RoleBinding to [`WRITE_ROLE`] in its namespace.
    pub fn write_binding(&self) -> Value {
        json!({
            "apiVersion": "rbac.authorization.k8s.io/v1", "kind": "RoleBinding",
            "metadata": { "name": crate::rbac::WRITE_BINDING, "namespace": self.namespace, "labels": { BUNDLE_LABEL: self.id } },
            "roleRef": { "apiGroup": "rbac.authorization.k8s.io", "kind": "ClusterRole", "name": WRITE_ROLE },
            "subjects": [{ "kind": "ServiceAccount", "name": crate::rbac::SA, "namespace": crate::rbac::NS }],
        })
    }

    /// Its RoleBindings in a project namespace.
    pub fn namespace_bindings(&self, ns: &str) -> Vec<Value> {
        bindings_in(
            &self.id,
            &self.namespace,
            &self.managed_namespace_bindings,
            ns,
        )
    }

    /// What's applied, in order: the namespace, CRDs, cluster roles and
    /// bindings, then the rest as written (webhooks and admission policies
    /// last); each labelled as the bundle's.
    pub fn objects(&self) -> Vec<Value> {
        let rank = |o: &Value| match o["kind"].as_str().unwrap_or("") {
            "CustomResourceDefinition" => 0,
            "ClusterRole" => 1,
            "ClusterRoleBinding" => 2,
            "MutatingWebhookConfiguration"
            | "ValidatingWebhookConfiguration"
            | "ValidatingAdmissionPolicy"
            | "ValidatingAdmissionPolicyBinding" => 4,
            _ => 3,
        };
        let mut rest: Vec<Value> = self.objects.clone();
        rest.sort_by_key(rank);
        let mut out = vec![self.namespace_object()];
        for mut o in rest {
            o["metadata"]["labels"][BUNDLE_LABEL] = json!(self.id);
            o["metadata"]["labels"]["app.kubernetes.io/managed-by"] = json!("pyo");
            out.push(o);
        }
        out
    }

    pub fn refs(&self) -> Vec<ObjectRef> {
        self.objects().iter().map(ObjectRef::of).collect()
    }

    /// It has cert-manager objects, so the cluster needs cert-manager first.
    pub fn needs_cert_manager(&self) -> bool {
        self.objects.iter().any(|o| {
            o["apiVersion"]
                .as_str()
                .is_some_and(|v| v.starts_with(&format!("{CERT_MANAGER_GROUP}/")))
        })
    }

    fn workloads(&self) -> impl Iterator<Item = &Value> {
        self.objects
            .iter()
            .filter(|o| WORKLOAD_KINDS.contains(&o["kind"].as_str().unwrap_or("")))
    }

    /// The containers its own workloads run (init containers too).
    pub fn containers(&self) -> Vec<Value> {
        let mut out = vec![];
        for o in self.workloads() {
            let spec = &o["spec"]["template"]["spec"];
            out.extend(
                spec["initContainers"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .chain(spec["containers"].as_array().into_iter().flatten())
                    .cloned(),
            );
        }
        out
    }

    fn names_of(&self, kind: &str) -> Vec<&str> {
        self.objects
            .iter()
            .filter(|o| o["kind"] == kind)
            .filter_map(|o| o["metadata"]["name"].as_str())
            .collect()
    }

    /// What's wrong with it, if anything: the shape the agent's roles and
    /// admission policy allow.
    pub fn check(&self) -> Result<(), String> {
        let id_ok = |s: &str| {
            !s.is_empty()
                && s.len() <= 40
                && s.bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        };
        if !id_ok(&self.id) || !id_ok(&self.namespace) {
            return Err(format!(
                "{}: the id and namespace are lowercase letters, digits and dashes",
                self.id
            ));
        }
        if self.version.is_empty() || self.name.is_empty() {
            return Err(format!("{}: no version or name", self.id));
        }
        if SYSTEM_NAMESPACES.contains(&self.namespace.as_str()) {
            return Err(format!(
                "{}: {} isn't the bundle's own namespace",
                self.id, self.namespace
            ));
        }
        for img in &self.images {
            let tag = img.rsplit('/').next().unwrap_or("");
            if !(img.contains('@') || tag.contains(':')) || tag.ends_with(":latest") {
                return Err(format!("{}: the image {img} isn't pinned", self.id));
            }
        }
        for o in &self.objects {
            let r = ObjectRef::of(o);
            if r.name.is_empty() {
                return Err(format!("{}: a {} has no name", self.id, r.kind));
            }
            let ok = match r.namespace.as_deref() {
                None => CLUSTER_KINDS.contains(&r.kind.as_str()),
                Some(ns) => ns == self.namespace && NAMESPACED_KINDS.contains(&r.kind.as_str()),
            };
            if !ok {
                return Err(format!(
                    "{}: {} {} can't be in a bundle (its namespace holds {}; cluster-wide, {})",
                    self.id,
                    r.kind,
                    r.name,
                    NAMESPACED_KINDS.join(", "),
                    CLUSTER_KINDS.join(", ")
                ));
            }
        }
        for c in &self.containers() {
            let img = c["image"].as_str().unwrap_or("");
            if !self.images.iter().any(|i| i == img) {
                return Err(format!("{}: the image {img} isn't in its images", self.id));
            }
        }
        for r in &self.ready {
            if !WORKLOAD_KINDS.contains(&r.kind.as_str()) {
                return Err(format!(
                    "{}: a ready check is for a Deployment, StatefulSet or DaemonSet",
                    self.id
                ));
            }
        }
        let (roles, accounts) = (
            self.names_of("ClusterRole"),
            self.names_of("ServiceAccount"),
        );
        // Its RoleBindings: of its own roles, to its own service accounts.
        for o in self.objects.iter().filter(|o| o["kind"] == "RoleBinding") {
            let own_role = match o["roleRef"]["kind"].as_str() {
                Some("Role") => self
                    .names_of("Role")
                    .contains(&o["roleRef"]["name"].as_str().unwrap_or("")),
                Some("ClusterRole") => roles.contains(&o["roleRef"]["name"].as_str().unwrap_or("")),
                _ => false,
            };
            let own_accounts = o["subjects"].as_array().is_some_and(|ss| {
                ss.iter().all(|x| {
                    x["kind"] == "ServiceAccount"
                        && x["namespace"] == json!(self.namespace)
                        && accounts.contains(&x["name"].as_str().unwrap_or(""))
                })
            });
            if !own_role || !own_accounts || o["metadata"]["name"] == crate::rbac::WRITE_BINDING {
                return Err(format!(
                    "{}: the RoleBinding {} binds one of its own roles to its own service accounts",
                    self.id, o["metadata"]["name"]
                ));
            }
        }
        for b in &self.managed_namespace_bindings {
            if !roles.contains(&b.cluster_role.as_str())
                || !accounts.contains(&b.service_account.as_str())
                || !id_ok(&b.name)
                || b.name == crate::rbac::WRITE_BINDING
            {
                return Err(format!(
                    "{}: the binding {} is of one of its own cluster roles, to one of its own service accounts",
                    self.id, b.name
                ));
            }
        }
        Ok(())
    }
}

/// A bundle's RoleBindings in a project namespace.
pub fn bindings_in(
    id: &str,
    bundle_ns: &str,
    bindings: &[NamespaceBinding],
    ns: &str,
) -> Vec<Value> {
    bindings
        .iter()
        .map(|b| {
            json!({
                "apiVersion": "rbac.authorization.k8s.io/v1", "kind": "RoleBinding",
                "metadata": { "name": b.name, "namespace": ns, "labels": { BUNDLE_LABEL: id, "app.kubernetes.io/managed-by": "pyo" } },
                "roleRef": { "apiGroup": "rbac.authorization.k8s.io", "kind": "ClusterRole", "name": b.cluster_role },
                "subjects": [{ "kind": "ServiceAccount", "name": b.service_account, "namespace": bundle_ns }],
            })
        })
        .collect()
}

/// Every bundle the agent's roles and admission policy allow: the WAF with
/// its images, and the databases system with the installation's
/// (`PYO_DB_CONTROLLER_IMAGE`, `PYO_KV_IMAGE`).
pub fn policy_bundles(waf_image: &str, geoip_image: &str) -> Vec<SystemBundle> {
    vec![
        waf_bundle(waf_image, geoip_image),
        databases_bundle(&crate::system::db_images_from_env()),
    ]
}

/// What the databases system is called: its id (the `pyo.dev/bundle` label on
/// its objects) and the row the API keeps of it per cluster.
pub const DATABASES_ID: &str = "databases";

/// Pyo's databases: Postgres through CloudNativePG, MySQL through MOCO
/// (which needs Pyo's cert-manager) and Valkey with Sentinel, all run by
/// pyo-db-controller from the Database objects the agent writes. The API
/// installs it on a cluster with its first database and updates it with
/// Pyo (pyo-api's bundles.rs, `sync_databases`). Its objects are `system/databases/`, with this
/// installation's images for pyo-db-controller and pyo-kv; its version
/// follows its content.
///
/// Its CRDs are never pruned or removed: deleting one deletes every
/// database of that kind.
pub fn databases_bundle(img: &crate::system::DbImages) -> SystemBundle {
    static T: OnceLock<SystemBundle> = OnceLock::new();
    let t = T.get_or_init(|| {
        let mut objects = vec![];
        for f in [
            include_str!("../system/databases/cnpg.yaml"),
            include_str!("../system/databases/moco.yaml"),
            include_str!("../system/databases/crd.yaml"),
            include_str!("../system/databases/controller.yaml"),
        ] {
            for doc in serde_norway::Deserializer::from_str(f) {
                let v = Value::deserialize(doc).unwrap_or_else(|e| panic!("system/databases: {e}"));
                if !v.is_null() {
                    objects.push(v);
                }
            }
        }
        SystemBundle {
            id: DATABASES_ID.into(),
            version: String::new(),
            name: "Databases".into(),
            description: "Postgres, MySQL and KV (Redis-compatible) databases, run on this cluster.".into(),
            namespace: "pyo-databases".into(),
            // Every image its objects run, pinned, and the ones
            // pyo-db-controller starts (Pyo's Postgres per version, MySQL's,
            // Valkey 8.1 with its modules; plain Valkey's follows the
            // database's version).
            images: [
                CONTROLLER_PLACEHOLDER,
                KV_PLACEHOLDER,
                "__POSTGRES_IMAGE_16__",
                "__POSTGRES_IMAGE_17__",
                "__POSTGRES_IMAGE_18__",
                "ghcr.io/cloudnative-pg/cloudnative-pg:1.30.1@sha256:923c267ec29636db3bee20f993d0ec4973fa22998e1adad37da79e4d32b5bc07",
                MYSQL_OPERATOR_PLACEHOLDER,
                MYSQL_BACKUP_PLACEHOLDER,
                "ghcr.io/cybozu-go/moco-agent:0.17.0@sha256:a6d9cc387a08a5d108ee89fb40e6274c47a435d3bb6fa71d4b822873e26b966b",
                "ghcr.io/cybozu-go/moco/fluent-bit:5.1.4.1@sha256:33895ebd0f7ba3d53d146b42a461ca6c05e51a78bed22cf158151bba059a5881",
                "ghcr.io/cybozu-go/moco/mysqld_exporter:0.20.0.1@sha256:793f8c94a43d03a2d1a176e80bfa99fa1ba0e4c4a83161f24a511e38da0e57e6",
                "ghcr.io/cybozu-go/moco/mysql:8.4.10.1@sha256:51dffb347697795e1680b9930fea1e1055a71387a0e769cc61ab4efc539c149b",
                "docker.io/valkey/valkey-bundle:8.1.11@sha256:3f57cc71a102e0e66991ae203b069038d8c3f6214ca887ff32f9a381091af8de",
            ]
            .map(str::to_owned)
            .to_vec(),
            ready: ["cnpg-controller-manager", "moco-controller", "pyo-db-controller"].map(|n| ReadyCheck { kind: "Deployment".into(), name: n.into(), namespace: None }).to_vec(),
            in_use: vec![KindRef { api_version: "pyo.dev/v1alpha1".into(), kind: "Database".into() }],
            // What the controller may do inside a namespace, Secrets
            // included, it may only do in the ones Pyo manages for projects.
            managed_namespace_bindings: vec![NamespaceBinding {
                name: "pyo-db-controller".into(),
                cluster_role: "pyo-db-controller-namespace".into(),
                service_account: "pyo-db-controller".into(),
            }],
            objects,
        }
    });
    fn fill(v: &mut Value, img: &crate::system::DbImages) {
        match v {
            Value::String(s) if s == CONTROLLER_PLACEHOLDER => *s = img.controller.clone(),
            Value::String(s) if s == KV_PLACEHOLDER => *s = img.kv.clone(),
            // The operator's image, and its flag naming the backup image.
            Value::String(s)
                if s.contains(MYSQL_OPERATOR_PLACEHOLDER)
                    || s.contains(MYSQL_BACKUP_PLACEHOLDER) =>
            {
                *s = s
                    .replace(MYSQL_OPERATOR_PLACEHOLDER, &img.mysql_operator)
                    .replace(MYSQL_BACKUP_PLACEHOLDER, &img.mysql_backup);
            }
            Value::String(s) if s.starts_with("__POSTGRES_IMAGE_") => {
                let major = s
                    .trim_start_matches("__POSTGRES_IMAGE_")
                    .trim_end_matches("__");
                if let Some(i) = crate::system::POSTGRES_MAJORS
                    .iter()
                    .position(|m| *m == major)
                {
                    *s = img.postgres[i].clone();
                }
            }
            Value::Array(a) => a.iter_mut().for_each(|x| fill(x, img)),
            Value::Object(m) => m.values_mut().for_each(|x| fill(x, img)),
            _ => {}
        }
    }
    let mut b = t.clone();
    let mut images = Value::from(b.images.clone());
    fill(&mut images, img);
    b.images = serde_json::from_value(images).unwrap_or_default();
    b.objects.iter_mut().for_each(|o| fill(o, img));
    // FNV-1a over what it applies: the same content, the same version.
    let text = serde_json::to_string(&(&b.images, &b.objects)).unwrap_or_default();
    let hash = text.bytes().fold(0xcbf2_9ce4_8422_2325_u64, |h, c| {
        (h ^ u64::from(c)).wrapping_mul(0x100_0000_01b3)
    });
    b.version = format!("{hash:016x}");
    b
}

const CONTROLLER_PLACEHOLDER: &str = "__DB_CONTROLLER_IMAGE__";
const KV_PLACEHOLDER: &str = "__KV_IMAGE__";
const MYSQL_OPERATOR_PLACEHOLDER: &str = "__MYSQL_OPERATOR_IMAGE__";
const MYSQL_BACKUP_PLACEHOLDER: &str = "__MYSQL_BACKUP_IMAGE__";

/// Never pruned when an update drops it, whatever the bundles say: a CRD
/// takes every object of its kind with it (and the namespace is the
/// bundle's own, gone only with the bundle).
pub fn never_pruned(r: &ObjectRef) -> bool {
    r.kind == "CustomResourceDefinition" || r.kind == "Namespace"
}

/// The web application firewall, with the installation's images (so it's
/// built here, not loaded). Installed with the first service on the cluster
/// that turns its WAF on, removed after the last one turns it off; its
/// version follows its images.
pub fn waf_bundle(waf_image: &str, geoip_image: &str) -> SystemBundle {
    use crate::system;
    // FNV-1a: the same images, the same version.
    let hash = format!("{waf_image}\n{geoip_image}")
        .bytes()
        .fold(0xcbf2_9ce4_8422_2325_u64, |h, b| {
            (h ^ u64::from(b)).wrapping_mul(0x100_0000_01b3)
        });
    // Its settings are the agent's (waf.rs writes `config.json`), not the bundle's.
    let config = json!({
        "apiVersion": "v1", "kind": "ConfigMap",
        "metadata": { "name": system::WAF_NAME, "namespace": system::WAF_NS, "labels": system::waf_labels() },
    });
    SystemBundle {
        id: WAF_ID.into(),
        version: format!("{:08x}", hash >> 32),
        name: "Web application firewall".into(),
        description: "Blocks common attacks on websites.".into(),
        namespace: system::WAF_NS.into(),
        images: vec![waf_image.to_owned(), geoip_image.to_owned()],
        ready: vec![ReadyCheck {
            kind: "DaemonSet".into(),
            name: system::WAF_NAME.into(),
            namespace: None,
        }],
        in_use: vec![],
        managed_namespace_bindings: vec![],
        objects: [config]
            .into_iter()
            .chain(system::waf_access())
            .chain([
                system::waf_daemonset(waf_image, geoip_image),
                system::waf_service(),
            ])
            .collect(),
    }
}

// ── The agent's roles and admission policy, from the bundles ──────────────

fn names_by_resource<'a>(
    refs: impl Iterator<Item = &'a ObjectRef>,
) -> Vec<((String, String), Vec<String>)> {
    let mut out: Vec<((String, String), Vec<String>)> = vec![];
    for r in refs {
        let (group, _, plural) = r.resource();
        let key = (group.to_owned(), plural);
        match out.iter_mut().find(|(k, _)| *k == key) {
            Some((_, names)) if !names.contains(&r.name) => names.push(r.name.clone()),
            Some(_) => {}
            None => out.push((key, vec![r.name.clone()])),
        }
    }
    out
}

/// Rules for [`CLUSTER_ROLE`]: create bundles' cluster-wide kinds, change and
/// delete them by name, bind the roles they bind (and write rules the agent
/// doesn't hold itself into them, which admission pins), delete their
/// namespaces and project-namespace bindings, bind [`WRITE_ROLE`], and count
/// their in-use kinds.
pub fn cluster_rules(bundles: &[SystemBundle]) -> Vec<Value> {
    let refs: Vec<ObjectRef> = bundles
        .iter()
        .flat_map(|b| b.objects.iter().map(ObjectRef::of))
        .filter(|r| r.namespace.is_none())
        .collect();
    let mut rules = vec![];
    for ((group, plural), names) in names_by_resource(refs.iter()) {
        rules.push(json!({ "apiGroups": [group], "resources": [plural], "verbs": ["create"] }));
        rules.push(json!({ "apiGroups": [group], "resources": [plural], "verbs": ["get", "patch", "delete"], "resourceNames": names }));
    }
    let bound: Vec<String> = bundles
        .iter()
        .flat_map(|b| b.names_of("ClusterRole"))
        .map(str::to_owned)
        .collect();
    let mut bind = vec![WRITE_ROLE.to_owned()];
    bind.extend(bound.iter().cloned());
    rules.push(json!({ "apiGroups": ["rbac.authorization.k8s.io"], "resources": ["clusterroles"], "verbs": ["bind"], "resourceNames": bind }));
    if !bound.is_empty() {
        rules.push(json!({ "apiGroups": ["rbac.authorization.k8s.io"], "resources": ["clusterroles"], "verbs": ["escalate"], "resourceNames": bound }));
    }
    let namespaces: Vec<&str> = bundles.iter().map(|b| b.namespace.as_str()).collect();
    if !namespaces.is_empty() {
        rules.push(json!({ "apiGroups": [""], "resources": ["namespaces"], "verbs": ["delete"], "resourceNames": namespaces }));
    }
    let bindings: Vec<&str> = bundles
        .iter()
        .flat_map(|b| b.managed_namespace_bindings.iter().map(|x| x.name.as_str()))
        .collect();
    if !bindings.is_empty() {
        rules.push(json!({ "apiGroups": ["rbac.authorization.k8s.io"], "resources": ["rolebindings"], "verbs": ["delete"], "resourceNames": bindings }));
    }
    for k in bundles.iter().flat_map(|b| b.in_use.iter()) {
        let (group, _) = k
            .api_version
            .split_once('/')
            .unwrap_or(("", &k.api_version));
        rules.push(json!({ "apiGroups": [group], "resources": [plural(&k.kind)], "verbs": ["get", "list"] }));
    }
    rules
}

/// Rules for [`WRITE_ROLE`]: the kinds a bundle may have in its namespace.
/// Secrets are written, never read; no pods, so nothing to exec into. Its
/// Roles may hold what the agent doesn't (escalate) and be bound (bind), by
/// name; admission pins them.
pub fn write_rules(bundles: &[SystemBundle]) -> Vec<Value> {
    let all = [
        "get", "list", "watch", "create", "update", "patch", "delete",
    ];
    let mut rules = vec![
        json!({ "apiGroups": [""], "resources": ["serviceaccounts", "services", "configmaps"], "verbs": all }),
        json!({ "apiGroups": [""], "resources": ["secrets"], "verbs": ["create", "patch", "delete"] }),
        json!({ "apiGroups": ["apps"], "resources": ["deployments", "statefulsets", "daemonsets"], "verbs": all }),
        json!({ "apiGroups": ["rbac.authorization.k8s.io"], "resources": ["roles", "rolebindings"], "verbs": all }),
        json!({ "apiGroups": [CERT_MANAGER_GROUP], "resources": ["certificates", "issuers"], "verbs": all }),
    ];
    let roles: Vec<&str> = bundles.iter().flat_map(|b| b.names_of("Role")).collect();
    if !roles.is_empty() {
        rules.push(json!({ "apiGroups": ["rbac.authorization.k8s.io"], "resources": ["roles"], "verbs": ["escalate", "bind"], "resourceNames": roles }));
    }
    rules
}

/// A CEL path to a field: `.name`, or `['some.key']` for map keys that
/// aren't names.
fn field(path: &str, key: &str) -> String {
    use crate::system::cel_literal;
    let plain = key
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    if plain {
        format!("{path}.{key}")
    } else {
        format!("{path}[{}]", cel_literal(&json!(key)))
    }
}

/// The CEL that `path` holds every value `v` sets: each leaf equal, each
/// list the same length. Fields `v` doesn't have may be there (the cluster
/// fills some in).
fn pins(path: &str, v: &Value, out: &mut Vec<String>) {
    use crate::system::cel_literal;
    match v {
        Value::Object(m) => {
            for (k, x) in m {
                pins(&field(path, k), x, out);
            }
        }
        Value::Array(a) => {
            out.push(format!("size({path}) == {}", a.len()));
            for (i, x) in a.iter().enumerate() {
                pins(&format!("{path}[{i}]"), x, out);
            }
        }
        _ => out.push(format!("{path} == {}", cel_literal(v))),
    }
}

/// The CEL that the map at `path` has only `v`'s keys and `filled` ones.
fn only_keys(path: &str, v: &Value, filled: &[&str]) -> String {
    use crate::system::cel_literal;
    let mut keys: Vec<Value> = v
        .as_object()
        .map(|m| m.keys().map(|k| json!(k)).collect())
        .unwrap_or_default();
    keys.extend(filled.iter().map(|k| json!(k)));
    format!("{path}.all(k, k in {})", cel_literal(&Value::Array(keys)))
}

/// Pod spec fields the cluster fills in on a workload's template.
const POD_FILLED: [&str; 7] = [
    "restartPolicy",
    "terminationGracePeriodSeconds",
    "dnsPolicy",
    "securityContext",
    "schedulerName",
    "serviceAccount",
    "enableServiceLinks",
];
/// Container fields the cluster fills in.
const CONTAINER_FILLED: [&str; 4] = [
    "terminationMessagePath",
    "terminationMessagePolicy",
    "imagePullPolicy",
    "resources",
];

/// The CEL that a workload's pod template is the bundle's.
fn pod_cel(o: &Value) -> String {
    let spec = &o["spec"]["template"]["spec"];
    let base = "variables.spec";
    let mut all = vec![only_keys(base, spec, &POD_FILLED)];
    let account = spec["serviceAccountName"].as_str().unwrap_or("default");
    all.push(format!(
        "(has({base}.serviceAccount) ? {base}.serviceAccount : '{account}') == '{account}'",
        account = account.replace(['\\', '\''], "")
    ));
    if spec.get("securityContext").is_some() {
        all.push(only_keys(
            &field(base, "securityContext"),
            &spec["securityContext"],
            &[],
        ));
    }
    let mut rest = spec.clone();
    for k in ["containers", "initContainers"] {
        rest.as_object_mut().map(|m| m.remove(k));
        let list = spec[k].as_array().cloned().unwrap_or_default();
        let path = field(base, k);
        if list.is_empty() {
            all.push(format!("!has({path})"));
            continue;
        }
        all.push(format!("size({path}) == {}", list.len()));
        for (i, c) in list.iter().enumerate() {
            let p = format!("{path}[{i}]");
            all.push(only_keys(&p, c, &CONTAINER_FILLED));
            if c.get("securityContext").is_some() {
                all.push(only_keys(
                    &field(&p, "securityContext"),
                    &c["securityContext"],
                    &[],
                ));
            }
            pins(&p, c, &mut all);
        }
    }
    pins(base, &rest, &mut all);
    all.join(" && ")
}

/// The CEL that one object of a bundle may be written as it is (or deleted).
fn object_cel(o: &Value) -> String {
    use crate::system::cel_literal;
    let r = ObjectRef::of(o);
    let (group, _, plural) = r.resource();
    let mut pin = vec![];
    match r.kind.as_str() {
        "ClusterRole" | "Role" => pin.push(format!("object.rules == {}", cel_literal(&o["rules"]))),
        "ClusterRoleBinding" | "RoleBinding" => pin.push(format!(
            "object.roleRef == {} && object.subjects == {}",
            cel_literal(&o["roleRef"]),
            cel_literal(&o["subjects"])
        )),
        "MutatingWebhookConfiguration" | "ValidatingWebhookConfiguration" => {
            pins("object.webhooks", &o["webhooks"], &mut pin)
        }
        "Certificate"
        | "Issuer"
        | "ValidatingAdmissionPolicy"
        | "ValidatingAdmissionPolicyBinding" => pins("object.spec", &o["spec"], &mut pin),
        k if WORKLOAD_KINDS.contains(&k) => pin.push(pod_cel(o)),
        _ => {}
    }
    let pin = if pin.is_empty() {
        "true".to_owned()
    } else {
        pin.join(" && ")
    };
    format!(
        "(request.resource.group == {} && request.resource.resource == {} && variables.ns == {} && variables.objName == {} && (request.operation == 'DELETE' || ({pin})))",
        cel_literal(&json!(group)),
        cel_literal(&json!(plural)),
        cel_literal(&json!(r.namespace.unwrap_or_default())),
        cel_literal(&json!(r.name)),
    )
}

/// The `bundleAllowed` variable: the request writes one of a bundle's
/// objects as the bundle has it, the agent's binding in its namespace, or
/// one of its bindings in a project namespace (not pyo-system).
pub fn allowed_cel(bundles: &[SystemBundle]) -> String {
    use crate::system::cel_literal;
    let mut any: Vec<String> = bundles
        .iter()
        .flat_map(|b| b.objects.iter().map(object_cel))
        .collect();
    let namespaces: Vec<Value> = bundles.iter().map(|b| json!(b.namespace)).collect();
    if let Some(first) = bundles.first() {
        let b = first.write_binding();
        any.push(format!(
            "(request.resource.resource == 'rolebindings' && variables.ns in {} && variables.objName == {} && (request.operation == 'DELETE' || (object.roleRef == {} && object.subjects == {})))",
            cel_literal(&Value::Array(namespaces)),
            cel_literal(&json!(crate::rbac::WRITE_BINDING)),
            cel_literal(&b["roleRef"]),
            cel_literal(&b["subjects"]),
        ));
    }
    for b in bundles {
        for nb in b.namespace_bindings("") {
            any.push(format!(
                "(request.resource.resource == 'rolebindings' && variables.managed && variables.ns != {} && variables.objName == {} && (request.operation == 'DELETE' || (object.roleRef == {} && object.subjects == {})))",
                cel_literal(&json!(crate::rbac::NS)),
                cel_literal(&nb["metadata"]["name"]),
                cel_literal(&nb["roleRef"]),
                cel_literal(&nb["subjects"]),
            ));
        }
    }
    if any.is_empty() {
        "false".to_owned()
    } else {
        any.join(" || ")
    }
}

/// The `bundlePods` validation: in a bundle's namespace, only its own
/// workloads make pods (and those are pinned in `bundleAllowed`).
pub fn pods_cel(bundles: &[SystemBundle]) -> String {
    use crate::system::cel_literal;
    let namespaces: Vec<Value> = bundles.iter().map(|b| json!(b.namespace)).collect();
    let mut own = vec![];
    for b in bundles {
        for o in b.workloads() {
            own.push(format!(
                "(variables.ns == {} && object.kind == {} && variables.objName == {})",
                cel_literal(&json!(b.namespace)),
                cel_literal(&o["kind"]),
                cel_literal(&o["metadata"]["name"])
            ));
        }
    }
    format!(
        "!variables.hasPods || !(variables.ns in {}) || {}",
        cel_literal(&Value::Array(namespaces)),
        if own.is_empty() {
            "false".to_owned()
        } else {
            own.join(" || ")
        }
    )
}

/// Cluster-wide resources the agent writes only as a bundle has them.
pub fn guarded_resources() -> Vec<String> {
    CLUSTER_KINDS.iter().map(|k| plural(k)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn images(controller: &str) -> crate::system::DbImages {
        crate::system::DbImages {
            controller: controller.into(),
            kv: "registry.example.com/pyo/kv:1".into(),
            postgres: ["16", "17", "18"]
                .map(|m| format!("registry.example.com/pyo/postgres:{m}-1")),
            mysql_operator: "registry.example.com/pyo/mysql-operator:1@sha256:aa".into(),
            mysql_backup: "registry.example.com/pyo/mysql-backup:1@sha256:bb".into(),
        }
    }

    fn db() -> SystemBundle {
        databases_bundle(&images("registry.example.com/pyo/db-controller:1"))
    }

    fn waf() -> SystemBundle {
        waf_bundle(
            "registry.example.com/pyo/waf:1",
            "registry.example.com/pyo/geoip:1",
        )
    }

    #[test]
    fn plurals() {
        assert_eq!(plural("Deployment"), "deployments");
        assert_eq!(plural("NetworkPolicy"), "networkpolicies");
        assert_eq!(plural("Ingress"), "ingresses");
        assert_eq!(plural("Gateway"), "gateways");
        assert_eq!(
            plural("CustomResourceDefinition"),
            "customresourcedefinitions"
        );
        assert_eq!(
            plural("MutatingWebhookConfiguration"),
            "mutatingwebhookconfigurations"
        );
        assert_eq!(plural("ReferenceGrant"), "referencegrants");
    }

    #[test]
    fn the_databases_bundle_is_valid() {
        let b = &db();
        assert_eq!(b.namespace, "pyo-databases");
        let kinds: Vec<&str> = b
            .objects
            .iter()
            .map(|o| o["kind"].as_str().unwrap())
            .collect();
        for k in [
            "CustomResourceDefinition",
            "ServiceAccount",
            "ClusterRole",
            "ClusterRoleBinding",
            "Deployment",
            "MutatingWebhookConfiguration",
            "ValidatingWebhookConfiguration",
        ] {
            assert!(kinds.contains(&k), "{k}");
        }
        // Everything CloudNativePG has is in the bundle's namespace.
        assert!(
            !serde_json::to_string(&b.objects)
                .unwrap()
                .contains("cnpg-system")
        );
        let cnpg = b
            .objects
            .iter()
            .find(|o| {
                o["kind"] == "Deployment" && o["metadata"]["name"] == "cnpg-controller-manager"
            })
            .unwrap();
        assert!(
            cnpg["spec"]["template"]["spec"]["containers"][0]["image"]
                .as_str()
                .unwrap()
                .contains("@sha256:")
        );
        // Namespace first, then CRDs and cluster roles, webhooks last; all labelled.
        let objs = b.objects();
        assert_eq!(objs[0]["kind"], "Namespace");
        assert_eq!(objs[1]["kind"], "CustomResourceDefinition");
        assert!(
            objs.last().unwrap()["kind"]
                .as_str()
                .unwrap()
                .ends_with("WebhookConfiguration")
        );
        assert!(
            objs.iter()
                .all(|o| o["metadata"]["labels"][BUNDLE_LABEL] == "databases")
        );
        assert_eq!(
            objs[0]["metadata"]["labels"][VERSION_LABEL],
            json!(b.version)
        );
        // No managed label: it isn't a project namespace.
        assert!(
            objs[0]["metadata"]["labels"]
                .get(crate::rbac::MANAGED_LABEL)
                .is_none()
        );
        assert!(b.check().is_ok(), "{:?}", b.check());
    }

    #[test]
    fn mysql_runs_on_moco_with_pyos_cert_manager() {
        let b = &db();
        // Its version follows its content: the images, here.
        assert_eq!(b.version, db().version);
        assert_ne!(
            b.version,
            databases_bundle(&images("registry.example.com/pyo/db-controller:2")).version
        );
        assert!(b.ready.iter().any(|r| r.name == "moco-controller"));
        assert!(
            !serde_json::to_string(&b.objects)
                .unwrap()
                .contains("moco-system"),
            "all of MOCO is in pyo-databases"
        );
        let kinds: Vec<&str> = b
            .objects
            .iter()
            .map(|o| o["kind"].as_str().unwrap())
            .collect();
        for k in [
            "Certificate",
            "Issuer",
            "ValidatingAdmissionPolicy",
            "ValidatingAdmissionPolicyBinding",
        ] {
            assert!(kinds.contains(&k), "{k}");
        }
        assert!(b.needs_cert_manager());
        assert!(!waf_bundle("w:1", "g:1").needs_cert_manager());
        // Its admission policy and binding come after the CRDs and roles,
        // with the webhooks; removal goes newest first, so they go first.
        let objs = b.objects();
        let at = |k: &str| objs.iter().position(|o| o["kind"] == k).unwrap();
        assert!(
            at("ValidatingAdmissionPolicy") > at("Deployment")
                && at("Certificate") > at("ClusterRoleBinding")
        );
        // Pinned: the policy's spec and the certificates' issuers.
        let allowed = allowed_cel(&[db()]);
        assert!(allowed.contains("request.resource.resource == 'validatingadmissionpolicies' && variables.ns == '' && variables.objName == 'moco-delete-validator'"));
        assert!(allowed.contains("object.spec.failurePolicy == 'Fail'"));
        assert!(allowed.contains("request.resource.resource == 'certificates' && variables.ns == 'pyo-databases' && variables.objName == 'moco-serving-cert'"));
        assert!(guarded_resources().contains(&"validatingadmissionpolicybindings".to_owned()));
        // The agent may write them: cluster-wide by name, cert-manager's in the bundle's namespace.
        let rules = cluster_rules(&[db()]);
        assert!(
            rules
                .iter()
                .any(|r| r["resources"] == json!(["validatingadmissionpolicies"])
                    && r["resourceNames"] == json!(["moco-delete-validator"]))
        );
        assert!(
            write_rules(&[db()])
                .iter()
                .any(|r| r["resources"] == json!(["certificates", "issuers"]))
        );
        // Its operator is Pyo's build (deploy/images/moco), and so is the
        // backup image it starts; new clusters get no name prefix.
        let moco = b
            .objects
            .iter()
            .find(|o| o["kind"] == "Deployment" && o["metadata"]["name"] == "moco-controller")
            .unwrap();
        let c = &moco["spec"]["template"]["spec"]["containers"][0];
        assert_eq!(
            c["image"],
            "registry.example.com/pyo/mysql-operator:1@sha256:aa"
        );
        let args: Vec<&str> = c["args"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(Value::as_str)
            .collect();
        assert!(
            args.contains(&"--backup-image=registry.example.com/pyo/mysql-backup:1@sha256:bb")
                && args.contains(&"--name-prefix="),
            "{args:?}"
        );
        assert!(
            b.images
                .contains(&"registry.example.com/pyo/mysql-operator:1@sha256:aa".to_owned())
                && b.images
                    .contains(&"registry.example.com/pyo/mysql-backup:1@sha256:bb".to_owned())
        );
        assert!(
            !serde_json::to_string(&(&b.images, &b.objects))
                .unwrap()
                .contains("__MYSQL_")
        );
        let ctl = b
            .objects
            .iter()
            .find(|o| o["kind"] == "Deployment" && o["metadata"]["name"] == "pyo-db-controller")
            .unwrap();
        let env = ctl["spec"]["template"]["spec"]["containers"][0]["env"].to_string();
        assert!(
            env.contains("PYO_DB_MYSQL_IMAGE")
                && env.contains("ghcr.io/cybozu-go/moco/mysql:8.4.10.1@sha256:")
        );
        assert!(env.contains("PYO_DB_POSTGRES_IMAGE") && env.contains("PYO_DB_VALKEY_IMAGE"));
    }

    #[test]
    fn the_controller_is_bound_in_project_namespaces_only() {
        let b = &db();
        let rb = &b.namespace_bindings("prj-shop")[0];
        assert_eq!(rb["metadata"]["name"], "pyo-db-controller");
        assert_eq!(rb["roleRef"]["name"], "pyo-db-controller-namespace");
        assert_eq!(
            rb["subjects"],
            json!([{ "kind": "ServiceAccount", "name": "pyo-db-controller", "namespace": "pyo-databases" }])
        );
        // Its cluster-wide role only watches; Secrets are per namespace.
        let wide = b
            .objects
            .iter()
            .find(|o| o["kind"] == "ClusterRole" && o["metadata"]["name"] == "pyo-db-controller")
            .unwrap();
        assert!(!wide["rules"].to_string().contains("secrets"));
        let allowed = allowed_cel(&[db()]);
        assert!(allowed.contains("variables.managed && variables.ns != 'pyo-system' && variables.objName == 'pyo-db-controller'"));
        let mut bad = b.clone();
        bad.managed_namespace_bindings[0].cluster_role = "cluster-admin".into();
        assert!(bad.check().is_err(), "only its own roles");
    }

    #[test]
    fn bundles_hold_only_what_the_agent_may_write() {
        let mut b = db();
        assert!(b.check().is_ok());
        b.objects.push(json!({ "apiVersion": "v1", "kind": "Pod", "metadata": { "name": "x", "namespace": "pyo-databases" } }));
        assert!(b.check().is_err(), "no bare pods");
        let mut b = db();
        b.objects.push(json!({ "apiVersion": "v1", "kind": "Service", "metadata": { "name": "x", "namespace": "pyo-system" } }));
        assert!(b.check().is_err(), "nothing outside its namespace");
        let mut b = db();
        b.objects.push(json!({ "apiVersion": "helm.cattle.io/v1", "kind": "HelmChart", "metadata": { "name": "x", "namespace": "kube-system" } }));
        assert!(b.check().is_err(), "no charts");
        let mut b = db();
        b.images[0] = "registry.example.com/pyo/db-controller:latest".into();
        assert!(b.check().is_err(), "pinned images");
        let mut b = db();
        b.namespace = "pyo-system".into();
        assert!(b.check().is_err());
    }

    #[test]
    fn roles_are_by_name() {
        let rules = cluster_rules(&policy_bundles("w:1", "g:1"));
        for r in &rules {
            let verbs = r["verbs"].to_string();
            if ["get", "patch", "delete", "bind", "escalate"]
                .iter()
                .any(|v| verbs.contains(v))
                && !verbs.contains("list")
            {
                assert!(r["resourceNames"].is_array(), "{r}");
            }
            assert!(
                !r["resources"].to_string().contains("secrets")
                    && !r["resources"].to_string().contains("\"*\""),
                "{r}"
            );
        }
        assert!(
            rules
                .iter()
                .any(|r| r["resources"] == json!(["databases"])
                    && r["verbs"] == json!(["get", "list"])),
            "it counts databases"
        );
        assert!(rules.iter().any(
            |r| r["resources"] == json!(["mutatingwebhookconfigurations"])
                && r["resourceNames"].as_array().is_some_and(|n| {
                    n.contains(&json!("cnpg-mutating-webhook-configuration"))
                        && n.contains(&json!("moco-mutating-webhook-configuration"))
                })
        ));
        assert!(
            rules
                .iter()
                .any(|r| r["resources"] == json!(["rolebindings"])
                    && r["resourceNames"] == json!(["pyo-db-controller"]))
        );
        let write = write_rules(&[db()])
            .iter()
            .map(|r| r.to_string())
            .collect::<String>();
        assert!(!write.contains("pods") && !write.contains("exec"));
        assert!(write_rules(&[db()]).iter().any(|r| {
            r["verbs"] == json!(["escalate", "bind"])
                && r["resourceNames"].as_array().is_some_and(|n| {
                    n.contains(&json!("pyo-db-controller-lease"))
                        && n.contains(&json!("moco-leader-election-role"))
                })
        }));
    }

    #[test]
    fn admission_pins_roles_webhooks_and_pods() {
        let allowed = allowed_cel(&[db()]);
        assert!(
            allowed.contains("request.resource.resource == 'clusterroles'")
                && allowed.contains("object.rules == ")
        );
        assert!(allowed.contains("variables.objName == 'databases.pyo.dev'"));
        assert!(
            allowed
                .contains("object.webhooks[0].clientConfig.service.namespace == 'pyo-databases'")
        );
        assert!(allowed.contains(
            "variables.spec.containers[0].image == 'registry.example.com/pyo/db-controller:1'"
        ));
        assert!(allowed.contains("size(variables.spec.containers) == 1"));
        assert!(!allowed.contains("helmcharts"));
        // Its lease Role and binding, as written; two replicas, spread.
        assert!(allowed.contains("request.resource.resource == 'roles' && variables.ns == 'pyo-databases' && variables.objName == 'pyo-db-controller-lease'"));
        assert!(allowed.contains("request.resource.resource == 'rolebindings' && variables.ns == 'pyo-databases' && variables.objName == 'pyo-db-controller-lease'"));
        assert!(allowed.contains("variables.spec.tolerations[0].tolerationSeconds == 10"));
        assert!(allowed.contains(
            "variables.spec.containers[0].env[8].valueFrom.fieldRef.fieldPath == 'metadata.name'"
        ));
        let pods = pods_cel(&[db()]);
        assert!(pods.contains("variables.objName == 'cnpg-controller-manager'"));
        // A quote can't end a CEL string.
        let mut b = db();
        b.objects[0]["metadata"]["name"] = json!("x' || true || 'y");
        assert!(!allowed_cel(&[b]).contains("x' ||"));
    }

    #[test]
    fn the_databases_bundle_takes_the_installations_images() {
        let b = db();
        assert_eq!(b.id, "databases");
        assert!(
            b.images
                .contains(&"registry.example.com/pyo/db-controller:1".to_owned())
                && b.images
                    .contains(&"registry.example.com/pyo/kv:1".to_owned())
        );
        let text = serde_json::to_string(&b.objects).unwrap();
        assert!(
            !text.contains("__") && !text.contains("ghcr.io/upsetdev/"),
            "every placeholder filled"
        );
        assert!(
            text.contains("\"value\":\"registry.example.com/pyo/postgres:18-1\"")
                && b.images
                    .contains(&"registry.example.com/pyo/postgres:16-1".to_owned())
        );
        assert!(text.contains("\"value\":\"registry.example.com/pyo/kv:1\""));
        assert!(
            policy_bundles("w:1", "g:1")
                .iter()
                .any(|b| b.id == DATABASES_ID)
        );
    }

    /// The names on the cluster and on the wire.
    #[test]
    fn names_on_the_cluster_and_the_wire() {
        assert_eq!(
            (
                BUNDLE_LABEL,
                VERSION_LABEL,
                IN_USE_ANNOTATION,
                BINDINGS_ANNOTATION
            ),
            (
                "pyo.dev/bundle",
                "pyo.dev/bundle-version",
                "pyo.dev/bundle-in-use",
                "pyo.dev/bundle-bindings"
            )
        );
        assert_eq!(
            (FIELD_MANAGER, WRITE_ROLE, CLUSTER_ROLE),
            ("pyo-agent", "pyo-agent-bundle-write", "pyo-agent-bundles")
        );
        assert_eq!(
            (waf().id.as_str(), waf().namespace.as_str()),
            ("waf", "pyo-waf")
        );
        assert_eq!(
            (db().id.as_str(), db().namespace.as_str()),
            ("databases", "pyo-databases")
        );
        assert!(
            waf()
                .objects()
                .iter()
                .all(|o| o["metadata"]["labels"][BUNDLE_LABEL] == "waf")
        );
        assert!(
            db().objects()
                .iter()
                .all(|o| o["metadata"]["labels"][BUNDLE_LABEL] == "databases")
        );
        let ids: Vec<String> = policy_bundles("w:1", "g:1")
            .into_iter()
            .map(|b| b.id)
            .collect();
        assert_eq!(ids, ["waf", "databases"]);
        let apply = serde_json::to_value(crate::Command::ApplyBundle {
            bundle: Box::new(waf()),
            prune: vec![],
        })
        .unwrap();
        assert_eq!(apply["kind"], "apply_bundle");
        let remove = serde_json::to_value(crate::Command::RemoveBundle {
            id: "waf".into(),
            objects: vec![],
        })
        .unwrap();
        assert_eq!(remove["kind"], "remove_bundle");
        let v = serde_json::to_value(crate::Heartbeat::default()).unwrap();
        assert!(v.get("bundles").is_some());
    }

    #[test]
    fn the_waf_is_a_bundle_of_its_own() {
        let b = waf();
        assert!(b.check().is_ok(), "{:?}", b.check());
        assert_eq!(b.containers()[0]["image"], "registry.example.com/pyo/waf:1");
        assert_eq!(b.namespace, crate::system::WAF_NS);
        assert!(
            b.objects
                .iter()
                .all(|o| o["metadata"]["namespace"] == crate::system::WAF_NS)
        );
        // Its settings are the agent's.
        assert!(b.objects[0].get("data").is_none());
        // Its version follows its images.
        assert_eq!(b.version, waf().version);
        assert_ne!(
            b.version,
            waf_bundle(
                "registry.example.com/pyo/waf:2",
                "registry.example.com/pyo/geoip:1"
            )
            .version
        );
        // The DaemonSet is pinned: its image, placement and the GeoIP image.
        let allowed = allowed_cel(std::slice::from_ref(&b));
        assert!(
            allowed
                .contains("variables.spec.containers[0].image == 'registry.example.com/pyo/waf:1'")
        );
        assert!(allowed.contains("variables.spec.affinity.nodeAffinity"));
        assert!(allowed.contains(
            "variables.spec.volumes[2].image.reference == 'registry.example.com/pyo/geoip:1'"
        ));
        // Its token, for its settings' ConfigMap alone.
        assert!(allowed.contains("variables.spec.automountServiceAccountToken == true"));
        assert!(allowed.contains("variables.spec.serviceAccountName == 'pyo-waf'"));
        let role = b.objects.iter().find(|o| o["kind"] == "Role").unwrap();
        assert_eq!(
            role["rules"],
            json!([{ "apiGroups": [""], "resources": ["configmaps"], "resourceNames": ["pyo-waf"], "verbs": ["get", "list", "watch"] }])
        );
        let binding = b
            .objects
            .iter()
            .find(|o| o["kind"] == "RoleBinding")
            .unwrap();
        assert_eq!(
            binding["subjects"],
            json!([{ "kind": "ServiceAccount", "name": "pyo-waf", "namespace": "pyo-waf" }])
        );
        // Before its pods, so they can read from the start.
        let at = |k: &str| b.objects().iter().position(|o| o["kind"] == k).unwrap();
        assert!(
            at("RoleBinding") < at("DaemonSet")
                && at("Role") < at("DaemonSet")
                && at("ServiceAccount") < at("DaemonSet")
        );
    }
}
