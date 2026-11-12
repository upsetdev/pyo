//! The web application firewall's settings, per service: the OWASP rules'
//! mode and tuning, and custom rules. While any service on a cluster has
//! its WAF on, the WAF's engine runs there (`wanted`, bundles.rs).
//!
//! Custom rules are data all the way: Pyo checks each value here and sends
//! the rules as JSON; pyo-waf evaluates them itself (plain comparisons and
//! RE2 regexes, in Go) before the OWASP rules. Nothing a user writes ever
//! becomes rule-language text, so no value can add or change a directive.

use pyo_proto::{
    WAF_ACTIONS, WAF_FIELDS, WAF_MAX_CONDITIONS, WAF_MAX_RULES, WAF_MAX_VALUE, WAF_MAX_VALUES,
    WAF_OPERATORS, WafCondition, WafRule, WafSettings,
};
use serde::Deserialize;
use serde_json::Value;
use sqlx::{Postgres, Transaction};

use crate::error::{ApiError, ApiResult};

/// A service's custom rules take at most this much (as JSON), so they fit
/// on its route's policy and in the processor's settings.
pub const MAX_RULES_BYTES: usize = 32 * 1024;

/// A condition as the API takes it: one `value`, or `values` for "in".
#[derive(Debug, Clone, Default, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ConditionInput {
    /// What it reads: "path", "query" (the query string), "method", "host", "header" (by `name`), "userAgent", "cookie" (by `name`), "body", "ip" (the visitor's address) or "country" (a two-letter code).
    pub field: String,
    /// The header's or cookie's name.
    #[serde(default)]
    pub name: String,
    /// "equals", "contains", "startsWith", "endsWith", "in" (one of `values`) or "regex" (RE2 syntax). "ip" takes "in" (addresses and ranges). "country" and "method" take "equals" or "in".
    pub op: String,
    #[serde(default)]
    pub value: Option<String>,
    #[serde(default)]
    pub values: Vec<String>,
    /// Match when the test fails instead.
    #[serde(default)]
    pub negate: bool,
}

fn bad(msg: impl Into<String>) -> ApiError {
    ApiError::bad_request(msg)
}

/// A header or cookie name: an HTTP token.
fn valid_name(n: &str) -> bool {
    !n.is_empty()
        && n.len() <= 64
        && n.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b))
}

/// A regex pyo-waf compiles the same way (Go's RE2): Rust's syntax minus
/// what only Rust has, bounded in size.
pub fn check_regex(re: &str) -> ApiResult<()> {
    if ["&&", "--", "~~"]
        .iter()
        .any(|op| re.contains(op) && re.contains('['))
        || re.contains("\\b{")
    {
        return Err(bad(
            "That regex uses syntax pyo-waf doesn't have (class set operations or \\b{...}).",
        ));
    }
    regex::RegexBuilder::new(re)
        .size_limit(1 << 20)
        .nest_limit(50)
        .build()
        .map_err(|e| {
            bad(format!(
                "That regex doesn't compile: {}",
                e.to_string().lines().last().unwrap_or("invalid")
            ))
        })?;
    Ok(())
}

/// A condition, checked and normalized (values trimmed of nothing: they
/// compare as written, except hosts, methods, countries and addresses).
pub fn condition(c: ConditionInput) -> ApiResult<WafCondition> {
    let field = c.field.trim();
    if !WAF_FIELDS.contains(&field) {
        return Err(bad(format!(
            "A condition's field is one of {}.",
            WAF_FIELDS.join(", ")
        )));
    }
    let op = c.op.trim();
    if !WAF_OPERATORS.contains(&op) {
        return Err(bad(format!(
            "A condition's operator is one of {}.",
            WAF_OPERATORS.join(", ")
        )));
    }
    let name = if matches!(field, "header" | "cookie") {
        let n = c.name.trim();
        if !valid_name(n) {
            return Err(bad(format!(
                "Name the {field}: letters, digits and - _ . only, up to 64."
            )));
        }
        if field == "header" {
            n.to_ascii_lowercase()
        } else {
            n.to_owned()
        }
    } else {
        String::new()
    };
    let mut values = c.values;
    if let Some(v) = c.value {
        values.insert(0, v);
    }
    match (op, values.len()) {
        ("in", 0) => return Err(bad("List at least one value.")),
        ("in", n) if n > WAF_MAX_VALUES => {
            return Err(bad(format!(
                "A list holds at most {WAF_MAX_VALUES} values."
            )));
        }
        ("in", _) => {}
        (_, 1) => {}
        _ => return Err(bad("Give one value (or use \"in\" for a list).")),
    }
    if let Some(v) = values.iter().find(|v| v.len() > WAF_MAX_VALUE) {
        return Err(bad(format!(
            "Values are at most {WAF_MAX_VALUE} bytes; {:?}… is longer.",
            v.chars().take(20).collect::<String>()
        )));
    }
    if values.iter().any(|v| v.chars().any(char::is_control)) {
        return Err(bad("Values can't hold control characters (like newlines)."));
    }
    let values = match field {
        "ip" => {
            if op != "in" {
                return Err(bad(
                    "An address condition uses \"in\": addresses and ranges.",
                ));
            }
            values
                .iter()
                .map(|v| {
                    crate::deploy::normalize_cidr(v).ok_or_else(|| {
                        bad(format!("{v:?} isn't an address or range (203.0.113.0/24)."))
                    })
                })
                .collect::<ApiResult<Vec<_>>>()?
        }
        "country" => {
            if !matches!(op, "equals" | "in") {
                return Err(bad("A country condition uses \"equals\" or \"in\"."));
            }
            values
                .iter()
                .map(|v| {
                    let v = v.trim();
                    if v.len() == 2 && v.bytes().all(|b| b.is_ascii_alphabetic()) {
                        Ok(v.to_ascii_uppercase())
                    } else {
                        Err(bad(format!("{v:?} isn't a two-letter country code.")))
                    }
                })
                .collect::<ApiResult<Vec<_>>>()?
        }
        "method" => {
            if !matches!(op, "equals" | "in") {
                return Err(bad("A method condition uses \"equals\" or \"in\"."));
            }
            values
                .iter()
                .map(|v| {
                    let v = v.trim().to_ascii_uppercase();
                    if !v.is_empty() && v.len() <= 20 && v.bytes().all(|b| b.is_ascii_uppercase()) {
                        Ok(v)
                    } else {
                        Err(bad(format!("{v:?} isn't a method, like GET.")))
                    }
                })
                .collect::<ApiResult<Vec<_>>>()?
        }
        "host" => values
            .iter()
            .map(|v| v.trim().to_ascii_lowercase())
            .collect(),
        _ => values,
    };
    if op == "regex" {
        check_regex(&values[0])?;
    }
    Ok(WafCondition {
        field: field.to_owned(),
        name,
        op: op.to_owned(),
        values,
        negate: c.negate,
    })
}

/// A rule's parts, checked.
pub fn check_rule(
    name: &str,
    matching: &str,
    conditions: Vec<ConditionInput>,
    action: &str,
) -> ApiResult<(String, Vec<WafCondition>)> {
    let name = name.trim();
    if name.is_empty() || name.chars().count() > 80 || name.chars().any(char::is_control) {
        return Err(bad("Name the rule: up to 80 characters."));
    }
    if !matches!(matching, "all" | "any") {
        return Err(bad(
            "A rule matches \"all\" of its conditions or \"any\" of them.",
        ));
    }
    if !WAF_ACTIONS.contains(&action) {
        return Err(bad(
            "A rule's action is \"block\", \"log\", \"allow\" or \"skipOwasp\".",
        ));
    }
    if conditions.is_empty() || conditions.len() > WAF_MAX_CONDITIONS {
        return Err(bad(format!(
            "A rule has 1 to {WAF_MAX_CONDITIONS} conditions."
        )));
    }
    let conditions = conditions
        .into_iter()
        .map(condition)
        .collect::<ApiResult<Vec<_>>>()?;
    Ok((name.to_owned(), conditions))
}

/// A service's rules in order, all of them (on or off).
pub async fn rules(
    tx: &mut Transaction<'_, Postgres>,
    service_id: &str,
) -> ApiResult<Vec<(WafRule, bool, String)>> {
    let rows: Vec<(String, i32, String, bool, String, Value, String)> = sqlx::query_as(
        "SELECT id, number, name, enabled, matching, conditions, action FROM pyo.waf_rules
          WHERE service_id = $1 ORDER BY position, created_at, id",
    )
    .bind(service_id)
    .fetch_all(&mut **tx)
    .await?;
    Ok(rows
        .into_iter()
        .map(
            |(id, number, name, enabled, matching, conditions, action)| {
                (
                    WafRule {
                        id: number as u32,
                        name,
                        matching,
                        conditions: serde_json::from_value(conditions).unwrap_or_default(),
                        action,
                    },
                    enabled,
                    id,
                )
            },
        )
        .collect())
}

/// A service can take one more rule, or this one changed: at most
/// `WAF_MAX_RULES` and `MAX_RULES_BYTES`.
pub async fn check_rules(
    tx: &mut Transaction<'_, Postgres>,
    service_id: &str,
    adding: Option<&WafRule>,
    replacing: Option<&str>,
) -> ApiResult<()> {
    let mut all: Vec<WafRule> = rules(tx, service_id)
        .await?
        .into_iter()
        .filter(|(_, _, id)| Some(id.as_str()) != replacing)
        .map(|(r, ..)| r)
        .collect();
    all.extend(adding.cloned());
    if all.len() > WAF_MAX_RULES {
        return Err(ApiError::conflict(format!(
            "A service holds at most {WAF_MAX_RULES} custom rules."
        )));
    }
    if serde_json::to_vec(&all).map(|v| v.len()).unwrap_or(0) > MAX_RULES_BYTES {
        return Err(ApiError::conflict(
            "These rules are too large together. Shorten lists, or combine rules.",
        ));
    }
    Ok(())
}

/// The service's OWASP settings as stored ("off" when unset).
pub fn own_settings(waf: &Value) -> WafSettings {
    let mut s: WafSettings = serde_json::from_value(waf.clone()).unwrap_or_default();
    // The mode as `wanted` reads it, whatever else is (or isn't) stored.
    s.mode = waf["mode"]
        .as_str()
        .filter(|m| matches!(*m, "log" | "block"))
        .unwrap_or("off")
        .to_owned();
    if s.level == 0 {
        s.level = 1;
    }
    s
}

/// A service's WAF as it applies: its settings, and its custom rules that
/// are on.
pub async fn for_service(
    tx: &mut Transaction<'_, Postgres>,
    service_id: &str,
) -> ApiResult<pyo_proto::Waf> {
    let waf: Value = sqlx::query_scalar("SELECT waf FROM pyo.services WHERE id = $1")
        .bind(service_id)
        .fetch_one(&mut **tx)
        .await?;
    let rules = rules(tx, service_id)
        .await?
        .into_iter()
        .filter(|(_, on, _)| *on)
        .map(|(r, ..)| r)
        .collect();
    Ok(pyo_proto::Waf {
        settings: own_settings(&waf),
        rules,
    })
}

/// A service's stored WAF has it on (log only or block).
pub fn is_on(waf: &Value) -> bool {
    own_settings(waf).on()
}

/// Whether the WAF's engine should run on a cluster: a service there (not
/// being deleted) has its WAF on.
pub async fn wanted(tx: &mut Transaction<'_, Postgres>, cluster_id: &str) -> ApiResult<bool> {
    Ok(sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM pyo.services WHERE cluster_id = $1 AND NOT deleting AND waf->>'mode' IN ('log', 'block'))")
        .bind(cluster_id)
        .fetch_one(&mut **tx)
        .await?)
}

/// A service's OWASP settings, checked.
pub fn check_settings(mode: &str, level: u8, exceptions: &[u32]) -> ApiResult<()> {
    if !matches!(mode, "off" | "log" | "block") {
        return Err(bad(
            "The WAF's mode is \"off\", \"log\" (log only) or \"block\".",
        ));
    }
    if !(1..=4).contains(&level) {
        return Err(bad("The WAF's strictness is 1 (fewest false alarms) to 4."));
    }
    if exceptions.len() > 200 {
        return Err(bad("A service has at most 200 WAF exceptions."));
    }
    // The Core Rule Set's rules are numbered 900000 to 999999.
    if let Some(id) = exceptions
        .iter()
        .find(|id| !(900_000..=999_999).contains(*id))
    {
        return Err(bad(format!(
            "{id} isn't a rule of the Core Rule Set (900000 to 999999)."
        )));
    }
    Ok(())
}

/// A service's anomaly threshold, checked.
pub fn check_threshold(threshold: u32) -> ApiResult<()> {
    if threshold != 0 && !pyo_proto::WAF_THRESHOLDS.contains(&threshold) {
        return Err(bad(format!(
            "The anomaly threshold is one of {:?}.",
            pyo_proto::WAF_THRESHOLDS
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn c(field: &str, op: &str, values: &[&str]) -> ConditionInput {
        ConditionInput {
            field: field.into(),
            op: op.into(),
            values: values.iter().map(|v| v.to_string()).collect(),
            ..Default::default()
        }
    }

    #[test]
    fn settings() {
        assert!(check_settings("block", 2, &[942100, 941160]).is_ok());
        assert!(check_settings("on", 1, &[]).is_err());
        assert!(check_settings("block", 5, &[]).is_err());
        assert!(check_settings("block", 1, &[42]).is_err());
        assert!(
            check_settings("default", 1, &[]).is_err(),
            "no cluster default"
        );
        assert!(check_settings("off", 1, &[]).is_ok());
        assert!(check_threshold(20).is_ok() && check_threshold(0).is_ok());
        assert!(check_threshold(7).is_err());
        // A new service's WAF is off.
        let d = own_settings(&json!({}));
        assert_eq!((d.mode.as_str(), d.level), ("off", 1));
        assert!(!is_on(&json!({})));
        let d = own_settings(&json!({ "mode": "log", "level": 2, "threshold": 20 }));
        assert_eq!((d.mode.as_str(), d.level, d.threshold), ("log", 2, 20));
        assert!(is_on(&json!({ "mode": "block" })) && is_on(&json!({ "mode": "log" })));
        assert!(
            !is_on(&json!({ "mode": "off", "level": 3 })) && !is_on(&json!({ "mode": "default" }))
        );
    }

    #[test]
    fn conditions() {
        let ok = |x: ConditionInput| condition(x).unwrap();
        assert_eq!(
            ok(c("path", "startsWith", &["/wp-admin"])).values,
            ["/wp-admin"]
        );
        assert_eq!(
            ok(c("ip", "in", &["203.0.113.7", "2001:db8::1/32"])).values,
            ["203.0.113.7/32", "2001:db8::/32"]
        );
        assert_eq!(ok(c("country", "in", &["de", " RU "])).values, ["DE", "RU"]);
        assert_eq!(ok(c("method", "equals", &["post"])).values, ["POST"]);
        assert_eq!(
            ok(c("host", "equals", &["Shop.Example.com"])).values,
            ["shop.example.com"]
        );
        let h = ok(ConditionInput {
            name: "X-Api-Key".into(),
            ..c("header", "equals", &["x"])
        });
        assert_eq!(h.name, "x-api-key");
        assert!(
            ok(ConditionInput {
                value: Some("a".into()),
                ..c("query", "contains", &[])
            })
            .values
                == ["a"]
        );
        // Wrong shapes.
        for bad in [
            c("nope", "equals", &["x"]),
            c("path", "like", &["x"]),
            c("path", "equals", &[]),
            c("path", "equals", &["a", "b"]),
            c("ip", "equals", &["1.2.3.4"]),
            c("ip", "in", &["1.2.3.4/33"]),
            c("country", "contains", &["D"]),
            c("country", "in", &["Germany"]),
            c("method", "regex", &["P.*"]),
            c("header", "equals", &["x"]),
            c("path", "regex", &["(unclosed"]),
            c("path", "regex", &["[a&&b]"]),
            c("path", "equals", &["a\nSecRule"]),
            c("path", "equals", &[&"a".repeat(257)]),
            c("path", "in", &vec!["a"; 51]),
        ] {
            assert!(condition(bad.clone()).is_err(), "{bad:?}");
        }
        assert!(
            condition(ConditionInput {
                name: "x y".into(),
                ..c("cookie", "equals", &["1"])
            })
            .is_err()
        );
        // A regex that would be huge is refused rather than compiled.
        assert!(check_regex("(a{1000}){1000}").is_err());
    }

    #[test]
    fn rules_checked() {
        assert!(
            check_rule(
                "Block admin",
                "all",
                vec![c("path", "startsWith", &["/wp-admin"])],
                "block"
            )
            .is_ok()
        );
        assert!(check_rule("", "all", vec![c("path", "startsWith", &["/x"])], "block").is_err());
        assert!(check_rule("x", "some", vec![c("path", "startsWith", &["/x"])], "block").is_err());
        assert!(check_rule("x", "all", vec![], "block").is_err());
        assert!(
            check_rule(
                "x",
                "all",
                vec![c("path", "startsWith", &["/x"]); 11],
                "block"
            )
            .is_err()
        );
        assert!(check_rule("x", "any", vec![c("path", "startsWith", &["/x"])], "deny").is_err());
        assert!(
            check_rule(
                "x",
                "any",
                vec![c("path", "startsWith", &["/x"])],
                "skipOwasp"
            )
            .is_ok()
        );
    }

    /// Whatever a value holds, it stays one JSON string in the processor's
    /// settings: quotes, rule-language text and odd bytes included.
    #[test]
    fn values_stay_data() {
        let mut seed = 0x2545_f491_4f6c_dd1du64;
        let mut next = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        let alphabet: Vec<char> = "\"'\\/{}[]$#;:,\u{2028}\u{feff}é漢 SecRule REQUEST_URI \"@rx .*\" \"id:1,deny\" Include /etc/passwd %00".chars().collect();
        for _ in 0..2000 {
            let len = (next() % 80) as usize;
            let v: String = (0..len)
                .map(|_| alphabet[(next() % alphabet.len() as u64) as usize])
                .collect();
            let Ok(cond) = condition(c("path", "contains", &[&v])) else {
                continue;
            };
            let rule = WafRule {
                id: 1_000_000,
                name: v.clone(),
                matching: "all".into(),
                conditions: vec![cond],
                action: "block".into(),
            };
            let route = pyo_proto::system::WafRoute {
                hosts: vec!["a.test".into()],
                settings: WafSettings::default(),
                rules: vec![rule],
            };
            let cfg = pyo_proto::system::waf_config(&[route]);
            let back: Value = serde_json::from_str(&serde_json::to_string(&cfg).unwrap()).unwrap();
            let list = &back["lists"]["l0"];
            assert_eq!(list.as_array().unwrap().len(), 1);
            assert_eq!(list[0]["conditions"][0]["values"], json!([v]));
            assert_eq!(list[0]["name"], json!(v));
            assert!(
                back["sets"].as_object().unwrap().is_empty(),
                "no directives come from a value"
            );
        }
    }

    /// The processor's settings for its docker harness (waf/test/rules.sh),
    /// checked and built the way a deploy builds them:
    /// `PYO_WAF_CONFIG=<file> cargo test -p pyo-api waf_harness_config -- --ignored`.
    #[test]
    #[ignore]
    fn waf_harness_config() {
        let mut n = 1_000_000;
        let mut rule = |name: &str, matching: &str, conds: Vec<ConditionInput>, action: &str| {
            let (name, conditions) = check_rule(name, matching, conds, action).unwrap();
            n += 1;
            WafRule {
                id: n,
                name,
                matching: matching.into(),
                conditions,
                action: action.into(),
            }
        };
        let hdr = |name: &str, v: &str| ConditionInput {
            name: name.into(),
            ..c("header", "equals", &[v])
        };
        // The service's rules, in order, as for_service lists them.
        let service = vec![
            rule(
                "Allow /order",
                "all",
                vec![c("path", "startsWith", &["/order"])],
                "allow",
            ),
            rule(
                "No wp-admin",
                "all",
                vec![c("path", "startsWith", &["/wp-admin"])],
                "block",
            ),
            rule(
                "Office",
                "all",
                vec![c("ip", "in", &["198.51.100.7"])],
                "allow",
            ),
            rule(
                "Skip OWASP on /skip",
                "all",
                vec![c("path", "startsWith", &["/skip"])],
                "skipOwasp",
            ),
            rule(
                "POST to /and",
                "all",
                vec![
                    c("method", "equals", &["POST"]),
                    c("path", "equals", &["/and"]),
                ],
                "block",
            ),
            rule(
                "Bad header or bot",
                "any",
                vec![hdr("X-Bad", "1"), c("userAgent", "contains", &["badbot"])],
                "block",
            ),
            rule(
                "US on /geo",
                "all",
                vec![
                    c("country", "in", &["US"]),
                    c("path", "startsWith", &["/geo"]),
                ],
                "block",
            ),
            rule(
                "Log /audit",
                "all",
                vec![c("path", "startsWith", &["/audit"])],
                "log",
            ),
        ];
        let settings = own_settings(&json!({ "mode": "block", "level": 1 }));
        let routes = [pyo_proto::system::WafRoute {
            hosts: vec!["rules.test".into()],
            settings,
            rules: service,
        }];
        let cfg = pyo_proto::system::waf_config(&routes);
        std::fs::write(
            std::env::var("PYO_WAF_CONFIG").unwrap(),
            serde_json::to_string_pretty(&cfg).unwrap(),
        )
        .unwrap();
    }
}
