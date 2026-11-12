//! The firewall's rules, and the WAF's settings, per service
//! (pyo-proto's firewall.rs says how they're checked): sent to the agent
//! with each deploy and, when they change, to every running service on the
//! cluster at once (`SyncFirewall`), with the WAF's engine installed or
//! removed first as the cluster's services need it (bundles.rs).

use pyo_proto::{
    Command, DeploySpec, FirewallSync, FirewallTarget,
    firewall::{
        Firewall, FirewallRule, MAX_RATE_LIMITS, MAX_RULES, MAX_SOURCES, RateLimit, is_country,
        rate_limit_cost,
    },
};
use serde_json::Value;
use sqlx::{Postgres, Transaction};

use crate::error::{ApiError, ApiResult};

/// A source as stored: an address range ("203.0.113.7" becomes
/// "203.0.113.7/32") or an upper-case country code.
pub fn normalize_source(s: &str) -> ApiResult<String> {
    let s = s.trim();
    if s.len() == 2 && s.bytes().all(|b| b.is_ascii_alphabetic()) {
        let cc = s.to_ascii_uppercase();
        if cc == "EU" || cc == "XX" || cc == "ZZ" {
            return Err(ApiError::bad_request(format!("{cc} isn't a country.")));
        }
        return Ok(cc);
    }
    crate::deploy::normalize_cidr(s).ok_or_else(|| {
        ApiError::bad_request(format!(
            "{s:?} isn't an IP address, a range (203.0.113.0/24) or a two-letter country code."
        ))
    })
}

/// A rule as stored, from what was asked: its action, operator, sources
/// (normalized, as `normalize_source`) and limit. Any client is "in" with no
/// sources; a rule lists addresses and ranges, or countries, not both.
pub fn check_rule(
    action: &str,
    operator: &str,
    sources: &[String],
    requests: Option<u32>,
    per: Option<&str>,
) -> ApiResult<FirewallRule> {
    if !matches!(action, "allow" | "block" | "rateLimit") {
        return Err(ApiError::bad_request(
            "A rule's action is \"allow\", \"block\" or \"rateLimit\".",
        ));
    }
    if !matches!(operator, "in" | "notIn") {
        return Err(ApiError::bad_request(
            "A rule's operator is \"in\" or \"notIn\".",
        ));
    }
    if sources.len() > MAX_SOURCES {
        return Err(ApiError::bad_request(format!(
            "A rule lists at most {MAX_SOURCES} values."
        )));
    }
    let mut out: Vec<String> = vec![];
    for s in sources {
        let s = normalize_source(s)?;
        if !out.contains(&s) {
            out.push(s);
        }
    }
    if out.is_empty() && operator == "notIn" {
        return Err(ApiError::bad_request("\"notIn\" needs at least one value."));
    }
    let countries = out.iter().filter(|s| is_country(s)).count();
    if countries > 0 && countries < out.len() {
        return Err(ApiError::bad_request(
            "List addresses and ranges, or countries, not both: make two rules.",
        ));
    }
    let limit = if action == "rateLimit" {
        if countries > 0 {
            return Err(ApiError::bad_request("Rate limits can't match countries."));
        }
        if operator == "notIn" && out.len() > 1 {
            return Err(ApiError::bad_request(
                "A rate limit with \"notIn\" takes one address or range.",
            ));
        }
        let requests =
            requests.ok_or_else(|| ApiError::bad_request("A rate limit needs \"requests\"."))?;
        if !(1..=1_000_000).contains(&requests) {
            return Err(ApiError::bad_request(
                "A rate limit is between 1 and 1,000,000 requests.",
            ));
        }
        let per = per.unwrap_or("minute");
        if !matches!(per, "second" | "minute") {
            return Err(ApiError::bad_request(
                "A rate limit is per \"second\" or \"minute\".",
            ));
        }
        Some(RateLimit {
            requests,
            per: per.to_owned(),
        })
    } else {
        None
    };
    Ok(FirewallRule {
        id: String::new(),
        action: action.to_owned(),
        operator: operator.to_owned(),
        sources: out,
        limit,
    })
}

type Row = (
    String,
    String,
    String,
    Vec<String>,
    Option<i32>,
    Option<String>,
);

fn rule_of((id, action, operator, sources, requests, per): Row) -> FirewallRule {
    let limit = match (requests, per) {
        (Some(n), Some(per)) => Some(RateLimit {
            requests: n.max(0) as u32,
            per,
        }),
        _ => None,
    };
    FirewallRule {
        id,
        action,
        operator,
        sources,
        limit,
    }
}

/// A service's firewall: its rules, in order.
pub async fn for_service(
    tx: &mut Transaction<'_, Postgres>,
    service_id: &str,
) -> ApiResult<Firewall> {
    let rules: Vec<Row> = sqlx::query_as("SELECT id, action, operator, sources, requests, per FROM pyo.firewall_rules WHERE service_id = $1 ORDER BY position, created_at, id")
        .bind(service_id)
        .fetch_all(&mut **tx)
        .await?;
    Ok(Firewall {
        rules: rules.into_iter().map(rule_of).collect(),
    })
}

/// A service can take one more rule (`adding`), or this one changed
/// (`changing`): at most `MAX_RULES`, and its rate limits within what the
/// proxy takes.
pub async fn check_count(
    tx: &mut Transaction<'_, Postgres>,
    service_id: &str,
    rule: &FirewallRule,
    changing: Option<&str>,
) -> ApiResult<()> {
    let fw = for_service(tx, service_id).await?;
    let others: Vec<&FirewallRule> = fw
        .rules
        .iter()
        .filter(|r| Some(r.id.as_str()) != changing)
        .collect();
    if changing.is_none() && others.len() >= MAX_RULES {
        return Err(ApiError::conflict(format!(
            "A service's firewall holds at most {MAX_RULES} rules. Combine addresses into ranges, or remove some first."
        )));
    }
    let cost: usize = others
        .iter()
        .copied()
        .chain([rule])
        .filter(|r| r.rate_limits())
        .map(rate_limit_cost)
        .sum();
    if cost > MAX_RATE_LIMITS {
        return Err(ApiError::conflict(format!(
            "A service's rate limits take at most {MAX_RATE_LIMITS} addresses or ranges in all (\"any client\" takes 2). Combine some first."
        )));
    }
    Ok(())
}

/// A rule in words, for the activity log: "Block if client IP is in
/// 203.0.113.0/24".
pub fn describe(r: &FirewallRule) -> String {
    let action = match r.action.as_str() {
        "allow" => "Allow".to_owned(),
        "block" => "Block".to_owned(),
        _ => match &r.limit {
            Some(l) => format!("Rate limit to {} a {}", l.requests, l.per),
            None => "Rate limit".to_owned(),
        },
    };
    if r.any() {
        return match (&r.limit, r.rate_limits()) {
            (Some(l), true) => format!("Rate limit any client to {} a {}", l.requests, l.per),
            _ => format!("{action} any client"),
        };
    }
    let field = if r.has_countries() {
        "country"
    } else {
        "client IP"
    };
    let op = if r.negated() { "is not in" } else { "is in" };
    let mut list = r
        .sources
        .iter()
        .take(3)
        .map(|s| match s.split_once('/') {
            Some((a, "32")) if !a.contains(':') => a.to_owned(),
            Some((a, "128")) => a.to_owned(),
            _ => s.clone(),
        })
        .collect::<Vec<_>>()
        .join(", ");
    if r.sources.len() > 3 {
        list.push_str(&format!(" and {} more", r.sources.len() - 3));
    }
    format!("{action} if {field} {op} {list}")
}

/// The WAF for a deploy or a sync: none while no service on the cluster has
/// its WAF on (its engine isn't there); else the service's, which is off
/// for a service that hasn't turned it on (the agent then rebuilds the
/// engine's settings without its routes).
async fn waf_for(
    tx: &mut Transaction<'_, Postgres>,
    engine: bool,
    service_id: &str,
) -> ApiResult<Option<pyo_proto::Waf>> {
    Ok(if engine {
        Some(crate::waf::for_service(tx, service_id).await?)
    } else {
        None
    })
}

/// Send the firewall and WAF as they now stand to every running service on
/// the cluster (their live deployments' routes and ports), with the WAF's
/// engine installed first when a service there has its WAF on, or removed
/// after the routes stop using it when none has.
pub async fn sync(tx: &mut Transaction<'_, Postgres>, cluster_id: &str) -> ApiResult<()> {
    let rows: Vec<(String, Value, bool, Value, i64)> = sqlx::query_as(
        "SELECT a.id, d.spec, p.network_isolation, coalesce(a.spec->'domains', '[]'), a.routes_revision
           FROM pyo.services a JOIN pyo.deployments d ON d.id = a.current_deployment_id JOIN pyo.projects p ON p.id = a.project_id
          WHERE a.cluster_id = $1 AND NOT a.deleting",
    )
    .bind(cluster_id)
    .fetch_all(&mut **tx)
    .await?;
    let engine = crate::waf::wanted(tx, cluster_id).await?;
    let mut targets = vec![];
    for (service_id, spec, isolation, domains, routes_revision) in rows {
        let firewall = for_service(tx, &service_id).await?;
        // Only what routes are made of: the live deployment's service and
        // ports (never its variables), and the service's domains.
        let Ok(service) = serde_json::from_value(spec["service"].clone()) else {
            continue;
        };
        targets.push(FirewallTarget {
            service,
            project_id: spec["projectId"].as_str().unwrap_or("").to_owned(),
            ports: serde_json::from_value(spec["ports"].clone()).unwrap_or_default(),
            domains: crate::routes::domain_specs(&domains),
            routes_revision,
            network_isolation: isolation,
            firewall,
            waf: waf_for(tx, engine, &service_id).await?,
        });
    }
    let status: Option<String> =
        sqlx::query_scalar("SELECT status FROM pyo.clusters WHERE id = $1")
            .bind(cluster_id)
            .fetch_optional(&mut **tx)
            .await?;
    // A cluster that isn't connected gets it with its next deploys (and the
    // command waits in its queue meanwhile).
    if status.is_some() {
        // The agent runs these in this order (a sync takes turns with the
        // WAF's bundle): the engine's install before routes point at it, its
        // removal after they no longer do.
        if engine {
            crate::bundles::sync_waf(tx, cluster_id, true).await?;
        }
        crate::deploy::enqueue(
            tx,
            cluster_id,
            Command::SyncFirewall(Box::new(FirewallSync {
                targets,
                waf: engine,
            })),
        )
        .await?;
        if !engine {
            crate::bundles::sync_waf(tx, cluster_id, false).await?;
        }
    }
    Ok(())
}

/// Every service's firewall on every cluster, for naming the rule behind a
/// refusal: (cluster, service) -> its firewall.
pub async fn all_firewalls(
    db: &sqlx::PgPool,
) -> ApiResult<std::collections::HashMap<(String, String), Firewall>> {
    let mut tx = db.begin().await?;
    let services: Vec<(String, String)> =
        sqlx::query_as("SELECT cluster_id, id FROM pyo.services WHERE NOT deleting")
            .fetch_all(&mut *tx)
            .await?;
    let mut out = std::collections::HashMap::new();
    for (cluster, service) in services {
        let f = for_service(&mut tx, &service).await?;
        out.insert((cluster, service), f);
    }
    tx.commit().await?;
    Ok(out)
}

/// Fill in a deploy's firewall (none without rules) and WAF
/// (`waf_for`).
pub async fn fill(
    tx: &mut Transaction<'_, Postgres>,
    cluster_id: &str,
    service_id: &str,
    spec: &mut DeploySpec,
) -> ApiResult<()> {
    let f = for_service(tx, service_id).await?;
    spec.firewall = (!f.is_unrestricted()).then_some(f);
    let engine = crate::waf::wanted(tx, cluster_id).await?;
    spec.waf = waf_for(tx, engine, service_id).await?;
    Ok(())
}

/// Why the proxy refused a request or connection, from its log line:
/// "blocked" (an allow or block rule; the proxy doesn't say which) or
/// "rate-limit"; "" when it didn't.
pub fn refusal(details: &str, termination: &str, status: u64) -> String {
    if [details, termination]
        .iter()
        .any(|d| d.starts_with("rbac_access_denied_matched_policy["))
    {
        return "blocked".into();
    }
    if status == 429 && details.contains("rate_limited") {
        return "rate-limit".into();
    }
    String::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sources() {
        assert_eq!(normalize_source(" de ").unwrap(), "DE");
        assert_eq!(normalize_source("203.0.113.7").unwrap(), "203.0.113.7/32");
        assert_eq!(
            normalize_source("203.0.113.7/24").unwrap(),
            "203.0.113.0/24"
        );
        assert_eq!(normalize_source("2001:db8::1/32").unwrap(), "2001:db8::/32");
        assert!(
            normalize_source("Germany").is_err()
                && normalize_source("10.0.0.0/33").is_err()
                && normalize_source("EU").is_err()
        );
    }

    #[test]
    fn rules() {
        let v = |s: &[&str]| s.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        let r = check_rule(
            "block",
            "notIn",
            &v(&["203.0.113.7", "203.0.113.7/32"]),
            None,
            None,
        )
        .unwrap();
        assert_eq!((r.sources, r.limit), (v(&["203.0.113.7/32"]), None));
        assert!(
            check_rule("block", "notIn", &[], None, None).is_err(),
            "not in nothing"
        );
        assert!(
            check_rule("block", "in", &v(&["DE", "10.0.0.0/8"]), None, None).is_err(),
            "mixed"
        );
        assert!(
            check_rule("rateLimit", "in", &v(&["DE"]), Some(10), None).is_err(),
            "countries"
        );
        assert!(
            check_rule(
                "rateLimit",
                "notIn",
                &v(&["10.0.0.0/8", "10.1.0.0/16"]),
                Some(10),
                None
            )
            .is_err()
        );
        assert!(
            check_rule("rateLimit", "in", &[], None, None).is_err(),
            "no limit"
        );
        assert!(check_rule("rateLimit", "in", &[], Some(10), Some("hour")).is_err());
        let r = check_rule("rateLimit", "in", &[], Some(10), Some("second")).unwrap();
        assert_eq!(
            r.limit,
            Some(RateLimit {
                requests: 10,
                per: "second".into()
            })
        );
        assert_eq!(describe(&r), "Rate limit any client to 10 a second");
        assert!(check_rule("skip", "in", &[], None, None).is_err());
        assert_eq!(
            describe(&check_rule("block", "notIn", &v(&["DE", "FR"]), None, None).unwrap()),
            "Block if country is not in DE, FR"
        );
        assert_eq!(
            describe(
                &check_rule(
                    "allow",
                    "in",
                    &v(&["203.0.113.7", "2001:db8::/32"]),
                    None,
                    None
                )
                .unwrap()
            ),
            "Allow if client IP is in 203.0.113.7, 2001:db8::/32"
        );
    }

    #[test]
    fn refusals() {
        assert_eq!(
            refusal("rbac_access_denied_matched_policy[DENY]", "", 403),
            "blocked"
        );
        assert_eq!(
            refusal("rbac_access_denied_matched_policy[none]", "", 403),
            "blocked"
        );
        assert_eq!(
            refusal("", "rbac_access_denied_matched_policy[DENY]", 0),
            "blocked"
        );
        assert_eq!(refusal("local_rate_limited", "", 429), "rate-limit");
        assert_eq!(refusal("via_upstream", "", 403), "");
    }
}
