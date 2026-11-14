//! Pyo's dashboard on the public IP (pyo-proto system.rs
//! `public_dashboard_objects`), on the cluster Pyo runs on: applied while
//! the API asks for it, removed once it's closed.

use kube::Client;
use kube::api::{DeleteParams, Patch, PatchParams};
use pyo_proto::{PublicDashboard, system};

/// What was last applied (a heartbeat that changes nothing writes nothing).
static APPLIED: std::sync::Mutex<Option<Option<String>>> = std::sync::Mutex::new(None);

pub async fn apply(kube: &Client, wanted: &PublicDashboard) {
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
