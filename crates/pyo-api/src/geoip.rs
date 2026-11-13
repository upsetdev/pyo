//! The GeoIP city database analytics locates visitors and servers with:
//! MaxMind GeoLite2 City.
//!
//! The image comes with a copy (bundled at build time, as Umami does), so
//! locations work out of the box. With `MAXMIND_LICENSE_KEY` set, each web
//! process also downloads a fresh copy from MaxMind each week into its own
//! /geoip folder, checks it opens, swaps it in and reloads it. A Postgres
//! advisory lock lets one process download at a time. Until the first
//! download (or when downloads fail), the bundled copy is used.

use std::{
    io::Read,
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};

use crate::{AppState, bg};

pub const LOCK_GEOIP: i64 = 7_007;
/// How often the file is checked.
const TICK: Duration = Duration::from_secs(60);
const MAX_AGE_MS: i64 = 7 * 86_400_000;
/// After a failed download, wait this long before trying again.
const RETRY_MS: i64 = 3_600_000;

/// What's on disk, next to the database (GeoLite2-City.json).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Meta {
    /// When it was downloaded (ms).
    fetched_at: i64,
}

fn dir() -> PathBuf {
    std::env::var("PYO_GEOIP_DB")
        .ok()
        .filter(|p| !p.is_empty())
        .and_then(|p| Path::new(&p).parent().map(Path::to_path_buf))
        .unwrap_or_else(|| PathBuf::from("/geoip"))
}

fn db_path() -> PathBuf {
    dir().join("GeoLite2-City.mmdb")
}

const META: &str = "GeoLite2-City.json";

fn read_meta() -> Option<Meta> {
    serde_json::from_slice(&std::fs::read(dir().join(META)).ok()?).ok()
}

fn license_key() -> Option<String> {
    std::env::var("MAXMIND_LICENSE_KEY")
        .ok()
        .map(|k| k.trim().to_owned())
        .filter(|k| !k.is_empty())
}

/// Web processes: keep the database current, and reload it when it changes.
pub async fn keeper(s: AppState) {
    // Downloaded before (a key was set), else the bundled copy.
    if read_meta().is_none() || s.observability.load_geoip(&db_path()).is_err() {
        load_bundled(&s);
    }
    let Some(key) = license_key() else {
        return;
    };
    let mut loaded: Option<i64> = None;
    let mut failed_at: i64 = 0;
    loop {
        let stale = read_meta().is_none_or(|m| pyo_proto::now_ms() - m.fetched_at > MAX_AGE_MS);
        if stale && pyo_proto::now_ms() - failed_at > RETRY_MS {
            let done = bg::as_leader(&s.db, LOCK_GEOIP, || async {
                download(&key).await.and_then(|b| install(&b))
            })
            .await;
            if let Some(Err(e)) = done {
                failed_at = pyo_proto::now_ms();
                tracing::warn!(error = %e, "GeoIP download failed; keeping the current database");
            }
        }
        if let Some(m) = read_meta()
            && loaded != Some(m.fetched_at)
        {
            match s.observability.load_geoip(&db_path()) {
                Ok(()) => {
                    loaded = Some(m.fetched_at);
                    tracing::info!("GeoIP database loaded");
                }
                Err(e) => tracing::warn!(error = %e, "GeoIP database didn't load"),
            }
        }
        tokio::time::sleep(TICK).await;
    }
}

/// The copy built into the image.
fn bundled_path() -> PathBuf {
    std::env::var("PYO_GEOIP_BUNDLED")
        .ok()
        .filter(|p| !p.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/app/geoip/GeoLite2-City.mmdb"))
}

fn load_bundled(s: &AppState) {
    let p = bundled_path();
    match s.observability.load_geoip(&p) {
        Ok(()) => tracing::info!(path = %p.display(), "GeoIP: using the bundled GeoLite2 database"),
        Err(e) => {
            tracing::warn!(path = %p.display(), error = %e, "GeoIP: no database; locations will be unknown")
        }
    }
}

async fn download(key: &str) -> Result<Vec<u8>> {
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(300))
        .build()?;
    let res = http
        .get("https://download.maxmind.com/app/geoip_download")
        .query(&[
            ("edition_id", "GeoLite2-City"),
            ("license_key", key),
            ("suffix", "tar.gz"),
        ])
        .send()
        .await
        .context("Can't reach MaxMind")?;
    match res.status().as_u16() {
        200 => {}
        401 | 403 => bail!("MaxMind didn't accept MAXMIND_LICENSE_KEY."),
        429 => bail!("MaxMind's daily download limit was reached."),
        s => bail!("MaxMind answered {s}."),
    }
    let gz = res.bytes().await?;
    let mut tar = vec![];
    flate2::read::GzDecoder::new(&gz[..])
        .read_to_end(&mut tar)
        .context("MaxMind's file isn't a gzip archive")?;
    tar_file(&tar, ".mmdb").context("No .mmdb file in MaxMind's archive")
}

/// The first file in a tar archive whose name ends with `suffix`.
fn tar_file(tar: &[u8], suffix: &str) -> Option<Vec<u8>> {
    let mut at = 0;
    while at + 512 <= tar.len() {
        let h = &tar[at..at + 512];
        if h.iter().all(|&b| b == 0) {
            return None;
        }
        let name = String::from_utf8_lossy(&h[..100])
            .trim_end_matches('\0')
            .to_owned();
        let size_field = String::from_utf8_lossy(&h[124..136])
            .trim_matches(|c: char| c == '\0' || c == ' ')
            .to_owned();
        let size = usize::from_str_radix(&size_field, 8).ok()?;
        let body = at + 512;
        if name.ends_with(suffix) && h[156] != b'5' {
            return tar.get(body..body + size).map(<[u8]>::to_vec);
        }
        at = body + size.div_ceil(512) * 512;
    }
    None
}

/// Check the file opens, then swap it in with its meta.
fn install(bytes: &[u8]) -> Result<()> {
    maxminddb::Reader::from_source(bytes)
        .map_err(|e| anyhow!("The downloaded database doesn't open: {e}"))?;
    let d = dir();
    std::fs::create_dir_all(&d).with_context(|| format!("Can't write to {}", d.display()))?;
    let tmp = d.join("GeoLite2-City.mmdb.tmp");
    std::fs::write(&tmp, bytes).with_context(|| format!("Can't write to {}", d.display()))?;
    std::fs::rename(&tmp, db_path())?;
    let meta = Meta {
        fetched_at: pyo_proto::now_ms(),
    };
    let tmp = d.join("GeoLite2-City.json.tmp");
    std::fs::write(&tmp, serde_json::to_vec(&meta)?)?;
    std::fs::rename(&tmp, d.join(META))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::tar_file;

    fn entry(name: &str, body: &[u8]) -> Vec<u8> {
        let mut h = vec![0u8; 512];
        h[..name.len()].copy_from_slice(name.as_bytes());
        let size = format!("{:011o}\0", body.len());
        h[124..136].copy_from_slice(size.as_bytes());
        h[156] = b'0';
        let mut out = h;
        out.extend_from_slice(body);
        out.resize(out.len().div_ceil(512) * 512, 0);
        out
    }

    #[test]
    fn finds_the_database_in_a_tar() {
        let mut tar = entry("GeoLite2-City_20261028/COPYRIGHT.txt", b"(c) MaxMind");
        tar.extend(entry("GeoLite2-City_20261028/GeoLite2-City.mmdb", b"MMDB"));
        tar.extend(vec![0u8; 1024]);
        assert_eq!(tar_file(&tar, ".mmdb").as_deref(), Some(&b"MMDB"[..]));
        assert_eq!(tar_file(&tar, ".csv"), None);
    }
}
