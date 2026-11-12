//! Before an import into a built-in database (CheckDatabaseImport): a short
//! Job in the team's databases namespace connects to the source, as the
//! import will, and to the database to import into, which must be empty.
//! It prints what it found as `PYO <key> <value>` lines; their meaning, and
//! a failed connection in plain words, come back in the result's `data`.
//! The source's password reaches the Job in a Secret deleted afterwards.

use std::time::Duration;

use k8s_openapi::api::{
    batch::v1::Job,
    core::v1::{Pod, Secret},
};
use kube::{
    Api, Client, ResourceExt,
    api::{DeleteParams, ListParams, LogParams, Patch, PatchParams, PostParams},
};
use pyo_proto::{
    CommandResult,
    dbimport::{self, ImportCheck},
};
use serde_json::{Value, json};

use crate::MANAGER;

const LABEL: &str = "pyo/import-check";

pub async fn check(client: &Client, spec: ImportCheck) -> CommandResult {
    match run(client, &spec).await {
        Ok(found) => CommandResult {
            ok: true,
            message: Some("Checked".into()),
            logs: vec![],
            data: Some(found),
        },
        Err(e) => crate::failed(format!("{e:#}")),
    }
}

fn script(engine: &str) -> String {
    match engine {
        "postgres" => format!(
            r#"set -u
q() {{ PGPASSWORD="$1" psql -X -At -v ON_ERROR_STOP=1 "$2" -c "$3" 2>&1; }}
one() {{ printf '%s' "$1" | tr '\n' ' '; }}
if [ -n "${{SRC:-}}" ]; then
  if ! out=$(q "$SRC_PASSWORD" "$SRC" "SELECT current_setting('server_version_num') || ' ' || pg_database_size(current_database())"); then
    echo "PYO source-error $(one "$out")"; exit 0
  fi
  echo "PYO source $out"
  out=$(q "$SRC_PASSWORD" "$SRC" "SELECT coalesce(string_agg(extname, ',' ORDER BY extname), '') FROM pg_extension WHERE extname <> 'plpgsql'") && echo "PYO extensions $out"
  out=$(q "$SRC_PASSWORD" "$SRC" "SELECT count(*) FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace WHERE c.relkind IN ('r', 'p', 'v', 'm', 'S') AND n.nspname NOT IN ('pg_catalog', 'information_schema') AND n.nspname NOT LIKE 'pg\_%' AND NOT has_table_privilege(c.oid, 'SELECT')") && echo "PYO unreadable $out"
fi
if [ -n "${{TGT_HOST:-}}" ]; then
  TGT="host=$TGT_HOST port=5432 dbname=app user=$TGT_USER connect_timeout=15"
  # A new pod's address reaches the network policy a few seconds late.
  for i in $(seq 15); do out=$(q "$TGT_PASSWORD" "$TGT" "{objects}") && break; out="FAILED $out"; sleep 2; done
  case "$out" in FAILED*) echo "PYO target-error $(one "${{out#FAILED }}")"; exit 0 ;; esac
  echo "PYO target-objects $out"
fi
echo "PYO done"
"#,
            objects = dbimport::PG_OBJECTS_SQL.replace('"', "\\\"")
        ),
        _ => format!(
            r#"set -u
esc() {{ printf '%s' "$1" | sed -e 's/\\/\\\\/g' -e 's/"/\\"/g'; }}
cnf() {{ printf '[client]\nhost="%s"\nport=%s\nuser="%s"\npassword="%s"\n' "$(esc "$1")" "$2" "$(esc "$3")" "$(esc "$4")"; }}
one() {{ printf '%s' "$1" | tr '\n' ' '; }}
CA=""
case "${{SRC_SSL:-}}" in VERIFY_*) for f in /etc/ssl/certs/ca-certificates.crt /etc/pki/tls/certs/ca-bundle.crt; do [ -f "$f" ] && CA="--ssl-ca=$f" && break; done ;; esac
if [ -n "${{SRC_HOST:-}}" ]; then
  cnf "$SRC_HOST" "$SRC_PORT" "$SRC_USER" "$SRC_PASSWORD" > /tmp/src.cnf
  if ! out=$(mysql --defaults-extra-file=/tmp/src.cnf --ssl-mode="$SRC_SSL" $CA --connect-timeout=15 -N -B -e "SELECT CONCAT(VERSION(), ' ', IFNULL((SELECT SUM(data_length + index_length) FROM information_schema.TABLES WHERE TABLE_SCHEMA = DATABASE()), 0))" "$SRC_DB" 2>&1); then
    echo "PYO source-error $(one "$out")"; exit 0
  fi
  echo "PYO source $out"
fi
if [ -n "${{TGT_HOST:-}}" ]; then
  cnf "$TGT_HOST" 3306 "$TGT_USER" "$TGT_PASSWORD" > /tmp/tgt.cnf
  # A new pod's address reaches the network policy a few seconds late.
  for i in $(seq 15); do out=$(mysql --defaults-extra-file=/tmp/tgt.cnf --connect-timeout=15 -N -B -e "{objects}" app 2>&1) && break; out="FAILED $out"; sleep 2; done
  case "$out" in FAILED*) echo "PYO target-error $(one "${{out#FAILED }}")"; exit 0 ;; esac
  echo "PYO target-objects $out"
fi
echo "PYO done"
"#,
            objects = dbimport::MYSQL_OBJECTS_SQL.replace('"', "\\\"")
        ),
    }
}

/// What the Job printed, as the result's data: `source` {version,
/// versionNum, sizeBytes}, `extensions`, `unreadable` (tables the source's
/// user can't read), `targetObjects`; or `error` {kind, side, message}
/// when a connection failed.
pub fn parse(spec: &ImportCheck, out: &str) -> anyhow::Result<Value> {
    let mut found = json!({});
    let mut done = false;
    for line in out.lines() {
        let Some(rest) = line.trim().strip_prefix("PYO ") else {
            continue;
        };
        let (key, value) = rest.split_once(' ').unwrap_or((rest, ""));
        let value = value.trim();
        match key {
            "source" => {
                let mut parts = value.split_whitespace();
                let version = parts.next().unwrap_or("");
                let size: i64 = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
                found["source"] = if spec.engine == "postgres" {
                    // server_version_num: 170004 is 17.4; 90624 is 9.6.24.
                    let n: i64 = version.parse().unwrap_or(0);
                    let (major, minor) = if n >= 100_000 {
                        (n / 10_000, n % 10_000)
                    } else {
                        (n / 10_000, n / 100 % 100)
                    };
                    json!({ "version": format!("{major}.{minor}"), "major": major, "versionNum": n, "sizeBytes": size })
                } else {
                    let major = version.split('.').take(2).collect::<Vec<_>>().join(".");
                    json!({ "version": version, "major": major, "mariadb": version.to_ascii_lowercase().contains("mariadb"), "sizeBytes": size })
                };
            }
            "extensions" => {
                found["extensions"] = json!(
                    value
                        .split(',')
                        .map(str::trim)
                        .filter(|e| !e.is_empty())
                        .collect::<Vec<_>>()
                )
            }
            "unreadable" => found["unreadable"] = json!(value.parse::<i64>().unwrap_or(0)),
            "target-objects" => found["targetObjects"] = json!(value.parse::<i64>().unwrap_or(-1)),
            "source-error" | "target-error" => {
                let side = key.trim_end_matches("-error");
                let (kind, message) = match (&spec.source, side) {
                    (Some(c), "source") => dbimport::explain_connect_error(
                        &spec.engine,
                        &c.host,
                        c.port,
                        &c.database,
                        value,
                    ),
                    _ => (
                        "target",
                        format!(
                            "Couldn't check the database to import into: {}",
                            value.chars().take(300).collect::<String>()
                        ),
                    ),
                };
                found["error"] = json!({ "side": side, "kind": kind, "message": message });
                done = true;
            }
            "done" => done = true,
            _ => {}
        }
    }
    anyhow::ensure!(
        done,
        "the check didn't finish: {}",
        out.lines()
            .rev()
            .find(|l| !l.trim().is_empty())
            .unwrap_or("no output")
    );
    Ok(found)
}

async fn run(client: &Client, spec: &ImportCheck) -> anyhow::Result<Value> {
    let ns = spec.namespace.as_str();
    anyhow::ensure!(
        ns.starts_with("pyo-db-"),
        "checks run in a team's databases namespace"
    );
    crate::scope::ensure_managed(client, ns).await?;
    let id = format!(
        "{:x}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0)
    );
    let name = format!("pyo-import-check-{id}");
    let (import_key, import_value) = dbimport::POD_LABEL;
    let labels =
        json!({ "app.kubernetes.io/managed-by": MANAGER, LABEL: id, import_key: import_value });
    let password = spec
        .env
        .iter()
        .find(|e| e.key == "password")
        .map(|e| e.value.clone())
        .unwrap_or_default();
    let mut data = serde_json::Map::new();
    let mut env = vec![];
    if let Some(c) = &spec.source {
        if spec.engine == "postgres" {
            data.insert("SRC".into(), json!(dbimport::pg_conninfo(c)));
        } else {
            for (k, v) in [
                ("SRC_HOST", c.host.clone()),
                ("SRC_PORT", c.port.to_string()),
                ("SRC_USER", c.user.clone()),
                ("SRC_DB", c.database.clone()),
                ("SRC_SSL", dbimport::mysql_ssl_mode(&c.tls).to_owned()),
            ] {
                data.insert(k.into(), json!(v));
            }
        }
        data.insert("SRC_PASSWORD".into(), json!(password));
    }
    if let Some(t) = &spec.target {
        data.insert("TGT_HOST".into(), json!(t.host));
        let key = |k: &str| json!({ "secretKeyRef": { "name": t.secret, "key": k } });
        env.push(json!({ "name": "TGT_USER", "valueFrom": key("username") }));
        env.push(json!({ "name": "TGT_PASSWORD", "valueFrom": key("password") }));
    }
    let secrets = Api::<Secret>::namespaced(client.clone(), ns);
    let jobs = Api::<Job>::namespaced(client.clone(), ns);
    secrets
        .patch(&name, &PatchParams::apply(MANAGER).force(), &Patch::Apply(json!({
            "apiVersion": "v1", "kind": "Secret", "metadata": { "name": name, "labels": labels }, "type": "Opaque", "stringData": data,
        })))
        .await?;
    let job = json!({
        "apiVersion": "batch/v1", "kind": "Job",
        "metadata": { "name": name, "labels": labels },
        "spec": { "backoffLimit": 0, "activeDeadlineSeconds": 150, "ttlSecondsAfterFinished": 300, "template": {
            "metadata": { "labels": labels },
            "spec": {
                "restartPolicy": "Never",
                "automountServiceAccountToken": false,
                "containers": [{
                    "name": "check",
                    "image": pyo_proto::db_tools_image(&spec.engine),
                    "command": ["sh", "-c", script(&spec.engine)],
                    "envFrom": [{ "secretRef": { "name": name } }],
                    "env": env,
                    "resources": { "requests": { "cpu": "50m", "memory": "64Mi" }, "limits": { "memory": "256Mi" } },
                }],
            },
        } },
    });
    let made = jobs
        .create(&PostParams::default(), &serde_json::from_value(job)?)
        .await;
    let outcome = match made {
        Ok(_) => wait(&jobs, &name, 160).await,
        Err(e) => Err(format!("couldn't start the check: {e}")),
    };
    let out = logs(client, ns, &id).await;
    let _ = jobs.delete(&name, &DeleteParams::background()).await;
    let _ = secrets.delete(&name, &DeleteParams::default()).await;
    match parse(spec, &out) {
        Ok(found) => Ok(found),
        Err(e) => match outcome {
            Err(why) => {
                let last = out
                    .lines()
                    .rev()
                    .find(|l| !l.trim().is_empty())
                    .unwrap_or("")
                    .trim();
                anyhow::bail!(
                    "{why}{}",
                    if last.is_empty() {
                        String::new()
                    } else {
                        format!(": {}", last.chars().take(300).collect::<String>())
                    }
                )
            }
            Ok(()) => Err(e),
        },
    }
}

/// Until the Job ends (or `secs` pass): Ok once it succeeded.
async fn wait(jobs: &Api<Job>, name: &str, secs: u64) -> Result<(), String> {
    for _ in 0..secs / 2 {
        tokio::time::sleep(Duration::from_secs(2)).await;
        let Ok(job) = jobs.get(name).await else {
            continue;
        };
        let st = job.status.unwrap_or_default();
        if st.succeeded.unwrap_or(0) > 0 {
            return Ok(());
        }
        if st.failed.unwrap_or(0) > 0 {
            return Err("the check failed".into());
        }
    }
    Err("the check didn't finish in time. Its image may still be downloading, so try again in a minute".into())
}

async fn logs(client: &Client, ns: &str, id: &str) -> String {
    let pods = Api::<Pod>::namespaced(client.clone(), ns);
    let Ok(list) = pods
        .list(&ListParams::default().labels(&format!("{LABEL}={id}")))
        .await
    else {
        return String::new();
    };
    let mut out = String::new();
    for p in list.items {
        if let Ok(l) = pods
            .logs(
                &p.name_any(),
                &LogParams {
                    tail_lines: Some(200),
                    ..Default::default()
                },
            )
            .await
        {
            out.push_str(&l);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use pyo_proto::dbimport::Connection;

    fn spec(engine: &str) -> ImportCheck {
        ImportCheck {
            namespace: "pyo-db-t".into(),
            engine: engine.into(),
            source: Some(Connection {
                host: "db.example.com".into(),
                port: 5432,
                user: "u".into(),
                database: "shop".into(),
                tls: String::new(),
            }),
            target: None,
            env: vec![],
        }
    }

    #[test]
    fn what_a_check_found() {
        let out = "PYO source 170004 123456\nPYO extensions citext,pgcrypto\nPYO unreadable 0\nPYO target-objects 0\nPYO done\n";
        let f = parse(&spec("postgres"), out).unwrap();
        assert_eq!(
            f["source"],
            json!({ "version": "17.4", "major": 17, "versionNum": 170004, "sizeBytes": 123456 })
        );
        assert_eq!(f["extensions"], json!(["citext", "pgcrypto"]));
        assert_eq!(f["targetObjects"], 0);
        assert_eq!(
            parse(
                &spec("postgres"),
                "PYO source 90624 1\nPYO extensions \nPYO done"
            )
            .unwrap()["source"]["version"],
            "9.6"
        );
        let f = parse(&spec("mysql"), "PYO source 8.0.36 2048\nPYO done\n").unwrap();
        assert_eq!(
            (
                f["source"]["major"].as_str(),
                f["source"]["mariadb"].as_bool()
            ),
            (Some("8.0"), Some(false))
        );
        assert_eq!(
            parse(&spec("mysql"), "PYO source 10.11.6-MariaDB 1\nPYO done").unwrap()["source"]["mariadb"],
            true
        );
        let f = parse(&spec("postgres"), "PYO source-error psql: error: FATAL:  password authentication failed for user \"u\" \n").unwrap();
        assert_eq!(f["error"]["kind"], "auth");
        assert!(parse(&spec("postgres"), "something broke").is_err());
    }

    #[test]
    fn scripts_quote_their_sql() {
        // The objects queries go inside double quotes.
        assert!(
            !dbimport::PG_OBJECTS_SQL.contains('"')
                && !dbimport::MYSQL_OBJECTS_SQL.contains('"')
                && !dbimport::PG_OBJECTS_SQL.contains('$')
        );
        assert!(
            script("postgres").contains("NOT LIKE 'pg\\_toast%'")
                && script("mysql").contains("information_schema.EVENTS")
        );
    }

    /// An isolated project's database copied by its team's import check;
    /// the team's database pods, and another team's imports, still can't
    /// reach it. PYO_TEST_KUBECONFIG=<throwaway k3s> cargo test -p pyo-agent
    /// -- --ignored imports_reach --nocapture (with postgres:18.6-alpine on
    /// the cluster; the check pulls its own image).
    #[tokio::test]
    #[ignore = "needs a throwaway Kubernetes cluster that enforces network policies"]
    async fn imports_reach_an_isolated_project_against_a_cluster() {
        use k8s_openapi::api::{
            apps::v1::Deployment,
            core::v1::{Namespace, Service},
        };
        let Ok(path) = std::env::var("PYO_TEST_KUBECONFIG") else {
            panic!("set PYO_TEST_KUBECONFIG to a throwaway cluster's kubeconfig")
        };
        let kc = kube::config::Kubeconfig::read_from(&path).unwrap();
        let client = Client::try_from(
            kube::Config::from_custom_kubeconfig(kc, &Default::default())
                .await
                .unwrap(),
        )
        .unwrap();
        let (team, other) = ("team_itest", "team_iother");
        let (project, db, other_db) = (
            "pyo-it-shop",
            pyo_proto::databases_namespace(team),
            pyo_proto::databases_namespace(other),
        );
        let namespaces = Api::<Namespace>::all(client.clone());
        let pp = PatchParams::apply("pyo-test").force();
        namespaces
            .patch(
                project,
                &pp,
                &Patch::Apply(crate::rollout::project_namespace(project, "prj_it", team)),
            )
            .await
            .unwrap();
        for (ns, t) in [(&db, team), (&other_db, other)] {
            let body = json!({ "apiVersion": "v1", "kind": "Namespace", "metadata": { "name": ns, "labels": { pyo_proto::rbac::MANAGED_LABEL: "true", pyo_proto::TEAM_LABEL: t } } });
            namespaces
                .patch(ns, &pp, &Patch::Apply(body))
                .await
                .unwrap();
        }
        // The source: Postgres in the project, then the project isolated.
        let labels = json!({ "app": "srcpg" });
        Api::<Deployment>::namespaced(client.clone(), project)
            .patch("srcpg", &pp, &Patch::Apply(json!({
                "apiVersion": "apps/v1", "kind": "Deployment", "metadata": { "name": "srcpg" },
                "spec": { "selector": { "matchLabels": labels }, "template": { "metadata": { "labels": labels }, "spec": { "containers": [{
                    "name": "pg", "image": "postgres:18.6-alpine", "env": [{ "name": "POSTGRES_PASSWORD", "value": "pw" }],
                    "readinessProbe": { "exec": { "command": ["pg_isready", "-U", "postgres"] }, "periodSeconds": 2 },
                }] } } },
            })))
            .await
            .unwrap();
        Api::<Service>::namespaced(client.clone(), project)
            .patch("srcpg", &pp, &Patch::Apply(json!({ "apiVersion": "v1", "kind": "Service", "metadata": { "name": "srcpg" }, "spec": { "selector": labels, "ports": [{ "port": 5432 }] } })))
            .await
            .unwrap();
        for _ in 0..120 {
            let d = Api::<Deployment>::namespaced(client.clone(), project)
                .get("srcpg")
                .await
                .unwrap();
            if d.status.and_then(|s| s.ready_replicas).unwrap_or(0) == 1 {
                break;
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        crate::rollout::set_isolation(&client, project, true)
            .await
            .unwrap();
        let host = format!("srcpg.{project}.svc.cluster.local");

        // The team's import check gets through.
        let check = |ns: String| ImportCheck {
            namespace: ns,
            engine: "postgres".into(),
            source: Some(Connection {
                host: host.clone(),
                port: 5432,
                user: "postgres".into(),
                database: "postgres".into(),
                tls: "disable".into(),
            }),
            target: None,
            env: vec![pyo_proto::EnvVar {
                key: "password".into(),
                value: "pw".into(),
            }],
        };
        let r = super::check(&client, check(db.clone())).await;
        assert!(r.ok, "{:?}", r.message);
        let found = r.data.unwrap();
        assert!(found.get("error").is_none(), "{found}");
        assert_eq!(found["source"]["major"], 18, "{found}");
        // Another team's import doesn't, and says where it couldn't reach.
        let r = super::check(&client, check(other_db.clone())).await;
        let found = r.data.unwrap_or_default();
        assert_eq!(found["error"]["kind"], "unreachable", "{found}");
        assert_eq!(
            found["error"]["message"],
            "Can't reach srcpg in project pyo-it-shop. Check it's running and listening on port 5432."
        );

        // A pod of the team's databases (not an import's) can't either.
        let pods = Api::<Pod>::namespaced(client.clone(), &db);
        let probe: Pod = serde_json::from_value(json!({
            "metadata": { "name": "not-an-import", "labels": { "pyo.dev/component": "postgres" } },
            "spec": { "restartPolicy": "Never", "containers": [{ "name": "c", "image": "postgres:18.6-alpine", "command": ["sh", "-c", format!("sleep 5; pg_isready -h {host} -t 10")] }] },
        }))
        .unwrap();
        let _ = pods.delete("not-an-import", &DeleteParams::default()).await;
        tokio::time::sleep(Duration::from_secs(2)).await;
        pods.create(&PostParams::default(), &probe).await.unwrap();
        let mut phase = String::new();
        for _ in 0..90 {
            phase = pods
                .get("not-an-import")
                .await
                .unwrap()
                .status
                .and_then(|s| s.phase)
                .unwrap_or_default();
            if phase == "Succeeded" || phase == "Failed" {
                break;
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        assert_eq!(phase, "Failed", "a database pod reached the project");

        for ns in [project.to_owned(), db, other_db] {
            namespaces
                .delete(&ns, &DeleteParams::default())
                .await
                .unwrap();
        }
    }
}
