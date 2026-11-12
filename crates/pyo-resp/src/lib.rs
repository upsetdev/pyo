//! A small RESP2 client for Valkey: enough to forward any command and keep
//! each reply's exact type. A status reply and a bulk string look the same in
//! most clients, and the REST API has to tell them apart.

use std::{
    io,
    sync::{Arc, Mutex},
    time::Duration,
};

use futures::FutureExt;
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader, BufWriter},
    net::{
        TcpStream,
        tcp::{OwnedReadHalf, OwnedWriteHalf},
    },
    sync::{OwnedSemaphorePermit, Semaphore},
};

/// One reply.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Value {
    Simple(Vec<u8>),
    Error(Vec<u8>),
    Int(i64),
    Bulk(Vec<u8>),
    /// A null bulk string or a null array.
    Nil,
    Array(Vec<Value>),
}

impl Value {
    pub fn is_error(&self) -> bool {
        matches!(self, Value::Error(_))
    }
    /// The text of a simple, error or bulk reply.
    pub fn text(&self) -> String {
        match self {
            Value::Simple(b) | Value::Error(b) | Value::Bulk(b) => {
                String::from_utf8_lossy(b).into_owned()
            }
            Value::Int(n) => n.to_string(),
            _ => String::new(),
        }
    }
}

/// A bulk string read from the server is at most this long.
pub const MAX_BULK: usize = 512 << 20;

fn bad(msg: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.into())
}

/// One connection.
pub struct Conn {
    r: BufReader<OwnedReadHalf>,
    w: BufWriter<OwnedWriteHalf>,
    /// Reply bytes read since the last send, so a caller can tell "the server
    /// never answered" from "it answered partly".
    read: usize,
    /// The protocol it speaks: 2, or 3 after HELLO 3.
    pub proto: u8,
    /// The database it has selected, as far as its user has told it.
    pub db: i64,
}

impl Conn {
    /// Connects and authenticates (when the password isn't empty).
    pub async fn connect(addr: &str, password: &str, timeout: Duration) -> io::Result<Conn> {
        Conn::connect_proto(addr, password, timeout, 2).await
    }

    /// Connects, authenticates, and speaks `proto` (2 or 3: RESP3 through
    /// HELLO 3).
    pub async fn connect_proto(
        addr: &str,
        password: &str,
        timeout: Duration,
        proto: u8,
    ) -> io::Result<Conn> {
        let tcp = tokio::time::timeout(timeout, TcpStream::connect(addr))
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "connect timed out"))??;
        tcp.set_nodelay(true)?;
        // A server that died without closing (a lost machine) never answers:
        // give up on a write nobody acknowledges within 5s, and probe idle
        // connections, rather than wait out a command's whole deadline.
        let sock = socket2::SockRef::from(&tcp);
        sock.set_tcp_keepalive(
            &socket2::TcpKeepalive::new()
                .with_time(Duration::from_secs(10))
                .with_interval(Duration::from_secs(2))
                .with_retries(3),
        )?;
        #[cfg(target_os = "linux")]
        sock.set_tcp_user_timeout(Some(Duration::from_secs(5)))?;
        let (r, w) = tcp.into_split();
        let mut c = Conn {
            r: BufReader::with_capacity(32 << 10, r),
            w: BufWriter::with_capacity(32 << 10, w),
            read: 0,
            proto: 2,
            db: 0,
        };
        if proto == 3 {
            let hello = async {
                let mut args: Vec<&[u8]> = vec![b"HELLO", b"3"];
                if !password.is_empty() {
                    args.extend([&b"AUTH"[..], b"default", password.as_bytes()]);
                }
                c.send(&[&args]).await?;
                let mut out = Vec::new();
                c.read_raw(&mut out).await?;
                io::Result::Ok(out)
            };
            let out = tokio::time::timeout(timeout, hello)
                .await
                .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "hello timed out"))??;
            if out.first() == Some(&b'-') {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    String::from_utf8_lossy(&out).trim().to_owned(),
                ));
            }
            c.proto = 3;
            return Ok(c);
        }
        if !password.is_empty() {
            let v = tokio::time::timeout(timeout, c.cmd(&[b"AUTH", password.as_bytes()]))
                .await
                .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "auth timed out"))??;
            if let Value::Error(e) = v {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    String::from_utf8_lossy(&e).into_owned(),
                ));
            }
        }
        Ok(c)
    }

    /// Whether any reply bytes arrived since the last send.
    pub fn answered(&self) -> bool {
        self.read > 0
    }

    /// Writes commands without waiting for the replies.
    pub async fn send<A: AsRef<[u8]>>(&mut self, cmds: &[&[A]]) -> io::Result<()> {
        self.read = 0;
        for args in cmds {
            self.w
                .write_all(format!("*{}\r\n", args.len()).as_bytes())
                .await?;
            for a in args.iter() {
                let a = a.as_ref();
                self.w
                    .write_all(format!("${}\r\n", a.len()).as_bytes())
                    .await?;
                self.w.write_all(a).await?;
                self.w.write_all(b"\r\n").await?;
            }
        }
        self.w.flush().await
    }

    /// Writes bytes already in the protocol, e.g. commands as a client sent
    /// them.
    pub async fn send_raw(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.read = 0;
        self.w.write_all(bytes).await?;
        self.w.flush().await
    }

    /// Reads one whole reply, RESP2 or RESP3, appending its exact bytes to
    /// out: an attribute and the value after it count as one reply.
    pub async fn read_raw(&mut self, out: &mut Vec<u8>) -> io::Result<()> {
        let mut pending: usize = 1;
        while pending > 0 {
            pending -= 1;
            let start = out.len();
            let n = self.r.read_until(b'\n', out).await?;
            self.read += n;
            if n == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "connection closed",
                ));
            }
            let end = out.len();
            if end - start < 3 || out[end - 2] != b'\r' {
                return Err(bad("line without CRLF"));
            }
            let num = || -> io::Result<i64> {
                std::str::from_utf8(&out[start + 1..end - 2])
                    .ok()
                    .and_then(|s| s.parse().ok())
                    .ok_or_else(|| bad("bad length"))
            };
            match out[start] {
                b'+' | b'-' | b':' | b'_' | b'#' | b',' | b'(' => {}
                b'$' | b'!' | b'=' => {
                    let len = num()?;
                    if len >= 0 {
                        let len = len as usize;
                        if len > MAX_BULK {
                            return Err(bad("bulk string too long"));
                        }
                        out.resize(end + len + 2, 0);
                        self.r.read_exact(&mut out[end..]).await?;
                        self.read += len + 2;
                    }
                }
                b'*' | b'~' | b'>' => pending += num()?.max(0) as usize,
                b'%' => pending += 2 * num()?.max(0) as usize,
                // Attributes, then the value they describe.
                b'|' => pending += 2 * num()?.max(0) as usize + 1,
                t => return Err(bad(format!("unknown reply type {:?}", t as char))),
            }
        }
        Ok(())
    }

    /// The two halves, with whatever is buffered, for copying bytes as they
    /// come (a subscribed connection).
    pub fn into_parts(self) -> (BufReader<OwnedReadHalf>, BufWriter<OwnedWriteHalf>) {
        (self.r, self.w)
    }

    /// Sends one command and reads its reply.
    pub async fn cmd<A: AsRef<[u8]>>(&mut self, args: &[A]) -> io::Result<Value> {
        self.send(&[args]).await?;
        self.read().await
    }

    /// Reads one reply.
    pub async fn read(&mut self) -> io::Result<Value> {
        // Arrays nest; keep a stack of open arrays instead of recursing.
        let mut stack: Vec<(Vec<Value>, usize)> = Vec::new();
        loop {
            let mut v = match self.read_frame().await? {
                Frame::Value(v) => v,
                Frame::Array(0) => Value::Array(Vec::new()),
                Frame::Array(n) => {
                    stack.push((Vec::with_capacity(n.min(1024)), n));
                    continue;
                }
            };
            // Close every array this value completes.
            loop {
                match stack.last_mut() {
                    None => return Ok(v),
                    Some((items, n)) => {
                        items.push(v);
                        if items.len() < *n {
                            break;
                        }
                        v = Value::Array(stack.pop().unwrap().0);
                    }
                }
            }
        }
    }

    async fn read_frame(&mut self) -> io::Result<Frame> {
        let mut line = Vec::new();
        let n = self.r.read_until(b'\n', &mut line).await?;
        self.read += n;
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "connection closed",
            ));
        }
        if line.len() < 3 || line[line.len() - 2] != b'\r' {
            return Err(bad("line without CRLF"));
        }
        let body = &line[1..line.len() - 2];
        let num = || -> io::Result<i64> {
            std::str::from_utf8(body)
                .ok()
                .and_then(|s| s.parse().ok())
                .ok_or_else(|| bad("bad number"))
        };
        Ok(Frame::Value(match line[0] {
            b'+' => Value::Simple(body.to_vec()),
            b'-' => Value::Error(body.to_vec()),
            b':' => Value::Int(num()?),
            b'$' => {
                let n = num()?;
                if n < 0 {
                    return Ok(Frame::Value(Value::Nil));
                }
                let n = n as usize;
                if n > MAX_BULK {
                    return Err(bad("bulk string too long"));
                }
                let mut buf = vec![0; n + 2];
                self.r.read_exact(&mut buf).await?;
                self.read += buf.len();
                buf.truncate(n);
                Value::Bulk(buf)
            }
            b'*' => {
                let n = num()?;
                if n < 0 {
                    return Ok(Frame::Value(Value::Nil));
                }
                return Ok(Frame::Array(n as usize));
            }
            t => return Err(bad(format!("unknown reply type {:?}", t as char))),
        }))
    }

    /// Peeks at an idle connection without waiting: one the server closed
    /// (a restart, a failover) reads EOF, and one with unread bytes is out of
    /// step. Either way it can't be reused.
    pub fn alive(&mut self) -> bool {
        if !self.r.buffer().is_empty() {
            return false;
        }
        let mut b = [0u8; 1];
        self.r.get_mut().peek(&mut b).now_or_never().is_none()
    }
}

enum Frame {
    Value(Value),
    /// An array header: this many elements follow.
    Array(usize),
}

/// The primary most sentinels agree on ("host:port"), asking each at addrs.
/// None when no answer has a majority of all of them.
pub async fn sentinel_primary(
    addrs: &[String],
    password: &str,
    name: &str,
) -> Result<String, String> {
    let asks = addrs.iter().map(|a| async move {
        let ask = async {
            let mut c = Conn::connect(a, password, Duration::from_secs(2)).await?;
            c.cmd(&["SENTINEL", "GET-MASTER-ADDR-BY-NAME", name]).await
        };
        match tokio::time::timeout(Duration::from_secs(2), ask).await {
            Ok(Ok(Value::Array(v))) if v.len() == 2 => {
                Ok(format!("{}:{}", v[0].text(), v[1].text()))
            }
            Ok(Ok(v)) => Err(format!("{a}: {}", v.text())),
            Ok(Err(e)) => Err(format!("{a}: {e}")),
            Err(_) => Err(format!("{a}: timed out")),
        }
    });
    let answers = futures::future::join_all(asks).await;
    let mut votes: std::collections::HashMap<&str, usize> = Default::default();
    let mut last = String::new();
    for a in &answers {
        match a {
            Ok(p) => *votes.entry(p.as_str()).or_default() += 1,
            Err(e) => last = e.clone(),
        }
    }
    votes
        .into_iter()
        .find(|(_, n)| *n > addrs.len() / 2)
        .map(|(p, _)| p.to_owned())
        .ok_or_else(|| format!("the sentinels don't agree on a primary ({last})"))
}

/// Whether the instance at addr says it's the primary.
pub async fn is_primary(addr: &str, password: &str) -> bool {
    let ask = async {
        let mut c = Conn::connect(addr, password, Duration::from_secs(2)).await?;
        c.cmd(&["ROLE"]).await
    };
    matches!(tokio::time::timeout(Duration::from_secs(2), ask).await,
        Ok(Ok(Value::Array(v))) if v.first().is_some_and(|r| r.text() == "master"))
}

/// Idle connections to one address.
#[derive(Clone)]
pub struct Pool {
    inner: Arc<PoolInner>,
}

struct PoolInner {
    /// Where connections go; it changes when the primary moves.
    addr: Mutex<String>,
    /// The protocol its connections speak.
    proto: u8,
    password: String,
    timeout: Duration,
    max_idle: usize,
    idle: Mutex<Vec<Conn>>,
    open: Arc<Semaphore>,
    /// Goes up whenever the address changes or the pool is reset: a
    /// connection from before is never given back to the idle list.
    generation: std::sync::atomic::AtomicU64,
}

/// A connection out of the pool. Give it back with [`Pool::put`].
pub struct Pooled {
    pub conn: Conn,
    /// It was reused rather than just opened.
    pub reused: bool,
    generation: u64,
    _permit: OwnedSemaphorePermit,
}

impl Pool {
    pub fn new(addr: impl Into<String>, password: impl Into<String>, max_open: usize) -> Pool {
        Pool::with_proto(addr, password, max_open, 2)
    }

    /// A pool whose connections speak `proto` (2 or 3).
    pub fn with_proto(
        addr: impl Into<String>,
        password: impl Into<String>,
        max_open: usize,
        proto: u8,
    ) -> Pool {
        Pool {
            inner: Arc::new(PoolInner {
                proto,
                addr: Mutex::new(addr.into()),
                password: password.into(),
                timeout: Duration::from_secs(5),
                max_idle: max_open.div_ceil(2),
                idle: Mutex::new(Vec::new()),
                open: Arc::new(Semaphore::new(max_open.max(1))),
                generation: Default::default(),
            }),
        }
    }

    pub fn addr(&self) -> String {
        self.inner.addr.lock().unwrap().clone()
    }

    /// Points new connections at addr, dropping idle ones to the old address.
    /// Returns whether it changed.
    pub fn set_addr(&self, addr: &str) -> bool {
        let mut a = self.inner.addr.lock().unwrap();
        if *a == addr {
            return false;
        }
        *a = addr.to_owned();
        drop(a);
        self.reset();
        true
    }

    pub async fn get(&self) -> io::Result<Pooled> {
        let permit = self
            .inner
            .open
            .clone()
            .acquire_owned()
            .await
            .expect("pool semaphore closed");
        let generation = self.generation();
        loop {
            let Some(mut c) = self.inner.idle.lock().unwrap().pop() else {
                break;
            };
            if c.alive() {
                return Ok(Pooled {
                    conn: c,
                    reused: true,
                    generation,
                    _permit: permit,
                });
            } // closed: drop it
        }
        let conn = Conn::connect_proto(
            &self.addr(),
            &self.inner.password,
            self.inner.timeout,
            self.inner.proto,
        )
        .await?;
        Ok(Pooled {
            conn,
            reused: false,
            generation,
            _permit: permit,
        })
    }

    fn generation(&self) -> u64 {
        self.inner
            .generation
            .load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Returns a healthy connection; a broken one is dropped.
    pub fn put(&self, p: Pooled, broken: bool) {
        if broken || p.generation != self.generation() {
            return;
        }
        let mut idle = self.inner.idle.lock().unwrap();
        if idle.len() < self.inner.max_idle {
            idle.push(p.conn);
        }
    }

    /// Drops every idle connection, e.g. after the primary moved.
    pub fn reset(&self) {
        let mut idle = self.inner.idle.lock().unwrap();
        self.inner
            .generation
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        idle.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncWriteExt;

    async fn parse(wire: &'static [u8]) -> Value {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap().to_string();
        tokio::spawn(async move {
            let (mut s, _) = l.accept().await.unwrap();
            s.write_all(wire).await.unwrap();
            tokio::time::sleep(Duration::from_millis(200)).await;
        });
        let mut c = Conn::connect(&addr, "", Duration::from_secs(1))
            .await
            .unwrap();
        c.read().await.unwrap()
    }

    async fn raw(wire: &'static [u8]) -> Vec<u8> {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap().to_string();
        tokio::spawn(async move {
            let (mut s, _) = l.accept().await.unwrap();
            s.write_all(wire).await.unwrap();
            tokio::time::sleep(Duration::from_millis(200)).await;
        });
        let mut c = Conn::connect(&addr, "", Duration::from_secs(1))
            .await
            .unwrap();
        let mut out = Vec::new();
        c.read_raw(&mut out).await.unwrap();
        out
    }

    #[tokio::test]
    async fn raw_replies() {
        // One reply each, the rest left unread.
        assert_eq!(raw(b"+OK\r\n:1\r\n").await, b"+OK\r\n");
        assert_eq!(raw(b"$3\r\na\r\n\r\n+next\r\n").await, b"$3\r\na\r\n\r\n");
        assert_eq!(
            raw(b"*2\r\n*1\r\n$-1\r\n:2\r\n+next\r\n").await,
            b"*2\r\n*1\r\n$-1\r\n:2\r\n"
        );
        // RESP3: a map, a set, null, double, bool, verbatim, and attributes.
        let m: &[u8] = b"%2\r\n+a\r\n,1.5\r\n+b\r\n~2\r\n_\r\n#t\r\n";
        assert_eq!(
            raw(b"%2\r\n+a\r\n,1.5\r\n+b\r\n~2\r\n_\r\n#t\r\n:9\r\n").await,
            m
        );
        assert_eq!(raw(b"=7\r\ntxt:abc\r\n:9\r\n").await, b"=7\r\ntxt:abc\r\n");
        assert_eq!(
            raw(b"|1\r\n+k\r\n+v\r\n:5\r\n:9\r\n").await,
            b"|1\r\n+k\r\n+v\r\n:5\r\n"
        );
        assert_eq!(raw(b"*-1\r\n:9\r\n").await, b"*-1\r\n");
    }

    #[tokio::test]
    async fn nested_arrays() {
        let v = parse(b"*3\r\n*2\r\n+OK\r\n:5\r\n*0\r\n$3\r\nabc\r\n").await;
        assert_eq!(
            v,
            Value::Array(vec![
                Value::Array(vec![Value::Simple(b"OK".to_vec()), Value::Int(5)]),
                Value::Array(vec![]),
                Value::Bulk(b"abc".to_vec()),
            ])
        );
        assert_eq!(parse(b"$-1\r\n").await, Value::Nil);
        assert_eq!(
            parse(b"-ERR no\r\n").await,
            Value::Error(b"ERR no".to_vec())
        );
        assert_eq!(
            parse(b"$4\r\na\r\nb\r\n").await,
            Value::Bulk(b"a\r\nb".to_vec())
        );
    }
}
