//! HTTPS for service domains: cert-manager (installed by the install manifest,
//! through k3s's Helm controller), with two cluster issuers the agent adds:
//! - `pyo-letsencrypt`: Let's Encrypt over HTTP-01 through the gateway's
//!   shared :80 listener, for domains the internet can reach;
//! - `pyo-selfsigned`: for private ones (tailnet, LAN, .localhost), which
//!   Let's Encrypt can't validate. Browsers warn about these.
//!
//! Each domain's Certificate lives with the gateway (gateway.rs); this
//! keeps the issuers and reports how certificates are doing.

use std::time::Duration;

use kube::{
    Api, Client, ResourceExt,
    api::{DynamicObject, ListParams, Patch, PatchParams},
    core::{ApiResource, GroupVersionKind},
};
use pyo_proto::CertificateReport;
use serde_json::json;

use crate::{MANAGER, observe::SERVICE_LABEL};

fn pp() -> PatchParams {
    PatchParams::apply(MANAGER).force()
}

fn resource(group: &str, version: &str, kind: &str) -> ApiResource {
    ApiResource::from_gvk(&GroupVersionKind::gvk(group, version, kind))
}

pub fn certificate_resource() -> ApiResource {
    resource("cert-manager.io", "v1", "Certificate")
}

/// The issuers, once cert-manager (from the install manifest) is there;
/// retried until its CRDs appear.
pub async fn ensure(client: Client) {
    let mut refusals = 0;
    loop {
        match ensure_once(&client).await {
            Ok(()) => {
                tracing::info!("HTTPS ready: cert-manager and issuers in place");
                return;
            }
            // The cluster's install is older than Pyo's: tried again once
            // its connect command runs.
            Err(e) if crate::refused::in_chain(&e) => {
                refusals += 1;
                tracing::warn!(error = %format!("{e:#}"), "the cluster's install refuses the HTTPS issuers; run the cluster's connect command (tried again once the install changes)");
                crate::refused::wait_for_install(&client, refusals).await;
            }
            Err(e) => {
                tracing::info!(error = %format!("{e:#}"), "HTTPS not ready yet; retrying");
                tokio::time::sleep(Duration::from_secs(30)).await;
            }
        }
    }
}

async fn ensure_once(client: &Client) -> anyhow::Result<()> {
    // cert-manager itself comes with the install manifest (pyo-proto's
    // system.rs); the agent only adds the issuers, once it's there.
    let issuers: Api<DynamicObject> = Api::all_with(
        client.clone(),
        &resource("cert-manager.io", "v1", "ClusterIssuer"),
    );
    for (name, spec) in pyo_proto::system::cluster_issuers() {
        let obj: DynamicObject = serde_json::from_value(json!({
            "apiVersion": "cert-manager.io/v1", "kind": "ClusterIssuer",
            "metadata": { "name": name, "labels": { "app.kubernetes.io/managed-by": MANAGER } },
            "spec": spec,
        }))?;
        issuers.patch(name, &pp(), &Patch::Apply(&obj)).await?;
    }
    Ok(())
}

/// Readiness of every service certificate, for the heartbeat.
pub async fn report(client: &Client) -> Vec<CertificateReport> {
    let api: Api<DynamicObject> = Api::all_with(client.clone(), &certificate_resource());
    let Ok(list) = api.list(&ListParams::default().labels(SERVICE_LABEL)).await else {
        return vec![];
    };
    list.items
        .into_iter()
        .filter_map(|c| {
            let service_id = c.labels().get(SERVICE_LABEL)?.clone();
            let hostname = c.data["spec"]["dnsNames"][0].as_str()?.to_owned();
            let created = c
                .metadata
                .creation_timestamp
                .as_ref()
                .map(|t| t.0.as_second());
            Some(CertificateReport {
                service_id,
                hostname,
                ..state(&c.data["status"], created)
            })
        })
        .collect()
}

/// A Certificate's status as the heartbeat reports it (no service or
/// hostname yet).
pub(crate) fn state(status: &serde_json::Value, created: Option<i64>) -> CertificateReport {
    let cond = |t: &str| {
        status["conditions"]
            .as_array()
            .and_then(|cs| cs.iter().find(|x| x["type"] == json!(t)))
    };
    let ready = cond("Ready");
    let issuing = cond("Issuing");
    let failed = !ready.is_some_and(|r| r["status"] == json!("True"))
        && (issuing.is_some_and(|i| i["reason"] == json!("Failed"))
            || status["lastFailureTime"].is_string());
    let text = |c: Option<&serde_json::Value>| {
        c.and_then(|c| c["message"].as_str())
            .unwrap_or("")
            .to_owned()
    };
    let since = issuing
        .filter(|i| i["status"] == json!("True"))
        .and_then(|i| {
            serde_json::from_value::<k8s_openapi::apimachinery::pkg::apis::meta::v1::Time>(
                i["lastTransitionTime"].clone(),
            )
            .ok()
        })
        .map(|t| t.0.as_second())
        .or(created);
    CertificateReport {
        service_id: String::new(),
        hostname: String::new(),
        ready: ready.is_some_and(|r| r["status"] == json!("True")),
        message: text(ready),
        failed,
        failure: if failed {
            text(issuing).chars().take(1000).collect()
        } else {
            String::new()
        },
        issuing_since: since,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_failed_try_carries_its_reason() {
        let s = state(
            &json!({ "lastFailureTime": "2026-11-15T10:00:00Z", "conditions": [
                { "type": "Ready", "status": "False", "message": "Issuing certificate as Secret does not exist" },
                { "type": "Issuing", "status": "False", "reason": "Failed", "message": "The certificate request has failed to complete and will be retried: 429 urn:ietf:params:acme:error:rateLimited: too many certificates" },
            ] }),
            Some(1),
        );
        assert!(!s.ready && s.failed);
        assert!(s.failure.contains("rateLimited"));
    }

    #[test]
    fn a_pending_try_says_since_when() {
        let s = state(
            &json!({ "conditions": [
                { "type": "Ready", "status": "False", "message": "Issuing certificate as Secret does not exist" },
                { "type": "Issuing", "status": "True", "reason": "DoesNotExist", "lastTransitionTime": "2026-11-15T10:00:00Z" },
            ] }),
            Some(1),
        );
        assert!(!s.failed && s.failure.is_empty());
        assert_eq!(s.issuing_since, Some(1_794_736_800));
        let ready = state(
            &json!({ "conditions": [{ "type": "Ready", "status": "True" }] }),
            Some(5),
        );
        assert!(ready.ready && !ready.failed);
        assert_eq!(ready.issuing_since, Some(5));
    }
}
