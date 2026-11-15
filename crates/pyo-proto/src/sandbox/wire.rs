//! The protocol between the daemon and `pyo-envd`, the agent inside every
//! sandbox. It runs over vsock on Firecracker and over a Unix socket on
//! gVisor. Clients speak it too, over the daemon's WebSocket, one frame per
//! binary message, and the daemon checks every request against the token.
//!
//! ## Who connects
//!
//! - On Firecracker envd listens on vsock port [`ENVD_VSOCK_PORT`] and dials
//!   the host once on [`READY_VSOCK_PORT`] as the ready signal.
//! - On gVisor envd never listens, because a bound host socket blocks
//!   `runsc checkpoint`. It dials a per-sandbox host Unix socket, keeps one
//!   idle connection waiting and dials again when that one gets used or
//!   closes.
//!
//! Either way the protocol is the same and `Hello` is the first frame.
//!
//! ## Frames
//!
//! ```text
//! [len u32 BE][kind u8][stream u32 BE][payload: len - 5 bytes]
//! ```
//!
//! `len` counts everything after itself, so a frame is `4 + len` bytes. The
//! payload of `Hello`, `Request` and `Response` is JSON. `Stdin`, `Stdout`
//! and `Stderr` carry raw bytes. `Eof` has no payload. Payloads are at most
//! [`MAX_PAYLOAD`] bytes. A reader drops the connection on a bigger frame or
//! an unknown kind.
//!
//! ## Streams
//!
//! - Stream 0 is the connection itself. envd sends [`Hello`] on it as the
//!   first frame of every connection, the ready connection included. Its
//!   `protocol` is the newest version envd speaks and `minProtocol` the
//!   oldest. The host picks a version both sides speak with [`negotiate`]
//!   and closes the connection when there is none.
//! - The host opens a stream with a `Request` on a new stream number above
//!   0. Many streams run at once on one connection.
//! - `Signal` and `Resize` are requests sent on the stream of the process
//!   they act on.
//! - An `Error` response ends its stream.
//!
//! | Request | Frames back |
//! |---|---|
//! | `ping`, `stat`, `list`, `mkdir`, `remove`, `setTime`, `setNetwork` | one response |
//! | `exec` | `started`, then `Stdout` and `Stderr`, then `exit` last. With `stdin: true` the host sends `Stdin` frames and `Eof` closes stdin |
//! | `pty` | `started`, then `Stdout` (the terminal), then `exit`. `Stdin` frames are typed into the terminal |
//! | `writeFile` | the host sends `Stdin` frames and `Eof` right after the request, without waiting. envd answers `written` at the end |
//! | `readFile` | `entry`, then `Stdout` frames, then `Eof` |
//! | `portConnect` | `ok`, then `Stdout` from the guest port and `Stdin` to it. `Eof` closes one direction. The stream ends when both are closed |

use std::collections::BTreeMap;
use std::io::{self, Read, Write};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

/// The newest protocol version this side speaks.
pub const WIRE_PROTOCOL: u16 = 1;
/// The oldest protocol version this side speaks.
pub const MIN_WIRE_PROTOCOL: u16 = 1;

/// envd listens on this vsock port.
pub const ENVD_VSOCK_PORT: u32 = 52;
/// envd's ready connection goes to this port on the host (CID 2).
pub const READY_VSOCK_PORT: u32 = 1024;
pub const HOST_CID: u32 = 2;

/// The largest payload of one frame.
pub const MAX_PAYLOAD: usize = 1 << 20;
/// What senders put in one data frame.
pub const CHUNK: usize = 64 * 1024;
/// `len` plus `kind` plus `stream`.
pub const HEADER_LEN: usize = 9;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum Kind {
    Hello = 1,
    Request = 2,
    Response = 3,
    Stdin = 4,
    Stdout = 5,
    Stderr = 6,
    Eof = 7,
}

impl Kind {
    pub fn from_u8(b: u8) -> Option<Kind> {
        Some(match b {
            1 => Kind::Hello,
            2 => Kind::Request,
            3 => Kind::Response,
            4 => Kind::Stdin,
            5 => Kind::Stdout,
            6 => Kind::Stderr,
            7 => Kind::Eof,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub kind: Kind,
    pub stream: u32,
    pub payload: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WireError {
    TooLarge(usize),
    UnknownKind(u8),
    /// The bytes end inside a frame.
    Truncated,
    /// Bytes left after one whole frame, where only one was expected.
    Trailing,
    Json(String),
}

impl std::fmt::Display for WireError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WireError::TooLarge(n) => write!(f, "frame payload of {n} bytes is over the limit"),
            WireError::UnknownKind(k) => write!(f, "unknown frame kind {k}"),
            WireError::Truncated => f.write_str("the frame is cut short"),
            WireError::Trailing => f.write_str("bytes after the frame"),
            WireError::Json(e) => write!(f, "bad frame JSON: {e}"),
        }
    }
}

impl std::error::Error for WireError {}

impl From<WireError> for io::Error {
    fn from(e: WireError) -> io::Error {
        io::Error::new(io::ErrorKind::InvalidData, e)
    }
}

impl Frame {
    pub fn new(kind: Kind, stream: u32, payload: Vec<u8>) -> Frame {
        Frame {
            kind,
            stream,
            payload,
        }
    }

    pub fn json<T: Serialize>(kind: Kind, stream: u32, value: &T) -> Frame {
        // Serializing these types can't fail: no maps with non-string keys.
        let payload = serde_json::to_vec(value).unwrap_or_default();
        Frame {
            kind,
            stream,
            payload,
        }
    }

    pub fn request(stream: u32, req: &Request) -> Frame {
        Frame::json(Kind::Request, stream, req)
    }

    pub fn response(stream: u32, res: &Response) -> Frame {
        Frame::json(Kind::Response, stream, res)
    }

    pub fn eof(stream: u32) -> Frame {
        Frame::new(Kind::Eof, stream, Vec::new())
    }

    pub fn parse<T: DeserializeOwned>(&self) -> Result<T, WireError> {
        serde_json::from_slice(&self.payload).map_err(|e| WireError::Json(e.to_string()))
    }

    pub fn encode_into(&self, out: &mut Vec<u8>) {
        let len = (self.payload.len() + 5) as u32;
        out.reserve(4 + len as usize);
        out.extend_from_slice(&len.to_be_bytes());
        out.push(self.kind as u8);
        out.extend_from_slice(&self.stream.to_be_bytes());
        out.extend_from_slice(&self.payload);
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut v = Vec::with_capacity(HEADER_LEN + self.payload.len());
        self.encode_into(&mut v);
        v
    }

    /// Exactly one frame, like a WebSocket message.
    pub fn decode(bytes: &[u8]) -> Result<Frame, WireError> {
        let mut d = Decoder::default();
        d.push(bytes);
        let f = d.next_frame()?.ok_or(WireError::Truncated)?;
        if !d.is_empty() {
            return Err(WireError::Trailing);
        }
        Ok(f)
    }
}

/// Parses a header into `(payload length, kind, stream)`.
fn parse_header(h: &[u8; HEADER_LEN]) -> Result<(usize, Kind, u32), WireError> {
    let len = u32::from_be_bytes([h[0], h[1], h[2], h[3]]) as usize;
    if len < 5 {
        return Err(WireError::Truncated);
    }
    let payload = len - 5;
    if payload > MAX_PAYLOAD {
        return Err(WireError::TooLarge(payload));
    }
    let kind = Kind::from_u8(h[4]).ok_or(WireError::UnknownKind(h[4]))?;
    let stream = u32::from_be_bytes([h[5], h[6], h[7], h[8]]);
    Ok((payload, kind, stream))
}

/// Writes one frame. Callers that share a writer between threads hold its
/// lock for the whole call, so frames never interleave.
pub fn write_frame<W: Write + ?Sized>(w: &mut W, frame: &Frame) -> io::Result<()> {
    if frame.payload.len() > MAX_PAYLOAD {
        return Err(WireError::TooLarge(frame.payload.len()).into());
    }
    w.write_all(&frame.encode())
}

/// Reads one frame. `Ok(None)` when the connection ends between frames.
pub fn read_frame<R: Read + ?Sized>(r: &mut R) -> io::Result<Option<Frame>> {
    let mut h = [0u8; HEADER_LEN];
    let mut got = 0;
    while got < HEADER_LEN {
        match r.read(&mut h[got..]) {
            Ok(0) if got == 0 => return Ok(None),
            Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
            Ok(n) => got += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    let (len, kind, stream) = parse_header(&h)?;
    let mut payload = vec![0u8; len];
    r.read_exact(&mut payload)?;
    Ok(Some(Frame {
        kind,
        stream,
        payload,
    }))
}

/// Turns a byte stream into frames, for async readers.
#[derive(Debug, Default)]
pub struct Decoder {
    buf: Vec<u8>,
}

impl Decoder {
    pub fn push(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    /// The next whole frame, or `None` until more bytes arrive.
    pub fn next_frame(&mut self) -> Result<Option<Frame>, WireError> {
        if self.buf.len() < HEADER_LEN {
            return Ok(None);
        }
        let h: [u8; HEADER_LEN] = self.buf[..HEADER_LEN].try_into().expect("header length");
        let (len, kind, stream) = parse_header(&h)?;
        if self.buf.len() < HEADER_LEN + len {
            return Ok(None);
        }
        let payload = self.buf[HEADER_LEN..HEADER_LEN + len].to_vec();
        self.buf.drain(..HEADER_LEN + len);
        Ok(Some(Frame {
            kind,
            stream,
            payload,
        }))
    }
}

// ── Messages ───────────────────────────────────────────────────────────────

/// envd's first frame on every connection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Hello {
    pub protocol: u16,
    pub min_protocol: u16,
    pub envd_version: String,
}

impl Hello {
    pub fn ours(envd_version: impl Into<String>) -> Hello {
        Hello {
            protocol: WIRE_PROTOCOL,
            min_protocol: MIN_WIRE_PROTOCOL,
            envd_version: envd_version.into(),
        }
    }
}

/// The version both sides speak: the newest one in both ranges.
pub fn negotiate(theirs: &Hello) -> Result<u16, String> {
    let v = theirs.protocol.min(WIRE_PROTOCOL);
    if v < theirs.min_protocol || v < MIN_WIRE_PROTOCOL {
        return Err(format!(
            "envd {} speaks protocol {} to {}, and this side speaks {} to {}",
            theirs.envd_version,
            theirs.min_protocol,
            theirs.protocol,
            MIN_WIRE_PROTOCOL,
            WIRE_PROTOCOL
        ));
    }
    Ok(v)
}

/// A command: a shell string run with `/bin/sh -c`, or an argv run as is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Cmd {
    Shell(String),
    Argv(Vec<String>),
}

impl Cmd {
    pub fn argv(&self) -> Vec<String> {
        match self {
            Cmd::Shell(s) => vec!["/bin/sh".into(), "-c".into(), s.clone()],
            Cmd::Argv(v) => v.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExecSpec {
    pub cmd: Cmd,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Whether the host sends stdin. Without it stdin is /dev/null.
    #[serde(default)]
    pub stdin: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PtySpec {
    /// The user's login shell when left out.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cmd: Option<Cmd>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    pub cols: u16,
    pub rows: u16,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "camelCase")]
pub enum Request {
    Ping,
    Exec(ExecSpec),
    Pty(PtySpec),
    #[serde(rename_all = "camelCase")]
    Resize {
        cols: u16,
        rows: u16,
    },
    Signal {
        signal: i32,
    },
    #[serde(rename_all = "camelCase")]
    WriteFile {
        path: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mode: Option<u32>,
        #[serde(default)]
        append: bool,
    },
    ReadFile {
        path: String,
    },
    List {
        path: String,
    },
    Stat {
        path: String,
    },
    Mkdir {
        path: String,
    },
    Remove {
        path: String,
        #[serde(default)]
        recursive: bool,
    },
    PortConnect {
        port: u16,
    },
    /// Sets the guest clock. The daemon sends it after a restore, since the
    /// guest clock stays at the snapshot's time.
    #[serde(rename_all = "camelCase")]
    SetTime {
        unix_ms: u64,
    },
    /// Sets up the guest's network interface, default route and
    /// `/etc/resolv.conf`. The daemon sends it after every start and
    /// restore, so a sandbox restored from a template snapshot gets its own
    /// address and not the template's.
    SetNetwork(GuestNetwork),
}

/// A sandbox's address on its tap link. Each sandbox has its own /30: the
/// host end is the gateway and the guest end is `address`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GuestNetwork {
    /// The guest interface, `eth0` on Firecracker.
    pub iface: String,
    pub address: std::net::Ipv4Addr,
    pub prefix: u8,
    pub gateway: std::net::Ipv4Addr,
    /// Written to `/etc/resolv.conf`.
    #[serde(default)]
    pub dns: Vec<std::net::Ipv4Addr>,
}

/// What a request needs from a client token. The daemon checks this on
/// every request a client sends over the WebSocket.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Scope {
    /// Create this sandbox. Only Pyo holds it.
    Create,
    /// Run commands.
    Exec,
    /// Open a terminal.
    Pty,
    /// Read and write files.
    Files,
    /// Reach the sandbox's ports.
    Ports,
    /// Pause, resume, change the lifetime and delete.
    Lifecycle,
    /// Everything on the server, for Pyo itself.
    Admin,
}

impl Request {
    /// The scope a client needs. `None` for requests that only act on a
    /// stream the client already opened, and for ping.
    pub fn scope(&self) -> Option<Scope> {
        match self {
            Request::Ping | Request::Resize { .. } | Request::Signal { .. } => None,
            Request::Exec(_) => Some(Scope::Exec),
            Request::Pty(_) => Some(Scope::Pty),
            Request::WriteFile { .. }
            | Request::ReadFile { .. }
            | Request::List { .. }
            | Request::Stat { .. }
            | Request::Mkdir { .. }
            | Request::Remove { .. } => Some(Scope::Files),
            Request::PortConnect { .. } => Some(Scope::Ports),
            Request::SetTime { .. } | Request::SetNetwork(_) => Some(Scope::Admin),
        }
    }

    /// Whether the request opens a new stream, rather than act on one.
    pub fn opens_stream(&self) -> bool {
        !matches!(self, Request::Resize { .. } | Request::Signal { .. })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FileKind {
    File,
    Dir,
    Symlink,
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileEntry {
    pub name: String,
    pub path: String,
    pub kind: FileKind,
    pub size: u64,
    /// Permission bits, like 0o644.
    pub mode: u32,
    /// Epoch milliseconds.
    pub modified: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ErrorKind {
    NotFound,
    PermissionDenied,
    AlreadyExists,
    InvalidInput,
    /// The stream number is already in use.
    Busy,
    Unsupported,
    Io,
}

impl From<&io::Error> for ErrorKind {
    fn from(e: &io::Error) -> ErrorKind {
        match e.kind() {
            io::ErrorKind::NotFound => ErrorKind::NotFound,
            io::ErrorKind::PermissionDenied => ErrorKind::PermissionDenied,
            io::ErrorKind::AlreadyExists => ErrorKind::AlreadyExists,
            io::ErrorKind::InvalidInput | io::ErrorKind::InvalidData => ErrorKind::InvalidInput,
            io::ErrorKind::Unsupported => ErrorKind::Unsupported,
            _ => ErrorKind::Io,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum Response {
    Pong,
    Ok,
    Started {
        pid: u32,
    },
    /// The process ended. `code` is None when a signal ended it.
    Exit {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        code: Option<i32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        signal: Option<i32>,
    },
    Written {
        bytes: u64,
    },
    Entry {
        entry: FileEntry,
    },
    Entries {
        entries: Vec<FileEntry>,
    },
    Error {
        kind: ErrorKind,
        message: String,
    },
}

impl Response {
    pub fn error(kind: ErrorKind, message: impl Into<String>) -> Response {
        Response::Error {
            kind,
            message: message.into(),
        }
    }

    pub fn io_error(e: &io::Error) -> Response {
        Response::Error {
            kind: e.into(),
            message: e.to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_layout_is_fixed() {
        let f = Frame::new(Kind::Stdout, 7, b"hi".to_vec());
        assert_eq!(f.encode(), vec![0, 0, 0, 7, 5, 0, 0, 0, 7, b'h', b'i']);
        assert_eq!(Frame::decode(&f.encode()).unwrap(), f);
        assert_eq!(Frame::eof(3).encode().len(), HEADER_LEN);
    }

    #[test]
    fn read_and_write_many_frames() {
        let frames = vec![
            Frame::json(Kind::Hello, 0, &Hello::ours("0.0.0")),
            Frame::request(
                1,
                &Request::Exec(ExecSpec {
                    cmd: Cmd::Shell("echo hi".into()),
                    cwd: None,
                    env: BTreeMap::new(),
                    stdin: false,
                }),
            ),
            Frame::new(Kind::Stdin, 1, vec![0u8; MAX_PAYLOAD]),
            Frame::eof(1),
        ];
        let mut buf = Vec::new();
        for f in &frames {
            write_frame(&mut buf, f).unwrap();
        }
        let mut r = io::Cursor::new(buf);
        for f in &frames {
            assert_eq!(&read_frame(&mut r).unwrap().unwrap(), f);
        }
        assert!(read_frame(&mut r).unwrap().is_none());
    }

    #[test]
    fn decoder_handles_split_bytes() {
        let a = Frame::new(Kind::Stdout, 1, b"abc".to_vec()).encode();
        let b = Frame::new(Kind::Stderr, 2, b"de".to_vec()).encode();
        let all: Vec<u8> = a.iter().chain(b.iter()).copied().collect();
        let mut d = Decoder::default();
        let mut got = Vec::new();
        for byte in all {
            d.push(&[byte]);
            while let Some(f) = d.next_frame().unwrap() {
                got.push(f);
            }
        }
        assert_eq!(got.len(), 2);
        assert_eq!(got[1].payload, b"de");
        assert!(d.is_empty());
    }

    #[test]
    fn bad_frames_are_refused() {
        let mut big = Vec::new();
        big.extend_from_slice(&((MAX_PAYLOAD + 6) as u32).to_be_bytes());
        big.extend_from_slice(&[5, 0, 0, 0, 1]);
        assert_eq!(
            Frame::decode(&big),
            Err(WireError::TooLarge(MAX_PAYLOAD + 1))
        );
        assert_eq!(
            Frame::decode(&[0, 0, 0, 5, 99, 0, 0, 0, 1]),
            Err(WireError::UnknownKind(99))
        );
        assert_eq!(
            Frame::decode(&[0, 0, 0, 9, 5, 0, 0, 0, 1]),
            Err(WireError::Truncated)
        );
        assert_eq!(Frame::decode(&[0, 0, 0, 2, 5]), Err(WireError::Truncated));
        let mut two = Frame::eof(1).encode();
        two.push(0);
        assert_eq!(Frame::decode(&two), Err(WireError::Trailing));
        // A cut frame is an error, a clean end is not.
        let cut = &Frame::eof(1).encode()[..4];
        assert!(read_frame(&mut io::Cursor::new(cut)).is_err());
        assert!(
            write_frame(
                &mut Vec::new(),
                &Frame::new(Kind::Stdin, 1, vec![0; MAX_PAYLOAD + 1])
            )
            .is_err()
        );
    }

    #[test]
    fn request_json() {
        let r = Request::WriteFile {
            path: "/a".into(),
            mode: Some(0o600),
            append: false,
        };
        let v = serde_json::to_value(&r).unwrap();
        assert_eq!(
            v,
            serde_json::json!({"op": "writeFile", "path": "/a", "mode": 384, "append": false})
        );
        let e: Request = serde_json::from_str(r#"{"op":"exec","cmd":["ls","-l"]}"#).unwrap();
        match e {
            Request::Exec(s) => {
                assert_eq!(s.cmd.argv(), vec!["ls", "-l"]);
                assert!(!s.stdin);
            }
            _ => panic!(),
        }
        let s: Request = serde_json::from_str(r#"{"op":"exec","cmd":"echo hi"}"#).unwrap();
        assert!(matches!(
            s,
            Request::Exec(ExecSpec {
                cmd: Cmd::Shell(_),
                ..
            })
        ));
        assert_eq!(
            serde_json::to_value(Request::SetTime { unix_ms: 5 }).unwrap(),
            serde_json::json!({"op": "setTime", "unixMs": 5})
        );
        assert!(serde_json::from_str::<Request>(r#"{"op":"reboot"}"#).is_err());
    }

    #[test]
    fn response_json() {
        let r = Response::Exit {
            code: Some(3),
            signal: None,
        };
        assert_eq!(
            serde_json::to_value(&r).unwrap(),
            serde_json::json!({"type": "exit", "code": 3})
        );
        let e = Response::io_error(&io::Error::from(io::ErrorKind::NotFound));
        assert!(matches!(
            e,
            Response::Error {
                kind: ErrorKind::NotFound,
                ..
            }
        ));
    }

    #[test]
    fn scopes_of_requests() {
        assert_eq!(Request::Ping.scope(), None);
        assert_eq!(
            Request::Stat { path: "/".into() }.scope(),
            Some(Scope::Files)
        );
        assert_eq!(
            Request::PortConnect { port: 3000 }.scope(),
            Some(Scope::Ports)
        );
        // Only Pyo may set the clock.
        assert_eq!(Request::SetTime { unix_ms: 0 }.scope(), Some(Scope::Admin));
        let net = Request::SetNetwork(GuestNetwork {
            iface: "eth0".into(),
            address: [198, 18, 0, 2].into(),
            prefix: 30,
            gateway: [198, 18, 0, 1].into(),
            dns: vec![[1, 1, 1, 1].into()],
        });
        assert_eq!(net.scope(), Some(Scope::Admin));
        assert_eq!(
            serde_json::to_value(&net).unwrap(),
            serde_json::json!({"op": "setNetwork", "iface": "eth0", "address": "198.18.0.2",
                "prefix": 30, "gateway": "198.18.0.1", "dns": ["1.1.1.1"]})
        );
        assert!(!Request::Signal { signal: 9 }.opens_stream());
    }

    #[test]
    fn version_negotiation() {
        assert_eq!(negotiate(&Hello::ours("x")), Ok(WIRE_PROTOCOL));
        let newer = Hello {
            protocol: WIRE_PROTOCOL + 3,
            min_protocol: MIN_WIRE_PROTOCOL,
            envd_version: "x".into(),
        };
        assert_eq!(negotiate(&newer), Ok(WIRE_PROTOCOL));
        let too_new = Hello {
            protocol: WIRE_PROTOCOL + 3,
            min_protocol: WIRE_PROTOCOL + 1,
            envd_version: "x".into(),
        };
        assert!(negotiate(&too_new).is_err());
    }
}
