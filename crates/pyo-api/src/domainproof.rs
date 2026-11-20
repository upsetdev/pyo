//! Proof of owning a domain, before a team may claim it (Pyo Cloud).
//!
//! Hostnames are first come over every team of an installation, so where
//! teams don't trust each other one could claim another's domain first.
//! There ([`crate::verify_domains`]), a team claims a hostname only with a
//! DNS TXT record `_pyo-verify.<host>` holding the token Pyo gives that
//! team for that host. A team that proves it can take a hostname over from
//! a team that can't (anymore). Free domains (nip.io and sslip.io) need no
//! record: their address must be one of the team's own servers.
//!
//! TXT records are read from the domain's own name servers, asked
//! directly with nothing cached ([`authoritative_txt`]): what the team set
//! there just now counts, and no resolver in between can answer for them.
//! The name servers themselves are found through the system's resolver.
//! For tests only, `PYO_DEV_TXT_FILE` names a JSON file of records to use
//! instead (`{"_pyo-verify.example.com": ["pyo-verify-…"]}`), and
//! `PYO_DEV_TXT_DOH` a DNS over HTTPS endpoint to ask instead (one that
//! answers `application/dns-json`, such as Cloudflare's).

use std::time::Duration;

use hickory_resolver::{
    config::{NameServerConfig, ResolveHosts, ResolverConfig, ResolverOpts},
    proto::rr::{Name, RData, RecordType},
};
use serde_json::Value;
use sqlx::{Postgres, Transaction};

use crate::error::ApiResult;

/// The record a team adds for `host`.
pub fn record(host: &str) -> String {
    format!("_pyo-verify.{host}")
}

/// The token a team puts in `host`'s record: the same every time, and
/// another team's never matches.
pub fn token(team_id: &str, host: &str) -> String {
    let h = crate::ids::sha256_hex(&format!("pyo-verify\n{team_id}\n{}", host.to_lowercase()));
    format!("pyo-verify-{}", &h[..32])
}

/// What to tell a team that hasn't proven `host` yet.
pub fn how_to(team_id: &str, host: &str) -> String {
    format!(
        "Prove you own {host} first: add a TXT record {} with the value {}, then try again.",
        record(host),
        token(team_id, host)
    )
}

/// The TXT values at `name`.
async fn txt(name: &str) -> Vec<String> {
    if let Ok(path) = std::env::var("PYO_DEV_TXT_FILE")
        && !path.trim().is_empty()
    {
        let v: Value = std::fs::read(path.trim())
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        return v[name]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect();
    }
    if let Ok(url) = std::env::var("PYO_DEV_TXT_DOH")
        && !url.trim().is_empty()
    {
        return doh_txt(url.trim(), name).await;
    }
    match authoritative_txt(name).await {
        Ok(v) => v,
        Err(e) => {
            tracing::info!(%name, error = %e, "domain proof: no TXT record read");
            vec![]
        }
    }
}

/// How long each step may take, and how many name servers are asked.
const DNS_TIMEOUT: Duration = Duration::from_secs(4);
const NAME_SERVERS: usize = 4;

/// The TXT values at `name`, asked of its zone's own name servers: the
/// zone is the nearest name at or above it with NS records, found through
/// the system's resolver; then its name servers are asked directly, without
/// recursion or a cache. A CNAME there is followed (a few times at most).
pub async fn authoritative_txt(name: &str) -> anyhow::Result<Vec<String>> {
    let recursive = hickory_resolver::Resolver::builder_tokio()?
        .with_options(dns_options(true))
        .build()?;
    let mut name = Name::from_ascii(name.trim_end_matches('.'))?;
    name.set_fqdn(true);
    for _ in 0..4 {
        let servers = zone_servers(&recursive, &name).await?;
        let config = ResolverConfig::from_name_servers(
            servers
                .into_iter()
                .map(NameServerConfig::udp_and_tcp)
                .collect(),
        );
        let authoritative = hickory_resolver::Resolver::builder_with_config(
            config,
            hickory_resolver::net::runtime::TokioRuntimeProvider::default(),
        )
        .with_options(dns_options(false))
        .build()?;
        let answers = match authoritative.lookup(name.clone(), RecordType::TXT).await {
            Ok(l) => l.answers().to_vec(),
            Err(e) if e.is_no_records_found() || e.is_nx_domain() => {
                // Nothing, unless the name is a CNAME to follow.
                match authoritative.lookup(name.clone(), RecordType::CNAME).await {
                    Ok(l) => l.answers().to_vec(),
                    Err(e) if e.is_no_records_found() || e.is_nx_domain() => return Ok(vec![]),
                    Err(e) => return Err(e.into()),
                }
            }
            Err(e) => return Err(e.into()),
        };
        let txt: Vec<String> = answers
            .iter()
            .filter_map(|r| match &r.data {
                RData::TXT(t) => Some(
                    t.txt_data
                        .iter()
                        .map(|c| String::from_utf8_lossy(c).into_owned())
                        .collect::<String>(),
                ),
                _ => None,
            })
            .collect();
        if !txt.is_empty() {
            return Ok(txt);
        }
        let Some(target) = answers.iter().find_map(|r| match &r.data {
            RData::CNAME(c) => Some(c.0.clone()),
            _ => None,
        }) else {
            return Ok(vec![]);
        };
        name = target;
    }
    anyhow::bail!("too many CNAMEs")
}

fn dns_options(recursive: bool) -> ResolverOpts {
    let mut o = ResolverOpts::default();
    o.timeout = DNS_TIMEOUT;
    o.attempts = 2;
    o.use_hosts_file = ResolveHosts::Never;
    if !recursive {
        // Ask the name servers themselves, fresh each time.
        o.recursion_desired = false;
        o.cache_size = 0;
    }
    o
}

/// The addresses of the name servers of the zone `name` is in.
async fn zone_servers(
    recursive: &hickory_resolver::TokioResolver,
    name: &Name,
) -> anyhow::Result<Vec<std::net::IpAddr>> {
    let mut zone = name.clone();
    loop {
        anyhow::ensure!(zone.num_labels() >= 2, "no zone found for {name}");
        if let Ok(l) = recursive.lookup(zone.clone(), RecordType::NS).await {
            let ns: Vec<Name> = l
                .answers()
                .iter()
                .filter_map(|r| match &r.data {
                    RData::NS(n) if r.name == zone => Some(n.0.clone()),
                    _ => None,
                })
                .collect();
            if !ns.is_empty() {
                let mut ips = vec![];
                for n in ns.into_iter().take(NAME_SERVERS) {
                    if let Ok(found) = recursive.lookup_ip(n).await {
                        ips.extend(found.iter().take(2));
                    }
                }
                anyhow::ensure!(
                    !ips.is_empty(),
                    "the name servers of {zone} have no address"
                );
                return Ok(ips);
            }
        }
        zone = zone.base_name();
    }
}

/// The TXT values at `name` through DNS over HTTPS (`PYO_DEV_TXT_DOH`).
async fn doh_txt(url: &str, name: &str) -> Vec<String> {
    let Ok(client) = reqwest::Client::builder()
        .timeout(Duration::from_secs(8))
        .build()
    else {
        return vec![];
    };
    let Ok(res) = client
        .get(url)
        .query(&[("name", name), ("type", "TXT")])
        .header("accept", "application/dns-json")
        .send()
        .await
    else {
        return vec![];
    };
    let v: Value = res.json().await.unwrap_or_default();
    v["Answer"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|a| a["type"] == 16)
        .filter_map(|a| a["data"].as_str())
        .map(|d| d.trim().trim_matches('"').to_owned())
        .collect()
}

/// Whether `host`'s record holds `team_id`'s token now.
pub async fn proven(team_id: &str, host: &str) -> bool {
    let want = token(team_id, host);
    txt(&record(host)).await.contains(&want)
}

/// A free domain on one of the team's own servers' addresses (it can't
/// hold a TXT record, and points only at that server anyway).
async fn own_free_domain(
    tx: &mut Transaction<'_, Postgres>,
    team_id: &str,
    host: &str,
) -> ApiResult<Option<bool>> {
    let Some(ip) = crate::freedomain::ip_in(host) else {
        return Ok(None);
    };
    let clusters: Vec<String> = sqlx::query_scalar("SELECT id FROM pyo.clusters WHERE team_id = $1")
        .bind(team_id)
        .fetch_all(&mut **tx)
        .await?;
    for c in clusters {
        if let Some((_, cluster)) = crate::freedomain::load(tx, &c).await?
            && cluster.known().contains(&std::net::IpAddr::V4(ip))
        {
            return Ok(Some(true));
        }
    }
    Ok(Some(false))
}

/// Checks the hostnames a service is about to claim, and takes over any
/// another team holds without proof. `new_hosts`: the ones it doesn't
/// have yet (the ones it has stay, proven when they were claimed).
pub async fn check_claims(
    tx: &mut Transaction<'_, Postgres>,
    team_id: &str,
    service_id: &str,
    new_hosts: &[String],
) -> ApiResult<Vec<String>> {
    use crate::error::ApiError;
    let mut wake = vec![];
    for host in new_hosts {
        if pyo_proto::system::is_tailnet_host(host) {
            continue;
        }
        match own_free_domain(tx, team_id, host).await? {
            Some(true) => continue,
            Some(false) => {
                return Err(ApiError::forbidden(format!(
                    "{host} points at a server that isn't one of this team's."
                )));
            }
            None => {}
        }
        if !proven(team_id, host).await {
            return Err(ApiError::forbidden(how_to(team_id, host)));
        }
        // Held by another team: theirs while they can still prove it.
        let holders: Vec<(String, String, Value)> = sqlx::query_as(
            "SELECT a.id, a.team_id, a.spec->'domains' FROM pyo.services a, jsonb_array_elements(a.spec->'domains') d
              WHERE a.id <> $1 AND a.team_id <> $2 AND NOT a.deleting AND lower(d->>'hostname') = $3",
        )
        .bind(service_id)
        .bind(team_id)
        .bind(host)
        .fetch_all(&mut **tx)
        .await?;
        for (other_service, other_team, domains) in holders {
            if proven(&other_team, host).await {
                return Err(ApiError::conflict(format!("{host} is already used by another service.")));
            }
            let kept: Vec<Value> = domains
                .as_array()
                .into_iter()
                .flatten()
                .filter(|d| d["hostname"].as_str().map(str::to_lowercase).as_deref() != Some(host.as_str()))
                .cloned()
                .collect();
            sqlx::query("UPDATE pyo.services SET spec = jsonb_set(spec, '{domains}', $2) WHERE id = $1")
                .bind(&other_service)
                .bind(Value::Array(kept))
                .execute(&mut **tx)
                .await?;
            if let Some(c) = crate::routes::changed(tx, &other_service, true).await? {
                wake.push(c);
            }
            crate::ops::log_activity(
                tx,
                &other_team,
                "Pyo",
                "domain.removed",
                "warning",
                &format!("{host} was removed: another team proved it owns it"),
                serde_json::json!({ "kind": "service", "id": other_service, "label": host }),
                serde_json::json!({}),
            )
            .await?;
            tracing::info!(%host, from = %other_team, to = %team_id, "domain taken over by its proven owner");
        }
    }
    Ok(wake)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_differ_by_team_and_host() {
        assert_eq!(token("t1", "Example.com"), token("t1", "example.com"));
        assert_ne!(token("t1", "example.com"), token("t2", "example.com"));
        assert_ne!(token("t1", "example.com"), token("t1", "www.example.com"));
        assert!(how_to("t1", "a.com").contains("_pyo-verify.a.com"));
    }

    /// Over the internet: a TXT record read from its zone's own name
    /// servers (`cargo test -p pyo-api domainproof -- --ignored`).
    #[tokio::test]
    #[ignore = "needs the internet"]
    async fn reads_txt_from_the_zones_own_name_servers() {
        let spf = authoritative_txt("google.com").await.unwrap();
        assert!(spf.iter().any(|t| t.starts_with("v=spf1")), "{spf:?}");
        // A name with no TXT record, and one that doesn't exist.
        assert!(
            authoritative_txt("_pyo-verify.google.com")
                .await
                .unwrap()
                .is_empty()
        );
        assert!(
            authoritative_txt("no-such-name-for-pyo.example.com")
                .await
                .unwrap()
                .is_empty()
        );
    }
}
