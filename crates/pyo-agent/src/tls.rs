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
    loop {
        match ensure_once(&client).await {
            Ok(()) => {
                tracing::info!("HTTPS ready: cert-manager and issuers in place");
                return;
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
            let ready = c.data["status"]["conditions"]
                .as_array()
                .and_then(|cs| cs.iter().find(|x| x["type"] == json!("Ready")));
            Some(CertificateReport {
                service_id,
                hostname,
                ready: ready.is_some_and(|r| r["status"] == json!("True")),
                message: ready
                    .and_then(|r| r["message"].as_str())
                    .unwrap_or("")
                    .to_owned(),
            })
        })
        .collect()
}
