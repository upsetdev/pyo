//! Kept requests, read from the clusters. Each cluster's traffic store
//! keeps its requests, connections and WAF findings (pyo-agent's traffic/);
//! Pyo asks the cluster's agent for the newest that match
//! (`QueryTraffic`), up to 500 at a time, and passes them on without
//! keeping them: the command's row, with the answer, is deleted once read.
//! The request explorer, its live tail, and the latest refusals and
//! findings on the firewall's and WAF's pages read them here. Addresses and
//! query strings are shown as the team's settings keep them now
//! (teamobs.rs), also for rows a cluster kept before they changed.

use std::{collections::BTreeMap, time::Duration};

use pyo_proto::traffic::{TrafficAnswer, TrafficQuery};
use pyo_proto::{Command, CommandResult};
use serde_json::{Value, json};

use crate::{
    AppState, deploy,
    error::{ApiError, ApiResult},
    observability::LogFilter,
    teamobs::Effective,
};

/// How long a cluster gets to answer.
const WAIT: Duration = Duration::from_secs(15);

/// Asks one cluster. The error is a plain sentence for the dashboard.
pub async fn ask(s: &AppState, cluster_id: &str, q: TrafficQuery) -> Result<TrafficAnswer, String> {
    let mut tx = s.db.begin().await.map_err(|e| e.to_string())?;
    let id = deploy::enqueue(&mut tx, cluster_id, Command::QueryTraffic(Box::new(q)))
        .await
        .map_err(|e| e.message)?;
    tx.commit().await.map_err(|e| e.to_string())?;
    s.wake(cluster_id).await;
    let deadline = tokio::time::Instant::now() + WAIT;
    let forget = || async {
        let _ = sqlx::query("DELETE FROM pyo.commands WHERE id = $1").bind(&id).execute(&s.db).await;
    };
    loop {
        let row: Option<(String, Option<Value>)> =
            sqlx::query_as("SELECT status, result FROM pyo.commands WHERE id = $1")
                .bind(&id)
                .fetch_optional(&s.db)
                .await
                .map_err(|e| e.to_string())?;
        if let Some((status, result)) = row
            && (status == "done" || status == "failed")
        {
            forget().await;
            let r: CommandResult = result.and_then(|v| serde_json::from_value(v).ok()).unwrap_or_default();
            if !r.ok {
                return Err(r.message.unwrap_or_else(|| "The cluster couldn't read its requests.".into()));
            }
            return serde_json::from_value(r.data.unwrap_or_default())
                .map_err(|_| "The cluster's answer couldn't be read. Update its agent.".to_owned());
        }
        if tokio::time::Instant::now() > deadline {
            forget().await;
            return Err("The cluster didn't answer in time. Is it connected?".into());
        }
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
}

/// The clusters a team's hosts are on: cluster id -> (its name, hosts).
pub async fn hosts_by_cluster(
    s: &AppState,
    team_id: &str,
    hosts: &[String],
) -> ApiResult<BTreeMap<String, (String, Vec<String>)>> {
    let rows: Vec<(String, String, String)> = sqlx::query_as(
        "SELECT DISTINCT a.cluster_id, c.name, lower(d->>'hostname')
           FROM pyo.services a JOIN pyo.clusters c ON c.id = a.cluster_id, jsonb_array_elements(CASE WHEN jsonb_typeof(a.spec->'domains') = 'array' THEN a.spec->'domains' ELSE '[]' END) d
          WHERE a.team_id = $1 AND lower(d->>'hostname') = ANY($2)",
    )
    .bind(team_id)
    .bind(hosts)
    .fetch_all(&s.db)
    .await?;
    let mut out: BTreeMap<String, (String, Vec<String>)> = BTreeMap::new();
    for (cluster, name, host) in rows {
        let e = out.entry(cluster).or_insert_with(|| (name, vec![]));
        e.1.push(host);
    }
    Ok(out)
}

/// The same query to several clusters at once (each with its own part),
/// merged newest first. Returns the rows, whether there are older ones,
/// and the clusters that couldn't answer (name, why).
pub async fn ask_all(
    s: &AppState,
    asks: Vec<(String, String, TrafficQuery)>,
    limit: usize,
) -> (Vec<Value>, bool, Vec<(String, String)>) {
    let answers = futures::future::join_all(asks.into_iter().map(|(cluster, name, q)| async move {
        (name, ask(s, &cluster, q).await)
    }))
    .await;
    let mut rows = vec![];
    let mut more = false;
    let mut failed = vec![];
    for (name, a) in answers {
        match a {
            Ok(a) => {
                more |= a.more;
                rows.extend(a.rows);
            }
            Err(why) => failed.push((name, why)),
        }
    }
    rows.sort_by_key(|r| std::cmp::Reverse(r["at"].as_i64().unwrap_or(0)));
    if rows.len() > limit {
        more = true;
        rows.truncate(limit);
    }
    (rows, more, failed)
}

/// A kept URL as the team's query string setting shows it.
fn url_of(r: &Value, team: &Effective) -> String {
    pyo_proto::traffic::shape_url(r["url"].as_str().unwrap_or(""), team.query_strings.as_str())
}

/// A kept address as the team's IP setting shows it.
fn ip_of(r: &Value, team: &Effective) -> String {
    team.ip_mode.apply(r["ip"].as_str().unwrap_or(""))
}

/// A kept request as the request log shows it.
fn request_row(r: &Value, team: &Effective) -> Value {
    json!({
        "at": r["at"], "host": r["host"], "method": r["method"], "path": r["path"], "url": url_of(r, team),
        "status": r["status"], "durationMs": r["durationMs"].as_f64().unwrap_or(0.0).round(), "bytes": r["bytes"],
        "country": r["country"], "referrer": r["referrer"], "device": r["device"], "browser": r["browser"],
        "userAgent": r["userAgent"], "server": r["server"], "blockedBy": r["blockedBy"],
        "ip": ip_of(r, team),
    })
}

/// What the request log answers for a team that turned observability off:
/// its servers keep no requests meanwhile.
pub fn team_off() -> ApiError {
    ApiError::new(
        axum::http::StatusCode::SERVICE_UNAVAILABLE,
        "Observability is off for this team. An owner or admin can turn it on in Settings › Observability.",
    )
}

fn range_ms(range: &str) -> i64 {
    match range {
        "1h" => 3_600_000,
        "24h" => 86_400_000,
        "30d" => 30 * 86_400_000,
        _ => 7 * 86_400_000,
    }
}

/// The web server's request log for some of a team's hosts: newest
/// first, `limit` before `before`, from the clusters that keep them (and,
/// on a self-hosted Pyo, from what Pyo kept itself before 0.5). The first
/// page has the totals for the range when nothing but the website is
/// filtered.
pub async fn access_logs(
    s: &AppState,
    team_id: &str,
    hosts: &[String],
    services: &[String],
    f: &LogFilter,
    path: Option<String>,
) -> ApiResult<Value> {
    let obs = &s.observability;
    if !obs.enabled().await {
        return Err(crate::observability::off());
    }
    let team = obs.team(team_id).await;
    if !team.on {
        return Err(team_off());
    }
    let now = pyo_proto::now_ms();
    let since = now - range_ms(&f.range);
    let before = if f.before > 0 { f.before } else { now + 1 };
    let limit = f.limit.clamp(1, pyo_proto::traffic::MAX_ROWS) as usize;
    let clusters = hosts_by_cluster(s, team_id, hosts).await?;
    let base = TrafficQuery {
        rows: "requests".into(),
        since_ms: since,
        before_ms: before,
        limit: limit as u32,
        status: f.status.clone(),
        method: f.method.clone(),
        path: path.clone().filter(|p| !p.is_empty()),
        q: f.q.clone(),
        services: services.to_vec(),
        ..Default::default()
    };
    let asks = clusters
        .iter()
        .map(|(id, (name, hosts))| (id.clone(), name.clone(), TrafficQuery { hosts: hosts.clone(), ..base.clone() }))
        .collect();
    let (mut rows, mut more, failed) = ask_all(s, asks, limit).await;
    let mut logs: Vec<Value> = rows.drain(..).map(|r| request_row(&r, &team)).collect();
    // What Pyo kept itself before 0.5 (self-hosted), older than the clusters' rows.
    if obs.has_legacy().await && path.as_deref().unwrap_or("").is_empty() {
        let legacy = LogFilter { before, limit: limit as u32, ..f.clone() };
        if let Ok(old) = obs.legacy_access_logs(team_id, hosts, &legacy).await {
            logs.extend(old["logs"].as_array().cloned().unwrap_or_default());
            more |= old["next"].is_number();
            logs.sort_by_key(|r| std::cmp::Reverse(r["at"].as_i64().unwrap_or(0)));
            if logs.len() > limit {
                more = true;
                logs.truncate(limit);
            }
        }
    } else if !clusters.is_empty() && failed.len() == clusters.len() {
        return Err(ApiError::new(axum::http::StatusCode::GATEWAY_TIMEOUT, failed[0].1.clone()));
    }
    let next = if more { logs.last().and_then(|l| l["at"].as_i64()) } else { None };
    let unfiltered = f.status.is_none() && f.method.is_none() && f.q.as_deref().unwrap_or("").trim().is_empty() && path.is_none();
    let summary = if f.before == 0 && unfiltered {
        obs.request_summary(team_id, hosts, services, since / 1000).await.ok().flatten()
    } else {
        None
    };
    Ok(json!({
        "logs": logs,
        "next": next,
        "ipMode": team.ip_mode,
        "queryStrings": team.query_strings,
        "requestLogs": team.request_logs,
        "summary": summary,
        "unavailable": failed.iter().map(|(name, why)| json!({ "cluster": name, "error": why })).collect::<Vec<_>>(),
    }))
}

/// The latest the firewall turned away for a service, and their busiest
/// sources (among the latest 500): requests to its websites and
/// connections to its ports, from its cluster. Best effort: empty, with
/// `recentError`, when the cluster can't answer.
pub async fn blocked_recent(s: &AppState, service_id: &str, range: &str, out: &mut Value) -> ApiResult<()> {
    let Some((team_id, cluster, hosts, ports)) = service_targets(s, service_id).await? else {
        return Ok(());
    };
    let team = s.observability.team(&team_id).await;
    if !team.on {
        return Ok(());
    }
    let since = pyo_proto::now_ms() - range_ms(range);
    let mut asks = vec![];
    if !hosts.is_empty() {
        asks.push((cluster.clone(), String::new(), TrafficQuery { rows: "requests".into(), hosts, services: vec![service_id.to_owned()], blocked: true, since_ms: since, limit: 500, ..Default::default() }));
    }
    if !ports.is_empty() {
        asks.push((cluster.clone(), String::new(), TrafficQuery { rows: "connections".into(), ports, outcome: Some("blocked".into()), since_ms: since, limit: 500, ..Default::default() }));
    }
    let (rows, _, failed) = ask_all(s, asks, 500).await;
    let mut by_ip: BTreeMap<String, (String, f64)> = BTreeMap::new();
    for r in &rows {
        let ip = ip_of(r, &team);
        if ip.is_empty() {
            continue;
        }
        let e = by_ip.entry(ip).or_insert_with(|| (r["country"].as_str().unwrap_or("").to_owned(), 0.0));
        e.1 += 1.0;
    }
    let mut sources: Vec<(String, (String, f64))> = by_ip.into_iter().collect();
    sources.sort_by(|a, b| b.1.1.total_cmp(&a.1.1));
    sources.truncate(50);
    out["sources"] = json!(sources.iter().map(|(ip, (country, n))| json!({ "ip": ip, "country": country, "count": n })).collect::<Vec<_>>());
    out["recent"] = json!(rows.iter().take(50).map(|r| {
        let website = r.get("host").is_some();
        json!({
            "at": r["at"], "serviceId": service_id,
            "kind": if website { "website".to_owned() } else { r["protocol"].as_str().unwrap_or("tcp").to_lowercase() },
            "target": if website { r["host"].clone() } else { json!(r["port"].to_string()) },
            "ip": ip_of(r, &team), "country": r["country"],
            "rule": if website { r["blockedBy"].clone() } else { r["rule"].clone() },
        })
    }).collect::<Vec<_>>());
    out["ipMode"] = json!(team.ip_mode);
    if let Some((_, why)) = failed.first() {
        out["recentError"] = json!(why);
    }
    Ok(())
}

/// The WAF's latest findings for a service, and their busiest sources
/// (among the latest 500), from its cluster.
pub async fn waf_recent(s: &AppState, service_id: &str, range: &str, out: &mut Value) -> ApiResult<()> {
    let Some((team_id, cluster, hosts, _)) = service_targets(s, service_id).await? else {
        return Ok(());
    };
    let team = s.observability.team(&team_id).await;
    if hosts.is_empty() || !team.on {
        return Ok(());
    }
    let since = pyo_proto::now_ms() - range_ms(range);
    let q = TrafficQuery { rows: "waf".into(), hosts, services: vec![service_id.to_owned()], since_ms: since, limit: 500, ..Default::default() };
    let (rows, _, failed) = ask_all(s, vec![(cluster, String::new(), q)], 500).await;
    let mut by_ip: BTreeMap<String, (String, f64, f64)> = BTreeMap::new();
    for r in &rows {
        let ip = ip_of(r, &team);
        if ip.is_empty() {
            continue;
        }
        let e = by_ip.entry(ip).or_insert_with(|| (r["country"].as_str().unwrap_or("").to_owned(), 0.0, 0.0));
        if r["blocked"] == json!(true) { e.1 += 1.0 } else { e.2 += 1.0 }
    }
    let mut sources: Vec<(String, (String, f64, f64))> = by_ip.into_iter().collect();
    sources.sort_by(|a, b| (b.1.1 + b.1.2).total_cmp(&(a.1.1 + a.1.2)));
    sources.truncate(50);
    out["sources"] = json!(sources.iter().map(|(ip, (c, nb, nl))| json!({ "ip": ip, "country": c, "blocked": nb, "logged": nl })).collect::<Vec<_>>());
    out["recent"] = json!(rows.iter().take(50).map(|r| json!({
        "at": r["at"], "serviceId": service_id, "host": r["host"], "method": r["method"], "url": url_of(r, &team),
        "ip": ip_of(r, &team), "country": r["country"], "blocked": r["blocked"],
        "rules": r["rules"], "messages": r["messages"], "severity": r["severity"],
    })).collect::<Vec<_>>());
    out["ipMode"] = json!(team.ip_mode);
    if let Some((_, why)) = failed.first() {
        out["recentError"] = json!(why);
    }
    Ok(())
}

/// A service's latest connections to its proxied ports, from its cluster.
pub async fn connections_recent(s: &AppState, service_id: &str, range: &str, out: &mut Value) -> ApiResult<()> {
    let Some((team_id, cluster, _, ports)) = service_targets(s, service_id).await? else {
        return Ok(());
    };
    let team = s.observability.team(&team_id).await;
    if ports.is_empty() || !team.on {
        return Ok(());
    }
    let since = pyo_proto::now_ms() - range_ms(range);
    let q = TrafficQuery { rows: "connections".into(), ports, since_ms: since, limit: 50, ..Default::default() };
    let (rows, _, failed) = ask_all(s, vec![(cluster, String::new(), q)], 50).await;
    out["recent"] = json!(rows.iter().map(|r| json!({
        "at": r["at"], "port": r["port"], "protocol": r["protocol"], "ip": ip_of(r, &team),
        "country": r["country"], "durationMs": r["durationMs"].as_f64().unwrap_or(0.0).round(),
        "bytesIn": r["bytesIn"], "bytesOut": r["bytesOut"], "outcome": r["outcome"],
    })).collect::<Vec<_>>());
    out["ipMode"] = json!(team.ip_mode);
    if let Some((_, why)) = failed.first() {
        out["recentError"] = json!(why);
    }
    Ok(())
}

/// A service's team, cluster, website hosts and proxied ports' keys.
async fn service_targets(s: &AppState, service_id: &str) -> ApiResult<Option<(String, String, Vec<String>, Vec<String>)>> {
    let row: Option<(String, String, String, String, Value, Value)> = sqlx::query_as(
        "SELECT a.team_id, a.cluster_id, p.namespace, a.name, a.spec->'domains', a.spec->'ports'
           FROM pyo.services a JOIN pyo.projects p ON p.id = a.project_id WHERE a.id = $1",
    )
    .bind(service_id)
    .fetch_optional(&s.db)
    .await?;
    let Some((team_id, cluster, ns, name, domains, ports)) = row else { return Ok(None) };
    let hosts = domains
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|d| d["hostname"].as_str().map(str::to_lowercase))
        .collect();
    let ports = crate::totals::port_keys(&ns, &name, &ports).into_iter().map(|(k, _)| k).collect();
    Ok(Some((team_id, cluster, hosts, ports)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::observability::IpMode;
    use crate::teamobs::QueryStrings;

    /// Rows a cluster kept before the team's settings changed are shown as
    /// they say now.
    #[test]
    fn kept_rows_are_shown_as_the_team_keeps_them_now() {
        let r = json!({ "at": 1, "host": "shop.example.com", "url": "/a?token=[redacted]&page=2", "ip": "203.0.113.9" });
        let team = |ip, qs| Effective { ip_mode: ip, query_strings: qs, ..Default::default() };
        let row = request_row(&r, &team(IpMode::Full, QueryStrings::Keep));
        assert_eq!((row["url"].as_str(), row["ip"].as_str()), (Some("/a?token=[redacted]&page=2"), Some("203.0.113.9")));
        let row = request_row(&r, &team(IpMode::Anonymized, QueryStrings::Names));
        assert_eq!((row["url"].as_str(), row["ip"].as_str()), (Some("/a?token&page"), Some("203.0.113.0")));
        let row = request_row(&r, &team(IpMode::None, QueryStrings::None));
        assert_eq!((row["url"].as_str(), row["ip"].as_str()), (Some("/a"), Some("")));
    }
}
