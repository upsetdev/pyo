//! Pyo's dashboard on the public IP (pyo-proto system.rs
//! `public_dashboard_objects`), on the cluster Pyo runs on: applied while
//! the API asks for it, removed once it's closed. Its certificate is how
//! Pyo learns the internet reaches the public IP: Let's Encrypt issues it
//! only when it reached the server on port 80 (the heartbeat reports it).
//!
//! A try that failed (ports 80 and 443 closed in a cloud firewall) is
//! tried again by cert-manager, an hour later, then two, four and so on.
//! Past [`RETRY_CAP`], the certificate is made again, which starts that
//! over: ports opened later are picked up within hours, and Let's
//! Encrypt's limit of 5 failed checks per name an hour is never near. The
//! install script, run again, removes a refused one (15 minutes after its
//! try at the soonest) to have it tried now.

use std::time::Duration;

use kube::api::{DeleteParams, DynamicObject, Patch, PatchParams};
use kube::{Api, Client};
use pyo_proto::{CertificateReport, PublicDashboard, system};

/// The longest wait between two tries at the certificate.
const RETRY_CAP: Duration = Duration::from_secs(6 * 3600);

fn certificates(kube: &Client) -> Api<DynamicObject> {
    Api::namespaced_with(
        kube.clone(),
        system::GATEWAY_NS,
        &crate::tls::certificate_resource(),
    )
}

/// The certificate as cert-manager has it, for the heartbeat (none while
/// there's no public dashboard).
pub async fn report(kube: &Client) -> Option<CertificateReport> {
    let c = certificates(kube)
        .get_opt(system::PUBLIC_DASHBOARD_TLS)
        .await
        .ok()??;
    let hostname = c.data["spec"]["dnsNames"][0].as_str()?.to_owned();
    let created = c
        .metadata
        .creation_timestamp
        .as_ref()
        .map(|t| t.0.as_second());
    Some(CertificateReport {
        hostname,
        ..crate::tls::state(&c.data["status"], created)
    })
}

/// Its last try failed longer than [`RETRY_CAP`] ago (unix seconds `now`):
/// time to make it again rather than wait for cert-manager's next try.
fn retry_now(status: &serde_json::Value, now: i64) -> bool {
    let ready = status["conditions"]
        .as_array()
        .into_iter()
        .flatten()
        .any(|c| c["type"] == "Ready" && c["status"] == "True");
    let failed_at = status["lastFailureTime"]
        .as_str()
        .and_then(|t| {
            serde_json::from_value::<k8s_openapi::apimachinery::pkg::apis::meta::v1::Time>(t.into())
                .ok()
        })
        .map(|t| t.0.as_second());
    !ready && failed_at.is_some_and(|t| now - t >= RETRY_CAP.as_secs() as i64)
}

/// A certificate whose try failed long ago is removed, so it's made again
/// at once (with a fresh try) by [`apply`]; so is one that's gone (the
/// install script removes a refused one to try again now).
async fn retry_failed(kube: &Client) {
    let api = certificates(kube);
    let c = match api.get_opt(system::PUBLIC_DASHBOARD_TLS).await {
        Ok(Some(c)) => c,
        Ok(None) => {
            if let Ok(mut a) = APPLIED.lock() {
                *a = None;
            }
            return;
        }
        Err(_) => return,
    };
    if !retry_now(&c.data["status"], pyo_proto::now_ms() / 1000) {
        return;
    }
    match api
        .delete(system::PUBLIC_DASHBOARD_TLS, &DeleteParams::default())
        .await
    {
        Ok(_) => {
            tracing::info!("public dashboard: trying its certificate again");
            if let Ok(mut a) = APPLIED.lock() {
                *a = None;
            }
        }
        Err(e) => tracing::warn!(error = %e, "couldn't renew the public dashboard's certificate"),
    }
}

/// What was last applied (a heartbeat that changes nothing writes nothing).
static APPLIED: std::sync::Mutex<Option<Option<String>>> = std::sync::Mutex::new(None);

pub async fn apply(kube: &Client, wanted: &PublicDashboard) {
    if wanted.host.is_some() {
        retry_failed(kube).await;
    }
    if APPLIED
        .lock()
        .map(|a| a.as_ref() == Some(&wanted.host))
        .unwrap_or(false)
    {
        return;
    }
    let pp = PatchParams::apply(crate::MANAGER).force();
    let mut ok = true;
    match &wanted.host {
        Some(host) => {
            for o in system::public_dashboard_objects(host) {
                let kind = o["kind"].as_str().unwrap_or("");
                let name = o["metadata"]["name"].as_str().unwrap_or("");
                if let Err(e) = crate::gateway::api_of_kind(kube, kind)
                    .patch(name, &pp, &Patch::Apply(&o))
                    .await
                {
                    tracing::warn!(error = %e, kind, name, "couldn't apply the public dashboard");
                    ok = false;
                }
            }
        }
        None => {
            for (kind, name) in [
                ("HTTPRoute", system::PUBLIC_DASHBOARD_REDIRECT),
                ("HTTPRoute", system::PUBLIC_DASHBOARD),
                ("Gateway", system::PUBLIC_DASHBOARD),
                ("Backend", system::PUBLIC_DASHBOARD),
                ("Certificate", system::PUBLIC_DASHBOARD_TLS),
            ] {
                match crate::gateway::api_of_kind(kube, kind)
                    .delete(name, &DeleteParams::default())
                    .await
                {
                    Ok(_) => {}
                    Err(kube::Error::Api(e)) if e.code == 404 => {}
                    Err(e) => {
                        tracing::warn!(error = %e, kind, name, "couldn't remove the public dashboard");
                        ok = false;
                    }
                }
            }
            let _ = kube::Api::<k8s_openapi::api::core::v1::Secret>::namespaced(
                kube.clone(),
                system::GATEWAY_NS,
            )
            .delete(system::PUBLIC_DASHBOARD_TLS, &DeleteParams::default())
            .await;
        }
    }
    if ok {
        tracing::info!(host = ?wanted.host, "public dashboard");
        if let Ok(mut a) = APPLIED.lock() {
            *a = Some(wanted.host.clone());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_failed_try_is_made_again_after_the_cap() {
        let failed = |at: &str| json!({ "lastFailureTime": at, "conditions": [{ "type": "Ready", "status": "False" }] });
        let t = |s: &str| {
            serde_json::from_value::<k8s_openapi::apimachinery::pkg::apis::meta::v1::Time>(json!(s))
                .unwrap()
                .0
                .as_second()
        };
        let now = t("2026-11-17T18:00:00Z");
        // cert-manager tries again within the cap itself.
        assert!(!retry_now(&failed("2026-11-17T17:00:00Z"), now));
        assert!(!retry_now(&failed("2026-11-17T12:00:01Z"), now));
        assert!(retry_now(&failed("2026-11-17T12:00:00Z"), now));
        // Issued (an older failure kept), on its way, or never failed: no.
        assert!(!retry_now(
            &json!({ "lastFailureTime": "2026-11-16T00:00:00Z", "conditions": [{ "type": "Ready", "status": "True" }] }),
            now
        ));
        assert!(!retry_now(&json!({ "conditions": [] }), now));
        assert!(!retry_now(&json!({}), now));
    }
}
