//! Pyo's built-in backup store: Garage (platform.yaml), one node, driven
//! over its admin API (v2, :3903, reachable from Pyo only). Its S3 API
//! (:3900) wants region "garage" and path-style addressing.
//!
//! At start Pyo puts the node in the layout, imports its own key (the
//! pyo-backup-store secret) and makes its bucket, `pyo-backups`; each step
//! is skipped when it's already done, so restarts are harmless. Each team
//! gets a bucket (`pyo-team-<id>`) and a key that reaches only that bucket;
//! Pyo's own key never leaves Pyo. A deleted team's key, bucket and backups
//! are removed by the team-backup runner ([`sweep`]): its repository
//! password died with it, so they can't be restored anyway.

use std::{collections::HashSet, time::Duration};

use anyhow::{Context, anyhow, bail, ensure};
use reqwest::Method;
use serde_json::{Value, json};
use sqlx::PgPool;

/// The region Garage signs with (garage.toml `s3_region`).
pub const REGION: &str = "garage";
/// Pyo's own backups' bucket.
pub const PYO_BUCKET: &str = "pyo-backups";
/// Teams' buckets and keys are named `pyo-team-<id>`.
const TEAM_PREFIX: &str = "pyo-team-";
/// The node's zone in the layout (one node: any name).
const ZONE: &str = "pyo";
/// A team key not saved this long after it was made is a leftover.
const STRAY_AFTER: Duration = Duration::from_secs(15 * 60);

/// A team's bucket in the store: "pyo-team-<id>" (lowercase, '-' for '_').
pub fn team_bucket(team_id: &str) -> String {
    let id: String = team_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    format!("{TEAM_PREFIX}{id}")
}

/// The admin API, with its bearer token.
#[derive(Clone)]
pub struct Admin {
    base: String,
    token: String,
    http: reqwest::Client,
}

/// The store's admin API, when this installation runs the store.
pub fn admin() -> Option<Admin> {
    let var = |k: &str| {
        std::env::var(k)
            .ok()
            .map(|v| v.trim().to_owned())
            .filter(|v| !v.is_empty())
    };
    Some(Admin::new(
        &var("PYO_BACKUP_STORE_ADMIN_URL")?,
        &var("PYO_BACKUP_STORE_ADMIN_TOKEN")?,
    ))
}

/// What a call to the admin API wants: endpoint, query and JSON body.
#[derive(Debug, PartialEq)]
struct Call {
    method: Method,
    endpoint: &'static str,
    query: Vec<(&'static str, String)>,
    body: Option<Value>,
}

impl Call {
    fn get(endpoint: &'static str, query: &[(&'static str, &str)]) -> Self {
        Self {
            method: Method::GET,
            endpoint,
            query: query.iter().map(|(k, v)| (*k, (*v).to_owned())).collect(),
            body: None,
        }
    }

    fn post(endpoint: &'static str, query: &[(&'static str, &str)], body: Option<Value>) -> Self {
        Self {
            method: Method::POST,
            endpoint,
            query: query.iter().map(|(k, v)| (*k, (*v).to_owned())).collect(),
            body,
        }
    }
}

// The requests, by Garage's admin API v2.
fn node_info() -> Call {
    Call::get("GetNodeInfo", &[("node", "self")])
}
fn layout() -> Call {
    Call::get("GetClusterLayout", &[])
}
fn add_node(node: &str, capacity: u64) -> Call {
    Call::post(
        "UpdateClusterLayout",
        &[],
        Some(json!({ "roles": [{ "id": node, "zone": ZONE, "capacity": capacity, "tags": [] }] })),
    )
}
fn apply_layout(version: u64) -> Call {
    Call::post(
        "ApplyClusterLayout",
        &[],
        Some(json!({ "version": version })),
    )
}
fn key_info(id: &str) -> Call {
    Call::get("GetKeyInfo", &[("id", id)])
}
fn import_key(id: &str, secret: &str) -> Call {
    Call::post(
        "ImportKey",
        &[],
        Some(json!({ "accessKeyId": id, "secretAccessKey": secret, "name": "pyo" })),
    )
}
fn create_key(name: &str) -> Call {
    Call::post("CreateKey", &[], Some(json!({ "name": name })))
}
fn delete_key(id: &str) -> Call {
    Call::post("DeleteKey", &[("id", id)], None)
}
fn bucket_info(alias: &str) -> Call {
    Call::get("GetBucketInfo", &[("globalAlias", alias)])
}
fn create_bucket(alias: &str) -> Call {
    Call::post("CreateBucket", &[], Some(json!({ "globalAlias": alias })))
}
fn delete_bucket(id: &str) -> Call {
    Call::post("DeleteBucket", &[("id", id)], None)
}
/// Read and write; `owner` too for Pyo's own bucket (and one it empties).
fn allow(bucket_id: &str, key: &str, owner: bool) -> Call {
    Call::post(
        "AllowBucketKey",
        &[],
        Some(
            json!({ "bucketId": bucket_id, "accessKeyId": key, "permissions": { "read": true, "write": true, "owner": owner } }),
        ),
    )
}
fn cleanup_uploads(bucket_id: &str) -> Call {
    Call::post(
        "CleanupIncompleteUploads",
        &[],
        Some(json!({ "bucketId": bucket_id, "olderThanSecs": 0 })),
    )
}
fn list_keys() -> Call {
    Call::get("ListKeys", &[])
}
fn list_buckets() -> Call {
    Call::get("ListBuckets", &[])
}

impl Admin {
    pub fn new(base: &str, token: &str) -> Self {
        Self {
            base: base.trim_end_matches('/').to_owned(),
            token: token.to_owned(),
            http: reqwest::Client::new(),
        }
    }

    /// One call: its status and JSON answer (Null when empty).
    async fn send(&self, c: Call) -> anyhow::Result<(u16, Value)> {
        let mut req = self
            .http
            .request(c.method, format!("{}/v2/{}", self.base, c.endpoint))
            .bearer_auth(&self.token)
            .query(&c.query)
            .timeout(Duration::from_secs(30));
        if let Some(b) = &c.body {
            req = req.json(b);
        }
        let res = req
            .send()
            .await
            .with_context(|| format!("the backup store's admin API ({})", c.endpoint))?;
        let status = res.status().as_u16();
        let text = res.text().await.unwrap_or_default();
        Ok((status, serde_json::from_str(&text).unwrap_or(Value::Null)))
    }

    /// One call that must succeed.
    async fn ok(&self, c: Call) -> anyhow::Result<Value> {
        let endpoint = c.endpoint;
        let (status, v) = self.send(c).await?;
        ensure!(
            (200..300).contains(&status),
            "the backup store answered {endpoint} with {status}: {}",
            v["message"].as_str().unwrap_or("")
        );
        Ok(v)
    }

    /// The bucket with this name, made if it isn't there.
    async fn bucket(&self, alias: &str) -> anyhow::Result<Value> {
        for _ in 0..3 {
            match self.send(bucket_info(alias)).await? {
                (200, v) => return Ok(v),
                (404, _) => match self.send(create_bucket(alias)).await? {
                    (200, v) => return Ok(v),
                    // Made by someone else just now: read it.
                    (409, _) => continue,
                    (s, v) => bail!(
                        "couldn't make the bucket {alias} ({s}: {})",
                        v["message"].as_str().unwrap_or("")
                    ),
                },
                (s, v) => bail!(
                    "couldn't read the bucket {alias} ({s}: {})",
                    v["message"].as_str().unwrap_or("")
                ),
            }
        }
        bail!("couldn't make the bucket {alias}")
    }

    /// Lets a key read and write a bucket, unless it already may.
    async fn grant(&self, bucket: &Value, key: &str, owner: bool) -> anyhow::Result<()> {
        if has_access(bucket, key, owner) {
            return Ok(());
        }
        self.ok(allow(bucket["id"].as_str().unwrap_or(""), key, owner))
            .await
            .map(|_| ())
    }

    /// A new key: (access key, secret).
    async fn new_key(&self, name: &str) -> anyhow::Result<(String, String)> {
        let v = self.ok(create_key(name)).await?;
        match (v["accessKeyId"].as_str(), v["secretAccessKey"].as_str()) {
            (Some(k), Some(s)) if !k.is_empty() && !s.is_empty() => {
                Ok((k.to_owned(), s.to_owned()))
            }
            _ => bail!("the backup store made no key"),
        }
    }

    /// Deletes a key; one that's already gone is fine.
    async fn remove_key(&self, id: &str) -> anyhow::Result<()> {
        match self.send(delete_key(id)).await? {
            (200 | 204 | 404, _) => Ok(()),
            (s, v) => bail!(
                "couldn't delete the key {id} ({s}: {})",
                v["message"].as_str().unwrap_or("")
            ),
        }
    }
}

/// Whether a bucket (GetBucketInfo) already lets `key` read and write (and
/// own it, if `owner`).
fn has_access(bucket: &Value, key: &str, owner: bool) -> bool {
    bucket["keys"].as_array().is_some_and(|keys| {
        keys.iter().any(|k| {
            let p = &k["permissions"];
            k["accessKeyId"] == key
                && p["read"] == true
                && p["write"] == true
                && (!owner || p["owner"] == true)
        })
    })
}

fn in_layout(layout: &Value, node: &str) -> bool {
    layout["roles"]
        .as_array()
        .is_some_and(|r| r.iter().any(|x| x["id"] == node))
}

/// Puts the node in the layout, unless it's there. Another process doing
/// the same at once (two workers starting) takes its turn on `db`'s lock;
/// and a layout version applied meanwhile (by a process without the lock,
/// or a lock not held: no `db`) is fine once the node is in it.
async fn join_layout(
    admin: &Admin,
    db: Option<&PgPool>,
    node: &str,
    info: &Value,
) -> anyhow::Result<()> {
    let mut conn = match db {
        Some(db) => {
            let mut c = db.acquire().await?;
            sqlx::query("SELECT pg_advisory_lock($1)")
                .bind(crate::bg::LOCK_BACKUP_STORE)
                .execute(&mut *c)
                .await?;
            Some(c)
        }
        None => None,
    };
    let joined = async {
        let current = admin.ok(layout()).await?;
        if in_layout(&current, node) {
            return Ok(());
        }
        // Capacity only weighs nodes against each other; with one, its disk.
        let capacity = info["dataPartition"]["total"]
            .as_u64()
            .filter(|c| *c > 0)
            .unwrap_or(1 << 40);
        admin.ok(add_node(node, capacity)).await?;
        match admin
            .send(apply_layout(current["version"].as_u64().unwrap_or(0) + 1))
            .await?
        {
            (200..300, _) => {}
            (status, v) => {
                if !in_layout(&admin.ok(layout()).await?, node) {
                    bail!(
                        "the backup store answered ApplyClusterLayout with {status}: {}",
                        v["message"].as_str().unwrap_or("")
                    );
                }
            }
        }
        tracing::info!(node = %node, "backup store: node added to the layout");
        anyhow::Ok(())
    }
    .await;
    if let Some(c) = conn.as_mut() {
        let _ = sqlx::query("SELECT pg_advisory_unlock($1)")
            .bind(crate::bg::LOCK_BACKUP_STORE)
            .execute(&mut **c)
            .await;
    }
    joined
}

/// Puts the node in the layout, then imports Pyo's key and makes its
/// bucket. Each step only when needed: safe to run at every start.
async fn bootstrap(
    admin: &Admin,
    db: Option<&PgPool>,
    key: &str,
    secret: &str,
) -> anyhow::Result<()> {
    let node = admin.ok(node_info()).await?;
    let (id, info) = node["success"]
        .as_object()
        .and_then(|m| m.iter().next())
        .ok_or_else(|| anyhow!("the backup store didn't say which node it is"))?;
    join_layout(admin, db, id, info).await?;
    match admin.send(key_info(key)).await? {
        (200, _) => {}
        (404, _) => match admin.send(import_key(key, secret)).await? {
            (200 | 409, _) => {}
            (s, v) => bail!(
                "couldn't import Pyo's key into the backup store ({s}: {})",
                v["message"].as_str().unwrap_or("")
            ),
        },
        (s, v) => bail!(
            "couldn't read Pyo's key in the backup store ({s}: {})",
            v["message"].as_str().unwrap_or("")
        ),
    }
    let bucket = admin.bucket(PYO_BUCKET).await?;
    admin.grant(&bucket, key, true).await
}

static READY: tokio::sync::OnceCell<()> = tokio::sync::OnceCell::const_new();

/// The store is set up (once per process; tried again after a failure).
pub async fn ready(db: &PgPool) -> anyhow::Result<()> {
    READY
        .get_or_try_init(|| async {
            let admin = admin().ok_or_else(|| anyhow!("The backup store's admin API (PYO_BACKUP_STORE_ADMIN_URL, PYO_BACKUP_STORE_ADMIN_TOKEN) isn't set."))?;
            let (_, key, secret) = crate::backup::local_store().ok_or_else(|| anyhow!("This installation has no backup store."))?;
            bootstrap(&admin, Some(db), &key, &secret).await
        })
        .await
        .map(|_| ())
}

/// At start: sets the store up, retrying until it answers.
pub async fn keeper(db: PgPool) {
    if admin().is_none() || crate::backup::local_store().is_none() {
        return;
    }
    let mut wait = 2;
    loop {
        match ready(&db).await {
            Ok(()) => return tracing::info!("backup store ready"),
            Err(e) => {
                tracing::warn!(error = %format!("{e:#}"), "backup store not ready; retrying");
                tokio::time::sleep(Duration::from_secs(wait)).await;
                wait = (wait * 2).min(60);
            }
        }
    }
}

/// The team's key to its bucket (access key, secret), made on first use
/// along with the bucket.
pub async fn team_login(db: &PgPool, team_id: &str) -> anyhow::Result<(String, String)> {
    let vault = crate::vault::get();
    let saved: Option<(String, String)> = sqlx::query_as(
        "SELECT access_key, secret_sealed FROM pyo.backup_store_logins WHERE team_id = $1",
    )
    .bind(team_id)
    .fetch_optional(db)
    .await?;
    if let Some((key, sealed)) = saved {
        return Ok((key, vault.open(&sealed).await?));
    }
    let admin = admin().ok_or_else(|| anyhow!("This installation has no backup store."))?;
    ready(db).await?;
    let name = team_bucket(team_id);
    let bucket = admin
        .bucket(&name)
        .await
        .context("couldn't make the team's bucket")?;
    let (key, secret) = admin
        .new_key(&name)
        .await
        .context("couldn't make the team's key")?;
    let saved = save_team_key(db, team_id, &admin, &bucket, &key, &secret).await;
    if !matches!(saved, Ok(true)) {
        // Not ours to keep: an error, or the other one won.
        if let Err(e) = admin.remove_key(&key).await {
            tracing::warn!(error = %e, "couldn't delete an unused backup store key");
        }
        saved?;
    }
    let (key, sealed): (String, String) = sqlx::query_as(
        "SELECT access_key, secret_sealed FROM pyo.backup_store_logins WHERE team_id = $1",
    )
    .bind(team_id)
    .fetch_one(db)
    .await?;
    Ok((key, vault.open(&sealed).await?))
}

/// Lets the new key into the team's bucket and saves it; false when another
/// was saved first (two at once: the first one wins).
async fn save_team_key(
    db: &PgPool,
    team_id: &str,
    admin: &Admin,
    bucket: &Value,
    key: &str,
    secret: &str,
) -> anyhow::Result<bool> {
    admin
        .grant(bucket, key, false)
        .await
        .context("couldn't let the team's key into its bucket")?;
    let sealed = crate::vault::get().seal(team_id, secret).await?;
    let r = sqlx::query("INSERT INTO pyo.backup_store_logins (team_id, access_key, secret_sealed) VALUES ($1, $2, $3) ON CONFLICT (team_id) DO NOTHING")
        .bind(team_id)
        .bind(key)
        .bind(sealed)
        .execute(db)
        .await?;
    Ok(r.rows_affected() == 1)
}

/// What the store has that no team has any more: (keys, buckets) by id.
/// A team's key that was never saved is a leftover once it's old enough.
fn orphans(
    keys: &[Value],
    buckets: &[Value],
    teams: &HashSet<String>,
    saved: &HashSet<String>,
    now: jiff::Timestamp,
) -> (Vec<String>, Vec<Value>) {
    let dead_keys = keys
        .iter()
        .filter(|k| {
            let name = k["name"].as_str().unwrap_or("");
            let id = k["id"].as_str().unwrap_or("");
            if !name.starts_with(TEAM_PREFIX) || id.is_empty() {
                return false;
            }
            let old = k["created"]
                .as_str()
                .and_then(|c| c.parse::<jiff::Timestamp>().ok())
                .is_some_and(|c| now.duration_since(c).unsigned_abs() > STRAY_AFTER);
            !teams.contains(name) || (!saved.contains(id) && old)
        })
        .filter_map(|k| k["id"].as_str().map(str::to_owned))
        .collect();
    let dead_buckets = buckets
        .iter()
        .filter(|b| {
            b["globalAliases"].as_array().is_some_and(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .any(|a| a.starts_with(TEAM_PREFIX) && !teams.contains(a))
            })
        })
        .cloned()
        .collect();
    (dead_keys, dead_buckets)
}

/// Removes deleted teams' keys and buckets (their backups with them), and
/// keys made but never saved (team-backup runner).
pub async fn sweep(db: &PgPool) -> anyhow::Result<()> {
    let Some(admin) = admin() else { return Ok(()) };
    if crate::backup::local_store().is_none() {
        return Ok(());
    }
    ready(db).await?;
    // The store first, then the teams: a bucket made meanwhile is a live team's.
    let keys = admin.ok(list_keys()).await?;
    let buckets = admin.ok(list_buckets()).await?;
    let teams: Vec<String> = sqlx::query_scalar("SELECT id FROM pyo.teams")
        .fetch_all(db)
        .await?;
    let teams: HashSet<String> = teams.iter().map(|t| team_bucket(t)).collect();
    let saved: Vec<String> = sqlx::query_scalar("SELECT access_key FROM pyo.backup_store_logins")
        .fetch_all(db)
        .await?;
    let (dead_keys, dead_buckets) = orphans(
        keys.as_array().map_or(&[][..], |v| v),
        buckets.as_array().map_or(&[][..], |v| v),
        &teams,
        &saved.into_iter().collect(),
        jiff::Timestamp::now(),
    );
    remove(&admin, &dead_keys, &dead_buckets).await
}

/// Deletes these keys, then empties and deletes these buckets.
async fn remove(admin: &Admin, dead_keys: &[String], dead_buckets: &[Value]) -> anyhow::Result<()> {
    for k in dead_keys {
        admin.remove_key(k).await?;
        tracing::info!(key = %k, "backup store: removed a key no team uses");
    }
    for b in dead_buckets {
        let id = b["id"].as_str().unwrap_or("");
        let alias = b["globalAliases"][0].as_str().unwrap_or("");
        purge(admin, id, alias)
            .await
            .with_context(|| format!("couldn't remove the bucket {alias}"))?;
        tracing::info!(bucket = %alias, "backup store: removed a deleted team's bucket");
    }
    Ok(())
}

/// Empties a bucket (with Pyo's key, let in for it) and deletes it.
async fn purge(admin: &Admin, id: &str, alias: &str) -> anyhow::Result<()> {
    let (_, key, _) = crate::backup::local_store()
        .ok_or_else(|| anyhow!("This installation has no backup store."))?;
    admin.ok(allow(id, &key, true)).await?;
    crate::backup::empty_local_bucket(alias).await?;
    admin.ok(cleanup_uploads(id)).await?;
    match admin.send(delete_bucket(id)).await? {
        (200 | 204 | 404, _) => Ok(()),
        (s, v) => bail!("{s}: {}", v["message"].as_str().unwrap_or("")),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use axum::{
        Json, Router,
        extract::{Query, State},
        http::{HeaderMap, Method as HttpMethod, StatusCode, Uri},
    };

    use super::*;

    #[test]
    fn names() {
        assert_eq!(team_bucket("team_Ab3x"), "pyo-team-team-ab3x");
    }

    #[test]
    fn request_shapes() {
        assert_eq!(
            add_node("n1", 5),
            Call {
                method: Method::POST,
                endpoint: "UpdateClusterLayout",
                query: vec![],
                body: Some(
                    json!({ "roles": [{ "id": "n1", "zone": "pyo", "capacity": 5, "tags": [] }] })
                ),
            }
        );
        assert_eq!(apply_layout(3).body, Some(json!({ "version": 3 })));
        assert_eq!(
            import_key("GK1", "s").body,
            Some(json!({ "accessKeyId": "GK1", "secretAccessKey": "s", "name": "pyo" }))
        );
        assert_eq!(
            create_bucket("pyo-backups").body,
            Some(json!({ "globalAlias": "pyo-backups" }))
        );
        assert_eq!(
            bucket_info("pyo-backups").query,
            vec![("globalAlias", "pyo-backups".to_owned())]
        );
        let team = allow("b1", "GK2", false);
        assert_eq!(team.endpoint, "AllowBucketKey");
        assert_eq!(
            team.body,
            Some(
                json!({ "bucketId": "b1", "accessKeyId": "GK2", "permissions": { "read": true, "write": true, "owner": false } })
            ),
            "a team's key never owns its bucket"
        );
        assert_eq!(
            delete_key("GK2"),
            Call {
                method: Method::POST,
                endpoint: "DeleteKey",
                query: vec![("id", "GK2".to_owned())],
                body: None
            }
        );
        assert_eq!(delete_bucket("b1").query, vec![("id", "b1".to_owned())]);
        assert_eq!(
            cleanup_uploads("b1").body,
            Some(json!({ "bucketId": "b1", "olderThanSecs": 0 }))
        );
        assert_eq!(node_info().query, vec![("node", "self".to_owned())]);
    }

    #[test]
    fn access() {
        let b = json!({ "id": "b", "keys": [{ "accessKeyId": "GK1", "permissions": { "read": true, "write": true, "owner": false } }] });
        assert!(has_access(&b, "GK1", false));
        assert!(!has_access(&b, "GK1", true), "not the owner yet");
        assert!(!has_access(&b, "GK2", false));
        assert!(!has_access(&json!({ "id": "b", "keys": [] }), "GK1", false));
    }

    #[test]
    fn leftovers() {
        let now: jiff::Timestamp = "2026-11-12T12:00:00Z".parse().unwrap();
        let key = |id: &str, name: &str, created: &str| json!({ "id": id, "name": name, "created": created });
        let keys = [
            key("GKpyo", "pyo", "2026-01-01T00:00:00Z"),
            key("GKa", "pyo-team-team-a", "2026-11-01T00:00:00Z"),
            key("GKa2", "pyo-team-team-a", "2026-11-01T00:00:00Z"),
            key("GKa3", "pyo-team-team-a", "2026-11-12T11:59:00Z"),
            key("GKgone", "pyo-team-team-gone", "2026-11-12T11:59:00Z"),
            key("GKother", "someone", "2026-01-01T00:00:00Z"),
        ];
        let buckets = [
            json!({ "id": "1", "globalAliases": ["pyo-backups"] }),
            json!({ "id": "2", "globalAliases": ["pyo-team-team-a"] }),
            json!({ "id": "3", "globalAliases": ["pyo-team-team-gone"] }),
            json!({ "id": "4", "globalAliases": [] }),
        ];
        let teams: HashSet<String> = [team_bucket("team_a")].into();
        let saved: HashSet<String> = ["GKa".to_owned()].into();
        let (k, b) = orphans(&keys, &buckets, &teams, &saved, now);
        assert_eq!(
            k,
            ["GKa2", "GKgone"],
            "a deleted team's, and an old unsaved one; not one being made, nor Pyo's"
        );
        assert_eq!(
            b.iter()
                .map(|b| b["id"].as_str().unwrap())
                .collect::<Vec<_>>(),
            ["3"]
        );
    }

    /// Just enough of Garage's admin API for the bootstrap and team keys.
    #[derive(Default)]
    struct Fake {
        node: String,
        version: u64,
        roles: Vec<Value>,
        staged: Vec<Value>,
        stale_reads: u64,
        keys: Vec<Value>,
        buckets: Vec<Value>,
        calls: Vec<String>,
    }

    async fn fake(
        State(s): State<Arc<Mutex<Fake>>>,
        method: HttpMethod,
        uri: Uri,
        headers: HeaderMap,
        Query(q): Query<std::collections::HashMap<String, String>>,
        body: String,
    ) -> (StatusCode, Json<Value>) {
        let mut f = s.lock().unwrap();
        if headers.get("authorization").and_then(|v| v.to_str().ok()) != Some("Bearer tok") {
            return (StatusCode::FORBIDDEN, Json(json!({ "code": "Forbidden" })));
        }
        let ep = uri.path().trim_start_matches("/v2/").to_owned();
        f.calls.push(format!("{method} {ep}"));
        let b: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
        let not_found = (StatusCode::NOT_FOUND, Json(json!({ "code": "NotFound" })));
        match ep.as_str() {
            "GetNodeInfo" => (
                StatusCode::OK,
                Json(
                    json!({ "success": { f.node.clone(): { "nodeId": f.node, "dataPartition": { "total": 1000 } } }, "error": {} }),
                ),
            ),
            // What another process saw before it applied the next version.
            "GetClusterLayout" if f.stale_reads > 0 => {
                f.stale_reads -= 1;
                (
                    StatusCode::OK,
                    Json(json!({ "version": f.version - 1, "roles": [] })),
                )
            }
            "GetClusterLayout" => (
                StatusCode::OK,
                Json(json!({ "version": f.version, "roles": f.roles })),
            ),
            "UpdateClusterLayout" => {
                f.staged = b["roles"].as_array().unwrap().clone();
                (StatusCode::OK, Json(json!({})))
            }
            "ApplyClusterLayout" => {
                // As Garage: only the next version.
                if b["version"] != f.version + 1 {
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(
                            json!({ "code": "InternalError", "message": "Invalid new layout version" }),
                        ),
                    );
                }
                f.version += 1;
                f.roles = std::mem::take(&mut f.staged);
                (StatusCode::OK, Json(json!({})))
            }
            "GetKeyInfo" => match f.keys.iter().find(|k| k["accessKeyId"] == q["id"].as_str()) {
                Some(k) => (StatusCode::OK, Json(k.clone())),
                None => not_found,
            },
            "ImportKey" => {
                f.keys
                    .push(json!({ "accessKeyId": b["accessKeyId"], "name": b["name"] }));
                (StatusCode::OK, Json(json!({})))
            }
            "CreateKey" => {
                let id = format!("GK{}", f.keys.len());
                f.keys.push(json!({ "accessKeyId": id, "name": b["name"] }));
                (
                    StatusCode::OK,
                    Json(json!({ "accessKeyId": id, "secretAccessKey": "sec" })),
                )
            }
            "GetBucketInfo" => match f
                .buckets
                .iter()
                .find(|x| x["globalAliases"][0] == q["globalAlias"].as_str())
            {
                Some(x) => (StatusCode::OK, Json(x.clone())),
                None => not_found,
            },
            "CreateBucket" => {
                let x = json!({ "id": format!("b{}", f.buckets.len()), "globalAliases": [b["globalAlias"]], "keys": [] });
                f.buckets.push(x.clone());
                (StatusCode::OK, Json(x))
            }
            "AllowBucketKey" => {
                let x = f
                    .buckets
                    .iter_mut()
                    .find(|x| x["id"] == b["bucketId"])
                    .unwrap();
                x["keys"].as_array_mut().unwrap().push(
                    json!({ "accessKeyId": b["accessKeyId"], "permissions": b["permissions"] }),
                );
                (StatusCode::OK, Json(x.clone()))
            }
            _ => not_found,
        }
    }

    async fn serve() -> (Admin, Arc<Mutex<Fake>>) {
        let state = Arc::new(Mutex::new(Fake {
            node: "n1".into(),
            ..Default::default()
        }));
        let app = Router::new().fallback(fake).with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (Admin::new(&format!("http://{addr}/"), "tok"), state)
    }

    #[tokio::test]
    async fn bootstrap_once_then_nothing_to_do() {
        let (admin, state) = serve().await;
        bootstrap(&admin, None, "GKpyo", "secret").await.unwrap();
        {
            let mut f = state.lock().unwrap();
            assert_eq!(
                f.calls,
                [
                    "GET GetNodeInfo",
                    "GET GetClusterLayout",
                    "POST UpdateClusterLayout",
                    "POST ApplyClusterLayout",
                    "GET GetKeyInfo",
                    "POST ImportKey",
                    "GET GetBucketInfo",
                    "POST CreateBucket",
                    "POST AllowBucketKey"
                ]
            );
            assert_eq!(
                f.roles,
                [json!({ "id": "n1", "zone": "pyo", "capacity": 1000, "tags": [] })]
            );
            assert!(
                has_access(&f.buckets[0], "GKpyo", true),
                "Pyo owns pyo-backups"
            );
            f.calls.clear();
        }
        // A restart: only reads.
        bootstrap(&admin, None, "GKpyo", "secret").await.unwrap();
        let f = state.lock().unwrap();
        assert_eq!(
            f.calls,
            [
                "GET GetNodeInfo",
                "GET GetClusterLayout",
                "GET GetKeyInfo",
                "GET GetBucketInfo"
            ]
        );
        assert_eq!(f.version, 1);
        assert_eq!(f.keys.len(), 1);
    }

    /// Against a real Garage, with PYO_BACKUP_STORE_* set (e.g. through
    /// `kubectl port-forward`): sets it up twice at once and again, then
    /// makes teams A and B a bucket and key each, written to
    /// $GARAGE_E2E_OUT for other tests.
    #[tokio::test]
    #[ignore]
    async fn garage_e2e_setup() {
        let admin = admin().expect("PYO_BACKUP_STORE_ADMIN_*");
        let (_, key, secret) = crate::backup::local_store().expect("PYO_BACKUP_STORE_*");
        // Two processes at once (no lock between them), then a restart.
        let (a, b) = tokio::join!(
            bootstrap(&admin, None, &key, &secret),
            bootstrap(&admin, None, &key, &secret)
        );
        a.unwrap();
        b.unwrap();
        bootstrap(&admin, None, &key, &secret).await.unwrap();
        let mut out = serde_json::Map::new();
        for t in ["team_a", "team_b"] {
            let name = team_bucket(t);
            let bucket = admin.bucket(&name).await.unwrap();
            let (k, s) = admin.new_key(&name).await.unwrap();
            admin.grant(&bucket, &k, false).await.unwrap();
            out.insert(t.into(), json!({ "bucket": name, "key": k, "secret": s }));
        }
        std::fs::write(
            std::env::var("GARAGE_E2E_OUT").unwrap(),
            Value::Object(out).to_string(),
        )
        .unwrap();
    }

    /// Then team B is deleted: what [`sweep`] removes, and that it's gone.
    #[tokio::test]
    #[ignore]
    async fn garage_e2e_remove_team() {
        let admin = admin().unwrap();
        let keys = admin.ok(list_keys()).await.unwrap();
        let buckets = admin.ok(list_buckets()).await.unwrap();
        let teams: HashSet<String> = [team_bucket("team_a")].into();
        let (k, b) = orphans(
            keys.as_array().unwrap(),
            buckets.as_array().unwrap(),
            &teams,
            &HashSet::new(),
            jiff::Timestamp::now(),
        );
        assert_eq!(k.len(), 1, "team B's key: {k:?}");
        assert_eq!(b.len(), 1, "team B's bucket");
        let before = admin.ok(bucket_info(&team_bucket("team_b"))).await.unwrap();
        println!(
            "team B's bucket before: {} objects, {} bytes",
            before["objects"], before["bytes"]
        );
        remove(&admin, &k, &b).await.unwrap();
        assert_eq!(
            admin
                .send(bucket_info(&team_bucket("team_b")))
                .await
                .unwrap()
                .0,
            404
        );
        assert_eq!(admin.send(key_info(&k[0])).await.unwrap().0, 404);
        assert_eq!(
            admin
                .send(bucket_info(&team_bucket("team_a")))
                .await
                .unwrap()
                .0,
            200,
            "team A keeps its own"
        );
        assert_eq!(admin.send(bucket_info(PYO_BUCKET)).await.unwrap().0, 200);
    }

    /// With Pyo's database too (DATABASE_URL, a throwaway one): a team's
    /// key is made once, even asked for twice at once, and [`sweep`]
    /// removes a deleted team's key and bucket (any other `pyo-team-*` the
    /// database doesn't know of as well).
    #[tokio::test]
    #[ignore = "needs DATABASE_URL and a Garage (PYO_BACKUP_STORE_*)"]
    async fn garage_e2e_team_lifecycle() {
        let db = PgPool::connect(&std::env::var("DATABASE_URL").unwrap())
            .await
            .unwrap();
        crate::migrate::run(&db).await.unwrap();
        crate::vault::init(crate::vault::Vault::new(
            db.clone(),
            Some(crate::vault::Provider::Local {
                current: zeroize::Zeroizing::new([6u8; 32]),
                previous: None,
            }),
        ));
        let run = crate::ids::new_id("t").to_lowercase();
        let (x, y) = (format!("tx_{run}"), format!("ty_{run}"));
        for t in [&x, &y] {
            sqlx::query("INSERT INTO pyo.teams (id, name, slug) VALUES ($1, $1, $1)")
                .bind(t)
                .execute(&db)
                .await
                .unwrap();
        }
        let first = team_login(&db, &x).await.unwrap();
        assert_eq!(team_login(&db, &x).await.unwrap(), first, "made once");
        let (a, b) = tokio::join!(team_login(&db, &y), team_login(&db, &y));
        assert_eq!(a.unwrap(), b.unwrap(), "two at once: one key");
        let admin = admin().unwrap();
        let keys = admin.ok(list_keys()).await.unwrap();
        let named = |name: String| {
            keys.as_array()
                .unwrap()
                .iter()
                .filter(|k| k["name"] == name.as_str())
                .count()
        };
        assert_eq!(named(team_bucket(&y)), 1, "the losing key was deleted");
        sqlx::query("DELETE FROM pyo.teams WHERE id = $1")
            .bind(&y)
            .execute(&db)
            .await
            .unwrap();
        sweep(&db).await.unwrap();
        assert_eq!(
            admin.send(bucket_info(&team_bucket(&y))).await.unwrap().0,
            404,
            "the deleted team's bucket"
        );
        let keys = admin.ok(list_keys()).await.unwrap();
        assert!(
            !keys
                .as_array()
                .unwrap()
                .iter()
                .any(|k| k["name"] == team_bucket(&y).as_str()),
            "and key"
        );
        let x_bucket = admin.ok(bucket_info(&team_bucket(&x))).await.unwrap();
        assert!(
            has_access(&x_bucket, &first.0, false),
            "the other team keeps its own"
        );
        sqlx::query("DELETE FROM pyo.teams WHERE id = $1")
            .bind(&x)
            .execute(&db)
            .await
            .unwrap();
        sweep(&db).await.unwrap();
    }

    /// Another process applied the next layout version between our read
    /// and our apply: its "Invalid new layout version" is fine once the
    /// node is in the layout, and an error when it isn't.
    #[tokio::test]
    async fn a_layout_applied_meanwhile() {
        let node = json!({ "dataPartition": { "total": 1000 } });
        for (applied_with_us, ok) in [(true, true), (false, false)] {
            let (admin, state) = serve().await;
            {
                let mut f = state.lock().unwrap();
                f.version = 1;
                f.roles = if applied_with_us {
                    vec![json!({ "id": "n1", "zone": "pyo", "capacity": 1000, "tags": [] })]
                } else {
                    vec![]
                };
                f.stale_reads = 1;
            }
            let r = join_layout(&admin, None, "n1", &node).await;
            assert_eq!(r.is_ok(), ok, "{r:?}");
            if !ok {
                assert!(format!("{:#}", r.unwrap_err()).contains("Invalid new layout version"));
            }
        }
    }

    /// Two processes at once, against the same store: both succeed, and the
    /// node is in the layout once.
    #[tokio::test]
    async fn concurrent_setups_both_succeed() {
        let (admin, state) = serve().await;
        let (a, b) = (admin.clone(), admin.clone());
        let (x, y) = tokio::join!(
            bootstrap(&a, None, "GKpyo", "secret"),
            bootstrap(&b, None, "GKpyo", "secret")
        );
        x.unwrap();
        y.unwrap();
        let f = state.lock().unwrap();
        assert_eq!(f.roles.len(), 1);
    }

    #[tokio::test]
    async fn wrong_token_is_an_error() {
        let (admin, _) = serve().await;
        let wrong = Admin {
            token: "nope".into(),
            ..admin
        };
        let err = bootstrap(&wrong, None, "GKpyo", "s").await.unwrap_err();
        assert!(format!("{err:#}").contains("403"), "{err:#}");
    }

    #[tokio::test]
    async fn a_team_key_reaches_its_bucket_only() {
        let (admin, state) = serve().await;
        bootstrap(&admin, None, "GKpyo", "secret").await.unwrap();
        let bucket = admin.bucket(&team_bucket("team_a")).await.unwrap();
        let (key, _) = admin.new_key(&team_bucket("team_a")).await.unwrap();
        admin.grant(&bucket, &key, false).await.unwrap();
        let again = admin.bucket(&team_bucket("team_a")).await.unwrap();
        assert_eq!(again["id"], bucket["id"], "the same bucket the second time");
        let f = state.lock().unwrap();
        let team = f
            .buckets
            .iter()
            .find(|b| b["globalAliases"][0] == "pyo-team-team-a")
            .unwrap();
        assert_eq!(
            team["keys"],
            json!([{ "accessKeyId": key, "permissions": { "read": true, "write": true, "owner": false } }])
        );
        let pyo = f
            .buckets
            .iter()
            .find(|b| b["globalAliases"][0] == PYO_BUCKET)
            .unwrap();
        assert!(!has_access(pyo, &key, false), "not Pyo's bucket");
    }
}
