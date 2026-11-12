//! Where each server is: its public address as the internet sees it.
//!
//! Node addresses are often private or tailnet IPs, which say nothing about
//! location. So, for each ready server not checked lately, the agent runs a
//! one-off pod on it (host network) that asks Cloudflare's trace endpoint
//! for the server's public IP and country, once over IPv4 and once over
//! IPv6 (a server can have one of each), and keeps the answer as node
//! annotations. The heartbeat reports them; the API looks the IP up (GeoIP)
//! for the city.

use std::time::Duration;

use k8s_openapi::api::core::v1::{Node, Pod};
use kube::{
    Api, Client, ResourceExt,
    api::{DeleteParams, ListParams, LogParams, Patch, PatchParams, PostParams},
};
use pyo_proto::{
    LOCATED_AT_ANNOTATION, PUBLIC_COUNTRY_ANNOTATION, PUBLIC_IP_ANNOTATION, PUBLIC_IPV4_ANNOTATION,
    PUBLIC_IPV6_ANNOTATION, now_ms,
};
use serde_json::json;

use crate::MANAGER;

const NS: &str = "pyo-system";
const IMAGE: &str = "curlimages/curl:8.11.1";
/// How often a server is checked again (its address can change).
const EVERY: Duration = Duration::from_secs(24 * 3600);

pub async fn run(client: Client) {
    let mut tick = tokio::time::interval(Duration::from_secs(600));
    loop {
        tick.tick().await;
        if let Err(e) = locate_all(&client).await {
            tracing::warn!(error = %e, "couldn't locate servers");
        }
    }
}

async fn locate_all(client: &Client) -> anyhow::Result<()> {
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
        if !ready || now_ms() - checked < EVERY.as_millis() as i64 {
            continue;
        }
        let name = n.name_any();
        match locate(client, &name).await {
            Ok(found) => {
                // The one address for location: IPv4 when there is one (GeoIP knows it better).
                let ip = found
                    .v4
                    .clone()
                    .or_else(|| found.v6.clone())
                    .unwrap_or_default();
                let country = found.country.clone();
                // A family it no longer has is removed (null).
                let patch = json!({ "metadata": { "annotations": {
                    PUBLIC_IP_ANNOTATION: ip, PUBLIC_IPV4_ANNOTATION: found.v4, PUBLIC_IPV6_ANNOTATION: found.v6,
                    PUBLIC_COUNTRY_ANNOTATION: country, LOCATED_AT_ANNOTATION: now_ms().to_string(),
                } } });
                nodes
                    .patch(&name, &PatchParams::default(), &Patch::Merge(&patch))
                    .await?;
                tracing::info!(node = %name, %ip, %country, "located server");
            }
            Err(e) => tracing::warn!(node = %name, error = %e, "couldn't locate server"),
        }
    }
    Ok(())
}

/// What a lookup found: the public address over each family, and the country.
#[derive(Debug, PartialEq)]
struct Found {
    v4: Option<String>,
    v6: Option<String>,
    country: String,
}

/// Public IPs and country code of one server.
async fn locate(client: &Client, node: &str) -> anyhow::Result<Found> {
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
                // Once per family, each answer's lines prefixed with it; a
                // family the server lacks just fails.
                "command": ["sh", "-c", "for f in 4 6; do curl -$f -s -m 8 https://cloudflare.com/cdn-cgi/trace | sed \"s/^/v$f /\"; done; exit 0"],
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
                        anyhow::anyhow!("unexpected answer from the trace endpoint")
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

/// "v4 ip=203.0.113.7\nv4 loc=DE\nv6 ip=2001:db8::7\n…": each family's
/// address (if it answered) and the country. None when neither did.
fn parse_trace(out: &str) -> Option<Found> {
    let field = |fam: &str, k: &str| {
        out.lines()
            .find_map(|l| l.strip_prefix(&format!("{fam} {k}=")))
            .map(|v| v.trim().to_owned())
    };
    let ip = |fam: &str, v6: bool| {
        field(fam, "ip").filter(|ip| {
            ip.parse::<std::net::IpAddr>()
                .is_ok_and(|a| a.is_ipv6() == v6)
        })
    };
    let (v4, v6) = (ip("v4", false), ip("v6", true));
    if v4.is_none() && v6.is_none() {
        return None;
    }
    let country = field("v4", "loc")
        .or_else(|| field("v6", "loc"))
        .filter(|c| c.len() == 2)
        .unwrap_or_default();
    Some(Found { v4, v6, country })
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
                country: "DE".into()
            })
        );
        let v6_only = "v6 ip=2001:db8::7\nv6 loc=SG\n";
        assert_eq!(
            parse_trace(v6_only),
            Some(Found {
                v4: None,
                v6: Some("2001:db8::7".into()),
                country: "SG".into()
            })
        );
        assert_eq!(parse_trace("nope"), None);
    }
}
