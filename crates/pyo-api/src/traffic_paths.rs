//! A cluster's traffic paths (docs/traffic-and-gateway-design.md §4 to
//! §9): Public ports, Cloudflare Tunnel and Tailnet, each on or off, and
//! the path each domain takes.
//!
//! - Public ports on is `clusters.traffic = 'public'`.
//! - Cloudflare Tunnel on is a `cluster_tunnels` row that isn't `removing`.
//! - Tailnet on is `clusters.tailnet_web`.
//!
//! `traffic` keeps deciding where free domains point (freedomain.rs), and
//! follows the switches: Public on → `public`, else Tunnel on → `tunnel`,
//! else `tailscale`.
//!
//! Turning a path off never cuts a domain the user didn't see listed: with
//! domains on that path, the op answers "confirm" with them first. Public
//! ports that close while domains move to the tunnel close only once every
//! one of them is on it (cloudflare/tunnel_loop.rs `close_public`). A
//! tunnel turned off while public ports are on is removed only once its
//! domains are back on public ports (`remove_when_back`).

use serde_json::{Value, json};
use sqlx::{Postgres, Transaction};

use crate::{
    AppState,
    auth::User,
    cloudflare::tunnel_ops,
    error::{ApiError, ApiResult},
    ops::{Role, log_activity, require_role, team_of},
};

/// The heartbeat reply's `publicPorts` and `tailnetWeb`. TCP and UDP ports
/// on the servers: with public traffic, else as the cluster's setting says,
/// else (a cluster nobody changed since) in tunnel mode while the tunnel
/// keeps them, as before.
pub const REPLY_PORTS: &str = "SELECT c.traffic = 'public' OR coalesce(c.public_ports, c.traffic = 'tunnel' AND coalesce(t.ports_public, true)), c.tailnet_web
   FROM pyo.clusters c LEFT JOIN pyo.cluster_tunnels t ON t.cluster_id = c.id WHERE c.id = $1";

/// Said when the last path would go off.
pub const ONE_PATH: &str = "Turn on another path first. A cluster needs one.";

/// The path a domain takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Path {
    Public,
    Tunnel,
    Tailnet,
}

impl Path {
    pub fn as_str(self) -> &'static str {
        match self {
            Path::Public => "public",
            Path::Tunnel => "tunnel",
            Path::Tailnet => "tailnet",
        }
    }
}

/// Which paths are on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Paths {
    pub public: bool,
    pub tunnel: bool,
    pub tailnet: bool,
}

impl Paths {
    pub fn any(self) -> bool {
        self.public || self.tunnel || self.tailnet
    }

    pub fn on(self, p: Path) -> bool {
        match p {
            Path::Public => self.public,
            Path::Tunnel => self.tunnel,
            Path::Tailnet => self.tailnet,
        }
    }

    /// `clusters.traffic` for these switches.
    pub fn traffic(self) -> &'static str {
        if self.public {
            "public"
        } else if self.tunnel {
            "tunnel"
        } else {
            "tailscale"
        }
    }
}

/// A domain's path, from what it is: a ts.net name or a free name on a
/// Tailscale address goes on the tailnet, one on the tunnel (or one level
/// under its default domain) through the tunnel, everything else on
/// public ports.
pub fn domain_path(d: &Value, default_domain: Option<&str>) -> Path {
    let host = d["hostname"].as_str().unwrap_or("").to_ascii_lowercase();
    if pyo_proto::system::is_tailnet_host(&host) {
        return Path::Tailnet;
    }
    if let Some(ip) = crate::freedomain::ip_in(&host) {
        return if crate::freedomain::tailnet_v4(&ip.to_string()).is_some() {
            Path::Tailnet
        } else {
            Path::Public
        };
    }
    if d["tunnel"].is_string()
        || default_domain.is_some_and(|dd| crate::cloudflare::tunnel_loop::under_default(&host, dd))
    {
        return Path::Tunnel;
    }
    Path::Public
}

/// Whether switching Tailnet changes the tailnet Gateway's HTTPS listener
/// on the cluster Pyo runs on: services' ts.net routes carry their route
/// policies (a rate limit). Before the Gateway section's release Pyo's own
/// routes there carried none, so with every service's ts.net route gone,
/// or back, the listener's filters changed, and open connections to Pyo's
/// dashboard on the tailnet dropped once. Now Pyo's own routes carry the
/// same policy, and the listener has an anchor route that always does
/// (gateway_settings.rs): once the cluster's agent writes them and its
/// install takes them (`refreshed`), nothing drops. On other clusters that
/// listener only has services' names, which stop anyway.
pub fn dashboard_drops(same_cluster: bool, refreshed: bool, domains: &[Domain]) -> bool {
    same_cluster && !refreshed && domains.iter().any(|d| pyo_proto::system::is_tailnet_host(&d.hostname))
}

/// Whether the cluster's agent writes Pyo's own tailnet routes with their
/// policy, and its install takes them: the Gateway section's agent, with
/// the install current.
pub fn refreshed(capabilities: &Value) -> bool {
    crate::gateway_settings::agent_ready(capabilities).is_ok()
}

/// A domain listed in a confirm answer.
#[derive(Debug, Clone, PartialEq)]
pub struct Listed {
    pub service_id: String,
    pub service: String,
    pub domain_id: String,
    pub hostname: String,
    pub path: Path,
    /// "moves" (to the tunnel, or back to public ports) or "stops".
    pub outcome: &'static str,
}

impl Listed {
    fn json(&self) -> Value {
        json!({ "serviceId": self.service_id, "service": self.service, "domainId": self.domain_id,
                "hostname": self.hostname, "path": self.path.as_str(), "outcome": self.outcome })
    }
}

/// A cluster's domain, as the plan sees it.
#[derive(Debug, Clone)]
pub struct Domain {
    pub service_id: String,
    pub service: String,
    pub domain_id: String,
    pub hostname: String,
    pub path: Path,
    /// It can move to the tunnel: in a tunnel-ready zone of its account,
    /// not on it yet.
    pub movable: bool,
    /// One level under the tunnel's default domain: no direct record to
    /// go back to.
    pub under_default: bool,
    /// Switch back leaves it a record: one it replaced is kept, or the
    /// cluster has a public IP to point it at.
    pub can_go_back: bool,
}

/// What turning paths off does to the domains on them: the ones listed,
/// with what happens to each. `move_to_tunnel`: domains that can move to
/// the tunnel do, before public ports close.
pub fn plan(now: Paths, want: Paths, domains: &[Domain], move_to_tunnel: bool) -> Vec<Listed> {
    let mut out = vec![];
    for d in domains {
        if !now.on(d.path) || want.on(d.path) {
            continue;
        }
        let outcome = match d.path {
            Path::Public if want.tunnel && move_to_tunnel && d.movable => "moves",
            Path::Tunnel if want.public && !d.under_default && d.can_go_back => "moves",
            _ => "stops",
        };
        out.push(Listed {
            service_id: d.service_id.clone(),
            service: d.service.clone(),
            domain_id: d.domain_id.clone(),
            hostname: d.hostname.clone(),
            path: d.path,
            outcome,
        });
    }
    out
}

/// What `setClusterPaths` asks.
#[derive(Debug, Default, Clone)]
pub struct Ask {
    pub public: Option<bool>,
    pub tunnel: Option<bool>,
    pub tailnet: Option<bool>,
    pub public_ports: Option<bool>,
    pub proxy_protocol: Option<bool>,
    pub default_domain: Option<String>,
    pub account_id: Option<String>,
    pub move_to_tunnel: Option<bool>,
    pub restore_records: Option<bool>,
    pub confirm: bool,
}

/// A cluster as the switches see it.
#[derive(sqlx::FromRow, Debug)]
struct Row {
    name: String,
    traffic: String,
    tailnet_web: bool,
    closing: bool,
    capabilities: Value,
    status: String,
    web_proxy_protocol: bool,
    same_cluster: bool,
    tunnel_state: Option<String>,
    tunnel_account: Option<String>,
    default_domain: Option<String>,
    removing_when_back: bool,
}

impl Row {
    fn paths(&self) -> Paths {
        Paths {
            public: self.traffic == "public",
            tunnel: self.tunnel_state.as_deref().is_some_and(|s| s != "removing")
                && !self.removing_when_back,
            tailnet: self.tailnet_web,
        }
    }
}

/// The cluster's domains with their paths.
async fn domains(
    s: &AppState,
    tx: &mut Transaction<'_, Postgres>,
    team_id: &str,
    cluster_id: &str,
    default_domain: Option<&str>,
    tunnel_account: Option<&str>,
) -> ApiResult<Vec<Domain>> {
    let services: Vec<(String, String, Value)> = sqlx::query_as(
        "SELECT id, name, coalesce(spec->'domains', '[]') FROM pyo.services WHERE cluster_id = $1 AND NOT deleting ORDER BY name, id",
    )
    .bind(cluster_id)
    .fetch_all(&mut **tx)
    .await?;
    // The zones a domain could move to the tunnel through: only asked of
    // Cloudflare's stored list, never Cloudflare itself.
    let zones = match tunnel_account {
        Some(_) => crate::cloudflare::account(s, tx, team_id)
            .await
            .ok()
            .flatten()
            .map(|a| a.zones)
            .unwrap_or_default(),
        None => vec![],
    };
    // Switch back puts a kept record back, else points the name at the
    // cluster's public IP, else leaves it with no record.
    let kept: Vec<(String, String)> = sqlx::query_as(
        "SELECT r.service_id, r.domain_id FROM pyo.dns_records r JOIN pyo.services a ON a.id = r.service_id
          WHERE a.cluster_id = $1 AND jsonb_typeof(r.previous) = 'array' AND jsonb_array_length(r.previous) > 0",
    )
    .bind(cluster_id)
    .fetch_all(&mut **tx)
    .await?;
    let public_ip = crate::freedomain::load(tx, cluster_id)
        .await?
        .and_then(|(_, c)| c.public_ip())
        .is_some();
    let mut out = vec![];
    for (service_id, service, ds) in services {
        for d in ds.as_array().into_iter().flatten() {
            let hostname = d["hostname"].as_str().unwrap_or("").to_ascii_lowercase();
            let path = domain_path(d, default_domain);
            let domain_id = d["id"].as_str().unwrap_or("").to_owned();
            out.push(Domain {
                can_go_back: public_ip || kept.iter().any(|(s, k)| *s == service_id && *k == domain_id),
                movable: path == Path::Public
                    && tunnel_account.is_some_and(|a| tunnel_ops::can_tunnel(&hostname, &zones, a).is_some()),
                under_default: default_domain
                    .is_some_and(|dd| crate::cloudflare::tunnel_loop::under_default(&hostname, dd)),
                service_id: service_id.clone(),
                service: service.clone(),
                domain_id,
                hostname,
                path,
            });
        }
    }
    Ok(out)
}

fn bad(m: impl Into<String>) -> ApiError {
    ApiError::bad_request(m.into())
}

/// Whether the cluster's services have public TCP or UDP ports.
async fn has_ports(tx: &mut Transaction<'_, Postgres>, cluster_id: &str) -> ApiResult<bool> {
    Ok(sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM pyo.services a, jsonb_array_elements(CASE WHEN jsonb_typeof(a.spec->'ports') = 'array' THEN a.spec->'ports' ELSE '[]' END) p
                         WHERE a.cluster_id = $1 AND NOT a.deleting AND p->>'exposure' = 'loadbalancer')",
    )
    .bind(cluster_id)
    .fetch_one(&mut **tx)
    .await?)
}

/// `setClusterPaths`.
pub async fn set(
    s: &AppState,
    tx: &mut Transaction<'_, Postgres>,
    user: &User,
    cluster_id: &str,
    ask: Ask,
    wake: &mut Vec<String>,
) -> ApiResult<Value> {
    let team_id = team_of(tx, "clusters", cluster_id).await?;
    require_role(tx, user, &team_id, Role::Admin).await?;
    let row: Row = sqlx::query_as(
        "SELECT c.name, c.traffic, c.tailnet_web, c.close_public_after IS NOT NULL AS closing,
                coalesce(c.capabilities, '{}') AS capabilities, c.status, c.web_proxy_protocol, c.same_cluster,
                t.state AS tunnel_state, t.account_id AS tunnel_account, t.default_domain,
                t.remove_when_back IS NOT NULL AS removing_when_back
           FROM pyo.clusters c LEFT JOIN pyo.cluster_tunnels t ON t.cluster_id = c.id
          WHERE c.id = $1 FOR UPDATE OF c",
    )
    .bind(cluster_id)
    .fetch_one(&mut **tx)
    .await?;
    let name = row.name.clone();
    let now = row.paths();
    let want = Paths {
        public: ask.public.unwrap_or(now.public && !row.closing),
        tunnel: ask.tunnel.unwrap_or(now.tunnel),
        tailnet: ask.tailnet.unwrap_or(now.tailnet),
    };
    if !want.any() {
        return Err(bad(ONE_PATH));
    }
    if want.tunnel && row.tunnel_state.as_deref() == Some("removing") {
        return Err(ApiError::conflict("The tunnel is still being removed. Try again in a minute."));
    }
    // The tunnel's own checks come first, so a confirm never lists
    // domains for a change that can't happen.
    let tunnel_settings = ask.default_domain.is_some() || ask.account_id.is_some();
    if (want.tunnel && !now.tunnel) || (want.tunnel && tunnel_settings) {
        tunnel_ops::install_ready(&row.capabilities).map_err(bad)?;
        if crate::cloudflare::account(s, tx, &team_id).await?.is_none() {
            return Err(bad("Connect Cloudflare first."));
        }
    }
    if let Some(on) = ask.proxy_protocol
        && on != row.web_proxy_protocol
        && row.status != "connected"
        && row.status != "degraded"
    {
        return Err(ApiError::conflict(format!("{name}'s agent isn't connected. Try again once it is.")));
    }
    let move_to_tunnel = ask.move_to_tunnel.unwrap_or(true);
    // The tunnel's account when it's on or turning on: domains in it can move.
    let account = match (&row.tunnel_account, want.tunnel) {
        (Some(a), _) if now.tunnel => Some(a.clone()),
        (_, true) => {
            let a = crate::cloudflare::account(s, tx, &team_id).await?;
            a.and_then(|a| {
                tunnel_ops::pick_account(&a.zones, ask.account_id.as_deref(), ask.default_domain.as_deref()).ok()
            })
        }
        _ => None,
    };
    let all = domains(s, tx, &team_id, cluster_id, row.default_domain.as_deref(), account.as_deref()).await?;
    let listed = plan(now, want, &all, move_to_tunnel);
    if !listed.is_empty() && !ask.confirm {
        return Ok(json!({
            "ok": true, "status": "confirm",
            "domains": listed.iter().map(Listed::json).collect::<Vec<_>>(),
            "moving": listed.iter().filter(|l| l.path == Path::Public && l.outcome == "moves").count(),
            "publicDashboard": row.same_cluster && now.public && !want.public,
            "dashboardDrops": now.tailnet != want.tailnet && dashboard_drops(row.same_cluster, refreshed(&row.capabilities), &all),
            "hasPorts": has_ports(tx, cluster_id).await?,
        }));
    }

    let mut said: Vec<String> = vec![];
    // PROXY protocol: as setClusterWebProxyProtocol.
    if let Some(on) = ask.proxy_protocol
        && on != row.web_proxy_protocol
    {
        sqlx::query("UPDATE pyo.clusters SET web_proxy_protocol = $2 WHERE id = $1")
            .bind(cluster_id)
            .bind(on)
            .execute(&mut **tx)
            .await?;
        crate::deploy::enqueue(tx, cluster_id, pyo_proto::Command::SetWebProxyProtocol { on }).await?;
        said.push(if on { "accepts PROXY protocol".into() } else { "no longer reads PROXY protocol".into() });
    }
    if let Some(on) = ask.public_ports {
        sqlx::query("UPDATE pyo.clusters SET public_ports = $2 WHERE id = $1 AND public_ports IS DISTINCT FROM $2")
            .bind(cluster_id)
            .bind(on)
            .execute(&mut **tx)
            .await?;
    } else if now.public && !want.public {
        // Public ports go off with nothing said about TCP and UDP ports:
        // they keep what they'd have had before (published in tunnel mode
        // while the tunnel keeps them).
        sqlx::query(
            "UPDATE pyo.clusters c SET public_ports = coalesce(c.public_ports, $2 AND coalesce((SELECT t.ports_public FROM pyo.cluster_tunnels t WHERE t.cluster_id = c.id), true))
              WHERE c.id = $1",
        )
        .bind(cluster_id)
        .bind(want.tunnel)
        .execute(&mut **tx)
        .await?;
    }

    // Tunnel on: made (or its settings changed). No domain moves for it.
    let mut zones_account: Option<(Vec<crate::cloudflare::Zone>, String)> = None;
    if want.tunnel && (!now.tunnel || tunnel_settings) {
        // A tunnel turned off and back on before its domains were back:
        // it stays, and the domains already back stay on public ports.
        sqlx::query("UPDATE pyo.cluster_tunnels SET remove_when_back = NULL WHERE cluster_id = $1")
            .bind(cluster_id)
            .execute(&mut **tx)
            .await?;
        let default = ask.default_domain.clone().or_else(|| row.default_domain.clone());
        let ports: Option<bool> = sqlx::query_scalar("SELECT public_ports FROM pyo.clusters WHERE id = $1")
            .bind(cluster_id)
            .fetch_one(&mut **tx)
            .await?;
        let (a, account_id) = tunnel_ops::upsert_tunnel(
            s,
            tx,
            user,
            &team_id,
            cluster_id,
            &name,
            default.as_deref(),
            ask.account_id.as_deref(),
            Some(ports.unwrap_or(true)),
        )
        .await?;
        zones_account = Some((a.zones, account_id));
        if !now.tunnel {
            said.push("takes websites through Cloudflare Tunnel".into());
        }
    }

    // Public ports off: the listed domains that can move go to the tunnel
    // first, and the ports close once they're all on it.
    let moves: Vec<(String, String, String)> = listed
        .iter()
        .filter(|l| l.path == Path::Public && l.outcome == "moves")
        .map(|l| (l.service_id.clone(), l.domain_id.clone(), l.hostname.clone()))
        .collect();
    let mut moving = 0;
    let mut closing = row.closing && !want.public && want.tunnel;
    if now.public && !want.public {
        if !moves.is_empty() {
            let (zones, account_id) = match zones_account.take() {
                Some(z) => z,
                None => {
                    let a = crate::cloudflare::account(s, tx, &team_id)
                        .await?
                        .ok_or_else(|| bad("Connect Cloudflare first."))?;
                    (a.zones, account.clone().unwrap_or_default())
                }
            };
            moving = tunnel_ops::move_domains(tx, &team_id, cluster_id, &zones, &account_id, Some(&moves), wake).await?;
        }
        // Domains already on their way to the tunnel hold the close too.
        let switching: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM pyo.services a, jsonb_array_elements(CASE WHEN jsonb_typeof(a.spec->'domains') = 'array' THEN a.spec->'domains' ELSE '[]' END) d
                             WHERE a.cluster_id = $1 AND NOT a.deleting AND d->>'tunnel' = 'switching')",
        )
        .bind(cluster_id)
        .fetch_one(&mut **tx)
        .await?;
        closing = want.tunnel && (moving > 0 || switching);
        said.push(if closing {
            format!("closes its public ports once {moving} domains are on the tunnel")
        } else {
            "no longer takes traffic on public ports".into()
        });
    } else if want.public && !now.public {
        said.push("takes traffic on public ports".into());
    } else if want.public && row.closing {
        said.push("keeps its public ports open".into());
    }
    let traffic = Paths {
        public: want.public || closing,
        ..want
    }
    .traffic();
    sqlx::query(
        "UPDATE pyo.clusters SET traffic = $2, close_public_after = CASE WHEN $3 THEN coalesce(close_public_after, now()) END WHERE id = $1",
    )
    .bind(cluster_id)
    .bind(traffic)
    .bind(closing)
    .execute(&mut **tx)
    .await?;

    // Tunnel off. With public ports on, its domains switch back first and
    // the tunnel goes once they're back. Without, it goes now.
    let restore = ask.restore_records.unwrap_or(true);
    if now.tunnel && !want.tunnel {
        let back: Vec<&Listed> = listed
            .iter()
            .filter(|l| l.path == Path::Tunnel && l.outcome == "moves")
            .collect();
        for l in &back {
            tunnel_ops::set_domain(s, tx, user, &l.service_id, &l.domain_id, false, false, restore, wake).await?;
        }
        if want.public && !back.is_empty() {
            sqlx::query("UPDATE pyo.cluster_tunnels SET remove_when_back = now(), restore_records = $2 WHERE cluster_id = $1")
                .bind(cluster_id)
                .bind(restore)
                .execute(&mut **tx)
                .await?;
            said.push(format!("removes its Cloudflare Tunnel once {} domains are back on public ports", back.len()));
        } else {
            sqlx::query(
                "UPDATE pyo.cluster_tunnels SET state = 'removing', remove_when_back = NULL, restore_records = $2, token_sent_at = NULL, updated_at = now()
                  WHERE cluster_id = $1 AND state <> 'removing'",
            )
            .bind(cluster_id)
            .bind(restore)
            .execute(&mut **tx)
            .await?;
            said.push("removes its Cloudflare Tunnel".into());
        }
    }

    // Tailnet: the services with domains there get their routes again
    // (routes.rs leaves them out while it's off).
    if want.tailnet != now.tailnet {
        sqlx::query("UPDATE pyo.clusters SET tailnet_web = $2 WHERE id = $1")
            .bind(cluster_id)
            .bind(want.tailnet)
            .execute(&mut **tx)
            .await?;
        let mut services: Vec<&str> = all
            .iter()
            .filter(|d| d.path == Path::Tailnet)
            .map(|d| d.service_id.as_str())
            .collect();
        services.dedup();
        for id in services {
            if let Some(c) = crate::routes::changed(tx, id, true).await? {
                wake.push(c);
            }
        }
        said.push(if want.tailnet {
            "serves websites on the tailnet".into()
        } else {
            "no longer serves websites on the tailnet".into()
        });
    }

    if !said.is_empty() {
        let stopped: Vec<&str> = listed
            .iter()
            .filter(|l| l.outcome == "stops")
            .map(|l| l.hostname.as_str())
            .collect();
        let moved: Vec<&str> = listed
            .iter()
            .filter(|l| l.outcome == "moves")
            .map(|l| l.hostname.as_str())
            .collect();
        let mut title = format!("{name} {}", said.join(", "));
        if !stopped.is_empty() {
            title.push_str(&format!(". Stopped: {}", stopped.join(", ")));
        }
        log_activity(
            tx,
            &team_id,
            &user.name,
            "cluster.updated",
            if stopped.is_empty() { "info" } else { "warning" },
            &title,
            json!({ "kind": "cluster", "id": cluster_id, "label": name }),
            json!({ "clusterId": cluster_id, "moved": moved, "stopped": stopped,
                    "paths": { "public": want.public, "tunnel": want.tunnel, "tailnet": want.tailnet } }),
        )
        .await?;
    }
    wake.push(cluster_id.to_owned());
    let status = if closing || (now.tunnel && !want.tunnel && want.public && listed.iter().any(|l| l.path == Path::Tunnel && l.outcome == "moves")) {
        "moving"
    } else {
        "done"
    };
    Ok(json!({
        "ok": true, "status": status,
        "domains": listed.iter().map(Listed::json).collect::<Vec<_>>(),
        "moving": moving,
        "traffic": traffic,
        "dashboardDrops": now.tailnet != want.tailnet && dashboard_drops(row.same_cluster, refreshed(&row.capabilities), &all),
    }))
}

/// A snapshot cluster's `paths` (`public`, `tunnel`, `tailnet`: the saved
/// switches), `publicPorts` (TCP and UDP ports on the servers while public
/// ports are off) and `proxyProtocol`. Needs its `tunnel` attached.
pub fn shown(c: &mut Value) {
    let paths = of_shown(c);
    let in_tunnel_mode = c["traffic"] == json!("tunnel");
    let ports = c["publicPortsSet"]
        .as_bool()
        .unwrap_or(in_tunnel_mode && c["tunnel"]["portsPublic"].as_bool().unwrap_or(true));
    c["paths"] = json!({ "public": paths.public, "tunnel": paths.tunnel, "tailnet": paths.tailnet });
    c["publicPorts"] = json!(ports);
    c["proxyProtocol"] = c["webProxyProtocol"].clone();
}

/// The switches of a snapshot cluster.
pub fn of_shown(c: &Value) -> Paths {
    Paths {
        public: c["traffic"] == json!("public"),
        tunnel: c["tunnel"].is_object()
            && c["tunnel"]["state"] != json!("removing")
            && c["tunnel"]["removeWhenBack"] != json!(true),
        tailnet: c["tailnetWeb"] != json!(false),
    }
}

/// Each domain's `trafficPath` and `served` (false while its path is off),
/// for the snapshot. Not `path`: that's the domain's URL path. `paths` and `default_domain` are its cluster's.
pub fn mark(domains: &mut Value, paths: Paths, default_domain: Option<&str>) {
    for d in domains.as_array_mut().into_iter().flatten() {
        let p = domain_path(d, default_domain);
        d["trafficPath"] = json!(p.as_str());
        d["served"] = json!(paths.on(p));
    }
}

/// Domains routes leave out: on the tailnet while the cluster's Tailnet
/// is off (their routes go, their certificates stay where they're
/// shared: ts.net names use the tailnet Gateway's own).
pub fn served_on_tailnet(hostname: &str, tailnet_web: bool) -> bool {
    tailnet_web || domain_path(&json!({ "hostname": hostname }), None) != Path::Tailnet
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dom(host: &str, path: Path, movable: bool) -> Domain {
        Domain {
            service_id: "svc_1".into(),
            service: "web".into(),
            domain_id: format!("dom_{host}"),
            hostname: host.into(),
            path,
            movable,
            under_default: host.ends_with(".apps.example.com") && host.matches('.').count() == 3,
            can_go_back: true,
        }
    }

    const ON: Paths = Paths { public: true, tunnel: true, tailnet: true };

    #[test]
    fn each_domain_takes_one_path() {
        let p = |d: Value| domain_path(&d, Some("apps.example.com"));
        assert_eq!(p(json!({ "hostname": "blog.tail1234.ts.net" })), Path::Tailnet);
        assert_eq!(p(json!({ "hostname": "web.100.64.0.2.nip.io" })), Path::Tailnet);
        assert_eq!(p(json!({ "hostname": "web.203.0.113.10.nip.io" })), Path::Public);
        assert_eq!(p(json!({ "hostname": "shop.example.com" })), Path::Public);
        assert_eq!(p(json!({ "hostname": "shop.example.com", "tunnel": "switching" })), Path::Tunnel);
        assert_eq!(p(json!({ "hostname": "shop.example.com", "tunnel": "on" })), Path::Tunnel);
        assert_eq!(p(json!({ "hostname": "web.apps.example.com" })), Path::Tunnel);
        // Deeper than one label under the default domain: its own record.
        assert_eq!(p(json!({ "hostname": "a.web.apps.example.com" })), Path::Public);
        assert_eq!(domain_path(&json!({ "hostname": "web.apps.example.com" }), None), Path::Public);
    }

    #[test]
    fn the_seven_states_and_no_path() {
        let states = [
            (true, false, true, "public"),
            (true, true, true, "public"),
            (false, true, true, "tunnel"),
            (false, false, true, "tailscale"),
            (true, true, false, "public"),
            (true, false, false, "public"),
            (false, true, false, "tunnel"),
        ];
        for (public, tunnel, tailnet, traffic) in states {
            let p = Paths { public, tunnel, tailnet };
            assert!(p.any());
            assert_eq!(p.traffic(), traffic, "{p:?}");
        }
        assert!(!Paths::default().any());
        assert_eq!(ONE_PATH, "Turn on another path first. A cluster needs one.");
    }

    #[test]
    fn turning_a_path_off_lists_its_domains() {
        let all = [
            dom("shop.example.com", Path::Public, true),
            dom("old.other.org", Path::Public, false),
            dom("api.example.com", Path::Tunnel, false),
            dom("web.apps.example.com", Path::Tunnel, false),
            dom("blog.tail1234.ts.net", Path::Tailnet, false),
        ];
        // Nothing turned off: nothing listed.
        assert!(plan(ON, ON, &all, true).is_empty());
        // Public off with the tunnel on: the ones in the account move.
        let l = plan(ON, Paths { public: false, ..ON }, &all, true);
        let got: Vec<(&str, &str)> = l.iter().map(|l| (l.hostname.as_str(), l.outcome)).collect();
        assert_eq!(got, [("shop.example.com", "moves"), ("old.other.org", "stops")]);
        // Not moved when asked not to.
        let l = plan(ON, Paths { public: false, ..ON }, &all, false);
        assert!(l.iter().all(|l| l.outcome == "stops"));
        // Public off with the tunnel off: all stop.
        let now = Paths { tunnel: false, ..ON };
        let l = plan(now, Paths { public: false, ..now }, &all, true);
        assert_eq!(l.len(), 2);
        assert!(l.iter().all(|l| l.outcome == "stops"));
        // Tunnel off with public ports on: back, except under the default domain.
        let l = plan(ON, Paths { tunnel: false, ..ON }, &all, true);
        let got: Vec<(&str, &str)> = l.iter().map(|l| (l.hostname.as_str(), l.outcome)).collect();
        assert_eq!(got, [("api.example.com", "moves"), ("web.apps.example.com", "stops")]);
        // Nothing to go back to (no kept record, no public IP): it stops.
        let mut lone = all.to_vec();
        lone[2].can_go_back = false;
        let l = plan(ON, Paths { tunnel: false, ..ON }, &lone, true);
        assert_eq!(l[0].outcome, "stops");
        // Tunnel off with public ports off: all stop.
        let now = Paths { public: false, ..ON };
        let l = plan(now, Paths { tunnel: false, ..now }, &all, true);
        assert!(l.iter().all(|l| l.outcome == "stops") && l.len() == 2);
        // Tailnet off.
        let l = plan(ON, Paths { tailnet: false, ..ON }, &all, true);
        assert_eq!(l.len(), 1);
        assert_eq!((l[0].hostname.as_str(), l[0].outcome), ("blog.tail1234.ts.net", "stops"));
        // A path that's already off lists nothing.
        let now = Paths { tailnet: false, ..ON };
        assert!(plan(now, now, &all, true).is_empty());
    }

    #[test]
    fn the_tailnet_switch_drops_the_dashboard_only_where_pyo_runs_with_tsnet_names() {
        let ts = [dom("blog.tail1234.ts.net", Path::Tailnet, false)];
        let free = [dom("web.100.64.0.2.nip.io", Path::Tailnet, false)];
        assert!(dashboard_drops(true, false, &ts));
        assert!(!dashboard_drops(false, false, &ts), "another cluster: only the names that stop");
        assert!(!dashboard_drops(true, false, &free), "its own listener, not the tailnet Gateway's");
        assert!(!dashboard_drops(true, false, &[]));
        // Pyo's own tailnet routes carry their policy: nothing drops.
        assert!(!dashboard_drops(true, true, &ts));
        let current = json!({ "gatewaySettings": true, "agentInstall": { "state": "current" } });
        assert!(refreshed(&current));
        assert!(!refreshed(&json!({ "gatewaySettings": true, "agentInstall": { "state": "outdated" } })));
        assert!(!refreshed(&json!({ "agentInstall": { "state": "current" } })), "an agent before the Gateway section");
    }

    #[test]
    fn domains_show_their_path_and_whether_its_served() {
        let mut ds = json!([{ "hostname": "shop.example.com", "path": "/api" }, { "hostname": "blog.tail1234.ts.net" }, { "hostname": "x.example.com", "tunnel": "on" }]);
        mark(&mut ds, Paths { public: false, tunnel: true, tailnet: true }, None);
        assert_eq!(ds[0]["trafficPath"], "public");
        assert_eq!(ds[0]["served"], false);
        assert_eq!(ds[0]["path"], "/api", "its URL path stays");
        assert_eq!(ds[1]["served"], true);
        assert_eq!(ds[2]["trafficPath"], "tunnel");
        assert_eq!(ds[2]["served"], true);
    }

    #[test]
    fn tailnet_off_leaves_only_tailnet_domains_out() {
        assert!(!served_on_tailnet("blog.tail1234.ts.net", false));
        assert!(!served_on_tailnet("web.100.64.0.2.nip.io", false));
        assert!(served_on_tailnet("web.203.0.113.10.nip.io", false));
        assert!(served_on_tailnet("shop.example.com", false));
        assert!(served_on_tailnet("blog.tail1234.ts.net", true));
    }
}
