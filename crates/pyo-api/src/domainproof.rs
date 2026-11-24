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

use std::{net::IpAddr, time::Duration};

use hickory_resolver::{
    config::{LookupIpStrategy, NameServerConfig, ResolveHosts, ResolverConfig, ResolverOpts},
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
            tracing::warn!(%name, error = %format!("{e:#}"), "domain proof: no TXT record read");
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
                .iter()
                .copied()
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
                    Err(e) => return Err(unanswered(&name, &servers, e).await),
                }
            }
            Err(e) => return Err(unanswered(&name, &servers, e).await),
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

/// No name server answered: what was asked, and why each address couldn't
/// be reached when the host says why. The resolver itself only says it had
/// no connection left, which hides a host without IPv6, say.
async fn unanswered(name: &Name, servers: &[IpAddr], e: impl std::fmt::Display) -> anyhow::Error {
    let mut asked = vec![];
    for ip in servers {
        asked.push(match reach(*ip).await {
            Ok(()) => ip.to_string(),
            Err(why) => format!("{ip} ({why})"),
        });
    }
    anyhow::anyhow!(
        "the name servers of {name} didn't answer: {e}. Asked {}",
        asked.join(", ")
    )
}

/// Whether this host has a route to a name server's address (a UDP socket
/// connected to its port 53, which sends nothing).
async fn reach(ip: IpAddr) -> std::io::Result<()> {
    let local: std::net::SocketAddr = if ip.is_ipv4() {
        (std::net::Ipv4Addr::UNSPECIFIED, 0).into()
    } else {
        (std::net::Ipv6Addr::UNSPECIFIED, 0).into()
    };
    tokio::net::UdpSocket::bind(local)
        .await?
        .connect((ip, 53))
        .await
}

fn dns_options(recursive: bool) -> ResolverOpts {
    let mut o = ResolverOpts::default();
    o.timeout = DNS_TIMEOUT;
    o.attempts = 2;
    o.use_hosts_file = ResolveHosts::Never;
    // Name servers' addresses of both families, IPv4 first.
    o.ip_strategy = LookupIpStrategy::Ipv4AndIpv6;
    if !recursive {
        // Ask the name servers themselves, fresh each time.
        o.recursion_desired = false;
        o.cache_size = 0;
    }
    o
}

/// Of one name server's addresses, the ones to ask: its first IPv4 and
/// first IPv6 address, so a host with only one of the two reaches it.
fn server_addresses(found: impl IntoIterator<Item = IpAddr>) -> Vec<IpAddr> {
    let found: Vec<IpAddr> = found.into_iter().collect();
    let v4 = found.iter().find(|a| a.is_ipv4());
    let v6 = found.iter().find(|a| a.is_ipv6());
    v4.into_iter().chain(v6).copied().collect()
}

/// Every name server's addresses, IPv4 first: hosts without IPv6 (most
/// pods) try those first, and hosts with only IPv6 still have theirs.
fn ipv4_first(mut ips: Vec<IpAddr>) -> Vec<IpAddr> {
    ips.sort_by_key(IpAddr::is_ipv6);
    ips
}

/// The addresses of the name servers of the zone `name` is in.
async fn zone_servers(
    recursive: &hickory_resolver::TokioResolver,
    name: &Name,
) -> anyhow::Result<Vec<IpAddr>> {
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
                        ips.extend(server_addresses(found.iter()));
                    }
                }
                anyhow::ensure!(
                    !ips.is_empty(),
                    "the name servers of {zone} have no address"
                );
                return Ok(ipv4_first(ips));
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

/// Not proven yet: the team's Cloudflare writes the record when it can
/// (cloudflare.rs `write_proof`), and it's read again for a few seconds
/// while Cloudflare's name servers take it. Proven or not, the record has
/// done its job then: Pyo deletes it again.
async fn prove_with_cloudflare(
    tx: &mut Transaction<'_, Postgres>,
    team_id: &str,
    host: &str,
) -> bool {
    match crate::cloudflare::write_proof(tx, team_id, host).await {
        Ok(true) => {}
        Ok(false) => return false,
        Err(e) => {
            tracing::info!(%host, error = ?e.message, "domain proof: Cloudflare didn't write it");
            // It may have been made before the error.
            clean_up(tx, team_id, host).await;
            return false;
        }
    }
    let mut done = false;
    for _ in 0..5 {
        if proven(team_id, host).await {
            done = true;
            break;
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    clean_up(tx, team_id, host).await;
    done
}

/// The proof records Pyo made for `host` go (only Pyo's, never one made by
/// hand). Best effort: one left behind goes when the domain is removed.
async fn clean_up(tx: &mut Transaction<'_, Postgres>, team_id: &str, host: &str) {
    if let Err(e) = crate::cloudflare::remove_proof(tx, team_id, host).await {
        tracing::info!(%host, error = ?e.message, "domain proof: Pyo's record stays for now");
    }
}

/// One label under the default domain of a Cloudflare Tunnel of the
/// team's (Pyo Cloud: the zone is the team's, its token edits it).
async fn own_tunnel_domain(
    tx: &mut Transaction<'_, Postgres>,
    team_id: &str,
    host: &str,
) -> ApiResult<bool> {
    let domains: Vec<String> = sqlx::query_scalar(
        "SELECT default_domain FROM pyo.cluster_tunnels WHERE team_id = $1 AND default_domain IS NOT NULL AND state <> 'removing'",
    )
    .bind(team_id)
    .fetch_all(&mut **tx)
    .await?;
    Ok(domains
        .iter()
        .any(|d| crate::cloudflare::tunnel_loop::under_default(host, d)))
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
        // A free name under one of the team's own tunnels' default
        // domains: a zone its Cloudflare token was checked to control.
        if own_tunnel_domain(tx, team_id, host).await? {
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
        if proven(team_id, host).await {
            // One Pyo wrote before (it stays when it couldn't be deleted).
            clean_up(tx, team_id, host).await;
        } else if !prove_with_cloudflare(tx, team_id, host).await {
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

    /// Name servers with both families (Cloudflare's, say) are asked over
    /// IPv4 first, and keep their IPv6 address for hosts with only that.
    /// Before, only the first two addresses were kept, both IPv6, so
    /// IPv4-only pods could reach none of them.
    #[test]
    fn name_servers_are_asked_over_ipv4_first() {
        let ip = |s: &str| s.parse::<IpAddr>().unwrap();
        // As a lookup gives them: IPv6 first.
        let first = server_addresses([
            ip("2001:db8::1"),
            ip("2001:db8::2"),
            ip("192.0.2.1"),
            ip("192.0.2.2"),
        ]);
        assert_eq!(first, [ip("192.0.2.1"), ip("2001:db8::1")]);
        let second = server_addresses([ip("2001:db8::3"), ip("198.51.100.1")]);
        let all = ipv4_first(first.into_iter().chain(second).collect());
        assert_eq!(
            all,
            [
                ip("192.0.2.1"),
                ip("198.51.100.1"),
                ip("2001:db8::1"),
                ip("2001:db8::3"),
            ]
        );
        // One family only: that's what there is.
        assert_eq!(
            server_addresses([ip("2001:db8::1"), ip("2001:db8::2")]),
            [ip("2001:db8::1")]
        );
        assert_eq!(server_addresses([ip("192.0.2.1")]), [ip("192.0.2.1")]);
        assert!(server_addresses([]).is_empty());
    }

    /// A name server that can't be reached says why, with its address.
    #[tokio::test]
    async fn a_name_server_that_isnt_reached_is_named() {
        let name = Name::from_ascii("_pyo-verify.example.com.").unwrap();
        let servers = ["192.0.2.1".parse().unwrap()];
        let m = unanswered(&name, &servers, "no connections available")
            .await
            .to_string();
        assert!(
            m.starts_with(
                "the name servers of _pyo-verify.example.com. didn't answer: no connections available. Asked 192.0.2.1"
            ),
            "{m}"
        );
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
