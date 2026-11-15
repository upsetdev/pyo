//! The storage pool on a sandbox server.
//!
//! ```text
//! <root>/
//!   templates/<name>/rootfs.ext4         read-only root, shared by every sandbox
//!   templates/<name>/snapshots/<key>/    vm.snap, vm.mem (holes punched), scratch.ext4
//!   cache/scratch-<MiB>.ext4             an empty writable layer per size, sparse
//!   cache/testboot.ext4                  the detection test boot's root
//!   sandboxes/<id>/                      one VM in dev mode (see below)
//!   jail/firecracker/<id>/root/          one VM under the jailer, the chroot
//! ```
//!
//! A VM directory holds `rootfs.ext4` and `vmlinux` (links), its own sparse
//! `scratch.ext4`, the Firecracker sockets `api.sock` and `v.sock`, its
//! config `vm.json`, `fc.log` and `sandbox.json` (the daemon's record).
//! Firecracker sees only relative paths, so one snapshot restores into any
//! VM directory and picks up that directory's own writable layer and
//! sockets. The root disk and template snapshots are never copied per
//! sandbox. Only the writable layer is, and it's sparse.
//!
//! Plain ext4 is fine. Reflinks (XFS or btrfs) only start to matter once
//! writable layers get big, for pause and fork.

use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::{Context, bail};
use tokio::process::Command;

#[derive(Debug, Clone)]
pub struct Layout {
    pub root: PathBuf,
    pub jailed: bool,
}

impl Layout {
    pub fn new(root: impl Into<PathBuf>, jailed: bool) -> Layout {
        Layout {
            root: root.into(),
            jailed,
        }
    }

    pub fn template_dir(&self, name: &str) -> PathBuf {
        self.root.join("templates").join(name)
    }

    pub fn template_rootfs(&self, name: &str) -> PathBuf {
        self.template_dir(name).join("rootfs.ext4")
    }

    pub fn template_snapshot(&self, name: &str, key: &str) -> PathBuf {
        self.template_dir(name).join("snapshots").join(key)
    }

    pub fn cache(&self) -> PathBuf {
        self.root.join("cache")
    }

    pub fn sandboxes(&self) -> PathBuf {
        if self.jailed {
            self.root.join("jail").join("firecracker")
        } else {
            self.root.join("sandboxes")
        }
    }

    /// Where a VM's files live. Under the jailer it's the chroot.
    pub fn vm_dir(&self, id: &str) -> PathBuf {
        if self.jailed {
            self.sandbox_top(id).join("root")
        } else {
            self.sandbox_top(id)
        }
    }

    /// What to delete when the sandbox goes.
    pub fn sandbox_top(&self, id: &str) -> PathBuf {
        self.sandboxes().join(instance_id(id))
    }

    /// Every directory the daemon writes to.
    pub async fn create(&self) -> anyhow::Result<()> {
        for d in [self.root.join("templates"), self.cache(), self.sandboxes()] {
            tokio::fs::create_dir_all(&d)
                .await
                .with_context(|| format!("creating {}", d.display()))?;
        }
        Ok(())
    }
}

/// The name Firecracker and the jailer know a VM by, which is also its
/// directory name. They take only letters, digits and `-`, so the `_` of a
/// sandbox ID becomes `-`.
pub fn instance_id(id: &str) -> String {
    id.replace('_', "-")
}

/// Snapshot key: snapshots only restore on the same Firecracker version and
/// CPU template, and with the same machine size.
pub fn snapshot_key(
    fc_version: &str,
    cpu_template: Option<&str>,
    cpus: u32,
    memory_mib: u32,
    disk_mib: u32,
) -> String {
    let v: String = fc_version
        .trim_start_matches('v')
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' {
                c
            } else {
                '_'
            }
        })
        .collect();
    format!(
        "fc{v}-{}{}",
        cpu_template.unwrap_or("none").to_lowercase(),
        pyo_proto::sandbox::snapshot_size_tag(cpus, memory_mib, disk_mib)
    )
}

/// Links `src` to `dst`: a hard link, so it also works inside a jail. Falls
/// back to a symlink across filesystems in dev mode.
pub fn link(src: &Path, dst: &Path, allow_symlink: bool) -> anyhow::Result<()> {
    let _ = std::fs::remove_file(dst);
    match std::fs::hard_link(src, dst) {
        Ok(()) => Ok(()),
        Err(_) if allow_symlink => std::os::unix::fs::symlink(src, dst)
            .with_context(|| format!("linking {} to {}", dst.display(), src.display())),
        Err(e) => {
            Err(e).with_context(|| format!("hard linking {} to {}", dst.display(), src.display()))
        }
    }
}

/// Copies a sparse file and keeps its holes. Uses a reflink when the
/// filesystem has them.
pub async fn copy_sparse(src: &Path, dst: &Path) -> anyhow::Result<()> {
    run(Command::new("cp")
        .arg("--sparse=always")
        .arg("--reflink=auto")
        .arg(src)
        .arg(dst))
    .await
}

/// An empty ext4 image of `mib` MiB, as a sparse file. Made once per size
/// and copied after that.
pub async fn empty_ext4(cache: &Path, mib: u32) -> anyhow::Result<PathBuf> {
    let path = cache.join(format!("scratch-{mib}.ext4"));
    if path.exists() {
        return Ok(path);
    }
    let tmp = cache.join(format!(".scratch-{mib}.{}.tmp", std::process::id()));
    let _ = tokio::fs::remove_file(&tmp).await;
    run(Command::new(mkfs())
        .args([
            "-q",
            "-F",
            "-L",
            "scratch",
            "-E",
            "lazy_itable_init=1,lazy_journal_init=1",
        ])
        .arg(&tmp)
        .arg(format!("{mib}M")))
    .await?;
    tokio::fs::rename(&tmp, &path).await?;
    Ok(path)
}

/// A read-only ext4 image made from a directory, without root.
pub async fn ext4_from_dir(dir: &Path, out: &Path, mib: u32, label: &str) -> anyhow::Result<()> {
    run(Command::new(mkfs())
        .args(["-q", "-F", "-L", label, "-d"])
        .arg(dir)
        .arg(out)
        .arg(format!("{mib}M")))
    .await
}

/// Punches holes where a file is all zeroes. A 256 MiB snapshot memory
/// file goes to about 67 MiB.
pub async fn dig_holes(path: &Path) -> anyhow::Result<()> {
    run(Command::new("fallocate").arg("--dig-holes").arg(path)).await
}

fn mkfs() -> &'static str {
    for p in ["/usr/sbin/mkfs.ext4", "/sbin/mkfs.ext4"] {
        if Path::new(p).exists() {
            return p;
        }
    }
    "mkfs.ext4"
}

async fn run(cmd: &mut Command) -> anyhow::Result<()> {
    let out = cmd
        .stdin(Stdio::null())
        .output()
        .await
        .context("starting a storage command")?;
    if !out.status.success() {
        bail!(
            "{:?} failed: {}",
            cmd.as_std(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

/// Bytes the filesystem holding `path` has free.
pub fn free_bytes(path: &Path) -> u64 {
    use std::os::unix::ffi::OsStrExt;
    let Ok(c) = std::ffi::CString::new(path.as_os_str().as_bytes()) else {
        return 0;
    };
    // SAFETY: statvfs fills the struct for a valid path.
    unsafe {
        let mut s: libc::statvfs = std::mem::zeroed();
        if libc::statvfs(c.as_ptr(), &mut s) != 0 {
            return 0;
        }
        s.f_bavail as u64 * s.f_frsize as u64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout() {
        let dev = Layout::new("/p", false);
        assert_eq!(dev.vm_dir("sbx_a"), PathBuf::from("/p/sandboxes/sbx-a"));
        assert_eq!(instance_id("testboot-1"), "testboot-1");
        assert_eq!(
            dev.template_rootfs("node-22"),
            PathBuf::from("/p/templates/node-22/rootfs.ext4")
        );
        let jail = Layout::new("/p", true);
        assert_eq!(
            jail.vm_dir("sbx_a"),
            PathBuf::from("/p/jail/firecracker/sbx-a/root")
        );
        assert_eq!(
            jail.sandbox_top("sbx_a"),
            PathBuf::from("/p/jail/firecracker/sbx-a")
        );
    }

    #[test]
    fn snapshot_keys() {
        assert_eq!(
            snapshot_key("v1.18.0", None, 1, 512, 2048),
            "fc1.18.0-none-1c-512m-2048d"
        );
        assert_eq!(
            snapshot_key("1.18.0", Some("T2"), 2, 1024, 64),
            "fc1.18.0-t2-2c-1024m-64d"
        );
        assert_eq!(
            snapshot_key("1.18/../x", None, 1, 1, 1),
            "fc1.18_.._x-none-1c-1m-1d"
        );
    }
}
