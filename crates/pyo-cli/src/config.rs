//! Where the CLI keeps things: your sign-in (`~/.config/pyo/config.toml`,
//! readable only by you, or `%APPDATA%\pyo\config.toml` on Windows, in
//! your user profile) and the service a folder deploys (`pyo.toml`, found
//! in the current folder or the nearest one above it).

use std::{
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::api::Api;

pub const FILE: &str = "pyo.toml";

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

/// The API, from flags or env (`PYO_URL`, `PYO_TOKEN`) first, then
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

// ── pyo.toml ──────────────────────────────────────────────────────────

/// What a folder deploys and how. Every setting is optional: what's set
/// here wins on `pyo deploy`; the rest stays as the dashboard has it.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServiceFile {
    /// The service's id, written by `pyo link`.
    pub service: Option<String>,
    /// Image services: "ghcr.io/acme/api:1.2" (tag or @sha256 digest).
    pub image: Option<String>,
    #[serde(default)]
    pub build: Build,
    #[serde(default)]
    pub deploy: Deploy,
    #[serde(default)]
    pub resources: Resources,
    /// Variables. Secrets stay in Pyo: `pyo env set --secret`.
    #[serde(default)]
    pub env: toml::Table,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Build {
    pub branch: Option<String>,
    pub dockerfile: Option<String>,
    pub context: Option<String>,
    /// Build arguments, used by the next build: Dockerfile ARGs, or
    /// Railpack settings like RAILPACK_BUILD_CMD. They can end up in the
    /// image, so no secrets.
    #[serde(default)]
    pub args: toml::Table,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Deploy {
    pub replicas: Option<i64>,
    /// The port the service listens on for HTTP.
    pub port: Option<i64>,
    /// When a replica is ready: an HTTP path ("/healthz"), "tcp" for its
    /// port accepting connections, or "none" to not wait.
    pub health: Option<String>,
    pub auto_rollback: Option<bool>,
    /// A build Pyo detects as a static site: "static" serves its files
    /// from the cluster's shared static server (the default), "service"
    /// runs it as a service of its own.
    pub serve: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Resources {
    /// Limit: "500m" or "1" (cores).
    pub cpu: Option<String>,
    /// Limit: "512Mi" or "1Gi".
    pub memory: Option<String>,
}

/// The nearest pyo.toml: this folder, or the closest one above it.
pub fn find() -> Option<PathBuf> {
    let mut dir = std::env::current_dir().ok()?;
    loop {
        let p = dir.join(FILE);
        if p.is_file() {
            return Some(p);
        }
        if !dir.pop() {
            return None;
        }
    }
}

pub fn load(path: &Path) -> Result<ServiceFile> {
    let s = fs::read_to_string(path).with_context(|| format!("Can't read {}", path.display()))?;
    toml::from_str(&s).map_err(|e| anyhow::anyhow!("{}: {}", path.display(), e.message()))
}

/// Set `service = "<id>"` in pyo.toml, keeping everything else; make the
/// file in the current folder when there's none.
pub fn write_service(path: Option<&Path>, id: &str, label: &str) -> Result<PathBuf> {
    let path = path
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from(FILE));
    let line = format!("service = \"{id}\"  # {label}");
    let text = match fs::read_to_string(&path) {
        Ok(s) => {
            let mut done = false;
            let mut out: Vec<String> = s
                .lines()
                .map(|l| {
                    if !done && l.trim_start().starts_with("service") && l.contains('=') {
                        done = true;
                        line.clone()
                    } else {
                        l.to_owned()
                    }
                })
                .collect();
            if !done {
                out.insert(0, line);
            }
            out.join("\n") + "\n"
        }
        Err(_) => format!(
            "{line}\n\n# Settings here win on `pyo deploy`. Anything left out stays as\n# the dashboard has it. Secrets: `pyo env set --secret KEY=value`.\n\n# [deploy]\n# replicas = 1\n# port = 8080\n# health = \"/healthz\"  # or \"tcp\" or \"none\"\n# serve = \"static\"  # a static site's files, or \"service\"\n\n# [build.args]\n# RAILPACK_SPA_OUTPUT_DIR = \"dist\"\n\n# [resources]\n# cpu = \"500m\"\n# memory = \"512Mi\"\n\n# [env]\n# LOG_LEVEL = \"info\"\n"
        ),
    };
    fs::write(&path, text)?;
    Ok(path)
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

    #[test]
    fn full_file_parses() {
        let f: ServiceFile = toml::from_str(
            r#"
            service = "svc_abc"
            [build]
            dockerfile = "Dockerfile"
            [build.args]
            RAILPACK_BUILD_CMD = "npm run build"
            [deploy]
            replicas = 2
            port = 8080
            health = "/healthz"
            [resources]
            cpu = "500m"
            memory = "1Gi"
            [env]
            LOG_LEVEL = "info"
            WORKERS = 4
            "#,
        )
        .unwrap();
        assert_eq!(f.service.as_deref(), Some("svc_abc"));
        assert_eq!(f.deploy.replicas, Some(2));
        assert_eq!(f.env.len(), 2);
        assert_eq!(
            f.build.args["RAILPACK_BUILD_CMD"].as_str(),
            Some("npm run build")
        );
        assert!(toml::from_str::<ServiceFile>("replicas = 2").is_err());
    }
}
