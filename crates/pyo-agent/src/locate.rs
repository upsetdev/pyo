//! Where each server is: its public address as the internet sees it, and
//! its Tailscale address.
//!
//! Node addresses are often private or tailnet IPs, which say nothing about
//! location. So, for each ready server not checked lately, the agent runs a
//! one-off pod on it (host network) that asks Cloudflare's trace endpoint
//! for the server's public IP and country, once over IPv4 and once over
//! IPv6 (a server can have one of each), and lists the server's own IPv4
//! addresses (its Tailscale one, and any public one on a network card,
//! among them), and keeps the answers as node annotations. The heartbeat
//! reports them; the API looks the IP up (GeoIP) for the city, and points
//! the cluster's free domains at the public or the Tailscale address (a
//! server behind NAT has only a private address on its network card).
//!
//! Cloudflare's trace answers over HTTPS from a nearby anycast address, on
//! both families (cloudflare.com has A and AAAA records), with the country
//! in the same answer, and needs no key. `PYO_PUBLIC_IP_LOOKUP` on the
//! agent changes that: another URL (answering in the same `ip=` lines, or
//! with just the address, like https://api.ipify.org), or `off` where the
//! servers can't reach the internet (the Tailscale address is still found).

use std::time::Duration;

use k8s_openapi::api::core::v1::{Node, Pod};
use kube::{
    Api, Client, ResourceExt,
    api::{DeleteParams, ListParams, LogParams, Patch, PatchParams, PostParams},
};
use pyo_proto::{
    LOCATED_AT_ANNOTATION, NIC_PUBLIC_IPV4_ANNOTATION, PUBLIC_COUNTRY_ANNOTATION,
    PUBLIC_IP_ANNOTATION, PUBLIC_IPV4_ANNOTATION, PUBLIC_IPV6_ANNOTATION, TAILNET_IPV4_ANNOTATION,
    now_ms,
};
use serde_json::json;

use crate::MANAGER;

const NS: &str = "pyo-system";
const IMAGE: &str = "curlimages/curl:8.11.1";
/// Where a server asks for its public address (see the module's notes).
const LOOKUP: &str = "https://cloudflare.com/cdn-cgi/trace";
const LOOKUP_ENV: &str = "PYO_PUBLIC_IP_LOOKUP";
/// How often a server is checked again (its address can change).
const EVERY: Duration = Duration::from_secs(24 * 3600);

/// The lookup URL, or None when it's off.
fn lookup_url(env: Option<&str>) -> Option<String> {
    match env.map(str::trim) {
        None | Some("") => Some(LOOKUP.to_owned()),
        Some(v) if v.eq_ignore_ascii_case("off") || v == "0" || v.eq_ignore_ascii_case("false") => {
            None
        }
        Some(v) => Some(v.to_owned()),
    }
}

/// A Tailscale IPv4 address: in 100.64.0.0/10 (CGNAT, which Tailscale uses).
pub fn tailnet_v4(ip: &str) -> bool {
    ip.parse::<std::net::Ipv4Addr>().is_ok_and(|a| {
        let [a, b, ..] = a.octets();
        a == 100 && (64..=127).contains(&b)
    })
}

pub async fn run(client: Client) {
    let url = lookup_url(std::env::var(LOOKUP_ENV).ok().as_deref());
    if url.is_none() {
        tracing::info!("public address lookup is off ({LOOKUP_ENV})");
    }
    let mut tick = tokio::time::interval(Duration::from_secs(600));
    loop {
        tick.tick().await;
        if let Err(e) = locate_all(&client, url.as_deref()).await {
            tracing::warn!(error = %e, "couldn't locate servers");
        }
    }
}

async fn locate_all(client: &Client, url: Option<&str>) -> anyhow::Result<()> {
    let nodes: Api<Node> = Api::all(client.clone());
    for n in nodes.list(&ListParams::default()).await?.items {
        let ready = n
            .status
            .as_ref()
            .and_then(|s| s.conditions.as_ref())
            .is_some_and(|cs| cs.iter().any(|c| c.type_ == "Ready" && c.status == "True"));
        let checked = n
            .annotations()
            .get(LOCATED_AT_ANNOTATION)
            .and_then(|v| v.parse::<i64>().ok())
            .unwrap_or(0);
        // Never looked for its Tailscale address or its network cards' public
        // ones: now (an agent that didn't yet).
        let tailnet_known = n.annotations().contains_key(TAILNET_IPV4_ANNOTATION)
            && n.annotations().contains_key(NIC_PUBLIC_IPV4_ANNOTATION);
        if !ready || (tailnet_known && now_ms() - checked < EVERY.as_millis() as i64) {
            continue;
        }
        let name = n.name_any();
        match locate(client, &name, url).await {
            Ok(found) => {
                // The one address for location: IPv4 when there is one (GeoIP knows it better).
                let ip = found
                    .v4
                    .clone()
                    .or_else(|| found.v6.clone())
                    .unwrap_or_default();
                let country = found.country.clone();
                let tailnet = found.tailnet.clone().unwrap_or_default();
                // A family it no longer has is removed (null). Without a
                // lookup, what an earlier one found stays.
                let mut annotations = json!({
                    TAILNET_IPV4_ANNOTATION: tailnet, LOCATED_AT_ANNOTATION: now_ms().to_string(),
                    NIC_PUBLIC_IPV4_ANNOTATION: found.nic_public.join(","),
                });
                if url.is_some() {
                    annotations[PUBLIC_IP_ANNOTATION] = json!(ip);
                    annotations[PUBLIC_IPV4_ANNOTATION] = json!(found.v4);
                    annotations[PUBLIC_IPV6_ANNOTATION] = json!(found.v6);
                    annotations[PUBLIC_COUNTRY_ANNOTATION] = json!(country);
                }
                let patch = json!({ "metadata": { "annotations": annotations } });
                nodes
                    .patch(&name, &PatchParams::default(), &Patch::Merge(&patch))
                    .await?;
                tracing::info!(node = %name, %ip, %country, %tailnet, "located server");
            }
            Err(e) => tracing::warn!(node = %name, error = %e, "couldn't locate server"),
        }
    }
    Ok(())
}

/// What a lookup found: the public address over each family, the country,
/// and the server's Tailscale IPv4 address.
#[derive(Debug, PartialEq)]
struct Found {
    v4: Option<String>,
    v6: Option<String>,
    country: String,
    tailnet: Option<String>,
    /// Public IPv4 addresses on its own network cards.
    nic_public: Vec<String>,
}

/// Public IPs, country code and Tailscale address of one server.
async fn locate(client: &Client, node: &str, url: Option<&str>) -> anyhow::Result<Found> {
    let pods: Api<Pod> = Api::namespaced(client.clone(), NS);
    let name = format!(
        "pyo-locate-{}",
        node.chars()
            .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
            .take(40)
            .collect::<String>()
            .to_lowercase()
    );
    let _ = pods.delete(&name, &DeleteParams::default()).await;
    tokio::time::sleep(Duration::from_secs(2)).await;
    let pod: Pod = serde_json::from_value(json!({
        "apiVersion": "v1", "kind": "Pod",
        "metadata": { "name": name, "labels": { "app.kubernetes.io/managed-by": MANAGER, "app.kubernetes.io/name": "pyo-locate" } },
        "spec": {
            "nodeName": node, "hostNetwork": true, "restartPolicy": "Never",
            "tolerations": [{ "operator": "Exists" }],
            "containers": [{
                "name": "locate", "image": IMAGE,
                // The server's own IPv4 addresses ("a " lines); then the
                // lookup once per family, each answer's lines prefixed with
                // it (a family the server lacks just fails). The URL is an
                // argument, never part of the script; empty: no lookup.
                "command": ["sh", "-c", "ip -4 -o addr show 2>/dev/null | sed \"s/^/a /\"; [ -z \"$1\" ] || for f in 4 6; do curl -$f -s -m 8 \"$1\" | sed \"s/^/v$f /\"; done; exit 0", "locate", url.unwrap_or("")],
                "resources": { "limits": { "cpu": "100m", "memory": "32Mi" } },
                "securityContext": { "runAsNonRoot": true, "runAsUser": 65534, "runAsGroup": 65534, "allowPrivilegeEscalation": false, "readOnlyRootFilesystem": true, "capabilities": { "drop": ["ALL"] } },
            }],
        },
    }))?;
    pods.create(&PostParams::default(), &pod).await?;
    let result = async {
        for _ in 0..60 {
            tokio::time::sleep(Duration::from_secs(2)).await;
            let status = pods.get(&name).await?.status.unwrap_or_default();
            // A container that can't start (bad image, config) won't get better: say why.
            if let Some(w) = status
                .container_statuses
                .iter()
                .flatten()
                .find_map(|c| c.state.as_ref()?.waiting.as_ref())
                && matches!(
                    w.reason.as_deref(),
                    Some(
                        "CreateContainerConfigError"
                            | "ErrImagePull"
                            | "ImagePullBackOff"
                            | "InvalidImageName"
                    )
                )
            {
                anyhow::bail!(
                    "{}: {}",
                    w.reason.clone().unwrap_or_default(),
                    w.message.clone().unwrap_or_default()
                );
            }
            let phase = status.phase.unwrap_or_default();
            match phase.as_str() {
                "Succeeded" => {
                    let out = pods.logs(&name, &LogParams::default()).await?;
                    return parse_trace(&out).ok_or_else(|| {
                        anyhow::anyhow!("no public or Tailscale address found (no internet access from this server?)")
                    });
                }
                "Failed" => {
                    anyhow::bail!("the lookup failed (no internet access from this server?)")
                }
                _ => {}
            }
        }
        anyhow::bail!("timed out")
    }
    .await;
    let _ = pods.delete(&name, &DeleteParams::default()).await;
    result
}

/// "a 3: tailscale0 inet 100.101.67.2/32 …\nv4 ip=203.0.113.7\nv4 loc=DE\n
/// v6 ip=2001:db8::7\n…": each family's public address (if it answered),
/// the country and the Tailscale address. An answer that is just the
/// address ("v4 203.0.113.7") counts too. None when nothing was found.
fn parse_trace(out: &str) -> Option<Found> {
    let field = |fam: &str, k: &str| {
        out.lines()
            .find_map(|l| l.strip_prefix(&format!("{fam} {k}=")))
            .map(|v| v.trim().to_owned())
    };
    let ip = |fam: &str, v6: bool| {
        let fits = |ip: &String| {
            ip.parse::<std::net::IpAddr>()
                .is_ok_and(|a| a.is_ipv6() == v6)
        };
        field(fam, "ip").filter(fits).or_else(|| {
            out.lines()
                .filter_map(|l| l.strip_prefix(&format!("{fam} ")))
                .map(|v| v.trim().to_owned())
                .find(fits)
        })
    };
    let (v4, v6) = (ip("v4", false), ip("v6", true));
    let tailnet = tailnet_of(out);
    let nic_public = nic_public_of(out);
    if v4.is_none() && v6.is_none() && tailnet.is_none() && nic_public.is_empty() {
        return None;
    }
    let country = field("v4", "loc")
        .or_else(|| field("v6", "loc"))
        .filter(|c| c.len() == 2)
        .unwrap_or_default();
    Some(Found {
        v4,
        v6,
        country,
        tailnet,
        nic_public,
    })
}

/// The server's own IPv4 addresses ("a " lines of `ip -4 -o addr`), each
/// with its line.
fn own_addresses(out: &str) -> impl Iterator<Item = (&str, String)> {
    out.lines()
        .filter_map(|l| l.strip_prefix("a "))
        .filter_map(|l| {
            let mut words = l.split_whitespace();
            words.find(|w| *w == "inet")?;
            let ip = words.next()?.split('/').next()?.to_owned();
            Some((l, ip))
        })
}

/// The public IPv4 addresses among the server's own: not private,
/// tailnet/CGNAT, loopback, link-local or unspecified.
fn nic_public_of(out: &str) -> Vec<String> {
    let mut ips: Vec<String> = own_addresses(out)
        .map(|(_, ip)| ip)
        .filter(|ip| {
            ip.parse::<std::net::Ipv4Addr>().is_ok_and(|a| {
                let [x, y, ..] = a.octets();
                !(a.is_private()
                    || a.is_loopback()
                    || a.is_link_local()
                    || a.is_unspecified()
                    || a.is_broadcast()
                    || a.is_multicast()
                    || (x == 100 && (64..=127).contains(&y)))
            })
        })
        .collect();
    ips.dedup();
    ips
}

/// The Tailscale address among the server's own: on its tailscale
/// interface first, else any in Tailscale's range.
fn tailnet_of(out: &str) -> Option<String> {
    let addrs: Vec<(&str, String)> = own_addresses(out)
        .filter(|(_, ip)| tailnet_v4(ip))
        .collect();
    addrs
        .iter()
        .find(|(l, _)| l.contains("tailscale"))
        .or(addrs.first())
        .map(|(_, ip)| ip.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trace() {
        let both = "v4 fl=123\nv4 ip=203.0.113.7\nv4 loc=DE\nv6 ip=2001:db8::7\nv6 loc=DE\n";
        assert_eq!(
            parse_trace(both),
            Some(Found {
                v4: Some("203.0.113.7".into()),
                v6: Some("2001:db8::7".into()),
                country: "DE".into(),
                tailnet: None,
                nic_public: vec![]
            })
        );
        let v6_only = "v6 ip=2001:db8::7\nv6 loc=SG\n";
        assert_eq!(
            parse_trace(v6_only),
            Some(Found {
                v4: None,
                v6: Some("2001:db8::7".into()),
                country: "SG".into(),
                tailnet: None,
                nic_public: vec![]
            })
        );
        assert_eq!(parse_trace("nope"), None);
        // A family whose request failed (no IPv6 here): just the other.
        assert_eq!(
            parse_trace("v4 ip=129.226.158.31\nv4 loc=SG\n"),
            Some(Found {
                v4: Some("129.226.158.31".into()),
                v6: None,
                country: "SG".into(),
                tailnet: None,
                nic_public: vec![]
            })
        );
        // A lookup answering with just the address.
        assert_eq!(
            parse_trace("v4 129.226.158.31\n"),
            Some(Found {
                v4: Some("129.226.158.31".into()),
                v6: None,
                country: String::new(),
                tailnet: None,
                nic_public: vec![]
            })
        );
        // An IPv4 answer over IPv6 (no IPv6 route, a proxy) isn't an IPv6 address.
        assert_eq!(parse_trace("v6 129.226.158.31\n"), None);
    }

    /// What the pod printed on a cloud server behind NAT, on a tailnet.
    #[test]
    fn tailnet_and_public_addresses() {
        let out = "a 1: lo    inet 127.0.0.1/8 scope host lo\\       valid_lft forever preferred_lft forever\n\
                   a 2: eth0    inet 10.3.8.13/22 brd 10.3.11.255 scope global eth0\\       valid_lft forever\n\
                   a 3: tailscale0    inet 100.101.67.2/32 scope global tailscale0\\       valid_lft forever\n\
                   a 5: cni0    inet 10.42.0.1/24 brd 10.42.0.255 scope global cni0\n\
                   v4 ip=129.226.158.31\nv4 loc=SG\n";
        assert_eq!(
            parse_trace(out),
            Some(Found {
                v4: Some("129.226.158.31".into()),
                v6: None,
                country: "SG".into(),
                tailnet: Some("100.101.67.2".into()),
                nic_public: vec![]
            })
        );
        // No internet (or the lookup off): the Tailscale address alone.
        let tailnet_only = "a 3: tailscale0    inet 100.101.67.2/32 scope global tailscale0\n";
        assert_eq!(
            parse_trace(tailnet_only).unwrap().tailnet.as_deref(),
            Some("100.101.67.2")
        );
        // A server with its public address on its own network card.
        let direct = "a 1: lo    inet 127.0.0.1/8 scope host lo\n\
                      a 2: eth0    inet 203.0.113.7/24 brd 203.0.113.255 scope global eth0\n\
                      a 3: eth1    inet 10.0.0.4/24 scope global eth1\n\
                      a 4: tailscale0    inet 100.64.0.9/32 scope global tailscale0\n\
                      v4 ip=203.0.113.7\nv4 loc=DE\n";
        assert_eq!(parse_trace(direct).unwrap().nic_public, ["203.0.113.7"]);
        // Even with the lookup off.
        assert_eq!(
            parse_trace("a 2: eth0    inet 203.0.113.7/24 scope global eth0\n")
                .unwrap()
                .nic_public,
            ["203.0.113.7"]
        );
        // Not on a tailnet, no internet: nothing found.
        assert_eq!(
            parse_trace("a 2: eth0    inet 10.3.8.13/22 scope global eth0\n"),
            None
        );
        // Another interface name (userspace networking): any address in its range.
        assert_eq!(
            tailnet_of("a 4: ts0    inet 100.64.0.9/32 scope global ts0\n").as_deref(),
            Some("100.64.0.9")
        );
        assert!(tailnet_v4("100.127.0.1") && !tailnet_v4("100.128.0.1") && !tailnet_v4("10.0.0.1"));
    }

    #[test]
    fn lookup_can_change_or_be_off() {
        assert_eq!(lookup_url(None).as_deref(), Some(LOOKUP));
        assert_eq!(lookup_url(Some(" ")).as_deref(), Some(LOOKUP));
        assert_eq!(lookup_url(Some("off")), None);
        assert_eq!(lookup_url(Some("OFF")), None);
        assert_eq!(
            lookup_url(Some("https://api.ipify.org")).as_deref(),
            Some("https://api.ipify.org")
        );
    }
}
