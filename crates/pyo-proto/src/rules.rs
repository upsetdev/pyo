//! A domain's rules in Pyo Gateway: its redirects and its header rules
//! (docs/gateway-routes.md). They ride on a domain entry
//! ([`crate::DomainSpec`]), are checked by the API before they're saved
//! ([`check_redirects`], [`check_headers`]), and become filters on the
//! domain's HTTPRoutes in the agent ([`redirect_rule`], [`with_headers`]).

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// Redirects one hostname can have.
pub const MAX_REDIRECTS: usize = 15;
/// Header rules per list (`set` or `remove`, response or request): the
/// Gateway API's own limit per filter.
pub const MAX_HEADERS: usize = 16;
/// The longest path a rule may have.
pub const MAX_PATH: usize = 200;
/// The longest header value.
pub const MAX_HEADER_VALUE: usize = 1024;

/// A redirect on a domain: requests under `path` go elsewhere.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Redirect {
    /// The path prefix it applies to: "/" for the whole hostname.
    #[serde(default = "root")]
    pub path: String,
    /// The hostname to send them to ("" keeps this one).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub to_host: String,
    /// The path prefix to send them to ("" keeps the path as it is).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub to_path: String,
    /// 301 (moved for good) or 302 (for now).
    #[serde(default = "moved")]
    pub status: u16,
    /// Keep the rest of the path after `path`. Off, every request goes to
    /// `to_path` itself. The query string is always kept.
    #[serde(default = "yes")]
    pub keep_path: bool,
}

fn root() -> String {
    "/".into()
}
fn moved() -> u16 {
    301
}
fn yes() -> bool {
    true
}

impl Default for Redirect {
    fn default() -> Self {
        Redirect {
            path: root(),
            to_host: String::new(),
            to_path: String::new(),
            status: moved(),
            keep_path: true,
        }
    }
}

/// A header to set.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Header {
    pub name: String,
    pub value: String,
}

/// Headers to set and to remove, on responses or on requests.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct HeaderSet {
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub set: Vec<Header>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub remove: Vec<String>,
}

impl HeaderSet {
    pub fn is_empty(&self) -> bool {
        self.set.is_empty() && self.remove.is_empty()
    }
}

/// A domain's header rules: on its responses, and on requests to its
/// service.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct HeaderRules {
    #[serde(skip_serializing_if = "HeaderSet::is_empty")]
    pub response: HeaderSet,
    #[serde(skip_serializing_if = "HeaderSet::is_empty")]
    pub request: HeaderSet,
}

impl HeaderRules {
    pub fn is_empty(&self) -> bool {
        self.response.is_empty() && self.request.is_empty()
    }
}

/// A path as rules keep it: starting with "/", without a trailing "/"
/// (except "/" itself). Refused when the Gateway API wouldn't take it.
pub fn normalize_path(p: &str) -> Result<String, String> {
    let p = p.trim();
    if p.is_empty() || p == "/" {
        return Ok("/".into());
    }
    if !p.starts_with('/') {
        return Err(format!("{p} isn't a path. Paths start with /."));
    }
    if p.len() > MAX_PATH {
        return Err(format!("Paths can have at most {MAX_PATH} characters."));
    }
    let lower = p.to_ascii_lowercase();
    let ok_chars = p
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b"-._~!$&'()*+,;=:@%/".contains(&b));
    if !ok_chars
        || p.contains('*')
        || p.contains("//")
        || p.contains("/./")
        || p.contains("/../")
        || p.ends_with("/.")
        || p.ends_with("/..")
        || lower.contains("%2f")
    {
        return Err(format!(
            "{p} isn't a path Pyo Gateway can match. Use letters, digits and - . _ ~ / only."
        ));
    }
    Ok(p.trim_end_matches('/').to_owned())
}

/// Headers Pyo Gateway sets or needs as they are: hop-by-hop headers, the
/// visitor's address, Pyo's own and the CDN's.
pub fn protected_header(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    const EXACT: &[&str] = &[
        "connection",
        "keep-alive",
        "te",
        "trailer",
        "transfer-encoding",
        "upgrade",
        "host",
        "content-length",
        "x-real-ip",
        "forwarded",
        "via",
    ];
    const PREFIX: &[&str] = &[":", "proxy-", "x-forwarded-", "x-envoy-", "cf-", "x-pyo-"];
    EXACT.contains(&n.as_str()) || PREFIX.iter().any(|p| n.starts_with(p))
}

/// An HTTP header name (a token), at most 100 characters.
fn header_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 100
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b))
}

/// A domain's header rules, before they're saved: names that are headers,
/// none of Pyo Gateway's own, values on one line, at most
/// [`MAX_HEADERS`] per list, each name once.
pub fn check_headers(h: &HeaderRules) -> Result<(), String> {
    for (side, set) in [("response", &h.response), ("request", &h.request)] {
        if set.set.len() > MAX_HEADERS || set.remove.len() > MAX_HEADERS {
            return Err(format!(
                "A domain can set and remove at most {MAX_HEADERS} {side} headers each."
            ));
        }
        let mut seen: Vec<String> = vec![];
        let names = set
            .set
            .iter()
            .map(|x| x.name.as_str())
            .chain(set.remove.iter().map(String::as_str));
        for name in names {
            let name = name.trim();
            if !header_name(name) {
                return Err(format!("{name:?} isn't a header name."));
            }
            if protected_header(name) {
                return Err(format!(
                    "{name} is set by Pyo Gateway and can't be changed."
                ));
            }
            let lower = name.to_ascii_lowercase();
            if seen.contains(&lower) {
                return Err(format!("{name} is in the {side} headers twice."));
            }
            seen.push(lower);
        }
        for x in &set.set {
            if x.value.len() > MAX_HEADER_VALUE {
                return Err(format!(
                    "{}: values can have at most {MAX_HEADER_VALUE} characters.",
                    x.name
                ));
            }
            if x.value.trim().is_empty() {
                return Err(format!("{} needs a value.", x.name));
            }
            if x.value.chars().any(|c| c.is_control()) {
                return Err(format!("{}: the value must be on one line.", x.name));
            }
        }
    }
    Ok(())
}

/// The header rules as they're kept: names trimmed, values trimmed.
pub fn tidy_headers(mut h: HeaderRules) -> HeaderRules {
    for set in [&mut h.response, &mut h.request] {
        for x in &mut set.set {
            x.name = x.name.trim().to_owned();
            x.value = x.value.trim().to_owned();
        }
        for r in &mut set.remove {
            *r = r.trim().to_owned();
        }
    }
    h
}

/// A hostname a redirect may go to: DNS labels with a dot.
fn redirect_host(h: &str) -> bool {
    h.len() <= 253
        && h.contains('.')
        && h.split('.').all(|l| {
            !l.is_empty()
                && l.len() <= 63
                && !l.starts_with('-')
                && !l.ends_with('-')
                && l.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
        })
}

/// A domain's redirects as they're kept: paths normalized, hostnames in
/// lower case. Refused when one isn't valid, a path is there twice, or one
/// redirects to itself. `routes` says whether the domain also routes to a
/// port: then a redirect of "/" would hide it, and is refused.
pub fn check_redirects(
    host: &str,
    redirects: &[Redirect],
    routes: bool,
) -> Result<Vec<Redirect>, String> {
    if redirects.len() > MAX_REDIRECTS {
        return Err(format!(
            "A domain can have at most {MAX_REDIRECTS} redirects."
        ));
    }
    let mut out: Vec<Redirect> = vec![];
    for r in redirects {
        let path = normalize_path(&r.path)?;
        let to_host = r.to_host.trim().trim_end_matches('.').to_lowercase();
        let to_path = if r.to_path.trim().is_empty() {
            String::new()
        } else {
            normalize_path(&r.to_path)?
        };
        if !to_host.is_empty() && !redirect_host(&to_host) {
            return Err(format!("{to_host} isn't a valid hostname."));
        }
        if !matches!(r.status, 301 | 302) {
            return Err("A redirect's status is 301 or 302.".into());
        }
        if to_host.is_empty() && to_path.is_empty() {
            return Err(format!(
                "The redirect of {} needs a hostname or a path to go to.",
                shown(host, &path)
            ));
        }
        let same_host = to_host.is_empty() || to_host.eq_ignore_ascii_case(host);
        let target = if to_path.is_empty() { &path } else { &to_path };
        if same_host && under(target, &path) {
            return Err(format!(
                "The redirect of {} points to itself.",
                shown(host, &path)
            ));
        }
        if routes && path == "/" {
            return Err(format!(
                "{host} routes to a port, so it can't redirect as a whole. Remove its port, or redirect a path."
            ));
        }
        if out.iter().any(|x| x.path == path) {
            return Err(format!("{} has two redirects.", shown(host, &path)));
        }
        out.push(Redirect {
            path,
            to_host,
            to_path,
            status: r.status,
            keep_path: r.keep_path,
        });
    }
    Ok(out)
}

/// Whether `path` is under the prefix `prefix`, as a `PathPrefix` match
/// sees it: whole segments only ("/api" covers "/api/x", not "/apix").
pub fn under(path: &str, prefix: &str) -> bool {
    prefix == "/" || path == prefix || path.starts_with(&format!("{prefix}/"))
}

/// "example.com/old", or "example.com" for "/".
pub fn shown(host: &str, path: &str) -> String {
    if path == "/" {
        host.to_owned()
    } else {
        format!("{host}{path}")
    }
}

/// A redirect as an HTTPRoute rule: its path, then a `RequestRedirect`
/// filter. `https` sends visitors to HTTPS (the domain has a certificate).
pub fn redirect_rule(r: &Redirect, https: bool) -> Value {
    let mut redirect = json!({ "statusCode": r.status });
    if https {
        redirect["scheme"] = json!("https");
    }
    if !r.to_host.is_empty() {
        redirect["hostname"] = json!(r.to_host);
    }
    if r.keep_path {
        if !r.to_path.is_empty() {
            redirect["path"] =
                json!({ "type": "ReplacePrefixMatch", "replacePrefixMatch": r.to_path });
        }
    } else {
        let to = if r.to_path.is_empty() {
            "/"
        } else {
            r.to_path.as_str()
        };
        redirect["path"] = json!({ "type": "ReplaceFullPath", "replaceFullPath": to });
    }
    json!({
        "matches": [{ "path": { "type": "PathPrefix", "value": r.path } }],
        "filters": [{ "type": "RequestRedirect", "requestRedirect": redirect }],
    })
}

/// A rule with the domain's header rules on it: a
/// `ResponseHeaderModifier`, and its request headers in the rule's
/// `RequestHeaderModifier` (one per rule, which Pyo's own headers may
/// already be in, such as a static site's).
pub fn with_headers(mut rule: Value, h: &HeaderRules) -> Value {
    if h.is_empty() {
        return rule;
    }
    let modifier = |s: &HeaderSet| {
        let mut m = json!({});
        if !s.set.is_empty() {
            m["set"] = json!(s.set);
        }
        if !s.remove.is_empty() {
            m["remove"] = json!(s.remove);
        }
        m
    };
    if !rule["filters"].is_array() {
        rule["filters"] = json!([]);
    }
    let Some(filters) = rule["filters"].as_array_mut() else {
        return rule;
    };
    if !h.request.is_empty() {
        let mine = modifier(&h.request);
        match filters
            .iter_mut()
            .find(|f| f["type"] == "RequestHeaderModifier")
        {
            Some(f) => {
                // Theirs first, Pyo's own after.
                for key in ["set", "remove"] {
                    if let Some(theirs) = mine[key].as_array() {
                        let ours = f["requestHeaderModifier"][key]
                            .as_array()
                            .cloned()
                            .unwrap_or_default();
                        f["requestHeaderModifier"][key] =
                            json!(theirs.iter().chain(ours.iter()).collect::<Vec<_>>());
                    }
                }
            }
            None => filters
                .push(json!({ "type": "RequestHeaderModifier", "requestHeaderModifier": mine })),
        }
    }
    if !h.response.is_empty() {
        filters.push(json!({ "type": "ResponseHeaderModifier", "responseHeaderModifier": modifier(&h.response) }));
    }
    rule
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_are_kept_one_way() {
        assert_eq!(normalize_path("").unwrap(), "/");
        assert_eq!(normalize_path("/").unwrap(), "/");
        assert_eq!(normalize_path("/api/").unwrap(), "/api");
        assert_eq!(normalize_path(" /Docs/v1 ").unwrap(), "/Docs/v1");
        for bad in [
            "api", "/a//b", "/a/../b", "/a/.", "/a*", "/a?b", "/a#b", "/a%2Fb", "/a b",
        ] {
            assert!(normalize_path(bad).is_err(), "{bad}");
        }
        assert!(normalize_path(&format!("/{}", "a".repeat(MAX_PATH))).is_err());
    }

    #[test]
    fn pyo_gateways_own_headers_cant_be_changed() {
        let rules = |name: &str| HeaderRules {
            response: HeaderSet {
                remove: vec![name.into()],
                ..Default::default()
            },
            ..Default::default()
        };
        for name in [
            "X-Forwarded-For",
            "x-real-ip",
            "Connection",
            "Transfer-Encoding",
            "X-Pyo-Site",
            "CF-Connecting-IP",
            "x-envoy-upstream-service-time",
            "Host",
            ":path",
        ] {
            assert!(check_headers(&rules(name)).is_err(), "{name}");
        }
        assert!(check_headers(&rules("X-Powered-By")).is_ok());
        let set = |name: &str, value: &str| HeaderRules {
            response: HeaderSet {
                set: vec![Header {
                    name: name.into(),
                    value: value.into(),
                }],
                ..Default::default()
            },
            ..Default::default()
        };
        assert!(check_headers(&set("Strict-Transport-Security", "max-age=31536000")).is_ok());
        assert_eq!(
            check_headers(&set("X-Frame-Options", "DENY\r\nX-Evil: 1")).unwrap_err(),
            "X-Frame-Options: the value must be on one line."
        );
        assert!(check_headers(&set("Bad Name", "x")).is_err());
        assert!(check_headers(&set("X-A", "  ")).is_err());
        assert!(check_headers(&set("X-A", &"v".repeat(MAX_HEADER_VALUE + 1))).is_err());
        // The same name twice in one list, in any case.
        let twice = HeaderRules {
            response: HeaderSet {
                set: vec![Header {
                    name: "X-A".into(),
                    value: "1".into(),
                }],
                remove: vec!["x-a".into()],
            },
            ..Default::default()
        };
        assert_eq!(
            check_headers(&twice).unwrap_err(),
            "x-a is in the response headers twice."
        );
        let many = HeaderRules {
            request: HeaderSet {
                remove: (0..=MAX_HEADERS).map(|i| format!("X-{i}")).collect(),
                ..Default::default()
            },
            ..Default::default()
        };
        assert!(check_headers(&many).is_err());
    }

    #[test]
    fn redirects_are_checked_and_tidied() {
        let r = |path: &str, host: &str, to: &str| Redirect {
            path: path.into(),
            to_host: host.into(),
            to_path: to.into(),
            ..Default::default()
        };
        let www = check_redirects("www.example.com", &[r("", "Example.COM.", "")], false).unwrap();
        assert_eq!(www, vec![r("/", "example.com", "")]);
        let moved = check_redirects("example.com", &[r("/old/", "", "/new/")], true).unwrap();
        assert_eq!(moved, vec![r("/old", "", "/new")]);
        // To itself.
        assert!(check_redirects("example.com", &[r("/", "example.com", "")], false).is_err());
        assert!(check_redirects("example.com", &[r("/a", "", "/a")], false).is_err());
        assert!(check_redirects("example.com", &[r("/a", "", "/a/b")], false).is_err());
        assert!(check_redirects("example.com", &[r("/", "", "/home")], false).is_err());
        assert!(check_redirects("example.com", &[r("/a", "", "/ab")], false).is_ok());
        // Nowhere to go.
        assert!(check_redirects("example.com", &[r("/a", "", "")], false).is_err());
        // The whole domain while it routes to a port.
        assert!(check_redirects("example.com", &[r("/", "other.com", "")], true).is_err());
        // A path twice.
        assert!(
            check_redirects(
                "example.com",
                &[r("/a", "", "/b"), r("/a/", "", "/c")],
                false
            )
            .is_err()
        );
        let mut bad = r("/a", "", "/b");
        bad.status = 307;
        assert!(check_redirects("example.com", &[bad], false).is_err());
        assert!(check_redirects("example.com", &[r("/a", "not a host", "")], false).is_err());
        let many: Vec<Redirect> = (0..=MAX_REDIRECTS)
            .map(|i| r(&format!("/{i}"), "", "/x"))
            .collect();
        assert!(check_redirects("example.com", &many, false).is_err());
    }

    #[test]
    fn redirects_become_request_redirect_filters() {
        let www = Redirect {
            to_host: "example.com".into(),
            ..Default::default()
        };
        assert_eq!(
            redirect_rule(&www, true),
            json!({
                "matches": [{ "path": { "type": "PathPrefix", "value": "/" } }],
                "filters": [{ "type": "RequestRedirect", "requestRedirect": { "statusCode": 301, "scheme": "https", "hostname": "example.com" } }],
            })
        );
        let moved = Redirect {
            path: "/old".into(),
            to_path: "/new".into(),
            status: 302,
            ..Default::default()
        };
        assert_eq!(
            redirect_rule(&moved, false)["filters"][0]["requestRedirect"],
            json!({ "statusCode": 302, "path": { "type": "ReplacePrefixMatch", "replacePrefixMatch": "/new" } })
        );
        let all = Redirect {
            path: "/old".into(),
            to_path: "/new".into(),
            keep_path: false,
            ..Default::default()
        };
        assert_eq!(
            redirect_rule(&all, false)["filters"][0]["requestRedirect"]["path"],
            json!({ "type": "ReplaceFullPath", "replaceFullPath": "/new" })
        );
        let home = Redirect {
            path: "/old".into(),
            to_host: "example.org".into(),
            keep_path: false,
            ..Default::default()
        };
        assert_eq!(
            redirect_rule(&home, false)["filters"][0]["requestRedirect"]["path"],
            json!({ "type": "ReplaceFullPath", "replaceFullPath": "/" })
        );
    }

    #[test]
    fn header_rules_join_pyos_own_filters() {
        let h = HeaderRules {
            response: HeaderSet {
                set: vec![Header {
                    name: "Strict-Transport-Security".into(),
                    value: "max-age=31536000".into(),
                }],
                remove: vec!["X-Powered-By".into()],
            },
            request: HeaderSet {
                set: vec![Header {
                    name: "X-Region".into(),
                    value: "eu".into(),
                }],
                ..Default::default()
            },
        };
        // A static site's rule already sets Pyo's header: theirs go first.
        let site = json!({ "backendRefs": [], "filters": [{ "type": "RequestHeaderModifier", "requestHeaderModifier": { "set": [{ "name": "x-pyo-site", "value": "svc/dpl" }] } }] });
        let out = with_headers(site, &h);
        assert_eq!(
            out["filters"],
            json!([
                { "type": "RequestHeaderModifier", "requestHeaderModifier": { "set": [{ "name": "X-Region", "value": "eu" }, { "name": "x-pyo-site", "value": "svc/dpl" }] } },
                { "type": "ResponseHeaderModifier", "responseHeaderModifier": { "set": [{ "name": "Strict-Transport-Security", "value": "max-age=31536000" }], "remove": ["X-Powered-By"] } },
            ])
        );
        // A plain rule gets filters of its own.
        let plain = with_headers(json!({ "backendRefs": [] }), &h);
        assert_eq!(plain["filters"].as_array().map(Vec::len), Some(2));
        // No rules: the rule as it was.
        assert_eq!(
            with_headers(json!({ "backendRefs": [] }), &HeaderRules::default()),
            json!({ "backendRefs": [] })
        );
    }
}
