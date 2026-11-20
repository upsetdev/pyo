//! Pyo control plane: one Rust service that serves the dashboard (a static
//! single-page app), its JSON API, sign-in, and the endpoints in-cluster
//! agents dial out to. Postgres is the source of truth.
//!
//! A library with one entry point, [`run`]: the `pyo-api` binary calls it
//! with [`Extensions::default()`]. A hosted build can pass its own routes,
//! background tasks, migrations, dashboard features and entitlement checks
//! (see [`Extensions`]); self-hosted Pyo has none of those.

mod agent;
mod agent_auth;
mod agent_tls;
mod alerts;
pub mod auth;
mod backup;
mod backupstore;
mod bg;
mod builders;
mod bundles;
mod capacity;
mod cdn;
mod certs;
mod cli;
mod cloud;
mod cloudflare;
pub mod compose;
mod cronruns;
mod database_analytics;
mod database_backups;
mod database_imports;
mod database_logs;
mod databases;
mod deploy;
mod domainproof;
pub mod error;
mod firewall;
mod freedomain;
mod geoip;
mod git;
mod github;
mod hooks;
pub mod ids;
mod images;
mod install;
mod jobs;
mod join;
mod mail;
mod mcp;
mod migrate;
mod oauth;
pub mod observability;
pub mod openapi;
pub mod ops;
mod outbound;
mod passkeys;
mod platform_import;
mod pressure;
mod publicdash;
mod ratelimit;
mod recovery;
mod refs;
mod registry;
mod registrydoor;
mod rls;
mod routes;
mod sandbox;
mod secrets;
mod secretstores;
mod sigstore;
mod sites;
mod snapshot;
mod tailscale;
mod tailscale_setup;
mod teambackup;
mod template_source;
mod terminal;
mod traffic;
mod updates;
mod util;
mod vault;
mod volumes;
mod waf;
mod web;

use std::{collections::HashMap, net::SocketAddr, path::PathBuf, sync::Arc, time::Duration};

use axum::{Router, extract::DefaultBodyLimit, routing::get};
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::{ConnectOptions, PgPool};
use tokio::sync::{Mutex, Notify};
use tower_http::{compression::CompressionLayer, services::ServeDir, trace::TraceLayer};
use tracing_subscriber::EnvFilter;

#[derive(Clone)]
pub struct Config {
    /// URL agents use to reach this API. In-cluster agents use the Service.
    pub agent_api_url: String,
    pub agent_image: String,
    /// The WAF's image (`PYO_WAF_IMAGE`): sent with its settings,
    /// and pinned in the agent's admission policy.
    pub waf_image: String,
    /// The proxies' GeoIP database image (`PYO_GEOIP_IMAGE`): pinned in the
    /// agent's admission policy, and the agent's own setting.
    pub geoip_image: String,
    /// Mark the session cookie Secure (set when served over HTTPS).
    pub cookie_secure: bool,
    /// Anyone can create an account. The first account is always allowed.
    pub allow_signup: bool,
    /// Image of in-cluster builders (BuildKit runs beside it).
    pub builder_image: String,
    /// This replica's own host:port (POD_IP), so other replicas can relay
    /// a terminal's agent socket to it. None when there's one process.
    pub relay_addr: Option<String>,
}

#[derive(Clone)]
pub struct AppState {
    pub db: PgPool,
    pub config: Arc<Config>,
    pub observability: Arc<observability::Observability>,
    pub sealer: Arc<secrets::Sealer>,
    /// Long-polls waiting in this process, by cluster id or "builders:<team>".
    wakers: Arc<Mutex<HashMap<String, Arc<Notify>>>>,
    /// Terminal browser sockets waiting for their agent to dial back.
    pub terminals: terminal::Waiting,
    /// True once SIGTERM arrived: readiness fails and long-polls return.
    stopping: tokio::sync::watch::Receiver<bool>,
}

impl AppState {
    pub async fn waker(&self, cluster_id: &str) -> Arc<Notify> {
        self.wakers
            .lock()
            .await
            .entry(cluster_id.to_owned())
            .or_default()
            .clone()
    }

    /// Wake long-polls waiting on `key` in every process (Postgres NOTIFY).
    pub async fn wake(&self, key: &str) {
        bg::wake(&self.db, key).await;
    }

    pub fn stopping(&self) -> bool {
        *self.stopping.borrow()
    }

    /// Wake every long-poll in this process whose key starts with `prefix`.
    pub async fn notify_prefix(&self, prefix: &str) {
        for (k, n) in self.wakers.lock().await.iter() {
            if k.starts_with(prefix) {
                n.notify_waiters();
            }
        }
    }
}

fn env(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_owned())
}

/// What a hosted build adds to Pyo. Everything is optional; the default
/// changes nothing.
#[derive(Default)]
pub struct Extensions {
    /// Extra routes, merged into the app (use your own `/api/...` prefix).
    pub routes: Option<Router<AppState>>,
    /// Background tasks, started on worker processes.
    pub tasks: Vec<Task>,
    /// Your own migrations, run after Pyo's. Give them their own schema and
    /// migrations table (`create_schema`, `dangerous_set_table_name`).
    pub migrator: Option<sqlx::migrate::Migrator>,
    /// Dashboard features to turn on, listed by `GET /api/v1/meta`.
    pub features: Vec<String>,
    /// Checks before a team adds a server or deploys.
    pub entitlements: Option<Arc<dyn Entitlements>>,
    /// Signs every command before an agent gets it, with a key this
    /// process doesn't hold. Agents installed from here are given its
    /// public keys and refuse anything unsigned.
    pub command_signer: Option<Arc<dyn CommandSigner>>,
    /// Hostnames need proof of ownership (a DNS TXT record) before a team
    /// can claim one. For an installation shared by teams that don't trust
    /// each other.
    pub verify_domains: bool,
}

/// A command to sign: its id, its `kind`, and the hex SHA-256 of its JSON.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ToSign {
    pub id: String,
    pub kind: String,
    pub sha256: String,
    /// The command's exact JSON, for kinds that carry no secrets and whose
    /// content the signer checks itself ([`SIGNER_CHECKS`]).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
}

/// Kinds a signer sees in full, to hold them to its own policy: an agent
/// update names the image every customer's agent will run.
pub const SIGNER_CHECKS: &[&str] = &["update_agent"];

/// A reply to sign (pyo-proto `signing`, `pyoreply1`): for this cluster,
/// to the request that sent `nonce`, whose JSON hashes to `sha256`.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReplyToSign {
    pub nonce: String,
    pub sha256: String,
}

/// A signature over a command's claims (pyo-proto `signing`), or why the
/// signer won't sign that command (`refused`).
#[derive(Debug, Clone, Default, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Signature {
    #[serde(default)]
    pub claims: String,
    #[serde(default)]
    pub sig: String,
    #[serde(default)]
    pub kid: String,
    #[serde(default)]
    pub refused: Option<String>,
    /// When a session key signed it: the trusted key's delegation to it,
    /// passed on to the agent with the signature.
    #[serde(default)]
    pub delegation: Option<pyo_proto::DelegationWire>,
}

/// Where commands get signed (pyo-proto `signing`). The signer picks each
/// command's sequence number and expiry.
pub trait CommandSigner: Send + Sync {
    /// Signs commands for one cluster, in this order: one [`Signature`]
    /// each, or its refusal.
    fn sign<'a>(
        &'a self,
        cluster_id: &'a str,
        items: &'a [ToSign],
    ) -> futures::future::BoxFuture<'a, anyhow::Result<Vec<Signature>>>;
    /// Signs a reply the agent acts on (the heartbeat's, a certificate, a
    /// registry login), bound to its request's nonce. The signer picks
    /// the expiry.
    fn sign_reply<'a>(
        &'a self,
        cluster_id: &'a str,
        reply: &'a ReplyToSign,
    ) -> futures::future::BoxFuture<'a, anyhow::Result<Signature>>;
    /// The public keys agents trust (`PYO_COMMAND_KEY`: base64url, comma separated).
    fn public_keys(&self) -> String;
}

/// A background task: given the app state, runs until the process stops.
pub type Task = Box<dyn FnOnce(AppState) -> futures::future::BoxFuture<'static, ()> + Send>;

/// Whether a team may do something that a hosted build may charge for.
/// `Err` carries the message shown to the user. Self-hosted: always allowed.
pub trait Entitlements: Send + Sync {
    /// Before a server joins a team (a new cluster's first one, or another).
    fn can_add_server<'a>(
        &'a self,
        conn: &'a mut sqlx::PgConnection,
        team_id: &'a str,
    ) -> futures::future::BoxFuture<'a, Result<(), String>>;
    /// Before a deployment starts.
    fn can_deploy<'a>(
        &'a self,
        conn: &'a mut sqlx::PgConnection,
        team_id: &'a str,
    ) -> futures::future::BoxFuture<'a, Result<(), String>> {
        let _ = (conn, team_id);
        Box::pin(async { Ok(()) })
    }
}

static ENTITLEMENTS: std::sync::OnceLock<Arc<dyn Entitlements>> = std::sync::OnceLock::new();
static SIGNER: std::sync::OnceLock<Arc<dyn CommandSigner>> = std::sync::OnceLock::new();
static VERIFY_DOMAINS: std::sync::OnceLock<bool> = std::sync::OnceLock::new();

/// The command signer, when this build has one.
pub(crate) fn command_signer() -> Option<&'static Arc<dyn CommandSigner>> {
    SIGNER.get()
}

/// Whether hostnames need proof of ownership (see [`Extensions::verify_domains`]).
pub(crate) fn verify_domains() -> bool {
    VERIFY_DOMAINS.get().copied().unwrap_or(false)
}
static FEATURES: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();

/// Fails with 402 and the entitlement's message when a team may not add a server.
pub(crate) async fn check_add_server(
    conn: &mut sqlx::PgConnection,
    team_id: &str,
) -> error::ApiResult<()> {
    let Some(e) = ENTITLEMENTS.get() else {
        return Ok(());
    };
    // A hosted build's own tables, outside row security (rls.rs).
    let lifted = rls::lift(conn).await?;
    let out = e.can_add_server(conn, team_id).await;
    lifted.lower(conn).await?;
    out.map_err(|m| error::ApiError::new(axum::http::StatusCode::PAYMENT_REQUIRED, m))
}

/// Fails with 402 and the entitlement's message when a team may not deploy.
pub(crate) async fn check_deploy(
    conn: &mut sqlx::PgConnection,
    team_id: &str,
) -> error::ApiResult<()> {
    let Some(e) = ENTITLEMENTS.get() else {
        return Ok(());
    };
    let lifted = rls::lift(conn).await?;
    let out = e.can_deploy(conn, team_id).await;
    lifted.lower(conn).await?;
    out.map_err(|m| error::ApiError::new(axum::http::StatusCode::PAYMENT_REQUIRED, m))
}

/// The dashboard features a hosted build turned on (none when self-hosted).
pub(crate) fn features() -> &'static [String] {
    FEATURES.get().map(Vec::as_slice).unwrap_or(&[])
}

/// Whether this Pyo is hosted (Pyo Cloud): one Pyo for many teams, outside
/// their clusters. A hosted build turns dashboard features on.
pub(crate) fn hosted() -> bool {
    !features().is_empty()
}

/// Runs Pyo: migrates, starts the background work, and serves until a
/// shutdown signal. The `pyo-api` binary is `run(Extensions::default())`.
pub async fn run(ext: Extensions) -> anyhow::Result<()> {
    let Extensions {
        routes: extra_routes,
        tasks,
        migrator: extra_migrator,
        features,
        entitlements,
        command_signer,
        verify_domains,
    } = ext;
    let _ = FEATURES.set(features);
    let _ = VERIFY_DOMAINS.set(verify_domains);
    let dev_txt = std::env::var("PYO_DEV_TXT_FILE").is_ok_and(|v| !v.trim().is_empty());
    if let Some(s) = command_signer {
        let _ = SIGNER.set(s);
    }
    if let Some(e) = entitlements {
        let _ = ENTITLEMENTS.set(e);
    }
    // `pyo-api decrypt-backup <file.pyobak> <out>`: open a backup of Pyo's data.
    // (Run from `run` so a hosted binary has the same subcommand.)
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("decrypt-backup") {
        let (Some(input), Some(output)) = (args.get(2), args.get(3)) else {
            anyhow::bail!("usage: pyo-api decrypt-backup <file.pyobak> <out>");
        };
        return backup::decrypt_file(input, output);
    }
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "pyo_api=info,cloud_api=info,tower_http=info".into()),
        )
        .init();

    let database_url =
        std::env::var("DATABASE_URL").map_err(|_| anyhow::anyhow!("DATABASE_URL is required"))?;
    let connect = database_url
        .parse::<PgConnectOptions>()?
        // Postgres's default search_path starts with "$user", and the user is
        // usually named pyo: sqlx's _sqlx_migrations would land in the pyo
        // schema once it exists. Pyo's own SQL is always schema-qualified.
        .options([("search_path", "public")])
        .log_statements(tracing::log::LevelFilter::Debug);
    let role = bg::Role::from_env()?;
    let pool = match std::env::var("PYO_DB_POOL")
        .ok()
        .and_then(|v| v.parse().ok())
    {
        Some(n) => n,
        None => bg::pool_size(role, max_connections(&connect).await?),
    };
    let options = PgPoolOptions::new()
        .max_connections(pool)
        .acquire_timeout(Duration::from_secs(10))
        // Connections a burst opened close after a minute idle: each one
        // is a Postgres process holding memory.
        .idle_timeout(Duration::from_secs(60));
    let db = retry_connect(Duration::from_secs(60), || {
        options.clone().connect_with(connect.clone())
    })
    .await?;
    tracing::info!(pool, "database pool");
    // Safe with several processes starting at once: sqlx holds a lock while
    // migrating (migrate.rs). Migrations must keep the previous release working
    // (add now, remove in a later release) so rolling updates never break.
    // For the same reason a release must start on a database a newer one has
    // already migrated: an old pod restarting mid-rollout, or a rollback.
    migrate::run(&db).await?;
    if let Some(m) = extra_migrator {
        m.run(&db).await?;
    }
    vault::init(vault::Vault::new(db.clone(), vault::Provider::from_env()));
    if role.web() {
        auth::ensure_first_admin(&db).await?;
    }

    let listen_port = env("LISTEN_ADDR", "0.0.0.0:8000")
        .rsplit(':')
        .next()
        .and_then(|p| p.parse::<u16>().ok())
        .unwrap_or(8000);
    let config = Config {
        agent_api_url: env("PYO_AGENT_API_URL", "http://pyo.pyo-system.svc:8000"),
        agent_image: env("PYO_AGENT_IMAGE", "127.0.0.1:5005/pyo/agent:dev"),
        waf_image: pyo_proto::system::waf_image_from_env(),
        geoip_image: pyo_proto::system::geoip_image_from_env(),
        // Secure when Pyo is on HTTPS (its tailnet name); browsers keep
        // Secure cookies on http://localhost too.
        cookie_secure: env("PYO_COOKIE_SECURE", "false") == "true",
        allow_signup: env("PYO_ALLOW_SIGNUP", "false") == "true",
        builder_image: env("PYO_BUILDER_IMAGE", "127.0.0.1:5005/pyo/builder:dev"),
        // Other replicas reach this one on the port it listens on.
        relay_addr: std::env::var("POD_IP")
            .ok()
            .filter(|ip| !ip.is_empty())
            .map(|ip| {
                if ip.contains(':') {
                    format!("[{ip}]:{listen_port}")
                } else {
                    format!("{ip}:{listen_port}")
                }
            }),
    };
    let _ = deploy::REGISTRY.set(deploy::Registry {
        pull: env("PYO_REGISTRY_PULL", deploy::PULL_DEFAULT),
        internal: env("PYO_REGISTRY", deploy::INTERNAL_DEFAULT),
    });
    let observability = observability::Observability::from_env(db.clone());
    tokio::spawn(observability.clone().ensure_schema());
    let (stop_tx, stopping) = tokio::sync::watch::channel(false);
    let state = AppState {
        db,
        config: Arc::new(config),
        observability,
        sealer: Arc::new(secrets::Sealer::from_env()),
        wakers: Default::default(),
        terminals: Default::default(),
        stopping,
    };
    if role.web() {
        tokio::spawn(bg::listen(state.clone()));
        tokio::spawn(geoip::keeper(state.clone()));
    }
    // Every process: the worker sends sandbox servers images through the
    // registry door's tailnet address (sandbox/templates.rs).
    tokio::spawn(tailscale::addresses_follower(state.db.clone()));
    if role.worker() {
        // Each loop runs on one worker at a time (leader lock); jobs on any.
        tokio::spawn(deploy::sweeper(state.clone()));
        tokio::spawn(deploy::dns_watcher(state.clone()));
        tokio::spawn(builders::watcher(state.clone()));
        tokio::spawn(github::poller(state.clone()));
        tokio::spawn(git::poller(state.clone()));
        tokio::spawn(jobs::runner(state.clone()));
        tokio::spawn(backupstore::keeper(state.db.clone()));
        tokio::spawn(sites::sweeper(state.db.clone()));
        tokio::spawn(backup::runner(state.clone()));
        tokio::spawn(teambackup::runner(state.clone()));
        tokio::spawn(database_imports::runner(state.clone()));
        tokio::spawn(updates::runner(state.clone()));
        tokio::spawn(updates::hosted_agents(state.clone()));
        tokio::spawn(tailscale::runner(state.clone()));
        tokio::spawn(cdn::keeper(state.clone()));
        tokio::spawn(sandbox::runner::runner(state.clone()));
        tokio::spawn(sandbox::base::seed(state.clone()));
        tokio::spawn(sandbox::images::runner(state.clone()));
        tokio::spawn(agent_auth::sweeper(state.db.clone()));
        for task in tasks {
            tokio::spawn(task(state.clone()));
        }
        // Seal what predates envelope encryption; rewrap after a key rotation.
        tokio::spawn(async {
            // Retried: a busy database at startup shouldn't leave values unsealed.
            for attempt in 1..=10u64 {
                match vault::get().upkeep().await {
                    Ok(()) => break,
                    Err(e) => {
                        tracing::warn!(attempt, error = %e, "vault upkeep failed; retrying");
                        tokio::time::sleep(Duration::from_secs(15 * attempt)).await;
                    }
                }
            }
        });
    }
    tracing::info!(role = ?role, version = env!("CARGO_PKG_VERSION"), "pyo starting");
    if dev_txt {
        tracing::warn!("PYO_DEV_TXT_FILE is set: domain proofs come from that file, not DNS. For tests only");
    }

    let web_dir = PathBuf::from(env("PYO_WEB_DIR", "dash/dist/client"));
    // Unknown paths are dashboard routes: hand them the app shell.
    let shell = web::Shell::load(&web_dir.join("_shell.html"));
    let spa = ServeDir::new(&web_dir).fallback(
        axum::handler::HandlerWithoutStateExt::into_service(move || shell.clone().serve()),
    );

    let probes = Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/readyz", get(readyz));
    let extra_routes = extra_routes.unwrap_or_default();
    let door_state = state.clone();
    let door_state_for_agents = state.clone();
    let app = if !role.web() {
        // A worker answers only its probes.
        probes.with_state(state)
    } else {
        probes
            .nest("/api/auth", auth::routes())
            // JSON is compressed; the terminal's WebSocket isn't (it's not a body).
            .nest(
                "/api/v1",
                ops::routes()
                    .merge(github::api_routes())
                    .merge(git::api_routes())
                    .merge(cli::routes())
                    .merge(sandbox::api::routes())
                    .layer(CompressionLayer::new())
                    .merge(terminal::routes())
                    .merge(database_imports::routes())
                    .merge(platform_import::routes()),
            )
            .nest("/api/cli", cli::public_routes())
            .nest("/api/github", github::routes())
            .nest("/api/git", git::routes())
            .nest("/api/hooks", hooks::routes())
            .nest("/api/mcp", mcp::routes())
            .nest("/api/oauth", oauth::routes())
            .merge(oauth::well_known())
            .nest(
                "/api/agent/v1",
                agent::routes()
                    .merge(sites::agent_routes())
                    .merge(terminal::agent_routes())
                    .merge(database_imports::agent_routes())
                    .merge(agent_auth::routes())
                    // Signed requests (agent_auth.rs): who sent it, from its key.
                    .layer(axum::middleware::from_fn_with_state(
                        state.clone(),
                        agent_auth::signed,
                    )),
            )
            // Builders and pyo-static sign with keys of their own.
            .nest(
                "/api/builder/v1",
                builders::routes()
                    .merge(sites::builder_routes())
                    .layer(axum::middleware::from_fn_with_state(
                        state.clone(),
                        agent_auth::signed,
                    )),
            )
            .nest(
                "/api/static/v1",
                sites::routes().layer(axum::middleware::from_fn_with_state(
                    state.clone(),
                    agent_auth::signed,
                )),
            )
            .merge(extra_routes)
            .route("/api/{*rest}", get(error::not_found).post(error::not_found))
            .route("/install/agent.yaml", get(install::manifest))
            .route("/install/gateway-crds.yaml", get(install::gateway_crds))
            .route("/install/this-machine", get(install::this_machine))
            .route("/install/first-sign-in", get(install::first_sign_in))
            .route(
                "/install/reset-login",
                axum::routing::post(install::reset_login),
            )
            .route(
                "/install/this-machine/tailnet",
                axum::routing::post(install::this_machine_tailnet),
            )
            .route("/install/join.sh", get(join::serve))
            .route("/install/connect.sh", get(join::serve_connect))
            .route("/install/sandbox.sh", get(join::serve_sandbox))
            .route("/install/host-units.sh", get(join::serve_host_units))
            .fallback_service(tower_http::compression::Compression::new(spa))
            .layer(axum::middleware::from_fn_with_state(
                state.config.cookie_secure,
                web::headers,
            ))
            .layer(DefaultBodyLimit::max(4 * 1024 * 1024))
            .layer(TraceLayer::new_for_http())
            // Sandboxes' pages on their own tailnet name, before anything
            // else: nothing of the API or dashboard answers there, and
            // their requests aren't traced (the first carries a token).
            .layer(axum::middleware::from_fn_with_state(
                state.clone(),
                sandbox::pages::gate,
            ))
            .with_state(state)
    };

    // The registry's front door for teams' own images (reached over the
    // tailnet only; registrydoor.rs), and where servers Pyo made at a cloud
    // provider fetch their join details (cloud/bootstrap.rs), and where
    // sandbox servers join (sandbox/join.rs). Both check each connection's
    // real peer.
    if role.web() {
        let door: SocketAddr = env("PYO_REGISTRY_DOOR_ADDR", "0.0.0.0:5006").parse()?;
        let door_app = registrydoor::router(door_state.clone())
            .merge(cloud::bootstrap::router(door_state.clone()))
            .merge(sandbox::join::router(door_state))
            .into_make_service_with_connect_info::<SocketAddr>();
        match tokio::net::TcpListener::bind(door).await {
            Ok(l) => {
                tracing::info!(%door, "registry door listening");
                tokio::spawn(async move {
                    if let Err(e) = axum::serve(l, door_app).await {
                        tracing::warn!(error = %e, "registry door stopped");
                    }
                });
            }
            Err(e) => tracing::warn!(%door, error = %e, "registry door not listening"),
        }
    }
    // Agents over mTLS (agent_tls.rs), when this Pyo has that endpoint.
    if role.web() {
        agent_tls::start(
            door_state_for_agents,
            agent::routes()
                .merge(sites::agent_routes())
                .merge(terminal::agent_routes())
                .merge(database_imports::agent_routes())
                .merge(agent_auth::component_routes()),
        )
        .await?;
    }
    let addr: SocketAddr = env("LISTEN_ADDR", "0.0.0.0:8000").parse()?;
    tracing::info!(%addr, web = %web_dir.display(), "pyo api listening");
    let listener = tokio::net::TcpListener::bind(addr).await?;
    // With the peer's address, for handlers that read it (ConnectInfo).
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(bg::shutdown_signal(stop_tx))
    .await?;
    Ok(())
}

/// Ready for traffic: not shutting down, and the database answers.
async fn readyz(
    axum::extract::State(s): axum::extract::State<AppState>,
) -> (axum::http::StatusCode, &'static str) {
    if s.stopping() {
        return (axum::http::StatusCode::SERVICE_UNAVAILABLE, "stopping");
    }
    match sqlx::query("SELECT 1").execute(&s.db).await {
        Ok(_) => (axum::http::StatusCode::OK, "ok"),
        Err(_) => (
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            "database unavailable",
        ),
    }
}

/// Connecting to Postgres at start, tried again while it can't be reached
/// yet, for up to `limit`, 250 ms apart and then up to 4 s: a new pod is let
/// into Postgres once the network policy has its address (pyo-databases in
/// platform.yaml), a moment after it starts, and until then its connections
/// are refused. Postgres starting up counts too. Any other error, such as a
/// wrong password, returns at once. (The pool's own connect tries again
/// while refused, for its acquire timeout.)
async fn retry_connect<T, F, Fut>(limit: Duration, mut connect: F) -> Result<T, sqlx::Error>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, sqlx::Error>>,
{
    let start = tokio::time::Instant::now();
    let mut wait = Duration::from_millis(250);
    loop {
        let r = connect().await;
        let again = match &r {
            Err(sqlx::Error::Io(_)) => true,
            // cannot_connect_now: "the database system is starting up".
            Err(sqlx::Error::Database(e)) => e.code().as_deref() == Some("57P03"),
            _ => false,
        };
        if !again || start.elapsed() + wait > limit {
            return r;
        }
        if let Err(e) = &r {
            tracing::info!(error = %e, "the database isn't reachable yet, trying again");
        }
        tokio::time::sleep(wait).await;
        wait = (wait * 2).min(Duration::from_secs(4));
    }
}

/// Postgres's max_connections, which pyo-postgres sets by the server's
/// memory when it starts (platform.yaml): the pools follow it.
async fn max_connections(connect: &PgConnectOptions) -> anyhow::Result<u32> {
    use sqlx::Connection;
    let mut c = retry_connect(Duration::from_secs(60), || {
        sqlx::PgConnection::connect_with(connect)
    })
    .await?;
    let max: String = sqlx::query_scalar("SHOW max_connections")
        .fetch_one(&mut c)
        .await?;
    let _ = c.close().await;
    Ok(max.parse()?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    fn refused() -> sqlx::Error {
        sqlx::Error::Io(std::io::ErrorKind::ConnectionRefused.into())
    }

    /// Refused at first (a new pod the network policy doesn't know yet):
    /// tried again until it connects.
    #[tokio::test]
    async fn connects_once_postgres_lets_it_in() {
        let tries = AtomicU32::new(0);
        let r = retry_connect(Duration::from_secs(60), || async {
            match tries.fetch_add(1, Ordering::SeqCst) {
                0 | 1 => Err(refused()),
                n => Ok(n),
            }
        })
        .await;
        assert_eq!(r.unwrap(), 2);
        assert_eq!(tries.load(Ordering::SeqCst), 3);
    }

    /// Any other error returns at once, and refusals stop at the limit.
    #[tokio::test]
    async fn gives_up_on_other_errors_and_at_the_limit() {
        let tries = AtomicU32::new(0);
        let r: Result<(), _> = retry_connect(Duration::from_secs(60), || async {
            tries.fetch_add(1, Ordering::SeqCst);
            Err(sqlx::Error::Configuration("wrong password".into()))
        })
        .await;
        assert!(matches!(r, Err(sqlx::Error::Configuration(_))));
        assert_eq!(tries.load(Ordering::SeqCst), 1);

        let tries = AtomicU32::new(0);
        let r: Result<(), _> = retry_connect(Duration::from_millis(400), || async {
            tries.fetch_add(1, Ordering::SeqCst);
            Err(refused())
        })
        .await;
        assert!(matches!(r, Err(sqlx::Error::Io(_))));
        assert_eq!(tries.load(Ordering::SeqCst), 2);
    }
}
