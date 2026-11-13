//! Whether a service's replicas fit on its servers. Pyo doesn't cap
//! replicas: the ones that don't fit wait until there's room (Kubernetes
//! keeps them pending), and the change that asked for them says so.

use serde_json::Value;
use sqlx::{Postgres, Transaction};

use crate::error::ApiResult;

/// What the warning is about.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Ask {
    /// Replicas that start now: scaling, deploying.
    Run,
    /// Autoscaling's maximum: replicas that may start later.
    UpTo,
}

/// Free CPU (cores) and memory (GiB) on the servers this service can run
/// on, from their last reports: what Kubernetes can hand out less what
/// pods there ask for. None when no server has reported any.
pub fn free_for(spec: &Value, servers: &[(String, Value, Vec<String>)]) -> Option<(f64, f64)> {
    let gpu = spec["resources"]["gpu"].as_u64().is_some_and(|g| g > 0);
    let named: Vec<&str> = spec["servers"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|n| n.as_str())
        .filter(|n| !n.is_empty())
        .collect();
    let tags: Vec<&str> = spec["serverTags"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|t| t.as_str())
        .collect();
    let mut seen = false;
    let (mut cpu, mut mem) = (0.0, 0.0);
    for (name, r, have) in servers {
        let usable = r["status"] == "ready"
            && r["schedulable"] == true
            && r["maintenance"] != true
            && r["builder"] != true
            && (gpu || r["gpuDedicated"] != true)
            && (named.is_empty() || named.contains(&name.as_str()))
            && tags.iter().all(|t| have.iter().any(|h| h == t));
        let alloc = (
            r["allocatable"]["cpu"].as_f64().unwrap_or(0.0),
            r["allocatable"]["memoryGi"].as_f64().unwrap_or(0.0),
        );
        if alloc.0 <= 0.0 {
            continue;
        }
        seen = true;
        if usable {
            cpu += (alloc.0 - r["requested"]["cpu"].as_f64().unwrap_or(0.0)).max(0.0);
            mem += (alloc.1 - r["requested"]["memoryGi"].as_f64().unwrap_or(0.0)).max(0.0);
        }
    }
    seen.then_some((cpu, mem))
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

/// The warning for `replicas` of a service asking `cpu_m` millicores and
/// `memory_mi` MiB each, when they won't fit: `running` of its replicas
/// already hold their share of what the servers have, and `free` is the
/// rest. None when they fit.
pub fn warning(
    ask: Ask,
    replicas: i64,
    cpu_m: i64,
    memory_mi: i64,
    running: i64,
    free: (f64, f64),
) -> Option<String> {
    if replicas < 1 {
        return None;
    }
    let (each_cpu, each_mem) = (
        cpu_m.max(0) as f64 / 1000.0,
        memory_mi.max(0) as f64 / 1024.0,
    );
    let own = running.clamp(0, replicas) as f64;
    let (need_cpu, need_mem) = (each_cpu * replicas as f64, each_mem * replicas as f64);
    let (room_cpu, room_mem) = (free.0 + each_cpu * own, free.1 + each_mem * own);
    // A little slack: the servers' figures are rounded.
    let short_cpu = need_cpu > room_cpu + 0.01;
    let short_mem = need_mem > room_mem + 0.01;
    let (need, room) = match (short_cpu, short_mem) {
        (false, false) => return None,
        (true, false) => (format!("{} CPU", amount(need_cpu)), amount(room_cpu)),
        (false, true) => (
            format!("{} GB of memory", amount(need_mem)),
            format!("{} GB", amount(room_mem)),
        ),
        (true, true) => (
            format!(
                "{} CPU and {} GB of memory",
                amount(need_cpu),
                amount(need_mem)
            ),
            format!("{} CPU and {} GB", amount(room_cpu), amount(room_mem)),
        ),
    };
    Some(match ask {
        Ask::Run if replicas == 1 => format!(
            "This replica needs about {need}. Your servers have {room} free, so it will wait until there's room."
        ),
        Ask::Run => format!(
            "These {replicas} replicas need about {need}. Your servers have {room} free, so some will wait until there's room."
        ),
        Ask::UpTo => format!(
            "At {replicas} replicas this service needs about {need}. Your servers have {room} free, so some would wait until there's room."
        ),
    })
}

/// The warning for running `replicas` of a service as it's set up now
/// (its requests and where it may run), or None when they fit or its
/// servers haven't said what they have. No `replicas`: as many as a deploy
/// of it starts.
pub async fn check(
    tx: &mut Transaction<'_, Postgres>,
    service_id: &str,
    ask: Ask,
    replicas: Option<i64>,
) -> ApiResult<Option<String>> {
    let Some((cluster_id, spec, runtime)): Option<(String, Value, Value)> =
        sqlx::query_as("SELECT cluster_id, spec, runtime FROM pyo.services WHERE id = $1")
            .bind(service_id)
            .fetch_optional(&mut **tx)
            .await?
    else {
        return Ok(None);
    };
    if spec["workloadKind"] == "CronJob" {
        return Ok(None);
    }
    let servers: Vec<(String, Value, Vec<String>)> = sqlx::query_as(
        "SELECT n.name, n.report, coalesce(t.tags, '{}') FROM pyo.servers n
           LEFT JOIN pyo.server_tags t ON t.cluster_id = n.cluster_id AND t.name = n.name
          WHERE n.cluster_id = $1 AND n.removal IS NULL",
    )
    .bind(&cluster_id)
    .fetch_all(&mut **tx)
    .await?;
    let Some(free) = free_for(&spec, &servers) else {
        return Ok(None);
    };
    let r = &spec["resources"];
    Ok(warning(
        ask,
        replicas.unwrap_or_else(|| deployed_replicas(&spec)),
        r["cpuRequest"].as_i64().unwrap_or(100),
        r["memoryRequest"].as_i64().unwrap_or(128),
        runtime["readyReplicas"].as_i64().unwrap_or(0),
        free,
    ))
}

/// The replicas a deploy of the service starts: autoscaling's minimum, or
/// its setting.
fn deployed_replicas(spec: &Value) -> i64 {
    spec["autoscale"]["min"]
        .as_i64()
        .unwrap_or_else(|| spec["replicas"].as_i64().unwrap_or(1))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn server(name: &str, alloc: (f64, f64), used: (f64, f64)) -> (String, Value, Vec<String>) {
        (
            name.into(),
            json!({
                "status": "ready", "schedulable": true, "maintenance": false,
                "allocatable": { "cpu": alloc.0, "memoryGi": alloc.1 },
                "requested": { "cpu": used.0, "memoryGi": used.1 },
            }),
            vec![],
        )
    }

    #[test]
    fn free_room_is_allocatable_less_requests_on_usable_servers() {
        let mut down = server("c", (8.0, 16.0), (0.0, 0.0));
        down.1["status"] = json!("not_ready");
        let mut tagged = server("d", (4.0, 8.0), (0.0, 0.0));
        tagged.2 = vec!["fast".into()];
        let servers = vec![
            server("a", (4.0, 8.0), (1.0, 2.0)),
            // Over-asked: counts as none, not less than none.
            server("b", (2.0, 4.0), (3.0, 5.0)),
            down,
            tagged,
        ];
        assert_eq!(free_for(&json!({}), &servers), Some((7.0, 14.0)));
        assert_eq!(
            free_for(&json!({ "serverTags": ["fast"] }), &servers),
            Some((4.0, 8.0))
        );
        assert_eq!(
            free_for(&json!({ "servers": ["a"] }), &servers),
            Some((3.0, 6.0))
        );
        assert_eq!(free_for(&json!({}), &[]), None, "nothing reported");
    }

    #[test]
    fn replicas_that_fit_say_nothing() {
        // 10 × 0.5 CPU and 512 MiB, with 4 running and 3 CPU free: 5 ≤ 3 + 2.
        assert_eq!(warning(Ask::Run, 10, 500, 512, 4, (3.0, 10.0)), None);
        assert_eq!(warning(Ask::Run, 0, 500, 512, 0, (0.0, 0.0)), None);
    }

    #[test]
    fn replicas_that_dont_fit_say_how_much_is_missing() {
        assert_eq!(
            warning(Ask::Run, 120, 250, 128, 0, (16.0, 64.0)).as_deref(),
            Some(
                "These 120 replicas need about 30 CPU. Your servers have 16 free, so some will wait until there's room."
            )
        );
        assert_eq!(
            warning(Ask::Run, 4, 100, 1024, 0, (8.0, 2.5)).as_deref(),
            Some(
                "These 4 replicas need about 4 GB of memory. Your servers have 2.5 GB free, so some will wait until there's room."
            )
        );
        assert_eq!(
            warning(Ask::Run, 1, 2000, 4096, 0, (1.0, 1.0)).as_deref(),
            Some(
                "This replica needs about 2 CPU and 4 GB of memory. Your servers have 1 CPU and 1 GB free, so it will wait until there's room."
            )
        );
        assert_eq!(
            warning(Ask::UpTo, 50, 1000, 128, 2, (10.0, 100.0)).as_deref(),
            Some(
                "At 50 replicas this service needs about 50 CPU. Your servers have 12 free, so some would wait until there's room."
            )
        );
    }
}
