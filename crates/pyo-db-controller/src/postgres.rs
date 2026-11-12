//! Postgres through CloudNativePG: a Cluster with instances spread over
//! different servers, the owner's login from the credentials secret, and
//! CloudNativePG's own failover; optionally a PgBouncer Pooler.
//!
//! Read replicas are a second Cluster, `<name>-read`, in replica mode: it
//! streams from the main cluster's `-rw` service (so it follows whichever
//! instance is primary), every instance in it is read-only, and CloudNativePG
//! never promotes it. A Pooler of type `r` in front of it reaches all of its
//! instances and nothing else.

use std::time::Duration;

use serde_json::{Value, json};

use k8s_openapi::{ByteString, api::core::v1::Secret};
use kube::{Api, api::PostParams};

use crate::{
    controller::{Kube, LABEL_COMPONENT, LABEL_DATABASE, phase_for, resources, secret_name},
    crd::{self, BackupStatus, Database, DatabaseStatus, Destination},
};

const CNPG: &str = "postgresql.cnpg.io/v1";

pub fn owner(db: &Database) -> String {
    db.spec
        .postgres
        .as_ref()
        .map(|p| p.owner.clone())
        .filter(|o| !o.is_empty())
        .unwrap_or_else(|| "app".into())
}

pub fn database(db: &Database) -> String {
    db.spec
        .postgres
        .as_ref()
        .map(|p| p.database.clone())
        .filter(|o| !o.is_empty())
        .unwrap_or_else(|| "app".into())
}

/// The read replicas' Cluster and Pooler.
pub fn read_cluster(name: &str) -> String {
    format!("{name}-read")
}

pub fn read_pooler(name: &str) -> String {
    format!("{name}-read-pooler")
}

/// The ConfigMap with the read replicas' lag query.
pub fn read_monitoring(name: &str) -> String {
    format!("{name}-read-monitoring")
}

/// The lag metric (`cnpg_pyo_read_replica_lag`). CloudNativePG's own
/// cnpg_pg_replication_lag counts a standby as behind whenever what it
/// received differs from what it replayed, and a standby that has just
/// (re)connected reports less received than replayed until new WAL comes:
/// on an idle database it would look minutes behind while caught up.
const LAG_QUERY: &str = r#"pyo_read_replica:
  query: |
    SELECT CASE
      WHEN NOT pg_is_in_recovery() THEN 0
      WHEN pg_last_wal_receive_lsn() IS NULL OR pg_last_wal_receive_lsn() <= pg_last_wal_replay_lsn() THEN 0
      ELSE GREATEST(0, EXTRACT(EPOCH FROM (now() - pg_last_xact_replay_timestamp())))
    END AS lag
  metrics:
    - lag:
        usage: GAUGE
        description: Seconds behind the primary (0 when it has replayed all it received)
"#;

/// The read replicas' component label.
pub const READ: &str = "read";
/// The main cluster's instances' component label.
pub const INSTANCE: &str = "postgres";

/// The ConfigMap with the main cluster's lag query.
pub fn monitoring(name: &str) -> String {
    format!("{name}-monitoring")
}

/// The replicas' lag metric (`cnpg_pyo_replica_lag`), as LAG_QUERY.
const REPLICA_LAG_QUERY: &str = r#"pyo_replica:
  query: |
    SELECT CASE
      WHEN NOT pg_is_in_recovery() THEN 0
      WHEN pg_last_wal_receive_lsn() IS NULL OR pg_last_wal_receive_lsn() <= pg_last_wal_replay_lsn() THEN 0
      ELSE GREATEST(0, EXTRACT(EPOCH FROM (now() - pg_last_xact_replay_timestamp())))
    END AS lag
  metrics:
    - lag:
        usage: GAUGE
        description: Seconds behind the primary (0 on the primary, and when it has replayed all it received)
"#;

/// The external cluster the read replicas stream from.
const SOURCE: &str = "source";

fn pooler_on(db: &Database) -> bool {
    db.spec.postgres.as_ref().is_some_and(|p| p.pooler)
}

/// The backup settings when backups are on.
pub fn backup_on(db: &Database) -> Option<&crd::BackupSpec> {
    db.spec.backup.as_ref().filter(|b| b.enabled)
}

/// Where a database's backups go: barman puts them under
/// `<destinationPath>/<serverName>/`, so `<path>/<namespace>/<name>/`.
pub fn destination_path(d: &Destination, namespace: &str) -> String {
    let path = d.path.trim_matches('/');
    if path.is_empty() {
        format!("s3://{}/{namespace}", d.bucket)
    } else {
        format!("s3://{}/{path}/{namespace}", d.bucket)
    }
}

/// The Secret holding a region (barman reads it only from a Secret). The
/// region is in the name, so a changed region is a new Secret and nothing
/// is ever updated.
pub fn region_secret(db_name: &str, region: &str) -> String {
    format!("{db_name}-backup-region-{}", region.to_lowercase())
}

/// barman-cloud settings for one database's backups.
pub fn object_store(
    d: &Destination,
    namespace: &str,
    server: &str,
    region_secret: Option<&str>,
) -> Value {
    let key = |k: &str| json!({ "name": d.credentials_secret, "key": k });
    let mut creds =
        json!({ "accessKeyId": key("accessKeyId"), "secretAccessKey": key("secretAccessKey") });
    if let Some(r) = region_secret {
        creds["region"] = json!({ "name": r, "key": "region" });
    }
    json!({
        "destinationPath": destination_path(d, namespace),
        "endpointURL": d.endpoint_url,
        "serverName": server,
        "s3Credentials": creds,
        "wal": { "compression": "gzip", "maxParallel": 2 },
        "data": { "compression": "gzip", "jobs": 2 },
    })
}

/// The backup part of the status, from CloudNativePG's Cluster status and
/// the time the primary last archived WAL.
pub fn backup_status(cnpg: &Value, last_archived: Option<String>) -> BackupStatus {
    let s = |k: &str| cnpg[k].as_str().unwrap_or("").to_owned();
    let cond = |t: &str| {
        cnpg["conditions"]
            .as_array()
            .and_then(|c| c.iter().find(|c| c["type"] == t))
            .cloned()
            .unwrap_or_default()
    };
    let mut message = String::new();
    for t in ["ContinuousArchiving", "LastBackupSucceeded"] {
        let c = cond(t);
        if c["status"] == "False" {
            let what = if t == "ContinuousArchiving" {
                "WAL archiving fails"
            } else {
                "the last base backup failed"
            };
            let why = explain(c["message"].as_str().unwrap_or("").trim());
            message = if why.is_empty() {
                what.to_owned()
            } else {
                format!("{what}: {why}")
            };
            break;
        }
    }
    BackupStatus {
        first_recoverability_point: s("firstRecoverabilityPoint"),
        last_successful_backup: s("lastSuccessfulBackup"),
        last_archived_wal: last_archived.unwrap_or_default(),
        last_failed_backup: s("lastFailedBackup"),
        message,
    }
}

/// The events a Backup's status calls for, (key, warning, reason, note),
/// leaving out what happened before `since` (RFC3339, like the times in it).
pub fn backup_changes(
    status: &Value,
    since: &str,
) -> Vec<(&'static str, bool, &'static str, String)> {
    let s = |k: &str| status[k].as_str().unwrap_or("");
    let mut ev = Vec::new();
    let (started, stopped) = (s("startedAt"), s("stoppedAt"));
    if !started.is_empty() && started >= since {
        ev.push((
            "started",
            false,
            "BackupStarted",
            "A base backup started".to_owned(),
        ));
    }
    if !stopped.is_empty() && stopped >= since {
        match s("phase") {
            "completed" => ev.push((
                "finished",
                false,
                "BackupFinished",
                "A base backup finished".to_owned(),
            )),
            "failed" => {
                let why = explain(s("error").trim());
                ev.push((
                    "finished",
                    true,
                    "BackupFailed",
                    if why.is_empty() {
                        "A base backup failed".to_owned()
                    } else {
                        format!("A base backup failed: {why}")
                    },
                ));
            }
            _ => {}
        }
    }
    ev
}

/// barman-cloud's exit codes in plain words; anything else as it came.
fn explain(why: &str) -> String {
    let hint = if why.contains("exit status 2") {
        "can't reach the storage endpoint"
    } else if why.contains("exit status 1") || why.contains("exit status 4") {
        "the storage refused it. Check the bucket and the access keys"
    } else {
        return why.to_owned();
    };
    format!("{hint} ({why})")
}

/// When the primary last archived WAL, from its metrics:
/// `cnpg_pg_stat_archiver_last_archived_time`, in Unix seconds.
async fn last_archived(pod_ip: &str) -> Option<String> {
    archived_time(&metrics(pod_ip).await?)
}

/// An instance's metrics page (plain HTTP on 9187).
async fn metrics(pod_ip: &str) -> Option<String> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let fetch = async {
        let mut c = tokio::net::TcpStream::connect((pod_ip, 9187)).await.ok()?;
        c.write_all(format!("GET /metrics HTTP/1.0\r\nHost: {pod_ip}\r\n\r\n").as_bytes())
            .await
            .ok()?;
        let mut body = String::new();
        c.read_to_string(&mut body).await.ok()?;
        Some(body)
    };
    tokio::time::timeout(Duration::from_secs(3), fetch)
        .await
        .ok()?
}

/// A metric's value, from a line without labels.
fn metric(metrics: &str, name: &str) -> Option<f64> {
    metrics.lines().find_map(|l| {
        let (k, v) = l.split_once(' ')?;
        (k == name).then(|| v.trim().parse().ok()).flatten()
    })
}

/// How far a read replica is behind the primary, in seconds (to a tenth),
/// from its metrics (see LAG_QUERY). None when it isn't receiving WAL, since
/// the lag is then unknown.
pub fn replication_lag(metrics: &str) -> Option<f64> {
    if metric(metrics, "cnpg_pg_replication_is_wal_receiver_up")? < 1.0 {
        return None;
    }
    let lag = metric(metrics, "cnpg_pyo_read_replica_lag")?;
    Some((lag.max(0.0) * 10.0).round() / 10.0)
}

/// The last archival time in a metrics page, as RFC3339 (UTC).
pub fn archived_time(metrics: &str) -> Option<String> {
    let v: f64 = metrics.lines().find_map(|l| {
        l.strip_prefix("cnpg_pg_stat_archiver_last_archived_time")?
            .split_whitespace()
            .last()?
            .parse()
            .ok()
    })?;
    (v > 0.0).then(|| rfc3339(v as i64))
}

/// Unix seconds as RFC3339 UTC.
pub fn rfc3339(secs: i64) -> String {
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    // Civil from days (Howard Hinnant).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

impl Kube<'_> {
    pub(crate) fn pg_image(&self) -> String {
        let s = &self.db.spec;
        let version = if s.version.is_empty() {
            &self.ctx.default_postgres
        } else {
            &s.version
        };
        match self.ctx.images.postgres.get(version) {
            Some(image) => image.clone(),
            None => format!("{}:{version}", self.ctx.images.postgres_repo),
        }
    }

    fn pg_storage(&self) -> Value {
        let s = &self.db.spec;
        let mut storage = json!({ "size": s.storage });
        if !s.storage_class.is_empty() {
            storage["storageClass"] = json!(s.storage_class);
        }
        storage
    }

    /// The Cluster's `postgresql`: its settings and the extensions'
    /// libraries and settings (None: nothing to set).
    fn pg_config(&self, ext: &crate::extensions::Plan) -> Option<Value> {
        let mut parameters = self
            .db
            .spec
            .postgres
            .as_ref()
            .map(|p| p.parameters.clone())
            .unwrap_or_default();
        parameters.extend(ext.parameters.clone());
        let mut pg = json!({});
        if !parameters.is_empty() {
            pg["parameters"] = json!(parameters);
        }
        if !ext.preload.is_empty() {
            pg["shared_preload_libraries"] = json!(ext.preload);
        }
        (pg != json!({})).then_some(pg)
    }

    pub fn postgres_cluster(&self, ext: &crate::extensions::Plan) -> Value {
        let s = &self.db.spec;
        let storage = self.pg_storage();
        let mut spec = json!({
            "instances": s.instances,
            "imageName": self.pg_image(),
            "primaryUpdateStrategy": "unsupervised",
            // With replicas, a restart (a new image, a library to load)
            // ends with a switchover to a restarted replica rather than the
            // primary restarting in place: no time without a primary.
            "primaryUpdateMethod": if s.instances > 1 { "switchover" } else { "restart" },
            // A single instance has nothing to fail over to: a disruption
            // budget would only block draining its server.
            "enablePDB": s.instances > 1,
            "storage": storage,
            "affinity": {
                "enablePodAntiAffinity": true,
                "podAntiAffinityType": "preferred",
                "topologyKey": "kubernetes.io/hostname",
            },
            "bootstrap": { "initdb": {
                "database": database(self.db),
                "owner": owner(self.db),
                "secret": { "name": secret_name(self.db) },
            }},
            // Its pods are pyo.dev/component=postgres.
            "inheritedMetadata": { "labels": { LABEL_DATABASE: self.name(), LABEL_COMPONENT: INSTANCE } },
            "monitoring": { "customQueriesConfigMap": [{ "name": monitoring(&self.name()), "key": "queries" }] },
        });
        if let Some(r) = resources(&s.cpu, &s.memory) {
            spec["resources"] = r;
        }
        if let Some(pg) = self.pg_config(ext) {
            spec["postgresql"] = pg;
        }
        if let Some(b) = backup_on(self.db) {
            let d = &b.destination;
            let region = (!d.region.is_empty()).then(|| region_secret(&self.name(), &d.region));
            let days = if b.retention_days == 0 {
                crd::DEFAULT_RETENTION_DAYS
            } else {
                b.retention_days
            };
            spec["backup"] = json!({
                "barmanObjectStore": object_store(d, &self.ns(), &self.name(), region.as_deref()),
                "retentionPolicy": format!("{days}d"),
            });
        }
        if let Some(r) = &s.restore {
            // A new database from another's backups. CloudNativePG reads
            // bootstrap only when it creates the cluster, and the Database
            // can't change restore, so nothing existing is ever replaced.
            let src = &r.from_backup;
            let ns = if src.source_namespace.is_empty() {
                self.ns()
            } else {
                src.source_namespace.clone()
            };
            let d = &src.destination;
            let region = (!d.region.is_empty()).then(|| region_secret(&self.name(), &d.region));
            let mut recovery = json!({
                "source": "origin",
                "database": database(self.db),
                "owner": owner(self.db),
                "secret": { "name": secret_name(self.db) },
            });
            if !r.target_time.is_empty() {
                recovery["recoveryTarget"] = json!({ "targetTime": r.target_time });
            }
            spec["bootstrap"] = json!({ "recovery": recovery });
            spec["externalClusters"] = json!([{ "name": "origin", "barmanObjectStore": object_store(d, &ns, &src.source_name, region.as_deref()) }]);
        }
        if let Some(imp) = crate::import::bootstrap(self.db) {
            // Postgres copied from a URL as it's made: CloudNativePG's own
            // import (import.rs). Read only when the Cluster is made.
            let (import, external) = crate::import::cluster_import(imp);
            spec["bootstrap"]["initdb"]["import"] = import;
            spec["externalClusters"] = json!([external]);
        }
        let mut o = self.object(CNPG, "Cluster", &self.name(), "postgres");
        o["spec"] = spec;
        o
    }

    /// Makes the region Secret a destination needs, if it's missing.
    async fn region_secret(&self, d: &Destination) -> anyhow::Result<()> {
        if d.region.is_empty() {
            return Ok(());
        }
        let api: Api<Secret> = Api::namespaced(self.ctx.client.clone(), &self.ns());
        let name = region_secret(&self.name(), &d.region);
        if api.get_opt(&name).await?.is_some() {
            return Ok(());
        }
        let mut o = self.object("v1", "Secret", &name, "backup");
        o["data"] = json!({});
        let mut s: Secret = serde_json::from_value(o)?;
        s.data = Some(
            [(
                "region".to_owned(),
                ByteString(d.region.clone().into_bytes()),
            )]
            .into(),
        );
        match api.create(&PostParams::default(), &s).await {
            Err(kube::Error::Api(e)) if e.code == 409 => Ok(()),
            r => r.map(|_| ()).map_err(Into::into),
        }
    }

    /// PgBouncer in transaction mode, in front of `cluster`'s instances of
    /// `type_` ("rw": the primary, "r": all of them).
    fn pooler(
        &self,
        name: &str,
        component: &str,
        cluster: &str,
        type_: &str,
        instances: i32,
    ) -> Value {
        let mut o = self.object(CNPG, "Pooler", name, component);
        o["spec"] = json!({
            "cluster": { "name": cluster },
            "instances": if instances > 1 { 2 } else { 1 },
            "type": type_,
            // Its pods, labelled for Pyo (CloudNativePG fills in the rest).
            "template": {
                "metadata": { "labels": { LABEL_DATABASE: self.name(), LABEL_COMPONENT: component } },
                "spec": { "containers": [] },
            },
            "pgbouncer": {
                "poolMode": "transaction",
                // max_prepared_statements: drivers and ORMs that prepare
                // statements work through the pooler (PgBouncer 1.21+).
                "parameters": { "max_client_conn": "1000", "default_pool_size": "20", "max_prepared_statements": "200" },
            },
        });
        o
    }

    pub fn postgres_pooler(&self) -> Value {
        self.pooler(
            &format!("{}-pooler", self.name()),
            "pooler",
            &self.name(),
            "rw",
            self.db.spec.instances,
        )
    }

    /// The read replicas' pooler: every instance of the read cluster is a
    /// replica, so type `r` (all instances) sends reads only there.
    pub fn read_replicas_pooler(&self) -> Value {
        let name = self.name();
        self.pooler(
            &read_pooler(&name),
            "read-pooler",
            &read_cluster(&name),
            "r",
            self.db.spec.read_instances(),
        )
    }

    /// The read replicas: a CloudNativePG replica cluster copied from the
    /// main one and streaming from its primary (through `-rw`, which moves
    /// with failovers), logging in as streaming_replica with the main
    /// cluster's replication certificate. Same image, size, storage and
    /// settings as the main cluster (a standby needs settings like
    /// max_connections at least the primary's).
    /// The read replicas load the same libraries, with the same settings:
    /// queries there need them too (a hypertable reads its chunks through
    /// TimescaleDB's).
    pub fn read_replicas_cluster(&self, ext: &crate::extensions::Plan) -> Value {
        let s = &self.db.spec;
        let name = self.name();
        let n = s.read_instances();
        let mut source = json!({
            "name": SOURCE,
            "connectionParameters": {
                // In the main cluster's server certificate.
                "host": format!("{name}-rw.{}.svc", self.ns()),
                "user": "streaming_replica",
                "dbname": "postgres",
                "sslmode": "verify-full",
                // Right after a failover -rw can still lead to the old
                // primary, and connecting there hangs until TCP gives up
                // (over a minute): give up after 5s and retry, which reaches
                // the new primary. The WAL receiver ignores connect_timeout;
                // tcp_user_timeout covers the connection attempt too.
                // Nothing else: CloudNativePG's own (Go) client sends what it
                // doesn't know to the server, which refuses anything that
                // isn't a server setting (keepalives_idle, for one).
                "connect_timeout": "5",
                "tcp_user_timeout": "5000",
            },
            "sslKey": { "name": format!("{name}-replication"), "key": "tls.key" },
            "sslCert": { "name": format!("{name}-replication"), "key": "tls.crt" },
            "sslRootCert": { "name": format!("{name}-ca"), "key": "ca.crt" },
        });
        // With backups on, WAL the primary no longer keeps comes from the
        // archive, so a replica that fell far behind catches up.
        if let Some(b) = backup_on(self.db) {
            let d = &b.destination;
            let region = (!d.region.is_empty()).then(|| region_secret(&name, &d.region));
            source["barmanObjectStore"] = object_store(d, &self.ns(), &name, region.as_deref());
        }
        let mut spec = json!({
            "instances": n,
            "imageName": self.pg_image(),
            "primaryUpdateStrategy": "unsupervised",
            "enablePDB": n > 1,
            "storage": self.pg_storage(),
            "affinity": {
                "enablePodAntiAffinity": true,
                "podAntiAffinityType": "preferred",
                "topologyKey": "kubernetes.io/hostname",
            },
            "replica": { "enabled": true, "source": SOURCE },
            "monitoring": { "customQueriesConfigMap": [{ "name": read_monitoring(&name), "key": "queries" }] },
            "bootstrap": { "pg_basebackup": { "source": SOURCE } },
            "externalClusters": [source],
            // Its pods are pyo.dev/component=read.
            "inheritedMetadata": { "labels": { LABEL_DATABASE: name, LABEL_COMPONENT: READ } },
        });
        if let Some(r) = resources(&s.cpu, &s.memory) {
            spec["resources"] = r;
        }
        if let Some(pg) = self.pg_config(ext) {
            spec["postgresql"] = pg;
        }
        let mut o = self.object(CNPG, "Cluster", &read_cluster(&name), READ);
        o["spec"] = spec;
        o
    }

    /// Events for base backups starting and ending, each once, from
    /// CloudNativePG's Backups of this cluster.
    async fn backup_events(&self) {
        let backups = match self.list(CNPG, "Backup").await {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!(db = %self.name(), error = format!("{e:#}"), "backups");
                return;
            }
        };
        for b in backups
            .iter()
            .filter(|b| b.data["spec"]["cluster"]["name"] == self.name().as_str())
        {
            let uid = b.metadata.uid.clone().unwrap_or_default();
            let bname = b.metadata.name.clone().unwrap_or_default();
            for (what, warning, reason, note) in
                backup_changes(&b.data["status"], &self.ctx.started)
            {
                let first = self
                    .ctx
                    .announced
                    .lock()
                    .unwrap()
                    .insert((uid.clone(), what));
                if first {
                    self.event(warning, reason, format!("{note} ({bname})"))
                        .await;
                }
            }
        }
    }

    /// Applies or removes the read replicas and reports them.
    /// `serving`: the main cluster has a primary to copy from.
    async fn postgres_read_replicas(
        &self,
        st: &mut DatabaseStatus,
        serving: bool,
        ext: &crate::extensions::Plan,
    ) -> anyhow::Result<()> {
        let name = self.name();
        let (cluster, pooler) = (read_cluster(&name), read_pooler(&name));
        let want = self.db.spec.read_instances();
        let have = self
            .get(CNPG, "Cluster", &cluster)
            .await?
            .map(|c| c.data["spec"]["instances"].as_i64().unwrap_or(0) as i32);
        if want == 0 {
            if have.is_some() {
                self.read_replicas_event(have.unwrap_or(0), 0).await;
            }
            self.remove(CNPG, "Pooler", &pooler).await?;
            self.remove(CNPG, "Cluster", &cluster).await?;
            self.remove("v1", "ConfigMap", &read_monitoring(&name))
                .await?;
            st.read_replicas = None;
            return Ok(());
        }
        // They're copied from the primary: wait for one rather than fail
        // the first copy.
        if serving || have.is_some() {
            let mut cm = self.object("v1", "ConfigMap", &read_monitoring(&name), READ);
            // CloudNativePG reloads the queries when they change.
            cm["metadata"]["labels"]["cnpg.io/reload"] = json!("");
            cm["data"] = json!({ "queries": LAG_QUERY });
            self.apply(&cm).await?;
            self.apply(&self.read_replicas_cluster(ext)).await?;
            self.apply(&self.read_replicas_pooler()).await?;
            self.read_replicas_event(have.unwrap_or(0), want).await;
        }
        let pods = self
            .pods(&format!(
                "cnpg.io/cluster={cluster},cnpg.io/podRole=instance"
            ))
            .await?;
        let mut members = Vec::new();
        for p in pods
            .iter()
            .filter(|p| p.metadata.deletion_timestamp.is_none())
        {
            let m = crate::controller::members(std::slice::from_ref(p), "", None).remove(0);
            let ip = p.status.as_ref().and_then(|s| s.pod_ip.clone());
            let lag = match ip {
                Some(ip) => metrics(&ip).await.and_then(|m| replication_lag(&m)),
                None => None,
            };
            members.push(crd::ReadMember {
                name: m.name,
                node: m.node,
                ready: m.ready,
                lag_seconds: lag,
            });
        }
        members.sort_by(|a, b| a.name.cmp(&b.name));
        let ready = members.iter().filter(|m| m.ready).count() as i32;
        st.read_replicas = Some(crd::ReadReplicasStatus { ready, members });
        st.endpoints.read = format!("{}:5432", self.svc_host(&pooler));
        Ok(())
    }

    /// Its extensions: what to load and make, from what's wanted and what
    /// the instances have loaded (extensions.rs).
    async fn extensions_plan(&self, name: &str) -> anyhow::Result<crate::extensions::Plan> {
        let wanted = self
            .db
            .spec
            .postgres
            .as_ref()
            .map(|p| p.extensions.clone())
            .unwrap_or_default();
        let made = self
            .get(
                CNPG,
                "Database",
                &crate::extensions::database_object_name(name),
            )
            .await?;
        let pods = self
            .pods(&format!("cnpg.io/cluster={name},cnpg.io/podRole=instance"))
            .await?;
        // Every instance that reports, alike: a library is loaded once all have it.
        let mut loaded: Option<std::collections::BTreeSet<String>> = None;
        for ip in pods
            .iter()
            .filter(|p| p.metadata.deletion_timestamp.is_none())
            .filter_map(|p| p.status.as_ref()?.pod_ip.clone())
        {
            if let Some(libs) = metrics(&ip)
                .await
                .and_then(|m| crate::extensions::loaded_libraries(&m))
            {
                loaded = Some(match loaded {
                    Some(have) => have.intersection(&libs).cloned().collect(),
                    None => libs,
                });
            }
        }
        if wanted.is_empty() && made.is_none() {
            return Ok(crate::extensions::plan(&[], &Default::default(), "", ""));
        }
        let have = crate::extensions::Have::from(made.as_ref().map(|m| &m.data), loaded);
        Ok(crate::extensions::plan(
            &wanted,
            &have,
            &database(self.db),
            &owner(self.db),
        ))
    }

    pub async fn postgres(
        &self,
        mut st: DatabaseStatus,
    ) -> anyhow::Result<(DatabaseStatus, Duration)> {
        let name = self.name();
        // A failed import started again: the Cluster is made again.
        if self.import_restart(&mut st).await? {
            st.phase = crd::CREATING.into();
            st.message = "Starting the import again".into();
            return Ok((st, Duration::from_secs(5)));
        }
        let mut cm = self.object("v1", "ConfigMap", &monitoring(&name), INSTANCE);
        // CloudNativePG reloads the queries when they change.
        cm["metadata"]["labels"]["cnpg.io/reload"] = json!("");
        cm["data"] = json!({ "queries": format!("{REPLICA_LAG_QUERY}{}", crate::extensions::PRELOAD_QUERY) });
        self.apply(&cm).await?;
        let ext = self.extensions_plan(&name).await?;
        let mut cluster = self.postgres_cluster(&ext);
        // With high availability CloudNativePG refuses a new image and new
        // settings at once (a switchover in between): the image first, the
        // settings at the next pass.
        if let Some(have) = self.get(CNPG, "Cluster", &name).await? {
            let spec = &have.data["spec"];
            if spec["imageName"] != cluster["spec"]["imageName"]
                && spec["postgresql"] != cluster["spec"]["postgresql"]
            {
                let current = spec["postgresql"].clone();
                let keep = |k: &str| current.get(k).cloned();
                let mut pg = json!({});
                for k in ["parameters", "shared_preload_libraries"] {
                    if let Some(v) = keep(k) {
                        pg[k] = v;
                    }
                }
                if pg == json!({}) {
                    cluster["spec"]
                        .as_object_mut()
                        .map(|m| m.remove("postgresql"));
                } else {
                    cluster["spec"]["postgresql"] = pg;
                }
            }
        }
        self.apply(&cluster).await.map_err(|e| {
            if e.to_string().contains("isn't known") {
                anyhow::anyhow!(
                    "the Postgres engine (CloudNativePG) isn't installed on this cluster"
                )
            } else {
                e
            }
        })?;
        if let Some(spec) = &ext.database {
            let mut d = self.object(
                CNPG,
                "Database",
                &crate::extensions::database_object_name(&name),
                INSTANCE,
            );
            d["spec"] = spec.clone();
            d["spec"]["cluster"] = json!({ "name": name });
            self.apply(&d).await?;
        }
        let pooler = format!("{name}-pooler");
        if pooler_on(self.db) {
            self.apply(&self.postgres_pooler()).await?;
        } else {
            self.remove(CNPG, "Pooler", &pooler).await?;
        }

        // Base backups on a schedule; WAL is archived as it's written.
        match backup_on(self.db) {
            Some(b) => {
                for d in [
                    Some(&b.destination),
                    self.db
                        .spec
                        .restore
                        .as_ref()
                        .map(|r| &r.from_backup.destination),
                ]
                .into_iter()
                .flatten()
                {
                    self.region_secret(d).await?;
                }
                let mut sb =
                    self.object(CNPG, "ScheduledBackup", &format!("{name}-backup"), "backup");
                let schedule = if b.base_backup_schedule.is_empty() {
                    crd::DEFAULT_BACKUP_SCHEDULE
                } else {
                    &b.base_backup_schedule
                };
                sb["spec"] = json!({
                    "schedule": schedule,
                    "cluster": { "name": name },
                    "method": "barmanObjectStore",
                    "backupOwnerReference": "self",
                    // The first base backup right away: until then there's
                    // nothing to restore.
                    "immediate": true,
                });
                self.apply(&sb).await?;
            }
            None => {
                if let Some(r) = &self.db.spec.restore {
                    self.region_secret(&r.from_backup.destination).await?;
                }
                self.remove(CNPG, "ScheduledBackup", &format!("{name}-backup"))
                    .await?;
            }
        }

        let c = self.get(CNPG, "Cluster", &name).await?;
        let status = c
            .as_ref()
            .map(|c| c.data["status"].clone())
            .unwrap_or_default();
        let ready = status["readyInstances"].as_i64().unwrap_or(0) as i32;
        let primary = status["currentPrimary"].as_str().unwrap_or("").to_owned();
        let cnpg_phase = status["phase"].as_str().unwrap_or("").to_owned();

        if !st.primary.is_empty() && !primary.is_empty() && primary != st.primary {
            st.failovers += 1;
        }
        if !primary.is_empty() {
            st.primary = primary.clone();
        }
        st.instances = self.db.spec.instances;
        st.ready_instances = ready;
        let want = st.instances;
        phase_for(&mut st, ready, want, !primary.is_empty() && ready > 0);
        if st.phase != crate::crd::READY && !cnpg_phase.is_empty() {
            st.message = cnpg_phase;
        }
        let pods = self
            .pods(&format!("cnpg.io/cluster={name},cnpg.io/podRole=instance"))
            .await?;
        st.members = crate::controller::members(&pods, &primary, Some("cnpg.io/instanceRole"));
        st.backup = match backup_on(self.db) {
            Some(_) => {
                let ip = pods
                    .iter()
                    .find(|p| p.metadata.name.as_deref() == Some(primary.as_str()))
                    .and_then(|p| p.status.as_ref()?.pod_ip.clone());
                let at = match ip {
                    Some(ip) => last_archived(&ip).await,
                    None => None,
                };
                // Keep the last known time when the metrics don't answer.
                let at = at.or_else(|| {
                    st.backup
                        .as_ref()
                        .map(|b| b.last_archived_wal.clone())
                        .filter(|s| !s.is_empty())
                });
                Some(backup_status(&status, at))
            }
            None => None,
        };
        if backup_on(self.db).is_some() {
            self.backup_events().await;
        }
        st.endpoints = Default::default();
        st.endpoints.primary = format!("{}:5432", self.svc_host(&format!("{name}-rw")));
        if self.db.spec.instances > 1 {
            st.endpoints.replicas = format!("{}:5432", self.svc_host(&format!("{name}-ro")));
        }
        if pooler_on(self.db) {
            st.endpoints.pooler = format!("{}:5432", self.svc_host(&pooler));
        }
        self.postgres_read_replicas(&mut st, !primary.is_empty() && ready > 0, &ext)
            .await?;
        st.extensions = ext.status;
        self.import(&mut st, !primary.is_empty() && ready > 0)
            .await?;
        let next = crate::import::requeue(&st, Duration::from_secs(10));
        Ok((st, next))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dest(path: &str) -> Destination {
        Destination {
            endpoint_url: "http://s3:9000".into(),
            bucket: "b".into(),
            path: path.into(),
            region: String::new(),
            credentials_secret: "s3".into(),
        }
    }

    #[test]
    fn paths_per_database() {
        assert_eq!(destination_path(&dest(""), "ns"), "s3://b/ns");
        assert_eq!(destination_path(&dest("/pg/"), "ns"), "s3://b/pg/ns");
        let o = object_store(
            &dest("pg"),
            "ns",
            "main",
            Some("main-backup-region-eu-west-1"),
        );
        assert_eq!(o["serverName"], "main");
        assert_eq!(
            o["s3Credentials"]["accessKeyId"],
            json!({ "name": "s3", "key": "accessKeyId" })
        );
        assert_eq!(
            o["s3Credentials"]["region"]["name"],
            "main-backup-region-eu-west-1"
        );
        assert!(
            object_store(&dest(""), "ns", "main", None)["s3Credentials"]
                .get("region")
                .is_none()
        );
    }

    #[test]
    fn times() {
        assert_eq!(rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339(1_794_204_079), "2026-11-09T06:01:19Z");
        assert_eq!(rfc3339(951_782_400), "2000-02-29T00:00:00Z");
        let m = "# HELP x\ncnpg_pg_stat_archiver_archived_count 12\ncnpg_pg_stat_archiver_last_archived_time 1.794204079e+09\n";
        assert_eq!(archived_time(m).as_deref(), Some("2026-11-09T06:01:19Z"));
        assert_eq!(
            archived_time("cnpg_pg_stat_archiver_last_archived_time 0\n"),
            None
        );
        assert_eq!(archived_time(""), None);
    }

    #[test]
    fn lag() {
        let m = "# HELP cnpg_pyo_read_replica_lag x\ncnpg_pg_replication_lag 80\ncnpg_pyo_read_replica_lag 1.2345\ncnpg_pg_replication_is_wal_receiver_up 1\n";
        assert_eq!(replication_lag(m), Some(1.2));
        assert_eq!(
            replication_lag(
                "cnpg_pyo_read_replica_lag -0.01\ncnpg_pg_replication_is_wal_receiver_up 1\n"
            ),
            Some(0.0)
        );
        // Not streaming: unknown, not 0.
        assert_eq!(
            replication_lag(
                "cnpg_pyo_read_replica_lag 0\ncnpg_pg_replication_is_wal_receiver_up 0\n"
            ),
            None
        );
        // Queries not loaded yet.
        assert_eq!(
            replication_lag("cnpg_pg_replication_is_wal_receiver_up 1\n"),
            None
        );
        assert_eq!(replication_lag(""), None);
    }

    #[test]
    fn backup_events() {
        let since = "2026-11-09T08:00:00Z";
        let running = json!({ "phase": "running", "startedAt": "2026-11-09T09:00:00Z" });
        assert_eq!(
            backup_changes(&running, since)
                .iter()
                .map(|e| e.2)
                .collect::<Vec<_>>(),
            ["BackupStarted"]
        );
        let done = json!({ "phase": "completed", "startedAt": "2026-11-09T09:00:00Z", "stoppedAt": "2026-11-09T09:01:00Z" });
        assert_eq!(
            backup_changes(&done, since)
                .iter()
                .map(|e| e.2)
                .collect::<Vec<_>>(),
            ["BackupStarted", "BackupFinished"]
        );
        let failed = json!({ "phase": "failed", "startedAt": "2026-11-09T09:00:00Z", "stoppedAt": "2026-11-09T09:01:00Z", "error": "exit status 2" });
        assert!(
            backup_changes(&failed, since)[1]
                .3
                .starts_with("A base backup failed: can't reach the storage endpoint")
        );
        // Before this controller started: not again.
        assert!(backup_changes(&done, "2026-11-10T00:00:00Z").is_empty());
    }

    #[test]
    fn backup_messages() {
        let ok = json!({ "firstRecoverabilityPoint": "2026-11-09T06:00:00Z", "lastSuccessfulBackup": "2026-11-09T06:00:05Z",
            "conditions": [{ "type": "ContinuousArchiving", "status": "True" }, { "type": "LastBackupSucceeded", "status": "True" }] });
        let b = backup_status(&ok, Some("t".into()));
        assert_eq!(
            (
                b.first_recoverability_point.as_str(),
                b.last_archived_wal.as_str(),
                b.message.as_str()
            ),
            ("2026-11-09T06:00:00Z", "t", "")
        );
        let bad = json!({ "conditions": [{ "type": "ContinuousArchiving", "status": "False", "message": "access denied" }] });
        assert_eq!(
            backup_status(&bad, None).message,
            "WAL archiving fails: access denied"
        );
        let bad = json!({ "conditions": [{ "type": "ContinuousArchiving", "status": "False", "message": "unexpected failure invoking barman-cloud-wal-archive: exit status 4" }] });
        assert!(backup_status(&bad, None).message.starts_with(
            "WAL archiving fails: the storage refused it. Check the bucket and the access keys ("
        ));
    }
}
