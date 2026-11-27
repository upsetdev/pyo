//! The tunnel loop (docs/cloudflare-tunnel.md §6.3): every 30 seconds on
//! the leader, each cluster tunnel is brought to what Pyo wants. The
//! tunnel exists at Cloudflare, its routes are the cluster's tunnel
//! domains, its certificate covers them, their records point at it (or
//! back), the agent has its token, and Cloudflare's view of it is read
//! every 2 minutes. A tunnel being removed is taken apart in order.
//!
//! Every step touches Cloudflare only when something differs. Errors are
//! kept on the tunnel (`error`) or the record (`dns_records.error`) in
//! Cloudflare's words, never with the token.
//!
//! The parts that talk to Cloudflare are generic over [`Cloudflare`] and
//! [`TunnelApi`], so the tests run them against the fake account.

use std::time::Duration;

use serde_json::{Value, json};
use sqlx::{Postgres, Transaction};

use super::{
    CfError, Cloudflare, Http, NewRecord, Record, delete_ours, install_id, marker, ours,
    same_name,
    tunnel::{self, Kept, OriginCert, Tunnel, TunnelApi},
    zone_for,
};
use crate::{AppState, error::ApiResult, ops::log_activity};

/// How often the loop runs.
const EVERY: Duration = Duration::from_secs(30);
/// Down this long before the alert goes out.
const DOWN_ALERT_SECS: i64 = 300;
/// How long removal waits for the agent to stop the connector.
const REMOVE_WAIT_SECS: i64 = 600;

/// The loop, on the leader.
pub async fn run(state: AppState) {
    let mut tick = tokio::time::interval(EVERY);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tick.tick().await;
        if let Some(Err(e)) =
            crate::bg::as_leader(&state.db, crate::bg::LOCK_TUNNEL, || pass(&state)).await
        {
            tracing::warn!(error = ?e.message, "tunnel loop failed");
        }
    }
}

/// One pass over every tunnel, one at a time.
pub async fn pass(s: &AppState) -> ApiResult<()> {
    let ids: Vec<String> = sqlx::query_scalar(
        "SELECT cluster_id FROM pyo.cluster_tunnels
          WHERE backoff_until IS NULL OR backoff_until < now() ORDER BY cluster_id",
    )
    .fetch_all(&s.db)
    .await?;
    for id in ids {
        if let Err(e) = one(s, &id).await {
            tracing::warn!(cluster = %id, error = ?e.message, "tunnel pass failed");
        }
    }
    leftovers(s).await
}

/// What's left at Cloudflare of a deleted service's tunnel domain or a
/// deleted cluster's tunnel (the migration's triggers keep them): only
/// Pyo's own records are deleted, then the tunnel and its certificates.
#[derive(sqlx::FromRow, Debug)]
struct Leftover {
    id: i64,
    team_id: String,
    kind: String,
    zone_id: Option<String>,
    record_id: Option<String>,
    hostname: Option<String>,
    account_id: Option<String>,
    tunnel_id: Option<String>,
    cf_certificates: Vec<String>,
}

async fn leftovers(s: &AppState) -> ApiResult<()> {
    let rows: Vec<Leftover> = sqlx::query_as(
        "SELECT id, team_id, kind, zone_id, record_id, hostname, account_id, tunnel_id, cf_certificates
           FROM pyo.tunnel_leftovers ORDER BY id LIMIT 50",
    )
    .fetch_all(&s.db)
    .await?;
    for l in rows {
        let mut tx = s.db.begin().await?;
        let account = super::account(s, &mut tx, &l.team_id).await.ok().flatten();
        let mark = marker(&install_id(&mut tx).await?);
        tx.commit().await?;
        let Some(account) = account else {
            sqlx::query("UPDATE pyo.tunnel_leftovers SET error = $2 WHERE id = $1")
                .bind(l.id)
                .bind(TOKEN_GONE)
                .execute(&s.db)
                .await?;
            continue;
        };
        let done = clean_up(&Http::new(&account.token)?, &l, &mark).await;
        match done {
            Ok(()) => {
                sqlx::query("DELETE FROM pyo.tunnel_leftovers WHERE id = $1")
                    .bind(l.id)
                    .execute(&s.db)
                    .await?;
            }
            Err(e) => {
                sqlx::query("UPDATE pyo.tunnel_leftovers SET error = $2 WHERE id = $1")
                    .bind(l.id)
                    .bind(e.to_string())
                    .execute(&s.db)
                    .await?;
            }
        }
    }
    Ok(())
}

/// One leftover, taken apart. Gone already counts as done.
async fn clean_up<C: Cloudflare + TunnelApi>(cf: &C, l: &Leftover, mark: &str) -> Result<(), CfError> {
    let gone = |r: Result<(), CfError>| match r {
        Ok(()) | Err(CfError::NotFound) => Ok(()),
        Err(e) => Err(e),
    };
    if let (Some(zone), Some(id), Some(host)) = (&l.zone_id, &l.record_id, &l.hostname) {
        delete_ours(cf, zone, id, host, mark).await?;
    }
    if l.kind == "tunnel"
        && let (Some(account), Some(id)) = (&l.account_id, &l.tunnel_id)
    {
        match cf.put_config(account, id, &tunnel::empty_ingress()).await {
            Ok(_) | Err(CfError::NotFound) => {}
            Err(e) => return Err(e),
        }
        gone(cf.drop_connections(account, id).await)?;
        gone(cf.delete_tunnel(account, id).await)?;
    }
    for c in &l.cf_certificates {
        gone(cf.revoke_origin_cert(c).await)?;
    }
    Ok(())
}

/// A cluster's tunnel as stored, with what's due.
#[derive(sqlx::FromRow, Debug, Clone, Default)]
pub(crate) struct Row {
    pub cluster_id: String,
    pub team_id: String,
    pub account_id: String,
    pub tunnel_id: Option<String>,
    pub tunnel_name: String,
    pub state: String,
    pub default_domain: Option<String>,
    pub default_zone_id: Option<String>,
    pub default_record_id: Option<String>,
    pub config_hash: String,
    pub config_version: Option<i32>,
    pub certificate_id: Option<String>,
    pub cf_certificate_id: Option<String>,
    pub cf_certificate_old: Option<String>,
    pub rotate: bool,
    pub restore_records: bool,
    pub agent_status: Value,
    pub down_alerted: bool,
    /// Computed: the old certificate may be revoked.
    pub old_due: bool,
    /// Computed: the token may go to the agent again.
    pub token_due: bool,
    /// Computed: Cloudflare's view is due.
    pub check_due: bool,
    /// Computed: seconds Cloudflare has said it's down.
    pub down_secs: Option<i64>,
    /// Computed: seconds since removal was asked for.
    pub removing_secs: i64,
    /// Computed: made in the last 5 minutes.
    pub young: bool,
}

const ROW: &str = "SELECT cluster_id, team_id, account_id, tunnel_id, tunnel_name, state, default_domain, default_zone_id,
        default_record_id, config_hash, config_version, certificate_id, cf_certificate_id, cf_certificate_old, rotate,
        restore_records, agent_status, down_alerted,
        coalesce(cf_certificate_old_at < now(), false) AS old_due,
        (token_sent_at IS NULL OR token_sent_at < now() - interval '5 minutes') AS token_due,
        (checked_at IS NULL OR checked_at < now() - interval '2 minutes') AS check_due,
        extract(epoch FROM now() - down_since)::bigint AS down_secs,
        extract(epoch FROM now() - updated_at)::bigint AS removing_secs,
        (created_at > now() - interval '5 minutes') AS young
   FROM pyo.cluster_tunnels WHERE cluster_id = $1";

pub(crate) async fn load(
    db: impl sqlx::PgExecutor<'_>,
    cluster_id: &str,
) -> ApiResult<Option<Row>> {
    Ok(sqlx::query_as::<_, Row>(ROW)
        .bind(cluster_id)
        .fetch_optional(db)
        .await?)
}

/// Keep an error on the tunnel (or clear it with None).
async fn set_error(s: &AppState, cluster_id: &str, error: Option<String>) -> ApiResult<()> {
    sqlx::query("UPDATE pyo.cluster_tunnels SET error = $2 WHERE cluster_id = $1 AND error IS DISTINCT FROM $2")
        .bind(cluster_id)
        .bind(error)
        .execute(&s.db)
        .await?;
    Ok(())
}

/// Cloudflare asked to slow down: this team's tunnels wait a minute.
async fn back_off(s: &AppState, team_id: &str) -> ApiResult<()> {
    sqlx::query("UPDATE pyo.cluster_tunnels SET backoff_until = now() + interval '60 seconds' WHERE team_id = $1")
        .bind(team_id)
        .execute(&s.db)
        .await?;
    Ok(())
}

/// Cloudflare's 429.
fn slow_down(e: &CfError) -> bool {
    matches!(e, CfError::Refused(m) if m.contains("429") || m.to_lowercase().contains("rate limit"))
}

/// Said when the team's token is gone or refused.
pub const TOKEN_GONE: &str = "Connect Cloudflare again to change the tunnel. Its websites keep working.";

/// One tunnel, one pass.
async fn one(s: &AppState, cluster_id: &str) -> ApiResult<()> {
    let Some(row) = load(&s.db, cluster_id).await? else {
        return Ok(());
    };
    let mut tx = s.db.begin().await?;
    let account = super::account(s, &mut tx, &row.team_id).await.ok().flatten();
    let mark = marker(&install_id(&mut tx).await?);
    tx.commit().await?;
    let Some(account) = account else {
        return set_error(s, cluster_id, Some(TOKEN_GONE.into())).await;
    };
    let cf = Http::new(&account.token)?;
    let r = if row.state == "removing" {
        remove(s, &cf, &row, &account.zones, &mark).await
    } else {
        keep(s, &cf, &row, &account.zones, &mark).await
    };
    match r {
        Ok(()) => Ok(()),
        Err(Step::Cf(e)) if slow_down(&e) => back_off(s, &row.team_id).await,
        Err(Step::Cf(e @ CfError::Denied(_))) => {
            set_error(s, cluster_id, Some(format!("{e} {TOKEN_GONE}"))).await
        }
        Err(Step::Cf(e)) => set_error(s, cluster_id, Some(e.to_string())).await,
        Err(Step::Api(e)) => Err(e),
    }
}

/// Why a step stopped: Cloudflare said no, or Pyo's own trouble.
#[derive(Debug)]
pub(crate) enum Step {
    Cf(CfError),
    Api(crate::error::ApiError),
}

impl From<CfError> for Step {
    fn from(e: CfError) -> Self {
        Step::Cf(e)
    }
}

impl From<crate::error::ApiError> for Step {
    fn from(e: crate::error::ApiError) -> Self {
        Step::Api(e)
    }
}

impl From<anyhow::Error> for Step {
    fn from(e: anyhow::Error) -> Self {
        Step::Api(e.into())
    }
}

impl From<sqlx::Error> for Step {
    fn from(e: sqlx::Error) -> Self {
        Step::Api(e.into())
    }
}

/// A tunnel domain of the cluster, as its service has it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct TunnelDomain {
    pub service_id: String,
    pub domain_id: String,
    pub host: String,
    /// "switching" or "on".
    pub tunnel: String,
    pub zone_id: String,
    pub zone_name: String,
}

/// The cluster's domains that go through its tunnel, with their zones (a
/// domain no connected zone covers is left out: it can't be routed).
async fn tunnel_domains(
    db: impl sqlx::PgExecutor<'_>,
    cluster_id: &str,
    zones: &[super::Zone],
) -> ApiResult<Vec<TunnelDomain>> {
    let rows: Vec<(String, String, String, String)> = sqlx::query_as(
        "SELECT a.id, d->>'id', lower(d->>'hostname'), d->>'tunnel'
           FROM pyo.services a, jsonb_array_elements(CASE WHEN jsonb_typeof(a.spec->'domains') = 'array' THEN a.spec->'domains' ELSE '[]' END) d
          WHERE a.cluster_id = $1 AND NOT a.deleting AND d->>'tunnel' IN ('switching', 'on')
          ORDER BY 3",
    )
    .bind(cluster_id)
    .fetch_all(db)
    .await?;
    Ok(rows
        .into_iter()
        .filter_map(|(service_id, domain_id, host, tunnel)| {
            let z = zone_for(&host, zones)?;
            Some(TunnelDomain {
                service_id,
                domain_id,
                host,
                tunnel,
                zone_id: z.id.clone(),
                zone_name: z.name.clone(),
            })
        })
        .collect())
}

// ── Keeping a tunnel ───────────────────────────────────────────────────────

async fn keep<C: Cloudflare + TunnelApi>(
    s: &AppState,
    cf: &C,
    row: &Row,
    zones: &[super::Zone],
    mark: &str,
) -> Result<(), Step> {
    let install = mark
        .trim_start_matches("Made by Pyo (")
        .trim_end_matches(')')
        .to_owned();
    // 1. The tunnel itself.
    let (t, recreated) = ensure_tunnel(cf, row, &install).await?;
    if row.tunnel_id.as_deref() != Some(t.id.as_str()) {
        sqlx::query(
            "UPDATE pyo.cluster_tunnels SET tunnel_id = $2, tunnel_name = $3, state = 'ready', config_hash = '', token_sent_at = NULL,
                    default_record_id = NULL, updated_at = now() WHERE cluster_id = $1",
        )
        .bind(&row.cluster_id)
        .bind(&t.id)
        .bind(&t.name)
        .execute(&s.db)
        .await?;
        if recreated {
            // Its records point at the old one: they move to the new one.
            sqlx::query(
                "UPDATE pyo.dns_records SET status = 'switching', updated_at = now()
                  WHERE via = 'tunnel' AND status = 'created'
                    AND service_id IN (SELECT id FROM pyo.services WHERE cluster_id = $1)",
            )
            .bind(&row.cluster_id)
            .execute(&s.db)
            .await?;
            let mut tx = s.db.begin().await?;
            log_activity(
                &mut tx,
                &row.team_id,
                "Pyo",
                "cluster.tunnel_recreated",
                "warning",
                "The Cloudflare Tunnel was gone, so Pyo made a new one",
                json!({ "kind": "cluster", "id": row.cluster_id, "label": t.name }),
                json!({}),
            )
            .await?;
            tx.commit().await?;
        }
    }
    let target = tunnel::cname_target(&t.id);
    // Free domains under the default domain: through the tunnel's wildcard.
    if let Some(d) = &row.default_domain {
        free_domains_follow(s, &row.cluster_id, d, row.default_record_id.is_some()).await?;
    }
    let domains = tunnel_domains(&s.db, &row.cluster_id, zones).await?;

    // 2. Its routes: every tunnel domain, the free ones under the default
    // domain among them. No wildcard route: a name no service has gets
    // the tunnel's own 404 (see `tunnel::ingress`).
    let hosts: Vec<String> = domains.iter().map(|d| d.host.clone()).collect();
    let config = tunnel::ingress(&hosts);
    let hash = tunnel::config_hash(&config);
    if hash != row.config_hash || recreated {
        let version = cf.put_config(&row.account_id, &t.id, &config).await?;
        sqlx::query("UPDATE pyo.cluster_tunnels SET config_hash = $2, config_version = $3 WHERE cluster_id = $1")
            .bind(&row.cluster_id)
            .bind(&hash)
            .bind(version as i32)
            .execute(&s.db)
            .await?;
    }

    // 3. Its certificate: every name covered.
    let pairs: Vec<(String, String)> = domains
        .iter()
        .map(|d| (d.host.clone(), d.zone_name.clone()))
        .collect();
    let certificate_id = match tunnel::cert_names(&pairs, row.default_domain.as_deref()) {
        Ok(names) => ensure_certificate(s, cf, row, &names).await?,
        Err(e) => {
            set_error(s, &row.cluster_id, Some(e)).await?;
            row.certificate_id.clone()
        }
    };

    // 4. Its domains' certificates, and their records.
    if let Some(cert) = &certificate_id {
        tls_follow(s, &row.cluster_id, cert).await?;
    }
    records(s, cf, row, &target, zones, mark).await?;
    default_record(s, cf, row, &target, mark).await?;
    // Public ports that wait for domains to move close once they have, and
    // a tunnel turned off goes once its domains are back
    // (traffic_paths.rs).
    close_public(s, row).await?;
    remove_when_back(s, row).await?;

    // 5. The agent's token.
    let agent_has = row.agent_status["tunnelId"].as_str();
    if agent_has != Some(t.id.as_str()) && row.token_due && agent_knows_tunnels(s, &row.cluster_id).await? {
        send_token(s, cf, row, &t.id, false).await?;
    }

    // 6. Rotate: a new secret, the token for it sent right away, then the
    // old connections dropped. The token goes out here, not through step 5:
    // the tunnel's id doesn't change, and every heartbeat reports the id
    // the agent's token is for, so step 5 would never see it as missing.
    if row.rotate {
        cf.rotate_secret(&row.account_id, &t.id, &tunnel::new_secret())
            .await?;
        send_token(s, cf, row, &t.id, true).await?;
        cf.drop_connections(&row.account_id, &t.id).await?;
    }

    // 7. Cloudflare's view, every 2 minutes.
    if row.check_due {
        let now = cf.tunnel(&row.account_id, &t.id).await?;
        status(s, row, &now).await?;
        // Its routes changed outside Pyo: Pyo's go back.
        if let Ok((version, _)) = cf.config(&row.account_id, &t.id).await
            && row.config_version.is_some_and(|v| i64::from(v) != version)
        {
            sqlx::query("UPDATE pyo.cluster_tunnels SET config_hash = '' WHERE cluster_id = $1")
                .bind(&row.cluster_id)
                .execute(&s.db)
                .await?;
        }
    }

    // 8. The certificate it replaced, a day later.
    if row.old_due
        && let Some(old) = &row.cf_certificate_old
    {
        match cf.revoke_origin_cert(old).await {
            Ok(()) | Err(CfError::NotFound) => {}
            Err(e) => return Err(e.into()),
        }
        sqlx::query("UPDATE pyo.cluster_tunnels SET cf_certificate_old = NULL, cf_certificate_old_at = NULL WHERE cluster_id = $1")
            .bind(&row.cluster_id)
            .execute(&s.db)
            .await?;
    }
    set_error(s, &row.cluster_id, None).await?;
    Ok(())
}

/// Send the agent the tunnel's token (`TunnelSet`). After a rotation it
/// also clears `rotate`, in the same transaction, so a failed send rotates
/// again on the next pass rather than leaving the agent on the old secret.
async fn send_token<C: TunnelApi>(
    s: &AppState,
    cf: &C,
    row: &Row,
    tunnel_id: &str,
    rotated: bool,
) -> Result<(), Step> {
    let token = cf.tunnel_token(&row.account_id, tunnel_id).await?;
    let mut tx = s.db.begin().await?;
    crate::deploy::enqueue(
        &mut tx,
        &row.cluster_id,
        pyo_proto::Command::TunnelSet {
            tunnel_id: tunnel_id.to_owned(),
            token,
        },
    )
    .await?;
    let sql = if rotated {
        "UPDATE pyo.cluster_tunnels SET token_sent_at = now(), rotate = false WHERE cluster_id = $1"
    } else {
        "UPDATE pyo.cluster_tunnels SET token_sent_at = now() WHERE cluster_id = $1"
    };
    sqlx::query(sql)
        .bind(&row.cluster_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    s.wake(&row.cluster_id).await;
    Ok(())
}

/// Whether the cluster's agent runs tunnels (an older one would never
/// answer `TunnelSet`).
async fn agent_knows_tunnels(s: &AppState, cluster_id: &str) -> ApiResult<bool> {
    Ok(sqlx::query_scalar(
        "SELECT coalesce((capabilities->>'tunnel')::boolean, false) FROM pyo.clusters WHERE id = $1",
    )
    .bind(cluster_id)
    .fetch_optional(&s.db)
    .await?
    .unwrap_or(false))
}

/// The tunnel at Cloudflare: the one stored, or (none yet, or deleted at
/// Cloudflare) one made now. A tunnel this installation made earlier under
/// the same name (a pass that stopped halfway) is taken, never someone
/// else's. (tunnel, made again after it was gone).
pub(crate) async fn ensure_tunnel<C: TunnelApi>(
    cf: &C,
    row: &Row,
    install: &str,
) -> Result<(Tunnel, bool), CfError> {
    if let Some(id) = &row.tunnel_id {
        match cf.tunnel(&row.account_id, id).await {
            Ok(t) if !t.deleted => return Ok((t, false)),
            Ok(_) | Err(CfError::NotFound) => {}
            Err(e) => return Err(e),
        }
    }
    let base = tunnel::tunnel_name(&row.cluster_id, install, 0);
    let taken = cf.tunnels_named(&row.account_id, &base).await?;
    if row.tunnel_id.is_none()
        && let Some(t) = taken.iter().find(|t| t.name == base && !t.deleted)
    {
        return Ok((t.clone(), false));
    }
    let recreated = row.tunnel_id.is_some();
    for attempt in 1..=20 {
        let name = tunnel::tunnel_name(&row.cluster_id, install, attempt);
        if taken.iter().any(|t| t.name == name) {
            continue;
        }
        return Ok((cf.create_tunnel(&row.account_id, &name).await?, recreated));
    }
    Err(CfError::Refused(
        "Too many tunnels with this cluster's name. Delete the old ones in Cloudflare.".into(),
    ))
}

// ── Records ────────────────────────────────────────────────────────────────

/// What became of a switch's record.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Switched {
    /// The name points at the tunnel: Pyo's record, and the ones it
    /// replaced (kept to put back).
    Done { record: Record, previous: Vec<Kept> },
    /// Records Pyo didn't make answer for the name: these. Nothing changed.
    Confirm(Vec<Record>),
}

/// Point `host` at the tunnel with a proxied CNAME. Pyo's own records on
/// the name go without asking. Anyone else's stop it, unless `replace`:
/// then they go and come back as `previous`. Never touches records of
/// other names, or types other than A, AAAA and CNAME.
/// The records at `host` in the way of the tunnel's CNAME: A, AAAA and
/// CNAME records this installation didn't make (`mark`).
pub(crate) fn not_ours<'a>(found: &'a [Kept], host: &str, mark: &str) -> Vec<&'a Kept> {
    found
        .iter()
        .filter(|k| matches!(k.kind.as_str(), "A" | "AAAA" | "CNAME") && same_name(&k.name, host))
        .filter(|k| !ours(&k.record(), mark, host))
        .collect()
}

pub(crate) async fn switch_record<C: Cloudflare + TunnelApi>(
    cf: &C,
    zone_id: &str,
    host: &str,
    target: &str,
    replace: bool,
    mark: &str,
) -> Result<Switched, CfError> {
    let found = cf.records_kept(zone_id, host).await?;
    let at: Vec<&Kept> = found
        .iter()
        .filter(|k| matches!(k.kind.as_str(), "A" | "AAAA" | "CNAME") && same_name(&k.name, host))
        .collect();
    let have = at
        .iter()
        .find(|k| k.kind == "CNAME" && k.content.eq_ignore_ascii_case(target) && ours(&k.record(), mark, host))
        .map(|k| k.record());
    let others: Vec<&Kept> = at
        .iter()
        .copied()
        .filter(|k| have.as_ref().is_none_or(|h| h.id != k.id))
        .collect();
    let theirs: Vec<&Kept> = not_ours(&found, host, mark);
    if !theirs.is_empty() && !replace {
        return Ok(Switched::Confirm(theirs.iter().map(|k| k.record()).collect()));
    }
    for k in &others {
        match cf.delete(zone_id, &k.id).await {
            Ok(()) | Err(CfError::NotFound) => {}
            Err(e) => return Err(e),
        }
    }
    let previous: Vec<Kept> = theirs.into_iter().cloned().collect();
    let record = match have {
        Some(r) if r.proxied => r,
        Some(r) => {
            cf.set_proxied(zone_id, &r.id, true).await?;
            Record { proxied: true, ..r }
        }
        None => {
            cf.create(
                zone_id,
                &NewRecord {
                    kind: "CNAME".into(),
                    name: host.to_owned(),
                    content: target.to_owned(),
                    proxied: true,
                    comment: mark.to_owned(),
                },
            )
            .await?
        }
    };
    Ok(Switched::Done { record, previous })
}

/// Point `host` back: Pyo's CNAME goes (only if it's still Pyo's), then
/// the kept records come back as they were. With none kept, `fallback`
/// (an A record to the cluster's public IP) is made instead, when given.
/// The record Pyo made, if any.
pub(crate) async fn revert_record<C: Cloudflare + TunnelApi>(
    cf: &C,
    zone_id: &str,
    host: &str,
    record_id: Option<&str>,
    previous: &[Kept],
    fallback: Option<&str>,
    mark: &str,
) -> Result<Option<Record>, CfError> {
    if let Some(id) = record_id {
        delete_ours(cf, zone_id, id, host, mark).await?;
    }
    for k in previous {
        cf.create_kept(zone_id, k).await?;
    }
    if !previous.is_empty() {
        return Ok(None);
    }
    let Some(ip) = fallback else {
        return Ok(None);
    };
    Ok(Some(
        cf.create(
            zone_id,
            &NewRecord {
                kind: "A".into(),
                name: host.to_owned(),
                content: ip.to_owned(),
                proxied: false,
                comment: mark.to_owned(),
            },
        )
        .await?,
    ))
}

/// A record row waiting for the loop.
#[derive(sqlx::FromRow, Debug)]
struct Waiting {
    service_id: String,
    domain_id: String,
    hostname: String,
    zone_id: String,
    record_id: Option<String>,
    status: String,
    previous: Value,
    replace: bool,
    restore: bool,
    /// The routes sync it waits for: done, failed or still on its way.
    routes: Option<String>,
    routes_error: Option<String>,
}

/// Rows switching to the tunnel or back, once the agent has the service's
/// routes for it (both paths answer then).
async fn records<C: Cloudflare + TunnelApi>(
    s: &AppState,
    cf: &C,
    row: &Row,
    target: &str,
    zones: &[super::Zone],
    mark: &str,
) -> Result<(), Step> {
    let waiting: Vec<Waiting> = sqlx::query_as(
        "SELECT r.service_id, r.domain_id, r.hostname, r.zone_id, r.record_id, r.status, r.previous, r.replace, r.restore,
                c.status AS routes, c.result->>'message' AS routes_error
           FROM pyo.dns_records r JOIN pyo.services a ON a.id = r.service_id
           LEFT JOIN pyo.commands c ON c.id = r.routes_command
          WHERE a.cluster_id = $1 AND NOT a.deleting AND r.status IN ('switching', 'reverting')
          ORDER BY r.hostname LIMIT 50",
    )
    .bind(&row.cluster_id)
    .fetch_all(&s.db)
    .await?;
    for w in waiting {
        // The same name on its way off another tunnel (a domain moved from
        // another service or cluster): that one first.
        if w.status == "switching" {
            let busy: bool = sqlx::query_scalar(
                "SELECT EXISTS (SELECT 1 FROM pyo.dns_records r WHERE lower(r.hostname) = lower($1) AND r.service_id <> $2
                                   AND (r.status = 'reverting' OR (r.via = 'tunnel' AND r.status = 'created')))
                     OR EXISTS (SELECT 1 FROM pyo.tunnel_leftovers l WHERE lower(l.hostname) = lower($1))",
            )
            .bind(&w.hostname)
            .bind(&w.service_id)
            .fetch_one(&s.db)
            .await?;
            if busy {
                continue;
            }
        }
        // The agent's routes first: no record moves before both paths answer.
        match w.routes.as_deref() {
            None | Some("done") => {}
            Some("failed") => {
                record_failed(
                    s,
                    &w,
                    &format!(
                        "The Gateway didn't take the change: {}",
                        w.routes_error.as_deref().unwrap_or("no reason given")
                    ),
                )
                .await?;
                continue;
            }
            Some(_) => continue,
        }
        let r = if w.status == "switching" {
            switch_one(s, cf, row, &w, target, mark).await
        } else {
            revert_one(s, cf, row, &w, zones, mark).await
        };
        match r {
            Ok(()) => {}
            Err(Step::Cf(e)) if slow_down(&e) => return Err(Step::Cf(e)),
            Err(Step::Cf(e)) => record_failed(s, &w, &e.to_string()).await?,
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

async fn record_failed(s: &AppState, w: &Waiting, error: &str) -> ApiResult<()> {
    sqlx::query("UPDATE pyo.dns_records SET error = $3, updated_at = now() WHERE service_id = $1 AND domain_id = $2")
        .bind(&w.service_id)
        .bind(&w.domain_id)
        .bind(error)
        .execute(&s.db)
        .await?;
    Ok(())
}

async fn switch_one<C: Cloudflare + TunnelApi>(
    s: &AppState,
    cf: &C,
    row: &Row,
    w: &Waiting,
    target: &str,
    mark: &str,
) -> Result<(), Step> {
    let out = switch_record(cf, &w.zone_id, &w.hostname, target, w.replace, mark).await?;
    let mut tx = s.db.begin().await?;
    match out {
        Switched::Confirm(existing) => {
            sqlx::query(
                "UPDATE pyo.dns_records SET status = 'conflict', existing = $3, error = NULL, updated_at = now()
                  WHERE service_id = $1 AND domain_id = $2",
            )
            .bind(&w.service_id)
            .bind(&w.domain_id)
            .bind(json!(existing))
            .execute(&mut *tx)
            .await?;
        }
        Switched::Done { record, previous } => {
            // What it replaced before (a repoint after the tunnel came back)
            // stays kept.
            let mut kept: Vec<Kept> = serde_json::from_value(w.previous.clone()).unwrap_or_default();
            kept.extend(previous.iter().cloned());
            sqlx::query(
                "UPDATE pyo.dns_records SET status = 'created', via = 'tunnel', record_id = $3, type = 'CNAME', content = $4,
                        proxied = true, proxy = true, previous = $5, existing = '[]', error = NULL, replace = false, updated_at = now()
                  WHERE service_id = $1 AND domain_id = $2",
            )
            .bind(&w.service_id)
            .bind(&w.domain_id)
            .bind(&record.id)
            .bind(target)
            .bind(json!(kept))
            .execute(&mut *tx)
            .await?;
            // Only through the tunnel now: the gate goes on, plain HTTP goes.
            set_domain(&mut tx, &w.service_id, &w.domain_id, |d| {
                d["tunnel"] = json!(pyo_proto::TUNNEL_ON);
                live(d);
            })
            .await?;
            let wake = crate::routes::changed(&mut tx, &w.service_id, true).await?;
            let replaced: Vec<String> = previous
                .iter()
                .map(|k| format!("{} {}", k.kind, k.content))
                .collect();
            log_activity(
                &mut tx,
                &row.team_id,
                "Pyo",
                "domain.tunnel",
                "success",
                &if replaced.is_empty() {
                    format!("{} goes through the Cloudflare Tunnel", w.hostname)
                } else {
                    format!(
                        "{} goes through the Cloudflare Tunnel (replaced {})",
                        w.hostname,
                        replaced.join(", ")
                    )
                },
                json!({ "kind": "service", "id": w.service_id, "label": w.hostname }),
                json!({ "serviceId": w.service_id, "replaced": previous }),
            )
            .await?;
            tx.commit().await?;
            if let Some(c) = wake {
                s.wake(&c).await;
            }
            return Ok(());
        }
    }
    tx.commit().await?;
    Ok(())
}

async fn revert_one<C: Cloudflare + TunnelApi>(
    s: &AppState,
    cf: &C,
    row: &Row,
    w: &Waiting,
    _zones: &[super::Zone],
    mark: &str,
) -> Result<(), Step> {
    let previous: Vec<Kept> = if w.restore {
        serde_json::from_value(w.previous.clone()).unwrap_or_default()
    } else {
        vec![]
    };
    // Without a kept record, on a cluster with public ports: its own A record.
    let mut tx = s.db.begin().await?;
    let ip = crate::freedomain::load(&mut tx, &row.cluster_id)
        .await?
        .and_then(|(_, c)| c.public_ip())
        .map(|ip| ip.to_string());
    tx.commit().await?;
    let fallback = ip.as_deref().filter(|_| w.restore);
    let made = revert_record(
        cf,
        &w.zone_id,
        &w.hostname,
        w.record_id.as_deref(),
        &previous,
        fallback,
        mark,
    )
    .await?;
    let mut tx = s.db.begin().await?;
    match &made {
        Some(r) => {
            sqlx::query(
                "UPDATE pyo.dns_records SET status = 'created', via = 'direct', record_id = $3, type = 'A', content = $4,
                        proxied = false, proxy = false, previous = '[]', existing = '[]', error = NULL, updated_at = now()
                  WHERE service_id = $1 AND domain_id = $2",
            )
            .bind(&w.service_id)
            .bind(&w.domain_id)
            .bind(&r.id)
            .bind(&r.content)
            .execute(&mut *tx)
            .await?;
        }
        None => {
            // Pyo's CNAME is gone already: nothing left for the leftovers.
            sqlx::query("UPDATE pyo.dns_records SET via = 'direct', record_id = NULL WHERE service_id = $1 AND domain_id = $2")
                .bind(&w.service_id)
                .bind(&w.domain_id)
                .execute(&mut *tx)
                .await?;
            sqlx::query("DELETE FROM pyo.dns_records WHERE service_id = $1 AND domain_id = $2")
                .bind(&w.service_id)
                .bind(&w.domain_id)
                .execute(&mut *tx)
                .await?;
        }
    }
    // Direct again: its route at the tunnel goes with the next pass, and
    // its certificate is what it was.
    set_domain(&mut tx, &w.service_id, &w.domain_id, |d| {
        leave_tunnel(d);
    })
    .await?;
    let wake = crate::routes::changed(&mut tx, &w.service_id, true).await?;
    log_activity(
        &mut tx,
        &row.team_id,
        "Pyo",
        "domain.tunnel",
        "info",
        &format!("{} no longer goes through the Cloudflare Tunnel", w.hostname),
        json!({ "kind": "service", "id": w.service_id, "label": w.hostname }),
        json!({ "serviceId": w.service_id, "restored": previous }),
    )
    .await?;
    tx.commit().await?;
    if let Some(c) = wake {
        s.wake(&c).await;
    }
    Ok(())
}

/// SQL: the cluster ($1) has a domain on its way to or from the tunnel,
/// or waiting for "Replace and switch": `tunnel` "switching" (both paths
/// answer for it).
const SWITCHING: &str = "SELECT EXISTS (SELECT 1 FROM pyo.services a, jsonb_array_elements(CASE WHEN jsonb_typeof(a.spec->'domains') = 'array' THEN a.spec->'domains' ELSE '[]' END) d
                  WHERE a.cluster_id = $1 AND NOT a.deleting AND d->>'tunnel' = 'switching')";

/// Public ports asked to close while domains moved to the tunnel: closed
/// (`traffic` 'tunnel') once none is still on its way, so no visitor of a
/// moved domain meets a closed port. A domain waiting for "Replace and
/// switch" holds it too. Whether they closed.
pub(crate) async fn close_public(s: &AppState, row: &Row) -> ApiResult<bool> {
    let waiting: bool = sqlx::query_scalar(
        "SELECT close_public_after IS NOT NULL AND traffic = 'public' FROM pyo.clusters WHERE id = $1",
    )
    .bind(&row.cluster_id)
    .fetch_optional(&s.db)
    .await?
    .unwrap_or(false);
    if !waiting || row.state == "removing" {
        return Ok(false);
    }
    let busy: bool = sqlx::query_scalar(SWITCHING)
        .bind(&row.cluster_id)
        .fetch_one(&s.db)
        .await?;
    if busy {
        return Ok(false);
    }
    let mut tx = s.db.begin().await?;
    let n = sqlx::query(
        "UPDATE pyo.clusters SET traffic = 'tunnel', close_public_after = NULL
          WHERE id = $1 AND close_public_after IS NOT NULL AND traffic = 'public'",
    )
    .bind(&row.cluster_id)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if n > 0 {
        let name: String = sqlx::query_scalar("SELECT name FROM pyo.clusters WHERE id = $1")
            .bind(&row.cluster_id)
            .fetch_one(&mut *tx)
            .await?;
        log_activity(
            &mut tx,
            &row.team_id,
            "Pyo",
            "cluster.updated",
            "info",
            &format!("Closed the public ports of {name}: its domains are on the Cloudflare Tunnel"),
            json!({ "kind": "cluster", "id": row.cluster_id, "label": name }),
            json!({ "clusterId": row.cluster_id }),
        )
        .await?;
    }
    tx.commit().await?;
    if n > 0 {
        s.wake(&row.cluster_id).await;
    }
    Ok(n > 0)
}

/// A tunnel turned off while public ports are on: removed once none of
/// its domains is still switching back. Whether it was.
pub(crate) async fn remove_when_back(s: &AppState, row: &Row) -> ApiResult<bool> {
    let waiting: bool = sqlx::query_scalar(
        "SELECT remove_when_back IS NOT NULL FROM pyo.cluster_tunnels WHERE cluster_id = $1 AND state <> 'removing'",
    )
    .bind(&row.cluster_id)
    .fetch_optional(&s.db)
    .await?
    .unwrap_or(false);
    if !waiting {
        return Ok(false);
    }
    let busy: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM pyo.dns_records r JOIN pyo.services a ON a.id = r.service_id
                         WHERE a.cluster_id = $1 AND NOT a.deleting AND r.status = 'reverting')",
    )
    .bind(&row.cluster_id)
    .fetch_one(&s.db)
    .await?;
    if busy {
        return Ok(false);
    }
    let n = sqlx::query(
        "UPDATE pyo.cluster_tunnels SET state = 'removing', remove_when_back = NULL, token_sent_at = NULL, updated_at = now()
          WHERE cluster_id = $1 AND remove_when_back IS NOT NULL AND state <> 'removing'",
    )
    .bind(&row.cluster_id)
    .execute(&s.db)
    .await?
    .rows_affected();
    if n > 0 {
        s.wake(&row.cluster_id).await;
    }
    Ok(n > 0)
}

/// A domain leaves the tunnel: no `tunnel`, and the certificate it had.
/// A domain the tunnel answers for now: active, and an error from before
/// (the direct DNS check's, say) no longer applies.
pub(crate) fn live(d: &mut Value) {
    d["status"] = json!("active");
    if let Some(o) = d.as_object_mut() {
        o.remove("error");
    }
}

pub(crate) fn leave_tunnel(d: &mut Value) {
    if let Some(o) = d.as_object_mut() {
        o.remove("tunnel");
        if let Some(before) = o.remove("tlsModeBefore") {
            o.insert("tlsMode".into(), before);
            match o.remove("certificateIdBefore") {
                Some(c) => {
                    o.insert("certificateId".into(), c);
                }
                None => {
                    o.remove("certificateId");
                }
            }
        }
    }
}

/// A tunnel domain takes the tunnel's certificate (the one it had is
/// kept to go back to). While it switches, a certificate already issued
/// for it stays (cloudflared trusts public CAs too, and direct visitors
/// still see a valid one). Whether it changed.
pub(crate) fn take_certificate(d: &mut Value, certificate_id: &str) -> bool {
    let switching = d["tunnel"] == json!(pyo_proto::TUNNEL_SWITCHING);
    let mode = d["tlsMode"].as_str().unwrap_or("auto").to_owned();
    let host = d["hostname"].as_str().unwrap_or("").to_owned();
    let issuer = crate::deploy::tls_issuer(&mode, &host);
    let has_tunnel_cert =
        issuer == "custom" && d["certificateId"] == json!(certificate_id);
    if has_tunnel_cert {
        return false;
    }
    let valid_now = (issuer == "letsencrypt" && d["tls"] == json!("issued")) || issuer == "custom";
    if switching && valid_now && d.get("tlsModeBefore").is_none() {
        return false;
    }
    if d.get("tlsModeBefore").is_none() {
        d["tlsModeBefore"] = json!(mode);
        if let Some(c) = d.get("certificateId").cloned() {
            d["certificateIdBefore"] = c;
        }
    }
    d["tlsMode"] = json!("custom");
    d["certificateId"] = json!(certificate_id);
    true
}

/// Whether `host` is a free name under the default domain `d`: one label
/// below it (`web-shop.example.com`), as the wildcard covers.
pub fn under_default(host: &str, d: &str) -> bool {
    host.to_ascii_lowercase()
        .strip_suffix(&format!(".{}", d.to_ascii_lowercase()))
        .is_some_and(|label| !label.is_empty() && !label.contains('.'))
}

/// A cluster's domains one label under its default domain, with no record
/// of their own (Pyo's or anyone's: an explicit record wins over the
/// wildcard), go only through the tunnel: `tunnel: "on"`, and active once
/// the wildcard record is there.
async fn free_domains_follow(
    s: &AppState,
    cluster_id: &str,
    default_domain: &str,
    wildcard: bool,
) -> ApiResult<()> {
    let services: Vec<(String, Value)> = sqlx::query_as(
        "SELECT a.id, coalesce(a.spec->'domains', '[]') FROM pyo.services a
          WHERE a.cluster_id = $1 AND NOT a.deleting
            AND EXISTS (SELECT 1 FROM jsonb_array_elements(CASE WHEN jsonb_typeof(a.spec->'domains') = 'array' THEN a.spec->'domains' ELSE '[]' END) d
                         WHERE lower(d->>'hostname') LIKE '%.' || lower($2)
                           AND (NOT d ? 'tunnel' OR (d->>'tunnel' = 'on' AND d->>'status' <> 'active')))",
    )
    .bind(cluster_id)
    .bind(default_domain)
    .fetch_all(&s.db)
    .await?;
    for (service_id, mut domains) in services {
        let own: Vec<String> = sqlx::query_scalar("SELECT domain_id FROM pyo.dns_records WHERE service_id = $1")
            .bind(&service_id)
            .fetch_all(&s.db)
            .await?;
        let mut routes = false;
        let mut changed = false;
        for d in domains.as_array_mut().into_iter().flatten() {
            let host = d["hostname"].as_str().unwrap_or("").to_owned();
            let id = d["id"].as_str().unwrap_or("").to_owned();
            if !under_default(&host, default_domain) || own.contains(&id) {
                continue;
            }
            if d.get("tunnel").is_none() {
                d["tunnel"] = json!(pyo_proto::TUNNEL_ON);
                routes = true;
                changed = true;
            }
            if d["tunnel"] == json!(pyo_proto::TUNNEL_ON) && wildcard && (d["status"] != json!("active") || d.get("error").is_some()) {
                live(d);
                changed = true;
            }
        }
        if !changed {
            continue;
        }
        let mut tx = s.db.begin().await?;
        sqlx::query("UPDATE pyo.services SET spec = jsonb_set(spec, '{domains}', $2) WHERE id = $1")
            .bind(&service_id)
            .bind(&domains)
            .execute(&mut *tx)
            .await?;
        let wake = if routes {
            crate::routes::changed(&mut tx, &service_id, true).await?
        } else {
            None
        };
        tx.commit().await?;
        if let Some(c) = wake {
            s.wake(&c).await;
        }
    }
    Ok(())
}

/// Change one domain of a service's spec.
async fn set_domain(
    tx: &mut Transaction<'_, Postgres>,
    service_id: &str,
    domain_id: &str,
    f: impl FnOnce(&mut Value),
) -> ApiResult<()> {
    let domains: Option<Value> = sqlx::query_scalar(
        "SELECT coalesce(spec->'domains', '[]') FROM pyo.services WHERE id = $1 FOR UPDATE",
    )
    .bind(service_id)
    .fetch_optional(&mut **tx)
    .await?;
    let Some(mut domains) = domains else {
        return Ok(());
    };
    if let Some(d) = domains
        .as_array_mut()
        .and_then(|ds| ds.iter_mut().find(|d| d["id"] == json!(domain_id)))
    {
        f(d);
    }
    sqlx::query("UPDATE pyo.services SET spec = jsonb_set(spec, '{domains}', $2) WHERE id = $1")
        .bind(service_id)
        .bind(&domains)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

/// Tunnel domains use the tunnel's certificate (see [`take_certificate`]).
/// Services whose domains changed get their routes again, and rows
/// switching for them wait for that sync.
async fn tls_follow(s: &AppState, cluster_id: &str, certificate_id: &str) -> ApiResult<()> {
    let services: Vec<(String, Value)> = sqlx::query_as(
        "SELECT a.id, coalesce(a.spec->'domains', '[]') FROM pyo.services a
          WHERE a.cluster_id = $1 AND NOT a.deleting
            AND EXISTS (SELECT 1 FROM jsonb_array_elements(CASE WHEN jsonb_typeof(a.spec->'domains') = 'array' THEN a.spec->'domains' ELSE '[]' END) d WHERE d->>'tunnel' IN ('switching', 'on'))",
    )
    .bind(cluster_id)
    .fetch_all(&s.db)
    .await?;
    for (service_id, mut domains) in services {
        let mut changed = false;
        for d in domains.as_array_mut().into_iter().flatten() {
            if matches!(d["tunnel"].as_str(), Some(pyo_proto::TUNNEL_SWITCHING | pyo_proto::TUNNEL_ON)) {
                changed |= take_certificate(d, certificate_id);
            }
        }
        if !changed {
            continue;
        }
        let mut tx = s.db.begin().await?;
        sqlx::query("UPDATE pyo.services SET spec = jsonb_set(spec, '{domains}', $2) WHERE id = $1")
            .bind(&service_id)
            .bind(&domains)
            .execute(&mut *tx)
            .await?;
        if let Some((cluster, command)) = crate::routes::changed_waiting(&mut tx, &service_id).await? {
            sqlx::query(
                "UPDATE pyo.dns_records SET routes_command = $2 WHERE service_id = $1 AND status IN ('switching', 'reverting')",
            )
            .bind(&service_id)
            .bind(&command)
            .execute(&mut *tx)
            .await?;
            tx.commit().await?;
            s.wake(&cluster).await;
        } else {
            tx.commit().await?;
        }
    }
    Ok(())
}

/// The default domain's wildcard record: `*.<zone>` to the tunnel, made
/// once. Someone else's record there stops it, and it's said.
async fn default_record<C: Cloudflare + TunnelApi>(
    s: &AppState,
    cf: &C,
    row: &Row,
    target: &str,
    mark: &str,
) -> Result<(), Step> {
    let (Some(domain), Some(zone)) = (&row.default_domain, &row.default_zone_id) else {
        return Ok(());
    };
    if row.default_record_id.is_some() {
        return Ok(());
    }
    let name = format!("*.{domain}");
    match switch_record(cf, zone, &name, target, false, mark).await? {
        Switched::Done { record, .. } => {
            sqlx::query("UPDATE pyo.cluster_tunnels SET default_record_id = $2 WHERE cluster_id = $1")
                .bind(&row.cluster_id)
                .bind(&record.id)
                .execute(&s.db)
                .await?;
        }
        Switched::Confirm(existing) => {
            let what: Vec<String> = existing
                .iter()
                .map(|r| format!("{} {}", r.kind, r.content))
                .collect();
            return Err(Step::Cf(CfError::Refused(format!(
                "Cloudflare has a record for {name} ({}). Remove it, or pick another default domain.",
                what.join(", ")
            ))));
        }
    }
    Ok(())
}

// ── The certificate ────────────────────────────────────────────────────────

/// Issue an Origin CA certificate for `names`, with a key made here.
pub(crate) async fn issue<C: TunnelApi>(
    cf: &C,
    names: &[String],
) -> Result<(OriginCert, String), CfError> {
    let (csr, key) = tunnel::key_and_csr(names)
        .map_err(|e| CfError::Refused(format!("Pyo couldn't make the certificate's key: {e}")))?;
    let cert = match cf.issue_origin_cert(&csr, names).await {
        Err(CfError::Denied(_)) => {
            return Err(CfError::Denied(format!(
                "give the token {} on the tunnel's zones.",
                tunnel::CERTS_EDIT
            )));
        }
        other => other?,
    };
    Ok((cert, key))
}

/// The tunnel's certificate covers `names` (issued again when it doesn't,
/// or when Rotate asked). Its id in the team's certificates.
async fn ensure_certificate<C: TunnelApi>(
    s: &AppState,
    cf: &C,
    row: &Row,
    names: &[String],
) -> Result<Option<String>, Step> {
    let have: Option<Vec<String>> = match &row.certificate_id {
        Some(id) => sqlx::query_scalar("SELECT hostnames FROM pyo.certificates WHERE id = $1")
            .bind(id)
            .fetch_optional(&s.db)
            .await?,
        None => None,
    };
    let covered = have.as_ref().is_some_and(|h| tunnel::covers_all(h, names));
    if covered && !row.rotate {
        return Ok(row.certificate_id.clone());
    }
    if names.is_empty() {
        return Ok(row.certificate_id.clone());
    }
    let (cert, key) = issue(cf, names).await?;
    let now = pyo_proto::now_ms() / 1000;
    let parsed = crate::certs::parse(&cert.pem, &key, now)
        .map_err(|e| Step::Cf(CfError::Refused(format!("Cloudflare's certificate: {}", e.message))))?;
    let sealed = crate::vault::get()
        .seal_credential(&row.team_id, &parsed.key_pem)
        .await?;
    let id = row
        .certificate_id
        .clone()
        .unwrap_or_else(|| crate::ids::new_id("crt"));
    let cluster_name: String = sqlx::query_scalar("SELECT name FROM pyo.clusters WHERE id = $1")
        .bind(&row.cluster_id)
        .fetch_one(&s.db)
        .await?;
    let mut tx = s.db.begin().await?;
    sqlx::query(
        "INSERT INTO pyo.certificates (id, team_id, name, hostnames, issuer, not_before, not_after, fingerprint, chain_pem, key_sealed)
         VALUES ($1, $2, $3, $4, $5, to_timestamp($6), to_timestamp($7), $8, $9, $10)
         ON CONFLICT (id) DO UPDATE SET hostnames = $4, issuer = $5, not_before = to_timestamp($6), not_after = to_timestamp($7),
           fingerprint = $8, chain_pem = $9, key_sealed = $10, warned_days = NULL, updated_at = now()",
    )
    .bind(&id)
    .bind(&row.team_id)
    .bind(format!("Cloudflare Tunnel ({cluster_name})"))
    .bind(&parsed.hostnames)
    .bind("Cloudflare Origin CA")
    .bind(parsed.not_before as f64)
    .bind(parsed.not_after as f64)
    .bind(&parsed.fingerprint)
    .bind(&parsed.chain_pem)
    .bind(&sealed)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "UPDATE pyo.cluster_tunnels SET certificate_id = $2, cf_certificate_id = $3, rotate = rotate AND $5,
                cf_certificate_old = coalesce($4, cf_certificate_old),
                cf_certificate_old_at = CASE WHEN $4 IS NULL THEN cf_certificate_old_at ELSE now() + interval '1 day' END
          WHERE cluster_id = $1",
    )
    .bind(&row.cluster_id)
    .bind(&id)
    .bind(&cert.id)
    .bind(&row.cf_certificate_id)
    // Rotate also needs the new secret (step 6): only that clears it.
    .bind(true)
    .execute(&mut *tx)
    .await?;
    // Services using it get it again (the same id, a new chain and key).
    let mut wake = vec![];
    for (service_id, _) in crate::certs::used_by(&mut tx, &id).await? {
        if let Some(c) = crate::routes::changed(&mut tx, &service_id, true).await? {
            wake.push(c);
        }
    }
    log_activity(
        &mut tx,
        &row.team_id,
        "Pyo",
        "domain.certificate_replaced",
        "success",
        &format!("Got the Cloudflare Tunnel certificate of {cluster_name}"),
        json!({ "kind": "certificate", "id": id, "label": format!("Cloudflare Tunnel ({cluster_name})") }),
        json!({}),
    )
    .await?;
    tx.commit().await?;
    wake.sort();
    wake.dedup();
    for c in wake {
        s.wake(&c).await;
    }
    Ok(Some(id))
}

// ── Health ─────────────────────────────────────────────────────────────────

/// The tunnel's health from Cloudflare's view and the agent's:
/// "connected", "degraded", "down", "starting" or "unknown".
///
/// `young`: made in the last 5 minutes. `failed`: Pyo's last
/// pass at Cloudflare failed (the tunnel has an error). Without a read of
/// Cloudflare's view yet, it's "starting" while young, and "unknown" (Pyo
/// can't reach Cloudflare) once a pass failed or none read it in time. The
/// loop reads it every 2 minutes, so 5 minutes is two reads missed.
pub fn health(cf: &Value, agent: &Value, young: bool, failed: bool) -> &'static str {
    let status = cf["status"].as_str().unwrap_or("");
    let wanted = agent["replicas"].as_i64().unwrap_or(0);
    let ready = agent["readyReplicas"].as_i64().unwrap_or(0);
    match status {
        "" if young && !failed => "starting",
        "" => "unknown",
        "healthy" if wanted > 0 && ready < wanted => "degraded",
        "healthy" => "connected",
        "degraded" => "degraded",
        _ if young => "starting",
        _ => "down",
    }
}

/// Keep Cloudflare's view of the tunnel, and say when it's been down for
/// 5 minutes (an alert), and when it's back.
async fn status(s: &AppState, row: &Row, t: &Tunnel) -> ApiResult<()> {
    let cf_status = json!({
        "status": t.status,
        "connections": t.connections,
        "at": pyo_proto::now_ms(),
    });
    let down = matches!(t.status.as_str(), "down" | "inactive") && !row.young;
    sqlx::query(
        "UPDATE pyo.cluster_tunnels SET cf_status = $2, checked_at = now(),
                down_since = CASE WHEN $3 THEN coalesce(down_since, now()) ELSE NULL END
          WHERE cluster_id = $1",
    )
    .bind(&row.cluster_id)
    .bind(&cf_status)
    .bind(down)
    .execute(&s.db)
    .await?;
    let alert = if down && !row.down_alerted && row.down_secs.unwrap_or(0) >= DOWN_ALERT_SECS {
        Some(true)
    } else if !down && row.down_alerted {
        Some(false)
    } else {
        None
    };
    if let Some(down) = alert {
        let mut tx = s.db.begin().await?;
        let name: String = sqlx::query_scalar("SELECT name FROM pyo.clusters WHERE id = $1")
            .bind(&row.cluster_id)
            .fetch_one(&mut *tx)
            .await?;
        sqlx::query("UPDATE pyo.cluster_tunnels SET down_alerted = $2 WHERE cluster_id = $1")
            .bind(&row.cluster_id)
            .bind(down)
            .execute(&mut *tx)
            .await?;
        log_activity(
            &mut tx,
            &row.team_id,
            "Pyo",
            if down { "cluster.tunnel_down" } else { "cluster.tunnel_up" },
            if down { "failure" } else { "success" },
            &if down {
                format!("The Cloudflare Tunnel of {name} is down")
            } else {
                format!("The Cloudflare Tunnel of {name} is back")
            },
            json!({ "kind": "cluster", "id": row.cluster_id, "label": name }),
            json!({}),
        )
        .await?;
        tx.commit().await?;
    }
    Ok(())
}

/// Read Cloudflare's view of a cluster's tunnel again when the last read
/// is older than 30 seconds (the tunnel endpoint's fresh read). Quiet when
/// Cloudflare can't be reached: the stored view is shown then.
pub async fn refresh(s: &AppState, cluster_id: &str) -> ApiResult<()> {
    let due: Option<(String, String, Option<String>)> = sqlx::query_as(
        "SELECT team_id, account_id, tunnel_id FROM pyo.cluster_tunnels
          WHERE cluster_id = $1 AND state <> 'removing' AND tunnel_id IS NOT NULL
            AND (checked_at IS NULL OR checked_at < now() - interval '30 seconds')",
    )
    .bind(cluster_id)
    .fetch_optional(&s.db)
    .await?;
    let Some((team_id, account_id, Some(tunnel_id))) = due else {
        return Ok(());
    };
    let mut tx = s.db.begin().await?;
    let account = super::account(s, &mut tx, &team_id).await.ok().flatten();
    tx.commit().await?;
    let Some(account) = account else {
        return Ok(());
    };
    let Ok(t) = Http::new(&account.token)?.tunnel(&account_id, &tunnel_id).await else {
        return Ok(());
    };
    if let Some(row) = load(&s.db, cluster_id).await? {
        status(s, &row, &t).await?;
    }
    Ok(())
}

// ── As shown ───────────────────────────────────────────────────────────────

/// A tunnel as the dashboard, CLI and API show it: never a token.
pub fn shown(row: &Value) -> Value {
    let cf = &row["cfStatus"];
    let agent = &row["agentStatus"];
    let connections = cf["connections"].as_array().cloned().unwrap_or_default();
    let mut colos: Vec<String> = connections
        .iter()
        .filter_map(|c| c["colo"].as_str().map(str::to_owned))
        .collect();
    colos.sort();
    colos.dedup();
    let pods = agent["pods"].as_array().cloned().unwrap_or_default();
    let connectors: Vec<Value> = pods
        .iter()
        .map(|p| {
            // Plain words, whichever agent sent them.
            let reason = p["reason"].as_str().and_then(pyo_proto::pod_problem);
            json!({ "server": p["server"], "ready": p["ready"], "reason": reason })
        })
        .collect();
    json!({
        "state": row["state"], "error": row["error"],
        "accountId": row["accountId"], "accountName": row["accountName"],
        "tunnelId": row["tunnelId"], "tunnelName": row["tunnelName"],
        "defaultDomain": row["defaultDomain"], "portsPublic": row["portsPublic"],
        // Turned off: removed once its domains are back on public ports.
        "removeWhenBack": row["removeWhenBack"] == json!(true),
        "connectors": connectors,
        "colos": colos,
        "connections": connections.len(),
        "health": if row["state"] == "removing" {
            "removing"
        } else {
            health(cf, agent, row["young"] == json!(true), !row["error"].is_null())
        },
        // The cluster's install refused the connector: its connect command
        // brings the newer one.
        "refused": agent["refused"] == json!(true),
        "certificate": row["certificate"],
        "checkedAt": cf["at"],
    })
}

/// Each cluster's tunnel, as `tunnel` on the snapshot's clusters.
pub async fn attach(
    db: &mut sqlx::PgConnection,
    team_ids: &[String],
    clusters: &mut [Value],
) -> ApiResult<()> {
    let rows: Vec<(String, Value)> = sqlx::query_as(SHOWN).bind(team_ids).fetch_all(&mut *db).await?;
    for c in clusters.iter_mut() {
        if let Some((_, r)) = rows.iter().find(|(id, _)| c["id"] == json!(id)) {
            c["tunnel"] = shown(r);
        }
    }
    Ok(())
}

/// One cluster's tunnel, shown. None without one.
pub async fn one_shown(db: impl sqlx::PgExecutor<'_>, cluster_id: &str) -> ApiResult<Option<Value>> {
    let row: Option<(String, Value)> = sqlx::query_as(SHOWN_ONE)
        .bind(cluster_id)
        .fetch_optional(db)
        .await?;
    Ok(row.map(|(_, r)| shown(&r)))
}

const SHOWN: &str = concat!(
    "SELECT t.cluster_id, jsonb_build_object(
        'state', t.state, 'error', t.error, 'accountId', t.account_id, 'accountName', t.account_name,
        'tunnelId', t.tunnel_id, 'tunnelName', t.tunnel_name, 'defaultDomain', t.default_domain, 'portsPublic', t.ports_public, 'removeWhenBack', t.remove_when_back IS NOT NULL,
        'cfStatus', t.cf_status, 'agentStatus', t.agent_status, 'young', t.created_at > now() - interval '5 minutes',
        'certificate', (SELECT jsonb_build_object('id', c.id, 'notAfter', pyo.ms(c.not_after)) FROM pyo.certificates c WHERE c.id = t.certificate_id))
   FROM pyo.cluster_tunnels t",
    " WHERE t.team_id = ANY($1)"
);
const SHOWN_ONE: &str = concat!(
    "SELECT t.cluster_id, jsonb_build_object(
        'state', t.state, 'error', t.error, 'accountId', t.account_id, 'accountName', t.account_name,
        'tunnelId', t.tunnel_id, 'tunnelName', t.tunnel_name, 'defaultDomain', t.default_domain, 'portsPublic', t.ports_public, 'removeWhenBack', t.remove_when_back IS NOT NULL,
        'cfStatus', t.cf_status, 'agentStatus', t.agent_status, 'young', t.created_at > now() - interval '5 minutes',
        'certificate', (SELECT jsonb_build_object('id', c.id, 'notAfter', pyo.ms(c.not_after)) FROM pyo.certificates c WHERE c.id = t.certificate_id))
   FROM pyo.cluster_tunnels t",
    " WHERE t.cluster_id = $1"
);

// ── Removing a tunnel ──────────────────────────────────────────────────────

/// Take a tunnel apart, in order: no routes but the catch-all, records
/// back (or only Pyo's deleted), the connector stopped, connections
/// dropped, the tunnel deleted, its certificate revoked. Then the row.
async fn remove<C: Cloudflare + TunnelApi>(
    s: &AppState,
    cf: &C,
    row: &Row,
    zones: &[super::Zone],
    mark: &str,
) -> Result<(), Step> {
    if let Some(id) = &row.tunnel_id {
        match cf
            .put_config(&row.account_id, id, &tunnel::empty_ingress())
            .await
        {
            Ok(_) | Err(CfError::NotFound) => {}
            Err(e) => return Err(e.into()),
        }
    }
    // Every record through the tunnel, back.
    let rows: Vec<Waiting> = sqlx::query_as(
        "SELECT r.service_id, r.domain_id, r.hostname, r.zone_id, r.record_id, r.status, r.previous, r.replace, r.restore,
                NULL::text AS routes, NULL::text AS routes_error
           FROM pyo.dns_records r JOIN pyo.services a ON a.id = r.service_id
          WHERE a.cluster_id = $1 AND (r.via = 'tunnel' OR r.status IN ('switching', 'reverting'))",
    )
    .bind(&row.cluster_id)
    .fetch_all(&s.db)
    .await?;
    for mut w in rows {
        w.restore = row.restore_records;
        revert_one(s, cf, row, &w, zones, mark).await?;
    }
    // Domains that never had a record: no longer through the tunnel.
    let services: Vec<String> = sqlx::query_scalar(
        "SELECT a.id FROM pyo.services a WHERE a.cluster_id = $1
            AND EXISTS (SELECT 1 FROM jsonb_array_elements(CASE WHEN jsonb_typeof(a.spec->'domains') = 'array' THEN a.spec->'domains' ELSE '[]' END) d WHERE d ? 'tunnel')",
    )
    .bind(&row.cluster_id)
    .fetch_all(&s.db)
    .await?;
    for service_id in services {
        let mut tx = s.db.begin().await?;
        let domains: Value = sqlx::query_scalar("SELECT coalesce(spec->'domains', '[]') FROM pyo.services WHERE id = $1 FOR UPDATE")
            .bind(&service_id)
            .fetch_one(&mut *tx)
            .await?;
        let mut domains = domains;
        for d in domains.as_array_mut().into_iter().flatten() {
            leave_tunnel(d);
        }
        sqlx::query("UPDATE pyo.services SET spec = jsonb_set(spec, '{domains}', $2) WHERE id = $1")
            .bind(&service_id)
            .bind(&domains)
            .execute(&mut *tx)
            .await?;
        let wake = crate::routes::changed(&mut tx, &service_id, true).await?;
        tx.commit().await?;
        if let Some(c) = wake {
            s.wake(&c).await;
        }
    }
    // The default domain's wildcard, only if it's still Pyo's.
    if let (Some(zone), Some(id), Some(domain)) =
        (&row.default_zone_id, &row.default_record_id, &row.default_domain)
    {
        delete_ours(cf, zone, id, &format!("*.{domain}"), mark).await?;
    }
    // The connector: asked once, then waited for (or 10 minutes).
    let agent_has = row.agent_status["tunnelId"].as_str().is_some();
    if agent_has && row.removing_secs < REMOVE_WAIT_SECS {
        if row.token_due {
            let mut tx = s.db.begin().await?;
            crate::deploy::enqueue(
                &mut tx,
                &row.cluster_id,
                pyo_proto::Command::TunnelRemove {},
            )
            .await?;
            sqlx::query("UPDATE pyo.cluster_tunnels SET token_sent_at = now() WHERE cluster_id = $1")
                .bind(&row.cluster_id)
                .execute(&mut *tx)
                .await?;
            tx.commit().await?;
            s.wake(&row.cluster_id).await;
        }
        return Ok(());
    }
    if let Some(id) = &row.tunnel_id {
        match cf.drop_connections(&row.account_id, id).await {
            Ok(()) | Err(CfError::NotFound) => {}
            Err(e) => return Err(e.into()),
        }
        match cf.delete_tunnel(&row.account_id, id).await {
            Ok(()) | Err(CfError::NotFound) => {}
            Err(e) => return Err(e.into()),
        }
    }
    for c in [&row.cf_certificate_id, &row.cf_certificate_old].into_iter().flatten() {
        match cf.revoke_origin_cert(c).await {
            Ok(()) | Err(CfError::NotFound) => {}
            Err(e) => return Err(e.into()),
        }
    }
    let mut tx = s.db.begin().await?;
    // Everything at Cloudflare is gone: nothing left for the leftovers.
    sqlx::query(
        "UPDATE pyo.cluster_tunnels SET tunnel_id = NULL, cf_certificate_id = NULL, cf_certificate_old = NULL, default_record_id = NULL
          WHERE cluster_id = $1",
    )
    .bind(&row.cluster_id)
    .execute(&mut *tx)
    .await?;
    sqlx::query("DELETE FROM pyo.cluster_tunnels WHERE cluster_id = $1")
        .bind(&row.cluster_id)
        .execute(&mut *tx)
        .await?;
    if let Some(cert) = &row.certificate_id
        && crate::certs::used_by(&mut tx, cert).await?.is_empty()
    {
        sqlx::query("DELETE FROM pyo.certificates WHERE id = $1")
            .bind(cert)
            .execute(&mut *tx)
            .await?;
    }
    log_activity(
        &mut tx,
        &row.team_id,
        "Pyo",
        "cluster.tunnel_removed",
        "info",
        "Removed the Cloudflare Tunnel",
        json!({ "kind": "cluster", "id": row.cluster_id, "label": row.tunnel_name }),
        json!({}),
    )
    .await?;
    tx.commit().await?;
    Ok(())
}

#[cfg(test)]
#[path = "tunnel_loop_tests.rs"]
mod tests;
