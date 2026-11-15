//! Machine setup when envd is PID 1.
//!
//! The root disk is the template's, shared by every sandbox and read-only.
//! The second disk (`/dev/vdb`) is the sandbox's own sparse writable layer.
//! PID 1 mounts it, puts an overlay of both in place of `/`, mounts the
//! usual filesystems inside and switches into it. Without a second disk, as
//! in the daemon's test boot, it keeps the read-only root.

use std::ffi::CString;
use std::path::Path;

const SCRATCH_DEV: &str = "/dev/vdb";
/// Where the writable disk is mounted before the switch. The template build
/// makes sure `/mnt` exists. `/tmp` and `/run` are tried after it.
const SCRATCH_DIRS: &[&str] = &["/mnt", "/tmp", "/run"];

pub fn setup() {
    if !Path::new(SCRATCH_DEV).exists() {
        // The kernel mounts devtmpfs itself when it's built with
        // DEVTMPFS_MOUNT. Mount it when it didn't.
        mount("devtmpfs", "/dev", "devtmpfs", 0, "");
    }
    let root = match writable_root() {
        Some(r) => r,
        None => "/".to_string(),
    };
    let p = |s: &str| format!("{}{}", root.trim_end_matches('/'), s);
    let nosuid = libc::MS_NOSUID;
    let noexec = libc::MS_NOEXEC | libc::MS_NODEV;
    mount("proc", &p("/proc"), "proc", nosuid | noexec, "");
    mount("sysfs", &p("/sys"), "sysfs", nosuid | noexec, "");
    mount(
        "cgroup2",
        &p("/sys/fs/cgroup"),
        "cgroup2",
        nosuid | noexec,
        "",
    );
    mount("devtmpfs", &p("/dev"), "devtmpfs", nosuid, "mode=0755");
    mount(
        "devpts",
        &p("/dev/pts"),
        "devpts",
        nosuid | libc::MS_NOEXEC,
        "gid=5,mode=620,ptmxmode=666",
    );
    mount(
        "tmpfs",
        &p("/dev/shm"),
        "tmpfs",
        nosuid | libc::MS_NODEV,
        "mode=1777",
    );
    mount(
        "tmpfs",
        &p("/tmp"),
        "tmpfs",
        nosuid | libc::MS_NODEV,
        "mode=1777",
    );
    mount(
        "tmpfs",
        &p("/run"),
        "tmpfs",
        nosuid | libc::MS_NODEV,
        "mode=0755",
    );
    if root != "/" {
        switch_root(&root);
    }
    // SAFETY: the setters run before any other thread starts.
    unsafe {
        std::env::set_var(
            "PATH",
            "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin",
        );
        std::env::set_var("HOME", "/root");
        std::env::set_var("LANG", "C.UTF-8");
        std::env::set_var("TERM", "xterm-256color");
    }
    let name = b"sandbox";
    // SAFETY: valid pointer and length.
    unsafe { libc::sethostname(name.as_ptr().cast(), name.len()) };
    loopback_up();
    let _ = std::env::set_current_dir("/root").or_else(|_| std::env::set_current_dir("/"));
}

/// Mounts the scratch disk and an overlay on it. Returns the new root.
fn writable_root() -> Option<String> {
    if !Path::new(SCRATCH_DEV).exists() {
        return None;
    }
    let dir = SCRATCH_DIRS.iter().find(|d| Path::new(d).is_dir())?;
    if !mount(SCRATCH_DEV, dir, "ext4", libc::MS_NOATIME, "") {
        return None;
    }
    let (upper, work, root) = (
        format!("{dir}/upper"),
        format!("{dir}/work"),
        format!("{dir}/root"),
    );
    for d in [&upper, &work, &root] {
        let _ = std::fs::create_dir_all(d);
    }
    let opts = format!("lowerdir=/,upperdir={upper},workdir={work}");
    mount("overlay", &root, "overlay", 0, &opts).then_some(root)
}

/// Moves `root` to `/` and chroots into it, like switch_root.
fn switch_root(root: &str) {
    if std::env::set_current_dir(root).is_err() {
        eprintln!("pyo-envd: can't enter {root}");
        return;
    }
    let dot = CString::new(".").expect("no NUL");
    let slash = CString::new("/").expect("no NUL");
    // SAFETY: valid C strings.
    unsafe {
        if libc::mount(
            dot.as_ptr(),
            slash.as_ptr(),
            std::ptr::null(),
            libc::MS_MOVE,
            std::ptr::null(),
        ) != 0
        {
            eprintln!(
                "pyo-envd: moving the root failed: {}",
                std::io::Error::last_os_error()
            );
        }
        if libc::chroot(dot.as_ptr()) != 0 {
            eprintln!(
                "pyo-envd: chroot failed: {}",
                std::io::Error::last_os_error()
            );
        }
    }
    let _ = std::env::set_current_dir("/");
}

/// Mounts and logs a failure. Returns whether it worked.
fn mount(src: &str, target: &str, fstype: &str, flags: libc::c_ulong, data: &str) -> bool {
    let _ = std::fs::create_dir_all(target);
    let c = |s: &str| CString::new(s).expect("no NUL");
    let (s, t, f, d) = (c(src), c(target), c(fstype), c(data));
    let data_ptr = if data.is_empty() {
        std::ptr::null()
    } else {
        d.as_ptr().cast()
    };
    // SAFETY: valid C strings that live across the call.
    let r = unsafe { libc::mount(s.as_ptr(), t.as_ptr(), f.as_ptr(), flags, data_ptr) };
    if r != 0 {
        eprintln!(
            "pyo-envd: mount {target} ({fstype}) failed: {}",
            std::io::Error::last_os_error()
        );
    }
    r == 0
}

/// Brings up `lo`, so port forwarding can reach services on 127.0.0.1.
fn loopback_up() {
    #[repr(C)]
    struct IfReq {
        name: [u8; 16],
        flags: libc::c_short,
        _pad: [u8; 22],
    }
    let mut req = IfReq {
        name: [0; 16],
        flags: 0,
        _pad: [0; 22],
    };
    req.name[..2].copy_from_slice(b"lo");
    // SAFETY: a plain socket and two ioctls on a properly sized ifreq.
    unsafe {
        let fd = libc::socket(libc::AF_INET, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, 0);
        if fd < 0 {
            return;
        }
        if libc::ioctl(fd, libc::SIOCGIFFLAGS as libc::Ioctl, &mut req) == 0 {
            req.flags |= libc::IFF_UP as libc::c_short;
            libc::ioctl(fd, libc::SIOCSIFFLAGS as libc::Ioctl, &req);
        }
        libc::close(fd);
    }
}
