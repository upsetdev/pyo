//! How long observability keeps what it stores.
//!
//! The installation's admin picks, in Settings › Observability:
//! analytics totals (the hourly traffic and port tables, and the WAF's rule
//! messages) 3, 6, 12 or 24 months; server, service and database metrics 7
//! to 90 days; build and deploy logs 30 to 365 days. Saved in pyo.settings
//! 'observability' as `retention`. ClickHouse keeps each table by its own
//! TTL, so a change is an `ALTER TABLE … MODIFY TTL` (`apply`), made again
//! whenever the tables are set up, so tables made with another TTL follow
//! the setting. Build logs live in Postgres: jobs.rs prunes them by it.
//!
//! A team may keep its analytics shorter (Team settings, `analyticsMonths`:
//! 3, 6 or 12 months, never more than the installation). That is on Pyo
//! Cloud too, where teams share the tables. ClickHouse's TTL is per table,
//! so a team's shorter time is a cleanup once a day (`team_cleanup`): one
//! `ALTER TABLE … DELETE` per table for every such team at once. A per-row
//! TTL (a retention column on each row) was the other way: it would only
//! reach rows stored after a change, and every row would carry the column
//! through the hourly views. The cleanup applies a change to what's
//! already there.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicI64, Ordering};

use serde_json::{Value, json};
use sqlx::PgPool;

use crate::error::{ApiError, ApiResult};
use crate::observability::ClickHouse;

/// What the installation may keep analytics totals for, in months.
pub const ANALYTICS_MONTHS: [u8; 4] = [3, 6, 12, 24];
/// What a team may pick, in months (never more than the installation).
pub const TEAM_ANALYTICS_MONTHS: [u8; 3] = [3, 6, 12];
/// Metrics, in days (inclusive).
pub const METRICS_DAYS: (u16, u16) = (7, 90);
/// Build and deploy logs, in days (inclusive).
pub const BUILD_LOG_DAYS: (u16, u16) = (30, 365);

/// The installation's retention ('retention' in pyo.settings
/// 'observability').
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct Retention {
    pub analytics_months: u8,
    pub metrics_days: u16,
    pub build_log_days: u16,
}

impl Default for Retention {
    fn default() -> Self {
        Retention { analytics_months: 12, metrics_days: 30, build_log_days: 90 }
    }
}

impl Retention {
    /// Why it can't be saved, if it can't.
    pub fn check(&self) -> Result<(), &'static str> {
        if !ANALYTICS_MONTHS.contains(&self.analytics_months) {
            return Err("Keep analytics 3, 6, 12 or 24 months.");
        }
        if !(METRICS_DAYS.0..=METRICS_DAYS.1).contains(&self.metrics_days) {
            return Err("Keep metrics 7 to 90 days.");
        }
        if !(BUILD_LOG_DAYS.0..=BUILD_LOG_DAYS.1).contains(&self.build_log_days) {
            return Err("Keep build logs 30 to 365 days.");
        }
        Ok(())
    }

    /// From the saved 'observability' setting: each figure as saved, or its
    /// default when it's missing or out of range.
    pub fn from_setting(v: &Value) -> Self {
        let r = &v["retention"];
        let d = Retention::default();
        let n = |k: &str| r[k].as_u64();
        Retention {
            analytics_months: n("analyticsMonths")
                .and_then(|m| u8::try_from(m).ok())
                .filter(|m| ANALYTICS_MONTHS.contains(m))
                .unwrap_or(d.analytics_months),
            metrics_days: n("metricsDays")
                .and_then(|m| u16::try_from(m).ok())
                .filter(|m| (METRICS_DAYS.0..=METRICS_DAYS.1).contains(m))
                .unwrap_or(d.metrics_days),
            build_log_days: n("buildLogDays")
                .and_then(|m| u16::try_from(m).ok())
                .filter(|m| (BUILD_LOG_DAYS.0..=BUILD_LOG_DAYS.1).contains(m))
                .unwrap_or(d.build_log_days),
        }
    }

    /// Each ClickHouse table's TTL for this retention, as ClickHouse
    /// prints it back (`engine_full`), so `apply` can tell what changed.
    pub fn ttls(&self) -> Vec<(&'static str, String)> {
        let months = |col: &str| format!("{col} + toIntervalMonth({})", self.analytics_months);
        let days = format!("ts + toIntervalDay({})", self.metrics_days);
        vec![
            ("traffic_1h", months("ts")),
            ("ports_1h", months("ts")),
            ("waf_rules", months("seen")),
            ("metrics", days.clone()),
            ("node_metrics", days.clone()),
            ("database_metrics", days),
        ]
    }
}

/// The installation's retention, as saved (defaults when it isn't).
pub async fn load(db: &PgPool) -> ApiResult<Retention> {
    let v: Option<Value> = sqlx::query_scalar("SELECT value FROM pyo.settings WHERE key = 'observability'")
        .fetch_optional(db)
        .await?;
    Ok(v.map(|v| Retention::from_setting(&v)).unwrap_or_default())
}

/// Save the installation's retention (its admin). Returns whether it
/// changed.
pub async fn save(tx: &mut sqlx::Transaction<'_, sqlx::Postgres>, r: &Retention) -> ApiResult<bool> {
    r.check().map_err(ApiError::bad_request)?;
    let v: Value = sqlx::query_scalar("SELECT value FROM pyo.settings WHERE key = 'observability'")
        .fetch_optional(&mut **tx)
        .await?
        .unwrap_or_else(|| json!({}));
    let changed = Retention::from_setting(&v) != *r;
    sqlx::query(
        "INSERT INTO pyo.settings (key, value) VALUES ('observability', jsonb_build_object('retention', $1::jsonb))
         ON CONFLICT (key) DO UPDATE SET value = pyo.settings.value || excluded.value, updated_at = now()",
    )
    .bind(json!(r))
    .execute(&mut **tx)
    .await?;
    Ok(changed)
}

/// Why a team's analytics months can't be saved, if they can't. 0 means
/// "as long as the installation".
pub fn check_team_months(months: u8, installation: &Retention) -> Result<(), String> {
    if months == 0 {
        return Ok(());
    }
    if !TEAM_ANALYTICS_MONTHS.contains(&months) {
        return Err("Keep analytics 3, 6 or 12 months.".into());
    }
    if months > installation.analytics_months {
        return Err(format!(
            "This installation keeps analytics {} months at most.",
            installation.analytics_months
        ));
    }
    Ok(())
}

/// The TTL in a table's `engine_full`, or None when it has none.
fn ttl_of(engine_full: &str) -> Option<&str> {
    let start = engine_full.find(" TTL ")? + 5;
    let rest = &engine_full[start..];
    Some(rest.find(" SETTINGS ").map_or(rest, |end| &rest[..end]).trim())
}

/// The `ALTER TABLE … MODIFY TTL` statements that bring these tables
/// (name, engine_full) to the retention. Tables not made yet are left out:
/// they're made with the default and follow at the next `apply`.
pub fn ttl_changes(r: &Retention, tables: &[(String, String)]) -> Vec<(&'static str, String)> {
    r.ttls()
        .into_iter()
        .filter_map(|(table, ttl)| {
            let (_, engine) = tables.iter().find(|(n, _)| n == table)?;
            (ttl_of(engine) != Some(ttl.as_str()))
                .then(|| (table, format!("ALTER TABLE pyo_observability.{table} MODIFY TTL {ttl}")))
        })
        .collect()
}

/// Bring the tables' TTLs to the retention. Returns the tables changed.
/// ClickHouse applies a new TTL to stored rows in the background.
pub async fn apply(ch: &ClickHouse, r: &Retention) -> anyhow::Result<Vec<String>> {
    let text = ch
        .exec(
            "SELECT name, engine_full FROM system.tables WHERE database = 'pyo_observability' FORMAT JSONEachRow",
            None,
            &[],
        )
        .await?;
    let tables: Vec<(String, String)> = text
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter_map(|v| Some((v["name"].as_str()?.to_owned(), v["engine_full"].as_str()?.to_owned())))
        .collect();
    let mut changed = vec![];
    for (table, sql) in ttl_changes(r, &tables) {
        ch.exec(&sql, None, &[]).await?;
        changed.push(table.to_owned());
    }
    if !changed.is_empty() {
        tracing::info!(tables = ?changed, "observability retention applied");
    }
    Ok(changed)
}

// ── Teams that keep analytics shorter ───────────────────────────────────────

/// The analytics tables and their time column.
const ANALYTICS_TABLES: [(&str, &str); 3] = [("traffic_1h", "ts"), ("ports_1h", "ts"), ("waf_rules", "seen")];

/// Team ids per statement (they're inlined in the SQL body).
const TEAMS_PER_STATEMENT: usize = 1000;

fn quote(s: &str) -> String {
    format!("'{}'", s.replace('\\', "\\\\").replace('\'', "\\'"))
}

/// The rows to remove from a table with time column `col`: each team's
/// older than its months before `now` (seconds). `teams`: months -> ids.
/// One condition per statement of at most `TEAMS_PER_STATEMENT` teams.
pub fn cleanup_conditions(teams: &BTreeMap<u8, Vec<String>>, col: &str, now: i64) -> Vec<String> {
    let mut all: Vec<(u8, &String)> = teams.iter().flat_map(|(m, ids)| ids.iter().map(move |id| (*m, id))).collect();
    all.sort();
    all.chunks(TEAMS_PER_STATEMENT)
        .map(|chunk| {
            let mut by_months: BTreeMap<u8, Vec<String>> = BTreeMap::new();
            for (m, id) in chunk {
                by_months.entry(*m).or_default().push(quote(id));
            }
            by_months
                .into_iter()
                .map(|(m, ids)| {
                    format!(
                        "(team_id IN ({}) AND {col} < subtractMonths(toDateTime({now}, 'UTC'), {m}))",
                        ids.join(", ")
                    )
                })
                .collect::<Vec<_>>()
                .join(" OR ")
        })
        .collect()
}

/// The teams keeping analytics shorter than the installation: months ->
/// team ids.
pub async fn shorter_teams(db: &PgPool, installation: &Retention) -> sqlx::Result<BTreeMap<u8, Vec<String>>> {
    let rows: Vec<(String, i16)> = sqlx::query_as(
        "SELECT id, analytics_months FROM pyo.teams WHERE analytics_months IS NOT NULL AND analytics_months < $1 ORDER BY id",
    )
    .bind(i16::from(installation.analytics_months))
    .fetch_all(db)
    .await?;
    let mut out: BTreeMap<u8, Vec<String>> = BTreeMap::new();
    for (id, m) in rows {
        out.entry(m as u8).or_default().push(id);
    }
    Ok(out)
}

/// Remove what teams keeping analytics shorter don't keep any more. A
/// table with a cleanup still running is left for the next time. Returns
/// the statements sent.
pub async fn team_cleanup(db: &PgPool, ch: &ClickHouse, now: i64) -> anyhow::Result<usize> {
    let installation = load(db).await.map_err(|e| anyhow::anyhow!("{}", e.message))?;
    let teams = shorter_teams(db, &installation).await?;
    if teams.is_empty() {
        return Ok(0);
    }
    let mut sent = 0;
    for (table, col) in ANALYTICS_TABLES {
        let running = ch
            .exec(
                &format!(
                    "SELECT count() FROM system.mutations WHERE database = 'pyo_observability' AND table = '{table}' AND NOT is_done"
                ),
                None,
                &[],
            )
            .await?;
        if running.trim() != "0" {
            continue;
        }
        for cond in cleanup_conditions(&teams, col, now) {
            let n = ch.exec(&format!("SELECT count() FROM pyo_observability.{table} WHERE {cond}"), None, &[]).await?;
            if n.trim() == "0" {
                continue;
            }
            ch.exec(&format!("ALTER TABLE pyo_observability.{table} DELETE WHERE {cond}"), None, &[]).await?;
            sent += 1;
        }
    }
    if sent > 0 {
        tracing::info!(statements = sent, "removed analytics teams keep shorter");
    }
    Ok(sent)
}

/// When `nightly` last ran (seconds), in this process.
static LAST_CLEANUP: AtomicI64 = AtomicI64::new(0);

/// Once a day, from the sweeper (one process at a time): the teams'
/// shorter analytics. Waits while observability is off or starting.
pub async fn nightly(s: &crate::AppState) {
    let now = pyo_proto::now_ms() / 1000;
    if now - LAST_CLEANUP.load(Ordering::Relaxed) < 86_400 {
        return;
    }
    let Some(ch) = s.observability.store().await else { return };
    if !s.observability.is_ready().await {
        return;
    }
    LAST_CLEANUP.store(now, Ordering::Relaxed);
    if let Err(e) = team_cleanup(&s.db, ch, now).await {
        tracing::warn!(error = %format!("{e:#}"), "couldn't remove analytics teams keep shorter; trying again in an hour");
        LAST_CLEANUP.store(now - 86_400 + 3600, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_are_checked() {
        assert_eq!(Retention::default().check(), Ok(()));
        let r = |a, m, b| Retention { analytics_months: a, metrics_days: m, build_log_days: b };
        assert!(r(24, 7, 365).check().is_ok());
        assert!(r(3, 90, 30).check().is_ok());
        assert!(r(13, 30, 90).check().is_err(), "13 months isn't a choice");
        assert!(r(1, 30, 90).check().is_err());
        assert!(r(12, 6, 90).check().is_err());
        assert!(r(12, 91, 90).check().is_err());
        assert!(r(12, 30, 29).check().is_err());
        assert!(r(12, 30, 366).check().is_err());
    }

    #[test]
    fn saved_settings_fall_back_per_figure() {
        assert_eq!(Retention::from_setting(&json!({ "enabled": true })), Retention::default());
        let v = json!({ "retention": { "analyticsMonths": 24, "metricsDays": 500, "buildLogDays": 30 } });
        assert_eq!(
            Retention::from_setting(&v),
            Retention { analytics_months: 24, metrics_days: 30, build_log_days: 30 }
        );
        let v = json!({ "retention": { "analyticsMonths": 13, "metricsDays": 7, "buildLogDays": "x" } });
        assert_eq!(
            Retention::from_setting(&v),
            Retention { analytics_months: 12, metrics_days: 7, build_log_days: 90 }
        );
        // What's saved reads back.
        let r = Retention { analytics_months: 6, metrics_days: 14, build_log_days: 180 };
        assert_eq!(Retention::from_setting(&json!({ "retention": r })), r);
    }

    #[test]
    fn teams_pick_shorter_only() {
        let at = |m| Retention { analytics_months: m, ..Default::default() };
        assert!(check_team_months(0, &at(3)).is_ok(), "as long as the installation");
        assert!(check_team_months(3, &at(12)).is_ok());
        assert!(check_team_months(12, &at(12)).is_ok());
        assert!(check_team_months(12, &at(24)).is_ok());
        assert!(check_team_months(24, &at(24)).is_err(), "teams pick 3, 6 or 12");
        assert!(check_team_months(9, &at(12)).is_err());
        assert_eq!(
            check_team_months(12, &at(6)).unwrap_err(),
            "This installation keeps analytics 6 months at most."
        );
    }

    #[test]
    fn ttls_change_only_what_differs() {
        let r = Retention { analytics_months: 24, metrics_days: 14, build_log_days: 90 };
        let tables = vec![
            // Made before with the old 13 months.
            (
                "traffic_1h".to_owned(),
                "AggregatingMergeTree PARTITION BY toYYYYMM(ts) ORDER BY (team_id, host, ts, service_id) TTL ts + toIntervalMonth(13) SETTINGS non_replicated_deduplication_window = 1000, index_granularity = 8192".to_owned(),
            ),
            (
                "ports_1h".to_owned(),
                "AggregatingMergeTree PARTITION BY toYYYYMM(ts) ORDER BY (team_id, service_id, port, protocol, ts) TTL ts + toIntervalMonth(24) SETTINGS index_granularity = 8192".to_owned(),
            ),
            (
                "waf_rules".to_owned(),
                "ReplacingMergeTree(seen) ORDER BY (team_id, rule) TTL seen + toIntervalMonth(12) SETTINGS index_granularity = 8192".to_owned(),
            ),
            (
                "metrics".to_owned(),
                "MergeTree PARTITION BY toYYYYMM(ts) ORDER BY (team_id, kind, id, ts) TTL ts + toIntervalDay(30) SETTINGS index_granularity = 8192".to_owned(),
            ),
            ("node_metrics".to_owned(), "MergeTree ORDER BY ts TTL ts + toIntervalDay(14)".to_owned()),
            // traffic_1m (3 days) isn't the setting's; database_metrics isn't made yet.
            ("traffic_1m".to_owned(), "AggregatingMergeTree TTL ts + toIntervalDay(3)".to_owned()),
        ];
        let sql: Vec<String> = ttl_changes(&r, &tables).into_iter().map(|(_, s)| s).collect();
        assert_eq!(
            sql,
            [
                "ALTER TABLE pyo_observability.traffic_1h MODIFY TTL ts + toIntervalMonth(24)",
                "ALTER TABLE pyo_observability.waf_rules MODIFY TTL seen + toIntervalMonth(24)",
                "ALTER TABLE pyo_observability.metrics MODIFY TTL ts + toIntervalDay(14)",
            ]
        );
        assert_eq!(ttl_of("MergeTree ORDER BY ts"), None);
    }

    #[test]
    fn cleanup_keeps_each_team_its_months() {
        let mut teams = BTreeMap::new();
        teams.insert(3u8, vec!["team_a".to_owned()]);
        teams.insert(6u8, vec!["team_b".to_owned(), "team_c".to_owned()]);
        let c = cleanup_conditions(&teams, "ts", 1_800_000_000);
        assert_eq!(
            c,
            ["(team_id IN ('team_a') AND ts < subtractMonths(toDateTime(1800000000, 'UTC'), 3)) OR \
              (team_id IN ('team_b', 'team_c') AND ts < subtractMonths(toDateTime(1800000000, 'UTC'), 6))"]
        );
        // Quotes can't end the string.
        assert_eq!(quote("a'b\\"), "'a\\'b\\\\'");
        // Many teams: a statement per thousand.
        let many: BTreeMap<u8, Vec<String>> = [(3u8, (0..2500).map(|i| format!("team_{i:04}")).collect())].into();
        let c = cleanup_conditions(&many, "seen", 0);
        assert_eq!(c.len(), 3);
        assert!(c.iter().all(|s| s.starts_with("(team_id IN (") && s.contains("seen < ")));
    }

    fn test_clickhouse() -> Option<ClickHouse> {
        let url = std::env::var("PYO_TEST_CLICKHOUSE_URL").ok()?;
        let password = std::env::var("PYO_TEST_CLICKHOUSE_PASSWORD").unwrap_or_default();
        Some(ClickHouse::new(url, "default".into(), password))
    }

    /// Tables as `ensure_schema` makes them, `traffic_1h` as it was made
    /// before (13 months).
    async fn fresh_tables(ch: &ClickHouse) {
        ch.exec("DROP DATABASE IF EXISTS pyo_observability", None, &[]).await.unwrap();
        ch.exec("CREATE DATABASE pyo_observability", None, &[]).await.unwrap();
        for sql in crate::totals::schema() {
            let sql = if sql.contains("pyo_observability.traffic_1h (") {
                sql.replace("ts + INTERVAL 12 MONTH", "ts + INTERVAL 13 MONTH")
            } else {
                sql
            };
            ch.exec(&sql, None, &[]).await.unwrap();
        }
        for sql in crate::observability::schema_for_tests() {
            ch.exec(sql, None, &[]).await.unwrap();
        }
    }

    async fn wait_for_mutations(ch: &ClickHouse) {
        for _ in 0..100 {
            let n = ch
                .exec("SELECT count() FROM system.mutations WHERE database = 'pyo_observability' AND NOT is_done", None, &[])
                .await
                .unwrap();
            if n.trim() == "0" {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        panic!("mutations still running");
    }

    /// Against a throwaway ClickHouse (`PYO_TEST_CLICKHOUSE_URL`): every
    /// table takes the setting's TTL, a table made with the old 13 months
    /// included, and once they have it nothing more is changed.
    #[tokio::test]
    #[ignore = "needs a throwaway ClickHouse"]
    async fn tables_follow_the_setting() {
        let Some(ch) = test_clickhouse() else { return };
        fresh_tables(&ch).await;
        // The defaults: only the table made with 13 months changes.
        assert_eq!(apply(&ch, &Retention::default()).await.unwrap(), ["traffic_1h"]);
        assert!(apply(&ch, &Retention::default()).await.unwrap().is_empty());
        let r = Retention { analytics_months: 24, metrics_days: 7, build_log_days: 365 };
        let mut changed = apply(&ch, &r).await.unwrap();
        changed.sort();
        assert_eq!(changed, ["database_metrics", "metrics", "node_metrics", "ports_1h", "traffic_1h", "waf_rules"]);
        assert!(apply(&ch, &r).await.unwrap().is_empty(), "nothing left to change");
        let ttl = |t: &'static str| {
            let ch = &ch;
            async move {
                let e = ch
                    .exec(&format!("SELECT engine_full FROM system.tables WHERE database = 'pyo_observability' AND name = '{t}'"), None, &[])
                    .await
                    .unwrap();
                ttl_of(e.trim()).unwrap_or_default().to_owned()
            }
        };
        assert_eq!(ttl("traffic_1h").await, "ts + toIntervalMonth(24)");
        assert_eq!(ttl("waf_rules").await, "seen + toIntervalMonth(24)");
        assert_eq!(ttl("node_metrics").await, "ts + toIntervalDay(7)");
        assert_eq!(ttl("traffic_1m").await, "ts + toIntervalDay(3)", "minutes stay 3 days");
        // Shorter: stored rows past it go.
        apply(&ch, &Retention { metrics_days: 30, ..r }).await.unwrap();
        let now = pyo_proto::now_ms() / 1000;
        let rows = [10, 2]
            .iter()
            .map(|d| json!({ "ts": now - d * 86400, "team_id": "team_x", "cluster_id": "c", "kind": "server", "id": "s", "name": "s" }).to_string())
            .collect::<Vec<_>>()
            .join("\n");
        ch.insert("metrics", rows, "m").await.unwrap();
        assert_eq!(apply(&ch, &Retention { metrics_days: 7, ..r }).await.unwrap(), ["metrics", "node_metrics", "database_metrics"]);
        wait_for_mutations(&ch).await;
        let left = ch.exec("SELECT count() FROM pyo_observability.metrics", None, &[]).await.unwrap();
        assert_eq!(left.trim(), "1", "the 10-day-old sample is gone");
        ch.exec("DROP DATABASE pyo_observability", None, &[]).await.unwrap();
    }

    /// Against a throwaway Postgres (DATABASE_URL) and ClickHouse: a team
    /// keeping 3 months loses what's older, one keeping 6 keeps 4 months
    /// back, and one following the installation keeps everything.
    #[tokio::test]
    #[ignore = "needs DATABASE_URL and PYO_TEST_CLICKHOUSE_URL (throwaway ones)"]
    async fn teams_keep_their_own_months() {
        let (Ok(db_url), Some(ch)) = (std::env::var("DATABASE_URL"), test_clickhouse()) else { return };
        let pool = PgPool::connect(&db_url).await.unwrap();
        crate::migrate::run(&pool).await.unwrap();
        sqlx::query("DELETE FROM pyo.settings WHERE key = 'observability'").execute(&pool).await.unwrap();
        fresh_tables(&ch).await;
        let run = crate::ids::new_id("t");
        let (three, six, all) = (format!("team_3{run}"), format!("team_6{run}"), format!("team_a{run}"));
        for (team, months) in [(&three, Some(3i16)), (&six, Some(6)), (&all, None)] {
            sqlx::query("INSERT INTO pyo.teams (id, name, slug, analytics_months) VALUES ($1, $1, $1, $2)")
                .bind(team)
                .bind(months)
                .execute(&pool)
                .await
                .unwrap();
        }
        // The column takes 3, 6 or 12 only.
        assert!(sqlx::query("UPDATE pyo.teams SET analytics_months = 24 WHERE id = $1").bind(&three).execute(&pool).await.is_err());
        let now = pyo_proto::now_ms() / 1000 / 3600 * 3600;
        let day = 86_400;
        let mut traffic = String::new();
        let mut ports = String::new();
        let mut waf = String::new();
        for team in [&three, &six, &all] {
            for days_ago in [300, 200, 120, 30, 1] {
                let ts = now - days_ago * day;
                traffic.push_str(&json!({ "ts": ts, "team_id": team, "service_id": "s", "host": "h", "requests": 1 }).to_string());
                traffic.push('\n');
                ports.push_str(&json!({ "ts": ts, "team_id": team, "service_id": "s", "port": 22, "protocol": "TCP", "connections": 1 }).to_string());
                ports.push('\n');
                waf.push_str(&json!({ "team_id": team, "rule": days_ago.to_string(), "message": "m", "seen": ts }).to_string());
                waf.push('\n');
            }
        }
        ch.insert("traffic_1h", traffic, "t").await.unwrap();
        ch.insert("ports_1h", ports, "p").await.unwrap();
        ch.insert("waf_rules", waf, "w").await.unwrap();

        // 3 tables, one statement each.
        assert_eq!(team_cleanup(&pool, &ch, now).await.unwrap(), 3);
        wait_for_mutations(&ch).await;
        let days_left = |table: &'static str, col: &'static str, team: String| {
            let ch = &ch;
            async move {
                let text = ch
                    .exec(
                        &format!("SELECT intDiv({now} - toUnixTimestamp({col}), 86400) AS d FROM pyo_observability.{table} WHERE team_id = {{t:String}} ORDER BY d DESC"),
                        None,
                        &[("t", team)],
                    )
                    .await
                    .unwrap();
                text.lines().map(|l| l.trim().parse::<i64>().unwrap()).collect::<Vec<_>>()
            }
        };
        for (table, col) in ANALYTICS_TABLES {
            assert_eq!(days_left(table, col, three.clone()).await, [30, 1], "{table}: 3 months");
            assert_eq!(days_left(table, col, six.clone()).await, [120, 30, 1], "{table}: 6 months");
            assert_eq!(days_left(table, col, all.clone()).await, [300, 200, 120, 30, 1], "{table}: the installation's");
        }
        // Nothing left to remove: nothing sent.
        assert_eq!(team_cleanup(&pool, &ch, now).await.unwrap(), 0);

        // The installation keeping 6 months: the 6-month team follows the
        // table's TTL, the 3-month team still gets its cleanup.
        let mut tx = pool.begin().await.unwrap();
        save(&mut tx, &Retention { analytics_months: 6, ..Default::default() }).await.unwrap();
        tx.commit().await.unwrap();
        let shorter = shorter_teams(&pool, &load(&pool).await.unwrap()).await.unwrap();
        assert!(shorter.get(&3).is_some_and(|t| t.contains(&three)));
        assert!(!shorter.values().flatten().any(|t| t == &six || t == &all));

        sqlx::query("DELETE FROM pyo.settings WHERE key = 'observability'").execute(&pool).await.unwrap();
        for team in [&three, &six, &all] {
            sqlx::query("DELETE FROM pyo.teams WHERE id = $1").bind(team).execute(&pool).await.unwrap();
        }
        ch.exec("DROP DATABASE pyo_observability", None, &[]).await.unwrap();
    }
}
