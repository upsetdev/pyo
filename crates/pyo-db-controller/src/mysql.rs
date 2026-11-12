//! MySQL through MOCO, with Pyo's patches (deploy/images/moco): a
//! MySQLCluster of 1, 3 or 5 instances (a primary and semi-synchronous
//! replicas, MOCO's own failover), with settings sized from the memory
//! limit, and the app's login and database made by the controller.
//!
//! - Names: a cluster this controller makes gets the name prefix "" (MOCO's
//!   `moco.cybozu.com/name-prefix` annotation), so its StatefulSet, pods
//!   (`<name>-<i>`), Services (`<name>-primary`, `<name>-replica`), its
//!   users' Secret (`<name>`) and Jobs (`backup-<name>-...`,
//!   `restore-<name>`) are named after the database.
//! - The ConfigMap `<name>-mysql` holds the settings (see [`settings`]); MOCO
//!   restarts the instances one at a time when it changes.
//! - The app's login is the credentials secret's `username` (default "app")
//!   and `password`, with every privilege on the database "app" and nothing
//!   else. MOCO's own users (moco-admin and the rest) stay in its users'
//!   Secret.
//! - Backups: a BackupPolicy `<name>-backup` (a full dump and the binlogs
//!   since the last one, on the schedule) with the ServiceAccount
//!   `<name>-backup` its Jobs run as; the first backup runs at once. They
//!   go under `<path>/<namespace>/<name>/` in the bucket, as Postgres's
//!   (`bucketConfig.prefix`), and each backup Job deletes the ones older
//!   than `retentionDays` that no restore in that time needs.

use std::{collections::BTreeMap, time::Duration};

use k8s_openapi::api::{
    batch::v1::{CronJob, Job},
    core::v1::{Pod, Secret},
};
use kube::{
    Api, ResourceExt,
    api::{ListParams, Patch, PatchParams, PostParams},
};
use mysql_async::prelude::Queryable;
use serde_json::{Value, json};

use crate::{
    controller::{Kube, LABEL_COMPONENT, LABEL_DATABASE, phase_for, resources},
    crd::{self, BackupStatus, DatabaseStatus, Destination},
    postgres::backup_changes,
};

pub const MOCO: &str = "moco.cybozu.com/v1beta2";
/// The app's login and database.
pub const APP: &str = "app";
/// The pods' component label.
pub const COMPONENT: &str = "mysql";
/// MOCO's label for an instance's role ("primary" or "replica").
const ROLE: &str = "moco.cybozu.com/role";
/// MOCO's annotation (Pyo's patch) with the prefix of the names it gives a
/// cluster's parts, set when the cluster is made and fixed after: none, so
/// they're named after the database.
pub const NAME_PREFIX: &str = "moco.cybozu.com/name-prefix";
const PORT: u16 = 3306;
/// mysqld_exporter's collectors: SHOW GLOBAL STATUS (Questions,
/// Threads_connected/running, the buffer pool's reads), SHOW GLOBAL
/// VARIABLES (max_connections) and SHOW REPLICA STATUS (lag). MOCO passes
/// each as --collect.<name>, and the exporter takes no "=false", so its
/// other default ones (info_schema.innodb_cmp, innodb_cmpmem: two small
/// tables; query_response_time: empty on MySQL) stay on.
const COLLECTORS: [&str; 3] = ["global_status", "global_variables", "slave_status"];
const MIB: u64 = 1 << 20;

/// A Kubernetes quantity in bytes ("1Gi", "1536Mi", "2G", "1073741824").
pub fn bytes(q: &str) -> Option<u64> {
    const UNITS: [(&str, f64); 8] = [
        ("Ki", 1024.0),
        ("Mi", 1048576.0),
        ("Gi", 1073741824.0),
        ("Ti", 1099511627776.0),
        ("k", 1e3),
        ("M", 1e6),
        ("G", 1e9),
        ("T", 1e12),
    ];
    let (num, mul) = UNITS
        .iter()
        .find_map(|(s, m)| q.strip_suffix(s).map(|n| (n, *m)))
        .unwrap_or((q, 1.0));
    let n: f64 = num
        .parse()
        .ok()
        .filter(|n: &f64| n.is_finite() && *n > 0.0)?;
    Some((n * mul) as u64)
}

/// mysqld's settings for a memory limit and a disk, in bytes. MOCO's
/// defaults are for big servers (a buffer pool of 70% of memory, 100000
/// connections, 65536 open tables, the performance schema on), which runs
/// out of memory at 512Mi; these keep mysqld well under the limit:
///
/// - the buffer pool: 50% of the memory under 2Gi, 55% under 4Gi, 60%
///   above, in whole 128Mi chunks (MySQL rounds it up to them);
/// - connections: one per 8Mi, 100 to 4000 (128 at 1Gi);
/// - the performance schema off under 2Gi (it sizes itself from the
///   connections and open tables and takes 100Mi and more);
/// - small per-connection buffers and in-memory temporary tables under
///   4Gi, MySQL's defaults above;
/// - the redo log a sixteenth of the disk (64Mi to 2Gi), binlogs kept 2
///   days (MOCO's are 800Mi of redo and 30 days of binlogs).
pub fn settings(memory: u64, storage: u64) -> BTreeMap<String, String> {
    let mi = memory / MIB;
    let pct = if mi < 2048 {
        50
    } else if mi < 4096 {
        55
    } else {
        60
    };
    let pool = (mi * pct / 100 / 128).max(1) * 128;
    let small = mi < 4096;
    let clamp = |v: u64, lo: u64, hi: u64| v.clamp(lo, hi);
    let m = |v: u64| format!("{v}M");
    let mut s = BTreeMap::new();
    let mut set = |k: &str, v: String| {
        s.insert(k.to_owned(), v);
    };
    set("innodb_buffer_pool_size", m(pool));
    set("max_connections", clamp(mi / 8, 100, 4000).to_string());
    set(
        "performance_schema",
        if mi < 2048 { "OFF" } else { "ON" }.into(),
    );
    set("table_open_cache", clamp(mi, 400, 4000).to_string());
    set(
        "table_definition_cache",
        clamp(mi / 2, 400, 2000).to_string(),
    );
    set("thread_cache_size", clamp(mi / 64, 16, 100).to_string());
    set("temptable_max_ram", m(clamp(mi / 16, 32, 1024)));
    set("tmp_table_size", if small { "16M" } else { "64M" }.into());
    set(
        "max_heap_table_size",
        if small { "16M" } else { "64M" }.into(),
    );
    set("sort_buffer_size", if small { "256K" } else { "2M" }.into());
    set("join_buffer_size", if small { "256K" } else { "1M" }.into());
    set(
        "innodb_log_buffer_size",
        if small { "16M" } else { "64M" }.into(),
    );
    set(
        "innodb_redo_log_capacity",
        m(clamp(storage / MIB / 16, 64, 2048)),
    );
    set("max_allowed_packet", "64M".into());
    set("binlog_expire_logs_seconds", "172800".into());
    // The app's login makes functions and triggers (an import's too)
    // without SUPER while the binlog is on.
    set("log_bin_trust_function_creators", "ON".into());
    s
}

/// A six-field cron (seconds first) as the five fields a CronJob takes.
pub fn five_field(cron: &str) -> String {
    let f: Vec<&str> = cron.split_whitespace().collect();
    if f.len() == 6 {
        f[1..].join(" ")
    } else {
        cron.to_owned()
    }
}

/// A user name MySQL takes unquoted (32 characters at most).
fn valid_user(u: &str) -> bool {
    !u.is_empty()
        && u.len() <= 32
        && u.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
        && !u.as_bytes()[0].is_ascii_digit()
}

/// A string as a quoted SQL literal.
fn literal(s: &str) -> String {
    let mut o = String::with_capacity(s.len() + 2);
    o.push('\'');
    for c in s.chars() {
        match c {
            '\'' => o.push_str("''"),
            '\\' => o.push_str("\\\\"),
            '\0' => o.push_str("\\0"),
            c => o.push(c),
        }
    }
    o.push('\'');
    o
}

/// The statements that make the app's database and login (idempotent).
pub fn app_statements(user: &str, password: &str) -> Vec<String> {
    let (u, p) = (literal(user), literal(password));
    vec![
        format!("CREATE DATABASE IF NOT EXISTS `{APP}`"),
        format!("CREATE USER IF NOT EXISTS {u}@'%' IDENTIFIED BY {p}"),
        format!("ALTER USER {u}@'%' IDENTIFIED BY {p}"),
        format!("GRANT ALL PRIVILEGES ON `{APP}`.* TO {u}@'%'"),
    ]
}

/// A connection to one mysqld, given up on after 5s. TLS (mysqld's own
/// certificate, not checked: it's inside the cluster) carries the password
/// for MySQL 8.4's caching_sha2_password.
async fn connect(
    host: &str,
    user: &str,
    password: &str,
    db: Option<&str>,
) -> anyhow::Result<mysql_async::Conn> {
    let ssl = mysql_async::SslOpts::default()
        .with_danger_accept_invalid_certs(true)
        .with_danger_skip_domain_validation(true);
    let opts = mysql_async::OptsBuilder::default()
        .ip_or_hostname(host)
        .tcp_port(PORT)
        .user(Some(user))
        .pass(Some(password))
        .db_name(db)
        .prefer_socket(false)
        .ssl_opts(ssl);
    Ok(tokio::time::timeout(Duration::from_secs(5), mysql_async::Conn::new(opts)).await??)
}

/// Whether the app can log in with `password` and has every privilege on
/// its database.
async fn app_ok(host: &str, user: &str, password: &str) -> bool {
    let check = async {
        let mut c = connect(host, user, password, Some(APP)).await.ok()?;
        let grants: Vec<String> = c.query("SHOW GRANTS").await.ok()?;
        let _ = c.disconnect().await;
        Some(
            grants
                .iter()
                .any(|g| g.starts_with(&format!("GRANT ALL PRIVILEGES ON `{APP}`.*"))),
        )
    };
    tokio::time::timeout(Duration::from_secs(8), check)
        .await
        .ok()
        .flatten()
        .unwrap_or(false)
}

/// Seconds a replica is behind its primary (SHOW REPLICA STATUS); None
/// when it isn't replicating or can't be asked.
async fn replica_lag(ip: &str, user: &str, password: &str) -> Option<f64> {
    let ask = async {
        let mut c = connect(ip, user, password, None).await.ok()?;
        let row: Option<mysql_async::Row> = c.query_first("SHOW REPLICA STATUS").await.ok()?;
        let _ = c.disconnect().await;
        let row = row?;
        let running = |k: &str| {
            row.get_opt::<String, _>(k)
                .and_then(Result::ok)
                .is_some_and(|v| v == "Yes")
        };
        if !running("Replica_IO_Running") || !running("Replica_SQL_Running") {
            return None;
        }
        row.get_opt::<Option<i64>, _>("Seconds_Behind_Source")
            .and_then(Result::ok)
            .flatten()
            .map(|s| s as f64)
    };
    tokio::time::timeout(Duration::from_secs(5), ask)
        .await
        .ok()
        .flatten()
}

fn secret_str(s: &Secret, key: &str) -> String {
    s.data
        .as_ref()
        .and_then(|d| d.get(key))
        .map(|b| String::from_utf8_lossy(&b.0).into_owned())
        .unwrap_or_default()
}

/// A Job's start and end as a Backup status `backup_changes` reads.
pub fn job_status(job: &Job) -> Value {
    let st = job.status.clone().unwrap_or_default();
    let cond = |t: &str| {
        st.conditions
            .as_ref()
            .and_then(|c| c.iter().find(|c| c.type_ == t && c.status == "True"))
            .cloned()
    };
    let t = |x: Option<&k8s_openapi::apimachinery::pkg::apis::meta::v1::Time>| {
        x.map(|t| crate::postgres::rfc3339(t.0.as_second()))
            .unwrap_or_default()
    };
    let mut v = json!({ "startedAt": t(st.start_time.as_ref()) });
    if let Some(c) = cond("Failed") {
        v["phase"] = json!("failed");
        v["stoppedAt"] = json!(t(c.last_transition_time.as_ref()));
        let why = c
            .message
            .clone()
            .filter(|m| !m.is_empty())
            .or(c.reason.clone())
            .unwrap_or_default();
        v["error"] = json!(why);
    } else if cond("Complete").is_some() {
        v["phase"] = json!("completed");
        v["stoppedAt"] = json!(t(st.completion_time.as_ref().or(cond("Complete")
            .and_then(|c| c.last_transition_time)
            .as_ref())));
    }
    v
}

impl Kube<'_> {
    fn backup_name(&self) -> String {
        format!("{}-backup", self.name())
    }

    fn config_name(&self) -> String {
        format!("{}-mysql", self.name())
    }

    /// Where MOCO's Jobs put and get backups, and the keys they use. The
    /// keys start with the destination's path, so they are
    /// `<path>/<namespace>/<name>/...`, as Postgres's.
    fn job_config(&self, d: &Destination) -> Value {
        let key = |env: &str, k: &str| json!({ "name": env, "valueFrom": { "secretKeyRef": { "name": d.credentials_secret, "key": k } } });
        let s = &self.db.spec;
        // The work directory holds a whole dump: a volume the size of the
        // database's own, gone with the Job.
        let mut claim = json!({ "accessModes": ["ReadWriteOnce"], "resources": { "requests": { "storage": s.storage } } });
        if !s.storage_class.is_empty() {
            claim["storageClassName"] = json!(s.storage_class);
        }
        json!({
            "serviceAccountName": self.backup_name(),
            "env": [key("AWS_ACCESS_KEY_ID", "accessKeyId"), key("AWS_SECRET_ACCESS_KEY", "secretAccessKey")],
            "bucketConfig": {
                "bucketName": d.bucket,
                "region": if d.region.is_empty() { "us-east-1" } else { &d.region },
                "endpointURL": d.endpoint_url,
                "usePathStyle": true,
                "prefix": d.path.trim_matches('/'),
            },
            "workVolume": { "ephemeral": { "volumeClaimTemplate": { "spec": claim } } },
            // MOCO asks for 4 CPUs and 4Gi by default.
            "threads": 2,
            "cpu": "100m",
            "memory": "256Mi",
            "maxMemory": "1Gi",
        })
    }

    pub fn mysql_config(&self) -> Value {
        let s = &self.db.spec;
        let mut o = self.object("v1", "ConfigMap", &self.config_name(), COMPONENT);
        o["data"] = json!(settings(
            bytes(&s.memory).unwrap_or(crd::MYSQL_MIN_MEMORY),
            bytes(&s.storage).unwrap_or(1 << 30)
        ));
        o
    }

    /// The MySQLCluster, its parts named after the database.
    pub fn mysql_cluster(&self) -> Value {
        let s = &self.db.spec;
        let mut mysqld = json!({ "name": "mysqld", "image": self.ctx.images.mysql_84 });
        if let Some(r) = resources(&s.cpu, &s.memory) {
            mysqld["resources"] = r;
        }
        let mut claim = json!({ "accessModes": ["ReadWriteOnce"], "resources": { "requests": { "storage": s.storage } } });
        if !s.storage_class.is_empty() {
            claim["storageClassName"] = json!(s.storage_class);
        }
        let mut spec = json!({
            "replicas": s.instances,
            "mysqlConfigMapName": self.config_name(),
            // Slow queries stay in mysqld's file (rotated by MOCO's agent)
            // rather than in a fluent-bit sidecar per instance.
            "disableSlowQueryLogContainer": true,
            // mysqld_exporter beside each instance (port 9104, named
            // mysqld-metrics by MOCO): global status and variables and
            // replica status (see COLLECTORS).
            "collectors": COLLECTORS,
            "podTemplate": {
                "metadata": { "labels": { LABEL_DATABASE: self.name(), LABEL_COMPONENT: COMPONENT } },
                // MOCO's 300s: a frozen mysqld (it can't act on SIGTERM) would
                // hold its pod, and the failover, for 5 minutes after its
                // liveness probe fails. InnoDB recovers from the kill.
                "spec": { "containers": [mysqld], "terminationGracePeriodSeconds": 60 },
                // MOCO gives the exporter 200m and 100Mi.
                "overwriteContainers": [{
                    "name": "mysqld-exporter",
                    "resources": { "requests": { "cpu": "10m", "memory": "24Mi" }, "limits": { "memory": "64Mi" } },
                }],
            },
            "volumeClaimTemplates": [{ "metadata": { "name": "mysql-data" }, "spec": claim }],
        });
        if crate::postgres::backup_on(self.db).is_some() {
            spec["backupPolicyName"] = json!(self.backup_name());
        }
        if let Some(r) = &s.restore {
            let src = &r.from_backup;
            // MOCO restores to a point in time: everything is up to when
            // this Database was made.
            let at = if r.target_time.is_empty() {
                self.db
                    .metadata
                    .creation_timestamp
                    .as_ref()
                    .map(|t| crate::postgres::rfc3339(t.0.as_second()))
                    .unwrap_or_default()
            } else {
                r.target_time.clone()
            };
            let ns = if src.source_namespace.is_empty() {
                self.ns()
            } else {
                src.source_namespace.clone()
            };
            spec["restore"] = json!({
                "sourceName": src.source_name,
                "sourceNamespace": ns,
                "restorePoint": at,
                "jobConfig": self.job_config(&src.destination),
            });
        }
        let mut o = self.object(MOCO, "MySQLCluster", &self.name(), COMPONENT);
        o["metadata"]["annotations"] = json!({ NAME_PREFIX: "" });
        o["spec"] = spec;
        o
    }

    pub fn mysql_backup_policy(&self, b: &crd::BackupSpec) -> Value {
        let schedule = if b.base_backup_schedule.is_empty() {
            crd::DEFAULT_BACKUP_SCHEDULE
        } else {
            &b.base_backup_schedule
        };
        let days = if b.retention_days == 0 {
            crd::DEFAULT_RETENTION_DAYS
        } else {
            b.retention_days
        };
        let mut o = self.object(MOCO, "BackupPolicy", &self.backup_name(), "backup");
        o["spec"] = json!({
            "schedule": five_field(schedule),
            "retentionDays": days,
            "concurrencyPolicy": "Forbid",
            "successfulJobsHistoryLimit": 2,
            "failedJobsHistoryLimit": 2,
            "jobConfig": self.job_config(&b.destination),
        });
        o
    }

    /// The ServiceAccount MOCO's backup and restore Jobs run as (MOCO
    /// gives it what they need on the MySQLCluster).
    async fn backup_account(&self) -> anyhow::Result<()> {
        if self
            .get("v1", "ServiceAccount", &self.backup_name())
            .await?
            .is_none()
        {
            let mut sa = self.object("v1", "ServiceAccount", &self.backup_name(), "backup");
            sa["automountServiceAccountToken"] = json!(true);
            self.apply(&sa).await?;
        }
        Ok(())
    }

    /// MOCO's backup and restore Jobs of this database.
    async fn mysql_jobs(&self) -> anyhow::Result<Vec<Job>> {
        let api: Api<Job> = Api::namespaced(self.ctx.client.clone(), &self.ns());
        let sel = format!(
            "app.kubernetes.io/name=mysql-backup,app.kubernetes.io/created-by=moco,app.kubernetes.io/instance={}",
            self.name()
        );
        Ok(api.list(&ListParams::default().labels(&sel)).await?.items)
    }

    /// The first backup, right away (the schedule may be a day off): a Job
    /// from MOCO's CronJob `cj_name`, as `kubectl create job --from=cronjob/...`.
    async fn first_backup(&self, jobs: &[Job], cj_name: &str) -> anyhow::Result<()> {
        if jobs.iter().any(|j| j.name_any().starts_with(cj_name)) {
            return Ok(());
        }
        let Some(cj) = Api::<CronJob>::namespaced(self.ctx.client.clone(), &self.ns())
            .get_opt(cj_name)
            .await?
        else {
            return Ok(());
        };
        let tmpl = cj.spec.job_template.clone();
        let job = Job {
            metadata: k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta {
                name: Some(format!("{cj_name}-first")),
                labels: tmpl.metadata.as_ref().and_then(|m| m.labels.clone()),
                owner_references: Some(vec![
                    k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference {
                        api_version: "batch/v1".into(),
                        kind: "CronJob".into(),
                        name: cj_name.to_owned(),
                        uid: cj.uid().unwrap_or_default(),
                        controller: Some(true),
                        block_owner_deletion: None,
                    },
                ]),
                ..Default::default()
            },
            spec: tmpl.spec,
            ..Default::default()
        };
        match Api::<Job>::namespaced(self.ctx.client.clone(), &self.ns())
            .create(&PostParams::default(), &job)
            .await
        {
            Err(kube::Error::Api(e)) if e.code == 409 => Ok(()),
            r => r.map(|_| ()).map_err(Into::into),
        }
    }

    /// Labels MOCO's Job pods (backups, restores; the restore Job is
    /// `restore_job`) for Pyo's Logs tab.
    async fn label_job_pods(&self, restore_job: &str) -> anyhow::Result<()> {
        let api: Api<Pod> = Api::namespaced(self.ctx.client.clone(), &self.ns());
        let sel = format!(
            "app.kubernetes.io/name=mysql-backup,app.kubernetes.io/created-by=moco,app.kubernetes.io/instance={},!{LABEL_DATABASE}",
            self.name()
        );
        for p in api.list(&ListParams::default().labels(&sel)).await?.items {
            let job = p.labels().get("job-name").cloned().unwrap_or_default();
            let component = if job == restore_job {
                "restore"
            } else {
                "backup"
            };
            let patch = json!({ "metadata": { "labels": { LABEL_DATABASE: self.name(), LABEL_COMPONENT: component } } });
            api.patch(
                &p.name_any(),
                &PatchParams::default(),
                &Patch::Merge(&patch),
            )
            .await?;
        }
        Ok(())
    }

    /// Events for backups (Jobs of the CronJob `cj_name`) starting and
    /// ending, each once.
    async fn mysql_backup_events(&self, jobs: &[Job], cj_name: &str) {
        for j in jobs.iter().filter(|j| j.name_any().starts_with(cj_name)) {
            let uid = j.uid().unwrap_or_default();
            for (what, warning, reason, note) in backup_changes(&job_status(j), &self.ctx.started) {
                let note = note.replace("A base backup", "A backup");
                let first = self
                    .ctx
                    .announced
                    .lock()
                    .unwrap()
                    .insert((uid.clone(), what));
                if first {
                    self.event(warning, reason, format!("{note} ({})", j.name_any()))
                        .await;
                }
            }
        }
    }

    /// The app's database and login, when they aren't right yet.
    async fn app_user(&self, secret: &Secret) -> anyhow::Result<()> {
        let name = self.name();
        let user = secret_str(secret, "username");
        let user = if user.is_empty() {
            APP.to_owned()
        } else {
            user
        };
        anyhow::ensure!(
            valid_user(&user),
            "the credentials' username must be lowercase letters, digits and underscores (at most 32)"
        );
        let password = secret_str(secret, "password");
        anyhow::ensure!(!password.is_empty(), "the credentials have no password");
        let host = self.svc_host(&format!("{name}-primary"));
        if app_ok(&host, &user, &password).await {
            return Ok(());
        }
        let api: Api<Secret> = Api::namespaced(self.ctx.client.clone(), &self.ns());
        let moco = api.get(&name).await?;
        let admin = secret_str(&moco, "ADMIN_PASSWORD");
        let mut c = connect(&host, "moco-admin", &admin, None)
            .await
            .map_err(|e| anyhow::anyhow!("can't reach the primary: {e}"))?;
        for q in app_statements(&user, &password) {
            c.query_drop(&q)
                .await
                .map_err(|e| anyhow::anyhow!("making the app's login: {e}"))?;
        }
        let _ = c.disconnect().await;
        tracing::info!(db = %self.name(), ns = %self.ns(), user, "app login set");
        Ok(())
    }

    pub async fn mysql(
        &self,
        mut st: DatabaseStatus,
        secret: &Secret,
    ) -> anyhow::Result<(DatabaseStatus, Duration)> {
        let name = self.name();
        let s = &self.db.spec;
        let existing = self
            .get(MOCO, "MySQLCluster", &name)
            .await
            .map_err(|e| self.not_installed(e))?;
        // MOCO grows a cluster but never shrinks it.
        if let Some(c) = &existing {
            let have = c.data["spec"]["replicas"].as_i64().unwrap_or(0) as i32;
            anyhow::ensure!(
                s.instances >= have,
                "MySQL can't go from {have} instances to {}: it only grows (1 to 3 or 5, 3 to 5)",
                s.instances
            );
        }
        let (cron_job, restore_job) = (format!("backup-{name}"), format!("restore-{name}"));
        let backup = crate::postgres::backup_on(self.db);
        if backup.is_some() || s.restore.is_some() {
            self.backup_account().await?;
        }
        if let Some(b) = backup {
            self.apply(&self.mysql_backup_policy(b)).await?;
        }
        self.apply(&self.mysql_config()).await?;
        self.apply(&self.mysql_cluster())
            .await
            .map_err(|e| self.not_installed(e))?;
        if backup.is_none() {
            // After the cluster stops naming it (MOCO refuses deleting a
            // policy in use).
            self.remove(MOCO, "BackupPolicy", &self.backup_name())
                .await?;
        }

        let c = self.get(MOCO, "MySQLCluster", &name).await?;
        let status = c
            .as_ref()
            .map(|c| c.data["status"].clone())
            .unwrap_or_default();
        let cond = |t: &str| {
            status["conditions"]
                .as_array()
                .is_some_and(|c| c.iter().any(|c| c["type"] == t && c["status"] == "True"))
        };
        let available = cond("Available");
        let restoring =
            s.restore.is_some() && status["restoredTime"].as_str().unwrap_or("").is_empty();
        let pods = self
            .pods(&format!(
                "{LABEL_DATABASE}={name},{LABEL_COMPONENT}={COMPONENT}"
            ))
            .await?;
        let labelled = pods
            .iter()
            .find(|p| p.labels().get(ROLE).map(String::as_str) == Some("primary"))
            .map(|p| p.name_any());
        let primary = match status["currentPrimaryIndex"].as_i64() {
            Some(i) if available => format!("{name}-{i}"),
            _ => labelled.unwrap_or_default(),
        };
        // A restore has no primary to speak of until its data is in.
        let primary = if restoring { String::new() } else { primary };
        // The primary moved when it's another instance.
        if !st.primary.is_empty() && !primary.is_empty() && primary != st.primary {
            st.failovers += 1;
        }
        if !primary.is_empty() {
            st.primary = primary.clone();
        }
        let mut members = crate::controller::members(&pods, &primary, Some(ROLE));
        let ready = members.iter().filter(|m| m.ready).count() as i32;
        st.instances = s.instances;
        st.ready_instances = ready;
        phase_for(
            &mut st,
            ready,
            s.instances,
            available && !restoring && !primary.is_empty(),
        );

        if available && !restoring {
            let errant: Vec<i64> = status["errantReplicaList"]
                .as_array()
                .map(|l| l.iter().filter_map(Value::as_i64).collect())
                .unwrap_or_default();
            for i in errant {
                let pod = format!("{name}-{i}");
                if pod != primary
                    && let Err(e) = self.rebuild(&pod).await
                {
                    tracing::warn!(db = %name, pod, error = format!("{e:#}"), "rebuild errant replica");
                }
            }
        }

        let jobs = self.mysql_jobs().await.unwrap_or_default();
        if restoring {
            st.message = "Restoring from backups".into();
            let failed = jobs
                .iter()
                .filter(|j| j.name_any() == restore_job)
                .map(job_status)
                .find(|v| v["phase"] == "failed");
            if let Some(f) = failed {
                st.phase = crd::FAILED.into();
                st.message = format!("Restoring failed: {}", f["error"].as_str().unwrap_or(""));
            }
        }
        let serving = available && !restoring;
        if serving {
            if let Err(e) = self.app_user(secret).await {
                st.phase = crd::DEGRADED.into();
                st.message = format!("The app's login isn't ready: {e:#}");
            }
            // How far each replica is behind.
            let moco = Api::<Secret>::namespaced(self.ctx.client.clone(), &self.ns())
                .get(&name)
                .await
                .ok();
            let ro = moco
                .as_ref()
                .map(|m| secret_str(m, "READONLY_PASSWORD"))
                .unwrap_or_default();
            for m in members.iter_mut().filter(|m| m.role == "replica") {
                let ip = pods
                    .iter()
                    .find(|p| p.name_any() == m.name)
                    .and_then(|p| p.status.as_ref()?.pod_ip.clone());
                if let Some(ip) = ip {
                    m.lag_seconds = replica_lag(&ip, "moco-readonly", &ro).await;
                }
            }
        }
        st.members = members;

        if let Err(e) = self.label_job_pods(&restore_job).await {
            tracing::warn!(db = %name, error = format!("{e:#}"), "label job pods");
        }
        st.backup = match backup {
            Some(b) => {
                if serving && let Err(e) = self.first_backup(&jobs, &cron_job).await {
                    tracing::warn!(db = %name, error = format!("{e:#}"), "first backup");
                }
                self.mysql_backup_events(&jobs, &cron_job).await;
                let days = if b.retention_days == 0 {
                    crd::DEFAULT_RETENTION_DAYS
                } else {
                    b.retention_days
                };
                Some(backup_status(
                    &status,
                    &jobs,
                    &cron_job,
                    days,
                    st.backup.as_ref(),
                ))
            }
            None => None,
        };

        st.endpoints = Default::default();
        st.endpoints.primary = format!("{}:{PORT}", self.svc_host(&format!("{name}-primary")));
        if s.instances > 1 {
            st.endpoints.replicas = format!("{}:{PORT}", self.svc_host(&format!("{name}-replica")));
        }
        // The app's login must work before anything is loaded with it.
        let ready = serving && st.phase != crd::DEGRADED;
        self.import(&mut st, ready).await?;
        let next = crate::import::requeue(&st, Duration::from_secs(10));
        Ok((st, next))
    }

    /// Starts an errant replica over: one with transactions the primary
    /// doesn't have, which MOCO leaves out of the cluster. With lossless
    /// semi-synchronous replication those are only ones no client was told
    /// were committed (an old primary that crashed between writing a
    /// transaction and a replica acknowledging it). Its data goes and MOCO
    /// clones the primary into the new disk.
    async fn rebuild(&self, pod: &str) -> anyhow::Result<()> {
        use k8s_openapi::api::core::v1::PersistentVolumeClaim;
        let pvcs: Api<PersistentVolumeClaim> = Api::namespaced(self.ctx.client.clone(), &self.ns());
        let pods: Api<Pod> = Api::namespaced(self.ctx.client.clone(), &self.ns());
        let claim = format!("mysql-data-{pod}");
        let dp = kube::api::DeleteParams::background();
        match pvcs.get_opt(&claim).await? {
            Some(c) if c.metadata.deletion_timestamp.is_none() => {
                pvcs.delete(&claim, &dp).await?;
                self.event(true, "ReplicaRebuilt", format!("{pod} had transactions the primary doesn't have, never acknowledged to a client. Its data was removed and it copies the primary again")).await;
            }
            _ => {}
        }
        // The pod holds the claim until it goes; one made before the claim
        // was gone waits for it, so it goes too.
        if pods.get_opt(pod).await?.is_some() {
            match pods.delete(pod, &dp).await {
                Ok(_) => {}
                Err(e) if crate::controller::not_found(&e) => {}
                Err(e) => return Err(e.into()),
            }
        }
        Ok(())
    }

    fn not_installed(&self, e: anyhow::Error) -> anyhow::Error {
        if e.to_string().contains("isn't known")
            || e.downcast_ref::<kube::Error>().is_some_and(|k| {
                crate::controller::not_found(k) && e.to_string().contains("page not found")
            })
        {
            anyhow::anyhow!("the MySQL engine isn't installed on this cluster")
        } else {
            e
        }
    }
}

/// The backup part of the status: the last backup MOCO recorded, the
/// first moment it can restore to, and the last failed backup Job (of the
/// CronJob `cron_job`). The first moment is the first backup seen, or once
/// backups are older than `days`, the last one's time less `days`: each
/// backup Job deletes the ones no restore after that needs.
pub fn backup_status(
    moco: &Value,
    jobs: &[Job],
    cron_job: &str,
    days: i32,
    before: Option<&BackupStatus>,
) -> BackupStatus {
    let last = moco["backup"]["time"]
        .as_str()
        .filter(|t| !t.is_empty() && !t.starts_with("0001-"))
        .unwrap_or("")
        .to_owned();
    let first = before
        .map(|b| b.first_recoverability_point.clone())
        .filter(|t| !t.is_empty() && (last.is_empty() || *t <= last))
        .unwrap_or_else(|| last.clone());
    let kept = last
        .parse::<k8s_openapi::jiff::Timestamp>()
        .ok()
        .and_then(|t| {
            t.checked_sub(k8s_openapi::jiff::SignedDuration::from_hours(
                24 * i64::from(days),
            ))
            .ok()
        })
        .map(|t| crate::postgres::rfc3339(t.as_second()));
    let first = match kept {
        Some(k) if k > first => k,
        _ => first,
    };
    let failed = jobs
        .iter()
        .filter(|j| j.name_any().starts_with(cron_job))
        .map(job_status)
        .filter(|v| v["phase"] == "failed")
        .max_by(|a, b| a["stoppedAt"].as_str().cmp(&b["stoppedAt"].as_str()));
    let last_failed = failed
        .as_ref()
        .and_then(|f| f["stoppedAt"].as_str())
        .unwrap_or("")
        .to_owned();
    let mut message = String::new();
    if !last_failed.is_empty() && last_failed > last {
        let why = failed
            .as_ref()
            .and_then(|f| f["error"].as_str())
            .unwrap_or("");
        message = if why.is_empty() {
            "the last backup failed".into()
        } else {
            format!("the last backup failed: {why}")
        };
    }
    let warnings: Vec<&str> = moco["backup"]["warnings"]
        .as_array()
        .map(|w| w.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    if message.is_empty() && !warnings.is_empty() {
        message = format!("the last backup has warnings: {}", warnings.join("; "));
    }
    BackupStatus {
        first_recoverability_point: first,
        last_successful_backup: last.clone(),
        last_archived_wal: last,
        last_failed_backup: last_failed,
        message,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quantities() {
        assert_eq!(bytes("1Gi"), Some(1 << 30));
        assert_eq!(bytes("1536Mi"), Some(1536 << 20));
        assert_eq!(bytes("2G"), Some(2_000_000_000));
        assert_eq!(bytes("1073741824"), Some(1 << 30));
        assert_eq!(bytes(""), None);
        assert_eq!(bytes("lots"), None);
        assert_eq!(bytes("-1Gi"), None);
    }

    #[test]
    fn small_settings() {
        let s = settings(1 << 30, 10 << 30);
        assert_eq!(s["innodb_buffer_pool_size"], "512M");
        assert_eq!(s["max_connections"], "128");
        assert_eq!(s["performance_schema"], "OFF");
        assert_eq!(s["temptable_max_ram"], "64M");
        assert_eq!(s["innodb_redo_log_capacity"], "640M");
        assert_eq!(s["sort_buffer_size"], "256K");
        // A small disk: the least redo.
        assert_eq!(
            settings(1 << 30, 1 << 30)["innodb_redo_log_capacity"],
            "64M"
        );
    }

    #[test]
    fn bigger_settings() {
        let s = settings(2 << 30, 10 << 30);
        assert_eq!(s["innodb_buffer_pool_size"], "1024M");
        assert_eq!(s["performance_schema"], "ON");
        let s = settings(8 << 30, 100 << 30);
        assert_eq!(s["innodb_buffer_pool_size"], "4864M");
        assert_eq!(s["max_connections"], "1024");
        assert_eq!(s["sort_buffer_size"], "2M");
        assert_eq!(s["innodb_redo_log_capacity"], "2048M");
        // Never more than 60% in the pool.
        for gi in 1..=64u64 {
            let pool: u64 = settings(gi << 30, 10 << 30)["innodb_buffer_pool_size"]
                .trim_end_matches('M')
                .parse()
                .unwrap();
            assert!(pool * MIB <= (gi << 30) * 60 / 100 && pool >= 128, "{gi}");
        }
    }

    #[test]
    fn cron() {
        assert_eq!(five_field("0 0 3 * * *"), "0 3 * * *");
        assert_eq!(five_field("0 3 * * *"), "0 3 * * *");
    }

    #[test]
    fn sql() {
        assert_eq!(literal("a'b\\c"), "'a''b\\\\c'");
        let q = app_statements("app", "p'w");
        assert_eq!(
            q[1],
            "CREATE USER IF NOT EXISTS 'app'@'%' IDENTIFIED BY 'p''w'"
        );
        assert_eq!(q[3], "GRANT ALL PRIVILEGES ON `app`.* TO 'app'@'%'");
        assert!(
            valid_user("app")
                && valid_user("app_1")
                && !valid_user("1app")
                && !valid_user("a-b")
                && !valid_user(&"a".repeat(33))
        );
    }

    fn job(name: &str, status: Value) -> Job {
        serde_json::from_value(
            json!({ "metadata": { "name": name, "uid": name }, "status": status }),
        )
        .unwrap()
    }

    #[test]
    fn jobs_as_backups() {
        let done = job(
            "backup-main-1",
            json!({ "startTime": "2026-11-09T09:00:00Z", "completionTime": "2026-11-09T09:01:00Z",
            "conditions": [{ "type": "Complete", "status": "True", "lastTransitionTime": "2026-11-09T09:01:00Z" }] }),
        );
        let v = job_status(&done);
        assert_eq!(
            (v["phase"].as_str(), v["stoppedAt"].as_str()),
            (Some("completed"), Some("2026-11-09T09:01:00Z"))
        );
        let failed = job(
            "backup-main-2",
            json!({ "startTime": "2026-11-10T09:00:00Z",
            "conditions": [{ "type": "Failed", "status": "True", "reason": "BackoffLimitExceeded", "message": "Job has reached the specified backoff limit", "lastTransitionTime": "2026-11-10T09:05:00Z" }] }),
        );
        let moco = json!({ "backup": { "time": "2026-11-09T09:00:30Z", "warnings": null } });
        let b = backup_status(
            &moco,
            &[done.clone(), failed.clone()],
            "backup-main",
            7,
            None,
        );
        assert_eq!(b.last_successful_backup, "2026-11-09T09:00:30Z");
        assert_eq!(b.first_recoverability_point, "2026-11-09T09:00:30Z");
        assert_eq!(b.last_failed_backup, "2026-11-10T09:05:00Z");
        assert!(
            b.message
                .starts_with("the last backup failed: Job has reached")
        );
        // The first one seen stays; a later success clears the failure.
        let before = BackupStatus {
            first_recoverability_point: "2026-11-01T00:00:00Z".into(),
            ..Default::default()
        };
        let later = json!({ "backup": { "time": "2026-11-11T09:00:30Z" } });
        let b = backup_status(
            &later,
            &[done.clone(), failed.clone()],
            "backup-main",
            30,
            Some(&before),
        );
        assert_eq!(
            (b.first_recoverability_point.as_str(), b.message.as_str()),
            ("2026-11-01T00:00:00Z", "")
        );
        // Kept 7 days: what's older is deleted, so the first moment moves up.
        let b = backup_status(&later, &[done, failed], "backup-main", 7, Some(&before));
        assert_eq!(b.first_recoverability_point, "2026-11-04T09:00:30Z");
        // No backup yet: MOCO's zero time.
        assert_eq!(
            backup_status(
                &json!({ "backup": { "time": "0001-01-01T00:00:00Z" } }),
                &[],
                "backup-main",
                7,
                None
            )
            .last_successful_backup,
            ""
        );
        let running = job(
            "backup-main-3",
            json!({ "startTime": "2026-11-10T09:00:00Z" }),
        );
        assert_eq!(
            backup_changes(&job_status(&running), "2026-11-01T00:00:00Z")
                .iter()
                .map(|e| e.2)
                .collect::<Vec<_>>(),
            ["BackupStarted"]
        );
    }
}
