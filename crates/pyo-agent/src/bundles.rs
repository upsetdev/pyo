//! System bundles (pyo-proto's bundles.rs): one installer for every bundle.
//!
//! Apply: its namespace, the agent's binding there (and wait until it
//! applies), then every object in the bundle's order, server-side, as
//! [`FIELD_MANAGER`] (the WAF's settings right after its ConfigMap, waf.rs);
//! then delete what an update dropped. Remove: every
//! object but the namespace, newest first; once they're gone (a HelmChart
//! takes its chart's uninstall), the namespace. Only objects labelled as the
//! bundle's are ever deleted, and an update never prunes a CRD (it takes
//! every object of its kind with it). The databases system is never
//! removed.
//!
//! Bindings in project namespaces (`managedNamespaceBindings`): applied in
//! every managed namespace but pyo-system on install, in each new one as
//! it's made (`bind_in`, from scope.rs's `ensure_managed`), and deleted on
//! removal. The bundle's namespace carries them, so a new namespace finds
//! them there.
//!
//! Each heartbeat reports the bundles found by their namespaces' labels,
//! with how many objects of their in-use kinds exist.

use std::time::Duration;

use k8s_openapi::api::core::v1::Namespace;
use kube::{
    Api, Client, ResourceExt,
    api::{DeleteParams, DynamicObject, ListParams, Patch, PatchParams},
    core::{ApiResource, GroupVersionKind},
};
use pyo_proto::{
    bundles::{
        BINDINGS_ANNOTATION, BUNDLE_LABEL, BundleReport, DATABASES_ID, FIELD_MANAGER,
        IN_USE_ANNOTATION, KindRef, NamespaceBinding, ObjectRef, POLICY_REFUSED, SystemBundle,
        VERSION_LABEL, WAF_ID, bindings_in, never_pruned,
    },
    rbac,
};
use serde_json::Value;

pub(crate) fn api_for(client: &Client, r: &ObjectRef) -> Api<DynamicObject> {
    let (group, version, plural) = r.resource();
    let ar =
        ApiResource::from_gvk_with_plural(&GroupVersionKind::gvk(group, version, &r.kind), &plural);
    match &r.namespace {
        Some(ns) => Api::namespaced_with(client.clone(), ns, &ar),
        None => Api::all_with(client.clone(), &ar),
    }
}

/// A Kubernetes API error on one of a bundle's objects. Its text (the API's
/// own) goes to the agent's log; the command reports [`plain`] words.
#[derive(Debug)]
struct ObjectError {
    what: String,
    source: kube::Error,
}

impl std::fmt::Display for ObjectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.what, self.source)
    }
}

impl std::error::Error for ObjectError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

fn object_error(r: &ObjectRef, source: kube::Error) -> anyhow::Error {
    anyhow::Error::new(ObjectError {
        what: format!("{} {}", r.kind, r.name),
        source,
    })
}

/// What a Kubernetes error means for a bundle command.
#[derive(Debug, PartialEq, Eq)]
enum Failure {
    /// The namespace is still being deleted (a removal just before):
    /// retried once it's gone.
    Terminating,
    /// The agent's admission policy refused it.
    Policy,
    Forbidden,
    Other,
    Unreachable,
}

fn classify(e: &kube::Error) -> Failure {
    let kube::Error::Api(s) = e else {
        return Failure::Unreachable;
    };
    let terminating = s.reason == "NamespaceTerminating"
        || s.details
            .as_ref()
            .is_some_and(|d| d.causes.iter().any(|c| c.reason == "NamespaceTerminating"))
        || s.message.contains("because it is being terminated");
    match s.code {
        _ if terminating => Failure::Terminating,
        _ if s.message.contains("ValidatingAdmissionPolicy") => Failure::Policy,
        403 => Failure::Forbidden,
        _ => Failure::Other,
    }
}

/// Whether an error is a namespace's deletion that hasn't finished.
fn is_terminating(e: &anyhow::Error) -> bool {
    e.chain()
        .filter_map(|c| c.downcast_ref::<kube::Error>())
        .any(|k| classify(k) == Failure::Terminating)
}

/// A bundle command's error in plain words; the API's text goes to the
/// agent's log. Errors that aren't Kubernetes' are already plain.
fn plain(bundle: &str, e: anyhow::Error) -> anyhow::Error {
    let object = e.chain().find_map(|c| c.downcast_ref::<ObjectError>());
    let Some(k) = object
        .map(|o| &o.source)
        .or_else(|| e.chain().find_map(|c| c.downcast_ref::<kube::Error>()))
    else {
        return e;
    };
    tracing::warn!(error = %format!("{e:#}"), bundle, "bundle command failed");
    let what = object.map_or_else(|| "an object".to_owned(), |o| o.what.clone());
    anyhow::anyhow!(match classify(k) {
        Failure::Terminating =>
            "The previous removal hasn't finished yet. Try again in a minute.".to_owned(),
        Failure::Policy =>
            format!("{what}: {POLICY_REFUSED}. Run the cluster's connect command again."),
        Failure::Forbidden => format!("The agent isn't allowed to change {what}."),
        Failure::Other =>
            format!("Kubernetes didn't accept {what}. The agent's log has the details."),
        Failure::Unreachable => "Couldn't reach the cluster's Kubernetes API.".to_owned(),
    })
}

/// Apply one object. A bundle's namespace is labelled managed just before
/// its objects go in; a refusal while the policy hasn't seen the label yet
/// is tried again.
async fn apply_one(client: &Client, o: &Value) -> anyhow::Result<()> {
    let r = ObjectRef::of(o);
    let api = api_for(client, &r);
    let params = PatchParams::apply(FIELD_MANAGER).force();
    crate::scope::retry_unlabelled(|| async {
        api.patch(&r.name, &params, &Patch::Apply(o))
            .await
            .map(drop)
            .map_err(|e| object_error(&r, e))
    })
    .await
}

/// Delete one of the bundle's objects; fine when it's gone, and skipped
/// when it isn't the bundle's.
async fn delete_one(client: &Client, id: &str, r: &ObjectRef) -> anyhow::Result<()> {
    let api = api_for(client, r);
    let Some(have) = api.get_opt(&r.name).await.map_err(|e| object_error(r, e))? else {
        return Ok(());
    };
    if have.labels().get(BUNDLE_LABEL).map(String::as_str) != Some(id) {
        tracing::warn!(kind = %r.kind, name = %r.name, bundle = id, "not deleting an object that isn't the bundle's");
        return Ok(());
    }
    match api.delete(&r.name, &DeleteParams::background()).await {
        Ok(_) => Ok(()),
        Err(kube::Error::Api(e)) if e.code == 404 => Ok(()),
        Err(e) => Err(object_error(r, e)),
    }
}

/// How long an install waits for a removal before it (its namespace's
/// deletion) to finish.
const TERMINATING_WAIT: Duration = Duration::from_secs(180);

/// Install or update a bundle. Its namespace still being deleted (turned
/// off and on again quickly) isn't a failure: it waits until it's gone.
pub async fn apply(
    client: &Client,
    bundle: &SystemBundle,
    prune: &[ObjectRef],
) -> anyhow::Result<String> {
    bundle.check().map_err(anyhow::Error::msg)?;
    let deadline = tokio::time::Instant::now() + TERMINATING_WAIT;
    loop {
        let attempt = async {
            namespace_gone_or_live(client, &bundle.namespace, deadline).await?;
            apply_once(client, bundle, prune).await
        };
        match attempt.await {
            Err(e) if is_terminating(&e) && tokio::time::Instant::now() < deadline => {
                tracing::info!(bundle = %bundle.id, namespace = %bundle.namespace, "waiting for the previous removal to finish");
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
            r => return r.map_err(|e| plain(&bundle.name, e)),
        }
    }
}

/// Wait while a namespace is being deleted (until `deadline`).
async fn namespace_gone_or_live(
    client: &Client,
    ns: &str,
    deadline: tokio::time::Instant,
) -> anyhow::Result<()> {
    let api = Api::<Namespace>::all(client.clone());
    let mut logged = false;
    loop {
        let Some(n) = api.get_opt(ns).await? else {
            return Ok(());
        };
        if n.metadata.deletion_timestamp.is_none() {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            // Reported as the namespace still terminating.
            return Err(kube::Error::Api(Box::new(
                kube::core::Status::failure(
                    &format!("namespace {ns} is still being terminated"),
                    "NamespaceTerminating",
                )
                .with_code(409),
            ))
            .into());
        }
        if !logged {
            tracing::info!(namespace = ns, "waiting for the previous removal to finish");
            logged = true;
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

async fn apply_once(
    client: &Client,
    bundle: &SystemBundle,
    prune: &[ObjectRef],
) -> anyhow::Result<String> {
    let objects = bundle.objects();
    apply_one(client, &objects[0]).await?;
    apply_one(client, &bundle.write_binding()).await?;
    crate::scope::wait_for_write(client, &bundle.namespace).await?;
    if bundle.needs_cert_manager() {
        cert_manager_ready(client, &bundle.namespace, &bundle.name).await?;
    }
    for o in &objects[1..] {
        apply_one(client, o).await?;
        // The WAF's settings as soon as its ConfigMap is there, before its
        // processor starts (a deploy or firewall sync that ran first had
        // nothing to write to yet).
        if bundle.id == WAF_ID
            && o["kind"] == "ConfigMap"
            && o["metadata"]["name"] == pyo_proto::system::WAF_NAME
            && let Err(e) = crate::waf::settings(client, true).await
        {
            tracing::warn!(error = %format!("{e:#}"), "couldn't write the WAF's settings while installing it");
        }
    }
    for o in objects
        .iter()
        .filter(|o| o["kind"] == "CustomResourceDefinition")
    {
        established(client, o["metadata"]["name"].as_str().unwrap_or("")).await;
    }
    for ns in project_namespaces(client).await? {
        for b in bundle.namespace_bindings(&ns) {
            apply_one(client, &b).await?;
        }
    }
    let keep = bundle.refs();
    let mut pruned = 0;
    for r in prune
        .iter()
        .rev()
        .filter(|r| !keep.contains(r) && !never_pruned(r))
    {
        delete_one(client, &bundle.id, r).await?;
        pruned += 1;
    }
    Ok(match pruned {
        0 => format!(
            "Applied {} {} ({} objects).",
            bundle.name,
            bundle.version,
            objects.len()
        ),
        n => format!(
            "Applied {} {} ({} objects) and removed {n} it no longer has.",
            bundle.name,
            bundle.version,
            objects.len()
        ),
    })
}

/// A CRD it applied is served (its kind can be written), within a few
/// seconds; what's written next (a Database) would otherwise find no kind.
async fn established(client: &Client, name: &str) {
    use k8s_openapi::apiextensions_apiserver::pkg::apis::apiextensions::v1::CustomResourceDefinition;
    let api = Api::<CustomResourceDefinition>::all(client.clone());
    for _ in 0..30 {
        let ok = api
            .get_opt(name)
            .await
            .ok()
            .flatten()
            .and_then(|c| c.status)
            .and_then(|s| s.conditions)
            .is_some_and(|cs| {
                cs.iter()
                    .any(|c| c.type_ == "Established" && c.status == "True")
            });
        if ok {
            return;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    tracing::warn!(crd = name, "not established yet");
}

/// cert-manager's API is there (Pyo installs it on every cluster, with
/// the gateway), else a plain reason instead of "no matches for kind".
async fn cert_manager_ready(client: &Client, ns: &str, name: &str) -> anyhow::Result<()> {
    let r = ObjectRef {
        api_version: format!("{}/v1", pyo_proto::bundles::CERT_MANAGER_GROUP),
        kind: "Certificate".into(),
        namespace: Some(ns.to_owned()),
        name: String::new(),
    };
    match api_for(client, &r)
        .list(&ListParams::default().limit(1))
        .await
    {
        Ok(_) => Ok(()),
        Err(kube::Error::Api(e)) if e.code == 404 => {
            anyhow::bail!(
                "{name} needs cert-manager, which Pyo installs with this cluster's gateway, and it isn't there yet. Once cert-manager runs (kubectl -n cert-manager get pods), install it again."
            )
        }
        Err(e) => Err(anyhow::anyhow!("Couldn't check for cert-manager: {e}")),
    }
}

/// Namespaces Pyo manages for projects (not its own).
async fn project_namespaces(client: &Client) -> anyhow::Result<Vec<String>> {
    let list = Api::<Namespace>::all(client.clone())
        .list(&ListParams::default().labels(&format!("{}=true", rbac::MANAGED_LABEL)))
        .await?;
    Ok(list
        .items
        .iter()
        .map(|n| n.name_any())
        .filter(|n| n != rbac::NS)
        .collect())
}

fn bindings_of(ns: &Namespace) -> Vec<NamespaceBinding> {
    ns.annotations()
        .get(BINDINGS_ANNOTATION)
        .and_then(|a| serde_json::from_str(a).ok())
        .unwrap_or_default()
}

/// A new project namespace: every installed bundle's bindings in it.
pub async fn bind_in(client: &Client, ns: &str) -> anyhow::Result<()> {
    if ns == rbac::NS {
        return Ok(());
    }
    for bundle_ns in Api::<Namespace>::all(client.clone())
        .list(&ListParams::default().labels(BUNDLE_LABEL))
        .await?
        .items
    {
        let Some(id) = bundle_ns.labels().get(BUNDLE_LABEL) else {
            continue;
        };
        for b in bindings_in(id, &bundle_ns.name_any(), &bindings_of(&bundle_ns), ns) {
            apply_one(client, &b).await?;
        }
    }
    Ok(())
}

/// Remove a bundle: its bindings in project namespaces, its objects, then
/// its namespace.
pub async fn remove(client: &Client, id: &str, objects: &[ObjectRef]) -> anyhow::Result<String> {
    if id == DATABASES_ID {
        anyhow::bail!("Databases are built into Pyo and can't be removed.");
    }
    remove_once(client, id, objects)
        .await
        .map_err(|e| plain(id, e))
}

async fn remove_once(client: &Client, id: &str, objects: &[ObjectRef]) -> anyhow::Result<String> {
    for r in objects.iter().filter(|r| r.kind == "Namespace") {
        let Some(bundle_ns) = Api::<Namespace>::all(client.clone())
            .get_opt(&r.name)
            .await?
        else {
            continue;
        };
        let bindings = bindings_of(&bundle_ns);
        for ns in project_namespaces(client).await? {
            for b in &bindings {
                let r = ObjectRef {
                    api_version: "rbac.authorization.k8s.io/v1".into(),
                    kind: "RoleBinding".into(),
                    namespace: Some(ns.clone()),
                    name: b.name.clone(),
                };
                delete_one(client, id, &r).await?;
            }
        }
    }
    let (namespaces, rest): (Vec<&ObjectRef>, Vec<&ObjectRef>) =
        objects.iter().partition(|r| r.kind == "Namespace");
    for r in rest.iter().rev() {
        delete_one(client, id, r).await?;
    }
    // Finalizers first (a chart's uninstall runs in its namespace).
    for _ in 0..60 {
        let mut left = 0;
        for r in &rest {
            if api_for(client, r)
                .get_opt(&r.name)
                .await?
                .is_some_and(|o| o.labels().get(BUNDLE_LABEL).map(String::as_str) == Some(id))
            {
                left += 1;
            }
        }
        if left == 0 {
            break;
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    for r in &namespaces {
        delete_one(client, id, r).await?;
    }
    // Done once the namespace is gone: an install right after it (turned
    // on again) would otherwise write into a namespace being deleted.
    let api = Api::<Namespace>::all(client.clone());
    for _ in 0..60 {
        let mut left = false;
        for r in &namespaces {
            left |= api
                .get_opt(&r.name)
                .await?
                .is_some_and(|n| n.labels().get(BUNDLE_LABEL).map(String::as_str) == Some(id));
        }
        if !left {
            break;
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    Ok(format!("Removed {id}."))
}

/// The bundles on this cluster, by their namespaces.
pub async fn report(client: &Client, namespaces: &[Namespace]) -> Vec<BundleReport> {
    let mut out = vec![];
    for ns in namespaces {
        let labels = ns.labels();
        let Some(id) = labels.get(BUNDLE_LABEL) else {
            continue;
        };
        let kinds: Vec<KindRef> = ns
            .annotations()
            .get(IN_USE_ANNOTATION)
            .and_then(|a| serde_json::from_str(a).ok())
            .unwrap_or_default();
        let mut in_use = 0;
        for k in &kinds {
            let r = ObjectRef {
                api_version: k.api_version.clone(),
                kind: k.kind.clone(),
                namespace: None,
                name: String::new(),
            };
            // Not there yet (or no longer): none.
            if let Ok(list) = api_for(client, &r).list(&ListParams::default()).await {
                in_use += list.items.len() as u32;
            }
        }
        out.push(BundleReport {
            id: id.clone(),
            version: labels.get(VERSION_LABEL).cloned().unwrap_or_default(),
            in_use,
        });
    }
    out
}

/// Namespaces of bundles: their workloads are Pyo's, not the customer's.
pub fn bundle_namespaces(namespaces: &[Namespace]) -> Vec<String> {
    namespaces
        .iter()
        .filter(|n| n.labels().contains_key(BUNDLE_LABEL))
        .map(|n| n.name_any())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use kube::core::{
        Status,
        response::{StatusCause, StatusDetails},
    };

    fn api(code: u16, reason: &str, message: &str) -> kube::Error {
        kube::Error::Api(Box::new(Status::failure(message, reason).with_code(code)))
    }

    /// The error turning the WAF on right after off gave.
    fn terminating() -> kube::Error {
        let details = StatusDetails {
            name: "pyo-waf".into(),
            kind: "configmaps".into(),
            causes: vec![StatusCause {
                reason: "NamespaceTerminating".into(),
                message: "namespace pyo-waf is being terminated".into(),
                field: "metadata.namespace".into(),
            }],
            group: String::new(),
            uid: String::new(),
            retry_after_seconds: 0,
        };
        kube::Error::Api(Box::new(
            Status::failure("configmaps \"pyo-waf\" is forbidden: unable to create new content in namespace pyo-waf because it is being terminated", "Forbidden").with_code(403).with_details(details),
        ))
    }

    /// The refusal the databases bundle got once on a server, right after
    /// its namespace was labelled: retried, not reported.
    #[test]
    fn a_refusal_before_the_label_reaches_the_policy_is_retried() {
        let r = ObjectRef {
            api_version: "rbac.authorization.k8s.io/v1".into(),
            kind: "RoleBinding".into(),
            namespace: Some("pyo-databases".into()),
            name: "pyo-agent".into(),
        };
        let e = object_error(
            &r,
            api(
                422,
                "Invalid",
                "admission webhook: ValidatingAdmissionPolicy 'pyo-agent' with binding 'pyo-agent' denied request: Pyo's agent only changes namespaces labelled pyo.dev/managed=true; pyo-databases isn't one.",
            ),
        );
        assert!(crate::scope::unlabelled_refusal(&e));
        let other = object_error(
            &r,
            api(
                422,
                "Invalid",
                "ValidatingAdmissionPolicy 'pyo-agent' denied request: In kube-system, Pyo's agent only writes its own objects, with exactly the content this release defines.",
            ),
        );
        assert!(!crate::scope::unlabelled_refusal(&other));
    }

    #[test]
    fn a_namespace_being_deleted_is_waited_for() {
        assert_eq!(classify(&terminating()), Failure::Terminating);
        // Without its details, by its message; and the wait's own timeout.
        assert_eq!(
            classify(&api(
                403,
                "Forbidden",
                "unable to create new content in namespace pyo-waf because it is being terminated"
            )),
            Failure::Terminating
        );
        assert_eq!(
            classify(&api(
                409,
                "NamespaceTerminating",
                "namespace pyo-waf is still being terminated"
            )),
            Failure::Terminating
        );
        let r = ObjectRef {
            api_version: "v1".into(),
            kind: "ConfigMap".into(),
            namespace: Some("pyo-waf".into()),
            name: "pyo-waf".into(),
        };
        assert!(is_terminating(&object_error(&r, terminating())));
        assert!(is_terminating(
            &anyhow::Error::from(terminating()).context("applying")
        ));
        assert!(!is_terminating(&object_error(
            &r,
            api(
                403,
                "Forbidden",
                "configmaps \"pyo-waf\" is forbidden: User cannot create"
            )
        )));
        assert!(!is_terminating(&anyhow::anyhow!(
            "the agent's role in pyo-waf didn't take effect"
        )));
    }

    /// Against a throwaway cluster (1.30+), as the agent's own
    /// ServiceAccount: the WAF turned off and on again right away installs
    /// once its namespace is gone, instead of failing on it.
    ///
    /// PYO_TEST_KUBECONFIG=<throwaway kubeconfig> cargo test -p pyo-agent -- --ignored off_and_on_against
    #[tokio::test]
    #[ignore = "needs a throwaway Kubernetes API server"]
    async fn off_and_on_against_a_cluster() {
        use k8s_openapi::api::{
            authentication::v1::TokenRequest,
            core::v1::{ConfigMap, ServiceAccount},
        };
        use kube::api::PostParams;
        let Ok(path) = std::env::var("PYO_TEST_KUBECONFIG") else {
            panic!("set PYO_TEST_KUBECONFIG to a throwaway cluster's kubeconfig")
        };
        let kc = kube::config::Kubeconfig::read_from(&path).unwrap();
        let config = kube::Config::from_custom_kubeconfig(kc, &Default::default())
            .await
            .unwrap();
        let admin = Client::try_from(config.clone()).unwrap();
        let db = pyo_proto::system::DbImages {
            controller: "c".into(),
            kv: "k".into(),
            postgres: ["p16".into(), "p17".into(), "p18".into()],
            mysql_operator: "mo".into(),
            mysql_backup: "mb".into(),
        };
        let (waf_image, geoip_image) =
            ("127.0.0.1:5005/pyo/waf:dev", "127.0.0.1:5005/pyo/geoip:dev");
        let install = rbac::InstallParts::new(false, true, waf_image, geoip_image, &db);
        let pp = PatchParams::apply("pyo-test").force();
        for o in install
            .access
            .iter()
            .chain(install.admission.iter())
            .filter(|o| o["kind"] != "HelmChart")
        {
            let r = ObjectRef::of(o);
            api_for(&admin, &r)
                .patch(&r.name, &pp, &Patch::Apply(o))
                .await
                .unwrap_or_else(|e| panic!("{}: {e}", r.name));
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
        let token = Api::<ServiceAccount>::namespaced(admin.clone(), rbac::NS)
            .create_token_request(
                rbac::SA,
                &PostParams::default(),
                &serde_json::from_value::<TokenRequest>(
                    serde_json::json!({ "spec": { "audiences": [], "expirationSeconds": 3600 } }),
                )
                .unwrap(),
            )
            .await
            .unwrap()
            .status
            .unwrap()
            .token
            .unwrap();
        let mut sa_config = config.clone();
        sa_config.auth_info = kube::config::AuthInfo {
            token: Some(token.into()),
            ..Default::default()
        };
        let sa = Client::try_from(sa_config).unwrap();
        let waf = pyo_proto::bundles::waf_bundle(waf_image, geoip_image);
        let namespaces = Api::<Namespace>::all(admin.clone());

        apply(&sa, &waf, &[]).await.expect("installed");
        // Off and on: the removal ends once its namespace is gone, so the
        // install after it (in line behind it) finds none.
        remove(&sa, &waf.id, &waf.refs()).await.expect("removed");
        assert!(
            namespaces.get_opt(&waf.namespace).await.unwrap().is_none(),
            "the removal ended before its namespace was gone"
        );
        apply(&sa, &waf, &[]).await.expect("installed again");

        // A namespace still being deleted (held by a finalizer for a few
        // seconds): the install waits for it, and succeeds.
        let held = serde_json::json!({ "apiVersion": "v1", "kind": "ConfigMap", "metadata": { "name": "held", "namespace": waf.namespace, "finalizers": ["pyo.dev/test"] } });
        let cms = Api::<ConfigMap>::namespaced(admin.clone(), &waf.namespace);
        cms.patch("held", &pp, &Patch::Apply(&held)).await.unwrap();
        namespaces
            .delete(&waf.namespace, &DeleteParams::background())
            .await
            .unwrap();
        assert!(
            namespaces
                .get(&waf.namespace)
                .await
                .unwrap()
                .metadata
                .deletion_timestamp
                .is_some()
        );
        let release = tokio::spawn({
            let cms = cms.clone();
            async move {
                tokio::time::sleep(Duration::from_secs(8)).await;
                cms.patch(
                    "held",
                    &PatchParams::default(),
                    &Patch::Merge(serde_json::json!({ "metadata": { "finalizers": null } })),
                )
                .await
                .unwrap();
            }
        });
        let started = tokio::time::Instant::now();
        let said = apply(&sa, &waf, &[])
            .await
            .expect("installed once the namespace was gone");
        assert!(
            started.elapsed() >= Duration::from_secs(7),
            "didn't wait: {said}"
        );
        release.await.unwrap();
        let ns = namespaces.get(&waf.namespace).await.unwrap();
        assert!(ns.metadata.deletion_timestamp.is_none());

        // And the classification on the real error: writing into a
        // namespace being deleted.
        cms.patch("held", &pp, &Patch::Apply(&held)).await.unwrap();
        namespaces
            .delete(&waf.namespace, &DeleteParams::background())
            .await
            .unwrap();
        let o = serde_json::json!({ "apiVersion": "v1", "kind": "ConfigMap", "metadata": { "name": "late", "namespace": waf.namespace } });
        let e = apply_one(&admin, &o)
            .await
            .expect_err("refused while terminating");
        assert!(is_terminating(&e), "{e:#}");
        assert_eq!(
            format!("{:#}", plain("waf", e)),
            "The previous removal hasn't finished yet. Try again in a minute."
        );
        cms.patch(
            "held",
            &PatchParams::default(),
            &Patch::Merge(serde_json::json!({ "metadata": { "finalizers": null } })),
        )
        .await
        .unwrap();
        while namespaces.get_opt(&waf.namespace).await.unwrap().is_some() {
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    }

    #[test]
    fn failures_are_plain() {
        let r = ObjectRef {
            api_version: "v1".into(),
            kind: "ConfigMap".into(),
            namespace: Some("pyo-waf".into()),
            name: "pyo-waf".into(),
        };
        let said = |e: anyhow::Error| format!("{:#}", plain("waf", e));
        let terminating = said(object_error(&r, terminating()));
        assert_eq!(
            terminating,
            "The previous removal hasn't finished yet. Try again in a minute."
        );
        assert_eq!(
            classify(&api(
                403,
                "Forbidden",
                "configmaps \"x\" is forbidden: ValidatingAdmissionPolicy 'pyo-agent' denied request"
            )),
            Failure::Policy
        );
        let policy = said(object_error(
            &r,
            api(
                403,
                "Forbidden",
                "ValidatingAdmissionPolicy 'pyo-agent' with binding 'pyo-agent' denied request",
            ),
        ));
        assert!(
            policy.starts_with("ConfigMap pyo-waf: ") && policy.contains(POLICY_REFUSED),
            "{policy}"
        );
        assert_eq!(
            said(object_error(&r, api(403, "Forbidden", "nope"))),
            "The agent isn't allowed to change ConfigMap pyo-waf."
        );
        assert_eq!(
            said(object_error(
                &r,
                api(422, "Invalid", "spec.x: Invalid value")
            )),
            "Kubernetes didn't accept ConfigMap pyo-waf. The agent's log has the details."
        );
        assert_eq!(
            said(anyhow::Error::from(api(500, "InternalError", "etcd"))),
            "Kubernetes didn't accept an object. The agent's log has the details."
        );
        // Ours are already plain.
        assert_eq!(
            said(anyhow::anyhow!(
                "the agent's role in pyo-waf didn't take effect"
            )),
            "the agent's role in pyo-waf didn't take effect"
        );
        for m in [terminating, policy] {
            assert!(!m.contains("ApiError") && !m.contains("Status {"), "{m}");
        }
    }
}
