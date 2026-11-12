//! The Redis protocol (RESP2 and RESP3), so apps use one address that
//! follows failovers at once and sends reads to read replicas.
//!
//! Clients log in with the database's password (as `default`, through AUTH
//! or HELLO). Commands without connection state share a few connections to
//! Valkey; what holds state on a connection gets one of its own:
//!
//! - MULTI and WATCH keep one primary connection until EXEC, DISCARD or
//!   UNWATCH. If the primary changes meanwhile, the transaction fails
//!   (EXECABORT) and the client can retry it.
//! - SUBSCRIBE, PSUBSCRIBE and SSUBSCRIBE turn the client connection into a
//!   tunnel to a connection of its own on the primary. When the primary
//!   changes the tunnel closes, and the client library reconnects and
//!   subscribes again: messages published during the switch are lost, as
//!   with any failover.
//! - Blocking commands (BLPOP and the like, XREAD and XREADGROUP with BLOCK)
//!   run on a primary connection of the client's own, closed if the client
//!   goes away so nothing is popped for nobody.
//! - SELECT is remembered per client and applied to whichever connection
//!   runs its commands. CLIENT SETNAME, GETNAME, ID and SETINFO are answered
//!   here; HELLO 3 gives the client RESP3 connections.
//! - WAIT is answered here from the replicas' offsets (see `wait`).
//!
//! Refused with an error: MONITOR, CLIENT TRACKING and the other CLIENT
//! subcommands, WAITAOF, and admin commands (CONFIG, DEBUG, SHUTDOWN,
//! REPLICAOF, FAILOVER, ACL, ...).
//!
//! Reads go to read replicas by the same rules as REST, with read-your-writes
//! per connection: after a write, the connection's reads go to the primary
//! until a replica has that write (see `reads`).

use std::{
    sync::{
        Arc, RwLock,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use pyo_resp::{Conn, Pool, Pooled};
use subtle::ConstantTimeEq;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::tcp::{OwnedReadHalf, OwnedWriteHalf},
};

use crate::{Cmd, reads::Reads};

pub struct Proxy {
    pub password: String,
    /// The primary, RESP2 and RESP3 (they follow failovers together).
    pub primary: Pool,
    pub primary3: Pool,
    pub reads: Arc<Reads>,
    /// Goes up each time the primary changes.
    pub epoch: tokio::sync::watch::Receiver<u64>,
    /// Bounds one exchange with Valkey (not blocking commands).
    pub timeout: Duration,
    next_id: AtomicU64,
    /// redis_version, for HELLO.
    version: RwLock<Option<String>>,
}

/// The most commands taken from a client at once.
const BATCH: usize = 1024;
/// The longest a client's command may be.
const MAX_COMMAND: usize = 512 << 20;

const LOST: &[u8] = b"-ERR pyo-kv lost the connection to the database, try again\r\n";
const MOVED_ON: &[u8] = b"-ERR the primary changed; the transaction was discarded, try again\r\n";

impl Proxy {
    pub fn new(
        password: String,
        primary: Pool,
        primary3: Pool,
        reads: Arc<Reads>,
        epoch: tokio::sync::watch::Receiver<u64>,
        timeout: Duration,
    ) -> Proxy {
        Proxy {
            password,
            primary,
            primary3,
            reads,
            epoch,
            timeout,
            next_id: AtomicU64::new(1),
            version: RwLock::new(None),
        }
    }

    fn pool(&self, proto: u8) -> &Pool {
        if proto == 3 {
            &self.primary3
        } else {
            &self.primary
        }
    }

    fn epoch(&self) -> u64 {
        *self.epoch.borrow()
    }

    async fn version(&self) -> String {
        if let Some(v) = self.version.read().unwrap().clone() {
            return v;
        }
        let ask = async {
            let mut p = self.primary.get().await?;
            let v = p.conn.cmd(&["INFO", "server"]).await?;
            self.primary.put(p, false);
            std::io::Result::Ok(v.text())
        };
        let v = match tokio::time::timeout(Duration::from_secs(2), ask).await {
            Ok(Ok(info)) => crate::reads::info_field(&info, "redis_version").map(str::to_owned),
            _ => None,
        };
        match v {
            Some(v) => {
                *self.version.write().unwrap() = Some(v.clone());
                v
            }
            None => "7.2.4".into(),
        }
    }

    fn password_ok(&self, user: Option<&[u8]>, pass: &[u8]) -> bool {
        user.is_none_or(|u| u.eq_ignore_ascii_case(b"default"))
            && bool::from(pass.ct_eq(self.password.as_bytes()))
    }
}

pub async fn serve(proxy: Arc<Proxy>, listener: tokio::net::TcpListener) {
    loop {
        let Ok((sock, _)) = listener.accept().await else {
            continue;
        };
        let _ = sock.set_nodelay(true);
        let p = proxy.clone();
        tokio::spawn(async move {
            let (rd, wr) = sock.into_split();
            let id = p.next_id.fetch_add(1, Ordering::Relaxed);
            let mut s = Session {
                authed: p.password.is_empty(),
                proto: 2,
                db: 0,
                name: None,
                id,
                pin: None,
                in_multi: false,
                broken: false,
                blocking: None,
                need: 0,
                wrote: false,
                rd,
                buf: Vec::with_capacity(16 << 10),
                out: Vec::with_capacity(16 << 10),
                wr: Some(wr),
            };
            s.run(&p).await;
        });
    }
}

/// A primary connection held for a transaction.
struct Pin {
    conn: Pooled,
    proto: u8,
    epoch: u64,
}

struct Session {
    authed: bool,
    proto: u8,
    db: i64,
    name: Option<Vec<u8>>,
    id: u64,
    /// Held from MULTI or WATCH until EXEC, DISCARD or UNWATCH.
    pin: Option<Pin>,
    in_multi: bool,
    /// The primary changed during the transaction: it's lost.
    broken: bool,
    /// For blocking commands: (connection, the primary epoch it was made in).
    blocking: Option<(Conn, u64)>,
    /// Read-your-writes: replicas must have the primary's offset at this look.
    need: u64,
    wrote: bool,
    rd: OwnedReadHalf,
    buf: Vec<u8>,
    out: Vec<u8>,
    wr: Option<OwnedWriteHalf>,
}

/// Where a run of commands goes.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Dest {
    Primary,
    Replica(usize),
}

enum Next {
    Go,
    Close,
    /// Subscribed: from the command at this index on, tunnel.
    Tunnel(usize),
}

impl Session {
    async fn run(&mut self, p: &Proxy) {
        loop {
            let cmds = match self.take() {
                Ok(c) => c,
                Err(e) => {
                    let _ = self
                        .write(format!("-ERR Protocol error: {e}\r\n").as_bytes())
                        .await;
                    return;
                }
            };
            if cmds.is_empty() {
                match self.rd.read_buf(&mut self.buf).await {
                    Ok(0) | Err(_) => return,
                    Ok(_) => continue,
                }
            }
            let next = self.batch(p, &cmds).await;
            if !self.out.is_empty() {
                let out = std::mem::take(&mut self.out);
                if self.write(&out).await.is_err() {
                    return;
                }
                self.out = out;
                self.out.clear();
            }
            match next {
                Next::Go => {}
                Next::Close => return,
                Next::Tunnel(i) => return self.tunnel(p, &cmds[i..]).await,
            }
        }
    }

    async fn write(&mut self, b: &[u8]) -> std::io::Result<()> {
        match self.wr.as_mut() {
            Some(w) => w.write_all(b).await,
            None => Err(std::io::ErrorKind::BrokenPipe.into()),
        }
    }

    /// The complete commands in the buffer, up to BATCH.
    fn take(&mut self) -> Result<Vec<Cmd>, String> {
        let mut cmds = Vec::new();
        let mut at = 0;
        while cmds.len() < BATCH {
            match parse(&self.buf[at..])? {
                Some((cmd, n)) => {
                    at += n;
                    if !cmd.is_empty() {
                        cmds.push(cmd);
                    }
                }
                None => break,
            }
        }
        self.buf.drain(..at);
        Ok(cmds)
    }

    fn null(&self) -> &'static [u8] {
        if self.proto == 3 {
            b"_\r\n"
        } else {
            b"$-1\r\n"
        }
    }

    /// Runs a batch, appending each command's reply to out, in order.
    async fn batch(&mut self, p: &Proxy, cmds: &[Cmd]) -> Next {
        crate::metrics::add(&crate::metrics::COUNTERS.commands_redis, cmds.len());
        // A run of commands for the same place, sent together.
        let mut run: Vec<&Cmd> = Vec::new();
        let mut run_dest = Dest::Primary;
        // One replica for the whole batch, so a pipeline of reads stays one
        // round trip; and after a write in it, its reads go to the primary.
        let mut replica: Option<Option<usize>> = None;
        let mut wrote_here = false;
        for (i, cmd) in cmds.iter().enumerate() {
            let name = String::from_utf8_lossy(&cmd[0]).to_ascii_uppercase();
            let dest = if self.authed && self.pin.is_none() && !self.broken {
                self.route(p, &name, cmd, &mut replica, wrote_here)
            } else {
                None
            };
            if dest.is_none() || (dest == Some(Dest::Primary) && p.reads.may_write(cmd)) {
                wrote_here = true;
            }
            if let Some(d) = dest
                && (run.is_empty() || d == run_dest)
            {
                run_dest = d;
                run.push(cmd);
                continue;
            }
            if !run.is_empty() {
                self.forward(p, run_dest, &run).await;
                run.clear();
            }
            if let Some(d) = dest {
                run_dest = d;
                run.push(cmd);
                continue;
            }
            match self.special(p, &name, cmd).await {
                Next::Go => {}
                Next::Tunnel(_) => return Next::Tunnel(i),
                Next::Close => return Next::Close,
            }
        }
        if !run.is_empty() {
            self.forward(p, run_dest, &run).await;
        }
        Next::Go
    }

    /// Where a stateless command goes, or None when it needs handling here.
    fn route(
        &self,
        p: &Proxy,
        name: &str,
        cmd: &Cmd,
        replica: &mut Option<Option<usize>>,
        wrote_here: bool,
    ) -> Option<Dest> {
        if handled_here(name) || blocking(name, cmd) || refused(name).is_some() {
            return None;
        }
        if !wrote_here && p.reads.routable(std::slice::from_ref(cmd)) {
            let picked = *replica.get_or_insert_with(|| {
                let need = if self.wrote { self.need } else { 0 };
                p.reads
                    .pick_since(need)
                    .and_then(|r| p.reads.replicas.iter().position(|x| std::ptr::eq(x, r)))
            });
            if let Some(i) = picked {
                return Some(Dest::Replica(i));
            }
        }
        Some(Dest::Primary)
    }

    /// Sends a run of commands and reads their replies into out.
    async fn forward(&mut self, p: &Proxy, dest: Dest, cmds: &[&Cmd]) {
        let mut wire = Vec::new();
        for c in cmds {
            encode(&mut wire, c);
        }
        use crate::metrics::{COUNTERS as C, add};
        if let Dest::Replica(i) = dest {
            let r = &p.reads.replicas[i];
            let start = self.out.len();
            match exchange(
                r.pool(self.proto),
                self.db,
                &wire,
                cmds.len(),
                &mut self.out,
                p.reads.timeout,
                None,
            )
            .await
            {
                Ok(()) if !unready(&self.out[start..]) => {
                    add(&C.reads_replica, cmds.len());
                    return;
                }
                _ => {
                    add(&C.fallbacks_redis, 1);
                    // The primary answers instead: these are reads.
                    tracing::warn!(replica = %r.name, "read replica failed; the primary answers");
                    r.fail();
                    self.out.truncate(start);
                }
            }
        }
        // Reads the primary answers (counted with read replicas only).
        if !p.reads.replicas.is_empty() {
            add(
                &C.reads_primary,
                cmds.iter()
                    .filter(|c| p.reads.routable(std::slice::from_ref(*c)))
                    .count(),
            );
        }
        let start = self.out.len();
        let mut epoch = p.epoch.clone();
        epoch.mark_unchanged();
        let pool = p.pool(self.proto);
        match exchange(
            pool,
            self.db,
            &wire,
            cmds.len(),
            &mut self.out,
            p.timeout,
            Some(&mut epoch),
        )
        .await
        {
            Ok(()) => {}
            Err(answered) => {
                // Whatever didn't get an answer gets an error, so replies stay
                // in step with commands.
                let mut ok = 0;
                let mut at = start;
                while ok < answered {
                    at += reply_len(&self.out[at..]).unwrap_or(0);
                    ok += 1;
                }
                self.out.truncate(at);
                for _ in answered..cmds.len() {
                    self.out.extend_from_slice(LOST);
                }
                add(&C.errors_redis, cmds.len() - answered);
            }
        }
        if self.out[start..].starts_with(b"-READONLY") {
            pool.reset();
        }
        if cmds.iter().any(|c| p.reads.may_write(c)) {
            self.wrote = true;
            self.need = p.reads.after_now();
        }
    }

    /// Commands that change or depend on this connection's state.
    async fn special(&mut self, p: &Proxy, name: &str, cmd: &Cmd) -> Next {
        let arg = |i: usize| cmd.get(i).map(Vec::as_slice);
        if !self.authed && !matches!(name, "AUTH" | "HELLO" | "QUIT" | "RESET") {
            self.out
                .extend_from_slice(b"-NOAUTH Authentication required.\r\n");
            return Next::Go;
        }
        match name {
            "AUTH" => {
                let ok = match cmd.len() {
                    2 => p.password_ok(None, &cmd[1]),
                    3 => p.password_ok(Some(&cmd[1]), &cmd[2]),
                    _ => {
                        self.out.extend_from_slice(
                            b"-ERR wrong number of arguments for 'auth' command\r\n",
                        );
                        return Next::Go;
                    }
                };
                self.authed = ok;
                self.out.extend_from_slice(if ok {
                    b"+OK\r\n"
                } else {
                    b"-WRONGPASS invalid username-password pair or user is disabled.\r\n"
                });
            }
            "HELLO" => self.hello(p, cmd).await,
            "QUIT" => {
                self.out.extend_from_slice(b"+OK\r\n");
                return Next::Close;
            }
            "RESET" => {
                self.unpin(p, false);
                self.in_multi = false;
                self.broken = false;
                self.blocking = None;
                self.db = 0;
                self.proto = 2;
                self.name = None;
                self.authed = p.password.is_empty();
                self.out.extend_from_slice(b"+RESET\r\n");
            }
            _ if self.broken && self.in_multi => match name {
                "EXEC" => {
                    self.in_multi = false;
                    self.broken = false;
                    self.out.extend_from_slice(b"-EXECABORT Transaction discarded because the primary changed, try again\r\n");
                }
                "DISCARD" => {
                    self.in_multi = false;
                    self.broken = false;
                    self.out.extend_from_slice(b"+OK\r\n");
                }
                _ => self.out.extend_from_slice(MOVED_ON),
            },
            _ if self.pin.is_some() => self.pinned(p, name, cmd).await,
            "MULTI" | "WATCH" => {
                self.broken = false;
                match self.pin_primary(p).await {
                    Ok(()) => self.pinned(p, name, cmd).await,
                    Err(()) => self.out.extend_from_slice(LOST),
                }
            }
            "EXEC" => self.out.extend_from_slice(b"-ERR EXEC without MULTI\r\n"),
            "DISCARD" => self
                .out
                .extend_from_slice(b"-ERR DISCARD without MULTI\r\n"),
            "UNWATCH" => self.out.extend_from_slice(b"+OK\r\n"),
            "SELECT" => self.select(p, cmd).await,
            "CLIENT" => self.client(arg(1).unwrap_or_default(), cmd),
            "SUBSCRIBE" | "PSUBSCRIBE" | "SSUBSCRIBE" => return Next::Tunnel(0),
            "WAIT" => self.wait(p, cmd).await,
            _ if blocking(name, cmd) => return self.block(p, cmd).await,
            _ => {
                let why = refused(name).unwrap_or("it isn't supported");
                self.out.extend_from_slice(
                    format!("-ERR {name} isn't available through pyo-kv: {why}\r\n").as_bytes(),
                );
            }
        }
        Next::Go
    }

    async fn hello(&mut self, p: &Proxy, cmd: &Cmd) {
        let mut proto = self.proto;
        if let Some(v) = cmd.get(1) {
            match v.as_slice() {
                b"2" => proto = 2,
                b"3" => proto = 3,
                _ => {
                    self.out
                        .extend_from_slice(b"-NOPROTO unsupported protocol version\r\n");
                    return;
                }
            }
        }
        let mut i = 2;
        let mut name = None;
        while i < cmd.len() {
            let opt = cmd[i].to_ascii_uppercase();
            if opt == b"AUTH" && i + 2 < cmd.len() {
                if !p.password_ok(Some(&cmd[i + 1]), &cmd[i + 2]) {
                    self.out.extend_from_slice(
                        b"-WRONGPASS invalid username-password pair or user is disabled.\r\n",
                    );
                    return;
                }
                self.authed = true;
                i += 3;
            } else if opt == b"SETNAME" && i + 1 < cmd.len() {
                name = Some(cmd[i + 1].clone());
                i += 2;
            } else {
                self.out.extend_from_slice(
                    format!(
                        "-ERR Syntax error in HELLO option '{}'\r\n",
                        String::from_utf8_lossy(&cmd[i])
                    )
                    .as_bytes(),
                );
                return;
            }
        }
        if !self.authed {
            self.out.extend_from_slice(b"-NOAUTH HELLO must be called with the client already authenticated, otherwise the HELLO <proto> AUTH <user> <pass> option can be used to authenticate the client and select the RESP protocol version at the same time\r\n");
            return;
        }
        if proto != self.proto && (self.pin.is_some() || self.in_multi) {
            self.out.extend_from_slice(
                b"-ERR HELLO can't change the protocol inside a transaction\r\n",
            );
            return;
        }
        if proto != self.proto {
            self.blocking = None;
        }
        self.proto = proto;
        if name.is_some() {
            self.name = name;
        }
        let version = p.version().await;
        let fields: [(&str, Field); 7] = [
            ("server", Field::Str("redis".into())),
            ("version", Field::Str(version)),
            ("proto", Field::Int(proto as i64)),
            ("id", Field::Int(self.id as i64)),
            ("mode", Field::Str("standalone".into())),
            ("role", Field::Str("master".into())),
            ("modules", Field::Empty),
        ];
        let o = &mut self.out;
        o.extend_from_slice(
            if proto == 3 {
                format!("%{}\r\n", fields.len())
            } else {
                format!("*{}\r\n", fields.len() * 2)
            }
            .as_bytes(),
        );
        for (k, v) in fields {
            bulk(o, k.as_bytes());
            match v {
                Field::Str(s) => bulk(o, s.as_bytes()),
                Field::Int(n) => o.extend_from_slice(format!(":{n}\r\n").as_bytes()),
                Field::Empty => o.extend_from_slice(b"*0\r\n"),
            }
        }
    }

    fn client(&mut self, sub: &[u8], cmd: &Cmd) {
        match sub.to_ascii_uppercase().as_slice() {
            b"SETNAME" if cmd.len() == 3 => {
                self.name = (!cmd[2].is_empty()).then(|| cmd[2].clone());
                self.out.extend_from_slice(b"+OK\r\n");
            }
            b"GETNAME" => match self.name.clone() {
                Some(n) => bulk(&mut self.out, &n),
                None => self.out.extend_from_slice(self.null()),
            },
            b"ID" => self
                .out
                .extend_from_slice(format!(":{}\r\n", self.id).as_bytes()),
            b"SETINFO" => self.out.extend_from_slice(b"+OK\r\n"),
            other => {
                let other = String::from_utf8_lossy(other);
                self.out.extend_from_slice(format!("-ERR CLIENT {other} isn't available through pyo-kv: it acts on one server connection, and clients share them\r\n").as_bytes());
            }
        }
    }

    async fn select(&mut self, p: &Proxy, cmd: &Cmd) {
        let pool = p.pool(self.proto);
        let mut wire = Vec::new();
        encode(&mut wire, cmd);
        let start = self.out.len();
        // Run it on a connection to check it (and to give the server's error).
        match exchange(pool, self.db, &wire, 1, &mut self.out, p.timeout, None).await {
            Ok(()) if self.out[start..].starts_with(b"+OK") => {
                self.db = String::from_utf8_lossy(&cmd[1]).parse().unwrap_or(0);
                // exchange left that connection on the new database; others
                // switch when they're next used.
            }
            Ok(()) => {}
            Err(_) => {
                self.out.truncate(start);
                self.out.extend_from_slice(LOST);
            }
        }
    }

    async fn pin_primary(&mut self, p: &Proxy) -> Result<(), ()> {
        let pool = p.pool(self.proto);
        let epoch = p.epoch();
        let mut conn = match tokio::time::timeout(p.timeout, pool.get()).await {
            Ok(Ok(c)) => c,
            _ => return Err(()),
        };
        if conn.conn.db != self.db && select_db(&mut conn.conn, self.db).await.is_err() {
            return Err(());
        }
        self.pin = Some(Pin {
            conn,
            proto: self.proto,
            epoch,
        });
        Ok(())
    }

    /// Gives the pinned connection back: to the pool when it's clean.
    fn unpin(&mut self, p: &Proxy, clean: bool) {
        if let Some(pin) = self.pin.take() {
            p.pool(pin.proto).put(pin.conn, !clean);
        }
    }

    /// A command on the transaction's connection.
    async fn pinned(&mut self, p: &Proxy, name: &str, cmd: &Cmd) {
        let pin = self.pin.as_mut().unwrap();
        if pin.epoch != p.epoch() {
            // The primary changed: this connection is to the old one.
            self.unpin(p, false);
            self.broken = self.in_multi || name == "MULTI";
            self.in_multi = self.broken;
            match name {
                "EXEC" => {
                    self.in_multi = false;
                    self.broken = false;
                    self.out.extend_from_slice(b"-EXECABORT Transaction discarded because the primary changed, try again\r\n");
                }
                "DISCARD" | "UNWATCH" => {
                    self.in_multi = false;
                    self.broken = false;
                    self.out.extend_from_slice(b"+OK\r\n");
                }
                _ => self.out.extend_from_slice(MOVED_ON),
            }
            return;
        }
        let mut wire = Vec::new();
        encode(&mut wire, cmd);
        let start = self.out.len();
        let mut epoch = p.epoch.clone();
        epoch.mark_unchanged();
        let res = exchange_on(
            &mut pin.conn.conn,
            &wire,
            1,
            &mut self.out,
            p.timeout,
            Some(&mut epoch),
        )
        .await;
        if res.is_err() {
            self.out.truncate(start);
            self.unpin(p, false);
            if self.in_multi || name == "MULTI" {
                self.in_multi = name != "EXEC" && name != "DISCARD";
                self.broken = self.in_multi;
            }
            self.out.extend_from_slice(if name == "EXEC" { b"-EXECABORT Transaction discarded because the connection to the primary was lost, try again\r\n" } else { LOST });
            return;
        }
        let ok = !self.out[start..].starts_with(b"-");
        match name {
            "MULTI" if ok => self.in_multi = true,
            "SELECT" if ok => {
                let db = String::from_utf8_lossy(&cmd[1]).parse().unwrap_or(0);
                self.db = db;
                pin.conn.conn.db = db;
            }
            "EXEC" | "DISCARD" => {
                self.in_multi = false;
                self.unpin(p, true);
                if name == "EXEC" {
                    self.wrote = true;
                    self.need = p.reads.after_now();
                }
            }
            "UNWATCH" if !self.in_multi => self.unpin(p, true),
            _ => {}
        }
    }

    /// A blocking command, on this client's own primary connection.
    async fn block(&mut self, p: &Proxy, cmd: &Cmd) -> Next {
        let epoch = p.epoch();
        if self.blocking.as_ref().is_some_and(|(_, e)| *e != epoch) {
            self.blocking = None;
        }
        if self.blocking.is_none() {
            let addr = p.primary.addr();
            match Conn::connect_proto(&addr, &p.password, Duration::from_secs(5), self.proto).await
            {
                Ok(mut c) => {
                    if self.db != 0 && select_db(&mut c, self.db).await.is_err() {
                        self.out.extend_from_slice(LOST);
                        return Next::Go;
                    }
                    self.blocking = Some((c, epoch));
                }
                Err(_) => {
                    self.out.extend_from_slice(LOST);
                    return Next::Go;
                }
            }
        }
        let (conn, _) = self.blocking.as_mut().unwrap();
        let mut wire = Vec::new();
        encode(&mut wire, cmd);
        if conn.send_raw(&wire).await.is_err() {
            self.blocking = None;
            self.out.extend_from_slice(LOST);
            return Next::Go;
        }
        let mut epoch_rx = p.epoch.clone();
        epoch_rx.mark_unchanged();
        // Wait for the reply, the primary changing, or the client leaving.
        let mut reply = Vec::new();
        let outcome = {
            let read = conn.read_raw(&mut reply);
            tokio::pin!(read);
            loop {
                tokio::select! {
                    r = &mut read => break if r.is_ok() { 0 } else { 1 },
                    _ = epoch_rx.changed() => break 1,
                    n = self.rd.read_buf(&mut self.buf) => {
                        // More commands queue up behind it; nothing more
                        // coming means the client left.
                        if !matches!(n, Ok(n) if n > 0) {
                            break 2;
                        }
                    }
                }
            }
        };
        match outcome {
            0 => {
                self.out.extend_from_slice(&reply);
                self.wrote = true;
                self.need = p.reads.after_now();
                Next::Go
            }
            1 => {
                self.blocking = None;
                self.out.extend_from_slice(LOST);
                Next::Go
            }
            _ => {
                // Closing the connection unblocks it on the server, so
                // nothing is taken for a client that's gone.
                self.blocking = None;
                Next::Close
            }
        }
    }

    /// WAIT numreplicas timeout: how many replicas have this connection's
    /// last write, by asking them for their offsets. Valkey can't count it
    /// itself: the write went over a shared connection.
    async fn wait(&mut self, p: &Proxy, cmd: &Cmd) {
        let num = |i: usize| {
            cmd.get(i)
                .and_then(|v| String::from_utf8_lossy(v).parse::<i64>().ok())
        };
        let (Some(want), Some(timeout)) = (num(1), num(2)) else {
            self.out
                .extend_from_slice(b"-ERR wrong number of arguments for 'wait' command\r\n");
            return;
        };
        let deadline = (timeout > 0)
            .then(|| tokio::time::Instant::now() + Duration::from_millis(timeout as u64));
        let need = if self.wrote { self.need } else { 0 };
        let mut looked = p.reads.looked.subscribe();
        let mut count = 0;
        loop {
            let target = if need == 0 {
                Some(0)
            } else {
                p.reads.offset_at(need)
            };
            if let Some(target) = target {
                let addrs = p.reads.all_replicas.lock().unwrap().clone();
                count = count_at(&addrs, &p.password, target).await;
                if count >= want {
                    break;
                }
            }
            let tick = tokio::time::sleep(Duration::from_millis(20));
            match deadline {
                Some(d) if tokio::time::Instant::now() >= d => break,
                Some(d) => {
                    tokio::select! {
                        _ = tick => {}
                        _ = looked.changed() => {}
                        _ = tokio::time::sleep_until(d) => {}
                    }
                }
                None => {
                    tokio::select! {
                        _ = tick => {}
                        _ = looked.changed() => {}
                    }
                }
            }
        }
        self.out
            .extend_from_slice(format!(":{count}\r\n").as_bytes());
    }

    /// From here on, client and a primary connection of its own exchange
    /// bytes as they are, until either closes or the primary changes.
    async fn tunnel(&mut self, p: &Proxy, cmds: &[Cmd]) {
        let epoch = p.epoch();
        let mut conn = match Conn::connect_proto(
            &p.primary.addr(),
            &p.password,
            Duration::from_secs(5),
            self.proto,
        )
        .await
        {
            Ok(c) => c,
            Err(_) => {
                let _ = self.write(LOST).await;
                return;
            }
        };
        if self.db != 0 && select_db(&mut conn, self.db).await.is_err() {
            let _ = self.write(LOST).await;
            return;
        }
        let mut wire = Vec::new();
        for c in cmds {
            encode(&mut wire, c);
        }
        wire.extend_from_slice(&self.buf);
        if conn.send_raw(&wire).await.is_err() {
            let _ = self.write(LOST).await;
            return;
        }
        let (mut br, mut bw) = conn.into_parts();
        let Some(mut wr) = self.wr.take() else { return };
        let mut epoch_rx = p.epoch.clone();
        if *epoch_rx.borrow_and_update() != epoch {
            return;
        }
        let up = async {
            let mut b = vec![0u8; 16 << 10];
            loop {
                match self.rd.read(&mut b).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if bw.write_all(&b[..n]).await.is_err() || bw.flush().await.is_err() {
                            break;
                        }
                    }
                }
            }
        };
        let down = async {
            let mut b = vec![0u8; 16 << 10];
            loop {
                match br.read(&mut b).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if wr.write_all(&b[..n]).await.is_err() {
                            break;
                        }
                    }
                }
            }
        };
        tokio::select! {
            _ = up => {}
            _ = down => {}
            // Close: the client's library reconnects and subscribes again,
            // on the new primary.
            _ = epoch_rx.changed() => {}
        }
    }
}

enum Field {
    Str(String),
    Int(i64),
    Empty,
}

/// How many of the replicas at addrs have reached offset.
async fn count_at(addrs: &[String], password: &str, target: i64) -> i64 {
    let asks = addrs.iter().map(|a| async move {
        let ask = async {
            let mut c = Conn::connect(a, password, Duration::from_millis(500)).await?;
            c.cmd(&["INFO", "replication"]).await
        };
        match tokio::time::timeout(Duration::from_millis(500), ask).await {
            Ok(Ok(v)) => crate::reads::info_field(&v.text(), "slave_repl_offset")
                .and_then(|o| o.parse::<i64>().ok())
                .is_some_and(|o| o >= target),
            _ => false,
        }
    });
    futures::future::join_all(asks)
        .await
        .into_iter()
        .filter(|ok| *ok)
        .count() as i64
}

async fn select_db(c: &mut Conn, db: i64) -> std::io::Result<()> {
    let mut out = Vec::new();
    c.send_raw(
        format!(
            "*2\r\n$6\r\nSELECT\r\n${}\r\n{db}\r\n",
            db.to_string().len()
        )
        .as_bytes(),
    )
    .await?;
    c.read_raw(&mut out).await?;
    if out.starts_with(b"+OK") {
        c.db = db;
        Ok(())
    } else {
        Err(std::io::Error::other(
            String::from_utf8_lossy(&out).into_owned(),
        ))
    }
}

/// Sends wire (n commands) on a pooled connection and appends the n replies
/// to out. Err(answered) when it broke after `answered` replies: the
/// connection is dropped and the pool reset (its other connections likely
/// broke too). An epoch change (the primary moved) counts as broken.
async fn exchange(
    pool: &Pool,
    db: i64,
    wire: &[u8],
    n: usize,
    out: &mut Vec<u8>,
    timeout: Duration,
    epoch: Option<&mut tokio::sync::watch::Receiver<u64>>,
) -> Result<(), usize> {
    let mut p = match tokio::time::timeout(timeout, pool.get()).await {
        Ok(Ok(p)) => p,
        _ => return Err(0),
    };
    if p.conn.db != db {
        let sel = tokio::time::timeout(timeout, select_db(&mut p.conn, db)).await;
        if !matches!(sel, Ok(Ok(()))) {
            pool.put(p, true);
            return Err(0);
        }
    }
    let r = exchange_on(&mut p.conn, wire, n, out, timeout, epoch).await;
    let broken = r.is_err();
    pool.put(p, broken);
    if broken {
        pool.reset();
    }
    r
}

async fn exchange_on(
    c: &mut Conn,
    wire: &[u8],
    n: usize,
    out: &mut Vec<u8>,
    timeout: Duration,
    epoch: Option<&mut tokio::sync::watch::Receiver<u64>>,
) -> Result<(), usize> {
    let mut answered = 0;
    let work = async {
        c.send_raw(wire).await?;
        for _ in 0..n {
            c.read_raw(out).await?;
            answered += 1;
        }
        std::io::Result::Ok(())
    };
    let res = match epoch {
        Some(e) => {
            tokio::select! {
                r = tokio::time::timeout(timeout, work) => r.ok().and_then(Result::ok),
                _ = e.changed() => None,
            }
        }
        None => tokio::time::timeout(timeout, work)
            .await
            .ok()
            .and_then(Result::ok),
    };
    match res {
        Some(()) => Ok(()),
        None => Err(answered),
    }
}

/// A replica's reply meaning "ask the primary" (any of a run's replies).
fn unready(out: &[u8]) -> bool {
    let mut at = 0;
    while at < out.len() {
        let r = &out[at..];
        // An unknown command: see lib.rs's unready.
        if r.starts_with(b"-LOADING")
            || r.starts_with(b"-MASTERDOWN")
            || r.starts_with(b"-ERR unknown command")
        {
            return true;
        }
        match reply_len(r) {
            Some(n) if n > 0 => at += n,
            _ => return false,
        }
    }
    false
}

/// The length of the first complete reply in b.
fn reply_len(b: &[u8]) -> Option<usize> {
    let mut at = 0;
    let mut pending = 1usize;
    while pending > 0 {
        pending -= 1;
        if at >= b.len() {
            return None;
        }
        let nl = b[at..].iter().position(|&c| c == b'\n')? + at;
        let line = &b[at..nl - 1];
        let num = || std::str::from_utf8(&line[1..]).ok()?.parse::<i64>().ok();
        at = nl + 1;
        match line.first()? {
            b'$' | b'!' | b'=' => {
                let n = num()?;
                if n >= 0 {
                    at += n as usize + 2;
                }
            }
            b'*' | b'~' | b'>' => pending += num()?.max(0) as usize,
            b'%' => pending += 2 * num()?.max(0) as usize,
            b'|' => pending += 2 * num()?.max(0) as usize + 1,
            _ => {}
        }
    }
    (at <= b.len()).then_some(at)
}

/// Commands this module handles itself.
fn handled_here(name: &str) -> bool {
    matches!(
        name,
        "AUTH"
            | "HELLO"
            | "QUIT"
            | "RESET"
            | "MULTI"
            | "EXEC"
            | "DISCARD"
            | "WATCH"
            | "UNWATCH"
            | "SELECT"
            | "CLIENT"
            | "SUBSCRIBE"
            | "PSUBSCRIBE"
            | "SSUBSCRIBE"
            | "WAIT"
    )
}

/// Commands that wait on the server.
pub fn blocking(name: &str, cmd: &Cmd) -> bool {
    match name {
        "BLPOP" | "BRPOP" | "BRPOPLPUSH" | "BLMOVE" | "BLMPOP" | "BZPOPMIN" | "BZPOPMAX"
        | "BZMPOP" => true,
        "XREAD" | "XREADGROUP" => cmd.iter().skip(1).any(|a| a.eq_ignore_ascii_case(b"BLOCK")),
        _ => false,
    }
}

/// Why a command isn't available, if it isn't.
pub fn refused(name: &str) -> Option<&'static str> {
    Some(match name {
        "MONITOR" => "it watches one server",
        "WAITAOF" => "writes share connections, so it can't tell which writes are this client's",
        "READONLY" | "READWRITE" | "CLUSTER" | "ASKING" => "this isn't a cluster",
        "SYNC" | "PSYNC" | "REPLCONF" | "SHUTDOWN" | "DEBUG" | "CONFIG" | "MODULE"
        | "REPLICAOF" | "SLAVEOF" | "FAILOVER" | "ACL" | "SAVE" | "BGSAVE" | "BGREWRITEAOF"
        | "MIGRATE" | "SENTINEL" | "LATENCY" | "SLOWLOG" => "it's an admin command",
        _ => return None,
    })
}

fn bulk(o: &mut Vec<u8>, b: &[u8]) {
    o.extend_from_slice(format!("${}\r\n", b.len()).as_bytes());
    o.extend_from_slice(b);
    o.extend_from_slice(b"\r\n");
}

/// A command as the protocol has it.
pub fn encode(o: &mut Vec<u8>, cmd: &[Vec<u8>]) {
    o.extend_from_slice(format!("*{}\r\n", cmd.len()).as_bytes());
    for a in cmd {
        bulk(o, a);
    }
}

/// One command from the start of b and the bytes it took, or None when
/// it isn't all there yet. An inline command (words on a line) counts too;
/// an empty line is an empty command.
pub fn parse(b: &[u8]) -> Result<Option<(Cmd, usize)>, String> {
    let Some(nl) = b.iter().position(|&c| c == b'\n') else {
        if b.len() > 64 << 10 {
            return Err("too big inline request".into());
        }
        return Ok(None);
    };
    if b[0] != b'*' {
        let line = String::from_utf8_lossy(&b[..nl]);
        return Ok(Some((
            line.split_whitespace()
                .map(|w| w.as_bytes().to_vec())
                .collect(),
            nl + 1,
        )));
    }
    let num = |line: &[u8]| -> Result<i64, String> {
        std::str::from_utf8(line)
            .ok()
            .map(|s| s.trim_end_matches('\r'))
            .and_then(|s| s.parse().ok())
            .ok_or_else(|| "invalid length".to_owned())
    };
    let n = num(&b[1..nl])?;
    if !(0..=1 << 20).contains(&n) {
        return Err("invalid multibulk length".into());
    }
    let mut at = nl + 1;
    let mut cmd = Vec::with_capacity(n as usize);
    let mut total = 0usize;
    for _ in 0..n {
        let Some(rel) = b[at..].iter().position(|&c| c == b'\n') else {
            return Ok(None);
        };
        if b[at] != b'$' {
            return Err(format!("expected '$', got '{}'", b[at] as char));
        }
        let len = num(&b[at + 1..at + rel])?;
        if len < 0 || len as usize > MAX_COMMAND {
            return Err("invalid bulk length".into());
        }
        total += len as usize;
        if total > MAX_COMMAND {
            return Err("command too big".into());
        }
        at += rel + 1;
        let end = at + len as usize;
        if b.len() < end + 2 {
            return Ok(None);
        }
        cmd.push(b[at..end].to_vec());
        at = end + 2;
    }
    Ok(Some((cmd, at)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_commands() {
        let (c, n) = parse(b"*2\r\n$3\r\nGET\r\n$1\r\nk\r\n*1").unwrap().unwrap();
        assert_eq!((c, n), (vec![b"GET".to_vec(), b"k".to_vec()], 20));
        assert!(parse(b"*2\r\n$3\r\nGET\r\n$1\r\n").unwrap().is_none());
        assert!(parse(b"*2\r\n$3\r\nGET").unwrap().is_none());
        let (c, n) = parse(b"PING hello\r\nrest").unwrap().unwrap();
        assert_eq!((c, n), (vec![b"PING".to_vec(), b"hello".to_vec()], 12));
        assert!(parse(b"*1\r\n:3\r\n").is_err());
        let mut w = Vec::new();
        encode(&mut w, &[b"SET".to_vec(), b"a\r\nb".to_vec()]);
        assert_eq!(
            parse(&w).unwrap().unwrap().0,
            vec![b"SET".to_vec(), b"a\r\nb".to_vec()]
        );
    }

    #[test]
    fn reply_lengths() {
        assert_eq!(reply_len(b"+OK\r\n:1\r\n"), Some(5));
        assert_eq!(reply_len(b"*2\r\n$1\r\na\r\n%1\r\n+k\r\n_\r\n"), Some(22));
        assert_eq!(reply_len(b"$5\r\nab"), None);
        assert!(unready(b"+OK\r\n-LOADING Valkey is loading\r\n"));
        assert!(!unready(b"+OK\r\n-ERR no\r\n"));
    }

    #[test]
    fn classifies() {
        let c = |s: &str| s.split(' ').map(|w| w.as_bytes().to_vec()).collect::<Cmd>();
        assert!(blocking("BLPOP", &c("BLPOP q 0")));
        assert!(blocking("XREAD", &c("XREAD block 100 STREAMS s $")));
        assert!(!blocking("XREAD", &c("XREAD STREAMS s 0")));
        assert!(
            refused("CONFIG").is_some() && refused("MONITOR").is_some() && refused("GET").is_none()
        );
    }
}
