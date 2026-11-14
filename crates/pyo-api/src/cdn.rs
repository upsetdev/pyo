//! The CDNs' address ranges, for firewall presets ("is in Cloudflare") and
//! services behind a CDN (pyo-proto's firewall.rs): each from its
//! provider's official list.
//!
//! - Cloudflare: https://www.cloudflare.com/ips-v4 and /ips-v6
//! - Fastly: https://api.fastly.com/public-ip-list
//! - bunny.net: its edge servers, https://api.bunny.net/system/edgeserverlist
//!   and /ipv6
//! - Amazon CloudFront: https://ip-ranges.amazonaws.com/ip-ranges.json,
//!   service `CLOUDFRONT_ORIGIN_FACING` (the addresses it reaches origins
//!   from)
//!
//! A worker fetches them each day (one at a time, by leader lock) into
//! `pyo.settings` (`cdn_ranges`). A list that doesn't come, or doesn't look
//! like one, keeps its last good copy. The image carries a copy of each
//! (`assets/cdn/ranges.json`), used until the first fetch, so an install
//! without internet access works too. When a list changes, the clusters
//! whose services use it get their firewall again.

use std::{
    sync::{OnceLock, RwLock},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, anyhow, bail};
use pyo_proto::firewall::{CdnRanges, PRESETS};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{AppState, bg};

pub const LOCK_CDN: i64 = 7_032;
/// How often the lists are fetched.
const MAX_AGE_MS: i64 = 86_400_000;
/// After a failed fetch, wait this long before trying again.
const RETRY_MS: i64 = 3_600_000;
const TICK: Duration = Duration::from_secs(600);
/// What one list may hold: a list with none, or far more than any CDN has,
/// isn't one.
const MAX_RANGES: usize = 5_000;
/// How long the API keeps the lists it read before reading them again.
const CACHE: Duration = Duration::from_secs(60);

const BUILT_IN: &str = include_str!("../assets/cdn/ranges.json");

/// The lists as stored: each CDN's ranges, and when they were fetched.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Stored {
    #[serde(default)]
    pub ranges: CdnRanges,
    /// When each list last came (ms).
    #[serde(default)]
    pub fetched_at: std::collections::BTreeMap<String, i64>,
    /// When the last round of fetches ran (ms), whatever came of it.
    #[serde(default)]
    pub tried_at: i64,
    /// When the last round failed for any list (ms).
    #[serde(default)]
    pub failed_at: i64,
}

/// The copy the image carries.
pub fn built_in() -> CdnRanges {
    #[derive(Deserialize)]
    struct Snapshot {
        ranges: CdnRanges,
    }
    serde_json::from_str::<Snapshot>(BUILT_IN)
        .map(|s| s.ranges)
        .unwrap_or_default()
}

/// Every CDN's ranges: as last fetched, else the image's copy.
pub fn with_fallback(stored: &CdnRanges) -> CdnRanges {
    let mut out = built_in();
    for (name, ranges) in stored {
        if !ranges.is_empty() {
            out.insert(name.clone(), ranges.clone());
        }
    }
    out
}

async fn stored<'e>(db: impl sqlx::PgExecutor<'e>) -> Result<Stored> {
    let v: Option<Value> =
        sqlx::query_scalar("SELECT value FROM pyo.settings WHERE key = 'cdn_ranges'")
            .fetch_optional(db)
            .await?;
    Ok(v.and_then(|v| serde_json::from_value(v).ok())
        .unwrap_or_default())
}

fn cache() -> &'static RwLock<Option<(Instant, CdnRanges)>> {
    static C: OnceLock<RwLock<Option<(Instant, CdnRanges)>>> = OnceLock::new();
    C.get_or_init(Default::default)
}

/// Every CDN's current ranges (cached for a minute).
pub async fn current<'e>(db: impl sqlx::PgExecutor<'e>) -> CdnRanges {
    if let Some((at, r)) = cache().read().ok().and_then(|g| g.clone())
        && at.elapsed() < CACHE
    {
        return r;
    }
    let r = match stored(db).await {
        Ok(s) => with_fallback(&s.ranges),
        Err(e) => {
            tracing::warn!(error = %e, "couldn't read the CDN ranges; using the built-in copy");
            built_in()
        }
    };
    if let Ok(mut g) = cache().write() {
        *g = Some((Instant::now(), r.clone()));
    }
    r
}

/// Only the CDNs these presets name.
pub fn only(all: &CdnRanges, names: &[&str]) -> CdnRanges {
    all.iter()
        .filter(|(n, _)| names.contains(&n.as_str()))
        .map(|(n, r)| (n.clone(), r.clone()))
        .collect()
}

// ── Parsing each provider's list ────────────────────────────────────────────

/// Ranges as stored ("203.0.113.0/24"; an address becomes a /32 or /128),
/// each once: none of them may be off, and there must be some.
fn ranges<'a>(items: impl IntoIterator<Item = &'a str>) -> Result<Vec<String>> {
    let mut out: Vec<String> = vec![];
    for i in items {
        let i = i.trim();
        if i.is_empty() {
            continue;
        }
        let c = crate::deploy::normalize_cidr(i).ok_or_else(|| anyhow!("{i:?} isn't a range"))?;
        // Nothing as wide as a whole family.
        if c.ends_with("/0") {
            bail!("{c} is everything");
        }
        if !out.contains(&c) {
            out.push(c);
        }
    }
    if out.is_empty() {
        bail!("the list is empty");
    }
    if out.len() > MAX_RANGES {
        bail!("the list has {} ranges", out.len());
    }
    Ok(out)
}

/// Cloudflare's: one range per line, IPv4 and IPv6 lists.
pub fn parse_cloudflare(v4: &str, v6: &str) -> Result<Vec<String>> {
    ranges(v4.lines().chain(v6.lines()))
}

/// Fastly's: `{"addresses": [...], "ipv6_addresses": [...]}`.
pub fn parse_fastly(body: &str) -> Result<Vec<String>> {
    let v: Value = serde_json::from_str(body).context("not JSON")?;
    let list = |k: &str| -> Vec<&str> {
        v[k].as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .collect()
    };
    ranges(list("addresses").into_iter().chain(list("ipv6_addresses")))
}

/// bunny.net's: a JSON array of addresses per family.
pub fn parse_bunny(v4: &str, v6: &str) -> Result<Vec<String>> {
    let a: Vec<String> = serde_json::from_str(v4).context("IPv4: not a JSON list")?;
    let b: Vec<String> = serde_json::from_str(v6).context("IPv6: not a JSON list")?;
    ranges(a.iter().chain(&b).map(String::as_str))
}

/// AWS's: the prefixes of service `CLOUDFRONT_ORIGIN_FACING`.
pub fn parse_cloudfront(body: &str) -> Result<Vec<String>> {
    let v: Value = serde_json::from_str(body).context("not JSON")?;
    let pick = |list: &str, field: &str| -> Vec<String> {
        v[list]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|p| p["service"] == "CLOUDFRONT_ORIGIN_FACING")
            .filter_map(|p| p[field].as_str().map(str::to_owned))
            .collect()
    };
    let all: Vec<String> = pick("prefixes", "ip_prefix")
        .into_iter()
        .chain(pick("ipv6_prefixes", "ipv6_prefix"))
        .collect();
    ranges(all.iter().map(String::as_str))
}

// ── Fetching ─────────────────────────────────────────────────────────────────

async fn get(http: &reqwest::Client, url: &str) -> Result<String> {
    let res = http
        .get(url)
        .header("Accept", "application/json, text/plain")
        .send()
        .await
        .with_context(|| format!("can't reach {url}"))?;
    if !res.status().is_success() {
        bail!("{url} answered {}", res.status());
    }
    Ok(res.text().await?)
}

async fn fetch(http: &reqwest::Client, cdn: &str) -> Result<Vec<String>> {
    match cdn {
        "cloudflare" => parse_cloudflare(
            &get(http, "https://www.cloudflare.com/ips-v4").await?,
            &get(http, "https://www.cloudflare.com/ips-v6").await?,
        ),
        "fastly" => parse_fastly(&get(http, "https://api.fastly.com/public-ip-list").await?),
        "bunny" => parse_bunny(
            &get(http, "https://api.bunny.net/system/edgeserverlist").await?,
            &get(http, "https://api.bunny.net/system/edgeserverlist/ipv6").await?,
        ),
        "cloudfront" => {
            parse_cloudfront(&get(http, "https://ip-ranges.amazonaws.com/ip-ranges.json").await?)
        }
        _ => bail!("no list for {cdn}"),
    }
}

/// One round: each list fetched on its own, a failed one kept as it was.
/// The CDNs whose ranges changed come back.
pub async fn refresh_with<F, Fut>(before: &Stored, fetch_one: F, now: i64) -> (Stored, Vec<String>)
where
    F: Fn(&'static str) -> Fut,
    Fut: std::future::Future<Output = Result<Vec<String>>>,
{
    let mut next = before.clone();
    next.tried_at = now;
    let mut changed = vec![];
    let mut failed = false;
    for p in PRESETS.iter().filter(|p| p.cdn) {
        match fetch_one(p.name).await {
            Ok(r) => {
                let was = with_fallback(&before.ranges).get(p.name).cloned();
                if was.as_ref() != Some(&r) {
                    changed.push(p.name.to_owned());
                }
                next.ranges.insert(p.name.to_owned(), r);
                next.fetched_at.insert(p.name.to_owned(), now);
            }
            Err(e) => {
                failed = true;
                tracing::warn!(cdn = p.name, error = %format!("{e:#}"), "couldn't fetch the CDN's ranges; keeping the last good copy");
            }
        }
    }
    if failed {
        next.failed_at = now;
    }
    (next, changed)
}

/// Fetch every list, store what came, and send the firewall again to the
/// clusters whose services use a list that changed.
async fn refresh(s: &AppState) -> Result<()> {
    let http = reqwest::Client::builder()
        .user_agent(concat!("pyo/", env!("CARGO_PKG_VERSION")))
        .timeout(Duration::from_secs(60))
        .build()?;
    let before = stored(&s.db).await?;
    let (next, changed) = refresh_with(&before, |cdn| fetch(&http, cdn), pyo_proto::now_ms()).await;
    sqlx::query(
        "INSERT INTO pyo.settings (key, value, updated_at) VALUES ('cdn_ranges', $1, now())
         ON CONFLICT (key) DO UPDATE SET value = excluded.value, updated_at = now()",
    )
    .bind(serde_json::to_value(&next)?)
    .execute(&s.db)
    .await?;
    if let Ok(mut g) = cache().write() {
        *g = None;
    }
    if changed.is_empty() {
        return Ok(());
    }
    tracing::info!(cdns = ?changed, "CDN ranges changed");
    let clusters: Vec<String> = sqlx::query_scalar(
        "SELECT DISTINCT s.cluster_id FROM pyo.services s
          WHERE NOT s.deleting AND (s.behind_cdn = ANY($1)
             OR EXISTS (SELECT 1 FROM pyo.firewall_rules r WHERE r.service_id = s.id AND r.sources && $1))",
    )
    .bind(&changed)
    .fetch_all(&s.db)
    .await?;
    for c in clusters {
        let mut tx = s.db.begin().await?;
        crate::firewall::sync(&mut tx, &c)
            .await
            .map_err(|e| anyhow!("{e:?}"))?;
        tx.commit().await?;
        bg::wake(&s.db, &c).await;
    }
    Ok(())
}

/// Workers: fetch the lists once a day (an hour after a failure).
pub async fn keeper(s: AppState) {
    loop {
        let due = match stored(&s.db).await {
            Ok(st) => {
                let now = pyo_proto::now_ms();
                let last_ok = st.fetched_at.values().copied().min().unwrap_or(0);
                let missing = PRESETS
                    .iter()
                    .filter(|p| p.cdn)
                    .any(|p| !st.fetched_at.contains_key(p.name));
                (missing || now - last_ok > MAX_AGE_MS) && now - st.tried_at > RETRY_MS
            }
            Err(_) => false,
        };
        if due && let Some(Err(e)) = bg::as_leader(&s.db, LOCK_CDN, || refresh(&s)).await {
            tracing::warn!(error = %format!("{e:#}"), "couldn't refresh the CDN ranges");
        }
        tokio::time::sleep(TICK).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_built_in_copy_has_every_cdn() {
        let b = built_in();
        for p in PRESETS.iter().filter(|p| p.cdn) {
            let r = b.get(p.name).unwrap_or_else(|| panic!("{}", p.name));
            assert!(!r.is_empty(), "{}", p.name);
            // Stored as the API stores ranges.
            assert_eq!(
                &ranges(r.iter().map(String::as_str)).unwrap(),
                r,
                "{}",
                p.name
            );
        }
        assert!(b["cloudflare"].contains(&"104.16.0.0/13".to_owned()));
        // Cloudflare's fit what a service's HTTPS may trust.
        assert!(b["cloudflare"].len() <= pyo_proto::system::MAX_TRUSTED_CIDRS);
    }

    #[test]
    fn providers_lists() {
        assert_eq!(
            parse_cloudflare("173.245.48.0/20\n103.21.244.0/22", "2400:cb00::/32\n").unwrap(),
            ["173.245.48.0/20", "103.21.244.0/22", "2400:cb00::/32"]
        );
        assert_eq!(
            parse_fastly(r#"{"addresses":["23.235.32.0/20"],"ipv6_addresses":["2a04:4e40::/32"]}"#)
                .unwrap(),
            ["23.235.32.0/20", "2a04:4e40::/32"]
        );
        assert_eq!(
            parse_bunny(
                r#"["89.187.188.227","89.187.188.227"]"#,
                r#"["2400:52e0:1500::714:1"]"#
            )
            .unwrap(),
            ["89.187.188.227/32", "2400:52e0:1500::714:1/128"]
        );
        let aws = r#"{"prefixes":[{"ip_prefix":"3.4.12.4/32","service":"AMAZON"},{"ip_prefix":"13.124.199.0/24","service":"CLOUDFRONT_ORIGIN_FACING"}],
                      "ipv6_prefixes":[{"ipv6_prefix":"2600:9000::/28","service":"CLOUDFRONT"}]}"#;
        assert_eq!(parse_cloudfront(aws).unwrap(), ["13.124.199.0/24"]);
        // Not a list: an error page, nothing, everything, junk.
        assert!(parse_cloudflare("<html>Too many requests</html>", "").is_err());
        assert!(parse_cloudflare("", "").is_err());
        assert!(parse_cloudflare("0.0.0.0/0", "").is_err());
        assert!(parse_fastly("{}").is_err());
        assert!(parse_bunny("[]", "[]").is_err());
        assert!(parse_cloudfront(r#"{"prefixes":[]}"#).is_err());
    }

    /// A round where one list doesn't come: it keeps its last good copy,
    /// the others are replaced, and only what changed is named.
    #[tokio::test]
    async fn refresh_keeps_the_last_good_copy() {
        let mut before = Stored::default();
        before
            .ranges
            .insert("fastly".into(), vec!["151.101.0.0/16".into()]);
        before.fetched_at.insert("fastly".into(), 1);
        let cf = built_in()["cloudflare"].clone();
        let (next, changed) = refresh_with(
            &before,
            |cdn| {
                let cf = cf.clone();
                async move {
                    match cdn {
                        "fastly" => Err(anyhow!("timed out")),
                        // As before (the built-in copy): not a change.
                        "cloudflare" => Ok(cf),
                        "bunny" => Ok(vec!["89.187.188.227/32".into()]),
                        _ => Ok(vec!["13.124.199.0/24".into()]),
                    }
                }
            },
            1000,
        )
        .await;
        assert_eq!(next.ranges["fastly"], ["151.101.0.0/16"]);
        assert_eq!(next.fetched_at["fastly"], 1);
        assert_eq!(next.ranges["bunny"], ["89.187.188.227/32"]);
        assert_eq!(next.fetched_at["bunny"], 1000);
        assert_eq!((next.tried_at, next.failed_at), (1000, 1000));
        assert_eq!(changed, ["bunny", "cloudfront"]);
        // Every list missing: the built-in copy.
        let all = with_fallback(&Default::default());
        assert_eq!(all, built_in());
        let mixed = with_fallback(&next.ranges);
        assert_eq!(mixed["bunny"], ["89.187.188.227/32"]);
        assert_eq!(mixed["cloudflare"], built_in()["cloudflare"]);
        // Only what a firewall names goes with it.
        assert_eq!(only(&mixed, &["bunny", "tailnet"]).len(), 1);
    }
}
