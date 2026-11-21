//! Where the CLI keeps things: your sign-in (`~/.config/pyo/config.toml`,
//! readable only by you, or `%APPDATA%\pyo\config.toml` on Windows, in
//! your user profile) and the service a folder deploys (`pyo.jsonc`, found
//! in the current folder or the nearest one above it).

use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::api::Api;

pub const FILE: &str = "pyo.jsonc";
/// The file Pyo read before pyo.jsonc. Finding one only points to pyo link.
const OLD_FILE: &str = "pyo.toml";
/// pyo.jsonc's JSON Schema, for editors. pyo-web serves it from pyo-docs.
pub const SCHEMA_URL: &str = "https://pyo.dev/schema/pyo.json";

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Credentials {
    pub url: Option<String>,
    pub token: Option<String>,
    pub email: Option<String>,
}

/// A folder from the environment variable `name`, when it's set.
fn env_dir(name: &str) -> Option<PathBuf> {
    std::env::var_os(name)
        .filter(|d| !d.is_empty())
        .map(PathBuf::from)
}

/// Your home folder: `HOME`, or on Windows `USERPROFILE` when there's no
/// `HOME`.
fn home() -> Option<PathBuf> {
    env_dir("HOME").or_else(|| cfg!(windows).then(|| env_dir("USERPROFILE")).flatten())
}

/// Where the CLI keeps its settings: `XDG_CONFIG_HOME`, `%APPDATA%` on
/// Windows, or `~/.config`.
fn config_dir() -> Option<PathBuf> {
    env_dir("XDG_CONFIG_HOME")
        .or_else(|| cfg!(windows).then(|| env_dir("APPDATA")).flatten())
        .or_else(|| Some(home()?.join(".config")))
}

/// Where the CLI keeps what it can lose: `XDG_CACHE_HOME`,
/// `%LOCALAPPDATA%` on Windows, or `~/.cache`.
pub fn cache_dir() -> Option<PathBuf> {
    env_dir("XDG_CACHE_HOME")
        .or_else(|| cfg!(windows).then(|| env_dir("LOCALAPPDATA")).flatten())
        .or_else(|| Some(home()?.join(".cache")))
}

fn credentials_path() -> Result<PathBuf> {
    let base = config_dir().context("Can't find your home folder: HOME isn't set")?;
    Ok(base.join("pyo").join("config.toml"))
}

impl Credentials {
    pub fn load() -> Result<Self> {
        let path = credentials_path()?;
        match fs::read_to_string(&path) {
            Ok(s) => toml::from_str(&s).with_context(|| format!("Can't read {}", path.display())),
            Err(_) => Ok(Self::default()),
        }
    }

    pub fn save(&self) -> Result<PathBuf> {
        let path = credentials_path()?;
        fs::create_dir_all(path.parent().unwrap())?;
        let mut open = fs::OpenOptions::new();
        open.write(true).create(true).truncate(true);
        // Readable only by you from the start. On Windows the file is in
        // your user profile, which only you (and the computer's
        // administrators) can read.
        #[cfg(unix)]
        {
            use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
            open.mode(0o600);
            // A file saved before keeps its mode: set it too.
            if path.exists() {
                fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
            }
        }
        std::io::Write::write_all(&mut open.open(&path)?, toml::to_string(self)?.as_bytes())?;
        Ok(path)
    }

    pub fn clear() -> Result<()> {
        let _ = fs::remove_file(credentials_path()?);
        Ok(())
    }
}

/// The API, from flags or env (`PYO_URL`, `PYO_API_KEY`) first, then
/// the saved sign-in. CI sets the env and never logs in.
pub fn client(url: Option<&str>, token: Option<&str>) -> Result<Api> {
    let saved = Credentials::load()?;
    let url = url
        .map(str::to_owned)
        .or(saved.url)
        .context("Not signed in. Run: pyo login")?;
    let token = token
        .map(str::to_owned)
        .or(saved.token)
        .context("Not signed in. Run: pyo login")?;
    Api::new(&url, &token)
}

// ── pyo.jsonc ─────────────────────────────────────────────────────────
//
// JSON with comments and trailing commas. The doc comments on these types
// are the descriptions editors show on hover: they go into the JSON Schema
// at https://pyo.dev/schema/pyo.json (see `schema` in the tests below).

/// Settings for a Pyo service, kept next to its code. pyo link writes this file, and pyo deploy applies it. Every setting is optional. What's set here wins, and the rest stays as it is in the dashboard.
#[derive(Debug, Default, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[cfg_attr(test, schemars(title = "pyo.jsonc"))]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ServiceFile {
    /// The JSON Schema for this file. Editors use it to complete and check your settings.
    #[serde(rename = "$schema")]
    #[allow(dead_code, reason = "for editors: Pyo only allows it")]
    pub schema: Option<String>,
    /// The service this folder deploys, by id. Written by pyo link.
    pub service: Option<String>,
    /// Image services only. The image to deploy, with a tag or an @sha256 digest, like ghcr.io/acme/api:1.2.
    pub image: Option<String>,
    /// How the service is built. Services built from Git only.
    #[serde(default)]
    pub build: Build,
    /// How the service runs.
    #[serde(default)]
    pub deploy: Deploy,
    /// The most CPU and memory each replica can use.
    #[serde(default)]
    pub resources: Resources,
    /// Variables, as text, numbers or true and false. Not secrets, because this file is usually committed. Keep secrets in Pyo with pyo env set --secret.
    #[serde(default)]
    #[cfg_attr(test, schemars(schema_with = "scalars"))]
    pub env: BTreeMap<String, Value>,
}

#[derive(Debug, Default, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Build {
    /// The Git branch to build.
    pub branch: Option<String>,
    /// The Dockerfile to build, like Dockerfile or docker/api.Dockerfile.
    pub dockerfile_path: Option<String>,
    /// The folder whose files are sent to the build, like . or apps/web.
    pub context: Option<String>,
    /// Build arguments, used on the next deploy: Dockerfile ARGs, or Railpack settings like RAILPACK_SPA_OUTPUT_DIR. They can end up in the image, so keep secrets out.
    #[serde(default)]
    #[cfg_attr(test, schemars(schema_with = "scalars"))]
    pub args: BTreeMap<String, Value>,
}

#[derive(Debug, Default, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Deploy {
    /// How many replicas run. A replica is one running copy of the service.
    #[cfg_attr(test, schemars(range(min = 0, max = 2147483647)))]
    pub replicas: Option<i64>,
    /// The port the service listens on for HTTP.
    #[cfg_attr(test, schemars(range(min = 1, max = 65535)))]
    pub port: Option<i64>,
    /// Checked before traffic moves over: an HTTP path like /healthz, "tcp" for its port accepting connections, or "none" to not wait.
    #[cfg_attr(test, schemars(extend("examples" = ["/healthz", "tcp", "none"])))]
    pub health: Option<String>,
    /// Put the previous version back when a deploy fails. On by default.
    pub auto_rollback: Option<bool>,
    /// A static site's build: "static" serves its files, "service" runs it as a service of its own.
    #[cfg_attr(test, schemars(extend("enum" = ["static", "service"])))]
    pub serve_as: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Resources {
    /// The CPU limit: "500m" or "1" (cores).
    #[cfg_attr(test, schemars(extend("examples" = ["500m", "1"])))]
    pub cpu: Option<String>,
    /// The memory limit: "512Mi" or "1Gi".
    #[cfg_attr(test, schemars(extend("examples" = ["512Mi", "1Gi"])))]
    pub memory: Option<String>,
}

/// env and build.args: names to text, numbers or true and false.
#[cfg(test)]
fn scalars(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
    schemars::json_schema!({
        "type": "object",
        "additionalProperties": { "type": ["string", "number", "boolean"] },
    })
}

/// What the nearest folder with settings has.
#[derive(Debug, PartialEq)]
pub enum Found {
    /// A pyo.jsonc.
    File(PathBuf),
    /// Only a pyo.toml, which Pyo no longer reads.
    Old(PathBuf),
}

/// The nearest pyo.jsonc (or pyo.toml) in `dir` or the closest folder above it.
pub fn find_from(dir: &Path) -> Option<Found> {
    dir.ancestors().find_map(|d| {
        let p = d.join(FILE);
        if p.is_file() {
            return Some(Found::File(p));
        }
        let old = d.join(OLD_FILE);
        old.is_file().then_some(Found::Old(old))
    })
}

/// The nearest pyo.jsonc: this folder, or the closest one above it.
pub fn find() -> Result<Option<PathBuf>> {
    match std::env::current_dir() {
        Ok(dir) => find_in(&dir),
        Err(_) => Ok(None),
    }
}

/// The nearest pyo.jsonc from `dir`. A pyo.toml found first is an error
/// that points to pyo link.
fn find_in(dir: &Path) -> Result<Option<PathBuf>> {
    match find_from(dir) {
        Some(Found::File(p)) => Ok(Some(p)),
        Some(Found::Old(_)) => bail!("Pyo now uses pyo.jsonc instead of pyo.toml. Run: pyo link"),
        None => Ok(None),
    }
}

/// What pyo.jsonc may hold beyond plain JSON: comments and trailing commas.
fn parse_options() -> jsonc_parser::ParseOptions {
    jsonc_parser::ParseOptions {
        allow_comments: true,
        allow_trailing_commas: true,
        allow_loose_object_property_names: false,
        allow_missing_commas: false,
        allow_single_quoted_strings: false,
        allow_hexadecimal_numbers: false,
        allow_unary_plus_numbers: false,
        allow_bare_decimal_point_numbers: false,
        allow_non_finite_numbers: false,
        allow_extended_string_escapes: false,
    }
}

pub fn load(path: &Path) -> Result<ServiceFile> {
    let s = fs::read_to_string(path).with_context(|| format!("Can't read {}", path.display()))?;
    parse(&s).map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))
}

/// pyo.jsonc's text as settings. Errors name the setting they're about.
pub fn parse(text: &str) -> Result<ServiceFile> {
    let value: Option<Value> = jsonc_parser::parse_to_serde_value(text, &parse_options())?;
    let value = value.unwrap_or_else(|| Value::Object(Default::default()));
    serde_path_to_error::deserialize(value).map_err(|e| {
        let path = e.path().to_string();
        let inner = e.into_inner().to_string();
        // serde says "unknown field `x`, expected one of ...".
        if let Some(rest) = inner.strip_prefix("unknown field `") {
            let key = rest.split('`').next().unwrap_or(rest);
            let at = match path.rsplit_once('.') {
                Some((parent, _)) if path != "." => format!("{parent}.{key}"),
                _ => key.to_owned(),
            };
            return anyhow::anyhow!("unknown setting \"{at}\"");
        }
        if path == "." {
            anyhow::anyhow!("{inner}")
        } else {
            anyhow::anyhow!("{path}: {inner}")
        }
    })
}

/// A new pyo.jsonc, linked to the service `id`.
fn new_file(id: &str) -> String {
    let id = Value::from(id);
    format!("{{\n  \"$schema\": \"{SCHEMA_URL}\",\n  \"service\": {id}\n}}\n")
}

/// Set `"service": "<id>"` in pyo.jsonc, keeping everything else as it
/// is, comments too. Without a file, make one in the current folder.
pub fn write_service(path: Option<&Path>, id: &str) -> Result<PathBuf> {
    let path = path
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from(FILE));
    let text = match fs::read_to_string(&path) {
        Ok(s) => set_service(&s, id).map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))?,
        Err(_) => new_file(id),
    };
    fs::write(&path, text)?;
    Ok(path)
}

/// `text` with its service set to `id`, and nothing else changed.
fn set_service(text: &str, id: &str) -> Result<String> {
    use jsonc_parser::cst::{CstInputValue, CstRootNode};
    if text.trim().is_empty() {
        return Ok(new_file(id));
    }
    let root = CstRootNode::parse(text, &parse_options())?;
    let Some(obj) = root.object_value() else {
        bail!("the file isn't an object, like {{ \"service\": \"...\" }}");
    };
    let value = CstInputValue::String(id.to_owned());
    match obj.get("service") {
        Some(prop) => prop.set_value(value),
        None => {
            // After "$schema" when it's first, else at the top.
            let at = usize::from(
                obj.properties()
                    .first()
                    .and_then(|p| p.name()?.decoded_value().ok())
                    .is_some_and(|n| n == "$schema"),
            );
            obj.insert(at, "service", value);
        }
    }
    Ok(root.to_string())
}

/// Set `deploy.replicas` in pyo.jsonc to `n`, keeping everything else as
/// it is, comments too. For a file that sets it already.
pub fn write_replicas(path: &Path, n: u32) -> Result<()> {
    let text = fs::read_to_string(path)?;
    fs::write(path, set_replicas(&text, n)?)?;
    Ok(())
}

/// `text` with `deploy.replicas` set to `n`, and nothing else changed.
fn set_replicas(text: &str, n: u32) -> Result<String> {
    use jsonc_parser::cst::{CstInputValue, CstRootNode};
    let root = CstRootNode::parse(text, &parse_options())?;
    let prop = root
        .object_value()
        .and_then(|o| o.object_value("deploy"))
        .and_then(|d| d.get("replicas"))
        .context("the file doesn't set deploy.replicas")?;
    prop.set_value(CstInputValue::Number(n.to_string()));
    Ok(root.to_string())
}

/// "500m" → 500, "1.5" or "1.5 cores" → 1500 (millicores).
pub fn parse_cpu(s: &str) -> Result<i64> {
    let s = s.trim();
    if let Some(m) = s.strip_suffix('m') {
        return m
            .trim()
            .parse()
            .with_context(|| format!("cpu: \"{s}\" isn't like \"500m\" or \"1\""));
    }
    let cores: f64 = s
        .parse()
        .with_context(|| format!("cpu: \"{s}\" isn't like \"500m\" or \"1\""))?;
    Ok((cores * 1000.0).round() as i64)
}

/// "512Mi" → 512, "1Gi" or "1G" → 1024 (MiB).
pub fn parse_memory(s: &str) -> Result<i64> {
    let s = s.trim();
    let (n, mult) = if let Some(n) = s.strip_suffix("Gi").or_else(|| s.strip_suffix('G')) {
        (n, 1024.0)
    } else if let Some(n) = s.strip_suffix("Mi").or_else(|| s.strip_suffix('M')) {
        (n, 1.0)
    } else {
        bail!("memory: \"{s}\" isn't like \"512Mi\" or \"1Gi\"");
    };
    let v: f64 = n
        .trim()
        .parse()
        .with_context(|| format!("memory: \"{s}\" isn't like \"512Mi\" or \"1Gi\""))?;
    Ok((v * mult).round() as i64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn units() {
        assert_eq!(parse_cpu("500m").unwrap(), 500);
        assert_eq!(parse_cpu("1.5").unwrap(), 1500);
        assert_eq!(parse_memory("512Mi").unwrap(), 512);
        assert_eq!(parse_memory("1Gi").unwrap(), 1024);
        assert!(parse_memory("12").is_err());
    }

    const FULL: &str = r#"{
  "$schema": "https://pyo.dev/schema/pyo.json",
  "service": "svc_abc", // written by pyo link
  /* Git services */
  "build": {
    "dockerfilePath": "Dockerfile",
    "args": { "RAILPACK_BUILD_CMD": "npm run build", },
  },
  "deploy": { "replicas": 2, "port": 8080, "health": "/healthz", "autoRollback": false, "serveAs": "service" },
  "resources": { "cpu": "500m", "memory": "1Gi" },
  "env": { "LOG_LEVEL": "info", "WORKERS": 4 },
}
"#;

    #[test]
    fn full_file_parses() {
        let f = parse(FULL).unwrap();
        assert_eq!(f.service.as_deref(), Some("svc_abc"));
        assert_eq!(f.build.dockerfile_path.as_deref(), Some("Dockerfile"));
        assert_eq!(f.deploy.replicas, Some(2));
        assert_eq!(f.deploy.auto_rollback, Some(false));
        assert_eq!(f.deploy.serve_as.as_deref(), Some("service"));
        assert_eq!(f.env.len(), 2);
        assert_eq!(f.build.args["RAILPACK_BUILD_CMD"], "npm run build");
        // An empty file is no settings.
        assert!(parse("").unwrap().service.is_none());
        assert!(parse("{}").unwrap().service.is_none());
    }

    #[test]
    fn unknown_settings_are_named() {
        let e = parse(r#"{ "replicas": 2 }"#).unwrap_err().to_string();
        assert_eq!(e, "unknown setting \"replicas\"");
        let e = parse(r#"{ "deploy": { "replica": 2 } }"#)
            .unwrap_err()
            .to_string();
        assert_eq!(e, "unknown setting \"deploy.replica\"");
        // The TOML spelling isn't taken either.
        let e = parse(r#"{ "deploy": { "auto_rollback": true } }"#)
            .unwrap_err()
            .to_string();
        assert_eq!(e, "unknown setting \"deploy.auto_rollback\"");
        let e = parse(r#"{ "deploy": { "replicas": "2" } }"#)
            .unwrap_err()
            .to_string();
        assert!(e.starts_with("deploy.replicas: invalid type"), "{e}");
        // load says which file.
        let dir = temp("unknown");
        fs::write(dir.join(FILE), r#"{ "servic": "svc_a" }"#).unwrap();
        let e = load(&dir.join(FILE)).unwrap_err().to_string();
        assert_eq!(
            e,
            format!("{}: unknown setting \"servic\"", dir.join(FILE).display())
        );
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn only_comments_and_trailing_commas_beyond_json() {
        assert!(parse("// a comment\n{ \"service\": \"svc_a\", /* here */ }").is_ok());
        assert!(parse("{ service: \"svc_a\" }").is_err());
        assert!(parse("{ \"service\": 'svc_a' }").is_err());
        assert!(parse("{ \"service\": \"svc_a\" \"image\": \"x\" }").is_err());
        assert!(parse("[]").is_err());
    }

    /// An empty folder of its own for a test.
    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("pyo-cli-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn found_in_this_folder_or_above() {
        let dir = temp("walk");
        let deep = dir.join("a/b");
        fs::create_dir_all(&deep).unwrap();
        assert_eq!(find_from(&deep), None);
        fs::write(dir.join(FILE), "{}").unwrap();
        assert_eq!(find_from(&deep), Some(Found::File(dir.join(FILE))));
        fs::write(dir.join("a").join(FILE), "{}").unwrap();
        assert_eq!(
            find_from(&deep),
            Some(Found::File(dir.join("a").join(FILE)))
        );
        // A pyo.toml nearer than any pyo.jsonc points to pyo link.
        fs::write(deep.join(OLD_FILE), "").unwrap();
        assert_eq!(find_from(&deep), Some(Found::Old(deep.join(OLD_FILE))));
        assert_eq!(
            find_in(&deep).unwrap_err().to_string(),
            "Pyo now uses pyo.jsonc instead of pyo.toml. Run: pyo link"
        );
        // Next to a pyo.jsonc, it's ignored.
        fs::write(deep.join(FILE), "{}").unwrap();
        assert_eq!(find_from(&deep), Some(Found::File(deep.join(FILE))));
        assert_eq!(find_in(&deep).unwrap(), Some(deep.join(FILE)));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn link_writes_a_new_file() {
        let dir = temp("link-new");
        let path = write_service(Some(&dir.join(FILE)), "svc_new").unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert_eq!(
            text,
            "{\n  \"$schema\": \"https://pyo.dev/schema/pyo.json\",\n  \"service\": \"svc_new\"\n}\n"
        );
        let f = load(&path).unwrap();
        assert_eq!(f.schema.as_deref(), Some(SCHEMA_URL));
        assert_eq!(f.service.as_deref(), Some("svc_new"));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn link_changes_only_the_service() {
        let text = set_service(FULL, "svc_new").unwrap();
        assert_eq!(text, FULL.replace("\"svc_abc\"", "\"svc_new\""));

        // No service yet: it goes after "$schema", or first.
        let text = set_service(
            "{\n  \"$schema\": \"https://pyo.dev/schema/pyo.json\",\n  // Port\n  \"deploy\": { \"port\": 80 },\n}\n",
            "svc_new",
        )
        .unwrap();
        let f = parse(&text).unwrap();
        assert_eq!(f.service.as_deref(), Some("svc_new"));
        assert_eq!(f.deploy.port, Some(80));
        assert!(text.contains("// Port"), "{text}");
        let schema = text.find("$schema").unwrap();
        let service = text.find("\"service\"").unwrap();
        let deploy = text.find("\"deploy\"").unwrap();
        assert!(schema < service && service < deploy, "{text}");

        let text = set_service("{ \"deploy\": { \"port\": 80 } }", "svc_new").unwrap();
        assert!(text.find("\"service\"").unwrap() < text.find("\"deploy\"").unwrap());
        assert_eq!(parse(&text).unwrap().service.as_deref(), Some("svc_new"));

        assert_eq!(set_service("  \n", "svc_new").unwrap(), new_file("svc_new"));
        assert!(set_service("[]", "svc_new").is_err());
    }

    #[test]
    fn scale_changes_only_the_replicas() {
        let text = set_replicas(FULL, 5).unwrap();
        assert_eq!(text, FULL.replace("\"replicas\": 2", "\"replicas\": 5"));
        assert_eq!(parse(&text).unwrap().deploy.replicas, Some(5));
        // Across lines, with comments.
        let text = "{\n  \"deploy\": {\n    // more for launch day\n    \"replicas\": 2, // was 1\n  },\n}\n";
        assert_eq!(
            set_replicas(text, 40).unwrap(),
            text.replace("\"replicas\": 2", "\"replicas\": 40")
        );
        // Nothing to change: the file doesn't set it.
        assert!(set_replicas("{ \"deploy\": { \"port\": 80 } }", 3).is_err());
        assert!(set_replicas("{ \"service\": \"svc_a\" }", 3).is_err());
        assert!(set_replicas("[]", 3).is_err());
        // write_replicas changes the file.
        let dir = temp("scale");
        let path = dir.join(FILE);
        fs::write(&path, FULL).unwrap();
        write_replicas(&path, 3).unwrap();
        assert_eq!(load(&path).unwrap().deploy.replicas, Some(3));
        assert!(
            fs::read_to_string(&path)
                .unwrap()
                .contains("// written by pyo link")
        );
        fs::remove_dir_all(dir).unwrap();
    }

    /// The JSON Schema, from the types above: draft 7, which editors know
    /// best, with each object's settings inline so their descriptions show.
    fn schema_json() -> String {
        let mut settings = schemars::generate::SchemaSettings::draft07();
        settings.inline_subschemas = true;
        let mut schema = settings
            .into_generator()
            .into_root_schema_for::<ServiceFile>();
        no_null(&mut schema);
        let mut out = serde_json::Map::new();
        out.insert("$schema".into(), schema.get("$schema").unwrap().clone());
        out.insert("$id".into(), SCHEMA_URL.into());
        for (k, v) in schema.as_object().unwrap() {
            out.entry(k.clone()).or_insert(v.clone());
        }
        let mut s = serde_json::to_string_pretty(&out).unwrap();
        s.push('\n');
        s
    }

    /// Unset settings are left out, not null: `type: ["string", "null"]`
    /// becomes `type: "string"`.
    fn no_null(schema: &mut schemars::Schema) {
        if let Some(Value::Array(types)) = schema.get("type") {
            let types: Vec<Value> = types.iter().filter(|t| *t != "null").cloned().collect();
            let t = match <[Value; 1]>::try_from(types) {
                Ok([one]) => one,
                Err(many) => Value::Array(many),
            };
            schema.insert("type".into(), t);
        }
        schemars::transform::transform_subschemas(&mut no_null, schema);
    }

    /// pyo-docs/schema/pyo.json, beside this repository (or in
    /// PYO_DOCS_DIR), is what https://pyo.dev/schema/pyo.json serves. Write
    /// it with: PYO_WRITE_SCHEMA=1 cargo test -p pyo-cli schema
    #[test]
    fn schema_is_current() {
        let docs = std::env::var_os("PYO_DOCS_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../pyo-docs"));
        let path = docs.join("schema/pyo.json");
        let want = schema_json();
        if std::env::var_os("PYO_WRITE_SCHEMA").is_some() {
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, &want).unwrap();
            return;
        }
        let Ok(have) = fs::read_to_string(&path) else {
            return;
        };
        assert!(
            have == want,
            "{} is out of date: run PYO_WRITE_SCHEMA=1 cargo test -p pyo-cli schema",
            path.display()
        );
    }

    #[test]
    fn schema_allows_what_the_cli_reads() {
        let schema: Value = serde_json::from_str(&schema_json()).unwrap();
        let props = &schema["properties"];
        assert_eq!(schema["additionalProperties"], false);
        assert_eq!(props["$schema"]["type"], "string");
        assert_eq!(props["deploy"]["additionalProperties"], false);
        assert!(props["deploy"]["properties"]["autoRollback"].is_object());
        assert!(props["build"]["properties"]["args"].is_object());
        assert_eq!(props["deploy"]["properties"]["port"]["maximum"], 65535);
        // Every setting says what it is.
        fn described(path: &str, s: &Value) {
            for (k, v) in s["properties"].as_object().into_iter().flatten() {
                let at = format!("{path}{k}");
                assert!(v["description"].is_string(), "{at} has no description");
                described(&format!("{at}."), v);
            }
        }
        described("", &schema);
    }
}
