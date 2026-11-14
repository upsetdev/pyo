//! Railway, through its public GraphQL API at
//! https://backboard.railway.com/graphql/v2
//! (https://docs.railway.com/integrations/api). Pyo talks to no other host.
//!
//! Tokens (same page): an account or workspace token goes as
//! `Authorization: Bearer`, a project token as `Project-Access-Token` and
//! sees one environment of one project. Pyo tries the first, then the second.
//! Rate limits are low (100 requests an hour on the free plan), so the walk
//! asks a few large questions instead of many small ones:
//!
//! - `apiToken { workspaces { id name } }`: the workspaces the token reaches
//!   (`Query.apiToken`, "Introspect the current API token and its accessible
//!   workspaces"). For a project token: `projectToken { projectId
//!   environmentId }` (docs.railway.com/integrations/api).
//! - Per workspace, `projects(workspaceId:)`, or `project(id:)` for a project
//!   token (docs.railway.com/integrations/api/manage-projects), each project
//!   with its services and its environments other than pull request ones
//!   (`environments(isEphemeral: false)`), each environment with its
//!   `serviceInstances` (the service's settings there: `source { image repo }`,
//!   `builder`, `buildCommand`, `startCommand`, `rootDirectory`,
//!   `dockerfilePath`, `healthcheckPath`, `numReplicas`, `cronSchedule`,
//!   `preDeployCommand`, `sleepApplication`, `railwayConfigFile`,
//!   `resolvedFileConfig { configFile fileManifest }` and `domains {
//!   customDomains serviceDomains }` with `domain` and `targetPort`),
//!   `volumeInstances` (`mountPath`, `sizeMB`, `volume { name }`),
//!   `deploymentTriggers` (`branch`, `repository` per service) and
//!   `variables` (`name`, `serviceId`, `isSealed`, never a value).
//! - Per project, one query with each environment's shared variables and
//!   each service's variables and `tcpProxies` (`domain`, `proxyPort`,
//!   `applicationPort`), as aliases. Services' variables are read with
//!   `unrendered: true`, so references stay as written and are resolved
//!   here. A database's are read rendered, for its public URL
//!   (docs.railway.com/integrations/api/manage-variables).
//!
//! Field names are in the docs pages above and the API's schema, read by
//! introspection of the endpoint (types Project, Environment,
//! ServiceInstance, ServiceSource, VolumeInstance, TCPProxy, CustomDomain,
//! ServiceDomain, DeploymentTrigger, Variable, ApiTokenContext,
//! ProjectToken).
//!
//! Variables: `${{shared.KEY}}`, `${{Service.KEY}}` and `${{KEY}}`
//! (docs.railway.com/variables, /variables/reference). Railway's own
//! `RAILWAY_PRIVATE_DOMAIN` becomes the service's name in Pyo (its address
//! inside the project) and `RAILWAY_PUBLIC_DOMAIN` its first custom domain.
//! A reference to a database there stays as written, with a note to connect
//! the new database. Sealed variables are never sent by the API
//! (docs.railway.com/variables#sealed-variables): they come over secret and
//! empty, with a note.
//!
//! Databases are services from an image (docs.railway.com/databases/reference):
//! Railway's Postgres runs `ghcr.io/railwayapp-templates/postgres-ssl`,
//! MySQL, Redis and MongoDB their Docker Hub images. Public access makes a
//! TCP proxy and fills `DATABASE_PUBLIC_URL` (Postgres) or
//! `MYSQL_PUBLIC_URL` (MySQL) (docs.railway.com/databases/postgresql,
//! /databases/mysql), which Pyo copies the data from.

use std::collections::{BTreeMap, HashMap, HashSet};

use serde_json::{Value, json};

use super::{
    Fetch, ImportDatabase, ImportPlan, ImportProject, ImportService, Platform, Responses, Web,
    coolify::internal_port,
    database_action, database_name, health_check, link_databases,
    links::{DbAddress, Role},
    plan_service, s, unique, url_escape, volume,
};
use crate::{
    compose::{self, PlanEnv, PlanSource},
    error::ApiResult,
};

pub(super) const API: &str = "https://backboard.railway.com/graphql/v2";
/// What imports from Railway record as their host.
pub(super) const HOST: &str = "railway.com";
const P: Platform = Platform::Railway;

/// The port a service gets when Railway says none: Railway gives each
/// service a `PORT` to listen on (docs.railway.com/deployments/healthchecks),
/// and Pyo sets `PORT` to the service's first port. 8080 is the port the
/// dashboard gives a new service (dash/src/lib/pyo/draft.ts).
const DEFAULT_PORT: u16 = 8080;

/// A project with what Pyo reads of it.
const PROJECT: &str = "id name primaryEnvironmentId
  environments(isEphemeral: false) { edges { node { id name
    serviceInstances { edges { node {
      serviceId serviceName builder buildCommand startCommand rootDirectory dockerfilePath
      healthcheckPath numReplicas cronSchedule preDeployCommand sleepApplication railwayConfigFile
      source { image repo }
      domains { customDomains { domain targetPort } serviceDomains { domain targetPort } }
      resolvedFileConfig { configFile fileManifest }
    } } }
    volumeInstances { edges { node { serviceId mountPath sizeMB volume { name } } } }
    deploymentTriggers { edges { node { serviceId branch repository provider } } }
    variables { edges { node { name serviceId isSealed } } }
  } } }";

fn arr(v: &Value) -> &[Value] {
    v.as_array().map(Vec::as_slice).unwrap_or(&[])
}

/// The nodes of a GraphQL connection.
fn nodes(v: &Value) -> impl Iterator<Item = &Value> {
    arr(&v["edges"]).iter().map(|e| &e["node"])
}

/// A project's environments Pyo imports, with their service instances.
/// With a project token, only the token's environment.
fn walk<'a>(p: &'a Value, only_env: Option<&str>) -> Vec<(&'a Value, Vec<&'a Value>)> {
    nodes(&p["environments"])
        .filter(|e| only_env.is_none_or(|id| s(e, "id") == id))
        .map(|e| (e, nodes(&e["serviceInstances"]).collect()))
        .collect()
}

/// The database engine a service runs, from its image: Railway's
/// databases are services from an image (no repository).
fn engine_of(i: &Value) -> Option<&'static str> {
    let src = &i["source"];
    if src["repo"].as_str().is_some_and(|r| !r.is_empty()) {
        return None;
    }
    let image = src["image"].as_str()?.to_lowercase();
    let name = image.rsplit('/').next().unwrap_or("");
    let name = name.split([':', '@']).next().unwrap_or("");
    let has = |w: &[&str]| w.iter().any(|x| name.contains(x));
    if has(&["postgres", "postgis", "pgvector", "timescale"]) {
        Some("postgres")
    } else if has(&["mysql"]) {
        Some("mysql")
    } else if has(&["mariadb"]) {
        Some("mariadb")
    } else if has(&["redis", "valkey", "keydb", "dragonfly"]) {
        Some("redis")
    } else if has(&["mongo"]) {
        Some("mongodb")
    } else if has(&["clickhouse"]) {
        Some("clickhouse")
    } else {
        None
    }
}

/// The question for a project's variables and TCP proxies, aliased by
/// position (`shared_<env>`, `v_<env>_<service>`, `t_<env>_<service>`).
pub(super) fn details_query(p: &Value, only_env: Option<&str>) -> (String, Value) {
    let mut decl = vec!["$projectId: String!".to_owned()];
    let mut fields = vec![];
    let mut vars = serde_json::Map::new();
    vars.insert("projectId".into(), json!(s(p, "id")));
    for (ei, (e, insts)) in walk(p, only_env).into_iter().enumerate() {
        decl.push(format!("$e{ei}: String!"));
        vars.insert(format!("e{ei}"), json!(s(e, "id")));
        fields.push(format!(
            "shared_{ei}: variables(projectId: $projectId, environmentId: $e{ei}, unrendered: true)"
        ));
        for (si, i) in insts.into_iter().enumerate() {
            decl.push(format!("$s{ei}_{si}: String!"));
            vars.insert(format!("s{ei}_{si}"), json!(s(i, "serviceId")));
            let unrendered = engine_of(i).is_none();
            fields.push(format!(
                "v_{ei}_{si}: variables(projectId: $projectId, environmentId: $e{ei}, serviceId: $s{ei}_{si}, unrendered: {unrendered})"
            ));
            fields.push(format!(
                "t_{ei}_{si}: tcpProxies(environmentId: $e{ei}, serviceId: $s{ei}_{si}) {{ domain proxyPort applicationPort }}"
            ));
        }
    }
    (
        format!("query({}) {{ {} }}", decl.join(", "), fields.join(" ")),
        Value::Object(vars),
    )
}

/// The projects read, and the environment a project token is limited to.
fn projects(r: &Responses) -> (Vec<&Value>, Option<&str>) {
    if let Some(t) = r.get("apiToken") {
        let list = arr(&t["data"]["apiToken"]["workspaces"])
            .iter()
            .filter_map(|w| r.get(&format!("projects:{}", s(w, "id"))))
            .flat_map(|got| nodes(&got["data"]["projects"]))
            .collect();
        return (list, None);
    }
    let Some(t) = r.get("projectToken") else {
        return (vec![], None);
    };
    let t = &t["data"]["projectToken"];
    let list = r
        .get(&format!("project:{}", s(t, "projectId")))
        .map(|got| &got["data"]["project"])
        .filter(|p| p.is_object())
        .into_iter()
        .collect();
    (list, t["environmentId"].as_str())
}

/// Reads everything the token sees.
pub(super) async fn gather(f: &Fetch) -> ApiResult<Responses> {
    let mut r = Responses::new();
    match f
        .graphql(
            "apiToken",
            "query { apiToken { workspaces { id name } } }",
            json!({}),
        )
        .await
    {
        Ok(tok) => {
            let ids: Vec<String> = arr(&tok["data"]["apiToken"]["workspaces"])
                .iter()
                .map(|w| s(w, "id").to_owned())
                .collect();
            r.insert("apiToken".into(), tok);
            for id in ids {
                let q = format!(
                    "query($workspaceId: String!) {{ projects(workspaceId: $workspaceId) {{ edges {{ node {{ {PROJECT} }} }} }} }}"
                );
                let got = f
                    .graphql(&format!("projects:{id}"), &q, json!({ "workspaceId": id }))
                    .await?;
                r.insert(format!("projects:{id}"), got);
            }
        }
        Err(first) => {
            // Not an account or workspace token: maybe a project token.
            f.use_project_token();
            let Ok(pt) = f
                .graphql(
                    "projectToken",
                    "query { projectToken { projectId environmentId } }",
                    json!({}),
                )
                .await
            else {
                return Err(first);
            };
            let id = s(&pt["data"]["projectToken"], "projectId").to_owned();
            r.insert("projectToken".into(), pt);
            let q = format!("query($id: String!) {{ project(id: $id) {{ {PROJECT} }} }}");
            let got = f
                .graphql(&format!("project:{id}"), &q, json!({ "id": id }))
                .await?;
            r.insert(format!("project:{id}"), got);
        }
    }
    let (list, only_env) = projects(&r);
    let mut asks = vec![];
    for p in list {
        if walk(p, only_env).is_empty() {
            continue;
        }
        let (q, vars) = details_query(p, only_env);
        asks.push((format!("details:{}", s(p, "id")), q, vars));
    }
    for (key, q, vars) in asks {
        let got = f.graphql(&key, &q, vars).await?;
        r.insert(key, got);
    }
    Ok(r)
}

/// A variables answer (a JSON object of names to values) as a map.
fn var_map(v: &Value) -> BTreeMap<String, String> {
    v.as_object()
        .map(|o| {
            o.iter()
                .map(|(k, v)| {
                    let v = match v {
                        Value::String(s) => s.clone(),
                        Value::Null => String::new(),
                        other => other.to_string(),
                    };
                    (k.clone(), v)
                })
                .collect()
        })
        .unwrap_or_default()
}

/// What references in one environment resolve against.
#[derive(Clone)]
struct Scope<'a> {
    shared: BTreeMap<String, String>,
    /// Railway's service name → its position.
    by_name: HashMap<&'a str, usize>,
    vars: Vec<BTreeMap<String, String>>,
    is_db: Vec<bool>,
    /// Each one's name in Pyo, its address inside the project.
    pyo_names: Vec<String>,
    /// Each one's first custom domain.
    public: Vec<Option<String>>,
    /// Resolve references to a database too (to its values there), to
    /// tell which database a variable points at. Off: they're kept.
    render_dbs: bool,
}

impl Scope<'_> {
    /// `value` with its `${{…}}` references resolved, as service `me`
    /// sees them. What can't be resolved stays as written, in `missing`.
    fn expand(&self, me: usize, value: &str, depth: u8, missing: &mut Vec<String>) -> String {
        let mut v = value;
        let mut out = String::new();
        while let Some(i) = v.find("${{") {
            let Some(j) = v[i..].find("}}") else { break };
            out.push_str(&v[..i]);
            let r = v[i + 3..i + j].trim();
            let got = match r.split_once('.') {
                Some(("shared", k)) => self
                    .shared
                    .get(k.trim())
                    .map(|x| self.deeper(me, x, depth, missing)),
                Some((svc, k)) => match self.by_name.get(svc.trim()) {
                    // A database there: kept, for a note to connect the new one.
                    Some(&n) if self.is_db[n] && !self.render_dbs => {
                        Some(format!("${{{{{}.{}}}}}", svc.trim(), k.trim()))
                    }
                    Some(&n) => self.lookup(n, k.trim(), depth, missing),
                    None => None,
                },
                None => self.lookup(me, r, depth, missing),
            };
            match got {
                Some(x) => out.push_str(&x),
                None => {
                    missing.push(r.to_owned());
                    out.push_str(&v[i..i + j + 2]);
                }
            }
            v = &v[i + j + 2..];
        }
        out.push_str(v);
        out
    }

    fn deeper(&self, me: usize, value: &str, depth: u8, missing: &mut Vec<String>) -> String {
        if depth >= 5 {
            value.to_owned()
        } else {
            self.expand(me, value, depth + 1, missing)
        }
    }

    /// Service `j`'s variable `k`, its own or one Railway provides.
    fn lookup(&self, j: usize, k: &str, depth: u8, missing: &mut Vec<String>) -> Option<String> {
        match k {
            // A database's, as it was there: it isn't a service in Pyo.
            "RAILWAY_PRIVATE_DOMAIN" if self.render_dbs && self.is_db[j] => {
                self.vars[j].get(k).cloned().or_else(|| {
                    self.by_name
                        .iter()
                        .find(|(_, n)| **n == j)
                        .map(|(name, _)| format!("{}.railway.internal", name.to_lowercase()))
                })
            }
            "RAILWAY_PRIVATE_DOMAIN" => Some(self.pyo_names[j].clone()),
            "RAILWAY_PUBLIC_DOMAIN" => self.public[j].clone(),
            _ => self.vars[j]
                .get(k)
                .map(|x| self.deeper(j, x, depth, missing)),
        }
    }
}

/// The note for references that can't be resolved.
fn unresolved(missing: &mut Vec<String>) -> Option<String> {
    if missing.is_empty() {
        return None;
    }
    missing.sort();
    missing.dedup();
    Some(format!(
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
    ))
}

/// A setting from the service's config file (railway.json or railway.toml,
/// docs.railway.com/config-as-code/reference) when it has one, else from
/// the service's settings.
fn setting<'a>(i: &'a Value, section: &str, key: &str) -> Option<&'a str> {
    i["resolvedFileConfig"]["fileManifest"][section][key]
        .as_str()
        .or_else(|| i[key].as_str())
        .map(str::trim)
        .filter(|x| !x.is_empty())
}

/// A Railway database as a Pyo database.
fn database(
    i: &Value,
    engine: &str,
    vars: &BTreeMap<String, String>,
    tcp: &[Value],
    name: String,
    key: String,
) -> ImportDatabase {
    let v = |k: &str| vars.get(k).map(String::as_str).unwrap_or("");
    let proxy = tcp.first();
    let from_proxy = |scheme: &str, user: &str, pass: &str, db: &str| {
        proxy.map(|t| {
            format!(
                "{scheme}://{}:{}@{}:{}/{}",
                url_escape(v(user)),
                url_escape(v(pass)),
                s(t, "domain"),
                t["proxyPort"].as_u64().unwrap_or(0),
                url_escape(v(db))
            )
        })
    };
    let public = |k: &str| {
        Some(v(k))
            .filter(|u| !u.is_empty() && !u.contains(".railway.internal") && !u.contains("${{"))
            .map(str::to_owned)
    };
    let url = match engine {
        "postgres" => public("DATABASE_PUBLIC_URL")
            .or_else(|| from_proxy("postgres", "PGUSER", "PGPASSWORD", "PGDATABASE")),
        "mysql" => public("MYSQL_PUBLIC_URL")
            .or_else(|| from_proxy("mysql", "MYSQLUSER", "MYSQLPASSWORD", "MYSQLDATABASE")),
        _ => None,
    };
    let (target, action, note) = database_action(engine, url.is_some(), P);
    let source_name = s(i, "serviceName").to_owned();
    // Its login, then where it's reached: its private domain inside the
    // project, its URLs, its TCP proxy.
    let first = |keys: &[&str]| {
        keys.iter()
            .map(|k| v(k))
            .find(|x| !x.is_empty())
            .unwrap_or("")
            .to_owned()
    };
    let mut address = DbAddress {
        user: first(&[
            "PGUSER",
            "POSTGRES_USER",
            "MYSQLUSER",
            "REDISUSER",
            "MONGOUSER",
        ]),
        password: first(&[
            "PGPASSWORD",
            "POSTGRES_PASSWORD",
            "MYSQLPASSWORD",
            "REDISPASSWORD",
            "REDIS_PASSWORD",
            "MONGOPASSWORD",
        ]),
        database: first(&[
            "PGDATABASE",
            "POSTGRES_DB",
            "MYSQLDATABASE",
            "MYSQL_DATABASE",
        ]),
        ..Default::default()
    };
    let port = first(&["PGPORT", "MYSQLPORT", "REDISPORT", "MONGOPORT"])
        .parse()
        .unwrap_or(internal_port(engine));
    for h in [
        v("RAILWAY_PRIVATE_DOMAIN").to_owned(),
        v("PGHOST").to_owned(),
        v("MYSQLHOST").to_owned(),
        v("REDISHOST").to_owned(),
        v("MONGOHOST").to_owned(),
        format!("{}.railway.internal", source_name.to_lowercase()),
    ] {
        if !h.contains("${{") {
            address.add_host(&h, port);
        }
    }
    for k in [
        "DATABASE_URL",
        "DATABASE_PUBLIC_URL",
        "MYSQL_URL",
        "MYSQL_PUBLIC_URL",
        "REDIS_URL",
        "REDIS_PUBLIC_URL",
        "MONGO_URL",
        "MONGO_PUBLIC_URL",
    ] {
        address.add_url(v(k));
    }
    if let Some(t) = proxy
        && let Some(p) = t["proxyPort"].as_u64().and_then(|p| u16::try_from(p).ok())
    {
        address.add_host(s(t, "domain"), p);
    }
    ImportDatabase {
        key,
        source_name,
        name,
        engine: engine.to_owned(),
        image: s(&i["source"], "image").to_owned(),
        target: target.map(str::to_owned),
        action: action.to_owned(),
        warnings: note.into_iter().collect(),
        existing_id: None,
        url,
        address,
        rest: false,
    }
}

/// What a database's own variable holds, by its name there
/// (docs.railway.com/databases): a reference to it says.
fn role_of(key: &str) -> Option<Role> {
    Some(match key {
        "DATABASE_URL"
        | "DATABASE_PRIVATE_URL"
        | "DATABASE_PUBLIC_URL"
        | "MYSQL_URL"
        | "MYSQL_PRIVATE_URL"
        | "MYSQL_PUBLIC_URL"
        | "REDIS_URL"
        | "REDIS_PRIVATE_URL"
        | "REDIS_PUBLIC_URL"
        | "MONGO_URL"
        | "MONGO_PUBLIC_URL" => Role::Url,
        "PGHOST" | "MYSQLHOST" | "REDISHOST" | "MONGOHOST" | "RAILWAY_PRIVATE_DOMAIN" => Role::Host,
        "PGPORT" | "MYSQLPORT" | "REDISPORT" | "MONGOPORT" => Role::Port,
        "PGUSER" | "POSTGRES_USER" | "MYSQLUSER" | "REDISUSER" | "MONGOUSER" => Role::User,
        "PGPASSWORD"
        | "POSTGRES_PASSWORD"
        | "MYSQLPASSWORD"
        | "MYSQL_ROOT_PASSWORD"
        | "REDISPASSWORD"
        | "REDIS_PASSWORD"
        | "MONGOPASSWORD" => Role::Password,
        "PGDATABASE" | "POSTGRES_DB" | "MYSQLDATABASE" | "MYSQL_DATABASE" => Role::Database,
        _ => return None,
    })
}

/// What one environment holds: each instance's variables and TCP proxies.
struct Env<'a> {
    e: &'a Value,
    insts: Vec<&'a Value>,
    vars: Vec<BTreeMap<String, String>>,
    tcp: Vec<&'a [Value]>,
    shared: BTreeMap<String, String>,
}

#[allow(clippy::too_many_arguments)]
fn application(
    i: &Value,
    me: usize,
    env: &Env,
    scope: &Scope,
    rendered: &Scope,
    dbs: &[ImportDatabase],
    sealed: &[&str],
    seen: &mut HashSet<String>,
) -> ImportService {
    let id = s(i, "serviceId");
    let eid = s(env.e, "id");
    let source_name = s(i, "serviceName").to_owned();
    let mut svc = plan_service(&scope.pyo_names[me], &source_name);
    let mut warnings = vec![];
    let mut importable = true;
    let mut builder = String::new();
    let src = &i["source"];
    let repo = src["repo"].as_str().unwrap_or("").trim();
    let image = src["image"].as_str().unwrap_or("").trim();
    let start = setting(i, "deploy", "startCommand");
    let build = setting(i, "build", "buildCommand");
    if !repo.is_empty() {
        // `repo` is GitHub's owner/name; the branch is its deployment
        // trigger's (DeploymentTrigger.branch).
        let branch = nodes(&env.e["deploymentTriggers"])
            .find(|t| s(t, "serviceId") == id)
            .map(|t| s(t, "branch"))
            .filter(|b| !b.is_empty())
            .unwrap_or("main");
        // Railway builds with a Dockerfile when the config file says so or
        // a path is set (docs.railway.com/config-as-code/reference).
        let dockerfile_path = setting(i, "build", "dockerfilePath").unwrap_or("");
        let dockerfile = !dockerfile_path.is_empty()
            || i["resolvedFileConfig"]["fileManifest"]["build"]["builder"].as_str()
                == Some("DOCKERFILE");
        svc.source = PlanSource {
            kind: "git".into(),
            repo_url: format!("https://github.com/{}", repo.trim_matches('/')),
            branch: branch.to_owned(),
            build_context: setting(i, "build", "rootDirectory")
                .unwrap_or("")
                .trim_matches('/')
                .to_owned(),
            dockerfile_path: dockerfile_path
                .trim_start_matches("./")
                .trim_start_matches('/')
                .to_owned(),
            ..Default::default()
        };
        builder = if dockerfile { "dockerfile" } else { "railpack" }.into();
        if dockerfile {
            if start.is_some() {
                warnings
                    .push("Its start command isn't carried over. Put it in the Dockerfile.".into());
            }
        } else {
            // Railway builds with Railpack too: its commands go to Pyo's
            // Railpack build as RAILPACK_BUILD_CMD and RAILPACK_START_CMD
            // (https://railpack.com/config/environment-variables).
            for (k, v) in [("RAILPACK_BUILD_CMD", build), ("RAILPACK_START_CMD", start)] {
                if let Some(v) = v {
                    svc.source.build_args.push(PlanEnv {
                        key: k.into(),
                        value: v.into(),
                        secret: false,
                    });
                }
            }
            // Builder: HEROKU, NIXPACKS, PAKETO or RAILPACK.
            if matches!(s(i, "builder"), "HEROKU" | "PAKETO") {
                warnings.push(
                    "Railway built it with buildpacks. Pyo builds it with zero config (Railpack)."
                        .into(),
                );
            }
        }
    } else if !image.is_empty() {
        let (img, tag) = compose::split_image(image);
        svc.source = PlanSource {
            kind: "image".into(),
            image: img,
            reference: tag,
            ..Default::default()
        };
        if start.is_some() {
            warnings.push(
                "Its start command isn't carried over. Pyo runs the image's own command.".into(),
            );
        }
    } else {
        importable = false;
        warnings.push("It was deployed from files uploaded with Railway's CLI. Pyo builds from a repository or runs an image.".into());
    }
    if i["preDeployCommand"]
        .as_array()
        .is_some_and(|c| !c.is_empty())
        || i["preDeployCommand"]
            .as_str()
            .is_some_and(|c| !c.trim().is_empty())
    {
        warnings.push("Its pre-deploy command isn't carried over. Run it from the service's start command if it's needed.".into());
    }
    if let Some(cron) = setting(i, "deploy", "cronSchedule") {
        warnings.push(format!(
            "Railway ran it on a schedule ({cron}). Pyo imports it as a service that keeps running: make a cron job for it instead."
        ));
    }
    if i["sleepApplication"].as_bool() == Some(true) {
        warnings.push("Railway put it to sleep when idle. In Pyo it keeps running.".into());
    }

    // Variables, references resolved; sealed ones by name only.
    let mut missing = vec![];
    let mut env_list: Vec<PlanEnv> = env.vars[me]
        .iter()
        .map(|(k, v)| PlanEnv {
            key: k.clone(),
            value: scope.expand(me, v, 0, &mut missing),
            secret: compose::secret_key(k),
        })
        .collect();
    warnings.extend(unresolved(&mut missing));
    let mut locked = vec![];
    for name in sealed {
        locked.push(*name);
        match env_list.iter_mut().find(|e| e.key == *name) {
            Some(e) => {
                e.value.clear();
                e.secret = true;
            }
            None => env_list.push(PlanEnv {
                key: (*name).to_owned(),
                value: String::new(),
                secret: true,
            }),
        }
    }
    if !locked.is_empty() {
        warnings.push(format!(
            "{} {} sealed in Railway, so {} can't be read. Set {} in Pyo.",
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
    // What each variable holds with references to a database resolved
    // too, and what a reference that is the whole value says it is
    // (`${{Postgres.DATABASE_URL}}`).
    let mut pairs = vec![];
    let mut explicit = BTreeMap::new();
    for (k, v) in &env.vars[me] {
        if sealed.contains(&k.as_str()) {
            continue;
        }
        let whole = v.trim();
        if let Some(r) = whole.strip_prefix("${{").and_then(|x| x.strip_suffix("}}"))
            && !r.contains("${{")
            && let Some((svc, key)) = r.split_once('.')
            && let Some(&n) = scope.by_name.get(svc.trim())
            && scope.is_db[n]
            && let Some(role) = role_of(key.trim())
        {
            explicit.insert(
                k.clone(),
                (format!("db:{}@{eid}", s(env.insts[n], "serviceId")), role),
            );
        }
        pairs.push((k.clone(), rendered.expand(me, v, 0, &mut vec![])));
    }

    // Ports: the domains' target port, else the PORT it sets, else any
    // (Pyo sets PORT to it).
    let domains = &i["domains"];
    let all: Vec<&Value> = arr(&domains["customDomains"])
        .iter()
        .chain(arr(&domains["serviceDomains"]))
        .collect();
    let port_of = |d: &Value| d["targetPort"].as_u64().and_then(|p| u16::try_from(p).ok());
    let set_port = env_list
        .iter()
        .find(|e| e.key == "PORT")
        .and_then(|e| e.value.trim().parse::<u16>().ok());
    let mut web = Web::default();
    // Without a domain or a PORT, Pyo's default HTTP port: Pyo sets PORT to
    // it, as Railway sets PORT to the one it picked. It may be a worker
    // that doesn't listen, so it gets no TCP check.
    let guessed = all.is_empty() && set_port.is_none() && importable;
    if !all.is_empty() || set_port.is_some() || guessed {
        let first = set_port
            .or_else(|| all.iter().find_map(|d| port_of(d)))
            .unwrap_or(DEFAULT_PORT);
        web.port(first, false);
    }
    for d in &all {
        web.domain(P, s(d, "domain"), port_of(d), "", seen);
    }
    for t in env.tcp[me] {
        let Some(app) = t["applicationPort"]
            .as_u64()
            .and_then(|p| u16::try_from(p).ok())
        else {
            continue;
        };
        let before = web.ports.len();
        web.published(app, app, "tcp");
        if web.ports.len() > before {
            warnings.push(format!(
                "Railway reached port {app} at {}:{}. In Pyo it's public on port {app}.",
                s(t, "domain"),
                t["proxyPort"].as_u64().unwrap_or(0)
            ));
        }
    }
    if guessed {
        warnings.push(format!(
            "It has no domain in Railway, so it gets HTTP port {DEFAULT_PORT} and Pyo sets PORT to it."
        ));
    }
    let check_path = setting(i, "deploy", "healthcheckPath");
    svc.health_check = health_check(
        web.ports
            .first()
            .map(|p| p.container_port)
            .filter(|_| !guessed || check_path.is_some()),
        check_path,
    );
    warnings.append(&mut web.warnings);
    svc.ports = web.ports;
    svc.env = env_list;
    let replicas = i["resolvedFileConfig"]["fileManifest"]["deploy"]["numReplicas"]
        .as_i64()
        .or_else(|| i["numReplicas"].as_i64());
    svc.replicas = replicas.map_or(1, |r| r.clamp(1, 20) as i32);

    // Volumes start empty, as big as there (at least 1 GiB).
    for v in nodes(&env.e["volumeInstances"]).filter(|v| s(v, "serviceId") == id) {
        let at = s(v, "mountPath");
        let mut vol = volume(s(&v["volume"], "name"), at, false);
        if let Some(mb) = v["sizeMB"].as_i64().filter(|m| *m > 0) {
            vol.size_gi = ((mb + 1023) / 1024).max(1);
        }
        svc.volumes.push(vol);
        warnings.push(format!("Volume {at} starts empty. Its data isn't copied."));
    }

    let mut out = ImportService {
        key: format!("app:{id}@{eid}"),
        source_name,
        builder,
        service: svc,
        domains: web.domains,
        warnings,
        importable,
        ..Default::default()
    };
    link_databases(&mut out, &pairs, &explicit, dbs, &[]);
    out
}

/// The plan from what `gather` read.
pub(super) fn plan(r: &Responses) -> ImportPlan {
    let mut out = ImportPlan {
        platform: "railway".into(),
        host: HOST.into(),
        ..Default::default()
    };
    let mut seen_domains = HashSet::new();
    let (list, only_env) = projects(r);
    let null = Value::Null;
    for p in list {
        let pid = s(p, "id");
        let details = r
            .get(&format!("details:{pid}"))
            .map(|d| &d["data"])
            .unwrap_or(&null);
        let envs = walk(p, only_env);
        let several = envs.len() > 1;
        for (ei, (e, insts)) in envs.into_iter().enumerate() {
            let eid = s(e, "id");
            let primary = !several
                || p["primaryEnvironmentId"].as_str() == Some(eid)
                || s(e, "name") == "production";
            let name = if primary {
                s(p, "name").to_owned()
            } else {
                format!("{} ({})", s(p, "name"), s(e, "name"))
            };
            let env = Env {
                e,
                vars: (0..insts.len())
                    .map(|si| var_map(&details[format!("v_{ei}_{si}")]))
                    .collect(),
                tcp: (0..insts.len())
                    .map(|si| arr(&details[format!("t_{ei}_{si}")]))
                    .collect(),
                shared: var_map(&details[format!("shared_{ei}")]),
                insts,
            };
            let mut names = HashSet::new();
            let scope = Scope {
                shared: env.shared.clone(),
                by_name: env
                    .insts
                    .iter()
                    .enumerate()
                    .map(|(n, i)| (s(i, "serviceName"), n))
                    .collect(),
                vars: env.vars.clone(),
                is_db: env.insts.iter().map(|i| engine_of(i).is_some()).collect(),
                pyo_names: env
                    .insts
                    .iter()
                    .map(|i| unique(s(i, "serviceName"), &mut names))
                    .collect(),
                public: env
                    .insts
                    .iter()
                    .map(|i| {
                        arr(&i["domains"]["customDomains"])
                            .first()
                            .map(|d| s(d, "domain").to_owned())
                    })
                    .collect(),
                render_dbs: false,
            };
            let rendered = Scope {
                render_dbs: true,
                ..scope.clone()
            };
            let mut project = ImportProject {
                key: format!("{pid}/{eid}"),
                name,
                ..Default::default()
            };
            for (n, i) in env.insts.iter().enumerate() {
                if let Some(engine) = engine_of(i) {
                    let base = if primary {
                        s(i, "serviceName").to_owned()
                    } else {
                        format!("{} {}", s(i, "serviceName"), s(e, "name"))
                    };
                    project.databases.push(database(
                        i,
                        engine,
                        &env.vars[n],
                        env.tcp[n],
                        database_name(&base),
                        format!("db:{}@{eid}", s(i, "serviceId")),
                    ));
                }
            }
            for (n, i) in env.insts.iter().enumerate() {
                if scope.is_db[n] {
                    continue;
                }
                let id = s(i, "serviceId");
                let sealed: Vec<&str> = nodes(&e["variables"])
                    .filter(|v| s(v, "serviceId") == id && v["isSealed"].as_bool() == Some(true))
                    .map(|v| s(v, "name"))
                    .collect();
                project.services.push(application(
                    i,
                    n,
                    &env,
                    &scope,
                    &rendered,
                    &project.databases,
                    &sealed,
                    &mut seen_domains,
                ));
            }
            out.projects.push(project);
        }
    }
    out
}
