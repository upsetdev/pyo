//! The web application firewall: Coraza with the OWASP Core Rule Set,
//! as an external processor beside the gateway (the processor is the
//! repository's `waf/`).
//!
//! It's a system bundle in its own namespace (pyo-proto's bundles.rs
//! `waf_bundle`), which the API sends when the first service on the cluster
//! turns its WAF on and removes when the last one turns it off. Here: its
//! settings ConfigMap, rebuilt from the routes' policies after every change
//! (an EnvoyExtensionPolicy on each route of a service with its WAF on,
//! gateway.rs), waiting
//! for it before routes point at it, and removing the routes' policies when
//! it goes. The settings are written only while it's on (the command says)
//! and its bundle is here with the agent's binding in effect ([`WafState`]):
//! the namespace has no binding for the agent before that.

use std::time::Duration;

use k8s_openapi::api::{
    apps::v1::DaemonSet,
    core::v1::{ConfigMap, Namespace},
};
use kube::{
    Api, Client, ResourceExt,
    api::{DeleteParams, DynamicObject, ListParams, Patch, PatchParams},
};
use pyo_proto::{
    bundles::{BUNDLE_LABEL, WAF_ID},
    system::{self, GATEWAY_NS, WAF_NAME, WAF_NS, WafRoute},
};
use serde_json::json;

use crate::{MANAGER, gateway};

/// Every route's WAF, from the routes' WAF policies.
async fn routes(client: &Client) -> anyhow::Result<Vec<WafRoute>> {
    let policies = Api::<DynamicObject>::namespaced_with(
        client.clone(),
        GATEWAY_NS,
        &gateway::extension_policies(),
    )
    .list(&ListParams::default())
    .await?;
    Ok(policies
        .items
        .iter()
        .filter_map(|p| p.annotations().get(gateway::WAF_ANNOTATION))
        .filter_map(|a| serde_json::from_str(a).ok())
        .collect())
}

/// What the agent knows of the WAF on this cluster, before it writes the
/// settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WafState {
    /// The API says it runs here: some service has its WAF on (a deploy's
    /// or firewall sync's `waf`).
    pub on: bool,
    /// Its bundle is here: its namespace, labelled as the WAF bundle's and
    /// not on its way out.
    pub installed: bool,
    /// The agent's binding there is in effect (bundles.rs makes it right
    /// after the namespace, before any of the bundle's objects).
    pub writable: bool,
}

impl WafState {
    /// Whether to write its settings. Off, or not (yet) installed: nothing
    /// to write to, and no warning (the bundle's install writes them,
    /// bundles.rs, as does the firewall sync once it's there, `wait_ready`).
    pub fn write_settings(self) -> bool {
        self.on && self.installed && self.writable
    }
}

/// Its bundle's namespace, as a namespace on the cluster says.
pub fn bundle_namespace(ns: Option<&Namespace>) -> bool {
    ns.is_some_and(|n| {
        n.labels().get(BUNDLE_LABEL).map(String::as_str) == Some(WAF_ID)
            && n.metadata.deletion_timestamp.is_none()
    })
}

/// Where it stands: `on` from the command, the rest asked of the cluster
/// only when it's on.
async fn state(client: &Client, on: bool) -> anyhow::Result<WafState> {
    let mut s = WafState {
        on,
        installed: false,
        writable: false,
    };
    if !on {
        return Ok(s);
    }
    s.installed = bundle_namespace(
        Api::<Namespace>::all(client.clone())
            .get_opt(WAF_NS)
            .await?
            .as_ref(),
    );
    if s.installed {
        s.writable = crate::scope::can_named(
            client,
            "patch",
            "",
            "configmaps",
            Some(WAF_NS),
            Some(WAF_NAME),
        )
        .await;
    }
    Ok(s)
}

/// Its settings, from the routes (the processor follows its ConfigMap
/// through the API, so a change is in effect within a second), while it's `on` in this cluster (the
/// command's word) and installed. Whether they were written: not while
/// it's off or its bundle isn't (all) there yet.
pub async fn settings(client: &Client, on: bool) -> anyhow::Result<bool> {
    let s = state(client, on).await?;
    if !s.write_settings() {
        tracing::debug!(?s, "not writing the WAF's settings");
        return Ok(false);
    }
    let maps: Api<ConfigMap> = Api::namespaced(client.clone(), WAF_NS);
    // The bundle's ConfigMap, applied after the binding: until it's there,
    // the bundle is still being applied (and writes them at its end).
    if maps.get_opt(WAF_NAME).await?.is_none() {
        return Ok(false);
    }
    let config = system::waf_config(&routes(client).await?);
    // Only `config.json`: the rest of the ConfigMap is the bundle's.
    let obj = json!({
        "apiVersion": "v1", "kind": "ConfigMap",
        "metadata": { "name": WAF_NAME, "namespace": WAF_NS },
        "data": { "config.json": serde_json::to_string_pretty(&config)? },
    });
    maps.patch(
        WAF_NAME,
        &PatchParams::apply(MANAGER).force(),
        &Patch::Apply(&obj),
    )
    .await?;
    Ok(true)
}

/// Wait (up to two minutes) for the processor the API installs: its
/// settings in place, then a copy ready on every server with a proxy.
/// Whether it's ready.
pub async fn wait_ready(client: &Client) -> bool {
    let sets: Api<DaemonSet> = Api::namespaced(client.clone(), WAF_NS);
    let mut settled = false;
    for _ in 0..120 {
        if !settled {
            settled = match settings(client, true).await {
                Ok(written) => written,
                Err(e) => {
                    tracing::warn!(error = %format!("{e:#}"), "couldn't write the WAF's settings");
                    false
                }
            };
        }
        let ready = sets
            .get_opt(WAF_NAME)
            .await
            .ok()
            .flatten()
            .and_then(|d| d.status)
            .is_some_and(|st| {
                st.desired_number_scheduled > 0 && st.number_ready >= st.desired_number_scheduled
            });
        if settled && ready {
            return true;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    false
}

/// The routes' WAF policies that are left (it's off).
pub async fn remove_policies(client: &Client) -> anyhow::Result<()> {
    let api = Api::<DynamicObject>::namespaced_with(
        client.clone(),
        GATEWAY_NS,
        &gateway::extension_policies(),
    );
    for p in api
        .list(&ListParams::default())
        .await?
        .items
        .iter()
        .filter(|p| p.annotations().contains_key(gateway::WAF_ANNOTATION))
    {
        let _ = api.delete(&p.name_any(), &DeleteParams::default()).await;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ns(bundle: Option<&str>, deleting: bool) -> Namespace {
        let mut meta = json!({ "name": WAF_NS });
        if let Some(b) = bundle {
            meta["labels"] = json!({ BUNDLE_LABEL: b });
        }
        if deleting {
            meta["deletionTimestamp"] = json!("2026-11-12T00:00:00Z");
        }
        serde_json::from_value(json!({ "metadata": meta })).unwrap()
    }

    #[test]
    fn settings_only_when_on_installed_and_writable() {
        let s = |on, installed, writable| {
            WafState {
                on,
                installed,
                writable,
            }
            .write_settings()
        };
        assert!(s(true, true, true));
        // No service wants it (no bundle, no namespace, no binding): nothing,
        // whatever the cluster has left.
        assert!(!s(false, true, true));
        assert!(!s(false, false, false));
        // On, but its bundle isn't applied yet, or its binding isn't in effect.
        assert!(!s(true, false, false));
        assert!(!s(true, true, false));
    }

    #[test]
    fn its_namespace_is_the_bundles() {
        assert!(bundle_namespace(Some(&ns(Some(WAF_ID), false))));
        assert!(!bundle_namespace(None));
        assert!(!bundle_namespace(Some(&ns(None, false))));
        assert!(!bundle_namespace(Some(&ns(Some("databases"), false))));
        assert!(!bundle_namespace(Some(&ns(Some(WAF_ID), true))));
    }
}
