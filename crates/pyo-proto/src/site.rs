//! Pyo Static: a static build's output, served as files by `pyo-static`
//! (one per cluster) instead of a pod per site.
//!
//! The builder uploads a static build's output folder; each deploy is its
//! own folder (`sites/<service>/<deploy>/` in the team's bucket), so a
//! deploy or a rollback only moves the site's pointer. The pointer rides on
//! the site's routes: the gateway sets [`crate::system::STATIC_HEADER`] to
//! `<service>/<deploy>` on every request it sends to `pyo-static`.
//!
//! This module has what the builder, the API and `pyo-static` share: the
//! deploy's manifest, how a request path finds its file, and the
//! `_headers` and `_redirects` files (the subset of Netlify's and
//! Cloudflare Pages' syntax that Pyo supports, see the docs).

use serde::{Deserialize, Serialize};

/// What a static deploy serves, on a [`crate::DeploySpec`]: the folder of
/// this deployment.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SiteSpec {
    /// The deployment whose folder is served.
    pub deploy: String,
    /// An unknown path gets `index.html` (a single-page app), not a 404.
    #[serde(default)]
    pub index_fallback: bool,
}

/// A deploy's files, as uploaded.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SiteManifest {
    pub files: Vec<SiteFile>,
    #[serde(default)]
    pub index_fallback: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SiteFile {
    /// Relative to the folder, `/`-separated, no leading `/`.
    pub path: String,
    pub size: u64,
    /// Hex SHA-256 of its content.
    pub sha256: String,
}

/// What a static build reports once its folder is uploaded.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SiteUpload {
    pub files: u64,
    pub bytes: u64,
    #[serde(default)]
    pub index_fallback: bool,
}

/// The most files a deploy may have.
pub const MAX_FILES: usize = 20_000;
/// The most bytes a deploy may have.
pub const MAX_BYTES: u64 = 1024 * 1024 * 1024;
/// The largest file: under Cloudflare's 100 MB a request, so a file
/// goes up in one request on Pyo Cloud and behind a Cloudflare proxy.
pub const MAX_FILE_BYTES: u64 = 90 * 1024 * 1024;
/// The most rules each of `_headers` and `_redirects` may have.
pub const MAX_RULES: usize = 2000;

/// A path inside a deploy's folder: relative, `/`-separated, with no empty,
/// `.` or `..` segments, no backslashes or control characters, and not too
/// long.
pub fn valid_path(p: &str) -> bool {
    !p.is_empty()
        && p.len() <= 1024
        && !p.starts_with('/')
        && !p.contains('\\')
        && !p.chars().any(char::is_control)
        && p.split('/').all(|s| !s.is_empty() && s != "." && s != "..")
}

/// A request path as it is in the folder: percent-decoded, without its
/// query, leading `/` and `.`/`..` segments resolved (never above the
/// root). None for a path that can't be one.
pub fn clean_path(raw: &str) -> Option<String> {
    let raw = raw.split(['?', '#']).next().unwrap_or("");
    let decoded = percent_decode(raw)?;
    if decoded.contains('\0') || decoded.contains('\\') {
        return None;
    }
    let mut out: Vec<&str> = vec![];
    for s in decoded.split('/') {
        match s {
            "" | "." => {}
            ".." => {
                out.pop();
            }
            s => out.push(s),
        }
    }
    let mut p = out.join("/");
    if decoded.ends_with('/') && !p.is_empty() {
        p.push('/');
    }
    Some(p)
}

fn percent_decode(s: &str) -> Option<String> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            let h = std::str::from_utf8(b.get(i + 1..i + 3)?).ok()?;
            out.push(u8::from_str_radix(h, 16).ok()?);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// How a request's path is answered, given which files the deploy has.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolved {
    /// This file, with 200.
    File(String),
    /// Add the trailing slash (a folder with an index.html).
    Slash,
    /// This file, with 404 (`404.html`), or a plain 404 when None.
    NotFound(Option<String>),
}

/// The file for a cleaned path ([`clean_path`]): the file itself, then
/// `<path>.html`, then `<path>/index.html` (a redirect adds the slash); a
/// path ending in `/` is its `index.html`. Not found: `index.html` for a
/// single-page app, else `404.html` when there is one.
pub fn resolve(path: &str, index_fallback: bool, has: impl Fn(&str) -> bool) -> Resolved {
    if path.is_empty() || path.ends_with('/') {
        let index = format!("{path}index.html");
        if has(&index) {
            return Resolved::File(index);
        }
    } else {
        if has(path) {
            return Resolved::File(path.to_owned());
        }
        let html = format!("{path}.html");
        if has(&html) {
            return Resolved::File(html);
        }
        if has(&format!("{path}/index.html")) {
            return Resolved::Slash;
        }
    }
    if index_fallback && has("index.html") {
        return Resolved::File("index.html".into());
    }
    Resolved::NotFound(has("404.html").then(|| "404.html".to_owned()))
}

/// A file's Content-Type, by its extension.
pub fn content_type(path: &str) -> &'static str {
    let ext = path
        .rsplit('/')
        .next()
        .and_then(|n| n.rsplit_once('.'))
        .map(|(_, e)| e.to_ascii_lowercase())
        .unwrap_or_default();
    match ext.as_str() {
        "html" | "htm" => "text/html; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "js" | "mjs" | "cjs" => "text/javascript; charset=utf-8",
        "json" | "map" => "application/json",
        "webmanifest" => "application/manifest+json",
        "xml" => "application/xml",
        "rss" => "application/rss+xml",
        "atom" => "application/atom+xml",
        "txt" | "md" => "text/plain; charset=utf-8",
        "csv" => "text/csv; charset=utf-8",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "avif" => "image/avif",
        "ico" => "image/x-icon",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "ttf" => "font/ttf",
        "otf" => "font/otf",
        "wasm" => "application/wasm",
        "pdf" => "application/pdf",
        "mp4" => "video/mp4",
        "webm" => "video/webm",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "zip" => "application/zip",
        _ => "application/octet-stream",
    }
}

// ── Patterns ───────────────────────────────────────────────────────────────

/// Matches a rule's path pattern against a request path (both starting
/// with `/`). `:name` matches one segment, `*` the rest of the path (the
/// splat). The captures come back, the splat as `splat`.
pub fn match_pattern(pattern: &str, path: &str) -> Option<Vec<(String, String)>> {
    let mut caps = vec![];
    if go(pattern.as_bytes(), path.as_bytes(), &mut caps) {
        Some(caps)
    } else {
        None
    }
}

fn go(p: &[u8], s: &[u8], caps: &mut Vec<(String, String)>) -> bool {
    match p.first() {
        None => s.is_empty(),
        Some(b'*') => {
            // Greedy, but backtracks for what follows.
            for i in (0..=s.len()).rev() {
                let n = caps.len();
                caps.push((
                    "splat".into(),
                    String::from_utf8_lossy(&s[..i]).into_owned(),
                ));
                if go(&p[1..], &s[i..], caps) {
                    return true;
                }
                caps.truncate(n);
            }
            false
        }
        Some(b':')
            if p.get(1)
                .is_some_and(|c| c.is_ascii_alphabetic() || *c == b'_') =>
        {
            let end = p[1..]
                .iter()
                .position(|c| !(c.is_ascii_alphanumeric() || *c == b'_'))
                .map_or(p.len(), |i| i + 1);
            let name = String::from_utf8_lossy(&p[1..end]).into_owned();
            let seg = s.iter().position(|c| *c == b'/').unwrap_or(s.len());
            if seg == 0 {
                return false;
            }
            for i in (1..=seg).rev() {
                let n = caps.len();
                caps.push((name.clone(), String::from_utf8_lossy(&s[..i]).into_owned()));
                if go(&p[end..], &s[i..], caps) {
                    return true;
                }
                caps.truncate(n);
            }
            false
        }
        Some(c) => s.first() == Some(c) && go(&p[1..], &s[1..], caps),
    }
}

/// A rule's target with the captures put in (`:splat`, `:name`).
pub fn substitute(to: &str, caps: &[(String, String)]) -> String {
    let mut out = String::with_capacity(to.len());
    let b = to.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b':'
            && b.get(i + 1)
                .is_some_and(|c| c.is_ascii_alphabetic() || *c == b'_')
        {
            let end = b[i + 1..]
                .iter()
                .position(|c| !(c.is_ascii_alphanumeric() || *c == b'_'))
                .map_or(b.len(), |j| i + 1 + j);
            let name = &to[i + 1..end];
            if let Some((_, v)) = caps.iter().find(|(k, _)| k == name) {
                out.push_str(v);
                i = end;
                continue;
            }
        }
        out.push(b[i] as char);
        i += 1;
    }
    out
}

/// A pattern Pyo accepts: a path from the root, no host.
fn valid_pattern(p: &str) -> bool {
    p.starts_with('/') && p.len() <= 1024 && !p.chars().any(char::is_control)
}

// ── _redirects ─────────────────────────────────────────────────────────────

/// One line of `_redirects`: `from to [status][!]`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Redirect {
    pub from: String,
    pub to: String,
    /// 301 (the default), 302, 303, 307 or 308 redirect; 200 serves `to`
    /// instead (a rewrite); 404 serves `to` with 404.
    pub status: u16,
}

/// Rules, and a warning per line left out (with its line number).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Parsed<T> {
    pub rules: Vec<T>,
    pub warnings: Vec<String>,
}

const REDIRECT_STATUSES: [u16; 7] = [200, 301, 302, 303, 307, 308, 404];

pub fn parse_redirects(text: &str) -> Parsed<Redirect> {
    let mut out = Parsed::default();
    for (n, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let n = n + 1;
        if out.rules.len() >= MAX_RULES {
            out.warnings.push(format!(
                "_redirects: only the first {MAX_RULES} rules are used"
            ));
            break;
        }
        let words: Vec<&str> = line.split_whitespace().collect();
        if words.len() < 2 {
            out.warnings.push(format!(
                "_redirects line {n}: a rule needs a path and a target"
            ));
            continue;
        }
        let (from, to) = (words[0], words[1]);
        if !valid_pattern(from) {
            out.warnings.push(format!(
                "_redirects line {n}: {from} isn't a path (it has to start with /)"
            ));
            continue;
        }
        let status = match words.get(2) {
            None => 301,
            Some(s) => match s.trim_end_matches('!').parse::<u16>() {
                Ok(s) if REDIRECT_STATUSES.contains(&s) => s,
                _ => {
                    out.warnings.push(format!(
                        "_redirects line {n}: {s} isn't a status Pyo supports (200, 301, 302, 303, 307, 308 or 404)"
                    ));
                    continue;
                }
            },
        };
        if words.len() > 3 {
            out.warnings.push(format!(
                "_redirects line {n}: conditions and other options aren't supported, so the rule is left out"
            ));
            continue;
        }
        let external = to.starts_with("https://") || to.starts_with("http://");
        if !external && !to.starts_with('/') {
            out.warnings.push(format!(
                "_redirects line {n}: {to} has to be a path or an http(s) address"
            ));
            continue;
        }
        if external && matches!(status, 200 | 404) {
            out.warnings.push(format!(
                "_redirects line {n}: a {status} rule serves a file of the site, so its target has to be a path"
            ));
            continue;
        }
        out.rules.push(Redirect {
            from: from.to_owned(),
            to: to.to_owned(),
            status,
        });
    }
    out
}

/// What `_redirects` does with a request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RedirectAction {
    /// Answer with this status and Location.
    Redirect(u16, String),
    /// Serve this path of the site instead (status 200 or 404).
    Rewrite(u16, String),
}

/// The first rule for this request path (starting with `/`). A redirect
/// applies before any file; a rewrite (200) only where no file is
/// (`file_exists`), as on Netlify. The request's query goes along when the
/// target has none.
pub fn redirect_for(
    rules: &[Redirect],
    path: &str,
    query: &str,
    file_exists: impl Fn(&str) -> bool,
) -> Option<RedirectAction> {
    for r in rules {
        // "/old" also matches "/old/".
        let trimmed = path.trim_end_matches('/');
        let Some(caps) = match_pattern(&r.from, path).or_else(|| {
            (path.len() > 1 && trimmed.len() < path.len())
                .then(|| match_pattern(&r.from, trimmed))
                .flatten()
        }) else {
            continue;
        };
        let mut to = substitute(&r.to, &caps);
        match r.status {
            200 | 404 => {
                if r.status == 200 && file_exists(path) {
                    continue;
                }
                return Some(RedirectAction::Rewrite(r.status, to));
            }
            s => {
                // A rule onto the same path would loop.
                if to == path {
                    continue;
                }
                if !query.is_empty() && !to.contains('?') {
                    to = format!("{to}?{query}");
                }
                return Some(RedirectAction::Redirect(s, to));
            }
        }
    }
    None
}

// ── _headers ───────────────────────────────────────────────────────────────

/// A block of `_headers`: a path pattern, then the headers its responses
/// get (indented `Name: value` lines), or lose (`! Name`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HeaderRule {
    pub pattern: String,
    pub set: Vec<(String, String)>,
    pub remove: Vec<String>,
}

/// Headers `_headers` can't set: the server's own.
const RESERVED_HEADERS: [&str; 8] = [
    "content-length",
    "transfer-encoding",
    "connection",
    "keep-alive",
    "upgrade",
    "trailer",
    "te",
    "content-encoding",
];

fn valid_header_name(n: &str) -> bool {
    !n.is_empty()
        && n.len() <= 256
        && n.bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&c))
}

pub fn parse_headers(text: &str) -> Parsed<HeaderRule> {
    let mut out: Parsed<HeaderRule> = Parsed::default();
    let mut current: Option<HeaderRule> = None;
    let mut full = false;
    for (n, raw) in text.lines().enumerate() {
        let n = n + 1;
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let indented = raw.starts_with([' ', '\t']);
        if !indented {
            if let Some(c) = current.take() {
                out.rules.push(c);
            }
            if out.rules.len() >= MAX_RULES {
                if !full {
                    out.warnings.push(format!(
                        "_headers: only the first {MAX_RULES} rules are used"
                    ));
                    full = true;
                }
                continue;
            }
            if !valid_pattern(line) {
                out.warnings.push(format!(
                    "_headers line {n}: {line} isn't a path (it has to start with /)"
                ));
                continue;
            }
            current = Some(HeaderRule {
                pattern: line.to_owned(),
                ..Default::default()
            });
            continue;
        }
        let Some(rule) = current.as_mut() else {
            if !full {
                out.warnings
                    .push(format!("_headers line {n}: a header has to follow a path"));
            }
            continue;
        };
        if let Some(name) = line.strip_prefix('!') {
            let name = name.trim();
            if valid_header_name(name) {
                rule.remove.push(name.to_ascii_lowercase());
            } else {
                out.warnings
                    .push(format!("_headers line {n}: {name} isn't a header name"));
            }
            continue;
        }
        let Some((name, value)) = line.split_once(':') else {
            out.warnings.push(format!(
                "_headers line {n}: a header is written Name: value"
            ));
            continue;
        };
        let (name, value) = (name.trim(), value.trim());
        if !valid_header_name(name) || value.chars().any(|c| c.is_control() && c != '\t') {
            out.warnings.push(format!(
                "_headers line {n}: {name} isn't a header Pyo can send"
            ));
            continue;
        }
        if RESERVED_HEADERS.contains(&name.to_ascii_lowercase().as_str()) {
            out.warnings
                .push(format!("_headers line {n}: {name} is set by the server"));
            continue;
        }
        rule.set.push((name.to_owned(), value.to_owned()));
    }
    if let Some(c) = current {
        out.rules.push(c);
    }
    out
}

/// The headers a response to this path gets, from every matching block in
/// order. A header set by several blocks gets each value, joined with ", "
/// (as Cloudflare Pages does); `! Name` takes one away.
pub fn headers_for(rules: &[HeaderRule], path: &str) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = vec![];
    for r in rules {
        let Some(caps) = match_pattern(&r.pattern, path) else {
            continue;
        };
        for (k, v) in &r.set {
            let v = substitute(v, &caps);
            match out.iter_mut().find(|(n, _)| n.eq_ignore_ascii_case(k)) {
                Some((_, old)) => {
                    old.push_str(", ");
                    old.push_str(&v);
                }
                None => out.push((k.clone(), v)),
            }
        }
        for k in &r.remove {
            out.retain(|(n, _)| !n.eq_ignore_ascii_case(k));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A file goes up in one request: Cloudflare takes at most 100 MB
    /// (decimal) a request, with room left for the request's headers.
    #[test]
    fn a_file_fits_one_cloudflare_request() {
        assert_eq!(MAX_FILE_BYTES, 90 * 1024 * 1024);
        assert!(MAX_FILE_BYTES < 100_000_000 - 1_000_000);
    }

    #[test]
    fn paths_stay_in_the_folder() {
        assert!(valid_path("index.html"));
        assert!(valid_path("a/b.css"));
        for p in ["", "/a", "a//b", "../x", "a/./b", "a\\b", "a/\n"] {
            assert!(!valid_path(p), "{p:?}");
        }
        assert_eq!(clean_path("/").unwrap(), "");
        assert_eq!(clean_path("/a/b/").unwrap(), "a/b/");
        assert_eq!(clean_path("/a/../../etc/passwd").unwrap(), "etc/passwd");
        assert_eq!(clean_path("/caf%C3%A9.html?x=1").unwrap(), "café.html");
        assert_eq!(clean_path("/%2e%2e/x").unwrap(), "x");
        assert!(clean_path("/a%00").is_none());
        assert!(clean_path("/a%zz").is_none());
    }

    #[test]
    fn a_path_finds_its_file() {
        let files = [
            "index.html",
            "about.html",
            "docs/index.html",
            "a.css",
            "404.html",
        ];
        let has = |p: &str| files.contains(&p);
        assert_eq!(resolve("", false, has), Resolved::File("index.html".into()));
        assert_eq!(resolve("a.css", false, has), Resolved::File("a.css".into()));
        assert_eq!(
            resolve("about", false, has),
            Resolved::File("about.html".into())
        );
        assert_eq!(resolve("docs", false, has), Resolved::Slash);
        assert_eq!(
            resolve("docs/", false, has),
            Resolved::File("docs/index.html".into())
        );
        assert_eq!(
            resolve("nope", false, has),
            Resolved::NotFound(Some("404.html".into()))
        );
        assert_eq!(
            resolve("nope", true, has),
            Resolved::File("index.html".into())
        );
        assert_eq!(resolve("x", false, |_| false), Resolved::NotFound(None));
    }

    #[test]
    fn patterns_capture_segments_and_the_rest() {
        assert_eq!(match_pattern("/a", "/a"), Some(vec![]));
        assert_eq!(match_pattern("/a", "/b"), None);
        assert_eq!(
            match_pattern("/blog/*", "/blog/2024/post"),
            Some(vec![("splat".into(), "2024/post".into())])
        );
        assert_eq!(
            match_pattern("/u/:id/edit", "/u/42/edit"),
            Some(vec![("id".into(), "42".into())])
        );
        assert_eq!(match_pattern("/u/:id", "/u/"), None);
        assert_eq!(match_pattern("/u/:id", "/u/1/2"), None);
        assert_eq!(
            match_pattern("/*.js", "/x/app.js"),
            Some(vec![("splat".into(), "x/app".into())])
        );
        assert_eq!(
            substitute(
                "/new/:splat?id=:id",
                &[("splat".into(), "a/b".into()), ("id".into(), "7".into())]
            ),
            "/new/a/b?id=7"
        );
        assert_eq!(substitute("/keep/:unknown", &[]), "/keep/:unknown");
    }

    #[test]
    fn redirects_parse_with_warnings() {
        let p = parse_redirects(
            "# comment\n/old /new\n/blog/* /posts/:splat 302\n/app/* /index.html 200\n\
             /x https://example.com/y 308!\nnot-a-path /y\n/a /b 418\n/c /d 301 Country=us\n/e\n/f relative\n/g https://e.com 200\n",
        );
        assert_eq!(
            p.rules,
            vec![
                Redirect {
                    from: "/old".into(),
                    to: "/new".into(),
                    status: 301
                },
                Redirect {
                    from: "/blog/*".into(),
                    to: "/posts/:splat".into(),
                    status: 302
                },
                Redirect {
                    from: "/app/*".into(),
                    to: "/index.html".into(),
                    status: 200
                },
                Redirect {
                    from: "/x".into(),
                    to: "https://example.com/y".into(),
                    status: 308
                },
            ]
        );
        assert_eq!(p.warnings.len(), 6, "{:?}", p.warnings);
        assert!(p.warnings[0].starts_with("_redirects line 6:"));
    }

    #[test]
    fn redirects_apply_in_order() {
        let rules = parse_redirects(
            "/old /new\n/blog/* /posts/:splat 302\n/app/* /index.html 200\n/gone/* /404.html 404\n/loop /loop\n",
        )
        .rules;
        let none = |_: &str| false;
        assert_eq!(
            redirect_for(&rules, "/old", "", none),
            Some(RedirectAction::Redirect(301, "/new".into()))
        );
        assert_eq!(
            redirect_for(&rules, "/old/", "a=1", none),
            Some(RedirectAction::Redirect(301, "/new?a=1".into()))
        );
        assert_eq!(
            redirect_for(&rules, "/blog/2024/x", "", none),
            Some(RedirectAction::Redirect(302, "/posts/2024/x".into()))
        );
        assert_eq!(
            redirect_for(&rules, "/app/settings", "", none),
            Some(RedirectAction::Rewrite(200, "/index.html".into()))
        );
        // A file there wins over a rewrite.
        assert_eq!(redirect_for(&rules, "/app/settings", "", |_| true), None);
        assert_eq!(
            redirect_for(&rules, "/gone/x", "", none),
            Some(RedirectAction::Rewrite(404, "/404.html".into()))
        );
        assert_eq!(redirect_for(&rules, "/loop", "", none), None);
        assert_eq!(redirect_for(&rules, "/other", "", none), None);
    }

    #[test]
    fn headers_parse_and_combine() {
        let p = parse_headers(
            "/*\n  X-Frame-Options: DENY\n  Cache-Control: no-cache\n\
             /assets/*\n  Cache-Control: public, max-age=31536000\n  ! X-Frame-Options\n\
             /files/:name\n  Content-Disposition: attachment; filename=:name\n\
               Orphan: x\nbad\n/x\n  Content-Length: 3\n  Broken\n  Bad Name: x\n",
        );
        assert_eq!(p.rules.len(), 4);
        assert_eq!(p.warnings.len(), 5, "{:?}", p.warnings);
        assert_eq!(
            headers_for(&p.rules, "/index.html"),
            vec![
                ("X-Frame-Options".into(), "DENY".into()),
                ("Cache-Control".into(), "no-cache".into()),
            ]
        );
        assert_eq!(
            headers_for(&p.rules, "/assets/app.js"),
            vec![(
                "Cache-Control".into(),
                "no-cache, public, max-age=31536000".into()
            )]
        );
        assert!(headers_for(&p.rules, "/files/report.pdf").contains(&(
            "Content-Disposition".into(),
            "attachment; filename=report.pdf".into()
        )));
    }

    #[test]
    fn content_types_by_extension() {
        assert_eq!(content_type("a/b.HTML"), "text/html; charset=utf-8");
        assert_eq!(content_type("app.mjs"), "text/javascript; charset=utf-8");
        assert_eq!(content_type("noext"), "application/octet-stream");
        assert_eq!(content_type(".well-known/x"), "application/octet-stream");
    }
}
