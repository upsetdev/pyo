//! `pyo upgrade`, the notice that a newer CLI is out, and the warning when
//! a Pyo is older than the CLI.
//!
//! Releases come from GitHub: `releases/latest` redirects to the latest
//! release's tag, which tells its version without the API (and its rate
//! limit). The binary is the release's build for this platform
//! (pyo-linux-x86_64, pyo-macos-aarch64 and so on, see `asset`), checked
//! against its .sha256, the same files `curl -fsSL https://pyo.dev/cli.sh
//! | sh` installs.
//!
//! Only release builds replace themselves: the release's binary is built
//! with PYO_CLI_RELEASE set (deploy/images/pyo.Dockerfile). A CLI built
//! from source with cargo is left to whoever built it.

use std::{
    io::{IsTerminal, Write},
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::ui;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
const RELEASE_BUILD: bool = option_env!("PYO_CLI_RELEASE").is_some();
const RELEASES: &str = "https://github.com/upsetdev/pyo/releases";
const INSTALL: &str = "curl -fsSL https://pyo.dev/cli.sh | sh";
const DAY: u64 = 24 * 60 * 60;

/// The release's binary for `os` and `arch` (`std::env::consts`'s names),
/// when the release has one: pyo-<os>-<arch>, with .exe on Windows.
/// deploy/cli.sh and the release workflow use the same names.
fn asset(os: &str, arch: &str) -> Option<&'static str> {
    Some(match (os, arch) {
        ("linux", "x86_64") => "pyo-linux-x86_64",
        ("linux", "aarch64") => "pyo-linux-aarch64",
        ("macos", "x86_64") => "pyo-macos-x86_64",
        ("macos", "aarch64") => "pyo-macos-aarch64",
        ("windows", "x86_64") => "pyo-windows-x86_64.exe",
        _ => return None,
    })
}

/// "1.2.3" or "v1.2.3" → (1, 2, 3). Anything after a `-` or `+` is ignored.
fn parse(v: &str) -> Option<(u64, u64, u64)> {
    let v = v.trim().trim_start_matches('v');
    let v = v.split(['-', '+']).next()?;
    let mut n = v.split('.').map(|p| p.parse::<u64>().ok());
    let out = (n.next()??, n.next()??, n.next()??);
    n.next().is_none().then_some(out)
}

/// Whether `a` is a later version than `b`. A release candidate compares
/// as its release does, and comes just before it, so a candidate's CLI
/// upgrades to its release.
fn newer(a: &str, b: &str) -> bool {
    pyo_proto::version_newer(a.trim(), b.trim())
}

fn http(timeout: Duration, redirects: bool) -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .user_agent(concat!("pyo-cli/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(10).min(timeout))
        .timeout(timeout)
        .redirect(if redirects {
            reqwest::redirect::Policy::limited(10)
        } else {
            reqwest::redirect::Policy::none()
        })
        .build()?)
}

/// The latest release's version, from where `releases/latest` redirects
/// (…/releases/tag/v1.2.3).
async fn latest(timeout: Duration) -> Result<String> {
    let res = http(timeout, false)?
        .get(format!("{RELEASES}/latest"))
        .send()
        .await
        .context("Can't reach GitHub to look for a new version")?;
    let to = res
        .headers()
        .get(reqwest::header::LOCATION)
        .and_then(|l| l.to_str().ok())
        .unwrap_or("");
    let tag = to.rsplit_once("/tag/").map(|(_, t)| t).unwrap_or("");
    match parse(tag) {
        Some((a, b, c)) => Ok(format!("{a}.{b}.{c}")),
        None => bail!(
            "GitHub didn't say which version is the latest ({}).",
            res.status()
        ),
    }
}

// ── pyo upgrade ───────────────────────────────────────────────────────

/// Replace this binary with the latest release, or with `version`.
pub async fn run(version: Option<String>) -> Result<()> {
    if !RELEASE_BUILD {
        bail!(
            "This pyo ({VERSION}) was built from source, so it doesn't upgrade itself. \
             Update it from source, or install the release: {INSTALL}"
        );
    }
    let os = std::env::consts::OS;
    let arch = std::env::consts::ARCH;
    let Some(asset) = asset(os, arch) else {
        bail!("There's no release of the CLI for {os} {arch}.");
    };
    let target = match version {
        Some(v) => match parse(&v) {
            Some((a, b, c)) => format!("{a}.{b}.{c}"),
            None => bail!("\"{v}\" isn't a version like 0.3.0."),
        },
        None => {
            let latest = latest(Duration::from_secs(20)).await?;
            if !newer(&latest, VERSION) {
                ui::ok(&format!("Pyo CLI is up to date ({VERSION})."));
                return Ok(());
            }
            latest
        }
    };
    if target == VERSION {
        ui::ok(&format!("Pyo CLI is up to date ({VERSION})."));
        return Ok(());
    }

    let exe = std::env::current_exe()
        .and_then(|p| p.canonicalize())
        .context("Can't tell where this pyo is")?;
    let dir = exe.parent().context("Can't tell where this pyo is")?;
    // Before downloading: can the new binary go beside this one?
    let tmp = dir.join(format!(".pyo.upgrade.{}", std::process::id()));
    match std::fs::File::create(&tmp) {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            bail!(
                "You can't write to {}, where pyo is. Run pyo upgrade again as a user who can, \
                 or install it into a folder of yours: {INSTALL}",
                dir.display()
            )
        }
        Err(e) => return Err(e).with_context(|| format!("Can't write to {}", dir.display())),
    }
    let result = replace(&exe, &tmp, &target, asset).await;
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result?;
    if newer(&target, VERSION) {
        ui::ok(&format!("Upgraded the Pyo CLI from {VERSION} to {target}."));
    } else {
        ui::ok(&format!("Changed the Pyo CLI from {VERSION} to {target}."));
    }
    Ok(())
}

/// Download `version`, check its sha256, write it to `tmp` (beside `exe`)
/// and rename it over `exe`: whoever runs pyo meanwhile gets the old binary
/// or the new one, never half of one.
async fn replace(exe: &Path, tmp: &Path, version: &str, asset: &str) -> Result<()> {
    let base = format!("{RELEASES}/download/v{version}");
    let client = http(Duration::from_secs(300), true)?;
    let get = |name: String| {
        let client = client.clone();
        async move {
            let res = client
                .get(&name)
                .send()
                .await
                .with_context(|| format!("Can't download {name}"))?;
            if res.status() == reqwest::StatusCode::NOT_FOUND {
                bail!("There's no Pyo CLI {version} to download.");
            }
            let res = res
                .error_for_status()
                .with_context(|| format!("Can't download {name}"))?;
            Ok::<_, anyhow::Error>(res.bytes().await?)
        }
    };
    println!("Downloading the Pyo CLI {version}…");
    let sums = get(format!("{base}/{asset}.sha256")).await?;
    let bin = get(format!("{base}/{asset}")).await?;
    if !matches_sha256(&sums, &bin) {
        bail!("The download doesn't match its sha256. Nothing was changed. Try again.");
    }
    let mut f = std::fs::File::create(tmp)?;
    f.write_all(&bin)?;
    f.sync_all()?;
    drop(f);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(tmp, std::fs::Permissions::from_mode(0o755))?;
    }
    swap(exe, tmp).with_context(|| format!("Can't replace {}", exe.display()))
}

/// Put `new` where `exe` is.
#[cfg(not(windows))]
fn swap(exe: &Path, new: &Path) -> std::io::Result<()> {
    std::fs::rename(new, exe)
}

/// Put `new` where `exe` is. Windows doesn't let a running exe be written
/// over, but it lets it be renamed. So the old one moves aside first (the
/// next pyo removes it, see `remove_old`), and moves back if `new` can't
/// take its place.
#[cfg(windows)]
fn swap(exe: &Path, new: &Path) -> std::io::Result<()> {
    let old = old_exe(exe);
    let _ = std::fs::remove_file(&old);
    std::fs::rename(exe, &old)?;
    if let Err(e) = std::fs::rename(new, exe) {
        let _ = std::fs::rename(&old, exe);
        return Err(e);
    }
    Ok(())
}

/// Where `pyo upgrade` moves the exe it replaces on Windows.
#[cfg(windows)]
fn old_exe(exe: &Path) -> PathBuf {
    exe.with_file_name(".pyo.old.exe")
}

/// Remove the exe a `pyo upgrade` moved aside on Windows, once it no
/// longer runs. Quiet when there's none or it's still running.
pub fn remove_old() {
    #[cfg(windows)]
    if let Ok(exe) = std::env::current_exe().and_then(|p| p.canonicalize()) {
        let _ = std::fs::remove_file(old_exe(&exe));
    }
}

/// Whether `bin` is what `sums` (sha256sum's "<hex>  <name>") says.
fn matches_sha256(sums: &[u8], bin: &[u8]) -> bool {
    let want = String::from_utf8_lossy(sums)
        .split_whitespace()
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    want.len() == 64 && want == hex::encode(Sha256::digest(bin))
}

// ── The notice ────────────────────────────────────────────────────────

/// What the last check found (`~/.cache/pyo/update-check.json`).
#[derive(Default, Serialize, Deserialize)]
struct Seen {
    /// When GitHub was last asked (Unix seconds).
    #[serde(default)]
    checked: u64,
    /// The latest version it answered.
    #[serde(default)]
    latest: String,
    /// When the notice was last shown.
    #[serde(default)]
    noticed: u64,
}

fn cache_path() -> Option<PathBuf> {
    Some(
        crate::config::cache_dir()?
            .join("pyo")
            .join("update-check.json"),
    )
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn set(name: &str) -> bool {
    std::env::var_os(name).is_some_and(|v| !v.is_empty() && v != "0")
}

/// A look for a newer release while a command runs: at most once a day,
/// in the background, so it never holds a command up. When the command
/// ends before GitHub answers, the next command asks again.
pub struct Check {
    path: PathBuf,
    seen: Seen,
    task: Option<tokio::task::JoinHandle<Result<String>>>,
}

impl Check {
    /// None when it's off: not a release build, stderr isn't a terminal,
    /// in CI, or PYO_NO_UPDATE_CHECK=1.
    pub fn start() -> Option<Self> {
        if !RELEASE_BUILD
            || !std::io::stderr().is_terminal()
            || set("CI")
            || set("PYO_NO_UPDATE_CHECK")
        {
            return None;
        }
        let path = cache_path()?;
        let seen: Seen = std::fs::read(&path)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        let task = (now().saturating_sub(seen.checked) >= DAY)
            .then(|| tokio::spawn(latest(Duration::from_secs(10))));
        Some(Self { path, seen, task })
    }

    /// After the command: note what GitHub said, if it answered, and say
    /// when a newer CLI is out (once a day at most).
    pub fn finish(mut self) {
        let mut changed = false;
        if let Some(task) = self.task.take().filter(|t| t.is_finished()) {
            // Answered, or failed (offline): either way, not again today.
            self.seen.checked = now();
            if let Some(Ok(Ok(v))) = futures::FutureExt::now_or_never(task) {
                self.seen.latest = v;
            }
            changed = true;
        }
        if newer(&self.seen.latest, VERSION) && now().saturating_sub(self.seen.noticed) >= DAY {
            eprintln!(
                "{}",
                ui::yellow(&format!(
                    "Pyo CLI {} is out (you have {VERSION}). Run pyo upgrade.",
                    self.seen.latest
                ))
            );
            self.seen.noticed = now();
            changed = true;
        }
        if changed {
            let _ = std::fs::create_dir_all(self.path.parent().unwrap_or(Path::new(".")));
            if let Ok(json) = serde_json::to_vec(&self.seen) {
                let _ = std::fs::write(&self.path, json);
            }
        }
    }
}

// ── Pyo older than the CLI ────────────────────────────────────────────

/// Warn when the Pyo at `base` is a minor version or more behind this CLI:
/// commands new in the CLI may not work there yet. Quiet when it can't
/// tell.
pub async fn warn_if_pyo_is_older(base: &str) {
    let Ok(client) = http(Duration::from_secs(5), true) else {
        return;
    };
    let Ok(res) = client.get(format!("{base}/api/v1/meta")).send().await else {
        return;
    };
    let Ok(meta) = res.json::<serde_json::Value>().await else {
        return;
    };
    let pyo = meta["version"].as_str().unwrap_or("");
    if pyo_is_behind(pyo, VERSION) {
        eprintln!(
            "{}",
            ui::yellow(&format!(
                "This Pyo runs {pyo}, older than the CLI ({VERSION}). Some commands may not work \
                 until you update it under Settings › Updates."
            ))
        );
    }
}

/// Whether Pyo `pyo` is behind CLI `cli` by a minor version or more.
fn pyo_is_behind(pyo: &str, cli: &str) -> bool {
    matches!((parse(pyo), parse(cli)), (Some((a, b, _)), Some((c, d, _))) if (c, d) > (a, b))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions() {
        assert_eq!(parse("v0.2.5"), Some((0, 2, 5)));
        assert_eq!(parse("1.10.0-rc.1"), Some((1, 10, 0)));
        assert_eq!(parse("1.2"), None);
        assert_eq!(parse(""), None);
        assert!(newer("0.10.0", "0.9.9"));
        assert!(!newer("0.2.5", "0.2.5"));
        assert!(!newer("", "0.2.5"));
        assert!(newer("v0.52.0", "0.52.0-rc.1"));
        assert!(newer("0.52.0-rc.2", "0.52.0-rc.1"));
        assert!(!newer("0.52.0-rc.1", "0.52.0"));
        assert!(newer("0.52.0-rc.1", "0.51.0"));
        assert!(!newer("0.51.0", "0.52.0-rc.1"));
        assert!(pyo_is_behind("0.2.5", "0.3.0"));
        assert!(!pyo_is_behind("0.2.5", "0.2.9"));
        assert!(!pyo_is_behind("0.3.0", "0.2.5"));
        assert!(!pyo_is_behind("", "0.3.0"));
    }

    #[test]
    fn assets() {
        assert_eq!(asset("linux", "x86_64"), Some("pyo-linux-x86_64"));
        assert_eq!(asset("linux", "aarch64"), Some("pyo-linux-aarch64"));
        assert_eq!(asset("macos", "x86_64"), Some("pyo-macos-x86_64"));
        assert_eq!(asset("macos", "aarch64"), Some("pyo-macos-aarch64"));
        assert_eq!(asset("windows", "x86_64"), Some("pyo-windows-x86_64.exe"));
        assert_eq!(asset("windows", "aarch64"), None);
        assert_eq!(asset("linux", "riscv64"), None);
        assert_eq!(asset("freebsd", "x86_64"), None);
        // This build's own platform, on every platform the release has.
        if cfg!(any(
            all(
                any(target_os = "linux", target_os = "macos"),
                any(target_arch = "x86_64", target_arch = "aarch64")
            ),
            all(windows, target_arch = "x86_64"),
        )) {
            assert!(asset(std::env::consts::OS, std::env::consts::ARCH).is_some());
        }
    }

    #[test]
    fn sha256() {
        let sum = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";
        assert!(matches_sha256(format!("{sum}  pyo\n").as_bytes(), b"hello"));
        assert!(matches_sha256(sum.to_uppercase().as_bytes(), b"hello"));
        assert!(!matches_sha256(format!("{sum}  pyo").as_bytes(), b"hello!"));
        assert!(!matches_sha256(b"", b""));
    }
}
