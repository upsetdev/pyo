//! Turns an image's layers into a template's root disk, without root.
//!
//! The layers are unpacked in order into a staging directory, with the OCI
//! whiteouts applied: `.wh.<name>` deletes `<name>` from the layers below,
//! and `.wh..wh..opq` hides everything the layers below put in its
//! directory. `mkfs.ext4 -d` then makes the disk from the directory.
//!
//! The staging copy can't carry what only root may set (owners, device
//! nodes, setuid bits), and the daemon makes it the same way as root or
//! not. So each entry's metadata is kept aside, and once the disk exists
//! `debugfs` sets owners and modes and makes device nodes and fifos on the
//! disk itself. Setuid bits never touch the host's disk.
//!
//! Images are untrusted. No path may leave the staging directory: `..` is
//! refused, and a symlink met on the way to an entry is resolved inside the
//! staging directory, as the guest would see it, never on the host.

use std::collections::{BTreeMap, HashSet, VecDeque};
use std::ffi::OsString;
use std::fs;
use std::io::{self, Read};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, UNIX_EPOCH};

use anyhow::{Context, bail};

const OPAQUE: &str = ".wh..wh..opq";
const WHITEOUT: &str = ".wh.";
/// Symlinks followed while resolving one path, like the kernel's limit.
const MAX_HOPS: usize = 40;
const BLOCK: u64 = 4096;

/// The guest's directories the template needs: the mount points envd uses
/// and `/mnt`, where the writable layer goes before the switch.
pub const GUEST_DIRS: &[&str] = &["proc", "sys", "dev", "tmp", "run", "mnt", "root"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    File,
    Dir,
    Symlink,
    Char(u32, u32),
    Block(u32, u32),
    Fifo,
}

/// What an entry should be on the disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Meta {
    pub kind: Kind,
    pub uid: u32,
    pub gid: u32,
    /// Permission bits with setuid, setgid and sticky.
    pub mode: u32,
}

impl Meta {
    fn dir() -> Meta {
        Meta {
            kind: Kind::Dir,
            uid: 0,
            gid: 0,
            mode: 0o755,
        }
    }
}

/// How a layer is compressed, from its media type.
pub fn decompress<'a>(media_type: &str, r: impl Read + 'a) -> anyhow::Result<Box<dyn Read + 'a>> {
    Ok(
        if media_type.ends_with("+gzip") || media_type.ends_with(".gzip") {
            Box::new(flate2::read::MultiGzDecoder::new(r))
        } else if media_type.ends_with("+zstd") || media_type.ends_with(".zstd") {
            Box::new(
                ruzstd::decoding::StreamingDecoder::new(r)
                    .map_err(|e| anyhow::anyhow!("reading a zstd layer: {e}"))?,
            )
        } else if media_type.ends_with(".tar") || media_type.ends_with("layer.v1.tar") {
            Box::new(r)
        } else {
            bail!("Layers of type {media_type} aren't supported.")
        },
    )
}

pub struct Unpacker {
    root: PathBuf,
    /// Wanted metadata by path inside the root ("" is the root itself).
    meta: BTreeMap<PathBuf, Meta>,
    /// Paths the current layer made, which its opaque markers keep.
    layer: HashSet<PathBuf>,
    /// Directory times, set once everything is in place.
    dir_times: BTreeMap<PathBuf, u64>,
    max_bytes: u64,
    bytes: u64,
    cancel: Arc<AtomicBool>,
}

impl Unpacker {
    /// `root` is an empty directory. Files may take `max_bytes` in all.
    pub fn new(root: &Path, max_bytes: u64, cancel: Arc<AtomicBool>) -> anyhow::Result<Unpacker> {
        fs::create_dir_all(root)?;
        fs::set_permissions(root, fs::Permissions::from_mode(0o700))?;
        let mut meta = BTreeMap::new();
        meta.insert(PathBuf::new(), Meta::dir());
        Ok(Unpacker {
            root: root.to_path_buf(),
            meta,
            layer: HashSet::new(),
            dir_times: BTreeMap::new(),
            max_bytes,
            bytes: 0,
            cancel,
        })
    }

    pub fn meta(&self, rel: &str) -> Option<Meta> {
        self.meta.get(Path::new(rel)).copied()
    }

    /// Bytes of file content unpacked so far.
    pub fn bytes(&self) -> u64 {
        self.bytes
    }

    fn check_cancel(&self) -> anyhow::Result<()> {
        if self.cancel.load(Ordering::Relaxed) {
            bail!("The template build was stopped.");
        }
        Ok(())
    }

    /// Applies one layer, an uncompressed tar stream, over the ones before.
    pub fn apply(&mut self, tar: impl Read) -> anyhow::Result<()> {
        self.layer.clear();
        let mut archive = tar::Archive::new(tar);
        for entry in archive.entries().context("reading a layer")? {
            self.check_cancel()?;
            let mut entry = entry.context("reading a layer entry")?;
            let path = entry.path().context("an entry's path")?.into_owned();
            self.entry(&path, &mut entry)
                .with_context(|| format!("unpacking /{}", path.display()))?;
        }
        Ok(())
    }

    fn entry<R: Read>(&mut self, path: &Path, entry: &mut tar::Entry<R>) -> anyhow::Result<()> {
        let comps = clean(path)?;
        let h = entry.header();
        let et = h.entry_type();
        let mut m = Meta {
            kind: Kind::File,
            uid: h.uid().unwrap_or(0) as u32,
            gid: h.gid().unwrap_or(0) as u32,
            mode: h.mode().unwrap_or(0o644) & 0o7777,
        };
        let mtime = h.mtime().unwrap_or(0);
        let Some((name, parents)) = comps.split_last() else {
            // The root itself.
            if et.is_dir() {
                m.kind = Kind::Dir;
                self.meta.insert(PathBuf::new(), m);
            }
            return Ok(());
        };
        if name == OPAQUE {
            let dir = self.resolve_dir(parents, true)?.expect("created");
            self.clear_below(&dir)?;
            return Ok(());
        }
        if let Some(hidden) = name.to_str().and_then(|n| n.strip_prefix(WHITEOUT)) {
            if hidden.is_empty() || hidden == "." || hidden == ".." {
                bail!("a bad whiteout");
            }
            if let Some(dir) = self.resolve_dir(parents, false)? {
                self.remove(&dir.join(hidden))?;
            }
            return Ok(());
        }
        let dir = self.resolve_dir(parents, true)?.expect("created");
        let rel = dir.join(name);
        let full = self.root.join(&rel);
        let existing = fs::symlink_metadata(&full).ok();
        if et.is_dir() {
            m.kind = Kind::Dir;
            match existing {
                Some(e) if e.is_dir() => {}
                Some(_) => {
                    self.remove(&rel)?;
                    mkdir(&full)?;
                }
                None => mkdir(&full)?,
            }
            self.dir_times.insert(rel.clone(), mtime);
        } else if et.is_file() || et == tar::EntryType::Continuous || et.is_gnu_sparse() {
            let size = h.size().unwrap_or(0);
            if self.bytes + size > self.max_bytes {
                bail!(
                    "The image is bigger than the {} MiB a template may take.",
                    self.max_bytes >> 20
                );
            }
            if existing.is_some() {
                self.remove(&rel)?;
            }
            let mut f = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&full)?;
            let n = io::copy(&mut entry.by_ref().take(size), &mut f)?;
            self.bytes += n;
            f.set_permissions(fs::Permissions::from_mode((m.mode & 0o777) | 0o400))?;
            let _ = f.set_modified(UNIX_EPOCH + Duration::from_secs(mtime));
        } else if et.is_symlink() {
            m.kind = Kind::Symlink;
            let target = entry
                .link_name()?
                .context("a symlink without a target")?
                .into_owned();
            if existing.is_some() {
                self.remove(&rel)?;
            }
            std::os::unix::fs::symlink(&target, &full)?;
        } else if et.is_hard_link() {
            let target = entry
                .link_name()?
                .context("a hard link without a target")?
                .into_owned();
            let tcomps = clean(&target)?;
            let Some((tname, tparents)) = tcomps.split_last() else {
                bail!("a hard link to the root");
            };
            let tdir = self
                .resolve_dir(tparents, false)?
                .with_context(|| format!("the link target /{} isn't there", target.display()))?;
            let trel = tdir.join(tname);
            if trel == rel {
                return Ok(());
            }
            let tm = self.meta.get(&trel).copied();
            match tm.map(|t| t.kind) {
                Some(Kind::Char(..) | Kind::Block(..) | Kind::Fifo) => {
                    if existing.is_some() {
                        self.remove(&rel)?;
                    }
                    m = tm.expect("matched");
                }
                _ => {
                    let tfull = self.root.join(&trel);
                    let tmeta = fs::symlink_metadata(&tfull).with_context(|| {
                        format!("the link target /{} isn't there", target.display())
                    })?;
                    if tmeta.is_dir() {
                        bail!("a hard link to a directory");
                    }
                    if existing.is_some() {
                        self.remove(&rel)?;
                    }
                    fs::hard_link(&tfull, &full)?;
                    m = tm.unwrap_or(m);
                }
            }
        } else if et.is_character_special() || et.is_block_special() || et.is_fifo() {
            let (maj, min) = (
                h.device_major().ok().flatten().unwrap_or(0),
                h.device_minor().ok().flatten().unwrap_or(0),
            );
            m.kind = if et.is_character_special() {
                Kind::Char(maj, min)
            } else if et.is_block_special() {
                Kind::Block(maj, min)
            } else {
                Kind::Fifo
            };
            if existing.is_some() {
                self.remove(&rel)?;
            }
        } else {
            // Pax and long-name headers are read by the tar crate. Anything
            // else (sockets, volume labels) has no place on the disk.
            return Ok(());
        }
        self.meta.insert(rel.clone(), m);
        self.layer.insert(rel);
        Ok(())
    }

    /// The real directory, inside the root, that `comps` names, following
    /// symlinks as the guest would. Missing directories are made when
    /// `create` is set, else it's None.
    fn resolve_dir(&mut self, comps: &[OsString], create: bool) -> anyhow::Result<Option<PathBuf>> {
        let mut cur: Vec<OsString> = Vec::new();
        let mut todo: VecDeque<OsString> = comps.iter().cloned().collect();
        let mut hops = 0;
        while let Some(c) = todo.pop_front() {
            if c == ".." {
                cur.pop();
                continue;
            }
            if c == "." || c.is_empty() {
                continue;
            }
            let mut next = cur.clone();
            next.push(c);
            let rel: PathBuf = next.iter().collect();
            let full = self.root.join(&rel);
            match fs::symlink_metadata(&full) {
                Ok(m) if m.is_dir() => cur = next,
                Ok(m) if m.file_type().is_symlink() => {
                    hops += 1;
                    if hops > MAX_HOPS {
                        bail!("too many symlinks on the way");
                    }
                    let target = fs::read_link(&full)?;
                    if target.is_absolute() {
                        cur.clear();
                    }
                    let mut new: VecDeque<OsString> = target
                        .components()
                        .filter_map(|c| match c {
                            Component::Normal(n) => Some(n.to_os_string()),
                            Component::ParentDir => Some(OsString::from("..")),
                            _ => None,
                        })
                        .collect();
                    new.extend(todo.drain(..));
                    todo = new;
                }
                Ok(_) if create => {
                    // A file where the layer wants a directory: the layer wins.
                    self.remove(&rel)?;
                    mkdir(&full)?;
                    self.meta.insert(rel.clone(), Meta::dir());
                    self.layer.insert(rel);
                    cur = next;
                }
                Ok(_) => return Ok(None),
                Err(e) if e.kind() == io::ErrorKind::NotFound && create => {
                    mkdir(&full)?;
                    self.meta.insert(rel.clone(), Meta::dir());
                    self.layer.insert(rel);
                    cur = next;
                }
                Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
                Err(e) => return Err(e.into()),
            }
        }
        Ok(Some(cur.iter().collect()))
    }

    /// Removes a path and everything below it, never through a symlink.
    fn remove(&mut self, rel: &Path) -> anyhow::Result<()> {
        let full = self.root.join(rel);
        match fs::symlink_metadata(&full) {
            Ok(m) if m.is_dir() => fs::remove_dir_all(&full)?,
            Ok(_) => fs::remove_file(&full)?,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        self.forget(rel);
        Ok(())
    }

    /// Drops the metadata of a path and everything below it.
    fn forget(&mut self, rel: &Path) {
        let gone: Vec<PathBuf> = self
            .meta
            .range(rel.to_path_buf()..)
            .take_while(|(k, _)| k.starts_with(rel))
            .map(|(k, _)| k.clone())
            .collect();
        for k in gone {
            self.meta.remove(&k);
            self.dir_times.remove(&k);
        }
    }

    /// An opaque directory: hides everything in it that this layer didn't
    /// make.
    fn clear_below(&mut self, dir: &Path) -> anyhow::Result<()> {
        // Device nodes and fifos live only in the metadata.
        let virtual_children: Vec<PathBuf> = self
            .meta
            .range(dir.to_path_buf()..)
            .take_while(|(k, _)| k.starts_with(dir))
            .filter(|(k, m)| {
                k.parent() == Some(dir)
                    && matches!(m.kind, Kind::Char(..) | Kind::Block(..) | Kind::Fifo)
            })
            .map(|(k, _)| k.clone())
            .collect();
        for k in virtual_children {
            if !self.layer.contains(&k) {
                self.forget(&k);
            }
        }
        let full = self.root.join(dir);
        for e in fs::read_dir(&full)? {
            let e = e?;
            let rel = dir.join(e.file_name());
            if !self.layer.contains(&rel) {
                self.remove(&rel)?;
            } else if e.file_type()?.is_dir() {
                self.clear_below(&rel)?;
            }
        }
        Ok(())
    }

    /// Puts a host file in the root (like `pyo-envd`), owned by root.
    pub fn add_file(&mut self, rel: &str, src: &Path, mode: u32) -> anyhow::Result<()> {
        let comps = clean(Path::new(rel))?;
        let (name, parents) = comps.split_last().context("no file name")?;
        let dir = self.resolve_dir(parents, true)?.expect("created");
        let rel = dir.join(name);
        self.remove(&rel)?;
        let full = self.root.join(&rel);
        fs::copy(src, &full).with_context(|| format!("copying {}", src.display()))?;
        fs::set_permissions(&full, fs::Permissions::from_mode((mode & 0o777) | 0o400))?;
        self.meta.insert(
            rel,
            Meta {
                kind: Kind::File,
                uid: 0,
                gid: 0,
                mode,
            },
        );
        Ok(())
    }

    /// Makes a directory the guest needs, when the image has nothing there.
    /// A symlink there is kept: the guest follows it.
    pub fn ensure_dir(&mut self, rel: &str) -> anyhow::Result<()> {
        let full = self.root.join(rel);
        if fs::symlink_metadata(&full).is_ok() {
            return Ok(());
        }
        let comps = clean(Path::new(rel))?;
        self.resolve_dir(&comps, true)?;
        Ok(())
    }

    /// Makes the ext4 root disk at `out` and sets on it what the staging
    /// directory couldn't hold. Returns its size in bytes, before sparse.
    pub fn build_ext4(&mut self, out: &Path, label: &str) -> anyhow::Result<u64> {
        self.check_cancel()?;
        let plan = self.plan()?;
        // Directory times last, deepest first, once nothing changes in them.
        for (rel, t) in self.dir_times.iter().rev() {
            let full = self.root.join(rel);
            if let Ok(f) = fs::File::open(&full) {
                let _ = f.set_modified(UNIX_EPOCH + Duration::from_secs(*t));
            }
        }
        let mut size = plan.size;
        let mut tries = 0;
        loop {
            self.check_cancel()?;
            let _ = fs::remove_file(out);
            let res = Command::new(crate::storage::mkfs())
                .args(["-q", "-F", "-L", label, "-m", "0", "-O", "^has_journal"])
                .arg("-N")
                .arg(plan.inodes.to_string())
                .args(["-E", "root_owner=0:0", "-d"])
                .arg(&self.root)
                .arg(out)
                .arg(format!("{}K", size / 1024))
                .output()
                .context("running mkfs.ext4")?;
            if res.status.success() {
                break;
            }
            let err = String::from_utf8_lossy(&res.stderr).trim().to_string();
            tries += 1;
            if tries >= 3 || !err.to_lowercase().contains("could not allocate") {
                bail!("mkfs.ext4 failed: {err}");
            }
            size = size * 3 / 2;
        }
        if !plan.script.is_empty() {
            let script = out.with_extension("debugfs");
            fs::write(&script, &plan.script)?;
            let res = Command::new(debugfs())
                .arg("-w")
                .arg("-f")
                .arg(&script)
                .arg(out)
                .output()
                .context("running debugfs")?;
            let _ = fs::remove_file(&script);
            // debugfs exits 0 when a command fails, and says so on stderr.
            let err: Vec<String> = String::from_utf8_lossy(&res.stderr)
                .lines()
                .filter(|l| !l.trim().is_empty() && !l.starts_with("debugfs "))
                .take(5)
                .map(str::to_string)
                .collect();
            if !res.status.success() || !err.is_empty() {
                bail!(
                    "setting owners and modes with debugfs failed: {}",
                    err.join(" / ")
                );
            }
        }
        Ok(size)
    }

    /// The disk's size and inodes, and the debugfs commands that set what
    /// the staging copy lacks.
    fn plan(&self) -> anyhow::Result<Plan> {
        let mut plan = Plan::default();
        let mut seen: HashSet<(u64, u64)> = HashSet::new();
        let mut data: u64 = 0;
        let mut entries: u64 = 0;
        let mut stack = vec![PathBuf::new()];
        while let Some(rel) = stack.pop() {
            let full = self.root.join(&rel);
            let st = fs::symlink_metadata(&full)?;
            entries += 1;
            let ft = st.file_type();
            if ft.is_dir() {
                let mut n = 0u64;
                for e in fs::read_dir(&full)? {
                    let e = e?;
                    n += 1;
                    stack.push(rel.join(e.file_name()));
                }
                data += round_up((n * 48).max(BLOCK));
            } else if ft.is_file() {
                if seen.insert((st.dev(), st.ino())) {
                    // Content plus about one extent block per 128 MiB.
                    data += round_up(st.size()) + (st.size() >> 27) * BLOCK;
                } else {
                    entries -= 1;
                }
            } else if ft.is_symlink() && st.size() > 59 {
                data += BLOCK;
            }
            let want = self.meta.get(&rel).copied().unwrap_or(if ft.is_dir() {
                Meta::dir()
            } else {
                Meta {
                    kind: if ft.is_symlink() {
                        Kind::Symlink
                    } else {
                        Kind::File
                    },
                    uid: 0,
                    gid: 0,
                    mode: st.mode() & 0o7777,
                }
            });
            let Some(p) = debugfs_path(&rel) else {
                plan.skipped += 1;
                continue;
            };
            let is_root = rel.as_os_str().is_empty();
            if is_root || st.uid() != want.uid {
                plan.line(format!("sif {p} uid {}", want.uid));
            }
            if is_root || st.gid() != want.gid {
                plan.line(format!("sif {p} gid {}", want.gid));
            }
            if !ft.is_symlink() && (is_root || st.mode() & 0o7777 != want.mode) {
                let ty = if ft.is_dir() { 0o040000 } else { 0o100000 };
                plan.line(format!("sif {p} mode 0{:o}", ty | want.mode));
            }
        }
        // Device nodes and fifos exist only on the disk.
        for (rel, m) in &self.meta {
            let (ty, node) = match m.kind {
                Kind::Char(a, b) => (0o020000, format!("c {a} {b}")),
                Kind::Block(a, b) => (0o060000, format!("b {a} {b}")),
                Kind::Fifo => (0o010000, "p".to_string()),
                _ => continue,
            };
            let parent = rel.parent().unwrap_or(Path::new(""));
            let name = rel.file_name().map(Path::new).unwrap_or(Path::new(""));
            let (Some(p), Some(dir), Some(name)) = (
                debugfs_path(rel),
                debugfs_path(parent),
                debugfs_path(name).map(|n| n.replacen("\"/", "\"", 1)),
            ) else {
                plan.skipped += 1;
                continue;
            };
            entries += 1;
            // mknod makes the node in the current directory.
            plan.line(format!("cd {dir}"));
            plan.line(format!("mknod {name} {node}"));
            plan.line("cd /".into());
            plan.line(format!("sif {p} mode 0{:o}", ty | m.mode));
            plan.line(format!("sif {p} uid {}", m.uid));
            plan.line(format!("sif {p} gid {}", m.gid));
        }
        if plan.skipped > 0 {
            tracing::warn!(
                skipped = plan.skipped,
                "some names debugfs can't take kept the daemon's owner"
            );
        }
        plan.inodes = entries + 256;
        let inode_table = plan.inodes * 256;
        let raw = data + inode_table;
        plan.size = round_up(raw + raw / 20 + (16 << 20)).next_multiple_of(1 << 20);
        Ok(plan)
    }
}

#[derive(Default)]
struct Plan {
    size: u64,
    inodes: u64,
    script: String,
    skipped: u64,
}

impl Plan {
    fn line(&mut self, l: String) {
        self.script.push_str(&l);
        self.script.push('\n');
    }
}

fn round_up(n: u64) -> u64 {
    n.div_ceil(BLOCK) * BLOCK
}

fn mkdir(full: &Path) -> io::Result<()> {
    fs::create_dir(full)?;
    fs::set_permissions(full, fs::Permissions::from_mode(0o755))
}

/// A path's parts, without `.` or a leading `/`. `..` is refused.
fn clean(p: &Path) -> anyhow::Result<Vec<OsString>> {
    let mut out = Vec::new();
    for c in p.components() {
        match c {
            Component::Normal(n) => out.push(n.to_os_string()),
            Component::CurDir | Component::RootDir => {}
            Component::ParentDir => bail!("the path {} has a ..", p.display()),
            Component::Prefix(_) => bail!("a bad path"),
        }
    }
    Ok(out)
}

/// A path as a debugfs argument, quoted. None for names debugfs can't take.
fn debugfs_path(rel: &Path) -> Option<String> {
    let s = rel.to_str()?;
    if s.contains(['"', '\n', '\r', '\\']) {
        return None;
    }
    Some(format!("\"/{s}\""))
}

fn debugfs() -> &'static str {
    for p in ["/usr/sbin/debugfs", "/sbin/debugfs"] {
        if Path::new(p).exists() {
            return p;
        }
    }
    "debugfs"
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Tmp(PathBuf);
    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    fn tmp(name: &str) -> Tmp {
        let p = std::env::temp_dir().join(format!(
            "pyo-rootfs-{name}-{}-{}",
            std::process::id(),
            crate::now_ms()
        ));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(&p).unwrap();
        Tmp(p)
    }

    /// Builds a layer from (path, kind, mode, uid, content or link target).
    enum E<'a> {
        Dir(&'a str, u32),
        File(&'a str, u32, u32, &'a str),
        Sym(&'a str, &'a str),
        Hard(&'a str, &'a str),
        Char(&'a str, u32, u32),
        Fifo(&'a str),
    }

    fn layer(entries: &[E]) -> Vec<u8> {
        let mut b = tar::Builder::new(Vec::new());
        for e in entries {
            let mut h = tar::Header::new_gnu();
            h.set_mtime(1_700_000_000);
            h.set_uid(0);
            h.set_gid(0);
            match e {
                E::Dir(p, mode) => {
                    h.set_entry_type(tar::EntryType::Directory);
                    h.set_mode(*mode);
                    h.set_size(0);
                    b.append_data(&mut h, p, io::empty()).unwrap();
                }
                E::File(p, mode, uid, body) => {
                    h.set_entry_type(tar::EntryType::Regular);
                    h.set_mode(*mode);
                    h.set_uid(*uid as u64);
                    h.set_gid(*uid as u64);
                    h.set_size(body.len() as u64);
                    b.append_data(&mut h, p, body.as_bytes()).unwrap();
                }
                E::Sym(p, t) => {
                    h.set_entry_type(tar::EntryType::Symlink);
                    h.set_mode(0o777);
                    h.set_size(0);
                    b.append_link(&mut h, p, t).unwrap();
                }
                E::Hard(p, t) => {
                    h.set_entry_type(tar::EntryType::Link);
                    h.set_mode(0o644);
                    h.set_size(0);
                    b.append_link(&mut h, p, t).unwrap();
                }
                E::Char(p, a, c) => {
                    h.set_entry_type(tar::EntryType::Char);
                    h.set_mode(0o666);
                    h.set_size(0);
                    h.set_device_major(*a).unwrap();
                    h.set_device_minor(*c).unwrap();
                    b.append_data(&mut h, p, io::empty()).unwrap();
                }
                E::Fifo(p) => {
                    h.set_entry_type(tar::EntryType::Fifo);
                    h.set_mode(0o600);
                    h.set_size(0);
                    b.append_data(&mut h, p, io::empty()).unwrap();
                }
            }
        }
        b.into_inner().unwrap()
    }

    fn unpacker(t: &Tmp) -> Unpacker {
        Unpacker::new(&t.0.join("root"), 1 << 30, Arc::new(AtomicBool::new(false))).unwrap()
    }

    fn read(t: &Tmp, p: &str) -> Option<String> {
        fs::read_to_string(t.0.join("root").join(p)).ok()
    }

    fn exists(t: &Tmp, p: &str) -> bool {
        fs::symlink_metadata(t.0.join("root").join(p)).is_ok()
    }

    #[test]
    fn whiteouts_remove_and_opaque_hides() {
        let t = tmp("wh");
        let mut u = unpacker(&t);
        u.apply(
            &layer(&[
                E::Dir("etc", 0o755),
                E::File("etc/a", 0o644, 0, "a"),
                E::File("etc/b", 0o644, 0, "b"),
                E::Dir("var/lib/x", 0o755),
                E::File("var/lib/x/old", 0o644, 0, "old"),
                E::Dir("var/lib/x/sub", 0o755),
                E::File("var/lib/x/sub/deep", 0o644, 0, "deep"),
                E::Char("var/lib/x/null", 1, 3),
            ])[..],
        )
        .unwrap();
        u.apply(
            &layer(&[
                E::File("etc/.wh.a", 0o644, 0, ""),
                E::File("etc/.wh.missing", 0o644, 0, ""),
                // The new file first, then the marker: the layer's own entries stay.
                E::File("var/lib/x/new", 0o644, 0, "new"),
                E::Dir("var/lib/x/sub", 0o700),
                E::File("var/lib/x/.wh..wh..opq", 0o644, 0, ""),
            ])[..],
        )
        .unwrap();
        assert!(!exists(&t, "etc/a"));
        assert_eq!(read(&t, "etc/b").as_deref(), Some("b"));
        assert!(!exists(&t, "var/lib/x/old"));
        assert_eq!(read(&t, "var/lib/x/new").as_deref(), Some("new"));
        assert!(exists(&t, "var/lib/x/sub"), "remade by the layer");
        assert!(
            !exists(&t, "var/lib/x/sub/deep"),
            "below an opaque directory, lower files are hidden even in kept directories"
        );
        assert!(u.meta("var/lib/x/null").is_none(), "device hidden too");
        assert!(u.meta("etc/a").is_none());
        assert_eq!(u.meta("var/lib/x/sub").unwrap().mode, 0o700);
        // A whiteout of a directory takes everything below it.
        u.apply(&layer(&[E::File("var/.wh.lib", 0o644, 0, "")])[..])
            .unwrap();
        assert!(!exists(&t, "var/lib"));
        assert!(u.meta("var/lib/x/new").is_none());
    }

    #[test]
    fn symlinks_stay_inside_the_root() {
        let t = tmp("sym");
        let outside = t.0.join("outside");
        fs::create_dir_all(&outside).unwrap();
        let mut u = unpacker(&t);
        u.apply(
            &layer(&[
                E::Sym("abs", "/"),
                E::Sym("up", "../../../.."),
                E::Sym("out", outside.to_str().unwrap()),
                E::Dir("usr/lib", 0o755),
                E::Sym("lib", "usr/lib"),
            ])[..],
        )
        .unwrap();
        u.apply(
            &layer(&[
                E::File("abs/etc/x", 0o644, 0, "x"),
                E::File("up/y", 0o644, 0, "y"),
                E::File("out/z", 0o644, 0, "z"),
                E::File("lib/libc.so", 0o755, 0, "libc"),
            ])[..],
        )
        .unwrap();
        assert_eq!(read(&t, "etc/x").as_deref(), Some("x"), "/ is the root");
        assert_eq!(read(&t, "y").as_deref(), Some("y"), ".. stops at the root");
        assert!(
            fs::read_dir(&outside).unwrap().next().is_none(),
            "nothing lands outside the root"
        );
        assert!(exists(
            &t,
            &format!("{}/z", outside.to_str().unwrap().trim_start_matches('/'))
        ));
        assert_eq!(read(&t, "usr/lib/libc.so").as_deref(), Some("libc"));
        assert!(
            fs::symlink_metadata(t.0.join("root/lib"))
                .unwrap()
                .file_type()
                .is_symlink()
        );
        // `..` in an entry is refused.
        let bad = {
            let mut b = tar::Builder::new(Vec::new());
            let mut h = tar::Header::new_gnu();
            h.set_entry_type(tar::EntryType::Regular);
            h.set_size(1);
            h.set_mode(0o644);
            let name = b"../escape";
            h.as_old_mut().name[..name.len()].copy_from_slice(name);
            h.set_cksum();
            b.append(&h, &b"x"[..]).unwrap();
            b.into_inner().unwrap()
        };
        assert!(u.apply(&bad[..]).is_err());
        assert!(!t.0.join("escape").exists());
    }

    #[test]
    fn hard_links_and_replacing_types() {
        let t = tmp("hard");
        let mut u = unpacker(&t);
        u.apply(
            &layer(&[
                E::File("bin/busybox", 0o4755, 0, "bb"),
                E::Hard("bin/sh", "bin/busybox"),
                E::Dir("thing", 0o755),
                E::File("thing/inner", 0o644, 0, "i"),
                E::File("file", 0o644, 0, "f"),
            ])[..],
        )
        .unwrap();
        let a = fs::metadata(t.0.join("root/bin/busybox")).unwrap();
        let b = fs::metadata(t.0.join("root/bin/sh")).unwrap();
        assert_eq!(a.ino(), b.ino());
        assert_eq!(u.meta("bin/sh").unwrap().mode, 0o4755);
        assert_eq!(a.mode() & 0o7000, 0, "no setuid on the host's disk");
        u.apply(
            &layer(&[
                E::File("thing", 0o644, 0, "now a file"),
                E::Dir("file", 0o711),
                E::File("file/x", 0o644, 0, "x"),
            ])[..],
        )
        .unwrap();
        assert_eq!(read(&t, "thing").as_deref(), Some("now a file"));
        assert!(u.meta("thing/inner").is_none());
        assert_eq!(read(&t, "file/x").as_deref(), Some("x"));
        assert_eq!(u.meta("file").unwrap().kind, Kind::Dir);
    }

    #[test]
    fn size_limit() {
        let t = tmp("limit");
        let mut u = Unpacker::new(&t.0.join("root"), 10, Arc::new(AtomicBool::new(false))).unwrap();
        let e = u
            .apply(&layer(&[E::File("big", 0o644, 0, "more than ten bytes")])[..])
            .unwrap_err();
        assert!(
            format!("{e:#}").contains("MiB a template may take"),
            "{e:#}"
        );
    }

    fn debugfs_stat(img: &Path, p: &str) -> String {
        let out = Command::new(debugfs())
            .arg("-R")
            .arg(format!("stat \"{p}\""))
            .arg(img)
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    fn debugfs_cat(img: &Path, p: &str) -> String {
        let out = Command::new(debugfs())
            .arg("-R")
            .arg(format!("cat \"{p}\""))
            .arg(img)
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    #[test]
    fn the_disk_gets_owners_modes_and_devices() {
        if !Path::new(crate::storage::mkfs()).exists() && which("mkfs.ext4").is_none() {
            eprintln!("skipping: no mkfs.ext4");
            return;
        }
        let t = tmp("ext4");
        let mut u = unpacker(&t);
        u.apply(
            &layer(&[
                E::Dir("etc", 0o755),
                E::File("etc/shadow", 0o640, 42, "secret"),
                E::File("usr/bin/sudo", 0o4755, 0, "sudo"),
                E::Hard("usr/bin/sudoedit", "usr/bin/sudo"),
                E::File("home/app/my file", 0o600, 1000, "spaces"),
                E::Dir("tmp", 0o1777),
                E::Sym("bin", "usr/bin"),
                E::Char("dev/null", 1, 3),
                E::Fifo("run/fifo"),
                E::File("locked", 0o000, 0, "locked"),
            ])[..],
        )
        .unwrap();
        let envd = t.0.join("envd");
        fs::write(&envd, "envd").unwrap();
        // /sbin is missing, so it's made.
        u.add_file("sbin/pyo-envd", &envd, 0o755).unwrap();
        for d in GUEST_DIRS {
            u.ensure_dir(d).unwrap();
        }
        let img = t.0.join("rootfs.ext4");
        u.build_ext4(&img, "test").unwrap();
        let s = debugfs_stat(&img, "/etc/shadow");
        assert!(
            s.contains("User:    42") && s.contains("Group:    42"),
            "{s}"
        );
        assert!(s.contains("Mode:  0640"), "{s}");
        let s = debugfs_stat(&img, "/usr/bin/sudo");
        assert!(
            s.contains("Mode:  04755") && s.contains("User:     0"),
            "{s}"
        );
        assert!(s.contains("Links: 2"), "hard link kept: {s}");
        let s = debugfs_stat(&img, "/home/app/my file");
        assert!(s.contains("User:  1000"), "{s}");
        let s = debugfs_stat(&img, "/tmp");
        assert!(s.contains("Mode:  01777"), "{s}");
        let s = debugfs_stat(&img, "/dev/null");
        assert!(
            s.contains("Type: character special") && s.contains("Mode:  0666"),
            "{s}"
        );
        let s = debugfs_stat(&img, "/run/fifo");
        assert!(s.contains("Type: FIFO"), "{s}");
        let s = debugfs_stat(&img, "/locked");
        assert!(s.contains("Mode:  0000"), "{s}");
        let s = debugfs_stat(&img, "/sbin/pyo-envd");
        assert!(
            s.contains("Mode:  0755") && s.contains("User:     0"),
            "{s}"
        );
        let s = debugfs_stat(&img, "/bin");
        assert!(s.contains("Type: symlink"), "{s}");
        let s = debugfs_stat(&img, "/");
        assert!(
            s.contains("User:     0") && s.contains("Mode:  0755"),
            "{s}"
        );
        for d in GUEST_DIRS {
            assert!(
                debugfs_stat(&img, &format!("/{d}")).contains("Type: directory"),
                "{d}"
            );
        }
        assert_eq!(debugfs_cat(&img, "/home/app/my file"), "spaces");
        assert_eq!(debugfs_cat(&img, "/sbin/pyo-envd"), "envd");
    }

    fn which(b: &str) -> Option<PathBuf> {
        std::env::var_os("PATH")?
            .to_str()?
            .split(':')
            .map(|d| Path::new(d).join(b))
            .find(|p| p.exists())
    }

    #[test]
    fn layer_media_types() {
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        io::Write::write_all(&mut gz, b"hello").unwrap();
        let gz = gz.finish().unwrap();
        let mut s = String::new();
        decompress("application/vnd.oci.image.layer.v1.tar+gzip", &gz[..])
            .unwrap()
            .read_to_string(&mut s)
            .unwrap();
        assert_eq!(s, "hello");
        let mut s = String::new();
        decompress("application/vnd.docker.image.rootfs.diff.tar.gzip", &gz[..])
            .unwrap()
            .read_to_string(&mut s)
            .unwrap();
        assert_eq!(s, "hello");
        // A zstd frame of "hello", made by `zstd`.
        let zst: &[u8] = &[
            0x28, 0xb5, 0x2f, 0xfd, 0x04, 0x58, 0x29, 0x00, 0x00, 0x68, 0x65, 0x6c, 0x6c, 0x6f,
            0xa3, 0x6d, 0x9f, 0x88,
        ];
        let mut s = String::new();
        decompress("application/vnd.oci.image.layer.v1.tar+zstd", zst)
            .unwrap()
            .read_to_string(&mut s)
            .unwrap();
        assert_eq!(s, "hello");
        assert!(decompress("application/vnd.oci.image.layer.v1.tar", &b""[..]).is_ok());
        assert!(decompress("application/x-weird", &b""[..]).is_err());
    }
}
