//! Answering a request: the site's `_redirects`, then its file (pyo-proto's
//! site.rs `resolve`), with its `_headers`. Each request is logged as one
//! JSON line on stdout with its site, which is how a site's logs are read.

use std::{sync::Arc, time::Instant};

use axum::{
    Json,
    body::Body,
    http::{
        HeaderMap, HeaderName, HeaderValue, Method, Request, StatusCode, header, request::Parts,
    },
    response::{IntoResponse, Response},
};
use pyo_proto::{
    site::{self, RedirectAction, Resolved},
    system::STATIC_HEADER,
};
use serde_json::json;

use crate::cache::{Cache, Deploy, valid_id};

pub async fn handle(cache: &Cache, req: Request<Body>) -> Response {
    // The body isn't read (and isn't Sync): only the head is kept.
    let (req, _) = req.into_parts();
    let Some(target) = req.headers.get(STATIC_HEADER).cloned() else {
        return internal(cache, &req).await;
    };
    let started = Instant::now();
    let method = req.method.clone();
    let path = req.uri.path().to_owned();
    let (site, deploy) = target
        .to_str()
        .ok()
        .and_then(|t| t.split_once('/'))
        .filter(|(s, d)| valid_id(s) && valid_id(d))
        .map(|(s, d)| (s.to_owned(), d.to_owned()))
        .unwrap_or_default();
    let res = if site.is_empty() {
        plain(StatusCode::BAD_REQUEST, "Bad request.")
    } else {
        match cache.deploy(&site, &deploy).await {
            Ok(d) => site_response(cache, &d, &req).await,
            Err(e) => {
                tracing::warn!(site = %site, deploy = %deploy, error = %format!("{e:#}"), "couldn't load a deploy");
                plain(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "This site isn't available right now. Try again in a moment.",
                )
            }
        }
    };
    println!(
        "{}",
        json!({
            "site": site, "deploy": deploy, "method": method.as_str(), "path": path,
            "status": res.status().as_u16(), "ms": started.elapsed().as_millis() as u64,
        })
    );
    res
}

/// Without a site: its readiness, and warming a deploy for the agent.
async fn internal(cache: &Cache, req: &Parts) -> Response {
    match (&req.method, req.uri.path()) {
        (&Method::GET, "/_pyo/ready") => plain(StatusCode::OK, "ok"),
        (&Method::POST, "/_pyo/warm") => {
            let q = req.uri.query().unwrap_or("");
            let get = |k: &str| {
                q.split('&')
                    .filter_map(|kv| kv.split_once('='))
                    .find(|(n, _)| *n == k)
                    .map(|(_, v)| v.to_owned())
                    .unwrap_or_default()
            };
            let (site, deploy) = (get("site"), get("deploy"));
            if !valid_id(&site) || !valid_id(&deploy) {
                return plain(StatusCode::BAD_REQUEST, "site and deploy are ids");
            }
            match cache.warm(&site, &deploy).await {
                Ok((files, fetched)) => {
                    tracing::info!(site = %site, deploy = %deploy, files, fetched, "warmed a deploy");
                    Json(json!({ "files": files, "fetched": fetched })).into_response()
                }
                Err(e) => (
                    StatusCode::BAD_GATEWAY,
                    Json(json!({ "error": format!("{e:#}") })),
                )
                    .into_response(),
            }
        }
        _ => plain(StatusCode::NOT_FOUND, "Not found."),
    }
}

fn plain(status: StatusCode, text: &'static str) -> Response {
    (
        status,
        [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        text,
    )
        .into_response()
}

fn redirect(status: u16, location: &str) -> Response {
    let status = StatusCode::from_u16(status).unwrap_or(StatusCode::MOVED_PERMANENTLY);
    match HeaderValue::from_str(location) {
        Ok(v) => (status, [(header::LOCATION, v)]).into_response(),
        Err(_) => plain(StatusCode::INTERNAL_SERVER_ERROR, "Bad redirect."),
    }
}

/// How a path is answered, by the deploy's rules and files.
#[derive(Debug, PartialEq, Eq)]
pub enum Answer {
    Redirect(u16, String),
    File(StatusCode, String),
    NotFound,
    BadRequest,
}

pub fn answer(d: &Deploy, raw_path: &str, query: &str) -> Answer {
    let Some(clean) = site::clean_path(raw_path) else {
        return Answer::BadRequest;
    };
    let has = |p: &str| d.has(p);
    let rule_path = format!("/{clean}");
    let exists = |p: &str| {
        site::clean_path(p)
            .is_some_and(|c| matches!(site::resolve(&c, false, has), Resolved::File(_)))
    };
    match site::redirect_for(&d.redirects, &rule_path, query, exists) {
        Some(RedirectAction::Redirect(s, to)) => return Answer::Redirect(s, to),
        Some(RedirectAction::Rewrite(s, to)) => {
            if let Some(c) = site::clean_path(&to)
                && let Resolved::File(f) = site::resolve(&c, false, has)
            {
                let status = if s == 404 {
                    StatusCode::NOT_FOUND
                } else {
                    StatusCode::OK
                };
                return Answer::File(status, f);
            }
        }
        None => {}
    }
    match site::resolve(&clean, d.index_fallback, has) {
        Resolved::File(f) => Answer::File(StatusCode::OK, f),
        Resolved::Slash => {
            let mut to = format!("{}/", raw_path.split('?').next().unwrap_or(raw_path));
            if !query.is_empty() {
                to = format!("{to}?{query}");
            }
            Answer::Redirect(301, to)
        }
        Resolved::NotFound(Some(f)) => Answer::File(StatusCode::NOT_FOUND, f),
        Resolved::NotFound(None) => Answer::NotFound,
    }
}

async fn site_response(cache: &Cache, d: &Arc<Deploy>, req: &Parts) -> Response {
    if !matches!(req.method, Method::GET | Method::HEAD) {
        return (
            StatusCode::METHOD_NOT_ALLOWED,
            [(header::ALLOW, "GET, HEAD")],
            "Method not allowed.",
        )
            .into_response();
    }
    let raw = req.uri.path();
    let query = req.uri.query().unwrap_or("");
    let (status, file) = match answer(d, raw, query) {
        Answer::Redirect(s, to) => return redirect(s, &to),
        Answer::BadRequest => return plain(StatusCode::BAD_REQUEST, "Bad request."),
        Answer::NotFound => return plain(StatusCode::NOT_FOUND, "Not found."),
        Answer::File(s, f) => (s, f),
    };
    let f = &d.files[&file];
    let etag = format!("\"{}\"", &f.sha256[..32]);
    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static(site::content_type(&file)),
    );
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("public, max-age=0, must-revalidate"),
    );
    if let Ok(v) = HeaderValue::from_str(&etag) {
        headers.insert(header::ETAG, v);
    }
    let rule_path = format!("/{}", site::clean_path(raw).unwrap_or_default());
    for (k, v) in site::headers_for(&d.headers, &rule_path) {
        if let (Ok(k), Ok(v)) = (
            HeaderName::from_bytes(k.as_bytes()),
            HeaderValue::from_str(&v),
        ) {
            headers.insert(k, v);
        }
    }
    let matches_etag = req
        .headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.split(',').any(|t| t.trim() == etag || t.trim() == "*"));
    if status == StatusCode::OK && matches_etag {
        headers.remove(header::CONTENT_TYPE);
        return (StatusCode::NOT_MODIFIED, headers).into_response();
    }
    let path = match cache.blob(d, f).await {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(site = %d.site, deploy = %d.deploy, error = %format!("{e:#}"), "couldn't get a file");
            return plain(
                StatusCode::SERVICE_UNAVAILABLE,
                "This page isn't available right now. Try again in a moment.",
            );
        }
    };
    headers.insert(header::CONTENT_LENGTH, HeaderValue::from(f.size));
    if req.method == Method::HEAD {
        return (status, headers).into_response();
    }
    match tokio::fs::File::open(&path).await {
        Ok(file) => (
            status,
            headers,
            Body::from_stream(tokio_util::io::ReaderStream::new(file)),
        )
            .into_response(),
        Err(_) => plain(
            StatusCode::SERVICE_UNAVAILABLE,
            "This page isn't available right now. Try again in a moment.",
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pyo_proto::site::SiteFile;

    fn deploy(files: &[&str], fallback: bool, redirects: &str) -> Deploy {
        Deploy::for_test(
            files
                .iter()
                .map(|p| SiteFile {
                    path: (*p).into(),
                    size: 1,
                    sha256: "0".repeat(64),
                })
                .collect(),
            fallback,
            site::parse_redirects(redirects).rules,
        )
    }

    #[test]
    fn requests_find_their_file_after_the_rules() {
        let d = deploy(
            &[
                "index.html",
                "about.html",
                "docs/index.html",
                "404.html",
                "_redirects",
                "app/index.html",
            ],
            false,
            "/old /about\n/app/* /app/index.html 200\n",
        );
        assert_eq!(
            answer(&d, "/", ""),
            Answer::File(StatusCode::OK, "index.html".into())
        );
        assert_eq!(
            answer(&d, "/about", ""),
            Answer::File(StatusCode::OK, "about.html".into())
        );
        assert_eq!(
            answer(&d, "/docs", "a=1"),
            Answer::Redirect(301, "/docs/?a=1".into())
        );
        assert_eq!(
            answer(&d, "/old", ""),
            Answer::Redirect(301, "/about".into())
        );
        assert_eq!(
            answer(&d, "/app/settings/x", ""),
            Answer::File(StatusCode::OK, "app/index.html".into())
        );
        assert_eq!(
            answer(&d, "/nope", ""),
            Answer::File(StatusCode::NOT_FOUND, "404.html".into())
        );
        // The rules files aren't served.
        assert_eq!(
            answer(&d, "/_redirects", ""),
            Answer::File(StatusCode::NOT_FOUND, "404.html".into())
        );
        assert_eq!(answer(&d, "/%zz", ""), Answer::BadRequest);
        let spa = deploy(&["index.html", "a.js"], true, "");
        assert_eq!(
            answer(&spa, "/route/x", ""),
            Answer::File(StatusCode::OK, "index.html".into())
        );
        assert_eq!(
            answer(&spa, "/a.js", ""),
            Answer::File(StatusCode::OK, "a.js".into())
        );
        let bare = deploy(&["a.css"], false, "");
        assert_eq!(answer(&bare, "/", ""), Answer::NotFound);
    }
}
