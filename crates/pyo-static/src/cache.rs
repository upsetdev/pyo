//! The folders this server has served, on the pod's disk:
//!
//! - `manifests/<site>/<deploy>.json`: a deploy's manifest, as Pyo has it;
//! - `blobs/<sha[..2]>/<sha>`: each file, by its SHA-256 (checked when it
//!   arrives), shared by every deploy that has it.
//!
//! Deploys in use are also kept in memory: their manifest by path, and
//! their `_headers` and `_redirects` rules, parsed once. What's on disk is
//! used whenever Pyo can't be reached, so a storage hiccup doesn't take a
//! site down. The keeper drops deploys not used for a week, and the
//! oldest ones when the cache grows past its size.

use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicI64, Ordering},
    },
    time::Duration,
};

use anyhow::{Context, anyhow, bail};
use futures::StreamExt;
use pyo_proto::site::{self, HeaderRule, Redirect, SiteFile, SiteManifest};
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;

/// The most the cache holds (its emptyDir allows 2 GiB).
const MAX_BYTES: u64 = 1536 * 1024 * 1024;
/// Deploys not used this long leave the cache.
const UNUSED_SECS: i64 = 7 * 24 * 3600;
/// Files fetched at once while warming a deploy.
const PARALLEL: usize = 8;
/// Names downloads apart.
static DOWNLOADS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// A deploy, ready to serve.
pub struct Deploy {
    pub site: String,
    pub deploy: String,
    pub files: HashMap<String, SiteFile>,
    pub index_fallback: bool,
    pub headers: Vec<HeaderRule>,
    pub redirects: Vec<Redirect>,
    last_used: AtomicI64,
}

impl Deploy {
    #[cfg(test)]
    pub fn for_test(files: Vec<SiteFile>, index_fallback: bool, redirects: Vec<Redirect>) -> Self {
        Self {
            site: "svc_1".into(),
            deploy: "dpl_1".into(),
            files: files.into_iter().map(|f| (f.path.clone(), f)).collect(),
            index_fallback,
            headers: vec![],
            redirects,
            last_used: AtomicI64::new(0),
        }
    }

    /// A file of the folder visitors may get (not `_headers` or
    /// `_redirects`).
    pub fn has(&self, path: &str) -> bool {
        path != "_headers" && path != "_redirects" && self.files.contains_key(path)
    }
}

pub struct Cache {
    dir: PathBuf,
    login: PathBuf,
    http: reqwest::Client,
    deploys: Mutex<HashMap<String, Arc<Deploy>>>,
    /// One load per deploy at a time.
    loading: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// An id as Pyo makes them: letters, digits, `_` and `-`.
pub fn valid_id(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-')
}

/// A path's segments, percent-encoded for a URL.
fn encode_path(p: &str) -> String {
    p.split('/')
        .map(|seg| {
            seg.bytes()
                .map(|b| match b {
                    b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                        (b as char).to_string()
                    }
                    _ => format!("%{b:02X}"),
                })
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("/")
}

impl Cache {
    pub fn open(dir: PathBuf, login: PathBuf) -> anyhow::Result<Self> {
        for d in ["manifests", "blobs", "tmp"] {
            std::fs::create_dir_all(dir.join(d))
                .with_context(|| format!("couldn't make {}", dir.join(d).display()))?;
        }
        // Downloads a restart cut short.
        if let Ok(rd) = std::fs::read_dir(dir.join("tmp")) {
            for e in rd.flatten() {
                let _ = std::fs::remove_file(e.path());
            }
        }
        Ok(Self {
            dir,
            login,
            http: reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(10))
                .build()?,
            deploys: Mutex::new(HashMap::new()),
            loading: Mutex::new(HashMap::new()),
        })
    }

    /// Pyo's address and the token, from the login Secret (re-read each
    /// time: it can change while the server runs).
    fn login(&self) -> anyhow::Result<(String, String)> {
        let read = |k: &str| {
            std::fs::read_to_string(self.login.join(k))
                .map(|s| s.trim().to_owned())
                .ok()
                .filter(|s| !s.is_empty())
        };
        match (read("url"), read("token")) {
            (Some(u), Some(t)) => Ok((u.trim_end_matches('/').to_owned(), t)),
            _ => bail!("no login to Pyo yet (the pyo-static-login Secret is empty)"),
        }
    }

    fn manifest_path(&self, site: &str, deploy: &str) -> PathBuf {
        self.dir
            .join("manifests")
            .join(site)
            .join(format!("{deploy}.json"))
    }

    pub fn blob_path(&self, sha: &str) -> PathBuf {
        self.dir.join("blobs").join(&sha[..2]).join(sha)
    }

    async fn get(&self, path: &str) -> anyhow::Result<reqwest::Response> {
        let (url, token) = self.login()?;
        let res = self
            .http
            .get(format!("{url}/api/static/v1/{path}"))
            .bearer_auth(token)
            .timeout(Duration::from_secs(300))
            .send()
            .await?;
        if !res.status().is_success() {
            bail!("Pyo answered {}", res.status());
        }
        Ok(res)
    }

    /// The deploy's manifest: on disk, else from Pyo (then kept on disk).
    async fn manifest(&self, site: &str, deploy: &str) -> anyhow::Result<SiteManifest> {
        let path = self.manifest_path(site, deploy);
        if let Ok(text) = tokio::fs::read(&path).await
            && let Ok(m) = serde_json::from_slice(&text)
        {
            return Ok(m);
        }
        let m: SiteManifest = self
            .get(&format!("sites/{site}/deploys/{deploy}/manifest"))
            .await
            .context("couldn't fetch the deploy's manifest")?
            .json()
            .await?;
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let tmp = self.dir.join("tmp").join(format!("{site}-{deploy}.json"));
        tokio::fs::write(&tmp, serde_json::to_vec(&m)?).await?;
        tokio::fs::rename(&tmp, &path).await?;
        Ok(m)
    }

    /// A file of a deploy, on disk: from the cache, else fetched (and
    /// checked against its SHA-256).
    pub async fn blob(&self, d: &Deploy, f: &SiteFile) -> anyhow::Result<PathBuf> {
        let path = self.blob_path(&f.sha256);
        if tokio::fs::metadata(&path)
            .await
            .is_ok_and(|m| m.len() == f.size)
        {
            return Ok(path);
        }
        let res = self
            .get(&format!(
                "sites/{}/deploys/{}/files/{}",
                d.site,
                d.deploy,
                encode_path(&f.path)
            ))
            .await
            .with_context(|| format!("couldn't fetch {}", f.path))?;
        let tmp = self.dir.join("tmp").join(format!(
            "{}.{}",
            f.sha256,
            DOWNLOADS.fetch_add(1, Ordering::Relaxed)
        ));
        let mut out = tokio::fs::File::create(&tmp).await?;
        let mut hash = Sha256::new();
        let mut got = 0u64;
        let mut body = res.bytes_stream();
        let written: anyhow::Result<()> = async {
            while let Some(chunk) = body.next().await {
                let chunk = chunk?;
                got += chunk.len() as u64;
                if got > f.size {
                    bail!("{} is larger than its manifest says", f.path);
                }
                hash.update(&chunk);
                out.write_all(&chunk).await?;
            }
            out.flush().await?;
            Ok(())
        }
        .await;
        drop(out);
        let ok = written.and_then(|()| {
            if got != f.size || hex::encode(hash.finalize()) != f.sha256 {
                Err(anyhow!("{} doesn't match its manifest", f.path))
            } else {
                Ok(())
            }
        });
        if let Err(e) = ok {
            let _ = tokio::fs::remove_file(&tmp).await;
            return Err(e);
        }
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        tokio::fs::rename(&tmp, &path).await?;
        Ok(path)
    }

    /// A deploy, ready to serve: from memory, else loaded (its manifest
    /// and its rules).
    pub async fn deploy(&self, site: &str, deploy: &str) -> anyhow::Result<Arc<Deploy>> {
        let key = format!("{site}/{deploy}");
        if let Some(d) = self.deploys.lock().unwrap().get(&key).cloned() {
            d.last_used.store(now(), Ordering::Relaxed);
            return Ok(d);
        }
        let lock = self
            .loading
            .lock()
            .unwrap()
            .entry(key.clone())
            .or_default()
            .clone();
        let _held = lock.lock().await;
        if let Some(d) = self.deploys.lock().unwrap().get(&key).cloned() {
            return Ok(d);
        }
        let m = self.manifest(site, deploy).await?;
        let mut d = Deploy {
            site: site.to_owned(),
            deploy: deploy.to_owned(),
            files: m
                .files
                .into_iter()
                .filter(|f| site::valid_path(&f.path) && f.sha256.len() == 64)
                .map(|f| (f.path.clone(), f))
                .collect(),
            index_fallback: m.index_fallback,
            headers: vec![],
            redirects: vec![],
            last_used: AtomicI64::new(now()),
        };
        if let Some(text) = self.rules_file(&d, "_headers").await {
            d.headers = site::parse_headers(&text).rules;
        }
        if let Some(text) = self.rules_file(&d, "_redirects").await {
            d.redirects = site::parse_redirects(&text).rules;
        }
        // Kept recent on disk: the keeper goes by when it was last loaded.
        let _ = touch(&self.manifest_path(site, deploy));
        let d = Arc::new(d);
        self.deploys.lock().unwrap().insert(key.clone(), d.clone());
        self.loading.lock().unwrap().remove(&key);
        Ok(d)
    }

    async fn rules_file(&self, d: &Deploy, name: &str) -> Option<String> {
        let f = d.files.get(name)?;
        match self.blob(d, f).await {
            Ok(p) => tokio::fs::read_to_string(p).await.ok(),
            Err(e) => {
                tracing::warn!(site = %d.site, deploy = %d.deploy, error = %format!("{e:#}"), "couldn't read {name}");
                None
            }
        }
    }

    /// Every file of a deploy on disk (the agent asks before a deploy
    /// moves the site here). The files fetched come back.
    pub async fn warm(&self, site: &str, deploy: &str) -> anyhow::Result<(usize, usize)> {
        let d = self.deploy(site, deploy).await?;
        let missing: Vec<SiteFile> = d
            .files
            .values()
            .filter(|f| {
                std::fs::metadata(self.blob_path(&f.sha256)).map_or(true, |m| m.len() != f.size)
            })
            .cloned()
            .collect();
        let fetched = missing.len();
        let mut results = futures::stream::iter(missing)
            .map(|f| {
                let d = d.clone();
                async move { self.blob(&d, &f).await }
            })
            .buffer_unordered(PARALLEL);
        while let Some(r) = results.next().await {
            r?;
        }
        Ok((d.files.len(), fetched))
    }

    /// Hourly: refreshes what's in use, then drops deploys not used for a
    /// week, and the oldest while the cache is over its size; then files
    /// no deploy left has.
    pub async fn keeper(cache: Arc<Self>) {
        let mut tick = tokio::time::interval(Duration::from_secs(3600));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            let c = cache.clone();
            match tokio::task::spawn_blocking(move || c.tidy()).await {
                Ok(Err(e)) => tracing::warn!(error = %format!("{e:#}"), "couldn't tidy the cache"),
                Err(e) => tracing::warn!(error = %e, "couldn't tidy the cache"),
                Ok(Ok(())) => {}
            }
        }
    }

    fn tidy(&self) -> anyhow::Result<()> {
        let cutoff = now() - 3600;
        let in_use: Vec<(String, String)> = {
            let mut deploys = self.deploys.lock().unwrap();
            // Out of memory once unused for an hour; still on disk.
            deploys.retain(|_, d| d.last_used.load(Ordering::Relaxed) > cutoff);
            deploys
                .values()
                .map(|d| (d.site.clone(), d.deploy.clone()))
                .collect()
        };
        for (site, deploy) in &in_use {
            let _ = touch(&self.manifest_path(site, deploy));
        }
        // Every manifest, newest first, with its files.
        let mut manifests: Vec<(i64, PathBuf, SiteManifest)> = vec![];
        for site in std::fs::read_dir(self.dir.join("manifests"))?.flatten() {
            for m in std::fs::read_dir(site.path())?.flatten() {
                let p = m.path();
                let age = mtime(&p);
                match std::fs::read(&p)
                    .ok()
                    .and_then(|t| serde_json::from_slice::<SiteManifest>(&t).ok())
                {
                    Some(sm) => manifests.push((age, p, sm)),
                    None => {
                        let _ = std::fs::remove_file(&p);
                    }
                }
            }
        }
        manifests.sort_by_key(|(t, ..)| std::cmp::Reverse(*t));
        let keep = pick(&manifests, now(), MAX_BYTES, cutoff);
        let mut wanted: HashSet<String> = HashSet::new();
        for (i, (_, p, m)) in manifests.iter().enumerate() {
            if keep.contains(&i) {
                wanted.extend(m.files.iter().map(|f| f.sha256.clone()));
            } else {
                let _ = std::fs::remove_file(p);
                tracing::info!(manifest = %p.display(), "dropped an unused deploy from the cache");
            }
        }
        for dir in std::fs::read_dir(self.dir.join("blobs"))?.flatten() {
            for b in std::fs::read_dir(dir.path())?.flatten() {
                if !wanted.contains(b.file_name().to_string_lossy().as_ref()) {
                    let _ = std::fs::remove_file(b.path());
                }
            }
        }
        Ok(())
    }
}

/// Which manifests (newest first) stay: those used within the week, while
/// the files they have fit in `max` bytes; one used in the last hour
/// (`recent`) always does.
fn pick(
    manifests: &[(i64, PathBuf, SiteManifest)],
    now: i64,
    max: u64,
    recent: i64,
) -> HashSet<usize> {
    let mut keep = HashSet::new();
    let mut seen: HashSet<&str> = HashSet::new();
    let mut total = 0u64;
    for (i, (t, _, m)) in manifests.iter().enumerate() {
        if *t < now - UNUSED_SECS {
            continue;
        }
        let extra: u64 = m
            .files
            .iter()
            .filter(|f| !seen.contains(f.sha256.as_str()))
            .map(|f| f.size)
            .sum();
        if total + extra > max && *t <= recent {
            continue;
        }
        total += extra;
        seen.extend(m.files.iter().map(|f| f.sha256.as_str()));
        keep.insert(i);
    }
    keep
}

fn mtime(p: &Path) -> i64 {
    std::fs::metadata(p)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn touch(p: &Path) -> std::io::Result<()> {
    std::fs::File::options()
        .append(true)
        .open(p)?
        .set_modified(std::time::SystemTime::now())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(files: &[(&str, u64)]) -> SiteManifest {
        SiteManifest {
            files: files
                .iter()
                .map(|(sha, size)| SiteFile {
                    path: format!("{sha}.html"),
                    size: *size,
                    sha256: (*sha).to_owned(),
                })
                .collect(),
            index_fallback: false,
        }
    }

    #[test]
    fn the_cache_keeps_recent_deploys_that_fit() {
        let now = 1_000_000_000;
        let list = vec![
            (now, PathBuf::from("a"), m(&[("x", 60), ("y", 30)])),
            // Shares x: only y2 counts.
            (now - 7200, PathBuf::from("b"), m(&[("x", 60), ("y2", 5)])),
            // Doesn't fit.
            (now - 8000, PathBuf::from("c"), m(&[("z", 50)])),
            (now - UNUSED_SECS - 1, PathBuf::from("d"), m(&[("w", 1)])),
        ];
        let keep = pick(&list, now, 100, now - 3600);
        assert_eq!(keep, HashSet::from([0, 1]));
        // One used in the last hour stays even over the size.
        let keep = pick(&list, now, 10, now - 3600);
        assert_eq!(keep, HashSet::from([0]));
    }

    #[test]
    fn ids_and_paths_are_safe_in_urls() {
        assert!(valid_id("svc_abc-1"));
        assert!(!valid_id("../x"));
        assert!(!valid_id(""));
        assert_eq!(encode_path("a b/c%d/é.html"), "a%20b/c%25d/%C3%A9.html");
    }

    #[tokio::test]
    async fn a_cached_deploy_serves_without_pyo() {
        let dir = std::env::temp_dir().join(format!("pyo-static-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let cache = Cache::open(dir.clone(), dir.join("no-login")).unwrap();
        let body = b"hello";
        let sha = hex::encode(Sha256::digest(body));
        let manifest = SiteManifest {
            files: vec![SiteFile {
                path: "index.html".into(),
                size: body.len() as u64,
                sha256: sha.clone(),
            }],
            index_fallback: true,
        };
        let mp = cache.manifest_path("svc_1", "dpl_1");
        std::fs::create_dir_all(mp.parent().unwrap()).unwrap();
        std::fs::write(&mp, serde_json::to_vec(&manifest).unwrap()).unwrap();
        let bp = cache.blob_path(&sha);
        std::fs::create_dir_all(bp.parent().unwrap()).unwrap();
        std::fs::write(&bp, body).unwrap();
        // No login: everything comes from the disk.
        let d = cache.deploy("svc_1", "dpl_1").await.unwrap();
        assert!(d.has("index.html") && d.index_fallback);
        assert_eq!(cache.warm("svc_1", "dpl_1").await.unwrap(), (1, 0));
        assert_eq!(cache.blob(&d, &d.files["index.html"]).await.unwrap(), bp);
        // A deploy that isn't cached needs Pyo.
        assert!(cache.deploy("svc_1", "dpl_2").await.is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
