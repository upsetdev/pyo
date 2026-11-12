//! A database's analytics: its members' CPU, memory and disk, and its
//! engine's figures (throughput, connections, cache hits, replication lag,
//! size), over a time range. The agent reads each member's metrics page
//! (dbmetrics.rs there); observability.rs stores the figures in ClickHouse
//! (`database_metrics`, kept 30 days) and reads them back per bucket; here
//! they become series for the dashboard. Team members can read it, like
//! service metrics, and it's an MCP tool like them.

use std::collections::BTreeMap;

use axum::{
    Json,
    extract::{Path, Query, State},
};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{
    AppState,
    auth::SessionUser,
    error::{ApiResult, ErrorBody},
    observability::DatabaseRows,
    ops::{Role, require_role, team_of},
};

#[derive(Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub struct AnalyticsQuery {
    /// "1h", "24h" (the default), "7d" or "30d".
    range: Option<String>,
}

/// A database's analytics
///
/// Its health over time, sampled about every 30 seconds. Per member
/// (`members`: each instance, read replica, pooler and KV endpoint), its
/// `role`, `server`, and series for `cpu` in cores, `memoryMi`,
/// `diskUsedGi` (only where the volume reports its own space),
/// `connections` and `lagSeconds`. For the whole database, `transactions`
/// (Postgres), `queries` (MySQL), `poolerQueries` and `commands` (KV) per
/// second, `connections` (`primary`, `replicas`, `limit`, and the pooler's
/// `poolerActive`, `poolerWaiting`, `poolerServersActive`,
/// `poolerServersIdle` and `poolerMaxWaitSeconds`), `cacheHitRatio`,
/// `keyHitRatio`, `evictions` per second, `routing` (KV reads per second
/// served by `replica`s or the `primary`, and `fallbacks` to the primary)
/// and `storage` (`sizeBytes`, `memoryUsedBytes`, `maxmemoryBytes`,
/// `keys`). Each series lines up with `times`, with `null` where nothing
/// was measured. A series is `null` when the database doesn't report it.
#[utoipa::path(get, path = "/databases/{id}/analytics", operation_id = "getDatabaseAnalytics", tag = "Databases",
    params(("id" = String, Path, description = "The database's id."), AnalyticsQuery),
    responses(
        (status = 200, description = "`range`, `stepMs`, `times` (milliseconds), `engine`, `members` and the database's series.", body = Object),
        (status = 404, description = "No such database, or you aren't in its team.", body = ErrorBody),
        (status = 503, description = "This installation doesn't store metrics.", body = ErrorBody),
    ),
    extensions(("x-pyo-access" = json!("read"))))]
pub async fn analytics(
    State(s): State<AppState>,
    SessionUser(user): SessionUser,
    Path(id): Path<String>,
    Query(q): Query<AnalyticsQuery>,
) -> ApiResult<Json<Value>> {
    let mut tx = s.db.begin().await?;
    let team_id = team_of(&mut tx, "databases", &id).await?;
    require_role(&mut tx, &user, &team_id, Role::Viewer).await?;
    let engine: String = sqlx::query_scalar("SELECT engine FROM pyo.databases WHERE id = $1")
        .bind(&id)
        .fetch_one(&mut *tx)
        .await?;
    tx.commit().await?;
    let range = q.range.as_deref().unwrap_or("24h");
    let rows = s
        .observability
        .database_metrics(&team_id, &id, range)
        .await?;
    Ok(Json(shape(&engine, range, &rows)))
}

/// The order members are listed in.
fn rank(component: &str, role: &str) -> u8 {
    match (component, role) {
        ("instance", "primary") => 0,
        ("instance", _) => 1,
        ("read", _) => 2,
        ("pooler", _) => 3,
        ("read-pooler", _) => 4,
        ("proxy", _) => 5,
        _ => 6,
    }
}

/// Members that hold data (instances and read replicas), not entry points.
fn holds_data(component: &str) -> bool {
    component == "instance" || component == "read"
}

struct Member {
    component: String,
    role: String,
    node: String,
    /// metric -> per bucket (average, worst).
    cells: BTreeMap<String, Vec<Option<(f64, f64)>>>,
}

fn round(x: f64) -> f64 {
    (x * 1000.0).round() / 1000.0
}

/// A missed sample or two is bridged with the last value; a longer
/// silence stays a gap (null), as in server metrics.
fn bridge(v: Vec<Option<f64>>) -> Vec<Option<f64>> {
    let mut last = None;
    let mut missed = 0;
    v.into_iter()
        .map(|x| match x {
            Some(x) => {
                missed = 0;
                last = Some(x);
                last
            }
            None => {
                missed += 1;
                if missed <= 2 { last } else { None }
            }
        })
        .collect()
}

/// Stored rows -> the endpoint's answer.
pub fn shape(engine: &str, range: &str, d: &DatabaseRows) -> Value {
    let Some(first) = d.rows.iter().filter_map(|r| r["b"].as_i64()).min() else {
        return json!({ "range": range, "stepMs": d.step * 1000, "times": [], "engine": engine, "members": [] });
    };
    let from = ((first - d.start) / d.step).clamp(0, d.points - 1);
    let times: Vec<i64> = (from..d.points)
        .map(|i| (d.start + i * d.step) * 1000)
        .collect();
    let n = times.len();

    let mut members: BTreeMap<String, Member> = BTreeMap::new();
    for r in &d.rows {
        let (Some(b), Some(name), Some(metric)) =
            (r["b"].as_i64(), r["member"].as_str(), r["metric"].as_str())
        else {
            continue;
        };
        let i = (b - d.start) / d.step - from;
        if i < 0 || i as usize >= n {
            continue;
        }
        let m = members.entry(name.to_owned()).or_insert_with(|| Member {
            component: String::new(),
            role: String::new(),
            node: String::new(),
            cells: BTreeMap::new(),
        });
        // Rows come oldest first: the latest bucket's role and server win.
        m.component = r["component"].as_str().unwrap_or("").to_owned();
        m.role = r["role"].as_str().unwrap_or("").to_owned();
        m.node = r["node"].as_str().unwrap_or("").to_owned();
        let (Some(v), mx) = (
            r["v"].as_f64().filter(|x| x.is_finite()),
            r["mx"].as_f64().filter(|x| x.is_finite()),
        ) else {
            continue;
        };
        m.cells
            .entry(metric.to_owned())
            .or_insert_with(|| vec![None; n])[i as usize] = Some((v, mx.unwrap_or(v)));
    }

    let has = |metric: &str, pick: &dyn Fn(&Member) -> bool| {
        members
            .values()
            .any(|m| pick(m) && m.cells.contains_key(metric))
    };
    // Per bucket, over the members `pick` chooses: their sum, or null where none reported.
    let sum = |metric: &str, pick: &dyn Fn(&Member) -> bool| -> Value {
        if !has(metric, pick) {
            return Value::Null;
        }
        let v: Vec<Option<f64>> = (0..n)
            .map(|i| {
                let vs: Vec<f64> = members
                    .values()
                    .filter(|m| pick(m))
                    .filter_map(|m| m.cells.get(metric).and_then(|c| c[i]).map(|x| x.0))
                    .collect();
                (!vs.is_empty()).then(|| round(vs.iter().sum()))
            })
            .collect();
        json!(bridge(v))
    };
    let max = |metric: &str, pick: &dyn Fn(&Member) -> bool| -> Value {
        if !has(metric, pick) {
            return Value::Null;
        }
        let v: Vec<Option<f64>> = (0..n)
            .map(|i| {
                members
                    .values()
                    .filter(|m| pick(m))
                    .filter_map(|m| m.cells.get(metric).and_then(|c| c[i]).map(|x| x.0))
                    .reduce(f64::max)
                    .map(round)
            })
            .collect();
        json!(bridge(v))
    };
    // hits / (hits + misses), from the members' rates in each bucket.
    let ratio = |hits: &str, misses: &str, pick: &dyn Fn(&Member) -> bool| -> Value {
        if !has(hits, pick) {
            return Value::Null;
        }
        let at = |metric: &str, i: usize| {
            members
                .values()
                .filter(|m| pick(m))
                .filter_map(|m| m.cells.get(metric).and_then(|c| c[i]).map(|x| x.0))
                .fold(None, |a: Option<f64>, x| Some(a.unwrap_or(0.0) + x))
        };
        let v: Vec<Option<f64>> = (0..n)
            .map(|i| {
                let h = at(hits, i)?;
                let total = h + at(misses, i).unwrap_or(0.0);
                (total > 0.0).then(|| (h / total * 10000.0).round() / 10000.0)
            })
            .collect();
        json!(bridge(v))
    };
    // The newest value of a figure among the chosen members.
    let latest = |metric: &str, pick: &dyn Fn(&Member) -> bool| -> Value {
        members
            .values()
            .filter(|m| pick(m))
            .filter_map(|m| {
                m.cells
                    .get(metric)
                    .and_then(|c| c.iter().rev().flatten().next())
                    .map(|x| x.0)
            })
            .reduce(f64::max)
            .map(round)
            .map(Value::from)
            .unwrap_or(Value::Null)
    };

    let data = |m: &Member| holds_data(&m.component);
    let primary = |m: &Member| m.component == "instance" && m.role == "primary";
    let others = |m: &Member| {
        holds_data(&m.component) && !(m.component == "instance" && m.role == "primary")
    };
    let pooler = |m: &Member| m.component == "pooler" || m.component == "read-pooler";
    let all = |_: &Member| true;

    let mut list: Vec<(&String, &Member)> = members.iter().collect();
    list.sort_by(|a, b| {
        rank(&a.1.component, &a.1.role)
            .cmp(&rank(&b.1.component, &b.1.role))
            .then_with(|| a.0.cmp(b.0))
    });
    let series = |m: &Member, metric: &str, worst: bool| -> Value {
        match m.cells.get(metric) {
            Some(c) => json!(
                c.iter()
                    .map(|x| x.map(|(v, mx)| round(if worst { mx } else { v })))
                    .collect::<Vec<_>>()
            ),
            None => Value::Null,
        }
    };
    let member_values: Vec<Value> = list
        .iter()
        .map(|(name, m)| {
            json!({
                "name": name, "component": m.component, "role": m.role, "server": m.node,
                "cpu": series(m, "cpu", false), "memoryMi": series(m, "memory_mi", false),
                "diskUsedGi": series(m, "disk_used_gi", false),
                "diskCapacityGi": m.cells.get("disk_capacity_gi").and_then(|c| c.iter().rev().flatten().next()).map(|x| round(x.0)),
                "connections": series(m, "connections", false),
                // The worst in each bucket: a spike shouldn't average away.
                "lagSeconds": if m.role == "primary" { Value::Null } else { series(m, "lag_seconds", true) },
            })
        })
        .collect();

    json!({
        "range": range,
        "stepMs": d.step * 1000,
        "times": times,
        "engine": engine,
        "members": member_values,
        "transactions": sum("transactions", &data),
        "queries": sum("queries", &data),
        "poolerQueries": sum("pooler_queries", &pooler),
        "commands": sum("commands", &data),
        "connections": {
            "primary": sum("connections", &primary),
            "replicas": sum("connections", &others),
            "limit": latest("max_connections", &data),
            "poolerActive": sum("pooler_active", &pooler),
            "poolerWaiting": sum("pooler_waiting", &pooler),
            "poolerServersActive": sum("pooler_servers_active", &pooler),
            "poolerServersIdle": sum("pooler_servers_idle", &pooler),
            "poolerMaxWaitSeconds": max("pooler_maxwait_seconds", &pooler),
        },
        "cacheHitRatio": ratio("cache_hits", "cache_misses", &data),
        "keyHitRatio": ratio("key_hits", "key_misses", &data),
        "evictions": sum("evictions", &data),
        "routing": { "replica": sum("reads_replica", &all), "primary": sum("reads_primary", &all), "fallbacks": sum("replica_fallbacks", &all) },
        "storage": {
            "sizeBytes": max("size_bytes", &data),
            "memoryUsedBytes": max("memory_used_bytes", &primary),
            "maxmemoryBytes": latest("maxmemory_bytes", &data),
            // Every member holds every key: the primary's count.
            "keys": max("keys", &primary),
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mysql_members() {
        let d = DatabaseRows {
            start: 0,
            step: 60,
            points: 60,
            rows: vec![
                row(60, "shop-0", "instance", "primary", "cpu", 0.5),
                row(60, "shop-1", "instance", "replica", "lag_seconds", 1.0),
            ],
        };
        let v = shape("mysql", "1h", &d);
        let names: Vec<&str> = v["members"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, ["shop-0", "shop-1"]);
        // The replica's lag, the worst in its bucket.
        assert_eq!(v["members"][1]["lagSeconds"][0], json!(2.0));
    }

    fn row(b: i64, member: &str, component: &str, role: &str, metric: &str, v: f64) -> Value {
        json!({ "b": b, "member": member, "metric": metric, "component": component, "role": role, "node": "s1", "v": v, "mx": v * 2.0 })
    }

    #[test]
    fn shapes_series() {
        let start = 1000;
        let rows = vec![
            row(1060, "pg-1", "instance", "primary", "transactions", 10.0),
            row(1060, "pg-2", "instance", "replica", "transactions", 2.0),
            row(1120, "pg-1", "instance", "primary", "transactions", 12.0),
            row(1060, "pg-1", "instance", "primary", "cache_hits", 90.0),
            row(1060, "pg-1", "instance", "primary", "cache_misses", 5.0),
            row(1060, "pg-2", "instance", "replica", "cache_hits", 5.0),
            row(1060, "pg-1", "instance", "primary", "connections", 7.0),
            row(1060, "pg-2", "instance", "replica", "connections", 3.0),
            row(1060, "pg-read-1", "read", "", "connections", 1.0),
            row(
                1060,
                "pg-1",
                "instance",
                "primary",
                "max_connections",
                100.0,
            ),
            row(1060, "pg-2", "instance", "replica", "lag_seconds", 0.5),
            row(1060, "pg-pooler-a", "pooler", "", "pooler_waiting", 1.0),
            row(1060, "pg-pooler-b", "pooler", "", "pooler_waiting", 2.0),
            row(1060, "pg-1", "instance", "primary", "size_bytes", 5e6),
        ];
        let d = DatabaseRows {
            start,
            step: 60,
            points: 5,
            rows,
        };
        let v = shape("postgres", "1h", &d);
        // From the first bucket with data.
        assert_eq!(
            v["times"],
            json!([1_060_000, 1_120_000, 1_180_000, 1_240_000])
        );
        assert_eq!(v["transactions"], json!([12.0, 12.0, 12.0, 12.0]));
        assert_eq!(v["cacheHitRatio"][0], json!(0.95));
        assert_eq!(v["connections"]["primary"][0], json!(7.0));
        assert_eq!(v["connections"]["replicas"][0], json!(4.0));
        assert_eq!(v["connections"]["limit"], json!(100.0));
        assert_eq!(v["connections"]["poolerWaiting"][0], json!(3.0));
        assert_eq!(v["queries"], Value::Null);
        assert_eq!(v["keyHitRatio"], Value::Null);
        assert_eq!(v["storage"]["sizeBytes"][0], json!(5e6));
        // Primary first; a replica's lag is its worst in the bucket.
        let names: Vec<&str> = v["members"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["name"].as_str().unwrap())
            .collect();
        assert_eq!(
            names,
            ["pg-1", "pg-2", "pg-read-1", "pg-pooler-a", "pg-pooler-b"]
        );
        assert_eq!(v["members"][1]["lagSeconds"][0], json!(1.0));
        assert_eq!(v["members"][0]["lagSeconds"], Value::Null);
    }

    #[test]
    fn gaps_are_bridged_briefly() {
        assert_eq!(
            bridge(vec![Some(1.0), None, None, None, Some(2.0)]),
            vec![Some(1.0), Some(1.0), Some(1.0), None, Some(2.0)]
        );
    }

    #[test]
    fn nothing_yet() {
        let v = shape(
            "valkey",
            "24h",
            &DatabaseRows {
                start: 0,
                step: 900,
                points: 96,
                rows: vec![],
            },
        );
        assert_eq!(v["times"], json!([]));
    }
}
