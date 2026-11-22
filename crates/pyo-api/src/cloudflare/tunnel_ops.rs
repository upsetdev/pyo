//! What people ask of a cluster's Cloudflare Tunnel (docs/cloudflare-tunnel.md
//! §5): the cluster's traffic, a domain through the tunnel or back, Rotate
//! and Remove. These only say what's wanted and return at once. The tunnel
//! loop (tunnel_loop.rs) does the Cloudflare work, so a slow Cloudflare
//! never holds a request.

use serde_json::{Value, json};
use sqlx::{Postgres, Transaction};

use super::{Action, Zone, account, role_for, tunnel, zone_for};
use crate::{
    AppState,
    auth::User,
    error::{ApiError, ApiResult},
    ops::{Role, log_activity, require_role, team_of},
};

/// The traffic choices.
pub const MODES: [&str; 3] = ["public", "tailscale", "tunnel"];

/// Said when the cluster's install can't run a tunnel yet.
pub const INSTALL_FIRST: &str = "Run the command under Refresh this cluster in the cluster's settings first.";

fn bad(m: impl Into<String>) -> ApiError {
    ApiError::bad_request(m.into())
}

/// The zones a tunnel in `account_id` may take: tunnel-ready, in that
/// account.
pub fn tunnel_zones<'a>(zones: &'a [Zone], account_id: &str) -> Vec<&'a Zone> {
    zones
        .iter()
        .filter(|z| z.tunnel_ready() && z.account_id == account_id)
        .collect()
}

/// The account a tunnel goes in: the one asked for, else the default
/// domain's, else the only one with tunnel-ready zones.
pub fn pick_account(
    zones: &[Zone],
    asked: Option<&str>,
    default_domain: Option<&str>,
) -> Result<String, String> {
    let mut accounts: Vec<&str> = zones
        .iter()
        .filter(|z| z.tunnel_ready() && !z.account_id.is_empty())
        .map(|z| z.account_id.as_str())
        .collect();
    accounts.sort();
    accounts.dedup();
    if accounts.is_empty() {
        return Err("Give the Cloudflare token Cloudflare Tunnel:Edit and SSL and Certificates:Edit, then test it under Integrations.".into());
    }
    if let Some(a) = asked {
        return if accounts.contains(&a) {
            Ok(a.to_owned())
        } else {
            Err("The token can't make tunnels in that Cloudflare account.".into())
        };
    }
    if let Some(d) = default_domain
        && let Some(z) = zones.iter().find(|z| z.name == d)
    {
        return Ok(z.account_id.clone());
    }
    match accounts.as_slice() {
        [one] => Ok((*one).to_owned()),
        _ => Err("Pick the Cloudflare account for the tunnel.".into()),
    }
}

/// The default domain as given: a tunnel-ready zone's own name (its apex),
/// since Cloudflare's free certificate covers only one level below it.
pub fn default_domain<'a>(zones: &'a [&'a Zone], given: &str) -> Result<&'a Zone, String> {
    let d = given.trim().trim_end_matches('.').trim_start_matches("*.").to_ascii_lowercase();
    zones
        .iter()
        .copied()
        .find(|z| z.name == d)
        .ok_or_else(|| {
            if zones.iter().any(|z| d.ends_with(&format!(".{}", z.name))) {
                format!("Pick a zone itself, like {}, not a name under it.", zones[0].name)
            } else {
                "Pick one of your Cloudflare zones that can use the tunnel.".into()
            }
        })
}

/// Whether a cluster's install can run a tunnel: its agent says it can,
/// and its install is this Pyo's (the admission policy lets the connector
/// in). Why not, when it can't.
pub fn install_ready(capabilities: &Value) -> Result<(), &'static str> {
    if capabilities["tunnel"] != json!(true) {
        return Err("This cluster's agent doesn't run Cloudflare Tunnel yet. Update the agent first.");
    }
    match capabilities["agentInstall"]["state"].as_str() {
        Some("current") | None => Ok(()),
        _ => Err(INSTALL_FIRST),
    }
}

/// A domain that can go through the tunnel: a hostname a tunnel-ready zone
/// of the tunnel's account covers. Not a free or tailnet domain.
fn can_tunnel(host: &str, zones: &[Zone], account_id: &str) -> Option<Zone> {
    if crate::freedomain::ip_in(host).is_some() || pyo_proto::system::is_tailnet_host(host) {
        return None;
    }
    zone_for(host, zones)
        .filter(|z| z.tunnel_ready() && z.account_id == account_id)
        .cloned()
}

/// What a switch to the tunnel makes of the domain's `tunnel`. A name
/// that's moving from the cluster's public ports takes `"switching"`, so
/// both paths answer while its record moves. One that's already only
/// through the tunnel (`"on"`: under the default domain's wildcard, or
/// switched before) stays as it is, so its gate never opens for a record
/// that changes nothing for direct visitors. None: leave it.
pub(crate) fn switch_state(now: Option<&str>) -> Option<&'static str> {
    match now {
        Some(pyo_proto::TUNNEL_ON) => None,
        _ => Some(pyo_proto::TUNNEL_SWITCHING),
    }
}

/// Whether a name asked onto the tunnel is left to the default domain's
/// wildcard, with no record of its own: it's one level under the default
/// domain (`under`), new to the tunnel (no `tunnel` on the domain yet and
/// no record row), and nothing at Cloudflare answers for that exact name
/// (`found`: the records on it). A record there, Pyo's or anyone's, would
/// answer ahead of the wildcard, so the name takes the switch instead,
/// which replaces Pyo's own and asks before anyone else's. A name already
/// on the tunnel takes the switch too: someone asked for its own record.
pub(crate) fn left_to_wildcard(under: bool, tunnel: bool, has_row: bool, host: &str, found: &[tunnel::Kept]) -> bool {
    under
        && !tunnel
        && !has_row
        && !found
            .iter()
            .any(|k| matches!(k.kind.as_str(), "A" | "AAAA" | "CNAME") && super::same_name(&k.name, host))
}

/// What Switch back does for a domain.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum Back {
    /// It isn't on the tunnel: nothing to do.
    Direct,
    /// A switch that never moved the record just stops.
    Stop,
    /// A switch that never moved the record stops, and the name stays on
    /// the tunnel through the default domain's wildcard.
    Stay,
    /// Its record moves back, both paths first.
    Revert,
    /// The default domain's wildcard would take it straight back: said,
    /// and nothing changes.
    Refuse,
}

/// What Switch back does for a domain. `under`: the tunnel's default
/// domain covers the name, so without a record of its own the loop puts
/// it through the wildcard (`free_domains_follow`). `record`: its record
/// row's (via, status). `tunnel`: the domain has `tunnel` set.
/// `puts_back`: a record it replaced comes back, which then answers for
/// the name ahead of the wildcard.
pub(crate) fn back_plan(under: bool, record: Option<(&str, &str)>, tunnel: bool, puts_back: bool) -> Back {
    let via_tunnel = record.is_some_and(|(via, _)| via == "tunnel");
    let waiting = record.is_some_and(|(via, status)| via != "tunnel" && matches!(status, "switching" | "conflict"));
    match (via_tunnel, tunnel, under) {
        (true, _, true) if !puts_back => Back::Refuse,
        (true, _, _) => Back::Revert,
        (false, false, _) => Back::Direct,
        (false, true, false) => Back::Stop,
        (false, true, true) if waiting => Back::Stay,
        (false, true, true) => Back::Refuse,
    }
}

/// Said when Switch back is asked for a name the default domain covers.
pub fn always_tunnel(host: &str) -> String {
    format!("{host} is under the tunnel's default domain, so it always goes through the tunnel.")
}

/// Ask for a domain's switch to the tunnel: its domain takes
/// `tunnel: "switching"` (both paths answer) unless it's already only
/// through the tunnel ([`switch_state`]), and its record row waits for the
/// loop, which moves the record once the agent has the routes. Whether the
/// domain's routes changed.
#[allow(clippy::too_many_arguments)]
async fn begin_switch(
    tx: &mut Transaction<'_, Postgres>,
    team_id: &str,
    service_id: &str,
    domain_id: &str,
    host: &str,
    zone: &Zone,
    replace: bool,
    now: Option<&str>,
) -> ApiResult<bool> {
    sqlx::query(
        "INSERT INTO pyo.dns_records (service_id, domain_id, team_id, hostname, zone_id, zone_name, type, status, replace, routes_command)
         VALUES ($1, $2, $3, $4, $5, $6, 'CNAME', 'switching', $7, NULL)
         ON CONFLICT (service_id, domain_id) DO UPDATE SET hostname = $4, zone_id = $5, zone_name = $6, status = 'switching',
           replace = $7, error = NULL, existing = '[]', routes_command = NULL, updated_at = now()",
    )
    .bind(service_id)
    .bind(domain_id)
    .bind(team_id)
    .bind(host)
    .bind(&zone.id)
    .bind(&zone.name)
    .bind(replace)
    .execute(&mut **tx)
    .await?;
    match switch_state(now) {
        Some(state) => {
            set_tunnel(tx, service_id, domain_id, Some(state)).await?;
            Ok(true)
        }
        None => Ok(false),
    }
}

async fn set_tunnel(
    tx: &mut Transaction<'_, Postgres>,
    service_id: &str,
    domain_id: &str,
    state: Option<&str>,
) -> ApiResult<()> {
    let domains: Value = sqlx::query_scalar(
        "SELECT coalesce(spec->'domains', '[]') FROM pyo.services WHERE id = $1 FOR UPDATE",
    )
    .bind(service_id)
    .fetch_one(&mut **tx)
    .await?;
    let mut domains = domains;
    if let Some(d) = domains
        .as_array_mut()
        .and_then(|ds| ds.iter_mut().find(|d| d["id"] == json!(domain_id)))
    {
        match state {
            Some(s) => {
                d["tunnel"] = json!(s);
                // Cloudflare's address, not the cluster's: the loop says
                // when it's ready.
                d["status"] = json!("pending_dns");
            }
            None => super::tunnel_loop::leave_tunnel(d),
        }
    }
    sqlx::query("UPDATE pyo.services SET spec = jsonb_set(spec, '{domains}', $2) WHERE id = $1")
        .bind(service_id)
        .bind(&domains)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

/// The service's routes follow, and its waiting rows wait for that sync.
async fn sync(
    tx: &mut Transaction<'_, Postgres>,
    service_id: &str,
    wake: &mut Vec<String>,
) -> ApiResult<()> {
    if let Some((cluster, command)) = crate::routes::changed_waiting(tx, service_id).await? {
        sqlx::query(
            "UPDATE pyo.dns_records SET routes_command = $2 WHERE service_id = $1 AND status IN ('switching', 'reverting')",
        )
        .bind(service_id)
        .bind(&command)
        .execute(&mut **tx)
        .await?;
        wake.push(cluster);
    }
    Ok(())
}

/// A cluster's tunnel as `setClusterTraffic` finds it: (state, account,
/// default domain, its zone, Pyo's wildcard record).
type Existing = (String, String, Option<String>, Option<String>, Option<String>);

/// `setClusterTraffic`.
#[allow(clippy::too_many_arguments)]
pub async fn set_traffic(
    s: &AppState,
    tx: &mut Transaction<'_, Postgres>,
    user: &User,
    cluster_id: &str,
    mode: &str,
    default_domain_given: Option<&str>,
    account_asked: Option<&str>,
    ports_public: Option<bool>,
    wake: &mut Vec<String>,
) -> ApiResult<Value> {
    let team_id = team_of(tx, "clusters", cluster_id).await?;
    require_role(tx, user, &team_id, Role::Admin).await?;
    if !MODES.contains(&mode) {
        return Err(bad("Pick public, tailscale or tunnel."));
    }
    let (name, caps): (String, Value) =
        sqlx::query_as("SELECT name, coalesce(capabilities, '{}') FROM pyo.clusters WHERE id = $1")
            .bind(cluster_id)
            .fetch_one(&mut **tx)
            .await?;
    if mode != "tunnel" {
        sqlx::query("UPDATE pyo.clusters SET traffic = $2 WHERE id = $1")
            .bind(cluster_id)
            .bind(mode)
            .execute(&mut **tx)
            .await?;
        log_activity(
            tx,
            &team_id,
            &user.name,
            "cluster.updated",
            "info",
            &format!(
                "{name} takes traffic {}",
                if mode == "public" { "on public ports" } else { "from Tailscale only" }
            ),
            json!({ "kind": "cluster", "id": cluster_id, "label": name }),
            json!({ "clusterId": cluster_id }),
        )
        .await?;
        wake.push(cluster_id.to_owned());
        return Ok(json!({ "ok": true, "traffic": mode }));
    }
    install_ready(&caps).map_err(bad)?;
    let a = account(s, tx, &team_id)
        .await?
        .ok_or_else(|| bad("Connect Cloudflare under Integrations first."))?;
    let given = default_domain_given.map(str::trim).filter(|d| !d.is_empty());
    let account_id = pick_account(&a.zones, account_asked, given).map_err(bad)?;
    let usable = tunnel_zones(&a.zones, &account_id);
    let default = match given {
        Some(d) => Some(default_domain(&usable, d).map_err(bad)?.clone()),
        None => None,
    };
    let account_name = usable
        .first()
        .map(|z| z.account_name.clone())
        .unwrap_or_default();
    let existing: Option<Existing> = sqlx::query_as(
        "SELECT state, account_id, default_domain, default_zone_id, default_record_id FROM pyo.cluster_tunnels WHERE cluster_id = $1 FOR UPDATE",
    )
    .bind(cluster_id)
    .fetch_optional(&mut **tx)
    .await?;
    match existing {
        Some((state, _, _, _, _)) if state == "removing" => {
            return Err(ApiError::conflict(
                "The tunnel is still being removed. Try again in a minute.",
            ));
        }
        Some((_, had_account, _, _, _)) if had_account != account_id => {
            return Err(ApiError::conflict(
                "This cluster's tunnel is in another Cloudflare account. Remove the tunnel first.",
            ));
        }
        Some((_, _, old_domain, old_zone, old_record)) => {
            // Another default domain: Pyo's old wildcard record goes.
            if old_domain.as_deref() != default.as_ref().map(|z| z.name.as_str())
                && let (Some(zone), Some(id), Some(d)) = (&old_zone, &old_record, &old_domain)
            {
                let mark = super::marker(&super::install_id(tx).await?);
                super::delete_ours(&super::Http::new(&a.token)?, zone, id, &format!("*.{d}"), &mark)
                    .await
                    .map_err(super::api_error)?;
            }
            sqlx::query(
                "UPDATE pyo.cluster_tunnels SET default_domain = $2, default_zone_id = $3,
                        default_record_id = CASE WHEN default_domain IS DISTINCT FROM $2 THEN NULL ELSE default_record_id END,
                        ports_public = coalesce($4, ports_public), updated_at = now()
                  WHERE cluster_id = $1",
            )
            .bind(cluster_id)
            .bind(default.as_ref().map(|z| &z.name))
            .bind(default.as_ref().map(|z| &z.id))
            .bind(ports_public)
            .execute(&mut **tx)
            .await?;
        }
        None => {
            let install = super::install_id(tx).await?;
            sqlx::query(
                "INSERT INTO pyo.cluster_tunnels (cluster_id, team_id, account_id, account_name, tunnel_name, default_domain, default_zone_id, ports_public, created_by)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, coalesce($8, true), $9)",
            )
            .bind(cluster_id)
            .bind(&team_id)
            .bind(&account_id)
            .bind(&account_name)
            .bind(tunnel::tunnel_name(cluster_id, &install, 0))
            .bind(default.as_ref().map(|z| &z.name))
            .bind(default.as_ref().map(|z| &z.id))
            .bind(ports_public)
            .bind(&user.name)
            .execute(&mut **tx)
            .await?;
            log_activity(
                tx,
                &team_id,
                &user.name,
                "cluster.tunnel_created",
                "info",
                &format!("Asked Cloudflare for a tunnel for {name}"),
                json!({ "kind": "cluster", "id": cluster_id, "label": name }),
                json!({ "clusterId": cluster_id, "accountId": account_id }),
            )
            .await?;
        }
    }
    sqlx::query("UPDATE pyo.clusters SET traffic = 'tunnel' WHERE id = $1")
        .bind(cluster_id)
        .execute(&mut **tx)
        .await?;
    // Its domains in the account move to the tunnel. Those with someone
    // else's record wait at "confirm" for an owner or admin.
    let services: Vec<(String, Value)> = sqlx::query_as(
        "SELECT id, coalesce(spec->'domains', '[]') FROM pyo.services WHERE cluster_id = $1 AND NOT deleting ORDER BY id",
    )
    .bind(cluster_id)
    .fetch_all(&mut **tx)
    .await?;
    let mut moving = 0;
    for (service_id, domains) in services {
        let mut any = false;
        for d in domains.as_array().into_iter().flatten() {
            let host = d["hostname"].as_str().unwrap_or("").to_lowercase();
            if d["tunnel"].is_string() {
                continue;
            }
            let Some(zone) = can_tunnel(&host, &a.zones, &account_id) else {
                continue;
            };
            let domain_id = d["id"].as_str().unwrap_or("");
            any |= begin_switch(tx, &team_id, &service_id, domain_id, &host, &zone, false, None).await?;
            moving += 1;
        }
        if any {
            sync(tx, &service_id, wake).await?;
        }
    }
    log_activity(
        tx,
        &team_id,
        &user.name,
        "cluster.updated",
        "info",
        &format!("{name} takes websites through Cloudflare Tunnel"),
        json!({ "kind": "cluster", "id": cluster_id, "label": name }),
        json!({ "clusterId": cluster_id, "moving": moving }),
    )
    .await?;
    wake.push(cluster_id.to_owned());
    Ok(json!({ "ok": true, "traffic": "tunnel", "moving": moving }))
}

/// `setDomainTunnel`.
#[allow(clippy::too_many_arguments)]
pub async fn set_domain(
    s: &AppState,
    tx: &mut Transaction<'_, Postgres>,
    user: &User,
    service_id: &str,
    domain_id: &str,
    on: bool,
    replace: bool,
    restore: bool,
    wake: &mut Vec<String>,
) -> ApiResult<Value> {
    let team_id = team_of(tx, "services", service_id).await?;
    let (cluster_id, domains): (String, Value) = sqlx::query_as(
        "SELECT cluster_id, coalesce(spec->'domains', '[]') FROM pyo.services WHERE id = $1 AND NOT deleting",
    )
    .bind(service_id)
    .fetch_optional(&mut **tx)
    .await?
    .ok_or_else(|| ApiError::not_found("Service"))?;
    let d = domains
        .as_array()
        .and_then(|ds| ds.iter().find(|d| d["id"] == json!(domain_id)))
        .cloned()
        .ok_or_else(|| ApiError::not_found("Domain"))?;
    let host = d["hostname"].as_str().unwrap_or("").to_lowercase();
    let row: Option<(String, String, Option<String>, Value)> = sqlx::query_as(
        "SELECT r.status, r.via, NULL::text, r.previous FROM pyo.dns_records r WHERE r.service_id = $1 AND r.domain_id = $2",
    )
    .bind(service_id)
    .bind(domain_id)
    .fetch_optional(&mut **tx)
    .await?;
    let kept = row
        .as_ref()
        .is_some_and(|r| r.3.as_array().is_some_and(|a| !a.is_empty()));
    // Replacing someone's record, or putting one back: owners and admins.
    let action = if replace || (!on && restore && kept) {
        Action::ReplaceRecord
    } else {
        Action::CreateRecord
    };
    require_role(tx, user, &team_id, role_for(action)).await?;
    let tunnel_row: Option<(String, String, Option<String>)> = sqlx::query_as(
        "SELECT state, account_id, default_domain FROM pyo.cluster_tunnels WHERE cluster_id = $1",
    )
    .bind(&cluster_id)
    .fetch_optional(&mut **tx)
    .await?;
    let Some((_, account_id, default_domain)) = tunnel_row.filter(|(st, _, _)| st != "removing") else {
        return Err(bad("This cluster has no Cloudflare Tunnel. Pick Cloudflare Tunnel under the cluster's Traffic first."));
    };
    // One level under the tunnel's default domain: its wildcard covers it.
    let under = default_domain
        .as_deref()
        .is_some_and(|dd| super::tunnel_loop::under_default(&host, dd));
    let status = row.as_ref().map(|r| r.0.as_str()).unwrap_or("");
    let via = row.as_ref().map(|r| r.1.as_str()).unwrap_or("direct");
    if on {
        let a = account(s, tx, &team_id)
            .await?
            .ok_or_else(|| bad("Connect Cloudflare under Integrations first."))?;
        let Some(zone) = can_tunnel(&host, &a.zones, &account_id) else {
            return Err(bad(format!(
                "No zone of the tunnel's Cloudflare account can take {host} through the tunnel."
            )));
        };
        if via == "tunnel" && status == "created" {
            return Ok(json!({ "ok": true, "status": "active" }));
        }
        // A new name under the default domain: the wildcard record has it,
        // so it needs no record of its own, whoever asks (dashboard, CLI,
        // MCP, API). Only when Cloudflare says nothing else answers for
        // the name. When Cloudflare can't be asked, the switch below finds
        // out.
        if under && row.is_none() && !d["tunnel"].is_string() {
            use tunnel::TunnelApi;
            let found = super::Http::new(&a.token)?.records_kept(&zone.id, &host).await;
            if found.is_ok_and(|f| left_to_wildcard(under, false, false, &host, &f)) {
                set_tunnel(tx, service_id, domain_id, Some(pyo_proto::TUNNEL_ON)).await?;
                sync(tx, service_id, wake).await?;
                log_activity(
                    tx,
                    &team_id,
                    &user.name,
                    "domain.tunnel",
                    "info",
                    &format!("{host} goes through the Cloudflare Tunnel, under its default domain"),
                    json!({ "kind": "service", "id": service_id, "label": host }),
                    json!({ "serviceId": service_id }),
                )
                .await?;
                return Ok(json!({ "ok": true, "status": "active" }));
            }
        }
        // Already only through the tunnel: it stays gated while its own
        // record is made, and the agent has nothing new to apply.
        if begin_switch(tx, &team_id, service_id, domain_id, &host, &zone, replace, d["tunnel"].as_str()).await? {
            sync(tx, service_id, wake).await?;
        }
        log_activity(
            tx,
            &team_id,
            &user.name,
            "domain.tunnel",
            "info",
            &format!("Asked to move {host} to the Cloudflare Tunnel"),
            json!({ "kind": "service", "id": service_id, "label": host }),
            json!({ "serviceId": service_id, "replace": replace }),
        )
        .await?;
        return Ok(json!({ "ok": true, "status": "switching" }));
    }
    // Back. A name under the default domain goes through the tunnel's
    // wildcard whenever it has no record of its own, so moving it back
    // would last until the loop's next pass: refused, unless a record it
    // replaced comes back (that one answers ahead of the wildcard).
    let plan = back_plan(
        under,
        row.as_ref().map(|r| (r.1.as_str(), r.0.as_str())),
        d["tunnel"].is_string(),
        restore && kept,
    );
    let stop_waiting = "DELETE FROM pyo.dns_records WHERE service_id = $1 AND domain_id = $2 AND status IN ('switching', 'conflict') AND via = 'direct'";
    match plan {
        Back::Refuse => return Err(bad(always_tunnel(&host))),
        Back::Direct => return Ok(json!({ "ok": true, "status": "direct" })),
        // A switch that never moved the record (still switching, or
        // waiting for a yes) just stops.
        Back::Stop => {
            set_tunnel(tx, service_id, domain_id, None).await?;
            sqlx::query(stop_waiting)
                .bind(service_id)
                .bind(domain_id)
                .execute(&mut **tx)
                .await?;
            sync(tx, service_id, wake).await?;
            return Ok(json!({ "ok": true, "status": "direct" }));
        }
        // The same, but the wildcard has the name: it stays only through
        // the tunnel, without a moment off it.
        Back::Stay => {
            sqlx::query(stop_waiting)
                .bind(service_id)
                .bind(domain_id)
                .execute(&mut **tx)
                .await?;
            if d["tunnel"] != json!(pyo_proto::TUNNEL_ON) {
                set_tunnel(tx, service_id, domain_id, Some(pyo_proto::TUNNEL_ON)).await?;
                sync(tx, service_id, wake).await?;
            }
            return Ok(json!({ "ok": true, "status": "active" }));
        }
        Back::Revert => {}
    }
    sqlx::query(
        "UPDATE pyo.dns_records SET status = 'reverting', restore = $3, error = NULL, updated_at = now()
          WHERE service_id = $1 AND domain_id = $2",
    )
    .bind(service_id)
    .bind(domain_id)
    .bind(restore)
    .execute(&mut **tx)
    .await?;
    // Both paths again before the record moves back.
    set_tunnel(tx, service_id, domain_id, Some(pyo_proto::TUNNEL_SWITCHING)).await?;
    sync(tx, service_id, wake).await?;
    log_activity(
        tx,
        &team_id,
        &user.name,
        "domain.tunnel",
        "info",
        &format!("Asked to move {host} back from the Cloudflare Tunnel"),
        json!({ "kind": "service", "id": service_id, "label": host }),
        json!({ "serviceId": service_id, "restore": restore }),
    )
    .await?;
    Ok(json!({ "ok": true, "status": "reverting" }))
}

/// `removeClusterTunnel`.
pub async fn remove(
    tx: &mut Transaction<'_, Postgres>,
    user: &User,
    cluster_id: &str,
    restore_records: bool,
    wake: &mut Vec<String>,
) -> ApiResult<Value> {
    let team_id = team_of(tx, "clusters", cluster_id).await?;
    require_role(tx, user, &team_id, Role::Admin).await?;
    let (name, traffic): (String, String) =
        sqlx::query_as("SELECT name, traffic FROM pyo.clusters WHERE id = $1")
            .bind(cluster_id)
            .fetch_one(&mut **tx)
            .await?;
    if traffic == "tunnel" {
        return Err(bad(
            "Pick Public ports or Tailscale only for the cluster first.",
        ));
    }
    let n = sqlx::query(
        "UPDATE pyo.cluster_tunnels SET state = 'removing', restore_records = $2, token_sent_at = NULL, updated_at = now()
          WHERE cluster_id = $1 AND state <> 'removing'",
    )
    .bind(cluster_id)
    .bind(restore_records)
    .execute(&mut **tx)
    .await?
    .rows_affected();
    if n > 0 {
        log_activity(
            tx,
            &team_id,
            &user.name,
            "cluster.tunnel_removing",
            "warning",
            &format!("Removing the Cloudflare Tunnel of {name}"),
            json!({ "kind": "cluster", "id": cluster_id, "label": name }),
            json!({ "clusterId": cluster_id, "restoreRecords": restore_records }),
        )
        .await?;
    }
    wake.push(cluster_id.to_owned());
    Ok(json!({ "ok": true }))
}

/// `rotateTunnelToken`.
pub async fn rotate(
    tx: &mut Transaction<'_, Postgres>,
    user: &User,
    cluster_id: &str,
) -> ApiResult<Value> {
    let team_id = team_of(tx, "clusters", cluster_id).await?;
    require_role(tx, user, &team_id, Role::Admin).await?;
    let n = sqlx::query(
        "UPDATE pyo.cluster_tunnels SET rotate = true WHERE cluster_id = $1 AND state <> 'removing'",
    )
    .bind(cluster_id)
    .execute(&mut **tx)
    .await?
    .rows_affected();
    if n == 0 {
        return Err(ApiError::not_found("Cloudflare Tunnel"));
    }
    let name: String = sqlx::query_scalar("SELECT name FROM pyo.clusters WHERE id = $1")
        .bind(cluster_id)
        .fetch_one(&mut **tx)
        .await?;
    log_activity(
        tx,
        &team_id,
        &user.name,
        "cluster.tunnel_rotated",
        "info",
        &format!("Asked for a new Cloudflare Tunnel token and certificate for {name}"),
        json!({ "kind": "cluster", "id": cluster_id, "label": name }),
        json!({ "clusterId": cluster_id }),
    )
    .await?;
    Ok(json!({ "ok": true }))
}

/// A cluster's Cloudflare Tunnel
///
/// The cluster's tunnel as Pyo last saw it: its state, Cloudflare's view
/// (`health`, `connections`, `colos`), the connectors in the cluster
/// (`connectors`: each one's server, whether it's ready, and why not), its
/// default domain, its certificate, and `refused` when the cluster's install
/// is older than Pyo's. Never a token. Cloudflare's view is read again when
/// it's older than 30 seconds.
#[utoipa::path(get, path = "/clusters/{id}/tunnel", operation_id = "getClusterTunnel", tag = "Clusters",
    params(("id" = String, Path, description = "The cluster's id (`cl_…`).")),
    responses(
        (status = 200, description = "`tunnel`: the cluster's tunnel, or null without one. `traffic`: \"public\", \"tailscale\" or \"tunnel\".", body = Object),
        (status = 404, description = "No such cluster, or it isn't yours to see.", body = crate::error::ErrorBody),
    ),
    extensions(("x-pyo-access" = json!("read"))))]
pub async fn get(
    axum::extract::State(s): axum::extract::State<AppState>,
    crate::auth::SessionUser(user): crate::auth::SessionUser,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> ApiResult<crate::error::Json<Value>> {
    let mut tx = crate::rls::begin(&s.db, &user).await?;
    let team_id = team_of(&mut tx, "clusters", &id).await?;
    require_role(&mut tx, &user, &team_id, Role::Viewer).await?;
    tx.commit().await?;
    // Cloudflare's view as of now (at most 30 seconds old).
    super::tunnel_loop::refresh(&s, &id).await?;
    let mut tx = crate::rls::begin(&s.db, &user).await?;
    let traffic: String = sqlx::query_scalar("SELECT traffic FROM pyo.clusters WHERE id = $1")
        .bind(&id)
        .fetch_one(&mut *tx)
        .await?;
    let tunnel = super::tunnel_loop::one_shown(&mut *tx, &id).await?;
    tx.commit().await?;
    Ok(crate::error::Json(json!({ "traffic": traffic, "tunnel": tunnel })))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cloudflare::tests::zone;

    fn in_account(id: &str, name: &str, account: &str) -> Zone {
        Zone {
            account_id: account.into(),
            ..zone(id, name)
        }
    }

    #[test]
    fn the_account_is_asked_for_only_when_it_isnt_clear() {
        let one = [zone("z1", "example.com"), zone("z2", "other.org")];
        assert_eq!(pick_account(&one, None, None).unwrap(), "acct1");
        let two = [in_account("z1", "example.com", "a1"), in_account("z2", "other.org", "a2")];
        assert_eq!(pick_account(&two, None, None).unwrap_err(), "Pick the Cloudflare account for the tunnel.");
        assert_eq!(pick_account(&two, None, Some("other.org")).unwrap(), "a2");
        assert_eq!(pick_account(&two, Some("a1"), None).unwrap(), "a1");
        assert!(pick_account(&two, Some("a9"), None).is_err());
        let mut dns_only = zone("z1", "example.com");
        dns_only.tunnel.ready = false;
        assert!(pick_account(&[dns_only], None, None).unwrap_err().starts_with("Give the Cloudflare token"));
    }

    #[test]
    fn the_default_domain_is_a_zone_itself() {
        let zones = [zone("z1", "example.com")];
        let usable: Vec<&Zone> = zones.iter().collect();
        assert_eq!(default_domain(&usable, "example.com").unwrap().id, "z1");
        assert_eq!(default_domain(&usable, "*.Example.com.").unwrap().id, "z1");
        assert_eq!(
            default_domain(&usable, "apps.example.com").unwrap_err(),
            "Pick a zone itself, like example.com, not a name under it."
        );
        assert!(default_domain(&usable, "other.org").is_err());
    }

    #[test]
    fn tunnel_mode_waits_for_an_install_that_can_run_it() {
        assert_eq!(install_ready(&json!({})).unwrap_err(), "This cluster's agent doesn't run Cloudflare Tunnel yet. Update the agent first.");
        assert_eq!(
            install_ready(&json!({ "tunnel": true, "agentInstall": { "state": "outdated" } })).unwrap_err(),
            INSTALL_FIRST
        );
        assert_eq!(
            install_ready(&json!({ "tunnel": true, "agentInstall": { "state": "updating" } })).unwrap_err(),
            INSTALL_FIRST
        );
        assert!(install_ready(&json!({ "tunnel": true, "agentInstall": { "state": "current" } })).is_ok());
        assert!(!INSTALL_FIRST.contains(';'));
    }

    #[test]
    fn a_name_already_on_the_tunnel_stays_gated_while_its_record_is_made() {
        // Moving from public: both paths while the record moves.
        assert_eq!(switch_state(None), Some("switching"));
        assert_eq!(switch_state(Some("switching")), Some("switching"));
        // Under the wildcard, or switched before: never back to both paths.
        assert_eq!(switch_state(Some("on")), None);
    }

    #[test]
    fn a_new_name_under_the_default_domain_gets_no_record_of_its_own() {
        let kept = |kind: &str, name: &str| tunnel::Kept {
            id: "r1".into(),
            kind: kind.into(),
            name: name.into(),
            content: "203.0.113.7".into(),
            proxied: false,
            ttl: 1,
            comment: None,
        };
        let host = "web-shop.example.com";
        // Nothing on the name: the wildcard has it.
        assert!(left_to_wildcard(true, false, false, host, &[]));
        // Records that don't answer for a website, or for another name, don't matter.
        assert!(left_to_wildcard(true, false, false, host, &[kept("TXT", host), kept("MX", host)]));
        assert!(left_to_wildcard(true, false, false, host, &[kept("A", "other.example.com")]));
        // A record on that exact name answers ahead of the wildcard: the
        // switch takes it (and stops at "conflict" for someone else's).
        for kind in ["A", "AAAA", "CNAME"] {
            assert!(!left_to_wildcard(true, false, false, host, &[kept(kind, "Web-Shop.example.com.")]), "{kind}");
        }
        // Outside the default domain: always its own record.
        assert!(!left_to_wildcard(false, false, false, "blog.other.org", &[]));
        // Already on the tunnel, or with a record row: someone asked for
        // its own record, so it gets one (and stays gated meanwhile).
        assert!(!left_to_wildcard(true, true, false, host, &[]));
        assert!(!left_to_wildcard(true, false, true, host, &[]));
    }

    #[test]
    fn switch_back_is_refused_under_the_default_domain() {
        let created = Some(("tunnel", "created"));
        // Outside the default domain: Switch back as ever.
        assert_eq!(back_plan(false, created, true, false), Back::Revert);
        assert_eq!(back_plan(false, created, true, true), Back::Revert);
        assert_eq!(back_plan(false, Some(("direct", "conflict")), true, false), Back::Stop);
        assert_eq!(back_plan(false, Some(("direct", "switching")), true, false), Back::Stop);
        assert_eq!(back_plan(false, Some(("direct", "created")), false, false), Back::Direct);
        assert_eq!(back_plan(false, None, false, false), Back::Direct);
        // Under it, the wildcard takes the name back: refused, with its own
        // record or only the wildcard.
        assert_eq!(back_plan(true, created, true, false), Back::Refuse);
        assert_eq!(back_plan(true, None, true, false), Back::Refuse);
        assert_eq!(back_plan(true, None, true, true), Back::Refuse);
        // A record it replaced comes back and answers ahead of the wildcard.
        assert_eq!(back_plan(true, created, true, true), Back::Revert);
        // A switch waiting for a yes can be cancelled: it stays on the tunnel.
        assert_eq!(back_plan(true, Some(("direct", "conflict")), true, false), Back::Stay);
        assert_eq!(back_plan(true, Some(("direct", "switching")), true, false), Back::Stay);
        // A direct record of its own wins over the wildcard: it is direct.
        assert_eq!(back_plan(true, Some(("direct", "created")), false, false), Back::Direct);
        let said = always_tunnel("web-shop.example.com");
        assert_eq!(said, "web-shop.example.com is under the tunnel's default domain, so it always goes through the tunnel.");
        assert!(!said.contains(';') && !said.contains(" - "));
    }

    #[test]
    fn free_and_tailnet_domains_stay_direct() {
        let zones = [zone("z1", "example.com"), in_account("z2", "other.org", "a2")];
        assert!(can_tunnel("blog.example.com", &zones, "acct1").is_some());
        assert!(can_tunnel("blog.other.org", &zones, "acct1").is_none(), "another account");
        assert!(can_tunnel("web.203.0.113.7.nip.io", &zones, "acct1").is_none());
        assert!(can_tunnel("web.tail1234.ts.net", &zones, "acct1").is_none());
        assert!(can_tunnel("blog.nowhere.dev", &zones, "acct1").is_none());
    }
}
