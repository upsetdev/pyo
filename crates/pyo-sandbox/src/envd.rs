//! The daemon's side of the envd wire protocol.
//!
//! Firecracker exposes the guest's vsock as a Unix socket on the host. The
//! host connects to it, writes `CONNECT <port>\n` and gets `OK <n>\n`. From
//! there on the bytes are the guest's. envd's first frame is `Hello`.

use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{Context, anyhow, bail};
use pyo_proto::sandbox::wire::{
    CHUNK, Decoder, ENVD_VSOCK_PORT, ExecSpec, Frame, Hello, Kind, Request, Response, negotiate,
};
use pyo_proto::sandbox::{ExecRequest, ExecResult};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};

/// The most output a buffered exec keeps, per stream.
pub const EXEC_OUTPUT_CAP: usize = 8 << 20;
pub const DEFAULT_EXEC_TIMEOUT: Duration = Duration::from_secs(60);

pub struct Conn {
    r: OwnedReadHalf,
    w: OwnedWriteHalf,
    dec: Decoder,
    pub hello: Hello,
    next_stream: u32,
}

impl Conn {
    /// Connects through Firecracker's vsock socket, retrying until
    /// `timeout`. After a restore envd is back within a few ms.
    pub async fn connect(vsock_uds: &Path, timeout: Duration) -> anyhow::Result<Conn> {
        let until = Instant::now() + timeout;
        loop {
            match Self::try_connect(vsock_uds).await {
                Ok(c) => return Ok(c),
                Err(e) if Instant::now() >= until => {
                    return Err(e.context(format!(
                        "envd didn't answer within {} ms",
                        timeout.as_millis()
                    )));
                }
                Err(_) => tokio::time::sleep(Duration::from_micros(500)).await,
            }
        }
    }

    async fn try_connect(vsock_uds: &Path) -> anyhow::Result<Conn> {
        let mut s = UnixStream::connect(vsock_uds).await?;
        s.write_all(format!("CONNECT {ENVD_VSOCK_PORT}\n").as_bytes())
            .await?;
        // Read the OK line. Whatever comes after it is already envd's.
        let mut buf = Vec::with_capacity(64);
        let mut chunk = [0u8; 256];
        let rest = loop {
            let n = s.read(&mut chunk).await?;
            if n == 0 {
                bail!("vsock closed during CONNECT");
            }
            buf.extend_from_slice(&chunk[..n]);
            if let Some(i) = buf.iter().position(|&b| b == b'\n') {
                if !buf.starts_with(b"OK") {
                    bail!("vsock CONNECT: {}", String::from_utf8_lossy(&buf[..i]));
                }
                break buf.split_off(i + 1);
            }
            if buf.len() > 128 {
                bail!("vsock CONNECT: no answer line");
            }
        };
        Self::start(s, &rest).await
    }

    /// A connection envd opened itself (the ready signal).
    pub async fn from_ready(s: UnixStream) -> anyhow::Result<Conn> {
        Self::start(s, &[]).await
    }

    async fn start(s: UnixStream, already: &[u8]) -> anyhow::Result<Conn> {
        let (r, w) = s.into_split();
        let mut dec = Decoder::default();
        dec.push(already);
        let mut c = Conn {
            r,
            w,
            dec,
            hello: Hello::ours(""),
            next_stream: 1,
        };
        let f = c
            .recv()
            .await?
            .ok_or_else(|| anyhow!("envd closed before Hello"))?;
        if f.kind != Kind::Hello {
            bail!("envd's first frame wasn't Hello");
        }
        c.hello = f.parse()?;
        negotiate(&c.hello).map_err(|e| anyhow!(e))?;
        Ok(c)
    }

    pub fn into_split(self) -> (FrameReader, OwnedWriteHalf) {
        (
            FrameReader {
                r: self.r,
                dec: self.dec,
            },
            self.w,
        )
    }

    pub async fn send(&mut self, f: &Frame) -> anyhow::Result<()> {
        self.w
            .write_all(&f.encode())
            .await
            .context("writing to envd")
    }

    pub async fn recv(&mut self) -> anyhow::Result<Option<Frame>> {
        read_frame(&mut self.r, &mut self.dec).await
    }

    fn stream(&mut self) -> u32 {
        let s = self.next_stream;
        self.next_stream += 1;
        s
    }

    /// One request with one response.
    pub async fn call(&mut self, req: Request) -> anyhow::Result<Response> {
        let id = self.stream();
        self.send(&Frame::request(id, &req)).await?;
        loop {
            let f = self
                .recv()
                .await?
                .ok_or_else(|| anyhow!("envd closed the connection"))?;
            if f.stream == id && f.kind == Kind::Response {
                return Ok(f.parse()?);
            }
        }
    }

    /// Runs a command and waits for it, keeping up to [`EXEC_OUTPUT_CAP`]
    /// of each output. Kills it after the timeout.
    pub async fn exec(
        &mut self,
        req: &ExecRequest,
        base_env: &std::collections::BTreeMap<String, String>,
    ) -> anyhow::Result<ExecResult> {
        let started = Instant::now();
        let id = self.stream();
        let mut env = base_env.clone();
        env.extend(req.env.clone());
        let spec = ExecSpec {
            cmd: req.cmd.clone(),
            cwd: req.cwd.clone(),
            env,
            stdin: req.stdin.is_some(),
        };
        self.send(&Frame::request(id, &Request::Exec(spec))).await?;
        if let Some(input) = &req.stdin {
            for chunk in input.as_bytes().chunks(CHUNK) {
                self.send(&Frame::new(Kind::Stdin, id, chunk.to_vec()))
                    .await?;
            }
            self.send(&Frame::eof(id)).await?;
        }
        let timeout = req
            .timeout_ms
            .map(Duration::from_millis)
            .unwrap_or(DEFAULT_EXEC_TIMEOUT);
        let deadline = tokio::time::Instant::now() + timeout;
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let mut truncated = false;
        let mut timed_out = false;
        loop {
            let f = if timed_out {
                // Killed. Give it a moment to report the exit.
                match tokio::time::timeout(Duration::from_secs(2), self.recv()).await {
                    Ok(f) => f?,
                    Err(_) => bail!("the command didn't stop after it was killed"),
                }
            } else {
                match tokio::time::timeout_at(deadline, self.recv()).await {
                    Ok(f) => f?,
                    Err(_) => {
                        timed_out = true;
                        self.send(&Frame::request(
                            id,
                            &Request::Signal {
                                signal: libc::SIGKILL,
                            },
                        ))
                        .await?;
                        continue;
                    }
                }
            };
            let f = f.ok_or_else(|| anyhow!("envd closed the connection"))?;
            if f.stream != id {
                continue;
            }
            match f.kind {
                Kind::Stdout | Kind::Stderr => {
                    let buf = if f.kind == Kind::Stdout {
                        &mut out
                    } else {
                        &mut err
                    };
                    let room = EXEC_OUTPUT_CAP.saturating_sub(buf.len());
                    if f.payload.len() > room {
                        truncated = true;
                    }
                    buf.extend_from_slice(&f.payload[..f.payload.len().min(room)]);
                }
                Kind::Response => match f.parse::<Response>()? {
                    Response::Started { .. } => {}
                    Response::Exit { code, signal } => {
                        return Ok(ExecResult {
                            exit_code: code,
                            signal,
                            stdout: String::from_utf8_lossy(&out).into_owned(),
                            stderr: String::from_utf8_lossy(&err).into_owned(),
                            truncated,
                            timed_out,
                            duration_ms: started.elapsed().as_millis() as u64,
                        });
                    }
                    Response::Error { message, .. } => bail!(message),
                    other => bail!("unexpected answer from envd: {other:?}"),
                },
                _ => {}
            }
        }
    }

    /// Reads a whole file, up to `cap` bytes.
    pub async fn read_file(&mut self, path: &str, cap: u64) -> anyhow::Result<Vec<u8>> {
        let id = self.stream();
        self.send(&Frame::request(
            id,
            &Request::ReadFile { path: path.into() },
        ))
        .await?;
        let mut data = Vec::new();
        loop {
            let f = self
                .recv()
                .await?
                .ok_or_else(|| anyhow!("envd closed the connection"))?;
            if f.stream != id {
                continue;
            }
            match f.kind {
                Kind::Response => match f.parse::<Response>()? {
                    Response::Entry { entry } if entry.size > cap => {
                        bail!(FileTooBig(entry.size))
                    }
                    Response::Entry { .. } => {}
                    Response::Error { kind, message } => bail!(EnvdError { kind, message }),
                    other => bail!("unexpected answer from envd: {other:?}"),
                },
                Kind::Stdout => {
                    if data.len() as u64 + f.payload.len() as u64 > cap {
                        bail!(FileTooBig(data.len() as u64 + f.payload.len() as u64));
                    }
                    data.extend_from_slice(&f.payload);
                }
                Kind::Eof => return Ok(data),
                _ => {}
            }
        }
    }

    pub async fn write_file(
        &mut self,
        path: &str,
        data: &[u8],
        mode: Option<u32>,
    ) -> anyhow::Result<u64> {
        let id = self.stream();
        self.send(&Frame::request(
            id,
            &Request::WriteFile {
                path: path.into(),
                mode,
                append: false,
            },
        ))
        .await?;
        for chunk in data.chunks(CHUNK) {
            self.send(&Frame::new(Kind::Stdin, id, chunk.to_vec()))
                .await?;
        }
        self.send(&Frame::eof(id)).await?;
        loop {
            let f = self
                .recv()
                .await?
                .ok_or_else(|| anyhow!("envd closed the connection"))?;
            if f.stream == id && f.kind == Kind::Response {
                return match f.parse::<Response>()? {
                    Response::Written { bytes } => Ok(bytes),
                    Response::Error { kind, message } => bail!(EnvdError { kind, message }),
                    other => bail!("unexpected answer from envd: {other:?}"),
                };
            }
        }
    }
}

/// An error envd reported, kept typed so the API can map it.
#[derive(Debug)]
pub struct EnvdError {
    pub kind: pyo_proto::sandbox::wire::ErrorKind,
    pub message: String,
}

impl std::fmt::Display for EnvdError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}
impl std::error::Error for EnvdError {}

#[derive(Debug)]
pub struct FileTooBig(pub u64);

impl std::fmt::Display for FileTooBig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "The file is {} bytes, more than this call returns. Use the WebSocket for big files.",
            self.0
        )
    }
}
impl std::error::Error for FileTooBig {}

pub struct FrameReader {
    r: OwnedReadHalf,
    dec: Decoder,
}

impl FrameReader {
    pub async fn recv(&mut self) -> anyhow::Result<Option<Frame>> {
        read_frame(&mut self.r, &mut self.dec).await
    }
}

async fn read_frame(r: &mut OwnedReadHalf, dec: &mut Decoder) -> anyhow::Result<Option<Frame>> {
    let mut buf = vec![0u8; CHUNK + 16];
    loop {
        if let Some(f) = dec.next_frame()? {
            return Ok(Some(f));
        }
        let n = r.read(&mut buf).await.context("reading from envd")?;
        if n == 0 {
            if dec.is_empty() {
                return Ok(None);
            }
            bail!("envd closed the connection inside a frame");
        }
        dec.push(&buf[..n]);
    }
}
