//! Pyo Static: a static build's output folder, uploaded to be served as
//! static files (pyo-proto's site.rs, pyo-api's sites.rs).
//!
//! Railpack's static image serves its files with Caddy, so the build's
//! image, exported to a folder beside it, says where they are: its
//! Caddyfile's `root` (under `/app` when relative), and whether an
//! unknown path gets `index.html` (`try_files` to it: a single-page app).
//! That folder is what's uploaded: the files Caddy would serve, exactly.

use std::path::{Component, Path, PathBuf};

use anyhow::{Context, bail};
use futures::StreamExt;
use pyo_proto::site::{self, SiteFile, SiteManifest};
use sha2::{Digest, Sha256};

/// Where Railpack's image keeps its Caddyfile: `/Caddyfile` for a
/// single-page app, the app's folder for its static file provider.
const CADDYFILES: [&str; 2] = ["Caddyfile", "app/Caddyfile"];
/// Files sent at once.
const PARALLEL: usize = 6;

/// The served folder in the exported image, and whether unknown paths get
/// `index.html`.
pub fn served_root(export: &Path) -> anyhow::Result<(PathBuf, bool)> {
    let text = CADDYFILES
        .iter()
        .find_map(|c| std::fs::read_to_string(export.join(c)).ok())
        .context("the image has no Caddyfile")?;
    let (root, fallback) = parse_caddyfile(&text).context("its Caddyfile has no root")?;
    let rel = if let Some(abs) = root.strip_prefix('/') {
        PathBuf::from(abs)
    } else {
        Path::new("app").join(&root)
    };
    if rel
        .components()
        .any(|c| !matches!(c, Component::Normal(_) | Component::CurDir))
    {
        bail!("its Caddyfile's root ({root}) is outside the image");
    }
    let dir = export.join(rel);
    let real = dir.canonicalize().context("its root folder isn't there")?;
    if !real.starts_with(export.canonicalize()?) || !real.is_dir() {
        bail!("its Caddyfile's root ({root}) isn't a folder of the image");
    }
    Ok((real, fallback))
}

/// A Caddyfile's `root` (`root * <dir>` or `root <dir>`), and whether it
/// falls back to `index.html`.
pub fn parse_caddyfile(text: &str) -> Option<(String, bool)> {
    let mut root = None;
    let mut fallback = false;
    for line in text.lines() {
        let words: Vec<&str> = line.split_whitespace().collect();
        match words.as_slice() {
            ["root", "*", dir, ..] | ["root", dir] if root.is_none() => {
                root = Some(dir.trim_matches('"').to_owned());
            }
            ["try_files", rest @ ..]
                if rest
                    .iter()
                    .any(|w| w.trim_matches('"').ends_with("index.html")) =>
            {
                fallback = true;
            }
            _ => {}
        }
    }
    root.filter(|r| !r.is_empty() && !r.contains('{'))
        .map(|r| (r, fallback))
}

/// Every file under `root`, as (path in the folder, file on disk). A link
/// counts only when it leads to a file inside the folder.
pub fn files(root: &Path) -> anyhow::Result<Vec<(String, PathBuf)>> {
    let mut out = vec![];
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for e in std::fs::read_dir(&dir)? {
            let e = e?;
            let p = e.path();
            let meta = std::fs::symlink_metadata(&p)?;
            let (real, is_dir) = if meta.file_type().is_symlink() {
                match p.canonicalize() {
                    Ok(r) if r.starts_with(root) => {
                        let d = r.is_dir();
                        (r, d)
                    }
                    _ => continue,
                }
            } else {
                (p.clone(), meta.is_dir())
            };
            if is_dir {
                // A link to a folder inside it is already walked.
                if !meta.file_type().is_symlink() {
                    stack.push(p);
                }
                continue;
            }
            if !real.is_file() {
                continue;
            }
            let rel = p
                .strip_prefix(root)?
                .components()
                .map(|c| c.as_os_str().to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join("/");
            out.push((rel, real));
        }
    }
    out.sort();
    Ok(out)
}

fn sha256_file(p: &Path) -> std::io::Result<(String, u64)> {
    use std::io::Read;
    let mut f = std::fs::File::open(p)?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    let mut n = 0u64;
    loop {
        let r = f.read(&mut buf)?;
        if r == 0 {
            break;
        }
        h.update(&buf[..r]);
        n += r as u64;
    }
    Ok((hex::encode(h.finalize()), n))
}

/// The folder's manifest and where each file is on disk, checked against
/// what a static site may have. Warnings for its `_headers` and
/// `_redirects` come back too.
pub fn manifest(
    root: &Path,
    index_fallback: bool,
) -> anyhow::Result<(SiteManifest, Vec<PathBuf>, Vec<String>)> {
    let list = files(root)?;
    if list.is_empty() {
        bail!("the output folder is empty");
    }
    if list.len() > site::MAX_FILES {
        bail!(
            "the output folder has {} files, over the {} a static site may have",
            list.len(),
            site::MAX_FILES
        );
    }
    let mut m = SiteManifest {
        files: vec![],
        index_fallback,
    };
    let mut on_disk = vec![];
    let mut total = 0u64;
    let mut skipped = vec![];
    for (rel, real) in list {
        if !site::valid_path(&rel) {
            skipped.push(rel);
            continue;
        }
        let (sha256, size) = sha256_file(&real)?;
        if size > site::MAX_FILE_BYTES {
            bail!(
                "{rel} is over the {} MiB a file of a static site may have",
                site::MAX_FILE_BYTES / 1024 / 1024
            );
        }
        total += size;
        m.files.push(SiteFile {
            path: rel,
            size,
            sha256,
        });
        on_disk.push(real);
    }
    if total > site::MAX_BYTES {
        bail!(
            "the output folder has {} MiB, over the {} MiB a static site may have",
            total / 1024 / 1024,
            site::MAX_BYTES / 1024 / 1024
        );
    }
    let mut warnings: Vec<String> = skipped
        .into_iter()
        .map(|p| format!("Left out {p}: Pyo can't serve that name"))
        .collect();
    for (name, parse) in [
        (
            "_headers",
            (|t: &str| site::parse_headers(t).warnings) as fn(&str) -> Vec<String>,
        ),
        ("_redirects", |t: &str| site::parse_redirects(t).warnings),
    ] {
        if let Ok(text) = std::fs::read_to_string(root.join(name)) {
            warnings.extend(parse(&text));
        }
    }
    Ok((m, on_disk, warnings))
}

/// A path's segments, percent-encoded for a URL.
pub fn encode_path(p: &str) -> String {
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

async fn check(res: reqwest::Response) -> anyhow::Result<()> {
    if res.status().is_success() {
        return Ok(());
    }
    let status = res.status();
    let body: serde_json::Value = res.json().await.unwrap_or_default();
    match body["error"].as_str() {
        Some(e) => bail!("{e}"),
        None => bail!("Pyo answered {status}"),
    }
}

/// Uploads the folder through Pyo: its manifest, each file, then done.
pub async fn upload(
    http: &reqwest::Client,
    auth: &crate::Auth,
    base: &str,
    job: &str,
    m: &SiteManifest,
    on_disk: &[PathBuf],
) -> anyhow::Result<()> {
    let jobs = format!("{base}/jobs/{job}/site");
    let manifest = serde_json::to_vec(m)?;
    let req = http.post(&jobs).header("content-type", "application/json");
    check(auth.sign(req, "POST", &jobs, &manifest).body(manifest).send().await?).await?;
    let mut sends = futures::stream::iter(m.files.iter().zip(on_disk))
        .map(|(f, p)| {
            let url = format!("{jobs}/files/{}", encode_path(&f.path));
            async move {
                let mut tries = 0;
                loop {
                    tries += 1;
                    let file = tokio::fs::File::open(p).await?;
                    let body = reqwest::Body::wrap_stream(tokio_util::io::ReaderStream::new(file));
                    let r = async {
                        let req = http
                            .put(&url)
                            .header("x-pyo-sha256", &f.sha256)
                            .header("content-length", f.size);
                        check(
                            auth.sign_hashed(req, "PUT", &url, &f.sha256)
                                .body(body)
                                .timeout(std::time::Duration::from_secs(600))
                                .send()
                                .await?,
                        )
                        .await
                    }
                    .await;
                    match r {
                        Ok(()) => return anyhow::Ok(()),
                        Err(e) if tries >= 3 => {
                            return Err(e.context(format!("couldn't upload {}", f.path)));
                        }
                        Err(_) => tokio::time::sleep(std::time::Duration::from_secs(2)).await,
                    }
                }
            }
        })
        .buffer_unordered(PARALLEL);
    while let Some(r) = sends.next().await {
        r?;
    }
    let done = format!("{jobs}/done");
    check(auth.sign(http.post(&done), "POST", &done, b"").send().await?).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let d =
            std::env::temp_dir().join(format!("pyo-builder-site-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn railpacks_caddyfiles_say_where_the_files_are() {
        let spa = "{\n\tadmin off\n}\n:{$PORT:80} {\n\troot * /app/dist\n\tencode gzip\n\ttry_files {path} /index.html\n\tfile_server\n}\n";
        assert_eq!(parse_caddyfile(spa), Some(("/app/dist".into(), true)));
        let plain = ":80 {\n  root * public\n  file_server\n}\n";
        assert_eq!(parse_caddyfile(plain), Some(("public".into(), false)));
        assert_eq!(parse_caddyfile("root * {$ROOT}\n"), None);
        assert_eq!(parse_caddyfile(":80 {\n file_server\n}"), None);
    }

    #[test]
    fn the_served_folder_is_found_in_the_image() {
        let ex = tmp("export");
        std::fs::create_dir_all(ex.join("app/dist/a")).unwrap();
        std::fs::write(
            ex.join("Caddyfile"),
            "root * /app/dist\ntry_files {path} /index.html\n",
        )
        .unwrap();
        std::fs::write(ex.join("app/dist/index.html"), "<h1>hi</h1>").unwrap();
        std::fs::write(ex.join("app/dist/a/b.css"), "a{}").unwrap();
        std::fs::write(ex.join("app/dist/_redirects"), "/old /new\nbad\n").unwrap();
        std::fs::write(ex.join("secret.txt"), "outside").unwrap();
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(ex.join("secret.txt"), ex.join("app/dist/leak")).unwrap();
            std::os::unix::fs::symlink(
                ex.join("app/dist/index.html"),
                ex.join("app/dist/home.html"),
            )
            .unwrap();
        }
        let (root, fallback) = served_root(&ex).unwrap();
        assert!(fallback);
        let (m, on_disk, warnings) = manifest(&root, fallback).unwrap();
        let paths: Vec<&str> = m.files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(paths, ["_redirects", "a/b.css", "home.html", "index.html"]);
        assert_eq!(on_disk.len(), 4);
        assert_eq!(m.files[3].size, 11);
        assert_eq!(
            m.files[3].sha256,
            hex::encode(Sha256::digest(b"<h1>hi</h1>"))
        );
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        // A root outside the image isn't one.
        std::fs::write(ex.join("Caddyfile"), "root * ../../etc\n").unwrap();
        assert!(served_root(&ex).is_err());
        let _ = std::fs::remove_dir_all(&ex);
    }
}
