//! Databases: Pyo's Database objects (pyo.dev), which pyo-db-controller
//! runs (the databases system, pyo-proto's `databases_bundle`) (CloudNativePG for Postgres, MOCO for MySQL, pyo-kv for Valkey).
//! The agent writes each one's login Secret and Database object in a
//! managed namespace (so the controller may work there), deletes them, and
//! reports their status every heartbeat.

use k8s_openapi::{
    ByteString,
    api::{
        core::v1::{Event, Namespace, Pod, Secret},
        networking::v1::NetworkPolicy,
    },
};
use kube::{
    Api, Client, ResourceExt,
    api::{DeleteParams, DynamicObject, ListParams, LogParams, Patch, PatchParams},
    core::{ApiResource, GroupVersionKind},
};
use pyo_proto::{
    CommandResult, DatabaseReport, DatabaseSpec, ISOLATION_POLICY, LogLine, ProjectTeam, TEAM_LABEL,
};
use serde_json::{Value, json};

use crate::MANAGER;

fn resource() -> ApiResource {
    ApiResource::from_gvk_with_plural(
        &GroupVersionKind::gvk("pyo.dev", "v1alpha1", "Database"),
        "databases",
    )
}

/// A team's databases namespace: `pyo-db-<team>` (pyo-api's `namespace_for`).
const NS_PREFIX: &str = "pyo-db-";
/// Where the databases system's operators run.
const SYSTEM_NS: &str = "pyo-databases";
/// Ports the team's services connect on (each pod's own, as a policy sees
/// it): Postgres (instances and poolers), MySQL and its X protocol, KV
/// (pyo-kv and Valkey), Sentinel, and KV's REST API.
const CLIENT_PORTS: [u16; 6] = [5432, 3306, 33060, 6379, 26379, 8080];

/// Who may reach a team's databases namespace. Like an isolated project's
/// (rollout.rs `set_isolation`), ingress only; and narrower: besides its
/// own pods, the team's project namespaces on the client ports, and each of
/// the system's components on the ports it uses. The node's own traffic
/// (kubelet probes) is always allowed by k3s's policy controller.
pub fn isolation_policy(team_id: &str) -> Value {
    let ports = |ps: &[u16]| {
        ps.iter()
            .map(|p| json!({ "protocol": "TCP", "port": p }))
            .collect::<Vec<_>>()
    };
    let pods = |ns: &str, labels: Value| json!({ "namespaceSelector": { "matchLabels": { "kubernetes.io/metadata.name": ns } }, "podSelector": { "matchLabels": labels } });
    let mut ingress = vec![
        // Its own pods: replication, Sentinels, pyo-kv, poolers, backups,
        // instance to instance.
        json!({ "from": [{ "podSelector": {} }] }),
    ];
    if !team_id.is_empty() {
        ingress.push(json!({ "from": [{ "namespaceSelector": { "matchLabels": { TEAM_LABEL: team_id, "app.kubernetes.io/managed-by": crate::MANAGER } } }], "ports": ports(&CLIENT_PORTS) }));
    }
    ingress.extend([
        // CloudNativePG: each instance's manager (status, backups, switchovers).
        json!({ "from": [pods(SYSTEM_NS, json!({ "app.kubernetes.io/name": "cloudnative-pg" }))], "ports": ports(&[8000]) }),
        // MySQL's operator: its agent beside mysqld (gRPC), and mysqld's admin port.
        json!({ "from": [pods(SYSTEM_NS, json!({ "app.kubernetes.io/name": "moco", "app.kubernetes.io/component": "moco-controller" }))], "ports": ports(&[9080, 33062]) }),
        // pyo-db-controller: MySQL logins, KV's Valkey and Sentinels, Postgres's metrics.
        json!({ "from": [pods(SYSTEM_NS, json!({ "app": "pyo-db-controller" }))], "ports": ports(&[3306, 6379, 26379, 9187]) }),
        // The agent's analytics: Postgres, PgBouncer, MySQL and KV metrics.
        json!({ "from": [pods(pyo_proto::rbac::NS, json!({ "app.kubernetes.io/name": "pyo-agent" }))], "ports": ports(&[9187, 9127, 9104, 9121]) }),
    ]);
    json!({
        "apiVersion": "networking.k8s.io/v1", "kind": "NetworkPolicy",
        "metadata": { "name": ISOLATION_POLICY, "labels": { "app.kubernetes.io/managed-by": crate::MANAGER } },
        "spec": { "podSelector": {}, "policyTypes": ["Ingress"], "ingress": ingress },
    })
}

/// The team a databases namespace belongs to: its label, else its name
/// (`pyo-db-<id>` for team `team_<id>`).
fn team_of(ns: &Namespace) -> String {
    if let Some(t) = ns.labels().get(TEAM_LABEL).filter(|t| !t.is_empty()) {
        return t.clone();
    }
    ns.name_any()
        .strip_prefix(NS_PREFIX)
        .map(|id| format!("team_{id}"))
        .unwrap_or_default()
}

/// Write a databases namespace's policy, unless it's already as wanted.
async fn isolate(client: &Client, ns: &str, team_id: &str) -> anyhow::Result<()> {
    let api: Api<NetworkPolicy> = Api::namespaced(client.clone(), ns);
    let want = isolation_policy(team_id);
    let have = api
        .get_opt(ISOLATION_POLICY)
        .await?
        .and_then(|p| serde_json::to_value(p.spec).ok());
    if have.as_ref() != Some(&want["spec"]) {
        api.patch(
            ISOLATION_POLICY,
            &PatchParams::apply(MANAGER).force(),
            &Patch::Apply(&want),
        )
        .await?;
        tracing::info!(
            namespace = ns,
            team = team_id,
            "isolated a databases namespace"
        );
    }
    Ok(())
}

/// Whether project namespaces have their team labels (the API's first
/// heartbeat reply since this agent started): until then no databases
/// namespace is isolated, or its team's services would be shut out.
static TEAMS_LABELLED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn teams_labelled() -> bool {
    TEAMS_LABELLED.load(std::sync::atomic::Ordering::Relaxed)
}

/// Every databases namespace's policy (each heartbeat): one missing gets
/// it, and one changed by hand is put back.
pub async fn isolate_all(client: &Client) {
    if !teams_labelled() {
        return;
    }
    let lp = ListParams::default().labels(&format!("{}=true", pyo_proto::rbac::MANAGED_LABEL));
    let namespaces = match Api::<Namespace>::all(client.clone()).list(&lp).await {
        Ok(l) => l.items,
        Err(e) => return tracing::warn!(error = %e, "couldn't list databases namespaces"),
    };
    for ns in namespaces
        .iter()
        .filter(|n| n.name_any().starts_with(NS_PREFIX))
    {
        if let Err(e) = isolate(client, &ns.name_any(), &team_of(ns)).await {
            tracing::warn!(namespace = ns.name_any(), error = %format!("{e:#}"), "couldn't isolate a databases namespace");
        }
    }
}

/// Label each project namespace with its team (as the API lists them), so
/// its team's databases let it in. Only Pyo's namespaces that exist.
pub async fn label_teams(client: &Client, teams: &[ProjectTeam]) {
    let api = Api::<Namespace>::all(client.clone());
    let lp = ListParams::default().labels(&format!("app.kubernetes.io/managed-by={MANAGER}"));
    let Ok(have) = api.list(&lp).await.map(|l| l.items) else {
        return;
    };
    let mut failed = false;
    for t in teams {
        let Some(ns) = have.iter().find(|n| n.name_any() == t.namespace) else {
            continue;
        };
        if t.team_id.is_empty() || ns.labels().get(TEAM_LABEL) == Some(&t.team_id) {
            continue;
        }
        let patch = json!({ "metadata": { "labels": { TEAM_LABEL: t.team_id } } });
        match api
            .patch(&t.namespace, &PatchParams::default(), &Patch::Merge(&patch))
            .await
        {
            Ok(_) => {
                tracing::info!(
                    namespace = t.namespace,
                    team = t.team_id,
                    "labelled a project namespace with its team"
                );
                // An isolated one's policy names its team (its imports come in).
                if let Ok(Some(_)) = Api::<NetworkPolicy>::namespaced(client.clone(), &t.namespace)
                    .get_opt(ISOLATION_POLICY)
                    .await
                    && let Err(e) = crate::rollout::set_isolation(client, &t.namespace, true).await
                {
                    tracing::warn!(namespace = t.namespace, error = %format!("{e:#}"), "couldn't update a project's isolation");
                }
            }
            Err(e) => {
                failed = true;
                tracing::warn!(namespace = t.namespace, error = %e, "couldn't label a project namespace with its team");
            }
        }
    }
    if !failed {
        TEAMS_LABELLED.store(true, std::sync::atomic::Ordering::Relaxed);
    }
}

/// Create or change one: its namespace (managed, labelled with its team and
/// isolated), Secret, then Database.
pub async fn apply(client: &Client, spec: &DatabaseSpec) -> anyhow::Result<String> {
    crate::scope::ensure_managed(client, &spec.namespace).await?;
    if !spec.team_id.is_empty() {
        let patch = json!({ "metadata": { "labels": { TEAM_LABEL: spec.team_id } } });
        Api::<Namespace>::all(client.clone())
            .patch(
                &spec.namespace,
                &PatchParams::default(),
                &Patch::Merge(&patch),
            )
            .await?;
    }
    // Before the first heartbeat's labels, the next heartbeat isolates it.
    if teams_labelled() {
        let ns = Api::<Namespace>::all(client.clone())
            .get(&spec.namespace)
            .await?;
        isolate(client, &spec.namespace, &team_of(&ns)).await?;
    }
    let data: std::collections::BTreeMap<String, ByteString> = spec
        .env
        .iter()
        .map(|e| (e.key.clone(), ByteString(e.value.clone().into_bytes())))
        .collect();
    let secret = Secret {
        metadata: kube::core::ObjectMeta {
            name: Some(spec.secret_name.clone()),
            namespace: Some(spec.namespace.clone()),
            labels: Some(
                [
                    (
                        "app.kubernetes.io/managed-by".to_owned(),
                        MANAGER.to_owned(),
                    ),
                    ("pyo.dev/database".to_owned(), spec.name.clone()),
                ]
                .into(),
            ),
            ..Default::default()
        },
        type_: Some(spec.secret_type.clone()),
        data: Some(data),
        ..Default::default()
    };
    let pp = PatchParams::apply(MANAGER).force();
    let secrets: Api<Secret> = Api::namespaced(client.clone(), &spec.namespace);
    secrets
        .patch(&spec.secret_name, &pp, &Patch::Apply(&secret))
        .await?;
    // Others beside it (the backup storage's keys), labelled the same so
    // deleting the database deletes them.
    for extra in &spec.secrets {
        let s = Secret {
            metadata: kube::core::ObjectMeta {
                name: Some(extra.name.clone()),
                namespace: Some(spec.namespace.clone()),
                labels: secret.metadata.labels.clone(),
                ..Default::default()
            },
            type_: Some("Opaque".into()),
            data: Some(
                extra
                    .env
                    .iter()
                    .map(|e| {
                        (
                            e.key.clone(),
                            ByteString(with_api_url(&e.value).into_bytes()),
                        )
                    })
                    .collect(),
            ),
            ..Default::default()
        };
        secrets.patch(&extra.name, &pp, &Patch::Apply(&s)).await?;
    }
    // An import's Secret once it's done: only one that's this database's.
    for name in &spec.remove_secrets {
        let Some(s) = secrets.get_opt(name).await? else {
            continue;
        };
        let labels = s.labels();
        if labels.get("pyo.dev/database") == Some(&spec.name)
            && labels
                .get("app.kubernetes.io/managed-by")
                .map(String::as_str)
                == Some(MANAGER)
        {
            match secrets.delete(name, &DeleteParams::default()).await {
                Err(kube::Error::Api(e)) if e.code != 404 => return Err(kube::Error::Api(e).into()),
                Err(e) if !matches!(e, kube::Error::Api(_)) => return Err(e.into()),
                _ => {}
            }
        }
    }
    let api = Api::<DynamicObject>::namespaced_with(client.clone(), &spec.namespace, &resource());
    // Strict: a field the cluster's databases system doesn't know yet (an
    // older CRD, before Pyo updates it) fails here instead of being dropped
    // without a word.
    let strict = PatchParams::apply(MANAGER).force().validation_strict();
    // Right after the databases system is installed its webhooks may not
    // answer yet: tried again for a minute before it counts as failed.
    let mut tries = 0;
    let applied = loop {
        match api
            .patch(&spec.name, &strict, &Patch::Apply(&spec.database))
            .await
        {
            Err(e) if tries < 12 && pyo_proto::transient_error(&e.to_string()) => {
                tries += 1;
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            }
            r => break r,
        }
    };
    applied.map_err(|e| match e {
        kube::Error::Api(ref a) if a.code == 404 => anyhow::anyhow!(
            "Databases are still being set up on this cluster. Try again in a minute."
        ),
        kube::Error::Api(ref a) if a.message.contains("unknown field") => {
            let field = a
                .message
                .split("unknown field")
                .nth(1)
                .map(|f| {
                    f.trim()
                        .trim_matches(|c| c == '"' || c == '\\' || c == ',')
                        .to_owned()
                })
                .unwrap_or_default();
            anyhow::anyhow!(
                "This cluster's databases are still being updated for {}. Try again in a minute.",
                if field.is_empty() {
                    "this setting".into()
                } else {
                    field
                }
            )
        }
        e => e.into(),
    })?;
    Ok(format!("Applied database {}.", spec.name))
}

/// A Secret value naming Pyo's API: `$PYO_API_URL/...` is this agent's
/// address for it (an import's dump is fetched from there, from inside
/// the cluster).
fn with_api_url(v: &str) -> String {
    match v.strip_prefix("$PYO_API_URL") {
        Some(rest) => format!(
            "{}{rest}",
            std::env::var("PYO_API_URL")
                .unwrap_or_default()
                .trim_end_matches('/')
        ),
        None => v.to_owned(),
    }
}

/// Delete one and its login Secret (the controller removes what it made).
pub async fn delete(client: &Client, namespace: &str, name: &str) -> anyhow::Result<String> {
    let api = Api::<DynamicObject>::namespaced_with(client.clone(), namespace, &resource());
    let gone = |r: Result<(), kube::Error>| match r {
        Err(kube::Error::Api(e)) if e.code != 404 => Err(anyhow::Error::from(kube::Error::Api(e))),
        Err(e) if !matches!(e, kube::Error::Api(_)) => Err(e.into()),
        _ => Ok(()),
    };
    gone(
        api.delete(name, &DeleteParams::background())
            .await
            .map(|_| ()),
    )?;
    let secrets: Api<Secret> = Api::namespaced(client.clone(), namespace);
    for s in secrets
        .list(&ListParams::default().labels(&format!(
            "pyo.dev/database={name},app.kubernetes.io/managed-by={MANAGER}"
        )))
        .await
        .map(|l| l.items)
        .unwrap_or_default()
    {
        gone(
            secrets
                .delete(&s.name_any(), &DeleteParams::default())
                .await
                .map(|_| ()),
        )?;
    }
    // Done only once the Database is gone (a finalizer can hold it): the API
    // removes its row on this answer, and asks again on a failure.
    for wait in 0..30 {
        match api.get_opt(name).await? {
            None => return Ok(format!("Deleted database {name}.")),
            Some(_) if wait < 29 => tokio::time::sleep(std::time::Duration::from_secs(2)).await,
            Some(_) => {}
        }
    }
    anyhow::bail!("Database {name} is still being deleted on the cluster.")
}

/// Every Database on the cluster and its status, and whether that's the
/// whole list (false before databases are set up, or when listing failed).
pub async fn report(client: &Client) -> (Vec<DatabaseReport>, bool) {
    let Ok(list) = Api::<DynamicObject>::all_with(client.clone(), &resource())
        .list(&ListParams::default())
        .await
    else {
        return (vec![], false);
    };
    let reports = list
        .items
        .iter()
        .map(|d| DatabaseReport {
            namespace: d.namespace().unwrap_or_default(),
            name: d.name_any(),
            id: d
                .labels()
                .get("pyo.dev/database-id")
                .cloned()
                .unwrap_or_default(),
            status: d.data.get("status").cloned().unwrap_or(json!({})),
        })
        .collect();
    (reports, true)
}

/// Whether a pod is part of database `name`: labelled by the controller, or
/// one of its CloudNativePG instances or poolers.
fn is_member(labels: &std::collections::BTreeMap<String, String>, name: &str) -> bool {
    let read = format!("{name}-read");
    let poolers = [format!("{name}-pooler"), format!("{name}-read-pooler")];
    labels.get("pyo.dev/database").is_some_and(|d| d == name)
        || labels
            .get("cnpg.io/cluster")
            .is_some_and(|c| c == name || *c == read)
        || labels
            .get("cnpg.io/poolerName")
            .is_some_and(|p| poolers.contains(p))
}

/// Whether `n` is one of MOCO's objects for MySQL database `name`: its
/// StatefulSet, data claims (`mysql-data-<pod>`), backup CronJob and Jobs
/// and restore Job, all named after the database.
fn mysql_part(n: &str, name: &str) -> bool {
    let backup = format!("backup-{name}");
    n == name
        || n.starts_with(&format!("{name}-"))
        || n.starts_with(&format!("mysql-data-{name}-"))
        || n == backup
        || n.starts_with(&format!("{backup}-"))
        || n == format!("restore-{name}")
}

/// FetchDatabaseLogs: each pod's recent output, oldest first, and the
/// events about the database and its parts. Lines are as printed; the API
/// reads their level and message.
pub async fn logs(
    client: &Client,
    ns: &str,
    name: &str,
    tail_lines: i64,
    since_seconds: Option<i64>,
) -> CommandResult {
    let pods_api = Api::<Pod>::namespaced(client.clone(), ns);
    let pods: Vec<Pod> = match pods_api.list(&ListParams::default()).await {
        Ok(l) => l
            .items
            .into_iter()
            .filter(|p| is_member(p.labels(), name))
            .collect(),
        Err(e) => return crate::failed(format!("Couldn't list pods: {e}")),
    };
    let tail = tail_lines.clamp(1, 5000);
    let mut lines: Vec<LogLine> = vec![];
    let mut info = serde_json::Map::new();
    for p in &pods {
        let pod = p.name_any();
        let keep = [
            "pyo.dev/component",
            "pyo.dev/role",
            "cnpg.io/instanceRole",
            "cnpg.io/cluster",
            "cnpg.io/poolerName",
            "moco.cybozu.com/role",
        ];
        let labels: serde_json::Map<String, serde_json::Value> = p
            .labels()
            .iter()
            .filter(|(k, _)| keep.contains(&k.as_str()))
            .map(|(k, v)| (k.clone(), json!(v)))
            .collect();
        info.insert(pod.clone(), json!({ "labels": labels, "server": p.spec.as_ref().and_then(|s| s.node_name.clone()) }));
        // The main container: CloudNativePG's "postgres", MOCO's "mysqld", else the first.
        let container = p
            .spec
            .as_ref()
            .and_then(|s| {
                s.containers
                    .iter()
                    .find(|c| c.name == "postgres" || c.name == "mysqld")
                    .or(s.containers.first())
            })
            .map(|c| c.name.clone());
        let lp = LogParams {
            tail_lines: Some(tail),
            since_seconds,
            timestamps: true,
            container,
            ..Default::default()
        };
        match pods_api.logs(&pod, &lp).await {
            Ok(out) => {
                for l in out.lines() {
                    let (at, msg) = match l.split_once(' ') {
                        Some((ts, rest)) => match ts.parse::<k8s_openapi::jiff::Timestamp>() {
                            Ok(t) => (t.as_millisecond(), rest),
                            Err(_) => (pyo_proto::now_ms(), l),
                        },
                        None => (pyo_proto::now_ms(), l),
                    };
                    lines.push(LogLine {
                        at,
                        level: String::new(),
                        source: pod.clone(),
                        message: msg.chars().take(8000).collect(),
                    });
                }
            }
            Err(e) => lines.push(LogLine {
                at: pyo_proto::now_ms(),
                level: "warn".into(),
                source: pod.clone(),
                message: format!("No output yet: {e}"),
            }),
        }
    }
    lines.sort_by_key(|l| l.at);
    let skip = lines.len().saturating_sub(tail as usize);
    let lines: Vec<LogLine> = lines.into_iter().skip(skip).collect();

    // Events about the Database, its CloudNativePG objects (cluster,
    // poolers, backups), its MOCO objects (MySQL) and its pods.
    let pod_names: std::collections::HashSet<String> = pods.iter().map(|p| p.name_any()).collect();
    let prefix = format!("{name}-");
    let events: Vec<serde_json::Value> = match Api::<Event>::namespaced(client.clone(), ns)
        .list(&ListParams::default())
        .await
    {
        Ok(l) => l
            .items
            .into_iter()
            .filter(|e| {
                let o = &e.involved_object;
                let (kind, n) = (
                    o.kind.as_deref().unwrap_or(""),
                    o.name.as_deref().unwrap_or(""),
                );
                match kind {
                    "Pod" => pod_names.contains(n),
                    "Database" => n == name,
                    "MySQLCluster" | "BackupPolicy" => n == name || n.starts_with(&prefix),
                    "Cluster"
                    | "Pooler"
                    | "Backup"
                    | "ScheduledBackup"
                    | "StatefulSet"
                    | "Deployment"
                    | "PersistentVolumeClaim"
                    | "CronJob"
                    | "Job" => n == name || n.starts_with(&prefix) || mysql_part(n, name),
                    _ => false,
                }
            })
            .map(|e| {
                let at = e
                    .last_timestamp
                    .as_ref()
                    .map(|t| t.0.as_millisecond())
                    .or_else(|| e.event_time.as_ref().map(|t| t.0.as_millisecond()))
                    .or_else(|| {
                        e.metadata
                            .creation_timestamp
                            .as_ref()
                            .map(|t| t.0.as_millisecond())
                    })
                    .unwrap_or_else(pyo_proto::now_ms);
                json!({
                    "at": at,
                    "type": e.type_.clone().unwrap_or_default(),
                    "reason": e.reason.clone().unwrap_or_default(),
                    "kind": e.involved_object.kind.clone().unwrap_or_default(),
                    "object": e.involved_object.name.clone().unwrap_or_default(),
                    "message": e.message.clone().unwrap_or_default(),
                    "count": e.count.unwrap_or(1),
                })
            })
            .collect(),
        Err(_) => vec![],
    };
    CommandResult {
        ok: true,
        message: Some(format!("{} pod(s)", pods.len())),
        logs: lines,
        data: Some(json!({ "pods": info, "events": events })),
    }
}

#[cfg(test)]
mod tests {
    use super::{isolation_policy, mysql_part, team_of};
    use serde_json::json;

    #[test]
    fn databases_namespace_policy() {
        let p = isolation_policy("team_abc");
        assert_eq!(p["metadata"]["name"], "pyo-isolation");
        assert_eq!(
            p["spec"]["policyTypes"],
            json!(["Ingress"]),
            "egress stays open: backups reach their storage"
        );
        let ingress = p["spec"]["ingress"].as_array().unwrap();
        assert_eq!(ingress[0], json!({ "from": [{ "podSelector": {} }] }));
        assert_eq!(
            ingress[1]["from"][0]["namespaceSelector"]["matchLabels"],
            json!({ "pyo/team-id": "team_abc", "app.kubernetes.io/managed-by": "pyo" })
        );
        assert!(ingress[1]["from"][0].get("podSelector").is_none());
        // Every rule but its own pods' names its ports.
        assert!(
            ingress[1..]
                .iter()
                .all(|r| r["ports"].as_array().is_some_and(|p| !p.is_empty()))
        );
        // No rule lets in a whole other namespace on any port.
        assert!(!ingress.iter().any(|r| {
            r["from"]
                .as_array()
                .unwrap()
                .iter()
                .any(|f| f.get("namespaceSelector") == Some(&json!({})))
        }));
        let operator = |ns: &str, label: &str| {
            ingress
                .iter()
                .find(|r| {
                    r["from"][0]["namespaceSelector"]["matchLabels"]["kubernetes.io/metadata.name"]
                        == ns
                        && r["from"][0]["podSelector"]["matchLabels"]
                            .get(label)
                            .is_some()
                })
                .map(|r| {
                    r["ports"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|p| p["port"].as_u64().unwrap())
                        .collect::<Vec<_>>()
                })
        };
        assert_eq!(
            operator("pyo-databases", "app.kubernetes.io/name")
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            operator("pyo-system", "app.kubernetes.io/name").unwrap(),
            [9187, 9127, 9104, 9121]
        );
        // Without a team: only the system and its own pods.
        assert_eq!(
            isolation_policy("")["spec"]["ingress"]
                .as_array()
                .unwrap()
                .len(),
            ingress.len() - 1
        );
    }

    /// One heartbeat's pass on a throwaway cluster: label the project
    /// namespaces in PYO_TEST_TEAMS (`<namespace>=<team id>,...`), then
    /// isolate every databases namespace. PYO_TEST_KUBECONFIG: best the
    /// agent's own account's, so its roles and admission policy are tested.
    #[tokio::test]
    #[ignore = "needs a throwaway Kubernetes API server"]
    async fn isolation_against_a_cluster() {
        let Ok(path) = std::env::var("PYO_TEST_KUBECONFIG") else {
            panic!("set PYO_TEST_KUBECONFIG to a throwaway cluster's kubeconfig")
        };
        let kc = kube::config::Kubeconfig::read_from(&path).unwrap();
        let cfg = kube::Config::from_custom_kubeconfig(kc, &Default::default())
            .await
            .unwrap();
        let client = kube::Client::try_from(cfg).unwrap();
        let teams: Vec<pyo_proto::ProjectTeam> = std::env::var("PYO_TEST_TEAMS")
            .unwrap_or_default()
            .split(',')
            .filter_map(|p| p.split_once('='))
            .map(|(namespace, team)| pyo_proto::ProjectTeam {
                namespace: namespace.into(),
                team_id: team.into(),
            })
            .collect();
        super::label_teams(&client, &teams).await;
        assert!(super::teams_labelled());
        // PYO_TEST_DATABASE_SPECS: a JSON file of DatabaseSpecs to apply first.
        if let Ok(file) = std::env::var("PYO_TEST_DATABASE_SPECS") {
            let specs: Vec<pyo_proto::DatabaseSpec> =
                serde_json::from_str(&std::fs::read_to_string(file).unwrap()).unwrap();
            for s in &specs {
                println!("{}", super::apply(&client, s).await.unwrap());
            }
        }
        super::isolate_all(&client).await;
    }

    #[test]
    fn team_from_label_or_name() {
        let ns = |name: &str, labels: serde_json::Value| {
            serde_json::from_value(json!({ "metadata": { "name": name, "labels": labels } }))
                .unwrap()
        };
        assert_eq!(
            team_of(&ns("pyo-db-q9pnllqh1p5f", json!({}))),
            "team_q9pnllqh1p5f"
        );
        assert_eq!(
            team_of(&ns("pyo-db-x", json!({ "pyo/team-id": "team_real" }))),
            "team_real"
        );
    }

    #[test]
    fn mysql_parts() {
        for n in [
            "shop",
            "shop-0",
            "mysql-data-shop-1",
            "backup-shop",
            "backup-shop-29384520",
            "restore-shop",
        ] {
            assert!(mysql_part(n, "shop"), "{n}");
        }
        for n in [
            "shopping",
            "mysql-data-shopping-0",
            "backup-shopping",
            "restore-shop-2",
            "moco-shop-0",
        ] {
            assert!(!mysql_part(n, "shop"), "{n}");
        }
    }

    /// The Logs tab's read of a MySQL database on a cluster: every source and
    /// event object by its real name. PYO_TEST_DATABASE is `<namespace>/<name>`.
    #[tokio::test]
    #[ignore = "needs a throwaway Kubernetes API server"]
    async fn mysql_logs_against_a_cluster() {
        let Ok(path) = std::env::var("PYO_TEST_KUBECONFIG") else {
            panic!("set PYO_TEST_KUBECONFIG to a throwaway cluster's kubeconfig")
        };
        let db = std::env::var("PYO_TEST_DATABASE").expect("PYO_TEST_DATABASE=<namespace>/<name>");
        let (ns, name) = db.split_once('/').unwrap();
        let kc = kube::config::Kubeconfig::read_from(&path).unwrap();
        let cfg = kube::Config::from_custom_kubeconfig(kc, &Default::default())
            .await
            .unwrap();
        let client = kube::Client::try_from(cfg).unwrap();
        let r = super::logs(&client, ns, name, 200, None).await;
        assert!(r.ok, "{:?}", r.message);
        let mut sources: Vec<&str> = r.logs.iter().map(|l| l.source.as_str()).collect();
        sources.dedup();
        let data = r.data.unwrap();
        let objects: Vec<String> = data["events"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| {
                format!(
                    "{}/{}: {}",
                    e["kind"].as_str().unwrap(),
                    e["object"].as_str().unwrap(),
                    e["reason"].as_str().unwrap()
                )
            })
            .collect();
        println!("sources: {sources:?}");
        println!("pods: {}", data["pods"]);
        println!("events: {objects:#?}");
        assert!(!sources.is_empty());
    }
}
