//! The traffic store (`pyo-agent traffic`, the pyo-traffic pod): where a
//! cluster's website and port traffic stays.
//!
//! Vector posts the gateway's access log lines here, inside the cluster.
//! Each line is cleaned (clean.rs: redaction, country, salted visitor
//! hash, the team's IP and query string settings), kept in hourly files
//! on this pod's small volume (files.rs, capped by the team's size and age
//! limits, and not at all while its request logs are off), and added to
//! per-minute totals (agg.rs). The agent takes the finished totals and sends
//! them to Pyo as its own signed requests; it brings Pyo's context (which
//! hosts and ports are services', firewalls, limits) and Pyo's queries for
//! kept rows (the request explorer). Raw lines and addresses never leave
//! the cluster.
//!
//! Why here and not in Vector: Vector's transforms add up counters and
//! histograms, but have no mergeable distinct count (a `set` keeps every
//! value), no top lists (a tag per path is a series per path), can't take a
//! salt that changes daily without reloading, and can't answer queries for
//! the files it writes. Vector stays a reader of the log files.

pub mod agg;
pub mod clean;
pub mod files;
pub mod link;

use std::{
    collections::VecDeque,
    path::PathBuf,
    sync::{Arc, Mutex, RwLock},
    time::Duration,
};

use axum::{
    Router,
    body::Bytes,
    extract::State,
    http::{HeaderMap, StatusCode},
    routing::{get, post, put},
};
use pyo_proto::traffic::{TotalsBatch, TrafficContext, TrafficQuery};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use clean::{Cleaned, Cleaner};

/// The store's name (Deployment, Service, volume claim) and port.
pub const NAME: &str = pyo_proto::traffic::STORE;
pub const PORT: u16 = pyo_proto::traffic::STORE_PORT;
/// Where the store is, from inside the cluster.
pub fn url() -> String {
    format!("http://{NAME}.{}.svc:{PORT}", pyo_proto::rbac::NS)
}

/// Finished minutes kept for the agent while Pyo can't be reached: six
/// hours' worth, then the oldest go.
const PENDING_MAX: usize = 360;
/// The largest batch Vector sends.
const MAX_BODY: usize = 16 << 20;

/// The store's token: Vector and the agent send it. Made from the agent's
/// own token, so the agent knows it after a restart without reading a
/// Secret, and a new agent token (the cluster connected again) changes it.
pub fn token_for(agent_token: &str) -> String {
    let d = Sha256::digest(format!("pyo-traffic\n{agent_token}").as_bytes());
    d.iter().map(|b| format!("{b:02x}")).collect()
}

/// The cluster's one GeoLite2 Country database: the image the gateway's
/// proxies and the WAF place visitors with (`PYO_GEOIP_IMAGE`, pinned by the
/// agent's install), mounted read-only into the store as an image volume
/// (observability.rs). Nothing is downloaded for it in the cluster.
const GEOIP: &str = pyo_proto::system::GEOIP_PATH;

pub struct Store {
    files: files::Files,
    dir: PathBuf,
    token: String,
    /// This process's id, so a batch sent twice is stored once.
    instance: String,
    geo: Option<maxminddb::Reader<maxminddb::Mmap>>,
    ctx: RwLock<Option<TrafficContext>>,
    agg: Mutex<agg::Aggregator>,
    pending: Mutex<(u64, VecDeque<(u64, TotalsBatch)>)>,
    salt: Mutex<(String, String)>,
    /// One writer at a time on the files.
    writing: Mutex<()>,
}

impl Store {
    pub fn open(dir: PathBuf, token: String, geoip: Option<PathBuf>) -> anyhow::Result<Self> {
        let geo = geoip.and_then(|p| {
            // SAFETY: the file is a read-only image volume's, never written.
            match unsafe { maxminddb::Reader::open_mmap(&p) } {
                Ok(r) => Some(r),
                Err(e) => {
                    tracing::warn!(error = %e, path = %p.display(), "no country database; countries are unknown");
                    None
                }
            }
        });
        Ok(Store {
            files: files::Files::new(&dir)?,
            dir,
            token,
            instance: pyo_proto::signing::new_nonce().replace(['+', '/', '='], ""),
            geo,
            ctx: RwLock::new(None),
            agg: Mutex::new(agg::Aggregator::default()),
            pending: Mutex::new((0, VecDeque::new())),
            salt: Mutex::new((String::new(), String::new())),
            writing: Mutex::new(()),
        })
    }

    fn country(&self, ip: &str) -> String {
        let (Some(geo), Ok(addr)) = (&self.geo, ip.parse::<std::net::IpAddr>()) else {
            return String::new();
        };
        geo.lookup(addr)
            .ok()
            .and_then(|r| r.decode::<maxminddb::geoip2::Country>().ok().flatten())
            .and_then(|c| c.country.iso_code.map(str::to_owned))
            .unwrap_or_default()
    }

    /// The day's salt, kept on the volume (a restart keeps the day's
    /// visitors the same), the day before's deleted.
    fn salt(&self, day: &str) -> String {
        let mut g = self.salt.lock().unwrap_or_else(|e| e.into_inner());
        if g.0 == day {
            return g.1.clone();
        }
        let dir = self.dir.join("salt");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join(day);
        let salt = match std::fs::read_to_string(&path) {
            Ok(s) if !s.trim().is_empty() => s.trim().to_owned(),
            _ => {
                let s = format!("{}{}", pyo_proto::signing::new_nonce(), pyo_proto::signing::new_nonce());
                let _ = std::fs::write(&path, &s);
                s
            }
        };
        if let Ok(rd) = std::fs::read_dir(&dir) {
            for e in rd.flatten() {
                if e.file_name().to_string_lossy().as_ref() < day {
                    let _ = std::fs::remove_file(e.path());
                }
            }
        }
        *g = (day.to_owned(), salt.clone());
        salt
    }

    /// Vector's lines: cleaned, kept and added up. Returns how many were kept.
    pub fn ingest(&self, body: &[u8], now_ms: i64) -> usize {
        let text = String::from_utf8_lossy(body);
        let lines: Vec<Value> = if text.trim_start().starts_with('[') {
            serde_json::from_str(&text).unwrap_or_default()
        } else {
            text.lines().filter(|l| !l.trim().is_empty()).filter_map(|l| serde_json::from_str(l).ok()).collect()
        };
        let ctx = self.ctx.read().unwrap_or_else(|e| e.into_inner()).clone();
        let known = ctx.is_some();
        let ctx = ctx.unwrap_or_default();
        let ua = woothee::parser::Parser::new();
        let locate = |ip: &str| self.country(ip);
        let mut cleaned = Vec::with_capacity(lines.len());
        // The salt is the day's of each line (a batch can cross midnight).
        let mut salts: Vec<(String, String)> = vec![];
        for l in &lines {
            let at = l["start_time"]
                .as_str()
                .or_else(|| l["time"].as_str())
                .and_then(pyo_proto::traffic::parse_time_ms)
                .unwrap_or(now_ms);
            let day = clean::day_of(at);
            let salt = match salts.iter().find(|s| s.0 == day) {
                Some(s) => s.1.clone(),
                None => {
                    let s = self.salt(&day);
                    salts.push((day, s.clone()));
                    s
                }
            };
            let c = Cleaner { ctx: &ctx, known, salt: &salt, locate: &locate, ua: &ua };
            if let Some(x) = c.clean(l, now_ms) {
                cleaned.push(x);
            }
        }
        {
            let mut agg = self.agg.lock().unwrap_or_else(|e| e.into_inner());
            for c in &cleaned {
                agg.add(c);
            }
        }
        // Rows by kind and hour, one gzip member each. With request logs
        // off, only the WAF's findings: requests and connections are
        // counted in the totals and not kept.
        let mut groups: std::collections::BTreeMap<(&str, i64), String> = Default::default();
        for c in &cleaned {
            let (kind, at, json) = match c {
                Cleaned::Request { .. } | Cleaned::Connection { .. } if ctx.request_logs_off => continue,
                Cleaned::Request { row, .. } => ("requests", row.at, serde_json::to_string(row)),
                Cleaned::Connection { row, .. } => ("connections", row.at, serde_json::to_string(row)),
                Cleaned::Waf { row, .. } => ("waf", row.at, serde_json::to_string(row)),
            };
            let Ok(json) = json else { continue };
            let s = groups.entry((kind, files::hour_of(at))).or_default();
            s.push_str(&json);
            s.push('\n');
        }
        let _w = self.writing.lock().unwrap_or_else(|e| e.into_inner());
        for ((kind, hour), lines) in groups {
            if let Err(e) = self.files.append(kind, hour, &lines) {
                tracing::warn!(error = %e, kind, "couldn't keep rows");
            }
        }
        cleaned.len()
    }

    /// Moves finished minutes to the pending batches.
    pub fn tick(&self, now_ms: i64) {
        let done = self.agg.lock().unwrap_or_else(|e| e.into_inner()).take_finished(now_ms / 1000);
        if let Some(mut b) = done {
            let mut p = self.pending.lock().unwrap_or_else(|e| e.into_inner());
            p.0 += 1;
            b.id = format!("{}-{}", self.instance, p.0);
            let seq = p.0;
            p.1.push_back((seq, b));
            while p.1.len() > PENDING_MAX {
                p.1.pop_front();
            }
        }
    }

    /// Applies the limits: the context's, or the defaults.
    pub fn enforce(&self, now_ms: i64) {
        let (mb, days) = self
            .ctx
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .map(|c| (c.max_mb, c.max_days))
            .filter(|(mb, d)| *mb > 0 && *d > 0)
            .unwrap_or((pyo_proto::traffic::DEFAULT_MAX_MB, pyo_proto::traffic::DEFAULT_MAX_DAYS));
        let _w = self.writing.lock().unwrap_or_else(|e| e.into_inner());
        let gone = self.files.enforce(now_ms, mb << 20, days);
        if gone > 0 {
            tracing::info!(files = gone, "removed the oldest kept requests");
        }
    }

    fn pending(&self, max: usize) -> Vec<Value> {
        let p = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        p.1.iter().take(max).map(|(seq, b)| json!({ "seq": seq, "batch": b })).collect()
    }

    fn ack(&self, upto: u64) {
        let mut p = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        p.1.retain(|(s, _)| *s > upto);
    }

    fn status(&self) -> Value {
        json!({
            "bytes": self.files.size(),
            "keptSince": self.files.kept_since(),
            "pending": self.pending.lock().map(|p| p.1.len()).unwrap_or(0),
            "open": self.agg.lock().map(|a| a.len()).unwrap_or(0),
            "context": self.ctx.read().map(|c| c.is_some()).unwrap_or(false),
        })
    }
}

fn allowed(token: &str, headers: &HeaderMap) -> bool {
    let got = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or("");
    got.len() == token.len() && got.bytes().zip(token.bytes()).fold(0u8, |a, (x, y)| a | (x ^ y)) == 0
}

type Shared = Arc<Store>;

fn guard(s: &Store, h: &HeaderMap) -> Result<(), (StatusCode, String)> {
    if allowed(&s.token, h) { Ok(()) } else { Err((StatusCode::UNAUTHORIZED, String::new())) }
}

fn ok(v: Value) -> (StatusCode, String) {
    (StatusCode::OK, v.to_string())
}

pub fn router(store: Shared) -> Router {
    Router::new()
        .route(
            "/ingest",
            post(|State(s): State<Shared>, h: HeaderMap, b: Bytes| async move {
                guard(&s, &h)?;
                let s2 = s.clone();
                let kept = tokio::task::spawn_blocking(move || s2.ingest(&b, pyo_proto::now_ms())).await.unwrap_or(0);
                Ok::<_, (StatusCode, String)>(ok(json!({ "kept": kept })))
            }),
        )
        .route(
            "/context",
            put(|State(s): State<Shared>, h: HeaderMap, b: Bytes| async move {
                guard(&s, &h)?;
                let ctx: TrafficContext =
                    serde_json::from_slice(&b).map_err(|_| (StatusCode::BAD_REQUEST, String::new()))?;
                *s.ctx.write().unwrap_or_else(|e| e.into_inner()) = Some(ctx);
                Ok::<_, (StatusCode, String)>(ok(json!({ "ok": true })))
            }),
        )
        .route(
            "/totals",
            get(|State(s): State<Shared>, h: HeaderMap| async move {
                guard(&s, &h)?;
                Ok::<_, (StatusCode, String)>(ok(json!({ "batches": s.pending(30) })))
            }),
        )
        .route(
            "/totals/ack",
            post(|State(s): State<Shared>, h: HeaderMap, b: Bytes| async move {
                guard(&s, &h)?;
                let v: Value = serde_json::from_slice(&b).unwrap_or_default();
                s.ack(v["upto"].as_u64().unwrap_or(0));
                Ok::<_, (StatusCode, String)>(ok(json!({ "ok": true })))
            }),
        )
        .route(
            "/query",
            post(|State(s): State<Shared>, h: HeaderMap, b: Bytes| async move {
                guard(&s, &h)?;
                let q: TrafficQuery =
                    serde_json::from_slice(&b).map_err(|_| (StatusCode::BAD_REQUEST, String::new()))?;
                let s2 = s.clone();
                let a = tokio::task::spawn_blocking(move || s2.files.query(&q))
                    .await
                    .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, String::new()))?;
                Ok::<_, (StatusCode, String)>(ok(serde_json::to_value(a).unwrap_or_default()))
            }),
        )
        .route(
            "/status",
            get(|State(s): State<Shared>, h: HeaderMap| async move {
                guard(&s, &h)?;
                Ok::<_, (StatusCode, String)>(ok(s.status()))
            }),
        )
        .route("/healthz", get(|| async { "ok" }))
        .layer(axum::extract::DefaultBodyLimit::max(MAX_BODY))
        .with_state(store)
}

/// `pyo-agent traffic`: serves the store until the pod stops.
pub async fn run() -> anyhow::Result<()> {
    let token = std::env::var("PYO_TRAFFIC_TOKEN").map_err(|_| anyhow::anyhow!("PYO_TRAFFIC_TOKEN is required"))?;
    let dir = PathBuf::from(std::env::var("PYO_TRAFFIC_DIR").unwrap_or_else(|_| "/data".into()));
    let geoip = std::env::var("PYO_TRAFFIC_GEOIP").unwrap_or_else(|_| GEOIP.into());
    let store = Arc::new(Store::open(dir, token, Some(PathBuf::from(geoip)))?);
    let s = store.clone();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(10));
        let mut n = 0u64;
        loop {
            tick.tick().await;
            let now = pyo_proto::now_ms();
            s.tick(now);
            // The limits once a minute (and at the start).
            if n.is_multiple_of(6) {
                let s = s.clone();
                let _ = tokio::task::spawn_blocking(move || s.enforce(now)).await;
            }
            n += 1;
        }
    });
    let addr = std::net::SocketAddr::from(([0, 0, 0, 0], PORT));
    let l = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!(%addr, "keeping the cluster's traffic");
    axum::serve(l, router(store)).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_in_totals_and_rows_out() {
        let d = std::env::temp_dir().join(format!("pyo-traffic-store-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        let s = Store::open(d.clone(), "t".into(), None).unwrap();
        let now = 1_794_000_000_000i64;
        let line = |path: &str, status: u16| {
            json!({ "start_time": jiff::Timestamp::from_millisecond(now).unwrap().to_string(), "host": "a.example", "method": "GET", "path": path,
                "status": status, "remote": "203.0.113.9", "duration_ms": 12, "bytes": 10, "content_type": "text/html" })
            .to_string()
        };
        let body = format!("{}\n{}\n{}\n", line("/", 200), line("/x?token=1", 404), line("/", 500));
        assert_eq!(s.ingest(body.as_bytes(), now), 3);
        // Kept, newest first, with values redacted and no address (IP
        // setting "none" until Pyo says otherwise).
        let a = s.files.query(&TrafficQuery { rows: "requests".into(), limit: 10, status: Some("4xx".into()), ..Default::default() });
        assert_eq!(a.rows.len(), 1);
        assert_eq!(a.rows[0]["url"], "/x?token=[redacted]");
        assert_eq!(a.rows[0]["ip"], "");
        // Totals once the minute is done, acknowledged once sent.
        s.tick(now + 120_000);
        let p = s.pending(10);
        assert_eq!(p.len(), 1);
        assert_eq!(p[0]["batch"]["hosts"][0]["requests"], 3);
        assert_eq!(p[0]["batch"]["hosts"][0]["errors"], 1);
        s.ack(p[0]["seq"].as_u64().unwrap());
        assert!(s.pending(10).is_empty());
        // The day's salt is kept.
        let day = clean::day_of(now);
        assert_eq!(s.salt(&day), std::fs::read_to_string(d.join("salt").join(&day)).unwrap());
        // With Pyo's context: other hosts aren't kept.
        *s.ctx.write().unwrap() = Some(TrafficContext { ip_mode: "full".into(), ..Default::default() });
        assert_eq!(s.ingest(line("/", 200).as_bytes(), now), 0);
        // Request logs off: counted, not kept.
        let mut ctx = TrafficContext { request_logs_off: true, ..Default::default() };
        ctx.hosts.insert("a.example".into(), "svc_1".into());
        *s.ctx.write().unwrap() = Some(ctx);
        let before = s.files.query(&TrafficQuery { rows: "requests".into(), limit: 10, ..Default::default() }).rows.len();
        assert_eq!(s.ingest(line("/off", 200).as_bytes(), now), 1);
        let after = s.files.query(&TrafficQuery { rows: "requests".into(), limit: 10, ..Default::default() });
        assert_eq!(after.rows.len(), before, "not kept");
        assert!(after.rows.iter().all(|r| r["path"] != "/off"));
        s.tick(now + 180_000);
        assert_eq!(s.pending(10)[0]["batch"]["hosts"][0]["requests"], 1, "still counted");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn tokens() {
        assert_eq!(token_for("a").len(), 64);
        assert_ne!(token_for("a"), token_for("b"));
        let mut h = HeaderMap::new();
        h.insert("authorization", format!("Bearer {}", token_for("a")).parse().unwrap());
        assert!(allowed(&token_for("a"), &h));
        assert!(!allowed(&token_for("b"), &h));
    }
}
