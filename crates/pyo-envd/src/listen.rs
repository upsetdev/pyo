//! How envd meets the host. Two modes, and `Hello` is the first frame on
//! every connection in both.
//!
//! - **Listen** (Firecracker): listen on a vsock port. Once listening, dial
//!   the host once and send `Hello`. That's the ready signal, and the ready
//!   connection is then served like any other. After a snapshot restore no
//!   one dials, so the daemon connects and retries.
//! - **Dial** (gVisor): never listen. A bound host socket blocks
//!   `runsc checkpoint`, so envd dials a per-sandbox host Unix socket
//!   (`runsc --host-uds=open`) and keeps retrying while nobody answers. It
//!   keeps one idle connection waiting: when the host sends the first frame
//!   on it, or it closes, envd dials the next one. The daemon closes its
//!   listener and every connection before a checkpoint, and envd just goes
//!   back to retrying.

use std::fs::File;
use std::io;
use std::os::fd::{FromRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::sync::mpsc;
use std::time::Duration;

use pyo_proto::sandbox::wire::{ENVD_VSOCK_PORT, HOST_CID, READY_VSOCK_PORT};

use crate::server;

/// Where envd dials.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    Vsock(u32, u32),
    Unix(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mode {
    /// Listen on this vsock port and send the ready signal to `ready`.
    Listen { port: u32, ready: Option<Target> },
    /// Only dial.
    Dial(Target),
}

impl Mode {
    /// Whether envd sets up the machine as PID 1. On gVisor, runsc builds
    /// the root filesystem and mounts, so it doesn't.
    pub fn sets_up_machine(&self) -> bool {
        matches!(self, Mode::Listen { .. })
    }
}

/// From `PYO_ENVD_LISTEN`, `PYO_ENVD_READY` and `PYO_ENVD_DIAL`.
pub fn mode(listen: Option<&str>, ready: Option<&str>, dial: Option<&str>) -> Result<Mode, String> {
    if let Some(d) = dial.filter(|d| !d.is_empty()) {
        if listen.is_some_and(|l| !l.is_empty()) {
            return Err("set PYO_ENVD_LISTEN or PYO_ENVD_DIAL, not both".into());
        }
        return parse_target(d).map(Mode::Dial);
    }
    let port = match listen.filter(|l| !l.is_empty()) {
        None => ENVD_VSOCK_PORT,
        Some(l) => l
            .strip_prefix("vsock:")
            .and_then(|p| p.parse().ok())
            .ok_or_else(|| format!("PYO_ENVD_LISTEN must be vsock:<port>, not {l:?}"))?,
    };
    let ready = match ready.filter(|r| !r.is_empty()) {
        None => Some(Target::Vsock(HOST_CID, READY_VSOCK_PORT)),
        Some("none") => None,
        Some(r) => Some(parse_target(r)?),
    };
    Ok(Mode::Listen { port, ready })
}

fn parse_target(s: &str) -> Result<Target, String> {
    if let Some(rest) = s.strip_prefix("vsock:") {
        let (cid, port) = rest
            .split_once(':')
            .ok_or_else(|| format!("{s:?} needs vsock:<cid>:<port>"))?;
        Ok(Target::Vsock(
            cid.parse().map_err(|_| format!("bad cid in {s:?}"))?,
            port.parse().map_err(|_| format!("bad port in {s:?}"))?,
        ))
    } else if let Some(p) = s.strip_prefix("unix:") {
        Ok(Target::Unix(p.to_string()))
    } else {
        Err(format!("{s:?} must be vsock:<cid>:<port> or unix:<path>"))
    }
}

pub fn mode_from_env() -> Mode {
    let env = |k: &str| std::env::var(k).ok();
    mode(
        env("PYO_ENVD_LISTEN").as_deref(),
        env("PYO_ENVD_READY").as_deref(),
        env("PYO_ENVD_DIAL").as_deref(),
    )
    .unwrap_or_else(|e| {
        eprintln!("pyo-envd: {e}");
        std::process::exit(2);
    })
}

/// Serves connections in the given mode. Never returns.
pub fn serve(mode: Mode) -> ! {
    match mode {
        Mode::Listen { port, ready } => listen(port, ready),
        Mode::Dial(target) => dial_forever(&target),
    }
}

fn listen(port: u32, ready: Option<Target>) -> ! {
    let fd = vsock_listen(port).unwrap_or_else(|e| {
        eprintln!("pyo-envd: can't listen on vsock port {port}: {e}");
        std::process::exit(1);
    });
    // The listener is up, so tell the host.
    if let Some(t) = ready {
        match dial_with_retry(&t, 20) {
            Some(conn) => {
                std::thread::spawn(move || serve_conn(conn, None));
            }
            None => eprintln!("pyo-envd: can't reach the host for the ready signal"),
        }
    }
    loop {
        match vsock_accept(&fd) {
            Ok(conn) => {
                std::thread::spawn(move || serve_conn(conn, None));
            }
            Err(e) => {
                eprintln!("pyo-envd: accept failed: {e}");
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    }
}

fn dial_forever(target: &Target) -> ! {
    loop {
        let Some(conn) = dial_with_retry(target, usize::MAX) else {
            continue;
        };
        let (tx, rx) = mpsc::channel::<()>();
        std::thread::spawn(move || {
            let first: Box<dyn FnOnce() + Send> = Box::new(move || {
                let _ = tx.send(());
            });
            serve_conn(conn, Some(first));
        });
        // Ok when the host started using it, Err when it closed unused.
        // Either way, dial the next one.
        let _ = rx.recv();
    }
}

fn serve_conn(conn: File, on_first: Option<Box<dyn FnOnce() + Send>>) {
    let Ok(reader) = conn.try_clone() else { return };
    server::Server::guest().serve_with(Box::new(reader), Box::new(conn), on_first);
}

/// Dials up to `tries` times, backing off from 2 ms to 50 ms.
fn dial_with_retry(target: &Target, tries: usize) -> Option<File> {
    let mut wait = Duration::from_millis(2);
    for _ in 0..tries {
        let r = match target {
            Target::Vsock(cid, port) => vsock_connect(*cid, *port),
            Target::Unix(p) => UnixStream::connect(p).map(|s| File::from(OwnedFd::from(s))),
        };
        match r {
            Ok(f) => return Some(f),
            Err(_) => {
                std::thread::sleep(wait);
                wait = (wait * 2).min(Duration::from_millis(50));
            }
        }
    }
    None
}

fn vsock_addr(cid: u32, port: u32) -> libc::sockaddr_vm {
    // SAFETY: sockaddr_vm is plain old data, all zeroes is valid.
    let mut a: libc::sockaddr_vm = unsafe { std::mem::zeroed() };
    a.svm_family = libc::AF_VSOCK as libc::sa_family_t;
    a.svm_port = port;
    a.svm_cid = cid;
    a
}

const VM_LEN: libc::socklen_t = std::mem::size_of::<libc::sockaddr_vm>() as libc::socklen_t;

fn vsock_socket() -> io::Result<OwnedFd> {
    // SAFETY: socket returns a new fd or -1.
    let fd = unsafe { libc::socket(libc::AF_VSOCK, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: fd is a fresh descriptor we own.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

fn vsock_listen(port: u32) -> io::Result<OwnedFd> {
    use std::os::fd::AsRawFd;
    let fd = vsock_socket()?;
    let a = vsock_addr(libc::VMADDR_CID_ANY, port);
    // SAFETY: a valid sockaddr_vm and its length.
    unsafe {
        if libc::bind(
            fd.as_raw_fd(),
            (&a as *const libc::sockaddr_vm).cast(),
            VM_LEN,
        ) != 0
            || libc::listen(fd.as_raw_fd(), 128) != 0
        {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(fd)
}

fn vsock_accept(fd: &OwnedFd) -> io::Result<File> {
    use std::os::fd::AsRawFd;
    // SAFETY: accept4 on a listening socket with no address out.
    let c = unsafe {
        libc::accept4(
            fd.as_raw_fd(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            libc::SOCK_CLOEXEC,
        )
    };
    if c < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: c is a fresh descriptor we own.
    Ok(unsafe { File::from_raw_fd(c) })
}

fn vsock_connect(cid: u32, port: u32) -> io::Result<File> {
    use std::os::fd::AsRawFd;
    let fd = vsock_socket()?;
    let a = vsock_addr(cid, port);
    // SAFETY: a valid sockaddr_vm and its length.
    if unsafe {
        libc::connect(
            fd.as_raw_fd(),
            (&a as *const libc::sockaddr_vm).cast(),
            VM_LEN,
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(File::from(fd))
}

#[cfg(test)]
mod tests {
    use super::*;
    use pyo_proto::sandbox::wire::{Frame, Kind, Request, Response, read_frame, write_frame};
    use std::os::unix::net::UnixListener;

    #[test]
    fn modes() {
        assert_eq!(
            mode(None, None, None),
            Ok(Mode::Listen {
                port: 52,
                ready: Some(Target::Vsock(2, 1024))
            })
        );
        assert_eq!(
            mode(Some("vsock:99"), Some("none"), None),
            Ok(Mode::Listen {
                port: 99,
                ready: None
            })
        );
        assert_eq!(
            mode(None, None, Some("unix:/pyo/host.sock")),
            Ok(Mode::Dial(Target::Unix("/pyo/host.sock".into())))
        );
        assert!(!mode(None, None, Some("unix:/x")).unwrap().sets_up_machine());
        assert!(mode(None, None, None).unwrap().sets_up_machine());
        // envd never listens on a Unix socket.
        assert!(mode(Some("unix:/x"), None, None).is_err());
        assert!(mode(Some("vsock:1"), None, Some("unix:/x")).is_err());
        assert!(mode(None, Some("vsock:3"), None).is_err());
    }

    /// Dial mode against a host socket: Hello first on each connection, one
    /// idle connection waiting, and redialing after the host drops all of
    /// them, like before a gVisor checkpoint.
    #[test]
    fn dial_mode_keeps_one_connection_waiting() {
        let dir = std::env::temp_dir().join(format!("pyo-envd-dial-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let sock = dir.join("host.sock");
        // Nobody listens yet. envd keeps retrying.
        let target = Target::Unix(sock.to_string_lossy().into());
        std::thread::spawn(move || dial_forever(&target));
        std::thread::sleep(Duration::from_millis(30));

        let accept_hello = |l: &UnixListener| {
            let (mut c, _) = l.accept().unwrap();
            c.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            let f = read_frame(&mut c).unwrap().unwrap();
            assert_eq!(f.kind, Kind::Hello);
            c
        };
        let l = UnixListener::bind(&sock).unwrap();
        let mut a = accept_hello(&l);
        // Unused, so no second connection yet.
        l.set_nonblocking(true).unwrap();
        std::thread::sleep(Duration::from_millis(50));
        assert!(
            l.accept().is_err(),
            "envd dialed again before the first was used"
        );
        l.set_nonblocking(false).unwrap();
        // Using it makes envd dial the next one.
        write_frame(&mut a, &Frame::request(0, &Request::Ping)).unwrap();
        assert_eq!(
            read_frame(&mut a)
                .unwrap()
                .unwrap()
                .parse::<Response>()
                .unwrap(),
            Response::Pong
        );
        let b = accept_hello(&l);

        // The host closes everything and the socket, then comes back.
        drop((a, b, l));
        std::fs::remove_file(&sock).unwrap();
        std::thread::sleep(Duration::from_millis(30));
        let l = UnixListener::bind(&sock).unwrap();
        let _c = accept_hello(&l);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
