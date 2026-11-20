//! Names on the tailnet: Tailscale Services (pyo-proto's system.rs, "Names
//! on the tailnet").
//!
//! Two halves, in the same binary:
//!
//! - **The servers' tailnet pods** (`pyo-agent tailnet`, a DaemonSet on
//!   every server that runs a proxy, with tailscaled's socket). Each talks
//!   only to its server's tailscaled (LocalAPI): it advertises the
//!   Services the cluster hosts, each with a `Tun` serve entry (tailscaled
//!   hands the VIP's packets to the server, never proxies or terminates
//!   TLS), keeps Tailscale's certificate for the Services this server holds
//!   in their Secrets, and prints the server's Tailscale on a log line the
//!   agent reads ([`STATUS_MARK`]).
//! - **The agent**: hands them the Services to host (a ConfigMap), takes the
//!   VIPs to the tailnet listeners (a Service with them as externalIPs),
//!   keeps the tailnet Gateway's fixed objects and certificates as they are,
//!   and reports every server's Tailscale in the heartbeat.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use k8s_openapi::api::apps::v1::{DaemonSet, Deployment};
use k8s_openapi::api::core::v1::{ConfigMap, Pod, Secret, Service};
use kube::{
    Api, Client,
    api::{DeleteParams, ListParams, LogParams, Patch, PatchParams},
};
use pyo_proto::system;
use pyo_proto::{ServerTailnet, TailnetHosted, TailnetHosting, TailnetWanted};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const NS: &str = "pyo-system";
/// A tailnet pod's status line starts with this, then the JSON.
pub const STATUS_MARK: &str = "pyoTailnet ";
/// The socket inside the pod.
const SOCKET: &str = "/var/run/tailscale/tailscaled.sock";

fn pp() -> PatchParams {
    PatchParams::apply(crate::MANAGER).force()
}

// ── LocalAPI ─────────────────────────────────────────────────────────────────

/// One LocalAPI call over tailscaled's socket: HTTP/1.0, so the answer is
/// the rest of the stream (never chunked). Returns the status, headers
/// (lower-case names) and body.
async fn local(
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: Option<&[u8]>,
    timeout: Duration,
) -> anyhow::Result<(u16, BTreeMap<String, String>, Vec<u8>)> {
    let call = async {
        let mut s = tokio::net::UnixStream::connect(SOCKET).await?;
        let mut req = format!(
            "{method} {path} HTTP/1.0\r\nHost: local-tailscaled.sock\r\nSec-Tailscale: localapi\r\n"
        );
        for (k, v) in headers {
            req.push_str(&format!("{k}: {v}\r\n"));
        }
        if let Some(b) = body {
            req.push_str(&format!(
                "Content-Type: application/json\r\nContent-Length: {}\r\n",
                b.len()
            ));
        }
        req.push_str("\r\n");
        s.write_all(req.as_bytes()).await?;
        if let Some(b) = body {
            s.write_all(b).await?;
        }
        let mut out = Vec::new();
        s.read_to_end(&mut out).await?;
        parse_response(&out)
    };
    tokio::time::timeout(timeout, call)
        .await
        .map_err(|_| anyhow::anyhow!("tailscaled didn't answer {path} in time"))?
}

fn parse_response(raw: &[u8]) -> anyhow::Result<(u16, BTreeMap<String, String>, Vec<u8>)> {
    let end = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or_else(|| anyhow::anyhow!("tailscaled's answer has no headers"))?;
    let head = std::str::from_utf8(&raw[..end])?;
    let mut lines = head.split("\r\n");
    let status: u16 = lines
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|c| c.parse().ok())
        .ok_or_else(|| anyhow::anyhow!("tailscaled's answer has no status"))?;
    let headers = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_owned()))
        .collect();
    Ok((status, headers, raw[end + 4..].to_vec()))
}

async fn local_json(method: &str, path: &str, body: Option<&Value>) -> anyhow::Result<Value> {
    let b = body.map(serde_json::to_vec).transpose()?;
    let (status, _, out) = local(method, path, &[], b.as_deref(), Duration::from_secs(20)).await?;
    if !(200..300).contains(&status) {
        anyhow::bail!("{path}: {status} {}", String::from_utf8_lossy(&out).trim());
    }
    if out.is_empty() {
        return Ok(Value::Null);
    }
    Ok(serde_json::from_slice(&out)?)
}

/// "svc:blog" → "blog".
fn bare(svc: &str) -> &str {
    svc.strip_prefix("svc:").unwrap_or(svc)
}

/// The server's Tailscale from LocalAPI's status and Tailnet Lock's.
fn read_status(server: &str, st: &Value, tka: &Value, advertised: &[String]) -> ServerTailnet {
    let me = &st["Self"];
    let strings = |v: &Value| -> Vec<String> {
        v.as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default()
    };
    // Tailnet Lock on, this server's key not signed: the command to sign it.
    let lock_sign = (tka["Enabled"].as_bool() == Some(true)
        && tka["NodeKeySigned"].as_bool() == Some(false))
    .then(|| tka["NodeKey"].as_str().unwrap_or(""))
    .filter(|k| !k.is_empty())
    .map(|k| format!("tailscale lock sign {k}"));
    ServerTailnet {
        server: server.to_owned(),
        state: st["BackendState"].as_str().unwrap_or("").to_owned(),
        node_id: me["ID"].as_str().unwrap_or("").to_owned(),
        dns_name: me["DNSName"]
            .as_str()
            .unwrap_or("")
            .trim_end_matches('.')
            .to_owned(),
        suffix: st["CurrentTailnet"]["MagicDNSSuffix"]
            .as_str()
            .or(st["MagicDNSSuffix"].as_str())
            .unwrap_or("")
            .trim_end_matches('.')
            .to_owned(),
        tags: strings(&me["Tags"]),
        ips: strings(&st["TailscaleIPs"]),
        version: st["Version"]
            .as_str()
            .unwrap_or("")
            .split('-')
            .next()
            .unwrap_or("")
            .to_owned(),
        lock_sign,
        advertised: advertised.iter().map(|s| bare(s).to_owned()).collect(),
        peers: {
            let mut p: Vec<String> = st["Peer"]
                .as_object()
                .into_iter()
                .flat_map(|m| m.values())
                .filter_map(|p| {
                    p["DNSName"]
                        .as_str()?
                        .split('.')
                        .next()
                        .map(str::to_ascii_lowercase)
                })
                .filter(|n| !n.is_empty())
                .collect();
            p.sort();
            p.dedup();
            p
        },
        certificates: Vec::new(),
        error: None,
    }
}

/// The advertised Services and serve config as they should be: `wanted`
/// on, `retired` off; anything else (someone's own) as it is.
fn wanted_prefs(current: &[String], wanted: &[String], retired: &[String]) -> Vec<String> {
    let svc = |n: &str| format!("svc:{n}");
    let off: BTreeSet<String> = retired.iter().map(|n| svc(n)).collect();
    let mut out: Vec<String> = current
        .iter()
        .filter(|s| !off.contains(*s))
        .cloned()
        .collect();
    for n in wanted {
        let s = svc(n);
        if !out.contains(&s) {
            out.push(s);
        }
    }
    out
}

/// The serve config with a `Tun` entry for each wanted Service and none for
/// retired ones; None when it's already so.
fn wanted_serve(config: &Value, wanted: &[String], retired: &[String]) -> Option<Value> {
    let mut c = if config.is_object() {
        config.clone()
    } else {
        json!({})
    };
    let services = c
        .as_object_mut()?
        .entry("Services")
        .or_insert_with(|| json!({}));
    if !services.is_object() {
        *services = json!({});
    }
    let m = services.as_object_mut()?;
    let mut changed = false;
    for n in retired {
        changed |= m.remove(&format!("svc:{n}")).is_some();
    }
    for n in wanted {
        let k = format!("svc:{n}");
        if m.get(&k) != Some(&json!({ "Tun": true })) {
            m.insert(k, json!({ "Tun": true }));
            changed = true;
        }
    }
    if m.is_empty() {
        c.as_object_mut()?.remove("Services");
    }
    changed.then_some(c)
}

/// "-----BEGIN PRIVATE KEY-----…-----BEGIN CERTIFICATE-----…" (LocalAPI's
/// `type=pair`): the key, then the chain.
fn split_pair(pem: &str) -> Option<(String, String)> {
    let at = pem.find("-----BEGIN CERTIFICATE-----")?;
    let (key, cert) = pem.split_at(at);
    (key.contains("PRIVATE KEY") && !cert.trim().is_empty())
        .then(|| (key.trim().to_owned() + "\n", cert.trim().to_owned() + "\n"))
}

/// The servers' tailnet pod: until it's stopped.
pub async fn run_pod() -> anyhow::Result<()> {
    let server = std::env::var("PYO_NODE_NAME").unwrap_or_default();
    let kube = Client::try_default().await?;
    let mut last_line = String::new();
    let mut last_print = std::time::Instant::now();
    // The certificate last written to each Service's Secret.
    let mut written: BTreeMap<String, String> = BTreeMap::new();
    let mut last_cert_check: BTreeMap<String, std::time::Instant> = BTreeMap::new();
    loop {
        let st = round(&kube, &server, &mut written, &mut last_cert_check).await;
        let line = serde_json::to_string(&st).unwrap_or_default();
        // A line when anything changes, and every few minutes (the agent
        // reads the last one).
        if line != last_line || last_print.elapsed() > Duration::from_secs(300) {
            println!("{STATUS_MARK}{line}");
            last_line = line;
            last_print = std::time::Instant::now();
        }
        tokio::time::sleep(Duration::from_secs(20)).await;
    }
}

/// One pass: read the server's Tailscale, host what's wanted, keep the
/// certificates this server holds.
async fn round(
    kube: &Client,
    server: &str,
    written: &mut BTreeMap<String, String>,
    last_check: &mut BTreeMap<String, std::time::Instant>,
) -> ServerTailnet {
    let st = match local_json("GET", "/localapi/v0/status?peers=false", None).await {
        Ok(v) => v,
        Err(e) => {
            return ServerTailnet {
                server: server.to_owned(),
                error: Some(format!("{e:#}")),
                ..Default::default()
            };
        }
    };
    let tka = local_json("GET", "/localapi/v0/tka/status", None)
        .await
        .unwrap_or(Value::Null);
    let prefs = local_json("GET", "/localapi/v0/prefs", None)
        .await
        .unwrap_or(Value::Null);
    let current: Vec<String> = prefs["AdvertiseServices"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();
    let mut out = read_status(server, &st, &tka, &current);
    if out.state != "Running" {
        return out;
    }
    let hosting = match wanted_hosting(kube).await {
        Ok(h) => h,
        Err(e) => {
            out.error = Some(format!("couldn't read the Services to host: {e:#}"));
            return out;
        }
    };
    let wanted: Vec<String> = hosting.services.iter().map(|s| s.name.clone()).collect();
    let mut errors = Vec::new();
    // Serve entries first, then advertise: a Service is only advertised
    // once the server takes its packets.
    match local(
        "GET",
        "/localapi/v0/serve-config",
        &[],
        None,
        Duration::from_secs(20),
    )
    .await
    {
        Ok((200, h, body)) => {
            let config: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
            if let Some(next) = wanted_serve(&config, &wanted, &hosting.retired) {
                let etag = h.get("etag").cloned().unwrap_or_default();
                let b = serde_json::to_vec(&next).unwrap_or_default();
                match local(
                    "POST",
                    "/localapi/v0/serve-config",
                    &[("If-Match", &etag)],
                    Some(&b),
                    Duration::from_secs(20),
                )
                .await
                {
                    Ok((200, _, _)) => {}
                    Ok((code, _, body)) => errors.push(format!(
                        "serve config: {code} {}",
                        String::from_utf8_lossy(&body).trim()
                    )),
                    Err(e) => errors.push(format!("serve config: {e:#}")),
                }
            }
        }
        Ok((code, _, body)) => errors.push(format!(
            "serve config: {code} {}",
            String::from_utf8_lossy(&body).trim()
        )),
        Err(e) => errors.push(format!("serve config: {e:#}")),
    }
    let next = wanted_prefs(&current, &wanted, &hosting.retired);
    if next != current {
        let patch = json!({ "AdvertiseServicesSet": true, "AdvertiseServices": next });
        match local_json("PATCH", "/localapi/v0/prefs", Some(&patch)).await {
            Ok(_) => out.advertised = next.iter().map(|s| bare(s).to_owned()).collect(),
            Err(e) => errors.push(format!("advertising: {e:#}")),
        }
    }
    // Certificates this server holds: hourly, or at once when new. LocalAPI
    // hands back tailscaled's copy until it's due for renewal, so this
    // asks Let's Encrypt only when needed.
    let secrets: Api<Secret> = Api::namespaced(kube.clone(), system::GATEWAY_NS);
    for s in hosting.services.iter().filter(|s| s.cert_server == server) {
        let due = last_check
            .get(&s.name)
            .is_none_or(|t| t.elapsed() > Duration::from_secs(3600))
            || !written.contains_key(&s.name);
        if due {
            last_check.insert(s.name.clone(), std::time::Instant::now());
            match local(
                "GET",
                &format!("/localapi/v0/cert/{}?type=pair", s.host),
                &[],
                None,
                Duration::from_secs(300),
            )
            .await
            {
                Ok((200, _, body)) => match split_pair(&String::from_utf8_lossy(&body)) {
                    Some((key, cert)) if written.get(&s.name) != Some(&cert) => {
                        let secret = json!({
                            "apiVersion": "v1", "kind": "Secret", "type": "kubernetes.io/tls",
                            "metadata": { "name": system::tailnet_tls_secret(&s.name), "labels": { "app.kubernetes.io/managed-by": "pyo", "pyo.dev/tailnet-service": s.name } },
                            "stringData": { "tls.crt": cert, "tls.key": key },
                        });
                        match secrets
                            .patch(
                                &system::tailnet_tls_secret(&s.name),
                                &pp(),
                                &Patch::Apply(&secret),
                            )
                            .await
                        {
                            Ok(_) => {
                                tracing::info!(service = %s.name, "certificate kept");
                                written.insert(s.name.clone(), cert);
                            }
                            Err(e) => errors
                                .push(format!("{}: couldn't keep its certificate: {e}", s.name)),
                        }
                    }
                    Some(_) => {}
                    None => errors.push(format!("{}: Tailscale's certificate didn't read", s.name)),
                },
                Ok((code, _, body)) => {
                    errors.push(format!(
                        "{}: certificate: {code} {}",
                        s.name,
                        String::from_utf8_lossy(&body).trim()
                    ));
                    // Not this server's yet (approval on its way): ask again soon.
                    last_check.remove(&s.name);
                }
                Err(e) => {
                    errors.push(format!("{}: certificate: {e:#}", s.name));
                    last_check.remove(&s.name);
                }
            }
        }
    }
    // Certificates of Services this server no longer holds: their Secrets go.
    let keep: BTreeSet<&str> = hosting
        .services
        .iter()
        .filter(|s| s.cert_server == server)
        .map(|s| s.name.as_str())
        .collect();
    let gone: Vec<String> = written
        .keys()
        .filter(|n| !keep.contains(n.as_str()))
        .cloned()
        .collect();
    for n in gone {
        written.remove(&n);
        last_check.remove(&n);
        // Another server may hold it now: only Services retired everywhere.
        if hosting.retired.contains(&n) {
            let _ = secrets
                .delete(&system::tailnet_tls_secret(&n), &DeleteParams::default())
                .await;
        }
    }
    out.certificates = written.keys().cloned().collect();
    if !errors.is_empty() {
        out.error = Some(errors.join("; "));
    }
    out
}

async fn wanted_hosting(kube: &Client) -> anyhow::Result<TailnetHosting> {
    let cm = Api::<ConfigMap>::namespaced(kube.clone(), system::GATEWAY_NS)
        .get_opt(system::TAILNET_WANTED)
        .await?;
    let Some(text) = cm
        .and_then(|c| c.data)
        .and_then(|d| d.get(system::TAILNET_WANTED_KEY).cloned())
    else {
        return Ok(TailnetHosting::default());
    };
    Ok(serde_json::from_str(&text)?)
}

// ── The agent's half ────────────────────────────────────────────────────────

/// The servers' Tailscale, from their tailnet pods' last status lines.
pub async fn statuses(kube: &Client) -> Vec<ServerTailnet> {
    let pods: Api<Pod> = Api::namespaced(kube.clone(), NS);
    let lp = ListParams::default().labels(&format!(
        "app.kubernetes.io/name={}",
        system::TAILNET_DAEMONSET
    ));
    let Ok(list) = pods.list(&lp).await else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for p in list.items {
        let Some(name) = p.metadata.name.clone() else {
            continue;
        };
        let server = p
            .spec
            .as_ref()
            .and_then(|s| s.node_name.clone())
            .unwrap_or_default();
        let logs = pods
            .logs(
                &name,
                &LogParams {
                    tail_lines: Some(400),
                    ..Default::default()
                },
            )
            .await
            .unwrap_or_default();
        let last = logs
            .lines()
            .rev()
            .find_map(|l| l.split_once(STATUS_MARK).map(|(_, j)| j))
            .and_then(|j| serde_json::from_str::<ServerTailnet>(j).ok());
        out.push(last.unwrap_or(ServerTailnet {
            server,
            error: Some("Its tailnet pod hasn't reported yet.".into()),
            ..Default::default()
        }));
    }
    out.sort_by(|a, b| a.server.cmp(&b.server));
    out
}

/// What was last applied (so a heartbeat that changes nothing writes nothing).
static APPLIED: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

/// The server that keeps a Service's certificate: the first by name among
/// those whose tailnet pod runs, so it stays put while that server does.
fn cert_server(statuses: &[ServerTailnet]) -> String {
    statuses
        .iter()
        .filter(|s| s.state == "Running")
        .map(|s| s.server.clone())
        .min()
        .unwrap_or_default()
}

/// Everything the agent applies for the tailnet, from the API's wanted
/// list (`None`: nothing heard yet, so only the fixed objects) and the
/// servers' statuses.
pub fn objects(
    wanted: &TailnetWanted,
    statuses: &[ServerTailnet],
    servers: &[String],
    own_names: bool,
) -> (Vec<Value>, Option<Value>, TailnetHosting) {
    let holder = cert_server(statuses);
    let hosting = TailnetHosting {
        services: wanted
            .services
            .iter()
            .map(|s| TailnetHosted {
                name: s.name.clone(),
                host: wanted.host(&s.name),
                cert_server: holder.clone(),
            })
            .collect(),
        retired: wanted.retired.clone(),
    };
    // Certificates that are there: the ones a server says it keeps, for
    // Services still wanted.
    let wanted_names: BTreeSet<&str> = wanted.services.iter().map(|s| s.name.as_str()).collect();
    let mut certs: Vec<String> = statuses
        .iter()
        .flat_map(|s| s.certificates.iter())
        .filter(|n| wanted_names.contains(n.as_str()))
        .map(|n| system::tailnet_tls_secret(n))
        .collect();
    certs.sort();
    certs.dedup();
    let own = if own_names {
        wanted.own.clone()
    } else {
        Default::default()
    };
    let mut objs = system::tailnet_objects(servers, &certs, &own);
    objs.push(json!({
        "apiVersion": "v1", "kind": "ConfigMap",
        "metadata": { "name": system::TAILNET_WANTED, "namespace": system::GATEWAY_NS, "labels": { "app.kubernetes.io/managed-by": "pyo" } },
        "data": { system::TAILNET_WANTED_KEY: serde_json::to_string(&hosting).unwrap_or_default() },
    }));
    let addrs: Vec<String> = wanted
        .services
        .iter()
        .flat_map(|s| s.addrs.iter().cloned())
        .collect();
    let vip = (!addrs.is_empty()).then(|| system::tailnet_vip_service(&addrs));
    (objs, vip, hosting)
}

/// Apply them: after each heartbeat reply.
pub async fn apply(kube: &Client, wanted: &TailnetWanted, same_cluster: bool) {
    let statuses = statuses(kube).await;
    let servers = match crate::gateway::server_cidrs(kube).await {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(error = %e, "couldn't list the servers for the tailnet");
            return;
        }
    };
    let (objs, vip, _) = objects(wanted, &statuses, &servers, same_cluster);
    let image = own_image(kube).await;
    let key = serde_json::to_string(&(&objs, &vip, &image)).unwrap_or_default();
    if APPLIED
        .lock()
        .map(|a| a.as_deref() == Some(key.as_str()))
        .unwrap_or(false)
    {
        return;
    }
    // Refused by the cluster's install as it still is: not tried (and
    // logged) again each heartbeat, only once that install or this changes.
    let refused_before = TAILNET_REFUSED
        .lock()
        .ok()
        .and_then(|r| r.clone())
        .filter(|(k, _)| *k == key);
    if let Some((_, stamp)) = refused_before
        && stamp == crate::refused::install_stamp(kube).await
    {
        return;
    }
    let mut ok = true;
    let mut refused = false;
    // The servers' tailnet pods, on the agent's own image.
    if let Some(image) = &image {
        let ds = system::tailnet_daemonset(image);
        if let Err(e) = Api::<DaemonSet>::namespaced(kube.clone(), NS)
            .patch(system::TAILNET_DAEMONSET, &pp(), &Patch::Apply(&ds))
            .await
        {
            tracing::warn!(error = %e, "couldn't apply the tailnet pods");
            refused |= crate::refused::is_refusal(&e);
            ok = false;
        }
    }
    for o in &objs {
        let kind = o["kind"].as_str().unwrap_or("");
        let name = o["metadata"]["name"].as_str().unwrap_or("");
        let r = if kind == "ConfigMap" {
            Api::<ConfigMap>::namespaced(kube.clone(), system::GATEWAY_NS)
                .patch(name, &pp(), &Patch::Apply(o))
                .await
                .map(|_| ())
        } else {
            crate::gateway::api_of_kind(kube, kind)
                .patch(name, &pp(), &Patch::Apply(o))
                .await
                .map(|_| ())
        };
        if let Err(e) = r {
            tracing::warn!(error = %e, kind, name, "couldn't apply a tailnet object");
            refused |= crate::refused::is_refusal(&e);
            ok = false;
        }
    }
    // Pyo's own routes, off a cluster that no longer has them.
    let own = if same_cluster {
        wanted.own.clone()
    } else {
        Default::default()
    };
    for ((name, _, _), on) in [
        (system::TAILNET_DASHBOARD, own.dashboard.is_some()),
        (system::TAILNET_REGISTRY, own.registry.is_some()),
        (system::TAILNET_SANDBOXES, own.sandboxes.is_some()),
    ] {
        if !on {
            for kind in ["HTTPRoute", "Backend"] {
                let _ = crate::gateway::api_of_kind(kube, kind)
                    .delete(name, &DeleteParams::default())
                    .await;
            }
        }
    }
    let services: Api<Service> = Api::namespaced(kube.clone(), system::GATEWAY_NS);
    let r = match &vip {
        Some(v) => services
            .patch(system::TAILNET_VIP_SERVICE, &pp(), &Patch::Apply(v))
            .await
            .map(|_| ()),
        None => match services
            .delete(system::TAILNET_VIP_SERVICE, &DeleteParams::default())
            .await
        {
            Ok(_) => Ok(()),
            Err(kube::Error::Api(e)) if e.code == 404 => Ok(()),
            Err(e) => Err(e),
        },
    };
    if let Err(e) = r {
        tracing::warn!(error = %e, "couldn't apply the tailnet VIPs' Service");
        refused |= crate::refused::is_refusal(&e);
        ok = false;
    }
    if refused {
        tracing::warn!(
            "the cluster's install refuses the tailnet's objects; run the cluster's connect command (tried again once the install changes)"
        );
        let stamp = crate::refused::install_stamp(kube).await;
        if let Ok(mut r) = TAILNET_REFUSED.lock() {
            *r = Some((key, stamp));
        }
    } else if ok && let Ok(mut a) = APPLIED.lock() {
        *a = Some(key);
    }
}

/// What the cluster's install refused last, and that install's stamp.
static TAILNET_REFUSED: std::sync::Mutex<Option<(String, String)>> = std::sync::Mutex::new(None);

/// The agent's own image (its Deployment's), for its tailnet pods and
/// the traffic store.
pub(crate) async fn own_image(kube: &Client) -> Option<String> {
    Api::<Deployment>::namespaced(kube.clone(), NS)
        .get_opt("pyo-agent")
        .await
        .ok()
        .flatten()?
        .spec?
        .template
        .spec?
        .containers
        .into_iter()
        .find(|c| c.name == "agent")?
        .image
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn localapi_answers_parse() {
        let raw =
            b"HTTP/1.0 200 OK\r\nEtag: \"abc\"\r\nContent-Type: application/json\r\n\r\n{\"a\":1}";
        let (code, h, body) = parse_response(raw).unwrap();
        assert_eq!(code, 200);
        assert_eq!(h["etag"], "\"abc\"");
        assert_eq!(body, b"{\"a\":1}");
        assert!(parse_response(b"garbage").is_err());
    }

    /// Only Pyo's Services move: someone's own stay advertised and served.
    #[test]
    fn hosting_leaves_others_alone() {
        let cur = vec!["svc:mine".to_owned(), "svc:old".to_owned()];
        assert_eq!(
            wanted_prefs(&cur, &["blog".into(), "pyo".into()], &["old".into()]),
            ["svc:mine", "svc:blog", "svc:pyo"]
        );
        let config = json!({ "TCP": { "22": { "TCPForward": "x" } }, "Services": { "svc:mine": { "TCP": {} }, "svc:old": { "Tun": true } } });
        let next = wanted_serve(&config, &["blog".into()], &["old".into()]).unwrap();
        assert_eq!(next["TCP"], config["TCP"]);
        assert_eq!(
            next["Services"],
            json!({ "svc:mine": { "TCP": {} }, "svc:blog": { "Tun": true } })
        );
        assert_eq!(
            wanted_serve(&next, &["blog".into()], &["old".into()]),
            None,
            "nothing to change"
        );
        let empty = wanted_serve(
            &json!({ "Services": { "svc:old": { "Tun": true } } }),
            &[],
            &["old".into()],
        )
        .unwrap();
        assert_eq!(empty, json!({}));
    }

    #[test]
    fn status_and_lock_command() {
        let st = json!({
            "BackendState": "Running", "Version": "1.90.1-tabc", "TailscaleIPs": ["100.64.0.2", "fd7a:115c:a1e0::2"],
            "CurrentTailnet": { "MagicDNSSuffix": "tail1234.ts.net" },
            "Self": { "ID": "nABC123CNTRL", "DNSName": "server-1.tail1234.ts.net.", "Tags": ["tag:pyo"] },
        });
        let tka = json!({ "Enabled": true, "NodeKeySigned": false, "NodeKey": "nodekey:abcd" });
        let s = read_status("server-1", &st, &tka, &["svc:pyo".into()]);
        assert_eq!(s.node_id, "nABC123CNTRL");
        assert_eq!(s.dns_name, "server-1.tail1234.ts.net");
        assert_eq!(s.suffix, "tail1234.ts.net");
        assert_eq!(s.version, "1.90.1");
        assert_eq!(s.tags, ["tag:pyo"]);
        assert_eq!(s.advertised, ["pyo"]);
        assert_eq!(
            s.lock_sign.as_deref(),
            Some("tailscale lock sign nodekey:abcd")
        );
        let signed = json!({ "Enabled": true, "NodeKeySigned": true, "NodeKey": "nodekey:abcd" });
        assert_eq!(read_status("s", &st, &signed, &[]).lock_sign, None);
        assert_eq!(read_status("s", &st, &Value::Null, &[]).lock_sign, None);
    }

    #[test]
    fn pairs_split_into_key_and_chain() {
        let pem = "-----BEGIN EC PRIVATE KEY-----\nk\n-----END EC PRIVATE KEY-----\n-----BEGIN CERTIFICATE-----\nc\n-----END CERTIFICATE-----\n-----BEGIN CERTIFICATE-----\ni\n-----END CERTIFICATE-----\n";
        let (key, cert) = split_pair(pem).unwrap();
        assert!(
            key.starts_with("-----BEGIN EC PRIVATE KEY-----")
                && key.ends_with("-----END EC PRIVATE KEY-----\n")
        );
        assert!(
            cert.starts_with("-----BEGIN CERTIFICATE-----")
                && cert.matches("BEGIN CERTIFICATE").count() == 2
        );
        assert_eq!(split_pair("-----BEGIN CERTIFICATE-----\nc\n"), None);
    }

    /// The VIPs go to the listeners, a certificate is listed once a server
    /// keeps it, and one server keeps them all.
    #[test]
    fn wanted_becomes_objects() {
        let wanted = TailnetWanted {
            suffix: "tail1234.ts.net".into(),
            services: vec![
                pyo_proto::TailnetService {
                    name: "pyo".into(),
                    addrs: vec!["100.100.0.1".into(), "fd7a:115c:a1e0::a".into()],
                },
                pyo_proto::TailnetService {
                    name: "blog".into(),
                    addrs: vec!["100.100.0.2".into()],
                },
            ],
            retired: vec!["old".into()],
            own: system::TailnetOwn {
                dashboard: Some("pyo.tail1234.ts.net".into()),
                registry: None,
                sandboxes: None,
            },
        };
        let statuses = vec![
            ServerTailnet {
                server: "b".into(),
                state: "Running".into(),
                certificates: vec!["pyo".into(), "gone".into()],
                ..Default::default()
            },
            ServerTailnet {
                server: "a".into(),
                state: "NeedsLogin".into(),
                ..Default::default()
            },
            ServerTailnet {
                server: "c".into(),
                state: "Running".into(),
                ..Default::default()
            },
        ];
        let (objs, vip, hosting) = objects(&wanted, &statuses, &[], true);
        assert_eq!(
            hosting
                .services
                .iter()
                .map(|s| s.cert_server.as_str())
                .collect::<Vec<_>>(),
            ["b", "b"]
        );
        assert_eq!(hosting.services[1].host, "blog.tail1234.ts.net");
        assert_eq!(hosting.retired, ["old"]);
        let gw = objs.iter().find(|o| o["kind"] == "Gateway").unwrap();
        assert_eq!(
            gw["spec"]["listeners"][1]["tls"]["certificateRefs"],
            json!([{ "group": "", "kind": "Secret", "name": "pyo-tls--pyo" }])
        );
        assert_eq!(
            vip.unwrap()["spec"]["externalIPs"],
            json!(["100.100.0.1", "fd7a:115c:a1e0::a", "100.100.0.2"])
        );
        assert!(
            objs.iter()
                .any(|o| o["metadata"]["name"] == system::TAILNET_DASHBOARD.0
                    && o["kind"] == "HTTPRoute")
        );
        // Another cluster: no routes of Pyo's own.
        let (objs, _, _) = objects(&wanted, &statuses, &[], false);
        assert!(
            !objs
                .iter()
                .any(|o| o["metadata"]["name"] == system::TAILNET_DASHBOARD.0)
        );
        // Nothing hosted: no VIPs' Service.
        let (_, vip, _) = objects(&TailnetWanted::default(), &statuses, &[], false);
        assert!(vip.is_none());
    }
}
