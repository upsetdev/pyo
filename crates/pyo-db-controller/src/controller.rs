//! Turns Databases into running Postgres, Valkey or MySQL and reports
//! what's running in their status.

use std::{sync::Arc, time::Duration};

use anyhow::anyhow;
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use k8s_openapi::{
    ByteString,
    api::core::v1::{Pod, Secret},
    apimachinery::pkg::apis::meta::v1::ObjectMeta,
};
use kube::{
    Api, Client, Resource, ResourceExt,
    api::{
        DeleteParams, DynamicObject, GroupVersionKind, ListParams, Patch, PatchParams, PostParams,
    },
    core::ApiResource,
    runtime::{
        controller::Action,
        events::{Event, EventType, Recorder},
    },
};
use rand::RngCore;
use serde_json::{Value, json};

use crate::crd::{self, Database, DatabaseStatus, Engine};

pub const OWNER: &str = "pyo-db-controller";
pub const LABEL_DATABASE: &str = "pyo.dev/database";
pub const LABEL_COMPONENT: &str = "pyo.dev/component";
pub const LABEL_ROLE: &str = "pyo.dev/role";
pub const LABEL_MANAGED_BY: &str = "app.kubernetes.io/managed-by";

/// Images the controller runs. The bundle pins them; Pyo's admission policy
/// allows only these.
#[derive(Clone, Debug)]
pub struct Images {
    /// Gets ":<version>" appended (CloudNativePG's images), for a version
    /// without one in `postgres`.
    pub postgres_repo: String,
    /// Pyo's Postgres image per major version ("18" -> a full reference).
    pub postgres: std::collections::BTreeMap<String, String>,
    /// Gets ":<version>" appended.
    pub valkey_repo: String,
    /// Valkey 8.1 with its modules (valkey-bundle, a full reference): what
    /// an instance runs while it loads any (modules.rs).
    pub valkey_bundle: String,
    pub pyo_kv: String,
    /// MOCO's mysqld image for MySQL 8.4 (a full reference: MOCO's tags
    /// are the patch release, e.g. 8.4.10.1).
    pub mysql_84: String,
}

pub struct Ctx {
    pub client: Client,
    /// Events on the Databases, for Pyo's log of what happened to them.
    pub recorder: Recorder,
    /// Backup events already sent (Backup uid and what happened), and when
    /// this controller started: backups that started or ended before that
    /// aren't announced again.
    pub announced: std::sync::Mutex<std::collections::HashSet<(String, &'static str)>>,
    pub started: String,
    /// When each Valkey's sentinels were last told to forget gone
    /// instances, by namespace/name: one sentinel at a time, apart.
    pub pruned: std::sync::Mutex<std::collections::HashMap<String, std::time::Instant>>,
    /// When each Valkey's primary was last handed over for a restart, by
    /// namespace/name: not again while that one settles.
    pub handed_over: std::sync::Mutex<std::collections::HashMap<String, std::time::Instant>>,
    pub images: Images,
    pub default_postgres: String,
    pub default_valkey: String,
    /// The cluster's DNS domain, for pod and service names.
    pub domain: String,
}

#[derive(Debug)]
pub struct Error(anyhow::Error);

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:#}", self.0)
    }
}

impl std::error::Error for Error {}

pub async fn reconcile(db: Arc<Database>, ctx: Arc<Ctx>) -> Result<Action, Error> {
    if db.meta().deletion_timestamp.is_some() {
        return Ok(Action::await_change()); // owner references clean up the rest
    }
    let before = db.status.clone().unwrap_or_default();
    let (mut st, requeue) = match run(&db, &ctx, before.clone()).await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(db = %db.name_any(), ns = db.namespace().unwrap_or_default(), error = format!("{e:#}"), "reconcile");
            let mut st = before.clone();
            if pyo_proto::transient_error(&format!("{e:#}")) {
                // The operators are still starting (right after they're
                // installed): not a failure. A running database keeps its phase.
                if st.phase.is_empty() || st.phase == crd::FAILED {
                    st.phase = crd::CREATING.into();
                }
                st.message = "Waiting for the database system to start".into();
                (st, Duration::from_secs(5))
            } else {
                st.phase = crd::FAILED.into();
                st.message = format!("{e:#}");
                (st, Duration::from_secs(15))
            }
        }
    };
    st.observed_generation = db.meta().generation.unwrap_or_default();
    for (warning, reason, note) in changes(&db, &before, &st) {
        event(&ctx, &db, warning, reason, note).await;
    }
    if st != before {
        let api: Api<Database> =
            Api::namespaced(ctx.client.clone(), &db.namespace().unwrap_or_default());
        // A merge patch keeps keys it doesn't mention: name the ones that
        // went away as null so the stored status is exactly st.
        let old = db
            .status
            .as_ref()
            .map(serde_json::to_value)
            .transpose()
            .map_err(|e| Error(e.into()))?
            .unwrap_or(Value::Null);
        let new = serde_json::to_value(&st).map_err(|e| Error(e.into()))?;
        api.patch_status(
            &db.name_any(),
            &PatchParams::default(),
            &Patch::Merge(json!({ "status": merge_patch(&old, &new) })),
        )
        .await
        .map_err(|e| Error(e.into()))?;
    }
    Ok(Action::requeue(requeue))
}

/// What happened between two statuses, as events: (warning, reason, note).
pub fn changes(
    db: &Database,
    before: &DatabaseStatus,
    st: &DatabaseStatus,
) -> Vec<(bool, &'static str, String)> {
    let mut ev = Vec::new();
    if st.failovers > before.failovers && !before.primary.is_empty() {
        ev.push((
            true,
            "Failover",
            format!(
                "The primary moved from {} to {}",
                before.primary, st.primary
            ),
        ));
    }
    if before.instances > 0 && st.instances != before.instances {
        ev.push((
            false,
            "Scaled",
            format!(
                "Instances went from {} to {}",
                before.instances, st.instances
            ),
        ));
    }
    if db.spec.restore.is_some() {
        let src = db
            .spec
            .restore
            .as_ref()
            .map(|r| r.from_backup.source_name.clone())
            .unwrap_or_default();
        if before.phase.is_empty() {
            ev.push((
                false,
                "RestoreStarted",
                format!("Restoring from {src}'s backups"),
            ));
        }
        if before.primary.is_empty() && !st.primary.is_empty() {
            ev.push((
                false,
                "RestoreFinished",
                format!("Restored from {src}'s backups"),
            ));
        }
    }
    ev
}

/// Records an event on the Database; a failure is only logged.
pub async fn event(ctx: &Ctx, db: &Database, warning: bool, reason: &str, note: String) {
    let ev = Event {
        type_: if warning {
            EventType::Warning
        } else {
            EventType::Normal
        },
        reason: reason.into(),
        note: Some(note),
        action: reason.into(),
        secondary: None,
    };
    if let Err(e) = ctx.recorder.publish(&ev, &db.object_ref(&())).await {
        tracing::warn!(db = %db.name_any(), reason, error = %e, "event");
    }
}

pub fn error_policy(_db: Arc<Database>, e: &Error, _ctx: Arc<Ctx>) -> Action {
    tracing::warn!(error = %e, "reconcile failed");
    Action::requeue(Duration::from_secs(15))
}

async fn run(
    db: &Database,
    ctx: &Ctx,
    mut st: DatabaseStatus,
) -> anyhow::Result<(DatabaseStatus, Duration)> {
    validate(db)?;
    let secret = credentials(db, ctx)
        .await
        .map_err(|e| match e.downcast_ref::<kube::Error>() {
            // The agent grants the controller only the namespaces Pyo manages.
            Some(kube::Error::Api(s)) if s.code == 403 => {
                anyhow!("This namespace isn't managed by Pyo")
            }
            _ => e.context("credentials"),
        })?;
    st.credentials_secret = secret.name_any();
    let k = Kube { db, ctx };
    match db.spec.engine {
        Engine::Postgres => k.postgres(st).await,
        Engine::Valkey => k.valkey(st, &secret).await,
        Engine::Mysql => k.mysql(st, &secret).await,
    }
}

fn validate(db: &Database) -> anyhow::Result<()> {
    let s = &db.spec;
    if s.engine == Engine::Mysql {
        anyhow::ensure!(
            crd::MYSQL_INSTANCES.contains(&s.instances),
            "MySQL runs 1, 3 or 5 instances"
        );
        anyhow::ensure!(s.read_instances() == 0, "MySQL has no read replicas");
        anyhow::ensure!(
            !s.postgres.as_ref().is_some_and(|p| p.pooler),
            "MySQL has no pooler"
        );
        anyhow::ensure!(
            s.version.is_empty() || s.version == "8.4",
            "the MySQL version is 8.4"
        );
        let memory = crate::mysql::bytes(&s.memory);
        anyhow::ensure!(
            memory.is_some_and(|m| m >= crd::MYSQL_MIN_MEMORY),
            "MySQL needs at least 1Gi of memory"
        );
        anyhow::ensure!(
            crate::mysql::bytes(&s.storage).is_some(),
            "storage is a size like 10Gi"
        );
    } else {
        let max = if s.engine == Engine::Valkey {
            crd::MAX_VALKEY_INSTANCES
        } else {
            crd::MAX_INSTANCES
        };
        anyhow::ensure!(
            (1..=max).contains(&s.instances),
            "instances must be 1 to {max}"
        );
        let max_read = if s.engine == Engine::Valkey {
            crd::MAX_VALKEY_READ_REPLICAS
        } else {
            crd::MAX_READ_REPLICAS
        };
        anyhow::ensure!(
            (0..=max_read).contains(&s.read_instances()),
            "read replicas must be 1 to {max_read}"
        );
    }
    anyhow::ensure!(!s.storage.is_empty(), "storage is required");
    anyhow::ensure!(
        !s.rest || s.engine == Engine::Valkey,
        "the REST API is for Valkey only"
    );
    anyhow::ensure!(
        s.backup.is_none() || s.engine != Engine::Valkey,
        "backups are for Postgres and MySQL"
    );
    anyhow::ensure!(
        s.restore.is_none() || s.engine != Engine::Valkey,
        "restoring is for Postgres and MySQL"
    );
    if let Some(i) = &s.import {
        anyhow::ensure!(
            s.engine != Engine::Valkey,
            "importing is for Postgres and MySQL"
        );
        anyhow::ensure!(
            s.restore.is_none(),
            "a database is either restored from backups or imported into"
        );
        anyhow::ensure!(
            !i.id.is_empty() && !i.secret.is_empty(),
            "an import needs an id and a secret"
        );
        match i.source.as_str() {
            "url" => anyhow::ensure!(
                i.connection.as_ref().is_some_and(|c| !c.host.is_empty()
                    && !c.database.is_empty()
                    && (1..=65535).contains(&c.port)),
                "an import from a URL needs its host, port and database"
            ),
            "file" => anyhow::ensure!(
                match s.engine {
                    Engine::Postgres => ["sql", "sql.gz", "dump"].contains(&i.format.as_str()),
                    _ => ["sql", "sql.gz"].contains(&i.format.as_str()),
                },
                "a file to import is sql or sql.gz (or a Postgres custom-format dump)"
            ),
            _ => anyhow::bail!("an import's source is url or file"),
        }
        anyhow::ensure!(
            !i.bootstrap || (s.engine == Engine::Postgres && i.source == "url"),
            "only a Postgres import from a URL is done as the database is made"
        );
    }
    let dest_ok = |d: &crd::Destination| {
        !d.endpoint_url.is_empty()
            && !d.bucket.is_empty()
            && !d.credentials_secret.is_empty()
            && d.region
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-')
    };
    if let Some(b) = s.backup.as_ref().filter(|b| b.enabled) {
        anyhow::ensure!(
            dest_ok(&b.destination),
            "backups need an endpoint, a bucket and a credentials secret"
        );
        anyhow::ensure!(
            b.retention_days == 0 || (1..=90).contains(&b.retention_days),
            "backups are kept 1 to 90 days"
        );
    }
    if let Some(r) = &s.restore {
        let src = &r.from_backup;
        anyhow::ensure!(
            dest_ok(&src.destination) && !src.source_name.is_empty(),
            "restoring needs an endpoint, a bucket, a credentials secret and the source database's name"
        );
        // Its own backups would land on the ones it's made from.
        let ns = db.namespace().unwrap_or_default();
        let src_ns = if src.source_namespace.is_empty() {
            ns.clone()
        } else {
            src.source_namespace.clone()
        };
        if let Some(b) = s.backup.as_ref().filter(|b| b.enabled) {
            // MySQL's backups are at moco/<namespace>/<name>/ in the bucket,
            // whatever the path.
            let place = |d: &crd::Destination, ns: &str| match s.engine {
                Engine::Mysql => format!("s3://{}/moco/{ns}", d.bucket),
                _ => crate::postgres::destination_path(d, ns),
            };
            let same = place(&b.destination, &ns) == place(&src.destination, &src_ns)
                && b.destination.endpoint_url == src.destination.endpoint_url;
            anyhow::ensure!(
                !(same && src.source_name == db.name_any()),
                "a database can't back up to the backups it's restored from"
            );
        }
    }
    Ok(())
}

pub fn secret_name(db: &Database) -> String {
    if db.spec.credentials_secret.is_empty() {
        format!("{}-credentials", db.name_any())
    } else {
        db.spec.credentials_secret.clone()
    }
}

pub fn token(bytes: usize) -> String {
    let mut b = vec![0u8; bytes];
    rand::rng().fill_bytes(&mut b);
    URL_SAFE_NO_PAD.encode(b)
}

/// The login secret, made when it doesn't exist.
async fn credentials(db: &Database, ctx: &Ctx) -> anyhow::Result<Secret> {
    let api: Api<Secret> = Api::namespaced(ctx.client.clone(), &db.namespace().unwrap_or_default());
    let name = secret_name(db);
    if let Some(s) = api.get_opt(&name).await? {
        return Ok(s);
    }
    let b = |s: String| ByteString(s.into_bytes());
    let mut data = std::collections::BTreeMap::from([("password".to_owned(), b(token(24)))]);
    let type_ = match db.spec.engine {
        // CloudNativePG reads the owner's login from a basic-auth secret.
        Engine::Postgres => {
            data.insert("username".into(), b(crate::postgres::owner(db)));
            "kubernetes.io/basic-auth"
        }
        Engine::Mysql => {
            data.insert("username".into(), b(crate::mysql::APP.into()));
            "kubernetes.io/basic-auth"
        }
        Engine::Valkey => {
            data.insert("username".into(), b("default".into()));
            data.insert("token".into(), b(token(32)));
            data.insert("readonly-token".into(), b(token(32)));
            "Opaque"
        }
    };
    let s = Secret {
        metadata: ObjectMeta {
            name: Some(name),
            labels: Some(
                labels(db, "credentials")
                    .as_object()
                    .unwrap()
                    .iter()
                    .map(|(k, v)| (k.clone(), v.as_str().unwrap().to_owned()))
                    .collect(),
            ),
            owner_references: db.controller_owner_ref(&()).map(|o| vec![o]),
            ..Default::default()
        },
        type_: Some(type_.into()),
        data: Some(data),
        ..Default::default()
    };
    Ok(api.create(&PostParams::default(), &s).await?)
}

pub fn labels(db: &Database, component: &str) -> Value {
    json!({ LABEL_DATABASE: db.name_any(), LABEL_COMPONENT: component, LABEL_MANAGED_BY: OWNER })
}

pub fn selector(db: &Database, component: &str) -> Value {
    json!({ LABEL_DATABASE: db.name_any(), LABEL_COMPONENT: component })
}

/// One Database's children.
pub struct Kube<'a> {
    pub db: &'a Database,
    pub ctx: &'a Ctx,
}

impl Kube<'_> {
    pub fn name(&self) -> String {
        self.db.name_any()
    }

    pub fn ns(&self) -> String {
        self.db.namespace().unwrap_or_default()
    }

    /// The pods matching a label selector in the Database's namespace.
    pub async fn pods(&self, selector: &str) -> anyhow::Result<Vec<Pod>> {
        let api: Api<Pod> = Api::namespaced(self.ctx.client.clone(), &self.ns());
        Ok(api
            .list(&ListParams::default().labels(selector))
            .await?
            .items)
    }

    /// The start of a child object: name, namespace, labels, owner.
    pub fn object(&self, api_version: &str, kind: &str, name: &str, component: &str) -> Value {
        json!({
            "apiVersion": api_version,
            "kind": kind,
            "metadata": {
                "name": name,
                "namespace": self.ns(),
                "labels": labels(self.db, component),
                "ownerReferences": [self.db.controller_owner_ref(&())],
            },
        })
    }

    fn dynamic(&self, api_version: &str, kind: &str) -> Api<DynamicObject> {
        let (group, version) = api_version.split_once('/').unwrap_or(("", api_version));
        let ar = ApiResource::from_gvk(&GroupVersionKind::gvk(group, version, kind));
        Api::namespaced_with(self.ctx.client.clone(), &self.ns(), &ar)
    }

    /// Server-side applies obj, owning every field it sets.
    pub async fn apply(&self, obj: &Value) -> anyhow::Result<()> {
        let (av, kind, name) = (
            obj["apiVersion"].as_str().unwrap(),
            obj["kind"].as_str().unwrap(),
            obj["metadata"]["name"].as_str().unwrap(),
        );
        self.dynamic(av, kind)
            .patch(name, &PatchParams::apply(OWNER).force(), &Patch::Apply(obj))
            .await
            .map_err(|e| {
                if not_found(&e) {
                    anyhow!("{kind} isn't known on this cluster ({av})")
                } else {
                    anyhow!(e).context(format!("apply {kind} {name}"))
                }
            })?;
        Ok(())
    }

    pub async fn get(
        &self,
        api_version: &str,
        kind: &str,
        name: &str,
    ) -> anyhow::Result<Option<DynamicObject>> {
        Ok(self.dynamic(api_version, kind).get_opt(name).await?)
    }

    /// Every object of a kind in the Database's namespace.
    pub async fn list(&self, api_version: &str, kind: &str) -> anyhow::Result<Vec<DynamicObject>> {
        Ok(self
            .dynamic(api_version, kind)
            .list(&ListParams::default())
            .await?
            .items)
    }

    /// Deletes a child that's no longer wanted, if it's there.
    pub async fn remove(&self, api_version: &str, kind: &str, name: &str) -> anyhow::Result<()> {
        match self
            .dynamic(api_version, kind)
            .delete(name, &DeleteParams::background())
            .await
        {
            Ok(_) => Ok(()),
            Err(e) if not_found(&e) => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    pub async fn event(&self, warning: bool, reason: &str, note: String) {
        event(self.ctx, self.db, warning, reason, note).await;
    }

    /// The event for read replicas going from `from` to `to` instances.
    pub async fn read_replicas_event(&self, from: i32, to: i32) {
        let (reason, note) = match (from, to) {
            (a, b) if a == b => return,
            (0, n) => (
                "ReadReplicasAdded",
                format!("Added {n} read replica{}", if n == 1 { "" } else { "s" }),
            ),
            (_, 0) => (
                "ReadReplicasRemoved",
                "Removed the read replicas".to_owned(),
            ),
            (a, b) => (
                "ReadReplicasScaled",
                format!("Read replicas went from {a} to {b}"),
            ),
        };
        self.event(false, reason, note).await;
    }

    pub fn svc_host(&self, name: &str) -> String {
        format!("{name}.{}.svc.{}", self.ns(), self.ctx.domain)
    }
}

pub fn not_found(e: &kube::Error) -> bool {
    matches!(e, kube::Error::Api(s) if s.code == 404)
}

pub fn phase_for(st: &mut DatabaseStatus, ready: i32, want: i32, serving: bool) {
    (st.phase, st.message) = if ready >= want && serving {
        (crd::READY.into(), String::new())
    } else if serving {
        (
            crd::DEGRADED.into(),
            format!("{ready} of {want} instances ready"),
        )
    } else {
        (
            crd::CREATING.into(),
            format!("{ready} of {want} instances ready"),
        )
    };
}

/// CPU and memory as container resources (memory is also the limit).
pub fn resources(cpu: &str, memory: &str) -> Option<Value> {
    if cpu.is_empty() && memory.is_empty() {
        return None;
    }
    let mut req = serde_json::Map::new();
    let mut r = json!({});
    if !cpu.is_empty() {
        req.insert("cpu".into(), json!(cpu));
    }
    if !memory.is_empty() {
        req.insert("memory".into(), json!(memory));
        r["limits"] = json!({ "memory": memory });
    }
    r["requests"] = Value::Object(req);
    Some(r)
}

/// The JSON merge patch (RFC 7386) that turns `old` into `new`: changed
/// values, and null for keys `new` no longer has.
pub fn merge_patch(old: &Value, new: &Value) -> Value {
    match (old, new) {
        (Value::Object(o), Value::Object(n)) => {
            let mut p = serde_json::Map::new();
            for (k, v) in n {
                match o.get(k) {
                    Some(ov) if ov == v => {}
                    Some(ov) => {
                        p.insert(k.clone(), merge_patch(ov, v));
                    }
                    None => {
                        p.insert(k.clone(), v.clone());
                    }
                }
            }
            for k in o.keys().filter(|k| !n.contains_key(*k)) {
                p.insert(k.clone(), Value::Null);
            }
            Value::Object(p)
        }
        _ => new.clone(),
    }
}

/// The instances for the status, the primary first, then by name. A pod's
/// role comes from `role_label` when it says "primary" or "replica",
/// otherwise from whether it's `primary`.
pub fn members(pods: &[Pod], primary: &str, role_label: Option<&str>) -> Vec<crd::Member> {
    let mut m: Vec<crd::Member> = pods
        .iter()
        .filter(|p| p.metadata.deletion_timestamp.is_none())
        .map(|p| {
            let name = p.metadata.name.clone().unwrap_or_default();
            let labelled = role_label
                .and_then(|l| p.metadata.labels.as_ref()?.get(l))
                .map(String::as_str)
                .filter(|r| *r == "primary" || *r == "replica");
            let role = labelled
                .unwrap_or(if !primary.is_empty() && name == primary {
                    "primary"
                } else {
                    "replica"
                })
                .to_owned();
            let status = p.status.as_ref();
            let ready = status
                .and_then(|s| s.conditions.as_ref())
                .is_some_and(|c| c.iter().any(|c| c.type_ == "Ready" && c.status == "True"));
            let node = p
                .spec
                .as_ref()
                .and_then(|s| s.node_name.clone())
                .unwrap_or_default();
            crd::Member {
                name,
                role,
                ready,
                node,
                lag_seconds: None,
            }
        })
        .collect();
    m.sort_by(|a, b| (a.role != "primary", &a.name).cmp(&(b.role != "primary", &b.name)));
    m
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db(engine: &str, instances: i32) -> Database {
        serde_json::from_value(serde_json::json!({
            "apiVersion": "pyo.dev/v1alpha1", "kind": "Database", "metadata": { "name": "main" },
            "spec": { "engine": engine, "instances": instances, "storage": "5Gi" },
        }))
        .unwrap()
    }

    #[test]
    fn instances_per_engine() {
        // Postgres: a primary and up to 8 replicas; a cache: up to 4.
        for n in 1..=9 {
            assert!(validate(&db("postgres", n)).is_ok(), "{n}");
        }
        assert!(validate(&db("postgres", 0)).is_err() && validate(&db("postgres", 10)).is_err());
        for n in 1..=5 {
            assert!(validate(&db("valkey", n)).is_ok(), "{n}");
        }
        assert!(validate(&db("valkey", 6)).is_err());
    }

    #[test]
    fn mysql_rules() {
        let my = |spec: serde_json::Value| {
            let mut v = serde_json::json!({ "engine": "mysql", "instances": 1, "storage": "5Gi", "memory": "1Gi" });
            v.as_object_mut()
                .unwrap()
                .extend(spec.as_object().unwrap().clone());
            validate(&serde_json::from_value(serde_json::json!({ "apiVersion": "pyo.dev/v1alpha1", "kind": "Database", "metadata": { "name": "main", "namespace": "ns" }, "spec": v })).unwrap())
        };
        // MOCO: a primary alone, or with 2 or 4 replicas.
        for n in [1, 3, 5] {
            assert!(my(serde_json::json!({ "instances": n })).is_ok(), "{n}");
        }
        for n in [0, 2, 4, 6] {
            assert!(my(serde_json::json!({ "instances": n })).is_err(), "{n}");
        }
        assert!(my(serde_json::json!({ "memory": "1536Mi", "version": "8.4" })).is_ok());
        for bad in [
            serde_json::json!({ "memory": "512Mi" }),
            serde_json::json!({ "memory": "" }),
            serde_json::json!({ "version": "8.0" }),
            serde_json::json!({ "readReplicas": { "instances": 1 } }),
            serde_json::json!({ "postgres": { "pooler": true } }),
            serde_json::json!({ "rest": true }),
        ] {
            assert!(my(bad.clone()).is_err(), "{bad}");
        }
        assert!(my(serde_json::json!({ "readReplicas": { "instances": 0 } })).is_ok());
        let d = serde_json::json!({ "endpointURL": "http://s3", "bucket": "b", "credentialsSecret": "s3" });
        assert!(my(serde_json::json!({ "backup": { "enabled": true, "destination": d } })).is_ok());
        let mut from = d.clone();
        from["sourceName"] = "old".into();
        assert!(my(serde_json::json!({ "restore": { "fromBackup": from, "targetTime": "2026-11-09T10:00:00Z" } })).is_ok());
        // Its backups are at moco/<ns>/<name>/ whatever the path: restoring
        // from itself into the same bucket would mix them.
        from["sourceName"] = "main".into();
        let mut other_path = d.clone();
        other_path["path"] = "x".into();
        assert!(my(serde_json::json!({ "restore": { "fromBackup": from }, "backup": { "enabled": true, "destination": other_path } })).is_err());
    }

    #[test]
    fn read_replicas_per_engine() {
        let with = |engine: &str, n: i32| {
            let mut d = db(engine, 1);
            d.spec.read_replicas = Some(crd::ReadReplicasSpec { instances: n });
            validate(&d)
        };
        for n in 0..=5 {
            assert!(with("postgres", n).is_ok(), "{n}");
        }
        for n in 0..=4 {
            assert!(with("valkey", n).is_ok(), "{n}");
        }
        assert!(
            with("postgres", 6).is_err()
                && with("valkey", 5).is_err()
                && with("postgres", -1).is_err()
        );
        assert_eq!(db("valkey", 1).spec.read_instances(), 0);
    }

    fn pod(name: &str, role: Option<&str>, ready: bool, node: Option<&str>) -> Pod {
        let mut labels = serde_json::Map::new();
        if let Some(r) = role {
            labels.insert("cnpg.io/instanceRole".into(), r.into());
        }
        serde_json::from_value(serde_json::json!({
            "metadata": { "name": name, "labels": labels },
            "spec": { "containers": [], "nodeName": node },
            "status": { "conditions": [{ "type": "Ready", "status": if ready { "True" } else { "False" } }] },
        }))
        .unwrap()
    }

    fn m(name: &str, role: &str, ready: bool, node: &str) -> crd::Member {
        crd::Member {
            name: name.into(),
            role: role.into(),
            ready,
            node: node.into(),
            lag_seconds: None,
        }
    }

    #[test]
    fn members_from_role_label() {
        // CloudNativePG labels the roles; the label wins over a stale primary.
        let pods = [
            pod("pg-3", Some("replica"), true, Some("b")),
            pod("pg-1", Some("replica"), false, None),
            pod("pg-2", Some("primary"), true, Some("a")),
        ];
        assert_eq!(
            members(&pods, "pg-1", Some("cnpg.io/instanceRole")),
            [
                m("pg-2", "primary", true, "a"),
                m("pg-1", "replica", false, ""),
                m("pg-3", "replica", true, "b")
            ]
        );
    }

    #[test]
    fn members_from_primary_name() {
        // No label (Valkey, or a pod CloudNativePG hasn't labelled yet).
        let pods = [
            pod("kv-0", None, true, Some("a")),
            pod("kv-2", None, true, Some("c")),
            pod("kv-1", None, true, Some("b")),
        ];
        assert_eq!(
            members(&pods, "kv-1", None),
            [
                m("kv-1", "primary", true, "b"),
                m("kv-0", "replica", true, "a"),
                m("kv-2", "replica", true, "c")
            ]
        );
        assert_eq!(
            members(&pods, "kv-1", Some("cnpg.io/instanceRole"))[0].name,
            "kv-1"
        );
        // No primary known: all replicas, by name.
        assert!(members(&pods, "", None).iter().all(|x| x.role == "replica"));
        assert_eq!(members(&[], "kv-0", None), []);
    }

    fn pg(spec: serde_json::Value) -> Database {
        let mut v = serde_json::json!({ "engine": "postgres", "instances": 1, "storage": "5Gi" });
        v.as_object_mut()
            .unwrap()
            .extend(spec.as_object().unwrap().clone());
        serde_json::from_value(serde_json::json!({ "apiVersion": "pyo.dev/v1alpha1", "kind": "Database", "metadata": { "name": "main", "namespace": "ns" }, "spec": v })).unwrap()
    }

    #[test]
    fn backup_and_restore_rules() {
        let d = serde_json::json!({ "endpointURL": "http://s3", "bucket": "b", "credentialsSecret": "s3" });
        assert!(
            validate(&pg(
                serde_json::json!({ "backup": { "enabled": true, "destination": d } })
            ))
            .is_ok()
        );
        assert!(validate(&pg(serde_json::json!({ "backup": { "enabled": true, "destination": d, "retentionDays": 91 } }))).is_err());
        // Off: the destination isn't checked.
        assert!(validate(&pg(serde_json::json!({ "backup": { "enabled": false, "destination": { "endpointURL": "", "bucket": "", "credentialsSecret": "" } } }))).is_ok());
        let mut from = d.clone();
        from["sourceName"] = "old".into();
        assert!(
            validate(&pg(
                serde_json::json!({ "restore": { "fromBackup": from } })
            ))
            .is_ok()
        );
        // Restoring from itself into the same place would mix the backups.
        from["sourceName"] = "main".into();
        assert!(validate(&pg(serde_json::json!({ "restore": { "fromBackup": from }, "backup": { "enabled": true, "destination": d } }))).is_err());
        assert!(
            validate(&pg(
                serde_json::json!({ "restore": { "fromBackup": from } })
            ))
            .is_ok()
        );
        let mut kv = pg(serde_json::json!({ "backup": { "enabled": true, "destination": d } }));
        kv.spec.engine = Engine::Valkey;
        assert!(validate(&kv).is_err());
    }

    #[test]
    fn events_from_status() {
        let d = db("postgres", 3);
        let before = DatabaseStatus {
            phase: "Ready".into(),
            primary: "pg-1".into(),
            instances: 3,
            failovers: 1,
            ..Default::default()
        };
        assert!(changes(&d, &before, &before).is_empty());
        let mut after = before.clone();
        after.primary = "pg-2".into();
        after.failovers = 2;
        after.instances = 5;
        let ev = changes(&d, &before, &after);
        assert_eq!(
            ev[0],
            (
                true,
                "Failover",
                "The primary moved from pg-1 to pg-2".into()
            )
        );
        assert_eq!(
            ev[1],
            (false, "Scaled", "Instances went from 3 to 5".into())
        );
        // A new database isn't "scaled" from 0.
        assert!(changes(&d, &DatabaseStatus::default(), &before).is_empty());
        let mut r = pg(
            serde_json::json!({ "restore": { "fromBackup": { "endpointURL": "http://s3", "bucket": "b", "credentialsSecret": "s3", "sourceName": "old" } } }),
        );
        r.spec.instances = 3;
        let start = changes(
            &r,
            &DatabaseStatus::default(),
            &DatabaseStatus {
                phase: "Creating".into(),
                instances: 3,
                ..Default::default()
            },
        );
        assert_eq!(
            start.iter().map(|e| e.1).collect::<Vec<_>>(),
            ["RestoreStarted"]
        );
        let creating = DatabaseStatus {
            phase: "Creating".into(),
            instances: 3,
            ..Default::default()
        };
        assert_eq!(
            changes(&r, &creating, &before)
                .iter()
                .map(|e| e.1)
                .collect::<Vec<_>>(),
            ["RestoreFinished"]
        );
    }

    /// Applies a merge patch the way the API server does (RFC 7386).
    fn apply_merge(target: &Value, patch: &Value) -> Value {
        let Value::Object(p) = patch else {
            return patch.clone();
        };
        let mut t = target.as_object().cloned().unwrap_or_default();
        for (k, v) in p {
            if v.is_null() {
                t.remove(k);
            } else {
                let cur = t.get(k).cloned().unwrap_or(Value::Null);
                t.insert(k.clone(), apply_merge(&cur, v));
            }
        }
        Value::Object(t)
    }

    /// What the API server stores: nulls dropped, as after a merge patch.
    fn stored(st: &DatabaseStatus) -> Value {
        apply_merge(&Value::Null, &serde_json::to_value(st).unwrap())
    }

    #[test]
    fn status_patch_clears_what_went_away() {
        let mut before = DatabaseStatus {
            message: "1 of 2 instances ready".into(),
            ..Default::default()
        };
        before.endpoints.primary = "main-rw:5432".into();
        before.endpoints.pooler = "main-pooler:5432".into();
        before.backup = Some(crd::BackupStatus {
            message: "WAL archiving fails".into(),
            ..Default::default()
        });
        before.members = vec![
            crd::Member {
                name: "main-1".into(),
                ..Default::default()
            },
            crd::Member {
                name: "main-2".into(),
                ..Default::default()
            },
        ];
        // Pooler and backups turned off, an instance gone, all well.
        let mut after = before.clone();
        after.message.clear();
        after.endpoints.pooler.clear();
        after.backup = None;
        after.members.pop();
        let old = stored(&before);
        let patch = merge_patch(&old, &serde_json::to_value(&after).unwrap());
        assert_eq!(patch["endpoints"], json!({ "pooler": null }));
        assert_eq!(apply_merge(&old, &patch), stored(&after));
        let got = apply_merge(&old, &patch);
        assert!(got.get("backup").is_none() && got["endpoints"].get("pooler").is_none());
        assert_eq!(got["members"].as_array().unwrap().len(), 1);
        // Read replicas turned off: the whole block goes.
        let mut on = after.clone();
        on.endpoints.read = "main-read-pooler:5432".into();
        on.read_replicas = Some(crd::ReadReplicasStatus {
            ready: 1,
            members: vec![crd::ReadMember {
                name: "main-read-1".into(),
                node: "a".into(),
                ready: true,
                lag_seconds: Some(0.5),
            }],
        });
        let old = stored(&on);
        let patch = merge_patch(&old, &serde_json::to_value(&after).unwrap());
        assert_eq!(patch["readReplicas"], Value::Null);
        assert_eq!(patch["endpoints"], json!({ "read": null }));
        let got = apply_merge(&old, &patch);
        assert!(got.get("readReplicas").is_none() && got["endpoints"].get("read").is_none());
        let old = stored(&after);
        // Nothing changed: an empty patch.
        assert_eq!(merge_patch(&old, &old), json!({}));
    }
}
