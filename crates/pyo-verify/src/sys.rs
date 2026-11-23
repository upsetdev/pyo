//! Everything the verifier touches outside itself, behind small traits: the
//! files (under a root directory), the commands it runs, HTTP, the clock and
//! the hand-over to a new binary. The real ones are below. Tests bring their
//! own, so they need no root, network or k3s.

use std::{
    io::{self, Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use crate::{release::ReleaseVerifier, version::Version};

/// What went wrong, for the server's owner: one plain line. `detail` is for
/// the journal only and never goes into the status.
#[derive(Debug, Clone, PartialEq)]
pub struct Problem {
    pub plain: String,
    pub detail: Option<String>,
}

impl Problem {
    pub fn new(plain: impl Into<String>) -> Self {
        Problem {
            plain: plain.into(),
            detail: None,
        }
    }

    /// With a technical note. It may quote what a server answered, so
    /// anything but plain printable text is dropped.
    pub fn with(plain: impl Into<String>, detail: impl AsRef<str>) -> Self {
        let detail: String = detail
            .as_ref()
            .chars()
            .map(|c| if c.is_control() { ' ' } else { c })
            .take(400)
            .collect();
        Problem {
            plain: plain.into(),
            detail: Some(detail),
        }
    }
}

pub struct CmdOut {
    pub ok: bool,
    pub stdout: Vec<u8>,
}

/// Runs a program with these arguments, never through a shell.
pub trait Runner {
    fn run(&self, argv: &[&str], stdin: Option<&[u8]>) -> io::Result<CmdOut>;
}

pub struct Response {
    pub status: u16,
    /// Where a 3xx answer points. Never followed by the transport itself.
    pub redirect: Option<String>,
    pub body: Vec<u8>,
}

#[derive(Debug)]
pub enum HttpError {
    /// The answer is longer than `max`.
    TooLarge,
    Failed(String),
}

/// One GET, no redirects followed, at most `max` bytes of answer.
pub trait Transport {
    fn get(&self, url: &str, max: usize) -> Result<Response, HttpError>;
}

pub trait Clock {
    /// Seconds since 1970.
    fn now(&self) -> i64;
}

/// Replaces this process with another program. The real one only returns
/// when that failed.
pub trait Handover {
    fn exec(&self, program: &Path, args: &[&str]) -> io::Result<()>;
}

pub struct Env<'a> {
    /// `/` on a server. Tests use a temporary directory.
    pub root: PathBuf,
    pub runner: &'a dyn Runner,
    pub http: &'a dyn Transport,
    pub clock: &'a dyn Clock,
    pub handover: &'a dyn Handover,
    pub verifier: &'a dyn ReleaseVerifier,
    /// This build's version.
    pub own: Version,
    /// `x86_64` or `aarch64`.
    pub arch: &'a str,
    /// Where this binary is. Its replacement is written next to it.
    pub exe: PathBuf,
}

impl Env<'_> {
    /// An absolute path, under the root.
    pub fn path(&self, absolute: &str) -> PathBuf {
        self.root.join(absolute.trim_start_matches('/'))
    }
}

/// Writes a file so that it is either the old or the new one, never half.
pub fn write_atomic(path: &Path, bytes: &[u8], mode: u32) -> io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let dir = path
        .parent()
        .ok_or_else(|| io::Error::other("no directory"))?;
    std::fs::create_dir_all(dir)?;
    let name = path
        .file_name()
        .ok_or_else(|| io::Error::other("no file name"))?
        .to_string_lossy();
    let tmp = dir.join(format!(".{name}.tmp"));
    // A fresh file: never written through something left at this name.
    let _ = std::fs::remove_file(&tmp);
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .open(&tmp)?;
    let written = f.write_all(bytes).and_then(|()| f.sync_all());
    if let Err(e) = written.and_then(|()| std::fs::rename(&tmp, path)) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    if let Ok(d) = std::fs::File::open(dir) {
        let _ = d.sync_all();
    }
    Ok(())
}

/// At most `max` bytes of a file. Longer is an error.
pub fn read_limited(path: &Path, max: usize) -> io::Result<Vec<u8>> {
    let mut out = Vec::new();
    std::fs::File::open(path)?
        .take(max as u64 + 1)
        .read_to_end(&mut out)?;
    if out.len() > max {
        return Err(io::Error::other("too large"));
    }
    Ok(out)
}

pub struct RealRunner;

impl Runner for RealRunner {
    fn run(&self, argv: &[&str], stdin: Option<&[u8]>) -> io::Result<CmdOut> {
        let (program, args) = argv
            .split_first()
            .ok_or_else(|| io::Error::other("no program"))?;
        let mut child = Command::new(program)
            .args(args)
            .stdin(if stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;
        if let (Some(bytes), Some(mut pipe)) = (stdin, child.stdin.take()) {
            // A program that stops reading early has failed, which its exit
            // status says.
            let _ = pipe.write_all(bytes);
        }
        let out = child.wait_with_output()?;
        Ok(CmdOut {
            ok: out.status.success(),
            stdout: out.stdout,
        })
    }
}

/// HTTP through the server's curl, like Pyo's host scripts.
pub struct Curl;

/// The line curl ends its error output with: the status and the redirect.
const CURL_MARK: &str = "pyo-verify-http ";

impl Transport for Curl {
    fn get(&self, url: &str, max: usize) -> Result<Response, HttpError> {
        let proto = if url.starts_with("https://") {
            "=https"
        } else {
            "=http"
        };
        let max_arg = max.to_string();
        let agent = format!("pyo-verify/{}", env!("CARGO_PKG_VERSION"));
        let write_out = format!("%{{stderr}}\n{CURL_MARK}%{{http_code}} %{{redirect_url}}\n");
        // -q first: no curlrc. No --location: redirects are checked by
        // fetch.rs, one at a time. The URL comes after `--`.
        let mut child = Command::new("curl")
            .args(["-q", "--silent", "--show-error", "--proto", proto])
            .args(["--connect-timeout", "15", "--max-time", "300"])
            .args(["--max-filesize", &max_arg, "--user-agent", &agent])
            .args(["--output", "-", "--write-out", &write_out, "--", url])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| HttpError::Failed(format!("curl did not start: {e}")))?;
        // The limit is kept here: curl's own only works when the server says
        // the length up front.
        let mut body = Vec::new();
        let read = child
            .stdout
            .take()
            .map(|out| out.take(max as u64 + 1).read_to_end(&mut body));
        if body.len() > max {
            let _ = child.kill();
            let _ = child.wait();
            return Err(HttpError::TooLarge);
        }
        let out = child
            .wait_with_output()
            .map_err(|e| HttpError::Failed(e.to_string()))?;
        if !matches!(read, Some(Ok(_))) {
            return Err(HttpError::Failed("the answer could not be read".into()));
        }
        let notes = String::from_utf8_lossy(&out.stderr);
        if out.status.code() == Some(63) {
            return Err(HttpError::TooLarge);
        }
        if !out.status.success() {
            return Err(HttpError::Failed(format!("curl: {}", notes.trim())));
        }
        let line = notes
            .lines()
            .rev()
            .find_map(|l| l.strip_prefix(CURL_MARK))
            .ok_or_else(|| HttpError::Failed("curl gave no status".into()))?;
        let (status, redirect) = line.split_once(' ').unwrap_or((line, ""));
        Ok(Response {
            status: status
                .parse()
                .map_err(|_| HttpError::Failed("curl gave no status".into()))?,
            redirect: Some(redirect.trim().to_owned()).filter(|r| !r.is_empty()),
            body,
        })
    }
}

pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs() as i64)
    }
}

pub struct Exec;

impl Handover for Exec {
    fn exec(&self, program: &Path, args: &[&str]) -> io::Result<()> {
        use std::os::unix::process::CommandExt;
        Err(Command::new(program).args(args).exec())
    }
}

#[cfg(test)]
mod tests {
    use std::{
        io::{BufRead, BufReader, Write},
        net::TcpListener,
    };

    use super::*;

    /// A web server on this machine for a few requests: curl for real, but
    /// no network.
    fn serve(requests: usize) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let own = base.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming().take(requests) {
                let mut stream = stream.unwrap();
                let mut lines = BufReader::new(stream.try_clone().unwrap()).lines();
                let request = lines.next().unwrap().unwrap();
                for line in lines {
                    if line.unwrap().is_empty() {
                        break;
                    }
                }
                let path = request.split(' ').nth(1).unwrap().to_owned();
                let answer = match path.as_str() {
                    "/ok" => {
                        "HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\nhello"
                            .to_owned()
                    }
                    "/moved" => format!(
                        "HTTP/1.1 302 Found\r\nLocation: {own}/ok\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    ),
                    "/missing" => {
                        "HTTP/1.1 404 Not Found\r\nContent-Length: 2\r\nConnection: close\r\n\r\nno"
                            .to_owned()
                    }
                    // Long, and says so.
                    "/long" => format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: 5000\r\nConnection: close\r\n\r\n{}",
                        "x".repeat(5000)
                    ),
                    // Long, without saying so.
                    _ => format!(
                        "HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n{}",
                        "x".repeat(200_000)
                    ),
                };
                let _ = stream.write_all(answer.as_bytes());
            }
        });
        base
    }

    #[test]
    fn curl_answers_are_read_with_their_status_redirect_and_limit() {
        let _alone = crate::testdir::spawning();
        if Command::new("curl").arg("--version").output().is_err() {
            eprintln!("no curl here: skipped");
            return;
        }
        let base = serve(5);
        let r = Curl.get(&format!("{base}/ok"), 100).unwrap();
        assert_eq!(
            (r.status, r.redirect, r.body.as_slice()),
            (200, None, &b"hello"[..])
        );
        // A redirect is reported, not followed.
        let r = Curl.get(&format!("{base}/moved"), 100).unwrap();
        assert_eq!(r.status, 302);
        assert_eq!(r.redirect, Some(format!("{base}/ok")));
        assert!(r.body.is_empty());
        let r = Curl.get(&format!("{base}/missing"), 100).unwrap();
        assert_eq!(r.status, 404);
        // Too long, whether the server says so up front or not.
        assert!(matches!(
            Curl.get(&format!("{base}/long"), 100),
            Err(HttpError::TooLarge)
        ));
        assert!(matches!(
            Curl.get(&format!("{base}/endless"), 100),
            Err(HttpError::TooLarge)
        ));
        // Nobody there.
        assert!(matches!(
            Curl.get("http://127.0.0.1:1/x", 100),
            Err(HttpError::Failed(_))
        ));
    }

    #[test]
    fn commands_run_without_a_shell() {
        let _alone = crate::testdir::spawning();
        let out = RealRunner
            .run(&["printf", "%s", "$(id); `id` && x"], None)
            .unwrap();
        assert!(out.ok);
        assert_eq!(out.stdout, b"$(id); `id` && x");
        let out = RealRunner.run(&["cat"], Some(b"from stdin")).unwrap();
        assert_eq!(out.stdout, b"from stdin");
        assert!(!RealRunner.run(&["false"], None).unwrap().ok);
        assert!(RealRunner.run(&["/nonexistent/program"], None).is_err());
    }

    #[test]
    fn files_are_replaced_whole() {
        let dir = crate::testdir::TestDir::new();
        let path = dir.path().join("a/b/file");
        write_atomic(&path, b"one", 0o644).unwrap();
        write_atomic(&path, b"two", 0o644).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"two");
        assert_eq!(
            std::fs::read_dir(path.parent().unwrap()).unwrap().count(),
            1
        );
        assert_eq!(read_limited(&path, 3).unwrap(), b"two");
        assert!(read_limited(&path, 2).is_err());
    }
}
