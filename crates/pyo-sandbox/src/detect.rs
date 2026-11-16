//! Runtime detection, run on every daemon start. If KVM is lost the server
//! becomes gVisor only.
//!
//! 1. vmx or svm in the CPU flags, `/dev/kvm` opens, KVM_GET_API_VERSION is
//!    12, KVM_CREATE_VM works, a host kernel of 5.10 or newer, cgroup v2,
//!    `/dev/net/tun` and nftables.
//! 2. A test boot: a tiny microVM with a 5 s timeout, booted with the same
//!    fast boot args as every sandbox. Under 300 ms is native KVM, slower is
//!    nested. The threshold only holds with those args.
//! 3. Without KVM: `runsc --platform=systrap --network=none do true` as
//!    root, which proves gVisor runs. The report names the runsc release.
//! 4. Neither: the server is refused with a plain reason.
//!
//! `/dev/net/tun`, nftables and another firewall's FORWARD policy only
//! decide whether sandboxes can get a network, not the runtime.

use std::os::fd::AsRawFd;
use std::path::Path;
use std::time::{Duration, Instant};

use pyo_proto::sandbox::{
    CapabilityReport, Check, Kvm, MIN_HOST_KERNEL, Runtime, TEST_BOOT_TIMEOUT_MS, classify_boot,
};
use tokio::process::Command;

use crate::config::is_root;
use crate::firecracker::{Firecracker, Machine};
use crate::storage::{self, Layout};

const KVM_GET_API_VERSION: libc::c_ulong = 0xAE00;
const KVM_CREATE_VM: libc::c_ulong = 0xAE01;
const KVM_API_VERSION: i32 = 12;

pub struct Inputs<'a> {
    pub fc: &'a Firecracker,
    pub layout: &'a Layout,
    pub envd: &'a Path,
    pub runsc: &'a Path,
}

pub async fn detect(inputs: Inputs<'_>) -> CapabilityReport {
    let cpuinfo = std::fs::read_to_string("/proc/cpuinfo").unwrap_or_default();
    let mut r = CapabilityReport {
        arch: std::env::consts::ARCH.into(),
        cpus: std::thread::available_parallelism()
            .map(|n| n.get() as u32)
            .unwrap_or(1),
        memory: crate::capacity::meminfo().total,
        disk: storage::free_bytes(&inputs.layout.root),
        cpu_vendor: cpu_vendor(&cpuinfo),
        public_ips: public_ips(),
        ..Default::default()
    };
    let mut check = |name: &str, ok: bool, detail: String| {
        r.checks.push(Check {
            name: name.into(),
            ok,
            detail,
        });
        ok
    };

    let flag = virt_flag(&cpuinfo);
    // aarch64 has no such flag. KVM_CREATE_VM decides there.
    let flag_ok = flag.is_some() || std::env::consts::ARCH != "x86_64";
    check("cpuVirtualization", flag_ok, flag.unwrap_or("none").into());
    let kvm = kvm_checks();
    let kvm_ok = match &kvm {
        Ok(()) => check(
            "kvm",
            true,
            "/dev/kvm opens, API version 12, a VM can be created".into(),
        ),
        Err(e) => check("kvm", false, e.clone()),
    };
    let (major, minor) = host_kernel();
    let kernel_ok = (major, minor) >= MIN_HOST_KERNEL;
    check("hostKernel", kernel_ok, format!("{major}.{minor}"));
    let cgroup2 = Path::new("/sys/fs/cgroup/cgroup.controllers").exists();
    check(
        "cgroupV2",
        cgroup2,
        if cgroup2 { "yes" } else { "no" }.into(),
    );
    let tun = rw_access(Path::new("/dev/net/tun"));
    check(
        "tun",
        tun,
        if tun {
            "/dev/net/tun is usable"
        } else {
            "/dev/net/tun is missing or not usable"
        }
        .into(),
    );
    let nft = Command::new("nft")
        .arg("--version")
        .output()
        .await
        .map(|o| o.status.success())
        .unwrap_or(false);
    check(
        "nftables",
        nft,
        if nft { "nft found" } else { "nft not found" }.into(),
    );
    // Another firewall's FORWARD policy that drops sandbox traffic. The
    // daemon lets taps through with iptables (see network.rs), so it's
    // only a problem without iptables.
    if let Some(why) = crate::network::forward_unhandled().await {
        check("forwarding", false, why);
    } else if let Some(chain) = crate::network::forward_state()
        .await
        .and_then(|s| s.chain())
    {
        check(
            "forwarding",
            true,
            format!("The FORWARD policy is DROP. The daemon lets sandbox taps through in {chain}."),
        );
    }
    // Set by the daemon once its nftables table is in place. See network.rs.
    r.network = false;

    if flag_ok && kvm_ok && kernel_ok && cgroup2 {
        match inputs.fc.version().await {
            Ok(v) => {
                r.firecracker_version = v;
                match test_boot(&inputs).await {
                    Ok(ms) => {
                        r.runtime = Runtime::Firecracker;
                        r.kvm = classify_boot(ms);
                        r.boot_ms = Some(ms);
                        r.checks.push(Check {
                            name: "testBoot".into(),
                            ok: true,
                            detail: format!("{ms} ms"),
                        });
                        return r;
                    }
                    Err(e) => r.checks.push(Check {
                        name: "testBoot".into(),
                        ok: false,
                        detail: format!("{e:#}"),
                    }),
                }
            }
            Err(e) => r.checks.push(Check {
                name: "firecracker".into(),
                ok: false,
                detail: format!("{e:#}"),
            }),
        }
    }

    match gvisor(inputs.runsc).await {
        Ok(version) => {
            r.runtime = Runtime::Gvisor;
            r.kvm = Kvm::None;
            r.checks.push(Check {
                name: "gvisor".into(),
                ok: true,
                detail: format!("runsc {version} works on systrap"),
            });
            r.gvisor_version = version;
        }
        Err(e) => {
            r.checks.push(Check {
                name: "gvisor".into(),
                ok: false,
                detail: e.clone(),
            });
            r.runtime = Runtime::None;
            r.kvm = Kvm::None;
            r.reason = Some(refusal(kvm_ok, &e));
        }
    }
    r
}

fn refusal(kvm_ok: bool, gvisor_err: &str) -> String {
    let kvm = if kvm_ok {
        "Firecracker didn't boot a test VM"
    } else {
        "This server has no usable KVM"
    };
    format!("{kvm}, and gVisor can't run either: {gvisor_err}.")
}

fn virt_flag(cpuinfo: &str) -> Option<&'static str> {
    let flags = cpuinfo.lines().find(|l| l.starts_with("flags"))?;
    let words: Vec<&str> = flags.split_whitespace().collect();
    if words.contains(&"vmx") {
        Some("vmx")
    } else if words.contains(&"svm") {
        Some("svm")
    } else {
        None
    }
}

fn cpu_vendor(cpuinfo: &str) -> String {
    cpuinfo
        .lines()
        .find_map(|l| l.strip_prefix("vendor_id"))
        .and_then(|rest| rest.split(':').nth(1))
        .map(|v| v.trim().to_string())
        .unwrap_or_default()
}

fn kvm_checks() -> Result<(), String> {
    let kvm = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/kvm")
        .map_err(|e| format!("/dev/kvm doesn't open: {e}"))?;
    // SAFETY: KVM ioctls with no argument on our own fd.
    let api = unsafe { libc::ioctl(kvm.as_raw_fd(), KVM_GET_API_VERSION as _, 0) };
    if api != KVM_API_VERSION {
        return Err(format!("KVM API version is {api}, not 12"));
    }
    // SAFETY: as above. The VM fd is closed right away.
    let vm = unsafe { libc::ioctl(kvm.as_raw_fd(), KVM_CREATE_VM as _, 0) };
    if vm < 0 {
        return Err(format!(
            "KVM_CREATE_VM failed: {}",
            std::io::Error::last_os_error()
        ));
    }
    // SAFETY: closing the fd KVM_CREATE_VM returned.
    unsafe { libc::close(vm) };
    Ok(())
}

pub fn host_kernel() -> (u32, u32) {
    let release = std::fs::read_to_string("/proc/sys/kernel/osrelease").unwrap_or_default();
    parse_kernel(&release)
}

fn parse_kernel(release: &str) -> (u32, u32) {
    let mut parts = release.trim().split(|c: char| !c.is_ascii_digit());
    let major = parts.next().and_then(|p| p.parse().ok()).unwrap_or(0);
    let minor = parts.next().and_then(|p| p.parse().ok()).unwrap_or(0);
    (major, minor)
}

fn rw_access(p: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt;
    let Ok(c) = std::ffi::CString::new(p.as_os_str().as_bytes()) else {
        return false;
    };
    // SAFETY: access on a valid C string.
    unsafe { libc::access(c.as_ptr(), libc::R_OK | libc::W_OK) == 0 }
}

/// Boots a tiny VM with only envd on a read-only root and times it.
async fn test_boot(inputs: &Inputs<'_>) -> anyhow::Result<u32> {
    let cache = inputs.layout.cache();
    tokio::fs::create_dir_all(&cache).await?;
    let image = cache.join("testboot.ext4");
    build_test_image(inputs.envd, &image).await?;
    let id = format!("testboot-{}", std::process::id());
    let dir = inputs.layout.vm_dir(&id);
    let m = Machine {
        id: &id,
        dir: &dir,
        rootfs: &image,
        scratch: None,
        cpus: 1,
        memory_mib: 128,
        nic: None,
    };
    let res = inputs
        .fc
        .cold_boot(&m, Duration::from_millis(TEST_BOOT_TIMEOUT_MS as u64))
        .await;
    let out = match res {
        Ok((mut vm, took)) => {
            vm.kill().await;
            Ok(took.as_millis() as u32)
        }
        Err(e) => Err(e),
    };
    let _ = tokio::fs::remove_dir_all(inputs.layout.sandbox_top(&id)).await;
    out
}

/// A 16 MiB root with envd and the mount points it uses.
async fn build_test_image(envd: &Path, out: &Path) -> anyhow::Result<()> {
    let t = Instant::now();
    let dir = out.with_extension(format!("d{}", std::process::id()));
    let _ = tokio::fs::remove_dir_all(&dir).await;
    for d in ["sbin", "proc", "sys", "dev", "tmp", "run", "root", "mnt"] {
        tokio::fs::create_dir_all(dir.join(d)).await?;
    }
    tokio::fs::copy(envd, dir.join("sbin/pyo-envd")).await?;
    let tmp = out.with_extension("tmp");
    let r = storage::ext4_from_dir(&dir, &tmp, 16, "testboot").await;
    let _ = tokio::fs::remove_dir_all(&dir).await;
    r?;
    tokio::fs::rename(&tmp, out).await?;
    tracing::debug!(
        ms = t.elapsed().as_millis() as u64,
        "built the test boot image"
    );
    Ok(())
}

/// Whether runsc runs a sandbox here, and its release.
async fn gvisor(runsc: &Path) -> Result<String, String> {
    if !is_root() {
        return Err("runsc needs root".into());
    }
    let run = Command::new(runsc)
        .args(["--platform=systrap", "--network=none", "do", "true"])
        .kill_on_drop(true)
        .output();
    match tokio::time::timeout(Duration::from_secs(30), run).await {
        Err(_) => Err("runsc timed out".into()),
        Ok(Err(e)) if e.kind() == std::io::ErrorKind::NotFound => {
            Err("runsc isn't installed".into())
        }
        Ok(Err(e)) => Err(format!("runsc didn't start: {e}")),
        Ok(Ok(o)) if o.status.success() => {
            let v = Command::new(runsc).arg("--version").output().await;
            Ok(v.ok()
                .and_then(|o| crate::gvisor::parse_version(&String::from_utf8_lossy(&o.stdout)))
                .unwrap_or_else(|| "unknown".into()))
        }
        Ok(Ok(o)) => Err(format!(
            "runsc failed: {}",
            String::from_utf8_lossy(&o.stderr).trim()
        )),
    }
}

/// The addresses the default routes leave from, when they're public: what
/// a DNS record for the edge points at. Connecting a UDP socket sends
/// nothing.
pub fn public_ips() -> Vec<std::net::IpAddr> {
    use std::net::{IpAddr, UdpSocket};
    let from = |bind: &str, to: &str| -> Option<IpAddr> {
        let s = UdpSocket::bind(bind).ok()?;
        s.connect(to).ok()?;
        Some(s.local_addr().ok()?.ip())
    };
    [
        from("0.0.0.0:0", "1.1.1.1:53"),
        from("[::]:0", "[2606:4700:4700::1111]:53"),
    ]
    .into_iter()
    .flatten()
    .filter(|ip| is_public(*ip))
    .collect()
}

/// An address the internet can reach: not private, shared (CGNAT, which
/// tailnets use), loopback, link-local, documentation or unique-local.
pub fn is_public(ip: std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(v) => {
            let [a, b, ..] = v.octets();
            !(v.is_private()
                || v.is_loopback()
                || v.is_link_local()
                || v.is_unspecified()
                || v.is_broadcast()
                || v.is_documentation()
                || v.is_multicast()
                || a == 0
                || a >= 240
                || (a == 100 && (64..128).contains(&b))
                || (a == 198 && (18..20).contains(&b)))
        }
        std::net::IpAddr::V6(v) => {
            let first = v.segments()[0];
            // Global unicast is 2000::/3, without documentation (2001:db8::/32).
            (first & 0xe000) == 0x2000 && !(first == 0x2001 && v.segments()[1] == 0x0db8)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn public_addresses() {
        for ip in ["203.0.114.7", "8.8.8.8", "2a01:4f8::1"] {
            assert!(is_public(ip.parse().unwrap()), "{ip}");
        }
        for ip in [
            "10.0.0.1",
            "192.168.1.1",
            "100.100.1.1",
            "127.0.0.1",
            "169.254.1.1",
            "198.18.0.1",
            "203.0.113.5",
            "::1",
            "fd7a:115c:a1e0::1",
            "fe80::1",
            "2001:db8::1",
        ] {
            assert!(!is_public(ip.parse().unwrap()), "{ip}");
        }
    }

    #[test]
    fn cpu_flags_and_vendor() {
        let info = "vendor_id\t: GenuineIntel\nflags\t\t: fpu vme vmx sse\n";
        assert_eq!(virt_flag(info), Some("vmx"));
        assert_eq!(virt_flag("flags : fpu svm\n"), Some("svm"));
        assert_eq!(virt_flag("flags : fpu vmxx\n"), None);
        assert_eq!(cpu_vendor(info), "GenuineIntel");
    }

    #[test]
    fn kernel_versions() {
        assert_eq!(parse_kernel("7.0.0-43-generic\n"), (7, 0));
        assert_eq!(parse_kernel("5.10.0"), (5, 10));
        assert!(parse_kernel("5.4.0") < MIN_HOST_KERNEL);
        assert!(parse_kernel("6.1") >= MIN_HOST_KERNEL);
        assert_eq!(parse_kernel(""), (0, 0));
    }

    #[test]
    fn refusal_is_one_plain_sentence() {
        let r = refusal(false, "runsc isn't installed");
        assert_eq!(
            r,
            "This server has no usable KVM, and gVisor can't run either: runsc isn't installed."
        );
        assert!(!r.contains(';'));
    }
}
