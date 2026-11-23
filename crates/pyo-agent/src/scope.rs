//! What the agent may do in its cluster (pyo-proto's rbac.rs), and the
//! customer's switches in `pyo-system/pyo-policy`.
//!
//! - Each namespace it writes in is labelled `pyo.dev/managed=true` and gets
//!   a RoleBinding to the write role (`ensure_managed`).
//! - Every heartbeat it finds out what it may do (`detect`) and reads the
//!   policy, which `refusal` applies to commands.
//! - Every heartbeat it also reads what the server's verifier says of its
//!   automatic updates (`pyo-system/pyo-verifier`) and passes it on. It
//!   never writes there, and nothing it does follows from it.

use std::sync::{
    Arc, RwLock,
    atomic::{AtomicBool, AtomicU8, Ordering},
};

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
    rbac::{self, AgentScope, Policy, VerifierStatus},
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
pub(crate) fn unlabelled_refusal(e: &anyhow::Error) -> bool {
    let m = format!("{e:#}");
    m.contains("ValidatingAdmissionPolicy")
        && m.contains("pyo.dev/managed=true")
        && m.contains("isn't one")
}

/// Run a write into a namespace Pyo just labelled managed, again while the
/// admission policy still sees it unlabelled (its namespace cache lags the
/// label for a moment), for up to 10 seconds.
pub async fn retry_unlabelled<T, F, Fut>(mut write: F) -> anyhow::Result<T>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<T>>,
{
    let mut wait = std::time::Duration::from_millis(100);
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        match write().await {
            Err(e) if unlabelled_refusal(&e) && tokio::time::Instant::now() < deadline => {
                tracing::info!(error = %format!("{e:#}"), "the namespace's label hasn't reached the admission policy yet; retrying");
                tokio::time::sleep(wait).await;
                wait = (wait * 2).min(std::time::Duration::from_secs(1));
            }
            r => return r,
        }
    }
}

/// A namespace the agent writes in: labelled, with the write role bound.
/// A new binding takes a moment to apply; this waits until it does.
pub async fn ensure_managed(client: &Client, ns: &str) -> anyhow::Result<()> {
    apply(client, &rbac::managed_namespace(ns)).await?;
    let binding = rbac::write_binding(ns);
    retry_unlabelled(|| apply(client, &binding)).await?;
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
    // Running proxies only, a page at a time, and of each only whether it
    // carries a token: a server short of disk evicts the proxy over and
    // over, and thousands of its Failed pods, whole, are more than the
    // agent's memory.
    let lp = ListParams::default()
        .labels("app.kubernetes.io/component=proxy,app.kubernetes.io/managed-by=envoy-gateway")
        .fields(crate::pods::LIVE);
    let tokens: Vec<bool> =
        crate::pods::list(&Api::<Pod>::namespaced(client.clone(), ns), &lp, |p| {
            Some(
                p.spec
                    .is_none_or(|s| s.automount_service_account_token != Some(false)),
            )
        })
        .await
        .ok()?;
    if tokens.is_empty() {
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
    Some(pyo_proto::rbac::ProxyScope {
        controller_reads_other_secrets: wide,
        proxies_have_credentials: tokens.contains(&true),
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

/// The verifier's status couldn't be read at the last heartbeat: said once,
/// then again only after a read went through.
static VERIFIER_UNREAD: AtomicBool = AtomicBool::new(false);
/// Whether the last heartbeat found a verifier: 0 not looked yet, 1 none,
/// 2 one. Said when it changes.
static VERIFIER_SEEN: AtomicU8 = AtomicU8::new(0);

/// What a read of the verifier's ConfigMap gave.
#[derive(Debug, PartialEq)]
enum VerifierRead {
    /// Its data.
    Found(std::collections::BTreeMap<String, String>),
    /// No ConfigMap: no verifier on this cluster.
    Missing,
    /// The install doesn't let the agent read it (one not refreshed yet).
    Refused,
    /// Anything else.
    Failed(String),
}

fn verifier_read(r: Result<Option<ConfigMap>, kube::Error>) -> VerifierRead {
    match r {
        Ok(Some(cm)) => VerifierRead::Found(cm.data.unwrap_or_default()),
        Ok(None) => VerifierRead::Missing,
        Err(kube::Error::Api(e)) if e.code == 403 => VerifierRead::Refused,
        Err(e) => VerifierRead::Failed(e.to_string()),
    }
}

/// The status a read gave (none unless one was found), and what to log of
/// it: each state is said when it starts, never at every heartbeat. A
/// refusal is expected on an install not refreshed yet, so it's said
/// quietly.
fn verifier_from(
    read: VerifierRead,
    unread: &AtomicBool,
    seen: &AtomicU8,
) -> (Option<VerifierStatus>, Option<(tracing::Level, String)>) {
    use crate::gateway::{first_time, settled};
    let mut said = None;
    let status = match read {
        VerifierRead::Found(data) => {
            settled(unread);
            let status = VerifierStatus::from_data(&data);
            if seen.swap(2, Ordering::Relaxed) != 2 {
                said = Some((
                    tracing::Level::INFO,
                    format!(
                        "the verifier on this cluster's server reports in (version {})",
                        status.version
                    ),
                ));
            }
            Some(status)
        }
        VerifierRead::Missing => {
            settled(unread);
            if seen.swap(1, Ordering::Relaxed) == 2 {
                said = Some((
                    tracing::Level::INFO,
                    "the verifier on this cluster's server no longer reports in".into(),
                ));
            }
            None
        }
        VerifierRead::Refused => {
            if first_time(unread) {
                said = Some((
                    tracing::Level::INFO,
                    "this install doesn't let the agent read the verifier's status yet: the cluster's connect command brings that".into(),
                ));
            }
            None
        }
        VerifierRead::Failed(e) => {
            if first_time(unread) {
                said = Some((
                    tracing::Level::WARN,
                    format!("couldn't read the verifier's status: {e}"),
                ));
            }
            None
        }
    };
    (status, said)
}

/// What the verifier on the cluster's first server last said of its
/// automatic updates: none when there's no ConfigMap (no verifier), or
/// when it can't be read. Passed on with the heartbeat, only to be shown.
pub async fn read_verifier(client: &Client) -> Option<VerifierStatus> {
    let read = verifier_read(
        Api::<ConfigMap>::namespaced(client.clone(), rbac::NS)
            .get_opt(rbac::VERIFIER_CONFIGMAP)
            .await,
    );
    let (status, said) = verifier_from(read, &VERIFIER_UNREAD, &VERIFIER_SEEN);
    match said {
        Some((tracing::Level::WARN, m)) => tracing::warn!("{m}"),
        Some((_, m)) => tracing::info!("{m}"),
        None => {}
    }
    status
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
    // The install it's from: the same stamp on the policy and on its
    // Deployment. And the roles a refresh left as they were, if any.
    let (install, roles_older) = match crate::install::deployment_stamp(client).await {
        Some(d) if !installed_policy.is_empty() && d == installed_policy => {
            rbac::parse_stamp(&installed_policy)
        }
        _ => (String::new(), vec![]),
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
        roles_older,
        self_update,
        headers_kept: crate::gateway::headers_kept(),
        verifier: read_verifier(client).await,
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
            Command::FetchLogs { .. }
                | Command::FollowLogs { .. }
                | Command::FollowDatabaseLogs { .. }
                | Command::FetchDatabaseLogs { .. }
                | Command::QueryTraffic(_)
                | Command::Cancel { .. }
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

    const UNLABELLED: &str = "Deployment pyo-db-controller: ApiError: admission webhook: ValidatingAdmissionPolicy 'pyo-agent' with binding 'pyo-agent' denied request: Pyo's agent only changes namespaces labelled pyo.dev/managed=true; pyo-databases isn't one. (Invalid)";

    #[tokio::test]
    async fn a_write_refused_while_the_label_lags_is_tried_again() {
        let tries = std::sync::atomic::AtomicU32::new(0);
        let r = retry_unlabelled(|| async {
            match tries.fetch_add(1, std::sync::atomic::Ordering::SeqCst) {
                0 => Err(anyhow::anyhow!(UNLABELLED)),
                n => Ok(n),
            }
        })
        .await;
        assert_eq!(r.unwrap(), 1);
    }

    #[tokio::test]
    async fn other_errors_are_not_retried() {
        let tries = std::sync::atomic::AtomicU32::new(0);
        let r: anyhow::Result<()> = retry_unlabelled(|| async {
            tries.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Err(anyhow::anyhow!("Deployment x: connection refused"))
        })
        .await;
        assert!(r.is_err());
        assert_eq!(tries.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    fn data(pairs: &[(&str, &str)]) -> std::collections::BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn api_error(code: u16, reason: &str) -> kube::Error {
        kube::Error::Api(Box::new(
            kube::core::Status::failure(&format!("configmaps \"pyo-verifier\" is {reason}"), reason)
                .with_code(code),
        ))
    }

    /// What the cluster answers for the verifier's ConfigMap: no ConfigMap
    /// is no verifier, and an install that refuses the read is told apart
    /// from any other failure.
    #[test]
    fn the_verifiers_configmap_is_read_as_the_cluster_answers() {
        assert_eq!(verifier_read(Ok(None)), VerifierRead::Missing);
        let cm = ConfigMap {
            data: Some(data(&[("version", "0.51.0")])),
            ..Default::default()
        };
        assert_eq!(
            verifier_read(Ok(Some(cm))),
            VerifierRead::Found(data(&[("version", "0.51.0")]))
        );
        // One without data: a verifier that said nothing yet.
        assert_eq!(
            verifier_read(Ok(Some(ConfigMap::default()))),
            VerifierRead::Found(Default::default())
        );
        assert_eq!(verifier_read(Err(api_error(403, "Forbidden"))), VerifierRead::Refused);
        assert!(matches!(
            verifier_read(Err(api_error(500, "InternalError"))),
            VerifierRead::Failed(_)
        ));
    }

    /// The status the heartbeat carries: only from a ConfigMap that's
    /// there, with missing and odd fields read as they are.
    #[test]
    fn the_verifiers_status_goes_into_the_scope_only_when_its_there() {
        let (unread, seen) = (AtomicBool::new(false), AtomicU8::new(0));
        let status = |read| verifier_from(read, &unread, &seen).0;
        assert_eq!(status(VerifierRead::Missing), None);
        assert_eq!(status(VerifierRead::Refused), None);
        assert_eq!(status(VerifierRead::Failed("timeout".into())), None);
        let full = status(VerifierRead::Found(data(&[
            ("version", "0.51.0"),
            ("live", "0.51.0"),
            ("checked", "2026-11-23T10:00:00Z"),
            ("error", ""),
            ("hold", "true"),
            ("host", "server-1"),
        ])))
        .unwrap();
        assert_eq!(full.version, "0.51.0");
        assert_eq!(full.checked, "2026-11-23T10:00:00Z");
        assert!(full.hold && full.error.is_empty());
        // Missing fields: empty. Odd ones: as they are, for Pyo to judge.
        assert_eq!(status(VerifierRead::Found(data(&[]))), Some(VerifierStatus::default()));
        let odd = status(VerifierRead::Found(data(&[("checked", "soon"), ("hold", "maybe")]))).unwrap();
        assert_eq!(odd.checked, "soon");
        assert!(!odd.hold);
    }

    /// Every heartbeat reads it: a refusal (an install not refreshed yet)
    /// or a failure is said once, not each time, and so is finding one.
    #[test]
    fn a_verifier_that_cant_be_read_is_said_once() {
        let (unread, seen) = (AtomicBool::new(false), AtomicU8::new(0));
        let said = |read| verifier_from(read, &unread, &seen).1;
        // Refused at every heartbeat: one quiet line.
        let first = said(VerifierRead::Refused).unwrap();
        assert_eq!(first.0, tracing::Level::INFO);
        assert!(first.1.contains("connect command"), "{}", first.1);
        for _ in 0..5 {
            assert_eq!(said(VerifierRead::Refused), None);
        }
        // No verifier at all: nothing to say, at any heartbeat.
        let (unread, seen) = (AtomicBool::new(false), AtomicU8::new(0));
        let said = |read| verifier_from(read, &unread, &seen).1;
        for _ in 0..3 {
            assert_eq!(said(VerifierRead::Missing), None);
        }
        // One appears: said once. A failure after: once, as a warning.
        assert_eq!(
            said(VerifierRead::Found(data(&[("version", "0.51.0")]))).unwrap().1,
            "the verifier on this cluster's server reports in (version 0.51.0)"
        );
        assert_eq!(said(VerifierRead::Found(data(&[]))), None);
        assert_eq!(said(VerifierRead::Failed("timeout".into())).unwrap().0, tracing::Level::WARN);
        assert_eq!(said(VerifierRead::Failed("timeout".into())), None);
        assert_eq!(said(VerifierRead::Refused), None);
        // Read again, then refused again: said again.
        assert_eq!(said(VerifierRead::Found(data(&[]))), None);
        assert!(said(VerifierRead::Refused).is_some());
        // And when it's gone.
        assert!(said(VerifierRead::Missing).is_some());
        assert_eq!(said(VerifierRead::Missing), None);
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
        let follow = Command::FollowLogs {
            session_id: "lgf_x".into(),
            service: pyo_proto::ServiceRef {
                service_id: "svc_x".into(),
                name: "web".into(),
                namespace: "pyo-p".into(),
                workload_kind: "Deployment".into(),
            },
            tail_lines: 100,
            since_seconds: None,
            replica: String::new(),
        };
        assert!(refusal(&ro, &follow).is_none(), "following logs only reads");
        let db_follow = Command::FollowDatabaseLogs {
            session_id: "lgf_y".into(),
            namespace: "pyo-db-team".into(),
            name: "main".into(),
            tail_lines: 500,
            since_seconds: None,
        };
        assert!(
            refusal(&ro, &db_follow).is_none(),
            "following a database's logs only reads"
        );
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
