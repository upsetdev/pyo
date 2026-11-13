//! The Database resource: the contract between Pyo (which writes the spec
//! through its agent and reads the status) and this controller (which runs
//! it). deploy/crd.yaml is its definition, with the validation rules.

use kube::CustomResource;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(CustomResource, Deserialize, Serialize, Clone, Debug, JsonSchema)]
#[kube(
    group = "pyo.dev",
    version = "v1alpha1",
    kind = "Database",
    namespaced,
    status = "DatabaseStatus",
    shortname = "pyodb"
)]
#[serde(rename_all = "camelCase")]
pub struct DatabaseSpec {
    /// "postgres", "valkey" or "mysql". Can't change.
    pub engine: Engine,
    /// The major version, e.g. "18" for Postgres, "8.1" for Valkey or "8.4"
    /// for MySQL; empty means the controller's default.
    #[serde(default)]
    pub version: String,
    /// Postgres 1 to 9, Valkey 1 to 5, MySQL 1, 3 or 5 (and it only grows).
    /// More than one runs replicas with automatic failover, spread over
    /// different servers when there are enough.
    pub instances: i32,
    /// Storage per instance, e.g. "10Gi".
    pub storage: String,
    #[serde(default)]
    pub storage_class: String,
    /// CPU and memory per instance, e.g. "500m" and "1Gi". Memory is also the
    /// limit; MySQL needs at least 1Gi and sizes its buffers from it.
    #[serde(default)]
    pub cpu: String,
    #[serde(default)]
    pub memory: String,
    /// The memory set aside for each instance (its request), when less than
    /// `memory`, e.g. "128Mi" for a small database that holds far less than
    /// it may use. Empty: `memory`.
    #[serde(default)]
    pub memory_request: String,
    /// Holds username, password and (Valkey) token and readonly-token. Pyo
    /// writes it so it can seal them; when it doesn't exist the controller
    /// makes one. Default "<name>-credentials".
    #[serde(default)]
    pub credentials_secret: String,
    #[serde(default)]
    pub postgres: Option<PostgresSpec>,
    #[serde(default)]
    pub valkey: Option<ValkeySpec>,
    /// Backups to S3: Postgres archives WAL continuously and takes base
    /// backups; MySQL takes a full dump and the binlogs since the last one on
    /// the schedule.
    #[serde(default)]
    pub backup: Option<BackupSpec>,
    /// Made from another database's backups at creation (Postgres and
    /// MySQL). Can't change.
    #[serde(default)]
    pub restore: Option<RestoreSpec>,
    /// The REST API (Valkey only).
    #[serde(default)]
    pub rest: bool,
    /// Instances that only serve reads and never become the primary, apart
    /// from the replicas above. Missing or 0 instances: none.
    #[serde(default)]
    pub read_replicas: Option<ReadReplicasSpec>,
    /// Data copied in from another database or a dump file (Postgres and
    /// MySQL): a new `id` runs it (again). See import.rs.
    #[serde(default)]
    pub import: Option<ImportSpec>,
}

/// An import into the database.
#[derive(Deserialize, Serialize, Clone, Debug, Default, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ImportSpec {
    /// A new id starts a new import; the status says how the last one went.
    pub id: String,
    /// "url": copied from another database (`connection`); "file": a dump
    /// fetched from Pyo (`format`, `size`).
    pub source: String,
    /// Postgres from a URL, as the database is made: CloudNativePG's own
    /// import (one database, "microservice"). Otherwise a Job copies it in
    /// once the database is ready.
    #[serde(default)]
    pub bootstrap: bool,
    /// A Secret beside it: `password` (url), or `url` and `token` (file:
    /// where the dump is fetched, with that bearer token).
    pub secret: String,
    #[serde(default)]
    pub connection: Option<ImportConnection>,
    /// File: "sql", "sql.gz" or "dump" (Postgres's custom format).
    #[serde(default)]
    pub format: String,
    /// File: its size in bytes, for the space it's fetched to.
    #[serde(default)]
    pub size: i64,
}

/// The database copied from, without its password.
#[derive(Deserialize, Serialize, Clone, Debug, Default, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ImportConnection {
    pub host: String,
    pub port: i32,
    pub user: String,
    pub database: String,
    /// "disable", "prefer" (the default), "require", "verify-ca" or "verify-full".
    #[serde(default)]
    pub tls: String,
}

#[derive(Deserialize, Serialize, Clone, Debug, Default, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ReadReplicasSpec {
    /// Postgres 1 to 5, Valkey 1 to 4; 0 turns them off. Same size and
    /// storage as the database.
    #[serde(default)]
    pub instances: i32,
}

impl DatabaseSpec {
    /// How many read replicas it has (0: off).
    pub fn read_instances(&self) -> i32 {
        self.read_replicas.as_ref().map_or(0, |r| r.instances)
    }
}

#[derive(Deserialize, Serialize, Clone, Copy, Debug, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Engine {
    Postgres,
    Valkey,
    Mysql,
}

#[derive(Deserialize, Serialize, Clone, Debug, Default, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PostgresSpec {
    /// The database and its owner made at creation (default "app" and "app").
    #[serde(default)]
    pub database: String,
    #[serde(default)]
    pub owner: String,
    /// PgBouncer in front of the primary (transaction mode).
    #[serde(default)]
    pub pooler: bool,
    /// postgresql.conf settings.
    #[serde(default)]
    pub parameters: std::collections::BTreeMap<String, String>,
    /// Extensions Pyo turns on (extensions.json's "managed" ones).
    #[serde(default)]
    pub extensions: Vec<String>,
}

#[derive(Deserialize, Serialize, Clone, Debug, Default, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ValkeySpec {
    /// Modules loaded on every instance (extensions.json's "kv" ones, e.g.
    /// "valkey-json"). Changing them restarts the instances, replicas first.
    #[serde(default)]
    pub modules: Vec<String>,
}

/// An S3-compatible bucket. The controller keeps each database under
/// `<path>/<namespace>/<name>/` in it.
#[derive(Deserialize, Serialize, Clone, Debug, Default, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Destination {
    #[serde(rename = "endpointURL")]
    pub endpoint_url: String,
    pub bucket: String,
    #[serde(default)]
    pub path: String,
    #[serde(default)]
    pub region: String,
    /// A Secret in the Database's namespace with accessKeyId and
    /// secretAccessKey.
    pub credentials_secret: String,
}

#[derive(Deserialize, Serialize, Clone, Debug, Default, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct BackupSpec {
    #[serde(default)]
    pub enabled: bool,
    pub destination: Destination,
    /// How long backups and WAL are kept: 1 to 90 days (7).
    #[serde(default)]
    pub retention_days: i32,
    /// Six-field cron (seconds first) for base backups ("0 0 3 * * *").
    #[serde(default)]
    pub base_backup_schedule: String,
}

#[derive(Deserialize, Serialize, Clone, Debug, Default, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RestoreSpec {
    pub from_backup: RestoreSource,
    /// RFC3339; empty restores everything archived.
    #[serde(default)]
    pub target_time: String,
}

#[derive(Deserialize, Serialize, Clone, Debug, Default, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RestoreSource {
    #[serde(flatten)]
    pub destination: Destination,
    /// The Database the backups are of.
    pub source_name: String,
    /// Its namespace; default this Database's.
    #[serde(default)]
    pub source_namespace: String,
}

pub const DEFAULT_RETENTION_DAYS: i32 = 7;
pub const DEFAULT_BACKUP_SCHEDULE: &str = "0 0 3 * * *";

/// The most instances a Database has: a primary and 8 replicas (Postgres).
pub const MAX_INSTANCES: i32 = 9;
/// The most a Valkey cache has: a primary and 4 replicas.
pub const MAX_VALKEY_INSTANCES: i32 = 5;
/// The most read replicas a Postgres database has.
pub const MAX_READ_REPLICAS: i32 = 5;
/// The most read replicas a Valkey cache has.
pub const MAX_VALKEY_READ_REPLICAS: i32 = 4;
/// The instance counts MOCO runs: a primary alone, or with 2 or 4
/// semi-synchronous replicas.
pub const MYSQL_INSTANCES: [i32; 3] = [1, 3, 5];
/// The least memory a MySQL instance gets, in bytes.
pub const MYSQL_MIN_MEMORY: u64 = 1 << 30;

pub const READY: &str = "Ready";
/// It serves, but not every instance is ready.
pub const DEGRADED: &str = "Degraded";
pub const CREATING: &str = "Creating";
pub const FAILED: &str = "Failed";

#[derive(Deserialize, Serialize, Clone, Debug, Default, PartialEq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct DatabaseStatus {
    #[serde(default)]
    pub phase: String,
    /// What's happening or what's wrong, in plain words.
    #[serde(default)]
    pub message: String,
    #[serde(default)]
    pub observed_generation: i64,
    #[serde(default)]
    pub instances: i32,
    #[serde(default)]
    pub ready_instances: i32,
    /// The instance taking writes.
    #[serde(default)]
    pub primary: String,
    #[serde(default)]
    pub credentials_secret: String,
    #[serde(default)]
    pub endpoints: Endpoints,
    /// Each instance and the server it runs on, the primary first.
    #[serde(default)]
    pub members: Vec<Member>,
    /// Backups (Postgres and MySQL with backup on).
    #[serde(default)]
    pub backup: Option<BackupStatus>,
    /// Primary changes the controller has seen.
    #[serde(default)]
    pub failovers: i32,
    /// The read replicas (when on).
    #[serde(default)]
    pub read_replicas: Option<ReadReplicasStatus>,
    /// Each extension (Postgres, extensions.rs) or module (Valkey,
    /// modules.rs) being turned on or off, or on.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extensions: Vec<ExtensionStatus>,
    /// The last import (spec.import).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub import: Option<ImportStatus>,
}

/// How an import is going: "Pending" (waiting for the database), "Running",
/// "Succeeded" or "Failed"; `message` is the step or what went wrong.
#[derive(Deserialize, Serialize, Clone, Debug, Default, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ImportStatus {
    pub id: String,
    pub phase: String,
    #[serde(default)]
    pub message: String,
    /// RFC3339.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub started_at: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub finished_at: String,
}

pub const IMPORT_PENDING: &str = "Pending";
pub const IMPORT_RUNNING: &str = "Running";
pub const IMPORT_SUCCEEDED: &str = "Succeeded";
pub const IMPORT_FAILED: &str = "Failed";

/// An extension's state: "enabled", "pending" (being made), "restarting"
/// (its library loads at the next restart), "disabling" or "failed"; and
/// why, in plain words.
#[derive(Deserialize, Serialize, Clone, Debug, Default, PartialEq, JsonSchema)]
pub struct ExtensionStatus {
    pub name: String,
    pub state: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub message: String,
}

#[derive(Deserialize, Serialize, Clone, Debug, Default, PartialEq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ReadReplicasStatus {
    #[serde(default)]
    pub ready: i32,
    /// By name.
    #[serde(default)]
    pub members: Vec<ReadMember>,
}

/// One read replica: its pod, its server, and how far behind the primary it is.
#[derive(Deserialize, Serialize, Clone, Debug, Default, PartialEq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ReadMember {
    pub name: String,
    /// The server (node) it runs on; empty while unscheduled.
    pub node: String,
    pub ready: bool,
    /// Seconds behind the primary; missing when it isn't replicating or
    /// can't be asked.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lag_seconds: Option<f64>,
}

/// Times are RFC3339. Every field is always written, so a merge patch
/// clears what no longer applies.
#[derive(Deserialize, Serialize, Clone, Debug, Default, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct BackupStatus {
    /// The earliest time a restore can go back to.
    #[serde(default)]
    pub first_recoverability_point: String,
    #[serde(default)]
    pub last_successful_backup: String,
    /// When the last WAL file was archived (MySQL: the binlogs up to the
    /// last backup, so the same as lastSuccessfulBackup).
    #[serde(default)]
    pub last_archived_wal: String,
    #[serde(default)]
    pub last_failed_backup: String,
    /// What's wrong, in plain words; empty when all is well.
    #[serde(default)]
    pub message: String,
}

/// One instance: its pod, whether it takes writes, and its server.
#[derive(Deserialize, Serialize, Clone, Debug, Default, PartialEq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Member {
    pub name: String,
    /// "primary" or "replica".
    pub role: String,
    pub ready: bool,
    /// The server (node) it runs on; empty while unscheduled.
    pub node: String,
    /// Seconds a replica is behind the primary (MySQL); missing for the
    /// primary and when it isn't replicating or can't be asked.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lag_seconds: Option<f64>,
}

/// In-cluster addresses (host:port, or a URL for REST).
#[derive(Deserialize, Serialize, Clone, Debug, Default, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Endpoints {
    /// Reads and writes.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub primary: String,
    /// Reads only.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub replicas: String,
    /// PgBouncer in front of the primary (Postgres).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub pooler: String,
    /// Answers which instance is primary (Valkey with replicas).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub sentinel: String,
    /// The REST API's URL (Valkey).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub rest: String,
    /// Reads only, on the read replicas (through PgBouncer for Postgres).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub read: String,
}
