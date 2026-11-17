//! Pyo's dashboard on the public IP: `https://pyo.<public IP>.nip.io` on
//! the cluster Pyo runs on, with Let's Encrypt's certificate (the agent's
//! publicdash.rs), so one server needs no Tailscale. An admin can close it
//! once Pyo is on the tailnet; removing Tailscale opens it again.
//!
//! Behind a cloud's NAT (the public IP mapped to the server, from the
//! cloud's metadata), Let's Encrypt is the check that the internet reaches
//! that IP: Pyo asks for the certificate there as well, and Let's Encrypt
//! issues it only once it reached the server on port 80 from outside. The
//! server never checks its own public IP (a cloud may not send its own
//! requests back in). Issued, the IP counts as the server's own: free
//! domains go there too ([`reached`], freedomain.rs). Refused (ports 80
//! and 443 closed in a cloud firewall), cert-manager tries again an hour
//! later, then less often, at most every few hours (the agent's
//! publicdash.rs): ports opened later are picked up by themselves.

use std::{net::Ipv4Addr, time::Duration};

use serde_json::{Value, json};
use sqlx::PgPool;

use crate::AppState;
use crate::error::{ApiError, ApiResult};

/// Whether an admin closed it.
pub async fn closed(db: &PgPool) -> ApiResult<bool> {
    let v: Option<bool> = sqlx::query_scalar(
        "SELECT (value->>'closed')::boolean FROM pyo.settings WHERE key = 'public_dashboard'",
    )
    .fetch_optional(db)
    .await?
    .flatten();
    Ok(v.unwrap_or(false))
}

/// Opens or closes it. Closing needs Pyo on the tailnet, so nobody is
/// locked out.
pub async fn set_open(db: &PgPool, open: bool) -> ApiResult<()> {
    if !open && crate::web::addresses().tailnet.is_none() {
        return Err(ApiError::conflict(
            "Pyo isn't on your tailnet yet, so closing this would lock you out.",
        ));
    }
    sqlx::query(
        "INSERT INTO pyo.settings (key, value) VALUES ('public_dashboard', jsonb_build_object('closed', $1))
         ON CONFLICT (key) DO UPDATE SET value = excluded.value, updated_at = now()",
    )
    .bind(!open)
    .execute(db)
    .await?;
    if !open {
        crate::tailscale::save_address(db, "public", None).await?;
    }
    Ok(())
}

/// Its name while it's open: "pyo.<public IP>.nip.io" of the cluster Pyo
/// runs on, when that takes public traffic and its free domains are on
/// its public IP, or it's behind the cloud's NAT (freedomain.rs
/// `dashboard_ip`).
pub async fn host(db: &PgPool) -> ApiResult<Option<String>> {
    if closed(db).await? {
        return Ok(None);
    }
    let id: Option<String> =
        sqlx::query_scalar("SELECT id FROM pyo.clusters WHERE same_cluster LIMIT 1")
            .fetch_optional(db)
            .await?;
    let Some(id) = id else { return Ok(None) };
    let mut tx = db.begin().await?;
    let c = crate::freedomain::load(&mut tx, &id).await?;
    tx.commit().await?;
    // Not behind a home router or a shared gateway (only what the internet
    // sees of the server's requests): Let's Encrypt can't reach it there.
    Ok(c.and_then(|(_, c)| c.dashboard_ip())
        .and_then(|ip| pyo_proto::system::public_dashboard_host(&ip.to_string())))
}

/// Let's Encrypt's last word on the dashboard's certificate, as kept:
/// the public IP and whether it was issued ("ready") or refused ("failed").
async fn reach<'e, E: sqlx::PgExecutor<'e>>(e: E) -> ApiResult<Option<(Ipv4Addr, String)>> {
    let v: Option<Value> =
        sqlx::query_scalar("SELECT value FROM pyo.settings WHERE key = 'public_reach'")
            .fetch_optional(e)
            .await?;
    Ok(v.and_then(|v| {
        let ip = v["ip"].as_str()?.parse().ok()?;
        Some((ip, v["state"].as_str()?.to_owned()))
    }))
}

/// The public IP Let's Encrypt reached: it issued the dashboard's
/// certificate there.
pub async fn reached<'e, E: sqlx::PgExecutor<'e>>(e: E) -> ApiResult<Option<Ipv4Addr>> {
    Ok(reach(e)
        .await?
        .filter(|(_, state)| state == "ready")
        .map(|(ip, _)| ip))
}

/// What a heartbeat of the cluster Pyo runs on says of the dashboard's
/// certificate: issued or refused is kept (a try on its way changes
/// nothing).
pub fn outcome(report: &pyo_proto::CertificateReport) -> Option<(Ipv4Addr, &'static str)> {
    let ip = crate::freedomain::ip_in(&report.hostname)?;
    if report.ready {
        Some((ip, "ready"))
    } else if report.failed {
        Some((ip, "failed"))
    } else {
        None
    }
}

/// Keeps what a heartbeat of the cluster Pyo runs on says of the
/// dashboard's certificate (see [`outcome`]).
pub async fn reported(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    report: Option<&pyo_proto::CertificateReport>,
) -> ApiResult<()> {
    let Some((ip, state)) = report.and_then(outcome) else {
        return Ok(());
    };
    if reach(&mut **tx)
        .await?
        .is_some_and(|(i, s)| i == ip && s == state)
    {
        return Ok(());
    }
    tracing::info!(%ip, state, "public dashboard's certificate");
    sqlx::query(
        "INSERT INTO pyo.settings (key, value) VALUES ('public_reach', jsonb_build_object('ip', $1::text, 'state', $2::text))
         ON CONFLICT (key) DO UPDATE SET value = excluded.value, updated_at = now()",
    )
    .bind(ip.to_string())
    .bind(state)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// The heartbeat reply's `publicDashboard`, for the cluster Pyo runs on.
pub async fn wanted_for(db: &PgPool, same_cluster: bool) -> ApiResult<Option<Value>> {
    if !same_cluster {
        return Ok(None);
    }
    Ok(Some(json!({ "host": host(db).await? })))
}

/// The leader, every round: Pyo's public address once its certificate is
/// issued (behind the cloud's NAT) or it answers over HTTPS with a valid
/// certificate, none while it's closed.
pub async fn tick(s: &AppState) -> ApiResult<()> {
    let want = host(&s.db).await?;
    let have = crate::web::addresses().public;
    match want {
        None if have.is_some() => crate::tailscale::save_address(&s.db, "public", None).await?,
        Some(h)
            if have.as_deref() != Some(h.as_str())
                && (issued(&s.db, &h).await? || answers(&h).await) =>
        {
            tracing::info!(host = %h, "public dashboard ready");
            crate::tailscale::save_address(&s.db, "public", Some(&h)).await?;
        }
        _ => {}
    }
    Ok(())
}

/// Let's Encrypt issued the certificate of `host` ("pyo.<ip>.nip.io").
async fn issued(db: &PgPool, host: &str) -> ApiResult<bool> {
    let ip = crate::freedomain::ip_in(host);
    Ok(ip.is_some() && reached(db).await? == ip)
}

async fn answers(host: &str) -> bool {
    let Ok(c) = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
    else {
        return false;
    };
    c.get(format!("https://{host}/healthz"))
        .send()
        .await
        .is_ok_and(|r| r.status().is_success())
}

/// For the dashboard (admins): its name, whether it's open and ready, and
/// whether Let's Encrypt couldn't reach the public IP (`blocked`: ports 80
/// and 443 closed in a cloud firewall).
pub async fn snapshot(db: &PgPool) -> ApiResult<Value> {
    let closed = closed(db).await?;
    let wanted = host(db).await?;
    // Behind a router's NAT (or without public traffic) there's none to
    // open or close.
    let possible = closed || wanted.is_some();
    let ip = wanted.as_deref().and_then(crate::freedomain::ip_in);
    let blocked = ip.is_some()
        && reach(db)
            .await?
            .is_some_and(|(i, s)| Some(i) == ip && s == "failed");
    Ok(json!({
        "open": !closed,
        "host": crate::web::addresses().public,
        "possible": possible,
        "blocked": blocked,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cert(host: &str, ready: bool, failed: bool) -> pyo_proto::CertificateReport {
        pyo_proto::CertificateReport {
            service_id: String::new(),
            hostname: host.into(),
            ready,
            message: String::new(),
            failed,
            failure: String::new(),
            issuing_since: None,
        }
    }

    /// Issued or refused is what counts; a try on its way says nothing.
    #[test]
    fn lets_encrypt_has_the_last_word() {
        let ip: Ipv4Addr = "43.156.1.2".parse().unwrap();
        assert_eq!(
            outcome(&cert("pyo.43.156.1.2.nip.io", true, false)),
            Some((ip, "ready"))
        );
        assert_eq!(
            outcome(&cert("pyo.43.156.1.2.nip.io", false, true)),
            Some((ip, "failed"))
        );
        assert_eq!(outcome(&cert("pyo.43.156.1.2.nip.io", false, false)), None);
        assert_eq!(outcome(&cert("pyo.example.com", true, false)), None);
    }
}
