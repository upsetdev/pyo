//! Folders sent with `pyo deploy`: built like a Git checkout by a builder in
//! the service's own cluster, then deployed.
//!
//! - **Upload:** the CLI packs the folder as a gzipped tar and says what it
//!   is (`POST /source-uploads`: its team, size and SHA-256). It sends the
//!   archive in parts of [`PART_SIZE`] (`PUT …/parts/{n}`, again when one
//!   fails: a part sent twice replaces itself), asks which arrived
//!   (`GET /source-uploads/{id}`) and finishes (`POST …/complete`), which
//!   checks the SHA-256. Parts stay small enough for any proxy in front
//!   (Cloudflare's 100 MB), so Pyo Cloud takes them the same way.
//! - **Where it waits:** in Pyo's own database (`pyo.source_upload_parts`),
//!   which every web replica shares, for at most [`KEEP_HOURS`].
//! - **Deploy:** `deploy` or `createService` with `uploadId` claims it for
//!   one deployment (deploy.rs). Its build job names it, and the builder
//!   that claims the job fetches it with its signed requests
//!   (`GET /api/builder/v1/jobs/{id}/source`), then says it has it
//!   (`DELETE` there): the archive is deleted from Pyo then. A cancelled or
//!   finished deployment, or one that never got a builder, loses it too.

use axum::{
    Router,
    body::{Body, Bytes},
    extract::{DefaultBodyLimit, Path, State},
    http::{StatusCode, header},
    response::Response,
    routing::{get, post, put},
};
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::{PgPool, Postgres, Transaction};

use crate::{
    AppState,
    auth::SessionUser,
    builders::BuilderAuth,
    error::{ApiError, ApiResult, ErrorBody, Json},
    ops::{Role, require_role},
};

/// The size of each part but the last.
pub const PART_SIZE: u64 = 8 << 20;
/// How long an upload is kept: to be sent, then to be built.
pub const KEEP_HOURS: i64 = 2;
/// Uploads one team may have at once.
const MAX_WAITING: i64 = 10;

/// The biggest archive Pyo takes: `PYO_SOURCE_UPLOAD_MAX_BYTES`, 500 MB by default.
pub fn max_bytes() -> u64 {
    std::env::var("PYO_SOURCE_UPLOAD_MAX_BYTES")
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .filter(|n| *n > 0)
        .unwrap_or(500_000_000)
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/source-uploads", post(create))
        .route("/source-uploads/{id}", get(status))
        .route(
            "/source-uploads/{id}/parts/{n}",
            put(part).layer(DefaultBodyLimit::max(PART_SIZE as usize + 1024)),
        )
        .route("/source-uploads/{id}/complete", post(complete))
}

/// For builders (signed): the archive of a build they're running.
pub fn builder_routes() -> Router<AppState> {
    Router::new().route("/jobs/{id}/source", get(fetch).delete(fetched))
}

/// Parts an archive of `size` bytes is sent in.
pub fn part_count(size: u64) -> u64 {
    size.div_ceil(PART_SIZE)
}

/// The size part `n` of an archive of `size` bytes must have, if there's
/// such a part.
pub fn part_len(size: u64, n: u64) -> Option<u64> {
    let count = part_count(size);
    if n >= count {
        return None;
    }
    Some(if n + 1 == count {
        size - n * PART_SIZE
    } else {
        PART_SIZE
    })
}

/// Text from the CLI shown on the dashboard: no control characters, short.
fn label(s: &str) -> String {
    s.chars().filter(|c| !c.is_control()).take(100).collect()
}

/// "500 MB".
fn mb(b: u64) -> String {
    format!("{} MB", b.div_ceil(1_000_000))
}

/// A folder about to be uploaded.
#[derive(Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct NewSourceUpload {
    /// The team's id.
    team_id: String,
    /// The archive's size in bytes (a gzipped tar).
    size: u64,
    /// Its SHA-256, in lowercase hex.
    sha256: String,
    /// How many files it holds.
    #[serde(default)]
    files: u32,
    /// The folder's name, shown with the deployment.
    #[serde(default)]
    folder: String,
    /// The computer's name, shown as "Uploaded from …".
    #[serde(default)]
    machine: String,
}

/// Start an upload
///
/// The first step of deploying a folder: say what the archive is (a
/// gzipped tar of the folder) and get where its parts go. Send each part
/// with `uploadSourcePart`, then `completeSourceUpload`, then `deploy` (or
/// `createService`) with the `uploadId`. At most
/// `PYO_SOURCE_UPLOAD_MAX_BYTES` (500 MB by default). Kept for 2 hours.
#[utoipa::path(post, path = "/source-uploads", operation_id = "createSourceUpload", tag = "Deployments",
    request_body = NewSourceUpload,
    responses(
        (status = 200, description = "`uploadId`, `partSize` (bytes in each part but the last), `parts` (how many) and `expiresAt`.", body = Object),
        (status = 400, description = "Empty, or not a SHA-256.", body = ErrorBody),
        (status = 403, description = "You can't deploy in this team.", body = ErrorBody),
        (status = 413, description = "Bigger than this installation takes.", body = ErrorBody),
        (status = 429, description = "Too many uploads waiting in this team.", body = ErrorBody),
    ),
    extensions(("x-pyo-access" = json!("deploy"))))]
pub async fn create(
    State(s): State<AppState>,
    SessionUser(user): SessionUser,
    Json(req): Json<NewSourceUpload>,
) -> ApiResult<Json<Value>> {
    if req.size == 0 {
        return Err(ApiError::bad_request("The folder is empty."));
    }
    let max = max_bytes();
    if req.size > max {
        return Err(ApiError::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            format!(
                "The folder packs to {}, more than the {} Pyo takes. Leave out more in .pyoignore.",
                mb(req.size),
                mb(max)
            ),
        ));
    }
    let sha = req.sha256.trim().to_ascii_lowercase();
    if sha.len() != 64 || !sha.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(ApiError::bad_request(
            "sha256 is the archive's SHA-256, in hex.",
        ));
    }
    let mut tx = s.db.begin().await?;
    require_role(&mut tx, &user, &req.team_id, Role::Developer).await?;
    let waiting: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM pyo.source_uploads WHERE team_id = $1 AND expires_at > now()",
    )
    .bind(&req.team_id)
    .fetch_one(&mut *tx)
    .await?;
    if waiting >= MAX_WAITING {
        return Err(ApiError::new(
            StatusCode::TOO_MANY_REQUESTS,
            "Too many uploads are waiting to be built in this team. Try again in a few minutes.",
        ));
    }
    let id = crate::ids::new_id("src");
    let expires_at: i64 = sqlx::query_scalar(
        "INSERT INTO pyo.source_uploads (id, team_id, size, sha256, files, folder, machine, created_by, expires_at)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, now() + make_interval(hours => $9)) RETURNING pyo.ms(expires_at)",
    )
    .bind(&id)
    .bind(&req.team_id)
    .bind(req.size as i64)
    .bind(&sha)
    .bind(req.files as i32)
    .bind(label(&req.folder))
    .bind(label(&req.machine))
    .bind(&user.name)
    .bind(KEEP_HOURS as i32)
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(Json(json!({
        "uploadId": id,
        "partSize": PART_SIZE,
        "parts": part_count(req.size),
        "expiresAt": expires_at,
    })))
}

/// An upload the user may send to: its team's developers and up.
async fn mine(
    tx: &mut Transaction<'_, Postgres>,
    user: &crate::auth::User,
    id: &str,
) -> ApiResult<(u64, String, String)> {
    let row: Option<(String, i64, String, String)> = sqlx::query_as(
        "SELECT team_id, size, sha256, state FROM pyo.source_uploads WHERE id = $1 AND expires_at > now()",
    )
    .bind(id)
    .fetch_optional(&mut **tx)
    .await?;
    let (team_id, size, sha, state) = row.ok_or_else(|| ApiError::not_found("Upload"))?;
    require_role(tx, user, &team_id, Role::Developer).await?;
    Ok((size as u64, sha, state))
}

/// Send a part
///
/// Part `n` (from 0) of an upload's archive, as the request's body: exactly
/// `partSize` bytes, the last part what's left. Sending a part again
/// replaces it.
#[utoipa::path(put, path = "/source-uploads/{id}/parts/{n}", operation_id = "uploadSourcePart", tag = "Deployments",
    params(("id" = String, Path, description = "The upload's id."), ("n" = u64, Path, description = "The part, from 0.")),
    request_body(content = String, content_type = "application/octet-stream", description = "The part's bytes."),
    responses(
        (status = 200, description = "Stored.", body = Object),
        (status = 400, description = "Not a part of this upload, or not its size.", body = ErrorBody),
        (status = 404, description = "No such upload, or it expired.", body = ErrorBody),
        (status = 409, description = "The upload is complete already.", body = ErrorBody),
    ),
    extensions(("x-pyo-access" = json!("deploy"))))]
pub async fn part(
    State(s): State<AppState>,
    SessionUser(user): SessionUser,
    Path((id, n)): Path<(String, u64)>,
    body: Bytes,
) -> ApiResult<Json<Value>> {
    let mut tx = crate::rls::begin(&s.db, &user).await?;
    let (size, _, state) = mine(&mut tx, &user, &id).await?;
    if state != "uploading" {
        return Err(ApiError::conflict("This upload is complete already."));
    }
    let want = part_len(size, n).ok_or_else(|| {
        ApiError::bad_request(format!("This upload has {} parts.", part_count(size)))
    })?;
    if body.len() as u64 != want {
        return Err(ApiError::bad_request(format!(
            "Part {n} is {want} bytes, not {}.",
            body.len()
        )));
    }
    sqlx::query(
        "INSERT INTO pyo.source_upload_parts (upload_id, n, data) VALUES ($1, $2, $3)
         ON CONFLICT (upload_id, n) DO UPDATE SET data = excluded.data",
    )
    .bind(&id)
    .bind(n as i32)
    .bind(body.as_ref())
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(Json(json!({ "ok": true })))
}

/// The parts of an upload Pyo has, in order.
async fn received(db: impl sqlx::PgExecutor<'_>, id: &str) -> sqlx::Result<Vec<i32>> {
    sqlx::query_scalar("SELECT n FROM pyo.source_upload_parts WHERE upload_id = $1 ORDER BY n")
        .bind(id)
        .fetch_all(db)
        .await
}

/// An upload
///
/// Which parts of an upload Pyo has (`received`), to send the rest after a
/// break, and its `state`: "uploading", "ready" (complete) or "used" (a
/// deployment builds it).
#[utoipa::path(get, path = "/source-uploads/{id}", operation_id = "getSourceUpload", tag = "Deployments",
    params(("id" = String, Path, description = "The upload's id.")),
    responses(
        (status = 200, description = "`uploadId`, `state`, `parts` and `received`.", body = Object),
        (status = 404, description = "No such upload, or it expired.", body = ErrorBody),
    ),
    extensions(("x-pyo-access" = json!("deploy"))))]
pub async fn status(
    State(s): State<AppState>,
    SessionUser(user): SessionUser,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    let mut tx = crate::rls::begin(&s.db, &user).await?;
    let (size, _, state) = mine(&mut tx, &user, &id).await?;
    let have = received(&mut *tx, &id).await?;
    tx.commit().await?;
    Ok(Json(json!({
        "uploadId": id,
        "state": state,
        "parts": part_count(size),
        "received": have,
    })))
}

/// The SHA-256 of an upload's parts, in order, read one at a time.
async fn digest(db: &PgPool, id: &str, count: u64) -> ApiResult<String> {
    use sha2::Digest;
    let mut hash = sha2::Sha256::new();
    for n in 0..count {
        let data: Vec<u8> = sqlx::query_scalar(
            "SELECT data FROM pyo.source_upload_parts WHERE upload_id = $1 AND n = $2",
        )
        .bind(id)
        .bind(n as i32)
        .fetch_one(db)
        .await?;
        hash.update(&data);
    }
    Ok(hex::encode(hash.finalize()))
}

/// Finish an upload
///
/// Once every part is sent: Pyo checks the archive against its SHA-256.
/// Then deploy it with `deploy` (or `createService`) and its `uploadId`.
#[utoipa::path(post, path = "/source-uploads/{id}/complete", operation_id = "completeSourceUpload", tag = "Deployments",
    params(("id" = String, Path, description = "The upload's id.")),
    responses(
        (status = 200, description = "Complete.", body = Object),
        (status = 400, description = "Parts are missing (`getSourceUpload` says which), or the archive isn't what was declared (the upload is gone then: start again).", body = ErrorBody),
        (status = 404, description = "No such upload, or it expired.", body = ErrorBody),
    ),
    extensions(("x-pyo-access" = json!("deploy"))))]
pub async fn complete(
    State(s): State<AppState>,
    SessionUser(user): SessionUser,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    let mut tx = s.db.begin().await?;
    let (size, sha, state) = mine(&mut tx, &user, &id).await?;
    tx.commit().await?;
    if state != "uploading" {
        return Ok(Json(json!({ "ok": true })));
    }
    let count = part_count(size);
    let have = received(&s.db, &id).await?;
    let missing: Vec<u64> = (0..count)
        .filter(|n| !have.contains(&(*n as i32)))
        .collect();
    if !missing.is_empty() {
        return Err(ApiError::bad_request(format!(
            "{} of {count} parts haven't arrived. Send them first.",
            missing.len()
        )));
    }
    if digest(&s.db, &id, count).await? != sha {
        delete(&s.db, &id).await?;
        return Err(ApiError::bad_request(
            "The upload doesn't match the folder that was packed. Deploy again.",
        ));
    }
    sqlx::query(
        "UPDATE pyo.source_uploads SET state = 'ready' WHERE id = $1 AND state = 'uploading'",
    )
    .bind(&id)
    .execute(&s.db)
    .await?;
    Ok(Json(json!({ "ok": true })))
}

/// An upload claimed for a deployment, as deploy.rs puts it in the
/// deployment's source and its build job.
pub struct Claimed {
    pub job: pyo_proto::SourceUpload,
    pub files: i32,
    pub folder: String,
    pub machine: String,
}

/// Take a complete upload of the team for one deployment, once.
pub async fn claim(
    tx: &mut Transaction<'_, Postgres>,
    id: &str,
    team_id: &str,
    deployment_id: &str,
) -> ApiResult<Claimed> {
    let row: Option<(i64, String, i32, String, String)> = sqlx::query_as(
        "UPDATE pyo.source_uploads SET state = 'used', deployment_id = $3, expires_at = now() + make_interval(hours => $4)
          WHERE id = $1 AND team_id = $2 AND state = 'ready' AND expires_at > now()
          RETURNING size, sha256, files, folder, machine",
    )
    .bind(id)
    .bind(team_id)
    .bind(deployment_id)
    .bind(KEEP_HOURS as i32)
    .fetch_optional(&mut **tx)
    .await?;
    let (size, sha256, files, folder, machine) = row.ok_or_else(|| {
        ApiError::conflict(
            "That upload isn't complete, was deployed already or expired. Deploy the folder again.",
        )
    })?;
    Ok(Claimed {
        job: pyo_proto::SourceUpload {
            id: id.to_owned(),
            size: size as u64,
            sha256,
        },
        files,
        folder,
        machine,
    })
}

/// A retried deployment builds the same upload, while Pyo still has it.
pub async fn reclaim(
    tx: &mut Transaction<'_, Postgres>,
    id: &str,
    team_id: &str,
    deployment_id: &str,
) -> ApiResult<pyo_proto::SourceUpload> {
    let row: Option<(i64, String)> = sqlx::query_as(
        "UPDATE pyo.source_uploads SET deployment_id = $3, expires_at = now() + make_interval(hours => $4)
          WHERE id = $1 AND team_id = $2 AND state = 'used' AND expires_at > now()
          RETURNING size, sha256",
    )
    .bind(id)
    .bind(team_id)
    .bind(deployment_id)
    .bind(KEEP_HOURS as i32)
    .fetch_optional(&mut **tx)
    .await?;
    let (size, sha256) = row.ok_or_else(|| {
        ApiError::conflict(
            "The folder sent for that deploy is gone. Deploy it again with pyo deploy.",
        )
    })?;
    Ok(pyo_proto::SourceUpload {
        id: id.to_owned(),
        size: size as u64,
        sha256,
    })
}

async fn delete(db: impl sqlx::PgExecutor<'_>, id: &str) -> sqlx::Result<()> {
    sqlx::query("DELETE FROM pyo.source_uploads WHERE id = $1")
        .bind(id)
        .execute(db)
        .await?;
    Ok(())
}

/// A deployment that's done (or cancelled) doesn't need its upload.
pub async fn drop_for(db: impl sqlx::PgExecutor<'_>, deployment_id: &str) -> sqlx::Result<()> {
    sqlx::query("DELETE FROM pyo.source_uploads WHERE deployment_id = $1")
        .bind(deployment_id)
        .execute(db)
        .await?;
    Ok(())
}

/// Uploads past their time, sent or not.
pub async fn sweep(db: &PgPool) -> sqlx::Result<u64> {
    Ok(
        sqlx::query("DELETE FROM pyo.source_uploads WHERE expires_at < now()")
            .execute(db)
            .await?
            .rows_affected(),
    )
}

/// The upload of a build this builder is running.
async fn job_upload(
    s: &AppState,
    b: &BuilderAuth,
    job_id: &str,
) -> ApiResult<pyo_proto::SourceUpload> {
    let job: Option<Value> = sqlx::query_scalar(
        "SELECT job FROM pyo.build_jobs WHERE id = $1 AND builder_id = $2 AND status = 'running'",
    )
    .bind(job_id)
    .bind(&b.id)
    .fetch_optional(&s.db)
    .await?;
    let job = job.ok_or_else(|| {
        ApiError::new(
            StatusCode::GONE,
            "This build isn't running any more (cancelled?).",
        )
    })?;
    serde_json::from_value(job["sourceUpload"].clone())
        .map_err(|_| ApiError::bad_request("This build has no uploaded folder."))
}

/// GET /api/builder/v1/jobs/{id}/source: the archive, streamed from its
/// parts in order.
async fn fetch(
    State(s): State<AppState>,
    b: BuilderAuth,
    Path(job_id): Path<String>,
) -> ApiResult<Response> {
    let up = job_upload(&s, &b, &job_id).await?;
    let count = part_count(up.size);
    let have = received(&s.db, &up.id).await?;
    if have.len() as u64 != count {
        return Err(ApiError::new(
            StatusCode::GONE,
            "The uploaded folder is no longer in Pyo. Deploy it again.",
        ));
    }
    let (db, id) = (s.db.clone(), up.id.clone());
    let parts = futures::stream::unfold(0u64, move |n| {
        let (db, id) = (db.clone(), id.clone());
        async move {
            if n >= count {
                return None;
            }
            let r: Result<Vec<u8>, sqlx::Error> = sqlx::query_scalar(
                "SELECT data FROM pyo.source_upload_parts WHERE upload_id = $1 AND n = $2",
            )
            .bind(&id)
            .bind(n as i32)
            .fetch_one(&db)
            .await;
            Some((r.map(Bytes::from).map_err(std::io::Error::other), n + 1))
        }
    });
    Response::builder()
        .header(header::CONTENT_TYPE, "application/gzip")
        .header(header::CONTENT_LENGTH, up.size)
        .header("x-pyo-sha256", &up.sha256)
        .body(Body::from_stream(parts))
        .map_err(|e| ApiError::from(anyhow::Error::from(e)))
}

/// DELETE /api/builder/v1/jobs/{id}/source: the builder has the archive,
/// so Pyo lets it go.
async fn fetched(
    State(s): State<AppState>,
    b: BuilderAuth,
    Path(job_id): Path<String>,
) -> ApiResult<Json<Value>> {
    let up = job_upload(&s, &b, &job_id).await?;
    delete(&s.db, &up.id).await?;
    Ok(Json(json!({ "ok": true })))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::teamsec_tests::run_big;

    #[test]
    fn parts_cover_the_archive() {
        assert_eq!(part_count(1), 1);
        assert_eq!(part_count(PART_SIZE), 1);
        assert_eq!(part_count(PART_SIZE + 1), 2);
        assert_eq!(part_len(PART_SIZE + 1, 0), Some(PART_SIZE));
        assert_eq!(part_len(PART_SIZE + 1, 1), Some(1));
        assert_eq!(part_len(PART_SIZE + 1, 2), None);
        assert_eq!(part_len(10, 0), Some(10));
    }

    #[test]
    fn labels_are_short_and_plain() {
        assert_eq!(label("my\napp\u{1b}[31m"), "myapp[31m");
        assert_eq!(label(&"x".repeat(300)).len(), 100);
        assert_eq!(mb(500_000_000), "500 MB");
        assert_eq!(mb(1), "1 MB");
    }

    fn state(db: PgPool) -> AppState {
        let config = crate::Config {
            agent_api_url: String::new(),
            agent_image: String::new(),
            waf_image: String::new(),
            geoip_image: String::new(),
            cookie_secure: false,
            allow_signup: false,
            builder_image: String::new(),
            relay_addr: None,
        };
        let (_stop, stopping) = tokio::sync::watch::channel(false);
        AppState {
            db: db.clone(),
            config: std::sync::Arc::new(config),
            observability: crate::observability::Observability::from_env(db),
            sealer: std::sync::Arc::new(crate::secrets::Sealer::for_tests()),
            wakers: Default::default(),
            terminals: Default::default(),
            stopping,
        }
    }

    /// Against a throwaway Postgres (DATABASE_URL): a developer uploads a
    /// folder in parts (one sent twice), completes it, and deploys it; the
    /// build job names it; the builder running the job fetches exactly the
    /// archive and lets it go, and Pyo deletes it. A deploy without an
    /// upload reuses the image built last; a viewer can't upload.
    #[test]
    #[ignore = "needs DATABASE_URL (a throwaway Postgres)"]
    fn a_folder_goes_through_pyo_to_the_builder() {
        run_big(async {
            use axum::http::{Method, Request};
            use sha2::Digest;
            use tower::ServiceExt as _;

            let db = PgPool::connect(&std::env::var("DATABASE_URL").unwrap())
                .await
                .unwrap();
            crate::migrate::run(&db).await.unwrap();
            crate::vault::init(crate::vault::Vault::new(
                db.clone(),
                crate::vault::Provider::from_env(),
            ));
            let run = crate::ids::random_string(8).to_lowercase();
            let id = |p: &str| format!("{p}_{run}");
            let (team, cluster, project, svc) = (id("team"), id("cl"), id("prj"), id("svc"));
            let q = |sql: &'static str| sqlx::query(sql);
            let user = |uid: String| crate::auth::User {
                id: uid.clone(),
                name: uid,
                email: String::new(),
                timezone: "UTC".into(),
                is_admin: false,
                must_set_up: false,
                key: None,
            };
            for u in [id("usr"), id("view")] {
                q("INSERT INTO pyo.users (id, email, name, password_hash) VALUES ($1, $1 || '@t.test', $1, '')").bind(&u).execute(&db).await.unwrap();
            }
            q("INSERT INTO pyo.teams (id, name, slug) VALUES ($1, $1, $1)")
                .bind(&team)
                .execute(&db)
                .await
                .unwrap();
            for (u, role) in [(id("usr"), "developer"), (id("view"), "viewer")] {
                q("INSERT INTO pyo.members (id, team_id, user_id, email, role, status) VALUES ($1 || 'm', $2, $1, $1 || '@t.test', $3, 'active')").bind(&u).bind(&team).bind(role).execute(&db).await.unwrap();
            }
            q("INSERT INTO pyo.clusters (id, team_id, name, agent_token_hash, status, last_seen_at) VALUES ($1, $2, $1, $1, 'connected', now())").bind(&cluster).bind(&team).execute(&db).await.unwrap();
            q("INSERT INTO pyo.projects (id, team_id, name, namespace) VALUES ($1, $2, 'shop', $1)")
                .bind(&project)
                .bind(&team)
                .execute(&db)
                .await
                .unwrap();
            let spec = json!({
                "name": "web", "source": { "type": "upload", "dockerfilePath": "Dockerfile", "buildContext": ".", "buildArgs": [] },
                "env": [], "replicas": 1,
                "resources": { "cpuRequest": 100, "cpuLimit": 500, "memoryRequest": 128, "memoryLimit": 256 },
                "healthCheck": { "type": "tcp", "path": "", "port": 8080, "command": "", "initialDelaySeconds": 10, "periodSeconds": 10, "auto": "default" },
                "ports": [{ "id": "port_1", "name": "http", "containerPort": 8080, "protocol": "HTTP", "exposure": "internal" }],
                "domains": [],
            });
            q("INSERT INTO pyo.services (id, team_id, project_id, cluster_id, name, spec, status) VALUES ($1, $2, $3, $4, 'web', $5, 'stopped')").bind(&svc).bind(&team).bind(&project).bind(&cluster).bind(&spec).execute(&db).await.unwrap();
            let token = format!("tok{run}");
            q("INSERT INTO pyo.builders (id, team_id, name, kind, cluster_id, server, token_hash, status) VALUES ($1, $2, $1, 'cluster', $3, 's', $4, 'ready')")
                .bind(id("bld")).bind(&team).bind(&cluster).bind(crate::ids::sha256_hex(&token)).execute(&db).await.unwrap();
            let s = state(db.clone());
            let dev = user(id("usr"));

            // An archive of two parts and a bit.
            let mut archive = vec![0u8; (PART_SIZE * 2 + 5) as usize];
            rand::fill(&mut archive[..]);
            let sha = hex::encode(sha2::Sha256::digest(&archive));
            let new = |sha: &str| NewSourceUpload {
                team_id: team.clone(),
                size: archive.len() as u64,
                sha256: sha.to_owned(),
                files: 2,
                folder: "shop".into(),
                machine: "laptop\n".into(),
            };
            let e = create(
                State(s.clone()),
                SessionUser(user(id("view"))),
                Json(new(&sha)),
            )
            .await
            .unwrap_err();
            assert_eq!(e.status, StatusCode::FORBIDDEN, "viewers can't upload");
            let started = create(State(s.clone()), SessionUser(dev.clone()), Json(new(&sha)))
                .await
                .unwrap()
                .0;
            assert_eq!(started["parts"], 3);
            let up = started["uploadId"].as_str().unwrap().to_owned();
            let chunk = |n: usize| {
                Bytes::copy_from_slice(
                    &archive[n * PART_SIZE as usize..((n + 1) * PART_SIZE as usize).min(archive.len())],
                )
            };
            let send = |n: usize, b: Bytes| {
                part(
                    State(s.clone()),
                    SessionUser(dev.clone()),
                    Path((up.clone(), n as u64)),
                    b,
                )
            };
            assert_eq!(
                send(0, Bytes::from_static(b"short"))
                    .await
                    .unwrap_err()
                    .status,
                StatusCode::BAD_REQUEST
            );
            assert_eq!(
                send(3, chunk(2)).await.unwrap_err().status,
                StatusCode::BAD_REQUEST
            );
            send(0, chunk(0)).await.unwrap();
            send(2, chunk(2)).await.unwrap();
            // Not all there yet.
            let st = status(State(s.clone()), SessionUser(dev.clone()), Path(up.clone()))
                .await
                .unwrap()
                .0;
            assert_eq!(st["received"], json!([0, 2]));
            assert!(
                complete(State(s.clone()), SessionUser(dev.clone()), Path(up.clone()))
                    .await
                    .is_err()
            );
            send(1, chunk(1)).await.unwrap();
            send(1, chunk(1)).await.unwrap();
            complete(State(s.clone()), SessionUser(dev.clone()), Path(up.clone()))
                .await
                .unwrap();

            // A wrong checksum: refused, and the upload is gone.
            let bad = create(
                State(s.clone()),
                SessionUser(dev.clone()),
                Json(new(&"0".repeat(64))),
            )
            .await
            .unwrap()
            .0;
            let bad = bad["uploadId"].as_str().unwrap().to_owned();
            for n in 0..3 {
                part(
                    State(s.clone()),
                    SessionUser(dev.clone()),
                    Path((bad.clone(), n)),
                    chunk(n as usize),
                )
                .await
                .unwrap();
            }
            assert_eq!(
                complete(
                    State(s.clone()),
                    SessionUser(dev.clone()),
                    Path(bad.clone())
                )
                .await
                .unwrap_err()
                .status,
                StatusCode::BAD_REQUEST
            );
            assert!(
                status(State(s.clone()), SessionUser(dev.clone()), Path(bad))
                    .await
                    .is_err()
            );

            // Deployed: the build job names it, the deployment says where it came from.
            let dep = id("dpl");
            let mut tx = db.begin().await.unwrap();
            crate::deploy::create(
                &mut tx,
                crate::deploy::DeployRequest {
                    deployment_id: &dep,
                    service_id: &svc,
                    trigger: "manual",
                    actor: "t",
                    rollback_of: None,
                    retry_of: None,
                    commit: None,
                    upload: Some(&up),
                },
            )
            .await
            .unwrap();
            tx.commit().await.unwrap();
            let (job_id, job): (String, Value) =
                sqlx::query_as("SELECT id, job FROM pyo.build_jobs WHERE deployment_id = $1")
                    .bind(&dep)
                    .fetch_one(&db)
                    .await
                    .unwrap();
            assert_eq!(job["sourceUpload"]["id"], json!(up));
            assert_eq!(job["repoUrl"], "");
            let data: Value = sqlx::query_scalar("SELECT data FROM pyo.deployments WHERE id = $1")
                .bind(&dep)
                .fetch_one(&db)
                .await
                .unwrap();
            assert_eq!(
                (
                    data["source"]["type"].as_str(),
                    data["source"]["machine"].as_str()
                ),
                (Some("upload"), Some("laptop"))
            );
            assert_eq!(data["revisionLabel"], json!(&sha[..7]));
            // Used once.
            let mut tx = db.begin().await.unwrap();
            assert!(claim(&mut tx, &up, &team, "dpl_other").await.is_err());
            tx.rollback().await.unwrap();

            // The builder: only while it runs the job.
            let app = axum::Router::new()
                .nest(
                    "/api/builder/v1",
                    crate::builders::routes().merge(builder_routes()),
                )
                .with_state(s.clone());
            let call = |method: Method, path: String| {
                let app = app.clone();
                let token = token.clone();
                async move {
                    let req = Request::builder()
                        .method(method)
                        .uri(path)
                        .header("authorization", format!("Bearer {token}"))
                        .body(Body::empty())
                        .unwrap();
                    let res = app.oneshot(req).await.unwrap();
                    let st = res.status();
                    (
                        st,
                        axum::body::to_bytes(res.into_body(), 64 << 20)
                            .await
                            .unwrap(),
                    )
                }
            };
            let path = format!("/api/builder/v1/jobs/{job_id}/source");
            assert_eq!(
                call(Method::GET, path.clone()).await.0,
                StatusCode::GONE,
                "not claimed yet"
            );
            q("UPDATE pyo.build_jobs SET status = 'running', builder_id = $2 WHERE id = $1")
                .bind(&job_id)
                .bind(id("bld"))
                .execute(&db)
                .await
                .unwrap();
            let (st, body) = call(Method::GET, path.clone()).await;
            assert_eq!(st, StatusCode::OK);
            assert_eq!(body.len(), archive.len());
            assert!(body == archive, "the same bytes");
            assert_eq!(call(Method::DELETE, path.clone()).await.0, StatusCode::OK);
            let left: i64 =
                sqlx::query_scalar("SELECT count(*) FROM pyo.source_upload_parts WHERE upload_id = $1")
                    .bind(&up)
                    .fetch_one(&db)
                    .await
                    .unwrap();
            assert_eq!(left, 0, "deleted from Pyo once the builder has it");
            assert_eq!(
                call(Method::GET, path).await.0,
                StatusCode::GONE,
                "gone once fetched"
            );

            // Built: deployed again without a folder, the image built last.
            let image = format!(
                "{}/pyo/services/{svc}@sha256:{}",
                crate::deploy::registry().pull,
                "a".repeat(64)
            );
            q("UPDATE pyo.build_jobs SET status = 'succeeded' WHERE id = $1")
                .bind(&job_id)
                .execute(&db)
                .await
                .unwrap();
            q("UPDATE pyo.deployments SET status = 'succeeded', spec = spec || jsonb_build_object('image', $2::text) WHERE id = $1").bind(&dep).bind(&image).execute(&db).await.unwrap();
            q("UPDATE pyo.services SET current_deployment_id = $2 WHERE id = $1")
                .bind(&svc)
                .bind(&dep)
                .execute(&db)
                .await
                .unwrap();
            let again = id("dpla");
            let mut tx = db.begin().await.unwrap();
            crate::deploy::create(
                &mut tx,
                crate::deploy::DeployRequest {
                    deployment_id: &again,
                    service_id: &svc,
                    trigger: "redeploy",
                    actor: "t",
                    rollback_of: None,
                    retry_of: None,
                    commit: None,
                    upload: None,
                },
            )
            .await
            .unwrap();
            tx.commit().await.unwrap();
            let (spec, data): (Value, Value) =
                sqlx::query_as("SELECT spec, data FROM pyo.deployments WHERE id = $1")
                    .bind(&again)
                    .fetch_one(&db)
                    .await
                    .unwrap();
            assert_eq!(spec["image"], json!(image));
            assert_eq!(data["source"]["folder"], "shop");
            let jobs: i64 =
                sqlx::query_scalar("SELECT count(*) FROM pyo.build_jobs WHERE deployment_id = $1")
                    .bind(&again)
                    .fetch_one(&db)
                    .await
                    .unwrap();
            assert_eq!(jobs, 0, "nothing to build");

            // Expired uploads go.
            let old = create(State(s.clone()), SessionUser(dev.clone()), Json(new(&sha)))
                .await
                .unwrap()
                .0;
            q("UPDATE pyo.source_uploads SET expires_at = now() - interval '1 minute' WHERE id = $1")
                .bind(old["uploadId"].as_str().unwrap())
                .execute(&db)
                .await
                .unwrap();
            assert!(sweep(&db).await.unwrap() >= 1);

            q("DELETE FROM pyo.teams WHERE id = $1")
                .bind(&team)
                .execute(&db)
                .await
                .unwrap();
            for u in [id("usr"), id("view")] {
                q("DELETE FROM pyo.users WHERE id = $1")
                    .bind(u)
                    .execute(&db)
                    .await
                    .unwrap();
            }
        })
    }
}
