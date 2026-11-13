//! Dokploy, through its API with an `x-api-key` header
//! (https://docs.dokploy.com/docs/api). Its answers are its database rows:
//! every field read here is a column in Dokploy's Drizzle schema or a
//! relation its routers load, at v0.31.2:
//! https://github.com/Dokploy/dokploy/tree/v0.31.2/packages/server/src/db/schema
//!
//! The walk:
//! - `GET /project.all`: projects with their `environments`, each listing
//!   its `applications`, `compose`, `postgres`, `mysql`, `mariadb`, `mongo`,
//!   `redis` and `libsql` by id (apps/dokploy/server/api/routers/project.ts).
//! - `GET /environment.one?environmentId=`: the environment's shared `env`.
//! - `GET /application.one?applicationId=`: the application with its
//!   `domains`, `mounts`, `ports`, `gitlab`, `gitea` and `server`
//!   (services/application.ts, findApplicationById).
//! - `GET /compose.one?composeId=`: the stack with `domains` and `mounts`.
//! - `GET /<engine>.one?<engine>Id=`: a database with its `server`.
//!
//! Variables are `KEY=value` lines (`env`), and may use the project's and
//! environment's shared ones as `${{project.KEY}}`, `${{environment.KEY}}`
//! or another of their own as `${{KEY}}`, resolved here as Dokploy does
//! (packages/server/src/utils/docker/utils.ts, prepareEnvironmentVariables).

use std::collections::{BTreeMap, HashSet};

use serde_json::Value;

use super::{
    Fetch, ImportDatabase, ImportDomain, ImportPlan, ImportProject, ImportService, ImportStack,
    Platform, Responses, Web, database_action, database_warnings, health_check, https_repo,
    plan_service, s, unique, volume,
};
use crate::{
    compose::{self, PlanEnv, PlanSource},
    error::ApiResult,
};

const P: Platform = Platform::Dokploy;

/// Dokploy's database kinds: the environment's array, the router, its id
/// column, and the engine.
const DATABASES: [(&str, &str, &str); 6] = [
    ("postgres", "postgres", "postgres"),
    ("mysql", "mysql", "mysql"),
    ("mariadb", "mariadb", "mariadb"),
    ("mongo", "mongo", "mongodb"),
    ("redis", "redis", "redis"),
    ("libsql", "libsql", "libsql"),
];

fn arr(v: &Value) -> &[Value] {
    v.as_array().map(Vec::as_slice).unwrap_or(&[])
}

/// A project's environments: older Dokploy had none, with everything on the
/// project itself.
fn environments(p: &Value) -> Vec<&Value> {
    match p["environments"].as_array() {
        Some(e) => e.iter().collect(),
        None => vec![p],
    }
}

fn one(kind: &str, id: &str) -> String {
    format!("/{kind}.one?{kind}Id={id}")
}

/// Reads everything the key sees.
pub(super) async fn gather(f: &Fetch) -> ApiResult<Responses> {
    let mut r = Responses::new();
    let projects = f.get("/project.all").await?;
    let mut paths = vec![];
    for p in arr(&projects) {
        for e in environments(p) {
            if let Some(id) = e["environmentId"].as_str() {
                paths.push(format!("/environment.one?environmentId={id}"));
            }
            for a in arr(&e["applications"]) {
                paths.push(one("application", s(a, "applicationId")));
            }
            for c in arr(&e["compose"]) {
                paths.push(one("compose", s(c, "composeId")));
            }
            for (list, kind, _) in DATABASES {
                // Pyo has no libSQL databases: listed, not read.
                if kind == "libsql" {
                    continue;
                }
                for d in arr(&e[list]) {
                    paths.push(one(kind, s(d, &format!("{kind}Id"))));
                }
            }
        }
    }
    r.insert("/project.all".into(), projects);
    f.get_all(paths, &mut r).await?;
    Ok(r)
}

/// `KEY=value` lines, resolved against the shared ones.
fn resolve(
    text: &str,
    project: &str,
    environment: &str,
    warnings: &mut Vec<String>,
) -> Vec<(String, String)> {
    let shared_p = compose::parse_dotenv(project);
    let shared_e = compose::parse_dotenv(environment);
    let own = compose::parse_dotenv(text);
    let mut missing = vec![];
    let out = own
        .iter()
        .map(|(k, v)| {
            let mut v = v.clone();
            let mut rest = String::new();
            while let Some(i) = v.find("${{") {
                let Some(j) = v[i..].find("}}") else { break };
                let r = v[i + 3..i + j].trim().to_owned();
                let got = if let Some(n) = r.strip_prefix("project.") {
                    shared_p.get(n)
                } else if let Some(n) = r.strip_prefix("environment.") {
                    shared_e.get(n)
                } else {
                    own.get(&r)
                };
                rest.push_str(&v[..i]);
                match got {
                    Some(x) => rest.push_str(x),
                    None => {
                        missing.push(r.clone());
                        rest.push_str(&v[i..i + j + 2]);
                    }
                }
                v = v[i + j + 2..].to_owned();
            }
            rest.push_str(&v);
            (k.clone(), rest)
        })
        .collect();
    if !missing.is_empty() {
        missing.sort();
        missing.dedup();
        warnings.push(format!(
            "{} can't be resolved, so {} kept as written. Set the values in Pyo.",
            missing
                .iter()
                .map(|m| format!("${{{{{m}}}}}"))
                .collect::<Vec<_>>()
                .join(", "),
            if missing.len() == 1 {
                "it's"
            } else {
                "they're"
            }
        ));
    }
    out
}

fn env_list(pairs: Vec<(String, String)>) -> Vec<PlanEnv> {
    pairs
        .into_iter()
        .map(|(key, value)| PlanEnv {
            secret: compose::secret_key(&key),
            key,
            value,
        })
        .collect()
}

/// The repository an application or stack builds from, its branch and
/// build path, by `sourceType` (application.ts and compose.ts columns).
fn repository(a: &Value) -> Result<(String, String, String), String> {
    let path = |k: &str| a[k].as_str().unwrap_or("").trim_matches('/').to_owned();
    match s(a, "sourceType") {
        "github" => Ok((
            format!("https://github.com/{}/{}", s(a, "owner"), s(a, "repository")),
            s(a, "branch").to_owned(),
            path("buildPath"),
        )),
        "gitlab" => {
            let base = a["gitlab"]["gitlabUrl"].as_str().unwrap_or("https://gitlab.com").trim_end_matches('/');
            let ns = a["gitlabPathNamespace"].as_str().filter(|n| !n.is_empty()).map(str::to_owned)
                .unwrap_or_else(|| format!("{}/{}", s(a, "gitlabOwner"), s(a, "gitlabRepository")));
            Ok((format!("{base}/{ns}"), s(a, "gitlabBranch").to_owned(), path("gitlabBuildPath")))
        }
        "gitea" => {
            let base = a["gitea"]["giteaUrl"].as_str().unwrap_or("https://gitea.com").trim_end_matches('/');
            Ok((format!("{base}/{}/{}", s(a, "giteaOwner"), s(a, "giteaRepository")), s(a, "giteaBranch").to_owned(), path("giteaBuildPath")))
        }
        "bitbucket" => {
            let slug = a["bitbucketRepositorySlug"].as_str().filter(|x| !x.is_empty()).unwrap_or(s(a, "bitbucketRepository"));
            Ok((format!("https://bitbucket.org/{}/{slug}", s(a, "bitbucketOwner")), s(a, "bitbucketBranch").to_owned(), path("bitbucketBuildPath")))
        }
        "git" => https_repo(s(a, "customGitUrl"))
            .map(|u| (u, s(a, "customGitBranch").to_owned(), path("customGitBuildPath")))
            .ok_or_else(|| "Its Git URL isn't one Pyo can build from.".to_owned()),
        "drop" => Err("It was deployed from an uploaded folder. Pyo builds from a repository or runs an image.".into()),
        other => Err(format!("Pyo can't build from a {other} source.")),
    }
}

fn mounts(a: &Value, svc: &mut compose::PlanService, warnings: &mut Vec<String>) {
    // `type`: "bind", "volume" or "file" (mount.ts).
    for m in arr(&a["mounts"]) {
        let at = s(m, "mountPath");
        match s(m, "type") {
            "volume" => {
                svc.volumes.push(volume(s(m, "volumeName"), at, false));
                warnings.push(format!("Volume {at} starts empty. Its data isn't copied."));
            }
            "bind" => {
                let name = at.trim_matches('/').replace('/', "-");
                svc.volumes.push(volume(&name, at, true));
                warnings.push(format!(
                    "{at} was a directory on the server. In Pyo it's an empty volume."
                ));
            }
            _ => warnings.push(format!(
                "File {at} isn't carried over. Add it to the image or a volume."
            )),
        }
    }
}

fn application(
    a: &Value,
    name: String,
    shared: (&str, &str),
    dbs: &[ImportDatabase],
    seen: &mut HashSet<String>,
) -> ImportService {
    let id = s(a, "applicationId");
    let source_name = s(a, "name").to_owned();
    let mut svc = plan_service(&name, &source_name);
    let mut warnings = vec![];
    let mut importable = true;
    let mut builder = String::new();
    if s(a, "sourceType") == "docker" {
        let (image, tag) = compose::split_image(s(a, "dockerImage"));
        if image.is_empty() {
            importable = false;
            warnings.push("Dokploy doesn't say which image it runs.".into());
        }
        if a["registryUrl"].as_str().is_some_and(|u| !u.is_empty())
            || a["username"].as_str().is_some_and(|u| !u.is_empty())
        {
            warnings
                .push("It's pulled with a registry login. Add that login in Pyo's Images.".into());
        }
        svc.source = PlanSource {
            kind: "image".into(),
            image,
            reference: tag,
            ..Default::default()
        };
    } else {
        match repository(a) {
            Ok((repo_url, branch, build_path)) => {
                // `buildType` (application.ts): dockerfile, heroku_buildpacks,
                // paketo_buildpacks, nixpacks, static or railpack.
                let kind = s(a, "buildType");
                let dockerfile = kind == "dockerfile";
                let context = a["dockerContextPath"]
                    .as_str()
                    .map(|c| c.trim_matches('/'))
                    .filter(|c| !c.is_empty() && *c != ".")
                    .map(str::to_owned);
                svc.source = PlanSource {
                    kind: "git".into(),
                    repo_url,
                    branch: if branch.is_empty() {
                        "main".into()
                    } else {
                        branch
                    },
                    build_context: if dockerfile {
                        context.unwrap_or(build_path)
                    } else {
                        build_path
                    },
                    dockerfile_path: if dockerfile {
                        s(a, "dockerfile").trim_start_matches("./").to_owned()
                    } else {
                        String::new()
                    },
                    ..Default::default()
                };
                builder = if dockerfile { "dockerfile" } else { "railpack" }.into();
                match kind {
                    "heroku_buildpacks" | "paketo_buildpacks" => warnings.push("Dokploy built it with buildpacks. Pyo builds it with zero config (Railpack).".into()),
                    "static" => warnings.push("Dokploy served it as a static site. Pyo builds it with zero config: check it serves your files.".into()),
                    _ => {}
                }
                if a["customGitSSHKeyId"]
                    .as_str()
                    .is_some_and(|k| !k.is_empty())
                {
                    warnings.push("Dokploy cloned it with an SSH key. Pyo builds public repositories, and private GitHub ones through its GitHub app.".into());
                }
                if !s(a, "command").trim().is_empty() {
                    warnings.push("Its start command isn't carried over. Zero config finds one, or put it in a Dockerfile.".into());
                }
            }
            Err(why) => {
                importable = false;
                warnings.push(why);
            }
        }
    }

    // Variables, and build arguments for a build.
    let env = env_list(resolve(s(a, "env"), shared.0, shared.1, &mut warnings));
    warnings.extend(database_warnings(&env, dbs));
    svc.env = env;
    if svc.source.kind == "git" {
        svc.source.build_args = env_list(resolve(
            s(a, "buildArgs"),
            shared.0,
            shared.1,
            &mut warnings,
        ));
    }
    if !s(a, "buildSecrets").trim().is_empty() {
        warnings.push("Its build secrets aren't carried over. Add them as build arguments if the build needs them.".into());
    }

    // Domains (domain.ts: `host`, `port`, `path`) and published ports
    // (port.ts: `publishedPort`, `targetPort`, `protocol`).
    let mut web = Web::default();
    for d in arr(&a["domains"]) {
        if s(d, "domainType") == "preview" || d["enabled"].as_bool() == Some(false) {
            continue;
        }
        let port = d["port"].as_u64().and_then(|p| u16::try_from(p).ok());
        web.domain(P, s(d, "host"), port, s(d, "path"), seen);
    }
    for p in arr(&a["ports"]) {
        let (Some(pp), Some(tp)) = (p["publishedPort"].as_u64(), p["targetPort"].as_u64()) else {
            continue;
        };
        match (u16::try_from(pp), u16::try_from(tp)) {
            (Ok(pp), Ok(tp)) => web.published(pp, tp, s(p, "protocol")),
            _ => warnings.push(format!("Port {pp} isn't one Pyo can use.")),
        }
    }
    svc.health_check = health_check(web.ports.first().map(|p| p.container_port), None);
    if web.ports.is_empty() && importable {
        warnings.push(
            "Dokploy doesn't say which port it listens on. Add one if it serves traffic.".into(),
        );
    }
    warnings.append(&mut web.warnings);
    svc.ports = web.ports;
    svc.replicas = a["replicas"].as_i64().map_or(1, |r| r.clamp(1, 20) as i32);
    mounts(a, &mut svc, &mut warnings);

    ImportService {
        key: format!("app:{id}"),
        source_name,
        builder,
        service: svc,
        domains: web.domains,
        warnings,
        importable,
        existing_id: None,
    }
}

fn stack(
    c: &Value,
    project: &str,
    shared: (&str, &str),
    seen: &mut HashSet<String>,
) -> ImportStack {
    let id = s(c, "composeId");
    let source_name = s(c, "name").to_owned();
    let mut warnings = vec![];
    let mut out = ImportStack {
        key: format!("compose:{id}"),
        source_name: source_name.clone(),
        ..Default::default()
    };
    // `composeFile` holds the file for a "raw" stack; others read it from
    // their repository at deploy time (`composePath`).
    let file = s(c, "composeFile");
    if s(c, "sourceType") != "raw" || file.trim().is_empty() {
        warnings.push(format!(
            "It's a Compose file in its repository ({}). Import it with Pyo's Compose import from the repository.",
            s(c, "composePath").trim_start_matches("./")
        ));
        out.warnings = warnings;
        return out;
    }
    if s(c, "composeType") == "stack" {
        warnings.push("It ran as a Docker Swarm stack. Pyo reads it as a Compose file.".into());
    }
    let env = resolve(s(c, "env"), shared.0, shared.1, &mut warnings);
    let mut files = BTreeMap::new();
    files.insert(
        ".env".to_owned(),
        env.iter()
            .map(|(k, v)| format!("{k}={}\n", quote(v)))
            .collect(),
    );
    let name = format!("{project} {source_name}");
    match compose::plan(&compose::Input {
        file,
        files,
        repo: None,
        name_hint: &name,
        domain_base: None,
    }) {
        Ok(mut plan) => {
            plan.project.name = name;
            // Domains name the Compose service they're for (`serviceName`).
            for d in arr(&c["domains"]) {
                let sn = s(d, "serviceName");
                let Some(ps) = plan.services.iter_mut().find(|x| x.compose_name == sn) else {
                    warnings.push(format!(
                        "{} is for {sn}, which isn't in the file, so it's left out.",
                        s(d, "host")
                    ));
                    continue;
                };
                let mut web = Web {
                    ports: std::mem::take(&mut ps.ports),
                    ..Default::default()
                };
                let port = d["port"].as_u64().and_then(|p| u16::try_from(p).ok());
                web.domain(P, s(d, "host"), port, s(d, "path"), seen);
                ps.ports = web.ports;
                out.domains
                    .extend(web.domains.into_iter().map(|x| ImportDomain {
                        service: sn.to_owned(),
                        ..x
                    }));
                warnings.extend(web.warnings.into_iter().map(|w| format!("{sn}: {w}")));
            }
            for w in &plan.warnings {
                warnings.push(match &w.service {
                    Some(sn) => format!("{sn}: {}", w.message),
                    None => w.message.clone(),
                });
            }
            out.importable = !plan.services.is_empty();
            out.plan = plan;
        }
        Err(e) => warnings.push(format!("Its Compose file can't be read: {e}")),
    }
    out.warnings = warnings;
    out
}

fn quote(v: &str) -> String {
    if v.chars()
        .all(|c| c.is_ascii_alphanumeric() || "-_./:@+,".contains(c))
    {
        v.to_owned()
    } else {
        format!("'{}'", v.replace('\'', "'\\''"))
    }
}

/// A database: reachable from outside when it has an `externalPort`, on
/// its server's `ipAddress` (server.ts), else on Dokploy's own address.
fn database(d: &Value, engine: &str, host: &str, taken: &mut HashSet<String>) -> ImportDatabase {
    let enc = |x: &str| url_escape(x);
    let ip = d["server"]["ipAddress"]
        .as_str()
        .filter(|i| !i.is_empty())
        .unwrap_or(host);
    let url = d["externalPort"].as_u64().and_then(|port| match engine {
        "postgres" => Some(format!(
            "postgres://{}:{}@{ip}:{port}/{}",
            enc(s(d, "databaseUser")),
            enc(s(d, "databasePassword")),
            enc(s(d, "databaseName"))
        )),
        "mysql" => Some(format!(
            "mysql://{}:{}@{ip}:{port}/{}",
            enc(s(d, "databaseUser")),
            enc(s(d, "databasePassword")),
            enc(s(d, "databaseName"))
        )),
        _ => None,
    });
    let reachable = url.is_some();
    let (target, action, note) = database_action(engine, reachable, P);
    let mut warnings: Vec<String> = note.into_iter().collect();
    if reachable && d["server"]["ipAddress"].as_str().is_none_or(str::is_empty) {
        warnings.push(format!("Pyo copies it from {host}, Dokploy's own address."));
    }
    ImportDatabase {
        key: format!(
            "db:{}",
            s(
                d,
                &format!("{}Id", if engine == "mongodb" { "mongo" } else { engine })
            )
        ),
        source_name: s(d, "name").to_owned(),
        name: unique(s(d, "name"), taken),
        engine: engine.to_owned(),
        image: s(d, "dockerImage").to_owned(),
        target: target.map(str::to_owned),
        action: action.to_owned(),
        warnings,
        existing_id: None,
        url,
        hosts: vec![s(d, "appName").to_owned()],
    }
}

/// Percent-encodes a URL's user, password or database name.
fn url_escape(s: &str) -> String {
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

/// The plan from what `gather` read.
pub(super) fn plan(r: &Responses, host: &str) -> ImportPlan {
    let mut out = ImportPlan {
        platform: "dokploy".into(),
        host: host.into(),
        ..Default::default()
    };
    let mut seen_domains = HashSet::new();
    let mut db_names = HashSet::new();
    let empty = Value::Null;
    for p in arr(r.get("/project.all").unwrap_or(&empty)) {
        let envs = environments(p);
        let several = envs.len() > 1;
        let project_env = s(p, "env");
        for e in envs {
            let eid = e["environmentId"].as_str().unwrap_or(s(p, "projectId"));
            let environment_env = r
                .get(&format!("/environment.one?environmentId={eid}"))
                .map(|x| s(x, "env"))
                .unwrap_or("");
            let shared = (project_env, environment_env);
            let name = if several
                && e["isDefault"].as_bool() != Some(true)
                && s(e, "name") != "production"
            {
                format!("{} ({})", s(p, "name"), s(e, "name"))
            } else {
                s(p, "name").to_owned()
            };
            let mut project = ImportProject {
                key: format!("{}/{eid}", s(p, "projectId")),
                name: name.clone(),
                ..Default::default()
            };
            for (list, kind, engine) in DATABASES {
                for d in arr(&e[list]) {
                    let did = s(d, &format!("{kind}Id"));
                    match r.get(&one(kind, did)) {
                        Some(full) => {
                            project
                                .databases
                                .push(database(full, engine, host, &mut db_names))
                        }
                        None => {
                            let (_, _, note) = database_action(engine, false, P);
                            project.databases.push(ImportDatabase {
                                key: format!("db:{did}"),
                                source_name: s(d, "name").to_owned(),
                                name: unique(s(d, "name"), &mut db_names),
                                engine: engine.to_owned(),
                                action: "manual".into(),
                                warnings: note.into_iter().collect(),
                                ..Default::default()
                            });
                        }
                    }
                }
            }
            let mut names = HashSet::new();
            for a in arr(&e["applications"]) {
                let Some(full) = r.get(&one("application", s(a, "applicationId"))) else {
                    continue;
                };
                let n = unique(s(full, "name"), &mut names);
                project.services.push(application(
                    full,
                    n,
                    shared,
                    &project.databases,
                    &mut seen_domains,
                ));
            }
            for c in arr(&e["compose"]) {
                if let Some(full) = r.get(&one("compose", s(c, "composeId"))) {
                    project
                        .stacks
                        .push(stack(full, &name, shared, &mut seen_domains));
                }
            }
            out.projects.push(project);
        }
    }
    out
}
