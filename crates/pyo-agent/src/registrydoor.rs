//! The door of the cluster's own registry (Pyo Cloud's clusters keep the
//! images they build: connect.sh `--local-registry`). The team pushes and
//! pulls its own images here with its registry tokens, by the same rules
//! as Pyo's own door (pyo-proto registry_door.rs): `<team>/<project>/<service>`
//! of its team only, and a pull token only pulls.
//!
//! Pyo never sends a token: only their SHA-256 and what each may do
//! ([`pyo_proto::Command::SyncRegistryTokens`], a signed command), kept in
//! a Secret in pyo-system so a restarted agent still has them. Until the
//! first sync every login is refused.
//!
//! It listens on [`PORT`], behind the Service `pyo-registry-door` that
//! connect.sh makes with the registry. People reach it at the cluster's
//! registry name on its tailnet, the same tailnet route Pyo's own registry
//! has. The registry itself answers only in the cluster.

use std::sync::RwLock;

use axum::{
    Router,
    body::Body,
    extract::Request,
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
    routing::any,
};
use base64::Engine;
use futures::TryStreamExt;
use k8s_openapi::api::core::v1::Secret;
use kube::api::{Patch, PatchParams};
use pyo_proto::registry_door::{self, RegistryTokens, Wants};
use serde_json::json;

/// Where the door listens (the Service's target port).
pub const PORT: u16 = 5006;
/// The cluster's registry, in the cluster.
const REGISTRY: &str = "pyo-registry.pyo-system.svc.cluster.local:5000";
/// The Secret the tokens' hashes are kept in.
const SECRET: &str = "pyo-registry-tokens";
const NS: &str = "pyo-system";
const KEY: &str = "tokens.json";

static TOKENS: RwLock<Option<RegistryTokens>> = RwLock::new(None);

fn tokens() -> Option<RegistryTokens> {
    TOKENS.read().unwrap_or_else(|e| e.into_inner()).clone()
}

fn set_tokens(t: RegistryTokens) {
    *TOKENS.write().unwrap_or_else(|e| e.into_inner()) = Some(t);
}

/// The tokens Pyo sent: used from now on, and kept.
pub async fn sync(kube: &kube::Client, t: &RegistryTokens) -> Result<String, String> {
    let api = kube::Api::<Secret>::namespaced(kube.clone(), NS);
    let s = json!({
        "apiVersion": "v1",
        "kind": "Secret",
        "metadata": { "name": SECRET, "namespace": NS, "labels": { "app.kubernetes.io/managed-by": crate::MANAGER } },
        "type": "Opaque",
        "stringData": { KEY: serde_json::to_string(t).map_err(|e| e.to_string())? },
    });
    api.patch(
        SECRET,
        &PatchParams::apply(crate::MANAGER).force(),
        &Patch::Apply(&s),
    )
    .await
    .map_err(|e| format!("couldn't keep the registry tokens: {e}"))?;
    let n = t.tokens.len();
    set_tokens(t.clone());
    Ok(format!("{n} registry token(s)"))
}

/// The kept tokens, at start.
async fn load(kube: &kube::Client) {
    let api = kube::Api::<Secret>::namespaced(kube.clone(), NS);
    let kept = match api.get_opt(SECRET).await {
        Ok(Some(s)) => s
            .data
            .unwrap_or_default()
            .get(KEY)
            .and_then(|b| serde_json::from_slice::<RegistryTokens>(&b.0).ok()),
        Ok(None) => None,
        Err(e) => {
            tracing::warn!(error = %e, "registry door: couldn't read the kept tokens");
            None
        }
    };
    if let Some(t) = kept {
        set_tokens(t);
    }
}

/// Serves the door for as long as the agent runs.
pub async fn serve(kube: kube::Client) {
    load(&kube).await;
    let app = Router::new()
        .route("/v2/", any(door))
        .route("/v2/{*rest}", any(door));
    let addr = std::net::SocketAddr::from(([0, 0, 0, 0], PORT));
    loop {
        match tokio::net::TcpListener::bind(addr).await {
            Ok(l) => {
                if let Err(e) = axum::serve(l, app.clone()).await {
                    tracing::warn!(error = %e, "the registry door stopped");
                }
            }
            Err(e) => tracing::warn!(%addr, error = %e, "the registry door can't listen"),
        }
        tokio::time::sleep(std::time::Duration::from_secs(10)).await;
    }
}

fn errors(status: StatusCode, code: &str, message: &str) -> Response {
    let mut r = (
        status,
        [(header::CONTENT_TYPE, "application/json")],
        json!({ "errors": [{ "code": code, "message": message }] }).to_string(),
    )
        .into_response();
    r.headers_mut().insert(
        "docker-distribution-api-version",
        HeaderValue::from_static("registry/2.0"),
    );
    if status == StatusCode::UNAUTHORIZED {
        r.headers_mut().insert(
            header::WWW_AUTHENTICATE,
            HeaderValue::from_static("Basic realm=\"Pyo registry\""),
        );
    }
    r
}

/// The password of a Basic login (any username).
fn password(headers: &HeaderMap) -> Option<String> {
    let v = headers
        .get(header::AUTHORIZATION)?
        .to_str()
        .ok()?
        .strip_prefix("Basic ")?;
    let decoded = String::from_utf8(
        base64::engine::general_purpose::STANDARD
            .decode(v.trim())
            .ok()?,
    )
    .ok()?;
    decoded
        .split_once(':')
        .map(|(_, p)| p.to_owned())
        .filter(|p| !p.is_empty())
}

/// What the door lets through, by the login and the path: the slug to
/// keep mounts within, or the answer instead.
#[allow(clippy::result_large_err)]
fn decide(
    tokens: Option<&RegistryTokens>,
    headers: &HeaderMap,
    method: &str,
    path: &str,
    now: i64,
) -> Result<Option<String>, Response> {
    let unauthorized = |m: &str| errors(StatusCode::UNAUTHORIZED, "UNAUTHORIZED", m);
    let Some(password) = password(headers) else {
        return Err(unauthorized("Log in with a registry token from Pyo."));
    };
    let Some(tokens) = tokens else {
        return Err(unauthorized(
            "This cluster has no registry tokens from Pyo yet.",
        ));
    };
    let Some(token) = tokens.find(&password, now) else {
        return Err(unauthorized(
            "That token isn't valid (revoked or expired?).",
        ));
    };
    let slug = &tokens.team_slug;
    match registry_door::wants(method, path) {
        Wants::Root => Ok(None),
        Wants::Repo { name, .. } if registry_door::allowed(slug, token.push, method, &name) => {
            Ok(Some(slug.clone()))
        }
        Wants::Repo { .. } => Err(errors(
            StatusCode::FORBIDDEN,
            "DENIED",
            &format!(
                "This token can only {} images named {slug}/<project>/<service>.",
                if token.push { "push and pull" } else { "pull" }
            ),
        )),
        Wants::Other => Err(errors(
            StatusCode::FORBIDDEN,
            "DENIED",
            "Not available here.",
        )),
    }
}

fn http() -> &'static reqwest::Client {
    static C: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    C.get_or_init(|| {
        reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(10))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("an HTTP client")
    })
}

async fn door(req: Request) -> Response {
    let (parts, body) = req.into_parts();
    let path = parts.uri.path().to_owned();
    let now = pyo_proto::now_ms() / 1000;
    let slug = match decide(
        tokens().as_ref(),
        &parts.headers,
        parts.method.as_str(),
        &path,
        now,
    ) {
        Ok(Some(slug)) => slug,
        Ok(None) => {
            return (
                [("docker-distribution-api-version", "registry/2.0")],
                [(header::CONTENT_TYPE, "application/json")],
                "{}",
            )
                .into_response();
        }
        Err(r) => return r,
    };
    let query = parts
        .uri
        .query()
        .map(|q| registry_door::scoped_query(q, &slug))
        .filter(|q| !q.is_empty())
        .map(|q| format!("?{q}"))
        .unwrap_or_default();
    let mut out = http().request(
        parts.method.clone(),
        format!("http://{REGISTRY}{path}{query}"),
    );
    for (k, v) in &parts.headers {
        if !matches!(
            *k,
            header::HOST | header::AUTHORIZATION | header::CONNECTION | header::TRANSFER_ENCODING
        ) {
            out = out.header(k, v);
        }
    }
    // Streamed through: a layer is never held whole.
    out = out.body(reqwest::Body::wrap_stream(
        body.into_data_stream().map_err(std::io::Error::other),
    ));
    let res = match out.send().await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(error = %e, "registry door: the registry didn't answer");
            return errors(
                StatusCode::BAD_GATEWAY,
                "UNAVAILABLE",
                "The cluster's registry didn't answer.",
            );
        }
    };
    let mut back = Response::builder().status(res.status().as_u16());
    for (k, v) in res.headers() {
        if matches!(*k, header::CONNECTION | header::TRANSFER_ENCODING) {
            continue;
        }
        // Uploads continue through the door, not at the registry's address.
        if *k == header::LOCATION
            && let Some(rest) = v
                .to_str()
                .ok()
                .and_then(|l| l.strip_prefix(&format!("http://{REGISTRY}")))
            && let Ok(v) = HeaderValue::from_str(rest)
        {
            back = back.header(k, v);
            continue;
        }
        back = back.header(k, v);
    }
    back.body(Body::from_stream(res.bytes_stream()))
        .unwrap_or_else(|_| {
            errors(
                StatusCode::BAD_GATEWAY,
                "UNAVAILABLE",
                "The cluster's registry didn't answer.",
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use pyo_proto::registry_door::RegistryToken;
    use sha2::{Digest, Sha256};

    fn login(token: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        let b = base64::engine::general_purpose::STANDARD.encode(format!("me:{token}"));
        h.insert(header::AUTHORIZATION, format!("Basic {b}").parse().unwrap());
        h
    }

    #[test]
    fn only_the_teams_tokens_and_images() {
        let hash = |t: &str| hex::encode(Sha256::digest(t.as_bytes()));
        let t = RegistryTokens {
            team_slug: "acme".into(),
            tokens: vec![
                RegistryToken {
                    sha256: hash("push-me"),
                    push: true,
                    expires_at: None,
                },
                RegistryToken {
                    sha256: hash("pull-me"),
                    push: false,
                    expires_at: Some(50),
                },
            ],
        };
        let status = |r: Result<Option<String>, Response>| match r {
            Ok(_) => StatusCode::OK,
            Err(r) => r.status(),
        };
        let path = "/v2/acme/shop/api/manifests/1";
        // Nothing synced yet, or no login: refused.
        assert_eq!(
            status(decide(None, &login("push-me"), "GET", path, 0)),
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            status(decide(Some(&t), &HeaderMap::new(), "GET", path, 0)),
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            status(decide(Some(&t), &login("nope"), "GET", path, 0)),
            StatusCode::UNAUTHORIZED
        );
        // A push token pushes the team's own.
        assert_eq!(
            decide(Some(&t), &login("push-me"), "PUT", path, 0).ok(),
            Some(Some("acme".into()))
        );
        assert_eq!(
            decide(Some(&t), &login("push-me"), "GET", "/v2/", 0).ok(),
            Some(None)
        );
        // Another team's, or the catalog: no.
        assert_eq!(
            status(decide(
                Some(&t),
                &login("push-me"),
                "GET",
                "/v2/other/shop/api/manifests/1",
                0
            )),
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            status(decide(
                Some(&t),
                &login("push-me"),
                "GET",
                "/v2/_catalog",
                0
            )),
            StatusCode::FORBIDDEN
        );
        // A pull token pulls, until it expires.
        assert_eq!(
            status(decide(Some(&t), &login("pull-me"), "GET", path, 49)),
            StatusCode::OK
        );
        assert_eq!(
            status(decide(Some(&t), &login("pull-me"), "PUT", path, 49)),
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            status(decide(Some(&t), &login("pull-me"), "GET", path, 50)),
            StatusCode::UNAUTHORIZED
        );
    }
}
