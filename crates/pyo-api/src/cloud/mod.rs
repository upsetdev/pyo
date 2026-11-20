//! Servers Pyo makes at a cloud provider and joins to a cluster over the
//! tailnet (Add server › Hetzner, Add cluster › Hetzner), or sets up as a
//! sandbox server (Sandboxes › Add server › Cloud provider).
//!
//! - **The connection** is a team's (Integrations › Hetzner, team admins):
//!   clusters belong to a team, and the servers it makes are billed to the
//!   token's project, so the team that grows its clusters pays for them. On
//!   a Pyo serving other people's teams, one team can never spend another's
//!   money. The token is sealed with the team's data key (vault.rs) in
//!   `pyo.integrations`.
//! - **Making a server**: its user data holds only two one-time values, a
//!   single-use Tailscale key and a single-use code ([`user_data`]). On its
//!   first boot it joins the tailnet, trades the code for its join details
//!   over the tailnet ([`bootstrap`]), and runs join.sh (or connect.sh for
//!   a new cluster) with them, or sandbox.sh for a sandbox server. The
//!   server carries this installation's label (`pyo=<install id>`, and
//!   `pyo-cluster=<cluster>` or `pyo-sandbox-server=<id>`) and Pyo's
//!   firewall: the web from anywhere and Tailscale's port, nothing else.
//! - **Following it** ([`tick`]): Creating → Booting → Joining → Ready,
//!   with the Tailnet Lock sign command when its key waits for a signature.
//!   After [`TIMEOUT`] it stops waiting and says why.
//! - **Deleting it**: drained and taken out of the cluster first, then off
//!   the tailnet, then deleted at the provider, only when it carries this
//!   installation's label ([`delete`]).
//!
//! [`Provider`] is what each provider does. Only Hetzner for now.

pub mod bootstrap;
pub mod hetzner;

use crate::error::Json;
use crate::error::Query;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::extract::State;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::PgPool;

use crate::{
    AppState,
    auth::SessionUser,
    error::{ApiError, ApiResult},
    ops::{Role, require_role},
};

/// The label every server and firewall Pyo makes carries: this
/// installation's ID.
pub const LABEL: &str = "pyo";
/// The cluster a server was made for.
pub const CLUSTER_LABEL: &str = "pyo-cluster";
/// The sandbox server a server was made as.
pub const SANDBOX_LABEL: &str = "pyo-sandbox-server";
/// How long Pyo waits for a new server to be ready.
pub const TIMEOUT: Duration = Duration::from_secs(15 * 60);
/// The providers there are.
pub const PROVIDERS: [&str; 1] = ["hetzner"];
/// Why there's no server to make yet.
pub const NEEDED: &str = "Connect Hetzner in Integrations first.";

// ── What a provider does ────────────────────────────────────────────────────

/// A place servers can be made.
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Location {
    /// "fsn1"
    pub name: String,
    pub city: String,
    pub country: String,
    pub network_zone: String,
}

/// A kind of server, and what it costs.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerType {
    /// "cx22"
    pub name: String,
    pub description: String,
    pub cores: u32,
    pub memory_gb: f64,
    pub disk_gb: u64,
    /// "shared" or "dedicated".
    pub cpu_type: String,
    /// "x86" or "arm".
    pub architecture: String,
    /// Pyo's servers run x86-64 only.
    pub supported: bool,
    /// A month in each location, before VAT, with its IPv4 address.
    pub prices: BTreeMap<String, f64>,
    /// The locations that can make it now.
    pub available: Vec<String>,
}

/// Locations, server types and prices, read live from the provider.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Catalog {
    pub locations: Vec<Location>,
    pub server_types: Vec<ServerType>,
    /// "EUR"
    pub currency: String,
}

impl Catalog {
    /// A type in a location that can be made now, and its monthly price.
    pub fn pick(&self, location: &str, server_type: &str) -> ApiResult<f64> {
        let loc = self
            .locations
            .iter()
            .find(|l| l.name == location)
            .ok_or_else(|| ApiError::bad_request(format!("Hetzner has no location {location}.")))?;
        let t = self
            .server_types
            .iter()
            .find(|t| t.name == server_type)
            .ok_or_else(|| {
                ApiError::bad_request(format!("Hetzner has no server type {server_type}."))
            })?;
        if !t.supported {
            return Err(ApiError::bad_request(format!(
                "{} is an {} server. Pyo's servers run x86-64.",
                t.name,
                if t.architecture == "arm" {
                    "Arm"
                } else {
                    "other"
                }
            )));
        }
        match t.prices.get(location) {
            Some(p) if t.available.iter().any(|a| a == location) => Ok(*p),
            _ => Err(ApiError::conflict(format!(
                "{} isn't available in {} right now. Pick another type or location.",
                t.name, loc.city
            ))),
        }
    }
}

/// A server to make.
pub struct Create<'a> {
    pub name: &'a str,
    pub location: &'a str,
    pub server_type: &'a str,
    pub user_data: &'a str,
    pub labels: BTreeMap<String, String>,
    pub firewall: Option<&'a str>,
}

/// A server at the provider.
#[derive(Debug, Clone, PartialEq)]
pub struct Remote {
    pub id: String,
    pub name: String,
    /// The provider's word: "initializing", "starting", "running"…
    pub status: String,
    pub labels: BTreeMap<String, String>,
    pub ipv4: Option<String>,
}

/// What went wrong at the provider, as people read it.
#[derive(Debug, Clone, PartialEq)]
pub enum CloudError {
    Unauthorized,
    ReadOnly,
    /// The project's server limit (new Hetzner accounts start low).
    Quota(String),
    /// The type, or the location, can't make servers now.
    Unavailable(String),
    NameTaken,
    RateLimited,
    NotFound,
    /// A server without this installation's label: never deleted.
    NotOurs,
    Network(String),
    Other {
        status: u16,
        message: String,
    },
}

impl std::fmt::Display for CloudError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unauthorized => f.write_str("Hetzner didn't accept the token. Check it, or make a new one."),
            Self::ReadOnly => f.write_str("This token can only read. Make one with Read & Write."),
            Self::Quota(_) => f.write_str("Your Hetzner project is at its server limit. New accounts start with a low limit. Ask Hetzner to raise it on the Limits page of its Console."),
            Self::Unavailable(_) => f.write_str("Hetzner can't make this server type in this location right now. Pick another type or location."),
            Self::NameTaken => f.write_str("Your Hetzner project has a server with this name. Pick another name."),
            Self::RateLimited => f.write_str("Hetzner is limiting requests. Try again in a minute."),
            Self::NotFound => f.write_str("Hetzner has no such server."),
            Self::NotOurs => f.write_str("This server doesn't carry this Pyo's label at Hetzner, so Pyo won't delete it."),
            Self::Network(e) => write!(f, "Couldn't reach Hetzner. {e}"),
            Self::Other { status, message } => write!(f, "Hetzner answered {status}: {message}"),
        }
    }
}

impl std::error::Error for CloudError {}

impl From<CloudError> for ApiError {
    fn from(e: CloudError) -> Self {
        use axum::http::StatusCode;
        let status = match &e {
            CloudError::Unauthorized | CloudError::ReadOnly => StatusCode::BAD_REQUEST,
            CloudError::Quota(_)
            | CloudError::Unavailable(_)
            | CloudError::NameTaken
            | CloudError::NotOurs => StatusCode::CONFLICT,
            CloudError::RateLimited => StatusCode::TOO_MANY_REQUESTS,
            CloudError::NotFound => StatusCode::NOT_FOUND,
            CloudError::Network(_) | CloudError::Other { .. } => StatusCode::BAD_GATEWAY,
        };
        ApiError::new(status, e.to_string())
    }
}

/// What a cloud provider does for Pyo. Kept small, so others can follow
/// Hetzner (DigitalOcean, Vultr, Linode).
pub trait Provider {
    /// "Hetzner"
    fn title(&self) -> &'static str;
    async fn locations(&self) -> Result<Vec<Location>, CloudError>;
    /// Server types and their prices, with the locations.
    async fn catalog(&self) -> Result<Catalog, CloudError>;
    /// Makes a server that runs `user_data` (cloud-init) on its first boot.
    async fn create(&self, r: &Create<'_>) -> Result<Remote, CloudError>;
    /// None once it's gone.
    async fn server(&self, id: &str) -> Result<Option<Remote>, CloudError>;
    /// Deletes a server without looking at it: use [`delete`].
    async fn delete_unchecked(&self, id: &str) -> Result<(), CloudError>;
    /// The installation's firewall (the web and Tailscale only), made the
    /// first time: its ID.
    async fn firewall(&self, install: &str) -> Result<String, CloudError>;
}

/// What a server is made for: a cluster, or a sandbox server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Owner<'a> {
    Cluster(&'a str),
    Sandbox(&'a str),
}

impl<'a> Owner<'a> {
    /// Its label at the provider.
    pub fn label(self) -> (&'static str, &'a str) {
        match self {
            Owner::Cluster(id) => (CLUSTER_LABEL, id),
            Owner::Sandbox(id) => (SANDBOX_LABEL, id),
        }
    }

    pub fn id(self) -> &'a str {
        self.label().1
    }
}

/// The labels a new server carries.
pub fn labels(install: &str, owner: Owner) -> BTreeMap<String, String> {
    let (key, value) = owner.label();
    BTreeMap::from([
        (LABEL.to_owned(), install.to_owned()),
        (key.to_owned(), value.to_owned()),
    ])
}

/// Whether Pyo may delete a server: it carries this installation's label,
/// and its cluster's or sandbox server's when one is given.
pub fn ours(r: &Remote, install: &str, owner: Option<Owner>) -> bool {
    r.labels.get(LABEL).map(String::as_str) == Some(install)
        && owner.is_none_or(|o| {
            let (key, value) = o.label();
            r.labels.get(key).map(String::as_str) == Some(value)
        })
}

/// Deletes a server, only when it carries this installation's label
/// ([`ours`]). One that's gone already is fine.
pub async fn delete<P: Provider>(
    p: &P,
    id: &str,
    install: &str,
    owner: Option<Owner<'_>>,
) -> Result<(), CloudError> {
    let Some(r) = p.server(id).await? else {
        return Ok(());
    };
    if !ours(&r, install, owner) {
        return Err(CloudError::NotOurs);
    }
    p.delete_unchecked(id).await
}

// ── Cloud-init ──────────────────────────────────────────────────────────────

/// An OpenSSH public key on one line: its type, its key, and a comment.
pub fn valid_ssh_key(k: &str) -> bool {
    const TYPES: [&str; 6] = [
        "ssh-ed25519",
        "ssh-rsa",
        "ecdsa-sha2-nistp256",
        "ecdsa-sha2-nistp384",
        "ecdsa-sha2-nistp521",
        "sk-ssh-ed25519@openssh.com",
    ];
    let mut parts = k.split_ascii_whitespace();
    let (Some(kind), Some(body)) = (parts.next(), parts.next()) else {
        return false;
    };
    k.len() <= 8192
        && k.bytes().all(|b| (0x20..0x7f).contains(&b))
        && TYPES.contains(&kind)
        && body.len() >= 32
        && body
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'+' || b == b'/' || b == b'=')
}

/// The script a new server runs on its first boot (as root, from its user
/// data): it joins the tailnet, trades its code for its join details over
/// the tailnet ([`bootstrap`]), and runs join.sh or connect.sh with them.
pub const BOOT_SCRIPT: &str = include_str!("boot.sh");
/// Where the user data puts the boot script, the key and the code: in
/// memory, root only.
pub const BOOT_DIR: &str = "/run/pyo-boot";

/// What a new server boots with.
pub struct Boot<'a> {
    /// Pyo's tailnet door, where it asks: "http://100.64.0.1:5006".
    pub pyo: &'a str,
    /// Its cloud server ID.
    pub server_id: &'a str,
    /// Its cluster, or the sandbox server it becomes.
    pub cluster_id: &'a str,
    pub role: bootstrap::Role,
    /// Its name in the cluster and on the tailnet.
    pub name: &'a str,
    /// Single-use, tagged tag:pyo, valid an hour.
    pub tailscale_key: &'a str,
    /// Single-use, valid 15 minutes ([`bootstrap::new_code`]).
    pub code: &'a str,
}

/// A value fit for the user data: one line, nothing YAML or a shell reads
/// specially.
fn plain(v: &str) -> bool {
    !v.is_empty()
        && v.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.:/".contains(&b))
}

/// The server's cloud-init user data. Its only secrets are two one-time
/// values: the Tailscale key and the code, written to root-only files in
/// memory and deleted once used. The boot script holds none. The
/// provider's metadata service, which serves the user data, is closed to
/// the server's pods at every boot, so only root on the server can read it
/// again. Password logins over SSH are off. `ssh_key` lets its owner in
/// over the tailnet.
pub fn user_data(b: &Boot, ssh_key: Option<&str>) -> ApiResult<String> {
    let values = [
        b.pyo,
        b.server_id,
        b.cluster_id,
        b.name,
        b.tailscale_key,
        b.code,
    ];
    if !values.iter().all(|v| plain(v)) {
        return Err(ApiError::conflict(
            "Pyo couldn't make this server's setup. Try again.",
        ));
    }
    let mut out = String::from(
        "#cloud-config\n# Made by Pyo: joins this server to Pyo over your tailnet.\nssh_pwauth: false\n",
    );
    if let Some(k) = ssh_key {
        // JSON's quoting is YAML's double-quoted string.
        out.push_str(&format!(
            "ssh_authorized_keys:\n  - {}\n",
            serde_json::to_string(k.trim()).unwrap_or_default()
        ));
    }
    out.push_str("bootcmd:\n  - [sh, -c, \"iptables -t raw -C PREROUTING -d 169.254.169.254/32 -j DROP 2>/dev/null || iptables -t raw -I PREROUTING -d 169.254.169.254/32 -j DROP\"]\n");
    out.push_str("write_files:\n");
    let file = |out: &mut String, name: &str, mode: &str, block: &str, body: &str| {
        out.push_str(&format!(
            "  - path: {BOOT_DIR}/{name}\n    owner: root:root\n    permissions: \"{mode}\"\n    content: {block}\n"
        ));
        for line in body.lines() {
            if line.is_empty() {
                out.push('\n');
            } else {
                out.push_str(&format!("      {line}\n"));
            }
        }
    };
    file(&mut out, "tailscale-key", "0600", "|-", b.tailscale_key);
    file(&mut out, "code", "0600", "|-", b.code);
    file(&mut out, "boot.sh", "0700", "|", BOOT_SCRIPT);
    out.push_str(&format!(
        "runcmd:\n  - [bash, {BOOT_DIR}/boot.sh, \"{}\", \"{}\", \"{}\", \"{}\", \"{}\"]\n",
        b.pyo,
        b.server_id,
        b.cluster_id,
        b.role.as_str(),
        b.name
    ));
    Ok(out)
}

// ── The connection (Integrations › Hetzner) ─────────────────────────────────

/// The team's token, opened.
pub async fn token(db: &PgPool, team_id: &str) -> ApiResult<Option<String>> {
    let sealed: Option<String> = sqlx::query_scalar(
        "SELECT secret FROM pyo.integrations WHERE team_id = $1 AND kind = 'hetzner'",
    )
    .bind(team_id)
    .fetch_optional(db)
    .await?;
    match sealed {
        Some(s) => Ok(Some(crate::vault::get().open(&s).await?)),
        None => Ok(None),
    }
}

/// The team's Hetzner, or why there's none.
pub async fn provider(db: &PgPool, team_id: &str) -> ApiResult<hetzner::Hetzner> {
    let t = token(db, team_id)
        .await?
        .ok_or_else(|| ApiError::bad_request(NEEDED))?;
    Ok(hetzner::Hetzner::new(&t)?)
}

/// Servers join over the tailnet: Tailscale comes first, on the cluster
/// they join (or, for a new cluster, the one Pyo runs on).
pub async fn needs_tailscale(db: &PgPool, cluster_id: Option<&str>) -> ApiResult<()> {
    crate::tailscale::key_cluster(db, cluster_id).await.map(|_| ())
}

/// Saves a team's token once Hetzner accepts it (it lists the locations).
pub async fn save(s: &AppState, team_id: &str, token: &str) -> ApiResult<Value> {
    let token = token.trim();
    if !hetzner::valid_token(token) {
        return Err(ApiError::bad_request(
            "Paste the API token from your Hetzner project's Security page. It's 64 letters and digits.",
        ));
    }
    let locations = hetzner::Hetzner::new(token)?.locations().await?;
    let sealed = crate::vault::get().seal(team_id, token).await?;
    if !crate::vault::Vault::is_sealed(&sealed) {
        return Err(ApiError::new(
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            "This installation has no PYO_SECRET_KEY, so it can't store the token.",
        ));
    }
    sqlx::query(
        "INSERT INTO pyo.integrations (team_id, kind, secret, hint) VALUES ($1, 'hetzner', $2, $3)
         ON CONFLICT (team_id, kind) DO UPDATE SET secret = excluded.secret, hint = excluded.hint, created_at = now()",
    )
    .bind(team_id)
    .bind(&sealed)
    .bind(format!("…{}", &token[token.len() - 4..]))
    .execute(&s.db)
    .await?;
    forget_catalog(team_id);
    Ok(json!({ "ok": true, "locations": locations }))
}

/// The Test button: the locations the saved token sees.
pub async fn test(s: &AppState, team_id: &str) -> ApiResult<Value> {
    let p = provider(&s.db, team_id).await?;
    let locations = p.locations().await?;
    Ok(json!({ "ok": true, "locations": locations }))
}

/// Forgets the token. Servers Pyo made stay at Hetzner.
pub async fn remove(db: &PgPool, team_id: &str) -> ApiResult<()> {
    sqlx::query("DELETE FROM pyo.integrations WHERE team_id = $1 AND kind = 'hetzner'")
        .bind(team_id)
        .execute(db)
        .await?;
    forget_catalog(team_id);
    Ok(())
}

// ── Locations, types and prices ─────────────────────────────────────────────

type Cached = (Instant, Arc<Catalog>);

fn cache() -> &'static Mutex<BTreeMap<String, Cached>> {
    static C: std::sync::OnceLock<Mutex<BTreeMap<String, Cached>>> = std::sync::OnceLock::new();
    C.get_or_init(Default::default)
}

fn forget_catalog(team_id: &str) {
    cache()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(team_id);
}

/// The team's catalog, read again after a minute.
pub async fn catalog(db: &PgPool, team_id: &str) -> ApiResult<Arc<Catalog>> {
    if let Some((at, c)) = cache()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(team_id)
        && at.elapsed() < Duration::from_secs(60)
    {
        return Ok(c.clone());
    }
    let c = Arc::new(provider(db, team_id).await?.catalog().await?);
    cache()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(team_id.to_owned(), (Instant::now(), c.clone()));
    Ok(c)
}

#[derive(Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
#[serde(rename_all = "camelCase")]
pub struct OptionsQuery {
    /// The team's id.
    team: String,
    /// The provider: "hetzner".
    #[serde(default)]
    provider: Option<String>,
}

/// Server types and prices
///
/// What a new server at a cloud provider can be, read live from it with the
/// team's token: its locations, and its server types with their specs,
/// architecture, where each can be made now and its monthly price in each
/// location (before VAT, with its IPv4 address). Pyo's servers run x86-64,
/// so Arm types have `supported` false.
#[utoipa::path(get, path = "/cloud/options", operation_id = "getCloudOptions", tag = "Servers",
    params(OptionsQuery),
    responses(
        (status = 200, description = "`provider`, `currency`, `locations` (each with `name`, `city`, `country` and `networkZone`) and `serverTypes` (each with `name`, `description`, `cores`, `memoryGb`, `diskGb`, `cpuType`, `architecture`, `supported`, `prices` by location and `available` locations).", body = Object),
        (status = 400, description = "The team hasn't connected the provider.", body = crate::error::ErrorBody),
        (status = 404, description = "No such team, or you aren't in it.", body = crate::error::ErrorBody),
        (status = 502, description = "The provider failed.", body = crate::error::ErrorBody),
    ),
    extensions(("x-pyo-access" = json!("read"))))]
pub async fn options(
    State(s): State<AppState>,
    SessionUser(user): SessionUser,
    Query(q): Query<OptionsQuery>,
) -> ApiResult<Json<Value>> {
    let mut tx = s.db.begin().await?;
    require_role(&mut tx, &user, &q.team, Role::Viewer).await?;
    drop(tx);
    let provider = q.provider.as_deref().unwrap_or("hetzner");
    if !PROVIDERS.contains(&provider) {
        return Err(ApiError::bad_request("The provider is \"hetzner\"."));
    }
    let c = catalog(&s.db, &q.team).await?;
    let mut v = serde_json::to_value(&*c).map_err(anyhow::Error::from)?;
    v["provider"] = json!(provider);
    Ok(Json(v))
}

// ── Following new servers, deleting old ones ────────────────────────────────

/// A row of `pyo.cloud_servers` being followed.
#[derive(sqlx::FromRow)]
struct Row {
    id: String,
    team_id: String,
    cluster_id: Option<String>,
    sandbox_server_id: Option<String>,
    name: String,
    remote_id: String,
    state: String,
    age_secs: f64,
    sign_command: Option<String>,
    first_server: bool,
}

/// What a server joining shows, from the provider, the tailnet and the
/// cluster.
#[derive(Debug, Clone, PartialEq)]
pub struct Seen {
    /// The provider's status; None once it's gone.
    pub remote: Option<String>,
    pub on_tailnet: bool,
    /// Its row in the cluster, and whether it's ready.
    pub in_cluster: Option<bool>,
    /// Tailnet Lock waits for its key to be signed: the command.
    pub sign: Option<String>,
    /// It becomes a sandbox server, not a cluster's.
    pub sandbox: bool,
    /// The sandbox server can't run sandboxes: why.
    pub refused: Option<String>,
}

/// The next state of a server being made, and why it failed, if it did.
pub fn next_state(seen: &Seen, age: Duration) -> (&'static str, Option<String>) {
    if seen.in_cluster == Some(true) {
        return ("ready", None);
    }
    let Some(remote) = seen.remote.as_deref() else {
        return ("failed", Some("It was deleted at Hetzner.".into()));
    };
    if let Some(why) = &seen.refused {
        return ("failed", Some(format!("It can't run sandboxes. {why}")));
    }
    let state = if seen.in_cluster.is_some() || seen.on_tailnet {
        "joining"
    } else if matches!(remote, "running" | "starting") {
        "booting"
    } else {
        "creating"
    };
    if age < TIMEOUT {
        return (state, None);
    }
    let why = match state {
        "creating" => "Hetzner didn't start it in 15 minutes.",
        "booting" => {
            "It didn't join your tailnet in 15 minutes. Its setup (cloud-init) may have failed. Open its console at Hetzner and read /var/log/pyo-join.log."
        }
        _ if seen.sign.is_some() => {
            "Its key wasn't signed in 15 minutes. Sign it, or delete the server."
        }
        _ if seen.sandbox => {
            "It joined your tailnet but didn't become a sandbox server in 15 minutes. Read /var/log/pyo-join.log on it."
        }
        _ => {
            "It joined your tailnet but not the cluster in 15 minutes. Read /var/log/pyo-join.log on it."
        }
    };
    ("failed", Some(why.into()))
}

/// The cluster whose tailnet a server joins: its own cluster's, a sandbox
/// server's cluster, or the one Pyo runs on.
async fn tailnet_of(db: &PgPool, r: &Row) -> ApiResult<Option<String>> {
    let via: Option<String> = match (&r.cluster_id, &r.sandbox_server_id) {
        (Some(c), _) => Some(c.clone()),
        (None, Some(sbs)) => sqlx::query_scalar::<_, Option<String>>(
            "SELECT via_cluster FROM pyo.sandbox_servers WHERE id = $1",
        )
        .bind(sbs)
        .fetch_optional(db)
        .await?
        .flatten(),
        (None, None) => None,
    };
    Ok(crate::tailscale::key_cluster(db, via.as_deref()).await.ok())
}

/// Pyo's machines on a cluster's tailnet, as its last sync saw them, or
/// None without Tailscale there.
async fn tailnet_devices(db: &PgPool, cluster: Option<&str>) -> Option<Vec<pyo_proto::tailscale::Machine>> {
    let t = crate::tailscale::of(db, cluster?).await.ok().flatten()?;
    Some(t.machines)
}

/// One pass: new servers move on, deleted ones go. Run by the Tailscale
/// loop's leader.
pub async fn tick(s: &AppState) -> ApiResult<()> {
    bootstrap::prune(&s.db).await?;
    let rows: Vec<Row> = sqlx::query_as(
        "SELECT id, team_id, cluster_id, sandbox_server_id, name, remote_id, state,
                extract(epoch FROM now() - created_at)::float8 AS age_secs, sign_command, first_server
           FROM pyo.cloud_servers WHERE state IN ('creating', 'booting', 'joining', 'removing')
          ORDER BY created_at",
    )
    .fetch_all(&s.db)
    .await?;
    if rows.is_empty() {
        return Ok(());
    }
    let install = crate::tailscale::install_id(&s.db).await?;
    let mut providers: BTreeMap<String, Option<hetzner::Hetzner>> = BTreeMap::new();
    for r in rows {
        if !providers.contains_key(&r.team_id) {
            let p = match token(&s.db, &r.team_id).await {
                Ok(Some(t)) => hetzner::Hetzner::new(&t).ok(),
                _ => None,
            };
            providers.insert(r.team_id.clone(), p);
        }
        let Some(p) = providers.get(&r.team_id).and_then(Option::as_ref) else {
            set_error(
                &s.db,
                &r.id,
                Some("Hetzner isn't connected. Connect it in Integrations."),
            )
            .await?;
            continue;
        };
        let tailnet = tailnet_of(&s.db, &r).await?;
        let devices = tailnet_devices(&s.db, tailnet.as_deref()).await;
        let result = if r.state == "removing" {
            finish_removal(s, p, &r, &install, tailnet.as_deref(), devices.as_deref()).await
        } else {
            follow(s, p, &r, devices.as_deref()).await
        };
        if let Err(e) = result {
            tracing::warn!(server = %r.name, error = %e.message, "cloud server");
            set_error(&s.db, &r.id, Some(&e.message)).await?;
        }
    }
    Ok(())
}

async fn set_error(db: &PgPool, id: &str, error: Option<&str>) -> ApiResult<()> {
    sqlx::query("UPDATE pyo.cloud_servers SET error = $2, updated_at = now() WHERE id = $1 AND error IS DISTINCT FROM $2")
        .bind(id)
        .bind(error)
        .execute(db)
        .await?;
    Ok(())
}

/// The machine on the tailnet that is this server (Pyo's machines are
/// tagged tag:pyo), by its hostname.
fn device_of<'a>(
    devices: &'a [pyo_proto::tailscale::Machine],
    name: &str,
) -> impl Iterator<Item = &'a pyo_proto::tailscale::Machine> {
    devices
        .iter()
        .filter(move |d| d.hostname.eq_ignore_ascii_case(name))
}

async fn follow(
    s: &AppState,
    p: &hetzner::Hetzner,
    r: &Row,
    devices: Option<&[pyo_proto::tailscale::Machine]>,
) -> ApiResult<()> {
    let remote = p.server(&r.remote_id).await?;
    let mut refused = None;
    let in_cluster: Option<bool> = match (&r.cluster_id, &r.sandbox_server_id) {
        (Some(c), _) => sqlx::query_scalar(
            "SELECT report->>'status' = 'ready' FROM pyo.servers WHERE cluster_id = $1 AND name = $2",
        )
        .bind(c)
        .bind(&r.name)
        .fetch_optional(&s.db)
        .await?
        .map(|x: Option<bool>| x.unwrap_or(false)),
        // A sandbox server counts once sandbox.sh redeemed its code, and is
        // ready once its daemon reports.
        (None, Some(sbs)) => {
            let row: Option<(String, bool, Option<String>)> = sqlx::query_as(
                "SELECT state, joined_at IS NOT NULL, error FROM pyo.sandbox_servers WHERE id = $1",
            )
            .bind(sbs)
            .fetch_optional(&s.db)
            .await?;
            match row {
                Some((state, _, _)) if state == "ready" => Some(true),
                Some((state, _, error)) if state == "refused" => {
                    refused = Some(error.unwrap_or_default());
                    Some(false)
                }
                Some((_, true, _)) => Some(false),
                _ => None,
            }
        }
        (None, None) => None,
    };
    let mine: Vec<&pyo_proto::tailscale::Machine> = devices
        .map(|d| device_of(d, &r.name).collect())
        .unwrap_or_default();
    let sign = mine.iter().find_map(|d| d.sign_command.clone());
    let seen = Seen {
        remote: remote.as_ref().map(|x| x.status.clone()),
        on_tailnet: !mine.is_empty(),
        in_cluster,
        sign: sign.clone(),
        sandbox: r.sandbox_server_id.is_some(),
        refused,
    };
    let (mut state, error) = next_state(&seen, Duration::from_secs_f64(r.age_secs.max(0.0)));
    // It traded its code over the tailnet: Joining, even when the tailnet's
    // list can't be read now.
    if r.state == "joining" && matches!(state, "creating" | "booting") {
        state = "joining";
    }
    let ipv4 = remote.and_then(|x| x.ipv4);
    if state != r.state || sign != r.sign_command || error.is_some() {
        sqlx::query(
            "UPDATE pyo.cloud_servers SET state = $2, error = $3, sign_command = $4, ipv4 = coalesce($5, ipv4),
                    ready_at = CASE WHEN $2 = 'ready' THEN now() ELSE ready_at END, updated_at = now()
              WHERE id = $1 AND state = $6",
        )
        .bind(&r.id)
        .bind(state)
        .bind(&error)
        .bind(if state == "ready" { None } else { sign })
        .bind(ipv4)
        .bind(&r.state)
        .execute(&s.db)
        .await?;
        if state != r.state {
            tracing::info!(server = %r.name, state, "cloud server");
        }
    }
    Ok(())
}

/// A server being deleted: once it's drained (or it never joined), off
/// the tailnet, then deleted at the provider.
async fn finish_removal(
    s: &AppState,
    p: &hetzner::Hetzner,
    r: &Row,
    install: &str,
    tailnet: Option<&str>,
    devices: Option<&[pyo_proto::tailscale::Machine]>,
) -> ApiResult<()> {
    let joined: Option<(Option<String>, Option<String>)> = match &r.cluster_id {
        Some(c) => sqlx::query_as(
            "SELECT removal->>'state', removal->>'message' FROM pyo.servers WHERE cluster_id = $1 AND name = $2",
        )
        .bind(c)
        .bind(&r.name)
        .fetch_optional(&s.db)
        .await?,
        None => None,
    };
    let cluster_gone: bool = match &r.cluster_id {
        Some(c) => sqlx::query_scalar(
            "SELECT coalesce((SELECT status IN ('pending', 'disconnected') FROM pyo.clusters WHERE id = $1), true)",
        )
        .bind(c)
        .fetch_one(&s.db)
        .await?,
        None => true,
    };
    match joined {
        Some((Some(state), message)) if state == "failed" => {
            let m = format!(
                "Moving its services off failed. {}",
                message.unwrap_or_default()
            );
            set_error(&s.db, &r.id, Some(m.trim())).await?;
            return Ok(());
        }
        // Still moving its services off.
        Some((Some(state), _)) if state == "draining" => return Ok(()),
        // Its cluster's control plane: kept until the cluster goes.
        Some((None, _)) if !cluster_gone && r.first_server => return Ok(()),
        // It joined after its deletion was asked: out of the cluster first.
        Some((None, _)) if !cluster_gone => {
            if let Some(c) = &r.cluster_id {
                let mut tx = s.db.begin().await?;
                sqlx::query("UPDATE pyo.servers SET removal = $3 WHERE cluster_id = $1 AND name = $2 AND removal IS NULL")
                    .bind(c)
                    .bind(&r.name)
                    .bind(json!({ "state": "removing", "force": true, "by": "Pyo", "startedAt": pyo_proto::now_ms() }))
                    .execute(&mut *tx)
                    .await?;
                crate::deploy::enqueue(
                    &mut tx,
                    c,
                    pyo_proto::Command::RemoveServer {
                        server: r.name.clone(),
                        force: true,
                    },
                )
                .await?;
                tx.commit().await?;
                s.wake(c).await;
            }
        }
        _ => {}
    }
    // Off the tailnet (with the cluster's next sync): by the node ID its
    // cluster reported, else by name.
    if let Some(tailnet) = tailnet {
        let reported: Option<String> = match &r.cluster_id {
            Some(cl) => sqlx::query_scalar(
                "SELECT t->>'nodeId' FROM pyo.clusters, jsonb_array_elements(coalesce(capabilities->'tailnet', '[]')) t
                  WHERE id = $1 AND t->>'server' = $2 AND coalesce(t->>'nodeId', '') <> '' LIMIT 1",
            )
            .bind(cl)
            .bind(&r.name)
            .fetch_optional(&s.db)
            .await?,
            None => None,
        };
        let by_name: Vec<String> = devices
            .map(|d| device_of(d, &r.name).map(|d| d.node_id.clone()).collect())
            .unwrap_or_default();
        let node = reported.or(if by_name.len() == 1 {
            by_name.into_iter().next()
        } else {
            None
        });
        if let Some(node) = node
            && let Err(e) = crate::tailscale::forget_machine(&s.db, tailnet, &node).await
        {
            tracing::warn!(server = %r.name, error = %e.message, "couldn't take the server off the tailnet");
        }
    }
    let owner = match (&r.cluster_id, &r.sandbox_server_id) {
        (Some(c), _) => Some(Owner::Cluster(c)),
        (None, Some(sbs)) => Some(Owner::Sandbox(sbs)),
        (None, None) => None,
    };
    match delete(p, &r.remote_id, install, owner).await {
        Ok(()) => {
            sqlx::query(
                "UPDATE pyo.cloud_servers SET state = 'deleted', error = NULL, sign_command = NULL, deleted_at = now(), updated_at = now() WHERE id = $1",
            )
            .bind(&r.id)
            .execute(&s.db)
            .await?;
            tracing::info!(server = %r.name, "cloud server deleted");
            Ok(())
        }
        Err(CloudError::NotOurs) => {
            sqlx::query(
                "UPDATE pyo.cloud_servers SET state = 'failed', error = $2, updated_at = now() WHERE id = $1",
            )
            .bind(&r.id)
            .bind(CloudError::NotOurs.to_string())
            .execute(&s.db)
            .await?;
            Ok(())
        }
        Err(e) => Err(e.into()),
    }
}

/// For the dashboard: the team's connections (the token's end only), and
/// the servers Pyo made.
pub async fn snapshot(db: &PgPool, team_ids: &[String]) -> ApiResult<(Value, Value)> {
    let connections: Vec<Value> = sqlx::query_scalar(
        "SELECT jsonb_build_object('teamId', team_id, 'provider', kind, 'hint', hint, 'createdAt', pyo.ms(created_at))
           FROM pyo.integrations WHERE team_id = ANY($1) AND kind = 'hetzner'",
    )
    .bind(team_ids)
    .fetch_all(db)
    .await?;
    let servers: Vec<Value> = sqlx::query_scalar(
        "SELECT jsonb_build_object('id', id, 'teamId', team_id, 'provider', provider, 'clusterId', cluster_id,
                'sandboxServerId', sandbox_server_id, 'name', name,
                'firstServer', first_server, 'builder', builder, 'location', location, 'serverType', server_type,
                'monthly', monthly, 'currency', currency, 'ipv4', ipv4, 'state', state, 'error', error,
                'signCommand', sign_command, 'createdBy', created_by, 'createdAt', pyo.ms(created_at),
                'deadlineAt', pyo.ms(created_at + interval '15 minutes'), 'readyAt', pyo.ms(ready_at))
           FROM pyo.cloud_servers WHERE team_id = ANY($1) AND state <> 'deleted' ORDER BY created_at",
    )
    .bind(team_ids)
    .fetch_all(db)
    .await?;
    Ok((json!(connections), json!(servers)))
}

#[cfg(test)]
mod tests {
    use super::hetzner::{Hetzner, mock};
    use super::*;

    const INSTALL: &str = "0123456789abcdef0123456789abcdef";

    fn create<'a>(name: &'a str, ty: &'a str, cluster: &str, user_data: &'a str) -> Create<'a> {
        Create {
            name,
            location: "fsn1",
            server_type: ty,
            user_data,
            labels: labels(INSTALL, Owner::Cluster(cluster)),
            firewall: None,
        }
    }

    #[tokio::test]
    async fn catalog_reads_types_prices_and_availability() {
        let h = Hetzner::mock(mock::TOKEN, mock::Mock::new());
        let c = h.catalog().await.unwrap();
        assert_eq!(c.currency, "EUR");
        assert_eq!(c.locations.len(), 2);
        // The deprecated type is left out; the cheapest comes first.
        let names: Vec<&str> = c.server_types.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, ["cax11", "cx22"]);
        let cx = c.server_types.iter().find(|t| t.name == "cx22").unwrap();
        // 3.79 for the server, 0.50 for its IPv4 address.
        assert_eq!(cx.prices.get("fsn1"), Some(&4.29));
        assert!(cx.supported && cx.architecture == "x86");
        assert_eq!(c.pick("fsn1", "cx22").unwrap(), 4.29);
        // Arm isn't for Pyo's servers.
        assert!(c.pick("fsn1", "cax11").unwrap_err().message.contains("Arm"));
        // Sold out in Ashburn.
        let e = c.pick("ash", "cx22").unwrap_err();
        assert!(
            e.message.contains("isn't available in Ashburn"),
            "{}",
            e.message
        );
        assert!(c.pick("nbg1", "cx22").is_err());
        assert!(c.pick("fsn1", "cx99").is_err());
    }

    #[tokio::test]
    async fn create_and_delete() {
        let m = mock::Mock::new();
        let h = Hetzner::mock(mock::TOKEN, m.clone());
        let fw = h.firewall(INSTALL).await.unwrap();
        // Made once, then found by its label.
        assert_eq!(h.firewall(INSTALL).await.unwrap(), fw);
        {
            let st = m.0.lock().unwrap();
            assert_eq!(st.firewalls.len(), 1);
            let rules = &st.firewalls.values().next().unwrap()["rules"];
            let open: Vec<String> = rules
                .as_array()
                .unwrap()
                .iter()
                .map(|r| {
                    format!(
                        "{}/{}",
                        r["protocol"].as_str().unwrap(),
                        r["port"].as_str().unwrap()
                    )
                })
                .collect();
            assert_eq!(open, ["tcp/80", "tcp/443", "udp/41641"]);
        }
        let mut c = create("web-1", "cx22", "cl_one", "#cloud-config\n");
        c.firewall = Some(&fw);
        let r = h.create(&c).await.unwrap();
        assert_eq!(r.status, "initializing");
        assert_eq!(r.labels.get("pyo").map(String::as_str), Some(INSTALL));
        assert_eq!(
            r.labels.get("pyo-cluster").map(String::as_str),
            Some("cl_one")
        );
        assert_eq!(r.ipv4.as_deref(), Some("203.0.113.7"));
        assert_eq!(h.server(&r.id).await.unwrap().unwrap().id, r.id);
        // The same name again: Hetzner's names are unique in a project.
        assert_eq!(h.create(&c).await.unwrap_err(), CloudError::NameTaken);
        delete(&h, &r.id, INSTALL, Some(Owner::Cluster("cl_one")))
            .await
            .unwrap();
        assert_eq!(h.server(&r.id).await.unwrap(), None);
        // Gone already is fine.
        delete(&h, &r.id, INSTALL, Some(Owner::Cluster("cl_one")))
            .await
            .unwrap();
        // Ids are numbers: anything else is never sent.
        assert_eq!(h.server("1/../../firewalls").await.unwrap(), None);
    }

    #[tokio::test]
    async fn only_servers_with_this_installations_label_are_deleted() {
        let m = mock::Mock::new();
        let h = Hetzner::mock(mock::TOKEN, m.clone());
        let unlabelled = m.add_server(json!({}));
        let other_pyo = m.add_server(
            json!({ "pyo": "ffffffffffffffffffffffffffffffff", "pyo-cluster": "cl_one" }),
        );
        let other_cluster = m.add_server(json!({ "pyo": INSTALL, "pyo-cluster": "cl_two" }));
        let sandbox = m.add_server(json!({ "pyo": INSTALL, "pyo-sandbox-server": "sbs_one" }));
        for id in [unlabelled, other_pyo, other_cluster, sandbox] {
            assert_eq!(
                delete(&h, &id.to_string(), INSTALL, Some(Owner::Cluster("cl_one"))).await,
                Err(CloudError::NotOurs)
            );
        }
        // A sandbox server only by its own label.
        assert_eq!(
            delete(
                &h,
                &other_cluster.to_string(),
                INSTALL,
                Some(Owner::Sandbox("sbs_one"))
            )
            .await,
            Err(CloudError::NotOurs)
        );
        assert_eq!(
            delete(
                &h,
                &sandbox.to_string(),
                INSTALL,
                Some(Owner::Sandbox("sbs_two"))
            )
            .await,
            Err(CloudError::NotOurs)
        );
        assert_eq!(m.0.lock().unwrap().servers.len(), 4);
        assert!(
            !m.0.lock()
                .unwrap()
                .calls
                .iter()
                .any(|c| c.starts_with("DELETE")),
            "nothing was deleted"
        );
        // Ours, without a cluster to match (a cluster that was removed).
        delete(&h, &other_cluster.to_string(), INSTALL, None)
            .await
            .unwrap();
        delete(
            &h,
            &sandbox.to_string(),
            INSTALL,
            Some(Owner::Sandbox("sbs_one")),
        )
        .await
        .unwrap();
        assert_eq!(m.0.lock().unwrap().servers.len(), 2);
    }

    #[tokio::test]
    async fn quota_unavailable_and_token_errors_say_what_to_do() {
        let m = mock::Mock::new();
        m.0.lock().unwrap().limit = 1;
        let h = Hetzner::mock(mock::TOKEN, m.clone());
        h.create(&create("a", "cx22", "cl_one", "")).await.unwrap();
        let e = h
            .create(&create("b", "cx22", "cl_one", ""))
            .await
            .unwrap_err();
        assert!(matches!(e, CloudError::Quota(_)));
        let api: ApiError = e.into();
        assert_eq!(api.status, axum::http::StatusCode::CONFLICT);
        assert!(api.message.contains("server limit") && api.message.contains("Limits page"));

        m.0.lock().unwrap().limit = 5;
        m.0.lock().unwrap().sold_out = vec!["cx32".into()];
        let e = h
            .create(&create("c", "cx32", "cl_one", ""))
            .await
            .unwrap_err();
        assert!(matches!(e, CloudError::Unavailable(_)));
        assert!(e.to_string().contains("Pick another type or location"));

        let bad = Hetzner::mock(&"x".repeat(64), m.clone());
        assert_eq!(bad.locations().await.unwrap_err(), CloudError::Unauthorized);
        assert_eq!(
            hetzner::error_of(
                403,
                &json!({ "error": { "code": "forbidden", "message": "insufficient permissions" } })
            ),
            CloudError::ReadOnly
        );
        assert_eq!(
            hetzner::error_of(429, &Value::Null),
            CloudError::RateLimited
        );
        assert!(hetzner::valid_token(mock::TOKEN));
        assert!(
            !hetzner::valid_token("short")
                && !hetzner::valid_token(&format!("{}!", &mock::TOKEN[1..]))
        );
    }

    const TS_KEY: &str = "tskey-auth-kAbc1CNTRL-0123456789abcdefSECRET";
    const CODE: &str = "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08";
    const SSH_KEY: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIGq0QvB8p6jWl1y3o7nZ me@laptop";

    fn boot(role: bootstrap::Role) -> Boot<'static> {
        Boot {
            pyo: "http://100.64.0.1:5006",
            server_id: "cs_web1",
            cluster_id: "cl_one",
            role,
            name: "web-1",
            tailscale_key: TS_KEY,
            code: CODE,
        }
    }

    #[test]
    fn user_data_holds_only_the_two_one_time_values_and_the_ssh_key() {
        let ud = user_data(&boot(bootstrap::Role::Join), Some(SSH_KEY)).unwrap();
        assert!(ud.starts_with("#cloud-config\n"));
        assert!(ud.contains("ssh_pwauth: false\n"));
        assert!(ud.contains("169.254.169.254"));
        // Line by line: every line is structure, the boot script (which
        // holds no secret), or one of the three values. Each value once.
        let script: std::collections::BTreeSet<&str> = BOOT_SCRIPT.lines().collect();
        let mut values = Vec::new();
        for line in ud.lines() {
            let l = line.trim();
            if [TS_KEY, CODE, &format!("- \"{SSH_KEY}\"")].contains(&l) {
                values.push(l.to_owned());
                continue;
            }
            let known = l.is_empty()
                || l.starts_with('#')
                || [
                    "ssh_pwauth: false",
                    "ssh_authorized_keys:",
                    "bootcmd:",
                    "write_files:",
                    "- path: /run/pyo-boot/tailscale-key",
                    "- path: /run/pyo-boot/code",
                    "- path: /run/pyo-boot/boot.sh",
                    "owner: root:root",
                    "permissions: \"0600\"",
                    "permissions: \"0700\"",
                    "content: |-",
                    "content: |",
                    "runcmd:",
                    "- [bash, /run/pyo-boot/boot.sh, \"http://100.64.0.1:5006\", \"cs_web1\", \"cl_one\", \"join\", \"web-1\"]",
                ]
                .contains(&l)
                || l.starts_with("- [sh, -c, \"iptables -t raw")
                || (line.starts_with("      ") && script.contains(&line[6..]));
            assert!(known, "unexpected line in user data: {line}");
        }
        assert_eq!(values.len(), 3, "{values:?}");
        // The boot script has no value of its own: no key, no token.
        for secret in ["tskey", "--token", "--registry-token", "K3S_TOKEN"] {
            assert!(!BOOT_SCRIPT.contains(secret), "{secret}");
        }

        // As cloud-init reads it.
        let y: serde_json::Value = serde_norway::from_str(&ud).unwrap();
        assert_eq!(y["ssh_pwauth"], false);
        assert_eq!(y["ssh_authorized_keys"][0], SSH_KEY);
        let files = y["write_files"].as_array().unwrap();
        let file = |name: &str| {
            files
                .iter()
                .find(|f| f["path"] == format!("/run/pyo-boot/{name}"))
                .unwrap()
                .clone()
        };
        assert_eq!(file("tailscale-key")["content"], TS_KEY);
        assert_eq!(file("tailscale-key")["permissions"], "0600");
        assert_eq!(file("code")["content"], CODE);
        assert_eq!(file("code")["permissions"], "0600");
        assert_eq!(file("boot.sh")["content"], BOOT_SCRIPT);
        assert_eq!(file("boot.sh")["permissions"], "0700");
        assert_eq!(files.len(), 3);
        assert_eq!(
            y["runcmd"][0],
            json!([
                "bash",
                "/run/pyo-boot/boot.sh",
                "http://100.64.0.1:5006",
                "cs_web1",
                "cl_one",
                "join",
                "web-1"
            ])
        );
        // Nothing else.
        let keys: Vec<&str> = y.as_object().unwrap().keys().map(String::as_str).collect();
        assert_eq!(
            keys,
            [
                "bootcmd",
                "runcmd",
                "ssh_authorized_keys",
                "ssh_pwauth",
                "write_files"
            ]
        );

        // Without an SSH key: no keys line. A new cluster's first server.
        let ud = user_data(&boot(bootstrap::Role::Connect), None).unwrap();
        assert!(!ud.contains("ssh_authorized_keys"));
        let y: serde_json::Value = serde_norway::from_str(&ud).unwrap();
        assert_eq!(y["runcmd"][0][5], "connect");
        // Nothing that could break out of its line.
        let mut bad = boot(bootstrap::Role::Join);
        bad.name = "web-1\"]\nruncmd: [x]";
        assert!(user_data(&bad, None).is_err());
        let mut bad = boot(bootstrap::Role::Join);
        bad.code = "abc\n  - path: /etc/x";
        assert!(user_data(&bad, None).is_err());
    }

    #[test]
    fn the_boot_script_parses_and_keeps_each_detail_whole() {
        let ok = std::process::Command::new("bash")
            .args(["-n", "-c", BOOT_SCRIPT])
            .status()
            .unwrap();
        assert!(ok.success(), "bash -n");
        // Its reading of Pyo's answer: each value one argument, as it was.
        let parse = BOOT_SCRIPT
            .split("# >>> parse\n")
            .nth(1)
            .and_then(|r| r.split("# <<< parse\n").next())
            .expect("the boot script has its parse block");
        let material = bootstrap::Material::new(
            "https://pyo.tail1234.ts.net/install/join.sh".into(),
            &crate::join::args(&crate::join::Join {
                server_ip: "100.64.0.1",
                token: "abc123.0123456789abcdef",
                server_name: "web 1 $(touch /tmp/x) 'q'",
                k3s_version: "v1.37.2+k3s1",
                tailscale_key: None,
                builder: true,
                registry: Some(("100.64.0.1:5006", "9b1c")),
            }),
        );
        let out = std::process::Command::new("bash")
            .args([
                "-c",
                &format!("set -u\n{parse}printf '%s\\n' \"$script\" \"${{#args[@]}}\"; printf '[%s]\\n' \"${{args[@]}}\""),
                "boot",
            ])
            .env("body", material.render().unwrap())
            .output()
            .unwrap();
        // `body` comes from the environment here; in the script, from curl.
        let out = String::from_utf8(out.stdout).unwrap();
        let mut lines = out.lines();
        assert_eq!(lines.next(), Some(material.script.as_str()));
        assert_eq!(lines.next(), Some(material.args.len().to_string().as_str()));
        let args: Vec<String> = lines.map(|l| l[1..l.len() - 1].to_owned()).collect();
        assert_eq!(args, material.args);
        assert!(args.contains(&"web 1 $(touch /tmp/x) 'q'".to_owned()));
        assert!(!args.iter().any(|a| a == "--tailscale-key"));
    }

    #[test]
    fn ssh_keys() {
        assert!(valid_ssh_key(
            "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIGq0QvB8p6jWl1y3o7nZ me@laptop"
        ));
        assert!(valid_ssh_key("ssh-rsa AAAAB3NzaC1yc2EAAAADAQABAAABAQC7"));
        assert!(!valid_ssh_key("ssh-ed25519"));
        assert!(!valid_ssh_key(
            "ssh-dss AAAAB3NzaC1kc3MAAACBAP1234567890abcdefghij"
        ));
        assert!(!valid_ssh_key(
            "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIGq0QvB8p6jWl1y3o7nZ\nruncmd: [x]"
        ));
        assert!(!valid_ssh_key(
            "ssh-ed25519 AAAA\"C3NzaC1lZDI1NTE5AAAAIGq0QvB8p6jWl1y3o7nZ"
        ));
        assert!(!valid_ssh_key(
            "-----BEGIN OPENSSH PRIVATE KEY----- b3BlbnNzaC1rZXktdjEAAAAABG5vbmU"
        ));
    }

    #[test]
    fn progress_and_timeouts() {
        let seen = |remote: Option<&str>, on_tailnet, in_cluster, sign: Option<&str>| Seen {
            remote: remote.map(str::to_owned),
            on_tailnet,
            in_cluster,
            sign: sign.map(str::to_owned),
            sandbox: false,
            refused: None,
        };
        let soon = Duration::from_secs(60);
        let late = TIMEOUT + Duration::from_secs(1);
        assert_eq!(
            next_state(&seen(Some("initializing"), false, None, None), soon).0,
            "creating"
        );
        assert_eq!(
            next_state(&seen(Some("running"), false, None, None), soon).0,
            "booting"
        );
        assert_eq!(
            next_state(&seen(Some("running"), true, None, None), soon).0,
            "joining"
        );
        assert_eq!(
            next_state(&seen(Some("running"), true, Some(false), None), soon).0,
            "joining"
        );
        assert_eq!(
            next_state(&seen(Some("running"), true, Some(true), None), late),
            ("ready", None)
        );
        let (s, why) = next_state(&seen(Some("running"), false, None, None), late);
        assert_eq!(s, "failed");
        assert!(why.unwrap().contains("cloud-init"));
        let (_, why) = next_state(
            &seen(
                Some("running"),
                true,
                None,
                Some("tailscale lock sign nodekey:1"),
            ),
            late,
        );
        assert!(why.unwrap().contains("wasn't signed"));
        let (s, why) = next_state(&seen(None, false, None, None), soon);
        assert_eq!(s, "failed");
        assert!(why.unwrap().contains("deleted at Hetzner"));

        // A sandbox server: refused at once, and its own words when late.
        let sbx = |in_cluster, refused: Option<&str>| Seen {
            sandbox: true,
            refused: refused.map(str::to_owned),
            ..seen(Some("running"), true, in_cluster, None)
        };
        assert_eq!(next_state(&sbx(Some(true), None), soon), ("ready", None));
        let (s, why) = next_state(&sbx(Some(false), Some("runsc isn't installed.")), soon);
        assert_eq!(s, "failed");
        assert_eq!(
            why.unwrap(),
            "It can't run sandboxes. runsc isn't installed."
        );
        let (s, why) = next_state(&sbx(Some(false), None), late);
        assert_eq!(s, "failed");
        assert!(why.unwrap().contains("didn't become a sandbox server"));
    }
}
