//! Tailscale, through the clusters' agents (pyo-proto's tailscale.rs).
//!
//! A team's Tailscale is one OAuth client, kept as a Secret in the
//! pyo-system of one of its clusters: the *holder*. Pyo never holds a
//! Tailscale credential: it passes a pasted token or client to the holder's
//! agent in a signed command, and keeps only what isn't secret
//! (`pyo.team_tailscale`: the client's ID and hint, the tailnet's suffix,
//! what the last sync said; `pyo.clusters.tailscale`: each cluster's names
//! and sync state).
//!
//! - **The holder acts** ([`holder`], [`route`]): every Tailscale command
//!   for a cluster of the team goes to the holder's agent, with
//!   `pyo.commands.for_cluster` naming the cluster it's for. The holder is
//!   always a cluster of the same team. Every cluster of a team with a
//!   holder is on its tailnet ([`ensure_members`]).
//! - **Setup** ([`plan`], [`set_up`], [`save_client`]): on the holder, kept
//!   or picked by Pyo (the cluster Pyo runs on, else the oldest connected).
//! - **Joining** ([`join_key`]): the holder makes a single-use, short-lived
//!   key. Pyo puts it into the join command and wipes it.
//! - **Names on the tailnet** are Tailscale Services ("svc:blog" is
//!   "blog.<tailnet>.ts.net"): Pyo's own (`pyo`, `registry`, `sandboxes`)
//!   on the cluster it runs on, and services' tailnet domains on theirs.
//!   [`tick`] sends the holder each cluster's sync
//!   ([`pyo_proto::Command::TailscaleSync`]) when that changes, and
//!   [`synced`] keeps what came back for the cluster it was for. The
//!   servers do the rest (the agent's tailnet.rs).
//! - **A removed cluster**: its machines and names are kept aside
//!   (`pyo.tailnet_leftovers`, by a trigger) and the holder takes them off
//!   the tailnet ([`leftovers`]).
//! - **Before this**, each cluster kept its own client. [`migrate_setups`]
//!   picks a holder per team among them, and the others on the same tailnet
//!   forget theirs once the holder synced them ([`forget_moved`]). A
//!   cluster with its own client on another tailnet keeps it, and its own
//!   agent acts for it, as before.
//! - **Before that**, the installation kept one client in `pyo.settings`.
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
    /// The holder whose sync for it last went through. A cluster with its
    /// own client on the holder's tailnet forgets it after that.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub held_by: Option<String>,
}

impl Tailnet {
    /// It keeps an OAuth client of its own.
    pub fn own_client(&self) -> bool {
        !self.client_id.is_empty()
    }
}

/// A team's Tailscale, as Pyo keeps it (`pyo.team_tailscale`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TeamTailnet {
    pub team_id: String,
    /// The cluster whose agent keeps the client: always one of the team's.
    /// None once it's gone.
    pub holder: Option<String>,
    pub holder_name: String,
    pub holder_connected: bool,
    pub client_id: String,
    pub hint: String,
    pub suffix: Option<String>,
    /// "holder_gone" or "client_refused".
    pub problem: Option<String>,
}

/// A team row as it's read: team, holder (its id, name, connected),
/// client ID, hint, suffix, problem.
type TeamRow = (
    String,
    Option<String>,
    Option<String>,
    bool,
    String,
    String,
    Option<String>,
    Option<String>,
);

/// The holder only when it's a cluster of the same team: the one place
/// that holds Tailscale work to its team.
macro_rules! team_rows {
    () => {
        "SELECT tt.team_id, c.id, c.name, coalesce(c.status IN ('connected', 'degraded'), false),
                tt.client_id, tt.hint, tt.suffix, tt.problem
           FROM pyo.team_tailscale tt
           LEFT JOIN pyo.clusters c ON c.id = tt.holder_cluster_id AND c.team_id = tt.team_id"
    };
}

fn team_from(r: TeamRow) -> TeamTailnet {
    let (team_id, holder, name, connected, client_id, hint, suffix, problem) = r;
    TeamTailnet {
        team_id,
        holder,
        holder_name: name.unwrap_or_default(),
        holder_connected: connected,
        client_id,
        hint,
        suffix,
        problem,
    }
}

/// A team's Tailscale, when it has one.
pub async fn team_tailnet<'e>(
    db: impl sqlx::PgExecutor<'e>,
    team_id: &str,
) -> ApiResult<Option<TeamTailnet>> {
    let r: Option<TeamRow> = sqlx::query_as(concat!(team_rows!(), " WHERE tt.team_id = $1"))
        .bind(team_id)
        .fetch_optional(db)
        .await?;
    Ok(r.map(team_from))
}

/// Every team's Tailscale, by team.
async fn team_tailnets(db: &PgPool) -> ApiResult<std::collections::BTreeMap<String, TeamTailnet>> {
    let rows: Vec<TeamRow> = sqlx::query_as(team_rows!()).fetch_all(db).await?;
    Ok(rows
        .into_iter()
        .map(team_from)
        .map(|t| (t.team_id.clone(), t))
        .collect())
}

/// The cluster whose agent does the Tailscale work of a team's clusters:
/// always one of the team's own.
pub async fn holder<'e>(db: impl sqlx::PgExecutor<'e>, team_id: &str) -> ApiResult<Option<String>> {
    Ok(team_tailnet(db, team_id).await?.and_then(|t| t.holder))
}

/// Whose agent does a cluster's Tailscale work.
#[derive(Debug, Clone, PartialEq)]
pub enum Acting {
    /// Its own, with its own client: on another tailnet than its team's,
    /// or its team has no holder yet.
    Own,
    /// The team's holder (the cluster itself, when it's the holder).
    Holder(String),
    /// The team's holder is gone: Tailscale is set up again first.
    Gone,
    /// No Tailscale for it.
    None,
}

/// Who acts for `cluster_id`, from its own Tailscale and its team's.
pub fn acting_for(cluster_id: &str, own: Option<&Tailnet>, team: Option<&TeamTailnet>) -> Acting {
    let own_client = own.is_some_and(Tailnet::own_client);
    let Some(team) = team else {
        return if own_client {
            Acting::Own
        } else {
            Acting::None
        };
    };
    if team.holder.as_deref() == Some(cluster_id) {
        return Acting::Holder(cluster_id.to_owned());
    }
    // Its own client on another tailnet (or one Pyo hasn't seen yet): its
    // own agent, as before.
    if own_client {
        let same = matches!((own.and_then(|t| t.suffix.as_deref()), team.suffix.as_deref()),
            (Some(a), Some(b)) if a == b);
        if !same {
            return Acting::Own;
        }
    }
    match &team.holder {
        Some(h) => Acting::Holder(h.clone()),
        None if own_client => Acting::Own,
        None => Acting::Gone,
    }
}

/// A cluster's Tailscale and who acts for it.
#[derive(Debug, Clone)]
pub struct Routed {
    pub team_id: String,
    pub acting: Acting,
    pub own: Option<Tailnet>,
    pub team: Option<TeamTailnet>,
}

/// Who acts for a cluster, and why.
pub async fn route(db: &PgPool, cluster_id: &str) -> ApiResult<Routed> {
    let row: Option<(String, Option<Value>)> =
        sqlx::query_as("SELECT team_id, tailscale FROM pyo.clusters WHERE id = $1")
            .bind(cluster_id)
            .fetch_optional(db)
            .await?;
    let (team_id, v) = row.ok_or_else(|| ApiError::not_found("Cluster"))?;
    let own: Option<Tailnet> = v.and_then(|v| serde_json::from_value(v).ok());
    let team = team_tailnet(db, &team_id).await?;
    Ok(Routed {
        acting: acting_for(cluster_id, own.as_ref(), team.as_ref()),
        team_id,
        own,
        team,
    })
}

/// Why a holder that's gone can't act.
pub const HOLDER_GONE: &str = "Set up Tailscale again. The cluster that kept it is gone.";

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

/// Queues a command for `acting`'s agent, for `for_cluster` when it acts
/// for another cluster: only ever one of its own team.
async fn enqueue_for(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    acting: &str,
    for_cluster: Option<&str>,
    command: Command,
) -> ApiResult<String> {
    let id = crate::deploy::enqueue(tx, acting, command).await?;
    if let Some(f) = for_cluster {
        let set = sqlx::query(
            "UPDATE pyo.commands m SET for_cluster = $2 WHERE m.id = $1
                AND EXISTS (SELECT 1 FROM pyo.clusters a JOIN pyo.clusters b ON b.team_id = a.team_id
                             WHERE a.id = m.cluster_id AND b.id = $2)",
        )
        .bind(&id)
        .bind(f)
        .execute(&mut **tx)
        .await?
        .rows_affected();
        if set == 0 {
            return Err(anyhow::anyhow!("{acting} isn't of the same team as {f}").into());
        }
    }
    Ok(id)
}

/// Sends a command to a cluster's agent and waits for its answer, up to
/// `wait`. A command the agent couldn't carry out is a 400 with its words.
pub(crate) async fn ask(
    s: &AppState,
    cluster_id: &str,
    command: Command,
    wait: Duration,
) -> ApiResult<(String, CommandResult)> {
    ask_as(s, cluster_id, None, command, wait).await
}

/// [`ask`] whoever acts for `cluster_id` ([`route`]): the team's holder, or
/// the cluster itself with a client of its own.
pub(crate) async fn ask_for(
    s: &AppState,
    cluster_id: &str,
    command: Command,
    wait: Duration,
) -> ApiResult<(String, CommandResult)> {
    let r = route(&s.db, cluster_id).await?;
    match r.acting {
        Acting::Own => ask_as(s, cluster_id, None, command, wait).await,
        Acting::Holder(h) => {
            if h != cluster_id
                && let Some(t) = r.team.as_ref().filter(|t| !t.holder_connected)
            {
                return Err(ApiError::conflict(format!(
                    "{} keeps your Tailscale and isn't connected.",
                    t.holder_name
                )));
            }
            ask_as(s, &h, Some(cluster_id), command, wait).await
        }
        Acting::Gone => Err(ApiError::bad_request(HOLDER_GONE)),
        Acting::None => Err(ApiError::bad_request(
            "Tailscale isn't set up on this cluster.",
        )),
    }
}

/// [`ask`] `cluster_id`'s agent, for `for_cluster` when it acts for another
/// cluster of its team.
async fn ask_as(
    s: &AppState,
    cluster_id: &str,
    for_cluster: Option<&str>,
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
    let id = enqueue_for(&mut tx, cluster_id, for_cluster, command).await?;
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

/// A cluster as a holder is picked among its team's, oldest first.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Candidate {
    pub id: String,
    /// Pyo runs on it.
    pub same_cluster: bool,
    pub connected: bool,
    /// The tailnet of its own client, when it keeps one and Pyo saw it.
    pub suffix: Option<String>,
}

/// The team's clusters, oldest first (only those with a client of their
/// own: `own_only`).
async fn candidates(db: &PgPool, team_id: &str, own_only: bool) -> ApiResult<Vec<Candidate>> {
    let rows: Vec<(String, bool, bool, Option<String>)> = sqlx::query_as(
        "SELECT id, same_cluster, status IN ('connected', 'degraded'),
                CASE WHEN coalesce(tailscale->>'clientId', '') <> '' THEN tailscale->>'suffix' END
           FROM pyo.clusters
          WHERE team_id = $1 AND (NOT $2 OR coalesce(tailscale->>'clientId', '') <> '')
          ORDER BY created_at, id",
    )
    .bind(team_id)
    .bind(own_only)
    .fetch_all(db)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(id, same_cluster, connected, suffix)| Candidate {
            id,
            same_cluster,
            connected,
            suffix,
        })
        .collect())
}

/// The holder of a team's new setup: the cluster Pyo runs on, else the
/// oldest connected one.
pub(crate) fn holder_for_setup(clusters: &[Candidate]) -> Option<String> {
    let mut connected = clusters.iter().filter(|c| c.connected);
    connected
        .clone()
        .find(|c| c.same_cluster)
        .or_else(|| connected.next())
        .map(|c| c.id.clone())
}

/// Where a setup runs, and whether it's the team's (`true`): on the team's
/// holder while it's connected, else on the one Pyo picks now. A cluster
/// with a client of its own on another tailnet than the team's sets its
/// own up, as before (`false`).
async fn cluster_target(db: &PgPool, cluster_id: &str) -> ApiResult<(String, bool)> {
    let r = route(db, cluster_id).await?;
    let team_holder = r.team.as_ref().and_then(|t| t.holder.clone());
    if r.acting == Acting::Own && team_holder.is_some() {
        return Ok((cluster_id.to_owned(), false));
    }
    if let Some(h) = team_holder
        && r.team.as_ref().is_some_and(|t| t.holder_connected)
    {
        return Ok((h, true));
    }
    let picked = holder_for_setup(&candidates(db, &r.team_id, false).await?);
    Ok((picked.unwrap_or_else(|| cluster_id.to_owned()), true))
}

/// What a Tailscale op is for: a team (Integrations › Tailscale), or one
/// cluster, as the ops were before teams kept one client.
#[derive(Debug, Clone, PartialEq)]
pub enum Scope {
    Team(String),
    Cluster(String),
}

/// Where a team's setup runs: its holder while it's connected, else the
/// one Pyo picks now ([`holder_for_setup`]).
async fn team_target(db: &PgPool, team_id: &str) -> ApiResult<String> {
    if let Some(t) = team_tailnet(db, team_id).await?
        && t.holder_connected
        && let Some(h) = t.holder
    {
        return Ok(h);
    }
    let clusters = candidates(db, team_id, false).await?;
    holder_for_setup(&clusters).ok_or_else(|| {
        ApiError::conflict(if clusters.is_empty() {
            "Connect a cluster first. Tailscale is set up on it."
        } else {
            "None of the team's clusters is connected. Tailscale is set up on one once it is."
        })
    })
}

/// Where a setup runs, and whether it's the team's (`true`).
async fn setup_target(db: &PgPool, scope: &Scope) -> ApiResult<(String, bool)> {
    match scope {
        Scope::Team(team) => Ok((team_target(db, team).await?, true)),
        Scope::Cluster(c) => cluster_target(db, c).await,
    }
}

/// planTailscaleSetup: what setting the tailnet up would change. It runs on
/// the team's holder (`clusterId` in the answer).
pub async fn plan(s: &AppState, scope: &Scope, token: String) -> ApiResult<Value> {
    token_shape(&token)?;
    let (target, _) = setup_target(&s.db, scope).await?;
    let (_, r) = ask(
        s,
        &target,
        Command::TailscaleSetup {
            token,
            version: None,
        },
        SETUP_WAIT,
    )
    .await?;
    Ok(json!({ "ok": true, "plan": r.data.unwrap_or(Value::Null), "clusterId": target }))
}

/// setUpTailscale: sets the tailnet up as reviewed, on the team's holder.
/// Pyo keeps the client's ID and hint the agent says it kept.
pub async fn set_up(s: &AppState, scope: &Scope, token: String, version: &str) -> ApiResult<Value> {
    token_shape(&token)?;
    let (target, team_wide) = setup_target(&s.db, scope).await?;
    let (_, r) = ask(
        s,
        &target,
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
        keep_client(s, &target, id, hint, None).await?;
        if team_wide {
            keep_team(&s.db, &target, id, hint, None).await?;
        }
    }
    if let Some(o) = out.as_object_mut() {
        o.insert("clusterId".into(), json!(target));
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

/// The holder's client is the team's now. Its tailnet is the one the
/// client says, else unknown until the holder's first sync, so no cluster
/// with a client of its own counts as on it by mistake.
async fn keep_team(
    db: &PgPool,
    holder: &str,
    client_id: &str,
    hint: &str,
    suffix: Option<&str>,
) -> ApiResult<()> {
    // The client it replaces goes on the list to delete in the Tailscale
    // console (the new client can't delete clients).
    sqlx::query(
        "UPDATE pyo.team_tailscale tt
            SET old_clients = tt.old_clients || jsonb_build_array(jsonb_build_object(
                  'clientId', tt.client_id, 'hint', tt.hint,
                  'clusterId', coalesce(tt.holder_cluster_id, ''),
                  'cluster', coalesce((SELECT name FROM pyo.clusters WHERE id = tt.holder_cluster_id), ''))),
                updated_at = now()
          WHERE tt.team_id = (SELECT team_id FROM pyo.clusters WHERE id = $1)
            AND tt.client_id <> $2 AND tt.client_id <> ''
            AND NOT tt.old_clients @> jsonb_build_array(jsonb_build_object('clientId', tt.client_id))",
    )
    .bind(holder)
    .bind(client_id)
    .execute(db)
    .await?;
    // And one it lists comes off once it's the team's client again.
    sqlx::query(
        "UPDATE pyo.team_tailscale tt
            SET old_clients = coalesce((SELECT jsonb_agg(o) FROM jsonb_array_elements(tt.old_clients) o WHERE o->>'clientId' <> $2), '[]')
          WHERE tt.team_id = (SELECT team_id FROM pyo.clusters WHERE id = $1)",
    )
    .bind(holder)
    .bind(client_id)
    .execute(db)
    .await?;
    sqlx::query(
        "INSERT INTO pyo.team_tailscale (team_id, holder_cluster_id, client_id, hint, suffix)
         SELECT team_id, id, $2, $3, $4 FROM pyo.clusters WHERE id = $1
         ON CONFLICT (team_id) DO UPDATE SET holder_cluster_id = excluded.holder_cluster_id,
           client_id = excluded.client_id, hint = excluded.hint, suffix = excluded.suffix,
           problem = NULL, updated_at = now()",
    )
    .bind(holder)
    .bind(client_id)
    .bind(hint)
    .bind(suffix)
    .execute(db)
    .await?;
    ensure_members(db).await
}

/// Every cluster of a team with a holder is on its tailnet: one that had
/// no Tailscale gets its names synced by the holder from now on.
pub async fn ensure_members(db: &PgPool) -> ApiResult<()> {
    sqlx::query(
        "UPDATE pyo.clusters c SET tailscale = jsonb_build_object('clientId', '', 'suffix', tt.suffix)
           FROM pyo.team_tailscale tt
          WHERE tt.team_id = c.team_id AND tt.holder_cluster_id IS NOT NULL AND c.tailscale IS NULL",
    )
    .execute(db)
    .await?;
    Ok(())
}

/// saveTailscale: the team's holder uses this OAuth client, once it signs
/// in. The secret goes to the agent sealed and is never kept by Pyo.
pub async fn save_client(
    s: &AppState,
    scope: &Scope,
    client_id: &str,
    secret: Option<&str>,
) -> ApiResult<Value> {
    let Some(secret) = secret.map(str::trim).filter(|x| !x.is_empty()) else {
        return Err(ApiError::bad_request("Paste the client secret too."));
    };
    let (target, team_wide) = setup_target(&s.db, scope).await?;
    let (_, r) = ask(
        s,
        &target,
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
        &target,
        &state.client_id,
        &state.hint,
        state.suffix.clone(),
    )
    .await?;
    if team_wide {
        keep_team(
            &s.db,
            &target,
            &state.client_id,
            &state.hint,
            state.suffix.as_deref(),
        )
        .await?;
    }
    Ok(json!({ "ok": state.ok, "checks": state.checks, "clusterId": target }))
}

/// The Test button: the team's client on its holder (or, for one cluster,
/// the client that acts for it), check by check.
pub async fn test_saved(s: &AppState, scope: &Scope) -> ApiResult<Value> {
    let cluster_id = match scope {
        Scope::Cluster(c) => c.clone(),
        Scope::Team(team) => match team_tailnet(&s.db, team).await? {
            Some(TeamTailnet {
                holder: Some(h), ..
            }) => h,
            Some(_) => return Err(ApiError::bad_request(HOLDER_GONE)),
            None => return Err(ApiError::bad_request("Tailscale isn't set up.")),
        },
    };
    let cluster_id = cluster_id.as_str();
    let r = route(&s.db, cluster_id).await?;
    if r.acting == Acting::None {
        return Err(ApiError::bad_request(
            "Tailscale isn't set up on this cluster.",
        ));
    }
    let (_, res) = ask_for(s, cluster_id, Command::TailscaleTest {}, SETUP_WAIT).await?;
    let state: ts::ClientState = res
        .data
        .and_then(|d| serde_json::from_value(d).ok())
        .unwrap_or_default();
    if let Some(x) = &state.suffix {
        match &r.acting {
            Acting::Holder(h) => {
                patch(&s.db, h, json!({ "suffix": x })).await?;
                sqlx::query("UPDATE pyo.team_tailscale SET suffix = $2, updated_at = now() WHERE holder_cluster_id = $1")
                    .bind(h)
                    .bind(x)
                    .execute(&s.db)
                    .await?;
            }
            _ => patch(&s.db, cluster_id, json!({ "suffix": x })).await?,
        }
    }
    Ok(json!({ "ok": state.ok, "checks": state.checks }))
}

/// Removes Tailscale. A cluster on its team's tailnet: the team's, for
/// every cluster of it. A cluster with a client of its own on another
/// tailnet: its own. Pyo's names there are deleted while it still can, and
/// the servers take them off. Pyo forgets it even when no agent can be
/// reached.
pub async fn remove(s: &AppState, scope: &Scope) -> ApiResult<()> {
    let cluster_id = match scope {
        Scope::Team(team) => return remove_team(s, team).await,
        Scope::Cluster(c) => c.as_str(),
    };
    let r = route(&s.db, cluster_id).await?;
    match r.acting {
        Acting::Holder(_) | Acting::Gone => remove_team(s, &r.team_id).await,
        Acting::Own | Acting::None => match r.own {
            Some(t) => remove_own(s, cluster_id, &t).await,
            None => Ok(()),
        },
    }
}

/// A cluster's own client, its names and Pyo's addresses on it.
async fn remove_own(s: &AppState, cluster_id: &str, t: &Tailnet) -> ApiResult<()> {
    let names: Vec<String> =
        sqlx::query_scalar("SELECT name FROM pyo.cluster_tailnet_names WHERE cluster_id = $1")
            .bind(cluster_id)
            .fetch_all(&s.db)
            .await?;
    let marks = marks(&s.db, cluster_id, t).await?;
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
    forget_cluster(&s.db, cluster_id).await
}

/// The team's Tailscale, on every cluster of it on its tailnet: the
/// holder deletes Pyo's names and forgets the client, and any cluster that
/// still keeps a client of its own there forgets it too.
async fn remove_team(s: &AppState, team_id: &str) -> ApiResult<()> {
    let team = team_tailnet(&s.db, team_id).await?;
    let rows: Vec<(String, Value)> = sqlx::query_as(
        "SELECT id, tailscale FROM pyo.clusters WHERE team_id = $1 AND tailscale IS NOT NULL ORDER BY created_at",
    )
    .bind(team_id)
    .fetch_all(&s.db)
    .await?;
    let members: Vec<(String, Tailnet)> = rows
        .into_iter()
        .filter_map(|(id, v)| Some((id, serde_json::from_value::<Tailnet>(v).ok()?)))
        .filter(|(id, t)| {
            matches!(
                acting_for(id, Some(t), team.as_ref()),
                Acting::Holder(_) | Acting::Gone
            )
        })
        .collect();
    let holder = team.as_ref().and_then(|t| t.holder.clone());
    let (mut names, mut all_marks) = (Vec::new(), Vec::new());
    for (id, t) in &members {
        let n: Vec<String> =
            sqlx::query_scalar("SELECT name FROM pyo.cluster_tailnet_names WHERE cluster_id = $1")
                .bind(id)
                .fetch_all(&s.db)
                .await?;
        names.extend(n);
        all_marks.extend(marks(&s.db, id, t).await?);
    }
    names.sort();
    names.dedup();
    all_marks.sort();
    all_marks.dedup();
    if let Some(h) = &holder
        && let Err(e) = ask(
            s,
            h,
            Command::TailscaleRemove {
                names,
                marks: all_marks,
            },
            SETUP_WAIT,
        )
        .await
    {
        tracing::warn!(cluster = %h, error = %e.message, "the holder couldn't remove Tailscale; Pyo forgets it anyway");
    }
    for (id, t) in &members {
        if Some(id) != holder.as_ref()
            && t.own_client()
            && let Err(e) = ask(
                s,
                id,
                Command::TailscaleRemove {
                    names: Vec::new(),
                    marks: Vec::new(),
                },
                Duration::from_secs(30),
            )
            .await
        {
            tracing::warn!(cluster = %id, error = %e.message, "the cluster couldn't forget its Tailscale client");
        }
    }
    sqlx::query("DELETE FROM pyo.team_tailscale WHERE team_id = $1")
        .bind(team_id)
        .execute(&s.db)
        .await?;
    for (id, _) in &members {
        forget_cluster(&s.db, id).await?;
    }
    Ok(())
}

/// Pyo forgets a cluster's Tailscale: its names are retired (its servers
/// take them off), and on the cluster Pyo runs on, Pyo's addresses there.
async fn forget_cluster(db: &PgPool, cluster_id: &str) -> ApiResult<()> {
    sqlx::query("UPDATE pyo.cluster_tailnet_names SET retired_at = coalesce(retired_at, now()), updated_at = now() WHERE cluster_id = $1")
        .bind(cluster_id)
        .execute(db)
        .await?;
    sqlx::query("UPDATE pyo.clusters SET tailscale = NULL WHERE id = $1")
        .bind(cluster_id)
        .execute(db)
        .await?;
    if host_cluster(db).await?.as_deref() == Some(cluster_id) {
        // Without the tailnet, the public dashboard is the way in again.
        sqlx::query("DELETE FROM pyo.settings WHERE key = 'public_dashboard'")
            .execute(db)
            .await?;
        for k in ["tailnet", "registry", "sandboxes"] {
            save_address(db, k, None).await?;
        }
    }
    Ok(())
}

/// Why a cluster can't move by itself: its servers move by hand.
pub const MOVE_BY_HAND: &str = "Move its servers by hand. Steps in the docs.";

/// moveClusterToTailnet: a single-server cluster on another tailnet than
/// its team's moves to the team's. Pyo's names on the old tailnet are
/// deleted and its own client forgotten (its ID goes on the team's list to
/// delete), its services' tailnet domains move to the team's tailnet, and
/// the holder acts for it from now on. The server leaves the old tailnet
/// and joins the team's with the command that comes back (`commands`, one
/// per server, with a fresh key from the holder). A cluster that moved
/// already but whose server isn't on the team's tailnet yet gets a fresh
/// command.
pub async fn move_cluster(s: &AppState, cluster_id: &str) -> ApiResult<Value> {
    let r = route(&s.db, cluster_id).await?;
    let Some(team) = r.team.clone() else {
        return Err(ApiError::bad_request(
            "Set up Tailscale on Integrations first.",
        ));
    };
    let Some(holder) = team.holder.clone() else {
        return Err(ApiError::bad_request(HOLDER_GONE));
    };
    let Some(suffix) = team.suffix.clone() else {
        return Err(ApiError::conflict(
            "Pyo hasn't seen your tailnet yet. Try again in a minute.",
        ));
    };
    let state = cluster_state(cluster_id, r.own.as_ref(), Some(&team));
    let reported = servers(&s.db, cluster_id).await?;
    let on_team = !reported.is_empty() && reported.iter().all(|x| x.suffix == suffix);
    match state {
        Some(ClusterTailnet::Elsewhere) => {}
        Some(ClusterTailnet::Member) if !on_team => {}
        Some(ClusterTailnet::Holder | ClusterTailnet::Member | ClusterTailnet::Moving) => {
            return Err(ApiError::conflict(
                "This cluster is on your tailnet already.",
            ));
        }
        _ => return Err(ApiError::bad_request(HOLDER_GONE)),
    }
    let names: Vec<String> =
        sqlx::query_scalar("SELECT name FROM pyo.servers WHERE cluster_id = $1 ORDER BY name")
            .bind(cluster_id)
            .fetch_all(&s.db)
            .await?;
    let [server] = names.as_slice() else {
        return Err(ApiError::conflict(MOVE_BY_HAND));
    };
    if !team.holder_connected {
        return Err(ApiError::conflict(format!(
            "{} keeps your Tailscale and isn't connected.",
            team.holder_name
        )));
    }
    // The key first: nothing changes when the holder can't make one.
    let (id, k) = ask_as(
        s,
        &holder,
        Some(cluster_id),
        Command::TailscaleKey {
            description: format!("Pyo: move {server}"),
        },
        Duration::from_secs(30),
    )
    .await
    .map_err(|e| {
        tracing::warn!(error = %e.message, "the holder made no key to move a cluster");
        ApiError::bad_request(
            "Couldn't get a Tailscale key to move it. Check Tailscale on Integrations.",
        )
    })?;
    forget_command(&s.db, &id).await;
    let key = k
        .data
        .as_ref()
        .and_then(|d| d[ts::KEY_FIELD].as_str())
        .map(str::to_owned)
        .ok_or_else(|| ApiError::bad_request("Tailscale made no auth key."))?;

    if let Some(own) = r.own.as_ref().filter(|t| t.own_client()) {
        // Off the old tailnet: Pyo's names there, and the client.
        let old_names: Vec<String> =
            sqlx::query_scalar("SELECT name FROM pyo.cluster_tailnet_names WHERE cluster_id = $1")
                .bind(cluster_id)
                .fetch_all(&s.db)
                .await?;
        let marks = marks(&s.db, cluster_id, own).await?;
        if let Err(e) = ask(
            s,
            cluster_id,
            Command::TailscaleRemove {
                names: old_names,
                marks,
            },
            Duration::from_secs(30),
        )
        .await
        {
            tracing::warn!(cluster = %cluster_id, error = %e.message, "the cluster couldn't remove its own Tailscale; it moves anyway");
        }
        let cluster_name: String =
            sqlx::query_scalar("SELECT name FROM pyo.clusters WHERE id = $1")
                .bind(cluster_id)
                .fetch_one(&s.db)
                .await?;
        sqlx::query(
            "UPDATE pyo.team_tailscale
                SET old_clients = old_clients || jsonb_build_array(jsonb_build_object('clientId', $2::text, 'hint', $3::text, 'clusterId', $4::text, 'cluster', $5::text, 'suffix', $6::text)),
                    updated_at = now()
              WHERE team_id = $1 AND $2 <> client_id AND NOT old_clients @> jsonb_build_array(jsonb_build_object('clientId', $2::text))",
        )
        .bind(&team.team_id)
        .bind(&own.client_id)
        .bind(&own.hint)
        .bind(cluster_id)
        .bind(&cluster_name)
        .bind(own.suffix.as_deref().unwrap_or(""))
        .execute(&s.db)
        .await?;
        let old = own.suffix.clone().unwrap_or_default();
        let mut tx = s.db.begin().await?;
        sqlx::query("UPDATE pyo.cluster_tailnet_names SET retired_at = coalesce(retired_at, now()), updated_at = now() WHERE cluster_id = $1")
            .bind(cluster_id)
            .execute(&mut *tx)
            .await?;
        sqlx::query("UPDATE pyo.clusters SET tailscale = jsonb_build_object('clientId', '', 'suffix', $2::text) WHERE id = $1")
            .bind(cluster_id)
            .bind(&suffix)
            .execute(&mut *tx)
            .await?;
        if !old.is_empty() {
            for service in move_domains(&mut tx, cluster_id, &old, &suffix).await? {
                crate::routes::changed(&mut tx, &service, true).await?;
            }
        }
        tx.commit().await?;
        s.wake(cluster_id).await;
    }
    let base = crate::join::scripts_url_before_tailnet();
    Ok(json!({
        "ok": true,
        "suffix": suffix,
        "commands": [{ "server": server, "command": crate::join::tailnet_move_command(&base, &key, &suffix) }],
        "expiresAt": pyo_proto::now_ms() as i64 + 3_600_000,
    }))
}

/// A cluster's services' tailnet domains on `old` move to `new`
/// ("blog.tail1.ts.net" is "blog.tail2.ts.net" now), each set up again.
/// The services whose domains moved.
async fn move_domains(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    cluster_id: &str,
    old: &str,
    new: &str,
) -> ApiResult<Vec<String>> {
    Ok(sqlx::query_scalar(
        "UPDATE pyo.services s SET spec = jsonb_set(s.spec, '{domains}', (
           SELECT coalesce(jsonb_agg(CASE WHEN lower(d->>'hostname') LIKE '%.' || $2
                    THEN (d - 'error' - 'tlsError') || jsonb_build_object(
                           'hostname', left(lower(d->>'hostname'), length(d->>'hostname') - length($2)) || $3,
                           'status', 'pending_dns', 'tls', 'pending')
                    ELSE d END), '[]')
             FROM jsonb_array_elements(s.spec->'domains') d))
          WHERE s.cluster_id = $1 AND NOT s.deleting
            AND EXISTS (SELECT 1 FROM jsonb_array_elements(s.spec->'domains') d WHERE lower(d->>'hostname') LIKE '%.' || $2)
          RETURNING s.id",
    )
    .bind(cluster_id)
    .bind(old)
    .bind(new)
    .fetch_all(&mut **tx)
    .await?)
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
        || named_elsewhere(&s.db, &cluster_id, t.suffix.as_deref(), &name).await?
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

/// What a name is on the cluster's tailnet: "free", "ours" or "taken". The
/// holder checks it, with the cluster's marks.
async fn check_name(s: &AppState, cluster_id: &str, t: &Tailnet, name: &str) -> ApiResult<String> {
    let marks = marks(&s.db, cluster_id, t).await?;
    let unchecked =
        || ApiError::bad_request("Couldn't check the name on your tailnet. Try again in a minute.");
    let (_, r) = ask_for(
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
        if e.status == axum::http::StatusCode::CONFLICT {
            e
        } else {
            unchecked()
        }
    })?;
    let c: ts::NameCheck = r
        .data
        .and_then(|d| serde_json::from_value(d).ok())
        .ok_or_else(unchecked)?;
    Ok(c.state)
}

/// Whether another cluster on the same tailnet has a name, or wants it
/// for a service's tailnet domain it hasn't made yet.
async fn named_elsewhere(
    db: &PgPool,
    cluster_id: &str,
    suffix: Option<&str>,
    label: &str,
) -> ApiResult<bool> {
    let Some(suffix) = suffix else {
        return Ok(false);
    };
    Ok(sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM pyo.cluster_tailnet_names n JOIN pyo.clusters c ON c.id = n.cluster_id
                         WHERE n.cluster_id <> $1 AND c.tailscale->>'suffix' = $2 AND n.name = $3 AND n.retired_at IS NULL)
             OR EXISTS (SELECT 1 FROM pyo.services s JOIN pyo.clusters c ON c.id = s.cluster_id
                          CROSS JOIN LATERAL jsonb_array_elements(s.spec->'domains') d
                         WHERE s.cluster_id <> $1 AND c.tailscale->>'suffix' = $2 AND NOT s.deleting
                           AND lower(d->>'hostname') = $3 || '.' || $2)",
    )
    .bind(cluster_id)
    .bind(suffix)
    .bind(label)
    .fetch_one(db)
    .await?)
}

/// Whether a tailnet domain someone typed for a service on a cluster can be
/// had: Pyo's reserved names and its own names are refused, and so is a
/// name something else on the cluster's tailnet has, another cluster of it
/// included.
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
        || named_elsewhere(&s.db, cluster_id, Some(suffix), label).await?
        || check_name(s, cluster_id, &t, label).await? == "taken"
    {
        return Err(ApiError::conflict("Taken on your tailnet"));
    }
    Ok(())
}

// ── Join keys ───────────────────────────────────────────────────────────────

/// The cluster whose tailnet a machine joins: `cluster_id`, once it's on
/// one (Tailscale of its own, or its team's). Else the cluster Pyo runs on
/// (self-hosted: the installation's tailnet). Its key comes from whoever
/// acts for it ([`join_key`]).
pub async fn key_cluster(db: &PgPool, cluster_id: Option<&str>) -> ApiResult<String> {
    if let Some(c) = cluster_id {
        let on: bool = sqlx::query_scalar(
            "SELECT tailscale IS NOT NULL OR EXISTS (SELECT 1 FROM pyo.team_tailscale tt WHERE tt.team_id = c.team_id)
               FROM pyo.clusters c WHERE id = $1",
        )
        .bind(c)
        .fetch_optional(db)
        .await?
        .unwrap_or(false);
        if on {
            return Ok(c.to_owned());
        }
    }
    match host(db).await? {
        Some((h, _)) => Ok(h),
        None => Err(ApiError::bad_request(NEEDED)),
    }
}

/// The cluster whose tailnet a team's new sandbox server joins, and
/// through which Pyo reaches it: the team's holder, else one of its
/// clusters with Tailscale (a connected one first), else the cluster Pyo
/// runs on. The installation's own servers join Pyo's.
pub async fn team_key_cluster(db: &PgPool, team_id: Option<&str>) -> ApiResult<String> {
    if let Some(t) = team_id {
        if let Some(tt) = team_tailnet(db, t).await?
            && let Some(h) = tt.holder.filter(|_| tt.holder_connected)
        {
            return Ok(h);
        }
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

/// A join command's auth key for a machine joining `cluster_id`'s tailnet,
/// from whoever acts for it (the team's holder): single-use,
/// preauthorized, tag:pyo, an hour. Pyo keeps it only to put it in the
/// command. `server` names what's being added, e.g. "a sandbox server".
pub async fn join_key(
    s: &AppState,
    cluster_id: &str,
    what: &str,
    server: &'static str,
) -> ApiResult<String> {
    match route(&s.db, cluster_id).await?.acting {
        Acting::None => return Err(ApiError::bad_request(NEEDED)),
        Acting::Gone => return Err(ApiError::bad_request(HOLDER_GONE)),
        Acting::Own | Acting::Holder(_) => {}
    }
    let (id, r) = ask_for(
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
        if e.status == axum::http::StatusCode::CONFLICT {
            return e;
        }
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
            if let Err(e) = migrate_setups(&st).await {
                tracing::warn!(error = %e.message, "tailscale: picking teams' holders");
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
        cluster: Some(cluster_id.to_owned()),
    })
}

/// A sync is sent again at least this often, for what changed on the
/// tailnet itself (a Service someone deleted, a host to approve).
const RESYNC_MS: i64 = 5 * 60 * 1000;

/// The cluster's next [`tick`] sends its sync again, changed or not.
pub(crate) async fn send_again(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    cluster_id: &str,
) -> ApiResult<()> {
    patch(&mut **tx, cluster_id, json!({ "sent": "" })).await
}

/// A cluster row as [`tick`] reads it: id, team, name, same cluster,
/// status, and its Tailscale.
type ClusterRow = (String, String, String, bool, String, Value);

/// Tailscale commands out on one holder at a time, at most (the Cloud
/// signer's rate limit, and big teams).
const HOLDER_IN_FLIGHT: i64 = 5;

/// Commands for `$1`: queued for it, by its own agent or the holder. A
/// removed cluster's leftovers (for nobody) aren't the holder's own.
macro_rules! for_subject {
    () => {
        "(for_cluster = $1 OR (for_cluster IS NULL AND cluster_id = $1 AND coalesce(command->>'cluster', $1) = $1))"
    };
}

/// One pass: each cluster on a tailnet gets its names synced by whoever
/// acts for it (the team's holder) when what Pyo wants changed (or a while
/// passed), then Pyo's addresses and the services' tailnet domains follow
/// what the servers report. Removed clusters' leftovers go, and clusters
/// that moved to the holder forget their own clients.
pub async fn tick(s: &AppState) -> ApiResult<()> {
    let rows = sync_names(s, None).await?;

    // Pyo's own names, once a server keeps their certificate.
    let mut ready = std::collections::BTreeMap::new();
    if let Some((id, _, _, _, _, v)) = rows.iter().find(|r| r.3)
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

    for (id, _, _, _, _, v) in &rows {
        if let Ok(t) = serde_json::from_value::<Tailnet>(v.clone()) {
            follow_domains(&s.db, id, &t).await?;
        }
    }
    Ok(())
}

/// The syncs of [`tick`], for every team's clusters (or one team's), and
/// the removed clusters' leftovers and the clusters that moved to the
/// holder after them. The clusters on a tailnet, as read.
async fn sync_names(s: &AppState, only: Option<&str>) -> ApiResult<Vec<ClusterRow>> {
    let install = install_id(&s.db).await?;
    ensure_members(&s.db).await?;
    sqlx::query(
        "UPDATE pyo.team_tailscale SET problem = 'holder_gone', updated_at = now()
          WHERE holder_cluster_id IS NULL AND problem IS DISTINCT FROM 'holder_gone'",
    )
    .execute(&s.db)
    .await?;
    let mut teams = team_tailnets(&s.db).await?;
    if let Some(only) = only {
        teams.retain(|t, _| t == only);
    }
    let rows: Vec<ClusterRow> = sqlx::query_as(
        "SELECT id, team_id, name, same_cluster, status, tailscale FROM pyo.clusters
          WHERE tailscale IS NOT NULL AND ($1::text IS NULL OR team_id = $1) ORDER BY created_at",
    )
    .bind(only)
    .fetch_all(&s.db)
    .await?;
    let now = pyo_proto::now_ms() as i64;
    for (id, team, name, same, status, v) in &rows {
        let Ok(t) = serde_json::from_value::<Tailnet>(v.clone()) else {
            continue;
        };
        let tt = teams.get(team);
        let (acting, connected) = match acting_for(id, Some(&t), tt) {
            Acting::Own => (
                id.clone(),
                matches!(status.as_str(), "connected" | "degraded"),
            ),
            Acting::Holder(h) => (h, tt.is_some_and(|x| x.holder_connected)),
            Acting::Gone | Acting::None => continue,
        };
        // Whoever acts has to be there. The cluster itself needn't be: its
        // servers pick its names up when they're back.
        if !connected {
            continue;
        }
        let w = sync_for(&s.db, &install, id, name, *same, &t).await?;
        let key = crate::ids::sha256_hex(&serde_json::to_string(&w).map_err(anyhow::Error::from)?);
        if key == t.sent && now - t.sent_at < RESYNC_MS {
            continue;
        }
        // One at a time per cluster: answered before the next is sent.
        let waiting: bool = sqlx::query_scalar(concat!(
            "SELECT EXISTS (SELECT 1 FROM pyo.commands WHERE ",
            for_subject!(),
            " AND command->>'kind' = 'tailscale_sync'
                AND status IN ('pending', 'sent') AND created_at > now() - interval '5 minutes')"
        ))
        .bind(id)
        .fetch_one(&s.db)
        .await?;
        if waiting || busy(&s.db, &acting).await? {
            continue;
        }
        let mut tx = s.db.begin().await?;
        // One never picked up: the new one replaces it.
        sqlx::query(concat!(
            "DELETE FROM pyo.commands WHERE ",
            for_subject!(),
            " AND command->>'kind' = 'tailscale_sync' AND status = 'pending'"
        ))
        .bind(id)
        .execute(&mut *tx)
        .await?;
        enqueue_for(
            &mut tx,
            &acting,
            Some(id),
            Command::TailscaleSync(Box::new(w)),
        )
        .await?;
        patch(&mut *tx, id, json!({ "sent": key, "sentAt": now })).await?;
        tx.commit().await?;
        s.wake(&acting).await;
    }
    leftovers(s, &install, &teams, only).await?;
    forget_moved(s, &teams).await?;
    Ok(rows)
}

/// The holder has as many Tailscale commands out as it may.
async fn busy(db: &PgPool, acting: &str) -> ApiResult<bool> {
    let n: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM pyo.commands WHERE cluster_id = $1 AND command->>'kind' LIKE 'tailscale\\_%'
            AND status IN ('pending', 'sent') AND created_at > now() - interval '5 minutes'",
    )
    .bind(acting)
    .fetch_one(db)
    .await?;
    Ok(n >= HOLDER_IN_FLIGHT)
}

/// A removed cluster's server on its tailnet, as kept in its leftovers.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LeftMachine {
    #[serde(default)]
    pub server: String,
    pub node_id: String,
}

/// Who takes a removed cluster's leftovers off `suffix` (its tailnet, as
/// last seen): the team's holder when it's the team's tailnet (or one Pyo
/// never saw), else a connected cluster of the team that keeps its own
/// client on it. None when no one can now.
async fn leftovers_actor(
    db: &PgPool,
    team_id: &str,
    team: Option<&TeamTailnet>,
    suffix: Option<&str>,
) -> ApiResult<Option<String>> {
    if let Some(t) = team
        && (suffix.is_none() || suffix == t.suffix.as_deref())
    {
        return Ok(t.holder.clone().filter(|_| t.holder_connected));
    }
    let Some(suffix) = suffix else {
        return Ok(None);
    };
    let rows: Vec<(String, Value)> = sqlx::query_as(
        "SELECT id, tailscale FROM pyo.clusters
          WHERE team_id = $1 AND status IN ('connected', 'degraded')
            AND coalesce(tailscale->>'clientId', '') <> '' AND tailscale->>'suffix' = $2
          ORDER BY created_at",
    )
    .bind(team_id)
    .bind(suffix)
    .fetch_all(db)
    .await?;
    Ok(rows.into_iter().find_map(|(id, v)| {
        let t: Tailnet = serde_json::from_value(v).ok()?;
        (acting_for(&id, Some(&t), team) == Acting::Own).then_some(id)
    }))
}

/// Removed clusters' machines and names on their tailnet: a cluster of the
/// team that keeps a client there ([`leftovers_actor`]) takes them off
/// with a sync for the removed cluster (its mark, no names wanted, its
/// machines forgotten). Tried every 5 minutes for a week. `only`: one
/// team's.
pub async fn leftovers(
    s: &AppState,
    install: &str,
    teams: &std::collections::BTreeMap<String, TeamTailnet>,
    only: Option<&str>,
) -> ApiResult<()> {
    sqlx::query("DELETE FROM pyo.tailnet_leftovers WHERE created_at < now() - interval '7 days'")
        .execute(&s.db)
        .await?;
    // A sync no one answers any more (the holder went, or its command
    // expired): tried again.
    sqlx::query(
        "UPDATE pyo.tailnet_leftovers l SET command_id = NULL
          WHERE command_id IS NOT NULL
            AND NOT EXISTS (SELECT 1 FROM pyo.commands m WHERE m.id = l.command_id AND m.status IN ('pending', 'sent')
                              AND m.created_at > now() - interval '10 minutes')",
    )
    .execute(&s.db)
    .await?;
    type Leftover = (
        i64,
        String,
        String,
        Option<String>,
        Vec<String>,
        Vec<String>,
        Value,
    );
    let rows: Vec<Leftover> = sqlx::query_as(
        "SELECT id, team_id, cluster_id, suffix, adopt, names, machines FROM pyo.tailnet_leftovers
          WHERE command_id IS NULL AND (tried_at IS NULL OR tried_at < now() - interval '5 minutes')
            AND ($1::text IS NULL OR team_id = $1)
          ORDER BY id",
    )
    .bind(only)
    .fetch_all(&s.db)
    .await?;
    for (lid, team, cluster, suffix, adopt, names, machines) in rows {
        let Some(h) = leftovers_actor(&s.db, &team, teams.get(&team), suffix.as_deref()).await?
        else {
            continue;
        };
        if busy(&s.db, &h).await? {
            continue;
        }
        // Never one a cluster of the team still uses.
        let names: Vec<String> = sqlx::query_scalar(
            "SELECT x FROM unnest($1::text[]) x
              WHERE NOT EXISTS (SELECT 1 FROM pyo.cluster_tailnet_names n JOIN pyo.clusters c ON c.id = n.cluster_id
                                 WHERE n.name = x AND n.retired_at IS NULL AND c.team_id = $2)
              ORDER BY x",
        )
        .bind(&names)
        .bind(&team)
        .fetch_all(&s.db)
        .await?;
        let machines: Vec<LeftMachine> = serde_json::from_value(machines).unwrap_or_default();
        let ids: Vec<String> = machines.iter().map(|m| m.node_id.clone()).collect();
        let nodes: Vec<String> = sqlx::query_scalar(
            "SELECT x FROM unnest($1::text[]) x
              WHERE NOT EXISTS (SELECT 1 FROM pyo.clusters c, jsonb_array_elements(coalesce(c.capabilities->'tailnet', '[]')) t
                                 WHERE t->>'nodeId' = x)
              ORDER BY x",
        )
        .bind(&ids)
        .fetch_all(&s.db)
        .await?;
        if names.is_empty() && nodes.is_empty() {
            sqlx::query("DELETE FROM pyo.tailnet_leftovers WHERE id = $1")
                .bind(lid)
                .execute(&s.db)
                .await?;
            continue;
        }
        let w = TailnetSync {
            mark: ts::marker(install, &cluster),
            adopt,
            made: names,
            forget: nodes,
            cluster: Some(cluster.clone()),
            ..Default::default()
        };
        let mut tx = s.db.begin().await?;
        let cmd = enqueue_for(&mut tx, &h, None, Command::TailscaleSync(Box::new(w))).await?;
        sqlx::query(
            "UPDATE pyo.tailnet_leftovers SET command_id = $2, tried_at = now() WHERE id = $1",
        )
        .bind(lid)
        .bind(&cmd)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        s.wake(&h).await;
        tracing::info!(cluster = %cluster, by = %h, "tailscale: taking a removed cluster's machines and names off the tailnet");
    }
    Ok(())
}

/// A removed cluster's leftovers, answered: gone once its machines are.
/// False when the command wasn't for leftovers.
async fn leftovers_synced(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    command_id: &str,
    r: &CommandResult,
) -> ApiResult<bool> {
    let row: Option<(i64, Value)> =
        sqlx::query_as("SELECT id, machines FROM pyo.tailnet_leftovers WHERE command_id = $1")
            .bind(command_id)
            .fetch_optional(&mut **tx)
            .await?;
    let Some((lid, machines)) = row else {
        return Ok(false);
    };
    let machines: Vec<LeftMachine> = serde_json::from_value(machines).unwrap_or_default();
    let done = r
        .data
        .clone()
        .and_then(|d| serde_json::from_value::<TailnetSynced>(d).ok())
        .filter(|_| r.ok);
    let left: Vec<LeftMachine> = machines
        .into_iter()
        .filter(|m| {
            !done
                .as_ref()
                .is_some_and(|d| d.forgotten.contains(&m.node_id))
        })
        .collect();
    if done.is_some() && left.is_empty() {
        sqlx::query("DELETE FROM pyo.tailnet_leftovers WHERE id = $1")
            .bind(lid)
            .execute(&mut **tx)
            .await?;
    } else {
        if !r.ok {
            tracing::warn!(error = ?r.message, "the removed cluster's servers couldn't be taken off the tailnet");
        }
        sqlx::query(
            "UPDATE pyo.tailnet_leftovers SET command_id = NULL, machines = $2 WHERE id = $1",
        )
        .bind(lid)
        .bind(serde_json::to_value(&left).map_err(anyhow::Error::from)?)
        .execute(&mut **tx)
        .await?;
    }
    Ok(true)
}

/// Clusters that kept a client of their own on the team's tailnet: once
/// the holder synced them, they forget it (deleting nothing), and its ID
/// goes on the team's list to delete in the Tailscale console.
pub async fn forget_moved(
    s: &AppState,
    teams: &std::collections::BTreeMap<String, TeamTailnet>,
) -> ApiResult<()> {
    let rows: Vec<(String, String, String, Value)> = sqlx::query_as(
        "SELECT id, team_id, name, tailscale FROM pyo.clusters
          WHERE coalesce(tailscale->>'clientId', '') <> '' AND status IN ('connected', 'degraded')
            AND team_id = ANY($1)
          ORDER BY created_at",
    )
    .bind(teams.keys().cloned().collect::<Vec<_>>())
    .fetch_all(&s.db)
    .await?;
    for (id, team, name, v) in rows {
        let Ok(t) = serde_json::from_value::<Tailnet>(v) else {
            continue;
        };
        let Some(tt) = teams.get(&team) else {
            continue;
        };
        let Acting::Holder(h) = acting_for(&id, Some(&t), Some(tt)) else {
            continue;
        };
        if h == id || t.held_by.as_deref() != Some(h.as_str()) {
            continue;
        }
        let r = ask_as(
            s,
            &id,
            None,
            Command::TailscaleRemove {
                names: Vec::new(),
                marks: Vec::new(),
            },
            Duration::from_secs(30),
        )
        .await;
        if let Err(e) = r {
            tracing::warn!(cluster = %id, error = %e.message, "the cluster couldn't forget its Tailscale client yet");
            continue;
        }
        patch(&s.db, &id, json!({ "clientId": "", "hint": "" })).await?;
        // The team's own client (one Pyo handed to several clusters before)
        // stays off the list.
        if t.client_id != tt.client_id {
            sqlx::query(
                "UPDATE pyo.team_tailscale
                    SET old_clients = old_clients || jsonb_build_array(jsonb_build_object('clientId', $2::text, 'hint', $3::text, 'clusterId', $4::text, 'cluster', $5::text)),
                        updated_at = now()
                  WHERE team_id = $1 AND NOT old_clients @> jsonb_build_array(jsonb_build_object('clientId', $2::text))",
            )
            .bind(&team)
            .bind(&t.client_id)
            .bind(&t.hint)
            .bind(&id)
            .bind(&name)
            .execute(&s.db)
            .await?;
        }
        tracing::info!(cluster = %id, holder = %h, "tailscale: the cluster forgot its own client, the team's holder acts for it");
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

/// Whether a sync's failure says Tailscale refuses the client: it was
/// revoked, or the holder lost it.
fn refused(message: &str) -> bool {
    message.contains("answered 401") || message.contains("Tailscale isn't set up on this cluster")
}

/// A sync's answer (agent.rs, when the agent reports it), kept for the
/// cluster it was for (`for_cluster`, else the agent's own): the names as
/// the agent left them, the suffix, Pyo's own names as picked. What the
/// tailnet's machines and Tailnet Lock say goes to the team when its
/// holder synced. Older syncs' commands go.
pub async fn synced(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    cluster_id: &str,
    command_id: &str,
    r: &CommandResult,
) -> ApiResult<()> {
    if leftovers_synced(tx, command_id, r).await? {
        return Ok(());
    }
    // Who ran it (the polling agent's cluster), and for whom.
    let acting = cluster_id;
    let subject: String = sqlx::query_scalar(
        "SELECT coalesce(for_cluster, command->>'cluster', cluster_id) FROM pyo.commands WHERE id = $1",
    )
    .bind(command_id)
    .fetch_optional(&mut **tx)
    .await?
    .unwrap_or_else(|| acting.to_owned());
    let cluster_id = subject.as_str();
    sqlx::query(concat!(
        "DELETE FROM pyo.commands WHERE ",
        for_subject!(),
        " AND command->>'kind' = 'tailscale_sync' AND id <> $2 AND status IN ('done', 'failed')"
    ))
    .bind(cluster_id)
    .bind(command_id)
    .execute(&mut **tx)
    .await?;
    // The team's, when its holder synced it.
    let team_id: Option<String> =
        sqlx::query_scalar("SELECT team_id FROM pyo.clusters WHERE id = $1")
            .bind(cluster_id)
            .fetch_optional(&mut **tx)
            .await?;
    let team = match team_id {
        Some(t) if holder(&mut **tx, &t).await?.as_deref() == Some(acting) => Some(t),
        _ => None,
    };
    if !r.ok {
        tracing::warn!(cluster = %cluster_id, error = ?r.message, "the cluster's tailnet names couldn't be synced");
        if let Some(team) = &team
            && r.message.as_deref().is_some_and(refused)
        {
            sqlx::query("UPDATE pyo.team_tailscale SET problem = 'client_refused', updated_at = now() WHERE team_id = $1 AND problem IS NULL")
                .bind(team)
                .execute(&mut **tx)
                .await?;
        }
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
    let mut fields = json!({
        "suffix": done.suffix.clone().or(t.suffix.clone()),
        "dashboardName": own("dashboard").or(t.dashboard_name.clone()),
        "registryName": own("registry").or(t.registry_name.clone()),
        "sandboxesName": own("sandboxes").or(t.sandboxes_name.clone()),
        "forget": forget,
        "syncFailed": false,
    });
    match &team {
        Some(team) => {
            sqlx::query(
                "UPDATE pyo.team_tailscale SET machines = $2, lock_waiting = $3, suffix = coalesce($4, suffix),
                        problem = CASE WHEN problem = 'client_refused' THEN NULL ELSE problem END, updated_at = now()
                  WHERE team_id = $1",
            )
            .bind(team)
            .bind(serde_json::to_value(&done.machines).map_err(anyhow::Error::from)?)
            .bind(serde_json::to_value(&done.lock_waiting).map_err(anyhow::Error::from)?)
            .bind(&done.suffix)
            .execute(&mut **tx)
            .await?;
            fields["heldBy"] = json!(acting);
            // Kept on the team now.
            fields["machines"] = json!([]);
            fields["lockWaiting"] = json!([]);
        }
        None => {
            fields["lockWaiting"] = json!(done.lock_waiting);
            fields["machines"] = json!(done.machines);
        }
    }
    patch(&mut **tx, cluster_id, fields).await
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

// ── Each cluster's client, before teams kept one ──────────────────────────

/// A team's tailnet among its clusters' own clients (oldest first): the
/// one of the cluster Pyo runs on, else the one most of them are on (a
/// tie goes to the oldest cluster's). With the clusters that may hold it,
/// in order: the cluster Pyo runs on alone when it's on it, else the
/// connected ones, oldest first.
pub(crate) fn pick_tailnet(clusters: &[Candidate]) -> Option<(String, Vec<String>)> {
    fn on<'a>(clusters: &'a [Candidate], x: &'a str) -> impl Iterator<Item = &'a Candidate> + 'a {
        clusters
            .iter()
            .filter(move |c| c.suffix.as_deref() == Some(x))
    }
    let suffix = match clusters
        .iter()
        .find(|c| c.same_cluster && c.suffix.is_some())
    {
        Some(host) => host.suffix.clone()?,
        None => {
            // Oldest first, so the first of the biggest is the oldest's.
            let mut best: Option<(&str, usize)> = None;
            for c in clusters {
                let Some(x) = c.suffix.as_deref() else {
                    continue;
                };
                let n = on(clusters, x).count();
                if best.is_none_or(|(_, m)| n > m) {
                    best = Some((x, n));
                }
            }
            best?.0.to_owned()
        }
    };
    let holders: Vec<String> = match on(clusters, &suffix).find(|c| c.same_cluster) {
        Some(host) => [host]
            .into_iter()
            .filter(|c| c.connected)
            .map(|c| c.id.clone())
            .collect(),
        None => on(clusters, &suffix)
            .filter(|c| c.connected)
            .map(|c| c.id.clone())
            .collect(),
    };
    Some((suffix, holders))
}

/// Teams whose holder couldn't be picked, and when: tried again after a
/// while (each try asks Tailscale for a test key).
static PICK_TRIED: std::sync::Mutex<std::collections::BTreeMap<String, std::time::Instant>> =
    std::sync::Mutex::new(std::collections::BTreeMap::new());

/// Setups from before teams kept one client: per team whose clusters keep
/// their own, a holder is picked on the team's tailnet ([`pick_tailnet`]):
/// the cluster Pyo runs on, else the oldest connected one whose client
/// passes the test. The others on that tailnet forget theirs once the
/// holder synced them ([`forget_moved`]). Ones on another tailnet keep
/// theirs and stay as they are.
pub async fn migrate_setups(s: &AppState) -> ApiResult<()> {
    let teams: Vec<String> = sqlx::query_scalar(
        "SELECT DISTINCT c.team_id FROM pyo.clusters c
          WHERE coalesce(c.tailscale->>'clientId', '') <> ''
            AND NOT EXISTS (SELECT 1 FROM pyo.team_tailscale tt WHERE tt.team_id = c.team_id)",
    )
    .fetch_all(&s.db)
    .await?;
    for team in teams {
        migrate_team(s, &team).await?;
    }
    Ok(())
}

/// [`migrate_setups`] for one team.
async fn migrate_team(s: &AppState, team: &str) -> ApiResult<()> {
    let tried = PICK_TRIED
        .lock()
        .map(|m| {
            m.get(team)
                .is_some_and(|t| t.elapsed() < Duration::from_secs(600))
        })
        .unwrap_or(false);
    if tried {
        return Ok(());
    }
    if team_tailnet(&s.db, team).await?.is_some() {
        return Ok(());
    }
    let clusters = candidates(&s.db, team, true).await?;
    let Some((suffix, holders)) = pick_tailnet(&clusters) else {
        return Ok(());
    };
    let mut picked = None;
    for id in holders {
        let host = clusters.iter().any(|c| c.id == id && c.same_cluster);
        if host {
            picked = Some(id);
            break;
        }
        match ask_as(
            s,
            &id,
            None,
            Command::TailscaleTest {},
            Duration::from_secs(30),
        )
        .await
        {
            Ok((_, r))
                if r.data
                    .as_ref()
                    .and_then(|d| ts::ClientState::deserialize(d).ok())
                    .is_some_and(|c| c.ok) =>
            {
                picked = Some(id);
                break;
            }
            Ok(_) => {
                tracing::info!(cluster = %id, "tailscale: its client doesn't pass the test, so it won't keep the team's")
            }
            Err(e) => {
                tracing::info!(cluster = %id, error = %e.message, "tailscale: couldn't test its client")
            }
        }
    }
    let Some(h) = picked else {
        if let Ok(mut m) = PICK_TRIED.lock() {
            m.insert(team.to_owned(), std::time::Instant::now());
        }
        return Ok(());
    };
    let Some(t) = of(&s.db, &h).await? else {
        return Ok(());
    };
    keep_team(&s.db, &h, &t.client_id, &t.hint, Some(&suffix)).await?;
    let elsewhere: Vec<&str> = clusters
        .iter()
        .filter(|c| c.suffix.as_deref() != Some(suffix.as_str()))
        .map(|c| c.id.as_str())
        .collect();
    tracing::info!(team = %team, holder = %h, tailnet = %suffix, ?elsewhere, "tailscale: the team's holder keeps its client now");
    Ok(())
}

/// Where a team's cluster stands with the team's tailnet.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ClusterTailnet {
    /// It keeps the team's client.
    Holder,
    /// The holder acts for it.
    Member,
    /// The holder acts for it, and it still keeps a client of its own,
    /// forgotten once the holder synced it.
    Moving,
    /// Its own client, on another tailnet: its own agent acts for it.
    Elsewhere,
    /// The holder is gone.
    Waiting,
}

/// Where a cluster stands with its team's tailnet. None without one.
pub fn cluster_state(
    cluster_id: &str,
    own: Option<&Tailnet>,
    team: Option<&TeamTailnet>,
) -> Option<ClusterTailnet> {
    Some(match acting_for(cluster_id, own, team) {
        Acting::Holder(h) if h == cluster_id => ClusterTailnet::Holder,
        Acting::Holder(_) if own.is_some_and(Tailnet::own_client) => ClusterTailnet::Moving,
        Acting::Holder(_) => ClusterTailnet::Member,
        Acting::Own if team.is_some() => ClusterTailnet::Elsewhere,
        Acting::Gone => ClusterTailnet::Waiting,
        Acting::Own | Acting::None => return None,
    })
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
    let mut tx = crate::rls::begin(&s.db, &user).await?;
    crate::ops::require_role(&mut tx, &user, &q.team, crate::ops::Role::Admin).await?;
    let team_id: String = sqlx::query_scalar("SELECT id FROM pyo.teams WHERE id = $1 OR slug = $1")
        .bind(&q.team)
        .fetch_one(&mut *tx)
        .await?;
    drop(tx);
    let rows: Vec<Value> = sqlx::query_scalar(
        "SELECT tailscale->'lockWaiting' FROM pyo.clusters WHERE (team_id = $1 OR same_cluster) AND tailscale ? 'lockWaiting'
         UNION ALL
         SELECT lock_waiting FROM pyo.team_tailscale
          WHERE team_id = $1 OR team_id IN (SELECT team_id FROM pyo.clusters WHERE same_cluster)",
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

/// A cluster's Tailscale for the dashboard: never a secret. On its team's
/// tailnet, the team's client and the cluster that keeps it.
pub fn cluster_view(cluster_id: &str, t: &Tailnet, team: Option<&TeamTailnet>) -> Value {
    match (acting_for(cluster_id, Some(t), team), team) {
        (Acting::Holder(_) | Acting::Gone, Some(team)) => json!({
            "clientId": team.client_id,
            "hint": team.hint,
            "suffix": t.suffix.clone().or(team.suffix.clone()),
            "holderId": team.holder,
            "state": cluster_state(cluster_id, Some(t), Some(team)),
            "problem": team.problem,
        }),
        _ => json!({
            "clientId": t.client_id,
            "hint": t.hint,
            "suffix": t.suffix,
            "state": cluster_state(cluster_id, Some(t), team),
        }),
    }
}

/// A team's Tailscale for the dashboard (getState `teams[].tailscale`):
/// never a secret. `clusters`: the team's, oldest first, with their own
/// Tailscale and their servers' (as reported). Each cluster with where it
/// stands, its tailnet, its servers on a tailnet (`servers`) and on the
/// team's (`onTailnet`). `leftovers`: servers of removed clusters still on
/// a tailnet a while after.
pub async fn team_view(
    db: &mut sqlx::PgConnection,
    team: &TeamTailnet,
    clusters: &[(String, Option<Tailnet>, Vec<pyo_proto::ServerTailnet>)],
) -> ApiResult<Value> {
    let (devices, old): (i32, Value) = sqlx::query_as(
        "SELECT jsonb_array_length(machines), old_clients FROM pyo.team_tailscale WHERE team_id = $1",
    )
    .bind(&team.team_id)
    .fetch_optional(&mut *db)
    .await?
    .unwrap_or((0, json!([])));
    let left: Vec<(String, Option<String>, Value)> = sqlx::query_as(
        "SELECT cluster_name, suffix, machines FROM pyo.tailnet_leftovers
          WHERE team_id = $1 AND jsonb_array_length(machines) > 0 AND created_at < now() - interval '15 minutes'
          ORDER BY id",
    )
    .bind(&team.team_id)
    .fetch_all(&mut *db)
    .await?;
    let leftovers: Vec<Value> = left
        .into_iter()
        .map(|(cluster, suffix, machines)| {
            let m: Vec<LeftMachine> = serde_json::from_value(machines).unwrap_or_default();
            json!({ "cluster": cluster, "suffix": suffix, "servers": m.into_iter().map(|m| m.server).collect::<Vec<_>>() })
        })
        .collect();
    let list: Vec<Value> = clusters
        .iter()
        .filter_map(|(id, t, servers)| {
            let state = cluster_state(id, t.as_ref(), Some(team))?;
            let joined = servers.iter().filter(|x| !x.node_id.is_empty());
            Some(json!({
                "id": id,
                "state": state,
                "suffix": t.as_ref().and_then(|t| t.suffix.clone()),
                "servers": joined.clone().count(),
                "onTailnet": joined
                    .filter(|x| team.suffix.as_deref().is_some_and(|s| x.suffix == s))
                    .count(),
            }))
        })
        .collect();
    Ok(json!({
        "suffix": team.suffix,
        "holderId": team.holder,
        "holderName": team.holder.as_ref().map(|_| team.holder_name.clone()),
        "holderConnected": team.holder_connected,
        "problem": team.problem,
        "clientId": team.client_id,
        "hint": team.hint,
        "devices": devices,
        "oldClients": old,
        "leftovers": leftovers,
        "clusters": list,
    }))
}

/// Pyo's machines on a cluster's tailnet, as the last sync saw them: its
/// team's, when the holder acts for it. None without Tailscale there.
pub async fn machines_of(db: &PgPool, cluster_id: &str) -> ApiResult<Option<Vec<ts::Machine>>> {
    let r = route(db, cluster_id).await?;
    Ok(match r.acting {
        Acting::Holder(_) | Acting::Gone => {
            let v: Option<Value> =
                sqlx::query_scalar("SELECT machines FROM pyo.team_tailscale WHERE team_id = $1")
                    .bind(&r.team_id)
                    .fetch_optional(db)
                    .await?;
            v.and_then(|v| serde_json::from_value(v).ok())
        }
        Acting::Own => r.own.map(|t| t.machines),
        Acting::None => None,
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
        let team =
            match sqlx::query_scalar::<_, String>("SELECT team_id FROM pyo.clusters WHERE id = $1")
                .bind(&cluster_id)
                .fetch_optional(db)
                .await?
            {
                Some(team) => team_tailnet(db, &team).await?,
                None => None,
            };
        let client = cluster_view(&cluster_id, &t, team.as_ref());
        v["clientId"] = client["clientId"].clone();
        v["hint"] = client["hint"].clone();
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
        assert_eq!(cluster_view("cl_a", &t, None)["clientId"], "kC1");
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

    // ── A team's holder ─────────────────────────────────────────────────────

    fn own(client: &str, suffix: Option<&str>) -> Tailnet {
        Tailnet {
            client_id: client.into(),
            suffix: suffix.map(str::to_owned),
            ..Default::default()
        }
    }

    fn team(holder: Option<&str>, suffix: Option<&str>) -> TeamTailnet {
        TeamTailnet {
            team_id: "team_a".into(),
            holder: holder.map(str::to_owned),
            holder_name: "home".into(),
            holder_connected: true,
            client_id: "kT".into(),
            hint: "…T".into(),
            suffix: suffix.map(str::to_owned),
            problem: None,
        }
    }

    /// Who acts for a cluster: the team's holder for every cluster of the
    /// team on its tailnet, the cluster itself with its own client on
    /// another, nobody once the holder is gone.
    #[test]
    fn the_holder_acts_for_its_teams_clusters() {
        let tt = team(Some("cl_h"), Some("tail1.ts.net"));
        let h = Acting::Holder("cl_h".into());
        let member = Tailnet::default();
        assert_eq!(
            acting_for("cl_h", Some(&own("kT", Some("tail1.ts.net"))), Some(&tt)),
            h
        );
        assert_eq!(acting_for("cl_b", Some(&member), Some(&tt)), h);
        assert_eq!(
            acting_for("cl_b", None, Some(&tt)),
            h,
            "not synced yet, still the team's"
        );
        // Its own client on the team's tailnet: the holder, until it forgets it.
        assert_eq!(
            acting_for("cl_b", Some(&own("kB", Some("tail1.ts.net"))), Some(&tt)),
            h
        );
        // Its own client on another tailnet, or one Pyo hasn't seen.
        assert_eq!(
            acting_for("cl_c", Some(&own("kC", Some("tail2.ts.net"))), Some(&tt)),
            Acting::Own
        );
        assert_eq!(
            acting_for("cl_c", Some(&own("kC", None)), Some(&tt)),
            Acting::Own
        );
        // Before the team has a holder: as before.
        assert_eq!(
            acting_for("cl_c", Some(&own("kC", None)), None),
            Acting::Own
        );
        assert_eq!(acting_for("cl_c", Some(&member), None), Acting::None);
        assert_eq!(acting_for("cl_c", None, None), Acting::None);
        // The holder is gone.
        let gone = team(None, Some("tail1.ts.net"));
        assert_eq!(acting_for("cl_b", Some(&member), Some(&gone)), Acting::Gone);
        assert_eq!(
            acting_for("cl_b", Some(&own("kB", Some("tail1.ts.net"))), Some(&gone)),
            Acting::Own
        );
        // A member shows the team's client.
        let v = cluster_view("cl_b", &member, Some(&tt));
        assert_eq!(
            (v["clientId"].as_str(), v["holderId"].as_str()),
            (Some("kT"), Some("cl_h"))
        );
        assert_eq!(
            cluster_view("cl_c", &own("kC", Some("tail2.ts.net")), Some(&tt))["clientId"],
            "kC"
        );
    }

    fn cand(id: &str, host: bool, connected: bool, suffix: Option<&str>) -> Candidate {
        Candidate {
            id: id.into(),
            same_cluster: host,
            connected,
            suffix: suffix.map(str::to_owned),
        }
    }

    /// The team's tailnet among its clusters' own clients, and who may keep
    /// its client.
    #[test]
    fn the_holder_is_picked_on_the_teams_tailnet() {
        let (t1, t2) = (Some("tail1.ts.net"), Some("tail2.ts.net"));
        // The biggest group, the oldest connected first.
        let cs = [
            cand("a", false, false, t1),
            cand("b", false, true, t2),
            cand("c", false, true, t1),
            cand("d", false, true, t1),
            cand("e", false, true, None),
        ];
        assert_eq!(
            pick_tailnet(&cs),
            Some(("tail1.ts.net".into(), vec!["c".into(), "d".into()]))
        );
        // A tie: the oldest cluster's.
        let cs = [cand("a", false, true, t2), cand("b", false, true, t1)];
        assert_eq!(pick_tailnet(&cs).unwrap().0, "tail2.ts.net");
        // The cluster Pyo runs on wins, and alone may keep it.
        let cs = [
            cand("a", false, true, t1),
            cand("b", false, true, t1),
            cand("h", true, true, t2),
        ];
        assert_eq!(
            pick_tailnet(&cs),
            Some(("tail2.ts.net".into(), vec!["h".into()]))
        );
        let cs = [cand("a", false, true, t2), cand("h", true, false, t2)];
        assert_eq!(pick_tailnet(&cs), Some(("tail2.ts.net".into(), vec![])));
        // No tailnet seen yet.
        assert_eq!(pick_tailnet(&[cand("a", false, true, None)]), None);
        // A new setup: the cluster Pyo runs on, else the oldest connected.
        let cs = [
            cand("a", false, false, None),
            cand("b", false, true, None),
            cand("h", true, true, None),
        ];
        assert_eq!(holder_for_setup(&cs).as_deref(), Some("h"));
        assert_eq!(holder_for_setup(&cs[..2]).as_deref(), Some("b"));
        assert_eq!(holder_for_setup(&cs[..1]), None);
    }

    #[test]
    fn a_revoked_client_is_told_apart() {
        assert!(refused("Tailscale answered 401: invalid client"));
        assert!(refused("Tailscale isn't set up on this cluster."));
        assert!(!refused("Tailscale answered 500: oops"));
        assert!(!refused("Couldn't reach Tailscale: timeout"));
    }

    /// The copy people read: no semicolons, no clauses joined by a dash.
    #[test]
    fn holder_copy_is_plain() {
        for text in [HOLDER_GONE, NEEDED] {
            assert!(
                !text.contains(';') && !text.contains(" - ") && !text.contains('—'),
                "{text}"
            );
        }
    }

    // Against a throwaway Postgres: a team of `n` connected clusters, the
    // first one keeping the team's client on a tailnet of its own.

    async fn holder_team(n: usize) -> (AppState, String, Vec<String>, String) {
        let url = std::env::var("DATABASE_URL").expect("DATABASE_URL");
        let pool = sqlx::PgPool::connect(&url).await.unwrap();
        crate::migrate::run(&pool).await.unwrap();
        crate::vault::init(crate::vault::Vault::new(
            pool.clone(),
            crate::vault::Provider::from_env(),
        ));
        let run = crate::ids::new_id("t").to_lowercase().replace('_', "");
        let team = format!("team_{run}");
        sqlx::query("INSERT INTO pyo.teams (id, name, slug) VALUES ($1, $1, $1)")
            .bind(&team)
            .execute(&pool)
            .await
            .unwrap();
        let mut clusters = Vec::new();
        for i in 0..n {
            let c = format!("cl_{run}{i}");
            sqlx::query("INSERT INTO pyo.clusters (id, team_id, name, agent_token_hash, status, created_at) VALUES ($1, $2, $1, $1, 'connected', now() + make_interval(secs => $3))")
                .bind(&c)
                .bind(&team)
                .bind(i as f64)
                .execute(&pool)
                .await
                .unwrap();
            clusters.push(c);
        }
        let suffix = format!("{run}.ts.net");
        (
            crate::sandbox::e2e::test_state(pool),
            team,
            clusters,
            suffix,
        )
    }

    async fn states(db: &PgPool, team: &str) -> Vec<(String, ClusterTailnet)> {
        let tt = team_tailnet(db, team).await.unwrap();
        let rows: Vec<(String, Option<Value>)> = sqlx::query_as(
            "SELECT id, tailscale FROM pyo.clusters WHERE team_id = $1 ORDER BY created_at, id",
        )
        .bind(team)
        .fetch_all(db)
        .await
        .unwrap();
        rows.into_iter()
            .filter_map(|(id, v)| {
                let t: Option<Tailnet> = v.and_then(|v| serde_json::from_value(v).ok());
                cluster_state(&id, t.as_ref(), tt.as_ref()).map(|x| (id, x))
            })
            .collect()
    }

    async fn set_tailscale(db: &PgPool, cluster: &str, t: &Tailnet) {
        sqlx::query("UPDATE pyo.clusters SET tailscale = $2 WHERE id = $1")
            .bind(cluster)
            .bind(serde_json::to_value(t).unwrap())
            .execute(db)
            .await
            .unwrap();
    }

    /// A command as the agent got it: its id, kind, who it's for, the
    /// command itself.
    type Got = (String, String, Option<String>, Value);

    async fn pending(db: &PgPool, cluster: &str) -> Vec<Got> {
        let rows: Vec<(String, Option<String>, Value)> = sqlx::query_as(
            "SELECT id, for_cluster, command FROM pyo.commands WHERE cluster_id = $1 AND status = 'pending' ORDER BY created_at",
        )
        .bind(cluster)
        .fetch_all(db)
        .await
        .unwrap();
        rows.into_iter()
            .map(|(id, f, c)| (id, c["kind"].as_str().unwrap_or("").to_owned(), f, c))
            .collect()
    }

    /// The holder's agent against a tailnet: answers each sync on it as the
    /// agent would (every wanted name made, every machine to forget gone),
    /// and the answer is kept as agent.rs keeps it.
    async fn holder_syncs(s: &AppState, holder: &str, suffix: &str) -> Vec<Got> {
        let got = pending(&s.db, holder).await;
        for (id, kind, _, cmd) in &got {
            if kind != "tailscale_sync" {
                continue;
            }
            let w: TailnetSync = serde_json::from_value(cmd.clone()).unwrap();
            let done = TailnetSynced {
                suffix: Some(suffix.to_owned()),
                names: w
                    .domains
                    .iter()
                    .map(|d| ts::NameState {
                        name: d.clone(),
                        kind: "domain".into(),
                        addrs: vec!["100.100.1.2".into()],
                        error: None,
                    })
                    .collect(),
                forgotten: w.forget.clone(),
                machines: vec![ts::Machine {
                    node_id: "nH".into(),
                    hostname: "home-1".into(),
                    sign_command: None,
                }],
                ..Default::default()
            };
            let r = CommandResult {
                ok: true,
                message: None,
                logs: vec![],
                data: Some(serde_json::to_value(&done).unwrap()),
            };
            let mut tx = s.db.begin().await.unwrap();
            sqlx::query("UPDATE pyo.commands SET status = 'done', result = $2, done_at = now() WHERE id = $1")
                .bind(id)
                .bind(serde_json::to_value(&r).unwrap())
                .execute(&mut *tx)
                .await
                .unwrap();
            synced(&mut tx, holder, id, &r).await.unwrap();
            tx.commit().await.unwrap();
        }
        got
    }

    /// The agent of `cluster`: answers its next command of `kind` with
    /// `data`, and says who it was for.
    async fn answers_for(
        db: PgPool,
        cluster: String,
        kind: &str,
        data: Value,
    ) -> (Value, Option<String>) {
        loop {
            let row: Option<(String, Value, Option<String>)> = sqlx::query_as(
                "SELECT id, command, for_cluster FROM pyo.commands WHERE cluster_id = $1 AND command->>'kind' = $2 AND status = 'pending' ORDER BY created_at LIMIT 1",
            )
            .bind(&cluster)
            .bind(kind)
            .fetch_optional(&db)
            .await
            .unwrap();
            if let Some((id, cmd, for_cluster)) = row {
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
                return (cmd, for_cluster);
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// A team of three clusters, one keeping its client: every sync runs on
    /// the holder, each for its cluster, each answer kept for that cluster,
    /// and the tailnet's machines kept once, on the team.
    #[tokio::test]
    #[ignore = "needs DATABASE_URL (a throwaway Postgres)"]
    async fn three_clusters_one_holder() {
        let (s, team, cl, suffix) = holder_team(3).await;
        let (h, b, c) = (&cl[0], &cl[1], &cl[2]);
        set_tailscale(&s.db, h, &own("kT", Some(&suffix))).await;
        keep_team(&s.db, h, "kT", "…T", Some(&suffix))
            .await
            .unwrap();
        // A service on b with a tailnet domain.
        let project = format!("prj_{team}");
        sqlx::query(
            "INSERT INTO pyo.projects (id, team_id, name, namespace) VALUES ($1, $2, $1, $1)",
        )
        .bind(&project)
        .bind(&team)
        .execute(&s.db)
        .await
        .unwrap();
        let spec = json!({ "domains": [{ "id": "dom_b", "hostname": format!("blog.{suffix}"), "portName": "http", "status": "pending_dns", "tls": "pending", "tlsMode": "auto", "redirectHttps": false, "createdAt": 1 }] });
        sqlx::query("INSERT INTO pyo.services (id, team_id, project_id, cluster_id, name, spec) VALUES ($1, $2, $3, $4, 'web', $5)")
            .bind(format!("svc_{team}"))
            .bind(&team)
            .bind(&project)
            .bind(b)
            .bind(&spec)
            .execute(&s.db)
            .await
            .unwrap();

        // b and c had no Tailscale: on the team's tailnet now.
        for x in [b, c] {
            let t = of(&s.db, x).await.unwrap().expect("a member");
            assert!(!t.own_client());
            assert_eq!(t.suffix.as_deref(), Some(suffix.as_str()));
        }
        assert_eq!(
            holder(&s.db, &team).await.unwrap().as_deref(),
            Some(h.as_str())
        );

        sync_names(&s, Some(&team)).await.unwrap();
        // Nothing waits on b or c themselves.
        assert!(pending(&s.db, b).await.is_empty() && pending(&s.db, c).await.is_empty());
        let syncs = pending(&s.db, h).await;
        let mut fors: Vec<_> = syncs.iter().map(|g| g.2.clone().unwrap()).collect();
        fors.sort();
        let mut want = cl.clone();
        want.sort();
        assert_eq!(fors, want, "one sync per cluster, on the holder");
        let for_b = syncs
            .iter()
            .find(|g| g.2.as_deref() == Some(b.as_str()))
            .unwrap();
        assert_eq!(for_b.3["domains"], json!(["blog"]));
        assert_eq!(for_b.3["cluster"], json!(b));
        let install = install_id(&s.db).await.unwrap();
        assert_eq!(
            for_b.3["mark"],
            json!(ts::marker(&install, b)),
            "b's own mark"
        );

        // Another pass while they're out: nothing more.
        sync_names(&s, Some(&team)).await.unwrap();
        assert_eq!(pending(&s.db, h).await.len(), 3);

        holder_syncs(&s, h, &suffix).await;
        let tb = of(&s.db, b).await.unwrap().unwrap();
        assert_eq!(tb.held_by.as_deref(), Some(h.as_str()));
        assert!(tb.machines.is_empty(), "kept on the team");
        let names: Vec<String> =
            sqlx::query_scalar("SELECT name FROM pyo.cluster_tailnet_names WHERE cluster_id = $1")
                .bind(b)
                .fetch_all(&s.db)
                .await
                .unwrap();
        assert_eq!(names, ["blog"]);
        let machines: Value =
            sqlx::query_scalar("SELECT machines FROM pyo.team_tailscale WHERE team_id = $1")
                .bind(&team)
                .fetch_one(&s.db)
                .await
                .unwrap();
        assert_eq!(machines[0]["nodeId"], "nH");
        assert_eq!(machines_of(&s.db, c).await.unwrap().unwrap().len(), 1);
        // The heartbeat tells b's servers what to host.
        let w = wanted_for(&s.db, b, false).await.unwrap().unwrap();
        assert_eq!(w.services[0].name, "blog");
        // Answered: b's again, as it made blog now, then nothing more.
        sync_names(&s, Some(&team)).await.unwrap();
        let again = pending(&s.db, h).await;
        assert_eq!(again.len(), 1);
        assert_eq!(again[0].3["made"], json!(["blog"]));
        holder_syncs(&s, h, &suffix).await;
        sync_names(&s, Some(&team)).await.unwrap();
        assert!(pending(&s.db, h).await.is_empty());

        // c can't have b's name: another cluster of the tailnet wants it.
        assert!(
            named_elsewhere(&s.db, c, Some(&suffix), "blog")
                .await
                .unwrap()
        );
        assert!(
            !named_elsewhere(&s.db, b, Some(&suffix), "blog")
                .await
                .unwrap()
        );
        assert_eq!(
            states(&s.db, &team).await,
            [
                (h.clone(), ClusterTailnet::Holder),
                (b.clone(), ClusterTailnet::Member),
                (c.clone(), ClusterTailnet::Member)
            ]
        );
    }

    /// A join key for a cluster that keeps no client comes from the holder,
    /// and goes once read. With the holder offline, it says who keeps
    /// Tailscale, and syncs wait until it's back.
    #[tokio::test]
    #[ignore = "needs DATABASE_URL (a throwaway Postgres)"]
    async fn join_keys_come_from_the_holder_and_wait_for_it() {
        let (s, team, cl, suffix) = holder_team(3).await;
        let (h, b) = (&cl[0], &cl[1]);
        set_tailscale(&s.db, h, &own("kT", Some(&suffix))).await;
        keep_team(&s.db, h, "kT", "…T", Some(&suffix))
            .await
            .unwrap();
        assert_eq!(key_cluster(&s.db, Some(b)).await.unwrap(), *b);

        let agent = tokio::spawn(answers_for(
            s.db.clone(),
            h.clone(),
            "tailscale_key",
            json!({ ts::KEY_FIELD: "tskey-auth-kB-SECRET" }),
        ));
        let key = join_key(&s, b, "Pyo: join web-2", "this server")
            .await
            .unwrap();
        assert_eq!(key, "tskey-auth-kB-SECRET");
        let (cmd, for_cluster) = agent.await.unwrap();
        assert_eq!(cmd["description"], "Pyo: join web-2");
        assert_eq!(for_cluster.as_deref(), Some(b.as_str()));
        assert_eq!(
            commands(&s.db, h, "tailscale_key").await,
            0,
            "gone once read"
        );
        assert_eq!(commands(&s.db, b, "tailscale_key").await, 0, "never on b");

        // The holder goes offline.
        sqlx::query("UPDATE pyo.clusters SET status = 'unreachable' WHERE id = $1")
            .bind(h)
            .execute(&s.db)
            .await
            .unwrap();
        let e = join_key(&s, b, "Pyo: join web-3", "this server")
            .await
            .unwrap_err();
        assert_eq!(e.status, axum::http::StatusCode::CONFLICT);
        assert_eq!(
            e.message,
            format!("{h} keeps your Tailscale and isn't connected.")
        );
        sync_names(&s, Some(&team)).await.unwrap();
        for x in &cl {
            assert!(
                pending(&s.db, x).await.is_empty(),
                "nothing queued while it's away"
            );
        }
        // Back: the syncs go out.
        sqlx::query("UPDATE pyo.clusters SET status = 'connected' WHERE id = $1")
            .bind(h)
            .execute(&s.db)
            .await
            .unwrap();
        sync_names(&s, Some(&team)).await.unwrap();
        assert_eq!(pending(&s.db, h).await.len(), 3);
    }

    /// At most five Tailscale commands out on a holder at a time.
    #[tokio::test]
    #[ignore = "needs DATABASE_URL (a throwaway Postgres)"]
    async fn a_holder_gets_five_at_a_time() {
        let (s, team, cl, suffix) = holder_team(7).await;
        set_tailscale(&s.db, &cl[0], &own("kT", Some(&suffix))).await;
        keep_team(&s.db, &cl[0], "kT", "…T", Some(&suffix))
            .await
            .unwrap();
        sync_names(&s, Some(&team)).await.unwrap();
        assert_eq!(pending(&s.db, &cl[0]).await.len(), 5);
        holder_syncs(&s, &cl[0], &suffix).await;
        sync_names(&s, Some(&team)).await.unwrap();
        assert_eq!(pending(&s.db, &cl[0]).await.len(), 2);
    }

    /// The holder is always a cluster of its own team.
    #[tokio::test]
    #[ignore = "needs DATABASE_URL (a throwaway Postgres)"]
    async fn a_holder_never_acts_for_another_team() {
        let (s, team, cl, _) = holder_team(1).await;
        let (_, other, ocl, _) = holder_team(1).await;
        sqlx::query("INSERT INTO pyo.team_tailscale (team_id, holder_cluster_id, client_id) VALUES ($1, $2, 'kX')")
            .bind(&team)
            .bind(&ocl[0])
            .execute(&s.db)
            .await
            .unwrap();
        assert_eq!(holder(&s.db, &team).await.unwrap(), None);
        assert_eq!(route(&s.db, &cl[0]).await.unwrap().acting, Acting::Gone);
        let mut tx = s.db.begin().await.unwrap();
        assert!(
            enqueue_for(&mut tx, &ocl[0], Some(&cl[0]), Command::TailscaleTest {})
                .await
                .is_err(),
            "{other}'s cluster can't run {team}'s"
        );
    }

    /// Setups from before: two clusters with their own clients on one
    /// tailnet and one on another. The oldest whose client passes keeps the
    /// team's, the other on its tailnet forgets its own once the holder
    /// synced it (its ID listed to delete), and the third stays as it was.
    #[tokio::test]
    #[ignore = "needs DATABASE_URL (a throwaway Postgres)"]
    async fn setups_from_before_move_to_one_holder() {
        let (s, team, cl, suffix) = holder_team(3).await;
        let (a, b, c) = (&cl[0], &cl[1], &cl[2]);
        let other = format!("o{suffix}");
        set_tailscale(&s.db, a, &own("kA", Some(&suffix))).await;
        set_tailscale(&s.db, b, &own("kB", Some(&suffix))).await;
        set_tailscale(&s.db, c, &own("kC", Some(&other))).await;

        let test = tokio::spawn(answers_for(
            s.db.clone(),
            a.clone(),
            "tailscale_test",
            json!({ "ok": true, "checks": [], "clientId": "kA", "hint": "…A", "suffix": suffix }),
        ));
        migrate_team(&s, &team).await.unwrap();
        test.await.unwrap();
        let tt = team_tailnet(&s.db, &team).await.unwrap().unwrap();
        assert_eq!(
            (tt.holder.as_deref(), tt.client_id.as_str()),
            (Some(a.as_str()), "kA")
        );
        assert_eq!(tt.suffix.as_deref(), Some(suffix.as_str()));
        assert_eq!(
            states(&s.db, &team).await,
            [
                (a.clone(), ClusterTailnet::Holder),
                (b.clone(), ClusterTailnet::Moving),
                (c.clone(), ClusterTailnet::Elsewhere)
            ]
        );

        // a syncs itself and b. c keeps syncing on its own.
        sync_names(&s, Some(&team)).await.unwrap();
        let on_a: Vec<_> = pending(&s.db, a)
            .await
            .into_iter()
            .map(|g| g.2.unwrap())
            .collect();
        assert_eq!(on_a.len(), 2);
        assert!(on_a.contains(a) && on_a.contains(b));
        assert!(pending(&s.db, b).await.is_empty());
        let on_c = pending(&s.db, c).await;
        assert_eq!(on_c.len(), 1);
        assert_eq!(on_c[0].2.as_deref(), Some(c.as_str()));
        // b isn't asked to forget before a's sync for it went through.
        let b_remove = commands(&s.db, b, "tailscale_remove").await;
        assert_eq!(b_remove, 0);

        holder_syncs(&s, a, &suffix).await;
        let forget = tokio::spawn(answers_for(
            s.db.clone(),
            b.clone(),
            "tailscale_remove",
            json!({}),
        ));
        sync_names(&s, Some(&team)).await.unwrap();
        let (cmd, _) = forget.await.unwrap();
        assert_eq!(
            (cmd["names"].clone(), cmd["marks"].clone()),
            (json!([]), json!([])),
            "it deletes nothing"
        );
        let tb = of(&s.db, b).await.unwrap().unwrap();
        assert!(!tb.own_client());
        let old: Value =
            sqlx::query_scalar("SELECT old_clients FROM pyo.team_tailscale WHERE team_id = $1")
                .bind(&team)
                .fetch_one(&s.db)
                .await
                .unwrap();
        assert_eq!(
            old,
            json!([{ "clientId": "kB", "hint": "", "clusterId": b, "cluster": b }])
        );
        assert_eq!(
            of(&s.db, c).await.unwrap().unwrap().client_id,
            "kC",
            "left as it was"
        );
        assert_eq!(
            states(&s.db, &team).await[1],
            (b.clone(), ClusterTailnet::Member)
        );
        // Picked once.
        migrate_team(&s, &team).await.unwrap();
        assert_eq!(commands(&s.db, a, "tailscale_test").await, 1);
    }

    /// A removed cluster's servers and names leave the tailnet: the holder
    /// takes them off. A removed holder leaves the team waiting for a new
    /// setup.
    #[tokio::test]
    #[ignore = "needs DATABASE_URL (a throwaway Postgres)"]
    async fn a_removed_clusters_servers_leave_the_tailnet() {
        let (s, team, cl, suffix) = holder_team(3).await;
        let (h, b, c) = (&cl[0], &cl[1], &cl[2]);
        set_tailscale(&s.db, h, &own("kT", Some(&suffix))).await;
        keep_team(&s.db, h, "kT", "…T", Some(&suffix))
            .await
            .unwrap();
        sqlx::query("UPDATE pyo.clusters SET capabilities = jsonb_build_object('tailnet', jsonb_build_array(jsonb_build_object('server', 'web-1', 'nodeId', 'nB1'))) WHERE id = $1")
            .bind(b)
            .execute(&s.db)
            .await
            .unwrap();
        sqlx::query("INSERT INTO pyo.cluster_tailnet_names (cluster_id, name, kind, addrs) VALUES ($1, 'shop', 'domain', '{100.100.1.3}')")
            .bind(b)
            .execute(&s.db)
            .await
            .unwrap();
        sqlx::query("DELETE FROM pyo.clusters WHERE id = $1")
            .bind(b)
            .execute(&s.db)
            .await
            .unwrap();
        sync_names(&s, Some(&team)).await.unwrap();
        let got = pending(&s.db, h).await;
        let gone = got
            .iter()
            .find(|g| g.3["cluster"] == json!(b))
            .expect("a sync for the removed cluster");
        assert_eq!(gone.2, None);
        assert_eq!(gone.3["forget"], json!(["nB1"]));
        assert_eq!(gone.3["made"], json!(["shop"]));
        assert_eq!(gone.3["domains"], json!([]));
        let install = install_id(&s.db).await.unwrap();
        assert_eq!(gone.3["mark"], json!(ts::marker(&install, b)));
        holder_syncs(&s, h, &suffix).await;
        let left: i64 =
            sqlx::query_scalar("SELECT count(*) FROM pyo.tailnet_leftovers WHERE team_id = $1")
                .bind(&team)
                .fetch_one(&s.db)
                .await
                .unwrap();
        assert_eq!(left, 0, "done once its machines are gone");

        // The holder is removed: the team waits for a new setup.
        sqlx::query("DELETE FROM pyo.clusters WHERE id = $1")
            .bind(h)
            .execute(&s.db)
            .await
            .unwrap();
        sync_names(&s, Some(&team)).await.unwrap();
        let tt = team_tailnet(&s.db, &team).await.unwrap().unwrap();
        assert_eq!(
            (tt.holder, tt.problem.as_deref()),
            (None, Some("holder_gone"))
        );
        let e = join_key(&s, c, "Pyo: join web-9", "this server")
            .await
            .unwrap_err();
        assert_eq!(e.message, HOLDER_GONE);
        let left: Vec<String> =
            sqlx::query_scalar("SELECT cluster_id FROM pyo.tailnet_leftovers WHERE team_id = $1")
                .bind(&team)
                .fetch_all(&s.db)
                .await
                .unwrap();
        assert_eq!(
            left,
            Vec::<String>::new(),
            "the holder had no servers on it"
        );

        // A new token: c keeps it now, and the old client is listed.
        let agent = tokio::spawn(answers_for(
            s.db.clone(),
            c.clone(),
            "tailscale_setup",
            json!({ "ok": true, "changed": [], "todo": [], "error": null, "checks": [], "tokenDeleted": true, "reviewAgain": false,
                    "client": { "clientId": "kNew", "hint": "…W" } }),
        ));
        set_up(
            &s,
            &Scope::Team(team.clone()),
            "tskey-api-kAbc-0123456789abcdef".into(),
            "v1",
        )
        .await
        .unwrap();
        agent.await.unwrap();
        let tt = team_tailnet(&s.db, &team).await.unwrap().unwrap();
        assert_eq!(
            (tt.holder.as_deref(), tt.problem.as_deref()),
            (Some(c.as_str()), None)
        );
        let old: Value =
            sqlx::query_scalar("SELECT old_clients FROM pyo.team_tailscale WHERE team_id = $1")
                .bind(&team)
                .fetch_one(&s.db)
                .await
                .unwrap();
        assert_eq!(old[0]["clientId"], "kT");
        assert_eq!(old[0]["clusterId"], "", "its cluster is gone");
    }

    /// On Pyo Cloud every Tailscale command for the team's clusters is
    /// signed for the holder: the cluster whose agent runs it. A member's
    /// agent would refuse it.
    #[tokio::test]
    #[ignore = "needs DATABASE_URL (a throwaway Postgres)"]
    async fn the_holders_commands_are_signed_for_the_holder() {
        use pyo_proto::signing::{Claims, Refused, Signer, TrustedKeys};
        struct Local(Signer);
        impl crate::CommandSigner for Local {
            fn sign<'a>(
                &'a self,
                cluster_id: &'a str,
                items: &'a [crate::ToSign],
            ) -> futures::future::BoxFuture<'a, anyhow::Result<Vec<crate::Signature>>> {
                Box::pin(async move {
                    let now = pyo_proto::now_ms() as u64;
                    Ok(items
                        .iter()
                        .enumerate()
                        .map(|(i, t)| {
                            let (claims, sig) = self.0.sign(&Claims {
                                v: 1,
                                id: t.id.clone(),
                                cluster: cluster_id.into(),
                                seq: i as u64 + 1,
                                iat: now,
                                exp: now + 600_000,
                                kind: t.kind.clone(),
                                sha256: t.sha256.clone(),
                            });
                            crate::Signature {
                                claims,
                                sig,
                                kid: self.0.kid().into(),
                                refused: None,
                                delegation: None,
                            }
                        })
                        .collect())
                })
            }
            fn sign_reply<'a>(
                &'a self,
                _: &'a str,
                _: &'a crate::ReplyToSign,
            ) -> futures::future::BoxFuture<'a, anyhow::Result<crate::Signature>> {
                Box::pin(async { Err(anyhow::anyhow!("not here")) })
            }
            fn public_keys(&self) -> String {
                self.0.public_key()
            }
        }
        let (s, team, cl, suffix) = holder_team(3).await;
        let h = &cl[0];
        set_tailscale(&s.db, h, &own("kT", Some(&suffix))).await;
        keep_team(&s.db, h, "kT", "…T", Some(&suffix))
            .await
            .unwrap();
        sync_names(&s, Some(&team)).await.unwrap();
        let got = pending(&s.db, h).await;
        assert_eq!(got.len(), 3);
        let out: Vec<pyo_proto::CommandEnvelope> = got
            .iter()
            .map(|(id, _, _, cmd)| pyo_proto::CommandEnvelope {
                id: id.clone(),
                command: serde_json::from_value(cmd.clone()).unwrap(),
                signed: None,
            })
            .collect();
        let signer = Local(Signer::from_pkcs8(&Signer::generate_pkcs8().unwrap()).unwrap());
        let keys = TrustedKeys::parse(&crate::CommandSigner::public_keys(&signer)).unwrap();
        let signed = crate::agent::sign_all(&s, &signer, h, out).await.unwrap();
        assert_eq!(signed.len(), 3);
        let now = pyo_proto::now_ms() as u64;
        for e in &signed {
            let c = keys.verify(e.signed.as_ref(), &e.id, h, 0, now).unwrap();
            assert_eq!(c.kind, "tailscale_sync");
            assert!(matches!(
                keys.verify(e.signed.as_ref(), &e.id, &cl[1], 0, now),
                Err(Refused::OtherCluster)
            ));
        }
    }

    // ── The team's Tailscale on Integrations ────────────────────────────────

    /// A team with no clusters can't set up yet. Then the oldest connected
    /// cluster keeps the team's client. With it offline, a new setup makes
    /// a new holder, and the old client goes on the list to delete.
    #[tokio::test]
    #[ignore = "needs DATABASE_URL (a throwaway Postgres)"]
    async fn a_team_setup_runs_on_the_holder_pyo_picks() {
        let (s, empty, _, _) = holder_team(0).await;
        let e = plan(
            &s,
            &Scope::Team(empty.clone()),
            "tskey-api-kAbc-0123456789abcdef".into(),
        )
        .await
        .unwrap_err();
        assert_eq!(
            e.message,
            "Connect a cluster first. Tailscale is set up on it."
        );

        let (s, team, cl, _) = holder_team(2).await;
        let setup = |cluster: &str, client: &str| {
            tokio::spawn(answers_for(
                s.db.clone(),
                cluster.to_owned(),
                "tailscale_setup",
                json!({ "ok": true, "changed": ["Made an OAuth client"], "todo": [], "error": null, "checks": [], "tokenDeleted": true, "reviewAgain": false,
                        "client": { "clientId": client, "hint": "…9" } }),
            ))
        };
        let agent = setup(&cl[0], "kN");
        let out = set_up(
            &s,
            &Scope::Team(team.clone()),
            "tskey-api-kAbc-0123456789abcdef".into(),
            "v1",
        )
        .await
        .unwrap();
        agent.await.unwrap();
        assert_eq!(out["clusterId"], json!(cl[0]));
        assert!(
            out.get("client").is_none(),
            "the client stays out of the answer"
        );
        let tt = team_tailnet(&s.db, &team).await.unwrap().unwrap();
        assert_eq!(
            (tt.holder.as_deref(), tt.client_id.as_str()),
            (Some(cl[0].as_str()), "kN")
        );
        assert_eq!(
            states(&s.db, &team).await[1],
            (cl[1].clone(), ClusterTailnet::Member)
        );

        sqlx::query("UPDATE pyo.clusters SET status = 'unreachable' WHERE id = $1")
            .bind(&cl[0])
            .execute(&s.db)
            .await
            .unwrap();
        let agent = setup(&cl[1], "kM");
        set_up(
            &s,
            &Scope::Team(team.clone()),
            "tskey-api-kAbc-0123456789abcdef".into(),
            "v1",
        )
        .await
        .unwrap();
        agent.await.unwrap();
        let tt = team_tailnet(&s.db, &team).await.unwrap().unwrap();
        assert_eq!(
            (tt.holder.as_deref(), tt.client_id.as_str()),
            (Some(cl[1].as_str()), "kM")
        );
        let old: Value =
            sqlx::query_scalar("SELECT old_clients FROM pyo.team_tailscale WHERE team_id = $1")
                .bind(&team)
                .fetch_one(&s.db)
                .await
                .unwrap();
        assert_eq!(
            old,
            json!([{ "clientId": "kN", "hint": "…9", "clusterId": cl[0], "cluster": cl[0] }])
        );

        // What the dashboard reads.
        let mut c = s.db.acquire().await.unwrap();
        let mine: Vec<(String, Option<Tailnet>, Vec<pyo_proto::ServerTailnet>)> = vec![
            (cl[0].clone(), of(&s.db, &cl[0]).await.unwrap(), vec![]),
            (cl[1].clone(), of(&s.db, &cl[1]).await.unwrap(), vec![]),
        ];
        let v = team_view(&mut c, &tt, &mine).await.unwrap();
        assert_eq!(v["holderId"], json!(cl[1]));
        assert_eq!(v["clientId"], "kM");
        assert_eq!(v["oldClients"][0]["clientId"], "kN");
        assert_eq!(v["clusters"][1]["state"], "holder");
        assert_eq!(v["devices"], 0);
        assert!(!v.to_string().contains("tskey"), "{v}");

        // Disconnect: the holder forgets the client, every cluster leaves.
        let forget = tokio::spawn(answers_for(
            s.db.clone(),
            cl[1].clone(),
            "tailscale_remove",
            json!({}),
        ));
        remove(&s, &Scope::Team(team.clone())).await.unwrap();
        forget.await.unwrap();
        assert!(team_tailnet(&s.db, &team).await.unwrap().is_none());
        assert!(of(&s.db, &cl[1]).await.unwrap().is_none());
        // The old holder still keeps its own (Pyo hasn't seen its tailnet).
        assert_eq!(of(&s.db, &cl[0]).await.unwrap().unwrap().client_id, "kN");
    }

    /// A single-server cluster on another tailnet moves to the team's: its
    /// names there go, its client is forgotten and listed, its tailnet
    /// domains move, and its server gets a command with the holder's key.
    /// More servers: by hand.
    #[tokio::test]
    #[ignore = "needs DATABASE_URL (a throwaway Postgres)"]
    async fn a_single_server_cluster_moves_to_the_teams_tailnet() {
        let (s, team, cl, suffix) = holder_team(3).await;
        let (h, e, big) = (&cl[0], &cl[1], &cl[2]);
        let other = format!("o{suffix}");
        set_tailscale(&s.db, h, &own("kT", Some(&suffix))).await;
        keep_team(&s.db, h, "kT", "…T", Some(&suffix))
            .await
            .unwrap();
        set_tailscale(&s.db, e, &own("kE", Some(&other))).await;
        set_tailscale(&s.db, big, &own("kB", Some(&other))).await;
        for (c, n) in [(e, "web-1"), (big, "a-1"), (big, "a-2")] {
            sqlx::query(
                "INSERT INTO pyo.servers (id, cluster_id, name, report) VALUES ($1, $2, $3, '{}')",
            )
            .bind(format!("{c}-{n}"))
            .bind(c)
            .bind(n)
            .execute(&s.db)
            .await
            .unwrap();
        }
        sqlx::query("UPDATE pyo.clusters SET capabilities = jsonb_build_object('tailnet', jsonb_build_array(jsonb_build_object('server', 'web-1', 'nodeId', 'nE1', 'suffix', $2::text))) WHERE id = $1")
            .bind(e)
            .bind(&other)
            .execute(&s.db)
            .await
            .unwrap();
        sqlx::query("INSERT INTO pyo.cluster_tailnet_names (cluster_id, name, kind, addrs) VALUES ($1, 'blog', 'domain', '{100.100.1.3}')")
            .bind(e)
            .execute(&s.db)
            .await
            .unwrap();
        let project = format!("prj_{team}");
        sqlx::query(
            "INSERT INTO pyo.projects (id, team_id, name, namespace) VALUES ($1, $2, $1, $1)",
        )
        .bind(&project)
        .bind(&team)
        .execute(&s.db)
        .await
        .unwrap();
        let spec = json!({ "domains": [
            { "id": "dom_b", "hostname": format!("blog.{other}"), "portName": "http", "status": "active", "tls": "issued", "tlsMode": "auto", "redirectHttps": false, "createdAt": 1 },
            { "id": "dom_p", "hostname": "blog.example.com", "portName": "http", "status": "active", "tls": "issued", "tlsMode": "auto", "redirectHttps": false, "createdAt": 1 }
        ] });
        let service = format!("svc_{team}");
        sqlx::query("INSERT INTO pyo.services (id, team_id, project_id, cluster_id, name, spec) VALUES ($1, $2, $3, $4, 'web', $5)")
            .bind(&service)
            .bind(&team)
            .bind(&project)
            .bind(e)
            .bind(&spec)
            .execute(&s.db)
            .await
            .unwrap();
        assert_eq!(
            states(&s.db, &team).await[1],
            (e.clone(), ClusterTailnet::Elsewhere)
        );

        assert_eq!(
            move_cluster(&s, big).await.unwrap_err().message,
            MOVE_BY_HAND
        );
        assert_eq!(
            move_cluster(&s, h).await.unwrap_err().message,
            "This cluster is on your tailnet already."
        );

        let key = tokio::spawn(answers_for(
            s.db.clone(),
            h.clone(),
            "tailscale_key",
            json!({ ts::KEY_FIELD: "tskey-auth-kMove-SECRET" }),
        ));
        let gone = tokio::spawn(answers_for(
            s.db.clone(),
            e.clone(),
            "tailscale_remove",
            json!({}),
        ));
        let out = move_cluster(&s, e).await.unwrap();
        let (_, for_cluster) = key.await.unwrap();
        assert_eq!(
            for_cluster.as_deref(),
            Some(e.as_str()),
            "the holder's key, for e"
        );
        let (removed, _) = gone.await.unwrap();
        assert_eq!(
            removed["names"],
            json!(["blog"]),
            "its names on the old tailnet go"
        );
        let cmd = out["commands"][0]["command"].as_str().unwrap();
        assert_eq!(out["commands"][0]["server"], "web-1");
        assert!(
            cmd.contains(&format!("--tailnet '{suffix}'"))
                && cmd.contains("tskey-auth-kMove-SECRET"),
            "{cmd}"
        );

        let t = of(&s.db, e).await.unwrap().unwrap();
        assert!(!t.own_client());
        assert_eq!(t.suffix.as_deref(), Some(suffix.as_str()));
        assert_eq!(
            states(&s.db, &team).await[1],
            (e.clone(), ClusterTailnet::Member)
        );
        let spec: Value = sqlx::query_scalar("SELECT spec FROM pyo.services WHERE id = $1")
            .bind(&service)
            .fetch_one(&s.db)
            .await
            .unwrap();
        assert_eq!(
            spec["domains"][0]["hostname"],
            json!(format!("blog.{suffix}"))
        );
        assert_eq!(spec["domains"][0]["status"], "pending_dns");
        assert_eq!(
            spec["domains"][1]["hostname"], "blog.example.com",
            "other domains stay"
        );
        let old: Value =
            sqlx::query_scalar("SELECT old_clients FROM pyo.team_tailscale WHERE team_id = $1")
                .bind(&team)
                .fetch_one(&s.db)
                .await
                .unwrap();
        assert_eq!(old[0]["clientId"], "kE");
        assert_eq!(old[0]["suffix"], json!(other));

        // The server isn't on the team's tailnet yet: a fresh command.
        let key = tokio::spawn(answers_for(
            s.db.clone(),
            h.clone(),
            "tailscale_key",
            json!({ ts::KEY_FIELD: "tskey-auth-kAgain-SECRET" }),
        ));
        let again = move_cluster(&s, e).await.unwrap();
        key.await.unwrap();
        assert!(
            again["commands"][0]["command"]
                .as_str()
                .unwrap()
                .contains("kAgain")
        );
        // On it: nothing to move.
        sqlx::query("UPDATE pyo.clusters SET capabilities = jsonb_build_object('tailnet', jsonb_build_array(jsonb_build_object('server', 'web-1', 'nodeId', 'nE2', 'suffix', $2::text))) WHERE id = $1")
            .bind(e)
            .bind(&suffix)
            .execute(&s.db)
            .await
            .unwrap();
        assert_eq!(
            move_cluster(&s, e).await.unwrap_err().message,
            "This cluster is on your tailnet already."
        );
    }

    /// A removed cluster on another tailnet: a cluster of the team with its
    /// own client there takes its servers off. With none, they're listed.
    #[tokio::test]
    #[ignore = "needs DATABASE_URL (a throwaway Postgres)"]
    async fn a_removed_cluster_on_another_tailnet_leaves_it_too() {
        let (s, team, cl, suffix) = holder_team(3).await;
        let (h, e1, e2) = (&cl[0], &cl[1], &cl[2]);
        let other = format!("o{suffix}");
        set_tailscale(&s.db, h, &own("kT", Some(&suffix))).await;
        keep_team(&s.db, h, "kT", "…T", Some(&suffix))
            .await
            .unwrap();
        set_tailscale(&s.db, e1, &own("kE1", Some(&other))).await;
        set_tailscale(&s.db, e2, &own("kE2", Some(&other))).await;
        let on = |c: &str, node: &str| {
            sqlx::query("UPDATE pyo.clusters SET capabilities = jsonb_build_object('tailnet', jsonb_build_array(jsonb_build_object('server', 'web-1', 'nodeId', $2::text))) WHERE id = $1")
                .bind(c.to_owned())
                .bind(node.to_owned())
                .execute(&s.db)
        };
        on(e1, "nA").await.unwrap();
        on(e2, "nB").await.unwrap();
        sqlx::query("DELETE FROM pyo.clusters WHERE id = $1")
            .bind(e1)
            .execute(&s.db)
            .await
            .unwrap();
        sync_names(&s, Some(&team)).await.unwrap();
        let got = pending(&s.db, e2).await;
        let gone = got
            .iter()
            .find(|g| g.3["cluster"] == json!(e1))
            .expect("e2 takes e1's server off their tailnet");
        assert_eq!(gone.3["forget"], json!(["nA"]));
        assert!(
            pending(&s.db, h)
                .await
                .iter()
                .all(|g| g.3["cluster"] != json!(e1)),
            "not the holder"
        );

        // e2 goes too: no one keeps a client there now.
        sqlx::query("DELETE FROM pyo.clusters WHERE id = $1")
            .bind(e2)
            .execute(&s.db)
            .await
            .unwrap();
        sync_names(&s, Some(&team)).await.unwrap();
        sqlx::query("UPDATE pyo.tailnet_leftovers SET created_at = now() - interval '1 hour' WHERE team_id = $1")
            .bind(&team)
            .execute(&s.db)
            .await
            .unwrap();
        let tt = team_tailnet(&s.db, &team).await.unwrap().unwrap();
        let mut c = s.db.acquire().await.unwrap();
        let v = team_view(&mut c, &tt, &[]).await.unwrap();
        let mut left: Vec<String> = v["leftovers"]
            .as_array()
            .unwrap()
            .iter()
            .map(|l| format!("{} {}", l["cluster"].as_str().unwrap(), l["servers"]))
            .collect();
        left.sort();
        assert_eq!(
            left,
            [format!("{e1} [\"web-1\"]"), format!("{e2} [\"web-1\"]")]
        );
    }
}
