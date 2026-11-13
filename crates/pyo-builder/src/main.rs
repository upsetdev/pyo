//! Pyo builder: turns Git sources into images.
//!
//! Runs next to a BuildKit daemon (as a pod pinned to a server) and dials
//! out to the Pyo API: it long-polls for build jobs, resolves
//! the branch to a commit, checks the commit out into a temporary directory,
//! builds it (with the repository's Dockerfile or Containerfile, or
//! zero-config with Railpack), pushes to Pyo's registry, and streams the build output back
//! as the deployment's logs.
//!
//! With `PYO_BUILDKIT_WAKE` (a directory shared with BuildKit's container,
//! see the agent's builder.rs), BuildKit runs only while there are builds:
//! a build wakes it (a `wanted` file there; BuildKit's side marks it `up`)
//! and after `PYO_BUILDKIT_IDLE_SECS` (600) without one it's let go again.

mod git;
mod source;

use std::{process::Stdio, time::Duration};

use anyhow::Context;
use pyo_proto::{
    AGENT_VERSION_HEADER, BuildJob, BuildProgress, BuildResult, BuilderHeartbeat, LogLine, now_ms,
};
use reqwest::header::{AUTHORIZATION, HeaderMap, HeaderValue};
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    process::Command,
    sync::{Mutex, mpsc},
};
use tracing_subscriber::EnvFilter;

const VERSION: &str = env!("CARGO_PKG_VERSION");

struct Builder {
    http: reqwest::Client,
    /// Its token: also its login at Pyo's registry door (username `pyo-builder`).
    token: String,
    base: String,
    buildkit: String,
    buildctl: String,
    busy: Mutex<Option<String>>,
    /// Starting BuildKit on demand: where, and when it was last used.
    wake: Option<std::path::PathBuf>,
    last_used: Mutex<tokio::time::Instant>,
    idle: Duration,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| "pyo_builder=info".into()),
        )
        .init();
    let url = std::env::var("PYO_API_URL").context("PYO_API_URL is required")?;
    let token = std::env::var("PYO_BUILDER_TOKEN").context("PYO_BUILDER_TOKEN is required")?;
    let mut headers = HeaderMap::new();
    let mut auth = HeaderValue::from_str(&format!("Bearer {token}"))?;
    auth.set_sensitive(true);
    headers.insert(AUTHORIZATION, auth);
    headers.insert(AGENT_VERSION_HEADER, HeaderValue::from_static(VERSION));
    let builder = std::sync::Arc::new(Builder {
        http: reqwest::Client::builder()
            .default_headers(headers)
            .connect_timeout(Duration::from_secs(10))
            .build()?,
        token: token.clone(),
        base: format!("{}/api/builder/v1", url.trim_end_matches('/')),
        buildkit: std::env::var("BUILDKIT_HOST")
            .unwrap_or_else(|_| "unix:///run/buildkit/buildkitd.sock".into()),
        buildctl: std::env::var("BUILDCTL").unwrap_or_else(|_| "buildctl".into()),
        busy: Mutex::new(None),
        wake: std::env::var_os("PYO_BUILDKIT_WAKE")
            .filter(|v| !v.is_empty())
            .map(std::path::PathBuf::from),
        last_used: Mutex::new(tokio::time::Instant::now()),
        idle: Duration::from_secs(
            std::env::var("PYO_BUILDKIT_IDLE_SECS")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(600),
        ),
    });
    tracing::info!(version = VERSION, api = %url, on_demand = builder.wake.is_some(), "pyo builder starting");

    tokio::spawn(heartbeat_loop(builder.clone()));
    if builder.wake.is_some() {
        tokio::spawn(idle_loop(builder.clone()));
    }
    let mut backoff = Duration::from_secs(1);
    // Shutting down (a new version is replacing this pod, or the VM stops):
    // take no new jobs. BuildKit beside us is stopping too.
    let stopping = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    {
        let stopping = stopping.clone();
        tokio::spawn(async move {
            if let Ok(mut term) =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            {
                term.recv().await;
                tracing::info!("stopping: no new builds");
                stopping.store(true, std::sync::atomic::Ordering::SeqCst);
            }
        });
    }
    loop {
        if stopping.load(std::sync::atomic::Ordering::SeqCst) {
            return Ok(());
        }
        // Only take a job when BuildKit answers (it can start after us),
        // or is asleep until a build wakes it.
        if !builder.buildkit_ok().await {
            tokio::time::sleep(Duration::from_secs(2)).await;
            continue;
        }
        match builder.next_job().await {
            Ok(Some(job)) => {
                backoff = Duration::from_secs(1);
                *builder.busy.lock().await = Some(job.id.clone());
                let result = builder.build(&job).await.unwrap_or_else(|e| BuildResult {
                    ok: false,
                    message: Some(format!("{e:#}")),
                    ..Default::default()
                });
                *builder.busy.lock().await = None;
                if let Err(e) = builder
                    .post(&format!("/jobs/{}/result", job.id), &result)
                    .await
                {
                    tracing::warn!(job = %job.id, error = %e, "couldn't report the build result");
                }
            }
            Ok(None) => backoff = Duration::from_secs(1),
            Err(e) => {
                tracing::warn!(error = %e, retry_in = ?backoff, "couldn't reach the Pyo API");
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(Duration::from_secs(30));
            }
        }
    }
}

async fn heartbeat_loop(b: std::sync::Arc<Builder>) {
    let mut tick = tokio::time::interval(Duration::from_secs(15));
    loop {
        tick.tick().await;
        let buildkit = b.buildkit_ok().await;
        let hb = BuilderHeartbeat {
            version: VERSION.into(),
            buildkit,
            cpus: std::thread::available_parallelism()
                .map(|n| n.get() as u32)
                .unwrap_or(1),
            memory_gi: memory_gi(),
            busy_with: b.busy.lock().await.clone(),
            hostname: std::fs::read_to_string("/etc/hostname")
                .ok()
                .map(|h| h.trim().to_owned())
                .filter(|h| !h.is_empty()),
        };
        if let Err(e) = b.post("/heartbeat", &hb).await {
            tracing::warn!(error = %e, "heartbeat not accepted");
        }
    }
}

/// Lets BuildKit go after a while without builds. Under the busy lock, so
/// a build can't start in between; held a little longer, so BuildKit's
/// side has seen it (and taken `up` away) before the next build wakes it.
async fn idle_loop(b: std::sync::Arc<Builder>) {
    let Some(dir) = b.wake.clone() else { return };
    let mut tick = tokio::time::interval(Duration::from_secs(15));
    loop {
        tick.tick().await;
        let busy = b.busy.lock().await;
        if busy.is_none()
            && dir.join("wanted").exists()
            && b.last_used.lock().await.elapsed() >= b.idle
        {
            match std::fs::remove_file(dir.join("wanted")) {
                Ok(()) => {
                    tracing::info!(idle = ?b.idle, "no builds for a while: stopping BuildKit");
                    tokio::time::sleep(Duration::from_secs(3)).await;
                }
                Err(e) => tracing::warn!(error = %e, "couldn't stop BuildKit"),
            }
        }
        drop(busy);
    }
}

fn memory_gi() -> f64 {
    let kb = std::fs::read_to_string("/proc/meminfo")
        .ok()
        .and_then(|m| {
            m.lines()
                .find(|l| l.starts_with("MemTotal:"))
                .and_then(|l| l.split_whitespace().nth(1))
                .and_then(|v| v.parse::<f64>().ok())
        })
        .unwrap_or(0.0);
    (kb / 1048576.0 * 10.0).round() / 10.0
}

fn line(level: &str, source: &str, message: &str) -> LogLine {
    LogLine {
        at: now_ms(),
        level: level.into(),
        source: source.into(),
        message: message.chars().take(4000).collect(),
    }
}

impl Builder {
    /// Does BuildKit answer? (`buildctl debug workers`, with a timeout.)
    async fn buildkit_ready(&self) -> bool {
        let check = Command::new(&self.buildctl)
            .args(["--addr", &self.buildkit, "debug", "workers"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .status();
        matches!(tokio::time::timeout(Duration::from_secs(10), check).await, Ok(Ok(st)) if st.success())
    }

    /// Fine to take builds: BuildKit answers, or it's started on demand and
    /// isn't wanted right now (asleep), or a build is waking it.
    async fn buildkit_ok(&self) -> bool {
        if let Some(dir) = &self.wake
            && (!dir.join("wanted").exists() || self.busy.lock().await.is_some())
        {
            return true;
        }
        self.buildkit_ready().await
    }

    /// Wake BuildKit for a build. True if it was asleep (it's starting).
    async fn wake_buildkit(&self) -> anyhow::Result<bool> {
        let Some(dir) = &self.wake else {
            return Ok(false);
        };
        *self.last_used.lock().await = tokio::time::Instant::now();
        let wanted = dir.join("wanted");
        let asleep = !wanted.exists() || !dir.join("up").exists();
        if !wanted.exists() {
            std::fs::write(&wanted, b"")
                .with_context(|| format!("couldn't start BuildKit ({})", wanted.display()))?;
        }
        Ok(asleep)
    }

    /// Wait for BuildKit to answer: started (`up`) and ready, for up to 2 minutes.
    async fn wait_buildkit(&self) -> bool {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(120);
        loop {
            let up = self.wake.as_ref().is_none_or(|d| d.join("up").exists());
            if up && self.buildkit_ready().await {
                return true;
            }
            if tokio::time::Instant::now() > deadline {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }

    async fn post<T: serde::Serialize + ?Sized>(&self, path: &str, body: &T) -> anyhow::Result<()> {
        let res = self
            .http
            .post(format!("{}{path}", self.base))
            .timeout(Duration::from_secs(20))
            .json(body)
            .send()
            .await?;
        if !res.status().is_success() {
            anyhow::bail!("{}: {}", res.status(), res.text().await.unwrap_or_default());
        }
        Ok(())
    }

    async fn next_job(&self) -> anyhow::Result<Option<BuildJob>> {
        let res = self
            .http
            .get(format!("{}/jobs?wait=25", self.base))
            .timeout(Duration::from_secs(40))
            .send()
            .await?;
        if !res.status().is_success() {
            anyhow::bail!("{}: {}", res.status(), res.text().await.unwrap_or_default());
        }
        Ok(res.json().await?)
    }

    async fn progress(&self, job: &str, p: BuildProgress) {
        if let Err(e) = self.post(&format!("/jobs/{job}/progress"), &p).await {
            tracing::warn!(%job, error = %e, "progress not delivered");
        }
    }

    async fn build(&self, job: &BuildJob) -> anyhow::Result<BuildResult> {
        tracing::info!(job = %job.id, repo = %job.repo_url, branch = %job.branch, "build");
        // BuildKit starts while the code is checked out.
        if self.wake_buildkit().await? {
            self.progress(
                &job.id,
                BuildProgress {
                    stage: "building".into(),
                    logs: vec![line("info", "builder", "Starting the builder…")],
                    ..Default::default()
                },
            )
            .await;
        }
        let result = self.build_inner(job).await;
        *self.last_used.lock().await = tokio::time::Instant::now();
        result
    }

    async fn build_inner(&self, job: &BuildJob) -> anyhow::Result<BuildResult> {
        let commit = if job.commit.is_empty() {
            match git::resolve(&job.repo_url, &job.branch).await {
                Ok(c) => c,
                Err(e) => {
                    return Ok(BuildResult {
                        ok: false,
                        message: Some(format!("{e:#}")),
                        ..Default::default()
                    });
                }
            }
        } else {
            job.commit.clone()
        };
        let short = &commit[..commit.len().min(12)];
        self.progress(
            &job.id,
            BuildProgress {
                stage: "building".into(),
                commit: Some(commit.clone()),
                logs: vec![line(
                    "info",
                    "builder",
                    &format!("Building {}@{} ({})", job.repo_url, short, job.branch),
                )],
            },
        )
        .await;

        // A temporary checkout, deleted when `ws` drops at the end of the build.
        let ws = source::Workspace::new(&job.id)?;
        let fail = |commit: &str, message: String| {
            Ok(BuildResult {
                ok: false,
                commit: commit.to_owned(),
                message: Some(message),
                ..Default::default()
            })
        };
        if let Err(e) = source::checkout(
            &job.repo_url,
            &job.branch,
            &commit,
            &job.build_context,
            &ws.src(),
            job.clone_token.as_deref(),
        )
        .await
        {
            return fail(&commit, format!("Couldn't get the code: {e:#}"));
        }
        let app_dir = match source::inside(&ws.src(), &job.build_context) {
            Ok(d) if d.is_dir() => d,
            Ok(_) => {
                return fail(
                    &commit,
                    format!(
                        "The repository has no {} directory at this commit. Check the build context.",
                        job.build_context
                    ),
                );
            }
            Err(e) => return fail(&commit, format!("{e:#}")),
        };
        // The path in the settings, or by default a Dockerfile, else a Containerfile.
        let build_file = match source::build_file(&app_dir, &job.dockerfile_path) {
            Ok(f) => f,
            Err(e) => return fail(&commit, format!("{e:#}")),
        };
        let default_dockerfile = source::default_path(&job.dockerfile_path);

        if !self.wait_buildkit().await {
            return fail(
                &commit,
                "The builder's BuildKit didn't start within 2 minutes. Try the build again.".into(),
            );
        }
        let tag = format!("{}:{short}", job.push_repository);
        let meta = ws.root.join("metadata.json");
        let mut cmd = Command::new(&self.buildctl);
        cmd.args(["--addr", &self.buildkit, "build", "--progress", "plain"])
            .arg("--local")
            .arg(format!("context={}", app_dir.display()));
        let detected: String;
        if let Some(dockerfile) = build_file {
            // The repository says how to build it.
            let name = dockerfile
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| "Dockerfile".into());
            detected = if name == "Containerfile" {
                "Containerfile"
            } else {
                "Dockerfile"
            }
            .into();
            let shown = dockerfile
                .strip_prefix(&app_dir)
                .map(|p| p.display().to_string())
                .unwrap_or_else(|_| name.clone());
            cmd.args(["--frontend", "dockerfile.v0"])
                .arg("--local")
                .arg(format!(
                    "dockerfile={}",
                    dockerfile.parent().unwrap_or(&app_dir).display()
                ))
                .args(["--opt", &format!("filename={name}")]);
            for a in &job.build_args {
                cmd.args(["--opt", &format!("build-arg:{}={}", a.key, a.value)]);
            }
            self.progress(
                &job.id,
                BuildProgress {
                    logs: vec![line(
                        "info",
                        "builder",
                        &format!("Using the {detected} at {shown}"),
                    )],
                    ..Default::default()
                },
            )
            .await;
        } else if default_dockerfile {
            // Zero config: Railpack reads the code and plans the build,
            // following links, so none may lead outside the repository.
            match source::remove_outside_links(&ws.src(), &app_dir) {
                Ok(removed) if !removed.is_empty() => {
                    let list = removed
                        .iter()
                        .map(|p| p.display().to_string())
                        .collect::<Vec<_>>()
                        .join(", ");
                    self.progress(
                        &job.id,
                        BuildProgress {
                            logs: vec![line(
                                "warn",
                                "builder",
                                &format!(
                                    "Left out symbolic links to outside the repository: {list}"
                                ),
                            )],
                            ..Default::default()
                        },
                    )
                    .await;
                }
                Ok(_) => {}
                Err(e) => {
                    return fail(
                        &commit,
                        format!("Couldn't check the repository's links: {e:#}"),
                    );
                }
            }
            let plan = ws.plan().join("railpack-plan.json");
            let info = ws.plan().join("railpack-info.json");
            let mut prep = Command::new("railpack");
            prep.arg("prepare")
                .arg(&app_dir)
                .arg("--plan-out")
                .arg(&plan)
                .arg("--info-out")
                .arg(&info);
            for a in &job.build_args {
                prep.args(["--env", &format!("{}={}", a.key, a.value)]);
            }
            let out = match tokio::time::timeout(
                Duration::from_secs(300),
                prep.stdin(Stdio::null()).output(),
            )
            .await
            {
                Ok(Ok(o)) => o,
                Ok(Err(e)) => return fail(&commit, format!("Couldn't run railpack: {e}")),
                Err(_) => {
                    return fail(
                        &commit,
                        "Detecting how to build the service took longer than 5 minutes.".into(),
                    );
                }
            };
            let printed: Vec<LogLine> = String::from_utf8_lossy(&out.stdout)
                .lines()
                .chain(String::from_utf8_lossy(&out.stderr).lines())
                .filter(|l| !l.trim().is_empty())
                .map(|l| line("info", "railpack", l))
                .collect();
            self.progress(
                &job.id,
                BuildProgress {
                    logs: printed,
                    ..Default::default()
                },
            )
            .await;
            let info_json: serde_json::Value = std::fs::read_to_string(&info)
                .ok()
                .and_then(|s| serde_json::from_str(&s).ok())
                .unwrap_or_default();
            if !out.status.success() || !plan.is_file() {
                return fail(
                    &commit,
                    "Pyo couldn't tell how to build this repository: there's no Dockerfile or Containerfile, and Railpack didn't recognize the language. Add a Dockerfile, or check the build context.".into(),
                );
            }
            detected = source::label(&info_json, &app_dir).unwrap_or_else(|| "Railpack".into());
            self.progress(&job.id, BuildProgress { logs: vec![line("info", "builder", &format!("No Dockerfile or Containerfile. Building it as {detected} with Railpack"))], ..Default::default() }).await;
            let cache_key = job
                .push_repository
                .rsplit('/')
                .next()
                .unwrap_or("app")
                .to_owned();
            cmd.args([
                "--frontend",
                "gateway.v0",
                "--opt",
                &format!("source={}", source::railpack_frontend()),
            ])
            .arg("--local")
            .arg(format!("dockerfile={}", ws.plan().display()))
            .args([
                "--opt",
                "filename=railpack-plan.json",
                "--opt",
                &format!("build-arg:cache-key={cache_key}"),
            ]);
            // Build-time variables reach the plan as secrets.
            for a in &job.build_args {
                cmd.args(["--secret", &format!("id={0},env={0}", a.key)])
                    .env(&a.key, &a.value);
            }
        } else {
            return fail(
                &commit,
                format!(
                    "There's no Dockerfile at {}. Fix the path in the service's settings, or clear it to let Pyo detect how to build the service.",
                    job.dockerfile_path
                ),
            );
        }
        cmd.args([
            "--output",
            &format!("type=image,name={tag},push=true,registry.insecure=true"),
        ])
        .arg("--metadata-file")
        .arg(&meta);
        // Pushed through Pyo's registry door, logged in as this builder.
        let docker = ws.root.join("docker");
        std::fs::create_dir_all(&docker)?;
        std::fs::write(
            docker.join("config.json"),
            docker_config(&job.registry_host, &self.token),
        )?;
        cmd.env("DOCKER_CONFIG", &docker);
        let mut child = cmd
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .context("couldn't start buildctl")?;

        // Merge stdout and stderr, forward in batches.
        let (tx, mut rx) = mpsc::channel::<String>(1024);
        for pipe in [
            child
                .stdout
                .take()
                .map(|p| Box::new(p) as Box<dyn tokio::io::AsyncRead + Unpin + Send>),
            child
                .stderr
                .take()
                .map(|p| Box::new(p) as Box<dyn tokio::io::AsyncRead + Unpin + Send>),
        ]
        .into_iter()
        .flatten()
        {
            let tx = tx.clone();
            tokio::spawn(async move {
                let mut lines = BufReader::new(pipe).lines();
                while let Ok(Some(l)) = lines.next_line().await {
                    if tx.send(l).await.is_err() {
                        break;
                    }
                }
            });
        }
        drop(tx);

        let deadline =
            tokio::time::Instant::now() + Duration::from_secs(job.timeout_seconds.max(60));
        let mut batch: Vec<LogLine> = vec![];
        let mut pushing = false;
        let mut tail: Vec<String> = vec![];
        let mut flush = tokio::time::interval(Duration::from_millis(1000));
        loop {
            tokio::select! {
                l = rx.recv() => match l {
                    Some(l) => {
                        let plain = pyo_proto::ansi::strip(&l);
                        let level = if plain.contains("ERROR") || plain.starts_with("error:") { "error" } else { "info" };
                        if !pushing && (l.contains("pushing layers") || l.contains("exporting to image") || l.contains("pushing manifest")) {
                            pushing = true;
                            self.progress(&job.id, BuildProgress { stage: "pushing".into(), logs: std::mem::take(&mut batch), commit: None }).await;
                        }
                        // For the failure's reason: words only.
                        tail.push(plain.into_owned());
                        if tail.len() > 30 { tail.remove(0); }
                        batch.push(line(level, "build", &l));
                        if batch.len() >= 200 {
                            self.progress(&job.id, BuildProgress { logs: std::mem::take(&mut batch), ..Default::default() }).await;
                        }
                    }
                    None => break,
                },
                _ = flush.tick() => {
                    if !batch.is_empty() {
                        self.progress(&job.id, BuildProgress { logs: std::mem::take(&mut batch), ..Default::default() }).await;
                    }
                }
                _ = tokio::time::sleep_until(deadline) => {
                    let _ = child.kill().await;
                    return Ok(BuildResult { ok: false, commit, message: Some(format!("The build took longer than {} minutes.", job.timeout_seconds / 60)), ..Default::default() });
                }
            }
        }
        if !batch.is_empty() {
            self.progress(
                &job.id,
                BuildProgress {
                    logs: batch,
                    ..Default::default()
                },
            )
            .await;
        }
        let status = child.wait().await?;
        if !status.success() {
            // BuildKit's last error line says why (missing Dockerfile, failed RUN, …).
            let why = tail
                .iter()
                .rev()
                .find(|l| l.contains("ERROR") || l.contains("error:"))
                .cloned()
                .unwrap_or_else(|| "The build failed.".into());
            return Ok(BuildResult {
                ok: false,
                commit,
                message: Some(explain(why.trim_start_matches("error: "))),
                ..Default::default()
            });
        }
        let meta_json = std::fs::read_to_string(&meta)
            .ok()
            .and_then(|m| serde_json::from_str::<serde_json::Value>(&m).ok())
            .unwrap_or_default();
        let digest = meta_json["containerimage.digest"]
            .as_str()
            .map(str::to_owned);
        let exposed_ports = match meta_json["containerimage.config.digest"].as_str() {
            Some(cfg) => {
                exposed_ports(&job.registry_host, &self.token, &job.push_repository, cfg).await
            }
            None => vec![],
        };
        Ok(BuildResult {
            ok: digest.is_some(),
            commit: commit.clone(),
            digest,
            tag: Some(short.to_owned()),
            message: None,
            exposed_ports,
            detected: Some(detected),
        })
    }
}

/// BuildKit's client login (`$DOCKER_CONFIG/config.json`) for the door.
fn docker_config(registry: &str, token: &str) -> String {
    use base64::Engine;
    let auth = base64::engine::general_purpose::STANDARD.encode(format!("pyo-builder:{token}"));
    serde_json::json!({ "auths": { registry: { "auth": auth } } }).to_string()
}

/// EXPOSE'd ports, from the image config blob in the registry we pushed to.
async fn exposed_ports(
    registry: &str,
    token: &str,
    repository: &str,
    config_digest: &str,
) -> Vec<u16> {
    let path = repository
        .strip_prefix(registry)
        .unwrap_or(repository)
        .trim_start_matches('/');
    let url = format!("http://{registry}/v2/{path}/blobs/{config_digest}");
    let Ok(res) = reqwest::Client::new()
        .get(&url)
        .basic_auth("pyo-builder", Some(token))
        .timeout(Duration::from_secs(20))
        .send()
        .await
    else {
        return vec![];
    };
    let Ok(cfg) = res.json::<serde_json::Value>().await else {
        return vec![];
    };
    let mut ports: Vec<u16> = cfg["config"]["ExposedPorts"]
        .as_object()
        .map(|o| {
            o.keys()
                .filter(|k| k.ends_with("/tcp") || !k.contains('/'))
                .filter_map(|k| k.split('/').next()?.parse().ok())
                .collect()
        })
        .unwrap_or_default();
    ports.sort_unstable();
    ports
}

/// BuildKit's errors in plain words, for the ones people hit most.
fn explain(why: &str) -> String {
    // COPY/ADD of a path that isn't in the repository at this commit.
    if why.contains("failed to calculate checksum")
        && let Some(path) = why
            .rsplit_once("\": not found")
            .and_then(|(head, _)| head.rsplit_once('"'))
            .map(|(_, p)| p)
    {
        return format!(
            "The Dockerfile copies {path}, but the repository doesn't have it at this commit. Commit it, or check that .gitignore doesn't leave it out."
        );
    }
    if why.contains("failed to read dockerfile") || why.contains("Dockerfile: no such file") {
        return "There's no Dockerfile at the path set in the service's settings. Check the Dockerfile path and build context.".into();
    }
    why.to_owned()
}

#[cfg(test)]
mod login_tests {
    #[test]
    fn docker_config_logs_in_at_the_door() {
        let v: serde_json::Value =
            serde_json::from_str(&super::docker_config("100.64.0.1:5005", "abc")).unwrap();
        assert_eq!(
            v["auths"]["100.64.0.1:5005"]["auth"],
            "cHlvLWJ1aWxkZXI6YWJj"
        );
    }
}

#[cfg(test)]
mod explain_tests {
    #[test]
    fn explains_missing_copy_source() {
        let raw = r#"failed to solve: failed to compute cache key: failed to calculate checksum of ref x9kr::pfwk: "/data": not found"#;
        assert!(super::explain(raw).starts_with("The Dockerfile copies /data,"));
        assert_eq!(super::explain("something else"), "something else");
    }
}
