//! The gVisor driver, for servers without usable KVM, such as Hetzner
//! Cloud. Detection picks it when Firecracker can't run (see `detect`).
//!
//! - **runsc as root**, `--platform=systrap`, never the KVM platform: on a
//!   cloud VM KVM is nested, and slower at everything (gVisor spike,
//!   item 18). Its state lives under `<root>/runsc`.
//! - **One OCI bundle per sandbox** in its directory. The root is a host
//!   overlay: the template's disk, mounted read-only once per template
//!   (`templates/<name>/root`), under a writable layer on the sandbox's own
//!   ext4 image of `diskMib`, so a sandbox can't fill the server's disk.
//!   runsc's own overlay is off (`--overlay2=none`), so writes go to that
//!   disk, not into memory, and a checkpoint stays small.
//! - **envd dials out.** It runs as the sandbox's PID 1 with
//!   `PYO_ENVD_DIAL=unix:/pyo/host.sock` and `--host-uds=open`. `/pyo` is
//!   the sandbox directory's `pyo/`, where the daemon listens on
//!   `host.sock` ([`Door`]). envd's first frame on each connection is the
//!   ready signal, and it keeps one idle connection waiting. It never
//!   listens, because a bound host socket blocks `runsc checkpoint`.
//! - **Network**: a namespace and veth pair per sandbox behind the same
//!   nftables table as Firecracker's taps (`network`), handed to runsc as
//!   the OCI network namespace with `--network=sandbox`. "No network" is a
//!   fresh namespace with `--network=none`.
//! - **Memory** is the sandbox cgroup's `memory.current`
//!   (`<cgroupParent>/<instance>`), which holds the sentry and the gofer
//!   and matches the host within 1 MiB.
//! - **Templates** are cold starts (37 ms in the spike), not memory
//!   snapshots: a gVisor restore costs more and shares no pages.
//! - **Pause** is `runsc checkpoint` into the sandbox directory, after the
//!   daemon closed its listener and every envd connection. **Resume** is
//!   `runsc restore`. Images are small (15 MiB for a warm Python sandbox).
//!   They need the same runsc release, which the capability report names.

use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex as StdMutex, Weak};
use std::time::{Duration, Instant};

use anyhow::{Context, anyhow, bail};
use serde_json::{Value, json};
use tokio::net::UnixListener;
use tokio::process::Command;

use crate::envd::{self, Guard};
use crate::firecracker::{DirHandle, process_alive};
use crate::storage;

pub const PLATFORM: &str = "systrap";
/// The sandbox directory's part that is `/pyo` inside.
pub const PYO_DIR: &str = "pyo";
pub const HOST_SOCK: &str = "host.sock";
/// Where envd dials, inside the sandbox.
pub const DIAL: &str = "unix:/pyo/host.sock";
const ROOTFS_DIR: &str = "rootfs";
const SCRATCH_IMG: &str = "scratch.ext4";
const SCRATCH_DIR: &str = "scratch";
const RESOLV: &str = "resolv.conf";
/// The checkpoint of a paused sandbox.
pub const IMAGE_DIR: &str = "pause";
const LOG: &str = "runsc.log";
/// Memory the sentry and the gofer take on top of the guest's.
const CGROUP_OVERHEAD: u64 = 64 << 20;
const PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

/// How the daemon runs runsc.
#[derive(Debug, Clone)]
pub struct Gvisor {
    pub runsc: PathBuf,
    /// runsc's `--root`: its state of every sandbox.
    pub state: PathBuf,
    /// The cgroup each sandbox goes under, like the jailer's.
    pub cgroup_parent: String,
    /// One template mount at a time.
    mounting: Arc<tokio::sync::Mutex<()>>,
}

/// A sandbox to start.
pub struct Machine<'a> {
    /// The instance ID: runsc's container ID and the directory's name.
    pub id: &'a str,
    pub dir: &'a Path,
    /// The template's root, mounted ([`Gvisor::template_root`]).
    pub template_root: &'a Path,
    /// An empty ext4 image of the sandbox's disk size, copied once.
    pub scratch: Option<&'a Path>,
    pub cpus: u32,
    pub memory_mib: u32,
    /// The lease's network namespace, or None for no network.
    pub netns: Option<&'a str>,
    pub dns: &'a [Ipv4Addr],
    /// The cgroup it goes under ([`Gvisor::cgroup_parent`]).
    pub cgroup_parent: &'a str,
}

/// A running sandbox.
#[derive(Debug)]
pub struct Container {
    pub id: String,
    pub dir: PathBuf,
    /// The sandbox process on the host.
    pub pid: u32,
    pub network: bool,
    pub door: Arc<Door>,
    gv: Gvisor,
}

/// Where envd dials in: the listener on `pyo/host.sock` and the
/// connections it gave out.
#[derive(Debug)]
pub struct Door {
    handle: DirHandle,
    listener: tokio::sync::Mutex<Option<UnixListener>>,
    conns: StdMutex<Vec<Weak<Guard>>>,
}

impl Door {
    /// Listens on the directory's `host.sock`, in place of any old one.
    pub fn open(pyo_dir: &Path) -> anyhow::Result<Arc<Door>> {
        let door = Arc::new(Door {
            handle: DirHandle::open(pyo_dir)?,
            listener: tokio::sync::Mutex::new(None),
            conns: StdMutex::new(Vec::new()),
        });
        door.listen_now()?;
        Ok(door)
    }

    fn bind(&self) -> anyhow::Result<UnixListener> {
        let path = self.handle.path(HOST_SOCK);
        let _ = std::fs::remove_file(&path);
        UnixListener::bind(&path).context("listening for envd")
    }

    fn listen_now(&self) -> anyhow::Result<()> {
        let l = self.bind()?;
        *self
            .listener
            .try_lock()
            .map_err(|_| anyhow!("the door is busy"))? = Some(l);
        Ok(())
    }

    /// Listens again after [`Door::close`].
    pub async fn reopen(&self) -> anyhow::Result<()> {
        let l = self.bind()?;
        *self.listener.lock().await = Some(l);
        Ok(())
    }

    /// The next connection envd dialed, past its `Hello`.
    pub async fn accept(&self, timeout: Duration) -> anyhow::Result<envd::Conn> {
        let until = Instant::now() + timeout;
        let stream = {
            let guard = self.listener.lock().await;
            let l = guard
                .as_ref()
                .ok_or_else(|| anyhow!("the sandbox is being paused"))?;
            match tokio::time::timeout_at(until.into(), l.accept()).await {
                Ok(r) => r.context("accepting envd")?.0,
                Err(_) => bail!("envd didn't answer within {} ms", timeout.as_millis()),
            }
        };
        let left = until.saturating_duration_since(Instant::now());
        let (conn, guard) =
            tokio::time::timeout(left.max(Duration::from_secs(1)), envd::Conn::dialed(stream))
                .await
                .map_err(|_| anyhow!("envd didn't say hello"))??;
        let mut conns = self.conns.lock().unwrap_or_else(|p| p.into_inner());
        conns.retain(|w| w.strong_count() > 0);
        conns.push(Arc::downgrade(&guard));
        Ok(conn)
    }

    /// Stops listening and cuts every connection, which a checkpoint needs.
    /// envd goes back to dialing.
    pub async fn close(&self) {
        *self.listener.lock().await = None;
        let _ = std::fs::remove_file(self.handle.path(HOST_SOCK));
        let conns = std::mem::take(&mut *self.conns.lock().unwrap_or_else(|p| p.into_inner()));
        for g in conns.iter().filter_map(Weak::upgrade) {
            g.shutdown();
        }
    }
}

impl Gvisor {
    pub fn new(runsc: PathBuf, state: PathBuf, cgroup_parent: String) -> Gvisor {
        Gvisor {
            runsc,
            state,
            cgroup_parent,
            mounting: Arc::new(tokio::sync::Mutex::new(())),
        }
    }

    /// `runsc --version`'s release, like `release-20261109.0`.
    pub async fn version(&self) -> anyhow::Result<String> {
        let out = Command::new(&self.runsc)
            .arg("--version")
            .output()
            .await
            .with_context(|| format!("running {}", self.runsc.display()))?;
        parse_version(&String::from_utf8_lossy(&out.stdout))
            .ok_or_else(|| anyhow!("runsc --version said nothing useful"))
    }

    /// runsc's global flags, the same for every command on a sandbox.
    pub fn flags(&self, network: bool) -> Vec<String> {
        vec![
            format!("--root={}", self.state.display()),
            format!("--platform={PLATFORM}"),
            format!("--network={}", if network { "sandbox" } else { "none" }),
            "--host-uds=open".into(),
            "--overlay2=none".into(),
            "--cpu-num-from-quota".into(),
        ]
    }

    fn cmd(&self, network: bool) -> Command {
        let mut c = Command::new(&self.runsc);
        c.args(self.flags(network))
            .stdin(Stdio::null())
            .kill_on_drop(true);
        c
    }

    /// A runsc command that ends on its own, with its output.
    async fn run(&self, network: bool, args: &[&str]) -> anyhow::Result<String> {
        let out = self
            .cmd(network)
            .args(args)
            .output()
            .await
            .with_context(|| format!("starting {}", self.runsc.display()))?;
        if !out.status.success() {
            bail!(
                "runsc {}: {}",
                args.first().copied().unwrap_or_default(),
                last_line(&String::from_utf8_lossy(&out.stderr))
            );
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    /// `run --detach` and `restore --detach` leave the sandbox holding
    /// their stdout and stderr, so those go to a log file, never a pipe.
    async fn run_detached(&self, network: bool, dir: &Path, args: &[&str]) -> anyhow::Result<()> {
        let log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join(LOG))?;
        let status = self
            .cmd(network)
            .args(args)
            .stdout(log.try_clone()?)
            .stderr(log)
            .status()
            .await
            .with_context(|| format!("starting {}", self.runsc.display()))?;
        if !status.success() {
            let text = std::fs::read_to_string(dir.join(LOG)).unwrap_or_default();
            bail!(
                "runsc {}: {}",
                args.first().copied().unwrap_or_default(),
                last_line(&text)
            );
        }
        Ok(())
    }

    /// The sandbox's state and host pid, from `runsc state`.
    pub async fn state(&self, id: &str) -> anyhow::Result<(String, u32)> {
        let out = self.run(false, &["state", id]).await?;
        let v: Value = serde_json::from_str(&out).context("reading runsc state")?;
        Ok((
            v["status"].as_str().unwrap_or_default().to_string(),
            v["pid"].as_u64().unwrap_or(0) as u32,
        ))
    }

    /// The template's disk, mounted read-only at `templates/<name>/root`
    /// (once, for every sandbox of it).
    pub async fn template_root(&self, rootfs: &Path) -> anyhow::Result<PathBuf> {
        let dir = rootfs
            .parent()
            .ok_or_else(|| anyhow!("no template directory"))?
            .join("root");
        let _turn = self.mounting.lock().await;
        if is_mount(&dir) {
            return Ok(dir);
        }
        tokio::fs::create_dir_all(&dir).await?;
        mount(&["-o", "loop,ro"], rootfs, &dir).await?;
        Ok(dir)
    }

    /// The root and the files runsc reads: the writable layer on its own
    /// disk, the overlay, `resolv.conf` and `config.json`. Done again
    /// after a reboot, for a paused sandbox.
    async fn prepare(&self, m: &Machine<'_>) -> anyhow::Result<()> {
        let dir = m.dir;
        let pyo = dir.join(PYO_DIR);
        let rootfs = dir.join(ROOTFS_DIR);
        let scratch = dir.join(SCRATCH_DIR);
        for d in [&pyo, &rootfs, &scratch] {
            tokio::fs::create_dir_all(d).await?;
        }
        set_mode(&pyo, 0o700)?;
        let img = dir.join(SCRATCH_IMG);
        if !img.exists() {
            let src = m
                .scratch
                .ok_or_else(|| anyhow!("the sandbox's disk is missing"))?;
            storage::copy_sparse(src, &img).await?;
        }
        if !is_mount(&scratch) {
            mount(&["-o", "loop"], &img, &scratch).await?;
        }
        for d in ["upper", "work"] {
            tokio::fs::create_dir_all(scratch.join(d)).await?;
        }
        if !is_mount(&rootfs) {
            let opts = format!(
                "lowerdir={},upperdir={},workdir={}",
                m.template_root.display(),
                scratch.join("upper").display(),
                scratch.join("work").display()
            );
            mount(
                &["-t", "overlay", "-o", &opts],
                Path::new("overlay"),
                &rootfs,
            )
            .await?;
        }
        tokio::fs::write(dir.join(RESOLV), resolv_conf(m.dns)).await?;
        let spec = oci_spec(m, &rootfs, &pyo, &dir.join(RESOLV));
        tokio::fs::write(dir.join("config.json"), serde_json::to_vec_pretty(&spec)?).await?;
        Ok(())
    }

    /// Starts a sandbox and waits for envd's first connection.
    pub async fn start(
        &self,
        m: &Machine<'_>,
        timeout: Duration,
    ) -> anyhow::Result<(Container, Duration)> {
        let t0 = Instant::now();
        self.prepare(m).await?;
        let door = Door::open(&m.dir.join(PYO_DIR))?;
        let network = m.netns.is_some();
        let bundle = format!("--bundle={}", m.dir.display());
        self.run_detached(network, m.dir, &["run", "--detach", &bundle, m.id])
            .await?;
        self.finish(m.id, m.dir, network, door, t0, timeout).await
    }

    /// Resumes a checkpoint made by [`Gvisor::checkpoint`].
    pub async fn restore(
        &self,
        m: &Machine<'_>,
        timeout: Duration,
    ) -> anyhow::Result<(Container, Duration)> {
        let t0 = Instant::now();
        self.prepare(m).await?;
        let door = Door::open(&m.dir.join(PYO_DIR))?;
        let network = m.netns.is_some();
        // A container left from the checkpoint would take the ID.
        let _ = self.run(network, &["delete", "--force", m.id]).await;
        let image = format!("--image-path={}", m.dir.join(IMAGE_DIR).display());
        let bundle = format!("--bundle={}", m.dir.display());
        self.run_detached(
            network,
            m.dir,
            &["restore", "--detach", &image, &bundle, m.id],
        )
        .await?;
        let started = self.finish(m.id, m.dir, network, door, t0, timeout).await?;
        let _ = tokio::fs::remove_dir_all(m.dir.join(IMAGE_DIR)).await;
        Ok(started)
    }

    async fn finish(
        &self,
        id: &str,
        dir: &Path,
        network: bool,
        door: Arc<Door>,
        t0: Instant,
        timeout: Duration,
    ) -> anyhow::Result<(Container, Duration)> {
        let ready = door.accept(timeout).await;
        let pid = self.state(id).await.map(|(_, pid)| pid).unwrap_or(0);
        let c = Container {
            id: id.into(),
            dir: dir.into(),
            pid,
            network,
            door,
            gv: self.clone(),
        };
        match ready {
            // Dropped unused: envd dials the next one.
            Ok(_hello) => Ok((c, t0.elapsed())),
            Err(e) => {
                let log = std::fs::read_to_string(dir.join(LOG)).unwrap_or_default();
                let mut c = c;
                c.kill().await;
                Err(e.context(format!("the sandbox didn't start. {}", last_line(&log))))
            }
        }
    }

    /// A sandbox a previous daemon started, when runsc still runs it.
    pub async fn adopt(&self, id: &str, dir: &Path, network: bool) -> Option<Container> {
        let (status, pid) = self.state(id).await.ok()?;
        if status != "running" || !process_alive(pid) {
            return None;
        }
        let door = Door::open(&dir.join(PYO_DIR)).ok()?;
        Some(Container {
            id: id.into(),
            dir: dir.into(),
            pid,
            network,
            door,
            gv: self.clone(),
        })
    }

    /// Takes everything of a sandbox off the host but its directory: the
    /// container, if runsc still has it, and its mounts.
    pub async fn teardown(&self, id: &str, dir: &Path) {
        for network in [false, true] {
            if self.run(network, &["delete", "--force", id]).await.is_ok() {
                break;
            }
        }
        unmount_below(dir).await;
    }
}

impl Container {
    pub fn exited(&self) -> bool {
        self.pid == 0 || !process_alive(self.pid)
    }

    pub async fn kill(&mut self) {
        self.door.close().await;
        let _ = self.gv.run(self.network, &["kill", &self.id, "KILL"]).await;
        let _ = self
            .gv
            .run(self.network, &["delete", "--force", &self.id])
            .await;
        for _ in 0..200 {
            if !process_alive(self.pid) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    /// Writes a checkpoint to the sandbox's `pause/` and stops it. Its
    /// mounts stay for the restore. On failure it keeps running.
    pub async fn checkpoint(&mut self) -> anyhow::Result<()> {
        let image = self.dir.join(IMAGE_DIR);
        let _ = tokio::fs::remove_dir_all(&image).await;
        tokio::fs::create_dir_all(&image).await?;
        self.door.close().await;
        let arg = format!("--image-path={}", image.display());
        let mut last = None;
        // envd may be halfway through a dial when the door closes.
        for wait in [0, 50, 150, 400] {
            tokio::time::sleep(Duration::from_millis(wait)).await;
            match self
                .gv
                .run(self.network, &["checkpoint", &arg, &self.id])
                .await
            {
                Ok(_) => {
                    let _ = self
                        .gv
                        .run(self.network, &["delete", "--force", &self.id])
                        .await;
                    return Ok(());
                }
                Err(e) => last = Some(e),
            }
        }
        let _ = tokio::fs::remove_dir_all(&image).await;
        self.door.reopen().await?;
        Err(last.unwrap_or_else(|| anyhow!("runsc checkpoint failed")))
    }
}

/// `runsc --version`'s first line: "runsc version release-20261109.0".
pub fn parse_version(text: &str) -> Option<String> {
    let line = text.lines().next()?;
    let v = line.strip_prefix("runsc version ")?.trim();
    (!v.is_empty()).then(|| v.to_string())
}

fn last_line(text: &str) -> String {
    text.lines()
        .rev()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("no output")
        .chars()
        .take(400)
        .collect()
}

pub fn resolv_conf(dns: &[Ipv4Addr]) -> String {
    let mut s = String::new();
    for d in dns {
        s.push_str(&format!("nameserver {d}\n"));
    }
    s
}

/// The OCI runtime spec runsc starts the sandbox from.
pub fn oci_spec(m: &Machine<'_>, rootfs: &Path, pyo: &Path, resolv: &Path) -> Value {
    let caps = json!([
        "CAP_CHOWN",
        "CAP_DAC_OVERRIDE",
        "CAP_FOWNER",
        "CAP_FSETID",
        "CAP_KILL",
        "CAP_SETGID",
        "CAP_SETUID",
        "CAP_SETPCAP",
        "CAP_NET_BIND_SERVICE",
        "CAP_NET_RAW",
        "CAP_SYS_CHROOT",
        "CAP_MKNOD",
        "CAP_AUDIT_WRITE",
        "CAP_SETFCAP"
    ]);
    let mut mounts = vec![
        json!({"destination": "/proc", "type": "proc", "source": "proc"}),
        json!({"destination": "/dev", "type": "tmpfs", "source": "tmpfs",
               "options": ["nosuid", "strictatime", "mode=755", "size=65536k"]}),
        json!({"destination": "/dev/pts", "type": "devpts", "source": "devpts",
               "options": ["nosuid", "noexec", "newinstance", "ptmxmode=0666", "mode=0620"]}),
        json!({"destination": "/dev/shm", "type": "tmpfs", "source": "shm",
               "options": ["nosuid", "noexec", "nodev", "mode=1777", "size=65536k"]}),
        json!({"destination": "/sys", "type": "sysfs", "source": "sysfs",
               "options": ["nosuid", "noexec", "nodev", "ro"]}),
        json!({"destination": "/pyo", "type": "bind", "source": pyo,
               "options": ["rbind", "rw"]}),
    ];
    if m.netns.is_some() {
        mounts.push(json!({"destination": "/etc/resolv.conf", "type": "bind",
                           "source": resolv, "options": ["rbind", "ro"]}));
    }
    let network = match m.netns {
        Some(path) => json!({"type": "network", "path": path}),
        None => json!({"type": "network"}),
    };
    let cpus = m.cpus.max(1) as u64;
    json!({
        "ociVersion": "1.0.2",
        "process": {
            "terminal": false,
            "user": {"uid": 0, "gid": 0},
            "args": ["/sbin/pyo-envd"],
            "env": [
                format!("PATH={PATH}"),
                "HOME=/root",
                format!("PYO_ENVD_DIAL={DIAL}"),
            ],
            "cwd": "/",
            "capabilities": {
                "bounding": caps, "effective": caps, "permitted": caps
            },
            "rlimits": [{"type": "RLIMIT_NOFILE", "hard": 1_048_576u64, "soft": 1_048_576u64}],
            "noNewPrivileges": false
        },
        "root": {"path": rootfs, "readonly": false},
        "hostname": "sandbox",
        "mounts": mounts,
        "linux": {
            "namespaces": [
                {"type": "pid"}, {"type": "ipc"}, {"type": "uts"}, {"type": "mount"}, network
            ],
            "cgroupsPath": format!("/{}/{}", m.cgroup_parent, m.id),
            "resources": {
                "memory": {"limit": ((m.memory_mib as u64) << 20) + CGROUP_OVERHEAD},
                "cpu": {"quota": cpus * 100_000, "period": 100_000u64}
            }
        }
    })
}

fn set_mode(p: &Path, mode: u32) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(p, std::fs::Permissions::from_mode(mode))
        .with_context(|| format!("setting the mode of {}", p.display()))
}

async fn mount(opts: &[&str], src: &Path, dst: &Path) -> anyhow::Result<()> {
    let out = Command::new("mount")
        .args(opts)
        .arg(src)
        .arg(dst)
        .stdin(Stdio::null())
        .output()
        .await
        .context("starting mount")?;
    if !out.status.success() {
        bail!(
            "mounting {}: {}",
            dst.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

/// Mount points from /proc/self/mountinfo (field 5, with `\040` and the
/// like decoded).
pub fn mount_points(mountinfo: &str) -> Vec<PathBuf> {
    mountinfo
        .lines()
        .filter_map(|l| l.split(' ').nth(4))
        .map(|p| PathBuf::from(unescape(p)))
        .collect()
}

fn unescape(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\'
            && i + 3 < b.len()
            && b[i + 1..i + 4].iter().all(|c| (b'0'..=b'7').contains(c))
        {
            let v = (b[i + 1] - b'0') * 64 + (b[i + 2] - b'0') * 8 + (b[i + 3] - b'0');
            out.push(v);
            i += 4;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn mounts_now() -> Vec<PathBuf> {
    mount_points(&std::fs::read_to_string("/proc/self/mountinfo").unwrap_or_default())
}

pub fn is_mount(p: &Path) -> bool {
    mounts_now().iter().any(|m| m == p)
}

/// The mounts at or under `dir`, deepest first.
pub fn mounts_below(points: &[PathBuf], dir: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = points
        .iter()
        .filter(|m| m.starts_with(dir))
        .cloned()
        .collect();
    v.sort_by_key(|m| std::cmp::Reverse(m.components().count()));
    v.dedup();
    v
}

/// Unmounts everything at or under `dir`, so removing it never reaches
/// into a template or another disk. A busy mount is detached lazily.
pub async fn unmount_below(dir: &Path) {
    for m in mounts_below(&mounts_now(), dir) {
        let ok = Command::new("umount")
            .arg(&m)
            .stdin(Stdio::null())
            .output()
            .await
            .map(|o| o.status.success())
            .unwrap_or(false);
        if !ok {
            let _ = Command::new("umount").arg("-l").arg(&m).output().await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions() {
        assert_eq!(
            parse_version("runsc version release-20261109.0\nspec: 1.1.0-rc.1\n").as_deref(),
            Some("release-20261109.0")
        );
        assert_eq!(parse_version(""), None);
        assert_eq!(parse_version("something else"), None);
    }

    #[test]
    fn mountinfo_and_what_goes_first() {
        let info = "22 1 8:1 / / rw - ext4 /dev/sda1 rw\n\
                    90 22 7:0 / /var/lib/pyo-sandbox/templates/t/root ro - ext4 /dev/loop0 ro\n\
                    91 22 7:1 / /var/lib/pyo-sandbox/sandboxes/a/scratch rw - ext4 /dev/loop1 rw\n\
                    92 22 0:50 / /var/lib/pyo-sandbox/sandboxes/a/rootfs rw - overlay overlay rw\n\
                    93 92 0:51 / /var/lib/pyo-sandbox/sandboxes/a/rootfs/with\\040space rw - tmpfs t rw\n";
        let points = mount_points(info);
        assert_eq!(points.len(), 5);
        assert!(points.contains(&PathBuf::from(
            "/var/lib/pyo-sandbox/sandboxes/a/rootfs/with space"
        )));
        let below = mounts_below(&points, Path::new("/var/lib/pyo-sandbox/sandboxes/a"));
        assert_eq!(
            below,
            vec![
                PathBuf::from("/var/lib/pyo-sandbox/sandboxes/a/rootfs/with space"),
                PathBuf::from("/var/lib/pyo-sandbox/sandboxes/a/scratch"),
                PathBuf::from("/var/lib/pyo-sandbox/sandboxes/a/rootfs"),
            ]
        );
        // Never the template or a sandbox with a longer name.
        assert!(
            mounts_below(&points, Path::new("/var/lib/pyo-sandbox/sandboxes/"))
                .iter()
                .all(|m| !m.starts_with("/var/lib/pyo-sandbox/templates"))
        );
        assert!(mounts_below(&points, Path::new("/var/lib/pyo-sandbox/sandboxes/ab")).is_empty());
    }

    #[test]
    fn the_spec_runs_envd_in_dial_mode_with_limits() {
        let dns = [Ipv4Addr::new(1, 1, 1, 1)];
        let m = Machine {
            id: "sbx-abc",
            dir: Path::new("/d"),
            template_root: Path::new("/t"),
            scratch: None,
            cpus: 2,
            memory_mib: 512,
            netns: Some("/var/run/netns/pyosbx3"),
            dns: &dns,
            cgroup_parent: "pyo-sandbox",
        };
        let s = oci_spec(
            &m,
            Path::new("/d/rootfs"),
            Path::new("/d/pyo"),
            Path::new("/d/resolv.conf"),
        );
        assert_eq!(s["process"]["args"], json!(["/sbin/pyo-envd"]));
        assert!(
            s["process"]["env"]
                .as_array()
                .unwrap()
                .contains(&json!("PYO_ENVD_DIAL=unix:/pyo/host.sock"))
        );
        assert_eq!(s["root"]["path"], json!("/d/rootfs"));
        assert_eq!(
            s["linux"]["resources"]["memory"]["limit"],
            json!((512u64 << 20) + CGROUP_OVERHEAD)
        );
        assert_eq!(s["linux"]["resources"]["cpu"]["quota"], json!(200_000));
        assert_eq!(s["linux"]["cgroupsPath"], json!("/pyo-sandbox/sbx-abc"));
        let ns = s["linux"]["namespaces"].as_array().unwrap();
        assert!(ns.contains(&json!({"type": "network", "path": "/var/run/netns/pyosbx3"})));
        let mounts = s["mounts"].as_array().unwrap();
        assert!(mounts.iter().any(|x| x["destination"] == "/pyo"));
        assert!(
            mounts
                .iter()
                .any(|x| x["destination"] == "/etc/resolv.conf")
        );

        let none = Machine { netns: None, ..m };
        let s = oci_spec(
            &none,
            Path::new("/d/rootfs"),
            Path::new("/d/pyo"),
            Path::new("/d/resolv.conf"),
        );
        let ns = s["linux"]["namespaces"].as_array().unwrap();
        assert!(ns.contains(&json!({"type": "network"})));
        assert!(
            !s["mounts"]
                .as_array()
                .unwrap()
                .iter()
                .any(|x| x["destination"] == "/etc/resolv.conf")
        );
        assert_eq!(resolv_conf(&dns), "nameserver 1.1.1.1\n");
    }

    /// envd's side of the door, as dial mode does it: connect, then Hello.
    async fn dial(sock: &Path) -> tokio::net::UnixStream {
        use pyo_proto::sandbox::wire::{Frame, Hello, Kind};
        use tokio::io::AsyncWriteExt;
        let mut s = tokio::net::UnixStream::connect(sock).await.unwrap();
        s.write_all(&Frame::json(Kind::Hello, 0, &Hello::ours("0.0.0")).encode())
            .await
            .unwrap();
        s
    }

    /// What a checkpoint needs: the door closes and every connection it
    /// gave out ends, also one still in use, and it opens again after.
    #[tokio::test]
    async fn the_door_cuts_every_connection_on_close() {
        use tokio::io::AsyncReadExt;
        let dir = std::env::temp_dir().join(format!("pyo-gv-door-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let sock = dir.join(HOST_SOCK);
        let door = Door::open(&dir).unwrap();

        let mut envd_side = dial(&sock).await;
        let conn = door.accept(Duration::from_secs(2)).await.unwrap();
        let (_reader, _writer) = conn.into_split();

        door.close().await;
        assert!(!sock.exists(), "no socket while closed");
        let mut buf = [0u8; 8];
        let n = tokio::time::timeout(Duration::from_secs(2), envd_side.read(&mut buf))
            .await
            .expect("envd's end sees the cut")
            .unwrap();
        assert_eq!(n, 0);
        assert!(door.accept(Duration::from_millis(50)).await.is_err());

        door.reopen().await.unwrap();
        let _again = dial(&sock).await;
        door.accept(Duration::from_secs(2)).await.unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn every_command_gets_the_same_flags() {
        let g = Gvisor::new(
            "runsc".into(),
            "/var/lib/pyo-sandbox/runsc".into(),
            "pyo-sandbox".into(),
        );
        let f = g.flags(true);
        assert!(f.contains(&"--platform=systrap".to_string()));
        assert!(f.contains(&"--network=sandbox".to_string()));
        assert!(f.contains(&"--host-uds=open".to_string()));
        assert!(f.contains(&"--overlay2=none".to_string()));
        assert!(g.flags(false).contains(&"--network=none".to_string()));
    }
}
