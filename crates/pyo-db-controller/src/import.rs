//! Imports (`spec.import`): data copied into a Postgres or MySQL database
//! from another database (a URL) or from a dump file Pyo keeps.
//!
//! - **Postgres from a URL, as the database is made** (`bootstrap`):
//!   CloudNativePG's own import. The Cluster bootstraps with `initdb.import`
//!   of type "microservice": the one source database becomes the app
//!   database, owned by its owner (pg_dump and pg_restore without owners or
//!   privileges), from the external cluster `import-source` with the
//!   password from the import's Secret. Retried after a failure, the Cluster
//!   and its disks are made again.
//! - **Everything else** (MySQL; Postgres from a file; into a database that
//!   exists): a Job in the database's namespace, once the database is ready
//!   (and, for Postgres, its extensions are on). It refuses a database that
//!   isn't empty, then streams the data in as the app's login:
//!   - Postgres: `pg_dump` of the source, or the file (`pg_restore -f -` for
//!     a custom-format dump), without owners, privileges, publications and
//!     subscriptions, through `psql --single-transaction`: a failure leaves
//!     nothing behind. The tools are the database's own image's, so as new
//!     as the database (the API refuses a source newer than it).
//!   - MySQL: `mysqldump --single-transaction --routines --triggers --events
//!     --set-gtid-purged=OFF` of the source, or the file, through `mysql`,
//!     with DEFINER clauses removed (the app's login becomes the definer) and
//!     the statements that need SUPER (GTID_PURGED, SQL_LOG_BIN) dropped. A
//!     failure empties the database again.
//!   - A file is fetched from Pyo (`url`, with the bearer `token`), through
//!     curl in the MySQL image: streamed for MySQL, to a scratch volume
//!     the size of the file for Postgres.
//!
//! `status.import` says how the last one went; its Job's pod is
//! `pyo.dev/component=import`, so its output is in the database's logs.

use std::time::Duration;

use k8s_openapi::api::{batch::v1::Job, core::v1::Pod};
use kube::{
    Api, Resource, ResourceExt,
    api::{ListParams, LogParams, PostParams},
};
use serde_json::{Value, json};

use crate::{
    controller::{Kube, labels, secret_name},
    crd::{self, DatabaseStatus, Engine, ImportSpec, ImportStatus},
};

/// The import Job's pods: pyo-proto's `dbimport::POD_LABEL`, which
/// isolated projects let in.
pub const COMPONENT: &str = pyo_proto::dbimport::POD_LABEL.1;
/// The external cluster CloudNativePG imports from.
pub const SOURCE: &str = "import-source";
/// The status message while a failed bootstrap import's Cluster is made again.
const STARTING_AGAIN: &str = "Starting again";
const CNPG: &str = "postgresql.cnpg.io/v1";

/// The Job for one import: `<name>-import-<the id's end>`.
pub fn job_name(db: &str, id: &str) -> String {
    let tail: String = id
        .chars()
        .rev()
        .filter(|c| c.is_ascii_alphanumeric())
        .take(10)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    format!("{db}-import-{}", tail.to_ascii_lowercase())
}

/// Now, RFC3339.
fn now() -> String {
    crate::postgres::rfc3339(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs() as i64),
    )
}

/// libpq's sslmode for a TLS setting (empty: "prefer").
fn sslmode(tls: &str) -> &str {
    if tls.is_empty() { "prefer" } else { tls }
}

/// A Postgres import done by CloudNativePG as the Cluster is made.
pub fn bootstrap(db: &crd::Database) -> Option<&ImportSpec> {
    db.spec.import.as_ref().filter(|i| {
        i.bootstrap
            && i.source == "url"
            && db.spec.engine == Engine::Postgres
            && i.connection.is_some()
    })
}

/// The Cluster's bootstrap import and the external cluster it reads.
pub fn cluster_import(imp: &ImportSpec) -> (Value, Value) {
    let c = imp.connection.clone().unwrap_or_default();
    let import = json!({ "type": "microservice", "databases": [c.database], "source": { "externalCluster": SOURCE } });
    let mut params = json!({ "host": c.host, "port": c.port.to_string(), "user": c.user, "dbname": c.database, "sslmode": sslmode(&c.tls) });
    if c.tls.starts_with("verify") {
        params["sslrootcert"] = json!("system");
    }
    let external = json!({ "name": SOURCE, "connectionParameters": params, "password": { "name": imp.secret, "key": "password" } });
    (import, external)
}

/// The awk program between a Postgres dump and psql: what needs a
/// superuser or other roles goes (owners, privileges, publications and
/// subscriptions, security labels, comments on extensions), COPY data
/// passes untouched.
pub const PG_FILTER: &str = r#"copy { print; if ($0 == "\\.") copy = 0; next }
/^COPY .* FROM stdin;$/ { copy = 1; print; next }
/^(GRANT|REVOKE) / { next }
/^ALTER DEFAULT PRIVILEGES / { next }
/^ALTER .* OWNER TO / { next }
/^COMMENT ON EXTENSION / { next }
/^SET transaction_timeout / { next }
/^(CREATE|ALTER) (PUBLICATION|SUBSCRIPTION) / { next }
/^SECURITY LABEL / { next }
/^CREATE SCHEMA public;$/ { next }
{ print }
"#;

/// The sed program between a MySQL dump and mysql: DEFINER clauses go
/// (the login loading it becomes the definer), and so do the statements
/// only a superuser may run (GTID_PURGED, SQL_LOG_BIN) and the ones that
/// pick another database. Rows (INSERT lines) pass untouched.
pub const MYSQL_SED: &str = r#"/^INSERT INTO /b
s/DEFINER *= *(`[^`]+`|'[^']+'|[A-Za-z0-9_.%$-]+)@(`[^`]+`|'[^']+'|[A-Za-z0-9_.%$-]+)//g
/^SET @@SESSION\.SQL_LOG_BIN/d
/^SET @MYSQLDUMP_TEMP_LOG_BIN/d
/^SET @@GLOBAL\.GTID_PURGED/{
:a
/;[[:space:]]*$/!{
N
ba
}
d
}
/^CREATE DATABASE /d
/^USE `/d
"#;

const COMMON: &str = r#"set -o pipefail
step() { echo "pyo-import: $*"; }
fail() { printf '%s' "$*" | tail -c 1800 > /dev/termination-log; echo "pyo-import: failed: $*" >&2; exit 1; }
lasterr() { grep -v '^\s*$' "$@" 2>/dev/null | tail -n 3 | tr '\n' ' '; }
"#;

/// The import Job's script for an engine and source.
pub fn script(engine: Engine, imp: &ImportSpec) -> String {
    let file = imp.source == "file";
    match engine {
        Engine::Postgres => {
            let read = if file {
                let reader = match imp.format.as_str() {
                    "dump" => {
                        "pg_restore --no-owner --no-acl --no-publications --no-subscriptions --no-tablespaces --no-security-labels -f - /work/dump"
                    }
                    "sql.gz" => "gunzip -c /work/dump",
                    _ => "cat /work/dump",
                };
                format!(
                    r#"if [ "$FORMAT" = dump ] && [ "$(head -c 5 /work/dump)" != PGDMP ]; then fail "The file isn't a custom-format dump (pg_dump -Fc)."; fi
step "Loading the dump"
{reader} 2>/tmp/read.err | awk "$FILTER" | psqlt -1 -f - 2>/tmp/load.err
rc=("${{PIPESTATUS[@]}}")
cat /tmp/read.err /tmp/load.err >&2
[ "${{rc[0]}}" = 0 ] || fail "Reading the dump failed, so nothing was imported: $(lasterr /tmp/read.err)"
"#
                )
            } else {
                r#"step "Copying from $SRC_LABEL"
PGPASSWORD="$SRC_PASSWORD" pg_dump --no-owner --no-acl --no-publications --no-subscriptions --no-tablespaces --no-security-labels "$SRC" 2>/tmp/read.err | awk "$FILTER" | psqlt -1 -f - 2>/tmp/load.err
rc=("${PIPESTATUS[@]}")
cat /tmp/read.err /tmp/load.err >&2
[ "${rc[0]}" = 0 ] || fail "Reading the source failed, so nothing was imported: $(lasterr /tmp/read.err)"
"#
                .to_owned()
            };
            format!(
                r#"{COMMON}TGT="host=$TGT_HOST port=5432 dbname=app user=$TGT_USER"
psqlt() {{ PGPASSWORD="$TGT_PASSWORD" psql -X -q -v ON_ERROR_STOP=1 "$TGT" "$@"; }}
step "Checking the database is empty"
# A new pod's address reaches the network policy a few seconds late.
for i in $(seq 30); do n=$(psqlt -At -c "$OBJECTS_SQL" 2>&1) && break; sleep 2; done
case "$n" in ''|*[!0-9]*) fail "Couldn't connect to the database: $n" ;; esac
[ "$n" = 0 ] || fail "The database isn't empty: it has $n tables, views or functions. Import only into an empty database."
{read}[ "${{rc[2]}}" = 0 ] || fail "Loading failed, so nothing was imported: $(lasterr /tmp/load.err)"
step "Updating statistics"
psqlt -c "ANALYZE" >/dev/null 2>&1 || true
step "Done"
"#
            )
        }
        _ => {
            let read = if file {
                r#"step "Loading the dump"
curl -fsS --retry 5 --retry-delay 3 -H "Authorization: Bearer $DUMP_TOKEN" "$DUMP_URL" 2>/tmp/read.err | { if [ "$FORMAT" = sql.gz ]; then gunzip -c; else cat; fi; } | sed -E -f /tmp/strip.sed | mysqlt --default-character-set=utf8mb4 app 2>/tmp/load.err
rc=("${PIPESTATUS[@]}")
cat /tmp/read.err /tmp/load.err >&2
[ "${rc[0]}" = 0 ] || undo "Couldn't download the dump from Pyo: $(lasterr /tmp/read.err)"
[ "${rc[1]}" = 0 ] || undo "The file isn't gzip-compressed SQL: $(lasterr /tmp/load.err)"
"#
            } else {
                r#"cnf "$SRC_HOST" "$SRC_PORT" "$SRC_USER" "$SRC_PASSWORD" > /tmp/src.cnf
CA=""
case "$SRC_SSL" in VERIFY_*) for f in /etc/ssl/certs/ca-certificates.crt /etc/pki/tls/certs/ca-bundle.crt; do [ -f "$f" ] && CA="--ssl-ca=$f" && break; done ;; esac
step "Copying from $SRC_LABEL"
mysqldump --defaults-extra-file=/tmp/src.cnf --ssl-mode="$SRC_SSL" $CA --single-transaction --routines --triggers --events --set-gtid-purged=OFF --no-tablespaces --hex-blob --default-character-set=utf8mb4 --column-statistics=0 "$SRC_DB" 2>/tmp/read.err | cat | sed -E -f /tmp/strip.sed | mysqlt --default-character-set=utf8mb4 app 2>/tmp/load.err
rc=("${PIPESTATUS[@]}")
cat /tmp/read.err /tmp/load.err >&2
[ "${rc[0]}" = 0 ] || undo "Reading the source failed: $(lasterr /tmp/read.err)"
"#
            };
            format!(
                r#"{COMMON}esc() {{ printf '%s' "$1" | sed -e 's/\\/\\\\/g' -e 's/"/\\"/g'; }}
cnf() {{ printf '[client]\nhost="%s"\nport=%s\nuser="%s"\npassword="%s"\n' "$(esc "$1")" "$2" "$(esc "$3")" "$(esc "$4")"; }}
printf '%s' "$STRIP" > /tmp/strip.sed
cnf "$TGT_HOST" 3306 "$TGT_USER" "$TGT_PASSWORD" > /tmp/tgt.cnf
mysqlt() {{ mysql --defaults-extra-file=/tmp/tgt.cnf "$@"; }}
# Whatever was loaded goes: the database is left empty.
undo() {{ mysqlt -e "DROP DATABASE IF EXISTS app; CREATE DATABASE app" >&2 || true; fail "$1 Nothing was kept."; }}
step "Checking the database is empty"
# A new pod's address reaches the network policy a few seconds late.
for i in $(seq 30); do n=$(mysqlt -N -B -e "$OBJECTS_SQL" app 2>&1) && break; sleep 2; done
case "$n" in ''|*[!0-9]*) fail "Couldn't connect to the database: $n" ;; esac
[ "$n" = 0 ] || fail "The database isn't empty: it has $n tables, views, routines or events. Import only into an empty database."
{read}[ "${{rc[3]}}" = 0 ] || undo "Loading failed: $(lasterr /tmp/load.err)"
step "Done"
"#
            )
        }
    }
}

impl Kube<'_> {
    /// Before the Postgres Cluster is applied: a bootstrap import started
    /// again after it failed needs a new Cluster, so the old one and its
    /// disks go first. True while that's under way (apply nothing).
    pub async fn import_restart(&self, st: &mut DatabaseStatus) -> anyhow::Result<bool> {
        let Some(imp) = bootstrap(self.db) else {
            return Ok(false);
        };
        let name = self.name();
        match st.import.clone() {
            Some(p)
                if p.id == imp.id
                    && p.phase == crd::IMPORT_PENDING
                    && p.message == STARTING_AGAIN =>
            {
                let cluster = self.get(CNPG, "Cluster", &name).await?;
                let disks = self
                    .list("v1", "PersistentVolumeClaim")
                    .await?
                    .into_iter()
                    .any(|c| c.labels().get("cnpg.io/cluster") == Some(&name));
                if cluster.is_some() || disks {
                    return Ok(true);
                }
                st.import = Some(ImportStatus {
                    id: imp.id.clone(),
                    phase: crd::IMPORT_PENDING.into(),
                    message: "Making the database".into(),
                    ..Default::default()
                });
                Ok(false)
            }
            Some(p) if p.id != imp.id && p.phase == crd::IMPORT_FAILED => {
                self.remove(CNPG, "Cluster", &name).await?;
                self.event(
                    false,
                    "ImportRetried",
                    "Starting the import again: the database is made again".into(),
                )
                .await;
                st.import = Some(ImportStatus {
                    id: imp.id.clone(),
                    phase: crd::IMPORT_PENDING.into(),
                    message: STARTING_AGAIN.into(),
                    ..Default::default()
                });
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    /// The import's progress this pass. `serving`: the database takes
    /// connections (its primary is ready).
    pub async fn import(&self, st: &mut DatabaseStatus, serving: bool) -> anyhow::Result<()> {
        let Some(imp) = self.db.spec.import.clone() else {
            st.import = None;
            return Ok(());
        };
        let before = st.import.clone().filter(|s| s.id == imp.id);
        let mut cur = before.clone().unwrap_or(ImportStatus {
            id: imp.id.clone(),
            phase: crd::IMPORT_PENDING.into(),
            message: "Waiting for the database".into(),
            ..Default::default()
        });
        if cur.phase == crd::IMPORT_SUCCEEDED || cur.phase == crd::IMPORT_FAILED {
            st.import = Some(cur);
            return Ok(());
        }
        if bootstrap(self.db).is_some() {
            self.bootstrap_progress(&imp, &mut cur, serving).await?;
        } else {
            self.job_progress(&imp, &mut cur, serving, st).await?;
        }
        let was = before.as_ref().map(|b| b.phase.as_str()).unwrap_or("");
        if cur.phase != was {
            match cur.phase.as_str() {
                crd::IMPORT_RUNNING => {
                    self.event(false, "ImportStarted", "Import started".into())
                        .await
                }
                crd::IMPORT_SUCCEEDED => {
                    self.event(false, "ImportFinished", "Import finished".into())
                        .await
                }
                crd::IMPORT_FAILED => {
                    self.event(
                        true,
                        "ImportFailed",
                        format!("Import failed: {}", cur.message),
                    )
                    .await
                }
                _ => {}
            }
        }
        st.import = Some(cur);
        Ok(())
    }

    /// CloudNativePG's import: done once the primary serves; failed when
    /// its Job did.
    async fn bootstrap_progress(
        &self,
        imp: &ImportSpec,
        cur: &mut ImportStatus,
        serving: bool,
    ) -> anyhow::Result<()> {
        let name = self.name();
        let jobs: Vec<Job> = Api::<Job>::namespaced(self.ctx.client.clone(), &self.ns())
            .list(&ListParams::default().labels(&format!("cnpg.io/cluster={name}")))
            .await?
            .items;
        let host = imp
            .connection
            .as_ref()
            .map(|c| format!("{} on {}", c.database, c.host))
            .unwrap_or_default();
        if serving {
            cur.phase = crd::IMPORT_SUCCEEDED.into();
            cur.message = String::new();
            cur.finished_at = now();
            if cur.started_at.is_empty() {
                cur.started_at = cur.finished_at.clone();
            }
            return Ok(());
        }
        // CloudNativePG runs its Job up to 7 times, minutes apart: two
        // failed tries are enough to say so (Retry makes it again).
        let failed_tries = |j: &Job| j.status.as_ref().and_then(|s| s.failed).unwrap_or(0);
        if let Some(failed) = jobs
            .iter()
            .find(|j| crate::mysql::job_status(j)["phase"] == "failed" || failed_tries(j) >= 2)
        {
            cur.phase = crd::IMPORT_FAILED.into();
            let why = self.cnpg_error(&failed.name_any()).await;
            let why = match &imp.connection {
                Some(c) if why.contains("connection to server") => {
                    pyo_proto::dbimport::explain_connect_error(
                        "postgres",
                        &c.host,
                        c.port as u16,
                        &c.database,
                        &why,
                    )
                    .1
                }
                _ => why,
            };
            cur.message = if why.is_empty() {
                format!("Copying {host} failed. The database's logs have the details.")
            } else {
                format!("Copying {host} failed: {why}")
            };
            cur.finished_at = now();
            return Ok(());
        }
        if let Some(j) = jobs.first() {
            cur.phase = crd::IMPORT_RUNNING.into();
            cur.message = format!("Copying {host}");
            if cur.started_at.is_empty() {
                cur.started_at = crate::mysql::job_status(j)["startedAt"]
                    .as_str()
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned)
                    .unwrap_or_else(now);
            }
        } else {
            cur.message = "Making the database".into();
        }
        Ok(())
    }

    /// The last error CloudNativePG's import Job logged (its lines are JSON).
    async fn cnpg_error(&self, job: &str) -> String {
        let pods = Api::<Pod>::namespaced(self.ctx.client.clone(), &self.ns());
        let Ok(list) = pods
            .list(&ListParams::default().labels(&format!("job-name={job}")))
            .await
        else {
            return String::new();
        };
        for p in list.items.iter().rev() {
            let Ok(out) = pods
                .logs(
                    &p.name_any(),
                    &LogParams {
                        tail_lines: Some(200),
                        ..Default::default()
                    },
                )
                .await
            else {
                continue;
            };
            if let Some(why) = cnpg_last_error(&out) {
                return why;
            }
        }
        String::new()
    }

    /// The import Job: made once the database is ready, then followed.
    async fn job_progress(
        &self,
        imp: &ImportSpec,
        cur: &mut ImportStatus,
        serving: bool,
        st: &DatabaseStatus,
    ) -> anyhow::Result<()> {
        let name = job_name(&self.name(), &imp.id);
        let api: Api<Job> = Api::namespaced(self.ctx.client.clone(), &self.ns());
        let Some(job) = api.get_opt(&name).await? else {
            if !serving {
                cur.message = "Waiting for the database".into();
                return Ok(());
            }
            let wanted = self
                .db
                .spec
                .postgres
                .as_ref()
                .map(|p| p.extensions.clone())
                .unwrap_or_default();
            let waiting: Vec<&str> = wanted
                .iter()
                .filter(|w| {
                    !st.extensions
                        .iter()
                        .any(|e| &e.name == *w && e.state == "enabled")
                })
                .map(String::as_str)
                .collect();
            if self.db.spec.engine == Engine::Postgres && !waiting.is_empty() {
                cur.message = format!("Waiting for {} to be on", waiting.join(", "));
                return Ok(());
            }
            let job: Job = serde_json::from_value(self.import_job(imp, &name))?;
            match api.create(&PostParams::default(), &job).await {
                Err(kube::Error::Api(e)) if e.code == 409 => {}
                r => {
                    r?;
                }
            }
            cur.phase = crd::IMPORT_RUNNING.into();
            cur.message = "Starting".into();
            cur.started_at = now();
            return Ok(());
        };
        let status = crate::mysql::job_status(&job);
        let pods = Api::<Pod>::namespaced(self.ctx.client.clone(), &self.ns());
        let list = pods
            .list(&ListParams::default().labels(&format!("job-name={name}")))
            .await
            .map(|l| l.items)
            .unwrap_or_default();
        if cur.started_at.is_empty() {
            cur.started_at = status["startedAt"]
                .as_str()
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
                .unwrap_or_else(now);
        }
        match status["phase"].as_str() {
            Some("completed") => {
                cur.phase = crd::IMPORT_SUCCEEDED.into();
                cur.message = String::new();
                cur.finished_at = status["stoppedAt"]
                    .as_str()
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned)
                    .unwrap_or_else(now);
            }
            Some("failed") => {
                cur.phase = crd::IMPORT_FAILED.into();
                let why = list
                    .iter()
                    .find_map(termination_message)
                    .unwrap_or_default();
                cur.message = if why.is_empty() {
                    format!(
                        "The import stopped: {}",
                        status["error"].as_str().unwrap_or("")
                    )
                } else {
                    why
                };
                cur.finished_at = status["stoppedAt"]
                    .as_str()
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned)
                    .unwrap_or_else(now);
            }
            _ => {
                cur.phase = crd::IMPORT_RUNNING.into();
                // The step it's on, from its output.
                let mut step = None;
                for p in &list {
                    let waiting_for_fetch = p
                        .status
                        .as_ref()
                        .and_then(|s| s.init_container_statuses.as_ref())
                        .is_some_and(|c| {
                            c.iter()
                                .any(|c| c.state.as_ref().is_some_and(|s| s.running.is_some()))
                        });
                    if waiting_for_fetch {
                        step = Some("Downloading the dump".to_owned());
                        continue;
                    }
                    if let Ok(out) = pods
                        .logs(
                            &p.name_any(),
                            &LogParams {
                                container: Some("import".into()),
                                tail_lines: Some(100),
                                ..Default::default()
                            },
                        )
                        .await
                    {
                        step = out
                            .lines()
                            .rev()
                            .find_map(|l| l.strip_prefix("pyo-import: "))
                            .filter(|l| !l.starts_with("failed"))
                            .map(str::to_owned)
                            .or(step);
                    }
                }
                if let Some(s) = step {
                    cur.message = s;
                }
            }
        }
        Ok(())
    }

    /// The import Job, as described at the top.
    pub fn import_job(&self, imp: &ImportSpec, name: &str) -> Value {
        let engine = self.db.spec.engine;
        let file = imp.source == "file";
        let mut lbl = labels(self.db, COMPONENT);
        lbl["pyo.dev/import-id"] = json!(
            imp.id
                .chars()
                .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
                .take(63)
                .collect::<String>()
        );
        let key = |secret: &str, k: &str| json!({ "secretKeyRef": { "name": secret, "key": k } });
        let plain = |k: &str, v: String| json!({ "name": k, "value": v });
        let creds = secret_name(self.db);
        let pg = engine == Engine::Postgres;
        let host = if pg {
            self.svc_host(&format!("{}-rw", self.name()))
        } else {
            self.svc_host(&format!("{}-primary", self.name()))
        };
        let mut env = vec![
            plain("TGT_HOST", host),
            json!({ "name": "TGT_USER", "valueFrom": key(&creds, "username") }),
            json!({ "name": "TGT_PASSWORD", "valueFrom": key(&creds, "password") }),
            plain(
                "OBJECTS_SQL",
                if pg {
                    pyo_proto::dbimport::PG_OBJECTS_SQL.to_owned()
                } else {
                    pyo_proto::dbimport::MYSQL_OBJECTS_SQL.to_owned()
                },
            ),
            plain("FORMAT", imp.format.clone()),
        ];
        env.push(if pg {
            plain("FILTER", PG_FILTER.to_owned())
        } else {
            plain("STRIP", MYSQL_SED.to_owned())
        });
        if file {
            env.push(json!({ "name": "DUMP_URL", "valueFrom": key(&imp.secret, "url") }));
            env.push(json!({ "name": "DUMP_TOKEN", "valueFrom": key(&imp.secret, "token") }));
        } else if let Some(c) = &imp.connection {
            env.push(plain("SRC_LABEL", format!("{} on {}", c.database, c.host)));
            env.push(json!({ "name": "SRC_PASSWORD", "valueFrom": key(&imp.secret, "password") }));
            if pg {
                let conn = pyo_proto::dbimport::Connection {
                    host: c.host.clone(),
                    port: c.port as u16,
                    user: c.user.clone(),
                    database: c.database.clone(),
                    tls: c.tls.clone(),
                };
                env.push(plain("SRC", pyo_proto::dbimport::pg_conninfo(&conn)));
            } else {
                for (k, v) in [
                    ("SRC_HOST", c.host.clone()),
                    ("SRC_PORT", c.port.to_string()),
                    ("SRC_USER", c.user.clone()),
                    ("SRC_DB", c.database.clone()),
                    (
                        "SRC_SSL",
                        pyo_proto::dbimport::mysql_ssl_mode(&c.tls).to_owned(),
                    ),
                ] {
                    env.push(plain(k, v));
                }
            }
        }
        let image = if pg {
            self.pg_image()
        } else {
            self.ctx.images.mysql_84.clone()
        };
        let mut container = json!({
            "name": COMPONENT,
            "image": image,
            "command": ["bash", "-c", script(engine, imp)],
            "env": env,
            "terminationMessagePolicy": "FallbackToLogsOnError",
            "resources": { "requests": { "cpu": "100m", "memory": "128Mi" }, "limits": { "memory": "1Gi" } },
            "securityContext": { "allowPrivilegeEscalation": false },
        });
        let mut pod = json!({
            "restartPolicy": "Never",
            "automountServiceAccountToken": false,
            "containers": [],
        });
        if pg && file {
            // Fetched whole first: pg_restore needs a file. curl is in the
            // MySQL image (the Postgres one has none).
            let gi = ((imp.size.max(0) as f64 * 1.1) / f64::from(1u32 << 30)).ceil() as i64 + 1;
            let mut claim = json!({ "accessModes": ["ReadWriteOnce"], "resources": { "requests": { "storage": format!("{gi}Gi") } } });
            if !self.db.spec.storage_class.is_empty() {
                claim["storageClassName"] = json!(self.db.spec.storage_class);
            }
            pod["volumes"] = json!([{ "name": "work", "ephemeral": { "volumeClaimTemplate": { "metadata": { "labels": labels(self.db, COMPONENT) }, "spec": claim } } }]);
            pod["securityContext"] = json!({ "fsGroup": 26 });
            pod["initContainers"] = json!([{
                "name": "fetch",
                "image": self.ctx.images.mysql_84,
                "command": ["bash", "-c", "set -o pipefail; echo 'pyo-import: Downloading the dump'; curl -fsS --retry 5 --retry-delay 3 -H \"Authorization: Bearer $DUMP_TOKEN\" -o /work/dump \"$DUMP_URL\" 2>/tmp/err || { e=$(tail -n 2 /tmp/err | tr '\\n' ' '); echo \"Couldn't download the dump from Pyo: $e\" | tee /dev/termination-log >&2; exit 1; }"],
                "env": [
                    { "name": "DUMP_URL", "valueFrom": key(&imp.secret, "url") },
                    { "name": "DUMP_TOKEN", "valueFrom": key(&imp.secret, "token") },
                ],
                "volumeMounts": [{ "name": "work", "mountPath": "/work" }],
                "terminationMessagePolicy": "FallbackToLogsOnError",
                "resources": { "requests": { "cpu": "50m", "memory": "64Mi" }, "limits": { "memory": "256Mi" } },
                "securityContext": { "allowPrivilegeEscalation": false },
            }]);
            container["volumeMounts"] = json!([{ "name": "work", "mountPath": "/work" }]);
        }
        pod["containers"] = json!([container]);
        json!({
            "apiVersion": "batch/v1", "kind": "Job",
            "metadata": { "name": name, "namespace": self.ns(), "labels": lbl, "ownerReferences": [self.db.controller_owner_ref(&())] },
            "spec": {
                "backoffLimit": 0,
                // A day at most; kept a day after, for its logs.
                "activeDeadlineSeconds": 86_400,
                "ttlSecondsAfterFinished": 86_400,
                "template": { "metadata": { "labels": lbl }, "spec": pod },
            },
        })
    }
}

/// Why a container stopped, when it failed: its termination message.
fn termination_message(p: &Pod) -> Option<String> {
    let st = p.status.as_ref()?;
    st.init_container_statuses
        .iter()
        .flatten()
        .chain(st.container_statuses.iter().flatten())
        .find_map(|c| {
            let t = c.state.as_ref()?.terminated.as_ref()?;
            (t.exit_code != 0)
                .then(|| t.message.clone().unwrap_or_default().trim().to_owned())
                .filter(|m| !m.is_empty())
        })
}

/// The last error in CloudNativePG's JSON log lines: what pg_dump or
/// pg_restore said, else its own message and error.
pub fn cnpg_last_error(out: &str) -> Option<String> {
    let tool = out.lines().rev().find_map(|l| {
        let v: Value = serde_json::from_str(l.trim()).ok()?;
        let logger = v["logger"].as_str().unwrap_or("");
        let msg = v["msg"].as_str().unwrap_or("");
        (["pg_dump", "pg_restore", "psql"].contains(&logger)
            && (msg.contains("error:") || msg.contains("FATAL")))
        .then(|| {
            let m = msg.split_once("error: ").map_or(msg, |(_, r)| r);
            m.chars().take(600).collect::<String>()
        })
    });
    if tool.is_some() {
        return tool;
    }
    out.lines().rev().find_map(|l| {
        let v: Value = serde_json::from_str(l.trim()).ok()?;
        if v["level"] != "error" {
            return None;
        }
        let msg = v["msg"].as_str().unwrap_or("");
        let err = v["error"].as_str().or(v["err"].as_str()).unwrap_or("");
        let s = match (msg.is_empty(), err.is_empty()) {
            (_, true) => msg.to_owned(),
            (true, false) => err.to_owned(),
            _ => format!("{msg}: {err}"),
        };
        (!s.is_empty()).then(|| s.chars().take(600).collect())
    })
}

/// How long to wait before the next pass while an import runs.
pub fn requeue(st: &DatabaseStatus, otherwise: Duration) -> Duration {
    match st.import.as_ref().map(|i| i.phase.as_str()) {
        Some(crd::IMPORT_PENDING | crd::IMPORT_RUNNING) => otherwise.min(Duration::from_secs(5)),
        _ => otherwise,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn import_pods_carry_the_label_projects_let_in() {
        assert_eq!(
            crate::controller::LABEL_COMPONENT,
            pyo_proto::dbimport::POD_LABEL.0
        );
        assert_eq!(COMPONENT, pyo_proto::dbimport::POD_LABEL.1);
    }

    #[test]
    fn job_names() {
        assert_eq!(job_name("main", "imp_ab12cd34ef"), "main-import-ab12cd34ef");
        assert_eq!(job_name("main", "imp_x"), "main-import-impx");
        assert!(job_name(&"a".repeat(40), "imp_0123456789abcdef").len() <= 63);
    }

    #[test]
    fn errors_from_cloudnativepg() {
        let out = "{\"level\":\"info\",\"msg\":\"starting\"}\n{\"level\":\"error\",\"msg\":\"Error while bootstrapping data directory\",\"error\":\"while executing pg_dump: exit status 1\"}\n{\"level\":\"info\",\"msg\":\"x\"}\n";
        assert_eq!(
            cnpg_last_error(out).unwrap(),
            "Error while bootstrapping data directory: while executing pg_dump: exit status 1"
        );
        assert_eq!(cnpg_last_error("plain\n"), None);
        let dump = "{\"level\":\"info\",\"logger\":\"pg_dump\",\"msg\":\"pg_dump: error: connection to server at \\\"db\\\" failed: FATAL:  password authentication failed for user \\\"u\\\"\",\"pipe\":\"stderr\"}\n{\"level\":\"error\",\"msg\":\"Error while bootstrapping data directory\",\"error\":\"x\"}\n";
        assert_eq!(
            cnpg_last_error(dump).unwrap(),
            "connection to server at \"db\" failed: FATAL:  password authentication failed for user \"u\""
        );
    }

    #[test]
    fn scripts() {
        let url = ImportSpec {
            id: "imp_1".into(),
            source: "url".into(),
            secret: "s".into(),
            ..Default::default()
        };
        let file = ImportSpec {
            source: "file".into(),
            format: "dump".into(),
            ..url.clone()
        };
        let pg = script(Engine::Postgres, &url);
        assert!(
            pg.contains("pg_dump --no-owner --no-acl")
                && pg.contains("psqlt -1 -f -")
                && pg.contains("${rc[2]}")
        );
        let pf = script(Engine::Postgres, &file);
        assert!(pf.contains("pg_restore --no-owner --no-acl") && pf.contains("PGDMP"));
        let my = script(Engine::Mysql, &url);
        assert!(
            my.contains(
                "--single-transaction --routines --triggers --events --set-gtid-purged=OFF"
            ) && my.contains("DROP DATABASE IF EXISTS app")
        );
        let mf = script(
            Engine::Mysql,
            &ImportSpec {
                format: "sql.gz".into(),
                ..file
            },
        );
        assert!(mf.contains("curl -fsS") && mf.contains("gunzip -c"));
        // Each pipeline's last stage is the one whose status is checked.
        assert!(my.contains("2>/tmp/read.err | cat | sed") && my.contains("${rc[3]}"));
        assert!(mf.contains("2>/tmp/read.err | { if"));
    }

    #[test]
    fn the_cloudnativepg_import() {
        let imp = ImportSpec {
            id: "imp_1".into(),
            source: "url".into(),
            bootstrap: true,
            secret: "main-import".into(),
            connection: Some(crd::ImportConnection {
                host: "db.example.com".into(),
                port: 5432,
                user: "u".into(),
                database: "shop".into(),
                tls: "verify-full".into(),
            }),
            ..Default::default()
        };
        let (import, external) = cluster_import(&imp);
        assert_eq!(
            import,
            json!({ "type": "microservice", "databases": ["shop"], "source": { "externalCluster": "import-source" } })
        );
        assert_eq!(external["connectionParameters"]["sslmode"], "verify-full");
        assert_eq!(external["connectionParameters"]["sslrootcert"], "system");
        assert_eq!(
            external["password"],
            json!({ "name": "main-import", "key": "password" })
        );
    }
}
