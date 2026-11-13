//! Vercel, through its REST API at https://api.vercel.com with a bearer
//! token (https://vercel.com/docs/rest-api, and the OpenAPI document
//! https://openapi.vercel.sh/: `servers`, `securitySchemes.bearerToken`).
//! Pyo talks to no other host. A team is picked with `teamId` or `slug` on
//! every request; without one the token's own scope is read.
//!
//! The walk (field names from the OpenAPI document's 200 responses):
//! - `GET /v10/projects?limit=100`, page by page with `from` set to
//!   `pagination.next`: each project's `id`, `name`.
//! - Per project, `GET /v9/projects/{id}`: `framework`, `link` (`type`
//!   github, github-limited, github-custom-host with `host`, gitlab with
//!   `projectUrl`, bitbucket with `owner` and `slug`; `org`, `repo`,
//!   `productionBranch`), `rootDirectory`, `sourceFilesOutsideRootDirectory`,
//!   `buildCommand`, `installCommand`, `outputDirectory`, `nodeVersion`,
//!   `crons.definitions` (`path`, `schedule`), `passwordProtection`,
//!   `autoExposeSystemEnvs`.
//! - `GET /v10/projects/{id}/env`: each variable's `id`, `key`, `type`
//!   (encrypted, plain, secret, sensitive, system), `visibility` (config,
//!   secret), `target` (production, preview, development), `gitBranch`,
//!   `contentHint` (`type` and `storeId` of the Vercel storage it belongs
//!   to), `edgeConfigId`, and `value` (plain ones only; `decrypt` is
//!   deprecated, so it isn't used).
//! - `GET /v1/projects/{id}/env/{envId}` for each production variable of
//!   type encrypted: its decrypted `value`. Sensitive variables and ones
//!   with secret visibility are write only: Vercel never sends their value
//!   (https://vercel.com/docs/environment-variables/sensitive-environment-variables),
//!   so they come over by name, secret and empty, with a note.
//! - `GET /v9/projects/{id}/domains?limit=100`: `name`, `redirect`,
//!   `gitBranch`, `customEnvironmentId`.
//! - Only when nothing is found and no team is given: `GET /v2/teams`, to
//!   say which teams the token could read instead.
//!
//! Projects run as a service built with zero config (Railpack) on port
//! 3000 (Next.js's default; Pyo sets PORT to it, which other frameworks
//! read). Build and install commands and the Node.js version go to
//! Railpack as RAILPACK_BUILD_CMD, RAILPACK_INSTALL_CMD and
//! RAILPACK_NODE_VERSION (https://railpack.com/config/environment-variables,
//! https://railpack.com/languages/node).

use std::collections::{BTreeMap, HashSet};

use serde_json::Value;

use super::{
    Fetch, ImportDatabase, ImportPlan, ImportProject, ImportService, Platform, Responses, Web,
    database_action, database_warnings, health_check, https_repo, plan_service, s, unique,
};
use crate::{
    compose::{self, PlanEnv, PlanSource},
    error::ApiResult,
};

pub(super) const API: &str = "https://api.vercel.com";
/// What imports from Vercel record as their host.
pub(super) const HOST: &str = "vercel.com";
const P: Platform = Platform::Vercel;
/// Next.js's default port.
const PORT: u16 = 3000;
/// Pages of projects read at most (100 each).
const MAX_PAGES: usize = 5;

/// Frameworks Vercel serves as static files (with functions on the side).
const STATIC: [&str; 14] = [
    "vite",
    "create-react-app",
    "vue",
    "angular",
    "gatsby",
    "hugo",
    "jekyll",
    "eleventy",
    "docusaurus",
    "docusaurus-2",
    "ember",
    "preact",
    "svelte",
    "hexo",
];
/// Frameworks that run on Vercel through a Vercel adapter.
const ADAPTED: [&str; 8] = [
    "sveltekit",
    "sveltekit-1",
    "nuxtjs",
    "remix",
    "astro",
    "solidstart",
    "solidstart-1",
    "react-router",
];
/// Prefixes of variables frameworks read while building, to put in the
/// browser bundle: given to the build too.
const PUBLIC_PREFIXES: [&str; 7] = [
    "NEXT_PUBLIC_",
    "VITE_",
    "PUBLIC_",
    "REACT_APP_",
    "NUXT_PUBLIC_",
    "GATSBY_",
    "VUE_APP_",
];

fn arr(v: &Value) -> &[Value] {
    v.as_array().map(Vec::as_slice).unwrap_or(&[])
}

fn projects_path(from: Option<&str>) -> String {
    match from {
        Some(next) => format!("/v10/projects?limit=100&from={next}"),
        None => "/v10/projects?limit=100".to_owned(),
    }
}

/// The env list answers either a list or `{ envs, pagination }`.
fn envs(v: &Value) -> &[Value] {
    match v.as_array() {
        Some(a) => a,
        None => arr(&v["envs"]),
    }
}

/// `next` of a page: a continuation token, or a timestamp on older answers.
fn next_page(v: &Value) -> Option<String> {
    match &v["pagination"]["next"] {
        Value::String(n) if !n.is_empty() => Some(n.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

fn targets_production(e: &Value) -> bool {
    match &e["target"] {
        Value::String(t) => t == "production",
        Value::Array(a) => a.iter().any(|t| t == "production"),
        _ => false,
    }
}

/// A production variable, not a branch's or custom environment's own.
fn production(e: &Value) -> bool {
    targets_production(e) && e["gitBranch"].as_str().is_none_or(str::is_empty)
}

/// Its value can never be read (write only).
fn write_only(e: &Value) -> bool {
    matches!(s(e, "type"), "sensitive" | "secret") || s(e, "visibility") == "secret"
}

/// Reads everything the token sees.
pub(super) async fn gather(f: &Fetch) -> ApiResult<Responses> {
    let mut r = Responses::new();
    let mut ids = vec![];
    let mut from: Option<String> = None;
    for _ in 0..MAX_PAGES {
        let path = projects_path(from.as_deref());
        let page = f.get(&path).await?;
        ids.extend(arr(&page["projects"]).iter().map(|p| s(p, "id").to_owned()));
        from = next_page(&page);
        r.insert(path, page);
        if from.is_none() {
            break;
        }
    }
    if ids.is_empty() {
        let teams = f.get("/v2/teams?limit=20").await?;
        r.insert("/v2/teams?limit=20".into(), teams);
        return Ok(r);
    }
    let mut paths = vec![];
    for id in &ids {
        paths.push(format!("/v9/projects/{id}"));
        paths.push(format!("/v10/projects/{id}/env"));
        paths.push(format!("/v9/projects/{id}/domains?limit=100"));
    }
    f.get_all(paths, &mut r).await?;
    let mut values = vec![];
    for id in &ids {
        let Some(list) = r.get(&format!("/v10/projects/{id}/env")) else {
            continue;
        };
        for e in envs(list) {
            if production(e) && s(e, "type") == "encrypted" && !write_only(e) {
                values.push(format!("/v1/projects/{id}/env/{}", s(e, "id")));
            }
        }
    }
    f.get_all(values, &mut r).await?;
    Ok(r)
}

/// The repository Vercel builds from, by `link.type`.
fn repository(link: &Value) -> Option<String> {
    match s(link, "type") {
        "github" | "github-limited" => Some(format!(
            "https://github.com/{}/{}",
            s(link, "org"),
            s(link, "repo")
        )),
        "github-custom-host" => Some(format!(
            "https://{}/{}/{}",
            s(link, "host"),
            s(link, "org"),
            s(link, "repo")
        )),
        "gitlab" => https_repo(s(link, "projectUrl")),
        "bitbucket" => Some(format!(
            "https://bitbucket.org/{}/{}",
            s(link, "owner"),
            s(link, "slug")
        )),
        _ => None,
    }
}

/// A Vercel storage store linked to projects through their variables.
struct Store {
    kind: &'static str,
    id: String,
    /// The project that lists it.
    project: String,
    /// The best URL to copy it from: the direct one before the pooled one.
    url: Option<String>,
    pooled: Option<String>,
    /// The hosts its variables point at, to note variables that still do.
    hosts: Vec<String>,
}

/// A variable's production value, when Vercel sends it.
fn value_of(r: &Responses, pid: &str, e: &Value) -> Option<String> {
    if write_only(e) {
        return None;
    }
    match s(e, "type") {
        "plain" => e["value"].as_str().map(str::to_owned),
        "encrypted" => r
            .get(&format!("/v1/projects/{pid}/env/{}", s(e, "id")))
            .and_then(|d| d["value"].as_str())
            .map(str::to_owned),
        _ => None,
    }
}

/// The databases Pyo makes for the stores projects use, each listed once
/// (under the first project that uses it).
fn stores(r: &Responses, projects: &[&Value]) -> Vec<Store> {
    let mut out: Vec<Store> = vec![];
    for p in projects {
        let pid = s(p, "id");
        let Some(list) = r.get(&format!("/v10/projects/{pid}/env")) else {
            continue;
        };
        for e in envs(list).iter().filter(|e| production(e)) {
            let hint = &e["contentHint"];
            let (kind, id) = (s(hint, "type"), s(hint, "storeId"));
            let kind = if kind.starts_with("postgres-") {
                "postgres"
            } else if kind.starts_with("redis-") {
                "redis"
            } else {
                continue;
            };
            if id.is_empty() {
                continue;
            }
            let at = match out.iter().position(|x| x.id == id) {
                Some(n) => n,
                None => {
                    out.push(Store {
                        kind,
                        id: id.to_owned(),
                        project: pid.to_owned(),
                        url: None,
                        pooled: None,
                        hosts: vec![],
                    });
                    out.len() - 1
                }
            };
            let value = value_of(r, pid, e);
            if let Some(h) = value
                .as_deref()
                .and_then(|v| reqwest::Url::parse(v).ok())
                .and_then(|u| u.host_str().map(str::to_owned))
                .filter(|h| !out[at].hosts.contains(h))
            {
                out[at].hosts.push(h);
            }
            let v = value.filter(|v| v.starts_with("postgres"));
            match s(hint, "type") {
                "postgres-url-non-pooling" if out[at].url.is_none() => out[at].url = v,
                "postgres-url" if out[at].pooled.is_none() => out[at].pooled = v,
                _ => {}
            }
        }
    }
    out
}

fn store_database(st: &Store, project_name: &str, taken: &mut HashSet<String>) -> ImportDatabase {
    let url = st.url.clone().or_else(|| st.pooled.clone());
    let (engine, base, source_name) = match st.kind {
        "postgres" => (
            "postgres",
            format!("{project_name}-db"),
            "Postgres database",
        ),
        _ => ("redis", format!("{project_name}-kv"), "key-value store"),
    };
    let (target, action, note) = database_action(engine, url.is_some(), P);
    let mut warnings: Vec<String> = vec![];
    if engine == "postgres" && url.is_none() {
        warnings.push("Its URL is secret in Vercel, so Pyo can't copy it. Pyo makes it empty: import a dump into it.".into());
    } else {
        warnings.extend(note);
    }
    ImportDatabase {
        key: format!("db:{}", st.id),
        source_name: source_name.to_owned(),
        name: unique(&base, taken),
        engine: engine.to_owned(),
        image: String::new(),
        target: target.map(str::to_owned),
        action: action.to_owned(),
        warnings,
        existing_id: None,
        url: if engine == "postgres" { url } else { None },
        hosts: st.hosts.clone(),
    }
}

fn project_service(
    r: &Responses,
    p: &Value,
    name: String,
    dbs: &[ImportDatabase],
    seen: &mut HashSet<String>,
) -> ImportService {
    let pid = s(p, "id");
    let source_name = s(p, "name").to_owned();
    let mut svc = plan_service(&name, &source_name);
    let mut warnings = vec![];
    let mut importable = true;
    let framework = p["framework"].as_str().unwrap_or("");
    let root = s(p, "rootDirectory").trim_matches('/').to_owned();
    let link = &p["link"];
    match repository(link) {
        Some(repo_url) => {
            let branch = s(link, "productionBranch");
            svc.source = PlanSource {
                kind: "git".into(),
                repo_url,
                branch: if branch.is_empty() { "main" } else { branch }.to_owned(),
                build_context: root.clone(),
                ..Default::default()
            };
        }
        None => {
            importable = false;
            warnings.push(if link.is_object() {
                "Vercel builds it from a source Pyo can't build from. Pyo builds from a Git repository.".into()
            } else {
                "It isn't connected to a Git repository in Vercel (it was deployed with Vercel's CLI). Connect one, or import it in Pyo from its repository.".into()
            });
        }
    }

    // Build settings for Railpack.
    let mut build_args = vec![];
    let mut arg = |k: &str, v: &str| {
        let v = v.trim();
        if !v.is_empty() {
            build_args.push(PlanEnv {
                key: k.into(),
                value: v.into(),
                secret: false,
            });
        }
    };
    arg("RAILPACK_INSTALL_CMD", s(p, "installCommand"));
    arg("RAILPACK_BUILD_CMD", s(p, "buildCommand"));
    // "22.x" → "22".
    arg(
        "RAILPACK_NODE_VERSION",
        s(p, "nodeVersion").trim_end_matches(".x"),
    );
    if framework.is_empty() || STATIC.contains(&framework) {
        arg("RAILPACK_SPA_OUTPUT_DIR", s(p, "outputDirectory"));
    }

    // Production variables. Write-only ones by name; storage ones noted.
    let mut env: Vec<PlanEnv> = vec![];
    let mut locked = vec![];
    let mut blob = false;
    let mut integration = vec![];
    let mut edge_config = vec![];
    let list = r
        .get(&format!("/v10/projects/{pid}/env"))
        .map(envs)
        .unwrap_or(&[]);
    for e in list.iter().filter(|e| production(e)) {
        let key = s(e, "key");
        if key.is_empty() || s(e, "type") == "system" || env.iter().any(|x| x.key == key) {
            continue;
        }
        let hint = s(&e["contentHint"], "type");
        if hint.starts_with("blob-") {
            blob = true;
        } else if matches!(hint, "integration-store-secret" | "flags-connection-string") {
            integration.push(key);
        }
        if e["edgeConfigId"].as_str().is_some_and(|x| !x.is_empty()) {
            edge_config.push(key);
        }
        match value_of(r, pid, e) {
            Some(value) => {
                if PUBLIC_PREFIXES.iter().any(|x| key.starts_with(x)) {
                    build_args.push(PlanEnv {
                        key: key.into(),
                        value: value.clone(),
                        secret: false,
                    });
                }
                env.push(PlanEnv {
                    key: key.into(),
                    secret: compose::secret_key(key),
                    value,
                });
            }
            None => {
                locked.push(key);
                env.push(PlanEnv {
                    key: key.into(),
                    value: String::new(),
                    secret: true,
                });
            }
        }
    }
    if !locked.is_empty() {
        warnings.push(format!(
            "{} {} secret in Vercel, so {} can't be read. Set {} in Pyo.",
            locked.join(", "),
            if locked.len() == 1 { "is" } else { "are" },
            if locked.len() == 1 {
                "its value"
            } else {
                "their values"
            },
            if locked.len() == 1 { "it" } else { "them" },
        ));
    }
    if blob {
        warnings.push(
            "It uses a Vercel Blob store, which isn't carried over. Copy its files yourself."
                .into(),
        );
    }
    if !integration.is_empty() {
        warnings.push(format!(
            "{} come{} from a Vercel integration. Check {} still work{} outside Vercel.",
            integration.join(", "),
            if integration.len() == 1 { "s" } else { "" },
            if integration.len() == 1 { "it" } else { "they" },
            if integration.len() == 1 { "s" } else { "" },
        ));
    }
    if !edge_config.is_empty() {
        warnings.push(format!(
            "{} read{} a Vercel Edge Config, which Pyo doesn't have.",
            edge_config.join(", "),
            if edge_config.len() == 1 { "s" } else { "" },
        ));
    }
    if p["autoExposeSystemEnvs"].as_bool() == Some(true) {
        warnings
            .push("Vercel's own variables (VERCEL_URL and the others) aren't set in Pyo.".into());
    }
    if env
        .iter()
        .any(|e| !PUBLIC_PREFIXES.iter().any(|x| e.key.starts_with(x)))
    {
        warnings.push("Vercel gave every variable to the build too. Pyo gives the build only public ones (like NEXT_PUBLIC_). Add others as build arguments if the build needs them.".into());
    }
    warnings.extend(database_warnings(&env, dbs));
    svc.env = env;
    if svc.source.kind == "git" {
        svc.source.build_args = build_args;
    }

    // What runs differently.
    if framework == "nextjs" {
        warnings.push("Next.js runs as one server here (next start). Middleware, ISR and image optimization run in it, not on Vercel's edge network.".into());
    } else if ADAPTED.contains(&framework) {
        warnings.push(format!(
            "If it uses {framework}'s Vercel adapter, switch to its Node adapter so it runs as a server."
        ));
    } else if framework.is_empty() || STATIC.contains(&framework) {
        warnings.push("Vercel Functions (an api folder) and Edge Functions don't run in Pyo. Pyo serves what the build makes.".into());
    }
    let crons: Vec<String> = arr(&p["crons"]["definitions"])
        .iter()
        .map(|c| format!("{} ({})", s(c, "path"), s(c, "schedule")))
        .collect();
    if !crons.is_empty() {
        warnings.push(format!(
            "Vercel called {} on a schedule. Add a Pyo cron job that calls {}.",
            crons.join(", "),
            if crons.len() == 1 { "it" } else { "them" }
        ));
    }
    if !root.is_empty() && p["sourceFilesOutsideRootDirectory"].as_bool() == Some(true) {
        warnings.push(format!(
            "Vercel built {root} with files from the rest of the repository too. Pyo builds only {root}: if the build fails, build from the repository's root."
        ));
    }
    if p["passwordProtection"].is_object() {
        warnings.push("Its password protection isn't carried over: the service is public.".into());
    }

    // Domains, on port 3000.
    let mut web = Web::default();
    web.port(PORT, false);
    let domains = r
        .get(&format!("/v9/projects/{pid}/domains?limit=100"))
        .map(|d| arr(&d["domains"]))
        .unwrap_or(&[]);
    for d in domains {
        let host = s(d, "name");
        if let Some(to) = d["redirect"].as_str().filter(|x| !x.is_empty()) {
            warnings.push(format!(
                "{host} redirected to {to} there. Pyo doesn't redirect, so it's left out."
            ));
            continue;
        }
        if let Some(b) = d["gitBranch"].as_str().filter(|x| !x.is_empty()) {
            warnings.push(format!(
                "{host} served the {b} branch there, so it's left out."
            ));
            continue;
        }
        if d["customEnvironmentId"]
            .as_str()
            .is_some_and(|x| !x.is_empty())
        {
            warnings.push(format!(
                "{host} served a custom environment there, so it's left out."
            ));
            continue;
        }
        web.domain(P, host, Some(PORT), "", seen);
    }
    svc.health_check = health_check(Some(PORT), None);
    warnings.append(&mut web.warnings);
    svc.ports = web.ports;

    ImportService {
        key: format!("app:{pid}"),
        source_name,
        builder: if svc.source.kind == "git" {
            "railpack".into()
        } else {
            String::new()
        },
        service: svc,
        domains: web.domains,
        warnings,
        importable,
        existing_id: None,
    }
}

/// The plan from what `gather` read.
pub(super) fn plan(r: &Responses) -> ImportPlan {
    let mut out = ImportPlan {
        platform: "vercel".into(),
        host: HOST.into(),
        ..Default::default()
    };
    // The pages of projects, in order.
    let mut list: Vec<&Value> = vec![];
    let mut from: Option<String> = None;
    for _ in 0..MAX_PAGES {
        let Some(page) = r.get(&projects_path(from.as_deref())) else {
            break;
        };
        list.extend(arr(&page["projects"]));
        from = next_page(page);
        if from.is_none() {
            break;
        }
    }
    // The full project where it was read.
    let full: Vec<&Value> = list
        .iter()
        .map(|p| r.get(&format!("/v9/projects/{}", s(p, "id"))).unwrap_or(p))
        .collect();
    if full.is_empty() {
        let teams: Vec<String> = r
            .get("/v2/teams?limit=20")
            .map(|t| arr(&t["teams"]))
            .unwrap_or(&[])
            .iter()
            .map(|t| s(t, "slug").to_owned())
            .filter(|t| !t.is_empty())
            .collect();
        if !teams.is_empty() {
            out.warnings.push(format!(
                "Vercel has no projects here. The token also reaches these teams: {}. Pick one as the team.",
                teams.join(", ")
            ));
        }
        return out;
    }
    out.warnings.push("Preview deployments and their variables aren't imported. Pyo imports each project's production settings.".into());
    let stores = stores(r, &full);
    let mut db_names = HashSet::new();
    let mut by_store: BTreeMap<String, ImportDatabase> = BTreeMap::new();
    for st in &stores {
        let pname = full
            .iter()
            .find(|p| s(p, "id") == st.project)
            .map(|p| s(p, "name"))
            .unwrap_or("vercel");
        by_store.insert(st.id.clone(), store_database(st, pname, &mut db_names));
    }
    let all: Vec<ImportDatabase> = by_store.values().cloned().collect();
    let mut seen_domains = HashSet::new();
    for p in full {
        let pid = s(p, "id");
        let mut names = HashSet::new();
        let name = unique(s(p, "name"), &mut names);
        let mut project = ImportProject {
            key: pid.to_owned(),
            name: s(p, "name").to_owned(),
            ..Default::default()
        };
        project.databases = stores
            .iter()
            .filter(|st| st.project == pid)
            .filter_map(|st| by_store.get(&st.id).cloned())
            .collect();
        project
            .services
            .push(project_service(r, p, name, &all, &mut seen_domains));
        out.projects.push(project);
    }
    out
}
