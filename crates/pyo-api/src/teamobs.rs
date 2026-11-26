//! Team observability (Team › Settings › Observability): whether a team's
//! servers collect analytics, request logs and usage history, whether they
//! keep its request logs, and what they keep of visitors' addresses and of
//! query strings.
//!
//! The installation sets a default and a limit for the address and query
//! string settings (Settings › Observability, in pyo.settings
//! 'observability': `ipMode` and `maxIpMode`, `queryStrings` and
//! `maxQueryStrings`). A team's own choice is a column of pyo.teams, null
//! while it follows the default. What applies is the team's choice (or the
//! default), never more than the limit. It's worked out each time it's
//! used: a lower limit hides more at once, and raising it again gives teams
//! their choice back for new data.
//!
//! Where it applies: the cluster's traffic store keeps rows as the team's
//! values say (totals.rs `context`, pyo-agent's traffic/clean.rs), and Pyo
//! applies them again when it reads rows (requests.rs) and stores totals,
//! for clusters whose agent doesn't know them yet. A team with observability
//! off gets no Vector or traffic store on its clusters (agent.rs), and Pyo
//! stores none of its analytics or usage history meanwhile.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::error::{ApiError, ApiResult};
use crate::observability::IpMode;

/// What a team's request logs and analytics keep of query strings. The
/// values of sensitive parameters (passwords, tokens) are never kept.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum QueryStrings {
    /// Names and values (the default).
    #[default]
    Keep,
    /// Parameter names only.
    Names,
    /// Nothing after the path.
    None,
}

impl QueryStrings {
    pub fn as_str(self) -> &'static str {
        match self {
            QueryStrings::Keep => "keep",
            QueryStrings::Names => "names",
            QueryStrings::None => "none",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "keep" => Some(QueryStrings::Keep),
            "names" => Some(QueryStrings::Names),
            "none" => Some(QueryStrings::None),
            _ => None,
        }
    }

    /// How much is kept: none < names < keep.
    fn rank(self) -> u8 {
        match self {
            QueryStrings::None => 0,
            QueryStrings::Names => 1,
            QueryStrings::Keep => 2,
        }
    }

    /// The stricter of the two.
    pub fn min(self, other: Self) -> Self {
        if other.rank() < self.rank() { other } else { self }
    }
}

pub fn ip_str(m: IpMode) -> &'static str {
    match m {
        IpMode::None => "none",
        IpMode::Anonymized => "anonymized",
        IpMode::Full => "full",
    }
}

pub fn parse_ip(s: &str) -> Option<IpMode> {
    match s {
        "none" => Some(IpMode::None),
        "anonymized" => Some(IpMode::Anonymized),
        "full" => Some(IpMode::Full),
        _ => None,
    }
}

fn ip_rank(m: IpMode) -> u8 {
    match m {
        IpMode::None => 0,
        IpMode::Anonymized => 1,
        IpMode::Full => 2,
    }
}

/// The stricter of two IP settings.
pub fn ip_min(a: IpMode, b: IpMode) -> IpMode {
    if ip_rank(b) < ip_rank(a) { b } else { a }
}

/// The installation's defaults and limits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Installation {
    pub ip_mode: IpMode,
    pub max_ip_mode: IpMode,
    pub query_strings: QueryStrings,
    pub max_query_strings: QueryStrings,
}

impl Installation {
    /// From pyo.settings 'observability'. Without a saved limit for
    /// addresses, the default is the limit: no team keeps more than the
    /// installation did before teams could choose. Query strings were
    /// always kept, so their limit and default are "keep" until set.
    pub fn from_settings(v: &Value) -> Self {
        let ip = |k: &str| v[k].as_str().and_then(parse_ip);
        let qs = |k: &str| v[k].as_str().and_then(QueryStrings::parse);
        let ip_mode = ip("ipMode").unwrap_or_default();
        let max_ip_mode = ip("maxIpMode").unwrap_or(ip_mode);
        let max_query_strings = qs("maxQueryStrings").unwrap_or_default();
        Installation {
            // A default above its limit (saved by hand) is the limit.
            ip_mode: ip_min(ip_mode, max_ip_mode),
            max_ip_mode,
            query_strings: qs("queryStrings").unwrap_or_default().min(max_query_strings),
            max_query_strings,
        }
    }

    /// As the snapshot and the settings endpoint show it.
    pub fn defaults_json(&self) -> Value {
        json!({ "ipMode": ip_str(self.ip_mode), "queryStrings": self.query_strings.as_str() })
    }

    pub fn limits_json(&self) -> Value {
        json!({ "ipMode": ip_str(self.max_ip_mode), "queryStrings": self.max_query_strings.as_str() })
    }
}

/// The installation's defaults and limits, as saved.
pub async fn installation<'e>(db: impl sqlx::PgExecutor<'e>) -> ApiResult<Installation> {
    let v: Option<Value> =
        sqlx::query_scalar("SELECT value FROM pyo.settings WHERE key = 'observability'")
            .fetch_optional(db)
            .await?;
    Ok(Installation::from_settings(&v.unwrap_or_default()))
}

/// A team's own choices. None: it follows the installation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Choice {
    pub on: Option<bool>,
    pub request_logs: Option<bool>,
    pub ip_mode: Option<IpMode>,
    pub query_strings: Option<QueryStrings>,
}

/// What applies to a team now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Effective {
    /// Its servers collect (the installation's switch is apart from this).
    pub on: bool,
    pub request_logs: bool,
    pub ip_mode: IpMode,
    pub query_strings: QueryStrings,
}

impl Default for Effective {
    /// A team Pyo knows nothing of: on, keeping nothing more than the
    /// strictest settings.
    fn default() -> Self {
        Effective { on: true, request_logs: true, ip_mode: IpMode::None, query_strings: QueryStrings::Keep }
    }
}

/// The team's choice or the default, within the limit.
pub fn effective(i: &Installation, c: &Choice) -> Effective {
    Effective {
        on: c.on.unwrap_or(true),
        request_logs: c.request_logs.unwrap_or(true),
        ip_mode: ip_min(c.ip_mode.unwrap_or(i.ip_mode), i.max_ip_mode),
        query_strings: c.query_strings.unwrap_or(i.query_strings).min(i.max_query_strings),
    }
}

type ChoiceRow = (String, Option<bool>, Option<bool>, Option<String>, Option<String>);

fn choice_of(r: &ChoiceRow) -> Choice {
    Choice {
        on: r.1,
        request_logs: r.2,
        ip_mode: r.3.as_deref().and_then(parse_ip),
        query_strings: r.4.as_deref().and_then(QueryStrings::parse),
    }
}

/// These teams' choices (teams that aren't there are missing).
pub async fn choices<'e>(db: impl sqlx::PgExecutor<'e>, team_ids: &[String]) -> ApiResult<HashMap<String, Choice>> {
    let rows: Vec<ChoiceRow> =
        sqlx::query_as("SELECT id, observability_on, request_logs_on, ip_mode, query_strings FROM pyo.teams WHERE id = ANY($1)")
            .bind(team_ids)
            .fetch_all(db)
            .await?;
    Ok(rows.iter().map(|r| (r.0.clone(), choice_of(r))).collect())
}

/// Every team's choices (for a change of the installation's limits).
pub async fn all_choices<'e>(db: impl sqlx::PgExecutor<'e>) -> ApiResult<Vec<(String, Choice)>> {
    let rows: Vec<ChoiceRow> =
        sqlx::query_as("SELECT id, observability_on, request_logs_on, ip_mode, query_strings FROM pyo.teams ORDER BY id")
            .fetch_all(db)
            .await?;
    Ok(rows.iter().map(|r| (r.0.clone(), choice_of(r))).collect())
}

/// What applies to these teams now.
pub async fn for_teams(db: &sqlx::PgPool, team_ids: &[String]) -> ApiResult<HashMap<String, Effective>> {
    let i = installation(db).await?;
    let c = choices(db, team_ids).await?;
    Ok(team_ids
        .iter()
        .map(|t| (t.clone(), effective(&i, &c.get(t).copied().unwrap_or_default())))
        .collect())
}

/// What applies to one team now.
pub async fn for_team(db: &sqlx::PgPool, team_id: &str) -> ApiResult<Effective> {
    Ok(for_teams(db, &[team_id.to_owned()]).await?.remove(team_id).unwrap_or_default())
}

/// The teams whose servers don't collect now (their observability is off).
pub async fn off_teams(db: &sqlx::PgPool) -> ApiResult<Vec<String>> {
    Ok(sqlx::query_scalar("SELECT id FROM pyo.teams WHERE observability_on = false")
        .fetch_all(db)
        .await?)
}

/// A team's settings in the snapshot: its own choices (null: the
/// installation's default), the installation's defaults and limits, and
/// what applies now.
pub fn snapshot(team: &mut Value, i: &Installation) {
    let c = Choice {
        on: team["observabilityOn"].as_bool(),
        request_logs: team["requestLogsOn"].as_bool(),
        ip_mode: team["ipMode"].as_str().and_then(parse_ip),
        query_strings: team["queryStrings"].as_str().and_then(QueryStrings::parse),
    };
    let e = effective(i, &c);
    team["observabilityDefaults"] = i.defaults_json();
    team["observabilityLimits"] = i.limits_json();
    team["observability"] = json!({
        "on": e.on, "requestLogs": e.request_logs,
        "ipMode": ip_str(e.ip_mode), "queryStrings": e.query_strings.as_str(),
    });
}

/// A choice to save from an op: a value, "default" (back to the
/// installation's), or absent (unchanged). Returns Some(None) for
/// "default".
pub fn requested<T>(v: Option<&str>, parse: impl Fn(&str) -> Option<T>, what: &str) -> ApiResult<Option<Option<T>>> {
    match v {
        None => Ok(None),
        Some("default") => Ok(Some(None)),
        Some(s) => parse(s).map(|x| Some(Some(x))).ok_or_else(|| ApiError::bad_request(what.to_owned())),
    }
}

/// As the dashboard names the choice.
pub fn ip_label(m: IpMode) -> &'static str {
    match m {
        IpMode::None => "don't store",
        IpMode::Anonymized => "anonymized",
        IpMode::Full => "full",
    }
}

pub fn query_label(q: QueryStrings) -> &'static str {
    match q {
        QueryStrings::Keep => "keep",
        QueryStrings::Names => "names only",
        QueryStrings::None => "don't keep",
    }
}

/// What a lower effective value means, in an activity entry's words.
pub fn ip_title(m: IpMode) -> &'static str {
    match m {
        IpMode::None => "Visitor IP addresses are no longer stored",
        IpMode::Anonymized => "Visitor IP addresses are now stored anonymized",
        IpMode::Full => "Full visitor IP addresses are now stored",
    }
}

pub fn query_title(q: QueryStrings) -> &'static str {
    match q {
        QueryStrings::None => "Query strings are no longer kept",
        QueryStrings::Names => "Query strings now keep parameter names only",
        QueryStrings::Keep => "Query strings are now kept",
    }
}

/// The teams whose values go down when the installation's settings change
/// from `before` to `after`, with what they become: (team, new IP mode if
/// it went down, new query strings if they went down).
pub fn lowered(
    before: &Installation,
    after: &Installation,
    teams: &[(String, Choice)],
) -> Vec<(String, Option<IpMode>, Option<QueryStrings>)> {
    teams
        .iter()
        .filter_map(|(id, c)| {
            let (b, a) = (effective(before, c), effective(after, c));
            let ip = (ip_rank(a.ip_mode) < ip_rank(b.ip_mode)).then_some(a.ip_mode);
            let qs = (a.query_strings.rank() < b.query_strings.rank()).then_some(a.query_strings);
            (ip.is_some() || qs.is_some()).then(|| (id.clone(), ip, qs))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inst(v: Value) -> Installation {
        Installation::from_settings(&v)
    }

    #[test]
    fn the_installation_before_teams_chose() {
        // Nothing saved: no addresses, query strings kept, no team keeps more.
        let i = inst(json!({}));
        assert_eq!(i, Installation { ip_mode: IpMode::None, max_ip_mode: IpMode::None, query_strings: QueryStrings::Keep, max_query_strings: QueryStrings::Keep });
        // An installation that kept full addresses: that's its limit too.
        let i = inst(json!({ "enabled": true, "ipMode": "full" }));
        assert_eq!((i.ip_mode, i.max_ip_mode), (IpMode::Full, IpMode::Full));
        // A default above its limit is the limit.
        let i = inst(json!({ "ipMode": "full", "maxIpMode": "anonymized", "queryStrings": "keep", "maxQueryStrings": "names" }));
        assert_eq!((i.ip_mode, i.query_strings), (IpMode::Anonymized, QueryStrings::Names));
        // Junk is ignored.
        let i = inst(json!({ "ipMode": 3, "maxIpMode": "everything", "queryStrings": "some" }));
        assert_eq!(i, inst(json!({})));
    }

    #[test]
    fn a_team_chooses_within_the_limit() {
        let i = inst(json!({ "ipMode": "none", "maxIpMode": "anonymized", "queryStrings": "keep", "maxQueryStrings": "keep" }));
        let e = effective(&i, &Choice::default());
        assert_eq!(e, Effective { on: true, request_logs: true, ip_mode: IpMode::None, query_strings: QueryStrings::Keep });
        let c = Choice { on: Some(false), request_logs: Some(false), ip_mode: Some(IpMode::Full), query_strings: Some(QueryStrings::Names) };
        let e = effective(&i, &c);
        assert_eq!(e, Effective { on: false, request_logs: false, ip_mode: IpMode::Anonymized, query_strings: QueryStrings::Names }, "full is above the limit");
        // Raised again: the team's choice is back.
        let raised = inst(json!({ "ipMode": "none", "maxIpMode": "full" }));
        assert_eq!(effective(&raised, &c).ip_mode, IpMode::Full);
    }

    #[test]
    fn teams_hear_when_theirs_goes_down() {
        let before = inst(json!({ "ipMode": "anonymized", "maxIpMode": "full", "queryStrings": "keep", "maxQueryStrings": "keep" }));
        let after = inst(json!({ "ipMode": "anonymized", "maxIpMode": "anonymized", "queryStrings": "keep", "maxQueryStrings": "names" }));
        let teams = vec![
            ("team_full".to_owned(), Choice { ip_mode: Some(IpMode::Full), ..Default::default() }),
            ("team_default".to_owned(), Choice::default()),
            ("team_strict".to_owned(), Choice { ip_mode: Some(IpMode::None), query_strings: Some(QueryStrings::None), ..Default::default() }),
        ];
        let l = lowered(&before, &after, &teams);
        assert_eq!(
            l,
            vec![
                ("team_full".to_owned(), Some(IpMode::Anonymized), Some(QueryStrings::Names)),
                ("team_default".to_owned(), None, Some(QueryStrings::Names)),
            ],
            "the strict team keeps less already"
        );
        assert!(lowered(&after, &before, &teams).is_empty(), "raising tells no one");
    }

    #[test]
    fn the_snapshot_shows_choices_defaults_limits_and_what_applies() {
        let i = inst(json!({ "ipMode": "anonymized", "maxIpMode": "anonymized" }));
        let mut t = json!({ "observabilityOn": null, "requestLogsOn": false, "ipMode": "full", "queryStrings": null });
        snapshot(&mut t, &i);
        assert_eq!(t["observabilityDefaults"], json!({ "ipMode": "anonymized", "queryStrings": "keep" }));
        assert_eq!(t["observabilityLimits"], json!({ "ipMode": "anonymized", "queryStrings": "keep" }));
        assert_eq!(t["observability"], json!({ "on": true, "requestLogs": false, "ipMode": "anonymized", "queryStrings": "keep" }));
        assert_eq!(t["ipMode"], "full", "the team's own choice stays as saved");
    }

    #[test]
    fn requested_values() {
        let p = |s: &str| requested(Some(s), parse_ip, "bad");
        assert_eq!(p("full").unwrap(), Some(Some(IpMode::Full)));
        assert_eq!(p("default").unwrap(), Some(None));
        assert!(p("all").is_err());
        assert_eq!(requested(None, parse_ip, "bad").unwrap(), None);
    }
}
