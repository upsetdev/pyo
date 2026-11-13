//! What the agent may do in its cluster (pyo-proto's rbac.rs), and the
//! customer's switches in `pyo-system/pyo-policy`.
//!
//! - Each namespace it writes in is labelled `pyo.dev/managed=true` and gets
//!   a RoleBinding to the write role (`ensure_managed`).
//! - Every heartbeat it finds out what it may do (`detect`) and reads the
//!   policy, which `refusal` applies to commands.

use std::sync::{Arc, RwLock};

use k8s_openapi::api::{
    authorization::v1::{ResourceAttributes, SelfSubjectAccessReview, SelfSubjectAccessReviewSpec},
    core::v1::ConfigMap,
};
use kube::{
    Api, Client, ResourceExt,
    api::{DynamicObject, ListParams, Patch, PatchParams, PostParams},
    core::{ApiResource, GroupVersionKind},
};
use pyo_proto::{
    Command,
    rbac::{self, AgentScope, Policy},
};
use serde_json::Value;

/// Field manager of the managed label and the write binding, apart from the
/// rest of a namespace's labels (which rollouts own).
const ACCESS_MANAGER: &str = "pyo-access";

pub type SharedPolicy = Arc<RwLock<Policy>>;

fn pp() -> PatchParams {
    PatchParams::apply(ACCESS_MANAGER).force()
}

fn resource_of(o: &Value) -> (ApiResource, Option<String>, String) {
    let api_version = o["apiVersion"].as_str().unwrap_or("");
    let kind = o["kind"].as_str().unwrap_or("");
    let (group, version) = api_version.split_once('/').unwrap_or(("", api_version));
    let plural = match kind {
        "ValidatingAdmissionPolicy" => "validatingadmissionpolicies".to_owned(),
        "ValidatingAdmissionPolicyBinding" => "validatingadmissionpolicybindings".to_owned(),
        k => format!("{}s", k.to_ascii_lowercase()),
    };
    let ar =
        ApiResource::from_gvk_with_plural(&GroupVersionKind::gvk(group, version, kind), &plural);
    (
        ar,
        o["metadata"]["namespace"].as_str().map(str::to_owned),
        o["metadata"]["name"].as_str().unwrap_or("").to_owned(),
    )
}

pub fn api_for(client: &Client, o: &Value) -> (Api<DynamicObject>, String) {
    let (ar, ns, name) = resource_of(o);
    let api = match ns {
        Some(ns) => Api::namespaced_with(client.clone(), &ns, &ar),
        None => Api::all_with(client.clone(), &ar),
    };
    (api, name)
}

async fn apply(client: &Client, o: &Value) -> anyhow::Result<()> {
    let (api, name) = api_for(client, o);
    api.patch(&name, &pp(), &Patch::Apply(o))
        .await
        .map_err(|e| anyhow::anyhow!("{} {name}: {e}", o["kind"].as_str().unwrap_or("")))?;
    Ok(())
}

/// Whether an error is the admission policy refusing a namespace that isn't
/// labelled managed. Right after the label is set the policy can still see
/// the namespace without it, for a moment.
fn unlabelled_refusal(e: &anyhow::Error) -> bool {
    let m = format!("{e:#}");
    m.contains("ValidatingAdmissionPolicy")
        && m.contains("pyo.dev/managed=true")
        && m.contains("isn't one")
}

/// A namespace the agent writes in: labelled, with the write role bound.
/// A new binding takes a moment to apply; this waits until it does.
pub async fn ensure_managed(client: &Client, ns: &str) -> anyhow::Result<()> {
    apply(client, &rbac::managed_namespace(ns)).await?;
    // The policy reads namespaces from a cache that can lag the label.
    let binding = rbac::write_binding(ns);
    let mut wait = std::time::Duration::from_millis(100);
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        match apply(client, &binding).await {
            Ok(()) => break,
            Err(e) if unlabelled_refusal(&e) && tokio::time::Instant::now() < deadline => {
                tokio::time::sleep(wait).await;
                wait = (wait * 2).min(std::time::Duration::from_secs(1));
            }
            Err(e) => return Err(e),
        }
    }
    // Installed bundles' bindings (the databases controller's, say).
    if let Err(e) = crate::bundles::bind_in(client, ns).await {
        tracing::warn!(namespace = ns, error = %format!("{e:#}"), "couldn't bind system bundles in a namespace");
    }
    for _ in 0..40 {
        if can(client, "patch", "", "secrets", Some(ns)).await {
            return Ok(());
        }
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
    anyhow::bail!("the agent's role in {ns} didn't take effect")
}

/// Wait until the agent's binding in a bundle's namespace applies.
pub async fn wait_for_write(client: &Client, ns: &str) -> anyhow::Result<()> {
    for _ in 0..40 {
        if can(client, "patch", "apps", "deployments", Some(ns)).await {
            return Ok(());
        }
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
    anyhow::bail!("the agent's role in {ns} didn't take effect")
}

/// Whether the agent may do something (a SelfSubjectAccessReview).
async fn can(client: &Client, verb: &str, group: &str, resource: &str, ns: Option<&str>) -> bool {
    can_named(client, verb, group, resource, ns, None).await
}

/// The same, for one object by name.
pub async fn can_named(
    client: &Client,
    verb: &str,
    group: &str,
    resource: &str,
    ns: Option<&str>,
    name: Option<&str>,
) -> bool {
    let review = SelfSubjectAccessReview {
        spec: SelfSubjectAccessReviewSpec {
            resource_attributes: Some(ResourceAttributes {
                verb: Some(verb.into()),
                group: Some(group.into()),
                resource: Some(resource.into()),
                namespace: ns.map(str::to_owned),
                name: name.map(str::to_owned),
                ..Default::default()
            }),
            ..Default::default()
        },
        ..Default::default()
    };
    match Api::<SelfSubjectAccessReview>::all(client.clone())
        .create(&PostParams::default(), &review)
        .await
    {
        Ok(r) => r.status.is_some_and(|s| s.allowed),
        Err(_) => false,
    }
}

/// The gateway's reach: whether its controller may read Secrets in
/// any namespace but its own (a SubjectAccessReview, which only asks), and
/// whether its proxies' pods carry a cluster token. None without the proxy.
async fn proxy_scope(client: &Client) -> Option<pyo_proto::rbac::ProxyScope> {
    use k8s_openapi::api::{
        authorization::v1::{SubjectAccessReview, SubjectAccessReviewSpec},
        core::v1::Pod,
    };
    let ns = pyo_proto::system::GATEWAY_NS;
    let pods =
        Api::<Pod>::namespaced(client.clone(), ns)
            .list(&ListParams::default().labels(
                "app.kubernetes.io/component=proxy,app.kubernetes.io/managed-by=envoy-gateway",
            ))
            .await
            .ok()?
            .items;
    if pods.is_empty() {
        return None;
    }
    let user = format!("system:serviceaccount:{ns}:envoy-gateway");
    let review = SubjectAccessReview {
        spec: SubjectAccessReviewSpec {
            user: Some(user),
            groups: Some(vec![
                "system:serviceaccounts".into(),
                format!("system:serviceaccounts:{ns}"),
            ]),
            resource_attributes: Some(ResourceAttributes {
                verb: Some("list".into()),
                resource: Some("secrets".into()),
                ..Default::default()
            }),
            ..Default::default()
        },
        ..Default::default()
    };
    let wide = Api::<SubjectAccessReview>::all(client.clone())
        .create(&PostParams::default(), &review)
        .await
        .ok()?
        .status
        .is_some_and(|s| s.allowed);
    let tokens = pods.iter().any(|p| {
        p.spec
            .as_ref()
            .is_none_or(|s| s.automount_service_account_token != Some(false))
    });
    Some(pyo_proto::rbac::ProxyScope {
        controller_reads_other_secrets: wide,
        proxies_have_credentials: tokens,
    })
}

/// Whether this cluster's API has admission policies at all.
async fn admission_supported(client: &Client) -> bool {
    match client
        .list_api_group_resources("admissionregistration.k8s.io/v1")
        .await
    {
        Ok(list) => list
            .resources
            .iter()
            .any(|r| r.name == "validatingadmissionpolicies"),
        Err(_) => false,
    }
}

/// The customer's switches; none when there's no ConfigMap.
pub async fn read_policy(client: &Client) -> Policy {
    match Api::<ConfigMap>::namespaced(client.clone(), rbac::NS)
        .get_opt(rbac::POLICY_CONFIGMAP)
        .await
    {
        Ok(Some(cm)) => Policy::from_data(&cm.data.unwrap_or_default()),
        _ => Policy::default(),
    }
}

/// What the agent may do, as the cluster answers.
pub async fn detect(client: &Client, policy: Policy) -> AgentScope {
    let full = can(client, "*", "*", "*", None).await;
    let mut installed_policy = String::new();
    let admission = {
        let vap = |kind: &str, plural: &str| {
            let ar = ApiResource::from_gvk_with_plural(
                &GroupVersionKind::gvk("admissionregistration.k8s.io", "v1", kind),
                plural,
            );
            Api::<DynamicObject>::all_with(client.clone(), &ar)
        };
        let policy = vap("ValidatingAdmissionPolicy", "validatingadmissionpolicies")
            .get_opt(rbac::ADMISSION_POLICY)
            .await;
        let binding = vap(
            "ValidatingAdmissionPolicyBinding",
            "validatingadmissionpolicybindings",
        )
        .get_opt(rbac::ADMISSION_POLICY)
        .await;
        match (policy, binding) {
            (Ok(Some(p)), Ok(Some(_))) => {
                installed_policy = p
                    .annotations()
                    .get(rbac::INSTALL_ANNOTATION)
                    .cloned()
                    .unwrap_or_default();
                "enforced"
            }
            _ if !admission_supported(client).await => "unsupported",
            _ => "missing",
        }
    };
    // The install it's from: the same stamp on the policy and on its Deployment.
    let install = match crate::install::deployment_stamp(client).await {
        Some(d) if !installed_policy.is_empty() && d == installed_policy => installed_policy,
        _ => String::new(),
    };
    let self_update = can_named(
        client,
        "update",
        "admissionregistration.k8s.io",
        "validatingadmissionpolicies",
        None,
        Some(rbac::ADMISSION_POLICY),
    )
    .await;
    let proxy = proxy_scope(client).await;
    AgentScope {
        proxy,
        access: if full { "full" } else { "limited" }.into(),
        admission: admission.into(),
        servers: can(client, "patch", "", "nodes", None).await,
        policy,
        install,
        self_update,
    }
}

/// The fields of `want` that `have` must match (rules, roleRef, subjects,
/// spec), allowing fields the server filled in (defaults).
pub fn same(want: &Value, have: &Value) -> bool {
    fn within(w: &Value, h: &Value) -> bool {
        match (w, h) {
            (Value::Object(w), Value::Object(h)) => w
                .iter()
                .all(|(k, v)| h.get(k).is_some_and(|x| within(v, x))),
            (Value::Array(w), Value::Array(h)) => {
                w.len() == h.len() && w.iter().zip(h).all(|(a, b)| within(a, b))
            }
            (w, h) => w == h,
        }
    }
    ["rules", "roleRef", "subjects", "spec"].iter().all(|k| {
        want.get(*k)
            .is_none_or(|w| have.get(*k).is_some_and(|h| within(w, h)))
    })
}

/// The policy's answer to a command: why it's refused, if it is.
pub fn refusal(policy: &Policy, command: &Command) -> Option<&'static str> {
    if policy.read_only
        && !matches!(
            command,
            Command::FetchLogs { .. } | Command::FetchDatabaseLogs { .. } | Command::Cancel { .. }
        )
    {
        return Some(
            "This cluster's policy (pyo-system/pyo-policy) is read only: Pyo can see it but not change it.",
        );
    }
    match command {
        Command::OpenTerminal { .. } if policy.no_terminal => {
            Some("This cluster's policy (pyo-system/pyo-policy) turns terminals off.")
        }
        Command::RemoveServer { force: true, .. } if policy.no_force_remove => Some(
            "This cluster's policy (pyo-system/pyo-policy) turns forced removal off: remove the server normally, so its services move first.",
        ),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn unlabelled_refusals_are_retried() {
        let denied = anyhow::anyhow!(
            "RoleBinding pyo-agent: ApiError: admission webhook: ValidatingAdmissionPolicy 'pyo-agent' with binding 'pyo-agent' denied request: Pyo's agent only changes namespaces labelled pyo.dev/managed=true; scale-test isn't one. (Invalid)"
        );
        assert!(unlabelled_refusal(&denied));
        let system = anyhow::anyhow!(
            "RoleBinding x: ValidatingAdmissionPolicy 'pyo-agent' denied request: In kube-system, Pyo's agent only writes its own objects, with exactly the content this release defines."
        );
        assert!(!unlabelled_refusal(&system));
        assert!(!unlabelled_refusal(&anyhow::anyhow!(
            "RoleBinding x: connection refused"
        )));
    }

    #[test]
    fn policy_refusals() {
        let logs = Command::Cancel {
            deployment_id: "dpl_x".into(),
        };
        let force = Command::RemoveServer {
            server: "a".into(),
            force: true,
        };
        let gentle = Command::RemoveServer {
            server: "a".into(),
            force: false,
        };
        let none = Policy::default();
        assert!(refusal(&none, &force).is_none());
        let p = Policy {
            no_force_remove: true,
            ..Default::default()
        };
        assert!(refusal(&p, &force).is_some() && refusal(&p, &gentle).is_none());
        let ro = Policy {
            read_only: true,
            ..Default::default()
        };
        assert!(refusal(&ro, &logs).is_none() && refusal(&ro, &gentle).is_some());
    }

    #[test]
    fn defaults_dont_count_as_changes() {
        let want = json!({ "spec": { "valuesContent": "a" }, "rules": [{ "verbs": ["get"] }] });
        assert!(same(
            &want,
            &json!({ "spec": { "valuesContent": "a", "failurePolicy": "reinstall" }, "rules": [{ "verbs": ["get"], "apiGroups": [""] }] })
        ));
        assert!(!same(
            &want,
            &json!({ "spec": { "valuesContent": "b" }, "rules": [{ "verbs": ["get"] }] })
        ));
        assert!(!same(
            &want,
            &json!({ "spec": { "valuesContent": "a" }, "rules": [] })
        ));
    }

    #[test]
    fn plurals() {
        for o in rbac::objects(true).iter().chain(
            rbac::admission("127.0.0.1:5005/pyo/waf:dev", "127.0.0.1:5005/pyo/geoip:dev").iter(),
        ) {
            let (ar, _, name) = resource_of(o);
            assert!(!name.is_empty());
            assert!(
                [
                    "namespaces",
                    "serviceaccounts",
                    "helmcharts",
                    "clusterroles",
                    "clusterrolebindings",
                    "roles",
                    "rolebindings",
                    "validatingadmissionpolicies",
                    "validatingadmissionpolicybindings"
                ]
                .contains(&ar.plural.as_str()),
                "{}",
                ar.plural
            );
        }
    }
}
