//! The cluster's Cloudflare Tunnel connector (docs/cloudflare-tunnel.md §7):
//! cloudflared in pyo-system with the tunnel's token, pointed at Pyo
//! Gateway's HTTPS through `system::TUNNEL_ORIGIN_SERVICE`. Pyo's control
//! plane holds the Cloudflare account. The agent only ever gets the
//! tunnel's own token (`TunnelSet`), keeps it in a Secret labelled with the
//! tunnel's id, and follows what each heartbeat reply wants.
//!
//! The connectors' pod addresses are the only ones the tunnel listeners
//! trust to say who the visitor is (gateway.rs, the `tunnel` client
//! policies), and on a switched domain the gate refuses everyone else. So
//! a new connector must be trusted before Cloudflare sends it a request:
//!
//! - [`run`] watches the connectors' pods. A new pod's address goes into
//!   the policies as soon as it has one. An address leaves them only once
//!   its pod is gone (a stopping pod still finishes requests), and then
//!   with the next address that comes, or [`REMOVE_AFTER`] later: every
//!   rewrite makes the proxies drain the tunnel listeners' connections.
//! - An address is in effect once every tunnel policy has it and the
//!   gateway's controller has answered for that version of each, plus
//!   [`SETTLE`] for the proxies to get it.
//! - The connector's init container ([`gate`], `pyo-agent tunnel-gate`)
//!   asks this agent ([`serve_gate`]) until its pod's addresses are in
//!   effect, and only then lets cloudflared start. It gives up after
//!   `system::TUNNEL_GATE_SECONDS`, so an agent that's down never keeps the
//!   tunnel from starting. Meanwhile the connectors already running serve.
//!
//! A cluster whose install is older than this agent refuses these objects:
//! what runs is kept, nothing more is written until the install changes,
//! and the heartbeat says so (`TunnelReport::refused`).

use std::{
    sync::{
        Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use k8s_openapi::api::{
    apps::v1::Deployment,
    core::v1::{Pod, Secret, Service},
    policy::v1::PodDisruptionBudget,
};
use kube::{
    Api, Client, ResourceExt,
    api::{DeleteParams, DynamicObject, ListParams, Patch, PatchParams},
};
use pyo_proto::{CommandResult, TunnelPod, TunnelReport, TunnelWanted, rbac, system};

use crate::{MANAGER, failed};

/// Said when the cluster's install refuses the tunnel's objects.
pub const REFUSED: &str = "This cluster's install is older than Pyo's. Run the command under Refresh this cluster in the cluster's settings to use Cloudflare Tunnel.";

/// How long the proxies get to take a policy the gateway's controller
/// has answered for, before its addresses count as in effect.
const SETTLE: Duration = Duration::from_secs(2);
/// How long after the last connector went its address stays in the
/// policies, when no new address takes it along first.
const REMOVE_AFTER: Duration = Duration::from_secs(60);
/// Between two looks at the connectors' pods with nothing waiting (the
/// watch says sooner when one changes).
const RESYNC: Duration = Duration::from_secs(30);
/// Between two looks while an address waits to be in effect: for
/// [`QUICK_FOR`] after the policies changed, then [`SLOW`].
const QUICK: Duration = Duration::from_millis(500);
const QUICK_FOR: Duration = Duration::from_secs(30);
const SLOW: Duration = Duration::from_secs(5);
/// After a look that failed.
const RETRY: Duration = Duration::from_secs(10);
/// The least time between two rewrites of policies found without the
/// connectors' addresses.
const REPAIR_EVERY: Duration = Duration::from_secs(10);

/// What the agent keeps between heartbeats.
#[derive(Default)]
struct Kept {
    /// The connectors' pod addresses written into the tunnel policies.
    pods: Vec<String>,
    /// Those of them in effect: what the gate goes by.
    in_effect: Vec<String>,
    /// Since when every tunnel policy has `pods` and the gateway's
    /// controller has answered for it.
    taken_at: Option<Instant>,
    /// Addresses in `pods` whose pod is gone, and when the last one went.
    leaving: Vec<String>,
    left_at: Option<Instant>,
    /// When the policies were last rewritten, and last repaired.
    changed_at: Option<Instant>,
    repaired_at: Option<Instant>,
    /// The connector's objects as last applied (their JSON).
    applied: String,
    /// The install stamp a write was refused on.
    refused_on: Option<String>,
}

static KEPT: Mutex<Kept> = Mutex::new(Kept {
    pods: vec![],
    in_effect: vec![],
    taken_at: None,
    leaving: vec![],
    left_at: None,
    changed_at: None,
    repaired_at: None,
    applied: String::new(),
    refused_on: None,
});
static REFUSED_NOW: AtomicBool = AtomicBool::new(false);

/// Tests that change what the agent keeps of the tunnel (gateway.rs has
/// one too) share it: one at a time.
#[cfg(test)]
pub(crate) static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

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
/// template, so a new token rolls the connector out. With `gate_image`
/// ([`gate_image`] picks it), the gate ahead of cloudflared and the
/// Service it asks the agent through.
fn connector(token: &str, gate_image: Option<&str>) -> [serde_json::Value; 4] {
    [
        system::tunnel_deployment(
            system::CLOUDFLARED_IMAGE,
            gate_image,
            &system::tunnel_token_hash(token),
        ),
        system::tunnel_budget(),
        system::tunnel_origin_service(),
        system::tunnel_gate_service(),
    ]
}

/// The agent's own image, for the gate: its Deployment's. None when it
/// has no Deployment here (an agent run outside the cluster).
async fn own_image(client: &Client) -> kube::Result<Option<String>> {
    let own = Api::<Deployment>::namespaced(client.clone(), rbac::NS)
        .get_opt(rbac::AGENT_DEPLOYMENT)
        .await?;
    Ok(own
        .and_then(|d| d.spec?.template.spec)
        .and_then(|pod| pod.containers.into_iter().find(|c| c.name == "agent")?.image))
}

/// The gate's image in a connector Deployment as it is, and its template's
/// hash (`system::TUNNEL_TEMPLATE_ANNOTATION`).
fn live_gate(d: &Deployment) -> Option<(String, String)> {
    let template = &d.spec.as_ref()?.template;
    let hash = template
        .metadata
        .as_ref()?
        .annotations
        .as_ref()?
        .get(system::TUNNEL_TEMPLATE_ANNOTATION)?
        .clone();
    let image = template
        .spec
        .as_ref()?
        .init_containers
        .as_ref()?
        .iter()
        .find(|c| c.name == system::TUNNEL_GATE)?
        .image
        .clone()?;
    Some((image, hash))
}

/// Whether the connectors' pods show the gate's `image` can start: one of
/// them ran it (it's pulled, so the registry had it), and none of them
/// waits because it can't pull it.
fn gate_pulled(list: &[Pod], image: &str) -> bool {
    let mut ran = false;
    for p in list {
        let has_it = p
            .spec
            .as_ref()
            .and_then(|s| s.init_containers.as_ref())
            .is_some_and(|cs| {
                cs.iter()
                    .any(|c| c.name == system::TUNNEL_GATE && c.image.as_deref() == Some(image))
            });
        if !has_it {
            continue;
        }
        let gate = p
            .status
            .as_ref()
            .and_then(|s| s.init_container_statuses.as_ref())
            .and_then(|cs| cs.iter().find(|c| c.name == system::TUNNEL_GATE));
        let Some(gate) = gate else { continue };
        let waiting = gate
            .state
            .as_ref()
            .and_then(|s| s.waiting.as_ref())
            .and_then(|w| w.reason.as_deref());
        if matches!(
            waiting,
            Some("ErrImagePull" | "ImagePullBackOff" | "InvalidImageName" | "ErrImageNeverPull")
        ) {
            return false;
        }
        ran |= !gate.image_id.is_empty();
    }
    ran
}

/// The image for the gate. The one the connector already runs while the
/// rest of its pod template stays the same and its pods show it pulled:
/// a new agent image alone never restarts the connector (its WebSockets
/// would drop with it). Else the agent's own (None outside the cluster),
/// in a template that changes anyway: a new cloudflared, a new token, a
/// new gate version or anything else, or a first gate.
async fn gate_image(
    client: &Client,
    live: Option<&Deployment>,
    token: &str,
) -> kube::Result<Option<String>> {
    if let Some((image, hash)) = live.and_then(live_gate) {
        let [d, ..] = connector(token, Some(&image));
        let same = d["spec"]["template"]["metadata"]["annotations"][system::TUNNEL_TEMPLATE_ANNOTATION]
            .as_str()
            == Some(hash.as_str());
        if same && gate_pulled(&pods(client).list(&selector()).await?.items, &image) {
            return Ok(Some(image));
        }
    }
    own_image(client).await
}

/// Write the connector's objects (only when they changed since the last
/// write here, or are missing). Its Secret goes first, whole: a Secret
/// written by an older agent has no local config, and the new pods mount it.
async fn apply_connector(client: &Client, tunnel_id: &str, token: &str, force: bool) -> kube::Result<()> {
    let deployments: Api<Deployment> = Api::namespaced(client.clone(), rbac::NS);
    let live = deployments.get_opt(system::TUNNEL_NAME).await?;
    // Read each time: an agent that couldn't read it once must not take
    // the gate off the connector.
    let image = gate_image(client, live.as_ref(), token).await?;
    let objects = connector(token, image.as_deref());
    let text = serde_json::to_string(&objects).unwrap_or_default();
    let there = live.is_some();
    if !force && there && kept().applied == text {
        return Ok(());
    }
    let [d, b, s, g] = &objects;
    secrets(client)
        .patch(
            system::TUNNEL_NAME,
            &pp(),
            &Patch::Apply(system::tunnel_secret(tunnel_id, token)),
        )
        .await?;
    // The gate's Service first: the new pods ask through it.
    Api::<Service>::namespaced(client.clone(), rbac::NS)
        .patch(system::TUNNEL_GATE_SERVICE, &pp(), &Patch::Apply(g))
        .await?;
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
    // The token is kept first, whatever comes after.
    let written = async {
        secrets(client)
            .patch(
                system::TUNNEL_NAME,
                &pp(),
                &Patch::Apply(system::tunnel_secret(tunnel_id, token)),
            )
            .await?;
        apply_connector(client, tunnel_id, token, true).await
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
    gone(Api::<Service>::namespaced(client.clone(), rbac::NS)
        .delete(system::TUNNEL_GATE_SERVICE, &dp)
        .await
        .map(|_| ()))?;
    kept().applied.clear();
    Ok(())
}

/// What a heartbeat reply wants: the connector for this tunnel (when the
/// token here is for it), or none. The tunnel policies follow the
/// connectors' pods by themselves ([`run`]).
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
            apply_connector(client, h, token, false).await
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
    }
}

fn pods(client: &Client) -> Api<Pod> {
    Api::namespaced(client.clone(), rbac::NS)
}

fn label() -> String {
    format!("app.kubernetes.io/name={}", system::TUNNEL_APP)
}

fn selector() -> ListParams {
    ListParams::default().labels(&label())
}

/// Every address of the connectors' pods, ready or not, stopping or not,
/// as /32 and /128. Not a pod's that ended (evicted, say): its address is
/// free for any other pod to get.
pub fn pod_cidrs(list: &[Pod]) -> Vec<String> {
    let ips: Vec<String> = list
        .iter()
        .filter_map(|p| p.status.as_ref())
        .filter(|s| !matches!(s.phase.as_deref(), Some("Succeeded" | "Failed")))
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

/// What the tunnel policies should trust, given what they do (`written`)
/// and the connectors' pods as they are (`now`), or None to leave them. A
/// new address goes in at once, and takes along the ones that are gone.
/// With only addresses gone, they wait until `removal_due`.
fn next_trusted(written: &[String], now: &[String], removal_due: bool) -> Option<Vec<String>> {
    let added = now.iter().any(|c| !written.contains(c));
    let gone = written.iter().any(|c| !now.contains(c));
    (added || (gone && removal_due)).then(|| now.to_vec())
}

/// Whether a tunnel policy trusts every one of `cidrs`.
fn policy_trusts(p: &DynamicObject, cidrs: &[String]) -> bool {
    let have = p.data["spec"]["clientIPDetection"]["xForwardedFor"]["trustedCIDRs"].as_array();
    cidrs
        .iter()
        .all(|c| have.is_some_and(|h| h.iter().any(|x| x.as_str() == Some(c.as_str()))))
}

/// Whether the gateway's controller has answered for this version of a
/// tunnel policy, on every Gateway it's for: accepted or not. One it
/// refuses doesn't get better by waiting, and its listeners trust nobody
/// either way.
fn policy_taken(p: &DynamicObject) -> bool {
    let Some(generation) = p.metadata.generation else {
        return false;
    };
    p.data["status"]["ancestors"].as_array().is_some_and(|a| {
        !a.is_empty()
            && a.iter().all(|x| {
                x["conditions"].as_array().is_some_and(|cs| {
                    cs.iter().any(|c| {
                        c["type"] == "Accepted"
                            && c["observedGeneration"].as_i64() == Some(generation)
                    })
                })
            })
    })
}

/// When to look again by itself.
fn next_look(k: &Kept) -> Duration {
    if k.pods.iter().any(|c| !k.in_effect.contains(c)) {
        return match k.changed_at {
            Some(t) if t.elapsed() < QUICK_FOR => QUICK,
            _ => SLOW,
        };
    }
    match k.left_at {
        Some(t) => REMOVE_AFTER.saturating_sub(t.elapsed()).clamp(QUICK, RESYNC),
        None => RESYNC,
    }
}

/// One look at a time.
static FOLLOWING: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// One look at the connectors' pods: the tunnel policies trust the new
/// ones at once and drop the gone ones late, and what's in effect is
/// noted for the gate. Says when to look again.
async fn follow_pods(client: &Client) -> anyhow::Result<Duration> {
    let _one = FOLLOWING.lock().await;
    let now = pod_cidrs(&pods(client).list(&selector()).await?.items);
    let (written, removal_due) = {
        let mut k = kept();
        let leaving: Vec<String> = k.pods.iter().filter(|c| !now.contains(c)).cloned().collect();
        if leaving != k.leaving {
            k.left_at = (!leaving.is_empty()).then(Instant::now);
            k.leaving = leaving;
        }
        let due = k.left_at.is_some_and(|t| t.elapsed() >= REMOVE_AFTER);
        (k.pods.clone(), due)
    };
    let written = match next_trusted(&written, &now, removal_due) {
        Some(next) => {
            crate::gateway::set_tunnel_pods(client, &written, &next).await?;
            tracing::info!(pods = next.len(), "the tunnel's trusted addresses changed");
            let mut k = kept();
            k.in_effect.retain(|c| next.contains(c));
            k.pods = next.clone();
            k.taken_at = None;
            k.leaving.clear();
            k.left_at = None;
            k.changed_at = Some(Instant::now());
            next
        }
        None => written,
    };
    if written.is_empty() {
        return Ok(next_look(&kept()));
    }
    // Every policy has them, and the gateway's controller has answered:
    // in effect once the proxies had their time. A policy without them (a
    // deploy wrote it with the addresses as they were): put back.
    let policies = crate::gateway::tunnel_policies(client).await?;
    let lacking = policies.iter().any(|p| !policy_trusts(p, &written));
    let taken = !lacking && policies.iter().all(policy_taken);
    let repair = {
        let mut k = kept();
        if lacking {
            k.in_effect.clear();
        }
        if taken {
            let since = *k.taken_at.get_or_insert_with(Instant::now);
            if since.elapsed() >= SETTLE && k.in_effect != written {
                tracing::info!(pods = written.len(), "the tunnel's trusted addresses are in effect");
                k.in_effect = written.clone();
            }
        } else {
            k.taken_at = None;
        }
        // Not while the cluster's install refuses the tunnel's policies.
        let repair = lacking
            && !REFUSED_NOW.load(Ordering::Relaxed)
            && k.repaired_at.is_none_or(|t| t.elapsed() >= REPAIR_EVERY);
        if repair {
            k.repaired_at = Some(Instant::now());
        }
        repair
    };
    if repair {
        crate::gateway::set_tunnel_pods(client, &written, &written).await?;
    }
    Ok(next_look(&kept()))
}

/// Keeps the tunnel policies on the connectors' pods for as long as the
/// agent runs: a look at each change the watch brings, and by itself
/// while something waits. Never in a read-only cluster.
pub async fn run(agent: crate::Agent) {
    use futures::{FutureExt, StreamExt};
    use kube::runtime::{WatchStreamExt, watcher};
    let config = watcher::Config::default().labels(&label());
    let mut changes = watcher(pods(&agent.kube), config).default_backoff().boxed();
    loop {
        let read_only = agent.policy.read().unwrap_or_else(|e| e.into_inner()).read_only;
        let wait = if read_only {
            RESYNC
        } else {
            match follow_pods(&agent.kube).await {
                Ok(wait) => wait,
                Err(e) => {
                    tracing::warn!(error = %format!("{e:#}"), "couldn't update the tunnel's trusted addresses");
                    RETRY
                }
            }
        };
        let change = tokio::select! {
            change = changes.next() => change,
            _ = tokio::time::sleep(wait) => continue,
        };
        if let Some(Err(e)) = &change {
            tracing::debug!(error = %e, "the watch on the tunnel's connectors stopped: starting it again");
        }
        // What came with it (a start lists every pod): one look.
        while let Some(Some(_)) = changes.next().now_or_never() {}
    }
}

/// Whether every one of these addresses (a pod's, comma separated) is in
/// effect: in every tunnel policy, with the proxies.
fn in_effect(ips: &str) -> bool {
    let k = kept();
    let mut asked = ips.split(',').map(str::trim).filter(|ip| !ip.is_empty()).peekable();
    asked.peek().is_some()
        && asked.all(|ip| {
            system::host_cidrs(&[ip.to_owned()])
                .first()
                .is_some_and(|c| k.in_effect.contains(c))
        })
}

/// The gate's answer to `?ip=<address>[,<address>]`.
fn gate_answer(query: Option<&str>) -> &'static str {
    let ips = query
        .into_iter()
        .flat_map(|q| q.split('&'))
        .find_map(|kv| kv.strip_prefix("ip="))
        .unwrap_or("");
    if in_effect(ips) { "yes" } else { "no" }
}

fn gate_router() -> axum::Router {
    use axum::{extract::RawQuery, routing::get};
    axum::Router::new().route(
        system::TUNNEL_GATE_PATH,
        get(|RawQuery(q): RawQuery| async move { gate_answer(q.as_deref()) }),
    )
}

/// Answers the connectors' gates for as long as the agent runs: "yes" or
/// "no" to whether a pod's addresses are in effect, and nothing else. On
/// the agent's pod address only, port `system::TUNNEL_GATE_PORT`.
pub async fn serve_gate() {
    let addr = std::net::SocketAddr::from(([0, 0, 0, 0], system::TUNNEL_GATE_PORT));
    loop {
        match tokio::net::TcpListener::bind(addr).await {
            Ok(l) => {
                if let Err(e) = axum::serve(l, gate_router()).await {
                    tracing::warn!(error = %e, "the tunnel's gate stopped answering");
                }
            }
            Err(e) => tracing::warn!(%addr, error = %e, "the tunnel's gate can't listen"),
        }
        tokio::time::sleep(RETRY).await;
    }
}

/// One question to the agent at `address` (host:port): are `ips` in effect?
async fn ask(address: &str, ips: &str) -> anyhow::Result<bool> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let asked = async {
        let mut s = tokio::net::TcpStream::connect(address).await?;
        let request = format!(
            "GET {}?ip={ips} HTTP/1.1\r\nHost: {address}\r\nConnection: close\r\n\r\n",
            system::TUNNEL_GATE_PATH
        );
        s.write_all(request.as_bytes()).await?;
        let mut answer = vec![];
        s.take(4096).read_to_end(&mut answer).await?;
        anyhow::Ok(answer)
    };
    let answer = tokio::time::timeout(Duration::from_secs(3), asked)
        .await
        .map_err(|_| anyhow::anyhow!("no answer in time"))??;
    let text = String::from_utf8_lossy(&answer);
    let (head, body) = text
        .split_once("\r\n\r\n")
        .ok_or_else(|| anyhow::anyhow!("not an HTTP answer"))?;
    anyhow::ensure!(
        head.split_whitespace().nth(1) == Some("200"),
        "{}",
        head.lines().next().unwrap_or_default()
    );
    Ok(body.trim() == "yes")
}

/// Asks every `every` until the agent says `ips` are in effect (true), or
/// `total` has passed (false).
async fn wait_trusted(address: &str, ips: &str, total: Duration, every: Duration) -> bool {
    let started = Instant::now();
    loop {
        match ask(address, ips).await {
            Ok(true) => return true,
            Ok(false) => {}
            Err(e) => tracing::debug!(error = %format!("{e:#}"), "couldn't ask the agent"),
        }
        if started.elapsed() + every > total {
            return false;
        }
        tokio::time::sleep(every).await;
    }
}

/// `pyo-agent tunnel-gate`, the connector's init container: returns once
/// the tunnel listeners trust this pod's addresses (`POD_IPS`), so
/// cloudflared starts after it. Never fails: without an answer in
/// `system::TUNNEL_GATE_SECONDS` cloudflared starts anyway.
pub async fn gate() {
    let ips = std::env::var("POD_IPS").unwrap_or_default();
    // Only addresses go into the question.
    let plain = !ips.is_empty()
        && ips.bytes().all(|b| b.is_ascii_hexdigit() || matches!(b, b'.' | b':' | b','));
    if !plain {
        // Nothing to ask with: the agent's watch has a few seconds.
        tracing::warn!("this pod's addresses aren't known: starting the connector in 15 seconds");
        tokio::time::sleep(Duration::from_secs(15)).await;
        return;
    }
    let total = Duration::from_secs(system::TUNNEL_GATE_SECONDS);
    let address = system::tunnel_gate_address();
    if wait_trusted(&address, &ips, total, Duration::from_secs(1)).await {
        tracing::info!(%ips, "the tunnel trusts this connector's addresses: starting it");
    } else {
        tracing::warn!(
            %ips,
            seconds = total.as_secs(),
            "the agent didn't say the tunnel trusts this connector's addresses: starting it anyway"
        );
    }
}

/// Why a pod isn't ready, in plain words ([`pyo_proto::pod_problem`]) from
/// a container's waiting or last terminated reason. Never Kubernetes' own
/// word.
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
        pyo_proto::pod_problem(&r).map(str::to_owned)
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
    const GATE: &str = "api/v1/namespaces/pyo-system/services/pyo-tunnel-gate";
    const AGENT: &str = "apis/apps/v1/namespaces/pyo-system/deployments/pyo-agent";
    const POLICY: &str = "apis/gateway.envoyproxy.io/v1alpha1/namespaces/pyo-gateway/clienttrafficpolicies/shop-web-tunnel";

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
        assert!(kube.get(GATE).is_some());
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
        for path in [SECRET, DEPLOYMENT, BUDGET, ORIGIN, GATE] {
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

    /// A Secret an older agent wrote has the token only. The connector's
    /// pods now mount its local config, so the Secret gets it, before the
    /// Deployment that mounts it, with the token kept.
    #[tokio::test]
    async fn an_older_secret_gets_the_local_config_before_the_connector() {
        let _one = SERIAL.lock().await;
        let (kube, client) = FakeKube::start().await;
        reset();
        kube.put(
            SECRET,
            json!({
                "apiVersion": "v1", "kind": "Secret",
                "metadata": { "name": system::TUNNEL_NAME, "namespace": rbac::NS, "labels": { system::TUNNEL_ID_LABEL: ID } },
                "stringData": { "token": "tok" },
            }),
        );
        let order = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let seen = order.clone();
        kube.admit(move |w| {
            seen.lock().unwrap().push(w.plural.clone());
            None
        });
        let wanted = TunnelWanted {
            tunnel_id: Some(ID.into()),
            ports_public: true,
        };
        follow(&client, &wanted).await;
        let s = kube.get(SECRET).unwrap();
        assert_eq!(s["stringData"]["token"], "tok");
        assert_eq!(s["stringData"][system::TUNNEL_CONFIG_KEY], system::tunnel_local_config());
        let writes = order.lock().unwrap().clone();
        let at = |p: &str| writes.iter().position(|w| w == p).unwrap_or_else(|| panic!("{p}: {writes:?}"));
        assert!(at("secrets") < at("deployments"), "{writes:?}");
        // The connector reads it, and only it: never the token as a file.
        let d = kube.get(DEPLOYMENT).unwrap();
        let pod = &d["spec"]["template"]["spec"];
        assert_eq!(pod["volumes"][0]["secret"]["items"], json!([{ "key": system::TUNNEL_CONFIG_KEY, "path": system::TUNNEL_CONFIG_KEY }]));
        let args = pod["containers"][0]["args"].to_string();
        assert!(args.contains(&format!("\"--config\",\"{}/{}\"", system::TUNNEL_CONFIG_DIR, system::TUNNEL_CONFIG_KEY)), "{args}");
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

    fn agent_deployment(image: &str) -> serde_json::Value {
        json!({
            "apiVersion": "apps/v1", "kind": "Deployment",
            "spec": {
                "selector": { "matchLabels": { "app.kubernetes.io/name": "pyo-agent" } },
                "template": {
                    "metadata": { "labels": { "app.kubernetes.io/name": "pyo-agent" } },
                    "spec": { "containers": [{ "name": "agent", "image": image }] },
                },
            },
        })
    }

    /// The gate ahead of cloudflared runs the agent's own image, as its
    /// Deployment has it now. An agent with no Deployment (run outside the
    /// cluster) starts the connector without one.
    #[tokio::test]
    async fn the_gate_runs_the_agents_own_image() {
        let _one = SERIAL.lock().await;
        let (kube, client) = FakeKube::start().await;
        reset();
        let gates = |kube: &FakeKube| {
            kube.get(DEPLOYMENT).expect("the connector")["spec"]["template"]["spec"]
                ["initContainers"]
                .clone()
        };
        assert!(set(&client, ID, "tok").await.ok);
        assert!(gates(&kube).is_null(), "no gate without the agent's image");

        kube.put(AGENT, agent_deployment("ghcr.io/upsetdev/pyo/agent:1.2.3"));
        let wanted = TunnelWanted {
            tunnel_id: Some(ID.into()),
            ports_public: true,
        };
        follow(&client, &wanted).await;
        let g = gates(&kube);
        assert_eq!(g[0]["name"], system::TUNNEL_GATE);
        assert_eq!(g[0]["image"], "ghcr.io/upsetdev/pyo/agent:1.2.3");
        assert_eq!(g[0]["args"], json!(["tunnel-gate"]));
        let service = kube.get(GATE).expect("the gate's Service");
        assert_eq!(service["spec"]["ports"][0]["port"], 8092);
        // An updated agent, with no connector pod that ran the gate yet:
        // its new image.
        kube.put(AGENT, agent_deployment("ghcr.io/upsetdev/pyo/agent:1.2.4"));
        follow(&client, &wanted).await;
        assert_eq!(gates(&kube)[0]["image"], "ghcr.io/upsetdev/pyo/agent:1.2.4");
    }

    /// A connector pod whose gate runs `image`: done (pulled, it has an
    /// image id) or waiting for `reason`.
    fn gate_pod(kube: &FakeKube, name: &str, image: &str, waiting: Option<&str>) {
        let state = match waiting {
            Some(r) => json!({ "waiting": { "reason": r } }),
            None => json!({ "terminated": { "exitCode": 0, "reason": "Completed" } }),
        };
        let image_id = if waiting.is_some() { "" } else { "ghcr.io/upsetdev/pyo/agent@sha256:0a1b" };
        let mut p = tunnel_pod("10.42.0.5");
        p["spec"]["initContainers"] = json!([{ "name": system::TUNNEL_GATE, "image": image }]);
        p["status"]["initContainerStatuses"] = json!([{
            "name": system::TUNNEL_GATE, "image": image, "imageID": image_id,
            "ready": waiting.is_none(), "restartCount": 0, "state": state,
        }]);
        kube.put(&pod_path(name), p);
    }

    /// A new agent image alone keeps the gate's image the connector runs,
    /// so an update never restarts it. It takes the agent's own when the
    /// template changes anyway, and when the image it runs can't be pulled
    /// or no pod shows it pulled.
    #[tokio::test]
    async fn a_new_agent_image_alone_keeps_the_gate() {
        let _one = SERIAL.lock().await;
        let (kube, client) = FakeKube::start().await;
        reset();
        let (v1, v2, v3) = (
            "ghcr.io/upsetdev/pyo/agent:1.2.3",
            "ghcr.io/upsetdev/pyo/agent:1.2.4",
            "ghcr.io/upsetdev/pyo/agent:1.2.5",
        );
        let gate = |kube: &FakeKube| {
            kube.get(DEPLOYMENT).expect("the connector")["spec"]["template"]["spec"]
                ["initContainers"][0]["image"]
                .as_str()
                .map(str::to_owned)
        };
        let template = |kube: &FakeKube| kube.get(DEPLOYMENT).unwrap()["spec"]["template"].clone();
        kube.put(AGENT, agent_deployment(v1));
        assert!(set(&client, ID, "tok").await.ok);
        assert_eq!(gate(&kube).as_deref(), Some(v1));
        gate_pod(&kube, "pyo-tunnel-a", v1, None);

        // The agent updates (and restarts: nothing kept in memory). The
        // template stays as it was, to the byte.
        let before = template(&kube);
        kube.put(AGENT, agent_deployment(v2));
        reset();
        assert!(set(&client, ID, "tok").await.ok);
        assert_eq!(template(&kube), before, "the connector isn't restarted");
        let wanted = TunnelWanted {
            tunnel_id: Some(ID.into()),
            ports_public: true,
        };
        reset();
        follow(&client, &wanted).await;
        assert_eq!(template(&kube), before);

        // A new token rolls it anyway: with the agent's own image.
        assert!(set(&client, ID, "tok2").await.ok);
        assert_eq!(gate(&kube).as_deref(), Some(v2));
        gate_pod(&kube, "pyo-tunnel-a", v2, None);
        kube.put(AGENT, agent_deployment(v3));
        assert!(set(&client, ID, "tok2").await.ok);
        assert_eq!(gate(&kube).as_deref(), Some(v2), "kept again");

        // Another gate version (an older agent wrote it): the template
        // changes, so the gate takes the agent's own image.
        let mut older = kube.get(DEPLOYMENT).unwrap();
        older["spec"]["template"]["metadata"]["annotations"][system::TUNNEL_GATE_VERSION_ANNOTATION] =
            json!("0");
        let hash = system::tunnel_template_hash(&older);
        older["spec"]["template"]["metadata"]["annotations"][system::TUNNEL_TEMPLATE_ANNOTATION] =
            json!(hash);
        kube.put(DEPLOYMENT, older);
        assert!(set(&client, ID, "tok2").await.ok);
        assert_eq!(gate(&kube).as_deref(), Some(v3));
        let d = kube.get(DEPLOYMENT).unwrap();
        assert_eq!(
            d["spec"]["template"]["metadata"]["annotations"][system::TUNNEL_GATE_VERSION_ANNOTATION],
            system::TUNNEL_GATE_VERSION
        );
    }

    /// The gate's image is kept only while the connectors' pods show it
    /// starts: a pod that can't pull it, or none that ran it, and the gate
    /// takes the agent's own. A connector without a gate (an update from
    /// 0.51.0) gets the agent's own, and an agent outside the cluster
    /// keeps the one there.
    #[tokio::test]
    async fn a_gate_image_that_cant_be_pulled_is_replaced() {
        let _one = SERIAL.lock().await;
        let (kube, client) = FakeKube::start().await;
        reset();
        let (old, own) = ("ghcr.io/upsetdev/pyo/agent:1.2.3", "ghcr.io/upsetdev/pyo/agent:1.2.4");
        let gate = |kube: &FakeKube| {
            kube.get(DEPLOYMENT).expect("the connector")["spec"]["template"]["spec"]
                ["initContainers"][0]["image"]
                .as_str()
                .map(str::to_owned)
        };
        // The connector as 0.51.0 left it: no gate.
        kube.put(SECRET, system::tunnel_secret(ID, "tok"));
        let mut bare = system::tunnel_deployment(system::CLOUDFLARED_IMAGE, None, "x");
        bare["spec"]["template"]["metadata"]["annotations"]
            .as_object_mut()
            .unwrap()
            .remove(system::TUNNEL_TEMPLATE_ANNOTATION);
        kube.put(DEPLOYMENT, bare);
        kube.put(AGENT, agent_deployment(own));
        assert!(set(&client, ID, "tok").await.ok);
        assert_eq!(gate(&kube).as_deref(), Some(own));

        // A gate on an older image that no pod shows pulled.
        let put_old = |kube: &FakeKube| {
            kube.put(
                DEPLOYMENT,
                system::tunnel_deployment(
                    system::CLOUDFLARED_IMAGE,
                    Some(old),
                    &system::tunnel_token_hash("tok"),
                ),
            )
        };
        put_old(&kube);
        assert!(set(&client, ID, "tok").await.ok);
        assert_eq!(gate(&kube).as_deref(), Some(own), "none ran it");

        // One ran it, another can't pull it (gone from the registry).
        put_old(&kube);
        gate_pod(&kube, "pyo-tunnel-a", old, None);
        gate_pod(&kube, "pyo-tunnel-b", old, Some("ImagePullBackOff"));
        assert!(set(&client, ID, "tok").await.ok);
        assert_eq!(gate(&kube).as_deref(), Some(own), "can't be pulled");

        // Pulled, and an agent with no Deployment here: kept.
        put_old(&kube);
        gate_pod(&kube, "pyo-tunnel-b", old, None);
        Api::<Deployment>::namespaced(client.clone(), rbac::NS)
            .delete(rbac::AGENT_DEPLOYMENT, &DeleteParams::default())
            .await
            .unwrap();
        assert!(set(&client, ID, "tok").await.ok);
        assert_eq!(gate(&kube).as_deref(), Some(old));
    }

    fn tunnel_pod(ip: &str) -> serde_json::Value {
        json!({
            "apiVersion": "v1", "kind": "Pod",
            "metadata": { "labels": { "app.kubernetes.io/name": system::TUNNEL_APP } },
            "spec": { "containers": [] },
            "status": { "phase": "Running", "podIP": ip, "podIPs": [{ "ip": ip }] },
        })
    }

    fn pod_path(name: &str) -> String {
        format!("api/v1/namespaces/pyo-system/pods/{name}")
    }

    /// A tunnel policy as the cluster has it: at `generation`, with the
    /// gateway's controller's answer for `answered`.
    fn tunnel_policy(trusted: &[&str], generation: i64, answered: i64) -> serde_json::Value {
        json!({
            "apiVersion": "gateway.envoyproxy.io/v1alpha1", "kind": "ClientTrafficPolicy",
            "metadata": { "labels": { system::CDN_LABEL: system::TUNNEL_CDN }, "generation": generation },
            "spec": { "clientIPDetection": { "xForwardedFor": { "trustedCIDRs": trusted } } },
            "status": { "ancestors": [{
                "ancestorRef": { "group": "gateway.networking.k8s.io", "kind": "Gateway", "name": "shop-web" },
                "conditions": [{ "type": "Accepted", "status": "True", "reason": "Accepted", "observedGeneration": answered }],
            }] },
        })
    }

    fn trusted_in(kube: &FakeKube) -> Vec<String> {
        let p = kube.get(POLICY).expect("the tunnel policy");
        p["spec"]["clientIPDetection"]["xForwardedFor"]["trustedCIDRs"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c.as_str().unwrap().to_owned())
            .collect()
    }

    /// The gateway's controller answers for the policy as it is now.
    fn answer(kube: &FakeKube) {
        let trusted = trusted_in(kube);
        let trusted: Vec<&str> = trusted.iter().map(String::as_str).collect();
        kube.put(POLICY, tunnel_policy(&trusted, 7, 7));
    }

    fn ago(d: Duration) -> Instant {
        Instant::now().checked_sub(d).expect("a clock that old")
    }

    /// A new address goes into the policies at once. One whose pod is gone
    /// stays until a new one takes it along, or until it's due.
    #[test]
    fn new_addresses_at_once_and_gone_ones_late() {
        let c = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let (a, b, n) = ("10.42.0.5/32", "10.42.1.5/32", "10.42.0.9/32");
        // Nothing changed.
        assert_eq!(next_trusted(&c(&[a, b]), &c(&[a, b]), false), None);
        assert_eq!(next_trusted(&c(&[a, b]), &c(&[a, b]), true), None);
        assert_eq!(next_trusted(&[], &[], true), None);
        // A new pod, the old ones still there (stopping or not).
        assert_eq!(next_trusted(&c(&[a, b]), &c(&[a, b, n]), false), Some(c(&[a, b, n])));
        // A pod gone: kept until it's due.
        assert_eq!(next_trusted(&c(&[a, b, n]), &c(&[b, n]), false), None);
        assert_eq!(next_trusted(&c(&[a, b, n]), &c(&[b, n]), true), Some(c(&[b, n])));
        // A pod gone and a new one: one write for both.
        assert_eq!(next_trusted(&c(&[a, b]), &c(&[b, n]), false), Some(c(&[b, n])));
        // The last one gone.
        assert_eq!(next_trusted(&c(&[a]), &[], false), None);
        assert_eq!(next_trusted(&c(&[a]), &[], true), Some(vec![]));
    }

    /// A pod that ended keeps its address in its status, and Kubernetes
    /// gives that address to another pod: never trusted. A stopping pod
    /// still is, it finishes its requests.
    #[test]
    fn a_pod_that_ended_is_not_trusted() {
        let pod = |ip: &str, phase: &str, stopping: bool| -> Pod {
            let mut p = tunnel_pod(ip);
            p["status"]["phase"] = json!(phase);
            if stopping {
                p["metadata"]["deletionTimestamp"] = json!("2026-01-01T00:00:00Z");
            }
            serde_json::from_value(p).unwrap()
        };
        let list = [
            pod("10.42.0.5", "Running", false),
            pod("10.42.0.6", "Running", true),
            pod("10.42.0.7", "Pending", false),
            pod("10.42.0.8", "Failed", false),
            pod("10.42.0.9", "Succeeded", false),
        ];
        assert_eq!(pod_cidrs(&list), ["10.42.0.5/32", "10.42.0.6/32", "10.42.0.7/32"]);
    }

    /// A policy counts once the gateway's controller answered for the
    /// version that's there, on every Gateway it's for.
    #[test]
    fn a_policy_is_taken_when_its_version_is_answered_for() {
        let policy = |v: serde_json::Value| -> DynamicObject { serde_json::from_value(v).unwrap() };
        let p = policy(tunnel_policy(&["10.42.0.5/32", "173.245.48.0/20"], 3, 3));
        assert!(policy_taken(&p));
        assert!(policy_trusts(&p, &["10.42.0.5/32".into()]));
        assert!(policy_trusts(&p, &[]));
        assert!(!policy_trusts(&p, &["10.42.0.5/32".into(), "10.42.0.9/32".into()]));
        // Written, not answered for yet.
        assert!(!policy_taken(&policy(tunnel_policy(&[], 4, 3))));
        // Refused is an answer too: waiting doesn't change it.
        let mut refused = tunnel_policy(&[], 4, 4);
        refused["status"]["ancestors"][0]["conditions"][0]["status"] = json!("False");
        assert!(policy_taken(&policy(refused)));
        // No answer at all, or not from every Gateway.
        let mut none = tunnel_policy(&[], 1, 1);
        none["status"] = json!({});
        assert!(!policy_taken(&policy(none)));
        let mut one = tunnel_policy(&[], 2, 2);
        let mut other = one["status"]["ancestors"][0].clone();
        other["conditions"][0]["observedGeneration"] = json!(1);
        one["status"]["ancestors"].as_array_mut().unwrap().push(other);
        assert!(!policy_taken(&policy(one)));
        let mut unversioned = tunnel_policy(&[], 2, 2);
        unversioned["metadata"].as_object_mut().unwrap().remove("generation");
        assert!(!policy_taken(&policy(unversioned)));
    }

    /// A connector's start to its end: its address is written as soon as
    /// it has one, the gate says yes only once the gateway's controller
    /// answered and the proxies had their time, the connectors already
    /// trusted stay so meanwhile, and a gone one's address leaves late.
    #[tokio::test]
    async fn a_new_connector_is_trusted_before_its_gate_opens() {
        let _one = SERIAL.lock().await;
        let (kube, client) = FakeKube::start().await;
        reset();
        kube.put(POLICY, tunnel_policy(&["0.0.0.0/32"], 1, 1));
        // No connector yet: nothing to write, nothing waits.
        assert_eq!(follow_pods(&client).await.unwrap(), RESYNC);
        assert_eq!(trusted_in(&kube), ["0.0.0.0/32"]);
        assert_eq!(gate_answer(Some("ip=10.42.0.5")), "no");

        // The first connector's pod has an address: written at once.
        kube.put(&pod_path("pyo-tunnel-a"), tunnel_pod("10.42.0.5"));
        assert_eq!(follow_pods(&client).await.unwrap(), QUICK);
        assert_eq!(trusted_in(&kube), ["10.42.0.5/32"]);
        assert_eq!(trusted_now(), ["10.42.0.5/32"]);
        // The gateway's controller hasn't answered: the gate stays shut.
        assert_eq!(follow_pods(&client).await.unwrap(), QUICK);
        assert_eq!(gate_answer(Some("ip=10.42.0.5")), "no");
        // It answered: the proxies get their time first.
        answer(&kube);
        assert_eq!(follow_pods(&client).await.unwrap(), QUICK);
        assert_eq!(gate_answer(Some("ip=10.42.0.5")), "no");
        kept().taken_at = Some(ago(SETTLE));
        assert_eq!(follow_pods(&client).await.unwrap(), RESYNC);
        assert_eq!(gate_answer(Some("ip=10.42.0.5")), "yes");

        // A rollout's new pod: in at once, next to the one still serving,
        // which stays trusted all along.
        kube.put(&pod_path("pyo-tunnel-b"), tunnel_pod("10.42.1.9"));
        assert_eq!(follow_pods(&client).await.unwrap(), QUICK);
        assert_eq!(trusted_in(&kube), ["10.42.0.5/32", "10.42.1.9/32"]);
        assert_eq!(gate_answer(Some("ip=10.42.0.5")), "yes");
        assert_eq!(gate_answer(Some("ip=10.42.1.9")), "no");
        answer(&kube);
        follow_pods(&client).await.unwrap();
        kept().taken_at = Some(ago(SETTLE));
        assert_eq!(follow_pods(&client).await.unwrap(), RESYNC);
        assert_eq!(gate_answer(Some("ip=10.42.1.9")), "yes");
        assert_eq!(gate_answer(Some("ip=10.42.0.5,10.42.1.9")), "yes");

        // The old pod is gone: its address stays for now, with no write.
        kube.admit(|w| (w.plural == "clienttrafficpolicies").then(|| (500, "not now".into())));
        let pods_api = pods(&client);
        pods_api.delete("pyo-tunnel-a", &DeleteParams::default()).await.unwrap();
        let wait = follow_pods(&client).await.unwrap();
        assert!(wait > SLOW && wait <= RESYNC, "{wait:?}");
        assert_eq!(trusted_in(&kube), ["10.42.0.5/32", "10.42.1.9/32"]);
        kube.admit(|_| None);
        // Due: one write takes it out, and the one left stays in effect.
        kept().left_at = Some(ago(REMOVE_AFTER));
        follow_pods(&client).await.unwrap();
        assert_eq!(trusted_in(&kube), ["10.42.1.9/32"]);
        assert_eq!(trusted_now(), ["10.42.1.9/32"]);
        assert_eq!(gate_answer(Some("ip=10.42.1.9")), "yes");
        assert_eq!(gate_answer(Some("ip=10.42.0.5")), "no");

        // A gone pod's address leaves with the next new one, in one write.
        pods_api.delete("pyo-tunnel-b", &DeleteParams::default()).await.unwrap();
        follow_pods(&client).await.unwrap();
        assert_eq!(trusted_in(&kube), ["10.42.1.9/32"]);
        kube.put(&pod_path("pyo-tunnel-c"), tunnel_pod("10.42.0.7"));
        follow_pods(&client).await.unwrap();
        assert_eq!(trusted_in(&kube), ["10.42.0.7/32"]);
        // The last one gone, and due: nobody, never anyone.
        pods_api.delete("pyo-tunnel-c", &DeleteParams::default()).await.unwrap();
        follow_pods(&client).await.unwrap();
        kept().left_at = Some(ago(REMOVE_AFTER));
        assert_eq!(follow_pods(&client).await.unwrap(), RESYNC);
        assert_eq!(trusted_in(&kube), ["0.0.0.0/32"]);
        assert_eq!(gate_answer(Some("ip=10.42.0.7")), "no");
    }

    /// A policy found without the connectors' addresses (a deploy wrote it
    /// with the list as it was a moment before) gets them back, and the
    /// gate waits for that. A write that fails is tried again.
    #[tokio::test]
    async fn a_policy_without_the_addresses_gets_them_back() {
        let _one = SERIAL.lock().await;
        let (kube, client) = FakeKube::start().await;
        reset();
        kube.put(POLICY, tunnel_policy(&["0.0.0.0/32"], 1, 1));
        kube.put(&pod_path("pyo-tunnel-a"), tunnel_pod("10.42.0.5"));
        // The write fails: nothing is taken as written.
        kube.admit(|w| (w.plural == "clienttrafficpolicies").then(|| (500, "etcd timed out".into())));
        assert!(follow_pods(&client).await.is_err());
        assert!(trusted_now().is_empty());
        kube.admit(|_| None);
        follow_pods(&client).await.unwrap();
        answer(&kube);
        follow_pods(&client).await.unwrap();
        kept().taken_at = Some(ago(SETTLE));
        follow_pods(&client).await.unwrap();
        assert_eq!(gate_answer(Some("ip=10.42.0.5")), "yes");

        // A deploy's write, with a CDN's range and without the pod.
        kube.put(POLICY, tunnel_policy(&["173.245.48.0/20"], 8, 8));
        assert_eq!(follow_pods(&client).await.unwrap(), QUICK);
        assert_eq!(gate_answer(Some("ip=10.42.0.5")), "no");
        assert_eq!(trusted_in(&kube), ["10.42.0.5/32", "173.245.48.0/20"]);
        // Not rewritten on every look: once, then after a while.
        kube.put(POLICY, tunnel_policy(&["173.245.48.0/20"], 9, 9));
        follow_pods(&client).await.unwrap();
        assert_eq!(trusted_in(&kube), ["173.245.48.0/20"]);
        kept().repaired_at = Some(ago(REPAIR_EVERY));
        follow_pods(&client).await.unwrap();
        assert_eq!(trusted_in(&kube), ["10.42.0.5/32", "173.245.48.0/20"]);
        answer(&kube);
        follow_pods(&client).await.unwrap();
        kept().taken_at = Some(ago(SETTLE));
        assert_eq!(follow_pods(&client).await.unwrap(), RESYNC);
        assert_eq!(gate_answer(Some("ip=10.42.0.5")), "yes");
    }

    /// With no tunnel domain there's no policy to wait for: a connector's
    /// address is in effect after the proxies' time.
    #[tokio::test]
    async fn without_tunnel_policies_the_gate_opens() {
        let _one = SERIAL.lock().await;
        let (kube, client) = FakeKube::start().await;
        reset();
        kube.put(&pod_path("pyo-tunnel-a"), tunnel_pod("10.42.0.5"));
        assert_eq!(follow_pods(&client).await.unwrap(), QUICK);
        assert_eq!(gate_answer(Some("ip=10.42.0.5")), "no");
        kept().taken_at = Some(ago(SETTLE));
        assert_eq!(follow_pods(&client).await.unwrap(), RESYNC);
        assert_eq!(gate_answer(Some("ip=10.42.0.5")), "yes");
    }

    /// The gate's answer: yes only when every address asked for is in
    /// effect, whatever else the question holds.
    #[tokio::test]
    async fn the_gate_says_yes_only_for_addresses_in_effect() {
        let _one = SERIAL.lock().await;
        reset();
        kept().in_effect = vec!["10.42.0.5/32".into(), "fd00::5/128".into()];
        for (question, answer) in [
            (Some("ip=10.42.0.5"), "yes"),
            (Some("ip=10.42.0.5,fd00::5"), "yes"),
            (Some("ip=10.42.0.5,fd00:0:0::5"), "yes"),
            (Some("x=1&ip=fd00::5"), "yes"),
            (Some("ip=10.42.0.5,10.42.0.6"), "no"),
            (Some("ip=10.42.0.6"), "no"),
            (Some("ip=10.42.0.5,"), "yes"),
            (Some("ip=10.42.0.5,nonsense"), "no"),
            (Some("ip=10.42.0.5/32"), "no"),
            (Some("ip=0.0.0.0/0"), "no"),
            (Some("ip="), "no"),
            (Some("ips=10.42.0.5"), "no"),
            (Some(""), "no"),
            (None, "no"),
        ] {
            assert_eq!(gate_answer(question), answer, "{question:?}");
        }
    }

    /// The init container's side: it asks until the agent says yes, and
    /// gives up in time when the agent says no, isn't there, or answers
    /// something else.
    #[tokio::test]
    async fn the_gate_waits_for_yes_and_gives_up_in_time() {
        let _one = SERIAL.lock().await;
        reset();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap().to_string();
        tokio::spawn(async move {
            let _ = axum::serve(listener, gate_router()).await;
        });
        let (total, every) = (Duration::from_millis(400), Duration::from_millis(40));
        assert!(!ask(&address, "10.42.0.5").await.unwrap());
        // Not in effect: asked until the time is up, then no.
        let started = Instant::now();
        assert!(!wait_trusted(&address, "10.42.0.5", total, every).await);
        assert!(started.elapsed() >= total - every && started.elapsed() < total * 5);
        // In effect a moment later: yes, well before the time is up.
        let later = tokio::spawn(async {
            tokio::time::sleep(Duration::from_millis(100)).await;
            kept().in_effect = vec!["10.42.0.5/32".into()];
        });
        let started = Instant::now();
        assert!(wait_trusted(&address, "10.42.0.5", Duration::from_secs(20), every).await);
        assert!(started.elapsed() < Duration::from_secs(10));
        later.await.unwrap();
        assert!(ask(&address, "10.42.0.5").await.unwrap());
        // Another path answers 404: an error, never a yes.
        let other = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let other_address = other.local_addr().unwrap().to_string();
        tokio::spawn(async move {
            let _ = axum::serve(other, axum::Router::new()).await;
        });
        assert!(ask(&other_address, "10.42.0.5").await.is_err());
        // No agent there: given up in time, and it's a no.
        let gone = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let gone_address = gone.local_addr().unwrap().to_string();
        drop(gone);
        assert!(ask(&gone_address, "10.42.0.5").await.is_err());
        let started = Instant::now();
        assert!(!wait_trusted(&gone_address, "10.42.0.5", total, every).await);
        assert!(started.elapsed() < total * 5);
    }

    #[test]
    fn pods_report_their_server_and_why_they_wait() {
        let pod: Pod = serde_json::from_value(json!({
            "metadata": { "name": "pyo-tunnel-1" },
            "spec": { "nodeName": "server-2", "containers": [] },
            "status": {
                "podIP": "10.42.1.7", "podIPs": [{ "ip": "10.42.1.7" }, { "ip": "fd00::7" }],
                "conditions": [{ "type": "Ready", "status": "False" }],
                "containerStatuses": [{ "name": "cloudflared", "ready": false, "restartCount": 3, "image": "x", "imageID": "",
                    "state": { "waiting": { "reason": "ImagePullBackOff" } } }],
            },
        }))
        .unwrap();
        let r = pod_reports(std::slice::from_ref(&pod));
        assert_eq!(r[0].server, "server-2");
        assert!(!r[0].ready);
        assert_eq!(r[0].restarts, 3);
        assert_eq!(r[0].reason.as_deref(), Some(pyo_proto::CANT_DOWNLOAD_IMAGE));
        assert_eq!(pod_cidrs(&[pod]), ["10.42.1.7/32", "fd00::7/128"]);
        assert!(id_ok(ID) && !id_ok("x"));
    }

    #[test]
    fn pods_never_report_kubernetes_words() {
        let pod = |state: serde_json::Value| -> Pod {
            serde_json::from_value(json!({
                "metadata": { "name": "pyo-tunnel-1" },
                "spec": { "nodeName": "server-2", "containers": [] },
                "status": {
                    "conditions": [{ "type": "Ready", "status": "False" }],
                    "containerStatuses": [{ "name": "cloudflared", "ready": false, "restartCount": 0, "image": "x", "imageID": "",
                        "state": state }],
                },
            }))
            .unwrap()
        };
        let reason = |state| pod_reports(&[pod(state)])[0].reason.clone();
        assert_eq!(
            reason(json!({ "waiting": { "reason": "CreateContainerConfigError" } })).as_deref(),
            Some(pyo_proto::SETTING_MISSING)
        );
        assert_eq!(
            reason(json!({ "waiting": { "reason": "CrashLoopBackOff" } })).as_deref(),
            Some(pyo_proto::KEEPS_STOPPING)
        );
        assert_eq!(
            reason(json!({ "terminated": { "reason": "OOMKilled", "exitCode": 137 } })).as_deref(),
            Some(pyo_proto::NOT_RUNNING_YET)
        );
        // Still being made: nothing to say yet.
        assert_eq!(reason(json!({ "waiting": { "reason": "ContainerCreating" } })), None);
    }
}
