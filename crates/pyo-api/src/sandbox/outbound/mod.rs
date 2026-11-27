//! Sandbox servers that work over HTTPS: a hosted Pyo's (Pyo Cloud), which
//! is on no team's tailnet, so it can neither take their join at its
//! tailnet door nor call their daemons. See docs/cloud-sandbox-servers.md.
//!
//! 1. A team owner or admin makes a join code (`createSandboxJoinCode`):
//!    Pyo makes the server's row (joining, `outbound`), a one-time code
//!    (kept as its hash, 15 minutes, burnt on first use) and a single-use
//!    Tailscale key from the team's holder cluster, and shows one command.
//!    It carries Pyo's command key too.
//! 2. sandbox.sh joins the team's tailnet with the key and installs the
//!    daemon. The daemon makes an Ed25519 key and posts its public half
//!    with the code to `POST /api/sandbox-agent/v1/join` over HTTPS
//!    ([`join`]). Pyo checks the code and that the server is a `tag:pyo`
//!    machine of the team's tailnet, keeps the key and answers with the
//!    daemon's config.
//! 3. The daemon long-polls for its commands and posts their results and
//!    its report, every request signed with its key ([`channel`]). The key
//!    is good for a day and renewed at half that. **Reset access**
//!    ([`reset_access`]) revokes it and makes a new code for the same
//!    server. Removing the server deletes its keys.
//!
//! A self-hosted Pyo's servers work the same way from 0.51.1 ([`add`],
//! docs/cloud-sandbox-servers.md "Self-hosted"): its own signer
//! ([`signer::Local`]), its tailnet address or public address for the
//! daemon ([`self_hosted_reach`]), and the installation's servers (no
//! team) too. Servers that joined at its door before take no commands
//! from 0.52.0 (`super::NEEDS_UPDATE`) until **Update** ([`reset_access`])
//! moves them over.

pub mod channel;
pub mod signer;
#[cfg(test)]
pub(crate) mod e2e;
#[cfg(test)]
mod self_hosted_tests;
#[cfg(test)]
pub(crate) mod tests;

use std::time::Duration;

use axum::http::StatusCode;
use pyo_proto::sandbox::{CapabilityReport, Runtime, outbound as wire};
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::{PgConnection, PgPool};

use super::{Need, join};
use crate::{
    AppState, CommandSigner,
    auth::User,
    error::{ApiError, ApiResult},
    ids::sha256_hex,
    ratelimit::{self, Limit},
    secrets::Sealer,
};

/// A server key's lifetime. The daemon renews it at half that.
pub const KEY_HOURS: i32 = 24;
/// How long a key stays good once a new one replaced it.
pub const RENEWED_KEY_GRACE: Duration = Duration::from_secs(15 * 60);

/// Join codes a team makes (new servers and Reset access).
pub const CODES_PER_TEAM: Limit = Limit {
    scope: "sandbox-join-codes:team",
    max: 20,
    window_secs: 60 * 60,
    backoff_cap_secs: 0,
};

/// Why a hosted Pyo can't take a team's sandbox servers yet.
pub const TAILSCALE_NEEDED: &str =
    "Sandbox servers join your tailnet. Set up Tailscale on Integrations first.";
/// Why there's no join for a server here.
pub const SELF_HOSTED: &str = "This Pyo adds sandbox servers with addSandboxServer.";
/// Where a server that joined over HTTPS answers browsers and SDKs on the
/// team's tailnet: HTTPS on its tailnet name, with the certificate its
/// `tailscaled` gets (sandbox.sh).
pub const TAILNET_PORT: u16 = wire::TAILNET_PORT;

/// That address for a server's tailnet name (`sbx-1.tail1234.ts.net`).
pub fn tailnet_url(tailnet_name: &str) -> String {
    format!(
        "https://{}:{TAILNET_PORT}",
        tailnet_name.trim_end_matches('.')
    )
}

/// Why commands can't be signed.
pub const NO_SIGNER: &str = "Pyo can't sign commands for sandbox servers right now.";

#[derive(Debug, Clone, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CreateJoinCode {
    /// The team whose server it is. It needs Tailscale (Integrations).
    pub team_id: String,
    /// Its name in Pyo: lowercase letters, digits and dashes. A server of
    /// that name that hasn't joined yet gets a new code.
    pub name: String,
    /// A domain whose wildcard points at the server, as for
    /// `addSandboxServer`.
    #[serde(default)]
    pub edge_domain: Option<String>,
}

/// A server waiting to join, and what its command carries.
#[derive(Debug, Clone)]
pub struct Joining {
    pub server_id: String,
    /// None: the installation's server (self-hosted only).
    pub team_id: Option<String>,
    pub name: String,
    pub code: String,
    pub tailscale_key: String,
    /// The keys its commands are signed with (base64url, comma separated).
    pub command_keys: String,
    /// Where the daemon reaches Pyo.
    pub pyo_url: String,
    pub edge_domain: Option<String>,
    /// When the code runs out (ms since the epoch).
    pub expires_at: u64,
}

/// Single-quoted for the shell.
fn q(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

impl Joining {
    /// The command that makes a machine this sandbox server (sandbox.sh's
    /// outbound mode: `--pyo-url` instead of `--pyo`).
    pub fn command(&self, base: &str, version: &str) -> String {
        let mut c = format!(
            "curl -fsSL {base}/sandbox.sh | bash -s -- --pyo-url {} --server {} --name {} --code {} --tailscale-key {} --command-key {} --version {}",
            q(&self.pyo_url),
            q(&self.server_id),
            q(&self.name),
            q(&self.code),
            q(&self.tailscale_key),
            q(&self.command_keys),
            q(version),
        );
        if let Some(d) = &self.edge_domain {
            c.push_str(&format!(" --edge-domain {}", q(d)));
        }
        c
    }
}

/// The team's tailnet, ready to hand out a key: its holder, connected.
async fn team_holder(db: &PgPool, team_id: &str) -> ApiResult<String> {
    let t = crate::tailscale::team_tailnet(db, team_id)
        .await?
        .ok_or_else(|| ApiError::bad_request(TAILSCALE_NEEDED))?;
    let Some(holder) = t.holder else {
        return Err(ApiError::bad_request(crate::tailscale::HOLDER_GONE));
    };
    if !t.holder_connected {
        return Err(ApiError::conflict(format!(
            "{} keeps your Tailscale and isn't connected.",
            t.holder_name
        )));
    }
    if t.suffix.is_none() {
        return Err(ApiError::conflict(
            "Pyo hasn't read your tailnet yet. Try again in a minute.",
        ));
    }
    Ok(holder)
}

/// A new code and a Tailscale key for a server already in `tx`.
async fn code_and_key(
    s: &AppState,
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    holder: &str,
    server_id: &str,
    name: &str,
) -> ApiResult<(String, String)> {
    let code = join::new_code();
    join::keep(tx, server_id, &code).await?;
    let key = crate::tailscale::join_key(
        s,
        holder,
        &format!("Pyo: sandbox server {name}"),
        "a sandbox server",
    )
    .await?;
    Ok((code, key))
}

fn expires_at() -> u64 {
    pyo_proto::now_ms() as u64 + join::CODE_TTL.as_millis() as u64
}

/// Adds a team's sandbox server that joins over HTTPS, or makes a new code
/// for one that hasn't joined yet.
pub async fn create_join_code(
    s: &AppState,
    signer: &dyn CommandSigner,
    user: &User,
    req: CreateJoinCode,
) -> ApiResult<Joining> {
    crate::util::require_dns_label(&req.name, "Server name")?;
    let edge = super::edge_domain(req.edge_domain.as_deref())?;
    let mut tx = crate::rls::begin(&s.db, user).await?;
    super::require(&mut tx, user, &req.team_id, Need::Manage).await?;
    if !s.sealer.enabled() {
        return Err(ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "This installation has no PYO_SECRET_KEY, so it can't sign sandbox tokens.",
        ));
    }
    let command_keys = signer.public_keys();
    if command_keys.trim().is_empty() {
        return Err(ApiError::new(StatusCode::SERVICE_UNAVAILABLE, NO_SIGNER));
    }
    let holder = team_holder(&s.db, &req.team_id).await?;
    ratelimit::take(&s.db, &[(CODES_PER_TEAM, req.team_id.as_str())]).await?;
    let existing: Option<(String, String, bool)> = sqlx::query_as(
        "SELECT id, state, outbound FROM pyo.sandbox_servers WHERE team_id = $1 AND name = $2",
    )
    .bind(&req.team_id)
    .bind(&req.name)
    .fetch_optional(&mut *tx)
    .await?;
    let id = match existing {
        Some((id, state, true)) if state == "joining" || state == "refused" => {
            sqlx::query("UPDATE pyo.sandbox_servers SET state = 'joining', error = NULL, edge_domain = $2, via_cluster = $3, pyo_url = $4, updated_at = now() WHERE id = $1")
                .bind(&id)
                .bind(&edge)
                .bind(&holder)
                .bind(&s.config.agent_api_url)
                .execute(&mut *tx)
                .await?;
            id
        }
        Some(_) => {
            return Err(ApiError::conflict(
                "A sandbox server has that name already.",
            ));
        }
        None => {
            let id = crate::ids::new_id("sbs");
            sqlx::query(
                "INSERT INTO pyo.sandbox_servers (id, team_id, name, state, created_by, edge_domain, outbound, via_cluster, pyo_url)
                 VALUES ($1, $2, $3, 'joining', $4, $5, true, $6, $7)",
            )
            .bind(&id)
            .bind(&req.team_id)
            .bind(&req.name)
            .bind(&user.name)
            .bind(&edge)
            .bind(&holder)
            .bind(&s.config.agent_api_url)
            .execute(&mut *tx)
            .await?;
            id
        }
    };
    let (code, tailscale_key) = code_and_key(s, &mut tx, &holder, &id, &req.name).await?;
    crate::ops::log_activity(
        &mut tx,
        &req.team_id,
        &user.name,
        "sandbox.server",
        "info",
        &format!("Added the sandbox server {}", req.name),
        json!({ "kind": "sandboxServer", "id": id, "label": req.name }),
        json!({}),
    )
    .await?;
    tx.commit().await?;
    Ok(Joining {
        server_id: id,
        team_id: Some(req.team_id),
        name: req.name,
        code,
        tailscale_key,
        command_keys,
        pyo_url: s.config.agent_api_url.clone(),
        edge_domain: edge,
        expires_at: expires_at(),
    })
}

/// Reset access: the server's keys stop working at once, its waiting
/// commands fail, and it joins again with a new code. Its sandboxes keep
/// their records.
///
/// On a self-hosted Pyo it's **Update** too, for a server that joined at
/// the door before: a command that moves it to this channel. It takes no
/// commands until it runs there (`super::NEEDS_UPDATE`). Its sandboxes
/// keep running.
pub async fn reset_access(
    s: &AppState,
    signer: &dyn CommandSigner,
    user: &User,
    id: &str,
    mode: &Mode,
) -> ApiResult<Joining> {
    let hosted = matches!(mode, Mode::Hosted);
    // The installation's servers show to everyone (row security), and
    // others' don't: not found.
    let mut tx = crate::rls::begin(&s.db, user).await?;
    let row: Option<(Option<String>, String, bool, Option<String>)> = sqlx::query_as(
        "SELECT team_id, name, outbound, edge_domain FROM pyo.sandbox_servers WHERE id = $1",
    )
    .bind(id)
    .fetch_optional(&mut *tx)
    .await?;
    let (team, name, outbound, edge) = row.ok_or_else(|| ApiError::not_found("Sandbox server"))?;
    if hosted && (!outbound || team.is_none()) {
        return Err(ApiError::conflict(
            "Only a sandbox server that joined over HTTPS has access to reset.",
        ));
    }
    match &team {
        Some(t) => super::require(&mut tx, user, t, Need::Manage).await?,
        None => {
            super::require_install_admin(user)?;
            // Its row is no team's to change: outside the scope.
            tx.rollback().await?;
            tx = s.db.begin().await?;
        }
    }
    if !s.sealer.enabled() {
        return Err(ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "This installation has no PYO_SECRET_KEY, so it can't sign sandbox tokens.",
        ));
    }
    let command_keys = signer.public_keys();
    if command_keys.trim().is_empty() {
        return Err(ApiError::new(StatusCode::SERVICE_UNAVAILABLE, NO_SIGNER));
    }
    let (via, pyo_url) = match (mode, &team) {
        (Mode::SelfHosted(at), _) => self_hosted_reach(&s.db, team.as_deref(), at).await?,
        (Mode::Hosted, Some(t)) => (team_holder(&s.db, t).await?, s.config.agent_api_url.clone()),
        (Mode::Hosted, None) => return Err(ApiError::not_found("Sandbox server")),
    };
    let limit_key = signer::claims_team(team.as_deref()).to_owned();
    ratelimit::take(&s.db, &[(CODES_PER_TEAM, limit_key.as_str())]).await?;
    if outbound {
        revoke(&mut tx, id, "Its access was reset.").await?;
        sqlx::query("UPDATE pyo.sandbox_servers SET state = 'joining', error = NULL, updated_at = now() WHERE id = $1")
            .bind(id)
            .execute(&mut *tx)
            .await?;
    }
    // A server that joined at the door keeps its cluster until it joins
    // here. One without takes the key's.
    sqlx::query(
        "UPDATE pyo.sandbox_servers SET via_cluster = CASE WHEN outbound THEN $2 ELSE coalesce(via_cluster, $2) END,
                pyo_url = $3 WHERE id = $1",
    )
    .bind(id)
    .bind(&via)
    .bind(&pyo_url)
    .execute(&mut *tx)
    .await?;
    let (code, tailscale_key) = code_and_key(s, &mut tx, &via, id, &name).await?;
    if let Some(t) = &team {
        let title = if outbound {
            format!("Reset the access of the sandbox server {name}")
        } else {
            format!("Made the command that updates the sandbox server {name}")
        };
        crate::ops::log_activity(
            &mut tx,
            t,
            &user.name,
            "sandbox.server",
            "info",
            &title,
            json!({ "kind": "sandboxServer", "id": id, "label": name }),
            json!({}),
        )
        .await?;
    }
    tx.commit().await?;
    s.wake(id).await;
    Ok(Joining {
        server_id: id.to_owned(),
        team_id: team,
        name,
        code,
        tailscale_key,
        command_keys,
        pyo_url,
        edge_domain: edge,
        expires_at: expires_at(),
    })
}

// ── Self-hosted ────────────────────────────────────────────────────────────

/// Why a team's server on a tailnet of its own can't reach Pyo.
pub const NOT_REACHABLE: &str =
    "Pyo isn't reachable from your team's tailnet. Turn on Public dashboard on the Tailscale card, then add the server again.";
/// Why there's no address on Pyo's tailnet yet.
pub const NOT_ON_TAILNET: &str = "Pyo's server isn't on your tailnet yet. Try again in a minute.";

/// Which Pyo this is, for the calls that differ.
#[derive(Debug, Clone)]
pub enum Mode {
    /// Pyo Cloud: its build's signer, `PYO_AGENT_API_URL`, team servers.
    Hosted,
    /// A self-hosted Pyo, and where it answers.
    SelfHosted(Where),
}

impl Mode {
    pub fn now() -> Mode {
        if crate::hosted() {
            Mode::Hosted
        } else {
            Mode::SelfHosted(Where::now())
        }
    }
}

/// Where a self-hosted Pyo answers, for [`pick_pyo_url`].
#[derive(Debug, Clone, Default)]
pub struct Where {
    /// Its full name on its tailnet.
    pub pyo_name: Option<String>,
    /// Pyo on its tailnet: `https://<name>`, or `http://<IP>:8000`.
    pub tailnet_api: Option<String>,
    /// Its public HTTPS address, when it has one.
    pub public: Option<String>,
}

impl Where {
    pub fn now() -> Where {
        let a = crate::web::addresses();
        Where {
            pyo_name: a.tailnet.clone().or(a.host_name.clone()),
            tailnet_api: crate::web::tailnet_api_url(),
            public: std::env::var("PYO_PUBLIC_API_URL")
                .ok()
                .filter(|u| !u.trim().is_empty())
                .or_else(|| a.public.as_ref().map(|h| format!("https://{h}"))),
        }
    }
}

/// The tailnet suffix of a full tailnet name: `tail1234.ts.net` of
/// `pyo.tail1234.ts.net`.
fn suffix_of(name: &str) -> Option<String> {
    let name = name.trim().trim_end_matches('.').to_ascii_lowercase();
    name.split_once('.').map(|(_, s)| s.to_owned()).filter(|s| s.contains('.'))
}

/// Where a self-hosted Pyo's new sandbox server reaches it, first of:
/// Pyo's tailnet address when the server joins Pyo's own tailnet (its
/// tailnet's suffix is Pyo's, or unknown), else Pyo's public HTTPS
/// address. `pyo_name`: Pyo's full name on its tailnet. `tailnet_api`:
/// Pyo on its tailnet (`https://<name>`, or `http://<IP>:8000`).
pub fn pick_pyo_url(
    server_suffix: Option<&str>,
    pyo_name: Option<&str>,
    tailnet_api: Option<&str>,
    public: Option<&str>,
) -> Result<String, &'static str> {
    let pyo_suffix = pyo_name.and_then(suffix_of);
    let ours = match (server_suffix, pyo_suffix.as_deref()) {
        (Some(a), Some(b)) => a.trim().trim_end_matches('.').eq_ignore_ascii_case(b),
        _ => true,
    };
    let public = public
        .map(|u| u.trim().trim_end_matches('/'))
        .filter(|u| u.starts_with("https://") && u.len() > "https://".len());
    match (ours, tailnet_api, public) {
        (true, Some(api), _) => Ok(api.trim().trim_end_matches('/').to_owned()),
        (_, _, Some(p)) => Ok(p.to_owned()),
        (true, None, None) => Err(NOT_ON_TAILNET),
        (false, _, None) => Err(NOT_REACHABLE),
    }
}

/// For a self-hosted Pyo's server of `team` (None: the installation's):
/// the cluster whose Tailscale makes its key (`via_cluster`), and where
/// the daemon reaches Pyo ([`pick_pyo_url`]).
pub async fn self_hosted_reach(db: &PgPool, team: Option<&str>, at: &Where) -> ApiResult<(String, String)> {
    let via = crate::tailscale::team_key_cluster(db, team).await?;
    let via_team: Option<String> =
        sqlx::query_scalar("SELECT team_id FROM pyo.clusters WHERE id = $1")
            .bind(&via)
            .fetch_optional(db)
            .await?;
    let suffix = match via_team {
        Some(t) => crate::tailscale::team_tailnet(db, &t).await?.and_then(|t| t.suffix),
        None => None,
    };
    let url = pick_pyo_url(
        suffix.as_deref(),
        at.pyo_name.as_deref(),
        at.tailnet_api.as_deref(),
        at.public.as_deref(),
    )
    .map_err(ApiError::conflict)?;
    Ok((via, url))
}

/// A self-hosted Pyo's new sandbox server (`addSandboxServer`), or a new
/// code for one that hasn't joined yet: a team's, or the installation's
/// (None, its admin's). It joins over the channel.
pub async fn add(
    s: &AppState,
    signer: &dyn CommandSigner,
    user: &User,
    team: Option<&str>,
    name: &str,
    edge: Option<String>,
    at: &Where,
) -> ApiResult<Joining> {
    let mut tx = s.db.begin().await?;
    match team {
        Some(t) => super::require(&mut tx, user, t, Need::Manage).await?,
        None => super::require_install_add(user)?,
    }
    if !s.sealer.enabled() {
        return Err(ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "This installation has no PYO_SECRET_KEY, so it can't sign sandbox tokens.",
        ));
    }
    let command_keys = signer.public_keys();
    if command_keys.trim().is_empty() {
        return Err(ApiError::new(StatusCode::SERVICE_UNAVAILABLE, NO_SIGNER));
    }
    let (via, pyo_url) = self_hosted_reach(&s.db, team, at).await?;
    ratelimit::take(&s.db, &[(CODES_PER_TEAM, signer::claims_team(team))]).await?;
    let existing: Option<(String, String)> = sqlx::query_as(
        "SELECT id, state FROM pyo.sandbox_servers WHERE coalesce(team_id, '') = coalesce($1, '') AND name = $2",
    )
    .bind(team)
    .bind(name)
    .fetch_optional(&mut *tx)
    .await?;
    let id = match existing {
        Some((id, state)) if state == "joining" || state == "refused" => {
            sqlx::query(
                "UPDATE pyo.sandbox_servers SET state = 'joining', error = NULL, edge_domain = $2, outbound = true,
                        via_cluster = $3, pyo_url = $4, updated_at = now() WHERE id = $1",
            )
            .bind(&id)
            .bind(&edge)
            .bind(&via)
            .bind(&pyo_url)
            .execute(&mut *tx)
            .await?;
            id
        }
        Some(_) => {
            return Err(ApiError::conflict(
                "A sandbox server has that name already.",
            ));
        }
        None => {
            let id = crate::ids::new_id("sbs");
            sqlx::query(
                "INSERT INTO pyo.sandbox_servers (id, team_id, name, state, created_by, edge_domain, outbound, via_cluster, pyo_url)
                 VALUES ($1, $2, $3, 'joining', $4, $5, true, $6, $7)",
            )
            .bind(&id)
            .bind(team)
            .bind(name)
            .bind(&user.name)
            .bind(&edge)
            .bind(&via)
            .bind(&pyo_url)
            .execute(&mut *tx)
            .await?;
            id
        }
    };
    let (code, tailscale_key) = code_and_key(s, &mut tx, &via, &id, name).await?;
    if let Some(t) = team {
        crate::ops::log_activity(
            &mut tx,
            t,
            &user.name,
            "sandbox.server",
            "info",
            &format!("Added the sandbox server {name}"),
            json!({ "kind": "sandboxServer", "id": id, "label": name }),
            json!({}),
        )
        .await?;
    }
    tx.commit().await?;
    Ok(Joining {
        server_id: id,
        team_id: team.map(str::to_owned),
        name: name.to_owned(),
        code,
        tailscale_key,
        command_keys,
        pyo_url,
        edge_domain: edge,
        expires_at: expires_at(),
    })
}

/// The server's keys stop working, and its commands that wait or are out
/// fail with `why`.
pub async fn revoke(conn: &mut PgConnection, server_id: &str, why: &str) -> sqlx::Result<()> {
    sqlx::query("UPDATE pyo.sandbox_server_keys SET revoked_at = now() WHERE server_id = $1 AND revoked_at IS NULL")
        .bind(server_id)
        .execute(&mut *conn)
        .await?;
    channel::fail_open(conn, server_id, why).await
}

// ── The join ───────────────────────────────────────────────────────────────

/// Why a join was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// As on a self-hosted Pyo's door.
    Door(join::Refusal),
    /// It asked with a key that isn't an Ed25519 public key.
    BadKey,
    /// It's on another tailnet than its team's.
    OtherTailnet,
    /// The team's tailnet doesn't list it as one of Pyo's machines yet.
    /// The code still works: the daemon asks again in a few seconds.
    NotSeen,
}

impl Refusal {
    pub fn status(&self) -> StatusCode {
        match self {
            Refusal::Door(join::Refusal::NoRuntime(_)) => StatusCode::UNPROCESSABLE_ENTITY,
            Refusal::BadKey => StatusCode::BAD_REQUEST,
            Refusal::NotSeen => StatusCode::CONFLICT,
            _ => StatusCode::FORBIDDEN,
        }
    }

    pub fn message(&self) -> String {
        match self {
            Refusal::Door(r) => r.message(),
            Refusal::BadKey => "That isn't an Ed25519 public key.".into(),
            Refusal::OtherTailnet => {
                "This server isn't on your team's tailnet. Join it with the key in the command.".into()
            }
            Refusal::NotSeen => {
                "Your tailnet doesn't list this server yet. Pyo checks again in a few seconds.".into()
            }
        }
    }

    fn error(&self) -> ApiError {
        ApiError::new(self.status(), self.message())
    }
}

/// A code as found, before it's burnt.
#[derive(Debug, sqlx::FromRow)]
struct Found {
    server_id: String,
    expired: bool,
    used: bool,
    state: Option<String>,
    team_id: Option<String>,
    outbound: Option<bool>,
    /// Where it reaches Pyo, as its command said. On a server that joined
    /// at the door: set by **Update**, which lets it move here.
    pyo_url: Option<String>,
    /// The team whose tailnet its key came from.
    tailnet_team: Option<String>,
    suffix: Option<String>,
    machines: Option<Value>,
}

impl Found {
    /// Whether the server may join now: an outbound server that waits for
    /// it, or (self-hosted) one that joined at the door before and has
    /// its **Update** command. A hosted Pyo's server always has a team.
    fn joinable(&self, hosted: bool) -> bool {
        let state = self.state.as_deref();
        let ok = match self.outbound {
            Some(true) => state == Some("joining"),
            Some(false) => !hosted && self.pyo_url.is_some() && state.is_some_and(|s| s != "refused"),
            None => false,
        };
        ok && (self.team_id.is_some() || !hosted)
    }
}

/// A full tailnet name, `<host>.<suffix>`, on the team's tailnet.
fn on_tailnet(tailnet_name: &str, suffix: &str) -> bool {
    let name = tailnet_name.trim().trim_end_matches('.').to_ascii_lowercase();
    let suffix = suffix.trim().trim_end_matches('.').to_ascii_lowercase();
    match name.strip_suffix(&suffix).and_then(|h| h.strip_suffix('.')) {
        Some(host) => {
            !suffix.is_empty()
                && !host.is_empty()
                && !host.contains('.')
                && host.len() <= 63
                && host.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
        }
        None => false,
    }
}

/// Whether the team's tailnet lists `hostname` among Pyo's machines
/// (`tag:pyo`), as its holder last synced them.
fn listed(machines: Option<&Value>, hostname: &str) -> bool {
    let want = hostname.trim();
    !want.is_empty()
        && machines
            .and_then(Value::as_array)
            .is_some_and(|m| {
                m.iter()
                    .any(|x| x["hostname"].as_str().is_some_and(|h| h.eq_ignore_ascii_case(want)))
            })
}

/// Checks a found code against the ask: Ok when it may join now. Every
/// refusal but [`Refusal::NotSeen`] burns the code.
fn check(f: &Found, ask: &wire::JoinRequest, hosted: bool) -> Result<(), Refusal> {
    if f.used {
        return Err(Refusal::Door(join::Refusal::Used));
    }
    if f.expired {
        return Err(Refusal::Door(join::Refusal::Expired));
    }
    if f.server_id != ask.server_id {
        return Err(Refusal::Door(join::Refusal::WrongServer));
    }
    if !f.joinable(hosted) {
        return Err(Refusal::Door(join::Refusal::Gone));
    }
    if !pyo_proto::agentauth::valid_public_key(&ask.public_key) {
        return Err(Refusal::BadKey);
    }
    if !join::tailnet_address(ask.address.trim()) {
        return Err(Refusal::Door(join::Refusal::BadAddress));
    }
    if let Some(c) = &ask.capability
        && c.runtime == Runtime::None
    {
        return Err(Refusal::Door(join::Refusal::NoRuntime(c.summary())));
    }
    let Some(suffix) = f.suffix.as_deref() else {
        return Err(Refusal::NotSeen);
    };
    if !on_tailnet(&ask.tailnet_name, suffix) {
        return Err(Refusal::OtherTailnet);
    }
    if !listed(f.machines.as_ref(), &ask.hostname) {
        return Err(Refusal::NotSeen);
    }
    Ok(())
}

/// Burns a code. False when it was burnt already.
async fn burn(conn: &mut PgConnection, hash: &str) -> sqlx::Result<bool> {
    Ok(sqlx::query("UPDATE pyo.sandbox_join_codes SET used_at = now() WHERE code_hash = $1 AND used_at IS NULL")
        .bind(hash)
        .execute(conn)
        .await?
        .rows_affected()
        == 1)
}

/// A new key for a server, good for [`KEY_HOURS`]: its id, and when it ends
/// (ms since the epoch).
pub(crate) async fn add_key(
    conn: &mut PgConnection,
    server_id: &str,
    team_id: Option<&str>,
    public_key: &str,
) -> sqlx::Result<(String, i64)> {
    let id = crate::ids::new_id("sbsk");
    let not_after: i64 = sqlx::query_scalar(
        "INSERT INTO pyo.sandbox_server_keys (id, server_id, team_id, public_key, expires_at)
         VALUES ($1, $2, $3, $4, now() + make_interval(hours => $5))
         RETURNING (extract(epoch FROM expires_at) * 1000)::bigint",
    )
    .bind(&id)
    .bind(server_id)
    .bind(team_id)
    .bind(public_key.trim())
    .bind(KEY_HOURS)
    .fetch_one(conn)
    .await?;
    Ok((id, not_after))
}

/// Checks a join and registers the server's key: the daemon's config, or
/// why not. `command_keys`: the keys its commands are signed with.
/// `pyo_url`: where the daemon reaches Pyo, unless its row says.
pub async fn join(
    db: &PgPool,
    sealer: &Sealer,
    command_keys: &str,
    pyo_url: &str,
    hosted: bool,
    ask: &wire::JoinRequest,
) -> ApiResult<wire::JoinAnswer> {
    let fits = |s: &str| {
        !s.is_empty() && s.len() <= 64 && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
    };
    let code = ask.code.trim();
    if !fits(&ask.server_id)
        || code.is_empty()
        || code.len() > 256
        || ask.hostname.len() > 253
        || ask.tailnet_name.len() > 253
    {
        return Err(ApiError::bad_request(
            "Ask with a server, a code, a key and the server's tailnet address and names.",
        ));
    }
    if command_keys.trim().is_empty() {
        return Err(ApiError::new(StatusCode::SERVICE_UNAVAILABLE, NO_SIGNER));
    }
    let limits = [
        (join::WRONG_CODE_ALL, "all"),
        (join::WRONG_CODE_SERVER, ask.server_id.as_str()),
    ];
    ratelimit::take(db, &limits).await?;
    let hash = sha256_hex(code);
    let found: Option<Found> = sqlx::query_as(
        "SELECT c.server_id, c.expires_at <= now() AS expired, c.used_at IS NOT NULL AS used,
                s.state, s.team_id, s.outbound, s.pyo_url, t.team_id AS tailnet_team, t.suffix, t.machines
           FROM pyo.sandbox_join_codes c
           LEFT JOIN pyo.sandbox_servers s ON s.id = c.server_id
           LEFT JOIN pyo.clusters vc ON vc.id = s.via_cluster AND NOT $2
           LEFT JOIN pyo.team_tailscale t ON t.team_id = coalesce(vc.team_id, s.team_id)
          WHERE c.code_hash = $1",
    )
    .bind(&hash)
    .bind(hosted)
    .fetch_optional(db)
    .await?;
    let Some(found) = found else {
        tracing::warn!(server = %ask.server_id, "sandbox server join refused: unknown code");
        return Err(Refusal::Door(join::Refusal::Unknown).error());
    };
    // A code Pyo made: not a guess.
    for (l, k) in &limits {
        let _ = ratelimit::forgive(db, l, k).await;
    }
    if let Err(r) = check(&found, ask, hosted) {
        tracing::warn!(server = %ask.server_id, "sandbox server join refused: {r:?}");
        match &r {
            Refusal::NotSeen => {
                if let Some(team) = &found.tailnet_team {
                    let _ = crate::tailscale::refresh_machines(db, team).await;
                }
            }
            Refusal::Door(join::Refusal::Used) => {}
            _ => {
                let mut c = db.acquire().await?;
                burn(&mut c, &hash).await?;
                if let Refusal::Door(join::Refusal::NoRuntime(why)) = &r {
                    sqlx::query(
                        "UPDATE pyo.sandbox_servers SET state = 'refused', error = $2, capability = $3, updated_at = now()
                          WHERE id = $1 AND state = 'joining' AND outbound",
                    )
                    .bind(&found.server_id)
                    .bind(why)
                    .bind(json!(ask.capability))
                    .execute(&mut *c)
                    .await?;
                }
            }
        }
        return Err(r.error());
    }
    let team = found.team_id.clone();
    let claims_team = signer::claims_team(team.as_deref()).to_owned();
    let keys = super::keys::verifying_keys(db, sealer).await?;
    let mut tx = db.begin().await?;
    if !burn(&mut tx, &hash).await? {
        return Err(Refusal::Door(join::Refusal::Used).error());
    }
    // Still waiting for it, in the same team, now that the code is ours.
    let still: Option<String> = sqlx::query_scalar(
        "SELECT id FROM pyo.sandbox_servers WHERE id = $1 AND team_id IS NOT DISTINCT FROM $2
            AND ((outbound AND state = 'joining') OR (NOT outbound AND NOT $3 AND pyo_url IS NOT NULL AND state <> 'refused'))
            FOR UPDATE",
    )
    .bind(&found.server_id)
    .bind(&team)
    .bind(hosted)
    .fetch_optional(&mut *tx)
    .await?;
    if still.is_none() {
        tx.commit().await?;
        return Err(Refusal::Door(join::Refusal::Gone).error());
    }
    // One server, one daemon: a new join ends the keys before it.
    revoke(&mut tx, &found.server_id, "The server joined again.").await?;
    let (key_id, not_after) =
        add_key(&mut tx, &found.server_id, team.as_deref(), &ask.public_key).await?;
    let address = ask.address.trim();
    let tailnet_name = ask.tailnet_name.trim().trim_end_matches('.').to_ascii_lowercase();
    // A server that joined at the door before works over this channel
    // from now on, and takes commands again.
    sqlx::query(
        "UPDATE pyo.sandbox_servers SET address = $2, tailnet_name = $3, capability = coalesce($4, capability),
                outbound = true, joined_at = now(), error = NULL, updated_at = now() WHERE id = $1",
    )
    .bind(&found.server_id)
    .bind(address)
    .bind(&tailnet_name)
    .bind(ask.capability.as_ref().map(|c: &CapabilityReport| json!(c)))
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    tracing::info!(server = %found.server_id, team = %claims_team, key = %key_id, "sandbox server joined over HTTPS");
    let mut config = join::daemon_config(&found.server_id, address, ask.jail_uid, ask.jail_gid, &keys);
    config["outbound"] = json!({
        "pyoUrl": found.pyo_url.as_deref().filter(|u| !u.is_empty()).unwrap_or(pyo_url),
        "teamId": claims_team,
        "tailnetName": tailnet_name,
    });
    Ok(wire::JoinAnswer {
        server_id: found.server_id,
        team_id: claims_team,
        key_id,
        not_after,
        command_keys: command_keys.to_owned(),
        config,
    })
}
