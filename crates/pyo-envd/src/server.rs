//! One connection: reads frames, runs requests, writes frames back.
//!
//! The logic only needs a reader and a writer, so the tests run it on the
//! host over a Unix socket pair.
//!
//! - Every connection starts with `Hello` on stream 0.
//! - Commands run in their own process group, so a signal reaches the whole
//!   tree. They keep running when the connection drops. Their output is
//!   read and thrown away then, so they never block on a full pipe.
//! - Terminals get SIGHUP when the connection drops, like any terminal.
//! - Data for a process or port goes through a writer thread per stream, so
//!   a process that doesn't read its stdin never stalls the connection.

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpStream};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, UNIX_EPOCH};

use pyo_proto::sandbox::wire::{
    CHUNK, ErrorKind, ExecSpec, FileEntry, FileKind, Frame, Hello, Kind, PtySpec, Request,
    Response, read_frame, write_frame,
};

/// How long output may still arrive after a command exits, from children
/// that hold its pipes, before `exit` is sent.
const DRAIN_AFTER_EXIT: Duration = Duration::from_millis(200);
/// The most entries `list` returns.
const MAX_LIST: usize = 100_000;

pub struct Server {
    /// Relative paths and commands without a `cwd` start here.
    home: PathBuf,
}

impl Server {
    pub fn guest() -> Server {
        Server {
            home: PathBuf::from("/root"),
        }
    }

    #[cfg(test)]
    pub fn with_home(home: impl Into<PathBuf>) -> Server {
        Server { home: home.into() }
    }

    /// Serves one connection until it ends.
    pub fn serve(&self, mut reader: Box<dyn Read + Send>, writer: Box<dyn Write + Send>) {
        let conn = Conn {
            out: Out(Arc::new(Mutex::new(writer))),
            streams: Arc::new(Mutex::new(HashMap::new())),
            home: self.home.clone(),
        };
        if !conn
            .out
            .send(&Frame::json(Kind::Hello, 0, &Hello::ours(crate::VERSION)))
        {
            return;
        }
        loop {
            match read_frame(&mut reader) {
                Ok(Some(f)) => conn.handle(f),
                Ok(None) => break,
                Err(e) => {
                    eprintln!("pyo-envd: dropping a connection: {e}");
                    break;
                }
            }
        }
        conn.close();
    }
}

/// The shared writer. Frames are written whole under the lock.
#[derive(Clone)]
struct Out(Arc<Mutex<Box<dyn Write + Send>>>);

impl Out {
    fn send(&self, f: &Frame) -> bool {
        let mut w = self.0.lock().unwrap_or_else(|p| p.into_inner());
        write_frame(&mut *w, f).is_ok() && w.flush().is_ok()
    }

    /// Sends unless the stream was closed. Checked under the writer lock,
    /// so nothing follows the frame that closed a stream.
    fn send_open(&self, closed: &AtomicBool, f: &Frame) -> bool {
        let mut w = self.0.lock().unwrap_or_else(|p| p.into_inner());
        if closed.load(Ordering::SeqCst) {
            return false;
        }
        write_frame(&mut *w, f).is_ok() && w.flush().is_ok()
    }

    /// Sends the last frame of a stream.
    fn send_last(&self, closed: &AtomicBool, f: &Frame) {
        let mut w = self.0.lock().unwrap_or_else(|p| p.into_inner());
        closed.store(true, Ordering::SeqCst);
        let _ = write_frame(&mut *w, f).and_then(|_| w.flush());
    }

    fn respond(&self, stream: u32, r: &Response) -> bool {
        self.send(&Frame::response(stream, r))
    }
}

/// Bytes for a writer thread. `None` closes it.
type Input = Sender<Option<Vec<u8>>>;

enum Stream {
    Exec {
        stdin: Option<Input>,
        pid: i32,
    },
    Pty {
        input: Input,
        master: Arc<File>,
        pid: i32,
    },
    WriteFile {
        file: Option<File>,
        bytes: u64,
        error: Option<io::Error>,
    },
    Port {
        input: Option<Input>,
        closed: Arc<AtomicU8>,
    },
    /// A read in flight. Holds the stream number.
    Busy,
}

type Streams = Arc<Mutex<HashMap<u32, Stream>>>;

struct Conn {
    out: Out,
    streams: Streams,
    home: PathBuf,
}

fn lock(s: &Streams) -> std::sync::MutexGuard<'_, HashMap<u32, Stream>> {
    s.lock().unwrap_or_else(|p| p.into_inner())
}

impl Conn {
    fn handle(&self, f: Frame) {
        match f.kind {
            Kind::Request => match f.parse::<Request>() {
                Ok(req) => self.request(f.stream, req),
                Err(e) => {
                    self.out.respond(
                        f.stream,
                        &Response::error(ErrorKind::InvalidInput, e.to_string()),
                    );
                }
            },
            Kind::Stdin => self.input(f.stream, f.payload),
            Kind::Eof => self.eof(f.stream),
            // Only envd sends the rest.
            _ => {}
        }
    }

    fn request(&self, id: u32, req: Request) {
        if req.opens_stream() && !matches!(req, Request::Ping) {
            if id == 0 {
                self.out.respond(
                    id,
                    &Response::error(ErrorKind::InvalidInput, "stream 0 is the connection"),
                );
                return;
            }
            if lock(&self.streams).contains_key(&id) {
                self.out.respond(
                    id,
                    &Response::error(ErrorKind::Busy, format!("stream {id} is in use")),
                );
                return;
            }
        }
        let res = match req {
            Request::Ping => Some(Response::Pong),
            Request::Exec(spec) => self.exec(id, spec).err(),
            Request::Pty(spec) => self.pty(id, spec).err(),
            Request::Resize { cols, rows } => {
                if let Some(Stream::Pty { master, .. }) = lock(&self.streams).get(&id) {
                    let _ = set_winsize(master, cols, rows);
                }
                None
            }
            Request::Signal { signal } => {
                let pid = match lock(&self.streams).get(&id) {
                    Some(Stream::Exec { pid, .. } | Stream::Pty { pid, .. }) => Some(*pid),
                    _ => None,
                };
                if let Some(pid) = pid {
                    signal_group(pid, signal);
                }
                None
            }
            Request::WriteFile { path, mode, append } => {
                let stream = match self.open_write(&path, mode, append) {
                    Ok(file) => Stream::WriteFile {
                        file: Some(file),
                        bytes: 0,
                        error: None,
                    },
                    Err(e) => Stream::WriteFile {
                        file: None,
                        bytes: 0,
                        error: Some(e),
                    },
                };
                lock(&self.streams).insert(id, stream);
                None
            }
            Request::ReadFile { path } => self.read_file(id, &path).err(),
            Request::List { path } => {
                Some(self.list(&path).unwrap_or_else(|e| Response::io_error(&e)))
            }
            Request::Stat { path } => Some(
                stat(&self.resolve(&path))
                    .map(|entry| Response::Entry { entry })
                    .unwrap_or_else(|e| Response::io_error(&e)),
            ),
            Request::Mkdir { path } => Some(ok_or(fs::create_dir_all(self.resolve(&path)))),
            Request::Remove { path, recursive } => {
                Some(ok_or(remove(&self.resolve(&path), recursive)))
            }
            Request::PortConnect { port } => self.port(id, port).err(),
            Request::SetTime { unix_ms } => Some(ok_or(set_time(unix_ms))),
        };
        if let Some(r) = res {
            self.out.respond(id, &r);
        }
    }

    fn resolve(&self, path: &str) -> PathBuf {
        let p = Path::new(path);
        if p.is_absolute() {
            p.to_path_buf()
        } else {
            self.home.join(p)
        }
    }

    fn cwd(&self, cwd: Option<&str>) -> PathBuf {
        match cwd {
            Some(c) => self.resolve(c),
            None if self.home.is_dir() => self.home.clone(),
            None => PathBuf::from("/"),
        }
    }

    fn exec(&self, id: u32, spec: ExecSpec) -> Result<(), Response> {
        let argv = spec.cmd.argv();
        let Some((prog, args)) = argv.split_first() else {
            return Err(Response::error(
                ErrorKind::InvalidInput,
                "the command is empty",
            ));
        };
        let mut cmd = Command::new(prog);
        cmd.args(args)
            .envs(&spec.env)
            .current_dir(self.cwd(spec.cwd.as_deref()))
            .stdin(if spec.stdin {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0);
        let mut child = cmd.spawn().map_err(|e| Response::io_error(&e))?;
        let pid = child.id() as i32;
        let stdin = child.stdin.take().map(|s| writer_thread(s, drop));
        lock(&self.streams).insert(id, Stream::Exec { stdin, pid });
        self.out.respond(id, &Response::Started { pid: pid as u32 });

        let (out, streams) = (self.out.clone(), self.streams.clone());
        let closed = Arc::new(AtomicBool::new(false));
        let (done_tx, done_rx) = mpsc::channel();
        for (pipe, kind) in [
            (
                child
                    .stdout
                    .take()
                    .map(|p| Box::new(p) as Box<dyn Read + Send>),
                Kind::Stdout,
            ),
            (
                child
                    .stderr
                    .take()
                    .map(|p| Box::new(p) as Box<dyn Read + Send>),
                Kind::Stderr,
            ),
        ] {
            let Some(pipe) = pipe else { continue };
            let (out, closed, done) = (out.clone(), closed.clone(), done_tx.clone());
            std::thread::spawn(move || {
                pump(pipe, |data| {
                    out.send_open(&closed, &Frame::new(kind, id, data.to_vec()));
                });
                let _ = done.send(());
            });
        }
        drop(done_tx);
        std::thread::spawn(move || {
            let status = child.wait();
            // Let the pipes drain. Background children may hold them open,
            // so don't wait for them forever.
            let until = Instant::now() + DRAIN_AFTER_EXIT;
            for _ in 0..2 {
                let left = until.saturating_duration_since(Instant::now());
                if done_rx.recv_timeout(left).is_err() {
                    break;
                }
            }
            out.send_last(&closed, &Frame::response(id, &exit_response(status)));
            lock(&streams).remove(&id);
        });
        Ok(())
    }

    fn pty(&self, id: u32, spec: PtySpec) -> Result<(), Response> {
        let io_err = |e: io::Error| Response::io_error(&e);
        let (master, slave) = open_pty(spec.cols, spec.rows).map_err(io_err)?;
        let argv = match &spec.cmd {
            Some(c) => c.argv(),
            None => vec![login_shell(), "-l".into()],
        };
        let Some((prog, args)) = argv.split_first() else {
            return Err(Response::error(
                ErrorKind::InvalidInput,
                "the command is empty",
            ));
        };
        let mut cmd = Command::new(prog);
        cmd.args(args)
            .env("TERM", "xterm-256color")
            .envs(&spec.env)
            .current_dir(self.cwd(spec.cwd.as_deref()))
            .stdin(Stdio::from(slave.try_clone().map_err(io_err)?))
            .stdout(Stdio::from(slave.try_clone().map_err(io_err)?))
            .stderr(Stdio::from(slave));
        // SAFETY: only async-signal-safe calls between fork and exec.
        unsafe {
            cmd.pre_exec(|| {
                if libc::setsid() < 0 || libc::ioctl(0, libc::TIOCSCTTY as libc::Ioctl, 0) < 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let spawned = cmd.spawn();
        // Drop the parent's copies of the terminal's other end, or reads on
        // the master never end.
        drop(cmd);
        let mut child = spawned.map_err(io_err)?;
        let pid = child.id() as i32;
        let master = Arc::new(master);
        let input = writer_thread(master.try_clone().map_err(io_err)?, drop);
        lock(&self.streams).insert(
            id,
            Stream::Pty {
                input,
                master: master.clone(),
                pid,
            },
        );
        self.out.respond(id, &Response::Started { pid: pid as u32 });

        let (out, streams) = (self.out.clone(), self.streams.clone());
        let closed = Arc::new(AtomicBool::new(false));
        std::thread::spawn(move || {
            // Reads end with EIO once every process on the terminal is gone.
            pump(&*master, |data| {
                out.send_open(&closed, &Frame::new(Kind::Stdout, id, data.to_vec()));
            });
            let status = child.wait();
            out.send_last(&closed, &Frame::response(id, &exit_response(status)));
            lock(&streams).remove(&id);
        });
        Ok(())
    }

    fn open_write(&self, path: &str, mode: Option<u32>, append: bool) -> io::Result<File> {
        let p = self.resolve(path);
        if let Some(dir) = p.parent() {
            fs::create_dir_all(dir)?;
        }
        let file = OpenOptions::new()
            .write(true)
            .create(true)
            .append(append)
            .truncate(!append)
            .mode(mode.unwrap_or(0o644))
            .open(&p)?;
        if let Some(m) = mode {
            file.set_permissions(fs::Permissions::from_mode(m))?;
        }
        Ok(file)
    }

    fn read_file(&self, id: u32, path: &str) -> Result<(), Response> {
        let p = self.resolve(path);
        let entry = stat(&p).map_err(|e| Response::io_error(&e))?;
        if entry.kind == FileKind::Dir {
            return Err(Response::error(
                ErrorKind::InvalidInput,
                format!("{path} is a directory"),
            ));
        }
        let file = File::open(&p).map_err(|e| Response::io_error(&e))?;
        lock(&self.streams).insert(id, Stream::Busy);
        self.out.respond(id, &Response::Entry { entry });
        let (out, streams) = (self.out.clone(), self.streams.clone());
        std::thread::spawn(move || {
            let closed = AtomicBool::new(false);
            let mut failed = None;
            let mut file = file;
            let mut buf = vec![0u8; CHUNK];
            loop {
                match file.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        if !out.send(&Frame::new(Kind::Stdout, id, buf[..n].to_vec())) {
                            break;
                        }
                    }
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                    Err(e) => {
                        failed = Some(e);
                        break;
                    }
                }
            }
            match failed {
                Some(e) => out.send_last(&closed, &Frame::response(id, &Response::io_error(&e))),
                None => out.send_last(&closed, &Frame::eof(id)),
            }
            lock(&streams).remove(&id);
        });
        Ok(())
    }

    fn list(&self, path: &str) -> io::Result<Response> {
        let dir = self.resolve(path);
        let mut entries = Vec::new();
        for e in fs::read_dir(&dir)?.take(MAX_LIST) {
            let e = e?;
            if let Ok(entry) = stat(&e.path()) {
                entries.push(entry);
            }
        }
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(Response::Entries { entries })
    }

    fn port(&self, id: u32, port: u16) -> Result<(), Response> {
        let addrs: [SocketAddr; 2] = [
            ([127, 0, 0, 1], port).into(),
            (std::net::Ipv6Addr::LOCALHOST, port).into(),
        ];
        let mut last = None;
        let mut tcp = None;
        for a in addrs {
            match TcpStream::connect_timeout(&a, Duration::from_secs(3)) {
                Ok(s) => {
                    tcp = Some(s);
                    break;
                }
                Err(e) => last = Some(e),
            }
        }
        let tcp = match tcp {
            Some(t) => t,
            None => {
                let e = last.unwrap_or_else(|| io::ErrorKind::ConnectionRefused.into());
                return Err(Response::error(
                    ErrorKind::from(&e),
                    format!("nothing answers on port {port} in the sandbox: {e}"),
                ));
            }
        };
        let _ = tcp.set_nodelay(true);
        let to_guest = tcp.try_clone().map_err(|e| Response::io_error(&e))?;
        let input = writer_thread(to_guest, |s| {
            let _ = s.shutdown(Shutdown::Write);
        });
        let closed = Arc::new(AtomicU8::new(0));
        lock(&self.streams).insert(
            id,
            Stream::Port {
                input: Some(input),
                closed: closed.clone(),
            },
        );
        self.out.respond(id, &Response::Ok);
        let (out, streams) = (self.out.clone(), self.streams.clone());
        std::thread::spawn(move || {
            let mut tcp = tcp;
            let mut buf = vec![0u8; CHUNK];
            loop {
                match tcp.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if !out.send(&Frame::new(Kind::Stdout, id, buf[..n].to_vec())) {
                            break;
                        }
                    }
                }
            }
            out.send(&Frame::eof(id));
            if closed.fetch_add(1, Ordering::SeqCst) == 1 {
                lock(&streams).remove(&id);
            }
        });
        Ok(())
    }

    fn input(&self, id: u32, data: Vec<u8>) {
        let mut streams = lock(&self.streams);
        match streams.get_mut(&id) {
            Some(Stream::Exec {
                stdin: Some(tx), ..
            }) => {
                let _ = tx.send(Some(data));
            }
            Some(Stream::Pty { input, .. }) => {
                let _ = input.send(Some(data));
            }
            Some(Stream::Port {
                input: Some(tx), ..
            }) => {
                let _ = tx.send(Some(data));
            }
            Some(Stream::WriteFile { file, bytes, error }) => {
                if let (Some(f), None) = (file.as_mut(), error.as_ref()) {
                    match f.write_all(&data) {
                        Ok(()) => *bytes += data.len() as u64,
                        Err(e) => *error = Some(e),
                    }
                }
            }
            _ => {}
        }
    }

    fn eof(&self, id: u32) {
        let mut streams = lock(&self.streams);
        match streams.get_mut(&id) {
            Some(Stream::Exec { stdin, .. }) => {
                if let Some(tx) = stdin.take() {
                    let _ = tx.send(None);
                }
            }
            Some(Stream::Port { input, closed }) => {
                if let Some(tx) = input.take() {
                    let _ = tx.send(None);
                }
                if closed.fetch_add(1, Ordering::SeqCst) == 1 {
                    streams.remove(&id);
                }
            }
            Some(Stream::WriteFile { .. }) => {
                let Some(Stream::WriteFile { file, bytes, error }) = streams.remove(&id) else {
                    return;
                };
                drop(streams);
                let res = match (file, error) {
                    (_, Some(e)) => Response::io_error(&e),
                    (Some(mut f), None) => match f.flush() {
                        Ok(()) => Response::Written { bytes },
                        Err(e) => Response::io_error(&e),
                    },
                    (None, None) => Response::error(ErrorKind::Io, "the file isn't open"),
                };
                self.out.respond(id, &res);
            }
            _ => {}
        }
    }

    /// The connection is gone. Terminals hang up, everything else lets go.
    fn close(&self) {
        let mut streams = lock(&self.streams);
        for (_, s) in streams.drain() {
            if let Stream::Pty { pid, .. } = s {
                signal_group(pid, libc::SIGHUP);
            }
        }
    }
}

/// Copies from `r` to `send` until EOF. When `send` can't deliver, the
/// caller's closure drops the data, and the pump keeps reading so the
/// writer on the other end never blocks.
fn pump<R: Read>(mut r: R, mut send: impl FnMut(&[u8])) {
    let mut buf = vec![0u8; CHUNK];
    loop {
        match r.read(&mut buf) {
            Ok(0) => return,
            Ok(n) => send(&buf[..n]),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => return,
        }
    }
}

/// A thread that writes what it's sent to `w`. `None`, or the sender going
/// away, ends it, and `done` gets the writer.
fn writer_thread<W: Write + Send + 'static>(
    mut w: W,
    done: impl FnOnce(W) + Send + 'static,
) -> Input {
    let (tx, rx) = mpsc::channel::<Option<Vec<u8>>>();
    std::thread::spawn(move || {
        while let Ok(Some(data)) = rx.recv() {
            if w.write_all(&data).and_then(|_| w.flush()).is_err() {
                break;
            }
        }
        done(w);
    });
    tx
}

fn exit_response(status: io::Result<ExitStatus>) -> Response {
    match status {
        Ok(s) => Response::Exit {
            code: s.code(),
            signal: s.signal(),
        },
        Err(e) => Response::io_error(&e),
    }
}

fn ok_or(r: io::Result<()>) -> Response {
    match r {
        Ok(()) => Response::Ok,
        Err(e) => Response::io_error(&e),
    }
}

fn signal_group(pid: i32, signal: i32) {
    // SAFETY: kill with a group, then the single process as a fallback.
    unsafe {
        if libc::kill(-pid, signal) != 0 {
            libc::kill(pid, signal);
        }
    }
}

fn remove(p: &Path, recursive: bool) -> io::Result<()> {
    let m = fs::symlink_metadata(p)?;
    if m.is_dir() {
        if recursive {
            fs::remove_dir_all(p)
        } else {
            fs::remove_dir(p)
        }
    } else {
        fs::remove_file(p)
    }
}

fn stat(p: &Path) -> io::Result<FileEntry> {
    let m = fs::symlink_metadata(p)?;
    let ft = m.file_type();
    let kind = if ft.is_dir() {
        FileKind::Dir
    } else if ft.is_file() {
        FileKind::File
    } else if ft.is_symlink() {
        FileKind::Symlink
    } else {
        FileKind::Other
    };
    let modified = m
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    Ok(FileEntry {
        name: p
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "/".into()),
        path: p.to_string_lossy().into_owned(),
        kind,
        size: m.len(),
        mode: m.mode() & 0o7777,
        modified,
        target: if ft.is_symlink() {
            fs::read_link(p)
                .ok()
                .map(|t| t.to_string_lossy().into_owned())
        } else {
            None
        },
    })
}

fn set_time(unix_ms: u64) -> io::Result<()> {
    let ts = libc::timespec {
        tv_sec: (unix_ms / 1000) as _,
        tv_nsec: ((unix_ms % 1000) * 1_000_000) as libc::c_long,
    };
    // SAFETY: a valid timespec.
    if unsafe { libc::clock_settime(libc::CLOCK_REALTIME, &ts) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn login_shell() -> String {
    ["/bin/bash", "/bin/ash", "/bin/sh"]
        .iter()
        .find(|s| Path::new(s).exists())
        .unwrap_or(&"/bin/sh")
        .to_string()
}

fn open_pty(cols: u16, rows: u16) -> io::Result<(File, File)> {
    let ws = libc::winsize {
        ws_row: rows.max(1),
        ws_col: cols.max(1),
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let (mut m, mut s) = (-1, -1);
    // SAFETY: openpty fills both fds. Both get CLOEXEC so other children
    // don't inherit them.
    unsafe {
        if libc::openpty(&mut m, &mut s, std::ptr::null_mut(), std::ptr::null(), &ws) != 0 {
            return Err(io::Error::last_os_error());
        }
        libc::fcntl(m, libc::F_SETFD, libc::FD_CLOEXEC);
        libc::fcntl(s, libc::F_SETFD, libc::FD_CLOEXEC);
        Ok((
            File::from(OwnedFd::from_raw_fd(m)),
            File::from(OwnedFd::from_raw_fd(s)),
        ))
    }
}

fn set_winsize(master: &File, cols: u16, rows: u16) -> io::Result<()> {
    let ws = libc::winsize {
        ws_row: rows.max(1),
        ws_col: cols.max(1),
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // SAFETY: TIOCSWINSZ with a valid winsize on our own fd.
    if unsafe { libc::ioctl(master.as_raw_fd(), libc::TIOCSWINSZ as libc::Ioctl, &ws) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use pyo_proto::sandbox::wire::Cmd;
    use std::collections::BTreeMap;
    use std::os::unix::net::UnixStream;

    struct TempDir(PathBuf);
    impl TempDir {
        fn new(name: &str) -> TempDir {
            let p =
                std::env::temp_dir().join(format!("pyo-envd-test-{name}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&p);
            fs::create_dir_all(&p).unwrap();
            TempDir(p)
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    struct Client {
        s: UnixStream,
    }

    impl Client {
        fn start(home: &Path) -> Client {
            let (a, b) = UnixStream::pair().unwrap();
            let home = home.to_path_buf();
            std::thread::spawn(move || {
                let r = b.try_clone().unwrap();
                Server::with_home(home).serve(Box::new(r), Box::new(b));
            });
            a.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
            let mut c = Client { s: a };
            let hello = c.next();
            assert_eq!(hello.kind, Kind::Hello);
            assert_eq!(hello.stream, 0);
            let h: Hello = hello.parse().unwrap();
            assert_eq!(h.protocol, pyo_proto::sandbox::wire::WIRE_PROTOCOL);
            c
        }
        fn send(&mut self, f: Frame) {
            write_frame(&mut self.s, &f).unwrap();
        }
        fn req(&mut self, id: u32, r: Request) {
            self.send(Frame::request(id, &r));
        }
        fn next(&mut self) -> Frame {
            read_frame(&mut self.s).unwrap().expect("a frame")
        }
        fn response(&mut self) -> Response {
            let f = self.next();
            assert_eq!(f.kind, Kind::Response, "{f:?}");
            f.parse().unwrap()
        }
        /// Runs an exec on `id` and collects its output until exit.
        fn collect(&mut self, id: u32) -> (Vec<u8>, Vec<u8>, Response) {
            let (mut out, mut err) = (Vec::new(), Vec::new());
            loop {
                let f = self.next();
                assert_eq!(f.stream, id);
                match f.kind {
                    Kind::Stdout => out.extend(f.payload),
                    Kind::Stderr => err.extend(f.payload),
                    Kind::Response => {
                        let r: Response = f.parse().unwrap();
                        if !matches!(r, Response::Started { .. }) {
                            return (out, err, r);
                        }
                    }
                    k => panic!("unexpected {k:?}"),
                }
            }
        }
    }

    fn exec(cmd: &str) -> Request {
        Request::Exec(ExecSpec {
            cmd: Cmd::Shell(cmd.into()),
            cwd: None,
            env: BTreeMap::new(),
            stdin: false,
        })
    }

    #[test]
    fn ping() {
        let t = TempDir::new("ping");
        let mut c = Client::start(&t.0);
        c.req(0, Request::Ping);
        assert_eq!(c.response(), Response::Pong);
    }

    #[test]
    fn exec_streams_output_and_exit_code() {
        let t = TempDir::new("exec");
        let mut c = Client::start(&t.0);
        c.req(1, exec("echo out; echo err >&2; pwd; exit 3"));
        let (out, err, exit) = c.collect(1);
        assert_eq!(
            String::from_utf8_lossy(&out),
            format!("out\n{}\n", t.0.display())
        );
        assert_eq!(err, b"err\n");
        assert_eq!(
            exit,
            Response::Exit {
                code: Some(3),
                signal: None
            }
        );
    }

    #[test]
    fn exec_with_env_cwd_and_argv() {
        let t = TempDir::new("env");
        let mut c = Client::start(&t.0);
        let mut env = BTreeMap::new();
        env.insert("GREETING".to_string(), "hello".to_string());
        c.req(
            1,
            Request::Exec(ExecSpec {
                cmd: Cmd::Argv(vec![
                    "/bin/sh".into(),
                    "-c".into(),
                    "echo $GREETING; pwd".into(),
                ]),
                cwd: Some("/".into()),
                env,
                stdin: false,
            }),
        );
        let (out, _, exit) = c.collect(1);
        assert_eq!(out, b"hello\n/\n");
        assert_eq!(
            exit,
            Response::Exit {
                code: Some(0),
                signal: None
            }
        );
    }

    #[test]
    fn exec_missing_program_is_not_found() {
        let t = TempDir::new("missing");
        let mut c = Client::start(&t.0);
        c.req(
            1,
            Request::Exec(ExecSpec {
                cmd: Cmd::Argv(vec!["/no/such/program".into()]),
                cwd: None,
                env: BTreeMap::new(),
                stdin: false,
            }),
        );
        assert!(matches!(
            c.response(),
            Response::Error {
                kind: ErrorKind::NotFound,
                ..
            }
        ));
    }

    #[test]
    fn exec_stdin() {
        let t = TempDir::new("stdin");
        let mut c = Client::start(&t.0);
        c.req(
            1,
            Request::Exec(ExecSpec {
                cmd: Cmd::Shell("tr a-z A-Z".into()),
                cwd: None,
                env: BTreeMap::new(),
                stdin: true,
            }),
        );
        c.send(Frame::new(Kind::Stdin, 1, b"hello ".to_vec()));
        c.send(Frame::new(Kind::Stdin, 1, b"world".to_vec()));
        c.send(Frame::eof(1));
        let (out, _, exit) = c.collect(1);
        assert_eq!(out, b"HELLO WORLD");
        assert_eq!(
            exit,
            Response::Exit {
                code: Some(0),
                signal: None
            }
        );
    }

    #[test]
    fn signal_kills_the_process_group() {
        let t = TempDir::new("signal");
        let mut c = Client::start(&t.0);
        c.req(1, exec("sleep 30 & sleep 30; echo never"));
        assert!(matches!(c.response(), Response::Started { .. }));
        c.req(
            1,
            Request::Signal {
                signal: libc::SIGKILL,
            },
        );
        let start = Instant::now();
        let (out, _, exit) = c.collect(1);
        assert!(out.is_empty());
        assert_eq!(
            exit,
            Response::Exit {
                code: None,
                signal: Some(libc::SIGKILL)
            }
        );
        assert!(start.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn background_children_dont_hold_up_exit() {
        let t = TempDir::new("bg");
        let mut c = Client::start(&t.0);
        let start = Instant::now();
        c.req(1, exec("sleep 5 & echo started"));
        let (out, _, exit) = c.collect(1);
        assert_eq!(out, b"started\n");
        assert_eq!(
            exit,
            Response::Exit {
                code: Some(0),
                signal: None
            }
        );
        assert!(start.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn concurrent_streams_and_busy_ids() {
        let t = TempDir::new("busy");
        let mut c = Client::start(&t.0);
        c.req(1, exec("sleep 0.3; echo one"));
        assert!(matches!(c.response(), Response::Started { .. }));
        c.req(1, exec("echo dup"));
        let f = c.next();
        assert_eq!(f.stream, 1);
        assert!(matches!(
            f.parse::<Response>().unwrap(),
            Response::Error {
                kind: ErrorKind::Busy,
                ..
            }
        ));
        c.req(2, exec("echo two"));
        let mut done = HashMap::new();
        while done.len() < 2 {
            let f = c.next();
            if f.kind == Kind::Response
                && let Ok(Response::Exit { code, .. }) = f.parse::<Response>()
            {
                done.insert(f.stream, code);
            }
        }
        assert_eq!(done[&1], Some(0));
        assert_eq!(done[&2], Some(0));
        c.req(0, exec("true"));
        assert!(matches!(
            c.response(),
            Response::Error {
                kind: ErrorKind::InvalidInput,
                ..
            }
        ));
    }

    #[test]
    fn files() {
        let t = TempDir::new("files");
        let mut c = Client::start(&t.0);
        // Write in two chunks without waiting, into a new directory.
        c.req(
            1,
            Request::WriteFile {
                path: "sub/a.txt".into(),
                mode: Some(0o600),
                append: false,
            },
        );
        c.send(Frame::new(Kind::Stdin, 1, b"hello ".to_vec()));
        c.send(Frame::new(Kind::Stdin, 1, b"world".to_vec()));
        c.send(Frame::eof(1));
        assert_eq!(c.response(), Response::Written { bytes: 11 });
        assert_eq!(fs::read(t.0.join("sub/a.txt")).unwrap(), b"hello world");

        c.req(
            2,
            Request::WriteFile {
                path: "sub/a.txt".into(),
                mode: None,
                append: true,
            },
        );
        c.send(Frame::new(Kind::Stdin, 2, b"!".to_vec()));
        c.send(Frame::eof(2));
        assert_eq!(c.response(), Response::Written { bytes: 1 });

        c.req(
            3,
            Request::ReadFile {
                path: t.0.join("sub/a.txt").to_string_lossy().into(),
            },
        );
        let Response::Entry { entry } = c.response() else {
            panic!()
        };
        assert_eq!(entry.size, 12);
        assert_eq!(entry.mode, 0o600);
        assert_eq!(entry.kind, FileKind::File);
        let mut data = Vec::new();
        loop {
            let f = c.next();
            match f.kind {
                Kind::Stdout => data.extend(f.payload),
                Kind::Eof => break,
                k => panic!("{k:?}"),
            }
        }
        assert_eq!(data, b"hello world!");

        c.req(
            4,
            Request::Mkdir {
                path: "sub/deep/er".into(),
            },
        );
        assert_eq!(c.response(), Response::Ok);
        c.req(5, Request::List { path: "sub".into() });
        let Response::Entries { entries } = c.response() else {
            panic!()
        };
        let names: Vec<_> = entries.iter().map(|e| (e.name.as_str(), e.kind)).collect();
        assert_eq!(
            names,
            vec![("a.txt", FileKind::File), ("deep", FileKind::Dir)]
        );

        c.req(
            6,
            Request::Stat {
                path: "nope".into(),
            },
        );
        assert!(matches!(
            c.response(),
            Response::Error {
                kind: ErrorKind::NotFound,
                ..
            }
        ));
        c.req(
            7,
            Request::Remove {
                path: "sub".into(),
                recursive: false,
            },
        );
        assert!(matches!(c.response(), Response::Error { .. }));
        c.req(
            8,
            Request::Remove {
                path: "sub".into(),
                recursive: true,
            },
        );
        assert_eq!(c.response(), Response::Ok);
        assert!(!t.0.join("sub").exists());

        // A write that can't open reports at the end, after its data.
        c.req(
            9,
            Request::WriteFile {
                path: "/proc/nope/x".into(),
                mode: None,
                append: false,
            },
        );
        c.send(Frame::new(Kind::Stdin, 9, b"x".to_vec()));
        c.send(Frame::eof(9));
        assert!(matches!(c.response(), Response::Error { .. }));
        c.req(
            10,
            Request::ReadFile {
                path: t.0.to_string_lossy().into(),
            },
        );
        assert!(matches!(
            c.response(),
            Response::Error {
                kind: ErrorKind::InvalidInput,
                ..
            }
        ));
    }

    #[test]
    fn large_file_round_trip() {
        let t = TempDir::new("large");
        let mut c = Client::start(&t.0);
        let data: Vec<u8> = (0..3 * CHUNK + 17).map(|i| (i % 251) as u8).collect();
        c.req(
            1,
            Request::WriteFile {
                path: "big".into(),
                mode: None,
                append: false,
            },
        );
        for ch in data.chunks(CHUNK) {
            c.send(Frame::new(Kind::Stdin, 1, ch.to_vec()));
        }
        c.send(Frame::eof(1));
        assert_eq!(
            c.response(),
            Response::Written {
                bytes: data.len() as u64
            }
        );
        c.req(2, Request::ReadFile { path: "big".into() });
        assert!(matches!(c.response(), Response::Entry { .. }));
        let mut got = Vec::new();
        loop {
            let f = c.next();
            if f.kind == Kind::Eof {
                break;
            }
            got.extend(f.payload);
        }
        assert_eq!(got, data);
    }

    #[test]
    fn port_forwarding() {
        let t = TempDir::new("port");
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let (mut s, _) = l.accept().unwrap();
            let mut buf = Vec::new();
            s.read_to_end(&mut buf).unwrap();
            s.write_all(&buf.iter().rev().copied().collect::<Vec<_>>())
                .unwrap();
        });
        let mut c = Client::start(&t.0);
        c.req(1, Request::PortConnect { port });
        assert_eq!(c.response(), Response::Ok);
        c.send(Frame::new(Kind::Stdin, 1, b"abc".to_vec()));
        c.send(Frame::eof(1));
        let mut got = Vec::new();
        loop {
            let f = c.next();
            match f.kind {
                Kind::Stdout => got.extend(f.payload),
                Kind::Eof => break,
                k => panic!("{k:?}"),
            }
        }
        assert_eq!(got, b"cba");

        // A closed port.
        let free = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        c.req(2, Request::PortConnect { port: free });
        assert!(matches!(c.response(), Response::Error { .. }));
    }

    #[test]
    fn pty_session() {
        let t = TempDir::new("pty");
        let mut c = Client::start(&t.0);
        c.req(
            1,
            Request::Pty(PtySpec {
                cmd: Some(Cmd::Argv(vec!["/bin/sh".into()])),
                cwd: None,
                env: BTreeMap::new(),
                cols: 80,
                rows: 24,
            }),
        );
        assert!(matches!(c.response(), Response::Started { .. }));
        c.req(
            1,
            Request::Resize {
                cols: 100,
                rows: 30,
            },
        );
        c.send(Frame::new(
            Kind::Stdin,
            1,
            b"stty size; tty -s && echo IS_TTY; exit 7\n".to_vec(),
        ));
        let mut out = Vec::new();
        let exit = loop {
            let f = c.next();
            match f.kind {
                Kind::Stdout => out.extend(f.payload),
                Kind::Response => break f.parse::<Response>().unwrap(),
                k => panic!("{k:?}"),
            }
        };
        let text = String::from_utf8_lossy(&out);
        assert!(text.contains("30 100"), "{text}");
        assert!(text.contains("IS_TTY"), "{text}");
        assert_eq!(
            exit,
            Response::Exit {
                code: Some(7),
                signal: None
            }
        );
    }

    #[test]
    fn bad_json_is_an_error_not_a_hangup() {
        let t = TempDir::new("badjson");
        let mut c = Client::start(&t.0);
        c.send(Frame::new(Kind::Request, 4, b"{nope".to_vec()));
        assert!(matches!(
            c.response(),
            Response::Error {
                kind: ErrorKind::InvalidInput,
                ..
            }
        ));
        c.req(0, Request::Ping);
        assert_eq!(c.response(), Response::Pong);
    }
}
