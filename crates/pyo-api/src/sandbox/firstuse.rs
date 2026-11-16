//! A template made on first use (Pyo's `base`, see [`super::base`]): the
//! first sandbox that needs it on a server has the server make it, waits
//! until it's ready, then starts. Later ones there start from that copy.
//!
//! - The create waits up to [`LIMIT`], under what HTTP clients wait for an
//!   answer, so `Sandbox.create()` just takes longer the first time. Past
//!   that the server keeps making it, and the create says to try again.
//! - Two creates at once share one build: they're placed on the server
//!   already making it ([`super::scheduler::place`]), the server makes a
//!   template it has only once, and a failed copy is cleared and made
//!   again by one create at a time (a lock per server and template).
//! - What the server says is saved in its report at once, so the
//!   templates page and the scheduler see the build before the next round.

use std::future::Future;
use std::time::{Duration, Instant};

use pyo_proto::sandbox::TemplateState;
use pyo_proto::sandbox::token::SigningKey;
use pyo_proto::sandbox::{DaemonCommand, DaemonError, DaemonResult, ErrorCode, TemplateReport};
use sqlx::PgPool;

use super::{daemon::Daemon, keys, templates};
use crate::error::ApiError;

/// How long a create waits for its server to make the template.
pub const LIMIT: Duration = Duration::from_secs(240);
/// How often it asks the server how far it got.
const EVERY: Duration = Duration::from_secs(1);

/// A server, as the first use sees it.
pub(crate) trait Host {
    type Guard: Send;
    /// One create at a time clears a failed copy and starts the build.
    fn lock(&self) -> impl Future<Output = Result<Self::Guard, ApiError>> + Send;
    /// Starts making the template unless the server has it (ready,
    /// building or failed), and says how far it got.
    fn prepare(&self) -> impl Future<Output = Result<TemplateReport, DaemonError>> + Send;
    /// Deletes the server's copy.
    fn delete(&self) -> impl Future<Output = Result<(), DaemonError>> + Send;
}

/// Why the template isn't ready on the server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NotReady {
    /// The build failed, and why.
    Failed(String),
    /// Still being made after the limit.
    Slow,
    /// The server didn't answer, or refused.
    Server(DaemonError),
}

impl NotReady {
    /// As the create answers it.
    pub fn error(self, template: &str, server: &str) -> ApiError {
        match self {
            NotReady::Failed(why) => ApiError::new(
                axum::http::StatusCode::BAD_GATEWAY,
                format!(
                    "{server} couldn't get {template} ready: {}",
                    why.trim_end_matches('.')
                ) + ".",
            ),
            NotReady::Slow => ApiError::new(
                axum::http::StatusCode::SERVICE_UNAVAILABLE,
                format!("{template} is still getting ready on {server}. Try again in a minute."),
            ),
            NotReady::Server(e) => super::daemon_error(e),
        }
    }
}

impl From<DaemonError> for NotReady {
    fn from(e: DaemonError) -> Self {
        NotReady::Server(e)
    }
}

/// Has `host` make the template unless it has it, then waits until it's
/// ready, up to `limit`, asking every `every`.
pub(crate) async fn get_ready<H: Host>(
    host: &H,
    limit: Duration,
    every: Duration,
) -> Result<(), NotReady> {
    let deadline = Instant::now() + limit;
    let mut r = {
        let _one_at_a_time = host
            .lock()
            .await
            .map_err(|e| NotReady::Server(DaemonError::new(ErrorCode::Internal, e.message)))?;
        let r = host.prepare().await?;
        if r.state == TemplateState::Failed {
            // Tried before and failed: try again.
            host.delete().await?;
            host.prepare().await?
        } else {
            r
        }
    };
    loop {
        match r.state {
            TemplateState::Ready => return Ok(()),
            TemplateState::Failed => {
                let why = r.build.and_then(|b| b.error);
                return Err(NotReady::Failed(
                    why.unwrap_or_else(|| "it failed to build".into()),
                ));
            }
            TemplateState::Building => {}
        }
        if Instant::now() >= deadline {
            return Err(NotReady::Slow);
        }
        tokio::time::sleep(every).await;
        r = host.prepare().await?;
    }
}

/// A sandbox server over the tailnet, for one template.
struct Server<'a> {
    db: &'a PgPool,
    id: &'a str,
    daemon: Daemon,
    token: String,
    prepare: DaemonCommand,
    name: String,
}

impl Server<'_> {
    /// Keeps what the server said in its report, in place of what it said
    /// of this template before.
    async fn save(&self, r: Option<&TemplateReport>) {
        let res = sqlx::query(
            "UPDATE pyo.sandbox_servers
                SET templates = coalesce((SELECT jsonb_agg(x) FROM jsonb_array_elements(templates) x
                                           WHERE x->>'name' <> $2), '[]'::jsonb)
                                || coalesce($3::jsonb, '[]'::jsonb)
              WHERE id = $1",
        )
        .bind(self.id)
        .bind(&self.name)
        .bind(r.map(|r| serde_json::json!([r])))
        .execute(self.db)
        .await;
        if let Err(e) = res {
            tracing::warn!(server = %self.id, template = %self.name, error = %e, "couldn't save how far the template got");
        }
    }
}

impl Host for Server<'_> {
    type Guard = sqlx::Transaction<'static, sqlx::Postgres>;

    async fn lock(&self) -> Result<Self::Guard, ApiError> {
        let mut tx = self.db.begin().await?;
        sqlx::query(
            "SELECT pg_advisory_xact_lock(hashtext('pyo-sandbox-template:' || $1 || ':' || $2))",
        )
        .bind(self.id)
        .bind(&self.name)
        .execute(&mut *tx)
        .await?;
        Ok(tx)
    }

    async fn prepare(&self) -> Result<TemplateReport, DaemonError> {
        match self.daemon.command(&self.token, &self.prepare).await? {
            DaemonResult::Template { template } => {
                self.save(Some(&template)).await;
                Ok(template)
            }
            other => Err(DaemonError::new(
                ErrorCode::Internal,
                format!("The sandbox server answered with something else: {other:?}"),
            )),
        }
    }

    async fn delete(&self) -> Result<(), DaemonError> {
        let cmd = DaemonCommand::DeleteTemplate {
            template: self.name.clone(),
        };
        self.daemon.command(&self.token, &cmd).await?;
        self.save(None).await;
        Ok(())
    }
}

/// Has server `server_id` (at `address`) make template `template_id`
/// under `name` (its current revision) and waits until it's ready there.
pub async fn on_server(
    db: &PgPool,
    key: &SigningKey,
    server_id: &str,
    address: &str,
    template_id: &str,
    name: &str,
) -> Result<(), NotReady> {
    let t = sqlx::query_as::<_, templates::Template>(
        "SELECT id, team_id, source, revision FROM pyo.sandbox_templates WHERE id = $1",
    )
    .bind(template_id)
    .fetch_optional(db)
    .await
    .map_err(|e| NotReady::Server(DaemonError::new(ErrorCode::Internal, e.to_string())))?
    .ok_or_else(|| NotReady::Failed("it was deleted".into()))?;
    // Pyo's registry has no tailnet address yet (it just started): the
    // create says to try again in a minute.
    let prepare = templates::prepare_command(key, &t).ok_or(NotReady::Slow)?;
    let host = Server {
        db,
        id: server_id,
        daemon: Daemon::new(address),
        token: keys::admin_token(key, server_id),
        prepare,
        name: name.to_owned(),
    };
    tracing::info!(server = %server_id, template = %name, "a sandbox needs a template its server doesn't have yet: making it");
    get_ready(&host, LIMIT, EVERY).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use pyo_proto::sandbox::TemplateBuild;
    use std::sync::Arc;
    use std::sync::Mutex as StdMutex;

    /// A server that makes a template in `polls` asks, or fails to.
    #[derive(Default)]
    struct Fake {
        /// The template's state on the server, and asks since its build
        /// started. None: it has none.
        state: StdMutex<Option<(TemplateState, u32)>>,
        builds: StdMutex<u32>,
        deletes: StdMutex<u32>,
        polls: u32,
        fails: Option<String>,
        lock: Arc<tokio::sync::Mutex<()>>,
    }

    impl Fake {
        fn report(&self, state: TemplateState) -> TemplateReport {
            TemplateReport {
                name: "stp_base.aaaa".into(),
                state,
                build: Some(TemplateBuild {
                    error: (state == TemplateState::Failed)
                        .then(|| self.fails.clone().unwrap_or_default()),
                    ..Default::default()
                }),
                ..Default::default()
            }
        }
    }

    impl Host for Fake {
        type Guard = tokio::sync::OwnedMutexGuard<()>;

        async fn lock(&self) -> Result<Self::Guard, ApiError> {
            Ok(self.lock.clone().lock_owned().await)
        }

        async fn prepare(&self) -> Result<TemplateReport, DaemonError> {
            // A real server answers over the network: others get a turn.
            tokio::task::yield_now().await;
            let mut st = self.state.lock().unwrap();
            let next = match *st {
                None => {
                    *self.builds.lock().unwrap() += 1;
                    (TemplateState::Building, 0)
                }
                Some((TemplateState::Building, n)) if n + 1 >= self.polls => match self.fails {
                    Some(_) => (TemplateState::Failed, n + 1),
                    None => (TemplateState::Ready, n + 1),
                },
                Some((TemplateState::Building, n)) => (TemplateState::Building, n + 1),
                Some(done) => done,
            };
            *st = Some(next);
            Ok(self.report(next.0))
        }

        async fn delete(&self) -> Result<(), DaemonError> {
            tokio::task::yield_now().await;
            *self.deletes.lock().unwrap() += 1;
            *self.state.lock().unwrap() = None;
            Ok(())
        }
    }

    const QUICK: Duration = Duration::from_millis(1);

    #[tokio::test]
    async fn makes_it_then_waits_until_ready() {
        let f = Fake {
            polls: 3,
            ..Default::default()
        };
        get_ready(&f, LIMIT, QUICK).await.unwrap();
        assert_eq!(*f.builds.lock().unwrap(), 1);
        // The next create finds it ready, with no build.
        get_ready(&f, LIMIT, QUICK).await.unwrap();
        assert_eq!(*f.builds.lock().unwrap(), 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn two_creates_at_once_start_one_build() {
        let f = Arc::new(Fake {
            polls: 5,
            ..Default::default()
        });
        let (a, b) = tokio::join!(get_ready(&*f, LIMIT, QUICK), get_ready(&*f, LIMIT, QUICK));
        a.unwrap();
        b.unwrap();
        assert_eq!(*f.builds.lock().unwrap(), 1);
        // Many at once, after a failed build: one clears it and starts
        // again, the rest wait on that build.
        let f = Arc::new(Fake {
            polls: 3,
            state: StdMutex::new(Some((TemplateState::Failed, 0))),
            ..Default::default()
        });
        let all = futures::future::join_all((0..8).map(|_| {
            let f = f.clone();
            tokio::spawn(async move { get_ready(&*f, LIMIT, QUICK).await })
        }))
        .await;
        for r in all {
            r.unwrap().unwrap();
        }
        assert_eq!(
            (*f.builds.lock().unwrap(), *f.deletes.lock().unwrap()),
            (1, 1)
        );
    }

    #[tokio::test]
    async fn a_failed_build_says_why_and_is_tried_again_next_time() {
        let f = Fake {
            polls: 2,
            fails: Some("no space left on device".into()),
            ..Default::default()
        };
        let e = get_ready(&f, LIMIT, QUICK).await.unwrap_err();
        assert_eq!(e, NotReady::Failed("no space left on device".into()));
        let api = e.error("base", "sandbox-1");
        assert_eq!(
            api.message,
            "sandbox-1 couldn't get base ready: no space left on device."
        );
        assert_eq!(api.status, axum::http::StatusCode::BAD_GATEWAY);
        get_ready(&f, LIMIT, QUICK).await.unwrap_err();
        assert_eq!(
            (*f.builds.lock().unwrap(), *f.deletes.lock().unwrap()),
            (2, 1)
        );
    }

    #[tokio::test]
    async fn past_the_limit_it_says_to_try_again() {
        let f = Fake {
            polls: u32::MAX,
            ..Default::default()
        };
        let e = get_ready(&f, Duration::from_millis(20), QUICK)
            .await
            .unwrap_err();
        assert_eq!(e, NotReady::Slow);
        let api = e.error("base", "sandbox-1");
        assert_eq!(
            api.message,
            "base is still getting ready on sandbox-1. Try again in a minute."
        );
        assert_eq!(api.status, axum::http::StatusCode::SERVICE_UNAVAILABLE);
        assert!(!api.message.contains(';'));
    }
}
