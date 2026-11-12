//! `/metrics` on its own port (PYO_KV_METRICS_LISTEN, 0.0.0.0:9121), in
//! Prometheus' text format:
//!
//! - pyo-kv's own counters (this pod's): commands by protocol, reads by
//!   who served them, errors and fallbacks from a read replica to the
//!   primary;
//! - each Valkey instance's `INFO`, labelled `member=<pod>`, asked when
//!   scraped (at most every 2s; one INFO per instance) and the same from
//!   every pyo-kv pod, so a scraper keeps one pod's;
//! - the read replicas' lag and health as the read watcher sees them.

use std::{
    collections::HashMap,
    fmt::Write as _,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use pyo_resp::Conn;

use crate::reads::{Reads, info_field};

/// One counter per series.
#[derive(Default)]
pub struct Counters {
    pub commands_redis: AtomicU64,
    pub commands_http: AtomicU64,
    pub reads_replica: AtomicU64,
    pub reads_primary: AtomicU64,
    pub errors_redis: AtomicU64,
    pub errors_http: AtomicU64,
    pub fallbacks_redis: AtomicU64,
    pub fallbacks_http: AtomicU64,
}

pub static COUNTERS: Counters = Counters {
    commands_redis: AtomicU64::new(0),
    commands_http: AtomicU64::new(0),
    reads_replica: AtomicU64::new(0),
    reads_primary: AtomicU64::new(0),
    errors_redis: AtomicU64::new(0),
    errors_http: AtomicU64::new(0),
    fallbacks_redis: AtomicU64::new(0),
    fallbacks_http: AtomicU64::new(0),
};

pub fn add(c: &AtomicU64, n: usize) {
    c.fetch_add(n as u64, Ordering::Relaxed);
}

/// Per instance (host:port): its address once looked up, and a connection.
type Conns = tokio::sync::Mutex<HashMap<String, (Option<String>, Option<Conn>)>>;

/// The Valkey instances: (pod name, host:port).
pub struct Members {
    list: Vec<(String, String)>,
    password: String,
    reads: Arc<Reads>,
    /// Each one's address once looked up, and a connection kept open.
    conns: Conns,
    cache: tokio::sync::Mutex<Option<(Instant, String)>>,
}

const ASK: Duration = Duration::from_secs(1);
const FRESH: Duration = Duration::from_secs(2);

/// The pod a per-pod service host:port is named after.
pub fn pod_of(host_port: &str) -> String {
    host_port.split([':', '.']).next().unwrap_or("").to_owned()
}

/// The INFO fields reported, as (field, metric, kind).
const FIELDS: [(&str, &str, &str); 8] = [
    (
        "instantaneous_ops_per_sec",
        "valkey_instantaneous_ops_per_sec",
        "gauge",
    ),
    (
        "total_commands_processed",
        "valkey_commands_processed_total",
        "counter",
    ),
    ("connected_clients", "valkey_connected_clients", "gauge"),
    ("keyspace_hits", "valkey_keyspace_hits_total", "counter"),
    ("keyspace_misses", "valkey_keyspace_misses_total", "counter"),
    ("evicted_keys", "valkey_evicted_keys_total", "counter"),
    ("used_memory", "valkey_used_memory_bytes", "gauge"),
    ("maxmemory", "valkey_maxmemory_bytes", "gauge"),
];

/// A primary's replicas from its INFO: (ip:port, lag seconds, offset).
pub fn replica_lags(info: &str) -> Vec<(String, f64, i64)> {
    let field = |v: &str, k: &str| {
        v.split(',')
            .find_map(|kv| kv.strip_prefix(k)?.strip_prefix('='))
            .map(str::to_owned)
    };
    info.lines()
        .filter_map(|l| {
            let (k, v) = l.trim_end().split_once(':')?;
            if !k.starts_with("slave") || !k[5..].bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            let addr = format!("{}:{}", field(v, "ip")?, field(v, "port")?);
            Some((
                addr,
                field(v, "lag")?.parse().ok()?,
                field(v, "offset")?.parse().ok()?,
            ))
        })
        .collect()
}

/// The keys in every database (`db0:keys=3,expires=0,...`).
pub fn keys(info: &str) -> u64 {
    info.lines()
        .filter(|l| l.starts_with("db"))
        .filter_map(|l| {
            l.split_once(':')?
                .1
                .split(',')
                .find_map(|kv| kv.strip_prefix("keys=")?.parse::<u64>().ok())
        })
        .sum()
}

impl Members {
    pub fn new(list: &[String], password: &str, reads: Arc<Reads>) -> Members {
        Members {
            list: list.iter().map(|m| (pod_of(m), m.clone())).collect(),
            password: password.to_owned(),
            reads,
            conns: tokio::sync::Mutex::new(HashMap::new()),
            cache: tokio::sync::Mutex::new(None),
        }
    }

    /// Every instance's INFO, or None for one that doesn't answer; and its
    /// address (ip:port).
    async fn infos(&self) -> Vec<(String, Option<String>, Option<String>)> {
        let mut conns = self.conns.lock().await;
        for (_, hp) in &self.list {
            conns.entry(hp.clone()).or_insert((None, None));
        }
        let password = &self.password;
        let looks = conns.iter_mut().map(|(hp, (addr, conn))| async move {
            if addr.is_none() {
                *addr = tokio::time::timeout(ASK, tokio::net::lookup_host(hp.as_str()))
                    .await
                    .ok()
                    .and_then(|r| r.ok()?.next())
                    .map(|a| a.to_string());
            }
            let Some(a) = addr.clone() else {
                return (hp.clone(), None, None);
            };
            let ask = async {
                if conn.is_none() {
                    *conn = Some(Conn::connect(&a, password, ASK).await?);
                }
                conn.as_mut().unwrap().cmd(&["INFO"]).await
            };
            let info = match tokio::time::timeout(ASK, ask).await {
                Ok(Ok(v)) if !v.is_error() => Some(v.text()),
                _ => {
                    *conn = None;
                    None
                }
            };
            (hp.clone(), Some(a), info)
        });
        futures::future::join_all(looks).await
    }

    /// The whole page.
    pub async fn render(&self) -> String {
        let mut cache = self.cache.lock().await;
        if let Some((at, page)) = cache.as_ref()
            && at.elapsed() < FRESH
        {
            return page.clone();
        }
        let page = self.page().await;
        *cache = Some((Instant::now(), page.clone()));
        page
    }

    async fn page(&self) -> String {
        let mut o = String::new();
        counters(&mut o);
        let infos = self.infos().await;
        let pod = |hp: &str| {
            self.list
                .iter()
                .find(|(_, h)| h == hp)
                .map(|(p, _)| p.clone())
                .unwrap_or_else(|| pod_of(hp))
        };
        let by_addr: HashMap<String, String> = infos
            .iter()
            .filter_map(|(hp, a, _)| Some((a.clone()?, pod(hp))))
            .collect();

        let _ = writeln!(
            o,
            "# HELP valkey_up 1 when the instance answered INFO; role is primary or replica.\n# TYPE valkey_up gauge"
        );
        for (hp, _, info) in &infos {
            let role = match info.as_deref().and_then(|i| info_field(i, "role")) {
                Some("master") => "primary",
                Some(_) => "replica",
                None => "unknown",
            };
            let _ = writeln!(
                o,
                "valkey_up{{member=\"{}\",role=\"{role}\"}} {}",
                pod(hp),
                u8::from(info.is_some())
            );
        }
        for (field, metric, kind) in FIELDS {
            let _ = writeln!(o, "# TYPE {metric} {kind}");
            for (hp, _, info) in &infos {
                if let Some(v) = info
                    .as_deref()
                    .and_then(|i| info_field(i, field))
                    .and_then(|v| v.trim().parse::<f64>().ok())
                {
                    let _ = writeln!(o, "{metric}{{member=\"{}\"}} {v}", pod(hp));
                }
            }
        }
        let _ = writeln!(o, "# TYPE valkey_keys gauge");
        for (hp, _, info) in &infos {
            if let Some(i) = info {
                let _ = writeln!(o, "valkey_keys{{member=\"{}\"}} {}", pod(hp), keys(i));
            }
        }
        // Lag, as the primary reports it: seconds since each replica's last
        // acknowledgement, and how many bytes behind it is.
        let _ = writeln!(
            o,
            "# HELP valkey_replica_lag_seconds Seconds since the replica last acknowledged the primary (INFO replication).\n# TYPE valkey_replica_lag_seconds gauge"
        );
        let _ = writeln!(
            o,
            "# HELP valkey_replica_behind_bytes How far the replica's offset is behind the primary's.\n# TYPE valkey_replica_behind_bytes gauge"
        );
        for (_, _, info) in &infos {
            let Some(i) = info
                .as_deref()
                .filter(|i| info_field(i, "role") == Some("master"))
            else {
                continue;
            };
            let primary_offset: i64 = info_field(i, "master_repl_offset")
                .and_then(|v| v.parse().ok())
                .unwrap_or(0);
            for (addr, lag, offset) in replica_lags(i) {
                let member = by_addr.get(&addr).cloned().unwrap_or(addr);
                let _ = writeln!(o, "valkey_replica_lag_seconds{{member=\"{member}\"}} {lag}");
                let _ = writeln!(
                    o,
                    "valkey_replica_behind_bytes{{member=\"{member}\"}} {}",
                    (primary_offset - offset).max(0)
                );
            }
        }
        // The read replicas as pyo-kv routes to them (this pod's view).
        if !self.reads.replicas.is_empty() {
            let _ = writeln!(
                o,
                "# HELP pyo_kv_read_replica_lag_seconds How far behind the primary pyo-kv sees each read replica; missing when it can't tell.\n# TYPE pyo_kv_read_replica_lag_seconds gauge"
            );
            let _ = writeln!(o, "# TYPE pyo_kv_read_replica_healthy gauge");
            for r in &self.reads.replicas {
                let member = pod_of(&r.name);
                let lag = r.lag();
                if lag < Duration::from_secs(3600) {
                    let _ = writeln!(
                        o,
                        "pyo_kv_read_replica_lag_seconds{{member=\"{member}\"}} {}",
                        lag.as_secs_f64()
                    );
                }
                let _ = writeln!(
                    o,
                    "pyo_kv_read_replica_healthy{{member=\"{member}\"}} {}",
                    u8::from(r.healthy())
                );
            }
        }
        o
    }
}

fn counters(o: &mut String) {
    let c = &COUNTERS;
    let get = |a: &AtomicU64| a.load(Ordering::Relaxed);
    let _ = writeln!(
        o,
        "# HELP pyo_kv_commands_total Commands received, by protocol (redis: each command; http: each command of a request or pipeline).\n# TYPE pyo_kv_commands_total counter"
    );
    let _ = writeln!(
        o,
        "pyo_kv_commands_total{{protocol=\"redis\"}} {}",
        get(&c.commands_redis)
    );
    let _ = writeln!(
        o,
        "pyo_kv_commands_total{{protocol=\"http\"}} {}",
        get(&c.commands_http)
    );
    let _ = writeln!(
        o,
        "# HELP pyo_kv_reads_total Read-only commands, by who answered them (with read replicas only).\n# TYPE pyo_kv_reads_total counter"
    );
    let _ = writeln!(
        o,
        "pyo_kv_reads_total{{served_by=\"replica\"}} {}",
        get(&c.reads_replica)
    );
    let _ = writeln!(
        o,
        "pyo_kv_reads_total{{served_by=\"primary\"}} {}",
        get(&c.reads_primary)
    );
    let _ = writeln!(
        o,
        "# HELP pyo_kv_errors_total Commands (redis) or requests (http) that got an error because the database couldn't be reached or timed out.\n# TYPE pyo_kv_errors_total counter"
    );
    let _ = writeln!(
        o,
        "pyo_kv_errors_total{{protocol=\"redis\"}} {}",
        get(&c.errors_redis)
    );
    let _ = writeln!(
        o,
        "pyo_kv_errors_total{{protocol=\"http\"}} {}",
        get(&c.errors_http)
    );
    let _ = writeln!(
        o,
        "# HELP pyo_kv_replica_fallbacks_total Times a read replica failed and the primary answered instead.\n# TYPE pyo_kv_replica_fallbacks_total counter"
    );
    let _ = writeln!(
        o,
        "pyo_kv_replica_fallbacks_total{{protocol=\"redis\"}} {}",
        get(&c.fallbacks_redis)
    );
    let _ = writeln!(
        o,
        "pyo_kv_replica_fallbacks_total{{protocol=\"http\"}} {}",
        get(&c.fallbacks_http)
    );
}

/// Serves `/metrics` (anything else: 404).
pub async fn serve(members: Arc<Members>, listener: tokio::net::TcpListener) {
    let app = axum::Router::new().route(
        "/metrics",
        axum::routing::get(move || {
            let m = members.clone();
            async move {
                (
                    [(
                        axum::http::header::CONTENT_TYPE,
                        "text/plain; version=0.0.4",
                    )],
                    m.render().await,
                )
            }
        }),
    );
    if let Err(e) = axum::serve(listener, app).await {
        tracing::warn!(error = %e, "metrics listener");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_info() {
        let info = "# Replication\r\nrole:master\r\nconnected_slaves:2\r\nslave0:ip=10.43.0.5,port=6379,state=online,offset=1200,lag=0\r\nslave1:ip=10.43.0.6,port=6379,state=online,offset=1000,lag=2\r\nmaster_repl_offset:1200\r\n# Keyspace\r\ndb0:keys=3,expires=1,avg_ttl=0\r\ndb2:keys=4,expires=0,avg_ttl=0\r\n";
        assert_eq!(
            replica_lags(info),
            vec![
                ("10.43.0.5:6379".into(), 0.0, 1200),
                ("10.43.0.6:6379".into(), 2.0, 1000)
            ]
        );
        assert_eq!(keys(info), 7);
        assert_eq!(
            pod_of("cache-read-1.app.svc.cluster.local:6379"),
            "cache-read-1"
        );
        assert_eq!(pod_of("cache-0:6379"), "cache-0");
    }
}
