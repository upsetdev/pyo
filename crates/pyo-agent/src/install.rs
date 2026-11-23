//! Bringing this cluster's install up to Pyo's (pyo-proto's rbac.rs,
//! `RefreshInstall`), with the optional self-update role. Pyo sends it when
//! the agent's heartbeat reports an older install (after Pyo was updated).
//!
//! 1. Its roles and bindings, as Pyo's are: the agent reads each one and
//!    updates those that differ, by name, the self-update role itself
//!    last (it's what allows the other writes). It never makes one. If
//!    one is missing or it may not update one (an install whose
//!    self-update role is older, a role object a release added, a chart
//!    installed with the agent that changed), only the cluster's connect
//!    command brings that, and nothing is changed.
//! 2. The admission policy is replaced as a whole (its binding only if that
//!    differs), then a dry run of the Deployment change below goes through
//!    it. If the new policy refuses that, the old one is put back, and
//!    the roles of step 1 with it.
//! 3. Last, after reporting: the Deployment's image pins, its stamp, and
//!    its pods' priority and tolerations (pyo-proto's `rbac::AGENT_PRIORITY`
//!    and `rbac::agent_tolerations`, the connect command's too). When more
//!    than the stamp changes, the agent restarts on it.
//!
//! Roles before the policy, the policy before the pins: the new policy only
//! narrows what the roles allow, so at every step in between the agent
//! holds a working install, and a refresh that stops there is made whole
//! by the next one.
//!
//! On a small server under load, the API server can take longer to check
//! the policy than it gives a write by default (34 s): each write here asks
//! for [`WRITE_TIMEOUT`] and one that times out (or finds the API server
//! busy) is tried again with a backoff, for [`BUDGET`] in all, before the
//! refresh fails. Each step is logged as it goes.
//!
//! Every step finds what's already done, so a refresh that stops halfway
//! is simply sent again. The agent's token, Pyo's address, its image and
//! everything else of its Deployment are never touched here, and the
//! admission policy refuses the agent any other change to it.

use std::time::{Duration, Instant};

use k8s_openapi::api::apps::v1::Deployment;
use kube::{
    Api, Client, ResourceExt,
    api::{DynamicObject, Patch, PatchParams, PostParams},
};
use pyo_proto::rbac::{self, InstallParts};
use serde_json::{Value, json};

use crate::scope::{api_for, same};

const DEPLOYMENT: &str = rbac::AGENT_DEPLOYMENT;
/// What a refresh can't do by itself.
const RERUN: &str = "run the cluster's connect command to bring them up to date";
/// What each write of the admission policy (and the dry run through it)
/// asks the API server for: more than its 34 s default for a write. It
/// caps this at its own request timeout (60 s unless set higher); the
/// agent waits a little longer than this for the answer.
const WRITE_TIMEOUT: Duration = Duration::from_secs(120);
/// How long a refresh keeps trying writes that time out before it fails.
const BUDGET: Duration = Duration::from_secs(300);
/// The first wait before trying again; it doubles up to [`MAX_BACKOFF`].
const FIRST_BACKOFF: Duration = Duration::from_secs(5);
const MAX_BACKOFF: Duration = Duration::from_secs(30);
/// How long the API server gets to start enforcing a policy just written.
const ENFORCE_WAIT: Duration = Duration::from_secs(if cfg!(test) { 0 } else { 3 });

/// A refresh's progress: logged as it goes, and sent back with its result.
#[derive(Default)]
pub struct Progress {
    pub lines: Vec<pyo_proto::LogLine>,
}

impl Progress {
    fn say(&mut self, message: String) {
        tracing::info!("{message}");
        self.lines.push(pyo_proto::LogLine {
            at: pyo_proto::now_ms(),
            level: "info".into(),
            source: "agent".into(),
            message,
        });
    }
}

/// Worth trying again: the API server ran out of time or was busy (it may
/// still have made the change), or the agent stopped waiting for it.
fn transient(e: &kube::Error) -> bool {
    match e {
        kube::Error::Api(s) => matches!(s.code, 429 | 500 | 502 | 503 | 504),
        kube::Error::HyperError(_) | kube::Error::Service(_) => true,
        _ => false,
    }
}

/// The wait before try `tries + 1`, if there's still time for it (`left`
/// of the budget).
fn backoff(tries: u32, left: Duration) -> Option<Duration> {
    let wait = FIRST_BACKOFF
        .saturating_mul(1 << tries.saturating_sub(1).min(8))
        .min(MAX_BACKOFF);
    (wait < left).then_some(wait)
}

/// `req` (a write, whose URL kube always ends with its query) asking the
/// API server for [`WRITE_TIMEOUT`].
fn with_timeout(mut req: http::Request<Vec<u8>>) -> Result<http::Request<Vec<u8>>, kube::Error> {
    let uri = req.uri().to_string();
    let sep = if uri.ends_with('?') { "" } else { "&" };
    *req.uri_mut() = format!("{uri}{sep}timeout={}s", WRITE_TIMEOUT.as_secs())
        .parse()
        .map_err(|e: http::uri::InvalidUri| kube::Error::Service(e.into()))?;
    Ok(req)
}

/// `req` with [`WRITE_TIMEOUT`] for the API server, sent; the agent waits a
/// little longer than that for its answer.
async fn send_with_timeout(
    client: &Client,
    req: http::Request<Vec<u8>>,
) -> Result<DynamicObject, kube::Error> {
    let req = with_timeout(req)?;
    match tokio::time::timeout(WRITE_TIMEOUT + Duration::from_secs(10), client.request(req)).await {
        Ok(r) => r,
        Err(_) => Err(kube::Error::Service(
            format!(
                "no answer from the API server in {}s",
                WRITE_TIMEOUT.as_secs() + 10
            )
            .into(),
        )),
    }
}

/// Replace `name` (in `api`'s resource) with `obj`, with [`WRITE_TIMEOUT`].
async fn replace(
    client: &Client,
    api: &Api<DynamicObject>,
    name: &str,
    obj: &DynamicObject,
) -> Result<DynamicObject, kube::Error> {
    let req = kube::core::Request::new(api.resource_url())
        .replace(
            name,
            &PostParams::default(),
            serde_json::to_vec(obj).map_err(kube::Error::SerdeError)?,
        )
        .map_err(kube::Error::BuildRequest)?;
    send_with_timeout(client, req).await
}

/// The install stamped on the agent's Deployment, if any.
pub async fn deployment_stamp(client: &Client) -> Option<String> {
    let d = Api::<Deployment>::namespaced(client.clone(), rbac::NS)
        .get_opt(DEPLOYMENT)
        .await
        .ok()
        .flatten()?;
    d.annotations().get(rbac::INSTALL_ANNOTATION).cloned()
}

/// What Pyo sent is a refresh of this install and nothing else: the
/// admission policy and its binding by name, still holding the agent (Deny,
/// failing closed, matching its account), the known pins, and a hash that
/// matches all of it.
pub fn check(p: &InstallParts) -> Result<(), String> {
    let [policy, binding] = p.admission.as_slice() else {
        return Err("expected the admission policy and its binding".into());
    };
    let named = |o: &Value, kind: &str| {
        o["kind"] == kind
            && o["metadata"]["name"] == rbac::ADMISSION_POLICY
            && o["metadata"]["namespace"].is_null()
    };
    if !named(policy, "ValidatingAdmissionPolicy")
        || !named(binding, "ValidatingAdmissionPolicyBinding")
    {
        return Err("expected the admission policy and its binding".into());
    }
    if policy["metadata"]["annotations"][rbac::INSTALL_ANNOTATION] != json!(p.hash) {
        return Err("the admission policy isn't stamped with this install".into());
    }
    let spec = &policy["spec"];
    // The agent, and the servers' tailnet pods it runs.
    let agent = json!([{ "name": "pyo-agent", "expression": format!("request.userInfo.username in ['{}', '{}']", rbac::AGENT_USER, rbac::TAILNET_USER) }]);
    if spec["failurePolicy"] != "Fail"
        || spec["matchConditions"] != agent
        || !spec["validations"]
            .as_array()
            .is_some_and(|v| !v.is_empty())
    {
        return Err("the admission policy wouldn't hold the agent".into());
    }
    if binding["spec"]
        != json!({ "policyName": rbac::ADMISSION_POLICY, "validationActions": ["Deny"] })
    {
        return Err("the admission policy's binding wouldn't deny".into());
    }
    let keys: Vec<&str> = p.pins.keys().map(String::as_str).collect();
    let mut want = rbac::PINNED_ENV.to_vec();
    want.sort_unstable();
    if keys != want {
        return Err("unexpected image pins".into());
    }
    if p.pins.values().any(|v| {
        v.is_empty()
            || v.chars()
                .any(|c| c.is_whitespace() || c == '"' || c == '\'')
    }) {
        return Err("an image pin isn't an image".into());
    }
    // The hash covers the policy as it was before stamping.
    let mut unstamped = p.admission.clone();
    if let Some(m) = unstamped[0]["metadata"].as_object_mut() {
        m.remove("annotations");
    }
    if rbac::install_hash(&p.access, &unstamped, &p.pins) != p.hash {
        return Err("its hash doesn't match its content".into());
    }
    // Its bindings: only to Pyo's own accounts (the admission policy says
    // the same of the ones the agent may update).
    for o in p.access.iter().filter(|o| is_binding(o)) {
        let own = o["subjects"].as_array().is_some_and(|s| {
            !s.is_empty()
                && s.iter().all(|s| {
                    s["kind"] == "ServiceAccount"
                        && s["namespace"] == rbac::NS
                        && [rbac::SA, rbac::TAILNET_SA].contains(&s["name"].as_str().unwrap_or(""))
                })
        });
        if !own {
            return Err("a binding isn't to the agent's own accounts".into());
        }
    }
    Ok(())
}

fn kind_of(o: &Value) -> &str {
    o["kind"].as_str().unwrap_or("")
}

fn is_binding(o: &Value) -> bool {
    matches!(kind_of(o), "RoleBinding" | "ClusterRoleBinding")
}

/// A role or binding: what a refresh may update.
fn is_role_object(o: &Value) -> bool {
    matches!(kind_of(o), "Role" | "ClusterRole") || is_binding(o)
}

/// A binding the cluster's owner may delete to take servers or their
/// figures away from the agent: missing, it stays missing.
fn optional_binding(o: &Value) -> bool {
    kind_of(o) == "ClusterRoleBinding"
        && [rbac::NODES_ROLE, rbac::STATS_ROLE].contains(&o["metadata"]["name"].as_str().unwrap_or(""))
}

/// An object of the install, as a refresh names it: "Kind/name", or
/// "Kind/namespace/name".
pub fn object_key(o: &Value) -> String {
    let name = o["metadata"]["name"].as_str().unwrap_or("");
    match o["metadata"]["namespace"].as_str() {
        Some(ns) => format!("{}/{ns}/{name}", kind_of(o)),
        None => format!("{}/{name}", kind_of(o)),
    }
}

/// The order roles are written in: roles, then bindings, then the
/// self-update role, which allows the writes before it.
fn write_rank(o: &Value) -> u8 {
    if kind_of(o) == "ClusterRole" && o["metadata"]["name"] == rbac::SELF_UPDATE_ROLE {
        2
    } else if is_binding(o) {
        1
    } else {
        0
    }
}

/// The Deployment change: its pins (env by name, the rest kept), stamp,
/// priority and tolerations (the whole list, as a strategic merge replaces
/// a list without a merge key).
fn deployment_patch(p: &InstallParts) -> Value {
    let env: Vec<Value> = p
        .pins
        .iter()
        .map(|(k, v)| json!({ "name": k, "value": v }))
        .collect();
    json!({
        "metadata": { "annotations": { rbac::INSTALL_ANNOTATION: p.hash } },
        "spec": { "template": { "spec": {
            "priorityClassName": rbac::AGENT_PRIORITY,
            "tolerations": rbac::agent_tolerations(),
            "containers": [{ "name": "agent", "env": env }],
        } } },
    })
}

/// Why a write of the install didn't happen.
enum WriteFailed {
    /// The object isn't there: the agent never makes one.
    Missing,
    /// The cluster refused it: this agent's roles or its admission policy.
    Refused(kube::Error),
    Other(anyhow::Error),
}

/// Replace `name` with `want` (its rules or spec, labels and annotations.
/// Other labels and annotations stay), trying again while the API server
/// is busy. Ok(Some(what was there before)) once written, Ok(None) when it
/// was already so.
async fn put(
    client: &Client,
    api: &Api<DynamicObject>,
    name: &str,
    want: &Value,
    progress: &mut Progress,
    deadline: Instant,
) -> Result<Option<DynamicObject>, WriteFailed> {
    let kind = kind_of(want);
    // What was there before the first write: put back if a later step
    // fails, even when that write's answer was lost.
    let mut before: Option<DynamicObject> = None;
    let mut tries = 0;
    loop {
        let old = match api.get_opt(name).await {
            Ok(Some(old)) => old,
            Ok(None) => return Err(WriteFailed::Missing),
            Err(e) if crate::refused::is_refusal(&e) => return Err(WriteFailed::Refused(e)),
            Err(e) => return Err(WriteFailed::Other(e.into())),
        };
        let old_value = serde_json::to_value(&old).map_err(|e| WriteFailed::Other(e.into()))?;
        let stamped = want["metadata"]["annotations"]
            .as_object()
            .is_none_or(|a| a.iter().all(|(k, v)| old.annotations().get(k).map(String::as_str) == v.as_str()));
        if same(want, &old_value) && stamped {
            // In place, maybe by an earlier try whose answer didn't come back.
            if before.is_some() {
                progress.say(format!("The {kind} {name} is in place."));
            }
            return Ok(before);
        }
        let mut new: DynamicObject =
            serde_json::from_value(want.clone()).map_err(|e| WriteFailed::Other(e.into()))?;
        new.metadata.resource_version = old.resource_version();
        // Other labels and annotations stay (kubectl's own, say).
        let mut labels = old.labels().clone();
        labels.extend(new.labels().clone());
        new.metadata.labels = Some(labels);
        let mut annotations = old.annotations().clone();
        annotations.extend(new.annotations().clone());
        new.metadata.annotations = Some(annotations);
        tries += 1;
        progress.say(if tries == 1 {
            format!("Updating the {kind} {name}…")
        } else {
            format!("Updating the {kind} {name} (try {tries})…")
        });
        if before.is_none() {
            before = Some(old);
        }
        match replace(client, api, name, &new).await {
            Ok(_) => return Ok(before),
            // Changed under it (another process of this agent, on its
            // way out, making the same refresh): read it again.
            Err(kube::Error::Api(e)) if e.code == 409 && tries < 3 => {
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
            Err(e) if crate::refused::is_refusal(&e) => return Err(WriteFailed::Refused(e)),
            Err(e) if transient(&e) => {
                let left = deadline.saturating_duration_since(Instant::now());
                let Some(wait) = backoff(tries, left) else {
                    return Err(WriteFailed::Other(anyhow::anyhow!(
                        "couldn't update {kind} {name} in {} minutes: {e}. The API server may be overloaded; this is tried again later",
                        BUDGET.as_secs() / 60
                    )));
                };
                progress.say(format!(
                    "The API server didn't take it ({e}). Trying again in {}s…",
                    wait.as_secs()
                ));
                tokio::time::sleep(wait).await;
            }
            Err(e) => {
                return Err(WriteFailed::Other(anyhow::anyhow!(
                    "couldn't update {kind} {name}: {e}"
                )));
            }
        }
    }
}

/// Put back what a refresh replaced, the last written first. What to say
/// when that failed too (empty when it didn't).
async fn put_back(client: &Client, replaced: Vec<(Api<DynamicObject>, DynamicObject)>) -> String {
    let mut failed = String::new();
    for (api, mut old) in replaced.into_iter().rev() {
        let name = old.name_any();
        old.metadata.managed_fields = None;
        for attempt in 1..=3 {
            old.metadata.resource_version = api
                .get(&name)
                .await
                .ok()
                .and_then(|cur| cur.resource_version());
            match replace(client, &api, &name, &old).await {
                Ok(_) => break,
                Err(e) => {
                    if attempt < 3
                        && (transient(&e) || matches!(&e, kube::Error::Api(s) if s.code == 409))
                    {
                        tokio::time::sleep(FIRST_BACKOFF).await;
                    } else {
                        failed = format!(" Putting back the old {name} failed too: {e}.");
                        break;
                    }
                }
            }
        }
    }
    failed
}

/// What step 1 did.
#[derive(Default)]
struct Roles {
    /// Updated: what each was before, to put back if a later step fails.
    written: Vec<(Api<DynamicObject>, DynamicObject)>,
    /// Not as Pyo's, and not this agent's to change ([`object_key`]s).
    older: Vec<String>,
}

/// Step 1: the install's roles, bindings and charts read, and the roles
/// and bindings that differ updated where the agent may.
async fn update_roles(
    client: &Client,
    p: &InstallParts,
    progress: &mut Progress,
    deadline: Instant,
) -> anyhow::Result<Roles> {
    let mut out = Roles::default();
    let mut differ: Vec<&Value> = vec![];
    for o in &p.access {
        let kind = kind_of(o);
        // Nothing in these to compare: there by now.
        if matches!(kind, "Namespace" | "ServiceAccount") {
            continue;
        }
        let (api, name) = api_for(client, o);
        let have = match api.get_opt(&name).await {
            Ok(h) => h,
            // It may not even read it: not this agent's to change.
            Err(e) if crate::refused::is_refusal(&e) => {
                out.older.push(object_key(o));
                continue;
            }
            Err(e) => anyhow::bail!("couldn't read its {kind} {name} ({e})"),
        };
        match have {
            None if optional_binding(o) => {}
            None => out.older.push(object_key(o)),
            Some(h) => {
                let h = serde_json::to_value(&h)?;
                if same(o, &h) {
                    continue;
                }
                // A chart, or a binding to another role (which Kubernetes
                // never changes): the connect command's.
                if is_role_object(o) && (!is_binding(o) || same(&json!({ "roleRef": o["roleRef"] }), &h)) {
                    differ.push(o);
                } else {
                    out.older.push(object_key(o));
                }
            }
        }
    }
    differ.sort_by_key(|o| write_rank(o));
    for o in differ {
        let (api, name) = api_for(client, o);
        match put(client, &api, &name, o, progress, deadline).await {
            Ok(Some(before)) => out.written.push((api, before)),
            Ok(None) => {}
            Err(WriteFailed::Missing) => out.older.push(object_key(o)),
            Err(WriteFailed::Refused(e)) => {
                progress.say(format!(
                    "This agent may not update its {} {name} ({e}).",
                    kind_of(o)
                ));
                out.older.push(object_key(o));
            }
            Err(WriteFailed::Other(e)) => {
                let back = put_back(client, std::mem::take(&mut out.written)).await;
                anyhow::bail!("{e:#}, so its roles stay as they were.{back}");
            }
        }
    }
    Ok(out)
}

/// Steps 1 and 2. Ok once the roles and the admission policy are this
/// install's and the policy lets the Deployment change through; `finish`
/// makes that change.
pub async fn refresh(
    client: &Client,
    p: &InstallParts,
    progress: &mut Progress,
) -> anyhow::Result<()> {
    check(p).map_err(|e| anyhow::anyhow!("Pyo sent an install this agent won't apply: {e}"))?;
    let deadline = Instant::now() + BUDGET;
    // 1. Its roles, as Pyo's are.
    let roles = update_roles(client, p, progress, deadline).await?;
    if !roles.older.is_empty() {
        let back = put_back(client, roles.written).await;
        anyhow::bail!(
            "{} ({} can't be brought up to date by the agent); {RERUN}.{back}",
            rbac::ROLES_OLDER,
            roles.older.join(", ")
        );
    }
    let mut replaced = roles.written;
    let wrote_roles = !replaced.is_empty();
    // 2. The admission policy, then its binding.
    let mut wrote_policy = false;
    for o in &p.admission {
        let (api, name) = api_for(client, o);
        let kind = kind_of(o);
        match put(client, &api, &name, o, progress, deadline).await {
            Ok(Some(before)) => {
                replaced.push((api, before));
                wrote_policy = true;
            }
            Ok(None) => {}
            Err(failed) => {
                let why = match failed {
                    WriteFailed::Missing => {
                        format!("this cluster has no admission policy for the agent; {RERUN}")
                    }
                    WriteFailed::Refused(e) => format!("couldn't update {kind} {name}: {e}"),
                    WriteFailed::Other(e) => format!("{e:#}"),
                };
                let back = put_back(client, replaced).await;
                anyhow::bail!("{why}{}{back}", if wrote_roles { ", so its roles stay as they were." } else { "" });
            }
        }
    }
    if wrote_policy {
        // The API server takes a moment to enforce it.
        tokio::time::sleep(ENFORCE_WAIT).await;
    }
    let deployments = Api::<Deployment>::namespaced(client.clone(), rbac::NS);
    progress.say("Checking the agent's own update against it…".into());
    let mut tries = 0;
    let probe = loop {
        tries += 1;
        let req = kube::core::Request::new(deployments.resource_url()).patch(
            DEPLOYMENT,
            &PatchParams {
                dry_run: true,
                ..Default::default()
            },
            &Patch::Strategic(deployment_patch(p)),
        );
        let sent = match req {
            Ok(req) => send_with_timeout(client, req).await.map(|_| ()),
            Err(e) => Err(kube::Error::BuildRequest(e)),
        };
        match sent {
            Err(e) if transient(&e) => {
                let left = deadline.saturating_duration_since(Instant::now());
                let Some(wait) = backoff(tries, left) else {
                    break Err(e);
                };
                progress.say(format!(
                    "The API server didn't answer the check ({e}). Trying again in {}s…",
                    wait.as_secs()
                ));
                tokio::time::sleep(wait).await;
            }
            other => break other,
        }
    };
    if let Err(e) = probe {
        let why = if transient(&e) {
            format!(
                "couldn't check the new admission policy against the agent's own update in {} minutes ({e})",
                BUDGET.as_secs() / 60
            )
        } else {
            format!("the new admission policy refused the agent's own update ({e})")
        };
        if !replaced.is_empty() {
            progress.say("Putting the old install back…".into());
        }
        let back = put_back(client, replaced).await;
        anyhow::bail!("{why}, so the old one stays.{back}");
    }
    progress.say("The new admission policy lets the agent's own update through.".into());
    Ok(())
}

/// Step 3: the Deployment's pins, stamp, priority and tolerations. When
/// more than the stamp changes, this agent restarts.
pub async fn finish(client: &Client, p: &InstallParts) -> anyhow::Result<()> {
    Api::<Deployment>::namespaced(client.clone(), rbac::NS)
        .patch(
            DEPLOYMENT,
            &PatchParams::default(),
            &Patch::Strategic(deployment_patch(p)),
        )
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parts() -> InstallParts {
        let db = pyo_proto::system::DbImages {
            controller: "c".into(),
            kv: "k".into(),
            postgres: ["p16".into(), "p17".into(), "p18".into()],
            mysql_operator: "mo".into(),
            mysql_backup: "mb".into(),
        };
        InstallParts::new(
            false,
            true,
            "127.0.0.1:5005/pyo/waf:dev",
            "127.0.0.1:5005/pyo/geoip:dev",
            &db,
        )
    }

    // ── A refresh against the in-memory API server ───────────────────────

    use crate::fakekube::FakeKube;
    use std::sync::{Arc, Mutex};

    /// Where the fake server keeps an object of the install.
    fn path_of(o: &Value) -> String {
        let version = o["apiVersion"].as_str().unwrap();
        let prefix = if version.contains('/') {
            format!("apis/{version}")
        } else {
            format!("api/{version}")
        };
        let plural = match kind_of(o) {
            "ValidatingAdmissionPolicy" => "validatingadmissionpolicies".to_owned(),
            k => format!("{}s", k.to_ascii_lowercase()),
        };
        let name = o["metadata"]["name"].as_str().unwrap();
        match o["metadata"]["namespace"].as_str() {
            Some(ns) => format!("{prefix}/namespaces/{ns}/{plural}/{name}"),
            None => format!("{prefix}/{plural}/{name}"),
        }
    }

    const DEPLOYMENT_PATH: &str = "apis/apps/v1/namespaces/pyo-system/deployments/pyo-agent";

    /// A cluster as the connect command left it at install `p`.
    fn installed(kube: &FakeKube, p: &InstallParts) {
        for o in p.access.iter().chain(p.admission.iter()) {
            kube.put(&path_of(o), o.clone());
        }
        kube.put(
            DEPLOYMENT_PATH,
            json!({
                "apiVersion": "apps/v1", "kind": "Deployment",
                "metadata": { "annotations": { rbac::INSTALL_ANNOTATION: p.hash } },
                "spec": { "template": { "spec": { "containers": [{ "name": "agent", "image": "agent:1" }] } } },
            }),
        );
    }

    /// `p` with its roles changed by `change`, as a later release of Pyo
    /// would hand it out (hashed and stamped again).
    fn later(p: &InstallParts, change: impl Fn(&mut Vec<Value>)) -> InstallParts {
        let mut p = p.clone();
        change(&mut p.access);
        p.admission[0]["metadata"]
            .as_object_mut()
            .unwrap()
            .remove("annotations");
        p.hash = rbac::install_hash(&p.access, &p.admission, &p.pins);
        p.admission[0]["metadata"]["annotations"] = json!({ rbac::INSTALL_ANNOTATION: p.hash });
        check(&p).unwrap();
        p
    }

    fn role<'a>(access: &'a mut [Value], kind: &str, name: &str) -> &'a mut Value {
        access
            .iter_mut()
            .find(|o| o["kind"] == kind && o["metadata"]["name"] == name)
            .unwrap()
    }

    fn stamp_at(kube: &FakeKube, path: &str) -> Value {
        kube.get(path).unwrap()["metadata"]["annotations"][rbac::INSTALL_ANNOTATION].clone()
    }

    /// What 0.52.0 could bring: a rule added to one role, one removed from
    /// another, and a name added to the self-update role.
    fn changed_roles(access: &mut Vec<Value>) {
        role(access, "ClusterRole", "pyo-agent-cluster")["rules"]
            .as_array_mut()
            .unwrap()
            .push(json!({ "apiGroups": ["example.pyo.dev"], "resources": ["widgets"], "verbs": ["get", "patch"] }));
        role(access, "Role", "pyo-agent-policy")["rules"] = json!([]);
        role(access, "ClusterRole", rbac::SELF_UPDATE_ROLE)["rules"][1]["resourceNames"]
            .as_array_mut()
            .unwrap()
            .push(json!("pyo-agent-later"));
    }

    /// The upgrade path's second half: a cluster whose owner ran the
    /// connect command once on 0.51.0 (so it has this release's
    /// self-update role). Pyo then moves to a release with other rules in
    /// roles the cluster already has. The refresh writes them, then the
    /// policy, then the pins, and stamps the install current. No command.
    #[tokio::test]
    async fn a_later_release_with_changed_roles_refreshes_by_itself() {
        let (kube, client) = FakeKube::start().await;
        let a = parts();
        installed(&kube, &a);
        let b = later(&a, changed_roles);
        assert_ne!(a.hash, b.hash);
        // Every write, in order.
        let writes: Arc<Mutex<Vec<String>>> = Arc::default();
        let seen = writes.clone();
        kube.admit(move |w| {
            seen.lock().unwrap().push(format!("{} {}/{}", w.method, w.plural, w.name));
            None
        });
        let mut progress = Progress::default();
        refresh(&client, &b, &mut progress).await.unwrap();
        // Roles first, the self-update role last of them, then the policy,
        // then the dry run of the agent's own update.
        assert_eq!(
            *writes.lock().unwrap(),
            [
                "PUT roles/pyo-agent-policy",
                "PUT clusterroles/pyo-agent-cluster",
                "PUT clusterroles/pyo-agent-self-update",
                "PUT validatingadmissionpolicies/pyo-agent",
                "PATCH deployments/pyo-agent",
            ]
        );
        for o in b.access.iter().filter(|o| is_role_object(o)) {
            assert!(same(o, &kube.get(&path_of(o)).unwrap()), "{}", object_key(o));
        }
        // The removed rule is gone, not merged.
        let policy_role = role(&mut b.access.clone(), "Role", "pyo-agent-policy").clone();
        assert_eq!(kube.get(&path_of(&policy_role)).unwrap()["rules"], json!([]));
        assert_eq!(stamp_at(&kube, &path_of(&b.admission[0])), json!(b.hash));
        // The Deployment last, after the result is reported.
        assert_eq!(stamp_at(&kube, DEPLOYMENT_PATH), json!(a.hash));
        finish(&client, &b).await.unwrap();
        assert_eq!(stamp_at(&kube, DEPLOYMENT_PATH), json!(b.hash));
        let d = kube.get(DEPLOYMENT_PATH).unwrap();
        assert_eq!(d["spec"]["template"]["spec"]["priorityClassName"], rbac::AGENT_PRIORITY);
        // Sent again: nothing left to write but the dry run.
        writes.lock().unwrap().clear();
        refresh(&client, &b, &mut Progress::default()).await.unwrap();
        assert_eq!(*writes.lock().unwrap(), ["PATCH deployments/pyo-agent"]);
    }

    /// A binding the owner deleted to take servers away stays deleted, and
    /// the refresh goes through without it.
    #[tokio::test]
    async fn a_refresh_leaves_a_deleted_optional_binding_alone() {
        let (kube, client) = FakeKube::start().await;
        let a = parts();
        let mut without = a.clone();
        without
            .access
            .retain(|o| !(o["kind"] == "ClusterRoleBinding" && o["metadata"]["name"] == rbac::NODES_ROLE));
        installed(&kube, &without);
        let b = later(&a, changed_roles);
        refresh(&client, &b, &mut Progress::default()).await.unwrap();
        let binding = b.access.iter().find(|o| optional_binding(o)).unwrap();
        assert!(kube.get(&path_of(binding)).is_none());
        assert_eq!(stamp_at(&kube, &path_of(&b.admission[0])), json!(b.hash));
    }

    /// A new policy that refuses the agent's own update: the old policy is
    /// put back, and the roles written before it too. The old install
    /// stays whole.
    #[tokio::test]
    async fn a_refused_policy_puts_the_roles_back_too() {
        let (kube, client) = FakeKube::start().await;
        let a = parts();
        installed(&kube, &a);
        let b = later(&a, changed_roles);
        kube.admit(|w| {
            (w.plural == "deployments").then(|| {
                (422, "ValidatingAdmissionPolicy 'pyo-agent' with binding 'pyo-agent' denied request".to_owned())
            })
        });
        let err = refresh(&client, &b, &mut Progress::default()).await.unwrap_err();
        assert!(format!("{err:#}").contains("refused the agent's own update"), "{err:#}");
        assert!(!format!("{err:#}").contains("failed too"), "{err:#}");
        for o in a.access.iter().filter(|o| is_role_object(o)).chain(a.admission.iter()) {
            assert!(same(o, &kube.get(&path_of(o)).unwrap()), "{}", object_key(o));
        }
        assert_eq!(stamp_at(&kube, &path_of(&a.admission[0])), json!(a.hash));
        assert_eq!(stamp_at(&kube, DEPLOYMENT_PATH), json!(a.hash));
    }

    /// Pyo only sends bindings to the agent's own accounts.
    #[test]
    fn refuses_a_binding_to_anyone_else() {
        let p = later(&parts(), |_| {});
        let mut other = p.clone();
        role(&mut other.access, "ClusterRoleBinding", "pyo-agent-read")["subjects"] =
            json!([{ "kind": "User", "name": "someone" }]);
        other.admission[0]["metadata"].as_object_mut().unwrap().remove("annotations");
        other.hash = rbac::install_hash(&other.access, &other.admission, &other.pins);
        other.admission[0]["metadata"]["annotations"] = json!({ rbac::INSTALL_ANNOTATION: other.hash });
        assert_eq!(check(&other).unwrap_err(), "a binding isn't to the agent's own accounts");
        assert!(check(&p).is_ok());
    }

    /// Writes ask the API server for more than its 34 s default.
    #[test]
    fn writes_ask_for_a_longer_timeout() {
        let r = kube::core::Request::new(
            "/apis/admissionregistration.k8s.io/v1/validatingadmissionpolicies",
        );
        let put = with_timeout(
            r.replace("pyo-agent", &PostParams::default(), vec![])
                .unwrap(),
        )
        .unwrap();
        assert_eq!(
            put.uri().to_string(),
            "/apis/admissionregistration.k8s.io/v1/validatingadmissionpolicies/pyo-agent?timeout=120s"
        );
        let r = kube::core::Request::new("/apis/apps/v1/namespaces/pyo-system/deployments");
        let dry = r
            .patch(
                DEPLOYMENT,
                &PatchParams {
                    dry_run: true,
                    ..Default::default()
                },
                &Patch::Strategic(json!({})),
            )
            .unwrap();
        let uri = with_timeout(dry).unwrap().uri().to_string();
        assert!(
            uri.starts_with("/apis/apps/v1/namespaces/pyo-system/deployments/pyo-agent?")
                && uri.contains("dryRun=All")
                && uri.ends_with("&timeout=120s")
                && uri.matches('?').count() == 1,
            "{uri}"
        );
    }

    /// Timeouts and a busy API server are tried again, with a backoff, for
    /// about five minutes; a refusal isn't.
    #[test]
    fn timeouts_are_tried_again_for_five_minutes() {
        let status = |code: u16| {
            kube::Error::Api(Box::new(
                serde_json::from_value(
                    json!({ "status": "Failure", "code": code, "message": "m", "reason": "r" }),
                )
                .unwrap(),
            ))
        };
        for code in [429, 500, 502, 503, 504] {
            assert!(transient(&status(code)), "{code}");
        }
        for code in [400, 403, 404, 409, 422] {
            assert!(!transient(&status(code)), "{code}");
        }
        assert!(transient(&kube::Error::Service("no answer".into())));
        let waits: Vec<u64> = (1..=6)
            .map(|t| backoff(t, BUDGET).unwrap().as_secs())
            .collect();
        assert_eq!(waits, [5, 10, 20, 30, 30, 30]);
        assert_eq!(backoff(1, Duration::from_secs(5)), None);
        assert_eq!(backoff(4, Duration::from_secs(31)), Some(MAX_BACKOFF));
        // Writes that time out at once: tries until the budget is spent.
        let (mut left, mut tries) = (BUDGET, 1);
        while let Some(w) = backoff(tries, left) {
            left -= w;
            tries += 1;
        }
        assert!((10..=14).contains(&tries), "{tries}");
        // A write that takes the API server's whole minute each time: fewer.
        let (mut left, mut tries) = (BUDGET, 1);
        loop {
            left = left.saturating_sub(Duration::from_secs(60));
            match backoff(tries, left) {
                Some(w) => {
                    left -= w;
                    tries += 1;
                }
                None => break,
            }
        }
        assert!((3..=5).contains(&tries), "{tries}");
    }

    /// The API server takes the timeout on a replace and a dry-run patch.
    ///
    /// PYO_TEST_KUBECONFIG=<throwaway kubeconfig> cargo test -p pyo-agent -- --ignored writes_with_a_timeout
    #[tokio::test]
    #[ignore = "needs a throwaway Kubernetes API server"]
    async fn writes_with_a_timeout_on_a_cluster() {
        use k8s_openapi::api::core::v1::ConfigMap;
        let Ok(path) = std::env::var("PYO_TEST_KUBECONFIG") else {
            panic!("set PYO_TEST_KUBECONFIG to a throwaway cluster's kubeconfig")
        };
        let kc = kube::config::Kubeconfig::read_from(&path).unwrap();
        let config = kube::Config::from_custom_kubeconfig(kc, &Default::default())
            .await
            .unwrap();
        let client = Client::try_from(config).unwrap();
        let maps = Api::<ConfigMap>::namespaced(client.clone(), "default");
        let _ = maps.delete("pyo-t-timeout", &Default::default()).await;
        maps.create(
            &PostParams::default(),
            &serde_json::from_value(
                json!({ "metadata": { "name": "pyo-t-timeout" }, "data": { "a": "1" } }),
            )
            .unwrap(),
        )
        .await
        .unwrap();
        let (api, name) = api_for(
            &client,
            &json!({ "apiVersion": "v1", "kind": "ConfigMap", "metadata": { "name": "pyo-t-timeout", "namespace": "default" } }),
        );
        let mut obj = api.get(&name).await.unwrap();
        obj.data["data"]["a"] = json!("2");
        let out = replace(&client, &api, &name, &obj).await.unwrap();
        assert_eq!(out.data["data"]["a"], "2");
        let req = kube::core::Request::new(api.resource_url())
            .patch(
                &name,
                &PatchParams {
                    dry_run: true,
                    ..Default::default()
                },
                &Patch::Strategic(json!({ "data": { "a": "3" } })),
            )
            .unwrap();
        let dry = send_with_timeout(&client, req).await.unwrap();
        assert_eq!(dry.data["data"]["a"], "3");
        assert_eq!(api.get(&name).await.unwrap().data["data"]["a"], "2");
        maps.delete("pyo-t-timeout", &Default::default())
            .await
            .unwrap();
    }

    #[test]
    fn takes_pyos_install_as_rendered() {
        assert_eq!(check(&parts()), Ok(()));
        // As it travels: a command, through JSON.
        let cmd = pyo_proto::Command::RefreshInstall(Box::new(parts()));
        let v = serde_json::to_value(&cmd).unwrap();
        assert_eq!(v["kind"], "refresh_install");
        let Ok(pyo_proto::Command::RefreshInstall(back)) =
            serde_json::from_value::<pyo_proto::Command>(v)
        else {
            panic!()
        };
        assert_eq!(check(&back), Ok(()));
    }

    #[test]
    fn refuses_anything_else() {
        let tamper = |f: &dyn Fn(&mut InstallParts)| {
            let mut p = parts();
            f(&mut p);
            check(&p)
        };
        // Loosening the policy, by its binding or by what it matches.
        assert!(
            tamper(&|p| p.admission[1]["spec"]["validationActions"] = json!(["Warn"])).is_err()
        );
        assert!(tamper(&|p| p.admission[0]["spec"]["failurePolicy"] = json!("Ignore")).is_err());
        assert!(
            tamper(&|p| p.admission[0]["spec"]["matchConditions"] =
                json!([{ "name": "pyo-agent", "expression": "false" }]))
            .is_err()
        );
        assert!(tamper(&|p| p.admission[0]["spec"]["validations"] = json!([])).is_err());
        // Another object, or another name.
        assert!(tamper(&|p| p.admission[0]["metadata"]["name"] = json!("other")).is_err());
        assert!(tamper(&|p| p.admission.push(json!({ "kind": "ClusterRoleBinding" }))).is_err());
        // Pins: only the known ones, as images.
        assert!(
            tamper(&|p| {
                p.pins.insert("PYO_API_URL".into(), "http://evil".into());
            })
            .is_err()
        );
        assert!(
            tamper(&|p| {
                p.pins.remove("PYO_WAF_IMAGE");
            })
            .is_err()
        );
        assert!(
            tamper(&|p| {
                p.pins.insert("PYO_WAF_IMAGE".into(), "x\" y".into());
            })
            .is_err()
        );
        // Content that doesn't match its hash.
        assert!(
            tamper(&|p| {
                p.pins.insert("PYO_WAF_IMAGE".into(), "other/waf:1".into());
            })
            .is_err()
        );
        assert!(
            tamper(&|p| p.admission[0]["spec"]["validations"][0]["expression"] = json!("true"))
                .is_err()
        );
        assert!(tamper(&|p| p.hash = "0000000000000000".into()).is_err());
    }

    #[test]
    fn the_deployment_change_is_its_pins_and_stamp() {
        let p = parts();
        let patch = deployment_patch(&p);
        assert_eq!(
            patch["metadata"]["annotations"][rbac::INSTALL_ANNOTATION],
            json!(p.hash)
        );
        let c = &patch["spec"]["template"]["spec"]["containers"][0];
        assert_eq!(c["name"], "agent");
        assert!(
            c.as_object()
                .unwrap()
                .keys()
                .all(|k| k == "name" || k == "env"),
            "no image, command or anything else"
        );
        assert_eq!(c["env"].as_array().unwrap().len(), rbac::PINNED_ENV.len());
        let spec = &patch["spec"]["template"]["spec"];
        let mut keys: Vec<&String> = spec.as_object().unwrap().keys().collect();
        keys.sort();
        assert_eq!(keys, ["containers", "priorityClassName", "tolerations"]);
        assert_eq!(spec["priorityClassName"], rbac::AGENT_PRIORITY);
        assert_eq!(spec["tolerations"], rbac::agent_tolerations());
    }

    /// A strategic merge patch as the API server makes it for these fields:
    /// maps merged, `containers` and `env` merged by name, other lists
    /// (tolerations) replaced.
    fn strategic_merge(target: &mut Value, patch: &Value) {
        let (Some(t), Some(p)) = (target.as_object_mut(), patch.as_object()) else {
            *target = patch.clone();
            return;
        };
        for (k, v) in p {
            match (t.get_mut(k), v) {
                (Some(Value::Array(have)), Value::Array(new))
                    if k == "containers" || k == "env" =>
                {
                    for item in new {
                        match have.iter_mut().find(|h| h["name"] == item["name"]) {
                            Some(h) => strategic_merge(h, item),
                            None => have.push(item.clone()),
                        }
                    }
                }
                (Some(have @ Value::Object(_)), Value::Object(_)) => strategic_merge(have, v),
                _ => {
                    t.insert(k.clone(), v.clone());
                }
            }
        }
    }

    /// An agent Deployment as an older connect command made it: no
    /// priority, the GPU toleration only, older pins.
    fn older_deployment() -> Value {
        json!({
            "apiVersion": "apps/v1", "kind": "Deployment",
            "metadata": { "name": DEPLOYMENT, "namespace": rbac::NS, "annotations": { rbac::INSTALL_ANNOTATION: "0123456789abcdef" } },
            "spec": { "replicas": 1, "template": { "metadata": { "annotations": { "pyo.dev/token": "t" } }, "spec": {
                "serviceAccountName": "pyo-agent",
                "tolerations": [{ "key": "nvidia.com/gpu", "operator": "Exists", "effect": "NoSchedule" }],
                "containers": [{ "name": "agent", "image": "agent@sha256:00", "envFrom": [{ "secretRef": { "name": "pyo-agent" } }],
                    "env": [{ "name": "RUST_LOG", "value": "pyo_agent=info" }, { "name": "PYO_WAF_IMAGE", "value": "waf:old" }] }],
            } } },
        })
    }

    /// A refresh brings an older Deployment the priority and tolerations
    /// (whole, the GPU one kept), and its pins and stamp; the token, Pyo's
    /// address and other env stay. Applied again, nothing changes.
    #[test]
    fn a_refresh_brings_the_priority_and_tolerations() {
        let p = parts();
        let patch = deployment_patch(&p);
        let mut d = older_deployment();
        strategic_merge(&mut d, &patch);
        let spec = &d["spec"]["template"]["spec"];
        assert_eq!(spec["priorityClassName"], rbac::AGENT_PRIORITY);
        assert_eq!(spec["tolerations"], rbac::agent_tolerations());
        assert_eq!(spec["serviceAccountName"], "pyo-agent");
        assert_eq!(
            d["metadata"]["annotations"][rbac::INSTALL_ANNOTATION],
            json!(p.hash)
        );
        assert_eq!(
            d["spec"]["template"]["metadata"]["annotations"]["pyo.dev/token"],
            "t"
        );
        let c = &spec["containers"][0];
        assert_eq!(c["image"], "agent@sha256:00");
        assert_eq!(
            c["envFrom"],
            json!([{ "secretRef": { "name": "pyo-agent" } }])
        );
        let env: std::collections::BTreeMap<&str, &str> = c["env"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| (e["name"].as_str().unwrap(), e["value"].as_str().unwrap()))
            .collect();
        assert_eq!(env["RUST_LOG"], "pyo_agent=info");
        for (k, v) in &p.pins {
            assert_eq!(env[k.as_str()], v, "{k}");
        }
        assert_eq!(env.len(), p.pins.len() + 1);
        // Sent again (a refresh that stopped halfway): the same Deployment.
        let once = d.clone();
        strategic_merge(&mut d, &patch);
        assert_eq!(d, once);
    }

    /// Against a throwaway cluster (1.30+), as the agent's own
    /// ServiceAccount: its admission policy holds what it runs to Pod
    /// Security's baseline, and it can't loosen a namespace's level.
    ///
    /// PYO_TEST_KUBECONFIG=<throwaway kubeconfig> cargo test -p pyo-agent -- --ignored pod_security_against
    #[tokio::test]
    #[ignore = "needs a throwaway Kubernetes API server"]
    async fn pod_security_against_a_cluster() {
        use k8s_openapi::api::{
            authentication::v1::TokenRequest,
            core::v1::{Namespace, Pod, ServiceAccount},
        };
        let Ok(path) = std::env::var("PYO_TEST_KUBECONFIG") else {
            panic!("set PYO_TEST_KUBECONFIG to a throwaway cluster's kubeconfig")
        };
        let kc = kube::config::Kubeconfig::read_from(&path).unwrap();
        let config = kube::Config::from_custom_kubeconfig(kc, &Default::default())
            .await
            .unwrap();
        let admin = Client::try_from(config.clone()).unwrap();
        let p = parts();
        let pp = PatchParams::apply("pyo-test").force();
        for o in p
            .access
            .iter()
            .chain(p.admission.iter())
            .filter(|o| o["kind"] != "HelmChart")
        {
            let (api, name) = api_for(&admin, o);
            api.patch(&name, &pp, &Patch::Apply(o))
                .await
                .unwrap_or_else(|e| panic!("{name}: {e}"));
        }
        // The policy has to compile: no type-checking warnings.
        let vaps = api_for(&admin, &p.admission[0]).0;
        for _ in 0..30 {
            let v = vaps.get(rbac::ADMISSION_POLICY).await.unwrap();
            if v.data["status"]["observedGeneration"].as_i64().is_some() {
                assert!(
                    v.data["status"]["typeChecking"]["expressionWarnings"]
                        .as_array()
                        .is_none_or(|w| w.is_empty()),
                    "{}",
                    v.data["status"]
                );
                break;
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
        let token = Api::<ServiceAccount>::namespaced(admin.clone(), rbac::NS)
            .create_token_request(
                rbac::SA,
                &PostParams::default(),
                &serde_json::from_value::<TokenRequest>(
                    json!({ "spec": { "audiences": [], "expirationSeconds": 3600 } }),
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

        // A project's namespace: baseline, as rollout.rs makes it.
        let namespaces = Api::<Namespace>::all(sa.clone());
        let project = json!({ "apiVersion": "v1", "kind": "Namespace", "metadata": { "name": "pyo-psa-test", "labels": {
            "app.kubernetes.io/managed-by": "pyo", "pyo/project-id": "prj_test", rbac::PSA_ENFORCE: "baseline" } } });
        let _ = Api::<Namespace>::all(admin.clone())
            .delete("pyo-psa-test", &Default::default())
            .await;
        while Api::<Namespace>::all(admin.clone())
            .get_opt("pyo-psa-test")
            .await
            .unwrap()
            .is_some()
        {
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        namespaces
            .patch(
                "pyo-psa-test",
                &PatchParams::apply("pyo").force(),
                &Patch::Apply(&project),
            )
            .await
            .unwrap();
        crate::scope::ensure_managed(&sa, "pyo-psa-test")
            .await
            .unwrap();
        // Not loosened, by anyone's patch through the agent.
        let loosen = namespaces
            .patch(
                "pyo-psa-test",
                &PatchParams::default(),
                &Patch::Merge(
                    json!({ "metadata": { "labels": { rbac::PSA_ENFORCE: "privileged" } } }),
                ),
            )
            .await;
        assert!(
            loosen.is_err_and(|e| e.to_string().contains("Pod Security")),
            "loosened"
        );
        let drop = namespaces
            .patch(
                "pyo-psa-test",
                &PatchParams::default(),
                &Patch::Merge(json!({ "metadata": { "labels": { rbac::PSA_ENFORCE: null } } })),
            )
            .await;
        assert!(drop.is_err(), "label dropped");
        namespaces
            .patch(
                "pyo-psa-test",
                &PatchParams::default(),
                &Patch::Merge(
                    json!({ "metadata": { "labels": { rbac::PSA_ENFORCE: "restricted" } } }),
                ),
            )
            .await
            .expect("stricter is fine");
        namespaces
            .patch(
                "pyo-psa-test",
                &PatchParams::default(),
                &Patch::Merge(
                    json!({ "metadata": { "labels": { rbac::PSA_ENFORCE: "baseline" } } }),
                ),
            )
            .await
            .expect_err("and stays");
        // A project namespace without a level, or a databases' one: refused; as managed_namespace makes it: fine.
        let bare = json!({ "apiVersion": "v1", "kind": "Namespace", "metadata": { "name": "pyo-psa-bare", "labels": { "pyo/project-id": "prj_x" } } });
        assert!(
            namespaces
                .patch(
                    "pyo-psa-bare",
                    &PatchParams::apply("pyo"),
                    &Patch::Apply(&bare)
                )
                .await
                .is_err()
        );
        let db_bare = json!({ "apiVersion": "v1", "kind": "Namespace", "metadata": { "name": "pyo-db-psatest", "labels": { rbac::MANAGED_LABEL: "true" } } });
        assert!(
            namespaces
                .patch(
                    "pyo-db-psatest",
                    &PatchParams {
                        dry_run: true,
                        ..PatchParams::apply("pyo")
                    },
                    &Patch::Apply(&db_bare)
                )
                .await
                .is_err()
        );
        namespaces
            .patch(
                "pyo-db-psatest",
                &PatchParams {
                    dry_run: true,
                    ..PatchParams::apply("pyo")
                },
                &Patch::Apply(&rbac::managed_namespace("pyo-db-psatest")),
            )
            .await
            .expect("databases' namespace as Pyo makes it");

        // In pyo-system, which has no Pod Security level of its own: the
        // agent's policy alone.
        let pods = Api::<Pod>::namespaced(sa.clone(), rbac::NS);
        let dry = PostParams {
            dry_run: true,
            ..Default::default()
        };
        let try_pod = |name: &str, spec: Value, annotations: Value| {
            let pods = pods.clone();
            let dry = dry.clone();
            let pod: Pod = serde_json::from_value(
                json!({ "metadata": { "name": name, "annotations": annotations }, "spec": spec }),
            )
            .unwrap();
            async move { pods.create(&dry, &pod).await }
        };
        let c = |sc: Value| json!({ "containers": [{ "name": "x", "image": "registry.k8s.io/pause:3.10", "securityContext": sc }] });
        let r = try_pod("fine", c(json!({ "allowPrivilegeEscalation": false, "capabilities": { "drop": ["ALL"] }, "seccompProfile": { "type": "RuntimeDefault" } })), json!({})).await;
        assert!(r.is_ok(), "a plain pod: {r:?}");
        let refused = |r: Result<Pod, kube::Error>, what: &str| {
            assert!(
                r.as_ref()
                    .is_err_and(|e| e.to_string().contains("Pyo's agent")),
                "{what}: {r:?}"
            )
        };
        refused(
            try_pod(
                "caps",
                c(json!({ "capabilities": { "add": ["SYS_ADMIN"] } })),
                json!({}),
            )
            .await,
            "capabilities.add",
        );
        refused(
            try_pod(
                "caps-net",
                c(json!({ "capabilities": { "add": ["NET_BIND_SERVICE"] } })),
                json!({}),
            )
            .await,
            "any added capability",
        );
        refused(
            try_pod(
                "seccomp",
                c(json!({ "seccompProfile": { "type": "Unconfined" } })),
                json!({}),
            )
            .await,
            "seccomp",
        );
        refused(
            try_pod(
                "apparmor",
                c(json!({ "appArmorProfile": { "type": "Unconfined" } })),
                json!({}),
            )
            .await,
            "AppArmor",
        );
        refused(
            try_pod(
                "apparmor-old",
                c(json!({})),
                json!({ "container.apparmor.security.beta.kubernetes.io/x": "unconfined" }),
            )
            .await,
            "AppArmor annotation",
        );
        refused(try_pod("podseccomp", json!({ "securityContext": { "seccompProfile": { "type": "Unconfined" } }, "containers": [{ "name": "x", "image": "registry.k8s.io/pause:3.10" }] }), json!({})).await, "pod seccomp");
        refused(try_pod("sysctl", json!({ "securityContext": { "sysctls": [{ "name": "kernel.msgmax", "value": "1" }] }, "containers": [{ "name": "x", "image": "registry.k8s.io/pause:3.10" }] }), json!({})).await, "sysctl");
        try_pod("safe-sysctl", json!({ "securityContext": { "sysctls": [{ "name": "net.ipv4.tcp_syncookies", "value": "1" }] }, "containers": [{ "name": "x", "image": "registry.k8s.io/pause:3.10" }] }), json!({})).await.expect("a safe sysctl");
        refused(try_pod("hostport", json!({ "containers": [{ "name": "x", "image": "registry.k8s.io/pause:3.10", "ports": [{ "containerPort": 80, "hostPort": 80 }] }] }), json!({})).await, "hostPort");
        // Pyo's own exceptions don't cover these either: a builder adding capabilities.
        let builder: Deployment = serde_json::from_value(json!({ "metadata": { "name": "pyo-builder-test" }, "spec": { "selector": { "matchLabels": { "a": "b" } }, "template": { "metadata": { "labels": { "a": "b" } },
            "spec": { "containers": [{ "name": "builder", "image": "registry.k8s.io/pause:3.10", "securityContext": { "capabilities": { "add": ["SYS_MODULE"] } } }] } } } })).unwrap();
        let r = Api::<Deployment>::namespaced(sa.clone(), rbac::NS)
            .create(&dry, &builder)
            .await;
        assert!(r.is_err_and(|e| e.to_string().contains("capabilities")));

        Api::<Namespace>::all(admin.clone())
            .delete("pyo-psa-test", &Default::default())
            .await
            .unwrap();
    }

    /// Against a throwaway cluster (1.30+), as the agent's own
    /// ServiceAccount with the self-update role: an install with older
    /// image pins is refreshed to Pyo's; one whose roles differ isn't.
    ///
    /// PYO_TEST_KUBECONFIG=<throwaway kubeconfig> cargo test -p pyo-agent -- --ignored refresh_against
    #[tokio::test]
    #[ignore = "needs a throwaway Kubernetes API server"]
    async fn refresh_against_a_cluster() {
        use k8s_openapi::api::{authentication::v1::TokenRequest, core::v1::ServiceAccount};
        let Ok(path) = std::env::var("PYO_TEST_KUBECONFIG") else {
            panic!("set PYO_TEST_KUBECONFIG to a throwaway cluster's kubeconfig")
        };
        let kc = kube::config::Kubeconfig::read_from(&path).unwrap();
        let config = kube::Config::from_custom_kubeconfig(kc, &Default::default())
            .await
            .unwrap();
        let admin = Client::try_from(config.clone()).unwrap();
        let db = pyo_proto::system::db_images_from_env();
        let new = InstallParts::new(
            false,
            true,
            &pyo_proto::system::waf_image_from_env(),
            &pyo_proto::system::geoip_image_from_env(),
            &db,
        );
        let old = InstallParts::new(
            false,
            true,
            "127.0.0.1:5005/pyo/waf:old",
            "127.0.0.1:5005/pyo/geoip:old",
            &db,
        );
        assert_ne!(old.hash, new.hash);
        let pp = PatchParams::apply("pyo-test").force();
        let apply = |o: Value| {
            let admin = admin.clone();
            async move {
                let (api, name) = api_for(&admin, &o);
                api.patch(&name, &pp_of(), &Patch::Apply(&o))
                    .await
                    .unwrap_or_else(|e| panic!("{name}: {e}"));
            }
        };
        fn pp_of() -> PatchParams {
            PatchParams::apply("pyo-test").force()
        }
        // What connect.sh would have installed: the old install, with the
        // self-update role bound, and a stand-in for the agent's Deployment.
        for o in old.access.iter().chain(old.admission.iter()) {
            apply(o.clone()).await;
        }
        let env: Vec<Value> = old
            .pins
            .iter()
            .map(|(k, v)| json!({ "name": k, "value": v }))
            .collect();
        let deployments = Api::<Deployment>::namespaced(admin.clone(), rbac::NS);
        let _ = deployments.delete(DEPLOYMENT, &Default::default()).await;
        tokio::time::sleep(Duration::from_secs(2)).await;
        let d = json!({
            "apiVersion": "apps/v1", "kind": "Deployment",
            "metadata": { "name": DEPLOYMENT, "namespace": rbac::NS, "annotations": { rbac::INSTALL_ANNOTATION: old.hash } },
            "spec": { "replicas": 0, "selector": { "matchLabels": { "app": "pyo-agent-test" } }, "template": { "metadata": { "labels": { "app": "pyo-agent-test" } }, "spec": {
                "serviceAccountName": "pyo-agent", "containers": [{ "name": "agent", "image": "registry.k8s.io/pause:3.10", "env": env }] } } },
        });
        deployments
            .patch(DEPLOYMENT, &pp, &Patch::Apply(&d))
            .await
            .unwrap();

        // From here on, only the agent's ServiceAccount.
        let token = Api::<ServiceAccount>::namespaced(admin.clone(), rbac::NS)
            .create_token_request(
                rbac::SA,
                &PostParams::default(),
                &serde_json::from_value::<TokenRequest>(
                    json!({ "spec": { "audiences": [], "expirationSeconds": 3600 } }),
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
        let detect = || crate::scope::detect(&sa, Default::default());
        let before = detect().await;
        assert_eq!(
            (
                before.admission.as_str(),
                before.install.as_str(),
                before.self_update
            ),
            ("enforced", old.hash.as_str(), true)
        );

        // The self-update role reaches only the agent's own policy.
        let vaps = api_for(&sa, &new.admission[0]).0;
        assert!(
            vaps.get("moco-delete-validator").await.is_err()
                || vaps
                    .replace(
                        "moco-delete-validator",
                        &PostParams::default(),
                        &vaps.get("moco-delete-validator").await.unwrap()
                    )
                    .await
                    .is_err()
        );

        // A role that differs: nothing changes, and it says to run the connect command.
        let roles = api_for(
            &admin,
            &new.access
                .iter()
                .find(|o| o["metadata"]["name"] == rbac::STATS_ROLE)
                .unwrap()
                .clone(),
        )
        .0;
        roles.patch(rbac::STATS_ROLE, &pp, &Patch::Apply(json!({ "apiVersion": "rbac.authorization.k8s.io/v1", "kind": "ClusterRole", "metadata": { "name": rbac::STATS_ROLE }, "rules": [] }))).await.unwrap();
        let err = refresh(&sa, &new, &mut Progress::default())
            .await
            .expect_err("its roles differ");
        assert!(format!("{err:#}").contains("connect command"), "{err:#}");
        assert_eq!(detect().await.install, old.hash, "nothing changed");
        apply(
            new.access
                .iter()
                .find(|o| o["metadata"]["name"] == rbac::STATS_ROLE)
                .unwrap()
                .clone(),
        )
        .await;

        // Its roles as Pyo's: the policy is refreshed, then the pins.
        let mut progress = Progress::default();
        refresh(&sa, &new, &mut progress).await.unwrap();
        let said: Vec<&str> = progress.lines.iter().map(|l| l.message.as_str()).collect();
        assert_eq!(
            said,
            [
                "Updating the ValidatingAdmissionPolicy…",
                "Checking the agent's own update against it…",
                "The new admission policy lets the agent's own update through.",
            ]
        );
        assert_eq!(
            vaps.get(rbac::ADMISSION_POLICY)
                .await
                .unwrap()
                .annotations()[rbac::INSTALL_ANNOTATION],
            new.hash
        );
        assert_eq!(
            detect().await.install,
            "",
            "the Deployment isn't stamped yet"
        );
        finish(&sa, &new).await.unwrap();
        let after = detect().await;
        assert_eq!(
            (after.admission.as_str(), after.install.as_str()),
            ("enforced", new.hash.as_str())
        );
        let d = deployments.get(DEPLOYMENT).await.unwrap();
        let c = &d.spec.unwrap().template.spec.unwrap().containers[0];
        assert_eq!(
            c.image.as_deref(),
            Some("registry.k8s.io/pause:3.10"),
            "its image stays"
        );
        let env: std::collections::BTreeMap<String, String> = c
            .env
            .clone()
            .unwrap()
            .into_iter()
            .map(|e| (e.name, e.value.unwrap_or_default()))
            .collect();
        assert_eq!(env, new.pins);
        // The stand-in had neither: the refresh brought both.
        let d = deployments.get(DEPLOYMENT).await.unwrap();
        let generation = d.metadata.generation;
        let pod = serde_json::to_value(d.spec.unwrap().template.spec.unwrap()).unwrap();
        assert_eq!(pod["priorityClassName"], rbac::AGENT_PRIORITY);
        assert_eq!(pod["tolerations"], rbac::agent_tolerations());
        // Sent again: nothing to do, and the Deployment is as it was.
        refresh(&sa, &new, &mut Progress::default()).await.unwrap();
        finish(&sa, &new).await.unwrap();
        assert_eq!(
            deployments
                .get(DEPLOYMENT)
                .await
                .unwrap()
                .metadata
                .generation,
            generation
        );

        // Its own Deployment, through the policy: only what a refresh or
        // an update of the agent changes, to exactly this release's.
        let own = Api::<Deployment>::namespaced(sa.clone(), rbac::NS);
        let try_patch = |patch: Value| {
            let own = own.clone();
            async move {
                own.patch(
                    DEPLOYMENT,
                    &PatchParams {
                        dry_run: true,
                        ..Default::default()
                    },
                    &Patch::Strategic(patch),
                )
                .await
            }
        };
        let refused = |r: Result<Deployment, kube::Error>, what: &str| {
            assert!(
                r.as_ref()
                    .is_err_and(|e| e.to_string().contains("own Deployment")),
                "{what}: {r:?}"
            )
        };
        let pod_patch = |spec: Value| json!({ "spec": { "template": { "spec": spec } } });
        try_patch(pod_patch(
            json!({ "containers": [{ "name": "agent", "image": "registry.k8s.io/pause:3.9" }] }),
        ))
        .await
        .expect("an update of the agent: its image");
        try_patch(deployment_patch(&new)).await.expect("a refresh");
        refused(
            try_patch(pod_patch(
                json!({ "priorityClassName": "system-node-critical" }),
            ))
            .await,
            "another priority class",
        );
        let mut extra = rbac::agent_tolerations();
        extra
            .as_array_mut()
            .unwrap()
            .push(json!({ "operator": "Exists" }));
        refused(
            try_patch(pod_patch(json!({ "tolerations": extra }))).await,
            "an extra toleration",
        );
        refused(
            try_patch(pod_patch(json!({ "tolerations": [{ "key": "node.kubernetes.io/disk-pressure", "operator": "Exists", "effect": "NoSchedule" }] }))).await,
            "other tolerations",
        );
        refused(
            try_patch(pod_patch(json!({ "containers": [{ "name": "agent", "env": [{ "name": "PYO_API_URL", "value": "https://elsewhere.example" }] }] }))).await,
            "Pyo's address",
        );
        refused(
            try_patch(pod_patch(json!({ "containers": [{ "name": "agent", "envFrom": [{ "secretRef": { "name": "other" } }] }] }))).await,
            "its token",
        );
        refused(
            try_patch(pod_patch(json!({ "serviceAccountName": "default" }))).await,
            "its service account",
        );
        refused(
            try_patch(pod_patch(
                json!({ "volumes": [{ "name": "x", "emptyDir": {} }] }),
            ))
            .await,
            "a volume",
        );
        refused(
            try_patch(json!({ "spec": { "template": { "metadata": { "annotations": { "pyo.dev/token": "other" } } } } })).await,
            "its token stamp",
        );
        refused(
            try_patch(json!({ "spec": { "replicas": 2 } })).await,
            "its replicas",
        );
        refused(
            own.delete(
                DEPLOYMENT,
                &kube::api::DeleteParams {
                    dry_run: true,
                    ..Default::default()
                },
            )
            .await
            .map(|_| Deployment::default()),
            "deleting it",
        );
        // Still enforced: a privileged pod is refused.
        let pod: k8s_openapi::api::core::v1::Pod = serde_json::from_value(json!({ "metadata": { "name": "pyo-test-privileged" }, "spec": { "containers": [{ "name": "x", "image": "registry.k8s.io/pause:3.10", "securityContext": { "privileged": true } }] } })).unwrap();
        let refused = Api::<k8s_openapi::api::core::v1::Pod>::namespaced(sa.clone(), rbac::NS)
            .create(
                &PostParams {
                    dry_run: true,
                    ..Default::default()
                },
                &pod,
            )
            .await;
        assert!(refused.is_err_and(|e| e.to_string().contains("privileged")));

        // The cluster's owner deletes the binding: the agent says it can't
        // refresh any more (Pyo then shows the connect command), and can't.
        let binding = rbac::self_update_objects()
            .into_iter()
            .find(|o| o["kind"] == "ClusterRoleBinding")
            .unwrap();
        let (api, name) = api_for(&admin, &binding);
        api.delete(&name, &Default::default()).await.unwrap();
        tokio::time::sleep(Duration::from_secs(2)).await;
        assert!(!detect().await.self_update);
        assert!(
            refresh(&sa, &old, &mut Progress::default()).await.is_err(),
            "without the binding it can't read its roles or write the policy"
        );

        // As it was: no stand-in, no self-update role.
        deployments
            .delete(DEPLOYMENT, &Default::default())
            .await
            .unwrap();
        for o in rbac::self_update_objects().iter().rev() {
            let (api, name) = api_for(&admin, o);
            let _ = api.delete(&name, &Default::default()).await;
        }
    }
}
