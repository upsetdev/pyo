//! The cluster's Cloudflare Tunnel connector (docs/cloudflare-tunnel.md §7):
//! cloudflared in pyo-system with the tunnel's token, pointed at Pyo
//! Gateway's HTTPS through `system::TUNNEL_ORIGIN_SERVICE`. Pyo's control
//! plane holds the Cloudflare account. The agent only ever gets the
//! tunnel's own token (`TunnelSet`), keeps it in a Secret labelled with the
//! tunnel's id, and follows what each heartbeat reply wants.
//!
//! The connectors' pod addresses are the only ones the tunnel listeners
//! trust to say who the visitor is (gateway.rs, the `tunnel` client
//! policies). They're looked at with each heartbeat and the policies follow.
//!
//! A cluster whose install is older than this agent refuses these objects:
//! what runs is kept, nothing more is written until the install changes,
//! and the heartbeat says so (`TunnelReport::refused`).

use std::sync::{
    Mutex,
    atomic::{AtomicBool, Ordering},
};

use k8s_openapi::api::{
    apps::v1::Deployment,
    core::v1::{Pod, Secret, Service},
    policy::v1::PodDisruptionBudget,
};
use kube::{
    Api, Client, ResourceExt,
    api::{DeleteParams, ListParams, Patch, PatchParams},
};
use pyo_proto::{CommandResult, TunnelPod, TunnelReport, TunnelWanted, rbac, system};

use crate::{MANAGER, failed};

/// Said when the cluster's install refuses the tunnel's objects.
pub const REFUSED: &str = "This cluster's install is older than Pyo's. Run the command under Refresh this cluster in the cluster's settings to use Cloudflare Tunnel.";

/// What the agent keeps between heartbeats.
#[derive(Default)]
struct Kept {
    /// The connectors' pod addresses the tunnel policies trust now.
    pods: Vec<String>,
    /// The connector's objects as last applied (their JSON).
    applied: String,
    /// The install stamp a write was refused on.
    refused_on: Option<String>,
}

static KEPT: Mutex<Kept> = Mutex::new(Kept {
    pods: vec![],
    applied: String::new(),
    refused_on: None,
});
static REFUSED_NOW: AtomicBool = AtomicBool::new(false);

fn kept() -> std::sync::MutexGuard<'static, Kept> {
    KEPT.lock().unwrap_or_else(|e| e.into_inner())
}

/// The connectors' pod addresses, as the tunnel policies trust them.
pub fn trusted_now() -> Vec<String> {
    kept().pods.clone()
}

/// The install refused a tunnel object: said once, and reported.
pub fn refused_now(e: &kube::Error) {
    if !REFUSED_NOW.swap(true, Ordering::Relaxed) {
        tracing::warn!(error = %e, "{REFUSED}");
    }
}

fn pp() -> PatchParams {
    PatchParams::apply(MANAGER).force()
}

/// Tunnel ids are UUIDs (they go into a label).
pub fn id_ok(id: &str) -> bool {
    id.len() == 36
        && id.bytes().enumerate().all(|(i, b)| match i {
            8 | 13 | 18 | 23 => b == b'-',
            _ => b.is_ascii_hexdigit(),
        })
}

fn secrets(client: &Client) -> Api<Secret> {
    Api::namespaced(client.clone(), rbac::NS)
}

/// The tunnel the token here is for, from its Secret's label.
async fn token_for(client: &Client) -> kube::Result<Option<String>> {
    Ok(here(client).await?.map(|(id, _)| id))
}

/// The tunnel the token here is for, and the token ("" when the Secret
/// has none).
async fn here(client: &Client) -> kube::Result<Option<(String, String)>> {
    Ok(secrets(client)
        .get_opt(system::TUNNEL_NAME)
        .await?
        .and_then(|s| {
            let id = s.labels().get(system::TUNNEL_ID_LABEL).cloned()?;
            let data = s
                .data
                .as_ref()
                .and_then(|d| d.get(system::TUNNEL_TOKEN_KEY))
                .map(|b| String::from_utf8_lossy(&b.0).into_owned());
            let token = data.or_else(|| {
                s.string_data
                    .as_ref()
                    .and_then(|d| d.get(system::TUNNEL_TOKEN_KEY))
                    .cloned()
            });
            Some((id, token.unwrap_or_default()))
        }))
}

/// The connector's objects, as written for `token`: its hash in the pod
/// template, so a new token rolls the connector out.
fn connector(token: &str) -> [serde_json::Value; 3] {
    [
        system::tunnel_deployment(system::CLOUDFLARED_IMAGE, &system::tunnel_token_hash(token)),
        system::tunnel_budget(),
        system::tunnel_origin_service(),
    ]
}

/// Write the connector's objects (only when they changed since the last
/// write here, or are missing).
async fn apply_connector(client: &Client, token: &str, force: bool) -> kube::Result<()> {
    let objects = connector(token);
    let text = serde_json::to_string(&objects).unwrap_or_default();
    let deployments: Api<Deployment> = Api::namespaced(client.clone(), rbac::NS);
    let there = deployments.get_opt(system::TUNNEL_NAME).await?.is_some();
    if !force && there && kept().applied == text {
        return Ok(());
    }
    let [d, b, s] = &objects;
    deployments
        .patch(system::TUNNEL_NAME, &pp(), &Patch::Apply(d))
        .await?;
    Api::<PodDisruptionBudget>::namespaced(client.clone(), rbac::NS)
        .patch(system::TUNNEL_NAME, &pp(), &Patch::Apply(b))
        .await?;
    Api::<Service>::namespaced(client.clone(), system::GATEWAY_NS)
        .patch(system::TUNNEL_ORIGIN_SERVICE, &pp(), &Patch::Apply(s))
        .await?;
    kept().applied = text;
    Ok(())
}

/// A refused write: kept until the install changes.
async fn refused(client: &Client, e: &kube::Error) {
    refused_now(e);
    kept().refused_on = Some(crate::refused::install_stamp(client).await);
}

/// `TunnelSet`: keep the token and run the connector with it.
pub async fn set(client: &Client, tunnel_id: &str, token: &str) -> CommandResult {
    if !id_ok(tunnel_id) {
        return failed("That isn't a tunnel id.");
    }
    if token.is_empty() || token.len() > 4096 || !token.bytes().all(|b| b.is_ascii_graphic()) {
        return failed("That isn't a tunnel token.");
    }
    let written = async {
        secrets(client)
            .patch(
                system::TUNNEL_NAME,
                &pp(),
                &Patch::Apply(system::tunnel_secret(tunnel_id, token)),
            )
            .await?;
        apply_connector(client, token, true).await
    }
    .await;
    match written {
        Ok(()) => {
            REFUSED_NOW.store(false, Ordering::Relaxed);
            kept().refused_on = None;
            CommandResult {
                data: serde_json::to_value(report(client).await).ok(),
                ..crate::ok("The tunnel's connector is starting.")
            }
        }
        Err(e) if crate::refused::is_refusal(&e) => {
            refused(client, &e).await;
            failed(REFUSED)
        }
        Err(e) => failed(format!("Couldn't start the tunnel's connector: {e}")),
    }
}

/// `TunnelRemove`: stop the connector, delete its token and its Service.
pub async fn remove(client: &Client) -> CommandResult {
    match remove_all(client).await {
        Ok(()) => crate::ok("The tunnel's connector is removed."),
        Err(e) => failed(format!("Couldn't remove the tunnel's connector: {e}")),
    }
}

async fn remove_all(client: &Client) -> kube::Result<()> {
    let gone = |r: kube::Result<()>| match r {
        Err(kube::Error::Api(e)) if e.code == 404 => Ok(()),
        other => other,
    };
    let dp = DeleteParams::default();
    gone(Api::<Deployment>::namespaced(client.clone(), rbac::NS)
        .delete(system::TUNNEL_NAME, &dp)
        .await
        .map(|_| ()))?;
    gone(Api::<PodDisruptionBudget>::namespaced(client.clone(), rbac::NS)
        .delete(system::TUNNEL_NAME, &dp)
        .await
        .map(|_| ()))?;
    gone(secrets(client).delete(system::TUNNEL_NAME, &dp).await.map(|_| ()))?;
    gone(Api::<Service>::namespaced(client.clone(), system::GATEWAY_NS)
        .delete(system::TUNNEL_ORIGIN_SERVICE, &dp)
        .await
        .map(|_| ()))?;
    kept().applied.clear();
    Ok(())
}

/// What a heartbeat reply wants: the connector for this tunnel (when the
/// token here is for it), or none. Then the tunnel policies follow the
/// connectors' pods.
pub async fn follow(client: &Client, wanted: &TunnelWanted) {
    // Refused before: nothing until the install changes.
    let waiting = kept().refused_on.clone();
    if let Some(on) = waiting {
        if crate::refused::install_stamp(client).await == on {
            return;
        }
        kept().refused_on = None;
        REFUSED_NOW.store(false, Ordering::Relaxed);
    }
    let have = match here(client).await {
        Ok(h) => h,
        Err(e) => {
            tracing::warn!(error = %e, "couldn't read the tunnel's token");
            return;
        }
    };
    let done = match (&wanted.tunnel_id, &have) {
        (None, None) => Ok(()),
        (None, Some(_)) => remove_all(client).await,
        // The token for it is here: the connector runs with it.
        (Some(w), Some((h, token))) if w == h && !token.is_empty() => {
            apply_connector(client, token, false).await
        }
        // Waiting for `TunnelSet` (the report says which token is here).
        (Some(_), _) => Ok(()),
    };
    if let Err(e) = done {
        if crate::refused::is_refusal(&e) {
            refused(client, &e).await;
        } else {
            tracing::warn!(error = %e, "couldn't apply the tunnel's connector");
        }
        return;
    }
    if let Err(e) = follow_pods(client).await {
        tracing::warn!(error = %format!("{e:#}"), "couldn't update the tunnel's trusted addresses");
    }
}

fn pods(client: &Client) -> Api<Pod> {
    Api::namespaced(client.clone(), rbac::NS)
}

fn selector() -> ListParams {
    ListParams::default().labels(&format!("app.kubernetes.io/name={}", system::TUNNEL_APP))
}

/// Every address of the connectors' pods, ready or not, as /32 and /128.
pub fn pod_cidrs(list: &[Pod]) -> Vec<String> {
    let ips: Vec<String> = list
        .iter()
        .filter_map(|p| p.status.as_ref())
        .flat_map(|s| {
            s.pod_ips
                .iter()
                .flatten()
                .map(|ip| ip.ip.clone())
                .chain(s.pod_ip.clone())
        })
        .collect();
    system::host_cidrs(&ips)
}

/// The tunnel policies trust the connectors' pods as they are now.
async fn follow_pods(client: &Client) -> anyhow::Result<()> {
    let now = pod_cidrs(&pods(client).list(&selector()).await?.items);
    let old = kept().pods.clone();
    if now == old {
        return Ok(());
    }
    crate::gateway::set_tunnel_pods(client, &old, &now).await?;
    tracing::info!(pods = now.len(), "the tunnel's trusted addresses changed");
    kept().pods = now;
    Ok(())
}

/// Why a pod isn't ready, at most 200 characters: a container's waiting
/// or last terminated reason.
fn not_ready_reason(p: &Pod) -> Option<String> {
    let statuses = p.status.as_ref()?.container_statuses.as_ref()?;
    statuses.iter().find_map(|c| {
        let st = c.state.as_ref()?;
        let r = st
            .waiting
            .as_ref()
            .and_then(|w| w.reason.clone())
            .or_else(|| st.terminated.as_ref().and_then(|t| t.reason.clone()))
            .or_else(|| {
                c.last_state
                    .as_ref()
                    .and_then(|l| l.terminated.as_ref())
                    .and_then(|t| t.reason.clone())
            })?;
        Some(r.chars().take(200).collect())
    })
}

/// Each connector pod as reported.
pub fn pod_reports(list: &[Pod]) -> Vec<TunnelPod> {
    let mut out: Vec<TunnelPod> = list
        .iter()
        .map(|p| {
            let ready = p
                .status
                .as_ref()
                .and_then(|s| s.conditions.as_ref())
                .is_some_and(|cs| cs.iter().any(|c| c.type_ == "Ready" && c.status == "True"));
            TunnelPod {
                server: p
                    .spec
                    .as_ref()
                    .and_then(|s| s.node_name.clone())
                    .unwrap_or_default(),
                ready,
                restarts: p
                    .status
                    .as_ref()
                    .and_then(|s| s.container_statuses.as_ref())
                    .map(|cs| cs.iter().map(|c| c.restart_count).sum())
                    .unwrap_or(0),
                reason: if ready { None } else { not_ready_reason(p) },
            }
        })
        .collect();
    out.sort_by(|a, b| a.server.cmp(&b.server));
    out
}

/// The connector as it is, for the heartbeat.
pub async fn report(client: &Client) -> TunnelReport {
    let tunnel_id = token_for(client).await.ok().flatten();
    let deployment = Api::<Deployment>::namespaced(client.clone(), rbac::NS)
        .get_opt(system::TUNNEL_NAME)
        .await
        .ok()
        .flatten();
    let (replicas, ready_replicas) = deployment
        .as_ref()
        .and_then(|d| d.status.as_ref())
        .map(|s| (s.replicas.unwrap_or(0), s.ready_replicas.unwrap_or(0)))
        .unwrap_or((0, 0));
    let list = if deployment.is_some() {
        pods(client)
            .list(&selector())
            .await
            .map(|l| l.items)
            .unwrap_or_default()
    } else {
        vec![]
    };
    let origin_ready = Api::<Service>::namespaced(client.clone(), system::GATEWAY_NS)
        .get_opt(system::TUNNEL_ORIGIN_SERVICE)
        .await
        .ok()
        .flatten()
        .is_some();
    TunnelReport {
        tunnel_id,
        replicas,
        ready_replicas,
        pods: pod_reports(&list),
        origin_ready,
        refused: REFUSED_NOW.load(Ordering::Relaxed),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fakekube::FakeKube;
    use serde_json::json;

    const ID: &str = "c1744f8b-faa1-48a4-9e5c-02ac921467fa";
    const SECRET: &str = "api/v1/namespaces/pyo-system/secrets/pyo-tunnel";
    const DEPLOYMENT: &str = "apis/apps/v1/namespaces/pyo-system/deployments/pyo-tunnel";
    const BUDGET: &str = "apis/policy/v1/namespaces/pyo-system/poddisruptionbudgets/pyo-tunnel";
    const ORIGIN: &str = "api/v1/namespaces/pyo-gateway/services/pyo-tunnel-origin";

    /// The tests share the agent's kept state: one at a time.
    static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    fn reset() {
        let mut k = kept();
        *k = Kept::default();
        REFUSED_NOW.store(false, Ordering::Relaxed);
    }

    /// The token goes into the Secret with its tunnel, the connector
    /// starts, the report says so, and removing takes all of it away.
    #[tokio::test]
    async fn the_connector_runs_with_its_token_and_goes_with_it() {
        let _one = SERIAL.lock().await;
        let (kube, client) = FakeKube::start().await;
        reset();
        assert!(!set(&client, "../x", "tok").await.ok);
        assert!(!set(&client, ID, "").await.ok);
        let r = set(&client, ID, "eyJhIjoiYiJ9").await;
        assert!(r.ok, "{:?}", r.message);
        let s = kube.get(SECRET).expect("the token's Secret");
        assert_eq!(s["metadata"]["labels"][system::TUNNEL_ID_LABEL], ID);
        assert_eq!(s["stringData"]["token"], "eyJhIjoiYiJ9");
        let d = kube.get(DEPLOYMENT).expect("the connector");
        assert_eq!(d["spec"]["replicas"], 2);
        assert!(kube.get(BUDGET).is_some());
        assert!(kube.get(ORIGIN).is_some());
        let rep = report(&client).await;
        assert_eq!(rep.tunnel_id.as_deref(), Some(ID));
        assert!(rep.origin_ready && !rep.refused);
        // The token never shows in what's reported.
        assert!(!serde_json::to_string(&r.data).unwrap().contains("eyJhIjoiYiJ9"));

        // Wanted for another tunnel: left alone, waiting for its token.
        follow(
            &client,
            &TunnelWanted {
                tunnel_id: Some("00000000-0000-4000-8000-000000000001".into()),
                ports_public: true,
            },
        )
        .await;
        assert!(kube.get(DEPLOYMENT).is_some());
        // Not wanted any more: gone, token and all.
        follow(&client, &TunnelWanted::default()).await;
        for path in [SECRET, DEPLOYMENT, BUDGET, ORIGIN] {
            assert!(kube.get(path).is_none(), "{path}");
        }
        assert_eq!(report(&client).await.tunnel_id, None);
        // Removing again is fine.
        assert!(remove(&client).await.ok);
    }

    /// A new token changes the connector's pod template (so its pods
    /// restart with it, cloudflared reads it only when it starts), the
    /// same token doesn't, and a write that failed after the Secret took
    /// the new token converges on the retry or the next heartbeat.
    #[tokio::test]
    async fn a_new_token_rolls_the_connector_out() {
        let _one = SERIAL.lock().await;
        let (kube, client) = FakeKube::start().await;
        reset();
        let hash = |kube: &FakeKube| {
            kube.get(DEPLOYMENT).expect("the connector")["spec"]["template"]["metadata"]
                ["annotations"][system::TUNNEL_TOKEN_HASH_ANNOTATION]
                .as_str()
                .map(str::to_owned)
        };
        let wanted = TunnelWanted {
            tunnel_id: Some(ID.into()),
            ports_public: true,
        };
        assert!(set(&client, ID, "eyJ0b2tlbiI6MX0").await.ok);
        let first = hash(&kube).expect("the token's hash");
        assert_eq!(first, system::tunnel_token_hash("eyJ0b2tlbiI6MX0"));
        let d = kube.get(DEPLOYMENT).unwrap().to_string();
        assert!(!d.contains("eyJ0b2tlbiI6MX0"), "never the token");
        // The same token again: nothing changes.
        assert!(set(&client, ID, "eyJ0b2tlbiI6MX0").await.ok);
        follow(&client, &wanted).await;
        assert_eq!(hash(&kube), Some(first.clone()));

        // The connector's write fails after the Secret has the new token.
        let fail = |kube: &FakeKube| {
            kube.admit(|w| (w.plural == "deployments").then(|| (500, "etcd timed out".into())))
        };
        fail(&kube);
        assert!(!set(&client, ID, "eyJ0b2tlbiI6Mn0").await.ok);
        assert_eq!(
            kube.get(SECRET).unwrap()["stringData"]["token"],
            "eyJ0b2tlbiI6Mn0"
        );
        assert_eq!(hash(&kube), Some(first.clone()), "not rolled out yet");
        // Pyo sends the same token again: it rolls out.
        kube.admit(|_| None);
        assert!(set(&client, ID, "eyJ0b2tlbiI6Mn0").await.ok);
        assert_eq!(
            hash(&kube),
            Some(system::tunnel_token_hash("eyJ0b2tlbiI6Mn0"))
        );

        // Again, and this time the next heartbeat gets there first.
        fail(&kube);
        assert!(!set(&client, ID, "eyJ0b2tlbiI6M30").await.ok);
        kube.admit(|_| None);
        follow(&client, &wanted).await;
        assert_eq!(
            hash(&kube),
            Some(system::tunnel_token_hash("eyJ0b2tlbiI6M30"))
        );
        assert!(!report(&client).await.refused);
    }

    /// The connector is put back when it's missing, with the token here.
    #[tokio::test]
    async fn a_missing_connector_is_put_back() {
        let _one = SERIAL.lock().await;
        let (kube, client) = FakeKube::start().await;
        reset();
        kube.put(SECRET, system::tunnel_secret(ID, "tok"));
        let wanted = TunnelWanted {
            tunnel_id: Some(ID.into()),
            ports_public: true,
        };
        follow(&client, &wanted).await;
        assert!(kube.get(DEPLOYMENT).is_some());
    }

    /// An install older than this agent refuses the connector: the command
    /// says what to run, the report says so, and nothing is tried again
    /// until the install changes.
    #[tokio::test]
    async fn an_older_install_is_reported_and_waited_for() {
        let _one = SERIAL.lock().await;
        let (kube, client) = FakeKube::start().await;
        reset();
        kube.admit(|w| {
            (w.plural == "deployments").then(|| (403, "deployments.apps \"pyo-tunnel\" is forbidden: ValidatingAdmissionPolicy 'pyo-agent' with binding 'pyo-agent' denied request".into()))
        });
        let r = set(&client, ID, "tok").await;
        assert!(!r.ok);
        assert_eq!(r.message.as_deref(), Some(REFUSED));
        assert!(report(&client).await.refused);
        let before = kube.refused().len();
        follow(
            &client,
            &TunnelWanted {
                tunnel_id: Some(ID.into()),
                ports_public: true,
            },
        )
        .await;
        assert_eq!(kube.refused().len(), before, "not tried again");
        assert!(!REFUSED.contains(';'));
    }

    #[test]
    fn pods_report_their_server_and_why_they_wait() {
        let pod: Pod = serde_json::from_value(json!({
            "metadata": { "name": "pyo-tunnel-1" },
            "spec": { "nodeName": "fal2", "containers": [] },
            "status": {
                "podIP": "10.42.1.7", "podIPs": [{ "ip": "10.42.1.7" }, { "ip": "fd00::7" }],
                "conditions": [{ "type": "Ready", "status": "False" }],
                "containerStatuses": [{ "name": "cloudflared", "ready": false, "restartCount": 3, "image": "x", "imageID": "",
                    "state": { "waiting": { "reason": "ImagePullBackOff" } } }],
            },
        }))
        .unwrap();
        let r = pod_reports(std::slice::from_ref(&pod));
        assert_eq!(r[0].server, "fal2");
        assert!(!r[0].ready);
        assert_eq!(r[0].restarts, 3);
        assert_eq!(r[0].reason.as_deref(), Some("ImagePullBackOff"));
        assert_eq!(pod_cidrs(&[pod]), ["10.42.1.7/32", "fd00::7/128"]);
        assert!(id_ok(ID) && !id_ok("x"));
    }
}
