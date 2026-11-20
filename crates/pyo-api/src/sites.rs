//! Pyo Static (pyo-proto's site.rs): static builds' folders, kept in the
//! team's bucket of Pyo's store (`sites/<service>/<deployment>/`), and
//! read by each cluster's `pyo-static`.
//!
//! - The builder uploads a static build's output: it starts the upload
//!   with the folder's manifest, puts each file, and marks it done
//!   (`/api/builder/v1/jobs/<id>/site…`). A deployment serves its folder
//!   only once it's complete ([`site_for`]); a build that couldn't upload
//!   runs as a service from its image, as before.
//! - `pyo-static` reads the manifest and the files through Pyo
//!   (`/api/static/v1`), read-only, with its cluster's token
//!   ([`reader_token`]): only folders of that cluster's sites that are
//!   complete and not removed.
//! - The newest [`KEEP`] folders of each service stay for rollbacks; older
//!   ones, and those of deleted services, are removed by [`sweeper`].

use crate::error::Json;
use std::time::Duration;

use axum::{
    Router,
    body::Body,
    extract::{FromRequestParts, Path, State},
    http::{HeaderMap, StatusCode, request::Parts},
    response::{IntoResponse, Response},
    routing::{get, post, put},
};
use pyo_proto::site::{self, SiteManifest, SiteSpec};
use serde_json::{Value, json};
use sqlx::{PgPool, Postgres, Transaction};

use crate::{
    AppState,
    builders::BuilderAuth,
    error::{ApiError, ApiResult},
};

/// Folders kept per service, the live one's aside.
pub const KEEP: i64 = 10;
/// An upload not finished in this long is a leftover.
const STALE_UPLOAD: &str = "3 hours";

/// The builder's routes, under `/api/builder/v1`.
pub fn builder_routes() -> Router<AppState> {
    Router::new()
        .route("/jobs/{id}/site", post(start_upload))
        .route("/jobs/{id}/site/files/{*path}", put(put_file))
        .route("/jobs/{id}/site/done", post(finish_upload))
}

/// The agent's route, under `/api/agent/v1`: its `pyo-static`'s login.
pub fn agent_routes() -> Router<AppState> {
    Router::new().route("/static-login", get(static_login))
}

/// `pyo-static`'s routes, under `/api/static/v1`.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/sites/{site}/deploys/{deploy}/manifest", get(manifest))
        .route("/sites/{site}/deploys/{deploy}/files/{*path}", get(file))
}

// ── The store ──────────────────────────────────────────────────────────────

/// Where a deployment's file is in the team's bucket.
pub fn object_key(service_id: &str, deployment_id: &str, path: &str) -> String {
    format!("sites/{service_id}/{deployment_id}/{path}")
}

/// The SHA-256 of nothing: what an empty body is signed with.
const EMPTY_SHA256: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

/// The team's bucket in Pyo's own store.
struct Store {
    http: reqwest::Client,
    endpoint: reqwest::Url,
    bucket: String,
    access_key: String,
    secret: String,
}

impl Store {
    async fn for_team(db: &PgPool, team_id: &str) -> ApiResult<Self> {
        let Some((url, _, _)) = crate::backup::local_store() else {
            return Err(ApiError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "This installation has no storage of its own for static sites.",
            ));
        };
        let (access_key, secret) = crate::backupstore::team_login(db, team_id).await?;
        let endpoint =
            reqwest::Url::parse(url.trim_end_matches('/')).map_err(anyhow::Error::from)?;
        Ok(Self {
            http: reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(10))
                .build()
                .map_err(anyhow::Error::from)?,
            endpoint,
            bucket: crate::backupstore::team_bucket(team_id),
            access_key,
            secret,
        })
    }

    /// A signed request for an object (path-style), its body given by its
    /// SHA-256.
    fn request(&self, method: reqwest::Method, key: &str, sha256: &str) -> reqwest::RequestBuilder {
        use crate::backup::{Request, enc, sigv4_hashed};
        let base = &self.endpoint;
        let host = match base.port() {
            Some(p) => format!("{}:{p}", base.host_str().unwrap_or("")),
            None => base.host_str().unwrap_or("").to_owned(),
        };
        let path = enc(
            &format!(
                "{}/{}/{}",
                base.path().trim_end_matches('/'),
                self.bucket,
                key
            )
            .replace("//", "/"),
            true,
        );
        let signed = sigv4_hashed(
            &Request {
                method: method.as_str(),
                host: &host,
                path: &path,
                query: "",
                body: &[],
                service: "s3",
                region: crate::backupstore::REGION,
                access_key: &self.access_key,
                secret: &self.secret,
            },
            sha256,
        );
        self.http
            .request(method, format!("{}://{host}{path}", base.scheme()))
            .header("x-amz-date", signed.amz_date)
            .header("x-amz-content-sha256", signed.payload_hash)
            .header("authorization", signed.authorization)
    }

    async fn ok(res: reqwest::Response) -> anyhow::Result<reqwest::Response> {
        if res.status().is_success() {
            return Ok(res);
        }
        let status = res.status();
        let text = res.text().await.unwrap_or_default();
        anyhow::bail!(
            "the store answered {status}: {}",
            text.chars().take(300).collect::<String>()
        )
    }

    async fn delete(&self, key: &str) -> anyhow::Result<()> {
        let res = self
            .request(reqwest::Method::DELETE, key, EMPTY_SHA256)
            .timeout(Duration::from_secs(60))
            .send()
            .await?;
        if res.status() == StatusCode::NOT_FOUND {
            return Ok(());
        }
        Self::ok(res).await.map(|_| ())
    }
}

// ── Uploads (the builder) ──────────────────────────────────────────────────

/// The running build job's deployment: (deployment, service, team, cluster),
/// when the job may upload a site.
async fn upload_job(
    s: &AppState,
    b: &BuilderAuth,
    job_id: &str,
) -> ApiResult<(String, String, String, String)> {
    let row: Option<(String, String, String, String, bool)> = sqlx::query_as(
        "SELECT d.id, d.service_id, d.team_id, d.cluster_id, coalesce((j.job->>'uploadSite')::boolean, false)
           FROM pyo.build_jobs j JOIN pyo.deployments d ON d.id = j.deployment_id
          WHERE j.id = $1 AND j.builder_id = $2 AND j.status = 'running'",
    )
    .bind(job_id)
    .bind(&b.id)
    .fetch_optional(&s.db)
    .await?;
    match row {
        Some((d, svc, team, cluster, true)) if team == b.team_id => Ok((d, svc, team, cluster)),
        Some(_) => Err(ApiError::forbidden(
            "This build doesn't upload a static site.",
        )),
        None => Err(ApiError::new(
            StatusCode::GONE,
            "This build isn't running any more (cancelled?).",
        )),
    }
}

/// Why a folder's manifest can't be uploaded, if it can't.
pub fn check_manifest(m: &SiteManifest) -> Result<(u64, u64), String> {
    if m.files.is_empty() {
        return Err("The output folder is empty.".into());
    }
    if m.files.len() > site::MAX_FILES {
        return Err(format!(
            "The output folder has {} files, over the {} a static site may have.",
            m.files.len(),
            site::MAX_FILES
        ));
    }
    let mut seen = std::collections::HashSet::new();
    let mut bytes = 0u64;
    for f in &m.files {
        if !site::valid_path(&f.path) {
            return Err(format!("{} isn't a path Pyo can serve.", f.path));
        }
        if !seen.insert(f.path.as_str()) {
            return Err(format!("{} is in the folder twice.", f.path));
        }
        if f.size > site::MAX_FILE_BYTES {
            return Err(format!(
                "{} is over the {} MiB a file may have.",
                f.path,
                site::MAX_FILE_BYTES / 1024 / 1024
            ));
        }
        if f.sha256.len() != 64 || !f.sha256.bytes().all(|c| c.is_ascii_hexdigit()) {
            return Err(format!("{} has no SHA-256.", f.path));
        }
        bytes += f.size;
    }
    if bytes > site::MAX_BYTES {
        return Err(format!(
            "The output folder has {} MiB, over the {} MiB a static site may have.",
            bytes / 1024 / 1024,
            site::MAX_BYTES / 1024 / 1024
        ));
    }
    Ok((m.files.len() as u64, bytes))
}

/// The start of an upload: the folder's manifest.
async fn start_upload(
    State(s): State<AppState>,
    b: BuilderAuth,
    Path(id): Path<String>,
    Json(m): Json<SiteManifest>,
) -> ApiResult<Json<Value>> {
    let (deployment, service, team, cluster) = upload_job(&s, &b, &id).await?;
    if crate::backup::local_store().is_none() || reader_key().is_none() {
        return Err(ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "This installation has no storage of its own for static sites.",
        ));
    }
    let (files, bytes) = check_manifest(&m).map_err(ApiError::bad_request)?;
    // The team's bucket and key, made on first use.
    Store::for_team(&s.db, &team).await?;
    sqlx::query(
        "INSERT INTO pyo.static_sites (deployment_id, team_id, service_id, cluster_id, manifest, files, bytes)
         VALUES ($1, $2, $3, $4, $5, $6, $7)
         ON CONFLICT (deployment_id) DO UPDATE SET manifest = $5, files = $6, bytes = $7, complete = false, removed_at = NULL",
    )
    .bind(&deployment)
    .bind(&team)
    .bind(&service)
    .bind(&cluster)
    .bind(serde_json::to_value(&m).map_err(anyhow::Error::from)?)
    .bind(files as i32)
    .bind(bytes as i64)
    .execute(&s.db)
    .await?;
    Ok(Json(json!({ "ok": true })))
}

/// One file, streamed to the store. The builder sends its SHA-256
/// (`x-pyo-sha256`) and length; the store refuses a body that doesn't
/// match them.
async fn put_file(
    State(s): State<AppState>,
    b: BuilderAuth,
    Path((id, path)): Path<(String, String)>,
    headers: HeaderMap,
    body: Body,
) -> ApiResult<Json<Value>> {
    let (deployment, service, team, _) = upload_job(&s, &b, &id).await?;
    if !site::valid_path(&path) {
        return Err(ApiError::bad_request("That isn't a path Pyo can serve."));
    }
    let sha = headers
        .get("x-pyo-sha256")
        .and_then(|v| v.to_str().ok())
        .filter(|v| v.len() == 64 && v.bytes().all(|c| c.is_ascii_hexdigit()))
        .ok_or_else(|| ApiError::bad_request("x-pyo-sha256 is missing."))?
        .to_ascii_lowercase();
    let len: u64 = headers
        .get("content-length")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse().ok())
        .ok_or_else(|| ApiError::bad_request("Content-Length is missing."))?;
    if len > site::MAX_FILE_BYTES {
        return Err(ApiError::bad_request("That file is too large."));
    }
    let open: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM pyo.static_sites WHERE deployment_id = $1 AND NOT complete AND removed_at IS NULL)",
    )
    .bind(&deployment)
    .fetch_one(&s.db)
    .await?;
    if !open {
        return Err(ApiError::conflict("Start the upload first."));
    }
    let store = Store::for_team(&s.db, &team).await?;
    let stream = reqwest::Body::wrap_stream(body.into_data_stream());
    let res = store
        .request(
            reqwest::Method::PUT,
            &object_key(&service, &deployment, &path),
            &sha,
        )
        .header("content-length", len)
        .header("content-type", site::content_type(&path))
        .body(stream)
        .timeout(Duration::from_secs(600))
        .send()
        .await
        .map_err(anyhow::Error::from)?;
    Store::ok(res).await?;
    Ok(Json(json!({ "ok": true })))
}

/// Every file is there: the deployment may serve the folder.
async fn finish_upload(
    State(s): State<AppState>,
    b: BuilderAuth,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    let (deployment, ..) = upload_job(&s, &b, &id).await?;
    let done = sqlx::query(
        "UPDATE pyo.static_sites SET complete = true WHERE deployment_id = $1 AND removed_at IS NULL",
    )
    .bind(&deployment)
    .execute(&s.db)
    .await?;
    if done.rows_affected() == 0 {
        return Err(ApiError::conflict("Start the upload first."));
    }
    Ok(Json(json!({ "ok": true })))
}

// ── Deploys ────────────────────────────────────────────────────────────────

/// The service's setting: serve a static build as static files (the
/// default) or run it as a service.
pub fn serves_static(service_spec: &Value) -> bool {
    service_spec["serveAs"].as_str() != Some("service")
}

/// What a deployment of the service serves: its folder, when it has a
/// complete one that's still kept and the service serves static builds as
/// files. Else None: it runs as a service from its image.
pub async fn site_for(
    tx: &mut Transaction<'_, Postgres>,
    service_spec: &Value,
    deployment_id: &str,
) -> ApiResult<Option<SiteSpec>> {
    if !serves_static(service_spec) {
        return Ok(None);
    }
    let fallback: Option<Option<bool>> = sqlx::query_scalar(
        "SELECT (manifest->>'indexFallback')::boolean FROM pyo.static_sites WHERE deployment_id = $1 AND complete AND removed_at IS NULL",
    )
    .bind(deployment_id)
    .fetch_optional(&mut **tx)
    .await?;
    Ok(fallback.map(|f| SiteSpec {
        deploy: deployment_id.to_owned(),
        index_fallback: f.unwrap_or(false),
    }))
}

/// Whether `pyo-static` should run on a cluster: a service there (not being
/// deleted) serves a folder, or a rollout of one is under way.
pub async fn wanted(tx: &mut Transaction<'_, Postgres>, cluster_id: &str) -> ApiResult<bool> {
    Ok(sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM pyo.services s JOIN pyo.deployments d ON d.service_id = s.id
          WHERE s.cluster_id = $1 AND NOT s.deleting AND jsonb_typeof(d.spec->'site') = 'object'
            AND (d.id = s.current_deployment_id OR d.status NOT IN ('succeeded', 'failed', 'cancelled')))",
    )
    .bind(cluster_id)
    .fetch_one(&mut **tx)
    .await?)
}

/// `pyo-static` on the cluster as its sites need it (bundles.rs).
pub async fn sync(tx: &mut Transaction<'_, Postgres>, cluster_id: &str) -> ApiResult<()> {
    let on = wanted(tx, cluster_id).await?;
    crate::bundles::sync_static(tx, cluster_id, on).await
}

// ── pyo-static's login ─────────────────────────────────────────────────────

/// The key its tokens are made with: Pyo's own secret key.
fn reader_key() -> Option<Vec<u8>> {
    std::env::var("PYO_SECRET_KEY")
        .ok()
        .and_then(|k| hex::decode(k.trim()).ok())
        .filter(|k| k.len() == 32)
}

fn reader_mac(key: &[u8], cluster_id: &str) -> String {
    hex::encode(crate::backup::hmac(
        key,
        &format!("pyo-static\n{cluster_id}"),
    ))
}

/// A cluster's `pyo-static` token: `pst.<cluster>.<mac>`. Made from Pyo's
/// key, so it isn't stored; it reads only that cluster's sites, and stops
/// working with the cluster.
pub fn reader_token(cluster_id: &str) -> Option<String> {
    let key = reader_key()?;
    Some(format!("pst.{cluster_id}.{}", reader_mac(&key, cluster_id)))
}

/// The cluster a token is for, when it's a real one.
fn token_cluster(key: &[u8], token: &str) -> Option<String> {
    let rest = token.strip_prefix("pst.")?;
    let (cluster, mac) = rest.rsplit_once('.')?;
    let want = reader_mac(key, cluster);
    let same = want.len() == mac.len()
        && want
            .bytes()
            .zip(mac.bytes())
            .fold(0u8, |acc, (a, b)| acc | (a ^ b))
            == 0;
    (same && !cluster.is_empty()).then(|| cluster.to_owned())
}

/// A token for an agent that gives pyo-static no key of its own (one older
/// than component keys), where agents may still use tokens.
async fn static_login(a: crate::agent::AgentCluster) -> ApiResult<Json<Value>> {
    if crate::agent_auth::refuse_tokens() {
        return Err(ApiError::new(
            StatusCode::GONE,
            "pyo-static signs its requests with a key from its cluster's agent here.",
        ));
    }
    let token = reader_token(&a.id).ok_or_else(|| {
        ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "This installation can't serve static sites (PYO_SECRET_KEY isn't set).",
        )
    })?;
    Ok(Json(json!({ "token": token })))
}

/// The cluster a `pyo-static` reads for.
pub struct StaticReader {
    pub cluster_id: String,
}

impl FromRequestParts<AppState> for StaticReader {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        // Its key (agent_auth.rs), made by the agent in its cluster.
        if let Some(v) = parts.extensions.get::<crate::agent_auth::VerifiedAgent>() {
            if v.component != crate::agent_auth::STATIC {
                return Err(ApiError::new(StatusCode::FORBIDDEN, "This key isn't pyo-static's."));
            }
            return Ok(StaticReader { cluster_id: v.cluster_id.clone() });
        }
        if crate::agent_auth::refuse_tokens() {
            return Err(ApiError::new(StatusCode::UNAUTHORIZED, "pyo-static signs its requests here."));
        }
        let token = parts
            .headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .ok_or_else(|| ApiError::new(StatusCode::UNAUTHORIZED, "Missing token."))?;
        let cluster = reader_key()
            .and_then(|k| token_cluster(&k, token))
            .ok_or_else(|| ApiError::new(StatusCode::UNAUTHORIZED, "Unknown token."))?;
        let known: bool =
            sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM pyo.clusters WHERE id = $1)")
                .bind(&cluster)
                .fetch_one(&state.db)
                .await?;
        // A cluster whose pyo-static has a key no longer reads with a token.
        if !known || crate::agent_auth::has_key(&state.db, &cluster, crate::agent_auth::STATIC, None).await? {
            return Err(ApiError::new(StatusCode::UNAUTHORIZED, "Unknown token."));
        }
        Ok(StaticReader {
            cluster_id: cluster,
        })
    }
}

/// A folder this cluster's `pyo-static` may read: its team, when it's one
/// of the cluster's sites, complete and not removed.
async fn published(db: &PgPool, r: &StaticReader, site: &str, deploy: &str) -> ApiResult<String> {
    sqlx::query_scalar(
        "SELECT team_id FROM pyo.static_sites WHERE deployment_id = $1 AND service_id = $2 AND cluster_id = $3 AND complete AND removed_at IS NULL",
    )
    .bind(deploy)
    .bind(site)
    .bind(&r.cluster_id)
    .fetch_optional(db)
    .await?
    .ok_or_else(|| ApiError::not_found("Site"))
}

async fn manifest(
    State(s): State<AppState>,
    r: StaticReader,
    Path((site, deploy)): Path<(String, String)>,
) -> ApiResult<Json<Value>> {
    published(&s.db, &r, &site, &deploy).await?;
    let m: Value =
        sqlx::query_scalar("SELECT manifest FROM pyo.static_sites WHERE deployment_id = $1")
            .bind(&deploy)
            .fetch_one(&s.db)
            .await?;
    Ok(Json(m))
}

async fn file(
    State(s): State<AppState>,
    r: StaticReader,
    Path((site, deploy, path)): Path<(String, String, String)>,
) -> ApiResult<Response> {
    if !site::valid_path(&path) {
        return Err(ApiError::not_found("File"));
    }
    let team = published(&s.db, &r, &site, &deploy).await?;
    let store = Store::for_team(&s.db, &team).await?;
    let res = store
        .request(
            reqwest::Method::GET,
            &object_key(&site, &deploy, &path),
            EMPTY_SHA256,
        )
        .timeout(Duration::from_secs(600))
        .send()
        .await
        .map_err(anyhow::Error::from)?;
    if res.status() == StatusCode::NOT_FOUND {
        return Err(ApiError::not_found("File"));
    }
    let res = Store::ok(res).await?;
    let mut out = Response::builder().status(StatusCode::OK);
    if let Some(len) = res.headers().get("content-length").cloned() {
        out = out.header("content-length", len);
    }
    out = out.header("content-type", "application/octet-stream");
    Ok(out
        .body(Body::from_stream(res.bytes_stream()))
        .map_err(anyhow::Error::from)?
        .into_response())
}

// ── Clean-up ───────────────────────────────────────────────────────────────

/// Which folders to remove: of services that are gone (or being deleted),
/// beyond the newest [`KEEP`] of a service (its live one is always kept),
/// and uploads left unfinished. Marked removed here; their files go next.
async fn mark_removed(db: &PgPool) -> anyhow::Result<u64> {
    let r = sqlx::query(sqlx::AssertSqlSafe(format!(
        "UPDATE pyo.static_sites t SET removed_at = now()
          WHERE removed_at IS NULL AND (
            NOT EXISTS (SELECT 1 FROM pyo.services s WHERE s.id = t.service_id AND NOT s.deleting)
            OR (NOT complete AND created_at < now() - interval '{STALE_UPLOAD}')
            OR (NOT EXISTS (SELECT 1 FROM pyo.services s WHERE s.current_deployment_id = t.deployment_id)
                AND NOT EXISTS (SELECT 1 FROM pyo.deployments d WHERE d.id = t.deployment_id AND d.status NOT IN ('succeeded', 'failed', 'cancelled'))
                AND (SELECT count(*) FROM pyo.static_sites n WHERE n.service_id = t.service_id AND n.removed_at IS NULL AND n.created_at > t.created_at) >= {KEEP}))"
    )))
    .execute(db)
    .await?;
    Ok(r.rows_affected())
}

/// Deletes the files of folders marked removed, then their rows.
async fn remove_files(db: &PgPool) -> anyhow::Result<()> {
    let rows: Vec<(String, String, String, Value)> = sqlx::query_as(
        "SELECT deployment_id, team_id, service_id, manifest FROM pyo.static_sites WHERE removed_at IS NOT NULL ORDER BY removed_at LIMIT 20",
    )
    .fetch_all(db)
    .await?;
    for (deployment, team, service, manifest) in rows {
        let store = Store::for_team(db, &team)
            .await
            .map_err(|e| anyhow::anyhow!(e.message))?;
        let m: SiteManifest = serde_json::from_value(manifest).unwrap_or_default();
        for f in &m.files {
            store
                .delete(&object_key(&service, &deployment, &f.path))
                .await?;
        }
        sqlx::query("DELETE FROM pyo.static_sites WHERE deployment_id = $1")
            .bind(&deployment)
            .execute(db)
            .await?;
        tracing::info!(deployment = %deployment, files = m.files.len(), "removed a static site's folder");
    }
    Ok(())
}

/// Every 10 minutes: removes the folders no deployment will serve again.
pub async fn sweeper(db: PgPool) {
    if crate::backup::local_store().is_none() {
        return;
    }
    let mut tick = tokio::time::interval(Duration::from_secs(10 * 60));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tick.tick().await;
        if let Err(e) = mark_removed(&db).await {
            tracing::warn!(error = %format!("{e:#}"), "couldn't pick the static sites' folders to remove");
            continue;
        }
        if let Err(e) = remove_files(&db).await {
            tracing::warn!(error = %format!("{e:#}"), "couldn't remove a static site's folder");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pyo_proto::site::SiteFile;

    fn file(path: &str, size: u64) -> SiteFile {
        SiteFile {
            path: path.into(),
            size,
            sha256: "a".repeat(64),
        }
    }

    #[test]
    fn a_folder_is_checked_before_its_upload() {
        let ok = SiteManifest {
            files: vec![file("index.html", 10), file("a/b.css", 5)],
            index_fallback: false,
        };
        assert_eq!(check_manifest(&ok), Ok((2, 15)));
        let mut m = ok.clone();
        m.files.push(file("index.html", 1));
        assert!(check_manifest(&m).unwrap_err().contains("twice"));
        let mut m = ok.clone();
        m.files.push(file("../etc/passwd", 1));
        assert!(check_manifest(&m).is_err());
        let mut m = ok.clone();
        m.files.push(file("big.bin", site::MAX_FILE_BYTES + 1));
        assert!(check_manifest(&m).is_err());
        let mut m = ok.clone();
        m.files[0].sha256 = "nope".into();
        assert!(check_manifest(&m).is_err());
        assert!(check_manifest(&SiteManifest::default()).is_err());
    }

    #[test]
    fn a_readers_token_is_for_its_cluster_only() {
        let key = [7u8; 32];
        let t = format!("pst.cl_1.{}", reader_mac(&key, "cl_1"));
        assert_eq!(token_cluster(&key, &t).as_deref(), Some("cl_1"));
        // Another cluster's id with this mac, or another key: no.
        let forged = t.replace("cl_1", "cl_2");
        assert_eq!(token_cluster(&key, &forged), None);
        assert_eq!(token_cluster(&[8u8; 32], &t), None);
        assert_eq!(token_cluster(&key, "pst.cl_1."), None);
        assert_eq!(token_cluster(&key, "cl_1"), None);
    }

    #[test]
    fn a_folder_lives_under_its_service_and_deployment() {
        assert_eq!(
            object_key("svc_1", "dpl_1", "a/index.html"),
            "sites/svc_1/dpl_1/a/index.html"
        );
        assert!(serves_static(&json!({})));
        assert!(serves_static(&json!({ "serveAs": "static" })));
        assert!(!serves_static(&json!({ "serveAs": "service" })));
    }
}
