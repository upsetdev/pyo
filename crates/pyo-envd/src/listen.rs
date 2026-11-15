//! Listening on vsock or a Unix socket, and the ready connection.

use std::fs::File;
use std::io;
use std::os::fd::{FromRawFd, OwnedFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::time::Duration;

use pyo_proto::sandbox::wire::{ENVD_VSOCK_PORT, HOST_CID, READY_VSOCK_PORT};

use crate::server;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Listen {
    Vsock(u32),
    Unix(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ready {
    Vsock(u32, u32),
    Unix(String),
    None,
}

pub fn parse_listen(s: Option<&str>) -> Result<Listen, String> {
    match s {
        None | Some("") => Ok(Listen::Vsock(ENVD_VSOCK_PORT)),
        Some(s) => {
            if let Some(p) = s.strip_prefix("vsock:") {
                p.parse()
                    .map(Listen::Vsock)
                    .map_err(|_| format!("bad vsock port in {s:?}"))
            } else if let Some(p) = s.strip_prefix("unix:") {
                Ok(Listen::Unix(p.to_string()))
            } else {
                Err(format!(
                    "PYO_ENVD_LISTEN must be vsock:<port> or unix:<path>, not {s:?}"
                ))
            }
        }
    }
}

pub fn parse_ready(s: Option<&str>) -> Result<Ready, String> {
    match s {
        None | Some("") => Ok(Ready::Vsock(HOST_CID, READY_VSOCK_PORT)),
        Some("none") => Ok(Ready::None),
        Some(s) => {
            if let Some(rest) = s.strip_prefix("vsock:") {
                let (cid, port) = rest.split_once(':').ok_or_else(|| format!("bad {s:?}"))?;
                Ok(Ready::Vsock(
                    cid.parse().map_err(|_| format!("bad cid in {s:?}"))?,
                    port.parse().map_err(|_| format!("bad port in {s:?}"))?,
                ))
            } else if let Some(p) = s.strip_prefix("unix:") {
                Ok(Ready::Unix(p.to_string()))
            } else {
                Err(format!(
                    "PYO_ENVD_READY must be vsock:<cid>:<port>, unix:<path> or none, not {s:?}"
                ))
            }
        }
    }
}

/// Listens, sends the ready signal and serves every connection in its own
/// thread. Never returns.
pub fn serve() -> ! {
    let env = |k: &str| std::env::var(k).ok();
    let (listen, ready) = match (
        parse_listen(env("PYO_ENVD_LISTEN").as_deref()),
        parse_ready(env("PYO_ENVD_READY").as_deref()),
    ) {
        (Ok(l), Ok(r)) => (l, r),
        (Err(e), _) | (_, Err(e)) => {
            eprintln!("pyo-envd: {e}");
            std::process::exit(2);
        }
    };
    let accept: Box<dyn FnMut() -> io::Result<File>> = match &listen {
        Listen::Vsock(port) => {
            let fd = vsock_listen(*port).unwrap_or_else(|e| {
                eprintln!("pyo-envd: can't listen on vsock port {port}: {e}");
                std::process::exit(1);
            });
            Box::new(move || vsock_accept(&fd))
        }
        Listen::Unix(path) => {
            let _ = std::fs::remove_file(path);
            let l = UnixListener::bind(path).unwrap_or_else(|e| {
                eprintln!("pyo-envd: can't listen on {path}: {e}");
                std::process::exit(1);
            });
            Box::new(move || l.accept().map(|(s, _)| File::from(OwnedFd::from(s))))
        }
    };
    // The listener is up, so tell the host. The ready connection is served
    // like any other, so the host can use it right away.
    if let Some(conn) = connect_ready(&ready) {
        spawn_conn(conn);
    }
    let mut accept = accept;
    loop {
        match accept() {
            Ok(conn) => spawn_conn(conn),
            Err(e) => {
                eprintln!("pyo-envd: accept failed: {e}");
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    }
}

fn spawn_conn(conn: File) {
    std::thread::spawn(move || {
        let Ok(reader) = conn.try_clone() else { return };
        server::Server::guest().serve(Box::new(reader), Box::new(conn));
    });
}

fn connect_ready(ready: &Ready) -> Option<File> {
    // A few tries, in case the host's listener isn't there yet.
    for _ in 0..20 {
        let r = match ready {
            Ready::None => return None,
            Ready::Vsock(cid, port) => vsock_connect(*cid, *port),
            Ready::Unix(p) => UnixStream::connect(p).map(|s| File::from(OwnedFd::from(s))),
        };
        match r {
            Ok(f) => return Some(f),
            Err(_) => std::thread::sleep(Duration::from_millis(50)),
        }
    }
    eprintln!("pyo-envd: can't reach the host for the ready signal");
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

    #[test]
    fn settings() {
        assert_eq!(parse_listen(None), Ok(Listen::Vsock(52)));
        assert_eq!(parse_listen(Some("vsock:99")), Ok(Listen::Vsock(99)));
        assert_eq!(
            parse_listen(Some("unix:/run/e.sock")),
            Ok(Listen::Unix("/run/e.sock".into()))
        );
        assert!(parse_listen(Some("tcp:1")).is_err());
        assert_eq!(parse_ready(None), Ok(Ready::Vsock(2, 1024)));
        assert_eq!(parse_ready(Some("none")), Ok(Ready::None));
        assert_eq!(parse_ready(Some("vsock:3:5")), Ok(Ready::Vsock(3, 5)));
        assert!(parse_ready(Some("vsock:3")).is_err());
    }
}
