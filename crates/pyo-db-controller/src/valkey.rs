//! Valkey: a StatefulSet of instances, three sentinels for failover when
//! there are replicas, a primary service the controller points at whichever
//! instance the sentinels say is primary, and optionally pyo-kv for REST.
//!
//! Read replicas are a second StatefulSet, `<name>-read`, of replicas with
//! priority 0: the sentinels never promote them, and move them to the new
//! primary after a failover like any replica. `<name>-read` is a service
//! over them alone.
//!
//! Instances restart only when the controller deletes them (`OnDelete`), in
//! an order that keeps a primary (`roll`): read replicas, then replicas,
//! then the primary hands over to a replica that has restarted.

use std::time::Duration;

use k8s_openapi::api::{
    apps::v1::StatefulSet,
    core::v1::{Pod, Secret, Service},
};
use kube::{
    Api,
    api::{ListParams, Patch, PatchParams},
};
use serde_json::{Value, json};

use crate::{
    controller::{
        Kube, LABEL_COMPONENT, LABEL_DATABASE, LABEL_ROLE, labels, phase_for, resources,
        secret_name, selector,
    },
    crd::{self, DatabaseStatus},
};

/// The read replicas' component label.
const READ: &str = "read";
/// pyo-kv's component label.
pub const KV: &str = "kv";

/// Three, so two can agree a primary is gone even when one went down with it.
pub const SENTINELS: usize = 3;
const VALKEY_SH: &str = include_str!("../scripts/valkey.sh");
const SENTINEL_SH: &str = include_str!("../scripts/sentinel.sh");

fn secret_env(name: &str, secret: &str, key: &str, optional: bool) -> Value {
    json!({ "name": name, "valueFrom": { "secretKeyRef": { "name": secret, "key": key, "optional": optional } } })
}

fn field_env(name: &str, path: &str) -> Value {
    json!({ "name": name, "valueFrom": { "fieldRef": { "fieldPath": path } } })
}

fn spread(sel: Value) -> Value {
    json!({ "podAntiAffinity": { "preferredDuringSchedulingIgnoredDuringExecution": [{
        "weight": 100,
        "podAffinityTerm": { "topologyKey": "kubernetes.io/hostname", "labelSelector": { "matchLabels": sel } },
    }]}})
}

fn pod_security() -> Value {
    json!({ "runAsNonRoot": true, "runAsUser": 999, "runAsGroup": 999, "fsGroup": 999, "seccompProfile": { "type": "RuntimeDefault" } })
}

fn container_security() -> Value {
    json!({ "allowPrivilegeEscalation": false, "capabilities": { "drop": ["ALL"] } })
}

fn ping_probe(port: u16) -> Value {
    json!({
        "exec": { "command": ["sh", "-c", format!(r#"valkey-cli --no-auth-warning -a "$PASSWORD" -p {port} ping | grep -q PONG"#)] },
        "periodSeconds": 5, "timeoutSeconds": 3, "failureThreshold": 3,
    })
}

/// Three quarters of a memory quantity, in bytes: the rest is left for
/// replication buffers and rewriting the append-only file, so Valkey refuses
/// writes instead of being killed.
pub fn maxmemory(q: &str) -> Option<u64> {
    const UNITS: [(&str, f64); 8] = [
        ("Ki", 1024.0),
        ("Mi", 1048576.0),
        ("Gi", 1073741824.0),
        ("Ti", 1099511627776.0),
        ("K", 1e3),
        ("M", 1e6),
        ("G", 1e9),
        ("T", 1e12),
    ];
    let (num, mul) = UNITS
        .iter()
        .find_map(|(s, m)| q.strip_suffix(s).map(|n| (n, *m)))
        .unwrap_or((q, 1.0));
    let n: f64 = num.parse().ok()?;
    Some((n * mul) as u64 * 3 / 4)
}

/// The instance number at the end of a pod name.
pub fn ordinal(pod: &str) -> Option<i32> {
    pod.rsplit_once('-')?.1.parse().ok()
}

impl Kube<'_> {
    fn version(&self) -> &str {
        let v = &self.db.spec.version;
        if v.is_empty() {
            &self.ctx.default_valkey
        } else {
            v
        }
    }

    /// The sentinels' image, and the instances' when they load no modules.
    fn image(&self) -> String {
        format!("{}:{}", self.ctx.images.valkey_repo, self.version())
    }

    /// The modules wanted (`spec.valkey.modules`).
    fn wanted_modules(&self) -> &[String] {
        self.db.spec.valkey.as_ref().map_or(&[], |v| &v.modules)
    }

    /// Whether this version has the modules: valkey-bundle is Valkey 8.1.
    fn modules_available(&self) -> bool {
        self.version() == "8.1" && !self.ctx.images.valkey_bundle.is_empty()
    }

    /// The files the instances load (modules.rs's `Plan::load`), and the
    /// image that has them.
    fn instance_image(&self, load: &[String]) -> (String, Vec<&'static str>) {
        let files = if self.modules_available() {
            crate::modules::files(load)
        } else {
            vec![]
        };
        if files.is_empty() {
            (self.image(), files)
        } else {
            (self.ctx.images.valkey_bundle.clone(), files)
        }
    }

    fn env(&self) -> Vec<Value> {
        vec![
            json!({ "name": "NAME", "value": self.name() }),
            json!({ "name": "DOMAIN", "value": self.ctx.domain }),
            json!({ "name": "INSTANCES", "value": self.db.spec.instances.to_string() }),
            json!({ "name": "SENTINEL", "value": if self.db.spec.instances > 1 { "1" } else { "0" } }),
            field_env("NAMESPACE", "metadata.namespace"),
            field_env("POD_NAME", "metadata.name"),
            secret_env("PASSWORD", &secret_name(self.db), "password", false),
        ]
    }

    fn with_role(&self, role: &str) -> Value {
        let mut s = selector(self.db, "valkey");
        s[LABEL_ROLE] = json!(role);
        s
    }

    fn service(
        &self,
        name: &str,
        component: &str,
        sel: Value,
        port: u16,
        target: u16,
        headless: bool,
    ) -> Value {
        let mut o = self.object("v1", "Service", name, component);
        o["spec"] = json!({ "selector": sel, "ports": [{ "name": component, "port": port, "targetPort": target }] });
        if headless {
            o["spec"]["clusterIP"] = json!("None");
            o["spec"]["publishNotReadyAddresses"] = json!(true);
        }
        o
    }

    /// A service for one pod, named after it, whose address never changes.
    fn pod_service(&self, pod: &str, component: &str, port: u16) -> Value {
        let mut o = self.object("v1", "Service", pod, component);
        o["spec"] = json!({
            "selector": { "statefulset.kubernetes.io/pod-name": pod },
            "publishNotReadyAddresses": true,
            "ports": [{ "name": component, "port": port, "targetPort": port }],
        });
        o
    }

    fn scripts_volume(&self) -> Value {
        json!({ "name": "scripts", "configMap": { "name": format!("{}-scripts", self.name()), "defaultMode": 0o555 } })
    }

    pub fn valkey_statefulset(&self, replicas: i32, load: &[String]) -> Value {
        self.instances(&self.name(), "valkey", replicas, false, load)
    }

    /// The read replicas: the same instances, started with READ=1.
    pub fn read_statefulset(&self, load: &[String]) -> Value {
        self.instances(
            &format!("{}-read", self.name()),
            READ,
            self.db.spec.read_instances(),
            true,
            load,
        )
    }

    fn instances(
        &self,
        sts: &str,
        component: &str,
        replicas: i32,
        read: bool,
        load: &[String],
    ) -> Value {
        let s = &self.db.spec;
        let mut env = self.env();
        if let Some(m) = maxmemory(&s.memory).filter(|_| !s.memory.is_empty()) {
            env.push(json!({ "name": "MAXMEMORY", "value": m.to_string() }));
        }
        if read {
            env.push(json!({ "name": "READ", "value": "1" }));
        }
        let (image, modules) = self.instance_image(load);
        if !modules.is_empty() {
            env.push(json!({ "name": "MODULES", "value": modules.join(" ") }));
        }
        let mut c = json!({
            "name": "valkey",
            "image": image,
            "command": ["sh", "/scripts/valkey.sh"],
            "env": env,
            "ports": [{ "name": "valkey", "containerPort": 6379 }],
            "readinessProbe": ping_probe(6379),
            "livenessProbe": { "tcpSocket": { "port": 6379 }, "initialDelaySeconds": 20, "periodSeconds": 10 },
            "securityContext": container_security(),
            "volumeMounts": [{ "name": "data", "mountPath": "/data" }, { "name": "scripts", "mountPath": "/scripts" }],
        });
        if let Some(r) = resources(&s.cpu, &s.memory, &s.memory_request) {
            c["resources"] = r;
        }
        let mut pvc = json!({ "accessModes": ["ReadWriteOnce"], "resources": { "requests": { "storage": s.storage } } });
        if !s.storage_class.is_empty() {
            pvc["storageClassName"] = json!(s.storage_class);
        }
        let mut o = self.object("apps/v1", "StatefulSet", sts, component);
        o["spec"] = json!({
            "serviceName": format!("{}-headless", self.name()),
            "replicas": replicas,
            "podManagementPolicy": "Parallel",
            // Restarted by `roll`, replicas first.
            "updateStrategy": { "type": "OnDelete" },
            "selector": { "matchLabels": selector(self.db, component) },
            "persistentVolumeClaimRetentionPolicy": { "whenDeleted": "Delete", "whenScaled": "Delete" },
            "template": {
                "metadata": { "labels": labels(self.db, component) },
                "spec": {
                    "affinity": spread(selector(self.db, component)),
                    "securityContext": pod_security(),
                    "terminationGracePeriodSeconds": 30,
                    "containers": [c],
                    "volumes": [self.scripts_volume()],
                },
            },
            "volumeClaimTemplates": [{ "metadata": { "name": "data", "labels": labels(self.db, component) }, "spec": pvc }],
        });
        o
    }

    pub fn sentinel_statefulset(&self) -> Value {
        let name = format!("{}-sentinel", self.name());
        let mut o = self.object("apps/v1", "StatefulSet", &name, "sentinel");
        o["spec"] = json!({
            "serviceName": name,
            "replicas": SENTINELS,
            "podManagementPolicy": "Parallel",
            "selector": { "matchLabels": selector(self.db, "sentinel") },
            "template": {
                "metadata": { "labels": labels(self.db, "sentinel") },
                "spec": {
                    "affinity": spread(selector(self.db, "sentinel")),
                    "securityContext": pod_security(),
                    "containers": [{
                        "name": "sentinel",
                        "image": self.image(),
                        "command": ["sh", "/scripts/sentinel.sh"],
                        "env": self.env(),
                        "ports": [{ "name": "sentinel", "containerPort": 26379 }],
                        "readinessProbe": ping_probe(26379),
                        "securityContext": container_security(),
                        "resources": { "requests": { "cpu": "20m", "memory": "32Mi" }, "limits": { "memory": "64Mi" } },
                        "volumeMounts": [{ "name": "conf", "mountPath": "/conf" }, { "name": "scripts", "mountPath": "/scripts" }],
                    }],
                    "volumes": [{ "name": "conf", "emptyDir": {} }, self.scripts_volume()],
                },
            },
        });
        o
    }

    /// pyo-kv: the database's address for the Redis protocol (the `<name>`
    /// service) and, with `rest`, the REST API. Two, on different servers.
    pub fn kv_deployment(&self) -> Value {
        let sec = secret_name(self.db);
        let rest = self.db.spec.rest;
        let mut env = vec![
            json!({ "name": "PYO_KV_ADDR", "value": format!("{}:6379", self.svc_host(&format!("{}-primary", self.name()))) }),
            json!({ "name": "PYO_KV_REST", "value": if rest { "1" } else { "0" } }),
            secret_env("PYO_KV_PASSWORD", &sec, "password", false),
            secret_env("PYO_KV_TOKEN", &sec, "token", !rest),
            secret_env("PYO_KV_READONLY_TOKEN", &sec, "readonly-token", true),
        ];
        if self.db.spec.instances > 1 {
            // Follow the sentinels directly: a failover reaches clients within
            // a second, even while this controller is down.
            let s: Vec<String> = (0..SENTINELS)
                .map(|i| {
                    format!(
                        "{}:26379",
                        self.svc_host(&format!("{}-sentinel-{i}", self.name()))
                    )
                })
                .collect();
            env.push(json!({ "name": "PYO_KV_SENTINELS", "value": s.join(",") }));
            env.push(json!({ "name": "PYO_KV_SENTINEL_NAME", "value": self.name() }));
        }
        // Every instance by its own service (named after its pod), for the
        // INFO on /metrics.
        let reads = self.db.spec.read_instances();
        let members: Vec<String> = (0..self.db.spec.instances)
            .map(|i| format!("{}-{i}", self.name()))
            .chain((0..reads).map(|i| format!("{}-read-{i}", self.name())))
            .map(|svc| format!("{}:6379", self.svc_host(&svc)))
            .collect();
        env.push(json!({ "name": "PYO_KV_MEMBERS", "value": members.join(",") }));
        if reads > 0 {
            // Read-only requests go to the read replicas, each by its own
            // service, so pyo-kv can tell which are healthy and caught up.
            let r: Vec<String> = (0..reads)
                .map(|i| {
                    format!(
                        "{}:6379",
                        self.svc_host(&format!("{}-read-{i}", self.name()))
                    )
                })
                .collect();
            env.push(json!({ "name": "PYO_KV_READ_REPLICAS", "value": r.join(",") }));
        }
        let mut o = self.object("apps/v1", "Deployment", &format!("{}-kv", self.name()), KV);
        o["spec"] = json!({
            "replicas": 2,
            "selector": { "matchLabels": selector(self.db, KV) },
            "template": {
                "metadata": { "labels": labels(self.db, KV) },
                "spec": {
                    "affinity": spread(selector(self.db, KV)),
                    "securityContext": { "runAsNonRoot": true, "runAsUser": 65532, "runAsGroup": 65532, "seccompProfile": { "type": "RuntimeDefault" } },
                    // Off a lost server in 10s, not the default 5 minutes.
                    "tolerations": [
                        { "key": "node.kubernetes.io/unreachable", "operator": "Exists", "effect": "NoExecute", "tolerationSeconds": 10 },
                        { "key": "node.kubernetes.io/not-ready", "operator": "Exists", "effect": "NoExecute", "tolerationSeconds": 10 },
                    ],
                    "terminationGracePeriodSeconds": 5,
                    "containers": [{
                        "name": "pyo-kv",
                        "image": self.ctx.images.pyo_kv,
                        "env": env,
                        // metrics: /metrics on 9121, apart from the port apps use.
                        "ports": [{ "name": "http", "containerPort": 8080 }, { "name": "redis", "containerPort": 6379 }, { "name": "metrics", "containerPort": 9121 }],
                        // Ready while the process answers, whatever Valkey is
                        // doing: during a failover clients get an error they
                        // can retry on, not a refused connection. A frozen
                        // pod stops answering and leaves the service in ~4s.
                        "readinessProbe": { "httpGet": { "path": "/_ready", "port": 8080 }, "periodSeconds": 2, "timeoutSeconds": 1, "failureThreshold": 2 },
                        // A hung one is restarted within ~20s, which closes
                        // its clients' connections so they reconnect to the
                        // other.
                        "livenessProbe": { "httpGet": { "path": "/_ready", "port": 8080 }, "periodSeconds": 5, "timeoutSeconds": 2, "failureThreshold": 3 },
                        "securityContext": { "allowPrivilegeEscalation": false, "readOnlyRootFilesystem": true, "capabilities": { "drop": ["ALL"] } },
                        // It holds whole replies while passing them on.
                        "resources": { "requests": { "cpu": "50m", "memory": "32Mi" }, "limits": { "memory": "512Mi" } },
                    }],
                },
            },
        });
        o
    }

    /// Keeps one pyo-kv up through a drain.
    pub fn kv_budget(&self) -> Value {
        let mut o = self.object(
            "policy/v1",
            "PodDisruptionBudget",
            &format!("{}-kv", self.name()),
            KV,
        );
        o["spec"] =
            json!({ "minAvailable": 1, "selector": { "matchLabels": selector(self.db, KV) } });
        o
    }

    pub async fn valkey(
        &self,
        mut st: DatabaseStatus,
        secret: &Secret,
    ) -> anyhow::Result<(DatabaseStatus, Duration)> {
        let name = self.name();
        let n = self.db.spec.instances;
        let password = secret
            .data
            .as_ref()
            .and_then(|d| d.get("password"))
            .map(|b| String::from_utf8_lossy(&b.0).into_owned())
            .unwrap_or_default();
        let sts_api: Api<StatefulSet> = Api::namespaced(self.ctx.client.clone(), &self.ns());

        let primary = self.primary(&sts_api, &password, &st).await?;

        // Scaling down would delete the primary when it's one of the
        // instances going away: hold at the current size until a failover
        // moves it.
        let current = sts_api
            .get_opt(&name)
            .await?
            .and_then(|s| s.spec.and_then(|s| s.replicas))
            .unwrap_or(0);
        let (replicas, held) = match ordinal(&primary) {
            Some(o) if current > n && o >= n => (
                current,
                Some(format!(
                    "Can't go down to {n} instances while {primary} is the primary"
                )),
            ),
            _ => (n, None),
        };

        let modules = self.module_plan(&primary, &password, &st).await?;

        let mut cm = self.object("v1", "ConfigMap", &format!("{name}-scripts"), "valkey");
        cm["data"] = json!({ "valkey.sh": VALKEY_SH, "sentinel.sh": SENTINEL_SH });
        // <name> is the address apps use: pyo-kv once one is ready, the
        // primary itself until then.
        let deploys: Api<k8s_openapi::api::apps::v1::Deployment> =
            Api::namespaced(self.ctx.client.clone(), &self.ns());
        let kv_ready = deploys
            .get_opt(&format!("{name}-kv"))
            .await?
            .and_then(|d| d.status?.ready_replicas)
            .unwrap_or(0)
            > 0;
        let main = if kv_ready {
            self.service(&name, KV, selector(self.db, KV), 6379, 6379, false)
        } else {
            self.service(
                &name,
                "valkey",
                self.with_role("primary"),
                6379,
                6379,
                false,
            )
        };
        let mut objs = vec![
            cm,
            self.service(
                &format!("{name}-headless"),
                "valkey",
                selector(self.db, "valkey"),
                6379,
                6379,
                true,
            ),
            self.service(
                &format!("{name}-primary"),
                "valkey",
                self.with_role("primary"),
                6379,
                6379,
                false,
            ),
            main,
            self.service(
                &format!("{name}-replicas"),
                "valkey",
                self.with_role("replica"),
                6379,
                6379,
                false,
            ),
            self.valkey_statefulset(replicas, &modules.load),
        ];
        // A service per instance and per sentinel, named after its pod: see
        // scripts/valkey.sh.
        for i in 0..replicas {
            objs.push(self.pod_service(&format!("{name}-{i}"), "valkey", 6379));
        }
        let sentinels = replicas > 1;
        if sentinels {
            for i in 0..SENTINELS {
                objs.push(self.pod_service(&format!("{name}-sentinel-{i}"), "sentinel", 26379));
            }
            objs.push(self.service(
                &format!("{name}-sentinel"),
                "sentinel",
                selector(self.db, "sentinel"),
                26379,
                26379,
                true,
            ));
            objs.push(self.sentinel_statefulset());
        }
        let reads = self.db.spec.read_instances();
        let had = sts_api
            .get_opt(&format!("{name}-read"))
            .await?
            .and_then(|s| s.spec?.replicas)
            .unwrap_or(0);
        if reads > 0 {
            for i in 0..reads {
                objs.push(self.pod_service(&format!("{name}-read-{i}"), READ, 6379));
            }
            objs.push(self.service(
                &format!("{name}-read"),
                READ,
                selector(self.db, READ),
                6379,
                6379,
                false,
            ));
            objs.push(self.read_statefulset(&modules.load));
        }
        // After the services it's given.
        objs.push(self.kv_deployment());
        objs.push(self.kv_budget());
        if self.db.spec.rest {
            objs.push(self.service(
                &format!("{name}-rest"),
                "rest",
                selector(self.db, KV),
                80,
                8080,
                false,
            ));
        }
        for o in &objs {
            self.apply(o).await?;
        }
        // Per-instance Services of instances it no longer has, up to the most it can.
        for i in replicas..crate::crd::MAX_INSTANCES {
            self.remove("v1", "Service", &format!("{name}-{i}")).await?;
        }
        if !sentinels {
            self.remove("apps/v1", "StatefulSet", &format!("{name}-sentinel"))
                .await?;
            self.remove("v1", "Service", &format!("{name}-sentinel"))
                .await?;
            for i in 0..SENTINELS {
                self.remove("v1", "Service", &format!("{name}-sentinel-{i}"))
                    .await?;
            }
        }
        if !self.db.spec.rest {
            self.remove("v1", "Service", &format!("{name}-rest"))
                .await?;
        }
        self.read_replicas_event(had, reads).await;
        for i in reads..crd::MAX_VALKEY_READ_REPLICAS {
            self.remove("v1", "Service", &format!("{name}-read-{i}"))
                .await?;
        }
        if reads == 0 {
            self.remove("apps/v1", "StatefulSet", &format!("{name}-read"))
                .await?;
            self.remove("v1", "Service", &format!("{name}-read"))
                .await?;
        }

        // Point the primary service at the primary.
        self.label_roles(&primary).await?;
        if sentinels {
            self.prune_sentinels(&password).await?;
        }

        let rolling = self.roll(&primary, &password, modules.unloading()).await?;
        st.extensions = modules.status;

        let ready = sts_api
            .get_opt(&name)
            .await?
            .and_then(|s| s.status)
            .and_then(|s| s.ready_replicas)
            .unwrap_or(0);
        if !st.primary.is_empty() && primary != st.primary {
            st.failovers += 1;
        }
        // Roles as just labelled: the sentinels' primary.
        let pods = self
            .pods(&format!("{LABEL_DATABASE}={name},{LABEL_COMPONENT}=valkey"))
            .await?;
        st.members = crate::controller::members(&pods, &primary, None);
        st.primary = primary;
        st.instances = n;
        st.ready_instances = ready;
        phase_for(&mut st, ready, n, ready > 0);
        if let Some(h) = held.or(rolling) {
            st.message = h;
        }
        st.endpoints = Default::default();
        st.endpoints.primary = format!("{}:6379", self.svc_host(&name));
        if n > 1 {
            st.endpoints.replicas = format!("{}:6379", self.svc_host(&format!("{name}-replicas")));
            st.endpoints.sentinel = format!("{}:26379", self.svc_host(&format!("{name}-sentinel")));
        }
        if self.db.spec.rest {
            st.endpoints.rest = format!("http://{}", self.svc_host(&format!("{name}-rest")));
        }
        st.read_replicas = None;
        if reads > 0 {
            st.endpoints.read = format!("{}:6379", self.svc_host(&format!("{name}-read")));
            st.read_replicas = Some(self.read_status(&st.primary, &password).await?);
        }
        // Sentinel can move the primary any time; look again soon.
        Ok((st, Duration::from_secs(5)))
    }

    /// Restarts the instances whose pod template changed (modules, version,
    /// size; the StatefulSets are `OnDelete`), one at a time, each once every
    /// instance is ready and every replica streams from the primary: read
    /// replicas, then replicas, then the primary. With sentinels the primary
    /// first hands over to a replica that has restarted (`hand_over`), and
    /// restarts as a replica on a later pass. What it's doing, for the status.
    async fn roll(
        &self,
        primary: &str,
        password: &str,
        unloading: bool,
    ) -> anyhow::Result<Option<String>> {
        let name = self.name();
        let sts_api: Api<StatefulSet> = Api::namespaced(self.ctx.client.clone(), &self.ns());
        // (pod, ready, read replica) of each instance not on the latest template.
        let mut stale: Vec<(String, bool, bool)> = vec![];
        let mut all_ready = true;
        let mut total = 0;
        for (sts, component) in [(name.clone(), "valkey"), (format!("{name}-read"), READ)] {
            let Some(rev) = sts_api
                .get_opt(&sts)
                .await?
                .and_then(|s| s.status?.update_revision)
            else {
                continue;
            };
            for p in self
                .pods(&format!(
                    "{LABEL_DATABASE}={name},{LABEL_COMPONENT}={component}"
                ))
                .await?
            {
                let pod = p.metadata.name.clone().unwrap_or_default();
                if p.metadata.deletion_timestamp.is_some() {
                    return Ok(Some(format!("Restarting {pod}")));
                }
                let ready = p
                    .status
                    .as_ref()
                    .and_then(|s| s.conditions.as_ref())
                    .is_some_and(|c| c.iter().any(|c| c.type_ == "Ready" && c.status == "True"));
                all_ready &= ready;
                total += 1;
                if p.metadata
                    .labels
                    .as_ref()
                    .and_then(|l| l.get("controller-revision-hash"))
                    != Some(&rev)
                {
                    stale.push((pod, ready, component == READ));
                }
            }
        }
        if stale.is_empty() {
            return Ok(None);
        }
        stale.sort();
        // One that isn't serving anyway goes at once.
        if let Some((pod, ..)) = stale.iter().find(|(p, ready, _)| !ready && p != primary) {
            self.delete_pod(pod).await?;
            return Ok(Some(format!("Restarting {pod}")));
        }
        let waiting = Some(format!("Restarting {} of {total} instances", stale.len()));
        if !all_ready {
            return Ok(waiting);
        }
        let info = info_replication(&format!("{}:6379", self.svc_host(primary)), password)
            .await
            .unwrap_or_default();
        if replica_lags(&info).len() + 1 < total {
            return Ok(waiting);
        }
        if let Some((pod, ..)) = stale
            .iter()
            .find(|s| s.2)
            .or_else(|| stale.iter().find(|s| s.0 != primary))
        {
            if unloading && !self.rewrite_aof(pod, password).await? {
                return Ok(Some(format!(
                    "{pod} rewrites its append-only file before it restarts"
                )));
            }
            tracing::info!(db = %name, %pod, "restarting for a new template");
            self.delete_pod(pod).await?;
            return Ok(Some(format!("Restarting {pod}")));
        }
        // Only the primary is left.
        if self.db.spec.instances > 1 {
            if let Err(e) = self.hand_over(primary, password).await {
                tracing::warn!(db = %name, error = format!("{e:#}"), "hand-over");
                return Ok(Some(format!(
                    "{primary} couldn't hand over before restarting: {e:#}"
                )));
            }
            return Ok(Some(format!(
                "{primary} hands over to a replica before it restarts"
            )));
        }
        if unloading && !self.rewrite_aof(primary, password).await? {
            return Ok(Some(format!(
                "{primary} rewrites its append-only file before it restarts"
            )));
        }
        tracing::info!(db = %name, pod = %primary, "restarting the only instance for a new template");
        self.delete_pod(primary).await?;
        Ok(Some(format!("Restarting {primary}")))
    }

    async fn delete_pod(&self, pod: &str) -> anyhow::Result<()> {
        let pods: Api<Pod> = Api::namespaced(self.ctx.client.clone(), &self.ns());
        match pods.delete(pod, &Default::default()).await {
            Ok(_) => Ok(()),
            Err(e) if crate::controller::not_found(&e) => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    /// Moves the primary to a caught-up replica without losing writes: the
    /// primary pauses writes, a replica gets everything, the sentinels fail
    /// over to it, and the old primary becomes its replica before the pause
    /// ends. Paused writes then fail there (READONLY) instead of being lost.
    /// (Sentinel's own reconfiguration of the old primary waits behind the
    /// pause, so the controller does that part.) Once per 30s at most.
    async fn hand_over(&self, primary: &str, password: &str) -> anyhow::Result<()> {
        let key = format!("{}/{}", self.ns(), self.name());
        if self
            .ctx
            .handed_over
            .lock()
            .unwrap()
            .get(&key)
            .is_some_and(|t| t.elapsed() < Duration::from_secs(30))
        {
            return Ok(());
        }
        self.ctx
            .handed_over
            .lock()
            .unwrap()
            .insert(key, std::time::Instant::now());
        let name = self.name();
        let svcs: Api<Service> = Api::namespaced(self.ctx.client.clone(), &self.ns());
        let ip = svcs
            .get(primary)
            .await?
            .spec
            .and_then(|s| s.cluster_ip)
            .ok_or_else(|| anyhow::anyhow!("{primary} has no address"))?;
        let sentinels: Vec<String> = self
            .pod_service_ips("sentinel")
            .await?
            .into_iter()
            .map(|ip| format!("{ip}:26379"))
            .collect();
        let replicas = self.pod_service_ips("valkey").await?;
        let mut p =
            pyo_resp::Conn::connect(&format!("{ip}:6379"), password, Duration::from_secs(2))
                .await?;
        let paused = p.cmd(&["CLIENT", "PAUSE", "10000", "WRITE"]).await?;
        anyhow::ensure!(!paused.is_error(), "CLIENT PAUSE: {}", paused.text());
        tracing::info!(db = %name, %primary, "handing over: writes paused");
        let moved = async {
            let until = tokio::time::Instant::now() + Duration::from_secs(2);
            while !caught_up(&p.cmd(&["INFO", "replication"]).await?.text(), &replicas) {
                anyhow::ensure!(tokio::time::Instant::now() < until, "no replica caught up");
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            let mut said = String::from("no sentinel answered");
            let mut asked = false;
            for s in &sentinels {
                let Ok(Ok(mut c)) = tokio::time::timeout(
                    Duration::from_secs(2),
                    pyo_resp::Conn::connect(s, password, Duration::from_secs(2)),
                )
                .await
                else {
                    continue;
                };
                match tokio::time::timeout(
                    Duration::from_secs(2),
                    c.cmd(&["SENTINEL", "FAILOVER", name.as_str()]),
                )
                .await
                {
                    Ok(Ok(r)) if !r.is_error() => {
                        asked = true;
                        break;
                    }
                    Ok(Ok(r)) => said = r.text(),
                    _ => {}
                }
            }
            anyhow::ensure!(asked, "SENTINEL FAILOVER: {said}");
            let until = tokio::time::Instant::now() + Duration::from_secs(9);
            loop {
                if let Ok(h) = pyo_resp::sentinel_primary(&sentinels, password, &name).await {
                    let host = h.rsplit_once(':').map_or(h.as_str(), |(h, _)| h).to_owned();
                    if host != ip {
                        let r = p.cmd(&["REPLICAOF", host.as_str(), "6379"]).await?;
                        anyhow::ensure!(!r.is_error(), "REPLICAOF: {}", r.text());
                        tracing::info!(db = %name, %primary, new = %host, "handed over");
                        return Ok(());
                    }
                }
                anyhow::ensure!(
                    tokio::time::Instant::now() < until,
                    "the sentinels didn't move the primary"
                );
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        }
        .await;
        let _ = p.cmd(&["CLIENT", "UNPAUSE"]).await;
        moved
    }

    /// What the instances load and each module's state (modules.rs), from
    /// every instance's own `MODULE LIST` and the primary's INFO; nothing to
    /// ask while none is wanted or reported.
    async fn module_plan(
        &self,
        primary: &str,
        password: &str,
        st: &DatabaseStatus,
    ) -> anyhow::Result<crate::modules::Plan> {
        let wanted = self.wanted_modules();
        if wanted.is_empty() && st.extensions.is_empty() {
            return Ok(crate::modules::Plan {
                load: vec![],
                status: vec![],
            });
        }
        let mut ips = self.pod_service_ips("valkey").await?;
        ips.extend(self.pod_service_ips(READ).await?);
        let ask = |addr: String, cmd: &'static [&'static str]| async move {
            let ask = async {
                let mut c =
                    pyo_resp::Conn::connect(&addr, password, Duration::from_secs(2)).await?;
                c.cmd(cmd).await
            };
            match tokio::time::timeout(Duration::from_secs(3), ask).await {
                Ok(Ok(v)) if !v.is_error() => Some(v),
                _ => None,
            }
        };
        let lists = futures::future::join_all(
            ips.iter()
                .map(|ip| ask(format!("{ip}:6379"), &["MODULE", "LIST"])),
        );
        let (lists, info) = tokio::join!(
            lists,
            ask(
                format!("{}:6379", self.svc_host(primary)),
                &["INFO", "everything"]
            )
        );
        let have = crate::modules::Have {
            members: lists
                .into_iter()
                .flatten()
                .map(|v| crate::modules::loaded(&v))
                .collect(),
            instances: ips.len(),
            primary_info: info.map(|v| v.text()).filter(|i| i.contains("role:master")),
        };
        let plan = crate::modules::plan(wanted, self.modules_available(), &have, &st.extensions);
        if plan.status != st.extensions {
            tracing::info!(db = %self.name(), members = ?have.members, of = have.instances, primary_info = have.primary_info.is_some(), load = ?plan.load, "modules");
        }
        Ok(plan)
    }

    /// Has an instance rewrite its append-only file (so it no longer replays
    /// a module's commands it won't have after the restart) and waits up to
    /// a minute: whether it's done.
    async fn rewrite_aof(&self, pod: &str, password: &str) -> anyhow::Result<bool> {
        let svcs: Api<Service> = Api::namespaced(self.ctx.client.clone(), &self.ns());
        let Some(ip) = svcs.get_opt(pod).await?.and_then(|s| s.spec?.cluster_ip) else {
            return Ok(false);
        };
        let mut c =
            pyo_resp::Conn::connect(&format!("{ip}:6379"), password, Duration::from_secs(2))
                .await?;
        let r = c.cmd(&["BGREWRITEAOF"]).await?;
        if r.is_error() && !r.text().contains("in progress") {
            anyhow::bail!("BGREWRITEAOF on {pod}: {}", r.text());
        }
        let until = tokio::time::Instant::now() + Duration::from_secs(60);
        while tokio::time::Instant::now() < until {
            tokio::time::sleep(Duration::from_millis(200)).await;
            let i = c.cmd(&["INFO", "persistence"]).await?.text();
            let f = |k: &str| {
                i.lines()
                    .find_map(|l| l.trim_end().strip_prefix(k)?.strip_prefix(':'))
                    .unwrap_or("")
                    .to_owned()
            };
            if f("aof_rewrite_in_progress") == "0" && f("aof_rewrite_scheduled") == "0" {
                anyhow::ensure!(
                    f("aof_last_bgrewrite_status") == "ok",
                    "{pod} couldn't rewrite its append-only file"
                );
                tracing::info!(db = %self.name(), %pod, "append-only file rewritten");
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// The addresses of a component's per-pod services, from the API rather
    /// than DNS: a DNS server on the server that just died would slow every
    /// lookup right when the primary has to move.
    async fn pod_service_ips(&self, component: &str) -> anyhow::Result<Vec<String>> {
        let svcs: Api<Service> = Api::namespaced(self.ctx.client.clone(), &self.ns());
        let lp = ListParams::default().labels(&format!(
            "{LABEL_DATABASE}={},{LABEL_COMPONENT}={component}",
            self.name()
        ));
        Ok(svcs
            .list(&lp)
            .await?
            .into_iter()
            .filter(|s| s.metadata.name.as_deref().and_then(ordinal).is_some())
            .filter_map(|s| s.spec?.cluster_ip.filter(|ip| ip != "None"))
            .collect())
    }

    /// Makes the sentinels forget instances that are gone (scaled down, or
    /// read replicas turned off). They'd otherwise keep them for good, and
    /// after a failover they move the remaining replicas one at a time,
    /// waiting on each: a gone one holds up the rest until the failover
    /// times out (30s). One sentinel per pass, only while no failover is
    /// under way, so the others keep their quorum; it relearns the replicas
    /// from the primary within seconds.
    async fn prune_sentinels(&self, password: &str) -> anyhow::Result<()> {
        let key = format!("{}/{}", self.ns(), self.name());
        if self
            .ctx
            .pruned
            .lock()
            .unwrap()
            .get(&key)
            .is_some_and(|t| t.elapsed() < Duration::from_secs(15))
        {
            return Ok(()); // the last one is still relearning
        }
        let sentinels = self.pod_service_ips("sentinel").await?;
        if sentinels.len() != SENTINELS {
            return Ok(());
        }
        let mut known = self.pod_service_ips("valkey").await?;
        known.extend(self.pod_service_ips(READ).await?);
        let name = self.name();
        for s in &sentinels {
            let addr = format!("{s}:26379");
            let ask = async {
                let mut c =
                    pyo_resp::Conn::connect(&addr, password, Duration::from_secs(2)).await?;
                let master = c.cmd(&["SENTINEL", "MASTER", name.as_str()]).await?;
                let replicas = c.cmd(&["SENTINEL", "REPLICAS", name.as_str()]).await?;
                std::io::Result::Ok((c, master, replicas))
            };
            let Ok(Ok((mut c, master, replicas))) =
                tokio::time::timeout(Duration::from_secs(3), ask).await
            else {
                continue;
            };
            if field(&master, "flags").as_deref() != Some("master") {
                return Ok(()); // failing over, or the primary looks down: not now
            }
            let gone: Vec<String> = match &replicas {
                pyo_resp::Value::Array(rs) => rs
                    .iter()
                    .filter_map(|r| field(r, "ip"))
                    .filter(|ip| !known.contains(ip))
                    .collect(),
                _ => continue,
            };
            if gone.is_empty() {
                continue;
            }
            tracing::info!(db = %name, sentinel = %s, ?gone, "the sentinel forgets instances that are gone");
            self.ctx
                .pruned
                .lock()
                .unwrap()
                .insert(key, std::time::Instant::now());
            let _ = tokio::time::timeout(
                Duration::from_secs(3),
                c.cmd(&["SENTINEL", "RESET", name.as_str()]),
            )
            .await;
            return Ok(());
        }
        Ok(())
    }

    /// The primary's pod name.
    async fn primary(
        &self,
        sts_api: &Api<StatefulSet>,
        password: &str,
        st: &DatabaseStatus,
    ) -> anyhow::Result<String> {
        let name = self.name();
        let first = format!("{name}-0");
        let ready = sts_api
            .get_opt(&format!("{name}-sentinel"))
            .await?
            .and_then(|s| s.status)
            .and_then(|s| s.ready_replicas)
            .unwrap_or(0);
        if ready == 0 {
            // No sentinels (a single instance, or not started yet): -0 is primary.
            return Ok(first);
        }
        let addrs: Vec<String> = self
            .pod_service_ips("sentinel")
            .await?
            .into_iter()
            .map(|ip| format!("{ip}:26379"))
            .collect();
        // Keep what we had rather than guess.
        let keep = if st.primary.is_empty() {
            first
        } else {
            st.primary.clone()
        };
        if addrs.len() != SENTINELS {
            return Ok(keep); // still being made
        }
        let host = match pyo_resp::sentinel_primary(&addrs, password, &name).await {
            Ok(h) => h.rsplit_once(':').map(|(h, _)| h.to_owned()).unwrap_or(h),
            Err(e) => {
                tracing::warn!(db = %name, error = %e, "sentinels");
                return Ok(keep);
            }
        };
        // Only an instance that says it's primary gets the primary service:
        // sentinels can name one that's gone or not yet promoted.
        if !pyo_resp::is_primary(&format!("{host}:6379"), password).await {
            tracing::warn!(db = %name, %host, "the sentinels' primary doesn't say it's primary");
            return Ok(keep);
        }
        // Sentinels give the address of the instance's own service: find it.
        let svcs: Api<Service> = Api::namespaced(self.ctx.client.clone(), &self.ns());
        let lp = ListParams::default()
            .labels(&format!("{LABEL_DATABASE}={name},{LABEL_COMPONENT}=valkey"));
        let pod = svcs.list(&lp).await?.into_iter().find_map(|s| {
            let ip = s.spec.as_ref()?.cluster_ip.as_deref()?;
            (ip == host && s.metadata.name.as_deref().and_then(ordinal).is_some())
                .then(|| s.metadata.name.clone())
                .flatten()
        });
        Ok(pod.unwrap_or(keep))
    }

    /// The read replicas, with their lag from the primary's INFO.
    async fn read_status(
        &self,
        primary: &str,
        password: &str,
    ) -> anyhow::Result<crd::ReadReplicasStatus> {
        let name = self.name();
        let pods = self
            .pods(&format!("{LABEL_DATABASE}={name},{LABEL_COMPONENT}={READ}"))
            .await?;
        // Each read replica is known to the primary by its service's address.
        let svcs: Api<Service> = Api::namespaced(self.ctx.client.clone(), &self.ns());
        let lp = ListParams::default()
            .labels(&format!("{LABEL_DATABASE}={name},{LABEL_COMPONENT}={READ}"));
        let ips: std::collections::HashMap<String, String> = svcs
            .list(&lp)
            .await?
            .into_iter()
            .filter_map(|s| {
                Some((
                    s.metadata.name?,
                    s.spec?.cluster_ip.filter(|ip| ip != "None")?,
                ))
            })
            .collect();
        let info = info_replication(&format!("{}:6379", self.svc_host(primary)), password)
            .await
            .unwrap_or_default();
        let lags = replica_lags(&info);
        let mut members: Vec<crd::ReadMember> = crate::controller::members(&pods, "", None)
            .into_iter()
            .map(|m| {
                let lag = ips.get(&m.name).and_then(|ip| lags.get(ip)).copied();
                crd::ReadMember {
                    name: m.name,
                    node: m.node,
                    ready: m.ready,
                    lag_seconds: lag,
                }
            })
            .collect();
        members.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(crd::ReadReplicasStatus {
            ready: members.iter().filter(|m| m.ready).count() as i32,
            members,
        })
    }

    /// Marks the primary pod and the replicas, which the services select.
    async fn label_roles(&self, primary: &str) -> anyhow::Result<()> {
        let pods: Api<Pod> = Api::namespaced(self.ctx.client.clone(), &self.ns());
        let lp = ListParams::default().labels(&format!(
            "{LABEL_DATABASE}={},{LABEL_COMPONENT}=valkey",
            self.name()
        ));
        for p in pods.list(&lp).await? {
            let pname = p.metadata.name.clone().unwrap_or_default();
            let role = if pname == primary {
                "primary"
            } else {
                "replica"
            };
            if p.metadata
                .labels
                .as_ref()
                .and_then(|l| l.get(LABEL_ROLE))
                .map(String::as_str)
                == Some(role)
            {
                continue;
            }
            pods.patch(
                &pname,
                &PatchParams::default(),
                &Patch::Merge(json!({ "metadata": { "labels": { LABEL_ROLE: role } } })),
            )
            .await?;
        }
        Ok(())
    }
}

/// A field of a flat key/value array reply (SENTINEL MASTER and the like).
fn field(v: &pyo_resp::Value, key: &str) -> Option<String> {
    let pyo_resp::Value::Array(kv) = v else {
        return None;
    };
    kv.chunks(2)
        .find(|p| p.len() == 2 && p[0].text() == key)
        .map(|p| p[1].text())
}

/// The primary's `INFO replication`.
async fn info_replication(addr: &str, password: &str) -> Option<String> {
    let ask = async {
        let mut c = pyo_resp::Conn::connect(addr, password, Duration::from_secs(2)).await?;
        c.cmd(&["INFO", "replication"]).await
    };
    match tokio::time::timeout(Duration::from_secs(3), ask).await {
        Ok(Ok(v)) if !v.is_error() => Some(v.text()),
        _ => None,
    }
}

/// Whether one of these replicas (addresses) has everything the primary
/// has, from its `INFO replication`.
pub fn caught_up(info: &str, replicas: &[String]) -> bool {
    let field = |l: &str, k: &str| {
        l.split(',')
            .find_map(|kv| kv.strip_prefix(k)?.strip_prefix('='))
            .map(str::to_owned)
    };
    let Some(offset) = info.lines().find_map(|l| {
        l.trim()
            .strip_prefix("master_repl_offset:")?
            .parse::<i64>()
            .ok()
    }) else {
        return false;
    };
    info.lines().any(|l| {
        let Some((k, v)) = l.trim().split_once(':') else {
            return false;
        };
        k.starts_with("slave")
            && field(v, "state").as_deref() == Some("online")
            && field(v, "ip").is_some_and(|ip| replicas.contains(&ip))
            && field(v, "offset").and_then(|o| o.parse::<i64>().ok()) == Some(offset)
    })
}

/// Each online replica's lag in seconds, by address, from the primary's
/// `INFO replication`. Caught up (same offset): 0; otherwise the seconds
/// since it last acknowledged.
pub fn replica_lags(info: &str) -> std::collections::HashMap<String, f64> {
    let field = |l: &str, k: &str| {
        l.split(',')
            .find_map(|kv| kv.strip_prefix(k)?.strip_prefix('='))
            .map(str::to_owned)
    };
    let offset: Option<i64> = info
        .lines()
        .find_map(|l| l.trim().strip_prefix("master_repl_offset:")?.parse().ok());
    info.lines()
        .filter_map(|l| {
            let (k, v) = l.trim().split_once(':')?;
            if !(k.starts_with("slave") && k[5..].chars().all(|c| c.is_ascii_digit()))
                || field(v, "state")? != "online"
            {
                return None;
            }
            let caught_up = offset.is_some() && field(v, "offset")?.parse::<i64>().ok() == offset;
            let lag = if caught_up {
                0.0
            } else {
                field(v, "lag")?.parse().ok()?
            };
            Some((field(v, "ip")?, lag))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fields() {
        use pyo_resp::Value::{Array, Bulk};
        let b = |s: &str| Bulk(s.as_bytes().to_vec());
        let m = Array(vec![
            b("name"),
            b("cache"),
            b("flags"),
            b("master"),
            b("ip"),
            b("10.0.0.1"),
        ]);
        assert_eq!(field(&m, "flags").as_deref(), Some("master"));
        assert_eq!(field(&m, "ip").as_deref(), Some("10.0.0.1"));
        assert_eq!(field(&m, "nope"), None);
        assert_eq!(field(&b("x"), "ip"), None);
    }

    #[test]
    fn lags() {
        let info = "# Replication\r\nrole:master\r\nconnected_slaves:3\r\n\
            slave0:ip=10.0.0.1,port=6379,state=online,offset=500,lag=1\r\n\
            slave1:ip=10.0.0.2,port=6379,state=online,offset=420,lag=3\r\n\
            slave2:ip=10.0.0.3,port=6379,state=wait_bgsave,offset=0,lag=0\r\n\
            master_repl_offset:500\r\n";
        let l = replica_lags(info);
        assert_eq!(l.get("10.0.0.1"), Some(&0.0));
        assert_eq!(l.get("10.0.0.2"), Some(&3.0));
        assert_eq!(l.get("10.0.0.3"), None);
        assert!(replica_lags("").is_empty());
    }

    #[test]
    fn caught_up_replicas() {
        let info = "role:master\r\nslave0:ip=10.0.0.1,port=6379,state=online,offset=500,lag=0\r\n\
            slave1:ip=10.0.0.2,port=6379,state=online,offset=420,lag=1\r\nmaster_repl_offset:500\r\n";
        assert!(caught_up(info, &["10.0.0.1".into()]));
        assert!(!caught_up(info, &["10.0.0.2".into()]), "behind");
        assert!(
            !caught_up(info, &["10.0.0.9".into()]),
            "a read replica doesn't count"
        );
        assert!(!caught_up("", &["10.0.0.1".into()]));
    }

    #[test]
    fn memory() {
        assert_eq!(maxmemory("1Gi"), Some(805306368));
        assert_eq!(maxmemory("512M"), Some(384000000));
        assert_eq!(maxmemory("1000"), Some(750));
        assert_eq!(maxmemory("lots"), None);
        assert_eq!(ordinal("cache-2"), Some(2));
        assert_eq!(ordinal("cache"), None);
    }
}
