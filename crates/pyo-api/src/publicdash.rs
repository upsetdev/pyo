//! Pyo's dashboard on the public IP: `https://pyo.<public IP>.nip.io` on
//! the cluster Pyo runs on, with Let's Encrypt's certificate (the agent's
//! publicdash.rs), so one server needs no Tailscale. An admin can close it
//! once Pyo is on the tailnet; removing Tailscale opens it again.

use std::time::Duration;

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
/// its public IP.
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
    // Only where its free domains are public too: behind NAT (free domains
    // on Tailscale) Let's Encrypt can't reach it, and every try would count
    // against its limits.
    Ok(
        c.filter(|(_, c)| c.public_traffic && c.mode() == crate::freedomain::Mode::Public)
            .and_then(|(_, c)| c.address())
            .and_then(|ip| pyo_proto::system::public_dashboard_host(&ip.to_string())),
    )
}

/// The heartbeat reply's `publicDashboard`, for the cluster Pyo runs on.
pub async fn wanted_for(db: &PgPool, same_cluster: bool) -> ApiResult<Option<Value>> {
    if !same_cluster {
        return Ok(None);
    }
    Ok(Some(json!({ "host": host(db).await? })))
}

/// The leader, every round: Pyo's public address once it answers over
/// HTTPS with a valid certificate, none while it's closed.
pub async fn tick(s: &AppState) -> ApiResult<()> {
    let want = host(&s.db).await?;
    let have = crate::web::addresses().public;
    match want {
        None if have.is_some() => crate::tailscale::save_address(&s.db, "public", None).await?,
        Some(h) if have.as_deref() != Some(h.as_str()) && answers(&h).await => {
            tracing::info!(host = %h, "public dashboard ready");
            crate::tailscale::save_address(&s.db, "public", Some(&h)).await?;
        }
        _ => {}
    }
    Ok(())
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

/// For the dashboard (admins): its name, and whether it's open and ready.
pub async fn snapshot(db: &PgPool) -> ApiResult<Value> {
    let closed = closed(db).await?;
    // Behind NAT (or without public traffic) there's none to open or close.
    let possible = closed || host(db).await?.is_some();
    Ok(json!({
        "open": !closed,
        "host": crate::web::addresses().public,
        "possible": possible,
    }))
}
