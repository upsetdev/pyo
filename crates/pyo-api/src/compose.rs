//! Docker Compose import: a Compose file becomes a plan of Pyo objects (one
//! project, a service per Compose service) that the user reviews, then
//! `importCompose` creates. Nothing is created from the file directly.
//!
//! The file is read as YAML (serde_norway), not with a typed Compose model:
//! the typed crates refuse a whole file over one field they don't know
//! (`build.secrets`, `deploy.endpoint_mode`…), where Pyo wants to import the
//! rest and say what it left out. What Pyo can't do becomes a warning on the
//! service, never a failure.
//!
//! - `${VAR}`, `${VAR:-default}` and the rest of Compose's interpolation,
//!   from the `.env` given alongside (empty, with a warning, when unset).
//! - Service names stay (made DNS-safe: `plausible_db` → `plausible-db`, with
//!   the old name changed in variables too), so `db:5432` keeps working.
//! - Published ports become public: HTTP ones with a suggested domain, others
//!   TCP/UDP. Ports only other services use stay internal. Images Pyo knows
//!   (postgres, redis…) get their port even when the file doesn't say it.
//! - Named volumes become volumes; a directory bind mount becomes an empty
//!   volume (its files aren't copied); host paths and single files are left
//!   out, with a warning.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// Default size of a volume made from the file, in GiB.
const VOLUME_GI: i64 = 10;

/// Where the file came from, when it's in a repository: builds then use it.
#[derive(Debug, Clone, Default, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct RepoRef {
    /// "https://github.com/owner/repo".
    pub repo_url: String,
    pub branch: String,
    /// The Compose file's path in the repository, e.g. "deploy/compose.yml".
    pub path: String,
    /// Pyo's GitHub App installation that reaches it, for private repositories.
    #[serde(default)]
    pub installation_id: Option<i64>,
    #[serde(default)]
    pub private: bool,
}

/// What the conversion reads.
#[derive(Debug, Default)]
pub struct Input<'a> {
    pub file: &'a str,
    /// Other files next to it, by path relative to the Compose file's
    /// directory: `.env` and the services' `env_file`s.
    pub files: BTreeMap<String, String>,
    pub repo: Option<RepoRef>,
    /// The project's name when the file has none.
    pub name_hint: &'a str,
    /// Suggested domains go under this ("1-2-3-4.nip.io"); none without it.
    pub domain_base: Option<String>,
}

/// A whole import: what will be created, and what was left out.
#[derive(Debug, Clone, Default, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ComposePlan {
    pub project: PlanProject,
    /// In deploy order: a service comes after those it depends on.
    pub services: Vec<PlanService>,
    pub warnings: Vec<PlanWarning>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct PlanProject {
    pub name: String,
    /// Its slug, used in service addresses: lowercase letters, digits and dashes.
    pub namespace: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct PlanService {
    /// Its name in Pyo, and its address inside the project.
    pub name: String,
    /// Its name in the Compose file.
    pub compose_name: String,
    pub source: PlanSource,
    pub ports: Vec<PlanPort>,
    pub env: Vec<PlanEnv>,
    pub volumes: Vec<PlanVolume>,
    #[schema(value_type = Object)]
    pub health_check: Value,
    pub replicas: i32,
    pub resources: PlanResources,
    #[schema(value_type = Object)]
    pub container: pyo_proto::ContainerOverrides,
    /// Services it starts after.
    pub depends_on: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct PlanSource {
    /// "image", or "git" (built from the repository).
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub image: String,
    /// Tag or digest.
    #[serde(default)]
    pub reference: String,
    #[serde(default)]
    pub repo_url: String,
    #[serde(default)]
    pub branch: String,
    /// The build's directory in the repository.
    #[serde(default)]
    pub build_context: String,
    /// Relative to the build context.
    #[serde(default)]
    pub dockerfile_path: String,
    #[serde(default)]
    pub build_args: Vec<PlanEnv>,
    #[serde(default)]
    pub installation_id: Option<i64>,
    #[serde(default)]
    pub private: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct PlanPort {
    pub name: String,
    pub container_port: u16,
    /// "HTTP", "TCP" or "UDP".
    pub protocol: String,
    /// Reachable from outside the project.
    pub public: bool,
    /// For a public TCP or UDP port: the port it's reached on.
    #[serde(default)]
    pub public_port: Option<u16>,
    /// For a public HTTP port: the domain it's served on.
    #[serde(default)]
    pub domain: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, utoipa::ToSchema, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PlanEnv {
    pub key: String,
    pub value: String,
    /// Sealed like any secret variable, never shown again.
    #[serde(default)]
    pub secret: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct PlanVolume {
    pub name: String,
    pub mount_path: String,
    pub size_gi: i64,
    /// Made from a bind mount: it starts empty.
    #[serde(default)]
    pub from_bind: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, utoipa::ToSchema, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PlanResources {
    /// Millicores.
    pub cpu_request: i64,
    pub cpu_limit: i64,
    /// MiB.
    pub memory_request: i64,
    pub memory_limit: i64,
    #[serde(default)]
    pub gpu: i64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, utoipa::ToSchema, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PlanWarning {
    /// The Compose service it's about. None for the whole file.
    #[serde(default)]
    pub service: Option<String>,
    /// The Compose field, e.g. "volumes" or "privileged".
    pub field: String,
    pub message: String,
}

// ── Reading the file ─────────────────────────────────────────────────────────

/// `KEY=value` lines of a .env file.
pub fn parse_dotenv(text: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(line);
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        let v = v.trim();
        let v = if (v.starts_with('"') && v.ends_with('"') && v.len() >= 2)
            || (v.starts_with('\'') && v.ends_with('\'') && v.len() >= 2)
        {
            v[1..v.len() - 1].to_owned()
        } else {
            // An unquoted value ends at " #".
            v.split(" #").next().unwrap_or("").trim().to_owned()
        };
        out.insert(k.trim().to_owned(), v);
    }
    out
}

/// Compose's interpolation of one string: `$$`, `$VAR`, `${VAR}`,
/// `${VAR:-default}`, `${VAR-default}`, `${VAR:?err}`, `${VAR?err}`,
/// `${VAR:+alt}`, `${VAR+alt}`. Variables used but not set go in `missing`.
pub fn interpolate(
    s: &str,
    vars: &BTreeMap<String, String>,
    missing: &mut BTreeSet<String>,
) -> String {
    let b = s.as_bytes();
    let mut out = String::new();
    let mut i = 0;
    while i < b.len() {
        if b[i] != b'$' {
            let ch = s[i..].chars().next().unwrap_or(' ');
            out.push(ch);
            i += ch.len_utf8();
            continue;
        }
        match b.get(i + 1) {
            Some(b'$') => {
                out.push('$');
                i += 2;
            }
            Some(b'{') => {
                // Find the matching brace (defaults may hold ${...} too).
                let mut depth = 0;
                let mut j = i + 1;
                while j < b.len() {
                    match b[j] {
                        b'{' => depth += 1,
                        b'}' => {
                            depth -= 1;
                            if depth == 0 {
                                break;
                            }
                        }
                        _ => {}
                    }
                    j += 1;
                }
                if j >= b.len() {
                    out.push_str(&s[i..]);
                    break;
                }
                let inner = &s[i + 2..j];
                let name_end = inner
                    .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                    .unwrap_or(inner.len());
                let (name, rest) = inner.split_at(name_end);
                let set = vars.get(name);
                let value = if let Some(d) = rest.strip_prefix(":-") {
                    set.filter(|v| !v.is_empty())
                        .cloned()
                        .unwrap_or_else(|| interpolate(d, vars, missing))
                } else if let Some(d) = rest.strip_prefix('-') {
                    set.cloned()
                        .unwrap_or_else(|| interpolate(d, vars, missing))
                } else if let Some(a) = rest.strip_prefix(":+") {
                    if set.is_some_and(|v| !v.is_empty()) {
                        interpolate(a, vars, missing)
                    } else {
                        String::new()
                    }
                } else if let Some(a) = rest.strip_prefix('+') {
                    if set.is_some() {
                        interpolate(a, vars, missing)
                    } else {
                        String::new()
                    }
                } else if rest.starts_with(":?") || rest.starts_with('?') {
                    if set.is_none_or(|v| v.is_empty() && rest.starts_with(":?")) {
                        missing.insert(name.to_owned());
                    }
                    set.cloned().unwrap_or_default()
                } else {
                    if set.is_none() {
                        missing.insert(name.to_owned());
                    }
                    set.cloned().unwrap_or_default()
                };
                out.push_str(&value);
                i = j + 1;
            }
            Some(c) if c.is_ascii_alphabetic() || *c == b'_' => {
                let mut j = i + 1;
                while j < b.len() && (b[j].is_ascii_alphanumeric() || b[j] == b'_') {
                    j += 1;
                }
                let name = &s[i + 1..j];
                match vars.get(name) {
                    Some(v) => out.push_str(v),
                    None => {
                        missing.insert(name.to_owned());
                    }
                }
                i = j;
            }
            _ => {
                out.push('$');
                i += 1;
            }
        }
    }
    out
}

fn interpolate_all(v: &mut Value, vars: &BTreeMap<String, String>, missing: &mut BTreeSet<String>) {
    match v {
        Value::String(s) => *s = interpolate(s, vars, missing),
        Value::Array(a) => a.iter_mut().for_each(|x| interpolate_all(x, vars, missing)),
        Value::Object(m) => m
            .values_mut()
            .for_each(|x| interpolate_all(x, vars, missing)),
        _ => {}
    }
}

/// The YAML as JSON, with anchors and `<<:` merges applied.
fn read_yaml(text: &str) -> Result<Value, String> {
    let mut y: serde_norway::Value = serde_norway::from_str(text)
        .map_err(|e| format!("This isn't a valid Compose file: {e}"))?;
    y.apply_merge()
        .map_err(|e| format!("This Compose file's merge keys (<<:) don't work: {e}"))?;
    serde_json::to_value(&y)
        .map_err(|e| format!("This Compose file has a key that isn't text: {e}"))
}

// ── Small conversions ────────────────────────────────────────────────────────

/// A DNS-safe name: lowercase letters, digits and dashes.
pub fn dns_name(s: &str, max: usize) -> String {
    let mut out = String::new();
    for c in s.to_lowercase().chars() {
        let c = if c.is_ascii_alphanumeric() { c } else { '-' };
        if !(c == '-' && (out.is_empty() || out.ends_with('-'))) {
            out.push(c);
        }
    }
    let out: String = out.trim_matches('-').chars().take(max).collect();
    let out = out.trim_end_matches('-').to_owned();
    if out.is_empty() { "app".into() } else { out }
}

/// Shell-like words, as Compose splits a string `command`/`entrypoint`.
pub fn shell_words(s: &str) -> Vec<String> {
    let mut out = vec![];
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    let mut any = false;
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some('"'), '\\') => {
                if let Some(n) = chars.next() {
                    cur.push(n);
                }
            }
            (Some(_), c) => cur.push(c),
            (None, '\'' | '"') => {
                quote = Some(c);
                any = true;
            }
            (None, '\\') => {
                if let Some(n) = chars.next() {
                    cur.push(n);
                }
                any = true;
            }
            (None, c) if c.is_whitespace() => {
                if any || !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
                any = false;
            }
            (None, c) => {
                cur.push(c);
                any = true;
            }
        }
    }
    if any || !cur.is_empty() {
        out.push(cur);
    }
    out
}

fn shell_quote(s: &str) -> String {
    if !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_./:=@%+,".contains(c))
    {
        s.to_owned()
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}

/// A list or a string (split like a shell) as words.
fn words(v: &Value) -> Vec<String> {
    match v {
        Value::String(s) => shell_words(s),
        Value::Array(a) => a.iter().map(scalar).collect(),
        _ => vec![],
    }
}

/// Any YAML scalar as text.
fn scalar(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// "1m30s", "30s", "1h", "500ms" → seconds.
pub fn duration_secs(s: &str) -> Option<i64> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let mut total = 0f64;
    let mut num = String::new();
    let mut chars = s.chars().peekable();
    let mut any = false;
    while let Some(c) = chars.next() {
        if c.is_ascii_digit() || c == '.' {
            num.push(c);
            continue;
        }
        let mut unit = c.to_string();
        while let Some(&n) = chars.peek() {
            if n.is_ascii_alphabetic() {
                unit.push(n);
                chars.next();
            } else {
                break;
            }
        }
        let n: f64 = num.parse().ok()?;
        num.clear();
        total += n * match unit.as_str() {
            "h" => 3600.0,
            "m" => 60.0,
            "s" => 1.0,
            "ms" => 0.001,
            "us" | "µs" => 0.000_001,
            _ => return None,
        };
        any = true;
    }
    if !num.is_empty() {
        total += num.parse::<f64>().ok()?;
        any = true;
    }
    any.then(|| total.ceil() as i64)
}

/// "512M", "1g", "256mb", "1073741824" → MiB.
pub fn memory_mi(s: &str) -> Option<i64> {
    let s = s.trim().to_lowercase();
    let split = s
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(s.len());
    let (n, unit) = s.split_at(split);
    let n: f64 = n.parse().ok()?;
    let bytes = n * match unit.trim().trim_end_matches('b').trim_end_matches('i') {
        "" => 1.0,
        "k" => 1024.0,
        "m" => 1024.0 * 1024.0,
        "g" => 1024.0 * 1024.0 * 1024.0,
        "t" => 1024f64.powi(4),
        _ => return None,
    };
    Some(((bytes / (1024.0 * 1024.0)).ceil() as i64).max(1))
}

/// "0.5" → 500 millicores.
fn cpu_millis(v: &Value) -> Option<i64> {
    let f: f64 = scalar(v).trim().parse().ok()?;
    Some(((f * 1000.0).round() as i64).max(1))
}

/// "nginx:1.27" → ("nginx", "1.27"); a digest stays a digest.
pub fn split_image(s: &str) -> (String, String) {
    let s = s.trim();
    if let Some((i, d)) = s.split_once('@') {
        return (i.to_owned(), d.to_owned());
    }
    match s.rsplit_once(':') {
        Some((i, t)) if !t.contains('/') => (i.to_owned(), t.to_owned()),
        _ => (s.to_owned(), "latest".to_owned()),
    }
}

/// Ports of images other services usually reach, when the file doesn't say.
fn known_ports(image: &str) -> &'static [(u16, &'static str)] {
    let base = image
        .rsplit('/')
        .next()
        .unwrap_or(image)
        .split([':', '@'])
        .next()
        .unwrap_or("");
    match base {
        b if b.starts_with("postgres")
            || b.starts_with("postgis")
            || b.starts_with("timescaledb")
            || b.contains("pgvecto") =>
        {
            &[(5432, "TCP")]
        }
        b if b.starts_with("mysql") || b.starts_with("mariadb") || b.starts_with("percona") => {
            &[(3306, "TCP")]
        }
        b if b.starts_with("redis")
            || b.starts_with("valkey")
            || b.starts_with("keydb")
            || b.starts_with("dragonfly") =>
        {
            &[(6379, "TCP")]
        }
        b if b.starts_with("mongo") => &[(27017, "TCP")],
        b if b.starts_with("clickhouse") => &[(8123, "HTTP"), (9000, "TCP")],
        b if b.starts_with("rabbitmq") => &[(5672, "TCP")],
        "memcached" => &[(11211, "TCP")],
        b if b.starts_with("elasticsearch") || b.starts_with("opensearch") => &[(9200, "HTTP")],
        b if b.starts_with("meilisearch") => &[(7700, "HTTP")],
        "nats" => &[(4222, "TCP")],
        _ => &[],
    }
}

/// Ports that carry something other than HTTP.
fn tcp_port(p: u16) -> bool {
    matches!(
        p,
        21 | 22
            | 25
            | 53
            | 110
            | 143
            | 465
            | 587
            | 993
            | 995
            | 1433
            | 1521
            | 1883
            | 2181
            | 3306
            | 4222
            | 5432
            | 5672
            | 6379
            | 7687
            | 8883
            | 9042
            | 9092
            | 11211
            | 27017
    )
}

pub fn secret_key(k: &str) -> bool {
    let k = k.to_uppercase();
    [
        "PASSWORD",
        "PASSWD",
        "SECRET",
        "TOKEN",
        "PRIVATE",
        "API_KEY",
        "APIKEY",
        "ACCESS_KEY",
        "_KEY",
        "CREDENTIAL",
        "SALT",
        "DSN",
    ]
    .iter()
    .any(|w| k.contains(w))
        || (k.contains("DATABASE_URL")
            || k.ends_with("_URL")
                && (k.contains("REDIS")
                    || k.contains("POSTGRES")
                    || k.contains("MONGO")
                    || k.contains("MYSQL")))
}

/// `a/b/../c` → `a/c`; "" for the repository's root.
fn normalize(path: &str) -> String {
    let mut parts: Vec<&str> = vec![];
    for p in path.split('/') {
        match p {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            p => parts.push(p),
        }
    }
    parts.join("/")
}

/// Replace `old` where it stands as a host name in `s`.
fn rename_host(s: &str, old: &str, new: &str) -> String {
    let word = |c: Option<char>| {
        c.is_some_and(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.')
    };
    let mut out = String::new();
    let mut rest = s;
    while let Some(i) = rest.find(old) {
        let before = rest[..i].chars().last();
        let after = rest[i + old.len()..].chars().next();
        out.push_str(&rest[..i]);
        if word(before) || word(after) {
            out.push_str(old);
        } else {
            out.push_str(new);
        }
        rest = &rest[i + old.len()..];
    }
    out.push_str(rest);
    out
}

// ── The conversion ───────────────────────────────────────────────────────────

struct Warnings(Vec<PlanWarning>);

impl Warnings {
    fn add(&mut self, service: Option<&str>, field: &str, message: impl Into<String>) {
        self.0.push(PlanWarning {
            service: service.map(str::to_owned),
            field: field.to_owned(),
            message: message.into(),
        });
    }
}

/// Service fields Pyo doesn't carry over, and what to say about each. Fields
/// not listed and not converted below are ignored without a word (labels,
/// restart, logging, container_name…).
const UNSUPPORTED: &[(&str, &str)] = &[
    (
        "privileged",
        "Privileged containers aren't allowed: it runs without the extra rights.",
    ),
    (
        "network_mode",
        "It runs on the project's own network, not the server's or another container's.",
    ),
    (
        "pid",
        "It can't share the server's or another container's processes.",
    ),
    (
        "ipc",
        "It can't share memory with the server or another container.",
    ),
    (
        "devices",
        "Devices on the server aren't passed in (GPUs are: ask for them under deploy.resources).",
    ),
    ("cap_add", "Added Linux capabilities aren't granted."),
    ("security_opt", "Security options aren't applied."),
    ("sysctls", "Kernel settings aren't applied."),
    (
        "ulimits",
        "Limits like open files use the server's defaults.",
    ),
    (
        "extra_hosts",
        "Extra host names aren't added: services reach each other by name.",
    ),
    ("dns", "It uses the project's DNS."),
    ("dns_search", "It uses the project's DNS."),
    ("tmpfs", "In-memory mounts aren't made."),
    ("shm_size", "Shared memory stays at the default size."),
    (
        "volumes_from",
        "Volumes can't be shared from another container.",
    ),
    (
        "extends",
        "Extending another service isn't imported yet: copy its settings into this one.",
    ),
    (
        "secrets",
        "Compose secrets aren't imported: put them in secret variables.",
    ),
    (
        "configs",
        "Compose configs aren't imported: build the file into the image or put it in a variable.",
    ),
    ("platform", "It runs on the servers' own platform."),
    ("runtime", "Other container runtimes aren't used."),
    ("userns_mode", "User namespaces aren't changed."),
    ("group_add", "Extra groups aren't added."),
    ("cgroup_parent", "Its control group isn't changed."),
    ("read_only", "Its filesystem stays writable."),
    ("cpuset", "It isn't pinned to particular CPUs."),
    ("oom_score_adj", "Out-of-memory priority isn't changed."),
];

/// The plan for a Compose file.
pub fn plan(input: &Input) -> Result<ComposePlan, String> {
    let mut w = Warnings(vec![]);
    let dir = input
        .repo
        .as_ref()
        .map(|r| {
            r.path
                .rsplit_once('/')
                .map(|(d, _)| d.to_owned())
                .unwrap_or_default()
        })
        .unwrap_or_default();
    let dotenv = input
        .files
        .get(".env")
        .map(|t| parse_dotenv(t))
        .unwrap_or_default();

    let mut doc = read_yaml(input.file)?;
    let mut missing = BTreeSet::new();
    interpolate_all(&mut doc, &dotenv, &mut missing);
    if !missing.is_empty() {
        let names: Vec<&str> = missing.iter().map(String::as_str).collect();
        w.add(None, "variables", format!("Not set, so left empty: {}. Fill them in on the services before deploying, or add a .env file.", names.join(", ")));
    }
    let Some(top) = doc.as_object() else {
        return Err("This isn't a Compose file: it has no services.".into());
    };
    let services = top
        .get("services")
        .and_then(Value::as_object)
        .ok_or("This Compose file has no services.")?;
    if top.contains_key("include") {
        w.add(
            None,
            "include",
            "Included Compose files aren't imported yet: only this file's services are.",
        );
    }
    for k in ["secrets", "configs"] {
        if top.contains_key(k) {
            w.add(
                None,
                k,
                format!("Top-level {k} aren't imported: use secret variables instead."),
            );
        }
    }
    let top_volumes = top
        .get("volumes")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();

    let project_name = top
        .get("name")
        .and_then(Value::as_str)
        .filter(|n| !n.is_empty())
        .unwrap_or(input.name_hint);
    let project = PlanProject {
        name: if project_name.is_empty() {
            "compose".into()
        } else {
            project_name.to_owned()
        },
        namespace: dns_name(
            if project_name.is_empty() {
                "compose"
            } else {
                project_name
            },
            40,
        ),
    };

    // Names in Pyo, unique, and what changed.
    let mut names: HashMap<String, String> = HashMap::new();
    let mut taken = BTreeSet::new();
    for (compose_name, svc) in services {
        if svc
            .get("profiles")
            .and_then(Value::as_array)
            .is_some_and(|p| !p.is_empty())
        {
            w.add(
                Some(compose_name),
                "profiles",
                "It's in a profile, so Compose doesn't start it by default. It isn't imported.",
            );
            continue;
        }
        let base = dns_name(compose_name, 40);
        let mut name = base.clone();
        let mut n = 2;
        while !taken.insert(name.clone()) {
            name = format!("{base}-{n}");
            n += 1;
        }
        if name != *compose_name {
            w.add(Some(compose_name), "name", format!("Named {name} in Pyo, since names are lowercase letters, digits and dashes. Its address in other services' variables was changed to match."));
        }
        names.insert(compose_name.clone(), name);
    }

    let mut out: Vec<PlanService> = vec![];
    let mut volume_owner: HashMap<String, String> = HashMap::new();
    let mut first_http = true;
    for (compose_name, svc) in services {
        let Some(name) = names.get(compose_name).cloned() else {
            continue;
        };
        let Some(svc) = svc.as_object() else { continue };
        let sn = Some(compose_name.as_str());
        for (field, why) in UNSUPPORTED {
            if svc.get(*field).is_some_and(|v| {
                !matches!(v, Value::Null | Value::Bool(false))
                    && v.as_array().is_none_or(|a| !a.is_empty())
            }) {
                w.add(sn, field, *why);
            }
        }

        // Source: an image, or a build from the repository.
        let image = svc
            .get("image")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_owned();
        let source = if let Some(build) = svc.get("build") {
            let (context, dockerfile, args) = match build {
                Value::String(c) => (c.clone(), String::new(), vec![]),
                Value::Object(b) => {
                    for k in [
                        "secrets",
                        "ssh",
                        "target",
                        "platforms",
                        "additional_contexts",
                        "dockerfile_inline",
                        "cache_to",
                        "network",
                        "extra_hosts",
                        "privileged",
                    ] {
                        if b.contains_key(k) {
                            w.add(sn, &format!("build.{k}"), format!("The build's {k} isn't used: it builds the Dockerfile's last stage, without it."));
                        }
                    }
                    let args = match b.get("args") {
                        Some(Value::Object(m)) => m
                            .iter()
                            .map(|(k, v)| PlanEnv {
                                key: k.clone(),
                                value: scalar(v),
                                secret: false,
                            })
                            .collect(),
                        Some(Value::Array(a)) => a
                            .iter()
                            .map(scalar)
                            .map(|kv| match kv.split_once('=') {
                                Some((k, v)) => PlanEnv {
                                    key: k.into(),
                                    value: v.into(),
                                    secret: false,
                                },
                                None => PlanEnv {
                                    key: kv.clone(),
                                    value: dotenv.get(&kv).cloned().unwrap_or_default(),
                                    secret: false,
                                },
                            })
                            .collect(),
                        _ => vec![],
                    };
                    (
                        b.get("context").map(scalar).unwrap_or_else(|| ".".into()),
                        b.get("dockerfile").map(scalar).unwrap_or_default(),
                        args,
                    )
                }
                _ => (".".into(), String::new(), vec![]),
            };
            if context.contains("://") || context.starts_with("git@") {
                w.add(sn, "build.context", "A build from another repository isn't imported: pick that repository for this service.");
            }
            match &input.repo {
                Some(r) => PlanSource {
                    kind: "git".into(),
                    repo_url: r.repo_url.clone(),
                    branch: r.branch.clone(),
                    build_context: normalize(&format!("{dir}/{context}")),
                    dockerfile_path: if dockerfile.is_empty() {
                        "Dockerfile".into()
                    } else {
                        dockerfile
                    },
                    build_args: args,
                    installation_id: r.installation_id,
                    private: r.private,
                    ..Default::default()
                },
                None if !image.is_empty() => {
                    w.add(sn, "build", format!("It's built from source, which needs its repository: it uses the image {image} instead. Import from the repository to build it."));
                    let (i, r) = split_image(&image);
                    PlanSource {
                        kind: "image".into(),
                        image: i,
                        reference: r,
                        ..Default::default()
                    }
                }
                None => {
                    w.add(sn, "build", "It's built from source, which needs its repository: import from the repository, or give it an image.");
                    PlanSource {
                        kind: "git".into(),
                        build_context: normalize(&context),
                        dockerfile_path: if dockerfile.is_empty() {
                            "Dockerfile".into()
                        } else {
                            dockerfile
                        },
                        build_args: args,
                        ..Default::default()
                    }
                }
            }
        } else if !image.is_empty() {
            let (i, r) = split_image(&image);
            PlanSource {
                kind: "image".into(),
                image: i,
                reference: r,
                ..Default::default()
            }
        } else {
            w.add(
                sn,
                "image",
                "It has no image and no build, so it isn't imported.",
            );
            continue;
        };

        // Ports: published ones are public.
        let mut ports: Vec<PlanPort> = vec![];
        let add_port = |ports: &mut Vec<PlanPort>,
                        container: u16,
                        protocol: &str,
                        published: Option<u16>,
                        w: &mut Warnings| {
            if ports.iter().any(|p| {
                p.container_port == container && (p.protocol == "UDP") == (protocol == "UDP")
            }) {
                return;
            }
            // Compose says tcp for everything that isn't udp: the port says the rest.
            let proto = match protocol {
                "UDP" => "UDP",
                // Known images say which; Compose's "tcp" is only "not udp".
                "HTTP" => "HTTP",
                "TCP" if published.is_none() => "TCP",
                _ if tcp_port(container) => "TCP",
                _ => "HTTP",
            };
            let pname = match proto {
                "HTTP" if !ports.iter().any(|p| p.name == "http") => "http".to_owned(),
                "HTTP" => format!("http-{container}"),
                "UDP" => format!("udp-{container}"),
                _ => format!("tcp-{container}"),
            };
            let mut p = PlanPort {
                name: pname,
                container_port: container,
                protocol: proto.into(),
                public: published.is_some(),
                ..Default::default()
            };
            if published.is_some() {
                if proto == "HTTP" {
                    if ports.iter().any(|x| x.public && x.protocol == "HTTP") {
                        p.public = false;
                        w.add(sn, "ports", format!("Port {container} is a second web port: it's kept internal. Make it public if it needs its own domain."));
                    } else {
                        p.domain = input
                            .domain_base
                            .as_ref()
                            .map(|b| format!("{name}-{}.{b}", project.namespace));
                    }
                } else if let Some(why) =
                    published.and_then(|pp| pyo_proto::reserved_public_port(proto, pp as i32))
                {
                    p.public = false;
                    w.add(
                        sn,
                        "ports",
                        format!(
                            "Port {}/{proto} is kept internal: {why}",
                            published.unwrap_or(container)
                        ),
                    );
                } else {
                    p.public_port = published;
                }
            }
            ports.push(p);
        };
        if let Some(list) = svc.get("ports").and_then(Value::as_array) {
            for item in list {
                let (target, published, protocol) = match item {
                    Value::Object(o) => (
                        o.get("target")
                            .map(scalar)
                            .and_then(|t| t.parse::<u16>().ok()),
                        o.get("published")
                            .map(scalar)
                            .and_then(|p| p.parse::<u16>().ok()),
                        o.get("protocol").map(scalar).unwrap_or_default(),
                    ),
                    v => {
                        let s = scalar(v);
                        let (spec, proto) = s
                            .split_once('/')
                            .map(|(a, b)| (a.to_owned(), b.to_owned()))
                            .unwrap_or((s.clone(), String::new()));
                        if spec.contains('-') {
                            w.add(
                                sn,
                                "ports",
                                format!(
                                    "Port ranges ({s}) aren't imported: add each port it needs."
                                ),
                            );
                            continue;
                        }
                        let parts: Vec<&str> = spec.rsplitn(3, ':').collect();
                        let target = parts.first().and_then(|p| p.parse::<u16>().ok());
                        let published = if parts.len() >= 2 {
                            parts[1].parse::<u16>().ok()
                        } else {
                            None
                        };
                        // "3000" alone publishes it on a random host port.
                        let published = if parts.len() == 1 { target } else { published };
                        (target, published, proto)
                    }
                };
                let Some(target) = target else {
                    w.add(
                        sn,
                        "ports",
                        format!("Couldn't read the port {}.", scalar(item)),
                    );
                    continue;
                };
                add_port(
                    &mut ports,
                    target,
                    &protocol.to_uppercase(),
                    published.or(Some(target)),
                    &mut w,
                );
            }
        }
        for e in svc
            .get("expose")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let s = scalar(e);
            let (p, proto) = s
                .split_once('/')
                .map(|(a, b)| (a.to_owned(), b.to_uppercase()))
                .unwrap_or((s.clone(), String::new()));
            if let Ok(p) = p.parse::<u16>() {
                add_port(&mut ports, p, &proto, None, &mut w);
            }
        }
        let image_name = if source.kind == "image" {
            source.image.clone()
        } else {
            String::new()
        };
        for (p, proto) in known_ports(&image_name) {
            add_port(&mut ports, *p, proto, None, &mut w);
        }
        if ports.is_empty() {
            w.add(
                sn,
                "ports",
                "No port is declared: if other services connect to it, add the port it listens on.",
            );
        }
        if ports.iter().any(|p| p.public && p.protocol == "HTTP")
            && input.domain_base.is_none()
            && first_http
        {
            first_http = false;
            w.add(
                None,
                "ports",
                "Public web ports need a domain: pick a cluster for a free one, or enter your own.",
            );
        }

        // Variables: env_file first, then environment.
        let mut env: Vec<PlanEnv> = vec![];
        let set = |env: &mut Vec<PlanEnv>, k: String, v: String| {
            let secret = secret_key(&k);
            match env.iter_mut().find(|e| e.key == k) {
                Some(e) => e.value = v,
                None => env.push(PlanEnv {
                    key: k,
                    value: v,
                    secret,
                }),
            }
        };
        let env_files: Vec<(String, bool)> = match svc.get("env_file") {
            Some(Value::String(s)) => vec![(s.clone(), true)],
            Some(Value::Array(a)) => a
                .iter()
                .map(|f| match f {
                    Value::Object(o) => (
                        o.get("path").map(scalar).unwrap_or_default(),
                        o.get("required").and_then(Value::as_bool).unwrap_or(true),
                    ),
                    v => (scalar(v), true),
                })
                .collect(),
            _ => vec![],
        };
        for (path, required) in env_files {
            let key = normalize(&path);
            match input.files.get(&key) {
                Some(text) => {
                    for (k, v) in parse_dotenv(text) {
                        set(&mut env, k, v);
                    }
                }
                None if required => w.add(sn, "env_file", format!("{path} wasn't found next to the Compose file, so its variables aren't set. Add them to the service.")),
                None => {}
            }
        }
        let mut unset = vec![];
        match svc.get("environment") {
            Some(Value::Object(m)) => {
                for (k, v) in m {
                    match v {
                        Value::Null => match dotenv.get(k) {
                            Some(v) => set(&mut env, k.clone(), v.clone()),
                            None => unset.push(k.clone()),
                        },
                        v => set(&mut env, k.clone(), scalar(v)),
                    }
                }
            }
            Some(Value::Array(a)) => {
                for item in a.iter().map(scalar) {
                    match item.split_once('=') {
                        Some((k, v)) => set(&mut env, k.to_owned(), v.to_owned()),
                        None => match dotenv.get(&item) {
                            Some(v) => set(&mut env, item.clone(), v.clone()),
                            None => unset.push(item.clone()),
                        },
                    }
                }
            }
            _ => {}
        }
        if !unset.is_empty() {
            w.add(sn, "environment", format!("Taken from the shell in Compose and not set here, so left out: {}. Add the ones it needs.", unset.join(", ")));
        }

        // Volumes.
        let mut volumes: Vec<PlanVolume> = vec![];
        for (i, m) in svc
            .get("volumes")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .enumerate()
        {
            let (kind, source, target) = match m {
                Value::Object(o) => (
                    o.get("type").map(scalar).unwrap_or_else(|| "volume".into()),
                    o.get("source").map(scalar).unwrap_or_default(),
                    o.get("target").map(scalar).unwrap_or_default(),
                ),
                v => {
                    let s = scalar(v);
                    let parts: Vec<&str> = s.split(':').collect();
                    match parts.as_slice() {
                        [t] => ("volume".to_owned(), String::new(), (*t).to_owned()),
                        [src, t, ..] => {
                            let bind = src.starts_with('.')
                                || src.starts_with('/')
                                || src.starts_with('~')
                                || src.is_empty();
                            (
                                if bind { "bind" } else { "volume" }.to_owned(),
                                (*src).to_owned(),
                                (*t).to_owned(),
                            )
                        }
                        [] => continue,
                    }
                }
            };
            if target.is_empty() {
                continue;
            }
            let file_like = target
                .rsplit('/')
                .next()
                .is_some_and(|last| last.contains('.') && !last.starts_with('.'));
            match kind.as_str() {
                "tmpfs" => w.add(sn, "volumes", format!("The in-memory mount at {target} isn't made.")),
                "bind" if source.ends_with("docker.sock") => w.add(sn, "volumes", "The Docker socket isn't available: Pyo doesn't run Docker on its servers."),
                "bind" if source.starts_with('/') || source.starts_with('~') || source.is_empty() => {
                    w.add(sn, "volumes", format!("{source} on the server isn't mounted at {target}: services don't use the server's files."))
                }
                "bind" if file_like => w.add(sn, "volumes", format!("The file {source} isn't copied to {target}: build it into the image, or put its content in a variable.")),
                "bind" => {
                    let vname = dns_name(&format!("{name}-{}", source.trim_start_matches("./").replace('/', "-")), 50);
                    volumes.push(PlanVolume { name: vname, mount_path: target.clone(), size_gi: VOLUME_GI, from_bind: true });
                    w.add(sn, "volumes", format!("{source} became a volume at {target}. It starts empty: the files in {source} aren't copied."));
                }
                _ if source.is_empty() => {
                    let vname = dns_name(&format!("{name}-{}", target.rsplit('/').next().unwrap_or("data")), 50);
                    volumes.push(PlanVolume { name: if volumes.iter().any(|v| v.name == vname) { format!("{vname}-{i}") } else { vname }, mount_path: target.clone(), size_gi: VOLUME_GI, from_bind: false });
                }
                _ => {
                    if let Some(owner) = volume_owner.get(&source) {
                        w.add(sn, "volumes", format!("The volume {source} is also {owner}'s. A Pyo volume belongs to one service, so it stays {owner}'s and isn't mounted here."));
                        continue;
                    }
                    volume_owner.insert(source.clone(), compose_name.clone());
                    let decl = top_volumes.get(&source);
                    if decl.and_then(|d| d.get("external")).is_some_and(|e| e.as_bool() == Some(true) || e.is_object()) {
                        w.add(sn, "volumes", format!("{source} is an existing volume elsewhere. A new, empty one is made."));
                    }
                    if decl.and_then(|d| d.get("driver")).is_some_and(|d| scalar(d) != "local") {
                        w.add(sn, "volumes", format!("{source}'s volume driver isn't used: it's stored on the server's disk."));
                    }
                    volumes.push(PlanVolume { name: dns_name(&source, 50), mount_path: target.clone(), size_gi: VOLUME_GI, from_bind: false });
                }
            }
        }

        // Health check.
        let first_port = ports.first().map(|p| p.container_port);
        let health = svc.get("healthcheck").and_then(Value::as_object);
        let disabled =
            health.is_some_and(|h| h.get("disable").and_then(Value::as_bool) == Some(true));
        let test = health.and_then(|h| h.get("test"));
        let command = match test {
            Some(Value::String(s)) => Some(s.clone()),
            Some(Value::Array(a)) => {
                let a: Vec<String> = a.iter().map(scalar).collect();
                match a.first().map(String::as_str) {
                    Some("NONE") => None,
                    Some("CMD-SHELL") => a.get(1).cloned(),
                    Some("CMD") => Some(
                        a[1..]
                            .iter()
                            .map(|x| shell_quote(x))
                            .collect::<Vec<_>>()
                            .join(" "),
                    ),
                    _ => Some(
                        a.iter()
                            .map(|x| shell_quote(x))
                            .collect::<Vec<_>>()
                            .join(" "),
                    ),
                }
            }
            _ => None,
        };
        let period = health
            .and_then(|h| h.get("interval"))
            .and_then(|v| duration_secs(&scalar(v)))
            .unwrap_or(10)
            .clamp(1, 300);
        let delay = health
            .and_then(|h| h.get("start_period"))
            .and_then(|v| duration_secs(&scalar(v)))
            .unwrap_or(5)
            .clamp(0, 600);
        let health_check = match (disabled, command, first_port) {
            (false, Some(c), p) => {
                json!({ "type": "exec", "command": c, "path": "", "port": p.unwrap_or(0), "initialDelaySeconds": delay, "periodSeconds": period })
            }
            (true, _, _) | (false, None, None) => {
                json!({ "type": "none", "command": "", "path": "", "port": 0, "initialDelaySeconds": delay, "periodSeconds": period })
            }
            (false, None, Some(p)) => {
                json!({ "type": "tcp", "command": "", "path": "", "port": p, "initialDelaySeconds": delay, "periodSeconds": period })
            }
        };

        // Replicas and resources.
        let deploy = svc.get("deploy").and_then(Value::as_object);
        if deploy
            .and_then(|d| d.get("mode"))
            .is_some_and(|m| scalar(m) == "global")
        {
            w.add(sn, "deploy.mode", "One copy per server isn't how Pyo runs services: it gets one replica. Raise it as needed.");
        }
        for k in [
            "placement",
            "update_config",
            "rollback_config",
            "endpoint_mode",
        ] {
            if deploy.is_some_and(|d| d.contains_key(k)) {
                w.add(
                    sn,
                    &format!("deploy.{k}"),
                    format!("deploy.{k} isn't used: Pyo places and updates services itself."),
                );
            }
        }
        let replicas = deploy
            .and_then(|d| d.get("replicas"))
            .and_then(Value::as_i64)
            .or_else(|| svc.get("scale").and_then(Value::as_i64))
            .unwrap_or(1)
            .clamp(0, i32::MAX as i64) as i32;
        let limits = deploy
            .and_then(|d| d.get("resources"))
            .and_then(|r| r.get("limits"));
        let reservations = deploy
            .and_then(|d| d.get("resources"))
            .and_then(|r| r.get("reservations"));
        let cpu_limit = limits
            .and_then(|l| l.get("cpus"))
            .and_then(cpu_millis)
            .or_else(|| svc.get("cpus").and_then(cpu_millis));
        let mem_limit = limits
            .and_then(|l| l.get("memory"))
            .map(scalar)
            .or_else(|| svc.get("mem_limit").map(scalar))
            .and_then(|m| memory_mi(&m));
        let cpu_request = reservations
            .and_then(|l| l.get("cpus"))
            .and_then(cpu_millis);
        let mem_request = reservations
            .and_then(|l| l.get("memory"))
            .map(scalar)
            .or_else(|| svc.get("mem_reservation").map(scalar))
            .and_then(|m| memory_mi(&m));
        let gpu_devices = reservations
            .and_then(|r| r.get("devices"))
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let mut gpu = 0;
        for d in &gpu_devices {
            let caps = d
                .get("capabilities")
                .and_then(Value::as_array)
                .map(|c| c.iter().map(scalar).collect::<Vec<_>>())
                .unwrap_or_default();
            if caps.iter().any(|c| c == "gpu") {
                match d.get("count") {
                    Some(Value::Number(n)) => gpu += n.as_i64().unwrap_or(1),
                    Some(Value::String(s)) if s == "all" => {
                        gpu += 1;
                        w.add(
                            sn,
                            "deploy.resources",
                            "\"All\" GPUs became one: raise it if it uses more.",
                        );
                    }
                    _ => {
                        gpu += d
                            .get("device_ids")
                            .and_then(Value::as_array)
                            .map(|a| a.len() as i64)
                            .unwrap_or(1)
                    }
                }
            }
        }
        let cpu_request =
            cpu_request.unwrap_or_else(|| cpu_limit.map(|l| l.min(100)).unwrap_or(100));
        let memory_request =
            mem_request.unwrap_or_else(|| mem_limit.map(|l| l.min(128)).unwrap_or(128));
        let resources = PlanResources {
            cpu_request,
            cpu_limit: cpu_limit.unwrap_or(1000).max(cpu_request),
            memory_request,
            memory_limit: mem_limit.unwrap_or(512).max(memory_request),
            gpu,
        };

        // How it starts.
        let mut container = pyo_proto::ContainerOverrides {
            entrypoint: svc.get("entrypoint").map(words).unwrap_or_default(),
            command: svc.get("command").map(words).unwrap_or_default(),
            working_dir: svc.get("working_dir").map(scalar).unwrap_or_default(),
            ..Default::default()
        };
        if let Some(user) = svc.get("user").map(scalar).filter(|u| !u.is_empty()) {
            let (u, g) = user
                .split_once(':')
                .map(|(a, b)| (a.to_owned(), Some(b.to_owned())))
                .unwrap_or((user.clone(), None));
            match (u.parse::<i64>(), g.as_deref().map(str::parse::<i64>)) {
                (Ok(u), None) => container.run_as_user = Some(u),
                (Ok(u), Some(Ok(g))) => {
                    container.run_as_user = Some(u);
                    container.run_as_group = Some(g);
                }
                _ => w.add(sn, "user", format!("It runs as the image's user: \"{user}\" isn't a numeric user id (like 1000 or 1000:1000).")),
            }
        }

        let depends_on: Vec<String> = match svc.get("depends_on") {
            Some(Value::Array(a)) => a.iter().map(scalar).collect(),
            Some(Value::Object(m)) => m.keys().cloned().collect(),
            _ => vec![],
        };
        let mut deps = vec![];
        for d in depends_on {
            match names.get(&d) {
                Some(n) => deps.push(n.clone()),
                None => w.add(
                    sn,
                    "depends_on",
                    format!("It depends on {d}, which isn't imported."),
                ),
            }
        }

        out.push(PlanService {
            name,
            compose_name: compose_name.clone(),
            source,
            ports,
            env,
            volumes,
            health_check,
            replicas,
            resources,
            container,
            depends_on: deps,
        });
    }

    // Renamed services: their old address in variables.
    let renamed: Vec<(String, String)> = names
        .iter()
        .filter(|(a, b)| a != b)
        .map(|(a, b)| (a.clone(), b.clone()))
        .collect();
    for s in &mut out {
        for e in &mut s.env {
            for (old, new) in &renamed {
                e.value = rename_host(&e.value, old, new);
            }
        }
    }

    // Deploy order: dependencies first, the file's order otherwise.
    let mut ordered: Vec<PlanService> = vec![];
    let mut left = out;
    while !left.is_empty() {
        let pos = left
            .iter()
            .position(|s| {
                s.depends_on.iter().all(|d| {
                    ordered.iter().any(|o| &o.name == d) || !left.iter().any(|l| &l.name == d)
                })
            })
            .unwrap_or(0);
        if left[pos].depends_on.iter().any(|d| {
            left.iter()
                .any(|l| &l.name == d && l.name != left[pos].name)
        }) {
            w.add(
                Some(&left[pos].compose_name),
                "depends_on",
                "Its dependencies go in a circle, so it's deployed in the file's order.",
            );
        }
        ordered.push(left.remove(pos));
    }
    if ordered.is_empty() {
        return Err("Nothing in this Compose file can be imported.".into());
    }
    Ok(ComposePlan {
        project,
        services: ordered,
        warnings: w.0,
    })
}

/// A plan service as the service object `createService` takes.
pub fn service_spec(
    s: &PlanService,
    id: &str,
    team_id: &str,
    project_id: &str,
    cluster_id: &str,
    now: i64,
    new_id: &dyn Fn(&str) -> String,
) -> Value {
    let env: Vec<Value> = s.env.iter().map(|e| json!({ "id": new_id("env"), "key": e.key, "value": e.value, "secret": e.secret, "updatedAt": now })).collect();
    let ports: Vec<Value> = s
        .ports
        .iter()
        .map(|p| {
            let exposure = match (p.public, p.protocol.as_str()) {
                (false, _) => "internal",
                (true, "HTTP") => "http",
                (true, _) => "loadbalancer",
            };
            let mut v = json!({ "id": new_id("port"), "name": p.name, "protocol": p.protocol, "containerPort": p.container_port, "exposure": exposure });
            if exposure == "loadbalancer" {
                // Published TCP and UDP ports go through the gateway.
                v["proxy"] = json!(true);
                if let Some(pp) = p.public_port {
                    v["publicPort"] = json!(pp);
                }
            }
            v
        })
        .collect();
    let domains: Vec<Value> = s
        .ports
        .iter()
        .filter(|p| p.public && p.protocol == "HTTP")
        .filter_map(|p| p.domain.as_ref().filter(|d| !d.trim().is_empty()).map(|d| (p, d.trim().to_lowercase())))
        .map(|(p, d)| {
            let private = crate::deploy::private_host(&d);
            json!({ "id": new_id("dom"), "hostname": d, "portName": p.name, "status": "pending_dns", "tls": "pending", "tlsMode": "auto", "redirectHttps": !private, "createdAt": now })
        })
        .collect();
    let source = if s.source.kind == "git" {
        json!({
            "type": "git", "repoUrl": s.source.repo_url, "branch": s.source.branch, "commit": "", "builder": "dockerfile",
            "dockerfilePath": s.source.dockerfile_path, "buildContext": s.source.build_context,
            "buildArgs": s.source.build_args.iter().map(|a| json!({ "key": a.key, "value": a.value })).collect::<Vec<_>>(),
            "private": s.source.private, "installationId": s.source.installation_id, "webhook": "inactive", "autoDeploy": s.source.installation_id.is_some(),
        })
    } else {
        json!({ "type": "image", "image": s.source.image, "reference": s.source.reference, "registryId": "reg_dockerhub", "private": false })
    };
    let mut spec = json!({
        "id": id, "teamId": team_id, "projectId": project_id, "clusterId": cluster_id,
        "name": s.name, "description": format!("Imported from Docker Compose ({})", s.compose_name), "kind": "application", "workloadKind": "Deployment",
        "source": source, "env": env, "ports": ports, "domains": domains,
        "resources": { "cpuRequest": s.resources.cpu_request, "cpuLimit": s.resources.cpu_limit, "memoryRequest": s.resources.memory_request, "memoryLimit": s.resources.memory_limit, "gpu": s.resources.gpu },
        "replicas": s.replicas, "healthCheck": s.health_check,
    });
    if !s.container.is_empty() {
        spec["container"] = serde_json::to_value(&s.container).unwrap_or(Value::Null);
    }
    spec
}

/// The volumes `createService` makes with a plan service.
pub fn volume_specs(s: &PlanService, new_id: &dyn Fn(&str) -> String) -> Vec<Value> {
    s.volumes.iter().map(|v| json!({ "id": new_id("vol"), "name": v.name, "mountPath": v.mount_path, "sizeGi": v.size_gi })).collect()
}

// ── The preview endpoint ─────────────────────────────────────────────────────

/// What to import: a file's text, or a file in one of the team's GitHub
/// repositories.
#[derive(Debug, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct PreviewBody {
    /// The team it's for.
    pub team_id: String,
    /// The Compose file's text, when it isn't read from a repository.
    #[serde(default)]
    pub file: Option<String>,
    /// A .env file's text, for `${VAR}`s and `env_file: .env`.
    #[serde(default)]
    pub env: Option<String>,
    /// Or: a repository the team's GitHub App installations can see.
    #[serde(default)]
    pub repo: Option<RepoInput>,
    /// The cluster it goes to: public web ports get a free domain on it.
    #[serde(default)]
    pub cluster_id: Option<String>,
    /// The project's name, when the file has none.
    #[serde(default)]
    pub name: Option<String>,
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct RepoInput {
    /// "owner/repo".
    pub full_name: String,
    pub branch: String,
    /// The Compose file's path, e.g. "compose.yml" or "deploy/compose.yml".
    pub path: String,
}

/// Preview a Docker Compose import
///
/// What importing a Compose file would create: a project, and a service per
/// Compose service with its source, ports, variables, volumes and health
/// check, in deploy order. It adds a warning for each thing that isn't
/// carried over. Nothing is created. Edit the plan if you like, then pass it
/// to the `importCompose` operation.
#[utoipa::path(post, path = "/compose/preview", operation_id = "previewCompose", tag = "Projects",
    request_body = PreviewBody,
    responses(
        (status = 200, description = "The plan.", body = ComposePlan),
        (status = 400, description = "Not a Compose file Pyo can read, nothing importable, or no file given.", body = crate::error::ErrorBody),
        (status = 404, description = "No such team, cluster or file.", body = crate::error::ErrorBody),
        (status = 502, description = "GitHub couldn't be read.", body = crate::error::ErrorBody),
    ),
    extensions(("x-pyo-access" = json!("read"))))]
pub async fn preview(
    axum::extract::State(s): axum::extract::State<crate::AppState>,
    crate::auth::SessionUser(user): crate::auth::SessionUser,
    axum::Json(b): axum::Json<PreviewBody>,
) -> crate::error::ApiResult<axum::Json<ComposePlan>> {
    use crate::error::ApiError;
    let mut tx = s.db.begin().await?;
    crate::ops::require_role(&mut tx, &user, &b.team_id, crate::ops::Role::Viewer).await?;
    // Free domains go on the cluster's public address.
    let cluster: Option<Value> = match &b.cluster_id {
        Some(id) => Some(
            sqlx::query_scalar(
                "SELECT capabilities FROM pyo.clusters WHERE id = $1 AND team_id = $2",
            )
            .bind(id)
            .bind(&b.team_id)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or_else(|| ApiError::not_found("Cluster"))?,
        ),
        None => None,
    };
    tx.commit().await?;
    let domain_base = cluster.and_then(|c| {
        c["loadBalancerIps"]
            .as_array()?
            .iter()
            .filter_map(Value::as_str)
            .find(|i| i.split('.').count() == 4 && i.split('.').all(|p| p.parse::<u8>().is_ok()))
            .map(|ip| format!("{}.nip.io", ip.replace('.', "-")))
    });

    let mut files = BTreeMap::new();
    let (text, repo, hint) = match (&b.file, &b.repo) {
        (Some(f), _) if !f.trim().is_empty() => {
            if let Some(e) = &b.env {
                files.insert(".env".to_owned(), e.clone());
            }
            (f.clone(), None, String::new())
        }
        (_, Some(r)) => {
            let repo = r
                .full_name
                .trim()
                .trim_start_matches("https://github.com/")
                .trim_end_matches(".git")
                .to_owned();
            let (text, installation) =
                crate::github::read_file(&s, &b.team_id, &repo, &r.branch, &r.path)
                    .await?
                    .ok_or_else(|| ApiError::not_found(&format!("{} on {}", r.path, r.branch)))?;
            let dir = r
                .path
                .trim_start_matches('/')
                .rsplit_once('/')
                .map(|(d, _)| d.to_owned())
                .unwrap_or_default();
            let near = |f: &str| {
                if dir.is_empty() {
                    normalize(f)
                } else {
                    normalize(&format!("{dir}/{f}"))
                }
            };
            // .env and the env_files the services name, next to the file.
            let mut wanted = vec![".env".to_owned()];
            if let Ok(doc) = read_yaml(&text) {
                for svc in doc["services"]
                    .as_object()
                    .into_iter()
                    .flat_map(|m| m.values())
                {
                    match &svc["env_file"] {
                        Value::String(f) => wanted.push(normalize(f)),
                        Value::Array(a) => wanted.extend(a.iter().map(|f| {
                            normalize(&f.get("path").map(scalar).unwrap_or_else(|| scalar(f)))
                        })),
                        _ => {}
                    }
                }
            }
            wanted.sort();
            wanted.dedup();
            for f in wanted.into_iter().take(20) {
                if let Some((t, _)) =
                    crate::github::read_file(&s, &b.team_id, &repo, &r.branch, &near(&f)).await?
                {
                    files.insert(f, t);
                }
            }
            let hint = repo.rsplit('/').next().unwrap_or("").to_owned();
            let repo = RepoRef {
                repo_url: format!("https://github.com/{repo}"),
                branch: r.branch.clone(),
                path: r.path.trim_start_matches('/').to_owned(),
                installation_id: Some(installation),
                private: true,
            };
            (text, Some(repo), hint)
        }
        _ => {
            return Err(ApiError::bad_request(
                "Paste a Compose file, or pick one in a repository.",
            ));
        }
    };
    if text.len() > 512 * 1024 {
        return Err(ApiError::bad_request(
            "This Compose file is too big to import.",
        ));
    }
    let name = b
        .name
        .clone()
        .filter(|n| !n.trim().is_empty())
        .unwrap_or(hint);
    let plan = plan(&Input {
        file: &text,
        files,
        repo,
        name_hint: &name,
        domain_base,
    })
    .map_err(ApiError::bad_request)?;
    Ok(axum::Json(plan))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> String {
        std::fs::read_to_string(format!(
            "{}/tests/fixtures/compose/{name}",
            env!("CARGO_MANIFEST_DIR")
        ))
        .unwrap()
    }

    fn plan_of(file: &str, env: Option<&str>) -> ComposePlan {
        let mut files = BTreeMap::new();
        if let Some(e) = env {
            files.insert(".env".to_owned(), fixture(e));
        }
        plan(&Input {
            file: &fixture(file),
            files,
            name_hint: "imported",
            domain_base: Some("1-2-3-4.nip.io".into()),
            ..Default::default()
        })
        .unwrap()
    }

    fn svc<'a>(p: &'a ComposePlan, name: &str) -> &'a PlanService {
        p.services
            .iter()
            .find(|s| s.name == name)
            .unwrap_or_else(|| {
                panic!(
                    "no {name} in {:?}",
                    p.services.iter().map(|s| &s.name).collect::<Vec<_>>()
                )
            })
    }

    fn warned(p: &ComposePlan, service: Option<&str>, field: &str) -> bool {
        p.warnings
            .iter()
            .any(|w| w.service.as_deref() == service && w.field == field)
    }

    #[test]
    fn interpolation() {
        let vars: BTreeMap<String, String> = [
            ("A".to_owned(), "1".to_owned()),
            ("E".to_owned(), String::new()),
        ]
        .into();
        let mut missing = BTreeSet::new();
        assert_eq!(interpolate("x${A}y$A$$A", &vars, &mut missing), "x1y1$A");
        assert_eq!(
            interpolate("${B:-dflt} ${E:-d2} ${E-d3} ${B-d4}", &vars, &mut missing),
            "dflt d2  d4"
        );
        assert_eq!(
            interpolate("${A:+on}${B:+on}${E+set}", &vars, &mut missing),
            "onset"
        );
        assert_eq!(interpolate("${B:-${A}}", &vars, &mut missing), "1");
        assert!(missing.is_empty());
        assert_eq!(
            interpolate("${MISSING}-${REQ:?must}", &vars, &mut missing),
            "-"
        );
        assert_eq!(
            missing.into_iter().collect::<Vec<_>>(),
            vec!["MISSING", "REQ"]
        );
    }

    #[test]
    fn small_parsers() {
        assert_eq!(
            shell_words(r#"sh -c "a && b" 'c d' e\ f"#),
            vec!["sh", "-c", "a && b", "c d", "e f"]
        );
        assert_eq!(duration_secs("1m30s"), Some(90));
        assert_eq!(duration_secs("500ms"), Some(1));
        assert_eq!(duration_secs("10"), Some(10));
        assert_eq!(memory_mi("512M"), Some(512));
        assert_eq!(memory_mi("1g"), Some(1024));
        assert_eq!(memory_mi("256mb"), Some(256));
        assert_eq!(memory_mi("1073741824"), Some(1024));
        assert_eq!(dns_name("Plausible_DB", 40), "plausible-db");
        assert_eq!(
            rename_host(
                "postgres://u:p@plausible_db:5432/x?plausible_db_x=1",
                "plausible_db",
                "plausible-db"
            ),
            "postgres://u:p@plausible-db:5432/x?plausible_db_x=1"
        );
        assert_eq!(
            split_image("ghcr.io/a/b:1.2"),
            ("ghcr.io/a/b".into(), "1.2".into())
        );
        assert_eq!(
            parse_dotenv("A=1\n# c\nexport B=\"x y\"\nC=z # note\n"),
            [
                ("A".into(), "1".into()),
                ("B".into(), "x y".into()),
                ("C".into(), "z".into())
            ]
            .into()
        );
    }

    #[test]
    fn plausible() {
        let p = plan_of("plausible.yml", None);
        assert_eq!(
            p.services
                .iter()
                .map(|s| s.name.as_str())
                .collect::<Vec<_>>(),
            vec!["plausible-db", "plausible-events-db", "plausible"],
            "dependencies first"
        );
        let db = svc(&p, "plausible-db");
        assert_eq!(db.ports[0].container_port, 5432, "postgres gets its port");
        assert!(!db.ports[0].public);
        assert_eq!(db.volumes[0].name, "db-data");
        assert_eq!(db.health_check["type"], "exec");
        assert_eq!(db.health_check["command"], "pg_isready -U postgres");
        assert_eq!(db.health_check["initialDelaySeconds"], 60);
        assert!(
            db.env
                .iter()
                .any(|e| e.key == "POSTGRES_PASSWORD" && e.secret)
        );
        let ch = svc(&p, "plausible-events-db");
        assert_eq!(
            ch.volumes.len(),
            2,
            "named volumes only: the three config files aren't copied"
        );
        assert_eq!(
            ch.ports
                .iter()
                .map(|p| (p.container_port, p.protocol.as_str()))
                .collect::<Vec<_>>(),
            vec![(8123, "HTTP"), (9000, "TCP")]
        );
        assert!(
            p.warnings
                .iter()
                .filter(
                    |w| w.service.as_deref() == Some("plausible_events_db") && w.field == "volumes"
                )
                .count()
                >= 3
        );
        assert!(warned(&p, Some("plausible_events_db"), "ulimits"));
        let app = svc(&p, "plausible");
        assert_eq!(app.depends_on, vec!["plausible-db", "plausible-events-db"]);
        assert_eq!(
            app.container.command,
            vec![
                "sh",
                "-c",
                "/entrypoint.sh db createdb && /entrypoint.sh db migrate && /entrypoint.sh run"
            ]
        );
        assert!(
            warned(&p, None, "variables"),
            "BASE_URL and SECRET_KEY_BASE aren't set"
        );
        assert!(
            warned(&p, Some("plausible"), "environment"),
            "pass-through variables"
        );
        assert!(warned(&p, Some("plausible_db"), "name"));
    }

    #[test]
    fn immich_with_its_env() {
        let p = plan_of("immich.yml", Some("immich.env"));
        assert_eq!(p.project.name, "immich");
        let server = svc(&p, "immich-server");
        assert_eq!(server.source.image, "ghcr.io/immich-app/immich-server");
        assert_eq!(
            server.source.reference,
            parse_dotenv(&fixture("immich.env"))["IMMICH_VERSION"],
            "IMMICH_VERSION from the .env"
        );
        let web = &server.ports[0];
        assert!(web.public && web.protocol == "HTTP" && web.container_port == 2283);
        assert_eq!(
            web.domain.as_deref(),
            Some("immich-server-immich.1-2-3-4.nip.io")
        );
        assert!(
            server
                .volumes
                .iter()
                .any(|v| v.mount_path == "/data" && v.from_bind),
            "./library became a volume"
        );
        assert!(
            warned(&p, Some("immich-server"), "volumes"),
            "/etc/localtime isn't mounted"
        );
        assert!(
            server
                .env
                .iter()
                .any(|e| e.key == "DB_PASSWORD" && e.secret),
            "env_file .env"
        );
        let db = svc(&p, "database");
        assert_eq!(
            db.env
                .iter()
                .find(|e| e.key == "POSTGRES_USER")
                .map(|e| e.value.as_str()),
            Some("postgres")
        );
        assert!(warned(&p, Some("database"), "shm_size"));
        assert_eq!(svc(&p, "redis").ports[0].container_port, 6379);
        assert_eq!(
            svc(&p, "redis").health_check["command"],
            "redis-cli ping | grep -q PONG || exit 1"
        );
        let at = |n: &str| p.services.iter().position(|s| s.name == n).unwrap();
        assert!(
            at("immich-server") > at("redis") && at("immich-server") > at("database"),
            "after what it depends on"
        );
    }

    #[test]
    fn n8n_uptime_kuma() {
        let p = plan_of("n8n.yml", Some("n8n.env"));
        let n8n = svc(&p, "n8n");
        assert!(
            n8n.ports
                .iter()
                .any(|x| x.container_port == 5678 && x.public && x.protocol == "HTTP")
        );
        assert!(n8n.depends_on.contains(&"postgres".to_owned()));
        assert!(
            warned(&p, Some("postgres"), "volumes"),
            "its init script is a file"
        );
        let p = plan_of("uptime-kuma.yml", None);
        let k = &p.services[0];
        assert!(k.ports[0].public && k.ports[0].container_port == 3001);
    }

    #[test]
    fn awesome_compose_samples() {
        let p = plan_of("ac-wordpress-mysql.yml", None);
        assert_eq!(svc(&p, "db").ports[0].container_port, 3306);
        assert!(
            svc(&p, "wordpress")
                .ports
                .iter()
                .any(|x| x.public && x.container_port == 80)
        );
        let p = plan_of("ac-nginx-golang-postgres.yml", None);
        assert!(
            p.services
                .iter()
                .any(|s| s.source.kind == "git" && s.source.repo_url.is_empty()),
            "builds without a repository"
        );
        assert!(p.warnings.iter().any(|w| w.field == "build"));
        let p = plan_of("ac-react-express-mongodb.yml", None);
        assert!(
            svc(&p, "mongo")
                .ports
                .iter()
                .any(|x| x.container_port == 27017)
        );
        for f in [
            "ac-django.yml",
            "ac-flask-redis.yml",
            "ac-prometheus-grafana.yml",
            "ac-nextcloud-postgres.yml",
            "ac-gitea-postgres.yml",
        ] {
            let p = plan_of(f, None);
            assert!(!p.services.is_empty(), "{f}");
        }
    }

    #[test]
    fn from_a_repository() {
        let mut files = BTreeMap::new();
        files.insert(".env".to_owned(), "TAG=2\n".to_owned());
        let file = "services:\n  web:\n    build:\n      context: ./app\n      dockerfile: docker/Dockerfile\n      args: ['VERSION=${TAG}']\n      secrets: [npmrc]\n    ports: ['8080:3000']\n";
        let repo = RepoRef {
            repo_url: "https://github.com/acme/shop".into(),
            branch: "main".into(),
            path: "deploy/compose.yml".into(),
            installation_id: Some(7),
            private: true,
        };
        let p = plan(&Input {
            file,
            files,
            repo: Some(repo),
            name_hint: "shop",
            ..Default::default()
        })
        .unwrap();
        let s = &p.services[0].source;
        assert_eq!(
            (
                s.kind.as_str(),
                s.build_context.as_str(),
                s.dockerfile_path.as_str()
            ),
            ("git", "deploy/app", "docker/Dockerfile")
        );
        assert_eq!(
            s.build_args,
            vec![PlanEnv {
                key: "VERSION".into(),
                value: "2".into(),
                secret: false
            }]
        );
        assert!(warned(&p, Some("web"), "build.secrets"));
        assert_eq!(p.services[0].ports[0].container_port, 3000);
        assert!(
            p.services[0].ports[0].domain.is_none() && warned(&p, None, "ports"),
            "no cluster: no free domain"
        );
    }

    #[test]
    fn edge_cases() {
        let p = plan_of("edge-cases.yml", None);
        let api = svc(&p, "api");
        assert_eq!(api.replicas, 3);
        assert_eq!(
            api.resources,
            PlanResources {
                cpu_request: 250,
                cpu_limit: 1500,
                memory_request: 256,
                memory_limit: 1024,
                gpu: 1
            }
        );
        assert_eq!(api.container.entrypoint, vec!["/bin/app"]);
        assert_eq!(api.container.command, vec!["serve", "--port", "8000"]);
        assert_eq!(
            (
                api.container.run_as_user,
                api.container.run_as_group,
                api.container.working_dir.as_str()
            ),
            (Some(1000), Some(1000), "/srv")
        );
        assert_eq!(api.health_check["type"], "exec");
        assert_eq!(
            api.health_check["command"],
            "curl -f http://localhost:8000/health"
        );
        assert_eq!(api.health_check["periodSeconds"], 15);
        let dns = svc(&p, "dns");
        // 53 is the servers' own (their DNS lookups): kept internal, with a warning.
        assert!(
            dns.ports
                .iter()
                .any(|x| x.protocol == "UDP" && !x.public && x.container_port == 53)
        );
        assert!(
            dns.ports
                .iter()
                .any(|x| x.protocol == "TCP" && !x.public && x.container_port == 53)
        );
        assert!(warned(&p, Some("dns"), "ports"));
        for field in [
            "privileged",
            "network_mode",
            "devices",
            "cap_add",
            "pid",
            "extends",
            "secrets",
        ] {
            assert!(p.warnings.iter().any(|w| w.field == field), "{field}");
        }
        assert!(
            p.warnings
                .iter()
                .any(|w| w.field == "volumes" && w.message.contains("Docker socket"))
        );
        assert!(
            warned(&p, Some("debug"), "profiles") && !p.services.iter().any(|s| s.name == "debug")
        );
        assert!(
            warned(&p, Some("worker"), "volumes"),
            "a volume shared with api"
        );
        assert!(warned(&p, Some("worker"), "user"), "a named user");
        assert_eq!(svc(&p, "worker").health_check["type"], "none");
        assert!(
            svc(&p, "worker")
                .env
                .iter()
                .any(|e| e.key == "API_URL" && e.value == "http://api:8000")
        );
    }
}
