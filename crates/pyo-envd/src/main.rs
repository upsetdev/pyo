//! pyo-envd: the agent inside every Pyo sandbox.
//!
//! In a Firecracker VM the kernel starts it as PID 1 (`init=/sbin/pyo-envd`).
//! PID 1 sets up the machine (see [`init`]), starts the server as its child
//! and then only reaps orphans, restarting the server if it dies.
//!
//! The server listens on vsock port 52, or on a Unix socket on gVisor. When
//! it's up it connects to the host and sends `Hello`, which is the ready
//! signal. Every connection speaks `pyo_proto::sandbox::wire`.
//!
//! Settings come from the environment, which the kernel fills from boot
//! args like `PYO_ENVD_LISTEN=unix:/run/envd.sock`:
//! - `PYO_ENVD_LISTEN`: `vsock:<port>` (default `vsock:52`) or `unix:<path>`
//! - `PYO_ENVD_READY`: `vsock:<cid>:<port>` (default `vsock:2:1024`),
//!   `unix:<path>` or `none`

mod init;
mod listen;
mod server;

use std::process::Command;
use std::time::Duration;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

fn main() {
    let arg = std::env::args().nth(1);
    match arg.as_deref() {
        Some("--version") => println!("pyo-envd {VERSION}"),
        Some("serve") => listen::serve(),
        _ if std::process::id() == 1 => pid1(),
        _ => listen::serve(),
    }
}

/// PID 1: set up the machine, run the server, reap everything.
fn pid1() -> ! {
    init::setup();
    let mut server = spawn_server();
    loop {
        let mut status = 0;
        // SAFETY: plain waitpid on any child.
        let pid = unsafe { libc::waitpid(-1, &mut status, 0) };
        if pid < 0 {
            std::thread::sleep(Duration::from_millis(200));
            if server.is_none() {
                server = spawn_server();
            }
            continue;
        }
        if Some(pid as u32) == server.as_ref().map(|c| c.id()) {
            eprintln!("pyo-envd: the server exited ({status}), starting it again");
            // The child was reaped above, so forget it without waiting.
            std::mem::forget(server.take());
            std::thread::sleep(Duration::from_millis(100));
            server = spawn_server();
        }
    }
}

fn spawn_server() -> Option<std::process::Child> {
    match Command::new("/proc/self/exe").arg("serve").spawn() {
        Ok(c) => Some(c),
        Err(e) => {
            eprintln!("pyo-envd: can't start the server: {e}");
            None
        }
    }
}
