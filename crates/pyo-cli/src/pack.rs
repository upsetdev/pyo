//! `pyo deploy` from a folder: pack it as a gzipped tar and send it to Pyo
//! in parts, which hands it to a builder in the service's cluster.
//!
//! What goes in: every file, dotfiles too, except what `.gitignore` (and
//! `.git/info/exclude`) and `.pyoignore` leave out, `.git`, and
//! `node_modules` (a `!node_modules/` line in `.pyoignore` keeps it). `.env`
//! files (`.env`, `.env.local`, `prod.env`) stay out unless asked for
//! (`.env.example` and the like go in).

use std::{
    collections::BTreeMap,
    io::{IsTerminal, Read, Seek, Write},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use crate::{api::Api, ui};

/// The most Pyo takes, packed.
pub const MAX_BYTES: u64 = 500_000_000;
/// What a folder leaves out unless its ignore files keep it.
const DEFAULT_IGNORES: &str = "node_modules/\n";

/// A file to pack: its path in the archive and on disk, and its size.
pub struct Entry {
    pub rel: PathBuf,
    pub abs: PathBuf,
    pub size: u64,
}

/// A `.env` file, kept out unless asked for. Examples and templates of one
/// (`.env.example`, `.env.sample`, …) aren't.
pub fn is_env_file(name: &str) -> bool {
    let env = name == ".env" || name.starts_with(".env.") || name.ends_with(".env");
    env && !matches!(
        name.rsplit('.').next().unwrap_or(""),
        "example" | "sample" | "template" | "dist" | "defaults"
    )
}

/// A walk over `root` that heeds the folder's own ignore files.
fn walker(root: &Path) -> ignore::WalkBuilder {
    let mut walk = ignore::WalkBuilder::new(root);
    walk.hidden(false)
        .ignore(false)
        .git_ignore(true)
        .git_exclude(true)
        .git_global(false)
        .require_git(false)
        .parents(true)
        .follow_links(false)
        .add_custom_ignore_filename(".pyoignore");
    walk
}

/// The files of `root` to pack, in order.
pub fn list(root: &Path, with_env: bool) -> Result<Vec<Entry>> {
    // The defaults rank below the folder's own ignore files: they leave a
    // path out only when those files say nothing about it, so a `!` line
    // there keeps it.
    let mut defaults = ignore::gitignore::GitignoreBuilder::new(root);
    for line in DEFAULT_IGNORES.lines() {
        defaults.add_line(None, line)?;
    }
    let defaults = defaults.build()?;
    let own = std::sync::Mutex::new(
        walker(root)
            .build_matchers()
            .pop()
            .context("Couldn't read the folder's ignore files")?,
    );
    let base = root.to_path_buf();
    let mut walk = walker(root);
    walk.sort_by_file_name(|a, b| a.cmp(b))
        .filter_entry(move |e| {
            let name = e.file_name().to_string_lossy();
            if name == ".git" {
                return false;
            }
            if !with_env && e.file_type().is_some_and(|t| !t.is_dir()) && is_env_file(&name) {
                return false;
            }
            let is_dir = e.file_type().is_some_and(|t| t.is_dir());
            if defaults.matched(e.path(), is_dir).is_ignore() {
                // The walk got here, so the folder's files don't leave it out.
                // Keep it only if they keep it on purpose.
                let rel = e.path().strip_prefix(&base).unwrap_or(e.path());
                return own
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .matched(rel, is_dir)
                    .is_whitelist();
            }
            true
        });
    let mut out = vec![];
    for e in walk.build() {
        let e = e?;
        let Some(t) = e.file_type() else { continue };
        if t.is_dir() {
            continue;
        }
        let rel = e
            .path()
            .strip_prefix(root)
            .unwrap_or(e.path())
            .to_path_buf();
        if rel.as_os_str().is_empty() {
            continue;
        }
        let size = if t.is_file() {
            e.metadata().map(|m| m.len()).unwrap_or(0)
        } else {
            0
        };
        out.push(Entry {
            rel,
            abs: e.path().to_path_buf(),
            size,
        });
    }
    Ok(out)
}

/// A packed folder, in a temporary file removed when it drops.
pub struct Packed {
    pub path: PathBuf,
    pub size: u64,
    pub sha256: String,
    pub files: usize,
}

impl Drop for Packed {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Counts and hashes what's written, and stops at `limit` bytes.
struct Counted<W> {
    inner: W,
    n: u64,
    limit: u64,
    hash: sha2::Sha256,
}

impl<W: Write> Write for Counted<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        use sha2::Digest;
        if self.n + buf.len() as u64 > self.limit {
            return Err(std::io::Error::other(TooBig));
        }
        let n = self.inner.write(buf)?;
        self.hash.update(&buf[..n]);
        self.n += n as u64;
        Ok(n)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

#[derive(Debug)]
struct TooBig;

impl std::fmt::Display for TooBig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("too big")
    }
}

impl std::error::Error for TooBig {}

/// "3.5 GB", "12.3 MB", "980 KB".
pub fn size_text(b: u64) -> String {
    if b >= 1_000_000_000 {
        format!("{:.1} GB", b as f64 / 1_000_000_000.0)
    } else if b >= 1_000_000 {
        format!("{:.1} MB", b as f64 / 1_000_000.0)
    } else {
        format!("{} KB", b.div_ceil(1000))
    }
}

/// The biggest folders and files at the top, for a folder that's too big.
fn biggest(files: &[Entry]) -> String {
    let mut by: BTreeMap<String, u64> = BTreeMap::new();
    for f in files {
        let top = f
            .rel
            .components()
            .next()
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .unwrap_or_default();
        *by.entry(top).or_default() += f.size;
    }
    let mut v: Vec<(String, u64)> = by.into_iter().collect();
    v.sort_by_key(|e| std::cmp::Reverse(e.1));
    v.iter()
        .take(3)
        .map(|(n, s)| format!("{n} {}", size_text(*s)))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Pack `files` of `root` into a temporary .tar.gz, at most `limit` bytes.
/// `tick` hears how many files are in so far.
pub fn pack(files: &[Entry], limit: u64, mut tick: impl FnMut(usize)) -> Result<Packed> {
    use sha2::Digest;
    let path = std::env::temp_dir().join(format!(
        "pyo-deploy-{}-{}.tar.gz",
        std::process::id(),
        crate::api::now_ms()
    ));
    let file = std::fs::File::create(&path).context("Couldn't write the archive")?;
    let mut packed = Packed {
        path,
        size: 0,
        sha256: String::new(),
        files: 0,
    };
    let counted = Counted {
        inner: std::io::BufWriter::new(file),
        n: 0,
        limit,
        hash: sha2::Sha256::new(),
    };
    let gz = flate2::write::GzEncoder::new(counted, flate2::Compression::default());
    let mut tar = tar::Builder::new(gz);
    // The same files give the same archive: no times, owners or modes
    // beyond "executable".
    tar.mode(tar::HeaderMode::Deterministic);
    tar.follow_symlinks(false);
    let too_big = |e: &std::io::Error| e.get_ref().is_some_and(|i| i.is::<TooBig>());
    let mut n = 0;
    for f in files {
        if let Err(e) = tar.append_path_with_name(&f.abs, &f.rel) {
            if too_big(&e) {
                bail!(
                    "The folder packs to more than {}, the most Pyo takes. Leave out more in .pyoignore. Biggest: {}.",
                    size_text(limit),
                    biggest(files)
                );
            }
            return Err(e).with_context(|| format!("Couldn't pack {}", f.rel.display()));
        }
        n += 1;
        tick(n);
    }
    let gz = tar.into_inner().map_err(|e| {
        if too_big(&e) {
            anyhow::anyhow!(
                "The folder packs to more than {}, the most Pyo takes. Leave out more in .pyoignore.",
                size_text(limit)
            )
        } else {
            anyhow::Error::from(e)
        }
    })?;
    let mut counted = gz.finish()?;
    counted.flush()?;
    packed.size = counted.n;
    packed.sha256 = hex::encode(counted.hash.finalize());
    packed.files = n;
    Ok(packed)
}

/// One line that rewrites itself on a terminal, at most every 100 ms.
/// Elsewhere (a CI log, say) a line of its own, at most every 30 seconds.
pub(crate) struct Progress {
    tty: bool,
    last: Option<Instant>,
}

impl Progress {
    pub(crate) fn new() -> Self {
        Self {
            tty: std::io::stderr().is_terminal(),
            last: None,
        }
    }

    pub(crate) fn show(&mut self, text: &str, force: bool) {
        let every = if self.tty {
            Duration::from_millis(100)
        } else {
            Duration::from_secs(30)
        };
        if !force && self.last.is_some_and(|t| t.elapsed() < every) {
            return;
        }
        self.last = Some(Instant::now());
        if self.tty {
            eprint!("\r\x1b[2K{}", ui::dim(text));
            let _ = std::io::stderr().flush();
        } else {
            eprintln!("{text}");
        }
    }

    pub(crate) fn done(&mut self) {
        if self.tty && self.last.is_some() {
            eprint!("\r\x1b[2K");
            let _ = std::io::stderr().flush();
        }
    }
}

/// Pack `root` and upload it for `team_id`: the upload's id, ready to deploy.
pub async fn upload(api: &Api, team_id: &str, root: &Path, with_env: bool) -> Result<String> {
    let mut progress = Progress::new();
    progress.show("Packing…", true);
    let dir = root.to_path_buf();
    let (files, packed) = tokio::task::spawn_blocking(move || -> Result<(usize, Packed)> {
        let files = list(&dir, with_env)?;
        if files.is_empty() {
            bail!("There's nothing to deploy in {}.", dir.display());
        }
        let total = files.len();
        let mut p = Progress::new();
        let packed = pack(&files, MAX_BYTES, |n| {
            p.show(&format!("Packing {n} of {total} files…"), false)
        });
        p.done();
        Ok((total, packed?))
    })
    .await??;
    progress.done();
    eprintln!(
        "{} Packed {files} file{}, {}",
        ui::green("✓"),
        if files == 1 { "" } else { "s" },
        size_text(packed.size)
    );

    let folder = root
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let started = api
        .post(
            "/api/v1/source-uploads",
            &json!({
                "teamId": team_id,
                "size": packed.size,
                "sha256": packed.sha256,
                "files": packed.files,
                "folder": folder,
                "machine": crate::login::machine_name(),
            }),
        )
        .await?;
    let id = started["uploadId"]
        .as_str()
        .context("Pyo gave no upload id")?
        .to_owned();
    let part_size = started["partSize"].as_u64().unwrap_or(8 << 20).max(1);
    let parts = packed.size.div_ceil(part_size);

    let mut file = std::fs::File::open(&packed.path)?;
    let mut sent = 0u64;
    let mut send_parts = |todo: Vec<u64>| {
        let mut out = vec![];
        for n in todo {
            let len = part_size.min(packed.size - n * part_size);
            let mut buf = vec![0u8; len as usize];
            file.seek(std::io::SeekFrom::Start(n * part_size))?;
            file.read_exact(&mut buf)?;
            out.push((n, buf));
        }
        Ok::<_, std::io::Error>(out)
    };
    // In batches, so a big folder isn't read into memory at once.
    let mut todo: Vec<u64> = (0..parts).collect();
    for round in 0..3 {
        for chunk in todo.clone().chunks(4) {
            for (n, buf) in send_parts(chunk.to_vec())? {
                let len = buf.len() as u64;
                let path = format!("/api/v1/source-uploads/{id}/parts/{n}");
                let mut tries = 0;
                loop {
                    match api.put_bytes(&path, buf.clone()).await {
                        Ok(_) => break,
                        Err(e) if tries < 4 => {
                            tries += 1;
                            progress.show(&format!("Retrying a part: {e}"), true);
                            tokio::time::sleep(Duration::from_secs(1 << tries)).await;
                        }
                        Err(_) if round < 2 => break,
                        Err(e) => {
                            progress.done();
                            return Err(e.context("The upload broke off. Try again."));
                        }
                    }
                }
                sent += len;
                progress.show(
                    &format!(
                        "Uploading {} of {} ({}%)",
                        size_text(sent.min(packed.size)),
                        size_text(packed.size),
                        sent.min(packed.size) * 100 / packed.size.max(1)
                    ),
                    false,
                );
            }
        }
        // What Pyo has: send the rest again.
        let st = api.get(&format!("/api/v1/source-uploads/{id}")).await?;
        let have: Vec<u64> = st["received"]
            .as_array()
            .map(|a| a.iter().filter_map(Value::as_u64).collect())
            .unwrap_or_default();
        todo = (0..parts).filter(|n| !have.contains(n)).collect();
        if todo.is_empty() {
            break;
        }
    }
    progress.done();
    if !todo.is_empty() {
        bail!("The upload broke off. Try again.");
    }
    api.post(&format!("/api/v1/source-uploads/{id}/complete"), &json!({}))
        .await?;
    eprintln!("{} Uploaded {}", ui::green("✓"), size_text(packed.size));
    Ok(id)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Dir(PathBuf);
    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn folder(files: &[(&str, &str)]) -> Dir {
        let d = std::env::temp_dir().join(format!(
            "pyo-pack-test-{}-{}",
            std::process::id(),
            crate::api::new_id("t")
        ));
        for (p, body) in files {
            let p = d.join(p);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, body).unwrap();
        }
        Dir(d)
    }

    fn names(root: &Path, with_env: bool) -> Vec<String> {
        list(root, with_env)
            .unwrap()
            .iter()
            .map(|e| e.rel.to_string_lossy().replace('\\', "/"))
            .collect()
    }

    #[test]
    fn env_files_are_secrets_but_examples_arent() {
        for n in [".env", ".env.local", ".env.production", "prod.env", "a.env"] {
            assert!(is_env_file(n), "{n}");
        }
        for n in [
            ".env.example",
            ".env.local.sample",
            ".env.template",
            ".envrc",
            "env",
            "env.ts",
        ] {
            assert!(!is_env_file(n), "{n}");
        }
    }

    #[test]
    fn ignore_files_and_defaults() {
        let d = folder(&[
            ("Dockerfile", "FROM scratch"),
            (".gitignore", "dist/\n*.log\n"),
            (".pyoignore", "secret.txt\n"),
            (".git/HEAD", "ref"),
            (".env", "A=1"),
            (".env.example", "A="),
            ("app.log", "x"),
            ("dist/out.js", "x"),
            ("node_modules/a/index.js", "x"),
            ("secret.txt", "x"),
            ("src/.hidden", "x"),
            ("src/main.rs", "x"),
        ]);
        assert_eq!(
            names(&d.0, false),
            [
                ".env.example",
                ".gitignore",
                ".pyoignore",
                "Dockerfile",
                "src/.hidden",
                "src/main.rs"
            ]
        );
        assert!(names(&d.0, true).contains(&".env".to_owned()));
    }

    #[test]
    fn pyoignore_can_keep_node_modules() {
        let d = folder(&[
            (".pyoignore", "!node_modules/\n"),
            ("node_modules/a/index.js", "x"),
            ("index.js", "x"),
        ]);
        assert!(names(&d.0, false).contains(&"node_modules/a/index.js".to_owned()));
    }

    #[test]
    fn defaults_rank_below_the_folders_ignore_files() {
        // A `.gitignore` can keep node_modules too, and the default reaches
        // nested ones.
        let d = folder(&[
            (".gitignore", "!node_modules/\n"),
            ("node_modules/a.js", "x"),
            ("web/node_modules/b.js", "x"),
        ]);
        let got = names(&d.0, false);
        assert!(got.contains(&"node_modules/a.js".to_owned()), "{got:?}");
        assert!(got.contains(&"web/node_modules/b.js".to_owned()), "{got:?}");
        let d = folder(&[
            ("web/node_modules/b.js", "x"),
            ("web/index.js", "x"),
            ("node_modules", "a file, not a folder"),
        ]);
        assert_eq!(names(&d.0, false), ["node_modules", "web/index.js"]);
        // `.pyoignore` wins over `.gitignore`.
        let d = folder(&[
            (".gitignore", "!node_modules/\n"),
            (".pyoignore", "node_modules/\n"),
            ("node_modules/a.js", "x"),
        ]);
        assert!(!names(&d.0, false).contains(&"node_modules/a.js".to_owned()));
    }

    #[test]
    fn lists_from_many_threads_at_once() {
        let d = folder(&[
            (".pyoignore", "secret.txt\n"),
            ("index.js", "x"),
            ("secret.txt", "x"),
            ("node_modules/a/index.js", "x"),
        ]);
        let root = d.0.clone();
        std::thread::scope(|s| {
            let runs: Vec<_> = (0..8)
                .map(|_| {
                    s.spawn(|| {
                        for _ in 0..50 {
                            assert_eq!(names(&root, false), [".pyoignore", "index.js"]);
                        }
                    })
                })
                .collect();
            for r in runs {
                r.join().unwrap();
            }
        });
    }

    #[test]
    fn packs_the_same_files_the_same_way() {
        let d = folder(&[("a.txt", "hello"), ("b/c.txt", "world")]);
        let files = list(&d.0, false).unwrap();
        let mut ticks = 0;
        let one = pack(&files, MAX_BYTES, |_| ticks += 1).unwrap();
        assert_eq!((one.files, ticks), (2, 2));
        let two = pack(&files, MAX_BYTES, |_| {}).unwrap();
        assert_eq!(one.sha256, two.sha256);
        assert_eq!(std::fs::metadata(&one.path).unwrap().len(), one.size);
        // It unpacks to the same files.
        let gz = flate2::read::GzDecoder::new(std::fs::File::open(&one.path).unwrap());
        let mut tar = tar::Archive::new(gz);
        let mut got: Vec<(String, String)> = tar
            .entries()
            .unwrap()
            .map(|e| {
                let mut e = e.unwrap();
                let mut s = String::new();
                e.read_to_string(&mut s).unwrap();
                (e.path().unwrap().to_string_lossy().into_owned(), s)
            })
            .collect();
        got.sort();
        assert_eq!(
            got,
            [
                ("a.txt".into(), "hello".into()),
                ("b/c.txt".into(), "world".into())
            ]
        );
        let path = one.path.clone();
        drop(one);
        assert!(!path.exists(), "the archive is removed");
    }

    #[test]
    fn a_folder_over_the_limit_says_what_is_big() {
        let mut noise = vec![0u8; 100_000];
        rand::fill(&mut noise[..]);
        let big = hex::encode(noise);
        let d = folder(&[("data/blob.txt", &big), ("small.txt", "x")]);
        let files = list(&d.0, false).unwrap();
        let e = pack(&files, 1000, |_| {}).err().unwrap().to_string();
        assert!(e.contains("more than 1 KB"), "{e}");
        assert!(e.contains("Biggest: data"), "{e}");
    }

    #[test]
    fn sizes_in_words() {
        assert_eq!(size_text(12_345_678), "12.3 MB");
        assert_eq!(size_text(3_456_789_012), "3.5 GB");
        assert_eq!(size_text(980_001), "981 KB");
        assert_eq!(size_text(1), "1 KB");
    }
}
