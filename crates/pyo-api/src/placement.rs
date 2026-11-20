//! Where a service runs: on any server of its cluster, or only on servers
//! picked by name and on servers with a tag picked (every server that has
//! it, tagged now or later). Kept in its spec as
//! `"placement": { "servers": [...], "tags": [...] }`, absent for any
//! server. Services saved before kept `servers` and `serverTags`, which
//! read as the same (their tags as tags picked).
//!
//! A change goes live in place, without a deploy: the agent patches the
//! workload's node affinity and Kubernetes replaces its pods with the same
//! image ([`go_live`], `Command::Place`).

use std::collections::HashMap;

use pyo_proto::{Command, CronTask};
use serde_json::{Value, json};
use sqlx::{Postgres, Transaction};

use crate::error::{ApiError, ApiResult};

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Placement {
    pub servers: Vec<String>,
    pub tags: Vec<String>,
}

fn strings(v: &Value) -> Vec<String> {
    v.as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect()
}

fn dedup(mut v: Vec<String>) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    v.retain(|x| seen.insert(x.clone()));
    v
}

impl Placement {
    /// A spec's placement: `placement` when it has one (null is any
    /// server), else the `servers` and `serverTags` it was saved with.
    pub fn of(spec: &Value) -> Self {
        let (servers, tags) = match spec.get("placement") {
            Some(p) => (strings(&p["servers"]), strings(&p["tags"])),
            None => (strings(&spec["servers"]), strings(&spec["serverTags"])),
        };
        Placement {
            servers: dedup(servers),
            tags: dedup(
                tags.iter()
                    .filter_map(|t| pyo_proto::valid_tag(t))
                    .collect(),
            ),
        }
    }

    /// Any server of its cluster.
    pub fn any(&self) -> bool {
        self.servers.is_empty() && self.tags.is_empty()
    }

    /// It may run on this server (with these tags).
    pub fn takes(&self, server: &str, tags: &[String]) -> bool {
        self.any()
            || self.servers.iter().any(|s| s == server)
            || self.tags.iter().any(|t| tags.contains(t))
    }

    pub fn to_value(&self) -> Value {
        json!({ "servers": self.servers, "tags": self.tags })
    }
}

/// A spec's placement as it's kept: `placement`, absent for any server.
/// `servers` and `serverTags`, as clients may still send, go into it
/// unless `placement` is there too (it wins). A tag that can't be one is
/// refused.
pub fn normalize(spec: &mut Value) -> ApiResult<()> {
    let raw_tags = match spec.get("placement") {
        Some(p) => strings(&p["tags"]),
        None => strings(&spec["serverTags"]),
    };
    if let Some(bad) = raw_tags.iter().find(|t| pyo_proto::valid_tag(t).is_none()) {
        return Err(ApiError::bad_request(format!(
            "{bad} isn't a tag. Tags are lowercase letters, digits and dashes, up to 32."
        )));
    }
    let p = Placement::of(spec);
    if let Some(o) = spec.as_object_mut() {
        o.remove("servers");
        o.remove("serverTags");
        if p.any() {
            o.remove("placement");
        } else {
            o.insert("placement".into(), p.to_value());
        }
    }
    Ok(())
}

/// A server of the cluster, as the checks see it.
#[derive(Debug, Clone)]
pub struct Server {
    pub name: String,
    pub tags: Vec<String>,
    /// Its last report (allocatable, requested, gpuDetected, builder...).
    pub report: Value,
}

/// What one replica of the service needs.
#[derive(Debug, Clone, Copy, Default)]
pub struct Need {
    pub gpu: bool,
    /// Cores.
    pub cpu: f64,
    /// GiB.
    pub memory_gi: f64,
}

/// "2.5", "30": one decimal under 10, whole numbers above.
fn amount(x: f64) -> String {
    if x < 10.0 {
        let r = (x * 10.0).round() / 10.0;
        if r.fract() == 0.0 {
            format!("{}", r as i64)
        } else {
            format!("{r:.1}")
        }
    } else {
        format!("{}", x.round() as i64)
    }
}

/// Why a service can't run where `p` puts it, or None when some server
/// picked can. Any server is never refused here: its volume, if any, keeps
/// it on its server, and Kubernetes picks a GPU server for a GPU service.
///
/// `volume`: one of its volumes that keeps data on one server's disk
/// (its name, that server). `own`: what its replicas ask for now on each
/// server, which they give back when they move.
pub fn refusal(
    p: &Placement,
    servers: &[Server],
    volume: Option<(&str, &str)>,
    need: Need,
    own: &HashMap<String, (f64, f64)>,
) -> Option<String> {
    if p.any() {
        return None;
    }
    if let Some(missing) = p
        .servers
        .iter()
        .find(|n| !servers.iter().any(|s| &s.name == *n))
    {
        return Some(format!("{missing} isn't a server of this cluster."));
    }
    let picked: Vec<&Server> = servers
        .iter()
        .filter(|s| p.takes(&s.name, &s.tags))
        .collect();
    if picked.is_empty() {
        return Some(match p.tags.as_slice() {
            [t] => format!("No server has the tag {t} yet. Tag one first, or pick servers."),
            _ => "No server has these tags yet. Tag one first, or pick servers.".to_owned(),
        });
    }
    if let Some((name, on)) = volume
        && !picked.iter().any(|s| s.name == on)
    {
        return Some(format!(
            "Its volume {name} lives on {on}, so it has to run there. Pick {on} too."
        ));
    }
    let usable: Vec<&Server> = picked
        .iter()
        .copied()
        .filter(|s| s.report["builder"] != json!(true))
        .collect();
    if usable.is_empty() {
        return Some("These servers only run builds. Pick another server.".to_owned());
    }
    let usable: Vec<&Server> = if need.gpu {
        let gpu: Vec<&Server> = usable
            .into_iter()
            .filter(|s| s.report["gpuDetected"] == json!(true))
            .collect();
        if gpu.is_empty() {
            return Some(
                "It asks for a GPU, and none of these servers has one. Pick a GPU server."
                    .to_owned(),
            );
        }
        gpu
    } else {
        let regular: Vec<&Server> = usable
            .into_iter()
            .filter(|s| s.report["gpuDedicated"] != json!(true))
            .collect();
        if regular.is_empty() {
            return Some(
                "These servers only run services that use a GPU. Pick another server.".to_owned(),
            );
        }
        regular
    };
    // Room: only when every one of them has said what it has.
    let alloc = |s: &Server| {
        (
            s.report["allocatable"]["cpu"].as_f64().unwrap_or(0.0),
            s.report["allocatable"]["memoryGi"].as_f64().unwrap_or(0.0),
        )
    };
    if usable.iter().any(|s| alloc(s).0 <= 0.0) {
        return None;
    }
    let fits = usable.iter().any(|s| {
        let (cpu, mem) = alloc(s);
        let (own_cpu, own_mem) = own.get(&s.name).copied().unwrap_or_default();
        let free_cpu = cpu - s.report["requested"]["cpu"].as_f64().unwrap_or(0.0) + own_cpu;
        let free_mem = mem - s.report["requested"]["memoryGi"].as_f64().unwrap_or(0.0) + own_mem;
        need.cpu <= free_cpu + 0.01 && need.memory_gi <= free_mem + 0.01
    });
    if !fits {
        return Some(format!(
            "None of these servers has room for it. It needs about {} CPU and {} GB of memory.",
            amount(need.cpu),
            amount(need.memory_gi)
        ));
    }
    None
}

/// The cluster's servers (not being removed), with their tags and reports.
pub async fn servers(
    tx: &mut Transaction<'_, Postgres>,
    cluster_id: &str,
) -> ApiResult<Vec<Server>> {
    let rows: Vec<(String, Vec<String>, Value)> = sqlx::query_as(
        "SELECT n.name, coalesce(t.tags, '{}'), coalesce(n.report, '{}') FROM pyo.servers n
           LEFT JOIN pyo.server_tags t ON t.cluster_id = n.cluster_id AND t.name = n.name
          WHERE n.cluster_id = $1 AND n.removal IS NULL",
    )
    .bind(cluster_id)
    .fetch_all(&mut **tx)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(name, tags, report)| Server { name, tags, report })
        .collect())
}

/// Refuses a placement no server picked can run the service with `spec`
/// on: none left, its volume elsewhere, no GPU, no room (only with
/// `room`: a change of anything else isn't held back for room). `service_id` is
/// None for a service not made yet (no volumes or replicas of its own).
pub async fn check(
    tx: &mut Transaction<'_, Postgres>,
    cluster_id: &str,
    service_id: Option<&str>,
    spec: &Value,
    room: bool,
) -> ApiResult<()> {
    let p = Placement::of(spec);
    if p.any() {
        return Ok(());
    }
    let servers = servers(tx, cluster_id).await?;
    let mut volume = None;
    let mut own: HashMap<String, (f64, f64)> = HashMap::new();
    let r = &spec["resources"];
    let need = Need {
        gpu: r["gpu"].as_u64().unwrap_or(0) > 0,
        cpu: if room {
            r["cpuRequest"].as_i64().unwrap_or(100).max(0) as f64 / 1000.0
        } else {
            0.0
        },
        memory_gi: if room {
            r["memoryRequest"].as_i64().unwrap_or(128).max(0) as f64 / 1024.0
        } else {
            0.0
        },
    };
    if let Some(id) = service_id {
        volume = pinned_volume(tx, cluster_id, id).await?;
        let runtime: Value =
            sqlx::query_scalar("SELECT coalesce(runtime, '{}') FROM pyo.services WHERE id = $1")
                .bind(id)
                .fetch_optional(&mut **tx)
                .await?
                .unwrap_or(Value::Null);
        // What its replicas hold now, by what the live version asks for.
        let old: Value = sqlx::query_scalar("SELECT spec FROM pyo.services WHERE id = $1")
            .bind(id)
            .fetch_optional(&mut **tx)
            .await?
            .unwrap_or(Value::Null);
        let each = (
            old["resources"]["cpuRequest"]
                .as_i64()
                .unwrap_or(100)
                .max(0) as f64
                / 1000.0,
            old["resources"]["memoryRequest"]
                .as_i64()
                .unwrap_or(128)
                .max(0) as f64
                / 1024.0,
        );
        for rep in runtime["replicas"].as_array().into_iter().flatten() {
            if let Some(s) = rep["server"].as_str().filter(|s| !s.is_empty()) {
                let e = own.entry(s.to_owned()).or_default();
                e.0 += each.0;
                e.1 += each.1;
            }
        }
    }
    let vol = volume.as_ref().map(|(n, s)| (n.as_str(), s.as_str()));
    match refusal(&p, &servers, vol, need, &own) {
        Some(why) => Err(ApiError::bad_request(why)),
        None => Ok(()),
    }
}

/// One of the service's volumes whose data stays on one server's disk:
/// its name and that server.
async fn pinned_volume(
    tx: &mut Transaction<'_, Postgres>,
    cluster_id: &str,
    service_id: &str,
) -> ApiResult<Option<(String, String)>> {
    let rows: Vec<(String, Value)> = sqlx::query_as(
        "SELECT data->>'name', data FROM pyo.volumes WHERE service_id = $1 AND NOT deleting ORDER BY created_at",
    )
    .bind(service_id)
    .fetch_all(&mut **tx)
    .await?;
    for (name, data) in rows {
        if let Some(server) = crate::volumes::pinned_server(tx, cluster_id, &data).await? {
            return Ok(Some((name, server)));
        }
    }
    Ok(None)
}

/// A placement change on a deployed service, live: its live deployment
/// takes the new placement (so a rollback to it or a failed rollout's
/// fallback keeps it) and the agent moves its workload in place. Returns
/// the cluster to wake, or None when nothing runs yet.
pub async fn go_live(
    tx: &mut Transaction<'_, Postgres>,
    service_id: &str,
    spec: &Value,
) -> ApiResult<Option<String>> {
    let (cluster_id, current): (String, Option<String>) =
        sqlx::query_as("SELECT cluster_id, current_deployment_id FROM pyo.services WHERE id = $1")
            .bind(service_id)
            .fetch_one(&mut **tx)
            .await?;
    let in_flight: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM pyo.deployments WHERE service_id = $1 AND status NOT IN ('succeeded', 'failed', 'cancelled'))")
        .bind(service_id)
        .fetch_one(&mut **tx)
        .await?;
    if in_flight {
        return Err(ApiError::conflict(
            "A deploy is running. Change where it runs once it finishes.",
        ));
    }
    let Some(current) = current else {
        return Ok(None);
    };
    let p = Placement::of(spec);
    let live: Value = sqlx::query_scalar("SELECT spec FROM pyo.deployments WHERE id = $1")
        .bind(&current)
        .fetch_one(&mut **tx)
        .await?;
    let mut live = live;
    live_placement(&mut live, &p);
    sqlx::query("UPDATE pyo.deployments SET spec = $2 WHERE id = $1")
        .bind(&current)
        .bind(&live)
        .execute(&mut **tx)
        .await?;
    // A static site runs on the cluster's static server, not on its own.
    if !live["site"].is_null() {
        return Ok(None);
    }
    let service = crate::ops::service_ref(tx, service_id).await?;
    let cron_jobs: Vec<CronTask> =
        serde_json::from_value(spec["cronJobs"].clone()).unwrap_or_default();
    crate::deploy::enqueue(
        tx,
        &cluster_id,
        Command::Place {
            service,
            server_names: p.servers,
            server_tags: p.tags,
            cron_jobs,
        },
    )
    .await?;
    Ok(Some(cluster_id))
}

/// A stored rollout spec (a `DeploySpec`) with this placement: its servers
/// and tags, and no tag labels left in its node selector.
pub fn live_placement(deploy_spec: &mut Value, p: &Placement) {
    deploy_spec["serverNames"] = json!(p.servers);
    deploy_spec["serverTags"] = json!(p.tags);
    if let Some(sel) = deploy_spec
        .get_mut("nodeSelector")
        .and_then(Value::as_object_mut)
    {
        sel.retain(|k, _| !k.starts_with(pyo_proto::TAG_LABEL_PREFIX));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn server(name: &str, tags: &[&str], report: Value) -> Server {
        Server {
            name: name.into(),
            tags: tags.iter().map(|t| t.to_string()).collect(),
            report,
        }
    }

    fn room(cpu: f64, mem: f64, used: (f64, f64)) -> Value {
        json!({ "status": "ready", "allocatable": { "cpu": cpu, "memoryGi": mem }, "requested": { "cpu": used.0, "memoryGi": used.1 } })
    }

    fn cluster() -> Vec<Server> {
        let mut gpu = room(16.0, 64.0, (1.0, 2.0));
        gpu["gpuDetected"] = json!(true);
        vec![
            server("server-1", &["eu"], room(4.0, 4.0, (1.0, 1.0))),
            server("gpu-box", &["gpu"], gpu),
            server("small-1", &[], room(2.0, 2.0, (1.9, 1.9))),
        ]
    }

    fn pick(servers: &[&str], tags: &[&str]) -> Placement {
        Placement {
            servers: servers.iter().map(|s| s.to_string()).collect(),
            tags: tags.iter().map(|s| s.to_string()).collect(),
        }
    }

    const SMALL: Need = Need {
        gpu: false,
        cpu: 0.1,
        memory_gi: 0.125,
    };

    #[test]
    fn old_server_tags_read_as_tags_picked() {
        let p = Placement::of(&json!({ "serverTags": ["gpu", "GPU", "eu"], "servers": ["a"] }));
        assert_eq!(p, pick(&["a"], &["gpu", "eu"]));
        // A placement saved wins over what's left of the old fields.
        let p = Placement::of(&json!({ "placement": null, "serverTags": ["gpu"] }));
        assert!(p.any());
        let p = Placement::of(&json!({ "placement": { "tags": ["eu"] }, "serverTags": ["gpu"] }));
        assert_eq!(p, pick(&[], &["eu"]));
        assert!(Placement::of(&json!({})).any());
    }

    #[test]
    fn saved_as_placement_only() {
        let mut spec = json!({ "name": "web", "serverTags": ["gpu"], "servers": [] });
        normalize(&mut spec).unwrap();
        assert_eq!(
            spec,
            json!({ "name": "web", "placement": { "servers": [], "tags": ["gpu"] } })
        );
        // Any server: nothing kept.
        let mut spec = json!({ "name": "web", "placement": { "servers": [], "tags": [] }, "serverTags": ["gpu"] });
        normalize(&mut spec).unwrap();
        assert_eq!(spec, json!({ "name": "web" }));
        let mut spec = json!({ "placement": { "tags": ["no way"] } });
        assert!(normalize(&mut spec).is_err());
    }

    #[test]
    fn servers_and_tags_add_up() {
        let p = pick(&["small-1"], &["gpu"]);
        assert!(p.takes("small-1", &[]));
        assert!(p.takes("gpu-box", &["gpu".into()]));
        assert!(p.takes("later", &["gpu".into()]), "tagged later");
        assert!(!p.takes("server-1", &["eu".into()]));
        assert!(Placement::default().takes("anything", &[]));
    }

    #[test]
    fn any_server_is_never_refused() {
        let need = Need {
            gpu: true,
            cpu: 99.0,
            memory_gi: 99.0,
        };
        assert_eq!(
            refusal(
                &Placement::default(),
                &cluster(),
                Some(("data", "x")),
                need,
                &HashMap::new()
            ),
            None
        );
    }

    #[test]
    fn an_empty_set_is_refused() {
        let why = refusal(
            &pick(&[], &["arm"]),
            &cluster(),
            None,
            SMALL,
            &HashMap::new(),
        );
        assert_eq!(
            why.as_deref(),
            Some("No server has the tag arm yet. Tag one first, or pick servers.")
        );
        let why = refusal(
            &pick(&["gone"], &[]),
            &cluster(),
            None,
            SMALL,
            &HashMap::new(),
        );
        assert_eq!(why.as_deref(), Some("gone isn't a server of this cluster."));
        assert_eq!(
            refusal(
                &pick(&["server-1"], &[]),
                &cluster(),
                None,
                SMALL,
                &HashMap::new()
            ),
            None
        );
    }

    #[test]
    fn a_volume_keeps_its_server() {
        let why = refusal(
            &pick(&["small-1"], &[]),
            &cluster(),
            Some(("data", "server-1")),
            SMALL,
            &HashMap::new(),
        );
        assert_eq!(
            why.as_deref(),
            Some("Its volume data lives on server-1, so it has to run there. Pick server-1 too.")
        );
        // Picked through a tag it has: fine.
        assert_eq!(
            refusal(
                &pick(&["small-1"], &["eu"]),
                &cluster(),
                Some(("data", "server-1")),
                SMALL,
                &HashMap::new()
            ),
            None
        );
    }

    #[test]
    fn a_gpu_service_needs_a_gpu_server() {
        let gpu = Need { gpu: true, ..SMALL };
        let why = refusal(
            &pick(&["server-1", "small-1"], &[]),
            &cluster(),
            None,
            gpu,
            &HashMap::new(),
        );
        assert_eq!(
            why.as_deref(),
            Some("It asks for a GPU, and none of these servers has one. Pick a GPU server.")
        );
        assert_eq!(
            refusal(
                &pick(&["server-1"], &["gpu"]),
                &cluster(),
                None,
                gpu,
                &HashMap::new()
            ),
            None
        );
        // A server kept for GPU work takes nothing else.
        let mut servers = cluster();
        servers[1].report["gpuDedicated"] = json!(true);
        let why = refusal(
            &pick(&["gpu-box"], &[]),
            &servers,
            None,
            SMALL,
            &HashMap::new(),
        );
        assert!(why.unwrap().contains("only run services that use a GPU"));
    }

    #[test]
    fn no_room_is_refused_counting_what_it_gives_back() {
        let big = Need {
            gpu: false,
            cpu: 0.5,
            memory_gi: 0.5,
        };
        let why = refusal(
            &pick(&["small-1"], &[]),
            &cluster(),
            None,
            big,
            &HashMap::new(),
        );
        assert_eq!(
            why.as_deref(),
            Some(
                "None of these servers has room for it. It needs about 0.5 CPU and 0.5 GB of memory."
            )
        );
        // Its own replica there now frees its share when it moves.
        let own = HashMap::from([("small-1".to_owned(), (0.5, 0.5))]);
        assert_eq!(
            refusal(&pick(&["small-1"], &[]), &cluster(), None, big, &own),
            None
        );
        // A server that hasn't said what it has: not refused.
        let mut servers = cluster();
        servers[2].report = json!({});
        assert_eq!(
            refusal(
                &pick(&["small-1"], &[]),
                &servers,
                None,
                big,
                &HashMap::new()
            ),
            None
        );
        // A build-only server isn't one to run on.
        servers[2].report = json!({ "builder": true });
        assert!(
            refusal(
                &pick(&["small-1"], &[]),
                &servers,
                None,
                big,
                &HashMap::new()
            )
            .unwrap()
            .contains("only run builds")
        );
    }

    #[test]
    fn the_live_version_takes_the_placement() {
        let mut live = json!({ "image": "web:1", "nodeSelector": { "pyo/tag-fast": "true", "topology.kubernetes.io/region": "sg" }, "serverNames": ["a"] });
        live_placement(&mut live, &pick(&[], &["gpu"]));
        assert_eq!(live["serverNames"], json!([]));
        assert_eq!(live["serverTags"], json!(["gpu"]));
        assert_eq!(
            live["nodeSelector"],
            json!({ "topology.kubernetes.io/region": "sg" })
        );
        assert_eq!(live["image"], "web:1");
        let d: pyo_proto::DeploySpec = serde_json::from_value(json!({
            "deploymentId": "dpl_1", "image": "web:1", "replicas": 1, "env": [], "ports": [], "domains": [], "volumes": [],
            "service": { "serviceId": "svc_1", "name": "web", "namespace": "shop", "workloadKind": "Deployment" },
            "projectId": "prj_1", "timeoutSeconds": 60,
            "resources": { "cpuRequest": 100, "cpuLimit": 500, "memoryRequest": 64, "memoryLimit": 128 },
            "healthCheck": { "type": "none", "path": "", "port": 0, "command": "", "initialDelaySeconds": 1, "periodSeconds": 1 },
            "serverTags": live["serverTags"],
        }))
        .unwrap();
        assert_eq!(d.server_tags, vec!["gpu".to_owned()]);
    }
}
