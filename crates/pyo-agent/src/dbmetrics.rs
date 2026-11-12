//! Database analytics: every 30 seconds the agent reads the metrics page of
//! each database member (pods labelled `pyo.dev/database`, at their
//! container port named "metrics"), keeps the figures in [`MAP`], turns
//! counters into rates, and each heartbeat sends the latest per member with
//! the pod's CPU, memory and volumes ([`report`]).
//!
//! A member without a metrics port, or whose page doesn't answer (a pod
//! starting), still reports its CPU, memory and
//! disk; it just has no engine figures.

use std::{
    collections::{BTreeMap, HashMap},
    sync::Mutex,
    time::{Duration, Instant},
};

use k8s_openapi::api::core::v1::{PersistentVolumeClaim, Pod};
use kube::{Api, Client, ResourceExt, api::ListParams};
use pyo_proto::DatabaseMemberMetrics;

const EVERY: Duration = Duration::from_secs(30);
const TIMEOUT: Duration = Duration::from_secs(4);
/// Figures older than this aren't sent: the page stopped answering.
const FRESH: Duration = Duration::from_secs(80);
const LABEL: &str = "pyo.dev/database";

// ── The metric mapping: the one place exporter names live ──────────────────

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Kind {
    /// Only goes up: sent as a rate per second.
    Counter,
    Gauge,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Agg {
    /// Over the family's label sets (databases, pools, states).
    Sum,
    Max,
}

/// One exporter metric family and the figure it feeds.
#[derive(Debug, Clone, Copy)]
pub struct Rule {
    /// The exporter's family name.
    pub metric: &'static str,
    /// Ours (see pyo_proto::DATABASE_METRIC_KEYS).
    pub key: &'static str,
    pub kind: Kind,
    pub agg: Agg,
    /// Only samples with this label value.
    pub only: Option<(&'static str, &'static str)>,
    /// Not samples with any of these label values.
    pub except: &'static [(&'static str, &'static str)],
    /// A fallback: used only when no earlier rule for the key is on the page.
    pub alt: bool,
}

const fn r(metric: &'static str, key: &'static str, kind: Kind, agg: Agg) -> Rule {
    Rule {
        metric,
        key,
        kind,
        agg,
        only: None,
        except: &[],
        alt: false,
    }
}
const fn only(mut x: Rule, label: &'static str, value: &'static str) -> Rule {
    x.only = Some((label, value));
    x
}
const fn except(mut x: Rule, pairs: &'static [(&'static str, &'static str)]) -> Rule {
    x.except = pairs;
    x
}
const fn alt(mut x: Rule) -> Rule {
    x.alt = true;
    x
}

use Agg::{Max, Sum};
use Kind::{Counter, Gauge};

/// The databases' own database (Postgres): not `postgres`, the templates or
/// shared objects.
const APP_DB: (&str, &str) = ("datname", "app");
/// Postgres's own connections: the metrics exporter and streaming replicas.
const NOT_CLIENTS: &[(&str, &str)] = &[
    ("usename", "cnpg_metrics_exporter"),
    ("usename", "streaming_replica"),
];

/// Exporter families -> our figures, as the databases system serves them:
/// - Postgres instances (9187, "metrics"): CloudNativePG 1.30's default
///   monitoring queries (cnpg-default-monitoring), plus pyo-db-controller's own
///   lag queries, `cnpg_pyo_replica_lag` (its replicas; 0 on the primary)
///   and `cnpg_pyo_read_replica_lag` (read replicas). No statements count
///   (pg_stat_statements is off): Postgres throughput is transactions.
/// - PgBouncer (9127, "metrics"): CloudNativePG's exporter; the `app` pool
///   only, which shows once a client connects.
/// - MySQL: mysqld_exporter in MOCO's `mysqld-exporter` container (9104,
///   "mysqld-metrics"). No size figure: the primary's disk stands in.
/// - KV: pyo-kv (9121, "metrics"). Every pyo-kv pod serves each Valkey
///   member's figures (`valkey_*{member}`, the same on each: one copy is
///   kept) and its own routing counters (`pyo_kv_*`, summed over pods).
pub const MAP: &[Rule] = &[
    // Postgres instances.
    only(
        r(
            "cnpg_pg_stat_database_xact_commit",
            "transactions",
            Counter,
            Sum,
        ),
        APP_DB.0,
        APP_DB.1,
    ),
    only(
        r(
            "cnpg_pg_stat_database_xact_rollback",
            "transactions",
            Counter,
            Sum,
        ),
        APP_DB.0,
        APP_DB.1,
    ),
    only(
        r("cnpg_pg_stat_database_blks_hit", "cache_hits", Counter, Sum),
        APP_DB.0,
        APP_DB.1,
    ),
    only(
        r(
            "cnpg_pg_stat_database_blks_read",
            "cache_misses",
            Counter,
            Sum,
        ),
        APP_DB.0,
        APP_DB.1,
    ),
    except(
        r("cnpg_backends_total", "connections", Gauge, Sum),
        NOT_CLIENTS,
    ),
    except(
        only(
            r("cnpg_backends_total", "connections_active", Gauge, Sum),
            "state",
            "active",
        ),
        NOT_CLIENTS,
    ),
    only(
        r("cnpg_pg_settings_setting", "max_connections", Gauge, Max),
        "name",
        "max_connections",
    ),
    only(
        r("cnpg_pg_database_size_bytes", "size_bytes", Gauge, Sum),
        APP_DB.0,
        APP_DB.1,
    ),
    r("cnpg_pyo_replica_lag", "lag_seconds", Gauge, Max),
    r("cnpg_pyo_read_replica_lag", "lag_seconds", Gauge, Max),
    alt(r("cnpg_pg_replication_lag", "lag_seconds", Gauge, Max)),
    r("cnpg_pg_replication_in_recovery", "in_recovery", Gauge, Max),
    // PgBouncer.
    only(
        r(
            "cnpg_pgbouncer_pools_cl_active",
            "pooler_active",
            Gauge,
            Sum,
        ),
        "database",
        "app",
    ),
    only(
        r(
            "cnpg_pgbouncer_pools_cl_waiting",
            "pooler_waiting",
            Gauge,
            Sum,
        ),
        "database",
        "app",
    ),
    only(
        r(
            "cnpg_pgbouncer_pools_sv_active",
            "pooler_servers_active",
            Gauge,
            Sum,
        ),
        "database",
        "app",
    ),
    only(
        r(
            "cnpg_pgbouncer_pools_sv_idle",
            "pooler_servers_idle",
            Gauge,
            Sum,
        ),
        "database",
        "app",
    ),
    only(
        r(
            "cnpg_pgbouncer_pools_maxwait",
            "pooler_maxwait_seconds",
            Gauge,
            Max,
        ),
        "database",
        "app",
    ),
    only(
        r(
            "cnpg_pgbouncer_stats_total_query_count",
            "pooler_queries",
            Counter,
            Sum,
        ),
        "database",
        "app",
    ),
    // MySQL.
    r("mysql_global_status_questions", "queries", Counter, Sum),
    r(
        "mysql_global_status_threads_connected",
        "connections",
        Gauge,
        Max,
    ),
    r(
        "mysql_global_status_threads_running",
        "connections_active",
        Gauge,
        Max,
    ),
    r(
        "mysql_global_variables_max_connections",
        "max_connections",
        Gauge,
        Max,
    ),
    r(
        "mysql_global_status_innodb_buffer_pool_read_requests",
        "cache_requests",
        Counter,
        Sum,
    ),
    r(
        "mysql_global_status_innodb_buffer_pool_reads",
        "cache_misses",
        Counter,
        Sum,
    ),
    // Replicas only (the primary has no replication status).
    r(
        "mysql_slave_status_seconds_behind_source",
        "lag_seconds",
        Gauge,
        Max,
    ),
    // KV: each Valkey member, by its `member` label.
    r("valkey_commands_processed_total", "commands", Counter, Sum),
    alt(r(
        "valkey_instantaneous_ops_per_sec",
        "commands",
        Gauge,
        Sum,
    )),
    r("valkey_connected_clients", "connections", Gauge, Sum),
    r("valkey_keyspace_hits_total", "key_hits", Counter, Sum),
    r("valkey_keyspace_misses_total", "key_misses", Counter, Sum),
    r("valkey_evicted_keys_total", "evictions", Counter, Sum),
    r("valkey_used_memory_bytes", "memory_used_bytes", Gauge, Max),
    r("valkey_maxmemory_bytes", "maxmemory_bytes", Gauge, Max),
    r("valkey_keys", "keys", Gauge, Sum),
    r("valkey_replica_lag_seconds", "lag_seconds", Gauge, Max),
    // KV: pyo-kv's own, per pod.
    only(
        r("pyo_kv_reads_total", "reads_replica", Counter, Sum),
        "served_by",
        "replica",
    ),
    only(
        r("pyo_kv_reads_total", "reads_primary", Counter, Sum),
        "served_by",
        "primary",
    ),
    r(
        "pyo_kv_replica_fallbacks_total",
        "replica_fallbacks",
        Counter,
        Sum,
    ),
];

/// Labels that say which member a figure is about, when the page is another
/// pod's (pyo-kv reports each Valkey member's INFO).
const MEMBER_LABELS: [&str; 2] = ["member", "pod"];

// ── Prometheus text format ─────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
pub struct Sample {
    pub name: String,
    pub labels: Vec<(String, String)>,
    pub value: f64,
}

impl Sample {
    fn label(&self, k: &str) -> Option<&str> {
        self.labels
            .iter()
            .find(|(n, _)| n == k)
            .map(|(_, v)| v.as_str())
    }
}

/// Samples in a Prometheus text exposition (version 0.0.4); comments, blank
/// and malformed lines are skipped. NaN and infinite values are dropped.
pub fn parse(text: &str) -> Vec<Sample> {
    text.lines().filter_map(parse_line).collect()
}

fn parse_line(line: &str) -> Option<Sample> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
        return None;
    }
    let name_end = line.find(|c: char| c == '{' || c.is_whitespace())?;
    let name = &line[..name_end];
    if name.is_empty() {
        return None;
    }
    let mut rest = &line[name_end..];
    let mut labels = vec![];
    if rest.starts_with('{') {
        let (ls, after) = parse_labels(&rest[1..])?;
        labels = ls;
        rest = after;
    }
    let value = rest.split_whitespace().next()?;
    let value: f64 = match value {
        "+Inf" | "-Inf" | "NaN" => return None,
        v => v.parse().ok()?,
    };
    value.is_finite().then(|| Sample {
        name: name.to_owned(),
        labels,
        value,
    })
}

/// `a="x",b="y\"z"}` -> labels and what follows the closing brace.
fn parse_labels(s: &str) -> Option<(Vec<(String, String)>, &str)> {
    let mut out = vec![];
    let mut chars = s.char_indices().peekable();
    loop {
        // Skip separators.
        while let Some((_, c)) = chars.peek() {
            if *c == ',' || c.is_whitespace() {
                chars.next();
            } else {
                break;
            }
        }
        let (i, c) = chars.next()?;
        if c == '}' {
            return Some((out, &s[i + 1..]));
        }
        let start = i;
        let mut eq = None;
        for (j, c) in chars.by_ref() {
            if c == '=' {
                eq = Some(j);
                break;
            }
        }
        let key = s[start..eq?].trim().to_owned();
        if chars.next()?.1 != '"' {
            return None;
        }
        let mut val = String::new();
        loop {
            let (_, c) = chars.next()?;
            match c {
                '\\' => match chars.next()?.1 {
                    'n' => val.push('\n'),
                    other => val.push(other),
                },
                '"' => break,
                c => val.push(c),
            }
        }
        out.push((key, val));
    }
}

// ── Page -> figures ────────────────────────────────────────────────────────

/// Raw figures (counters as totals) per member: None is the page's own pod.
pub type Raw = HashMap<Option<String>, BTreeMap<&'static str, (Kind, f64)>>;

/// The mapped figures on a page. Samples labelled with one of `members`
/// (see [`MEMBER_LABELS`]) are that member's; the rest are the page's own.
pub fn extract(samples: &[Sample], members: &[String]) -> Raw {
    let mut out: Raw = HashMap::new();
    let present = |metric: &str| samples.iter().any(|s| s.name == metric);
    for (i, rule) in MAP.iter().enumerate() {
        if rule.alt
            && MAP[..i]
                .iter()
                .any(|e| e.key == rule.key && present(e.metric))
        {
            continue;
        }
        let family: Vec<&Sample> = samples.iter().filter(|s| s.name == rule.metric).collect();
        // A summed family that's on the page with none of its rows kept
        // (no client connections, no app pool yet) is 0, not unknown.
        if rule.agg == Agg::Sum
            && !family.is_empty()
            && family
                .iter()
                .all(|s| MEMBER_LABELS.iter().all(|l| s.label(l).is_none()))
        {
            out.entry(None)
                .or_default()
                .entry(rule.key)
                .or_insert((rule.kind, 0.0));
        }
        for s in family {
            if let Some((k, v)) = rule.only
                && s.label(k) != Some(v)
            {
                continue;
            }
            if rule.except.iter().any(|(k, v)| s.label(k) == Some(v)) {
                continue;
            }
            let who = MEMBER_LABELS
                .iter()
                .find_map(|l| s.label(l))
                .filter(|m| members.iter().any(|x| x == m))
                .map(str::to_owned);
            let figures = out.entry(who).or_default();
            match figures.get_mut(rule.key) {
                Some((_, acc)) => {
                    *acc = match rule.agg {
                        Agg::Sum => *acc + s.value,
                        Agg::Max => acc.max(s.value),
                    }
                }
                None => {
                    figures.insert(rule.key, (rule.kind, s.value));
                }
            }
        }
    }
    out
}

/// Last counter totals: (member, key) -> (total, at in seconds).
pub type Counters = HashMap<(String, &'static str), (f64, f64)>;

/// Figures to send: gauges as read, counters per second since the last
/// reading (none on the first, or when a counter went back: a restart).
/// MySQL's buffer pool hits are its read requests that weren't reads from
/// disk.
pub fn rates(
    member: &str,
    raw: &BTreeMap<&'static str, (Kind, f64)>,
    at: f64,
    last: &mut Counters,
) -> BTreeMap<String, f64> {
    let mut out = BTreeMap::new();
    for (key, (kind, v)) in raw {
        match kind {
            Kind::Gauge => {
                out.insert((*key).to_owned(), *v);
            }
            Kind::Counter => {
                let prev = last.insert((member.to_owned(), key), (*v, at));
                if let Some((pv, pt)) = prev
                    && *v >= pv
                    && at > pt
                {
                    out.insert((*key).to_owned(), round3((v - pv) / (at - pt)));
                }
            }
        }
    }
    if let Some(req) = out.remove("cache_requests") {
        let misses = out.get("cache_misses").copied().unwrap_or(0.0);
        out.insert("cache_hits".into(), round3((req - misses).max(0.0)));
    }
    // On a primary the lag is 0 by definition; only replicas' is kept.
    if out.get("in_recovery").is_some_and(|r| *r < 1.0) {
        out.remove("lag_seconds");
    }
    out.remove("in_recovery");
    out
}

fn round3(x: f64) -> f64 {
    (x * 1000.0).round() / 1000.0
}

// ── Members ────────────────────────────────────────────────────────────────

/// What part of database `db` a pod is, from its labels; None for pods that
/// aren't a lasting part (backups, restores).
pub fn component(labels: &BTreeMap<String, String>, db: &str) -> Option<&'static str> {
    match labels.get("pyo.dev/component").map(String::as_str) {
        Some("read") => Some("read"),
        Some("pooler") => Some("pooler"),
        Some("read-pooler") => Some("read-pooler"),
        Some("kv") => Some("proxy"),
        Some("sentinel") => Some("sentinel"),
        Some("mysql" | "valkey" | "postgres" | "instance") => Some("instance"),
        Some(_) => None,
        // CloudNativePG's instances of the database itself carry no component.
        None => (labels.get("cnpg.io/cluster").map(String::as_str) == Some(db)
            && labels.get("cnpg.io/podRole").map(String::as_str) == Some("instance"))
        .then_some("instance"),
    }
}

/// "primary" or "replica" for instances, by whichever controller labels it.
pub fn role(labels: &BTreeMap<String, String>, component: &str) -> &'static str {
    if component != "instance" {
        return "";
    }
    let r = [
        "cnpg.io/instanceRole",
        "moco.cybozu.com/role",
        "pyo.dev/role",
        "role",
    ]
    .iter()
    .find_map(|k| labels.get(*k));
    match r.map(String::as_str) {
        Some("primary") => "primary",
        _ => "replica",
    }
}

fn running(p: &Pod) -> bool {
    let phase = p
        .status
        .as_ref()
        .and_then(|s| s.phase.as_deref())
        .unwrap_or("");
    phase != "Succeeded" && phase != "Failed"
}

/// The pod's metrics port, by name: "metrics" (CloudNativePG, PgBouncer,
/// pyo-kv) or "mysqld-metrics" (MOCO's mysqld-exporter sidecar).
pub fn metrics_port(p: &Pod) -> Option<i32> {
    let ports = || {
        p.spec
            .iter()
            .flat_map(|s| s.containers.iter())
            .flat_map(|c| c.ports.iter().flatten())
    };
    ["metrics", "mysqld-metrics"]
        .iter()
        .find_map(|n| ports().find(|pt| pt.name.as_deref() == Some(n)))
        .map(|pt| pt.container_port)
}

// ── The scrape loop ────────────────────────────────────────────────────────

/// Whether a volume's figures are its own: its filesystem is about the
/// size of its claim. A claim on local-path (k3s's default) is a directory
/// on the server's disk, so the kubelet reports that whole disk for it:
/// shown, it would read as the server's disk use, not the database's.
pub fn own_disk(capacity_gi: f64, claimed_gi: Option<f64>) -> bool {
    claimed_gi.is_some_and(|c| c > 0.0 && capacity_gi <= c * 1.25 + 0.5)
}

/// (namespace, claim) -> its size in GiB, from the claims in databases'
/// namespaces (read with each scrape).
static CLAIMS: Mutex<Option<HashMap<(String, String), f64>>> = Mutex::new(None);

async fn claims(client: &Client, pods: &[Pod]) -> HashMap<(String, String), f64> {
    let mut namespaces: Vec<String> = pods.iter().filter_map(|p| p.namespace()).collect();
    namespaces.sort();
    namespaces.dedup();
    let mut out = HashMap::new();
    for ns in namespaces {
        let Ok(list) = Api::<PersistentVolumeClaim>::namespaced(client.clone(), &ns)
            .list(&ListParams::default())
            .await
        else {
            continue;
        };
        for c in list.items {
            let size = c
                .status
                .as_ref()
                .and_then(|s| s.capacity.as_ref())
                .and_then(|m| m.get("storage"))
                .map(|q| crate::observe::bytes(&q.0) / 1073741824.0);
            if let Some(size) = size {
                out.insert((ns.clone(), c.name_any()), size);
            }
        }
    }
    out
}

/// (namespace, pod) -> when read, and its figures.
type Latest = HashMap<(String, String), (Instant, BTreeMap<String, f64>)>;
static LATEST: Mutex<Option<Latest>> = Mutex::new(None);

/// A database member's pod.
#[derive(Debug, Clone)]
pub struct Member {
    pub namespace: String,
    pub pod: String,
    pub database: String,
}

/// One scrape's pages -> each member's figures, by (namespace, pod).
/// Figures labelled with another member are that member's (pyo-kv serves
/// each Valkey member's): the first copy is kept, so the identical copies
/// from every pyo-kv pod count once. Rates are kept per page and member, as
/// each copy comes from its own counters.
pub fn combine(
    pages: &[(Member, String)],
    all: &[Member],
    at: f64,
    counters: &mut Counters,
) -> HashMap<(String, String), BTreeMap<String, f64>> {
    let mut next: HashMap<(String, String), BTreeMap<String, f64>> = HashMap::new();
    for (p, page) in pages {
        let members: Vec<String> = all
            .iter()
            .filter(|q| q.namespace == p.namespace && q.database == p.database)
            .map(|q| q.pod.clone())
            .collect();
        for (who, raw) in extract(&parse(page), &members) {
            let member = who.unwrap_or_else(|| p.pod.clone());
            let figures = rates(&format!("{}/{}", p.pod, member), &raw, at, counters);
            let e = next.entry((p.namespace.clone(), member)).or_default();
            for (k, v) in figures {
                e.entry(k).or_insert(v);
            }
        }
    }
    next
}

/// Reads every member's page, every 30 seconds, for good.
pub async fn run(client: Client) {
    let http = match reqwest::Client::builder().timeout(TIMEOUT).build() {
        Ok(h) => h,
        Err(e) => {
            tracing::warn!(error = %e, "database analytics off: no HTTP client");
            return;
        }
    };
    let mut counters: Counters = HashMap::new();
    let mut tick = tokio::time::interval(EVERY);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tick.tick().await;
        let Ok(pods) = Api::<Pod>::all(client.clone())
            .list(&ListParams::default().labels(LABEL))
            .await
        else {
            continue;
        };
        let pods: Vec<Pod> = pods.items.into_iter().filter(running).collect();
        let sizes = claims(&client, &pods).await;
        *CLAIMS.lock().unwrap_or_else(|e| e.into_inner()) = Some(sizes);
        let pages = futures::future::join_all(pods.iter().map(|p| {
            let http = http.clone();
            let target = p
                .status
                .as_ref()
                .and_then(|s| s.pod_ip.clone())
                .zip(metrics_port(p));
            async move {
                let (ip, port) = target?;
                let host = if ip.contains(':') {
                    format!("[{ip}]")
                } else {
                    ip
                };
                let res = http
                    .get(format!("http://{host}:{port}/metrics"))
                    .send()
                    .await
                    .ok()?;
                if !res.status().is_success() {
                    return None;
                }
                res.text().await.ok()
            }
        }))
        .await;
        let at = pyo_proto::now_ms() as f64 / 1000.0;
        let now = Instant::now();
        let all: Vec<Member> = pods
            .iter()
            .map(|p| Member {
                namespace: p.namespace().unwrap_or_default(),
                pod: p.name_any(),
                database: p.labels().get(LABEL).cloned().unwrap_or_default(),
            })
            .collect();
        let read: Vec<(Member, String)> = all
            .iter()
            .cloned()
            .zip(pages)
            .filter_map(|(m, page)| Some((m, page?)))
            .collect();
        let next: Latest = combine(&read, &all, at, &mut counters)
            .into_iter()
            .map(|(k, v)| (k, (now, v)))
            .collect();
        // Forget counters of pods that are gone.
        counters.retain(|(k, _), _| {
            pods.iter()
                .any(|p| k.starts_with(&format!("{}/", p.name_any())))
        });
        *LATEST.lock().unwrap_or_else(|e| e.into_inner()) = Some(next);
    }
}

/// Each database member's figures for the heartbeat: its pod's CPU and
/// memory (metrics-server), its volumes (kubelet), and the latest page.
pub fn report(
    pods: &[Pod],
    usage: &HashMap<(String, String), (f64, f64)>,
    volumes: &HashMap<(String, String), pyo_proto::FsUsage>,
) -> Vec<DatabaseMemberMetrics> {
    let latest = LATEST.lock().unwrap_or_else(|e| e.into_inner());
    let sizes = CLAIMS.lock().unwrap_or_else(|e| e.into_inner());
    let mut out = vec![];
    for p in pods.iter().filter(|p| running(p)) {
        let labels = p.labels();
        let Some(db) = labels.get(LABEL) else {
            continue;
        };
        let Some(component) = component(labels, db) else {
            continue;
        };
        let ns = p.namespace().unwrap_or_default();
        let name = p.name_any();
        let (cpu, mem) = usage.get(&(ns.clone(), name.clone())).copied().unzip();
        let claims: Vec<String> = p
            .spec
            .as_ref()
            .and_then(|s| s.volumes.as_ref())
            .into_iter()
            .flatten()
            .filter_map(|v| {
                v.persistent_volume_claim
                    .as_ref()
                    .map(|c| c.claim_name.clone())
            })
            .collect();
        // Only volumes that report their own space (see own_disk).
        let disks: Vec<pyo_proto::FsUsage> = claims
            .iter()
            .filter_map(|c| {
                volumes.get(&(ns.clone(), c.clone())).copied().filter(|u| {
                    own_disk(
                        u.capacity_gi,
                        sizes
                            .as_ref()
                            .and_then(|s| s.get(&(ns.clone(), c.clone())))
                            .copied(),
                    )
                })
            })
            .collect();
        let values = latest
            .as_ref()
            .and_then(|l| l.get(&(ns.clone(), name.clone())))
            .filter(|(t, _)| t.elapsed() < FRESH)
            .map(|(_, v)| v.clone())
            .unwrap_or_default();
        out.push(DatabaseMemberMetrics {
            namespace: ns,
            database: db.clone(),
            role: role(labels, component).into(),
            component: component.into(),
            server: p
                .spec
                .as_ref()
                .and_then(|s| s.node_name.clone())
                .unwrap_or_default(),
            member: name,
            cpu: cpu.map(round3),
            memory_mi: mem.map(|m| (m / 1048576.0).round()),
            disk_used_gi: (!disks.is_empty())
                .then(|| round3(disks.iter().map(|d| d.used_gi).sum())),
            disk_capacity_gi: (!disks.is_empty())
                .then(|| round3(disks.iter().map(|d| d.capacity_gi).sum())),
            values,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const PG: &str = r#"# HELP cnpg_pg_stat_database_xact_commit Number of transactions
# TYPE cnpg_pg_stat_database_xact_commit counter
cnpg_pg_stat_database_xact_commit{datname="app"} 1000
cnpg_pg_stat_database_xact_commit{datname="postgres"} 200
cnpg_pg_stat_database_xact_rollback{datname="app"} 10
cnpg_pg_stat_database_blks_hit{datname="app"} 9900
cnpg_pg_stat_database_blks_read{datname="app"} 100
cnpg_backends_total{datname="app",usename="app",application_name="",state="active"} 3
cnpg_backends_total{datname="app",usename="app",application_name="",state="idle"} 7
cnpg_backends_total{datname="",usename="streaming_replica",application_name="pg-2",state="active"} 1
cnpg_backends_total{datname="app",usename="cnpg_metrics_exporter",application_name="",state="active"} 1
cnpg_pg_stat_database_xact_commit{datname="template1"} 50
cnpg_pg_settings_setting{name="max_connections"} 100
cnpg_pg_settings_setting{name="max_wal_senders"} 10
cnpg_pg_database_size_bytes{datname="app"} 8.5e+06
cnpg_pg_database_size_bytes{datname="postgres"} 7.5e+06
cnpg_pg_replication_lag 4
cnpg_pg_replication_in_recovery 1
cnpg_pyo_read_replica_lag 1.5
cnpg_collector_pg_wal{value="slots_max"} NaN
go_goroutines 186
"#;

    #[test]
    fn parses_the_text_format() {
        let s = parse(
            "# c\n\nm 1\nm{a=\"x\",b=\"y\\\"z,}\"} 2.5 1700000000\nbad line here\nn{a=\"1\"} +Inf\nk{} 3e2\n",
        );
        assert_eq!(s.len(), 3);
        assert_eq!(
            s[0],
            Sample {
                name: "m".into(),
                labels: vec![],
                value: 1.0
            }
        );
        assert_eq!(
            s[1].labels,
            vec![("a".into(), "x".into()), ("b".into(), "y\"z,}".into())]
        );
        assert_eq!(s[1].value, 2.5);
        assert_eq!(s[2].value, 300.0);
        // NaN and the Go runtime's figures parse; only mapped ones are kept.
        assert_eq!(
            parse(PG)
                .iter()
                .filter(|s| s.name.starts_with("cnpg_collector"))
                .count(),
            0
        );
    }

    #[test]
    fn maps_postgres() {
        let raw = &extract(&parse(PG), &[])[&None];
        assert_eq!(raw["transactions"], (Kind::Counter, 1010.0));
        assert_eq!(raw["cache_hits"], (Kind::Counter, 9900.0));
        assert_eq!(raw["connections"], (Kind::Gauge, 10.0));
        assert_eq!(raw["connections_active"], (Kind::Gauge, 3.0));
        assert_eq!(raw["max_connections"], (Kind::Gauge, 100.0));
        assert_eq!(raw["size_bytes"], (Kind::Gauge, 8.5e6));
        // pyo-db-controller's lag wins over CloudNativePG's.
        assert_eq!(raw["lag_seconds"], (Kind::Gauge, 1.5));
        let fallback = extract(&parse("cnpg_pg_replication_lag 4\n"), &[]);
        assert_eq!(fallback[&None]["lag_seconds"], (Kind::Gauge, 4.0));
    }

    #[test]
    fn no_clients_is_zero_connections() {
        let page = "cnpg_backends_total{datname=\"\",usename=\"streaming_replica\",state=\"active\"} 1\ncnpg_backends_total{datname=\"app\",usename=\"cnpg_metrics_exporter\",state=\"active\"} 1\n";
        let raw = &extract(&parse(page), &[])[&None];
        assert_eq!(raw["connections"], (Kind::Gauge, 0.0));
        assert_eq!(raw["connections_active"], (Kind::Gauge, 0.0));
    }

    #[test]
    fn counters_become_rates() {
        let mut last = Counters::new();
        let raw = |v: f64| {
            BTreeMap::from([
                ("transactions", (Kind::Counter, v)),
                ("connections", (Kind::Gauge, 5.0)),
            ])
        };
        let first = rates("pg-1", &raw(1000.0), 100.0, &mut last);
        assert_eq!(first.get("transactions"), None);
        assert_eq!(first["connections"], 5.0);
        assert_eq!(
            rates("pg-1", &raw(1300.0), 130.0, &mut last)["transactions"],
            10.0
        );
        // A restart: the counter went back, so no rate this time.
        assert_eq!(
            rates("pg-1", &raw(50.0), 160.0, &mut last).get("transactions"),
            None
        );
        assert_eq!(
            rates("pg-1", &raw(110.0), 190.0, &mut last)["transactions"],
            2.0
        );
    }

    #[test]
    fn primary_has_no_lag_and_mysql_hits_are_derived() {
        let mut last = Counters::new();
        let raw = BTreeMap::from([
            ("lag_seconds", (Kind::Gauge, 0.0)),
            ("in_recovery", (Kind::Gauge, 0.0)),
        ]);
        assert!(rates("pg-1", &raw, 1.0, &mut last).is_empty());
        let my = |req: f64, reads: f64| {
            BTreeMap::from([
                ("cache_requests", (Kind::Counter, req)),
                ("cache_misses", (Kind::Counter, reads)),
            ])
        };
        rates("my-0", &my(1000.0, 10.0), 0.0, &mut last);
        let out = rates("my-0", &my(4000.0, 40.0), 30.0, &mut last);
        assert_eq!(out["cache_hits"], 99.0);
        assert_eq!(out["cache_misses"], 1.0);
        assert!(!out.contains_key("cache_requests"));
    }

    #[test]
    fn pooler_skips_its_admin_pool() {
        let page = "cnpg_pgbouncer_pools_cl_waiting{database=\"pgbouncer\",user=\"pgbouncer\"} 4\ncnpg_pgbouncer_pools_cl_waiting{database=\"app\",user=\"app\"} 2\ncnpg_pgbouncer_pools_maxwait{database=\"app\",user=\"app\"} 3\n";
        let raw = &extract(&parse(page), &[])[&None];
        assert_eq!(raw["pooler_waiting"], (Kind::Gauge, 2.0));
        assert_eq!(raw["pooler_maxwait_seconds"], (Kind::Gauge, 3.0));
    }

    #[test]
    fn member_labels_attribute_figures() {
        let page = "valkey_connected_clients{member=\"cache-0\"} 4\nvalkey_connected_clients{member=\"cache-1\"} 2\nvalkey_connected_clients{member=\"elsewhere\"} 9\npyo_kv_reads_total{served_by=\"replica\"} 10\npyo_kv_reads_total{served_by=\"primary\"} 3\n";
        let members = vec!["cache-0".to_owned(), "cache-1".to_owned()];
        let raw = extract(&parse(page), &members);
        assert_eq!(
            raw[&Some("cache-0".into())]["connections"],
            (Kind::Gauge, 4.0)
        );
        assert_eq!(
            raw[&Some("cache-1".into())]["connections"],
            (Kind::Gauge, 2.0)
        );
        // Unknown members stay the page's own.
        assert_eq!(raw[&None]["connections"], (Kind::Gauge, 9.0));
        assert_eq!(raw[&None]["reads_replica"], (Kind::Counter, 10.0));
        assert_eq!(raw[&None]["reads_primary"], (Kind::Counter, 3.0));
    }

    #[test]
    fn kv_members_count_once_and_routing_sums() {
        let m = |pod: &str| Member {
            namespace: "ns".into(),
            pod: pod.into(),
            database: "cache".into(),
        };
        let all = vec![m("cache-0"), m("cache-1"), m("kv-a"), m("kv-b")];
        let page = |reads: f64| {
            format!(
                "valkey_connected_clients{{member=\"cache-0\"}} 4\nvalkey_commands_processed_total{{member=\"cache-0\"}} {}\npyo_kv_reads_total{{served_by=\"replica\"}} {reads}\n",
                reads * 10.0
            )
        };
        let mut counters = Counters::new();
        combine(
            &[(m("kv-a"), page(100.0)), (m("kv-b"), page(200.0))],
            &all,
            0.0,
            &mut counters,
        );
        let out = combine(
            &[(m("kv-a"), page(130.0)), (m("kv-b"), page(260.0))],
            &all,
            30.0,
            &mut counters,
        );
        let member = &out[&("ns".to_owned(), "cache-0".to_owned())];
        // One copy of the member: kv-a's, not both added up.
        assert_eq!(member["connections"], 4.0);
        assert_eq!(member["commands"], 10.0);
        // Each pyo-kv pod's own routing.
        assert_eq!(
            out[&("ns".to_owned(), "kv-a".to_owned())]["reads_replica"],
            1.0
        );
        assert_eq!(
            out[&("ns".to_owned(), "kv-b".to_owned())]["reads_replica"],
            2.0
        );
    }

    #[test]
    fn finds_the_metrics_port() {
        let pod = |containers: serde_json::Value| -> Pod {
            serde_json::from_value(serde_json::json!({ "metadata": { "name": "p" }, "spec": { "containers": containers } })).unwrap()
        };
        // MOCO: moco-agent's "agent-metrics" isn't it; the exporter sidecar's is.
        let moco = pod(serde_json::json!([
            { "name": "mysqld", "ports": [{ "name": "mysql", "containerPort": 3306 }] },
            { "name": "agent", "ports": [{ "name": "agent-metrics", "containerPort": 8080 }] },
            { "name": "mysqld-exporter", "ports": [{ "name": "mysqld-metrics", "containerPort": 9104 }] },
        ]));
        assert_eq!(metrics_port(&moco), Some(9104));
        let kv = pod(
            serde_json::json!([{ "name": "pyo-kv", "ports": [{ "name": "http", "containerPort": 8080 }, { "name": "metrics", "containerPort": 9121 }] }]),
        );
        assert_eq!(metrics_port(&kv), Some(9121));
        let valkey = pod(
            serde_json::json!([{ "name": "valkey", "ports": [{ "name": "valkey", "containerPort": 6379 }] }]),
        );
        assert_eq!(metrics_port(&valkey), None);
    }

    #[test]
    fn shared_server_disks_are_not_a_volume() {
        // local-path: a 1 GiB claim reporting the server's 340 GiB disk.
        assert!(!own_disk(339.8, Some(1.0)));
        assert!(own_disk(0.98, Some(1.0)));
        assert!(own_disk(10.2, Some(10.0)));
        assert!(!own_disk(10.0, None));
    }

    #[test]
    fn every_rule_feeds_a_known_key() {
        for r in MAP {
            assert!(
                pyo_proto::DATABASE_METRIC_KEYS.contains(&r.key)
                    || ["cache_requests", "in_recovery"].contains(&r.key),
                "{} -> {}",
                r.metric,
                r.key
            );
        }
    }

    #[test]
    fn components_and_roles() {
        let l = |pairs: &[(&str, &str)]| {
            pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect::<BTreeMap<_, _>>()
        };
        let pg1 = l(&[
            ("pyo.dev/database", "pg"),
            ("cnpg.io/cluster", "pg"),
            ("cnpg.io/podRole", "instance"),
            ("cnpg.io/instanceRole", "primary"),
        ]);
        assert_eq!(component(&pg1, "pg"), Some("instance"));
        assert_eq!(role(&pg1, "instance"), "primary");
        assert_eq!(
            component(
                &l(&[
                    ("pyo.dev/component", "read"),
                    ("cnpg.io/cluster", "pg-read")
                ]),
                "pg"
            ),
            Some("read")
        );
        assert_eq!(
            component(&l(&[("pyo.dev/component", "kv")]), "cache"),
            Some("proxy")
        );
        assert_eq!(
            component(
                &l(&[("pyo.dev/component", "postgres"), ("cnpg.io/cluster", "pg")]),
                "pg"
            ),
            Some("instance")
        );
        // The read cluster's own primary is still a read replica.
        let rr = l(&[
            ("pyo.dev/component", "read"),
            ("cnpg.io/cluster", "pg-read"),
            ("cnpg.io/instanceRole", "primary"),
        ]);
        assert_eq!(component(&rr, "pg"), Some("read"));
        assert_eq!(role(&rr, "read"), "");
        assert_eq!(
            component(&l(&[("pyo.dev/component", "backup")]), "my1"),
            None
        );
        let my = l(&[("pyo.dev/component", "mysql")]);
        assert_eq!(role(&my, "instance"), "replica");
        assert_eq!(
            role(
                &l(&[("pyo.dev/component", "valkey"), ("pyo.dev/role", "primary")]),
                "instance"
            ),
            "primary"
        );
    }
}
