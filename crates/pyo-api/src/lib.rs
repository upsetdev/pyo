//! Pyo control plane: one Rust service that serves the dashboard (a static
//! single-page app), its JSON API, sign-in, and the endpoints in-cluster
//! agents dial out to. Postgres is the source of truth.
//!
//! A library with one entry point, [`run`]: the `pyo-api` binary calls it
//! with [`Extensions::default()`]. A hosted build can pass its own routes,
//! background tasks, migrations, dashboard features and entitlement checks
//! (see [`Extensions`]); self-hosted Pyo has none of those.

mod agent;
pub mod auth;
mod backup;
mod backupstore;
mod bg;
mod builders;
mod bundles;
mod certs;
mod cli;
pub mod compose;
mod cronruns;
mod database_analytics;
mod database_backups;
mod database_imports;
mod database_logs;
mod databases;
mod deploy;
pub mod error;
mod firewall;
mod geoip;
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
mod ratelimit;
mod refs;
mod registry;
mod registrydoor;
mod routes;
mod secrets;
mod secretstores;
mod sigstore;
mod snapshot;
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
    /// This API on the tailnet, for servers that join over it.
    pub tailnet_api_url: String,
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
static FEATURES: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();

/// Fails with 402 and the entitlement's message when a team may not add a server.
pub(crate) async fn check_add_server(
    conn: &mut sqlx::PgConnection,
    team_id: &str,
) -> error::ApiResult<()> {
    match ENTITLEMENTS.get() {
        Some(e) => e
            .can_add_server(conn, team_id)
            .await
            .map_err(|m| error::ApiError::new(axum::http::StatusCode::PAYMENT_REQUIRED, m)),
        None => Ok(()),
    }
}

/// Fails with 402 and the entitlement's message when a team may not deploy.
pub(crate) async fn check_deploy(
    conn: &mut sqlx::PgConnection,
    team_id: &str,
) -> error::ApiResult<()> {
    match ENTITLEMENTS.get() {
        Some(e) => e
            .can_deploy(conn, team_id)
            .await
            .map_err(|m| error::ApiError::new(axum::http::StatusCode::PAYMENT_REQUIRED, m)),
        None => Ok(()),
    }
}

/// The dashboard features a hosted build turned on (none when self-hosted).
pub(crate) fn features() -> &'static [String] {
    FEATURES.get().map(Vec::as_slice).unwrap_or(&[])
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
    } = ext;
    let _ = FEATURES.set(features);
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
    let db = PgPoolOptions::new()
        .max_connections(pool)
        .acquire_timeout(Duration::from_secs(10))
        // Connections a burst opened close after a minute idle: each one
        // is a Postgres process holding memory.
        .idle_timeout(Duration::from_secs(60))
        .connect_with(connect)
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
        cookie_secure: env("PYO_COOKIE_SECURE", "false") == "true" || web::tailnet_name().is_some(),
        allow_signup: env("PYO_ALLOW_SIGNUP", "false") == "true",
        builder_image: env("PYO_BUILDER_IMAGE", "127.0.0.1:5005/pyo/builder:dev"),
        tailnet_api_url: env("PYO_TAILNET_API_URL", ""),
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
        push: env("PYO_REGISTRY_PUSH", ""),
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
    if role.worker() {
        // Each loop runs on one worker at a time (leader lock); jobs on any.
        tokio::spawn(deploy::sweeper(state.clone()));
        tokio::spawn(deploy::dns_watcher(state.clone()));
        tokio::spawn(builders::watcher(state.clone()));
        tokio::spawn(github::poller(state.clone()));
        tokio::spawn(jobs::runner(state.clone()));
        tokio::spawn(backupstore::keeper(state.db.clone()));
        tokio::spawn(backup::runner(state.clone()));
        tokio::spawn(teambackup::runner(state.clone()));
        tokio::spawn(database_imports::runner(state.clone()));
        tokio::spawn(updates::runner(state.clone()));
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
                    .merge(cli::routes())
                    .layer(CompressionLayer::new())
                    .merge(terminal::routes())
                    .merge(database_imports::routes()),
            )
            .nest("/api/cli", cli::public_routes())
            .nest("/api/github", github::routes())
            .nest("/api/hooks", hooks::routes())
            .nest("/api/mcp", mcp::routes())
            .nest("/api/oauth", oauth::routes())
            .merge(oauth::well_known())
            .nest(
                "/api/agent/v1",
                agent::routes()
                    .merge(terminal::agent_routes())
                    .merge(database_imports::agent_routes()),
            )
            .nest("/api/builder/v1", builders::routes())
            .merge(extra_routes)
            .route("/install/pyo", get(cli::binary))
            .route("/install/cli.sh", get(cli::install_script))
            .route("/api/{*rest}", get(error::not_found).post(error::not_found))
            .route("/install/agent.yaml", get(install::manifest))
            .route("/install/gateway-crds.yaml", get(install::gateway_crds))
            .route("/install/this-machine", get(install::this_machine))
            .route("/install/join.sh", get(join::serve))
            .route("/install/connect.sh", get(join::serve_connect))
            .fallback_service(tower_http::compression::Compression::new(spa))
            .layer(axum::middleware::from_fn_with_state(
                state.config.cookie_secure,
                web::headers,
            ))
            .layer(axum::middleware::from_fn(web::redirect_to_https))
            .layer(DefaultBodyLimit::max(4 * 1024 * 1024))
            .layer(TraceLayer::new_for_http())
            .with_state(state)
    };

    // The registry's front door for teams' own images (reached over the
    // tailnet only; registrydoor.rs).
    if role.web() {
        let door: SocketAddr = env("PYO_REGISTRY_DOOR_ADDR", "0.0.0.0:5006").parse()?;
        let door_app = registrydoor::router(door_state);
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

/// Postgres's max_connections, which pyo-postgres sets by the server's
/// memory when it starts (platform.yaml): the pools follow it.
async fn max_connections(connect: &PgConnectOptions) -> anyhow::Result<u32> {
    use sqlx::Connection;
    let mut c = sqlx::PgConnection::connect_with(connect).await?;
    let max: String = sqlx::query_scalar("SHOW max_connections")
        .fetch_one(&mut c)
        .await?;
    let _ = c.close().await;
    Ok(max.parse()?)
}
