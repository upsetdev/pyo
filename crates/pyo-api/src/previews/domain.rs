//! Where previews are reached, and a cluster's preview domain.
//!
//! - Private previews: the cluster's tailnet (`web-pr-42.<tailnet>.ts.net`),
//!   a Tailscale Service its agent makes as for any tailnet domain.
//! - Else the cluster's preview domain when it has one
//!   (`web-pr-42.preview.example.com`): a wildcard record points at the
//!   cluster's public IP. With the team's Cloudflare covering the domain,
//!   Pyo makes that record and gets a wildcard certificate (DNS-01, as for
//!   sandbox edges), kept with the team's certificates and renewed 30
//!   days before it runs out. Without, each preview gets its own
//!   certificate, and the record is made by hand.
//! - Else nip.io on the cluster's free-domain address
//!   (`web-pr-42.203.0.113.7.nip.io`).
//! - In Cloudflare Tunnel mode, public previews go under the tunnel's
//!   default domain (`web-pr-42.example.com`), covered by its wildcard
//!   record and route, like free domains. The preview domain isn't used
//!   then. Without a default domain there's no preview URL.
//!
//! A domain is per cluster: its wildcard record can point at one cluster.

use std::{net::Ipv4Addr, time::Duration};

use instant_acme::RetryPolicy;
use serde_json::{Value, json};
use sqlx::{Postgres, Transaction};

use super::{Settings, naming};
use crate::{
    AppState,
    auth::User,
    cloudflare::{self, Cloudflare},
    error::{ApiError, ApiResult},
    ops::{Role, log_activity, require_role},
};

/// A preview's domain on its copy, as the dashboard makes domains.
pub fn new_domain(host: &str, port: &str) -> Value {
    json!({
        "id": crate::ids::new_id("dom"), "hostname": host, "portName": port,
        "status": "pending_dns", "tls": "pending", "tlsMode": "auto",
        "redirectHttps": !crate::deploy::private_host(host),
        "createdAt": pyo_proto::now_ms(),
    })
}

/// The cluster's preview domain as stored.
async fn stored(tx: &mut Transaction<'_, Postgres>, cluster_id: &str) -> ApiResult<Value> {
    Ok(sqlx::query_scalar::<_, Option<Value>>(
        "SELECT preview_domain FROM pyo.clusters WHERE id = $1",
    )
    .bind(cluster_id)
    .fetch_optional(&mut **tx)
    .await?
    .flatten()
    .unwrap_or(Value::Null))
}

/// A domain under the cluster's preview domain uses its wildcard
/// certificate once there is one; else its own (Let's Encrypt).
pub async fn with_certificate(
    tx: &mut Transaction<'_, Postgres>,
    cluster_id: &str,
    mut d: Value,
) -> ApiResult<Value> {
    // Through the cluster's tunnel: the tunnel loop keeps its certificate.
    if d["tunnel"].is_string() {
        return Ok(d);
    }
    let pd = stored(tx, cluster_id).await?;
    let host = d["hostname"].as_str().unwrap_or("").to_owned();
    let under = pd["domain"]
        .as_str()
        .is_some_and(|dom| host.ends_with(&format!(".{dom}")));
    match pd["certificateId"].as_str() {
        Some(cert) if under && pd["certificate"]["state"] == json!("issued") => {
            d["tlsMode"] = json!("custom");
            d["certificateId"] = json!(cert);
        }
        _ => {
            if d["tlsMode"] == json!("custom") {
                d["tlsMode"] = json!("auto");
            }
            if let Some(o) = d.as_object_mut() {
                o.remove("certificateId");
            }
        }
    }
    Ok(d)
}

/// The suffix of a preview's URL on the cluster, or why it has none.
async fn suffix(
    tx: &mut Transaction<'_, Postgres>,
    cluster_id: &str,
    settings: &Settings,
) -> ApiResult<Result<String, String>> {
    if settings.access == "private" {
        let t = crate::tailscale::of(&mut **tx, cluster_id).await?;
        return Ok(match t.and_then(|t| t.suffix) {
            Some(s) => Ok(s),
            None => Err("Private previews need Tailscale on the cluster.".into()),
        });
    }
    let preview_domain = stored(tx, cluster_id).await?["domain"]
        .as_str()
        .map(str::to_owned);
    let cluster = crate::freedomain::load(tx, cluster_id)
        .await?
        .map(|(_, c)| c)
        .unwrap_or_default();
    Ok(public_suffix(&cluster, preview_domain.as_deref()))
}

/// Said when a cluster in tunnel mode has no default domain for previews.
pub const PICK_DEFAULT_DOMAIN: &str = "Pick a default domain for the cluster's Cloudflare Tunnel first. Previews get names under it.";

/// Where a public preview's URL goes: under the tunnel's default domain in
/// tunnel mode (none without one), else the preview domain, else the
/// cluster's free-domain address on nip.io.
pub fn public_suffix(c: &crate::freedomain::Cluster, preview_domain: Option<&str>) -> Result<String, String> {
    if c.tunnel {
        return c.tunnel_domain.clone().ok_or_else(|| PICK_DEFAULT_DOMAIN.to_owned());
    }
    if let Some(d) = preview_domain {
        return Ok(d.to_owned());
    }
    c.suffix()
        .ok_or_else(|| "The cluster has no address for preview URLs yet.".into())
}

/// Whether a host is taken: by a service's domain (not `copy`), or by
/// another preview that isn't removed. Any team's.
async fn taken(
    tx: &mut Transaction<'_, Postgres>,
    host: &str,
    preview_id: &str,
    copy: Option<&str>,
) -> ApiResult<bool> {
    let lifted = crate::rls::lift(tx).await?;
    let taken: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM pyo.services a, jsonb_array_elements(CASE WHEN jsonb_typeof(a.spec->'domains') = 'array' THEN a.spec->'domains' ELSE '[]' END) d
                         WHERE NOT a.deleting AND lower(d->>'hostname') = $1 AND a.id IS DISTINCT FROM $3)
             OR EXISTS (SELECT 1 FROM pyo.previews WHERE hostname = $1 AND id <> $2 AND state <> 'removed')",
    )
    .bind(host)
    .bind(preview_id)
    .bind(copy)
    .fetch_one(&mut **tx)
    .await?;
    lifted.lower(tx).await?;
    Ok(taken)
}

/// A preview's URL host: the one it had while it still fits where its
/// previews go now, else `<service>-pr-<n>` there (told apart by a short
/// hash when another has it). The inner error says why there's none.
#[allow(clippy::too_many_arguments)]
pub async fn host_for(
    tx: &mut Transaction<'_, Postgres>,
    cluster_id: &str,
    settings: &Settings,
    service: &str,
    number: i64,
    service_id: &str,
    current: Option<&str>,
    preview_id: &str,
    copy: Option<&str>,
) -> ApiResult<Result<Option<String>, String>> {
    let suffix = match suffix(tx, cluster_id, settings).await? {
        Ok(s) => s.to_ascii_lowercase(),
        Err(why) => return Ok(Err(why)),
    };
    if let Some(h) = current
        && h.ends_with(&format!(".{suffix}"))
        && !taken(tx, h, preview_id, copy).await?
    {
        return Ok(Ok(Some(h.to_owned())));
    }
    for label in [
        naming::label(service, number, naming::LABEL_MAX),
        naming::label_salted(service, number, naming::LABEL_MAX, service_id),
    ] {
        let h = naming::host(&label, &suffix);
        if h.len() <= 253 && !taken(tx, &h, preview_id, copy).await? {
            return Ok(Ok(Some(h)));
        }
    }
    Ok(Err("Another service has this preview's address.".into()))
}

// ── The cluster's preview domain ───────────────────────────────────────────

/// For the dashboard: the domain, the wildcard record to make (at the
/// cluster's public IP), whether Pyo makes it in Cloudflare, and the
/// wildcard certificate.
pub fn show(stored: &Value, public_ip: Option<Ipv4Addr>) -> Value {
    let Some(domain) = stored["domain"].as_str() else {
        return Value::Null;
    };
    let cf = stored["dns"] == json!("cloudflare");
    json!({
        "domain": domain,
        "record": { "name": format!("*.{domain}"), "type": "A", "content": public_ip.map(|i| i.to_string()) },
        "dns": if cf { "cloudflare" } else { "manual" },
        "certificate": if cf && stored["certificate"].is_object() {
            json!({
                "state": stored["certificate"]["state"],
                "notAfter": stored["certificate"]["notAfter"],
                "error": stored["certificate"]["error"],
            })
        } else { Value::Null },
        "error": stored["record"]["error"],
    })
}

/// A preview domain as typed: a hostname of two labels or more, not one of
/// the free or tailnet ones.
pub fn parse(domain: &str) -> ApiResult<String> {
    let d = domain
        .trim()
        .trim_end_matches('.')
        .trim_start_matches("*.")
        .to_ascii_lowercase();
    if !crate::util::hostname(&d) {
        return Err(ApiError::bad_request(
            "Give a domain like preview.example.com.",
        ));
    }
    if [".nip.io", ".sslip.io", ".ts.net"]
        .iter()
        .any(|s| d.ends_with(s) || d == s[1..])
    {
        return Err(ApiError::bad_request(
            "Use a domain of your own. Previews get a nip.io address without one.",
        ));
    }
    Ok(d)
}

/// setClusterPreviewDomain: sets the cluster's preview domain (None
/// removes it). Running previews keep their address until the next push.
pub async fn set(
    tx: &mut Transaction<'_, Postgres>,
    user: &User,
    cluster_id: &str,
    domain: Option<&str>,
) -> ApiResult<Value> {
    let team = crate::ops::team_of(tx, "clusters", cluster_id).await?;
    require_role(tx, user, &team, Role::Admin).await?;
    let domain = match domain.map(str::trim).filter(|d| !d.is_empty()) {
        Some(d) => Some(parse(d)?),
        None => None,
    };
    let before = stored(tx, cluster_id).await?;
    if let Some(d) = &domain {
        let lifted = crate::rls::lift(tx).await?;
        let other: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM pyo.clusters WHERE id <> $1 AND preview_domain->>'domain' = $2)",
        )
        .bind(cluster_id)
        .bind(d)
        .fetch_one(&mut **tx)
        .await?;
        lifted.lower(tx).await?;
        if other {
            return Err(ApiError::conflict(format!(
                "{d} is the preview domain of another cluster."
            )));
        }
        // Where teams don't trust each other (Pyo Cloud): theirs only.
        if crate::verify_domains() && !crate::domainproof::proven(&team, d).await {
            return Err(ApiError::forbidden(crate::domainproof::how_to(&team, d)));
        }
    }
    let value = match &domain {
        // The same domain keeps its certificate and record.
        Some(d) if before["domain"].as_str() == Some(d) => before.clone(),
        Some(d) => json!({ "domain": d }),
        None => Value::Null,
    };
    sqlx::query("UPDATE pyo.clusters SET preview_domain = $2 WHERE id = $1")
        .bind(cluster_id)
        .bind(if value.is_null() { None } else { Some(&value) })
        .execute(&mut **tx)
        .await?;
    let name: String = sqlx::query_scalar("SELECT name FROM pyo.clusters WHERE id = $1")
        .bind(cluster_id)
        .fetch_one(&mut **tx)
        .await?;
    log_activity(
        tx,
        &team,
        &user.name,
        "cluster.updated",
        "info",
        &match &domain {
            Some(d) => format!("Previews on {name} use {d}"),
            None => format!("Previews on {name} use nip.io"),
        },
        json!({ "kind": "cluster", "id": cluster_id, "label": name }),
        json!({}),
    )
    .await?;
    Ok(json!({ "ok": true }))
}

/// A wildcard certificate that failed is tried again after this long.
const RETRY_MS: i64 = 6 * 3600 * 1000;
/// One still "issuing" after this long was cut off.
const STALE_MS: i64 = 15 * 60 * 1000;

/// For each cluster's preview domain in a zone of the team's Cloudflare:
/// the wildcard record at its public IP, and the wildcard certificate (got,
/// renewed, retried after a failure). Without Cloudflare: nothing to do.
pub async fn tick(s: &AppState) -> ApiResult<()> {
    let rows: Vec<(String, String, Value)> = sqlx::query_as(
        "SELECT id, team_id, preview_domain FROM pyo.clusters WHERE preview_domain IS NOT NULL",
    )
    .fetch_all(&s.db)
    .await?;
    for (cluster, team, pd) in rows {
        if let Err(e) = one(s, &cluster, &team, pd).await {
            tracing::info!(%cluster, error = %e.message, "previews: preview domain");
        }
    }
    Ok(())
}

async fn one(s: &AppState, cluster: &str, team: &str, mut pd: Value) -> ApiResult<()> {
    let Some(domain) = pd["domain"].as_str().map(str::to_owned) else {
        return Ok(());
    };
    let mut tx = s.db.begin().await?;
    let account = cloudflare::account(s, &mut tx, team).await?;
    let ip = crate::freedomain::load(&mut tx, cluster)
        .await?
        .and_then(|(_, c)| c.public_ip());
    let marker = cloudflare::marker(&cloudflare::install_id(&mut tx).await?);
    tx.commit().await?;
    let zone = account.as_ref().and_then(|a| {
        cloudflare::zone_for(&format!("x.{domain}"), &a.zones)
            .filter(|z| z.can_edit)
            .cloned()
    });
    let (Some(account), Some(zone)) = (account, zone) else {
        if pd["dns"] == json!("cloudflare") {
            pd["dns"] = json!("manual");
            save(s, cluster, &pd).await?;
        }
        return Ok(());
    };
    let before = pd.clone();
    pd["dns"] = json!("cloudflare");
    let cf = cloudflare::Http::new(&account.token)?;
    let now = pyo_proto::now_ms();
    // The wildcard record, at the public IP (Pyo's old ones go first).
    if let Some(ip) = ip.map(|i| i.to_string())
        && (pd["record"]["content"] != json!(ip)
            || (pd["record"]["state"] != json!("ok")
                && now - pd["record"]["at"].as_i64().unwrap_or(0) > 600_000))
    {
        let name = format!("*.{domain}");
        if let Ok(found) = cf.records(&zone.id, &name).await {
            for r in found
                .iter()
                .filter(|r| cloudflare::ours(r, &marker, &name) && r.kind == "A" && r.content != ip)
            {
                let _ = cf.delete(&zone.id, &r.id).await;
            }
        }
        let out = crate::sandbox::edgecert::ensure_wildcard(
            &cf,
            &zone.id,
            &name,
            &[("A".to_owned(), ip.clone())],
            &marker,
        )
        .await;
        pd["record"] = match out {
            Ok(o) if o.status == "conflict" => json!({
                "content": ip, "state": "conflict", "at": now,
                "error": format!("{name} has other records in Cloudflare. Remove them or point them at {ip}."),
            }),
            Ok(_) => json!({ "content": ip, "state": "ok", "at": now }),
            Err(e) => {
                json!({ "content": ip, "state": "failed", "at": now, "error": e.to_string() })
            }
        };
    }
    // The wildcard certificate.
    let cert = &pd["certificate"];
    let state = cert["state"].as_str().unwrap_or("");
    let at = cert["at"].as_i64().unwrap_or(0);
    let due = match state {
        "issuing" => now - at > STALE_MS,
        "failed" => now - at > RETRY_MS,
        "issued" => crate::sandbox::edgecert::due(
            cert["notBefore"].as_i64().unwrap_or(0) / 1000,
            cert["notAfter"].as_i64().unwrap_or(0) / 1000,
            now / 1000,
        ),
        _ => true,
    };
    if due && s.sealer.enabled() {
        pd["certificate"]["state"] = json!("issuing");
        pd["certificate"]["at"] = json!(now);
        save(s, cluster, &pd).await?;
        let (s2, c2, t2, d2, z2, tok) = (
            s.clone(),
            cluster.to_owned(),
            team.to_owned(),
            domain.clone(),
            zone.id.clone(),
            account.token.clone(),
        );
        tokio::spawn(async move {
            let out = issue(&s2, &c2, &t2, &d2, &z2, &tok, &marker).await;
            if let Err(e) = &out {
                tracing::warn!(cluster = %c2, error = %e, "previews: wildcard certificate");
            }
            if let Err(e) = issued(&s2, &c2, &t2, &d2, out).await {
                tracing::warn!(cluster = %c2, error = %e.message, "previews: keeping the wildcard certificate");
            }
        });
        return Ok(());
    }
    if pd != before {
        save(s, cluster, &pd).await?;
    }
    Ok(())
}

async fn save(s: &AppState, cluster: &str, pd: &Value) -> ApiResult<()> {
    // Only while it's still the same domain (it may have been changed).
    sqlx::query("UPDATE pyo.clusters SET preview_domain = $2 WHERE id = $1 AND preview_domain->>'domain' = $2->>'domain'")
        .bind(cluster)
        .bind(pd)
        .execute(&s.db)
        .await?;
    Ok(())
}

/// Gets `*.<domain>` from Let's Encrypt with a DNS-01 challenge in the zone.
async fn issue(
    s: &AppState,
    _cluster: &str,
    _team: &str,
    domain: &str,
    zone_id: &str,
    token: &str,
    marker: &str,
) -> Result<crate::certs::Parsed, String> {
    use crate::sandbox::edgecert;
    let client = crate::outbound::client(
        crate::outbound::Reach::installation(),
        Duration::from_secs(30),
    )
    .map_err(|e| e.to_string())?;
    let account = edgecert::account(s, client, &edgecert::directory_url())
        .await
        .map_err(|e| e.message)?;
    let cf = cloudflare::Http::new(token).map_err(|e| e.message)?;
    let doh = crate::outbound::client(
        crate::outbound::Reach { private: false },
        Duration::from_secs(10),
    )
    .map_err(|e| e.to_string())?;
    let retry = RetryPolicy::new()
        .initial_delay(Duration::from_secs(2))
        .timeout(Duration::from_secs(180));
    let got = edgecert::obtain(
        &account,
        &cf,
        zone_id,
        marker,
        domain,
        &edgecert::DnsOverHttps(doh),
        &retry,
    )
    .await?;
    edgecert::check(&got, domain, pyo_proto::now_ms() / 1000)
}

/// Keeps a wildcard certificate with the team's certificates (the same one
/// renewed in place), and points the cluster's previews under the domain
/// at it, live. Or keeps why it failed.
async fn issued(
    s: &AppState,
    cluster: &str,
    team: &str,
    domain: &str,
    out: Result<crate::certs::Parsed, String>,
) -> ApiResult<()> {
    let mut tx = s.db.begin().await?;
    let mut pd = stored(&mut tx, cluster).await?;
    if pd["domain"].as_str() != Some(domain) {
        return Ok(());
    }
    let now = pyo_proto::now_ms();
    let p = match out {
        Ok(p) => p,
        Err(e) => {
            pd["certificate"] = json!({ "state": "failed", "at": now, "error": e, "notAfter": pd["certificate"]["notAfter"], "notBefore": pd["certificate"]["notBefore"] });
            sqlx::query("UPDATE pyo.clusters SET preview_domain = $2 WHERE id = $1")
                .bind(cluster)
                .bind(&pd)
                .execute(&mut *tx)
                .await?;
            tx.commit().await?;
            return Ok(());
        }
    };
    let sealed = crate::vault::get()
        .seal_credential(team, &p.key_pem)
        .await?;
    let existing: Option<String> = match pd["certificateId"].as_str() {
        Some(id) => {
            sqlx::query_scalar("SELECT id FROM pyo.certificates WHERE id = $1 AND team_id = $2")
                .bind(id)
                .bind(team)
                .fetch_optional(&mut *tx)
                .await?
        }
        None => None,
    };
    let id = existing.unwrap_or_else(|| crate::ids::new_id("crt"));
    sqlx::query(
        "INSERT INTO pyo.certificates (id, team_id, name, hostnames, issuer, not_before, not_after, fingerprint, chain_pem, key_sealed)
         VALUES ($1, $2, $3, $4, $5, to_timestamp($6), to_timestamp($7), $8, $9, $10)
         ON CONFLICT (id) DO UPDATE SET hostnames = $4, issuer = $5, not_before = to_timestamp($6), not_after = to_timestamp($7),
           fingerprint = $8, chain_pem = $9, key_sealed = $10, warned_days = NULL, updated_at = now()",
    )
    .bind(&id)
    .bind(team)
    .bind(format!("Previews (*.{domain})"))
    .bind(&p.hostnames)
    .bind(&p.issuer)
    .bind(p.not_before as f64)
    .bind(p.not_after as f64)
    .bind(&p.fingerprint)
    .bind(&p.chain_pem)
    .bind(&sealed)
    .execute(&mut *tx)
    .await?;
    pd["certificateId"] = json!(id);
    pd["certificate"] = json!({ "state": "issued", "at": now, "notBefore": p.not_before * 1000, "notAfter": p.not_after * 1000, "error": null });
    sqlx::query("UPDATE pyo.clusters SET preview_domain = $2 WHERE id = $1")
        .bind(cluster)
        .bind(&pd)
        .execute(&mut *tx)
        .await?;
    // The copies under the domain: on the wildcard certificate, live.
    let copies: Vec<(String, Value)> = sqlx::query_as(
        "SELECT id, coalesce(spec->'domains', '[]') FROM pyo.services WHERE cluster_id = $1 AND preview_of IS NOT NULL AND NOT deleting",
    )
    .bind(cluster)
    .fetch_all(&mut *tx)
    .await?;
    let mut wake = vec![];
    for (copy, domains) in copies {
        let mut moved = Value::Array(vec![]);
        let mut changed = false;
        for d in domains.as_array().into_iter().flatten() {
            let host = d["hostname"].as_str().unwrap_or("");
            let mut d = d.clone();
            // Under the domain: on the certificate (and a renewed one is
            // fetched again with the sync).
            if host.ends_with(&format!(".{domain}")) {
                d["tlsMode"] = json!("custom");
                d["certificateId"] = json!(id);
                changed = true;
            }
            if let Some(a) = moved.as_array_mut() {
                a.push(d);
            }
        }
        if !changed {
            continue;
        }
        sqlx::query(
            "UPDATE pyo.services SET spec = jsonb_set(spec, '{domains}', $2) WHERE id = $1",
        )
        .bind(&copy)
        .bind(&moved)
        .execute(&mut *tx)
        .await?;
        if let Some(c) = crate::routes::changed(&mut tx, &copy, true).await? {
            wake.push(c);
        }
    }
    log_activity(
        &mut tx,
        team,
        "Pyo",
        "domain.certificate_replaced",
        "success",
        &format!("Got a certificate for *.{domain}"),
        json!({ "kind": "certificate", "id": id, "label": format!("*.{domain}") }),
        json!({}),
    )
    .await?;
    tx.commit().await?;
    for c in wake {
        s.wake(&c).await;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preview_domains_as_typed() {
        assert_eq!(
            parse(" Preview.Example.com. ").unwrap(),
            "preview.example.com"
        );
        assert_eq!(
            parse("*.preview.example.com").unwrap(),
            "preview.example.com"
        );
        assert!(parse("localhost").is_err());
        assert!(parse("x.203.0.113.7.nip.io").is_err());
        assert!(parse("tail1234.ts.net").is_err());
        assert!(parse("bad_domain!.com").is_err());
    }

    #[test]
    fn shown_with_the_record_to_make() {
        let ip: Ipv4Addr = "203.0.113.7".parse().unwrap();
        assert_eq!(show(&Value::Null, Some(ip)), Value::Null);
        let v = show(&json!({ "domain": "preview.example.com" }), Some(ip));
        assert_eq!(v["record"]["name"], "*.preview.example.com");
        assert_eq!(v["record"]["content"], "203.0.113.7");
        assert_eq!(v["dns"], "manual");
        assert_eq!(v["certificate"], Value::Null);
        let v = show(
            &json!({ "domain": "preview.example.com", "dns": "cloudflare", "certificate": { "state": "issued", "notAfter": 5, "at": 1 } }),
            None,
        );
        assert_eq!(v["dns"], "cloudflare");
        assert_eq!(v["certificate"]["state"], "issued");
        assert_eq!(v["record"]["content"], Value::Null);
    }

    #[test]
    fn previews_in_tunnel_mode_go_under_the_default_domain() {
        use crate::freedomain::Cluster;
        let server = json!({ "name": "vps", "status": "ready", "internalIP": "10.0.0.5", "addresses": ["10.0.0.5"],
                             "publicIpv4": "203.0.113.7", "nicPublicIps": ["203.0.113.7"] });
        let public = Cluster {
            public_traffic: true,
            servers: vec![server],
            ..Default::default()
        };
        assert_eq!(public_suffix(&public, None).unwrap(), "203.0.113.7.nip.io");
        assert_eq!(public_suffix(&public, Some("preview.example.com")).unwrap(), "preview.example.com");
        // Tunnel mode: under the default domain, whatever the preview domain.
        let tunnel = Cluster {
            tunnel: true,
            tunnel_domain: Some("example.com".into()),
            ..Default::default()
        };
        assert_eq!(public_suffix(&tunnel, Some("preview.example.com")).unwrap(), "example.com");
        assert!(crate::cloudflare::tunnel_loop::under_default(
            &naming::host(&naming::label("web", 42, naming::LABEL_MAX), "example.com"),
            "example.com"
        ));
        // Without one: no preview URL, and why.
        let bare = Cluster {
            tunnel: true,
            ..Default::default()
        };
        assert_eq!(public_suffix(&bare, None).unwrap_err(), PICK_DEFAULT_DOMAIN);
        assert!(!PICK_DEFAULT_DOMAIN.contains(';'));
    }

    #[test]
    fn new_domains_redirect_only_where_the_internet_reaches() {
        let d = new_domain("web-pr-42.203.0.113.7.nip.io", "http");
        assert_eq!(d["redirectHttps"], true);
        assert_eq!(d["tlsMode"], "auto");
        assert_eq!(d["status"], "pending_dns");
        assert!(d["id"].as_str().unwrap().starts_with("dom_"));
        assert_eq!(
            new_domain("web-pr-42.100.64.0.9.nip.io", "http")["redirectHttps"],
            false
        );
    }
}
