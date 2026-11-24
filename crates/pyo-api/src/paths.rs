//! Paths of one hostname on several services of a team
//! (docs/gateway-routes.md). Each domain entry has a path ("/" when it
//! has none), and the longest prefix a request is under wins.
//!
//! One entry per hostname *holds* it: its listener and certificate in
//! Pyo Gateway, its DNS record, its tunnel, its redirects and its HTTPS
//! settings. That's every entry saved before paths existed, and the first
//! one added for a hostname since. The others are paths of it
//! (`"primary": false`): they follow the holder's settings, and their
//! routes join its listener (pyo-proto's `DomainSpec::primary_of`).
//!
//! - **Saving** ([`share`]): a hostname and path once per team, a hostname
//!   on one cluster, redirects only where it's held. A path copies the
//!   holder's HTTPS settings, so it shows them.
//! - **Handing out** ([`specs`]): a path gets the holder's service (whose
//!   Gateway it joins), certificate, redirect to HTTPS, tunnel and header
//!   rules, as they are then.
//! - **Following** ([`sharers`]): when a service's routes change, the
//!   others on its hostnames get theirs again (routes.rs `changed`).
//! - **Handing over** ([`settle`]): the entry that held a hostname goes
//!   (removed, or its service deleted) while paths of it stay: the oldest
//!   path (of a running service first) takes it over, with its settings,
//!   tunnel and DNS record.

use pyo_proto::DomainSpec;
use serde_json::{Value, json};
use sqlx::{PgConnection, Postgres, Transaction};

use crate::error::{ApiError, ApiResult};

/// Whether an entry holds its hostname (not a path of one another holds).
pub fn holds(d: &Value) -> bool {
    d["primary"] != json!(false)
}

/// An entry's path: "/" without one.
pub fn path_of(d: &Value) -> &str {
    match d["path"].as_str() {
        Some(p) if !pyo_proto::rules::is_root(p) => p,
        _ => "/",
    }
}

/// "example.com", or "example.com/api" for a path: how Compose files and
/// lists name an entry.
pub fn shown_key(host: &str, path: &str) -> String {
    pyo_proto::rules::shown(host, path)
}

fn host_of(d: &Value) -> String {
    d["hostname"].as_str().unwrap_or("").trim().to_lowercase()
}

/// What a path takes from the entry that holds its hostname.
const HELD: &[&str] = &["tlsMode", "certificateId", "redirectHttps"];

/// An entry of a hostname, on one of the team's services.
#[derive(Debug, Clone)]
pub struct Entry {
    pub service_id: String,
    pub service: String,
    pub cluster_id: String,
    pub domain: Value,
    /// Its service runs (it has a live deployment).
    pub deployed: bool,
}

/// The team's entries for these hostnames, on services other than
/// `except` ("" for all), not being deleted.
pub async fn entries(
    conn: &mut PgConnection,
    team_id: &str,
    hosts: &[String],
    except: &str,
) -> ApiResult<Vec<Entry>> {
    let rows: Vec<(String, String, String, Value, bool)> = sqlx::query_as(
        "SELECT a.id, a.name, a.cluster_id, d, a.current_deployment_id IS NOT NULL
           FROM pyo.services a, jsonb_array_elements(CASE WHEN jsonb_typeof(a.spec->'domains') = 'array' THEN a.spec->'domains' ELSE '[]' END) d
          WHERE a.team_id = $1 AND a.id <> $2 AND NOT a.deleting AND lower(d->>'hostname') = ANY($3)
          ORDER BY coalesce((d->>'createdAt')::bigint, 0), a.id",
    )
    .bind(team_id)
    .bind(except)
    .bind(hosts)
    .fetch_all(conn)
    .await?;
    Ok(rows
        .into_iter()
        .map(
            |(service_id, service, cluster_id, domain, deployed)| Entry {
                service_id,
                service,
                cluster_id,
                domain,
                deployed,
            },
        )
        .collect())
}

/// A service's entries as it's saved, checked against the team's: paths
/// tidied, a hostname and path used once in the team, a hostname on one
/// cluster, and which entry holds each hostname (the one that did, else
/// another service's, else its "/" or its first). Paths copy the holder's
/// HTTPS settings and have no tunnel or redirects of their own.
pub async fn share(
    tx: &mut Transaction<'_, Postgres>,
    team_id: &str,
    service_id: &str,
    cluster_id: &str,
    spec: &mut Value,
    old: &Value,
) -> ApiResult<()> {
    let Some(domains) = spec["domains"].as_array_mut() else {
        return Ok(());
    };
    for d in domains.iter_mut() {
        let host = host_of(d);
        let path = pyo_proto::rules::normalize_path(d["path"].as_str().unwrap_or("/"))
            .map_err(ApiError::bad_request)?;
        let strip = d["stripPrefix"] == json!(true);
        let Some(o) = d.as_object_mut() else { continue };
        if path == "/" {
            if strip {
                return Err(ApiError::bad_request(format!(
                    "{host}: strip prefix needs a path other than /."
                )));
            }
            o.remove("path");
            o.remove("stripPrefix");
        } else {
            o.insert("path".into(), json!(path));
            if !strip {
                o.remove("stripPrefix");
            }
        }
    }
    let shown = |d: &Value| pyo_proto::rules::shown(&host_of(d), path_of(d));
    for (i, d) in domains.iter().enumerate() {
        if domains[..i]
            .iter()
            .any(|x| host_of(x) == host_of(d) && path_of(x) == path_of(d))
        {
            return Err(ApiError::conflict(format!("{} is already used.", shown(d))));
        }
    }
    let mut hosts: Vec<String> = domains.iter().map(host_of).collect();
    hosts.sort();
    hosts.dedup();
    let theirs = entries(tx, team_id, &hosts, service_id).await?;
    let before: Vec<String> = old["domains"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|d| holds(d))
        .filter_map(|d| d["id"].as_str().map(str::to_owned))
        .collect();
    for host in &hosts {
        let others: Vec<&Entry> = theirs
            .iter()
            .filter(|e| host_of(&e.domain) == *host)
            .collect();
        if let Some(e) = others.iter().find(|e| e.cluster_id != cluster_id) {
            return Err(ApiError::conflict(format!(
                "{host} is used by {}, on another cluster. Paths of one domain must be on one cluster.",
                e.service
            )));
        }
        let mine: Vec<usize> = (0..domains.len())
            .filter(|&i| host_of(&domains[i]) == *host)
            .collect();
        for &i in &mine {
            if let Some(e) = others
                .iter()
                .find(|e| path_of(&e.domain) == path_of(&domains[i]))
            {
                return Err(ApiError::conflict(format!(
                    "{} is already used by {}.",
                    shown(&domains[i]),
                    e.service
                )));
            }
        }
        // Who holds it, and the settings its paths follow.
        let (holder, name): (Value, String) = match others.iter().find(|e| holds(&e.domain)) {
            Some(e) => (e.domain.clone(), e.service.clone()),
            None => {
                let pick = mine
                    .iter()
                    .copied()
                    .find(|&i| {
                        domains[i]["id"]
                            .as_str()
                            .is_some_and(|id| before.iter().any(|b| b == id))
                    })
                    .or_else(|| mine.iter().copied().find(|&i| path_of(&domains[i]) == "/"))
                    .unwrap_or(mine[0]);
                if let Some(o) = domains[pick].as_object_mut() {
                    o.remove("primary");
                }
                (domains[pick].clone(), String::new())
            }
        };
        let holder_id = holder["id"].clone();
        for &i in &mine {
            let d = &mut domains[i];
            if d["id"] == holder_id && name.is_empty() {
                continue;
            }
            if d["redirects"].as_array().is_some_and(|r| !r.is_empty()) {
                return Err(ApiError::conflict(match name.as_str() {
                    "" => format!(
                        "Redirects of {host} go on its entry for {}.",
                        path_of(&holder)
                    ),
                    n => format!("Redirects of {host} are set on {n}."),
                }));
            }
            if d["portName"].as_str().unwrap_or("").is_empty() {
                return Err(ApiError::bad_request(format!(
                    "{} needs a port to route to.",
                    shown(&*d)
                )));
            }
            d["primary"] = json!(false);
            for k in HELD {
                match holder.get(*k) {
                    Some(v) => d[*k] = v.clone(),
                    None => {
                        if let Some(o) = d.as_object_mut() {
                            o.remove(*k);
                        }
                    }
                }
            }
            if let Some(o) = d.as_object_mut() {
                o.remove("tunnel");
            }
        }
        // A redirect where a path routes: neither would win.
        let routes: Vec<(String, String)> = others
            .iter()
            .map(|e| (path_of(&e.domain).to_owned(), e.service.clone()))
            .chain(
                mine.iter()
                    .filter(|&&i| !domains[i]["portName"].as_str().unwrap_or("").is_empty())
                    .map(|&i| (path_of(&domains[i]).to_owned(), String::new())),
            )
            .collect();
        for r in holder["redirects"].as_array().into_iter().flatten() {
            let at = r["path"].as_str().unwrap_or("/");
            if let Some((_, by)) = routes.iter().find(|(p, _)| p == at) {
                return Err(ApiError::conflict(format!(
                    "{} routes to {}, so it can't redirect too.",
                    pyo_proto::rules::shown(host, at),
                    if by.is_empty() { "this service" } else { by }
                )));
            }
        }
    }
    Ok(())
}

/// A service's entries as the agent applies them: paths with what they
/// take from the entry that holds their hostname, as it is now (its
/// service's Gateway, certificate, redirect to HTTPS, tunnel, and its
/// header rules before their own). A path whose hostname nobody holds
/// (it shouldn't happen) holds it itself.
pub async fn specs(
    conn: &mut PgConnection,
    service_id: &str,
    domains: &Value,
) -> anyhow::Result<Vec<DomainSpec>> {
    let mut out = crate::routes::domain_specs(domains);
    let raw: Vec<&Value> = domains.as_array().into_iter().flatten().collect();
    let paths: Vec<String> = raw
        .iter()
        .filter(|d| !holds(d))
        .map(|d| host_of(d))
        .collect();
    if paths.is_empty() {
        return Ok(out);
    }
    let holders: Vec<(String, Value)> = sqlx::query_as(
        "SELECT p.namespace || '-' || a.name, d
           FROM pyo.services a JOIN pyo.projects p ON p.id = a.project_id,
                jsonb_array_elements(CASE WHEN jsonb_typeof(a.spec->'domains') = 'array' THEN a.spec->'domains' ELSE '[]' END) d
          WHERE a.team_id = (SELECT team_id FROM pyo.services WHERE id = $1) AND NOT a.deleting
            AND lower(d->>'hostname') = ANY($2) AND coalesce(d->>'primary', 'true') <> 'false'",
    )
    .bind(service_id)
    .bind(&paths)
    .fetch_all(conn)
    .await?;
    for (spec, d) in out.iter_mut().zip(raw) {
        if holds(d) {
            continue;
        }
        let Some((base, h)) = holders.iter().find(|(_, h)| host_of(h) == host_of(d)) else {
            continue;
        };
        let held = &crate::routes::domain_specs(&json!([h]))[0];
        spec.primary_of = base.clone();
        spec.tls = held.tls.clone();
        spec.certificate_id = held.certificate_id.clone();
        spec.redirect_https = held.redirect_https;
        spec.tunnel = held.tunnel.clone();
        spec.headers = pyo_proto::rules::merged(&held.headers, &spec.headers);
    }
    Ok(out)
}

/// The other services of the team that have an entry for one of this
/// service's hostnames.
pub async fn sharers(conn: &mut PgConnection, service_id: &str) -> ApiResult<Vec<String>> {
    Ok(sqlx::query_scalar(
        "SELECT DISTINCT b.id
           FROM pyo.services a, jsonb_array_elements(CASE WHEN jsonb_typeof(a.spec->'domains') = 'array' THEN a.spec->'domains' ELSE '[]' END) d,
                pyo.services b, jsonb_array_elements(CASE WHEN jsonb_typeof(b.spec->'domains') = 'array' THEN b.spec->'domains' ELSE '[]' END) e
          WHERE a.id = $1 AND b.team_id = a.team_id AND b.id <> a.id AND NOT b.deleting
            AND lower(e->>'hostname') = lower(d->>'hostname')",
    )
    .bind(service_id)
    .fetch_all(conn)
    .await?)
}

/// The entries of `old` that held their hostname and are gone from `new`.
pub fn held_and_gone(old: &Value, new: &Value) -> Vec<Value> {
    let kept: Vec<&Value> = new["domains"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|d| holds(d))
        .collect();
    old["domains"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|d| holds(d))
        .filter(|d| !kept.iter().any(|k| host_of(k) == host_of(d)))
        .cloned()
        .collect()
}

/// Hostnames these entries held, and that the team still uses (another
/// entry routes a path of them): the oldest of those (of a running service
/// first) takes each over, with its HTTPS settings, tunnel, redirects and
/// DNS record, and the services on it follow. `from`: the service the
/// entries were on. The clusters to wake come back.
pub async fn settle(
    tx: &mut Transaction<'_, Postgres>,
    team_id: &str,
    from: &str,
    gone: &[Value],
) -> ApiResult<Vec<String>> {
    let mut wake = vec![];
    for g in gone {
        let host = host_of(g);
        let left = entries(tx, team_id, std::slice::from_ref(&host), "").await?;
        if left.is_empty() || left.iter().any(|e| holds(&e.domain)) {
            continue;
        }
        let Some(next) = left
            .iter()
            .find(|e| e.deployed)
            .or_else(|| left.first())
            .cloned()
        else {
            continue;
        };
        let mut spec: Value = sqlx::query_scalar("SELECT spec FROM pyo.services WHERE id = $1")
            .bind(&next.service_id)
            .fetch_one(&mut **tx)
            .await?;
        let id = next.domain["id"].clone();
        for d in spec["domains"].as_array_mut().into_iter().flatten() {
            if d["id"] != id {
                continue;
            }
            if let Some(o) = d.as_object_mut() {
                o.remove("primary");
            }
            for k in [
                "tlsMode",
                "certificateId",
                "redirectHttps",
                "tunnel",
                "redirects",
            ] {
                if let Some(v) = g.get(k) {
                    d[k] = v.clone();
                }
            }
        }
        sqlx::query("UPDATE pyo.services SET spec = $2 WHERE id = $1")
            .bind(&next.service_id)
            .bind(&spec)
            .execute(&mut **tx)
            .await?;
        // Its DNS record (and the tunnel's) goes with it.
        sqlx::query(
            "UPDATE pyo.dns_records SET service_id = $3, domain_id = $4 WHERE service_id = $1 AND domain_id = $2",
        )
        .bind(from)
        .bind(g["id"].as_str().unwrap_or(""))
        .bind(&next.service_id)
        .bind(id.as_str().unwrap_or(""))
        .execute(&mut **tx)
        .await?;
        tracing::info!(%host, to = %next.service_id, "a shared hostname changed hands");
        if let Some(c) = crate::routes::changed(tx, &next.service_id, true).await? {
            wake.push(c);
        }
    }
    Ok(wake)
}

/// A hostname's DNS record and tunnel are set on the entry that holds it:
/// refused on a path of one another entry holds, saying which.
pub async fn held_here(
    tx: &mut Transaction<'_, Postgres>,
    service_id: &str,
    domain_id: &str,
) -> ApiResult<()> {
    let row: Option<(String, Value)> = sqlx::query_as(
        "SELECT a.team_id, d FROM pyo.services a, jsonb_array_elements(CASE WHEN jsonb_typeof(a.spec->'domains') = 'array' THEN a.spec->'domains' ELSE '[]' END) d
          WHERE a.id = $1 AND d->>'id' = $2",
    )
    .bind(service_id)
    .bind(domain_id)
    .fetch_optional(&mut **tx)
    .await?;
    let Some((team_id, d)) = row.filter(|(_, d)| !holds(d)) else {
        return Ok(());
    };
    let host = host_of(&d);
    let holder = entries(tx, &team_id, std::slice::from_ref(&host), "")
        .await?
        .into_iter()
        .find(|e| holds(&e.domain))
        .map(|e| e.service)
        .unwrap_or_else(|| "the service that has it".into());
    Err(ApiError::conflict(format!(
        "{} is a path of {host}. Its DNS record and tunnel are set on {holder}.",
        pyo_proto::rules::shown(&host, path_of(&d))
    )))
}

/// Of these hostnames, the ones the team no longer uses at all.
pub async fn unused(
    tx: &mut Transaction<'_, Postgres>,
    team_id: &str,
    hosts: &[String],
) -> ApiResult<Vec<String>> {
    let left = entries(tx, team_id, hosts, "").await?;
    Ok(hosts
        .iter()
        .filter(|h| !left.iter().any(|e| host_of(&e.domain) == **h))
        .cloned()
        .collect())
}

/// Whether other entries of the team use this hostname too (besides
/// `service_id`'s entry `domain_id`).
pub async fn shared(
    tx: &mut Transaction<'_, Postgres>,
    team_id: &str,
    service_id: &str,
    domain_id: &str,
    host: &str,
) -> ApiResult<bool> {
    let left = entries(tx, team_id, &[host.to_lowercase()], "").await?;
    Ok(left
        .iter()
        .any(|e| !(e.service_id == service_id && e.domain["id"] == json!(domain_id))))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entries_hold_their_hostname_unless_they_say_otherwise() {
        assert!(holds(&json!({ "hostname": "a.example.com" })));
        assert!(holds(&json!({ "primary": true })));
        assert!(!holds(&json!({ "primary": false })));
        assert_eq!(path_of(&json!({})), "/");
        assert_eq!(path_of(&json!({ "path": "" })), "/");
        assert_eq!(path_of(&json!({ "path": "/api" })), "/api");
    }

    #[test]
    fn hostnames_whose_holder_went() {
        let old = json!({ "domains": [
            { "id": "a", "hostname": "Example.com" },
            { "id": "b", "hostname": "example.com", "path": "/api", "primary": false },
            { "id": "c", "hostname": "other.example.com" },
        ] });
        let new = json!({ "domains": [{ "id": "b", "hostname": "example.com", "path": "/api", "primary": false }] });
        let gone: Vec<String> = held_and_gone(&old, &new)
            .iter()
            .filter_map(|d| d["id"].as_str().map(str::to_owned))
            .collect();
        assert_eq!(gone, ["a", "c"]);
        // Still held (by an entry that took over): not gone.
        let taken =
            json!({ "domains": [{ "id": "b", "hostname": "example.com", "path": "/api" }] });
        let gone: Vec<String> = held_and_gone(&old, &taken)
            .iter()
            .filter_map(|d| d["id"].as_str().map(str::to_owned))
            .collect();
        assert_eq!(gone, ["c"]);
        assert!(held_and_gone(&json!({}), &new).is_empty());
    }
}
