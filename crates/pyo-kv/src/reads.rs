//! Read replicas: read-only requests go to a healthy read replica that is
//! close enough behind the primary; everything else goes to the primary.
//!
//! A watcher asks the primary and every read replica for `INFO replication`
//! ten times a second. It keeps a short history of the primary's replication
//! offset, so a replica's offset tells how many seconds behind it is: the
//! primary had that offset at some recent moment, and the replica has at least
//! everything written before then.
//!
//! The same history gives read-your-writes on a Redis connection: each look
//! at the primary is numbered, and a write that finished before look n
//! started is in the offset look n saw. After a write, a connection's reads
//! go to the primary until a replica has the offset of the next look.

use std::{
    collections::{HashSet, VecDeque},
    sync::{
        Arc, Mutex, RwLock,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use pyo_resp::{Conn, Pool, Value};

use crate::Cmd;

/// How often the watcher looks.
const EVERY: Duration = Duration::from_millis(100);
/// How long the primary's offsets are remembered: a replica further behind
/// than this counts as lost.
const HISTORY: Duration = Duration::from_secs(30);
/// How long one look at an instance may take.
const ASK: Duration = Duration::from_secs(1);
/// How often the primary is asked again which commands replicas may
/// answer: modules turned on or off change them.
const COMMANDS_EVERY: Duration = Duration::from_secs(10);

pub struct Reads {
    pub replicas: Vec<Replica>,
    next: AtomicUsize,
    /// A replica further behind than this gets no reads.
    pub max_lag: Duration,
    /// Bounds a request on a replica; past it, the primary answers instead.
    pub timeout: Duration,
    /// After a write, reads go to the primary this long (zero: off).
    pub after_write: Duration,
    last_write: AtomicU64,
    start: Instant,
    /// Commands a replica may answer: read-only by Valkey's own flags and not
    /// blocking. Empty until the primary has said which they are.
    routable: RwLock<HashSet<String>>,
    /// Commands that don't write (Valkey flags them neither write nor
    /// may_replicate): after them, reads needn't wait for replicas.
    harmless: RwLock<HashSet<String>>,
    /// Looks at the primary started so far (see the module docs).
    started: AtomicU64,
    /// The primary's offset at each recent look.
    history: Mutex<Offsets>,
    /// The number of the latest look that finished.
    pub looked: tokio::sync::watch::Sender<u64>,
    /// Every replica of the primary (host:port), from its last look.
    pub all_replicas: Mutex<Vec<String>>,
}

pub struct Replica {
    /// As configured (a service name and port).
    pub name: String,
    /// RESP2 connections; `pool3` speaks RESP3.
    pub pool: Pool,
    pub pool3: Pool,
    healthy: AtomicBool,
    lag_ms: AtomicU64,
    /// The latest look at the primary whose offset it has.
    caught_up: AtomicU64,
}

impl Replica {
    pub fn healthy(&self) -> bool {
        self.healthy.load(Ordering::Relaxed)
    }

    pub fn lag(&self) -> Duration {
        Duration::from_millis(self.lag_ms.load(Ordering::Relaxed))
    }

    /// Takes it out of rotation until the watcher sees it well again.
    pub fn fail(&self) {
        self.healthy.store(false, Ordering::Relaxed);
        self.pool.reset();
        self.pool3.reset();
    }

    pub fn pool(&self, proto: u8) -> &Pool {
        if proto == 3 { &self.pool3 } else { &self.pool }
    }
}

impl Reads {
    pub fn new(
        names: &[String],
        password: &str,
        max_conns: usize,
        max_lag: Duration,
        timeout: Duration,
        after_write: Duration,
    ) -> Reads {
        Reads {
            replicas: names
                .iter()
                .map(|n| Replica {
                    name: n.clone(),
                    pool: Pool::new(n.clone(), password, max_conns),
                    pool3: Pool::with_proto(n.clone(), password, max_conns, 3),
                    healthy: AtomicBool::new(false),
                    lag_ms: AtomicU64::new(u64::MAX),
                    caught_up: AtomicU64::new(0),
                })
                .collect(),
            next: AtomicUsize::new(0),
            max_lag,
            timeout,
            after_write,
            last_write: AtomicU64::new(0),
            start: Instant::now(),
            routable: RwLock::new(HashSet::new()),
            harmless: RwLock::new(HashSet::new()),
            started: AtomicU64::new(0),
            history: Mutex::new(Offsets::default()),
            looked: tokio::sync::watch::channel(0).0,
            all_replicas: Mutex::new(Vec::new()),
        }
    }

    /// What a connection that just wrote must wait for: a look that starts
    /// after now.
    pub fn after_now(&self) -> u64 {
        self.started.load(Ordering::SeqCst) + 1
    }

    /// The primary's offset at look `n` or the first after it.
    pub fn offset_at(&self, n: u64) -> Option<i64> {
        self.history
            .lock()
            .unwrap()
            .0
            .iter()
            .find(|s| s.0 >= n)
            .map(|s| s.2)
    }

    /// The next healthy replica that has look `need`'s offset, taking turns.
    pub fn pick_since(&self, need: u64) -> Option<&Replica> {
        let n = self.replicas.len();
        if n == 0 {
            return None;
        }
        let first = self.next.fetch_add(1, Ordering::Relaxed);
        (0..n)
            .map(|i| &self.replicas[(first + i) % n])
            .find(|r| r.healthy() && r.caught_up.load(Ordering::SeqCst) >= need)
    }

    /// Whether a replica may answer all of cmds.
    pub fn routable(&self, cmds: &[Cmd]) -> bool {
        let r = self.routable.read().unwrap();
        !r.is_empty()
            && cmds
                .iter()
                .all(|c| r.contains(String::from_utf8_lossy(&c[0]).to_ascii_uppercase().as_str()))
    }

    pub fn set_routable(&self, names: HashSet<String>) {
        *self.routable.write().unwrap() = names;
    }

    /// Whether a command may write (unknown ones may).
    pub fn may_write(&self, cmd: &Cmd) -> bool {
        !self.harmless.read().unwrap().contains(
            String::from_utf8_lossy(&cmd[0])
                .to_ascii_uppercase()
                .as_str(),
        )
    }

    /// The next healthy replica, taking turns.
    pub fn pick(&self) -> Option<&Replica> {
        if self.after_write > Duration::ZERO {
            let last = self.last_write.load(Ordering::Relaxed);
            if last > 0
                && self.start.elapsed().as_millis() as u64
                    <= last + self.after_write.as_millis() as u64
            {
                return None;
            }
        }
        self.pick_since(0)
    }

    /// Notes a request that wrote (or may have).
    pub fn wrote(&self) {
        if self.after_write > Duration::ZERO {
            self.last_write.store(
                (self.start.elapsed().as_millis() as u64).max(1),
                Ordering::Relaxed,
            );
        }
    }
}

/// The commands Valkey flags readonly and not blocking, among `allowed`,
/// from its `COMMAND` reply.
pub fn readonly_commands(command: &Value, allowed: &HashSet<&'static str>) -> HashSet<String> {
    let Value::Array(entries) = command else {
        return HashSet::new();
    };
    entries
        .iter()
        .filter_map(|e| {
            let Value::Array(e) = e else { return None };
            let name = e.first()?.text().to_ascii_uppercase();
            let Some(Value::Array(flags)) = e.get(2) else {
                return None;
            };
            let flags: Vec<String> = flags.iter().map(Value::text).collect();
            // Not SCAN and its kin: a cursor from one instance means nothing
            // on another. Not EVALSHA_RO: scripts loaded on the primary
            // aren't on its replicas.
            let elsewhere = matches!(
                name.as_str(),
                "SCAN" | "HSCAN" | "SSCAN" | "ZSCAN" | "EVALSHA_RO"
            );
            let ok = flags.iter().any(|f| f == "readonly")
                && !flags.iter().any(|f| f == "blocking")
                && !elsewhere
                && allowed.contains(name.as_str());
            ok.then_some(name)
        })
        .collect()
}

/// The commands Valkey flags neither write nor may_replicate.
pub fn harmless_commands(command: &Value) -> HashSet<String> {
    let Value::Array(entries) = command else {
        return HashSet::new();
    };
    entries
        .iter()
        .filter_map(|e| {
            let Value::Array(e) = e else { return None };
            let name = e.first()?.text().to_ascii_uppercase();
            let Some(Value::Array(flags)) = e.get(2) else {
                return None;
            };
            (!flags
                .iter()
                .any(|f| matches!(f.text().as_str(), "write" | "may_replicate")))
            .then_some(name)
        })
        .collect()
}

/// A field of an `INFO` reply.
pub fn info_field<'a>(info: &'a str, key: &str) -> Option<&'a str> {
    info.lines()
        .find_map(|l| l.trim_end().strip_prefix(key)?.strip_prefix(':'))
}

/// The primary's replication offsets over the last HISTORY: (look, when,
/// offset).
#[derive(Default)]
pub struct Offsets(VecDeque<(u64, Instant, i64)>);

impl Offsets {
    pub fn push(&mut self, n: u64, at: Instant, offset: i64) {
        self.0.push_back((n, at, offset));
        while self
            .0
            .front()
            .is_some_and(|(_, t, _)| at.duration_since(*t) > HISTORY)
        {
            self.0.pop_front();
        }
    }

    /// The latest look whose offset a replica at `offset` has (0: none).
    pub fn caught_up(&self, offset: i64) -> u64 {
        self.0
            .iter()
            .rev()
            .find(|(_, _, o)| *o <= offset)
            .map_or(0, |(n, _, _)| *n)
    }

    pub fn clear(&mut self) {
        self.0.clear();
    }

    /// How far behind a replica at `offset` is at `now`: zero when it has
    /// the latest offset, otherwise the time since the primary was last at
    /// or below it (an upper bound). None when that's longer ago than the
    /// history goes, or there's no history yet.
    pub fn lag(&self, offset: i64, now: Instant) -> Option<Duration> {
        let (_, _, latest) = self.0.back()?;
        if offset >= *latest {
            return Some(Duration::ZERO);
        }
        self.0
            .iter()
            .rev()
            .find(|(_, _, o)| *o <= offset)
            .map(|(_, t, _)| now.duration_since(*t))
    }
}

async fn info(conn: &mut Option<Conn>, addr: &str, password: &str) -> Option<String> {
    let ask = async {
        if conn.is_none() {
            *conn = Some(Conn::connect(addr, password, ASK).await?);
        }
        conn.as_mut().unwrap().cmd(&["INFO", "replication"]).await
    };
    match tokio::time::timeout(ASK, ask).await {
        Ok(Ok(v)) if !v.is_error() => Some(v.text()),
        _ => {
            *conn = None;
            None
        }
    }
}

/// The first address a name resolves to, once it does.
async fn resolve(name: &str) -> Option<String> {
    tokio::time::timeout(ASK, tokio::net::lookup_host(name))
        .await
        .ok()?
        .ok()?
        .next()
        .map(|a| a.to_string())
}

/// Keeps every replica's health and lag current. `primary` is the pool the
/// rest of the server uses, which follows failovers.
pub async fn watch(
    reads: Arc<Reads>,
    primary: Pool,
    password: String,
    allowed: HashSet<&'static str>,
) {
    let mut primary_conn: Option<Conn> = None;
    let mut primary_addr = String::new();
    // The replicas' service addresses never change: look them up once, so a
    // slow DNS server can't stall this.
    let mut addrs: Vec<Option<String>> = vec![None; reads.replicas.len()];
    let mut conns: Vec<Option<Conn>> = (0..reads.replicas.len()).map(|_| None).collect();
    let mut tick = tokio::time::interval(EVERY);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut commands_at: Option<Instant> = None;
    loop {
        tick.tick().await;
        let addr = primary.addr();
        if addr != primary_addr {
            // A new primary: its offsets aren't comparable with the old one's,
            // and it may have other modules.
            reads.history.lock().unwrap().clear();
            primary_conn = None;
            primary_addr = addr.clone();
            commands_at = None;
        }
        if reads.routable.read().unwrap().is_empty()
            || commands_at.is_none_or(|t| t.elapsed() >= COMMANDS_EVERY)
        {
            commands_at = Some(Instant::now());
            let cmds = async {
                let mut c = Conn::connect(&addr, &password, ASK).await?;
                c.cmd(&["COMMAND"]).await
            };
            if let Ok(Ok(v)) = tokio::time::timeout(Duration::from_secs(3), cmds).await {
                let r = readonly_commands(&v, &allowed);
                if *reads.routable.read().unwrap() != r {
                    tracing::info!(commands = r.len(), "commands replicas may answer");
                }
                *reads.harmless.write().unwrap() = harmless_commands(&v);
                reads.set_routable(r);
            }
        }
        let now = Instant::now();
        let look = reads.started.fetch_add(1, Ordering::SeqCst) + 1;
        match info(&mut primary_conn, &addr, &password).await {
            Some(i) if info_field(&i, "role") == Some("master") => {
                if let Some(o) = info_field(&i, "master_repl_offset").and_then(|o| o.parse().ok()) {
                    reads.history.lock().unwrap().push(look, now, o);
                }
                *reads.all_replicas.lock().unwrap() = replica_addrs(&i);
            }
            // Not answering, or not primary (yet): nothing to compare with.
            _ => reads.history.lock().unwrap().clear(),
        }
        let looks = reads
            .replicas
            .iter()
            .zip(addrs.iter_mut())
            .zip(conns.iter_mut())
            .map(|((r, a), c)| {
                let password = &password;
                async move {
                    if a.is_none() {
                        *a = resolve(&r.name).await;
                        if let Some(ip) = a.as_deref() {
                            r.pool.set_addr(ip);
                        }
                    }
                    let ip = a.as_deref()?;
                    let i = info(c, ip, password).await?;
                    if info_field(&i, "master_link_status") != Some("up") {
                        return None;
                    }
                    info_field(&i, "slave_repl_offset").and_then(|o| o.parse::<i64>().ok())
                }
            });
        let offsets = futures::future::join_all(looks).await;
        let history = reads.history.lock().unwrap();
        for (r, o) in reads.replicas.iter().zip(offsets) {
            let lag = o.and_then(|o| history.lag(o, now));
            r.caught_up
                .store(o.map_or(0, |o| history.caught_up(o)), Ordering::SeqCst);
            r.lag_ms.store(
                lag.map_or(u64::MAX, |l| l.as_millis() as u64),
                Ordering::Relaxed,
            );
            let ok = lag.is_some_and(|l| l <= reads.max_lag);
            if r.healthy.swap(ok, Ordering::Relaxed) != ok {
                tracing::info!(replica = %r.name, healthy = ok, lag_ms = lag.map(|l| l.as_millis() as u64), "read replica");
                if !ok {
                    r.pool.reset();
                    r.pool3.reset();
                }
            }
        }
        drop(history);
        reads.looked.send_replace(look);
    }
}

/// Every replica's host:port from the primary's `INFO replication`.
pub fn replica_addrs(info: &str) -> Vec<String> {
    let field = |v: &str, k: &str| {
        v.split(',')
            .find_map(|kv| kv.strip_prefix(k)?.strip_prefix('='))
            .map(str::to_owned)
    };
    info.lines()
        .filter_map(|l| {
            let (k, v) = l.trim_end().split_once(':')?;
            if !(k.starts_with("slave")
                && k[5..].chars().all(|c| c.is_ascii_digit())
                && !k[5..].is_empty())
            {
                return None;
            }
            Some(format!("{}:{}", field(v, "ip")?, field(v, "port")?))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lag_from_offsets() {
        let t0 = Instant::now();
        let at = |ms: u64| t0 + Duration::from_millis(ms);
        let mut h = Offsets::default();
        assert_eq!(h.lag(10, t0), None);
        h.push(1, at(0), 100);
        h.push(2, at(250), 100);
        h.push(3, at(500), 180);
        h.push(4, at(750), 300);
        assert_eq!(
            (
                h.caught_up(300),
                h.caught_up(299),
                h.caught_up(180),
                h.caught_up(99)
            ),
            (4, 3, 3, 0)
        );
        assert_eq!(h.lag(300, at(750)), Some(Duration::ZERO));
        assert_eq!(h.lag(400, at(750)), Some(Duration::ZERO));
        // Has 180: everything up to the 500ms sample.
        assert_eq!(h.lag(200, at(750)), Some(Duration::from_millis(250)));
        assert_eq!(h.lag(100, at(750)), Some(Duration::from_millis(500)));
        // Behind anything remembered.
        assert_eq!(h.lag(50, at(750)), None);
        h.push(5, at(40_000), 300);
        assert_eq!(h.0.len(), 1);
    }

    #[test]
    fn replicas_from_info() {
        let i = "role:master\r\nconnected_slaves:2\r\nslave0:ip=10.0.0.1,port=6379,state=online,offset=5,lag=0\r\nslave1:ip=10.0.0.2,port=6380,state=online,offset=5,lag=0\r\nslave_read_repl_offset:5\r\n";
        assert_eq!(replica_addrs(i), ["10.0.0.1:6379", "10.0.0.2:6380"]);
    }

    #[test]
    fn readonly_from_command_flags() {
        let entry = |name: &str, flags: &[&str]| {
            Value::Array(vec![
                Value::Bulk(name.as_bytes().to_vec()),
                Value::Int(-2),
                Value::Array(
                    flags
                        .iter()
                        .map(|f| Value::Simple(f.as_bytes().to_vec()))
                        .collect(),
                ),
            ])
        };
        let reply = Value::Array(vec![
            entry("get", &["readonly", "fast"]),
            entry("set", &["write", "denyoom"]),
            entry("xread", &["readonly", "blocking"]),
            entry("keys", &["readonly"]),
            entry("ping", &["fast", "stale"]),
            entry("scan", &["readonly"]),
            entry("evalsha_ro", &["readonly", "noscript"]),
        ]);
        let allowed: HashSet<&str> = ["GET", "SET", "XREAD", "PING", "SCAN", "EVALSHA_RO"].into();
        // KEYS is readonly but not allowed here; XREAD may block.
        assert_eq!(
            readonly_commands(&reply, &allowed),
            HashSet::from(["GET".to_owned()])
        );
        let h = harmless_commands(&reply);
        assert!(h.contains("PING") && h.contains("GET") && !h.contains("SET"));
    }

    #[test]
    fn module_commands_by_their_flags() {
        let entry = |name: &str, flags: &[&str]| {
            Value::Array(vec![
                Value::Bulk(name.as_bytes().to_vec()),
                Value::Int(-2),
                Value::Array(
                    flags
                        .iter()
                        .map(|f| Value::Simple(f.as_bytes().to_vec()))
                        .collect(),
                ),
            ])
        };
        // As valkey-bundle 8.1.11 flags them: Search's reads aren't readonly.
        let reply = Value::Array(vec![
            entry("JSON.GET", &["readonly", "module", "fast"]),
            entry("JSON.SET", &["write", "denyoom", "module"]),
            entry("BF.EXISTS", &["readonly", "module", "fast"]),
            entry("BF.ADD", &["write", "denyoom", "module", "fast"]),
            entry("FT.SEARCH", &["denyoom", "module"]),
            entry("FT.INFO", &["module"]),
        ]);
        let r = readonly_commands(&reply, &crate::READS);
        assert_eq!(
            r,
            HashSet::from(["JSON.GET".to_owned(), "BF.EXISTS".to_owned()])
        );
        assert!(
            crate::READS.contains("FT.SEARCH"),
            "a read-only token may search"
        );
    }

    #[test]
    fn picks_in_turn_and_skips_unhealthy() {
        let r = Reads::new(
            &["a:1".into(), "b:1".into(), "c:1".into()],
            "",
            4,
            Duration::from_secs(1),
            Duration::from_secs(1),
            Duration::ZERO,
        );
        assert!(r.pick().is_none());
        r.replicas[0].healthy.store(true, Ordering::Relaxed);
        r.replicas[2].healthy.store(true, Ordering::Relaxed);
        let picks: Vec<&str> = (0..4).map(|_| r.pick().unwrap().name.as_str()).collect();
        assert!(picks.contains(&"a:1") && picks.contains(&"c:1") && !picks.contains(&"b:1"));
        r.replicas[0].fail();
        assert!((0..3).all(|_| r.pick().unwrap().name == "c:1"));
    }

    #[test]
    fn reads_wait_after_a_write() {
        let r = Reads::new(
            &["a:1".into()],
            "",
            4,
            Duration::from_secs(1),
            Duration::from_secs(1),
            Duration::from_secs(60),
        );
        r.replicas[0].healthy.store(true, Ordering::Relaxed);
        assert!(r.pick().is_some());
        r.wrote();
        assert!(r.pick().is_none());
        let off = Reads::new(
            &["a:1".into()],
            "",
            4,
            Duration::from_secs(1),
            Duration::from_secs(1),
            Duration::ZERO,
        );
        off.replicas[0].healthy.store(true, Ordering::Relaxed);
        off.wrote();
        assert!(off.pick().is_some());
    }

    #[test]
    fn routes_only_known_reads() {
        let r = Reads::new(
            &["a:1".into()],
            "",
            4,
            Duration::from_secs(1),
            Duration::from_secs(1),
            Duration::ZERO,
        );
        let get = vec![b"get".to_vec(), b"k".to_vec()];
        let set = vec![b"SET".to_vec(), b"k".to_vec(), b"v".to_vec()];
        // Nothing known yet: everything to the primary.
        assert!(!r.routable(std::slice::from_ref(&get)));
        r.set_routable(HashSet::from(["GET".to_owned()]));
        assert!(r.routable(std::slice::from_ref(&get)));
        assert!(!r.routable(&[get, set]));
    }
}
