//! The Firecracker driver: boot, snapshot, restore, pause and stop VMs.
//!
//! Each VM runs in its own directory (see `storage`) and Firecracker only
//! sees relative paths, so one snapshot restores into any directory.
//!
//! - **Dev mode** runs `firecracker` directly as the current user, with no
//!   jailer, no seccomp per jail and no cgroup per VM.
//! - **As root** every VM goes through the `jailer`: a chroot per VM (the VM
//!   directory), its own cgroup with `memory.max`, and Firecracker dropped
//!   to the `pyo-sandbox` user. The daemon itself runs as root because the
//!   jailer, tap devices and nftables need it.
//!
//! VMs are started in their own process group, so restarting the daemon
//! keeps them. On start the daemon adopts the ones that still run.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use anyhow::{Context, anyhow, bail};
use pyo_proto::sandbox::FAST_BOOT_ARGS;
use pyo_proto::sandbox::wire::Request;
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tokio::process::{Child, Command};

use crate::envd;
use crate::storage;

pub const API_SOCK: &str = "api.sock";
pub const VSOCK: &str = "v.sock";
/// Where envd's ready connection lands: `<vsock>_<port>`.
pub const READY_SOCK: &str = "v.sock_1024";
pub const ROOTFS: &str = "rootfs.ext4";
pub const SCRATCH: &str = "scratch.ext4";
pub const KERNEL: &str = "vmlinux";
pub const GUEST_CID: u32 = 3;
/// Memory a VM's cgroup may use past its guest memory: Firecracker itself,
/// page tables and kernel memory.
const CGROUP_OVERHEAD: u64 = 128 << 20;

#[derive(Debug, Clone)]
pub struct Jailer {
    pub jailer: PathBuf,
    pub uid: u32,
    pub gid: u32,
    /// `<root>/jail`. The jailer puts each VM in
    /// `<base>/firecracker/<id>/root`, which is `Layout::vm_dir`.
    pub chroot_base: PathBuf,
    pub cgroup_parent: String,
}

#[derive(Debug, Clone)]
pub struct Firecracker {
    pub firecracker: PathBuf,
    pub kernel: PathBuf,
    pub jailer: Option<Jailer>,
    pub cpu_template: Option<String>,
    /// Guest console into fc.log, for debugging. Slows boot by a few ms.
    pub serial: bool,
}

/// One machine to boot.
#[derive(Debug, Clone)]
pub struct Machine<'a> {
    pub id: &'a str,
    pub dir: &'a Path,
    pub rootfs: &'a Path,
    /// The writable layer to copy in. None boots with the read-only root
    /// only, like the detection test boot.
    pub scratch: Option<&'a Path>,
    pub cpus: u32,
    pub memory_mib: u32,
}

/// A running Firecracker process.
#[derive(Debug)]
pub struct Vm {
    pub id: String,
    pub dir: PathBuf,
    pub pid: u32,
    /// None for a VM adopted after a daemon restart.
    child: Option<Child>,
    /// The VM directory, held open for short socket paths.
    handle: DirHandle,
}

/// An open VM directory. Unix socket paths can't be longer than 108
/// bytes, and VM directories can be deep, especially under the jailer.
/// `/proc/self/fd/<fd>/<name>` reaches the same socket with a short path.
#[derive(Debug)]
pub struct DirHandle(std::fs::File);

impl DirHandle {
    pub fn open(dir: &Path) -> anyhow::Result<DirHandle> {
        std::fs::File::open(dir)
            .map(DirHandle)
            .with_context(|| format!("opening {}", dir.display()))
    }

    pub fn path(&self, name: &str) -> PathBuf {
        use std::os::fd::AsRawFd;
        PathBuf::from(format!("/proc/self/fd/{}/{name}", self.0.as_raw_fd()))
    }
}

impl Vm {
    /// A VM a previous daemon started.
    pub fn adopt(id: &str, dir: &Path, pid: u32) -> anyhow::Result<Vm> {
        Ok(Vm {
            id: id.into(),
            dir: dir.into(),
            pid,
            child: None,
            handle: DirHandle::open(dir)?,
        })
    }

    pub fn vsock(&self) -> PathBuf {
        self.handle.path(VSOCK)
    }

    /// A second handle on the VM directory and the vsock path through it.
    /// Callers that connect after letting go of the VM keep the handle
    /// until they're connected, so the fd number in the path can't be
    /// reused for another VM's directory in between.
    pub fn vsock_pinned(&self) -> anyhow::Result<(DirHandle, PathBuf)> {
        let h = DirHandle(self.handle.0.try_clone()?);
        let p = h.path(VSOCK);
        Ok((h, p))
    }

    pub fn api(&self) -> PathBuf {
        self.handle.path(API_SOCK)
    }

    /// Whether the process has ended.
    pub fn exited(&mut self) -> bool {
        match &mut self.child {
            Some(c) => !matches!(c.try_wait(), Ok(None)),
            None => !process_alive(self.pid),
        }
    }

    pub async fn kill(&mut self) {
        // SAFETY: plain kill on our VM's pid.
        unsafe { libc::kill(self.pid as i32, libc::SIGKILL) };
        match &mut self.child {
            Some(c) => {
                let _ = c.wait().await;
            }
            None => {
                for _ in 0..200 {
                    if !process_alive(self.pid) {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            }
        }
    }

    pub async fn pause(&self) -> anyhow::Result<()> {
        api(
            &self.api(),
            "PATCH",
            "/vm",
            Some(json!({"state": "Paused"})),
        )
        .await
    }

    pub async fn resume(&self) -> anyhow::Result<()> {
        api(
            &self.api(),
            "PATCH",
            "/vm",
            Some(json!({"state": "Resumed"})),
        )
        .await
    }

    /// Pauses the VM and writes a full snapshot into its directory, as
    /// `<name>.snap` and `<name>.mem`. The VM stays paused.
    pub async fn snapshot(&self, name: &str) -> anyhow::Result<()> {
        self.pause().await?;
        api(
            &self.api(),
            "PUT",
            "/snapshot/create",
            Some(json!({
                "snapshot_type": "Full",
                "snapshot_path": format!("{name}.snap"),
                "mem_file_path": format!("{name}.mem"),
            })),
        )
        .await
    }
}

/// Whether a pid is a live process, not a zombie.
pub fn process_alive(pid: u32) -> bool {
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(s) => s
            .rsplit_once(") ")
            .map(|(_, rest)| !rest.starts_with('Z'))
            .unwrap_or(false),
        Err(_) => false,
    }
}

/// Whether a pid is a Firecracker process, for adopting VMs.
pub fn is_firecracker(pid: u32) -> bool {
    std::fs::read(format!("/proc/{pid}/cmdline"))
        .map(|c| String::from_utf8_lossy(&c).contains("firecracker"))
        .unwrap_or(false)
        && process_alive(pid)
}

impl Firecracker {
    /// `v1.18.0` from `firecracker --version`.
    pub async fn version(&self) -> anyhow::Result<String> {
        let out = Command::new(&self.firecracker)
            .arg("--version")
            .output()
            .await?;
        let text = String::from_utf8_lossy(&out.stdout);
        text.split_whitespace()
            .find(|w| w.starts_with('v') && w[1..].starts_with(|c: char| c.is_ascii_digit()))
            .map(str::to_string)
            .ok_or_else(|| anyhow!("can't read the Firecracker version from {text:?}"))
    }

    pub fn boot_args(&self) -> String {
        let args = if self.serial {
            FAST_BOOT_ARGS.replace("8250.nr_uarts=0", "console=ttyS0")
        } else {
            FAST_BOOT_ARGS.into()
        };
        format!("reboot=k panic=1 pci=off ro quiet loglevel=1 {args} init=/sbin/pyo-envd")
    }

    /// The VM config Firecracker boots from. Paths are relative to the VM
    /// directory.
    pub fn vm_config(&self, m: &Machine<'_>) -> serde_json::Value {
        let mut drives = vec![json!({
            "drive_id": "root", "path_on_host": ROOTFS, "is_root_device": true, "is_read_only": true,
        })];
        if m.scratch.is_some() {
            drives.push(json!({
                "drive_id": "scratch", "path_on_host": SCRATCH, "is_root_device": false, "is_read_only": false,
            }));
        }
        let mut machine = json!({"vcpu_count": m.cpus, "mem_size_mib": m.memory_mib});
        if let Some(t) = &self.cpu_template {
            machine["cpu_template"] = json!(t);
        }
        json!({
            "boot-source": {"kernel_image_path": KERNEL, "boot_args": self.boot_args()},
            "drives": drives,
            "machine-config": machine,
            "vsock": {"guest_cid": GUEST_CID, "uds_path": VSOCK},
        })
    }

    /// Puts the root disk, the kernel and the writable layer in the VM
    /// directory.
    async fn prepare(&self, m: &Machine<'_>, with_kernel: bool) -> anyhow::Result<()> {
        tokio::fs::create_dir_all(m.dir).await?;
        let dev = self.jailer.is_none();
        storage::link(m.rootfs, &m.dir.join(ROOTFS), dev)?;
        if with_kernel {
            storage::link(&self.kernel, &m.dir.join(KERNEL), dev)?;
        }
        if let Some(src) = m.scratch {
            storage::copy_sparse(src, &m.dir.join(SCRATCH)).await?;
        }
        for s in [API_SOCK, VSOCK, READY_SOCK] {
            let _ = tokio::fs::remove_file(m.dir.join(s)).await;
        }
        if let Some(j) = &self.jailer {
            // A resume reuses the chroot. The jailer makes its device nodes
            // and copies Firecracker in on every start, and refuses when
            // they're already there.
            let _ = tokio::fs::remove_dir_all(m.dir.join("dev")).await;
            if let Some(name) = self.firecracker.file_name() {
                let _ = tokio::fs::remove_file(m.dir.join(name)).await;
                let mut pid = name.to_os_string();
                pid.push(".pid");
                let _ = tokio::fs::remove_file(m.dir.join(pid)).await;
            }
            chown(m.dir, j.uid, j.gid)?;
            if m.scratch.is_some() {
                chown(&m.dir.join(SCRATCH), j.uid, j.gid)?;
            }
        }
        Ok(())
    }

    fn spawn(&self, m: &Machine<'_>, firecracker_args: &[&str]) -> anyhow::Result<Vm> {
        let instance = storage::instance_id(m.id);
        let log = std::fs::File::create(m.dir.join("fc.log"))?;
        let mut cmd = match &self.jailer {
            None => {
                let mut c = Command::new(&self.firecracker);
                c.current_dir(m.dir)
                    .args(["--api-sock", API_SOCK, "--id", &instance])
                    .args(firecracker_args);
                c
            }
            Some(j) => {
                let mem_max = ((m.memory_mib as u64) << 20) + CGROUP_OVERHEAD;
                let mut c = Command::new(&j.jailer);
                c.args(["--id", &instance])
                    .arg("--exec-file")
                    .arg(&self.firecracker)
                    .args(["--uid", &j.uid.to_string(), "--gid", &j.gid.to_string()])
                    .arg("--chroot-base-dir")
                    .arg(&j.chroot_base)
                    .args(["--cgroup-version", "2", "--parent-cgroup", &j.cgroup_parent])
                    .args(["--cgroup", &format!("memory.max={mem_max}")])
                    .args(["--", "--api-sock", API_SOCK])
                    .args(firecracker_args);
                c
            }
        };
        cmd.stdin(Stdio::null())
            .stdout(log.try_clone()?)
            .stderr(log)
            .process_group(0)
            .kill_on_drop(false);
        let child = cmd
            .spawn()
            .with_context(|| format!("starting {}", self.firecracker.display()))?;
        let pid = child
            .id()
            .ok_or_else(|| anyhow!("Firecracker exited at once"))?;
        Ok(Vm {
            id: m.id.into(),
            dir: m.dir.into(),
            pid,
            child: Some(child),
            handle: DirHandle::open(m.dir)?,
        })
    }

    /// Boots a VM from scratch and waits for envd's ready signal. Returns
    /// the VM and the time from spawning it to `Hello`.
    pub async fn cold_boot(
        &self,
        m: &Machine<'_>,
        timeout: Duration,
    ) -> anyhow::Result<(Vm, Duration)> {
        self.prepare(m, true).await?;
        let config = m.dir.join("vm.json");
        tokio::fs::write(&config, serde_json::to_vec_pretty(&self.vm_config(m))?).await?;
        let ready = UnixListener::bind(DirHandle::open(m.dir)?.path(READY_SOCK))
            .context("listening for envd's ready signal")?;
        if let Some(j) = &self.jailer {
            chown(&m.dir.join(READY_SOCK), j.uid, j.gid)?;
        }
        let t0 = Instant::now();
        let mut vm = self.spawn(m, &["--config-file", "vm.json"])?;
        let waited = tokio::time::timeout(timeout, async {
            let (s, _) = ready.accept().await?;
            envd::Conn::from_ready(s).await
        })
        .await;
        let took = t0.elapsed();
        let _ = tokio::fs::remove_file(m.dir.join(READY_SOCK)).await;
        match waited {
            Ok(Ok(_conn)) => Ok((vm, took)),
            Ok(Err(e)) => {
                vm.kill().await;
                Err(e.context(log_tail(m.dir)))
            }
            Err(_) => {
                vm.kill().await;
                bail!(
                    "the VM wasn't ready within {} ms. {}",
                    timeout.as_millis(),
                    log_tail(m.dir)
                )
            }
        }
    }

    /// Restores a VM from `<snap>.snap` and `<snap>.mem` in its directory and
    /// waits until envd answers. Then sets the guest clock, which stays at
    /// the snapshot's time otherwise. Returns the VM and the time from
    /// spawning it to envd's first answer.
    pub async fn restore(
        &self,
        m: &Machine<'_>,
        snap: &str,
        timeout: Duration,
    ) -> anyhow::Result<(Vm, Duration)> {
        self.prepare(m, false).await?;
        let t0 = Instant::now();
        let mut vm = self.spawn(m, &[])?;
        let res = async {
            wait_for(&vm.api(), timeout).await?;
            api(
                &vm.api(),
                "PUT",
                "/snapshot/load",
                Some(json!({
                    "snapshot_path": format!("{snap}.snap"),
                    "mem_backend": {"backend_type": "File", "backend_path": format!("{snap}.mem")},
                    "resume_vm": true,
                })),
            )
            .await?;
            let mut conn = envd::Conn::connect(&vm.vsock(), timeout).await?;
            let took = t0.elapsed();
            conn.call(Request::SetTime {
                unix_ms: crate::now_ms(),
            })
            .await?;
            anyhow::Ok(took)
        }
        .await;
        match res {
            Ok(took) => Ok((vm, took)),
            Err(e) => {
                vm.kill().await;
                Err(e.context(log_tail(m.dir)))
            }
        }
    }
}

/// The end of fc.log, for errors.
fn log_tail(dir: &Path) -> String {
    let text = std::fs::read_to_string(dir.join("fc.log")).unwrap_or_default();
    let lines: Vec<&str> = text.lines().rev().take(5).collect();
    if lines.is_empty() {
        "Firecracker logged nothing.".into()
    } else {
        format!(
            "Firecracker log: {}",
            lines.into_iter().rev().collect::<Vec<_>>().join(" | ")
        )
    }
}

fn chown(path: &Path, uid: u32, gid: u32) -> anyhow::Result<()> {
    std::os::unix::fs::chown(path, Some(uid), Some(gid))
        .with_context(|| format!("chown {}", path.display()))
}

async fn wait_for(path: &Path, timeout: Duration) -> anyhow::Result<()> {
    let until = Instant::now() + timeout;
    while !path.exists() {
        if Instant::now() > until {
            bail!("{} didn't appear", path.display());
        }
        tokio::time::sleep(Duration::from_micros(200)).await;
    }
    Ok(())
}

/// One call to Firecracker's API over its Unix socket. Plain HTTP/1.1,
/// one connection per call. Fails on any status from 300 up.
pub async fn api(
    sock: &Path,
    method: &str,
    path: &str,
    body: Option<serde_json::Value>,
) -> anyhow::Result<()> {
    let body = body.map(|b| b.to_string()).unwrap_or_default();
    let mut s = UnixStream::connect(sock)
        .await
        .with_context(|| format!("connecting to {}", sock.display()))?;
    let req = format!(
        "{method} {path} HTTP/1.1\r\nHost: localhost\r\nAccept: application/json\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
    s.write_all(req.as_bytes()).await?;
    let (status, text) = read_response(&mut s).await?;
    if !(200..300).contains(&status) {
        bail!("Firecracker {method} {path}: {status} {}", text.trim());
    }
    Ok(())
}

async fn read_response(s: &mut UnixStream) -> anyhow::Result<(u16, String)> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    let head_end = loop {
        let n = s.read(&mut chunk).await?;
        if n == 0 {
            bail!("Firecracker closed the API connection");
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break i + 4;
        }
        if buf.len() > 64 << 10 {
            bail!("Firecracker's API answer has no end of headers");
        }
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
    let status: u16 = head
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| anyhow!("bad API status line {head:?}"))?;
    let len: usize = head
        .lines()
        .find_map(|l| {
            let (k, v) = l.split_once(':')?;
            k.eq_ignore_ascii_case("content-length")
                .then(|| v.trim().parse().ok())?
        })
        .unwrap_or(0);
    while buf.len() < head_end + len {
        let n = s.read(&mut chunk).await?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    let end = buf.len().min(head_end + len);
    Ok((
        status,
        String::from_utf8_lossy(&buf[head_end..end]).into_owned(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fc() -> Firecracker {
        Firecracker {
            firecracker: "/x/firecracker".into(),
            kernel: "/x/vmlinux".into(),
            jailer: None,
            cpu_template: None,
            serial: false,
        }
    }

    #[test]
    fn boot_args_always_skip_the_slow_probe() {
        let a = fc().boot_args();
        assert!(
            a.contains("i8042.noaux i8042.nomux i8042.dumbkbd 8250.nr_uarts=0"),
            "{a}"
        );
        assert!(a.ends_with("init=/sbin/pyo-envd"));
        let s = Firecracker {
            serial: true,
            ..fc()
        }
        .boot_args();
        assert!(
            s.contains("console=ttyS0") && !s.contains("nr_uarts"),
            "{s}"
        );
        assert!(s.contains("i8042.noaux"));
    }

    #[test]
    fn vm_config_uses_relative_paths() {
        let dir = PathBuf::from("/p/sandboxes/sbx_a");
        let m = Machine {
            id: "sbx_a",
            dir: &dir,
            rootfs: Path::new("/p/templates/t/rootfs.ext4"),
            scratch: Some(Path::new("/p/cache/scratch-64.ext4")),
            cpus: 2,
            memory_mib: 512,
        };
        let c = Firecracker {
            cpu_template: Some("T2".into()),
            ..fc()
        }
        .vm_config(&m);
        assert_eq!(c["boot-source"]["kernel_image_path"], "vmlinux");
        assert_eq!(c["drives"][0]["path_on_host"], "rootfs.ext4");
        assert_eq!(c["drives"][0]["is_read_only"], true);
        assert_eq!(c["drives"][1]["path_on_host"], "scratch.ext4");
        assert_eq!(c["machine-config"]["vcpu_count"], 2);
        assert_eq!(c["machine-config"]["cpu_template"], "T2");
        assert_eq!(c["vsock"]["uds_path"], "v.sock");
        let no_scratch = fc().vm_config(&Machine { scratch: None, ..m });
        assert_eq!(no_scratch["drives"].as_array().unwrap().len(), 1);
        assert!(no_scratch["machine-config"].get("cpu_template").is_none());
    }

    #[test]
    fn process_checks() {
        assert!(process_alive(std::process::id()));
        assert!(!process_alive(u32::MAX - 1));
        assert!(!is_firecracker(std::process::id()));
    }

    #[tokio::test]
    async fn api_client_speaks_http() {
        let dir = std::env::temp_dir().join(format!("pyo-sbx-api-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let sock = dir.join("api.sock");
        let l = UnixListener::bind(&sock).unwrap();
        tokio::spawn(async move {
            for answer in [
                "HTTP/1.1 204 \r\nServer: Firecracker API\r\n\r\n",
                "HTTP/1.1 400 \r\nContent-Type: application/json\r\nContent-Length: 27\r\n\r\n{\"fault_message\":\"no way\"}\n",
            ] {
                let (mut s, _) = l.accept().await.unwrap();
                let mut buf = vec![0u8; 4096];
                let n = s.read(&mut buf).await.unwrap();
                let req = String::from_utf8_lossy(&buf[..n]).into_owned();
                assert!(req.starts_with("PATCH /vm HTTP/1.1\r\n"), "{req}");
                assert!(req.ends_with("{\"state\":\"Paused\"}"), "{req}");
                s.write_all(answer.as_bytes()).await.unwrap();
            }
        });
        let body = || Some(json!({"state": "Paused"}));
        api(&sock, "PATCH", "/vm", body()).await.unwrap();
        let e = api(&sock, "PATCH", "/vm", body())
            .await
            .unwrap_err()
            .to_string();
        assert!(e.contains("400") && e.contains("no way"), "{e}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
