//! Coolify (v4), through its API v1 with a bearer token
//! (https://coolify.io/docs/api-reference/authorization). Every field read
//! here is in Coolify's own openapi.json or its API controllers, at v4.7.5:
//! https://github.com/coollabsio/coolify/blob/v4.7.5/openapi.json
//!
//! The walk:
//! - `GET /projects`: `uuid`, `name` (ProjectController::projects).
//! - `GET /projects/{uuid}/environments`: `uuid`, `name`
//!   (ProjectController::get_environments).
//! - `GET /projects/{uuid}/{environment_uuid}`: the environment with its
//!   `applications`, `services`, `postgresqls`, `mysqls`, `mariadbs`,
//!   `mongodbs` and `redis` (ProjectController::environment_details).
//! - Per application: `GET /applications/{uuid}/envs` (EnvironmentVariable)
//!   and `GET /applications/{uuid}/storages` (`persistent_storages`,
//!   `file_storages`: ApplicationsController::storages).
//! - Per service (a one-click or Compose stack): `GET /services/{uuid}/envs`
//!   and `GET /services/{uuid}/applications` (its containers, each with
//!   `name` and `fqdn`: app/Models/ServiceApplication.php).
//! - `GET /databases`: every database, for engines the environment doesn't
//!   list (KeyDB, Dragonfly, ClickHouse).
//!
//! Values: Coolify sends variables' values, Compose files and database URLs
//! only to tokens with `read:sensitive` (or `root`), and never a variable
//! locked with `is_shown_once`.

use std::collections::{BTreeMap, HashSet};

use serde_json::Value;

use super::{
    Fetch, ImportDatabase, ImportDomain, ImportPlan, ImportProject, ImportService, ImportStack,
    Platform, Responses, Web, database_action, database_name, database_warnings, health_check,
    https_repo, plan_service, s, volume,
};
use crate::{
    compose::{self, PlanEnv, PlanSource},
    error::ApiResult,
};

const P: Platform = Platform::Coolify;

/// The environment's arrays of databases, with each one's engine.
const DATABASES: [(&str, &str); 5] = [
    ("postgresqls", "postgres"),
    ("mysqls", "mysql"),
    ("mariadbs", "mariadb"),
    ("mongodbs", "mongodb"),
    ("redis", "redis"),
];

fn arr(v: &Value) -> &[Value] {
    v.as_array().map(Vec::as_slice).unwrap_or(&[])
}

/// Reads everything the token sees.
pub(super) async fn gather(f: &Fetch) -> ApiResult<Responses> {
    let mut r = Responses::new();
    let projects = f.get("/projects").await?;
    let mut paths = vec![];
    for p in arr(&projects) {
        paths.push(format!("/projects/{}/environments", s(p, "uuid")));
    }
    r.insert("/projects".into(), projects);
    f.get_all(paths.clone(), &mut r).await?;
    let mut envs = vec![];
    for path in &paths {
        let project = path
            .trim_start_matches("/projects/")
            .trim_end_matches("/environments");
        for e in arr(&r[path]) {
            envs.push(format!("/projects/{project}/{}", s(e, "uuid")));
        }
    }
    f.get_all(envs.clone(), &mut r).await?;
    let mut more = vec!["/databases".to_owned()];
    for e in &envs {
        for a in arr(&r[e]["applications"]) {
            more.push(format!("/applications/{}/envs", s(a, "uuid")));
            more.push(format!("/applications/{}/storages", s(a, "uuid")));
        }
        for sv in arr(&r[e]["services"]) {
            more.push(format!("/services/{}/envs", s(sv, "uuid")));
            more.push(format!("/services/{}/applications", s(sv, "uuid")));
        }
    }
    f.get_all(more, &mut r).await?;
    Ok(r)
}

/// A variable's value: `real_value` resolves Coolify's shared variables
/// (`{{team.X}}`), `value` is as typed. None when Coolify didn't send it.
fn value(e: &Value) -> Option<String> {
    e["real_value"]
        .as_str()
        .or_else(|| e["value"].as_str())
        .map(str::to_owned)
}

/// Runtime variables and build arguments, and what couldn't be read.
fn variables(
    envs: &Value,
    warnings: &mut Vec<String>,
    unreadable: &mut bool,
) -> (Vec<PlanEnv>, Vec<PlanEnv>) {
    let (mut env, mut build) = (vec![], vec![]);
    let mut locked = vec![];
    for e in arr(envs) {
        // Preview deployments' own variables aren't the service's.
        if e["is_preview"].as_bool() == Some(true) {
            continue;
        }
        let key = s(e, "key").to_owned();
        if key.is_empty() {
            continue;
        }
        let shown_once = e["is_shown_once"].as_bool() == Some(true);
        let v = value(e);
        if shown_once {
            locked.push(key.clone());
        } else if v.is_none() {
            *unreadable = true;
        }
        let secret = shown_once || compose::secret_key(&key);
        let var = PlanEnv {
            key,
            value: v.unwrap_or_default(),
            secret,
        };
        let runtime = e["is_runtime"].as_bool() != Some(false);
        let buildtime = e["is_buildtime"].as_bool() == Some(true);
        if buildtime && !runtime {
            build.push(var);
        } else {
            env.push(var);
        }
    }
    if !locked.is_empty() {
        warnings.push(format!(
            "{} {} locked in Coolify, so {} value can't be read. Set {} in Pyo.",
            locked.join(", "),
            if locked.len() == 1 { "is" } else { "are" },
            if locked.len() == 1 { "its" } else { "their" },
            if locked.len() == 1 { "it" } else { "them" },
        ));
    }
    (env, build)
}

/// `fqdn`: "https://a.com,http://b.com:3000/api" → (host, port, path).
fn fqdns(s: &str) -> Vec<(String, Option<u16>, String)> {
    s.split(',')
        .filter_map(|d| {
            let d = d.trim();
            if d.is_empty() {
                return None;
            }
            let u = if d.contains("://") {
                d.to_owned()
            } else {
                format!("http://{d}")
            };
            let url = reqwest::Url::parse(&u).ok()?;
            Some((
                url.host_str()?.to_owned(),
                url.port(),
                url.path().to_owned(),
            ))
        })
        .collect()
}

/// `ports_exposes`: "3000,8080".
fn exposed(s: &str) -> Vec<u16> {
    s.split(',')
        .filter_map(|p| p.trim().parse().ok())
        .filter(|p| *p > 0)
        .collect()
}

fn application(
    a: &Value,
    r: &Responses,
    name: String,
    dbs: &[ImportDatabase],
    seen: &mut HashSet<String>,
    unreadable: &mut bool,
) -> ImportService {
    let uuid = s(a, "uuid");
    let source_name = s(a, "name").to_owned();
    let mut svc = plan_service(&name, &source_name);
    let mut warnings = vec![];
    let mut importable = true;
    let mut builder = String::new();
    let repo = s(a, "git_repository").trim();
    // Coolify keeps a placeholder repository for applications made from a
    // Docker image or a pasted Dockerfile (ApplicationsController.php,
    // create_dockerfile_application and create_dockerimage_application).
    let placeholder = repo.is_empty() || repo == "coollabsio/coolify";
    match s(a, "build_pack") {
        "dockerimage" => {
            let image = s(a, "docker_registry_image_name").trim();
            let tag = a["docker_registry_image_tag"]
                .as_str()
                .map(str::trim)
                .filter(|t| !t.is_empty())
                .unwrap_or("latest");
            if image.is_empty() {
                importable = false;
                warnings.push("Coolify doesn't say which image it runs.".into());
            }
            svc.source = PlanSource {
                kind: "image".into(),
                image: image.into(),
                reference: tag.into(),
                ..Default::default()
            };
        }
        "dockerfile" if placeholder && !s(a, "dockerfile").is_empty() => {
            importable = false;
            warnings.push("It's built from a Dockerfile pasted into Coolify. Pyo builds from a repository: put the Dockerfile in one and deploy that.".into());
        }
        "dockercompose" => {
            importable = false;
            warnings.push(format!(
                "It's a Compose file in its repository ({}). Import it with Pyo's Compose import from the repository.",
                s(a, "docker_compose_location").trim_start_matches('/')
            ));
        }
        pack => {
            // `git_repository` is owner/name on github.com, else the URL
            // (ApplicationsController.php, create_public_application).
            let url = if repo.contains("://") || repo.starts_with("git@") {
                https_repo(repo)
            } else if repo.split('/').count() == 2 {
                Some(format!("https://github.com/{repo}"))
            } else {
                None
            };
            match url {
                Some(url) if !placeholder => {
                    svc.source = PlanSource {
                        kind: "git".into(),
                        repo_url: url,
                        branch: a["git_branch"]
                            .as_str()
                            .filter(|b| !b.is_empty())
                            .unwrap_or("main")
                            .into(),
                        build_context: s(a, "base_directory").trim_matches('/').into(),
                        dockerfile_path: if pack == "dockerfile" {
                            s(a, "dockerfile_location").trim_start_matches('/').into()
                        } else {
                            String::new()
                        },
                        ..Default::default()
                    };
                    builder = if pack == "dockerfile" {
                        "dockerfile"
                    } else {
                        "railpack"
                    }
                    .into();
                    if pack == "static" {
                        warnings.push("Coolify served it as a static site. Pyo builds it with zero config: check it serves your files.".into());
                    }
                    if a["private_key_id"].is_number() {
                        warnings.push("Coolify cloned it with a deploy key. Pyo builds public repositories, and private GitHub ones through its GitHub app.".into());
                    }
                    for (k, what) in [
                        ("install_command", "install"),
                        ("build_command", "build"),
                        ("start_command", "start"),
                    ] {
                        if !s(a, k).trim().is_empty() {
                            warnings.push(format!("Its {what} command isn't carried over. Zero config finds one, or put it in a Dockerfile."));
                        }
                    }
                }
                _ => {
                    importable = false;
                    warnings.push("Coolify doesn't say which repository it's built from.".into());
                }
            }
        }
    }

    // Variables.
    let (env, build) = variables(
        &r[&format!("/applications/{uuid}/envs")],
        &mut warnings,
        unreadable,
    );
    warnings.extend(database_warnings(&env, dbs));
    svc.env = env;
    if svc.source.kind == "git" {
        svc.source.build_args = build;
    } else if !build.is_empty() {
        warnings.push("Its build variables aren't used: it runs an image.".into());
    }

    // Ports and domains.
    let mut web = Web::default();
    for p in exposed(s(a, "ports_exposes")) {
        web.port(p, false);
    }
    for (host, port, path) in fqdns(s(a, "fqdn")) {
        web.domain(P, &host, port, &path, seen);
    }
    // `ports_mappings`: "8080:80,5432:5432" (host:container).
    for m in s(a, "ports_mappings")
        .split(',')
        .map(str::trim)
        .filter(|m| !m.is_empty())
    {
        let parts: Vec<&str> = m.split(':').collect();
        let (published, target) = match parts.as_slice() {
            [p, t] => (
                p.parse().ok(),
                t.split('/').next().and_then(|t| t.parse().ok()),
            ),
            _ => (None, None),
        };
        match (published, target) {
            (Some(p), Some(t)) => {
                web.published(p, t, if m.ends_with("/udp") { "udp" } else { "tcp" })
            }
            _ => warnings.push(format!(
                "Port mapping {m} isn't one Pyo can read, so it's left out."
            )),
        }
    }
    let first = web.ports.first().map(|p| p.container_port);
    let health_port = a["health_check_port"]
        .as_str()
        .and_then(|p| p.parse().ok())
        .or(first);
    let path =
        (a["health_check_enabled"].as_bool() == Some(true)).then(|| s(a, "health_check_path"));
    svc.health_check = health_check(health_port, path);
    if web.ports.is_empty() && importable {
        warnings.push(
            "Coolify doesn't say which port it listens on. Add one if it serves traffic.".into(),
        );
    }
    warnings.append(&mut web.warnings);
    svc.ports = web.ports;

    // Resources: `limits_memory` ("512m", "0" for none), `limits_cpus` ("1.5").
    if let Some(m) = compose::memory_mi(s(a, "limits_memory")).filter(|m| *m > 1) {
        svc.resources.memory_limit = m.max(svc.resources.memory_request);
    }
    if let Some(c) = s(a, "limits_cpus")
        .trim()
        .parse::<f64>()
        .ok()
        .filter(|c| *c > 0.0)
    {
        svc.resources.cpu_limit = ((c * 1000.0).round() as i64).max(svc.resources.cpu_request);
    }

    // Volumes: persistent storages (a `host_path` is a directory on the
    // server); file storages aren't copied. LocalPersistentVolume and
    // LocalFileVolume in app/Models.
    let storages = &r[&format!("/applications/{uuid}/storages")];
    for v in arr(&storages["persistent_storages"]) {
        let bind = v["host_path"].as_str().is_some_and(|h| !h.is_empty());
        svc.volumes
            .push(volume(s(v, "name"), s(v, "mount_path"), bind));
        if bind {
            warnings.push(format!(
                "{} was a directory on the server. In Pyo it's an empty volume.",
                s(v, "mount_path")
            ));
        } else {
            warnings.push(format!(
                "Volume {} starts empty. Its data isn't copied.",
                s(v, "mount_path")
            ));
        }
    }
    for v in arr(&storages["file_storages"]) {
        warnings.push(format!(
            "File {} isn't carried over. Add it to the image or a volume.",
            s(v, "mount_path")
        ));
    }

    ImportService {
        key: format!("app:{uuid}"),
        source_name,
        builder,
        service: svc,
        domains: web.domains,
        warnings,
        importable,
        existing_id: None,
    }
}

/// A Coolify service: a Compose stack (one-click services are too).
fn service(
    sv: &Value,
    r: &Responses,
    project: &str,
    seen: &mut HashSet<String>,
    unreadable: &mut bool,
) -> ImportStack {
    let uuid = s(sv, "uuid");
    let source_name = s(sv, "name").to_owned();
    let mut warnings = vec![];
    let (env, _) = variables(
        &r[&format!("/services/{uuid}/envs")],
        &mut warnings,
        unreadable,
    );
    let raw = s(sv, "docker_compose_raw");
    let mut stack = ImportStack {
        key: format!("compose:{uuid}"),
        source_name: source_name.clone(),
        importable: false,
        ..Default::default()
    };
    if raw.trim().is_empty() {
        warnings
            .push("Coolify didn't send its Compose file. Give the token read:sensitive.".into());
        stack.warnings = warnings;
        return stack;
    }
    // Its variables fill the file's ${VAR}s, Coolify's generated ones
    // (SERVICE_PASSWORD_…, SERVICE_FQDN_…) among them.
    let dotenv: String = env
        .iter()
        .map(|e| format!("{}={}\n", e.key, dotenv_quote(&e.value)))
        .collect();
    let mut files = BTreeMap::new();
    files.insert(".env".to_owned(), dotenv);
    let name = format!("{project} {source_name}");
    match compose::plan(&compose::Input {
        file: raw,
        files,
        repo: None,
        name_hint: &name,
        domain_base: None,
    }) {
        Ok(mut plan) => {
            plan.project.name = name;
            // The stack's containers and their domains.
            for c in arr(&r[&format!("/services/{uuid}/applications")]) {
                let cname = s(c, "name");
                let Some(ps) = plan.services.iter_mut().find(|x| x.compose_name == cname) else {
                    continue;
                };
                let mut web = Web {
                    ports: std::mem::take(&mut ps.ports),
                    ..Default::default()
                };
                for (host, port, path) in fqdns(s(c, "fqdn")) {
                    web.domain(P, &host, port, &path, seen);
                }
                ps.ports = web.ports;
                stack
                    .domains
                    .extend(web.domains.into_iter().map(|d| ImportDomain {
                        service: cname.to_owned(),
                        ..d
                    }));
                warnings.extend(web.warnings.into_iter().map(|w| format!("{cname}: {w}")));
            }
            for w in &plan.warnings {
                warnings.push(match &w.service {
                    Some(sn) => format!("{sn}: {}", w.message),
                    None => w.message.clone(),
                });
            }
            stack.importable = !plan.services.is_empty();
            stack.plan = plan;
        }
        Err(e) => warnings.push(format!("Its Compose file can't be read: {e}")),
    }
    stack.warnings = warnings;
    stack
}

/// A value for a .env line, quoted so the Compose reader takes it as is.
fn dotenv_quote(v: &str) -> String {
    if v.chars()
        .all(|c| c.is_ascii_alphanumeric() || "-_./:@+,".contains(c))
    {
        v.to_owned()
    } else {
        format!("'{}'", v.replace('\'', "'\\''"))
    }
}

fn database(d: &Value, engine: &str) -> ImportDatabase {
    // `is_public` and `external_db_url` (with the password) when Coolify
    // publishes it; the container is named by its uuid inside Coolify's
    // network (`internal_db_url`).
    let url = d["external_db_url"]
        .as_str()
        .filter(|u| !u.is_empty() && d["is_public"].as_bool() != Some(false));
    let (target, action, note) = database_action(engine, url.is_some(), P);
    ImportDatabase {
        key: format!("db:{}", s(d, "uuid")),
        source_name: s(d, "name").to_owned(),
        name: database_name(s(d, "name")),
        engine: engine.to_owned(),
        image: s(d, "image").to_owned(),
        target: target.map(str::to_owned),
        action: action.to_owned(),
        warnings: note.into_iter().collect(),
        existing_id: None,
        url: url.map(str::to_owned),
        hosts: vec![s(d, "uuid").to_owned()],
    }
}

/// The plan from what `gather` read.
pub(super) fn plan(r: &Responses, host: &str) -> ImportPlan {
    let mut out = ImportPlan {
        platform: "coolify".into(),
        host: host.into(),
        ..Default::default()
    };
    let mut seen_domains = HashSet::new();
    let mut seen_dbs = HashSet::new();
    let mut unreadable = false;
    // Environment ids (the Environment schema's `id`), by project.
    let mut env_ids: Vec<Option<i64>> = vec![];
    let empty = Value::Null;
    for p in arr(r.get("/projects").unwrap_or(&empty)) {
        let pid = s(p, "uuid");
        let envs = r
            .get(&format!("/projects/{pid}/environments"))
            .unwrap_or(&empty);
        let several = arr(envs).len() > 1;
        for e in arr(envs) {
            let eid = s(e, "uuid");
            let Some(env) = r.get(&format!("/projects/{pid}/{eid}")) else {
                continue;
            };
            let name = if several && s(e, "name") != "production" {
                format!("{} ({})", s(p, "name"), s(e, "name"))
            } else {
                s(p, "name").to_owned()
            };
            let mut project = ImportProject {
                key: format!("{pid}/{eid}"),
                name: name.clone(),
                ..Default::default()
            };
            for (k, engine) in DATABASES {
                for d in arr(&env[k]) {
                    seen_dbs.insert(s(d, "uuid").to_owned());
                    project.databases.push(database(d, engine));
                }
            }
            let mut names = HashSet::new();
            for a in arr(&env["applications"]) {
                let n = super::unique(s(a, "name"), &mut names);
                project.services.push(application(
                    a,
                    r,
                    n,
                    &project.databases,
                    &mut seen_domains,
                    &mut unreadable,
                ));
            }
            for sv in arr(&env["services"]) {
                project
                    .stacks
                    .push(service(sv, r, &name, &mut seen_domains, &mut unreadable));
            }
            out.projects.push(project);
            env_ids.push(env["id"].as_i64());
        }
    }
    // Databases the environments don't list (`database_type` is
    // "standalone-<engine>": the Database schema's enum).
    for d in arr(r.get("/databases").unwrap_or(&empty)) {
        if seen_dbs.contains(s(d, "uuid")) {
            continue;
        }
        let engine = s(d, "database_type").trim_start_matches("standalone-");
        let engine = if engine == "postgresql" {
            "postgres"
        } else {
            engine
        };
        let at = d["environment_id"]
            .as_i64()
            .and_then(|id| env_ids.iter().position(|e| *e == Some(id)));
        match at {
            Some(i) => {
                let db = database(d, engine);
                out.projects[i].databases.push(db);
            }
            None => out.warnings.push(format!(
                "Coolify doesn't say which project {} ({engine}) is in, so it's left out.",
                s(d, "name")
            )),
        }
    }
    if unreadable {
        out.warnings.push("The token can't read variables' values, so they come over empty. Give it read:sensitive in Coolify.".into());
    }
    out
}
