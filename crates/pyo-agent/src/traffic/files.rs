//! The kept rows: one file per kind and hour (UTC), `<kind>/<YYYYMMDDHH>.ndjson.gz`,
//! one JSON row per line. Each batch is added as its own gzip member, so a
//! file is readable while it's written and a cut-off last batch loses only
//! itself. The oldest hours go first once the team's size or age limit is
//! reached.

use std::{
    collections::BinaryHeap,
    fs,
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
};

use flate2::{Compression, read::MultiGzDecoder, write::GzEncoder};
use pyo_proto::traffic::{ConnectionRow, RequestRow, TrafficAnswer, TrafficQuery, WafRow};
use serde_json::Value;

pub const KINDS: [&str; 3] = ["requests", "connections", "waf"];
/// The most a query reads (uncompressed), so a query over a very busy week
/// can't hold the store up: it answers with what it found (`partial`).
const READ_BUDGET: u64 = 2 << 30;

/// An hour's number: hours since 1970.
pub fn hour_of(ms: i64) -> i64 {
    ms.div_euclid(3_600_000)
}

fn file_name(hour: i64) -> String {
    jiff::Timestamp::from_second(hour * 3600)
        .map(|t| t.strftime("%Y%m%d%H").to_string())
        .unwrap_or_else(|_| hour.to_string())
}

fn hour_of_name(name: &str) -> Option<i64> {
    let stem = name.strip_suffix(".ndjson.gz")?;
    let t = jiff::civil::DateTime::strptime("%Y%m%d%H", stem).ok()?;
    let ts = t.to_zoned(jiff::tz::TimeZone::UTC).ok()?.timestamp();
    Some(ts.as_second() / 3600)
}

pub struct Files {
    dir: PathBuf,
}

impl Files {
    pub fn new(dir: &Path) -> anyhow::Result<Self> {
        for k in KINDS {
            fs::create_dir_all(dir.join(k))?;
        }
        Ok(Files { dir: dir.to_owned() })
    }

    /// Adds rows (JSON lines) to their hour's file, as one gzip member.
    pub fn append(&self, kind: &str, hour: i64, lines: &str) -> anyhow::Result<()> {
        if lines.is_empty() {
            return Ok(());
        }
        let mut gz = GzEncoder::new(Vec::with_capacity(lines.len() / 4), Compression::new(6));
        gz.write_all(lines.as_bytes())?;
        let bytes = gz.finish()?;
        let mut f = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.dir.join(kind).join(format!("{}.ndjson.gz", file_name(hour))))?;
        f.write_all(&bytes)?;
        Ok(())
    }

    /// Every kept file: (hour, size, path), oldest first.
    pub fn list(&self) -> Vec<(i64, u64, PathBuf)> {
        let mut out = vec![];
        for k in KINDS {
            let Ok(rd) = fs::read_dir(self.dir.join(k)) else { continue };
            for e in rd.flatten() {
                let name = e.file_name().to_string_lossy().to_string();
                let (Some(h), Ok(m)) = (hour_of_name(&name), e.metadata()) else { continue };
                out.push((h, m.len(), e.path()));
            }
        }
        out.sort_by(|a, b| a.0.cmp(&b.0).then(a.2.cmp(&b.2)));
        out
    }

    /// Bytes on disk.
    pub fn size(&self) -> u64 {
        self.list().iter().map(|f| f.1).sum()
    }

    /// The oldest kept time (ms), if anything is kept.
    pub fn kept_since(&self) -> Option<i64> {
        self.list().first().map(|f| f.0 * 3_600_000)
    }

    /// Deletes hours older than `max_days`, then the oldest until the rest
    /// fits in `max_bytes`. Returns how many files went.
    pub fn enforce(&self, now_ms: i64, max_bytes: u64, max_days: u32) -> usize {
        let oldest = hour_of(now_ms - max_days as i64 * 86_400_000);
        let mut files = self.list();
        let mut gone = 0;
        files.retain(|(h, _, p)| {
            if *h < oldest {
                gone += fs::remove_file(p).is_ok() as usize;
                false
            } else {
                true
            }
        });
        let mut total: u64 = files.iter().map(|f| f.1).sum();
        for (_, size, p) in files {
            if total <= max_bytes {
                break;
            }
            if fs::remove_file(&p).is_ok() {
                gone += 1;
                total -= size;
            }
        }
        gone
    }

    /// The newest rows that match, newest first.
    pub fn query(&self, q: &TrafficQuery) -> TrafficAnswer {
        let limit = q.limit();
        let kind = if KINDS.contains(&q.rows.as_str()) { q.rows.as_str() } else { "requests" };
        let before = if q.before_ms > 0 { q.before_ms } else { i64::MAX };
        let (from, to) = (hour_of(q.since_ms.max(0)), hour_of(before.min(i64::MAX - 1)));
        let mut files: Vec<(i64, u64, PathBuf)> = self
            .list()
            .into_iter()
            .filter(|(h, _, p)| *h >= from && *h <= to && p.parent().is_some_and(|d| d.ends_with(kind)))
            .collect();
        files.sort_by_key(|f| std::cmp::Reverse(f.0));
        // The newest `limit + 1` (one more says there are older ones):
        // a min-heap on time, so the oldest kept is dropped first.
        let mut heap: BinaryHeap<std::cmp::Reverse<(i64, usize)>> = BinaryHeap::new();
        let mut vals: std::collections::HashMap<usize, Value> = Default::default();
        let mut seq = 0usize;
        let mut read = 0u64;
        let mut partial = false;
        'files: for (_, _, path) in files {
            let Ok(f) = fs::File::open(&path) else { continue };
            let mut r = BufReader::new(MultiGzDecoder::new(BufReader::new(f)));
            let mut line = String::new();
            loop {
                line.clear();
                // A cut-off last batch ends the file early.
                match r.read_line(&mut line) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => read += n as u64,
                }
                let Some((at, v)) = matching(kind, q, line.trim_end()) else { continue };
                if at < q.since_ms || at >= before {
                    continue;
                }
                if heap.len() > limit && heap.peek().is_some_and(|m| m.0.0 >= at) {
                    continue;
                }
                seq += 1;
                heap.push(std::cmp::Reverse((at, seq)));
                vals.insert(seq, v);
                if heap.len() > limit + 1
                    && let Some(std::cmp::Reverse((_, gone))) = heap.pop()
                {
                    vals.remove(&gone);
                }
                if read > READ_BUDGET {
                    partial = true;
                    break 'files;
                }
            }
            // Files are hours, newest first: once there are enough, an
            // older hour has nothing newer.
            if heap.len() > limit {
                break;
            }
        }
        let mut rows: Vec<(i64, usize)> = heap.into_iter().map(|r| r.0).collect();
        rows.sort_by(|a, b| b.0.cmp(&a.0).then(b.1.cmp(&a.1)));
        let more = rows.len() > limit;
        rows.truncate(limit);
        TrafficAnswer {
            rows: rows.into_iter().filter_map(|r| vals.remove(&r.1)).collect(),
            more,
            kept_since: self.kept_since(),
            partial,
        }
    }
}

/// A line's time and row, when it's the query's.
fn matching(kind: &str, q: &TrafficQuery, line: &str) -> Option<(i64, Value)> {
    if line.is_empty() {
        return None;
    }
    match kind {
        "connections" => {
            let r: ConnectionRow = serde_json::from_str(line).ok()?;
            q.matches_connection(&r).then(|| (r.at, serde_json::to_value(&r).unwrap_or_default()))
        }
        "waf" => {
            let r: WafRow = serde_json::from_str(line).ok()?;
            q.matches_waf(&r).then(|| (r.at, serde_json::to_value(&r).unwrap_or_default()))
        }
        _ => {
            let r: RequestRow = serde_json::from_str(line).ok()?;
            q.matches_request(&r).then(|| (r.at, serde_json::to_value(&r).unwrap_or_default()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("pyo-traffic-test-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        d
    }

    fn rows(from_ms: i64, n: i64, step_ms: i64, status: u16) -> String {
        (0..n)
            .map(|i| {
                let r = RequestRow { at: from_ms + i * step_ms, host: "a.example".into(), path: format!("/{i}"), url: format!("/{i}"), status, ..Default::default() };
                format!("{}\n", serde_json::to_string(&r).unwrap())
            })
            .collect()
    }

    #[test]
    fn newest_first_across_hours_and_pages() {
        let d = dir("query");
        let f = Files::new(&d).unwrap();
        let h0 = 1_794_000_000_000 / 3_600_000 * 3_600_000;
        // Two batches in the first hour (one 5xx), one in the next.
        f.append("requests", hour_of(h0), &rows(h0, 10, 1000, 200)).unwrap();
        f.append("requests", hour_of(h0), &rows(h0 + 20_000, 1, 1000, 503)).unwrap();
        f.append("requests", hour_of(h0 + 3_600_000), &rows(h0 + 3_600_000, 5, 1000, 200)).unwrap();
        let q = |limit: u32, before: i64, status: Option<&str>| TrafficQuery {
            rows: "requests".into(),
            limit,
            before_ms: before,
            status: status.map(str::to_owned),
            hosts: vec!["a.example".into()],
            ..Default::default()
        };
        let a = f.query(&q(3, 0, None));
        let ats: Vec<i64> = a.rows.iter().map(|r| r["at"].as_i64().unwrap()).collect();
        assert_eq!(ats, [h0 + 3_604_000, h0 + 3_603_000, h0 + 3_602_000]);
        assert!(a.more);
        // The next page from the last one's time, across the hour.
        let b = f.query(&q(4, *ats.last().unwrap(), None));
        let ats: Vec<i64> = b.rows.iter().map(|r| r["at"].as_i64().unwrap()).collect();
        assert_eq!(ats, [h0 + 3_601_000, h0 + 3_600_000, h0 + 20_000, h0 + 9_000]);
        let e = f.query(&q(50, 0, Some("5xx")));
        assert_eq!((e.rows.len(), e.more), (1, false));
        assert_eq!(e.kept_since, Some(h0));
        // A cut-off batch (a crash mid-write) costs only itself.
        let p = d.join("requests").join(format!("{}.ndjson.gz", file_name(hour_of(h0))));
        let mut bytes = fs::read(&p).unwrap();
        bytes.extend_from_slice(&[0x1f, 0x8b, 8, 0, 0]);
        fs::write(&p, bytes).unwrap();
        assert_eq!(f.query(&q(50, 0, Some("5xx"))).rows.len(), 1);
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn oldest_hours_go_first() {
        let d = dir("enforce");
        let f = Files::new(&d).unwrap();
        let now = 1_794_000_000_000i64;
        for h in 0..10 {
            f.append("requests", hour_of(now) - h * 24, &rows(now - h * 86_400_000, 200, 10, 200)).unwrap();
        }
        assert_eq!(f.list().len(), 10);
        // Older than 7 days: three files.
        assert_eq!(f.enforce(now, u64::MAX, 7), 2);
        let size = f.size();
        let each = size / 8;
        // Room for about five: the oldest go.
        f.enforce(now, each * 5 + each / 2, 7);
        let left = f.list();
        assert_eq!(left.len(), 5);
        assert_eq!(left.last().unwrap().0, hour_of(now), "the newest stays");
        assert_eq!(hour_of_name(&format!("{}.ndjson.gz", file_name(477_000))), Some(477_000));
        let _ = fs::remove_dir_all(&d);
    }
}
