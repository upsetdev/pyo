//! The front door of Pyo's registry, the only way to it from outside Pyo's
//! own node. Every request has a login (Basic auth):
//! - a team's registry token (`docker login`, any username): push and pull
//!   `<team>/<project>/<service>` (slugs) of its own team only, and push
//!   only to a project and service that exist;
//! - a cluster's pull token (username `pyo-cluster`, in its servers'
//!   registries.yaml): pull its team's images, Pyo's builds of its team's
//!   services (`pyo/services/<id>`) and Pyo's own (`pyo/<name>`);
//! - a builder's token (username `pyo-builder`): push and pull its team's
//!   builds (`pyo/services/<id>`), pull its team's images.
//!
//! It listens beside the API (`PYO_REGISTRY_DOOR_ADDR`, :5006, in the
//! cluster). Users reach it over HTTPS on the host's tailnet name, port
//! 5443: the gateway terminates TLS with Tailscale's certificate and admits
//! tailnet addresses only (pyo-proto system.rs `tailnet_objects`). Other
//! machines' containerd and builders reach it on the host's tailnet IP,
//! :5005 (pyo-host); this cluster's builders in the cluster. The registry
//! itself answers only Pyo, in the cluster (platform.yaml).
//!
//! A repository belongs to the team that first pushed it
//! (`pyo.registry_repos`). Renaming a team, project or service doesn't move
//! its images: deployments keep the image references they ran, and new
//! pushes use the new names.
//!
//! Every request is checked (Basic auth: any username, the token as the
//! password), then streamed to the registry and back: layers are never held
//! in memory.

use axum::{
    Router,
    body::Body,
    extract::{Request, State},
    http::{HeaderMap, HeaderValue, Method, StatusCode, header},
    response::{IntoResponse, Response},
    routing::any,
};
use base64::Engine;
use futures::TryStreamExt;
use serde_json::json;

use crate::{AppState, deploy, ids::sha256_hex};

/// Where users push from: the host's tailnet name on the registry's HTTPS
/// port ("<host>.<tailnet>.ts.net:5443"). None while Pyo is only on its own
/// machine (no tailnet with MagicDNS and HTTPS certificates).
pub fn address() -> Option<String> {
    crate::web::tailnet_name().map(|n| format!("{n}:{}", pyo_proto::system::REGISTRY_PORT))
}

/// The path a team's images go under: its slug.
pub fn team_path(slug: &str) -> String {
    slug.to_owned()
}

/// A team's own repository: "<team>/<project>/<service>", its three parts.
pub(crate) fn parts(name: &str) -> Option<(&str, &str, &str)> {
    let mut it = name.split('/');
    let (t, p, s) = (it.next()?, it.next()?, it.next()?);
    let label = |x: &str| {
        !x.is_empty()
            && x.len() <= 63
            && x.bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    };
    (it.next().is_none() && label(t) && label(p) && label(s) && t != "pyo").then_some((t, p, s))
}

/// A team's own repository as users name it from the tailnet:
/// "acme/shop/api" → "<host>:5443/acme/shop/api". None for Pyo's own
/// repositories (its builds, which the door doesn't serve) or without a
/// tailnet.
pub fn as_named(repo: &str) -> Option<String> {
    parts(repo)?;
    Some(format!("{}/{repo}", address()?))
}

/// An image the door's address names, as servers pull it (the registry's
/// pull address): "<host>:5443/acme/shop/api:1" → "127.0.0.1:5005/acme/shop/api:1".
pub fn as_pulled(image: &str) -> Option<String> {
    let door = address()?;
    image
        .strip_prefix(&format!("{door}/"))
        .map(|rest| format!("{}/{rest}", deploy::registry().pull))
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/v2/", any(door))
        .route("/v2/{*rest}", any(door))
        .with_state(state)
}

fn errors(status: StatusCode, code: &str, message: &str) -> Response {
    let mut r = (
        status,
        axum::Json(json!({ "errors": [{ "code": code, "message": message }] })),
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

/// A Basic login: (username, password).
fn login_of(headers: &HeaderMap) -> Option<(String, String)> {
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
        .map(|(u, p)| (u.to_owned(), p.to_owned()))
        .filter(|(_, p)| !p.is_empty())
}

/// The username a cluster's servers pull with (their registries.yaml).
pub const CLUSTER_USER: &str = "pyo-cluster";
/// The username builders push with.
pub const BUILDER_USER: &str = "pyo-builder";

/// Who's at the door, by their login.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum Machine {
    /// A cluster's servers: pull only.
    Cluster,
    /// A builder: its team's builds too.
    Builder,
}

/// What a cluster's servers or a builder may do, by the path alone:
/// `service_team` is the team of the service in a `pyo/services/<id>` path
/// (looked up by the door), `slug` the machine's team's.
pub(crate) fn machine_allowed(
    who: Machine,
    method: &Method,
    name: &str,
    slug: &str,
    service_team_is_theirs: bool,
) -> bool {
    let read = matches!(*method, Method::GET | Method::HEAD);
    let write = matches!(*method, Method::POST | Method::PUT | Method::PATCH);
    if name
        .strip_prefix("pyo/services/")
        .is_some_and(|id| !id.is_empty() && !id.contains('/'))
    {
        return service_team_is_theirs && (read || (write && who == Machine::Builder));
    }
    // Pyo's own images (a dev build's agent, WAF...): nothing secret.
    if name
        .strip_prefix("pyo/")
        .is_some_and(|n| !n.is_empty() && !n.contains('/'))
    {
        return read && who == Machine::Cluster;
    }
    read && parts(name).is_some_and(|(t, _, _)| t == slug)
}

/// What a request asks for: the repository it names (None for /v2/ itself)
/// and whether it writes.
#[derive(Debug, PartialEq)]
pub(crate) enum Wants {
    Root,
    Repo { name: String, write: bool },
    Other,
}

pub(crate) fn wants(method: &Method, path: &str) -> Wants {
    let Some(rest) = path.strip_prefix("/v2/") else {
        return Wants::Other;
    };
    if rest.is_empty() {
        return Wants::Root;
    }
    let name = ["/manifests/", "/blobs/", "/tags/list"]
        .iter()
        .filter_map(|m| rest.find(m))
        .min()
        .map(|i| &rest[..i]);
    match name {
        Some(n) if !n.is_empty() => Wants::Repo {
            name: n.to_owned(),
            write: !matches!(*method, Method::GET | Method::HEAD),
        },
        _ => Wants::Other,
    }
}

/// Whether a token for `slug` (with `push` or only pull) may do this, by
/// the path alone (the door then checks the project, service and owner).
pub(crate) fn allowed(slug: &str, push: bool, method: &Method, name: &str) -> bool {
    let ours = parts(name).is_some_and(|(t, _, _)| t == slug);
    match *method {
        Method::GET | Method::HEAD => ours,
        // Pushing: uploads, then the manifest. Deleting isn't offered.
        Method::POST | Method::PUT | Method::PATCH => ours && push,
        _ => false,
    }
}

/// A query without a cross-repository mount from outside the team (the
/// registry then takes a plain upload instead).
pub(crate) fn scoped_query(query: &str, slug: &str) -> String {
    let prefix = format!("{}/", team_path(slug));
    // (Another repository of its own team is fine to mount from.)
    let pairs: Vec<(&str, &str)> = query
        .split('&')
        .filter(|p| !p.is_empty())
        .map(|p| p.split_once('=').unwrap_or((p, "")))
        .collect();
    let foreign = pairs
        .iter()
        .any(|(k, v)| *k == "from" && !decode(v).starts_with(&prefix));
    pairs
        .into_iter()
        .filter(|(k, _)| !(foreign && (*k == "from" || *k == "mount")))
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("&")
}

/// Percent-decoding of a query value ("+" is a space).
fn decode(v: &str) -> String {
    let b = v.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'%' if i + 2 < b.len() => match std::str::from_utf8(&b[i + 1..i + 3])
                .ok()
                .and_then(|h| u8::from_str_radix(h, 16).ok())
            {
                Some(x) => {
                    out.push(x);
                    i += 3;
                    continue;
                }
                None => out.push(b'%'),
            },
            b'+' => out.push(b' '),
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// One client for every request through the door: no overall timeout (a
/// layer can take long), but a connect timeout.
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

async fn door(State(s): State<AppState>, req: Request) -> Response {
    let (parts, body) = req.into_parts();
    let Some((user, password)) = login_of(&parts.headers) else {
        return errors(
            StatusCode::UNAUTHORIZED,
            "UNAUTHORIZED",
            "Log in with a registry token from Pyo.",
        );
    };
    if user == CLUSTER_USER || user == BUILDER_USER {
        return machine_door(
            &s,
            if user == CLUSTER_USER {
                Machine::Cluster
            } else {
                Machine::Builder
            },
            &password,
            parts,
            body,
        )
        .await;
    }
    let token = password;
    let found: Option<(String, String, String, String)> = match sqlx::query_as(
        "SELECT t.id, t.scope, m.slug, t.team_id FROM pyo.registry_tokens t JOIN pyo.teams m ON m.id = t.team_id
          WHERE t.token_hash = $1 AND (t.expires_at IS NULL OR t.expires_at > now())",
    )
    .bind(sha256_hex(&token))
    .fetch_optional(&s.db)
    .await
    {
        Ok(f) => f,
        Err(e) => {
            tracing::warn!(error = %e, "registry door: token lookup failed");
            return errors(StatusCode::SERVICE_UNAVAILABLE, "UNAVAILABLE", "Try again.");
        }
    };
    let Some((id, scope, slug, team_id)) = found else {
        return errors(
            StatusCode::UNAUTHORIZED,
            "UNAUTHORIZED",
            "That token isn't valid (revoked or expired?).",
        );
    };
    let _ = sqlx::query("UPDATE pyo.registry_tokens SET last_used_at = now() WHERE id = $1 AND (last_used_at IS NULL OR last_used_at < now() - interval '1 minute')").bind(&id).execute(&s.db).await;
    let path = parts.uri.path().to_owned();
    let repo: Option<(String, Option<String>)>;
    match wants(&parts.method, &path) {
        // A login check.
        Wants::Root => return logged_in(),
        Wants::Repo { name, write } if allowed(&slug, scope == "push", &parts.method, &name) => {
            match owner(&s, &team_id, &name, write).await {
                Ok(Ok(service)) => repo = Some((name, service)),
                Ok(Err(why)) => return errors(StatusCode::FORBIDDEN, "DENIED", &why),
                Err(e) => {
                    tracing::warn!(error = %e, "registry door: repository lookup failed");
                    return errors(StatusCode::SERVICE_UNAVAILABLE, "UNAVAILABLE", "Try again.");
                }
            }
        }
        Wants::Repo { .. } => {
            let msg = format!(
                "This token can only {} images named {}/<project>/<service>.",
                if scope == "push" {
                    "push and pull"
                } else {
                    "pull"
                },
                team_path(&slug)
            );
            return errors(StatusCode::FORBIDDEN, "DENIED", &msg);
        }
        Wants::Other => return errors(StatusCode::FORBIDDEN, "DENIED", "Not available here."),
    }
    let query = parts
        .uri
        .query()
        .map(|q| scoped_query(q, &slug))
        .filter(|q| !q.is_empty())
        .map(|q| format!("?{q}"))
        .unwrap_or_default();
    forward(
        &s,
        parts,
        body,
        &path,
        &query,
        repo.map(|(name, service)| (name, service, team_id)),
    )
    .await
}

fn logged_in() -> Response {
    Response::builder()
        .header("docker-distribution-api-version", "registry/2.0")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from("{}"))
        .unwrap_or_else(|_| StatusCode::OK.into_response())
}

/// A cluster's servers or a builder at the door.
async fn machine_door(
    s: &AppState,
    who: Machine,
    password: &str,
    parts: axum::http::request::Parts,
    body: Body,
) -> Response {
    let sql = match who {
        Machine::Cluster => {
            "SELECT c.team_id, t.slug FROM pyo.clusters c JOIN pyo.teams t ON t.id = c.team_id WHERE c.registry_token_hash = $1"
        }
        Machine::Builder => {
            "SELECT b.team_id, t.slug FROM pyo.builders b JOIN pyo.teams t ON t.id = b.team_id WHERE b.token_hash = $1 AND b.status <> 'removing'"
        }
    };
    let found: Option<(String, String)> = match sqlx::query_as(sql)
        .bind(sha256_hex(password))
        .fetch_optional(&s.db)
        .await
    {
        Ok(f) => f,
        Err(e) => {
            tracing::warn!(error = %e, "registry door: login lookup failed");
            return errors(StatusCode::SERVICE_UNAVAILABLE, "UNAVAILABLE", "Try again.");
        }
    };
    let Some((team_id, slug)) = found else {
        return errors(
            StatusCode::UNAUTHORIZED,
            "UNAUTHORIZED",
            "That login isn't valid (rotated, or the builder removed?).",
        );
    };
    let path = parts.uri.path().to_owned();
    let name = match wants(&parts.method, &path) {
        Wants::Root => return logged_in(),
        Wants::Repo { name, .. } => name,
        Wants::Other => return errors(StatusCode::FORBIDDEN, "DENIED", "Not available here."),
    };
    let service_team: Option<String> = match name.strip_prefix("pyo/services/") {
        Some(id) => match sqlx::query_scalar("SELECT team_id FROM pyo.services WHERE id = $1")
            .bind(id)
            .fetch_optional(&s.db)
            .await
        {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!(error = %e, "registry door: service lookup failed");
                return errors(StatusCode::SERVICE_UNAVAILABLE, "UNAVAILABLE", "Try again.");
            }
        },
        None => None,
    };
    if !machine_allowed(
        who,
        &parts.method,
        &name,
        &slug,
        service_team.as_deref() == Some(team_id.as_str()),
    ) {
        return errors(
            StatusCode::FORBIDDEN,
            "DENIED",
            "Not one of this team's images.",
        );
    }
    // A team's own repository: only one it pushed first.
    if self::parts(&name).is_some() {
        match owner(s, &team_id, &name, false).await {
            Ok(Ok(_)) => {}
            Ok(Err(why)) => return errors(StatusCode::FORBIDDEN, "DENIED", &why),
            Err(e) => {
                tracing::warn!(error = %e, "registry door: repository lookup failed");
                return errors(StatusCode::SERVICE_UNAVAILABLE, "UNAVAILABLE", "Try again.");
            }
        }
    }
    // No cross-repository mounts: a plain upload instead.
    let query = parts
        .uri
        .query()
        .map(|q| {
            q.split('&')
                .filter(|p| !p.starts_with("mount=") && !p.starts_with("from="))
                .collect::<Vec<_>>()
                .join("&")
        })
        .filter(|q| !q.is_empty())
        .map(|q| format!("?{q}"))
        .unwrap_or_default();
    forward(s, parts, body, &path, &query, None).await
}

/// Streams a request through to the registry and its answer back. `pushed`:
/// a team's repository, recorded as its own once a manifest lands.
async fn forward(
    s: &AppState,
    parts: axum::http::request::Parts,
    body: Body,
    path: &str,
    query: &str,
    pushed: Option<(String, Option<String>, String)>,
) -> Response {
    let push = deploy::registry().internal.clone();
    let mut out = http().request(parts.method.clone(), format!("http://{push}{path}{query}"));
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
                "Pyo's registry didn't answer.",
            );
        }
    };
    // A pushed manifest: the repository is its team's (and its service's).
    if parts.method == Method::PUT
        && path.contains("/manifests/")
        && res.status().is_success()
        && let Some((name, service, team_id)) = &pushed
    {
        let _ = sqlx::query(
            "INSERT INTO pyo.registry_repos (repo, team_id, service_id) VALUES ($1, $2, $3)
             ON CONFLICT (repo) DO UPDATE SET last_push_at = now(), service_id = coalesce(excluded.service_id, registry_repos.service_id)",
        )
        .bind(name)
        .bind(team_id)
        .bind(service)
        .execute(&s.db)
        .await;
    }
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
                .and_then(|l| l.strip_prefix(&format!("http://{push}")))
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
                "Pyo's registry didn't answer.",
            )
        })
}

/// A repository the token's team may use: its own (or nobody's yet), and
/// for a push, an existing project and service of the team. Ok(Err(why))
/// when it may not; Ok(Ok(the service's id)) when it may.
async fn owner(
    s: &AppState,
    team_id: &str,
    name: &str,
    write: bool,
) -> sqlx::Result<Result<Option<String>, String>> {
    let Some((_, project, service)) = parts(name) else {
        return Ok(Err("Name it <team>/<project>/<service>.".into()));
    };
    let owned: Option<String> =
        sqlx::query_scalar("SELECT team_id FROM pyo.registry_repos WHERE repo = $1")
            .bind(name)
            .fetch_optional(&s.db)
            .await?;
    if owned.as_deref().is_some_and(|t| t != team_id) {
        return Ok(Err(format!("{name} belongs to another team.")));
    }
    let service_id: Option<String> = sqlx::query_scalar(
        "SELECT a.id FROM pyo.services a JOIN pyo.projects p ON p.id = a.project_id
          WHERE a.team_id = $1 AND p.namespace = $2 AND a.name = $3 AND NOT a.deleting",
    )
    .bind(team_id)
    .bind(project)
    .bind(service)
    .fetch_optional(&s.db)
    .await?;
    if write && service_id.is_none() {
        return Ok(Err(format!(
            "There's no service {service} in a project {project} of this team. Create it in Pyo first, then push to {name}."
        )));
    }
    Ok(Ok(service_id))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_are_scoped_to_the_team() {
        let get = Method::GET;
        assert_eq!(wants(&get, "/v2/"), Wants::Root);
        assert_eq!(
            wants(&Method::PUT, "/v2/acme/shop/web/manifests/1.0"),
            Wants::Repo {
                name: "acme/shop/web".into(),
                write: true
            }
        );
        assert_eq!(
            wants(&Method::PATCH, "/v2/acme/shop/web/blobs/uploads/uuid"),
            Wants::Repo {
                name: "acme/shop/web".into(),
                write: true
            }
        );
        assert_eq!(
            wants(&get, "/v2/acme/shop/web/tags/list"),
            Wants::Repo {
                name: "acme/shop/web".into(),
                write: false
            }
        );
        assert_eq!(wants(&get, "/v2/_catalog"), Wants::Other);
        // Its own team's path, pushes only with a push token, never deletes.
        assert!(allowed("acme", true, &Method::PUT, "acme/shop/web"));
        assert!(allowed("acme", false, &Method::GET, "acme/shop/web"));
        assert!(!allowed("acme", false, &Method::PUT, "acme/shop/web"));
        assert!(!allowed("acme", true, &Method::DELETE, "acme/shop/web"));
        assert!(!allowed("acme", true, &Method::GET, "other/shop/web"));
        assert!(!allowed("acme", true, &Method::GET, "pyo/services/svc_x"));
        assert!(
            !allowed("acme", true, &Method::PUT, "acmeco/shop/web"),
            "not another team's"
        );
        assert!(
            !allowed("acme", true, &Method::PUT, "acme/shop"),
            "three parts"
        );
        assert!(!allowed("acme", true, &Method::PUT, "acme/shop/web/x"));
        assert!(
            !allowed("acme", true, &Method::PUT, "acme/Shop/web"),
            "slugs"
        );
        assert!(
            parts("pyo/services/svc").is_none(),
            "Pyo's own builds aren't a team's"
        );
        assert!(
            as_named("pyo/services/svc").is_none(),
            "nor named through the door"
        );
        // A mount from another team's repository becomes a plain upload.
        assert_eq!(
            scoped_query("mount=sha256:aa&from=other/shop/x", "acme"),
            ""
        );
        assert_eq!(
            scoped_query("mount=sha256:aa&from=other%2Fshop%2Fx", "acme"),
            ""
        );
        assert_eq!(
            scoped_query("mount=sha256:aa&from=acme/shop/x", "acme"),
            "mount=sha256:aa&from=acme/shop/x"
        );
        assert_eq!(
            scoped_query("digest=sha256%3Aaa", "acme"),
            "digest=sha256%3Aaa"
        );
    }

    #[test]
    fn basic_logins() {
        let mut h = HeaderMap::new();
        let v = base64::engine::general_purpose::STANDARD.encode("acme:pyr_abc");
        h.insert(
            header::AUTHORIZATION,
            HeaderValue::from_str(&format!("Basic {v}")).unwrap(),
        );
        assert_eq!(login_of(&h), Some(("acme".into(), "pyr_abc".into())));
        assert_eq!(login_of(&HeaderMap::new()), None);
    }

    #[test]
    fn machines_reach_only_their_teams_images() {
        use Machine::*;
        let (get, put, del) = (Method::GET, Method::PUT, Method::DELETE);
        // A build of one of its team's services: servers pull, builders push too.
        assert!(machine_allowed(
            Cluster,
            &get,
            "pyo/services/svc_a",
            "acme",
            true
        ));
        assert!(
            !machine_allowed(Cluster, &put, "pyo/services/svc_a", "acme", true),
            "servers only pull"
        );
        assert!(machine_allowed(
            Builder,
            &put,
            "pyo/services/svc_a",
            "acme",
            true
        ));
        assert!(
            !machine_allowed(Builder, &del, "pyo/services/svc_a", "acme", true),
            "nobody deletes"
        );
        // Another team's build: nothing.
        assert!(!machine_allowed(
            Cluster,
            &get,
            "pyo/services/svc_b",
            "acme",
            false
        ));
        assert!(!machine_allowed(
            Builder,
            &put,
            "pyo/services/svc_b",
            "acme",
            false
        ));
        assert!(!machine_allowed(
            Cluster,
            &get,
            "pyo/services/svc_a/x",
            "acme",
            true
        ));
        // Pyo's own images: servers pull them.
        assert!(machine_allowed(Cluster, &get, "pyo/agent", "acme", false));
        assert!(!machine_allowed(Builder, &put, "pyo/agent", "acme", false));
        assert!(!machine_allowed(Cluster, &put, "pyo/agent", "acme", false));
        // Teams' pushed images: their own team's, pulled.
        assert!(machine_allowed(
            Cluster,
            &get,
            "acme/shop/web",
            "acme",
            false
        ));
        assert!(!machine_allowed(
            Cluster,
            &get,
            "other/shop/web",
            "acme",
            false
        ));
        assert!(
            !machine_allowed(Builder, &put, "acme/shop/web", "acme", false),
            "builders push only builds"
        );
    }
}
