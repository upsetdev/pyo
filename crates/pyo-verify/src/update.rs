//! The verifier replacing itself with the one from a signed release.
//!
//! The new binary is downloaded, held against the SHA-256 in the signed
//! release.json, written next to the current one, asked for its version,
//! and only then renamed over it. The binary it replaces stays as
//! `<name>.previous`.

use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::{
    fetch::{MAX_BINARY, Source},
    release::platform,
    sys::{Env, Problem},
    version::Version,
};

fn beside(exe: &Path, suffix: &str) -> PathBuf {
    let mut name = exe.file_name().unwrap_or_default().to_os_string();
    name.push(suffix);
    exe.with_file_name(name)
}

/// The binary this one replaced.
pub fn previous(exe: &Path) -> PathBuf {
    beside(exe, ".previous")
}

fn staged(exe: &Path) -> PathBuf {
    beside(exe, ".new")
}

/// Puts release `version`'s verifier in this binary's place. On any error
/// the current binary is untouched.
pub fn replace(env: &Env, source: &Source, version: &Version, sha256: &str) -> Result<(), Problem> {
    let file = format!("pyo-verify-{}", platform(env.arch));
    let bytes = source.fetch(env, version, &file, MAX_BINARY)?;
    let got = hex::encode(Sha256::digest(&bytes));
    if got != sha256 {
        return Err(Problem::with(
            format!(
                "The verifier of release {version} does not match its signed checksum. Nothing was changed."
            ),
            format!("sha256 {got}, signed {sha256}"),
        ));
    }
    let new = staged(&env.exe);
    let cannot_write = |e: std::io::Error| {
        Problem::with(
            format!(
                "The verifier of release {version} could not be written to this server. Nothing was changed."
            ),
            e.to_string(),
        )
    };
    write_new(&new, &bytes).map_err(cannot_write)?;
    // A binary that does not start here, or is another version than its
    // release says, never becomes the verifier.
    let new_str = new.to_string_lossy();
    let says = env
        .runner
        .run(&[&new_str, "version"], None)
        .ok()
        .filter(|o| o.ok)
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned());
    if says.as_deref() != Some(version.to_string().as_str()) {
        let _ = std::fs::remove_file(&new);
        return Err(Problem::with(
            format!(
                "The verifier of release {version} does not run on this server. Nothing was changed."
            ),
            format!("it answered {says:?}"),
        ));
    }
    let keep = previous(&env.exe);
    let _ = std::fs::remove_file(&keep);
    let kept =
        std::fs::hard_link(&env.exe, &keep).or_else(|_| std::fs::copy(&env.exe, &keep).map(|_| ()));
    if let Err(e) = kept.and_then(|()| std::fs::rename(&new, &env.exe)) {
        let _ = std::fs::remove_file(&new);
        return Err(cannot_write(e));
    }
    if let Some(Ok(dir)) = env.exe.parent().map(std::fs::File::open) {
        let _ = dir.sync_all();
    }
    Ok(())
}

fn write_new(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::{io::Write, os::unix::fs::OpenOptionsExt};
    let _ = std::fs::remove_file(path);
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o755)
        .open(path)?;
    f.write_all(bytes)?;
    f.sync_all()
}

/// Puts the previous binary back, after a new one that would not start.
pub fn roll_back(env: &Env) -> std::io::Result<()> {
    std::fs::rename(previous(&env.exe), &env.exe)
}
