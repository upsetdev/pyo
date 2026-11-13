//! Moving a team's services from another platform (Coolify and Dokploy,
//! self hosted, or Railway and Vercel) into Pyo, read through that
//! platform's own API with a token the user gives. Two steps, like a Compose import:
//!
//! - **Preview** (`POST /platform-imports/preview`): read what the token can
//!   see and show what each thing becomes in Pyo, with a warning for what
//!   doesn't carry over. Nothing is created. Variables' values aren't sent
//!   back, only their names.
//! - **Import** (`POST /platform-imports`): read it all again (the token is
//!   never kept) and create what the user picked: projects, services with
//!   their variables, domains, ports and empty volumes, and databases. A
//!   Postgres or MySQL database the platform makes reachable from outside is
//!   copied through the database import (database_imports.rs). Services are
//!   saved without deploying unless asked.
//!
//! Safety: the address is checked and every connection to it guarded by
//! outbound.rs (never this machine, the cluster's networks or metadata
//! services). Railway and Vercel have no address to give: Pyo talks only to
//! their official API hosts. The token goes only to that address. It is never logged or
//! stored, and errors never pass on what the platform answered. Owners and
//! admins only, and rate limited.
//!
//! Idempotent: each service records where it came from in
//! `source.importedFrom` (`platform`, `host`, `id`, `project`), so a second
//! run finds it and leaves it alone, and adds new services to the project
//! the first run made. Databases are matched by name in the team.
//!
//! Each platform has a module that walks its API (`gather`, every answer
//! kept by its path) and maps those answers to a plan (`plan`, a pure
//! function, tested against recorded answers in tests/fixtures/platforms).

mod coolify;
mod dokploy;
mod railway;
mod vercel;

use std::collections::{BTreeMap, HashSet};
use std::time::Duration;

use axum::{Json, Router, extract::State, http::StatusCode, routing::post};
use reqwest::Url;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::{Postgres, Transaction};

use crate::{
    AppState,
    auth::{KeyAccess, SessionUser, User},
    compose::{ComposePlan, PlanEnv, PlanPort, PlanResources, PlanService, PlanVolume},
    error::{ApiError, ApiResult, ErrorBody},
    ids::new_id,
    ops::{Role, require_role},
    outbound::{self, Reach, Refused},
    ratelimit,
};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/platform-imports/preview", post(preview))
        .route("/platform-imports", post(apply))
}

/// Previews and imports per person, in 15 minutes: each one reads the
/// whole platform.
const LIMIT: ratelimit::Limit = ratelimit::Limit {
    scope: "platform-import:user",
    max: 30,
    window_secs: 15 * 60,
};

/// One request to the platform.
const TIMEOUT: Duration = Duration::from_secs(20);
/// The most of one answer read.
const MAX_BODY: usize = 16 << 20;
/// The most requests one preview or import makes.
const MAX_REQUESTS: usize = 600;
/// Default size of a volume made from one of theirs, in GiB.
const VOLUME_GI: i64 = 10;

// ── The plan ─────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum Platform {
    Coolify,
    Dokploy,
    Railway,
    Vercel,
}

impl Platform {
    pub fn label(self) -> &'static str {
        match self {
            Platform::Coolify => "Coolify",
            Platform::Dokploy => "Dokploy",
            Platform::Railway => "Railway",
            Platform::Vercel => "Vercel",
        }
    }

    /// The platform's own API, for a hosted one: the only address Pyo
    /// sends its token to. None for a self-hosted one (the user gives it).
    pub fn official_api(self) -> Option<&'static str> {
        match self {
            // https://docs.railway.com/integrations/api
            Platform::Railway => Some(railway::API),
            // https://openapi.vercel.sh/ (`servers`)
            Platform::Vercel => Some(vercel::API),
            Platform::Coolify | Platform::Dokploy => None,
        }
    }
}

/// What importing would create, project by project.
#[derive(Debug, Clone, Default, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ImportPlan {
    /// "coolify", "dokploy", "railway" or "vercel".
    pub platform: String,
    /// The platform's host, as given (railway.com or vercel.com for those).
    pub host: String,
    pub projects: Vec<ImportProject>,
    /// About the whole import, e.g. a token that can't read variables' values.
    pub warnings: Vec<String>,
}

/// A project there (in Coolify, one of its environments) and what's in it.
#[derive(Debug, Clone, Default, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ImportProject {
    /// Its id there.
    pub key: String,
    /// The Pyo project's name.
    pub name: String,
    pub services: Vec<ImportService>,
    /// Compose stacks there: each becomes a project of its own, as Pyo's
    /// Compose import makes one.
    pub stacks: Vec<ImportStack>,
    /// Databases belong to the team in Pyo, not to a project.
    pub databases: Vec<ImportDatabase>,
    /// The Pyo project an earlier import made for it.
    #[serde(default)]
    pub existing_project_id: Option<String>,
}

/// One of their applications as a Pyo service.
#[derive(Debug, Clone, Default, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ImportService {
    /// What to pick it by: "app:<id there>".
    pub key: String,
    /// Its name there.
    pub source_name: String,
    /// For a service built from Git: "dockerfile", or "railpack" (zero config).
    #[serde(default)]
    pub builder: String,
    /// The service: its source, ports, variables (no values in a preview), volumes and health check.
    pub service: PlanService,
    pub domains: Vec<ImportDomain>,
    pub warnings: Vec<String>,
    /// False when it can't be imported (the warnings say why).
    pub importable: bool,
    /// The Pyo service an earlier import made from it.
    #[serde(default)]
    pub existing_id: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, utoipa::ToSchema, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ImportDomain {
    /// In a stack: the Compose service it's for.
    #[serde(default)]
    pub service: String,
    pub hostname: String,
    /// The service's HTTP port it routes to.
    pub port_name: String,
}

/// A Compose stack there as a Pyo project.
#[derive(Debug, Clone, Default, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ImportStack {
    /// "compose:<id there>".
    pub key: String,
    pub source_name: String,
    pub plan: ComposePlan,
    pub domains: Vec<ImportDomain>,
    pub warnings: Vec<String>,
    pub importable: bool,
    #[serde(default)]
    pub existing_project_id: Option<String>,
}

/// One of their databases.
#[derive(Debug, Clone, Default, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ImportDatabase {
    /// "db:<id there>".
    pub key: String,
    pub source_name: String,
    /// Its name in Pyo.
    pub name: String,
    /// Its engine there: "postgres", "mysql", "mariadb", "mongodb", "redis", "keydb", "dragonfly", "clickhouse" or "libsql".
    pub engine: String,
    /// Its image there.
    #[serde(default)]
    pub image: String,
    /// Pyo's engine for it: "postgres", "mysql" or "valkey" (KV). None when Pyo has none.
    #[serde(default)]
    pub target: Option<String>,
    /// "import" (made and its data copied), "create" (made empty) or "manual" (left for you).
    pub action: String,
    pub warnings: Vec<String>,
    /// A database of the team's with its name already.
    #[serde(default)]
    pub existing_id: Option<String>,
    /// Where Pyo copies it from. Never sent back.
    #[serde(skip)]
    pub url: Option<String>,
    /// The names services there reach it by, to warn about variables that still do.
    #[serde(skip)]
    pub hosts: Vec<String>,
}

// ── Shared mapping helpers ───────────────────────────────────────────────────

/// Every answer a walk of the platform's API got, by the path it asked
/// (with its query). Recorded fixtures have the same shape.
pub type Responses = BTreeMap<String, Value>;

fn s<'a>(v: &'a Value, k: &str) -> &'a str {
    v[k].as_str().unwrap_or("")
}

/// A Git URL as Pyo builds from it: https, no `.git`, no login.
/// `git@host:owner/repo.git` → `https://host/owner/repo`.
pub(crate) fn https_repo(url: &str) -> Option<String> {
    let u = url.trim().trim_end_matches('/');
    let u = u.strip_suffix(".git").unwrap_or(u);
    if let Some(rest) = u.strip_prefix("git@") {
        let (host, path) = rest.split_once(':')?;
        return Some(format!("https://{host}/{}", path.trim_start_matches('/')));
    }
    if let Some(rest) = u.strip_prefix("ssh://") {
        let rest = rest.split_once('@').map_or(rest, |(_, r)| r);
        let (host, path) = rest.split_once('/')?;
        let host = host.split(':').next().unwrap_or(host);
        return Some(format!("https://{host}/{path}"));
    }
    let url = Url::parse(u).ok()?;
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    let host = url.host_str()?;
    let port = url.port().map(|p| format!(":{p}")).unwrap_or_default();
    Some(format!(
        "https://{host}{port}{}",
        url.path().trim_end_matches('/')
    ))
}

/// A unique DNS-safe name from `base` among `taken`.
fn unique(base: &str, taken: &mut HashSet<String>) -> String {
    let base = crate::compose::dns_name(base, 40);
    let base = if crate::ops::RESERVED_PROJECT_SLUGS.contains(&base.as_str())
        || [
            "analytics",
            "deployments",
            "networking",
            "settings",
            "storage",
        ]
        .contains(&base.as_str())
    {
        format!("{base}-app")
    } else {
        base
    };
    let mut name = base.clone();
    let mut n = 2;
    while !taken.insert(name.clone()) {
        name = format!("{base}-{n}");
        n += 1;
    }
    name
}

/// A domain the platform made up for itself, which points at it: Pyo gives
/// its own free domain instead.
fn generated_domain(host: &str) -> bool {
    let h = host.to_lowercase();
    // Railway's `*.up.railway.app` (and older `*.railway.app`), Vercel's
    // `*.vercel.app`.
    [
        ".sslip.io",
        ".nip.io",
        ".traefik.me",
        ".railway.app",
        ".vercel.app",
    ]
    .iter()
    .any(|d| h.ends_with(d))
}

/// Resources when the platform sets none: Compose import's defaults.
fn default_resources() -> PlanResources {
    PlanResources {
        cpu_request: 100,
        cpu_limit: 1000,
        memory_request: 128,
        memory_limit: 512,
        gpu: 0,
    }
}

/// A health check on the first port: HTTP when there's a path, else TCP.
fn health_check(port: Option<u16>, http_path: Option<&str>) -> Value {
    match (port, http_path.filter(|p| p.starts_with('/'))) {
        (Some(p), Some(path)) => {
            json!({ "type": "http", "path": path, "port": p, "command": "", "initialDelaySeconds": 10, "periodSeconds": 10 })
        }
        (Some(p), None) => {
            json!({ "type": "tcp", "path": "", "port": p, "command": "", "initialDelaySeconds": 10, "periodSeconds": 10 })
        }
        (None, _) => {
            json!({ "type": "none", "path": "", "port": 0, "command": "", "initialDelaySeconds": 10, "periodSeconds": 10 })
        }
    }
}

/// Collects a service's HTTP ports and domains: `domains` are (hostname,
/// container port or None for the first one, path), `exposed` the ports it
/// listens on.
#[derive(Default)]
struct Web {
    ports: Vec<PlanPort>,
    domains: Vec<ImportDomain>,
    warnings: Vec<String>,
}

impl Web {
    fn port(&mut self, container_port: u16, public: bool) -> String {
        if let Some(p) = self
            .ports
            .iter_mut()
            .find(|p| p.container_port == container_port && p.protocol == "HTTP")
        {
            p.public |= public;
            return p.name.clone();
        }
        let name = if self.ports.iter().any(|p| p.name == "http") {
            format!("http-{container_port}")
        } else {
            "http".to_owned()
        };
        self.ports.push(PlanPort {
            name: name.clone(),
            container_port,
            protocol: "HTTP".into(),
            public,
            public_port: None,
            domain: None,
        });
        name
    }

    fn domain(
        &mut self,
        platform: Platform,
        host: &str,
        port: Option<u16>,
        path: &str,
        seen: &mut HashSet<String>,
    ) {
        let host = host.trim().trim_end_matches('.').to_lowercase();
        if host.is_empty() {
            return;
        }
        if generated_domain(&host) {
            self.warnings.push(format!(
                "{host} is a free domain {} made. Add a free Pyo domain instead.",
                platform.label()
            ));
            return;
        }
        if !crate::util::hostname(&host) || host.starts_with("*.") {
            self.warnings.push(format!(
                "{host} isn't a hostname Pyo can serve, so it's left out."
            ));
            return;
        }
        let Some(port) = port.or_else(|| self.ports.first().map(|p| p.container_port)) else {
            self.warnings.push(format!(
                "{host} is left out: Pyo doesn't know which port it serves. Add a port, then the domain."
            ));
            return;
        };
        if !path.is_empty() && path != "/" {
            self.warnings.push(format!(
                "{host} served only {path} there. In Pyo it serves the whole service."
            ));
        }
        if !seen.insert(host.clone()) {
            self.warnings.push(format!(
                "{host} is on another service in this import too, so it's left out here."
            ));
            return;
        }
        let port_name = self.port(port, true);
        self.domains.push(ImportDomain {
            service: String::new(),
            hostname: host,
            port_name,
        });
    }

    /// A port published on the server there: a public TCP or UDP port.
    fn published(&mut self, published: u16, target: u16, protocol: &str) {
        let protocol = if protocol.eq_ignore_ascii_case("udp") {
            "UDP"
        } else {
            "TCP"
        };
        let port = json!({ "name": "p", "containerPort": target, "protocol": protocol, "exposure": "loadbalancer", "publicPort": published, "proxy": true });
        if crate::ops::validate_public_port(&port).is_err() {
            self.warnings.push(format!(
                "Port {published}/{} can't be public in Pyo, so it's left out.",
                protocol.to_lowercase()
            ));
            return;
        }
        self.ports.push(PlanPort {
            name: format!("{}-{published}", protocol.to_lowercase()),
            container_port: target,
            protocol: protocol.into(),
            public: true,
            public_port: Some(published),
            domain: None,
        });
    }
}

/// Warnings for variables that still point at one of the platform's
/// databases by its name there.
fn database_warnings(env: &[PlanEnv], dbs: &[ImportDatabase]) -> Vec<String> {
    let mut out = vec![];
    for db in dbs {
        let keys: Vec<&str> = env
            .iter()
            .filter(|e| {
                db.hosts
                    .iter()
                    .any(|h| !h.is_empty() && e.value.contains(h.as_str()))
            })
            .map(|e| e.key.as_str())
            .collect();
        if !keys.is_empty() {
            out.push(format!(
                "{} point{} at {} there. Connect the service to it in Pyo and remove {}.",
                keys.join(", "),
                if keys.len() == 1 { "s" } else { "" },
                db.source_name,
                if keys.len() == 1 { "it" } else { "them" }
            ));
        }
    }
    out
}

/// A plain service (no stack) with its defaults filled in.
fn plan_service(name: &str, source_name: &str) -> PlanService {
    PlanService {
        name: name.to_owned(),
        compose_name: source_name.to_owned(),
        replicas: 1,
        resources: default_resources(),
        health_check: health_check(None, None),
        ..Default::default()
    }
}

fn volume(name: &str, mount_path: &str, from_bind: bool) -> PlanVolume {
    PlanVolume {
        name: crate::compose::dns_name(name, 40),
        mount_path: mount_path.to_owned(),
        size_gi: VOLUME_GI,
        from_bind,
    }
}

/// What Pyo makes of one of their databases, from its engine and whether
/// Pyo can reach it from outside.
fn database_action(
    engine: &str,
    reachable: bool,
    platform: Platform,
) -> (Option<&'static str>, &'static str, Option<String>) {
    let p = platform.label();
    match engine {
        "postgres" | "mysql" if reachable => (Some(if engine == "postgres" { "postgres" } else { "mysql" }), "import", None),
        "postgres" | "mysql" => (
            Some(if engine == "postgres" { "postgres" } else { "mysql" }),
            "create",
            Some(format!("It isn't reachable from outside {p}, so Pyo makes it empty. To copy its data, make it public in {p} and import again, or import a dump into it.")),
        ),
        "redis" | "keydb" | "dragonfly" => (
            Some("valkey"),
            "create",
            Some("Pyo makes a KV database (Redis compatible) for it, empty. Its data isn't copied.".to_owned()),
        ),
        "mariadb" => (
            None,
            "manual",
            Some("Pyo runs MySQL, not MariaDB. Import a dump of it into a MySQL database if it's compatible.".to_owned()),
        ),
        _ => (
            None,
            "manual",
            Some(format!("Pyo has no {engine} databases. Run it as a service from its image, with a volume.")),
        ),
    }
}

/// Percent-encodes a URL's user, password or database name.
pub(super) fn url_escape(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Blanks every variable's value: a preview shows names only.
fn redact(plan: &mut ImportPlan) {
    let blank = |svc: &mut PlanService| {
        for e in &mut svc.env {
            e.value.clear();
        }
        for a in &mut svc.source.build_args {
            a.value.clear();
        }
    };
    for p in &mut plan.projects {
        p.services.iter_mut().for_each(|s| blank(&mut s.service));
        for st in &mut p.stacks {
            st.plan.services.iter_mut().for_each(blank);
        }
    }
}

// ── Reading the platform ─────────────────────────────────────────────────────

/// Where answers come from: the platform, or recorded ones in tests.
pub(crate) enum Fetch {
    Http {
        client: reqwest::Client,
        base: Url,
        platform: Platform,
        token: String,
        count: std::sync::atomic::AtomicUsize,
        /// Added to every request: Vercel's team (`teamId` or `slug`).
        scope: Vec<(String, String)>,
        /// Railway: send the token as a project token
        /// (`Project-Access-Token`) instead of a bearer token.
        project_token: std::sync::atomic::AtomicBool,
    },
    #[cfg(test)]
    Recorded(Responses),
}

/// The platform failed or refused: what went wrong, never its answer.
fn upstream(what: String) -> ApiError {
    ApiError::new(StatusCode::BAD_GATEWAY, what)
}

/// What a platform says when it refuses the token.
fn refused(p: Platform) -> String {
    match p {
        Platform::Coolify => "Coolify refused the token, or its API is off. Turn the API on in Coolify's Settings, and give the token read access.".to_owned(),
        Platform::Dokploy => "Dokploy refused the token: it can't read this. Use an owner's or admin's token.".to_owned(),
        Platform::Railway => "Railway refused the token. Use an account, workspace or project token.".to_owned(),
        Platform::Vercel => "Vercel refused the token: it can't read this. Check the token's scope and the team.".to_owned(),
    }
}

impl Fetch {
    /// GET `path` (with its query) as JSON. Errors say what went wrong
    /// without the platform's answer or the token.
    async fn get(&self, path: &str) -> ApiResult<Value> {
        match self {
            #[cfg(test)]
            Fetch::Recorded(r) => r
                .get(path)
                .cloned()
                .ok_or_else(|| ApiError::not_found(&format!("Recorded answer for {path}"))),
            Fetch::Http {
                client,
                base,
                scope,
                ..
            } => {
                let (p, query) = path.split_once('?').unwrap_or((path, ""));
                let mut url = base.clone();
                {
                    let mut seg = url
                        .path_segments_mut()
                        .map_err(|_| ApiError::bad_request("That address can't be used."))?;
                    seg.pop_if_empty();
                    seg.extend(p.split('/').filter(|x| !x.is_empty()));
                }
                {
                    let mut q = url.query_pairs_mut();
                    if !query.is_empty() {
                        for pair in query.split('&') {
                            let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
                            q.append_pair(k, v);
                        }
                    }
                    for (k, v) in scope {
                        q.append_pair(k, v);
                    }
                }
                if url.query() == Some("") {
                    url.set_query(None);
                }
                self.send(client.get(url), path).await
            }
        }
    }

    /// A GraphQL query (Railway), kept as `key`. IDs go in `variables`,
    /// never into the query text. A query the platform answers with errors
    /// and no data is an error; partial data is kept.
    async fn graphql(&self, key: &str, query: &str, variables: Value) -> ApiResult<Value> {
        let body = match self {
            #[cfg(test)]
            Fetch::Recorded(r) => r
                .get(key)
                .cloned()
                .ok_or_else(|| ApiError::not_found(&format!("Recorded answer for {key}")))?,
            Fetch::Http { client, base, .. } => {
                let req = client
                    .post(base.clone())
                    .json(&json!({ "query": query, "variables": variables }));
                self.send(req, key).await?
            }
        };
        let errors = body["errors"].as_array().map(Vec::as_slice).unwrap_or(&[]);
        if body["data"].is_null() && !errors.is_empty() {
            // "Not Authorized" is how Railway refuses a token per field.
            let denied = errors.iter().any(|e| {
                let m = e["message"].as_str().unwrap_or("").to_lowercase();
                m.contains("not authorized")
                    || m.contains("unauthorized")
                    || m.contains("forbidden")
            });
            return Err(upstream(if denied {
                refused(self.platform())
            } else {
                format!(
                    "{} couldn't answer Pyo's question.",
                    self.platform().label()
                )
            }));
        }
        Ok(body)
    }

    fn platform(&self) -> Platform {
        match self {
            #[cfg(test)]
            Fetch::Recorded(_) => Platform::Railway,
            Fetch::Http { platform, .. } => *platform,
        }
    }

    /// Railway: send the token as a project token from now on.
    fn use_project_token(&self) {
        match self {
            Fetch::Http { project_token, .. } => {
                project_token.store(true, std::sync::atomic::Ordering::Relaxed)
            }
            #[cfg(test)]
            Fetch::Recorded(_) => {}
        }
    }

    /// Sends one request with the token and reads its JSON answer.
    async fn send(&self, req: reqwest::RequestBuilder, what: &str) -> ApiResult<Value> {
        let (base, p, token, count, project_token) = match self {
            Fetch::Http {
                base,
                platform,
                token,
                count,
                project_token,
                ..
            } => (base, *platform, token, count, project_token),
            #[cfg(test)]
            Fetch::Recorded(_) => unreachable!("recorded answers aren't sent"),
        };
        if count.fetch_add(1, std::sync::atomic::Ordering::Relaxed) >= MAX_REQUESTS {
            return Err(ApiError::bad_request(format!(
                "There's too much in {} to read at once. Use a token that sees fewer projects.",
                p.label()
            )));
        }
        let req = req.header("accept", "application/json");
        let req = match p {
            Platform::Dokploy => req.header("x-api-key", token),
            // https://docs.railway.com/integrations/api: project tokens go
            // in `Project-Access-Token`, the others as a bearer token.
            Platform::Railway if project_token.load(std::sync::atomic::Ordering::Relaxed) => {
                req.header("Project-Access-Token", token)
            }
            _ => req.bearer_auth(token),
        };
        let host = base.host_str().unwrap_or("");
        let mut res = req.send().await.map_err(|e| {
            if e.is_timeout() {
                upstream(format!("{} didn't answer in time.", p.label()))
            } else {
                upstream(format!("Pyo can't reach {} at {host}.", p.label()))
            }
        })?;
        let status = res.status();
        if status.is_redirection() {
            return Err(upstream(format!(
                "{} answered with a redirect, which Pyo doesn't follow. Use its final address, with https://.",
                p.label()
            )));
        }
        if status == reqwest::StatusCode::UNAUTHORIZED {
            return Err(upstream(format!(
                "{} refused the token. Check it and try again.",
                p.label()
            )));
        }
        if status == reqwest::StatusCode::FORBIDDEN {
            return Err(upstream(refused(p)));
        }
        if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            return Err(upstream(format!(
                "{} says the token made too many requests. Try again later.",
                p.label()
            )));
        }
        if status == reqwest::StatusCode::NOT_FOUND {
            return Err(upstream(match p.official_api() {
                Some(_) => format!("{} has nothing at {what}.", p.label()),
                None => format!(
                    "{host} has nothing at {what}. Is this the address of {}?",
                    p.label()
                ),
            }));
        }
        // GraphQL answers its errors with 200 or 400 and a JSON body.
        let graphql_error = p == Platform::Railway && status == reqwest::StatusCode::BAD_REQUEST;
        if !status.is_success() && !graphql_error {
            return Err(upstream(format!(
                "{} answered with an error ({}).",
                p.label(),
                status.as_u16()
            )));
        }
        let mut body = Vec::new();
        while let Some(chunk) = res
            .chunk()
            .await
            .map_err(|_| upstream(format!("{} stopped answering.", p.label())))?
        {
            if body.len() + chunk.len() > MAX_BODY {
                return Err(upstream(format!(
                    "{}'s answer is too big to read.",
                    p.label()
                )));
            }
            body.extend_from_slice(&chunk);
        }
        serde_json::from_slice(&body).map_err(|_| {
            upstream(match p.official_api() {
                Some(_) => format!("{} didn't answer with JSON.", p.label()),
                None => format!(
                    "{host} didn't answer with JSON. Is this the address of {}?",
                    p.label()
                ),
            })
        })
    }

    /// Several paths, a few at a time, each kept by its path.
    async fn get_all(&self, paths: Vec<String>, into: &mut Responses) -> ApiResult<()> {
        use futures::{StreamExt, TryStreamExt};
        let got: Vec<(String, Value)> = futures::stream::iter(paths)
            .map(|p| async move { self.get(&p).await.map(|v| (p, v)) })
            .buffered(4)
            .try_collect()
            .await?;
        into.extend(got);
        Ok(())
    }
}

/// The platform's API base. A self-hosted one's from the address the user
/// gives (with or without the API's own path), checked by outbound.rs. A
/// hosted one's is fixed: an address given must be that platform's own.
async fn api_base(platform: Platform, url: &str) -> ApiResult<Url> {
    if let Some(api) = platform.official_api() {
        let api =
            Url::parse(api).map_err(|_| ApiError::bad_request(Refused::Invalid.to_string()))?;
        let given = url.trim();
        if !given.is_empty() {
            let with_scheme = if given.contains("://") {
                given.to_owned()
            } else {
                format!("https://{given}")
            };
            let ok = Url::parse(&with_scheme).ok().is_some_and(|u| {
                u.scheme() == "https"
                    && u.username().is_empty()
                    && u.password().is_none()
                    && u.port().is_none()
                    && u.host_str() == api.host_str()
            });
            if !ok {
                return Err(ApiError::bad_request(format!(
                    "Pyo reads {} only from its own API. Leave the address empty.",
                    platform.label()
                )));
            }
        }
        return Ok(api);
    }
    let mut url = url.trim().trim_end_matches('/').to_owned();
    if url.is_empty() {
        return Err(ApiError::bad_request(format!(
            "Give the address of your {}.",
            platform.label()
        )));
    }
    if !url.contains("://") {
        url = format!("https://{url}");
    }
    for suffix in ["/api/v1", "/api"] {
        if let Some(u) = url.strip_suffix(suffix) {
            url = u.to_owned();
        }
    }
    let base = outbound::parse(&url).map_err(|e| ApiError::bad_request(e.to_string()))?;
    if base.query().is_some() || base.fragment().is_some() {
        return Err(ApiError::bad_request(Refused::Invalid.to_string()));
    }
    outbound::check(&base, Reach::team())
        .await
        .map_err(|e| match e {
            Refused::Unknown => ApiError::new(StatusCode::BAD_GATEWAY, e.to_string()),
            e => ApiError::bad_request(e.to_string()),
        })?;
    let api = match platform {
        Platform::Coolify => format!("{}/api/v1", base.as_str().trim_end_matches('/')),
        _ => format!("{}/api", base.as_str().trim_end_matches('/')),
    };
    Url::parse(&api).map_err(|_| ApiError::bad_request(Refused::Invalid.to_string()))
}

/// Vercel's team as a query parameter: `teamId` for an id (`team_…`),
/// else `slug`. Nothing for the token's own scope.
fn team_scope(platform: Platform, team: Option<&str>) -> ApiResult<Vec<(String, String)>> {
    let Some(team) = team.map(str::trim).filter(|t| !t.is_empty()) else {
        return Ok(vec![]);
    };
    if platform != Platform::Vercel {
        return Err(ApiError::bad_request(format!(
            "{} has no teams to pick. Leave the team empty.",
            platform.label()
        )));
    }
    if team.len() > 100
        || !team
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(ApiError::bad_request(
            "That isn't a Vercel team id or slug.",
        ));
    }
    let key = if team.starts_with("team_") {
        "teamId"
    } else {
        "slug"
    };
    Ok(vec![(key.to_owned(), team.to_owned())])
}

/// Reads the platform and makes its plan, without values hidden.
async fn read(
    platform: Platform,
    url: &str,
    token: &str,
    team: Option<&str>,
) -> ApiResult<ImportPlan> {
    let token = token.trim();
    if token.is_empty() || token.len() > 4096 || token.chars().any(char::is_control) {
        return Err(ApiError::bad_request(format!(
            "Paste a {} API token.",
            platform.label()
        )));
    }
    let scope = team_scope(platform, team)?;
    let base = api_base(platform, url).await?;
    let host = match platform {
        Platform::Railway => railway::HOST.to_owned(),
        Platform::Vercel => vercel::HOST.to_owned(),
        _ => base.host_str().unwrap_or("").to_owned(),
    };
    let client = outbound::client(Reach::team(), TIMEOUT)
        .map_err(|e| ApiError::from(anyhow::Error::from(e)))?;
    let f = Fetch::Http {
        client,
        base,
        platform,
        token: token.to_owned(),
        count: Default::default(),
        scope,
        project_token: Default::default(),
    };
    Ok(match platform {
        Platform::Coolify => coolify::plan(&coolify::gather(&f).await?, &host),
        Platform::Dokploy => dokploy::plan(&dokploy::gather(&f).await?, &host),
        Platform::Railway => railway::plan(&railway::gather(&f).await?),
        Platform::Vercel => vercel::plan(&vercel::gather(&f).await?),
    })
}

// ── What's there already ─────────────────────────────────────────────────────

/// The Pyo service an earlier import made from `id` there.
async fn imported_service(
    tx: &mut Transaction<'_, Postgres>,
    team_id: &str,
    platform: &str,
    host: &str,
    id: &str,
) -> ApiResult<Option<String>> {
    Ok(sqlx::query_scalar(
        "SELECT id FROM pyo.services WHERE team_id = $1 AND NOT deleting
            AND spec->'source'->'importedFrom'->>'platform' = $2
            AND spec->'source'->'importedFrom'->>'host' = $3
            AND spec->'source'->'importedFrom'->>'id' = $4
          LIMIT 1",
    )
    .bind(team_id)
    .bind(platform)
    .bind(host)
    .bind(id)
    .fetch_optional(&mut **tx)
    .await?)
}

/// The Pyo project an earlier import made for `project` there (by its
/// services' records).
async fn imported_project(
    tx: &mut Transaction<'_, Postgres>,
    team_id: &str,
    platform: &str,
    host: &str,
    project: &str,
) -> ApiResult<Option<String>> {
    Ok(sqlx::query_scalar(
        "SELECT project_id FROM pyo.services WHERE team_id = $1 AND NOT deleting
            AND spec->'source'->'importedFrom'->>'platform' = $2
            AND spec->'source'->'importedFrom'->>'host' = $3
            AND spec->'source'->'importedFrom'->>'project' = $4
          ORDER BY created_at LIMIT 1",
    )
    .bind(team_id)
    .bind(platform)
    .bind(host)
    .bind(project)
    .fetch_optional(&mut **tx)
    .await?)
}

async fn database_named(
    tx: &mut Transaction<'_, Postgres>,
    team_id: &str,
    name: &str,
) -> ApiResult<Option<String>> {
    Ok(
        sqlx::query_scalar("SELECT id FROM pyo.databases WHERE team_id = $1 AND name = $2")
            .bind(team_id)
            .bind(name)
            .fetch_optional(&mut **tx)
            .await?,
    )
}

/// Fills in what earlier imports made, so the preview shows it and the
/// import leaves it alone.
async fn mark_existing(
    tx: &mut Transaction<'_, Postgres>,
    team_id: &str,
    plan: &mut ImportPlan,
) -> ApiResult<()> {
    let (platform, host) = (plan.platform.clone(), plan.host.clone());
    for p in &mut plan.projects {
        p.existing_project_id = imported_project(tx, team_id, &platform, &host, &p.key).await?;
        for svc in &mut p.services {
            svc.existing_id = imported_service(tx, team_id, &platform, &host, &svc.key).await?;
        }
        for st in &mut p.stacks {
            st.existing_project_id =
                imported_project(tx, team_id, &platform, &host, &st.key).await?;
        }
        for db in &mut p.databases {
            db.existing_id = database_named(tx, team_id, &db.name).await?;
        }
    }
    Ok(())
}

// ── Endpoints ────────────────────────────────────────────────────────────────

#[derive(Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct PreviewBody {
    pub team_id: String,
    /// "coolify", "dokploy", "railway" or "vercel".
    pub platform: Platform,
    /// The platform's address, e.g. https://coolify.example.com. Empty for Railway and Vercel: Pyo reads them only from their own API.
    #[serde(default)]
    pub url: String,
    /// An API token of the platform's: Coolify's with read and read:sensitive, a Dokploy API key, a Railway account, workspace or project token, or a Vercel token. Used for this request only, never stored.
    pub token: String,
    /// Vercel only: the team to read (its id or slug). The token's own scope when omitted.
    #[serde(default)]
    pub team: Option<String>,
}

/// Owners and admins, with a key that may change things, within the rate
/// limit.
async fn allowed(s: &AppState, user: &User, team_id: &str) -> ApiResult<()> {
    if user
        .key
        .as_ref()
        .is_some_and(|k| k.access != KeyAccess::Full)
    {
        return Err(ApiError::forbidden(
            "Importing needs an API key with full access.",
        ));
    }
    let mut tx = s.db.begin().await?;
    require_role(&mut tx, user, team_id, Role::Admin).await?;
    tx.commit().await?;
    ratelimit::take(&s.db, &[(LIMIT, &user.id)]).await
}

/// Preview an import from another platform
///
/// Reads a Coolify or Dokploy installation, or a Railway or Vercel
/// account, through its API with a token you give, and shows what importing would create: per project there, its
/// services (source, ports, domains, variable names, volumes), Compose
/// stacks and databases, each with what doesn't carry over. Nothing is
/// created and the token isn't kept. Things an earlier import made are
/// marked (`existingId`). Owners and admins.
#[utoipa::path(post, path = "/platform-imports/preview", operation_id = "previewPlatformImport", tag = "Projects",
    request_body = PreviewBody,
    responses(
        (status = 200, description = "The plan. Variables have no values.", body = ImportPlan),
        (status = 400, description = "The address or token isn't usable.", body = ErrorBody),
        (status = 403, description = "You aren't an owner or admin of the team, or the API key can't make changes.", body = ErrorBody),
        (status = 429, description = "Too many imports for now.", body = ErrorBody),
        (status = 502, description = "The platform couldn't be reached or refused the token.", body = ErrorBody),
    ),
    extensions(("x-pyo-access" = json!("full"))))]
pub async fn preview(
    State(s): State<AppState>,
    SessionUser(user): SessionUser,
    Json(b): Json<PreviewBody>,
) -> ApiResult<Json<ImportPlan>> {
    allowed(&s, &user, &b.team_id).await?;
    let mut plan = read(b.platform, &b.url, &b.token, b.team.as_deref()).await?;
    let mut tx = s.db.begin().await?;
    mark_existing(&mut tx, &b.team_id, &mut plan).await?;
    tx.commit().await?;
    redact(&mut plan);
    Ok(Json(plan))
}

#[derive(Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ApplyBody {
    pub team_id: String,
    pub platform: Platform,
    #[serde(default)]
    pub url: String,
    /// Read again for this request, never stored.
    pub token: String,
    /// Vercel only: the team to read (its id or slug).
    #[serde(default)]
    pub team: Option<String>,
    /// Where the services and databases go. The team's default cluster when omitted.
    #[serde(default)]
    pub cluster_id: Option<String>,
    /// The `key`s to import, from the preview. All of them when omitted.
    #[serde(default)]
    pub items: Option<Vec<String>>,
    /// Deploy each service once it's made. Off: they're saved, stopped, to deploy when you like.
    #[serde(default)]
    pub deploy: bool,
}

/// What became of one picked thing.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ImportResult {
    pub key: String,
    pub name: String,
    /// "created", "importing" (a database whose data is being copied), "exists" (an earlier import made it), "skipped" (it can't be imported) or "failed".
    pub state: String,
    #[serde(default)]
    pub message: String,
    /// The Pyo service, project or database.
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub project_id: Option<String>,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ImportOutcome {
    pub ok: bool,
    pub results: Vec<ImportResult>,
}

fn result(key: &str, name: &str, state: &str, message: impl Into<String>) -> ImportResult {
    ImportResult {
        key: key.to_owned(),
        name: name.to_owned(),
        state: state.to_owned(),
        message: message.into(),
        id: None,
        project_id: None,
    }
}

/// Import from another platform
///
/// Reads the platform again (as `previewPlatformImport`) and creates the
/// picked things (`items`, all by default): a project per project there,
/// its services with their variables (secret ones sealed), domains (live
/// after the first deploy), ports and empty volumes, a project per Compose
/// stack, and databases. A Postgres or MySQL database reachable from
/// outside the platform is copied by a database import; others are made
/// empty or left for you. Services aren't deployed unless `deploy`.
/// Running it again skips what an earlier import made. Owners and admins.
#[utoipa::path(post, path = "/platform-imports", operation_id = "startPlatformImport", tag = "Projects",
    request_body = ApplyBody,
    responses(
        (status = 200, description = "What became of each picked thing.", body = ImportOutcome),
        (status = 400, description = "The address, token or cluster isn't usable.", body = ErrorBody),
        (status = 403, description = "You aren't an owner or admin of the team, or the API key can't make changes.", body = ErrorBody),
        (status = 429, description = "Too many imports for now.", body = ErrorBody),
        (status = 502, description = "The platform couldn't be reached or refused the token.", body = ErrorBody),
    ),
    extensions(("x-pyo-access" = json!("full"))))]
pub async fn apply(
    State(s): State<AppState>,
    SessionUser(user): SessionUser,
    Json(b): Json<ApplyBody>,
) -> ApiResult<Json<ImportOutcome>> {
    allowed(&s, &user, &b.team_id).await?;
    let plan = read(b.platform, &b.url, &b.token, b.team.as_deref()).await?;
    let out = create(
        &s,
        &user,
        &b.team_id,
        b.cluster_id.clone(),
        plan,
        b.items.as_deref(),
        b.deploy,
    )
    .await?;
    Ok(Json(out))
}

/// Creates what's picked of a plan (with its values): `items` by key, all
/// when None.
async fn create(
    s: &AppState,
    user: &User,
    team_id: &str,
    cluster_id: Option<String>,
    mut plan: ImportPlan,
    items: Option<&[String]>,
    deploy: bool,
) -> ApiResult<ImportOutcome> {
    let picked = |key: &str| items.is_none_or(|i| i.iter().any(|k| k == key));
    let platform: Platform = serde_json::from_value(json!(plan.platform))
        .map_err(|_| ApiError::bad_request("Unknown platform."))?;
    let mut tx = s.db.begin().await?;
    require_role(&mut tx, user, team_id, Role::Admin).await?;
    mark_existing(&mut tx, team_id, &mut plan).await?;
    let cluster_id = match cluster_id.filter(|c| !c.is_empty()) {
        Some(c) => c,
        None => sqlx::query_scalar::<_, Option<String>>(
            "SELECT default_cluster_id FROM pyo.teams WHERE id = $1",
        )
        .bind(team_id)
        .fetch_one(&mut *tx)
        .await?
        .ok_or_else(|| {
            ApiError::bad_request("This team has no cluster to deploy to. Add a server first.")
        })?,
    };
    if crate::ops::team_of(&mut tx, "clusters", &cluster_id).await? != team_id {
        return Err(ApiError::not_found("Cluster"));
    }
    // GitHub repositories the team's GitHub App reaches, by owner.
    let installations: Vec<(i64, String)> = sqlx::query_as(
        "SELECT id, lower(account) FROM pyo.github_installations WHERE team_id = $1",
    )
    .bind(team_id)
    .fetch_all(&mut *tx)
    .await?;

    let label = platform.label();
    let mut results = vec![];
    let mut wake = vec![];
    let now = pyo_proto::now_ms();
    let from = |id: &str, project: &str| json!({ "platform": plan.platform, "host": plan.host, "id": id, "project": project });
    for p in &plan.projects {
        let mut project_id = p.existing_project_id.clone();
        for svc in p.services.iter().filter(|x| picked(&x.key)) {
            if let Some(id) = &svc.existing_id {
                let mut r = result(
                    &svc.key,
                    &svc.service.name,
                    "exists",
                    "An earlier import made it.",
                );
                r.id = Some(id.clone());
                results.push(r);
                continue;
            }
            if !svc.importable {
                results.push(result(
                    &svc.key,
                    &svc.source_name,
                    "skipped",
                    svc.warnings.join(" "),
                ));
                continue;
            }
            let mut sp = sqlx::Acquire::begin(&mut *tx).await?;
            let made = async {
                let pid = match &project_id {
                    Some(id) => id.clone(),
                    None => make_project(&mut sp, user, team_id, &p.name, label).await?,
                };
                let mut service = svc.service.clone();
                github_app(&mut service, &installations);
                let id = new_id("svc");
                let mut spec = crate::compose::service_spec(
                    &service,
                    &id,
                    team_id,
                    &pid,
                    &cluster_id,
                    now,
                    &new_id,
                );
                spec["description"] = json!(format!("Imported from {label} ({})", svc.source_name));
                if service.source.kind == "git" && !svc.builder.is_empty() {
                    spec["source"]["builder"] = json!(svc.builder);
                }
                spec["source"]["importedFrom"] = from(&svc.key, &p.key);
                spec["domains"] = domains_json(&svc.domains, now);
                let volumes = crate::compose::volume_specs(&service, &new_id);
                let deployment = deploy.then(|| new_id("dpl"));
                crate::ops::create_service(
                    &mut sp,
                    user,
                    spec,
                    deployment.as_deref(),
                    volumes,
                    vec![],
                    &mut wake,
                )
                .await?;
                Ok::<_, ApiError>((pid, id))
            }
            .await;
            match made {
                Ok((pid, id)) => {
                    sp.commit().await?;
                    project_id = Some(pid.clone());
                    let mut r = result(&svc.key, &svc.service.name, "created", "");
                    r.id = Some(id);
                    r.project_id = Some(pid);
                    results.push(r);
                }
                Err(e) => {
                    sp.rollback().await?;
                    results.push(result(&svc.key, &svc.service.name, "failed", e.message));
                }
            }
        }
        for st in p.stacks.iter().filter(|x| picked(&x.key)) {
            if let Some(id) = &st.existing_project_id {
                let mut r = result(
                    &st.key,
                    &st.plan.project.name,
                    "exists",
                    "An earlier import made it.",
                );
                r.project_id = Some(id.clone());
                results.push(r);
                continue;
            }
            if !st.importable {
                results.push(result(
                    &st.key,
                    &st.source_name,
                    "skipped",
                    st.warnings.join(" "),
                ));
                continue;
            }
            let mut sp = sqlx::Acquire::begin(&mut *tx).await?;
            let made = async {
                let pid =
                    make_project(&mut sp, user, team_id, &st.plan.project.name, label).await?;
                for cs in &st.plan.services {
                    let id = new_id("svc");
                    let mut spec = crate::compose::service_spec(
                        cs,
                        &id,
                        team_id,
                        &pid,
                        &cluster_id,
                        now,
                        &new_id,
                    );
                    spec["description"] = json!(format!(
                        "Imported from {label} ({} {})",
                        st.source_name, cs.compose_name
                    ));
                    spec["source"]["importedFrom"] =
                        from(&format!("{}/{}", st.key, cs.compose_name), &st.key);
                    let mine: Vec<ImportDomain> = st
                        .domains
                        .iter()
                        .filter(|d| d.service == cs.compose_name)
                        .cloned()
                        .collect();
                    spec["domains"] = domains_json(&mine, now);
                    let volumes = crate::compose::volume_specs(cs, &new_id);
                    let deployment = deploy.then(|| new_id("dpl"));
                    crate::ops::create_service(
                        &mut sp,
                        user,
                        spec,
                        deployment.as_deref(),
                        volumes,
                        vec![],
                        &mut wake,
                    )
                    .await?;
                }
                Ok::<_, ApiError>(pid)
            }
            .await;
            match made {
                Ok(pid) => {
                    sp.commit().await?;
                    let mut r = result(&st.key, &st.plan.project.name, "created", "");
                    r.project_id = Some(pid);
                    results.push(r);
                }
                Err(e) => {
                    sp.rollback().await?;
                    results.push(result(&st.key, &st.plan.project.name, "failed", e.message));
                }
            }
        }
    }
    tx.commit().await?;
    wake.sort();
    wake.dedup();
    for c in wake {
        s.wake(&c).await;
    }
    s.wake("builders:*").await;

    // Databases, each on its own: an import checks its source on the
    // cluster first, which takes a while.
    let dbs: Vec<&ImportDatabase> = plan
        .projects
        .iter()
        .flat_map(|p| &p.databases)
        .filter(|d| picked(&d.key))
        .collect();
    let jobs = dbs
        .into_iter()
        .map(|db| database(s, user, team_id, &cluster_id, db));
    results.extend(futures::future::join_all(jobs).await);
    Ok(ImportOutcome { ok: true, results })
}

/// A new project for an import, with a free slug.
async fn make_project(
    tx: &mut Transaction<'_, Postgres>,
    user: &User,
    team_id: &str,
    name: &str,
    platform: &str,
) -> ApiResult<String> {
    let base = crate::compose::dns_name(name, 40);
    let namespace = crate::ops::free_namespace(tx, team_id, &base).await?;
    let id = new_id("prj");
    let name: String = name.trim().chars().take(60).collect();
    let name = if name.is_empty() {
        namespace.clone()
    } else {
        name
    };
    crate::ops::create_project(
        tx,
        user,
        &id,
        team_id,
        &name,
        &namespace,
        &format!("Imported from {platform}"),
    )
    .await?;
    Ok(id)
}

/// A GitHub repository whose owner has the team's GitHub App: built
/// through it, so a private one works too.
fn github_app(svc: &mut PlanService, installations: &[(i64, String)]) {
    if svc.source.kind != "git" {
        return;
    }
    let Some(full) = crate::github::full_name(&svc.source.repo_url) else {
        return;
    };
    let owner = full.split('/').next().unwrap_or("").to_lowercase();
    if let Some((id, _)) = installations.iter().find(|(_, a)| *a == owner) {
        svc.source.installation_id = Some(*id);
        svc.source.private = true;
        svc.source.repo_url = format!("https://github.com/{full}");
    }
}

fn domains_json(domains: &[ImportDomain], now: i64) -> Value {
    Value::Array(
        domains
            .iter()
            .map(|d| {
                let private = crate::deploy::private_host(&d.hostname);
                json!({ "id": new_id("dom"), "hostname": d.hostname, "portName": d.port_name, "status": "pending_dns", "tls": "pending", "tlsMode": "auto", "redirectHttps": !private, "createdAt": now })
            })
            .collect(),
    )
}

/// One database: copied, made empty, or left for you.
async fn database(
    s: &AppState,
    user: &User,
    team_id: &str,
    cluster_id: &str,
    db: &ImportDatabase,
) -> ImportResult {
    if let Some(id) = &db.existing_id {
        let mut r = result(
            &db.key,
            &db.name,
            "exists",
            format!("The team has a database called {} already.", db.name),
        );
        r.id = Some(id.clone());
        return r;
    }
    let Some(engine) = db.target.as_deref().filter(|_| db.action != "manual") else {
        return result(&db.key, &db.source_name, "skipped", db.warnings.join(" "));
    };
    let id = new_id("mdb");
    let size = if engine == "mysql" { "medium" } else { "small" };
    let out = match (db.action.as_str(), &db.url) {
        ("import", Some(url)) => {
            let req = json!({
                "teamId": team_id, "engine": engine, "url": url,
                "newDatabase": { "id": id, "clusterId": cluster_id, "name": db.name, "size": size, "instances": 1 },
            });
            match serde_json::from_value(req) {
                Ok(req) => crate::database_imports::start(State(s.clone()), SessionUser(user.clone()), Json(req))
                    .await
                    .map(|_| "importing"),
                Err(e) => Err(ApiError::bad_request(e.to_string())),
            }
        }
        _ => crate::ops::run_json(
            s,
            user,
            json!({ "op": "createDatabase", "id": id, "teamId": team_id, "clusterId": cluster_id, "name": db.name, "engine": engine, "size": size, "instances": 1 }),
        )
        .await
        .map(|_| "created"),
    };
    match out {
        Ok(state) => {
            let mut r = result(&db.key, &db.name, state, db.warnings.join(" "));
            r.id = Some(id);
            r
        }
        Err(e) => result(&db.key, &db.name, "failed", e.message),
    }
}

#[cfg(test)]
mod tests;
