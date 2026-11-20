//! Tailscale, through each cluster's agent (pyo-proto's tailscale.rs).
//!
//! Every cluster keeps its own OAuth client, as a Secret in its pyo-system.
//! Pyo never holds a Tailscale credential: it passes a pasted token or
//! client to the cluster's agent in a signed command, and keeps only what
//! isn't secret (`pyo.clusters.tailscale`: the client's ID and hint, the
//! tailnet's suffix, Pyo's own names there, what the last sync said).
//!
//! - **Setup** ([`plan`], [`set_up`], [`save_client`]): the agent sets
//!   the tailnet up and keeps the client. One per cluster: a cluster added
//!   later gets its own from a freshly pasted token.
//! - **Joining** ([`join_key`]): the cluster's agent makes a single-use,
//!   short-lived key. Pyo puts it into the join command and wipes it.
//! - **Names on the tailnet** are Tailscale Services ("svc:blog" is
//!   "blog.<tailnet>.ts.net"): Pyo's own (`pyo`, `registry`, `sandboxes`)
//!   on the cluster it runs on, and services' tailnet domains on theirs.
//!   [`tick`] sends each cluster what it should have
//!   ([`pyo_proto::Command::TailscaleSync`]) when that changes, and
//!   [`synced`] keeps what came back. The servers do the rest (the agent's
//!   tailnet.rs).
//! - **Before this** the installation kept one client in `pyo.settings`.
//!   [`hand_over`] passes it to the agent of the cluster Pyo runs on once,
//!   then deletes it.

use std::collections::BTreeSet;
use std::time::Duration;

use pyo_proto::tailscale::{
    self as ts, LockWaiting, OwnName, SyncServer, TailnetSync, TailnetSynced,
};
use pyo_proto::{Command, CommandResult};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::PgPool;

use crate::AppState;
use crate::error::{ApiError, ApiResult};

pub use ts::RESERVED;

/// Why a join or connect command can't be made yet.
pub const NEEDED: &str = "Servers join over your tailnet. Set up Tailscale on the cluster first.";

/// A pasted API access token: never printed, and passed on to the
/// cluster's agent as it is.
#[derive(Deserialize)]
#[serde(transparent)]
pub struct Token(String);

impl Token {
    pub fn into_inner(self) -> String {
        self.0
    }
}

impl std::fmt::Debug for Token {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Token(•••)")
    }
}

/// A cluster's Tailscale, as Pyo keeps it (`pyo.clusters.tailscale`).
/// Nothing in it is secret.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Tailnet {
    pub client_id: String,
    /// The secret's last characters, to tell it apart.
    #[serde(default)]
    pub hint: String,
    /// The tailnet's MagicDNS suffix ("tail1234.ts.net"), as last seen.
    #[serde(default)]
    pub suffix: Option<String>,
    /// Pyo's own names, on the cluster Pyo runs on.
    #[serde(default)]
    pub dashboard_name: Option<String>,
    #[serde(default)]
    pub registry_name: Option<String>,
    #[serde(default)]
    pub sandboxes_name: Option<String>,
    #[serde(default)]
    pub lock_waiting: Vec<LockWaiting>,
    /// Pyo's machines on the tailnet, as last seen.
    #[serde(default)]
    pub machines: Vec<ts::Machine>,
    /// Machines to take off the tailnet with the next sync.
    #[serde(default)]
    pub forget: Vec<String>,
    /// Older marks that are this cluster's too ([`hand_over`]).
    #[serde(default)]
    pub adopt: Vec<String>,
    /// What the last sync asked for (its hash), and when (ms).
    #[serde(default)]
    pub sent: String,
    #[serde(default)]
    pub sent_at: i64,
    /// The last sync failed: tailnet domains it didn't make yet say so.
    #[serde(default)]
    pub sync_failed: bool,
}

/// A cluster's Tailscale, when it has one.
pub async fn of<'e>(db: impl sqlx::PgExecutor<'e>, cluster_id: &str) -> ApiResult<Option<Tailnet>> {
    let v: Option<Option<Value>> =
        sqlx::query_scalar("SELECT tailscale FROM pyo.clusters WHERE id = $1")
            .bind(cluster_id)
            .fetch_optional(db)
            .await?;
    Ok(v.flatten().and_then(|v| serde_json::from_value(v).ok()))
}

/// Merges `fields` into a cluster's Tailscale, when it has one.
async fn patch<'e>(
    db: impl sqlx::PgExecutor<'e>,
    cluster_id: &str,
    fields: Value,
) -> ApiResult<()> {
    sqlx::query("UPDATE pyo.clusters SET tailscale = tailscale || $2::jsonb WHERE id = $1 AND tailscale IS NOT NULL")
        .bind(cluster_id)
        .bind(fields)
        .execute(db)
        .await?;
    Ok(())
}

/// The cluster Pyo runs on (self-hosted).
pub async fn host_cluster(db: &PgPool) -> ApiResult<Option<String>> {
    Ok(sqlx::query_scalar(
        "SELECT id FROM pyo.clusters WHERE same_cluster ORDER BY created_at LIMIT 1",
    )
    .fetch_optional(db)
    .await?)
}

/// The Tailscale of the cluster Pyo runs on: the installation's tailnet.
pub async fn host(db: &PgPool) -> ApiResult<Option<(String, Tailnet)>> {
    let Some(id) = host_cluster(db).await? else {
        return Ok(None);
    };
    Ok(of(db, &id).await?.map(|t| (id, t)))
}

/// This installation's ID (made once, kept with Pyo's data, so a restore
/// keeps it): part of what marks the Services Pyo made.
pub async fn install_id(db: &PgPool) -> ApiResult<String> {
    let id = format!("{:032x}", rand::random::<u128>());
    sqlx::query("INSERT INTO pyo.settings (key, value) VALUES ('install', jsonb_build_object('id', $1::text)) ON CONFLICT (key) DO NOTHING")
        .bind(&id)
        .execute(db)
        .await?;
    let v: String =
        sqlx::query_scalar("SELECT value->>'id' FROM pyo.settings WHERE key = 'install'")
            .fetch_one(db)
            .await?;
    Ok(v)
}

/// What marks the Services a cluster makes, and the older marks it counts
/// as its own too.
async fn marks(db: &PgPool, cluster_id: &str, t: &Tailnet) -> ApiResult<Vec<String>> {
    let install = install_id(db).await?;
    let mut out = vec![ts::marker(&install, cluster_id)];
    out.extend(t.adopt.iter().cloned());
    Ok(out)
}

// ── Asking the agent ────────────────────────────────────────────────────────

/// Sends a command to a cluster's agent and waits for its answer, up to
/// `wait`. A command the agent couldn't carry out is a 400 with its words.
pub(crate) async fn ask(
    s: &AppState,
    cluster_id: &str,
    command: Command,
    wait: Duration,
) -> ApiResult<(String, CommandResult)> {
    let status: Option<String> =
        sqlx::query_scalar("SELECT status FROM pyo.clusters WHERE id = $1")
            .bind(cluster_id)
            .fetch_optional(&s.db)
            .await?;
    match status.as_deref() {
        None => return Err(ApiError::not_found("Cluster")),
        Some("connected" | "degraded") => {}
        Some(_) => {
            return Err(ApiError::conflict(
                "The cluster's agent isn't connected. Check the cluster first.",
            ));
        }
    }
    let mut tx = s.db.begin().await?;
    let id = crate::deploy::enqueue(&mut tx, cluster_id, command).await?;
    tx.commit().await?;
    s.wake(cluster_id).await;
    let deadline = tokio::time::Instant::now() + wait;
    loop {
        let row: Option<(String, Option<Value>)> =
            sqlx::query_as("SELECT status, result FROM pyo.commands WHERE id = $1")
                .bind(&id)
                .fetch_optional(&s.db)
                .await?;
        if let Some((status, result)) = row
            && (status == "done" || status == "failed")
        {
            let r: CommandResult = result
                .and_then(|v| serde_json::from_value(v).ok())
                .unwrap_or_default();
            if !r.ok {
                return Err(ApiError::bad_request(
                    r.message
                        .unwrap_or_else(|| "The cluster's agent couldn't do it.".into()),
                ));
            }
            return Ok((id, r));
        }
        if tokio::time::Instant::now() > deadline {
            abandon(&s.db, &id).await;
            return Err(ApiError::new(
                axum::http::StatusCode::GATEWAY_TIMEOUT,
                "The cluster's agent didn't answer in time. Is it connected?",
            ));
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
}

/// A command no one waits for any more: not sent yet, it never is. Sent,
/// its secrets are blanked in Pyo's copy.
async fn abandon(db: &PgPool, id: &str) {
    let r = async {
        sqlx::query("DELETE FROM pyo.commands WHERE id = $1 AND status = 'pending'")
            .bind(id)
            .execute(db)
            .await?;
        let cmd: Option<Value> =
            sqlx::query_scalar("SELECT command FROM pyo.commands WHERE id = $1")
                .bind(id)
                .fetch_optional(db)
                .await?;
        if let Some(mut cmd) = cmd
            && crate::teambackup::wipe_command(&mut cmd)
        {
            sqlx::query("UPDATE pyo.commands SET command = $2 WHERE id = $1")
                .bind(id)
                .bind(cmd)
                .execute(db)
                .await?;
        }
        Ok::<_, sqlx::Error>(())
    };
    if let Err(e) = r.await {
        tracing::warn!(%id, error = %e, "couldn't drop an unanswered command");
    }
}

/// A command whose answer carried something secret (a join key, a sandbox
/// token): gone once read.
pub(crate) async fn forget_command(db: &PgPool, id: &str) {
    if let Err(e) = sqlx::query("DELETE FROM pyo.commands WHERE id = $1")
        .bind(id)
        .execute(db)
        .await
    {
        tracing::warn!(%id, error = %e, "couldn't delete an answered command");
    }
}

/// Only the team's admins set Tailscale up on its cluster.
pub async fn require_cluster_admin(
    s: &AppState,
    user: &crate::auth::User,
    cluster_id: &str,
) -> ApiResult<()> {
    let team: String = sqlx::query_scalar("SELECT team_id FROM pyo.clusters WHERE id = $1")
        .bind(cluster_id)
        .fetch_optional(&s.db)
        .await?
        .ok_or_else(|| ApiError::not_found("Cluster"))?;
    let mut tx = s.db.begin().await?;
    crate::ops::require_role(&mut tx, user, &team, crate::ops::Role::Admin).await?;
    Ok(())
}

// ── Setting up ──────────────────────────────────────────────────────────────

/// The setup's commands carry the pasted token: checked for shape here, so
/// a wrong paste never leaves Pyo.
fn token_shape(token: &str) -> ApiResult<()> {
    let s = token.trim();
    let why = if s.starts_with("tskey-client-") {
        "That's an OAuth client secret. Paste an API access token, or set up with an OAuth client instead."
    } else if s.starts_with("tskey-auth-") {
        "That's an auth key. Paste an API access token. It starts with tskey-api-."
    } else if !s.starts_with("tskey-api-") || s.len() < 20 || s.contains(char::is_whitespace) {
        "Paste an API access token. It starts with tskey-api-."
    } else {
        return Ok(());
    };
    Err(ApiError::bad_request(why))
}

/// How long the setup may take on the cluster.
const SETUP_WAIT: Duration = Duration::from_secs(90);

/// planTailscaleSetup: what setting the cluster's tailnet up would change.
pub async fn plan(s: &AppState, cluster_id: &str, token: String) -> ApiResult<Value> {
    token_shape(&token)?;
    let (_, r) = ask(
        s,
        cluster_id,
        Command::TailscaleSetup {
            token,
            version: None,
        },
        SETUP_WAIT,
    )
    .await?;
    Ok(json!({ "ok": true, "plan": r.data.unwrap_or(Value::Null) }))
}

/// setUpTailscale: sets the cluster's tailnet up as reviewed. Pyo keeps
/// the client's ID and hint the agent says it kept.
pub async fn set_up(
    s: &AppState,
    cluster_id: &str,
    token: String,
    version: &str,
) -> ApiResult<Value> {
    token_shape(&token)?;
    let (_, r) = ask(
        s,
        cluster_id,
        Command::TailscaleSetup {
            token,
            version: Some(version.to_owned()),
        },
        SETUP_WAIT,
    )
    .await?;
    let mut out = r.data.unwrap_or(Value::Null);
    if let Some(c) = out.as_object_mut().and_then(|o| o.remove("client"))
        && let Some(id) = c["clientId"].as_str()
    {
        let hint = c["hint"].as_str().unwrap_or("");
        keep_client(s, cluster_id, id, hint, None).await?;
    }
    Ok(out)
}

/// Saves the cluster's client as Pyo shows it. Another tailnet: Pyo's
/// names there are picked again.
async fn keep_client(
    s: &AppState,
    cluster_id: &str,
    id: &str,
    hint: &str,
    suffix: Option<String>,
) -> ApiResult<()> {
    let mut t = of(&s.db, cluster_id).await?.unwrap_or_default();
    if t.suffix.is_some() && suffix.is_some() && t.suffix != suffix {
        t.dashboard_name = None;
        t.registry_name = None;
        t.sandboxes_name = None;
    }
    t.client_id = id.to_owned();
    t.hint = hint.to_owned();
    t.suffix = suffix.or(t.suffix);
    // Synced again now.
    t.sent = String::new();
    sqlx::query("UPDATE pyo.clusters SET tailscale = $2 WHERE id = $1")
        .bind(cluster_id)
        .bind(serde_json::to_value(&t).map_err(anyhow::Error::from)?)
        .execute(&s.db)
        .await?;
    Ok(())
}

/// saveTailscale: the cluster uses this OAuth client, once it signs in.
/// The secret goes to the agent sealed and is never kept by Pyo.
pub async fn save_client(
    s: &AppState,
    cluster_id: &str,
    client_id: &str,
    secret: Option<&str>,
) -> ApiResult<Value> {
    let Some(secret) = secret.map(str::trim).filter(|x| !x.is_empty()) else {
        return Err(ApiError::bad_request("Paste the client secret too."));
    };
    let (_, r) = ask(
        s,
        cluster_id,
        Command::TailscaleClient {
            client_id: client_id.trim().to_owned(),
            secret: secret.to_owned(),
        },
        SETUP_WAIT,
    )
    .await?;
    let state: ts::ClientState = r
        .data
        .and_then(|d| serde_json::from_value(d).ok())
        .unwrap_or_default();
    keep_client(
        s,
        cluster_id,
        &state.client_id,
        &state.hint,
        state.suffix.clone(),
    )
    .await?;
    Ok(json!({ "ok": state.ok, "checks": state.checks }))
}

/// The Test button: the cluster's client, check by check.
pub async fn test_saved(s: &AppState, cluster_id: &str) -> ApiResult<Value> {
    if of(&s.db, cluster_id).await?.is_none() {
        return Err(ApiError::bad_request(
            "Tailscale isn't set up on this cluster.",
        ));
    }
    let (_, r) = ask(s, cluster_id, Command::TailscaleTest {}, SETUP_WAIT).await?;
    let state: ts::ClientState = r
        .data
        .and_then(|d| serde_json::from_value(d).ok())
        .unwrap_or_default();
    if let Some(x) = &state.suffix {
        patch(&s.db, cluster_id, json!({ "suffix": x })).await?;
    }
    Ok(json!({ "ok": state.ok, "checks": state.checks }))
}

/// Forgets the cluster's client: Pyo's names on its tailnet are deleted
/// while it still can, and its servers take them off. Pyo forgets it even
/// when the agent can't be reached.
pub async fn remove(s: &AppState, cluster_id: &str) -> ApiResult<()> {
    let Some(t) = of(&s.db, cluster_id).await? else {
        return Ok(());
    };
    let names: Vec<String> =
        sqlx::query_scalar("SELECT name FROM pyo.cluster_tailnet_names WHERE cluster_id = $1")
            .bind(cluster_id)
            .fetch_all(&s.db)
            .await?;
    let marks = marks(&s.db, cluster_id, &t).await?;
    if let Err(e) = ask(
        s,
        cluster_id,
        Command::TailscaleRemove { names, marks },
        SETUP_WAIT,
    )
    .await
    {
        tracing::warn!(cluster = %cluster_id, error = %e.message, "the agent couldn't remove Tailscale; Pyo forgets it anyway");
    }
    sqlx::query("UPDATE pyo.cluster_tailnet_names SET retired_at = coalesce(retired_at, now()), updated_at = now() WHERE cluster_id = $1")
        .bind(cluster_id)
        .execute(&s.db)
        .await?;
    sqlx::query("UPDATE pyo.clusters SET tailscale = NULL WHERE id = $1")
        .bind(cluster_id)
        .execute(&s.db)
        .await?;
    if host_cluster(&s.db).await?.as_deref() == Some(cluster_id) {
        // Without the tailnet, the public dashboard is the way in again.
        sqlx::query("DELETE FROM pyo.settings WHERE key = 'public_dashboard'")
            .execute(&s.db)
            .await?;
        for k in ["tailnet", "registry", "sandboxes"] {
            save_address(&s.db, k, None).await?;
        }
    }
    Ok(())
}

/// Renames Pyo's own name on the tailnet ("dashboard", "registry" or
/// "sandboxes"), on the cluster Pyo runs on. The old one goes once the new
/// one is made.
pub async fn rename(s: &AppState, kind: &str, name: &str) -> ApiResult<()> {
    let name = name.trim().to_ascii_lowercase();
    let Some((cluster_id, mut t)) = host(&s.db).await? else {
        return Err(ApiError::bad_request("Tailscale isn't set up."));
    };
    if !ts::valid_name(&name) {
        return Err(ApiError::bad_request(
            "Use one name: lowercase letters, digits and dashes.",
        ));
    }
    let others = match kind {
        "dashboard" => [&t.registry_name, &t.sandboxes_name],
        "registry" => [&t.dashboard_name, &t.sandboxes_name],
        "sandboxes" => [&t.dashboard_name, &t.registry_name],
        _ => {
            return Err(ApiError::bad_request(
                "The kind is \"dashboard\", \"registry\" or \"sandboxes\".",
            ));
        }
    };
    let domain: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM pyo.cluster_tailnet_names WHERE cluster_id = $1 AND name = $2 AND kind = 'domain' AND retired_at IS NULL)",
    )
    .bind(&cluster_id)
    .bind(&name)
    .fetch_one(&s.db)
    .await?;
    if others.iter().any(|o| o.as_deref() == Some(name.as_str())) || domain {
        return Err(ApiError::conflict("Pyo uses this name already."));
    }
    if peer_names(&s.db, &cluster_id).await?.contains(&name)
        || check_name(s, &cluster_id, &t, &name).await? == "taken"
    {
        return Err(ApiError::conflict("Taken on your tailnet"));
    }
    match kind {
        "dashboard" => t.dashboard_name = Some(name),
        "registry" => t.registry_name = Some(name),
        _ => t.sandboxes_name = Some(name),
    }
    patch(
        &s.db,
        &cluster_id,
        json!({ "dashboardName": t.dashboard_name, "registryName": t.registry_name, "sandboxesName": t.sandboxes_name, "sent": "" }),
    )
    .await
}

/// What a name is on the cluster's tailnet: "free", "ours" or "taken".
async fn check_name(s: &AppState, cluster_id: &str, t: &Tailnet, name: &str) -> ApiResult<String> {
    let marks = marks(&s.db, cluster_id, t).await?;
    let unchecked =
        || ApiError::bad_request("Couldn't check the name on your tailnet. Try again in a minute.");
    let (_, r) = ask(
        s,
        cluster_id,
        Command::TailscaleCheckName {
            name: name.to_owned(),
            marks,
        },
        Duration::from_secs(30),
    )
    .await
    .map_err(|e| {
        tracing::warn!(error = %e.message, "tailnet name check failed");
        unchecked()
    })?;
    let c: ts::NameCheck = r
        .data
        .and_then(|d| serde_json::from_value(d).ok())
        .ok_or_else(unchecked)?;
    Ok(c.state)
}

/// Whether a tailnet domain someone typed for a service on a cluster can be
/// had: Pyo's reserved names and its own names are refused, and so is a
/// name something else on the cluster's tailnet has.
pub async fn check_domain(s: &AppState, cluster_id: &str, host: &str) -> ApiResult<()> {
    let Some(t) = of(&s.db, cluster_id).await? else {
        return Err(ApiError::bad_request(
            "Tailnet domains need Tailscale. Set it up on the cluster.",
        ));
    };
    let Some(suffix) = t.suffix.as_deref() else {
        return Err(ApiError::bad_request(
            "Pyo hasn't seen the cluster's tailnet yet. Try again in a minute.",
        ));
    };
    let host = host.to_ascii_lowercase();
    let Some(label) = host.strip_suffix(&format!(".{suffix}")) else {
        return Err(ApiError::bad_request(format!(
            "A tailnet domain on this cluster ends with .{suffix}."
        )));
    };
    if !ts::valid_name(label) {
        return Err(ApiError::bad_request(
            "Use one name: lowercase letters, digits and dashes.",
        ));
    }
    if RESERVED.contains(&label)
        || t.dashboard_name.as_deref() == Some(label)
        || t.registry_name.as_deref() == Some(label)
        || t.sandboxes_name.as_deref() == Some(label)
    {
        return Err(ApiError::bad_request("Pyo uses this name itself."));
    }
    if peer_names(&s.db, cluster_id).await?.contains(label)
        || check_name(s, cluster_id, &t, label).await? == "taken"
    {
        return Err(ApiError::conflict("Taken on your tailnet"));
    }
    Ok(())
}

// ── Join keys ───────────────────────────────────────────────────────────────

/// The cluster whose agent makes a key for a machine joining a tailnet:
/// `cluster_id`, once Tailscale is set up on it. Else the cluster Pyo runs
/// on (self-hosted: the installation's tailnet).
pub async fn key_cluster(db: &PgPool, cluster_id: Option<&str>) -> ApiResult<String> {
    if let Some(c) = cluster_id
        && of(db, c).await?.is_some()
    {
        return Ok(c.to_owned());
    }
    match host(db).await? {
        Some((h, _)) => Ok(h),
        None => Err(ApiError::bad_request(NEEDED)),
    }
}

/// The cluster whose tailnet a team's new sandbox server joins: one of the
/// team's clusters with Tailscale (a connected one first), else the
/// cluster Pyo runs on. The installation's own servers join Pyo's.
pub async fn team_key_cluster(db: &PgPool, team_id: Option<&str>) -> ApiResult<String> {
    if let Some(t) = team_id {
        let c: Option<String> = sqlx::query_scalar(
            "SELECT id FROM pyo.clusters WHERE team_id = $1 AND tailscale IS NOT NULL
              ORDER BY status IN ('connected', 'degraded') DESC, created_at LIMIT 1",
        )
        .bind(t)
        .fetch_optional(db)
        .await?;
        if let Some(c) = c {
            return Ok(c);
        }
    }
    key_cluster(db, None).await
}

/// A join command's auth key, from the cluster's agent: single-use,
/// preauthorized, tag:pyo, an hour. Pyo keeps it only to put it in the
/// command. `server` names what's being added, e.g. "a sandbox server".
pub async fn join_key(
    s: &AppState,
    cluster_id: &str,
    what: &str,
    server: &'static str,
) -> ApiResult<String> {
    if of(&s.db, cluster_id).await?.is_none() {
        return Err(ApiError::bad_request(NEEDED));
    }
    let (id, r) = ask(
        s,
        cluster_id,
        Command::TailscaleKey {
            description: what.to_owned(),
        },
        Duration::from_secs(30),
    )
    .await
    .map_err(|e| {
        tracing::warn!(error = %e.message, "the cluster made no auth key");
        ApiError::bad_request(format!(
            "Couldn't get a Tailscale key from the cluster to add {server}. Check Tailscale on it."
        ))
    })?;
    forget_command(&s.db, &id).await;
    r.data
        .as_ref()
        .and_then(|d| d[ts::KEY_FIELD].as_str())
        .map(str::to_owned)
        .ok_or_else(|| ApiError::bad_request("Tailscale made no auth key."))
}

// ── Keeping the names ───────────────────────────────────────────────────────

pub async fn runner(state: AppState) {
    loop {
        let st = state.clone();
        crate::bg::as_leader(&state.db, crate::bg::LOCK_TAILSCALE, || async move {
            if let Err(e) = hand_over(&st).await {
                tracing::warn!(error = %e.message, "tailscale hand-over");
            }
            if let Err(e) = tick(&st).await {
                tracing::warn!(error = %format!("{e:?}"), "tailscale");
            }
            if let Err(e) = crate::publicdash::tick(&st).await {
                tracing::warn!(error = %format!("{e:?}"), "public dashboard");
            }
            if let Err(e) = crate::cloud::tick(&st).await {
                tracing::warn!(error = %e.message, "cloud servers");
            }
        })
        .await;
        tokio::time::sleep(Duration::from_secs(20)).await;
    }
}

/// Every process: Pyo's addresses as the runner last saved them.
pub async fn addresses_follower(db: PgPool) {
    loop {
        if let Ok(Some(v)) =
            sqlx::query_scalar::<_, Value>("SELECT value FROM pyo.settings WHERE key = 'addresses'")
                .fetch_optional(&db)
                .await
        {
            let s = |k: &str| v[k].as_str().filter(|x| !x.is_empty()).map(str::to_owned);
            crate::web::set_addresses(crate::web::Addresses {
                tailnet: s("tailnet"),
                registry: s("registry"),
                sandboxes: s("sandboxes"),
                public: s("public"),
                host_ip: s("hostIp"),
                host_name: s("hostName"),
            });
        }
        tokio::time::sleep(Duration::from_secs(10)).await;
    }
}

/// Saves one of Pyo's addresses: its HTTPS names ("tailnet", "registry",
/// "sandboxes", "public") and this machine's on the tailnet ("hostIp", "hostName").
pub async fn save_address(db: &PgPool, key: &str, host: Option<&str>) -> ApiResult<()> {
    sqlx::query(
        "INSERT INTO pyo.settings (key, value) VALUES ('addresses', jsonb_build_object($1::text, $2::text))
         ON CONFLICT (key) DO UPDATE SET value = pyo.settings.value || jsonb_build_object($1::text, $2::text), updated_at = now()",
    )
    .bind(key)
    .bind(host.unwrap_or(""))
    .execute(db)
    .await?;
    Ok(())
}

/// A cluster's servers' Tailscale, as it last reported them.
async fn servers(db: &PgPool, cluster_id: &str) -> ApiResult<Vec<pyo_proto::ServerTailnet>> {
    let v: Option<Value> = sqlx::query_scalar(
        "SELECT coalesce(capabilities->'tailnet', '[]') FROM pyo.clusters WHERE id = $1",
    )
    .bind(cluster_id)
    .fetch_optional(db)
    .await?;
    Ok(v.and_then(|v| serde_json::from_value(v).ok())
        .unwrap_or_default())
}

/// The tailnet's machines as the cluster's servers see them (their
/// tailscaled's peers): the API's device list only shows what the client's
/// tags allow.
async fn peer_names(db: &PgPool, cluster_id: &str) -> ApiResult<BTreeSet<String>> {
    Ok(servers(db, cluster_id)
        .await?
        .into_iter()
        .flat_map(|s| s.peers)
        .collect())
}

/// What a cluster's names on its tailnet should be.
async fn sync_for(
    db: &PgPool,
    install: &str,
    cluster_id: &str,
    cluster_name: &str,
    same_cluster: bool,
    t: &Tailnet,
) -> ApiResult<TailnetSync> {
    let more = crate::util::slugify(cluster_name);
    let own = if same_cluster {
        let o = |kind: &str, current: &Option<String>, base: &str, keep: bool| OwnName {
            kind: kind.into(),
            current: current.clone(),
            base: base.into(),
            more: more.clone(),
            keep,
        };
        vec![
            o("dashboard", &t.dashboard_name, "pyo", true),
            o("registry", &t.registry_name, "registry", true),
            o("sandboxes", &t.sandboxes_name, "sandboxes", false),
        ]
    } else {
        Vec::new()
    };
    let mut domains: Vec<String> = match &t.suffix {
        Some(suffix) => {
            let hosts: Vec<String> = sqlx::query_scalar(
                "SELECT lower(d->>'hostname') FROM pyo.services s, jsonb_array_elements(s.spec->'domains') d
                  WHERE s.cluster_id = $1 AND NOT s.deleting AND lower(d->>'hostname') LIKE '%.' || $2",
            )
            .bind(cluster_id)
            .bind(suffix)
            .fetch_all(db)
            .await?;
            hosts
                .iter()
                .filter_map(|h| h.strip_suffix(&format!(".{suffix}")))
                .filter(|l| ts::valid_name(l) && !RESERVED.contains(l))
                .map(str::to_owned)
                .collect()
        }
        None => Vec::new(),
    };
    domains.sort();
    domains.dedup();
    let made: Vec<String> = sqlx::query_scalar(
        "SELECT name FROM pyo.cluster_tailnet_names WHERE cluster_id = $1 ORDER BY name",
    )
    .bind(cluster_id)
    .fetch_all(db)
    .await?;
    let reported = servers(db, cluster_id).await?;
    let mut peers: Vec<String> = reported.iter().flat_map(|s| s.peers.clone()).collect();
    peers.sort();
    peers.dedup();
    let mut servers: Vec<SyncServer> = reported
        .into_iter()
        .filter(|s| !s.node_id.is_empty())
        .map(|s| SyncServer {
            server: s.server,
            node_id: s.node_id,
            advertised: s.advertised,
        })
        .collect();
    servers.sort_by(|a, b| a.server.cmp(&b.server));
    let mut forget = t.forget.clone();
    forget.sort();
    forget.dedup();
    Ok(TailnetSync {
        mark: ts::marker(install, cluster_id),
        adopt: t.adopt.clone(),
        own,
        domains,
        made,
        peers,
        servers,
        forget,
    })
}

/// A sync is sent again at least this often, for what changed on the
/// tailnet itself (a Service someone deleted, a host to approve).
const RESYNC_MS: i64 = 5 * 60 * 1000;

/// A cluster row as [`tick`] reads it: id, name, same cluster, status, and
/// its Tailscale.
type ClusterRow = (String, String, bool, String, Value);

/// One pass: each cluster with Tailscale gets its names synced when what
/// Pyo wants changed (or a while passed), then Pyo's addresses and the
/// services' tailnet domains follow what the servers report.
pub async fn tick(s: &AppState) -> ApiResult<()> {
    let install = install_id(&s.db).await?;
    let rows: Vec<ClusterRow> = sqlx::query_as(
        "SELECT id, name, same_cluster, status, tailscale FROM pyo.clusters WHERE tailscale IS NOT NULL",
    )
    .fetch_all(&s.db)
    .await?;
    let now = pyo_proto::now_ms() as i64;
    for (id, name, same, status, v) in &rows {
        let Ok(t) = serde_json::from_value::<Tailnet>(v.clone()) else {
            continue;
        };
        if !matches!(status.as_str(), "connected" | "degraded") {
            continue;
        }
        let w = sync_for(&s.db, &install, id, name, *same, &t).await?;
        let key = crate::ids::sha256_hex(&serde_json::to_string(&w).map_err(anyhow::Error::from)?);
        if key == t.sent && now - t.sent_at < RESYNC_MS {
            continue;
        }
        // One at a time: the agent answers each before the next is sent.
        let waiting: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM pyo.commands WHERE cluster_id = $1 AND command->>'kind' = 'tailscale_sync'
                AND status IN ('pending', 'sent') AND created_at > now() - interval '5 minutes')",
        )
        .bind(id)
        .fetch_one(&s.db)
        .await?;
        if waiting {
            continue;
        }
        let mut tx = s.db.begin().await?;
        crate::deploy::enqueue(&mut tx, id, Command::TailscaleSync(Box::new(w))).await?;
        patch(&mut *tx, id, json!({ "sent": key, "sentAt": now })).await?;
        tx.commit().await?;
        s.wake(id).await;
    }

    // Pyo's own names, once a server keeps their certificate.
    let mut ready = std::collections::BTreeMap::new();
    if let Some((id, _, _, _, v)) = rows.iter().find(|r| r.2)
        && let Some(suffix) = v["suffix"].as_str()
    {
        let sv = servers(&s.db, id).await?;
        let names: Vec<(String, String)> = sqlx::query_as(
            "SELECT name, kind FROM pyo.cluster_tailnet_names WHERE cluster_id = $1 AND kind <> 'domain'
                AND retired_at IS NULL AND error IS NULL AND cardinality(addrs) > 0",
        )
        .bind(id)
        .fetch_all(&s.db)
        .await?;
        for (name, kind) in names {
            if sv.iter().any(|x| x.certificates.contains(&name)) {
                ready.insert(kind, format!("{name}.{suffix}"));
            }
        }
    }
    for (key, kind) in [
        ("tailnet", "dashboard"),
        ("registry", "registry"),
        ("sandboxes", "sandboxes"),
    ] {
        save_address(&s.db, key, ready.get(kind).map(String::as_str)).await?;
    }

    for (id, _, _, _, v) in &rows {
        if let Ok(t) = serde_json::from_value::<Tailnet>(v.clone()) {
            follow_domains(&s.db, id, &t).await?;
        }
    }
    Ok(())
}

/// Why a tailnet domain the agent never made isn't live: its last sync
/// failed.
pub const SYNC_FAILED: &str = "Couldn't set it up on your tailnet. Check Tailscale on the cluster.";

/// A tailnet domain's state, from what the cluster's agent and servers
/// report. `status` None: it stays as it is (still being set up, or live).
#[derive(Debug, PartialEq)]
pub struct DomainState {
    pub status: Option<&'static str>,
    pub tls: &'static str,
    pub error: Option<String>,
}

/// `name`: the name as the agent left it (its addresses and error), None
/// before it made it. `served`: a server of the cluster advertises it.
/// `certified`: a server keeps its certificate. `sync_failed`: the
/// cluster's last sync failed.
pub fn domain_state(
    name: Option<(&[String], Option<&str>)>,
    served: bool,
    certified: bool,
    sync_failed: bool,
) -> DomainState {
    let state = |status, tls, error: Option<&str>| DomainState {
        status,
        tls,
        error: error.map(str::to_owned),
    };
    match name {
        Some((_, Some(e))) => state(Some("error"), "failed", Some(e)),
        Some(_) if certified => state(Some("active"), "issued", None),
        Some((addrs, None)) if served && !addrs.is_empty() => {
            state(Some("active"), "pending", None)
        }
        None if sync_failed => state(Some("error"), "pending", Some(SYNC_FAILED)),
        _ => state(None, "pending", None),
    }
}

/// A cluster's services' tailnet domains follow what its agent and servers
/// report: live once a server serves the name, with its certificate as
/// their HTTPS. Pyo can't look these names up itself: only devices on the
/// tailnet resolve them (and Pyo Cloud isn't on the team's tailnet).
async fn follow_domains(db: &PgPool, cluster_id: &str, t: &Tailnet) -> ApiResult<()> {
    let Some(suffix) = t.suffix.as_deref() else {
        return Ok(());
    };
    let sv = servers(db, cluster_id).await?;
    let names: Vec<(String, Vec<String>, Option<String>)> = sqlx::query_as(
        "SELECT name, addrs, error FROM pyo.cluster_tailnet_names WHERE cluster_id = $1 AND kind = 'domain' AND retired_at IS NULL",
    )
    .bind(cluster_id)
    .fetch_all(db)
    .await?;
    let mut hosts: Vec<String> = sqlx::query_scalar(
        "SELECT lower(d->>'hostname') FROM pyo.services s, jsonb_array_elements(s.spec->'domains') d
          WHERE s.cluster_id = $1 AND NOT s.deleting AND lower(d->>'hostname') LIKE '%.' || $2",
    )
    .bind(cluster_id)
    .bind(suffix)
    .fetch_all(db)
    .await?;
    hosts.sort();
    hosts.dedup();
    for host in hosts {
        let Some(label) = host.strip_suffix(&format!(".{suffix}")) else {
            continue;
        };
        let name = names
            .iter()
            .find(|(n, _, _)| n == label)
            .map(|(_, a, e)| (a.as_slice(), e.as_deref()));
        let st = domain_state(
            name,
            sv.iter().any(|x| x.advertised.iter().any(|a| a == label)),
            sv.iter().any(|x| x.certificates.iter().any(|c| c == label)),
            t.sync_failed,
        );
        // Ones going live now, for the activity log.
        let live: Vec<(String, String)> = if st.status == Some("active") {
            sqlx::query_as(
                "SELECT s.id, s.team_id FROM pyo.services s
                  WHERE s.cluster_id = $1 AND NOT s.deleting
                    AND EXISTS (SELECT 1 FROM jsonb_array_elements(s.spec->'domains') d
                                 WHERE lower(d->>'hostname') = $2 AND d->>'status' IS DISTINCT FROM 'active')",
            )
            .bind(cluster_id)
            .bind(&host)
            .fetch_all(db)
            .await?
        } else {
            Vec::new()
        };
        // Without a status: an error goes once the agent no longer reports it.
        sqlx::query(
            "WITH x AS (SELECT $2::text AS host, $3::text AS tls, $4::text AS tls_error, $5::text AS status, $6::text AS error)
             UPDATE pyo.services SET spec = jsonb_set(spec, '{domains}', (
               SELECT coalesce(jsonb_agg(CASE WHEN lower(d->>'hostname') = x.host
                        THEN (d - 'tlsError' - 'error') || jsonb_strip_nulls(jsonb_build_object(
                               'status', coalesce(x.status, CASE WHEN d->>'status' = 'error' THEN 'pending_dns' ELSE d->>'status' END),
                               'tls', x.tls, 'tlsError', x.tls_error, 'error', x.error))
                        ELSE d END), '[]')
                 FROM jsonb_array_elements(spec->'domains') d))
              FROM x
              WHERE cluster_id = $1 AND NOT deleting
                AND EXISTS (SELECT 1 FROM jsonb_array_elements(spec->'domains') d
                             WHERE lower(d->>'hostname') = x.host
                               AND (coalesce(d->>'tls', '') <> x.tls OR coalesce(d->>'tlsError', '') <> coalesce(x.tls_error, '')
                                    OR coalesce(d->>'error', '') <> coalesce(x.error, '')
                                    OR d->>'status' IS DISTINCT FROM coalesce(x.status, CASE WHEN d->>'status' = 'error' THEN 'pending_dns' ELSE d->>'status' END)))",
        )
        .bind(cluster_id)
        .bind(&host)
        .bind(st.tls)
        .bind(st.error.as_ref().filter(|_| st.tls == "failed"))
        .bind(st.status)
        .bind(&st.error)
        .execute(db)
        .await?;
        for (service_id, team_id) in live {
            let mut tx = db.begin().await?;
            crate::ops::log_activity(
                &mut tx,
                &team_id,
                "pyo",
                "domain.verified",
                "success",
                &format!("{host} is on your tailnet now"),
                json!({ "kind": "service", "id": service_id, "label": host }),
                json!({ "serviceId": service_id }),
            )
            .await?;
            tx.commit().await?;
        }
    }
    Ok(())
}

/// A sync's answer (agent.rs, when the agent reports it): the names as the
/// agent left them, the suffix, Pyo's own names as picked, and what Tailnet
/// Lock waits for. Older syncs' commands go.
pub async fn synced(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    cluster_id: &str,
    command_id: &str,
    r: &CommandResult,
) -> ApiResult<()> {
    sqlx::query("DELETE FROM pyo.commands WHERE cluster_id = $1 AND command->>'kind' = 'tailscale_sync' AND id <> $2 AND status IN ('done', 'failed')")
        .bind(cluster_id)
        .bind(command_id)
        .execute(&mut **tx)
        .await?;
    if !r.ok {
        tracing::warn!(cluster = %cluster_id, error = ?r.message, "the cluster couldn't sync its tailnet names");
        // Tried again with the next pass.
        return patch(
            &mut **tx,
            cluster_id,
            json!({ "sent": "", "syncFailed": true }),
        )
        .await;
    }
    let Some(done) = r
        .data
        .clone()
        .and_then(|d| serde_json::from_value::<TailnetSynced>(d).ok())
    else {
        return Ok(());
    };
    let Some(t) = of(&mut **tx, cluster_id).await? else {
        return Ok(());
    };
    for n in &done.names {
        sqlx::query(
            "INSERT INTO pyo.cluster_tailnet_names (cluster_id, name, kind, addrs, error) VALUES ($1, $2, $3, $4, $5)
             ON CONFLICT (cluster_id, name) DO UPDATE SET kind = excluded.kind, addrs = excluded.addrs, error = excluded.error, retired_at = NULL, updated_at = now()
             WHERE (pyo.cluster_tailnet_names.kind, pyo.cluster_tailnet_names.addrs, pyo.cluster_tailnet_names.error, pyo.cluster_tailnet_names.retired_at)
                   IS DISTINCT FROM (excluded.kind, excluded.addrs, excluded.error, NULL)",
        )
        .bind(cluster_id)
        .bind(&n.name)
        .bind(&n.kind)
        .bind(&n.addrs)
        .bind(&n.error)
        .execute(&mut **tx)
        .await?;
    }
    let kept: Vec<String> = done.names.iter().map(|n| n.name.clone()).collect();
    sqlx::query("UPDATE pyo.cluster_tailnet_names SET retired_at = now(), updated_at = now() WHERE cluster_id = $1 AND retired_at IS NULL AND NOT (name = ANY($2))")
        .bind(cluster_id)
        .bind(&kept)
        .execute(&mut **tx)
        .await?;
    sqlx::query("DELETE FROM pyo.cluster_tailnet_names WHERE cluster_id = $1 AND retired_at < now() - interval '7 days'")
        .bind(cluster_id)
        .execute(&mut **tx)
        .await?;
    let own = |kind: &str| {
        done.names
            .iter()
            .find(|n| n.kind == kind)
            .map(|n| n.name.clone())
    };
    let forget: Vec<&String> = t
        .forget
        .iter()
        .filter(|n| !done.forgotten.contains(n))
        .collect();
    patch(
        &mut **tx,
        cluster_id,
        json!({
            "suffix": done.suffix.clone().or(t.suffix.clone()),
            "dashboardName": own("dashboard").or(t.dashboard_name.clone()),
            "registryName": own("registry").or(t.registry_name.clone()),
            "sandboxesName": own("sandboxes").or(t.sandboxes_name.clone()),
            "lockWaiting": done.lock_waiting,
            "machines": done.machines,
            "forget": forget,
            "syncFailed": false,
        }),
    )
    .await
}

/// Takes a machine off a cluster's tailnet with its next sync.
pub async fn forget_machine(db: &PgPool, cluster_id: &str, node_id: &str) -> ApiResult<()> {
    sqlx::query(
        "UPDATE pyo.clusters SET tailscale = jsonb_set(tailscale, '{forget}', coalesce(tailscale->'forget', '[]') || to_jsonb($2::text))
          WHERE id = $1 AND tailscale IS NOT NULL AND NOT coalesce(tailscale->'forget', '[]') ? $2",
    )
    .bind(cluster_id)
    .bind(node_id)
    .execute(db)
    .await?;
    Ok(())
}

/// A row of `pyo.cluster_tailnet_names`: name, kind, addresses, error,
/// retired.
type NameRow = (String, String, Vec<String>, Option<String>, bool);

/// What a cluster's agent hosts on the tailnet (its heartbeat reply). None
/// without Tailscale set up on it, and no names to take off.
pub async fn wanted_for(
    db: &PgPool,
    cluster_id: &str,
    same_cluster: bool,
) -> ApiResult<Option<pyo_proto::TailnetWanted>> {
    let rows: Vec<NameRow> = sqlx::query_as(
        "SELECT name, kind, addrs, error, retired_at IS NOT NULL FROM pyo.cluster_tailnet_names WHERE cluster_id = $1",
    )
    .bind(cluster_id)
    .fetch_all(db)
    .await?;
    // Without Tailscale (removed): the servers take Pyo's names off.
    let suffix = match of(db, cluster_id).await?.and_then(|t| t.suffix) {
        Some(x) => x,
        None if rows.is_empty() => return Ok(None),
        None => String::new(),
    };
    let mut w = pyo_proto::TailnetWanted {
        suffix: suffix.clone(),
        ..Default::default()
    };
    for (name, kind, addrs, error, retired) in rows {
        let here = !suffix.is_empty() && !retired && error.is_none() && !addrs.is_empty();
        if !here {
            w.retired.push(name);
            continue;
        }
        if same_cluster {
            match kind.as_str() {
                "dashboard" => w.own.dashboard = Some(format!("{name}.{suffix}")),
                "registry" => w.own.registry = Some(format!("{name}.{suffix}")),
                "sandboxes" => w.own.sandboxes = Some(format!("{name}.{suffix}")),
                _ => {}
            }
        }
        w.services.push(pyo_proto::TailnetService { name, addrs });
    }
    Ok(Some(w))
}

// ── The installation's client, before clusters kept their own ──────────────

/// What `pyo.settings` ('tailscale') held before: the installation's
/// client and Pyo's names on its tailnet.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Before {
    client_id: String,
    secret_sealed: String,
    #[serde(default)]
    dashboard_name: Option<String>,
    #[serde(default)]
    registry_name: Option<String>,
    #[serde(default)]
    sandboxes_name: Option<String>,
    #[serde(default)]
    suffix: Option<String>,
}

/// An installation from before: its client goes, once, in a signed command
/// (sealed while queued), to the agent of the cluster Pyo runs on, and to
/// each other connected cluster that already has names or servers on the
/// tailnet, so nothing there stops working. Each counts the Services Pyo
/// made before as its own. Once every one keeps it, Pyo deletes its copy.
pub async fn hand_over(s: &AppState) -> ApiResult<()> {
    let v: Option<Value> =
        sqlx::query_scalar("SELECT value FROM pyo.settings WHERE key = 'tailscale'")
            .fetch_optional(&s.db)
            .await?;
    let Some(before) = v.and_then(|v| serde_json::from_value::<Before>(v).ok()) else {
        return Ok(());
    };
    let host = host_cluster(&s.db).await?;
    let others: Vec<String> = sqlx::query_scalar(
        "SELECT id FROM pyo.clusters c WHERE NOT same_cluster AND status IN ('connected', 'degraded')
            AND (EXISTS (SELECT 1 FROM pyo.cluster_tailnet_names n WHERE n.cluster_id = c.id AND n.retired_at IS NULL)
                 OR EXISTS (SELECT 1 FROM jsonb_array_elements(coalesce(c.capabilities->'tailnet', '[]')) t WHERE coalesce(t->>'nodeId', '') <> ''))
          ORDER BY created_at",
    )
    .fetch_all(&s.db)
    .await?;
    let clusters: Vec<String> = host.iter().cloned().chain(others).collect();
    if clusters.is_empty() {
        return Ok(());
    }
    hand_over_to(s, &clusters, host.as_deref(), &before).await
}

async fn hand_over_to(
    s: &AppState,
    clusters: &[String],
    host: Option<&str>,
    before: &Before,
) -> ApiResult<()> {
    let install = install_id(&s.db).await?;
    let mut all = true;
    for cluster_id in clusters {
        if of(&s.db, cluster_id).await?.is_some() {
            continue;
        }
        let secret = s.sealer.open(&before.secret_sealed)?;
        let r = ask(
            s,
            cluster_id,
            Command::TailscaleClient {
                client_id: before.client_id.clone(),
                secret,
            },
            SETUP_WAIT,
        )
        .await;
        let r = match r {
            Ok((_, r)) => r,
            Err(e) => {
                tracing::warn!(cluster = %cluster_id, error = %e.message, "couldn't hand the Tailscale client over yet");
                all = false;
                continue;
            }
        };
        let state: ts::ClientState = r
            .data
            .and_then(|d| serde_json::from_value(d).ok())
            .unwrap_or_default();
        let own = host == Some(cluster_id.as_str());
        let t = Tailnet {
            client_id: state.client_id,
            hint: state.hint,
            suffix: state.suffix.or(before.suffix.clone()),
            dashboard_name: before.dashboard_name.clone().filter(|_| own),
            registry_name: before.registry_name.clone().filter(|_| own),
            sandboxes_name: before.sandboxes_name.clone().filter(|_| own),
            adopt: vec![ts::install_marker(&install)],
            ..Default::default()
        };
        sqlx::query("UPDATE pyo.clusters SET tailscale = $2 WHERE id = $1")
            .bind(cluster_id)
            .bind(serde_json::to_value(&t).map_err(anyhow::Error::from)?)
            .execute(&s.db)
            .await?;
        tracing::info!(cluster = %cluster_id, "the installation's Tailscale client now lives in the cluster");
    }
    if all {
        sqlx::query("DELETE FROM pyo.settings WHERE key = 'tailscale'")
            .execute(&s.db)
            .await?;
        tracing::info!("Pyo no longer keeps a Tailscale client");
    }
    Ok(())
}

// ── For the dashboard ───────────────────────────────────────────────────────

#[derive(Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub struct LockQuery {
    /// The team adding the server (slug or id).
    team: String,
}

/// Machines waiting for a Tailnet Lock signature
///
/// With Tailnet Lock on, a new server stays off the tailnet until a
/// signing device signs its key. These are the ones on the team's
/// clusters' tailnets that wait, with the command to run on a signing
/// device, as each cluster last saw them. Empty without Tailscale set up.
#[utoipa::path(get, path = "/tailnet/lock", operation_id = "getTailnetLock", tag = "Servers",
    params(LockQuery),
    responses(
        (status = 200, description = "`waiting`: each with `name` (the machine's hostname) and `signCommand`.", body = Object),
        (status = 403, description = "Only owners and admins add servers.", body = crate::error::ErrorBody),
        (status = 404, description = "No such team, or you aren't in it.", body = crate::error::ErrorBody),
    ),
    extensions(("x-pyo-access" = json!("read"))))]
pub async fn lock(
    axum::extract::State(s): axum::extract::State<AppState>,
    crate::auth::SessionUser(user): crate::auth::SessionUser,
    crate::error::Query(q): crate::error::Query<LockQuery>,
) -> ApiResult<crate::error::Json<Value>> {
    let mut tx = s.db.begin().await?;
    crate::ops::require_role(&mut tx, &user, &q.team, crate::ops::Role::Admin).await?;
    let team_id: String = sqlx::query_scalar("SELECT id FROM pyo.teams WHERE id = $1 OR slug = $1")
        .bind(&q.team)
        .fetch_one(&mut *tx)
        .await?;
    drop(tx);
    let rows: Vec<Value> = sqlx::query_scalar(
        "SELECT tailscale->'lockWaiting' FROM pyo.clusters WHERE (team_id = $1 OR same_cluster) AND tailscale ? 'lockWaiting'",
    )
    .bind(&team_id)
    .fetch_all(&s.db)
    .await?;
    let mut waiting: Vec<LockWaiting> = rows
        .into_iter()
        .flat_map(|v| serde_json::from_value::<Vec<LockWaiting>>(v).unwrap_or_default())
        .collect();
    waiting.sort_by(|a, b| a.name.cmp(&b.name));
    waiting.dedup();
    Ok(crate::error::Json(json!({ "waiting": waiting })))
}

/// A cluster's Tailscale for the dashboard: never a secret.
pub fn cluster_view(t: &Tailnet) -> Value {
    json!({
        "clientId": t.client_id,
        "hint": t.hint,
        "suffix": t.suffix,
    })
}

/// For the dashboard: the installation's tailnet (the cluster Pyo runs on)
/// and Pyo's names on it; the client (never the secret) only for admins.
pub async fn snapshot(db: &PgPool, admin: bool) -> ApiResult<Value> {
    let Some((cluster_id, t)) = host(db).await? else {
        return Ok(Value::Null);
    };
    let a = crate::web::addresses();
    let mut v = json!({
        "clusterId": cluster_id,
        "suffix": t.suffix,
        "dashboard": a.tailnet,
        "registry": a.registry,
        "sandboxes": a.sandboxes,
        "onTailnet": a.host_ip.is_some(),
        "hostMove": host_move(db).await?,
    });
    if admin {
        let rows: Vec<(String, Option<String>)> = sqlx::query_as(
            "SELECT name, error FROM pyo.cluster_tailnet_names WHERE cluster_id = $1 AND kind IN ('dashboard', 'registry', 'sandboxes') AND retired_at IS NULL",
        )
        .bind(&cluster_id)
        .fetch_all(db)
        .await?;
        let error = |n: &Option<String>| {
            n.as_ref()
                .and_then(|n| rows.iter().find(|r| r.0 == *n))
                .and_then(|r| r.1.clone())
        };
        v["clientId"] = json!(t.client_id);
        v["hint"] = json!(t.hint);
        v["dashboardName"] = json!(t.dashboard_name);
        v["dashboardError"] = json!(error(&t.dashboard_name));
        v["registryName"] = json!(t.registry_name);
        v["registryError"] = json!(error(&t.registry_name));
        v["sandboxesName"] = json!(t.sandboxes_name);
        v["sandboxesError"] = json!(error(&t.sandboxes_name));
        v["hostName"] = json!(a.host_name);
    }
    Ok(v)
}

/// Pyo's own server moving k3s onto the tailnet: `{"state": "moving"}`, or
/// `{"state": "failed", "error"}`. Null once done, or when it never did.
async fn host_move(db: &PgPool) -> ApiResult<Value> {
    let v: Option<(Value, bool)> = sqlx::query_as(
        "SELECT value, updated_at < now() - interval '30 minutes' FROM pyo.settings WHERE key = 'host_k3s'",
    )
    .fetch_optional(db)
    .await?;
    Ok(match v {
        // A move that never said how it ended: Pyo's own restart lost it.
        Some((v, stale)) if v["state"] == "moving" && !stale => v,
        Some((v, _)) if v["state"] == "failed" => v,
        _ => Value::Null,
    })
}

/// When Pyo's server last got a key to join with: one at a time.
static HOST_KEY_AT: std::sync::Mutex<Option<std::time::Instant>> = std::sync::Mutex::new(None);

/// Pyo's own server, on its timer (install.sh `host_units`): what its
/// Tailscale says (`ip` and `name`, empty when it's not on a tailnet). Kept
/// as Pyo's tailnet address. Answers `{"state": "off"}` without Tailscale
/// on Pyo's cluster, `{"state": "ok"}` once it's on the tailnet, or
/// `{"state": "join", "authKey"}` to join it (at most one key every ten
/// minutes, made by the cluster's agent).
pub async fn host_report(
    s: &AppState,
    ip: &str,
    name: &str,
    k3s: &str,
    error: &str,
) -> ApiResult<Value> {
    // Moving k3s onto the tailnet: what the dashboard shows meanwhile.
    if matches!(k3s, "moving" | "done" | "failed") {
        let error: String = error.trim().chars().take(300).collect();
        sqlx::query(
            "INSERT INTO pyo.settings (key, value) VALUES ('host_k3s', jsonb_build_object('state', $1::text, 'error', $2::text))
             ON CONFLICT (key) DO UPDATE SET value = excluded.value, updated_at = now()",
        )
        .bind(k3s)
        .bind((k3s == "failed").then_some(error))
        .execute(&s.db)
        .await?;
        tracing::info!(state = k3s, "this machine: k3s onto the tailnet");
    }
    let ip = ip.trim();
    let ip = crate::freedomain::tailnet_v4(ip).map(|v| v.to_string());
    let name = name.trim().trim_end_matches('.').to_ascii_lowercase();
    let name = (ip.is_some() && name.ends_with(".ts.net") && name.split('.').all(ts::valid_name))
        .then_some(name);
    let a = crate::web::addresses();
    if a.host_ip != ip || a.host_name != name {
        save_address(&s.db, "hostIp", ip.as_deref()).await?;
        save_address(&s.db, "hostName", name.as_deref()).await?;
    }
    let Some((cluster_id, _)) = host(&s.db).await? else {
        return Ok(json!({ "state": "off" }));
    };
    if ip.is_some() {
        return Ok(json!({ "state": "ok" }));
    }
    {
        let mut at = HOST_KEY_AT.lock().unwrap_or_else(|e| e.into_inner());
        if at.is_some_and(|t| t.elapsed() < Duration::from_secs(600)) {
            return Ok(json!({ "state": "wait" }));
        }
        *at = Some(std::time::Instant::now());
    }
    let key = join_key(s, &cluster_id, "Pyo: its own server", "this server").await?;
    tracing::info!("this machine: joining the tailnet");
    Ok(json!({ "state": "join", "authKey": key }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tailnet_keeps_nothing_secret() {
        let t = Tailnet {
            client_id: "kC1".into(),
            hint: "…WXYZ".into(),
            suffix: Some("tail1.ts.net".into()),
            ..Default::default()
        };
        let text = serde_json::to_value(&t).unwrap().to_string();
        assert!(!text.to_lowercase().contains("secret"), "{text}");
        assert_eq!(cluster_view(&t)["clientId"], "kC1");
    }

    #[test]
    fn tokens_are_checked_before_they_leave_pyo() {
        assert!(token_shape("tskey-api-kAbc-0123456789abcdef").is_ok());
        assert!(token_shape("tskey-client-kAbc-0123456789abcdef").is_err());
        assert!(token_shape("tskey-auth-kAbc-0123456789abcdef").is_err());
        assert!(token_shape("nope").is_err());
    }

    #[test]
    fn credentials_are_wiped_from_answered_commands() {
        let mut setup =
            json!({ "kind": "tailscale_setup", "token": "pyo1:sealed", "version": "v1" });
        assert!(crate::teambackup::wipe_command(&mut setup));
        assert_eq!(
            setup,
            json!({ "kind": "tailscale_setup", "token": "", "version": "v1" })
        );
        let mut client =
            json!({ "kind": "tailscale_client", "clientId": "k1", "secret": "pyo1:sealed" });
        assert!(crate::teambackup::wipe_command(&mut client));
        assert_eq!(client["secret"], "");
        assert_eq!(client["clientId"], "k1");
        let mut other = json!({ "kind": "backup", "password": "pyo1:sealed" });
        assert!(!crate::teambackup::wipe_command(&mut other));
        assert_eq!(other["password"], "pyo1:sealed");
    }

    // Against a throwaway Postgres (DATABASE_URL), with a stand-in agent
    // that answers the cluster's commands.

    async fn db() -> (AppState, String) {
        let url = std::env::var("DATABASE_URL").expect("DATABASE_URL");
        let pool = sqlx::PgPool::connect(&url).await.unwrap();
        crate::migrate::run(&pool).await.unwrap();
        crate::vault::init(crate::vault::Vault::new(
            pool.clone(),
            crate::vault::Provider::from_env(),
        ));
        let run = crate::ids::new_id("t");
        let (team, cluster) = (format!("team_{run}"), format!("cl_{run}"));
        sqlx::query("INSERT INTO pyo.teams (id, name, slug) VALUES ($1, $1, $1)")
            .bind(&team)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO pyo.clusters (id, team_id, name, agent_token_hash, status) VALUES ($1, $2, $1, $1, 'connected')")
            .bind(&cluster)
            .bind(&team)
            .execute(&pool)
            .await
            .unwrap();
        (crate::sandbox::e2e::test_state(pool), cluster)
    }

    /// The agent: takes the cluster's next command of `kind`, opened as it
    /// would get it, and answers it with `data`.
    async fn agent_answers(db: PgPool, cluster: String, kind: &str, data: Value) -> Value {
        loop {
            let row: Option<(String, Value)> = sqlx::query_as(
                "SELECT id, command FROM pyo.commands WHERE cluster_id = $1 AND command->>'kind' = $2 AND status = 'pending' ORDER BY created_at LIMIT 1",
            )
            .bind(&cluster)
            .bind(kind)
            .fetch_optional(&db)
            .await
            .unwrap();
            if let Some((id, mut cmd)) = row {
                crate::vault::get().open_env(&mut cmd).await.unwrap();
                crate::teambackup::open_command(&mut cmd).await.unwrap();
                let r = CommandResult {
                    ok: true,
                    message: None,
                    logs: vec![],
                    data: Some(data),
                };
                sqlx::query("UPDATE pyo.commands SET status = 'done', result = $2, done_at = now() WHERE id = $1")
                    .bind(&id)
                    .bind(serde_json::to_value(&r).unwrap())
                    .execute(&db)
                    .await
                    .unwrap();
                return cmd;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    async fn commands(db: &PgPool, cluster: &str, kind: &str) -> i64 {
        sqlx::query_scalar(
            "SELECT count(*) FROM pyo.commands WHERE cluster_id = $1 AND command->>'kind' = $2",
        )
        .bind(cluster)
        .bind(kind)
        .fetch_one(db)
        .await
        .unwrap()
    }

    /// An installation from before: its client goes to its cluster's agent
    /// once, and Pyo keeps nothing of it but the ID and hint.
    #[tokio::test]
    #[ignore = "needs DATABASE_URL (a throwaway Postgres)"]
    async fn the_installations_client_goes_to_its_cluster_once() {
        let (s, cluster) = db().await;
        let secret = "tskey-client-kAbc-SECRETSECRET1234";
        let before = Before {
            client_id: "kAbc".into(),
            secret_sealed: s.sealer.seal(secret).unwrap(),
            dashboard_name: Some("pyo".into()),
            registry_name: Some("registry".into()),
            suffix: Some("tail1.ts.net".into()),
            ..Default::default()
        };
        let agent = tokio::spawn(agent_answers(
            s.db.clone(),
            cluster.clone(),
            "tailscale_client",
            json!({ "ok": true, "checks": [], "clientId": "kAbc", "hint": "…1234", "suffix": "tail1.ts.net" }),
        ));
        hand_over_to(&s, std::slice::from_ref(&cluster), Some(&cluster), &before)
            .await
            .unwrap();
        let sent = agent.await.unwrap();
        assert_eq!(sent["client_id"], "kAbc");
        assert_eq!(sent["secret"], secret, "the agent gets the secret");
        let t = of(&s.db, &cluster).await.unwrap().unwrap();
        assert_eq!((t.client_id.as_str(), t.hint.as_str()), ("kAbc", "…1234"));
        assert_eq!(t.dashboard_name.as_deref(), Some("pyo"));
        let install = install_id(&s.db).await.unwrap();
        assert_eq!(t.adopt, [ts::install_marker(&install)]);
        let kept: Option<Value> =
            sqlx::query_scalar("SELECT value FROM pyo.settings WHERE key = 'tailscale'")
                .fetch_optional(&s.db)
                .await
                .unwrap();
        assert_eq!(kept, None, "Pyo's copy is deleted");
        let leaked: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM pyo.clusters WHERE tailscale::text LIKE '%SECRETSECRET%'",
        )
        .fetch_one(&s.db)
        .await
        .unwrap();
        assert_eq!(leaked, 0);
    }

    /// A join key comes from the cluster's agent, and its command (with the
    /// key in its answer) is gone once read.
    #[tokio::test]
    #[ignore = "needs DATABASE_URL (a throwaway Postgres)"]
    async fn a_join_key_comes_from_the_cluster_and_is_not_kept() {
        let (s, cluster) = db().await;
        assert!(
            join_key(&s, &cluster, "Pyo: join web-2", "this server")
                .await
                .is_err(),
            "no Tailscale on it yet"
        );
        sqlx::query("UPDATE pyo.clusters SET tailscale = '{\"clientId\": \"k1\"}' WHERE id = $1")
            .bind(&cluster)
            .execute(&s.db)
            .await
            .unwrap();
        let agent = tokio::spawn(agent_answers(
            s.db.clone(),
            cluster.clone(),
            "tailscale_key",
            json!({ ts::KEY_FIELD: "tskey-auth-k9-SECRET" }),
        ));
        let key = join_key(&s, &cluster, "Pyo: join web-2", "this server")
            .await
            .unwrap();
        assert_eq!(key, "tskey-auth-k9-SECRET");
        assert_eq!(agent.await.unwrap()["description"], "Pyo: join web-2");
        assert_eq!(commands(&s.db, &cluster, "tailscale_key").await, 0);
        assert_eq!(key_cluster(&s.db, Some(&cluster)).await.unwrap(), cluster);
    }

    /// Pyo's calls to a sandbox server that joined through a cluster go
    /// through that cluster's agent, and leave nothing behind.
    #[tokio::test]
    #[ignore = "needs DATABASE_URL (a throwaway Postgres)"]
    async fn sandbox_calls_go_through_the_cluster() {
        let (s, cluster) = db().await;
        crate::sandbox::daemon::init(s.clone());
        let address = format!(
            "100.64.{}.{}",
            rand::random::<u8>() % 64,
            1 + rand::random::<u8>() % 250
        );
        sqlx::query("INSERT INTO pyo.sandbox_servers (id, name, state, created_by, address, via_cluster) VALUES ($1, $1, 'ready', 'test', $2, $3)")
            .bind(crate::ids::new_id("sbs"))
            .bind(&address)
            .bind(&cluster)
            .execute(&s.db)
            .await
            .unwrap();
        let agent = tokio::spawn(agent_answers(
            s.db.clone(),
            cluster.clone(),
            "sandbox_call",
            // Bytes both ways: the agent answers in base64 too.
            json!({ "status": 200, "body": "eyJieXRlcyI6IDV9" }),
        ));
        let d = crate::sandbox::daemon::Daemon::new(&address);
        let n = d
            .write_file("tok", "sbx_1", "/a b", b"hello".to_vec())
            .await
            .unwrap();
        assert_eq!(n, 5);
        let sent = agent.await.unwrap();
        assert_eq!(sent["address"], json!(address));
        assert_eq!(sent["method"], "PUT");
        assert_eq!(sent["path"], "/v1/sandboxes/sbx_1/files?path=%2Fa+b");
        assert_eq!(sent["token"], "tok");
        assert_eq!(sent["raw"], true);
        assert_eq!(sent["body"], "aGVsbG8=");
        assert_eq!(commands(&s.db, &cluster, "sandbox_call").await, 0);
    }

    /// A sync's answer: names kept, the rest retired, machines taken off
    /// forgotten, and the servers told what to host.
    #[tokio::test]
    #[ignore = "needs DATABASE_URL (a throwaway Postgres)"]
    async fn a_sync_answer_is_kept() {
        let (s, cluster) = db().await;
        sqlx::query("UPDATE pyo.clusters SET tailscale = '{\"clientId\": \"k1\", \"forget\": [\"n1\", \"n2\"]}' WHERE id = $1")
            .bind(&cluster)
            .execute(&s.db)
            .await
            .unwrap();
        sqlx::query("INSERT INTO pyo.cluster_tailnet_names (cluster_id, name, kind, addrs) VALUES ($1, 'old', 'domain', '{100.100.1.1}')")
            .bind(&cluster)
            .execute(&s.db)
            .await
            .unwrap();
        let done = TailnetSynced {
            suffix: Some("tail1.ts.net".into()),
            names: vec![
                ts::NameState {
                    name: "blog".into(),
                    kind: "domain".into(),
                    addrs: vec!["100.100.1.2".into()],
                    error: None,
                },
                ts::NameState {
                    name: "shop".into(),
                    kind: "domain".into(),
                    addrs: vec![],
                    error: Some("Taken on your tailnet".into()),
                },
            ],
            forgotten: vec!["n1".into()],
            ..Default::default()
        };
        let r = CommandResult {
            ok: true,
            message: None,
            logs: vec![],
            data: Some(serde_json::to_value(&done).unwrap()),
        };
        let mut tx = s.db.begin().await.unwrap();
        synced(&mut tx, &cluster, "cmd_x", &r).await.unwrap();
        tx.commit().await.unwrap();
        let t = of(&s.db, &cluster).await.unwrap().unwrap();
        assert_eq!(t.suffix.as_deref(), Some("tail1.ts.net"));
        assert_eq!(t.forget, ["n2"]);
        let w = wanted_for(&s.db, &cluster, false).await.unwrap().unwrap();
        assert_eq!(w.suffix, "tail1.ts.net");
        assert_eq!(
            w.services
                .iter()
                .map(|x| x.name.as_str())
                .collect::<Vec<_>>(),
            ["blog"]
        );
        let mut retired = w.retired.clone();
        retired.sort();
        assert_eq!(retired, ["old", "shop"]);
    }

    #[test]
    fn a_tailnet_domain_is_live_once_a_server_serves_it() {
        let addrs = vec!["100.100.1.2".to_owned()];
        let made = Some((addrs.as_slice(), None));
        let st = |name, served, certified, failed| domain_state(name, served, certified, failed);
        // Not made yet, or made and not served yet: still being set up.
        assert_eq!(st(None, false, false, false).status, None);
        assert_eq!(st(made, false, false, false).status, None);
        assert_eq!(st(Some((&[][..], None)), true, false, false).status, None);
        // Served: live, its certificate on its way, then issued.
        assert_eq!(
            st(made, true, false, false),
            DomainState {
                status: Some("active"),
                tls: "pending",
                error: None
            }
        );
        assert_eq!(
            st(made, true, true, false),
            DomainState {
                status: Some("active"),
                tls: "issued",
                error: None
            }
        );
        assert_eq!(st(made, false, true, false).status, Some("active"));
        // The agent couldn't make it.
        assert_eq!(
            st(
                Some((&[][..], Some("Taken on your tailnet"))),
                false,
                false,
                false
            ),
            DomainState {
                status: Some("error"),
                tls: "failed",
                error: Some("Taken on your tailnet".into())
            }
        );
        // The sync failed: only names it never made say so.
        assert_eq!(st(None, false, false, true).status, Some("error"));
        assert_eq!(
            st(None, false, false, true).error.as_deref(),
            Some(SYNC_FAILED)
        );
        assert_eq!(st(made, false, false, true).status, None);
        assert_eq!(st(made, true, false, true).status, Some("active"));
    }

    /// Services' tailnet domains follow the cluster: set up, live once a
    /// server serves them, an error when the agent reports one, and never
    /// looked up in DNS.
    #[tokio::test]
    #[ignore = "needs DATABASE_URL (a throwaway Postgres)"]
    async fn tailnet_domains_follow_the_cluster() {
        let (s, cluster) = db().await;
        let team: String = sqlx::query_scalar("SELECT team_id FROM pyo.clusters WHERE id = $1")
            .bind(&cluster)
            .fetch_one(&s.db)
            .await
            .unwrap();
        let run = crate::ids::new_id("t");
        let suffix = format!("{}.ts.net", run.to_lowercase().replace('_', "-"));
        let (project, service) = (format!("prj_{run}"), format!("svc_{run}"));
        sqlx::query(
            "INSERT INTO pyo.projects (id, team_id, name, namespace) VALUES ($1, $2, $1, $1)",
        )
        .bind(&project)
        .bind(&team)
        .execute(&s.db)
        .await
        .unwrap();
        let domain = |id: &str, host: &str| json!({ "id": id, "hostname": host, "portName": "http", "status": "pending_dns", "tls": "pending", "tlsMode": "auto", "redirectHttps": false, "createdAt": 1 });
        let (blog, shop, wiki) = (
            format!("blog.{suffix}"),
            format!("shop.{suffix}"),
            format!("wiki.{suffix}"),
        );
        let spec = json!({ "domains": [domain("dom_b", &blog), domain("dom_s", &shop), domain("dom_w", &wiki)] });
        sqlx::query("INSERT INTO pyo.services (id, team_id, project_id, cluster_id, name, spec) VALUES ($1, $2, $3, $4, 'web', $5)")
            .bind(&service)
            .bind(&team)
            .bind(&project)
            .bind(&cluster)
            .bind(&spec)
            .execute(&s.db)
            .await
            .unwrap();
        let t = Tailnet {
            client_id: "k1".into(),
            suffix: Some(suffix.clone()),
            ..Default::default()
        };
        sqlx::query("UPDATE pyo.clusters SET tailscale = $2 WHERE id = $1")
            .bind(&cluster)
            .bind(serde_json::to_value(&t).unwrap())
            .execute(&s.db)
            .await
            .unwrap();
        let state = || async {
            let spec: Value = sqlx::query_scalar("SELECT spec FROM pyo.services WHERE id = $1")
                .bind(&service)
                .fetch_one(&s.db)
                .await
                .unwrap();
            spec["domains"]
                .as_array()
                .unwrap()
                .iter()
                .map(|d| {
                    (
                        d["status"].as_str().unwrap_or("").to_owned(),
                        d["tls"].as_str().unwrap_or("").to_owned(),
                        d["error"].as_str().map(str::to_owned),
                    )
                })
                .collect::<Vec<_>>()
        };
        let pending = ("pending_dns".to_owned(), "pending".to_owned(), None);

        // The DNS watcher leaves them alone: Pyo can't look them up.
        crate::deploy::check_pending_dns(&s).await.unwrap();
        let checked: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM pyo.services s, jsonb_array_elements(s.spec->'domains') d WHERE s.id = $1 AND d ? 'dnsCheckedAt'",
        )
        .bind(&service)
        .fetch_one(&s.db)
        .await
        .unwrap();
        assert_eq!(checked, 0);

        // The sync failed before the agent made any: they say so.
        let failed = CommandResult {
            ok: false,
            message: Some("x".into()),
            logs: vec![],
            data: None,
        };
        let mut tx = s.db.begin().await.unwrap();
        synced(&mut tx, &cluster, "cmd_1", &failed).await.unwrap();
        tx.commit().await.unwrap();
        let t = of(&s.db, &cluster).await.unwrap().unwrap();
        assert!(t.sync_failed);
        follow_domains(&s.db, &cluster, &t).await.unwrap();
        let failing = (
            "error".to_owned(),
            "pending".to_owned(),
            Some(SYNC_FAILED.to_owned()),
        );
        assert_eq!(state().await, [failing.clone(), failing.clone(), failing]);

        // The next sync made blog and wiki, and shop is someone else's.
        let name = |n: &str, error: Option<&str>| ts::NameState {
            name: n.into(),
            kind: "domain".into(),
            addrs: if error.is_some() {
                vec![]
            } else {
                vec!["100.100.1.2".into()]
            },
            error: error.map(str::to_owned),
        };
        let done = TailnetSynced {
            suffix: Some(suffix.clone()),
            names: vec![
                name("blog", None),
                name("shop", Some("Taken on your tailnet")),
                name("wiki", None),
            ],
            ..Default::default()
        };
        let ok = CommandResult {
            ok: true,
            message: None,
            logs: vec![],
            data: Some(serde_json::to_value(&done).unwrap()),
        };
        let mut tx = s.db.begin().await.unwrap();
        synced(&mut tx, &cluster, "cmd_2", &ok).await.unwrap();
        tx.commit().await.unwrap();
        let t = of(&s.db, &cluster).await.unwrap().unwrap();
        assert!(!t.sync_failed);
        follow_domains(&s.db, &cluster, &t).await.unwrap();
        let taken = (
            "error".to_owned(),
            "failed".to_owned(),
            Some("Taken on your tailnet".to_owned()),
        );
        assert_eq!(
            state().await,
            [pending.clone(), taken.clone(), pending.clone()]
        );

        // A server serves blog: live. Then it keeps blog's certificate.
        let server = |certificates: Vec<String>| json!([{ "server": "web-1", "nodeId": "n1", "advertised": ["blog"], "certificates": certificates }]);
        let report = |v: Value| {
            sqlx::query("UPDATE pyo.clusters SET capabilities = jsonb_set(coalesce(capabilities, '{}'), '{tailnet}', $2) WHERE id = $1")
                .bind(&cluster)
                .bind(v)
                .execute(&s.db)
        };
        report(server(vec![])).await.unwrap();
        follow_domains(&s.db, &cluster, &t).await.unwrap();
        let live = ("active".to_owned(), "pending".to_owned(), None);
        assert_eq!(state().await, [live, taken.clone(), pending.clone()]);
        let logged: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM pyo.activity WHERE team_id = $1 AND data->>'type' = 'domain.verified'",
        )
        .bind(&team)
        .fetch_one(&s.db)
        .await
        .unwrap();
        assert_eq!(logged, 1);
        report(server(vec!["blog".into()])).await.unwrap();
        follow_domains(&s.db, &cluster, &t).await.unwrap();
        let issued = ("active".to_owned(), "issued".to_owned(), None);
        assert_eq!(
            state().await,
            [issued.clone(), taken.clone(), pending.clone()]
        );

        // Its routes syncing leaves them as they are.
        let mut tx = s.db.begin().await.unwrap();
        crate::routes::mark_routed(&mut tx, &service, None)
            .await
            .unwrap();
        tx.commit().await.unwrap();
        assert_eq!(state().await, [issued, taken, pending]);
    }

    #[test]
    fn the_old_settings_are_read_for_the_hand_over() {
        let v = json!({
            "clientId": "kC1", "secretSealed": "v1:abc", "hint": "…WXYZ",
            "dashboardName": "pyo", "registryName": "registry", "suffix": "tail1.ts.net"
        });
        let b: Before = serde_json::from_value(v).unwrap();
        assert_eq!(b.client_id, "kC1");
        assert_eq!(b.secret_sealed, "v1:abc");
        assert_eq!(b.dashboard_name.as_deref(), Some("pyo"));
        assert_eq!(b.sandboxes_name, None);
    }
}
