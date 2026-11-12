//! Backups of Pyo's own data: teams, projects, services, settings,
//! deployment history and activity (every table in the `pyo` schema but
//! sessions), to any S3-compatible storage.
//!
//! An archive is the tables as Postgres `COPY` text, gzipped, then encrypted
//! with secretbox (XSalsa20-Poly1305) in 1 MiB chunks under a key derived from
//! PYO_SECRET_KEY. That key is needed to restore anyway: the secrets inside
//! are sealed with it. Each chunk's nonce carries its index and whether it's
//! the last, so chunks can't be reordered, dropped or cut short unnoticed.
//!
//! File: `PYB1` + 16-byte random nonce prefix, then chunks of
//! (u32 big-endian length, ciphertext).

use std::{io::Write, time::Duration};

use anyhow::{Context, anyhow, bail, ensure};
use crypto_secretbox::{
    Key, Nonce, XSalsa20Poly1305,
    aead::{Aead, KeyInit},
};
use futures::StreamExt;
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::PgPool;

pub use crate::backupstore::{team_bucket, team_login};
use crate::{
    AppState,
    backupstore::{PYO_BUCKET, REGION},
    error::ApiResult,
    ids::new_id,
    outbound::Reach,
};

const MAGIC: &[u8; 4] = b"PYB1";
const CHUNK: usize = 1 << 20;
/// Tables a restore doesn't want: sign-ins start over.
const SKIP: &[&str] = &["sessions", "_sqlx_migrations"];

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct Settings {
    /// "local": the backup store on this cluster (the default); "s3": a bucket of yours.
    #[serde(default = "local")]
    pub destination: String,
    pub enabled: bool,
    /// "daily" or "weekly", at `hour` (UTC).
    pub schedule: String,
    pub hour: u8,
    /// Backups kept; older ones are deleted.
    pub keep: u32,
    pub endpoint: String,
    pub region: String,
    pub bucket: String,
    pub prefix: String,
    pub access_key: String,
    #[serde(default)]
    pub secret_sealed: Option<String>,
}

fn local() -> String {
    "local".into()
}

/// The backup store on this cluster (platform.yaml): its URL and keys.
pub fn local_store() -> Option<(String, String, String)> {
    let var = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
    Some((
        var("PYO_BACKUP_STORE_URL")?,
        var("PYO_BACKUP_STORE_ACCESS_KEY")?,
        var("PYO_BACKUP_STORE_SECRET_KEY")?,
    ))
}

impl Settings {
    /// Backups to this cluster's store, daily at 02:00 UTC, keeping 14.
    pub fn local_default(enabled: bool) -> Self {
        Self {
            destination: local(),
            enabled,
            schedule: "daily".into(),
            hour: 2,
            keep: 14,
            ..Default::default()
        }
    }

    pub fn is_local(&self) -> bool {
        self.destination != "s3"
    }

    /// Where backups actually go: for "local", the store's address and key.
    pub fn resolved(&self) -> anyhow::Result<Self> {
        if !self.is_local() {
            return Ok(self.clone());
        }
        let (url, key, _) = local_store().ok_or_else(|| {
            anyhow!(
                "This installation has no backup store; back up to an S3-compatible bucket instead."
            )
        })?;
        Ok(Self {
            endpoint: url,
            region: REGION.into(),
            bucket: PYO_BUCKET.into(),
            access_key: key,
            secret_sealed: None,
            ..self.clone()
        })
    }

    /// What the dashboard sees: everything but the secret key.
    pub fn public(&self) -> Value {
        json!({
            "destination": if self.is_local() { "local" } else { "s3" },
            "localAvailable": local_store().is_some(),
            "enabled": self.enabled, "schedule": self.schedule, "hour": self.hour, "keep": self.keep,
            "endpoint": self.endpoint, "region": self.region, "bucket": self.bucket, "prefix": self.prefix,
            "accessKey": self.access_key, "hasSecret": self.secret_sealed.is_some(),
        })
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        ensure!(
            matches!(self.destination.as_str(), "local" | "s3"),
            "Pick this server or an S3-compatible bucket."
        );
        if self.is_local() {
            ensure!(
                local_store().is_some(),
                "This installation has no backup store; back up to an S3-compatible bucket instead."
            );
        } else {
            ensure!(
                self.endpoint.starts_with("https://") || self.endpoint.starts_with("http://"),
                "The endpoint is a URL, e.g. https://s3.us-east-1.amazonaws.com."
            );
            ensure!(!self.bucket.trim().is_empty(), "Enter the bucket.");
            ensure!(!self.access_key.trim().is_empty(), "Enter the access key.");
        }
        ensure!(
            matches!(self.schedule.as_str(), "daily" | "weekly"),
            "Pick daily or weekly."
        );
        ensure!(self.hour < 24, "Pick an hour from 0 to 23.");
        ensure!(
            (1..=365).contains(&self.keep),
            "Keep between 1 and 365 backups."
        );
        Ok(())
    }
}

pub async fn settings(db: &PgPool) -> ApiResult<Option<Settings>> {
    let v: Option<Value> =
        sqlx::query_scalar("SELECT value FROM pyo.settings WHERE key = 'backup'")
            .fetch_optional(db)
            .await?;
    Ok(v.and_then(|v| serde_json::from_value(v).ok()))
}

// ── Archive ────────────────────────────────────────────────────────────────

fn archive_key() -> anyhow::Result<[u8; 32]> {
    labeled_key(b"pyo-backup-v1")
}

fn labeled_key(label: &[u8]) -> anyhow::Result<[u8; 32]> {
    let k = hex::decode(
        std::env::var("PYO_SECRET_KEY")
            .context("PYO_SECRET_KEY isn't set")?
            .trim(),
    )?;
    ensure!(k.len() == 32, "PYO_SECRET_KEY must be 64 hex characters");
    let mut h = Sha256::new();
    h.update(label);
    h.update(&k);
    Ok(h.finalize().into())
}

/// A backup's file name: `pyo-<stamp>-<id>.pyobak`.
const EXT: &str = ".pyobak";

/// The `<stamp>-<id>` of a backup's object key (which sorts by time), or
/// None if the key isn't a backup.
fn backup_stamp(key: &str) -> Option<&str> {
    let name = key.rsplit('/').next().unwrap_or(key);
    name.strip_suffix(EXT).and_then(|n| n.strip_prefix("pyo-"))
}

fn chunk_nonce(prefix: &[u8; 16], index: u64, last: bool) -> [u8; 24] {
    let mut n = [0u8; 24];
    n[..16].copy_from_slice(prefix);
    let mut i = index;
    if last {
        i |= 1 << 63;
    }
    n[16..].copy_from_slice(&i.to_be_bytes());
    n
}

pub fn encrypt(key: &[u8; 32], plain: &[u8]) -> anyhow::Result<Vec<u8>> {
    let cipher = XSalsa20Poly1305::new(&Key::from(*key));
    let mut prefix = [0u8; 16];
    rand::fill(&mut prefix);
    let mut out = MAGIC.to_vec();
    out.extend(prefix);
    let chunks: Vec<&[u8]> = if plain.is_empty() {
        vec![&[][..]]
    } else {
        plain.chunks(CHUNK).collect()
    };
    for (i, c) in chunks.iter().enumerate() {
        let nonce = chunk_nonce(&prefix, i as u64, i + 1 == chunks.len());
        let sealed = cipher
            .encrypt(&Nonce::from(nonce), *c)
            .map_err(|_| anyhow!("encryption failed"))?;
        out.extend((sealed.len() as u32).to_be_bytes());
        out.extend(sealed);
    }
    Ok(out)
}

/// `pyo-api decrypt-backup <file.pyobak> <out>`: the archive in the clear
/// (COPY text per table), with only PYO_SECRET_KEY and no running Pyo.
pub fn decrypt_file(input: &str, output: &str) -> anyhow::Result<()> {
    let sealed = std::fs::read(input).with_context(|| format!("can't read {input}"))?;
    let gz = decrypt(&archive_key()?, &sealed)?;
    let mut plain = Vec::new();
    std::io::Read::read_to_end(&mut flate2::read::GzDecoder::new(&gz[..]), &mut plain)?;
    std::fs::write(output, &plain).with_context(|| format!("can't write {output}"))?;
    let header = plain.split(|b| *b == b'\n').next().unwrap_or_default();
    println!("Decrypted to {output}: {}", String::from_utf8_lossy(header));
    Ok(())
}

pub fn decrypt(key: &[u8; 32], data: &[u8]) -> anyhow::Result<Vec<u8>> {
    ensure!(data.len() >= 20 && &data[..4] == MAGIC, "not a Pyo backup");
    let cipher = XSalsa20Poly1305::new(&Key::from(*key));
    let prefix: [u8; 16] = data[4..20].try_into()?;
    let (mut pos, mut index, mut out) = (20usize, 0u64, Vec::new());
    loop {
        ensure!(pos + 4 <= data.len(), "backup is cut short");
        let len = u32::from_be_bytes(data[pos..pos + 4].try_into()?) as usize;
        pos += 4;
        ensure!(pos + len <= data.len(), "backup is cut short");
        let body = &data[pos..pos + len];
        pos += len;
        let last = pos == data.len();
        let plain = cipher
            .decrypt(&Nonce::from(chunk_nonce(&prefix, index, last)), body)
            .map_err(|_| {
                anyhow!("can't decrypt (wrong PYO_SECRET_KEY, or the file was changed)")
            })?;
        out.extend(plain);
        if last {
            return Ok(out);
        }
        index += 1;
    }
}

/// The tables as COPY text: a JSON header line, then for each table a
/// `TABLE <name> <bytes>` line and its rows.
async fn export(db: &PgPool) -> anyhow::Result<(Vec<u8>, usize)> {
    let tables: Vec<String> = sqlx::query_scalar(
        "SELECT tablename::text FROM pg_tables WHERE schemaname = 'pyo' ORDER BY tablename",
    )
    .fetch_all(db)
    .await?;
    let tables: Vec<String> = tables
        .into_iter()
        .filter(|t| !SKIP.contains(&t.as_str()))
        .collect();
    let migration: Option<i64> =
        sqlx::query_scalar("SELECT max(version) FROM _sqlx_migrations WHERE success")
            .fetch_one(db)
            .await
            .ok()
            .flatten();
    let mut out = Vec::new();
    writeln!(
        out,
        "{}",
        json!({ "format": "pyo-backup", "version": 1, "createdAt": pyo_proto::now_ms(), "migration": migration, "tables": tables })
    )?;
    // One snapshot of every table, so they agree with each other.
    let mut tx = db.begin().await?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY")
        .execute(&mut *tx)
        .await?;
    for t in &tables {
        let mut rows = Vec::new();
        {
            let mut stream = tx
                .copy_out_raw(&format!("COPY pyo.\"{t}\" TO STDOUT"))
                .await?;
            while let Some(chunk) = stream.next().await {
                rows.extend_from_slice(&chunk?);
            }
        }
        writeln!(out, "TABLE {t} {}", rows.len())?;
        out.extend(rows);
    }
    tx.commit().await?;
    Ok((out, tables.len()))
}

fn gzip(data: &[u8]) -> anyhow::Result<Vec<u8>> {
    let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    e.write_all(data)?;
    Ok(e.finish()?)
}

// ── S3 (Signature Version 4) ───────────────────────────────────────────────

struct S3<'a> {
    cfg: &'a Settings,
    secret: String,
    http: reqwest::Client,
    /// Where a bucket someone entered may be (`crate::outbound`); None for
    /// the store on this cluster.
    reach: Option<Reach>,
}

fn hmac(key: &[u8], msg: &str) -> Vec<u8> {
    let mut m = <Hmac<Sha256> as hmac::KeyInit>::new_from_slice(key).expect("any key length");
    m.update(msg.as_bytes());
    m.finalize().into_bytes().to_vec()
}

/// RFC 3986 encoding, as SigV4 wants it ('/' kept in paths).
pub(crate) fn enc(s: &str, path: bool) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            b'/' if path => "/".into(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// A request to sign with SigV4 (path and query already encoded).
pub(crate) struct Request<'a> {
    pub(crate) method: &'a str,
    pub(crate) host: &'a str,
    pub(crate) path: &'a str,
    pub(crate) query: &'a str,
    pub(crate) body: &'a [u8],
    pub(crate) service: &'a str,
    pub(crate) region: &'a str,
    pub(crate) access_key: &'a str,
    pub(crate) secret: &'a str,
}

pub(crate) struct Signed {
    pub(crate) amz_date: String,
    pub(crate) payload_hash: String,
    pub(crate) authorization: String,
}

/// SigV4 over host, x-amz-content-sha256 and x-amz-date.
pub(crate) fn sigv4(r: &Request) -> Signed {
    let amz_date = stamp(pyo_proto::now_ms());
    let date = amz_date[..8].to_owned();
    let payload_hash = hex::encode(Sha256::digest(r.body));
    let canonical = format!(
        "{}\n{}\n{}\nhost:{}\nx-amz-content-sha256:{payload_hash}\nx-amz-date:{amz_date}\n\nhost;x-amz-content-sha256;x-amz-date\n{payload_hash}",
        r.method, r.path, r.query, r.host
    );
    let scope = format!("{date}/{}/{}/aws4_request", r.region, r.service);
    let to_sign = format!(
        "AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{}",
        hex::encode(Sha256::digest(canonical.as_bytes()))
    );
    let k = hmac(format!("AWS4{}", r.secret).as_bytes(), &date);
    let k = hmac(&k, r.region);
    let k = hmac(&k, r.service);
    let k = hmac(&k, "aws4_request");
    let signature = hex::encode(hmac(&k, &to_sign));
    let authorization = format!(
        "AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders=host;x-amz-content-sha256;x-amz-date, Signature={signature}",
        r.access_key
    );
    Signed {
        amz_date,
        payload_hash,
        authorization,
    }
}

impl S3<'_> {
    async fn send(
        &self,
        method: reqwest::Method,
        key: &str,
        query: &[(&str, &str)],
        body: Vec<u8>,
    ) -> anyhow::Result<reqwest::Response> {
        let base = reqwest::Url::parse(self.cfg.endpoint.trim_end_matches('/'))?;
        let host = match base.port() {
            Some(p) => format!("{}:{p}", base.host_str().unwrap_or("")),
            None => base.host_str().unwrap_or("").to_owned(),
        };
        // Path-style: works with AWS, R2, B2, MinIO alike.
        let path = if key.is_empty() {
            format!("{}/{}", base.path().trim_end_matches('/'), self.cfg.bucket)
        } else {
            format!(
                "{}/{}/{}",
                base.path().trim_end_matches('/'),
                self.cfg.bucket,
                key
            )
        };
        let path = enc(&path.replace("//", "/"), true);
        let mut q: Vec<(String, String)> = query
            .iter()
            .map(|(k, v)| (enc(k, false), enc(v, false)))
            .collect();
        q.sort();
        let canonical_query = q
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join("&");
        let region = if self.cfg.region.trim().is_empty() {
            "us-east-1"
        } else {
            self.cfg.region.trim()
        };
        let signed = sigv4(&Request {
            method: method.as_str(),
            host: &host,
            path: &path,
            query: &canonical_query,
            body: &body,
            service: "s3",
            region,
            access_key: self.cfg.access_key.trim(),
            secret: &self.secret,
        });
        let url = format!(
            "{}://{host}{path}{}",
            base.scheme(),
            if canonical_query.is_empty() {
                String::new()
            } else {
                format!("?{canonical_query}")
            }
        );
        if let Some(reach) = self.reach {
            crate::outbound::check(&reqwest::Url::parse(&url)?, reach).await?;
        }
        let res = self
            .http
            .request(method, url)
            .header("x-amz-date", &signed.amz_date)
            .header("x-amz-content-sha256", &signed.payload_hash)
            .header("authorization", signed.authorization)
            .body(body)
            .timeout(Duration::from_secs(600))
            .send()
            .await?;
        if !res.status().is_success() {
            let status = res.status();
            let text = res.text().await.unwrap_or_default();
            let code = text
                .split("<Code>")
                .nth(1)
                .and_then(|s| s.split('<').next())
                .unwrap_or("");
            bail!(
                "storage answered {status}{}",
                if code.is_empty() {
                    String::new()
                } else {
                    format!(" ({code})")
                }
            );
        }
        Ok(res)
    }

    async fn put(&self, key: &str, body: Vec<u8>) -> anyhow::Result<()> {
        self.send(reqwest::Method::PUT, key, &[], body)
            .await
            .map(|_| ())
    }

    async fn delete(&self, key: &str) -> anyhow::Result<()> {
        self.send(reqwest::Method::DELETE, key, &[], vec![])
            .await
            .map(|_| ())
    }

    async fn list(&self, prefix: &str) -> anyhow::Result<Vec<String>> {
        let xml = self
            .send(
                reqwest::Method::GET,
                "",
                &[("list-type", "2"), ("prefix", prefix)],
                vec![],
            )
            .await?
            .text()
            .await?;
        Ok(xml
            .split("<Key>")
            .skip(1)
            .filter_map(|s| s.split("</Key>").next())
            .map(|k| k.replace("&amp;", "&"))
            .collect())
    }
}

/// The access and secret key for a resolved destination.
pub fn credentials(
    state: &AppState,
    cfg: &Settings,
    secret: Option<&str>,
) -> anyhow::Result<(String, String)> {
    if cfg.is_local() {
        let (_, key, secret) =
            local_store().ok_or_else(|| anyhow!("This installation has no backup store."))?;
        return Ok((key, secret));
    }
    let secret = match secret {
        Some(s) => s.to_owned(),
        None => state.sealer.open(
            cfg.secret_sealed
                .as_deref()
                .ok_or_else(|| anyhow!("Enter the secret key."))?,
        )?,
    };
    Ok((cfg.access_key.clone(), secret))
}

/// A client for a resolved destination. A bucket someone entered is only
/// reached where `reach` allows (never this machine or the cluster's own
/// network, which hosts the store on this cluster).
fn client<'a>(
    state: &AppState,
    cfg: &'a Settings,
    secret: Option<&str>,
    reach: Reach,
) -> anyhow::Result<S3<'a>> {
    let (_, secret) = credentials(state, cfg, secret)?;
    if cfg.is_local() {
        return Ok(S3 {
            cfg,
            secret,
            http: reqwest::Client::new(),
            reach: None,
        });
    }
    Ok(S3 {
        cfg,
        secret,
        http: crate::outbound::client(reach, Duration::from_secs(600))?,
        reach: Some(reach),
    })
}

fn folder(cfg: &Settings) -> String {
    let p = cfg.prefix.trim().trim_matches('/');
    if p.is_empty() {
        "pyo-backups/".into()
    } else {
        format!("{p}/")
    }
}

/// Writes and removes a small object, so saving settings proves they work.
/// `reach`: where the bucket may be, by who entered it
/// ([`Reach::installation`] for Pyo's own backups, [`Reach::team`] for a
/// team's).
pub async fn probe(
    state: &AppState,
    cfg: &Settings,
    secret: Option<&str>,
    reach: Reach,
) -> anyhow::Result<()> {
    let cfg = &cfg.resolved()?;
    let s3 = client(state, cfg, secret, reach)?;
    if cfg.is_local() {
        crate::backupstore::ready(&state.db)
            .await
            .context("couldn't reach this server's backup store")?;
    }
    let key = format!("{}.pyo-probe", folder(cfg));
    s3.put(&key, b"ok".to_vec())
        .await
        .context("couldn't write to the bucket")?;
    s3.delete(&key)
        .await
        .context("could write but not delete, and Pyo needs both to keep backups tidy")?;
    Ok(())
}

/// Deletes everything in a bucket of the store on this cluster, with Pyo's
/// key (backupstore.rs lets it in first): a deleted team's backups.
pub async fn empty_local_bucket(bucket: &str) -> anyhow::Result<()> {
    let (url, key, secret) =
        local_store().ok_or_else(|| anyhow!("This installation has no backup store."))?;
    let cfg = Settings {
        destination: local(),
        endpoint: url,
        region: REGION.into(),
        bucket: bucket.into(),
        access_key: key,
        ..Default::default()
    };
    let s3 = S3 {
        cfg: &cfg,
        secret,
        http: reqwest::Client::new(),
        reach: None,
    };
    // A listing is at most 1000 keys: until there are none.
    loop {
        let keys = s3.list("").await?;
        if keys.is_empty() {
            return Ok(());
        }
        for chunk in keys.chunks(16) {
            let deletes: Vec<_> = chunk.iter().map(|k| s3.delete(k)).collect();
            futures::future::try_join_all(deletes).await?;
        }
    }
}

// ── Running ────────────────────────────────────────────────────────────────

/// One backup: export, compress, encrypt, upload, then prune.
async fn run_one(state: &AppState, id: &str, cfg: &Settings) -> anyhow::Result<(String, i64, i32)> {
    let cfg = &cfg.resolved()?;
    let (plain, tables) = export(&state.db).await?;
    let sealed = encrypt(&archive_key()?, &gzip(&plain)?)?;
    let size = sealed.len() as i64;
    let key = format!(
        "{}pyo-{}-{id}{EXT}",
        folder(cfg),
        stamp(pyo_proto::now_ms())
    );
    let s3 = client(state, cfg, None, Reach::installation())?;
    if cfg.is_local() {
        crate::backupstore::ready(&state.db).await?;
    }
    s3.put(&key, sealed).await?;
    // Names sort by time: keep the newest `keep`.
    let mut all: Vec<String> = s3
        .list(&folder(cfg))
        .await?
        .into_iter()
        .filter(|k| backup_stamp(k).is_some())
        .collect();
    all.sort_by(|a, b| backup_stamp(a).cmp(&backup_stamp(b)));
    let excess = all.len().saturating_sub(cfg.keep.max(1) as usize);
    for old in &all[..excess] {
        if let Err(e) = s3.delete(old).await {
            tracing::warn!(key = %old, error = %e, "couldn't delete an old backup");
        }
    }
    Ok((key, size, tables as i32))
}

/// Queue one now (the dashboard's "Back up now").
pub async fn queue(db: &PgPool, trigger: &str) -> anyhow::Result<String> {
    let id = new_id("bkp");
    sqlx::query("INSERT INTO pyo.control_backups (id, trigger) VALUES ($1, $2)")
        .bind(&id)
        .bind(trigger)
        .execute(db)
        .await?;
    Ok(id)
}

/// UTC as `YYYYMMDDTHHMMSSZ` (what SigV4 wants, and names sort by time).
fn stamp(ms: i64) -> String {
    let secs = ms.div_euclid(1000);
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}{m:02}{d:02}T{:02}{:02}{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

pub fn due(cfg: &Settings, last_ms: Option<i64>) -> bool {
    let now = pyo_proto::now_ms();
    let every: i64 = if cfg.schedule == "weekly" {
        7 * 86_400_000
    } else {
        86_400_000
    };
    let hour = (now.div_euclid(1000).rem_euclid(86_400) / 3600) as u8;
    let grace = 3_600_000;
    match last_ms {
        None => true,
        // At the chosen hour, or late by more than an interval (e.g. it was down then).
        Some(t) => now - t >= every - grace && (hour == cfg.hour || now - t > every + grace),
    }
}

/// Worker loop: queues scheduled backups and runs queued ones.
pub async fn runner(state: AppState) {
    loop {
        let st = state.clone();
        crate::bg::as_leader(&state.db, crate::bg::LOCK_BACKUP, || async move {
            if let Err(e) = tick(&st).await {
                tracing::warn!(error = %e, "backup runner");
            }
        })
        .await;
        tokio::time::sleep(Duration::from_secs(20)).await;
    }
}

async fn tick(state: &AppState) -> anyhow::Result<()> {
    // Nothing saved: back up to this server's store, daily, when there is one.
    let cfg = match settings(&state.db).await.map_err(|e| anyhow!("{e:?}"))? {
        Some(c) => c,
        None if local_store().is_some() => Settings::local_default(true),
        None => return Ok(()),
    };
    if cfg.enabled {
        let last: Option<i64> = sqlx::query_scalar(
            "SELECT pyo.ms(max(created_at)) FROM pyo.control_backups WHERE trigger = 'schedule'",
        )
        .fetch_one(&state.db)
        .await?;
        let waiting: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM pyo.control_backups WHERE status IN ('queued', 'running'))").fetch_one(&state.db).await?;
        if !waiting && due(&cfg, last) {
            queue(&state.db, "schedule").await?;
        }
    }
    // A run cut off by a restart: mark it, so it doesn't look stuck.
    sqlx::query("UPDATE pyo.control_backups SET status = 'failed', error = 'Interrupted', finished_at = now() WHERE status = 'running' AND started_at < now() - interval '1 hour'")
        .execute(&state.db)
        .await?;
    let next: Option<String> = sqlx::query_scalar(
        "UPDATE pyo.control_backups SET status = 'running', started_at = now()
          WHERE id = (SELECT id FROM pyo.control_backups WHERE status = 'queued' ORDER BY created_at LIMIT 1) RETURNING id",
    )
    .fetch_optional(&state.db)
    .await?;
    let Some(id) = next else { return Ok(()) };
    match run_one(state, &id, &cfg).await {
        Ok((key, size, tables)) => {
            sqlx::query("UPDATE pyo.control_backups SET status = 'succeeded', object_key = $2, size_bytes = $3, tables = $4, finished_at = now() WHERE id = $1")
                .bind(&id)
                .bind(&key)
                .bind(size)
                .bind(tables)
                .execute(&state.db)
                .await?;
            tracing::info!(%key, size, "backed up Pyo's data");
        }
        Err(e) => {
            sqlx::query("UPDATE pyo.control_backups SET status = 'failed', error = $2, finished_at = now() WHERE id = $1").bind(&id).bind(format!("{e:#}")).execute(&state.db).await?;
            tracing::error!(error = %e, "backup failed");
        }
    }
    Ok(())
}

/// The dashboard's view, for the installation's admin.
pub async fn snapshot(db: &PgPool) -> ApiResult<Value> {
    let history: Vec<Value> = sqlx::query_scalar(
        "SELECT jsonb_build_object('id', id, 'status', status, 'trigger', trigger, 'objectKey', object_key, 'sizeBytes', size_bytes,
                                  'tables', tables, 'error', error, 'createdAt', pyo.ms(created_at), 'finishedAt', pyo.ms(finished_at))
           FROM pyo.control_backups ORDER BY created_at DESC LIMIT 15",
    )
    .fetch_all(db)
    .await?;
    let cfg = settings(db)
        .await?
        .or_else(|| local_store().map(|_| Settings::local_default(true)));
    Ok(json!({ "settings": cfg.map(|s| s.public()), "history": history }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backup_names_sort_by_time() {
        let mut keys = [
            "pyo-backups/pyo-20261112T020000Z-bkp_c.pyobak",
            "pyo-backups/pyo-20261031T120626Z-bkp_a.pyobak",
            "pyo-backups/pyo-20261105T020000Z-bkp_b.pyobak",
        ];
        keys.sort_by(|a, b| backup_stamp(a).cmp(&backup_stamp(b)));
        assert_eq!(
            keys.iter()
                .map(|k| backup_stamp(k).unwrap())
                .collect::<Vec<_>>(),
            [
                "20261031T120626Z-bkp_a",
                "20261105T020000Z-bkp_b",
                "20261112T020000Z-bkp_c"
            ]
        );
        assert_eq!(backup_stamp("pyo-backups/.pyo-probe"), None);
        assert_eq!(backup_stamp("pyo-backups/notes.txt"), None);
    }

    #[test]
    fn encrypt_round_trip_and_tamper() {
        let key = [3u8; 32];
        let plain: Vec<u8> = (0..(CHUNK * 2 + 123)).map(|i| (i % 251) as u8).collect();
        let sealed = encrypt(&key, &plain).unwrap();
        assert_eq!(decrypt(&key, &sealed).unwrap(), plain);
        assert!(decrypt(&[4u8; 32], &sealed).is_err(), "another key");
        // Cut after the first chunk: that chunk wasn't marked last.
        let first = 20 + 4 + u32::from_be_bytes(sealed[20..24].try_into().unwrap()) as usize;
        assert!(decrypt(&key, &sealed[..first]).is_err(), "truncated");
        let mut bad = sealed.clone();
        bad[30] ^= 1;
        assert!(decrypt(&key, &bad).is_err(), "changed");
        assert_eq!(decrypt(&key, &encrypt(&key, b"").unwrap()).unwrap(), b"");
    }

    /// Against a real database (read-only): `DATABASE_URL=... cargo test
    /// export_real_db -- --ignored`.
    #[tokio::test]
    #[ignore]
    async fn export_real_db() {
        let db = PgPool::connect(&std::env::var("DATABASE_URL").unwrap())
            .await
            .unwrap();
        let (plain, tables) = export(&db).await.unwrap();
        let header =
            String::from_utf8_lossy(plain.split(|b| *b == b'\n').next().unwrap()).to_string();
        let head: Value = serde_json::from_str(&header).unwrap();
        assert_eq!(head["tables"].as_array().unwrap().len(), tables);
        assert!(
            !head["tables"]
                .as_array()
                .unwrap()
                .iter()
                .any(|t| t == "sessions")
        );
        let key = [5u8; 32];
        let sealed = encrypt(&key, &gzip(&plain).unwrap()).unwrap();
        let gz = decrypt(&key, &sealed).unwrap();
        let mut back = Vec::new();
        std::io::Read::read_to_end(&mut flate2::read::GzDecoder::new(&gz[..]), &mut back).unwrap();
        assert_eq!(back, plain);
        let count = |name: &str| {
            plain
                .windows(name.len())
                .filter(|w| *w == name.as_bytes())
                .count()
        };
        println!(
            "{tables} tables, {} bytes plain, {} bytes sealed; services section present: {}",
            plain.len(),
            sealed.len(),
            count("TABLE services ") == 1
        );
    }

    /// Against a real Garage, after backupstore's `garage_e2e_setup`: Pyo's
    /// key uses pyo-backups (signed for region "garage", path-style) and no
    /// team's bucket; a team's key only its own.
    #[tokio::test]
    #[ignore]
    async fn garage_e2e_keys() {
        let (url, key, secret) = local_store().unwrap();
        let teams: Value = serde_json::from_str(
            &std::fs::read_to_string(std::env::var("GARAGE_E2E_OUT").unwrap()).unwrap(),
        )
        .unwrap();
        let client = |bucket: &str, key: &str, secret: &str| {
            let cfg = Settings {
                destination: local(),
                endpoint: url.clone(),
                region: REGION.into(),
                bucket: bucket.into(),
                access_key: key.into(),
                ..Default::default()
            };
            (cfg, secret.to_owned())
        };
        let denied = |r: anyhow::Result<()>, what: &str| {
            let e = format!("{:#}", r.expect_err(what));
            assert!(
                e.contains("AccessDenied") || e.contains("403"),
                "{what}: {e}"
            );
        };
        let (pyo, pyo_secret) = client(PYO_BUCKET, &key, &secret);
        let s3 = S3 {
            cfg: &pyo,
            secret: pyo_secret,
            http: reqwest::Client::new(),
            reach: None,
        };
        s3.put(
            "pyo-backups/pyo-20261112T000000Z-bkp_e2e.pyobak",
            vec![7u8; 3 << 20],
        )
        .await
        .unwrap();
        assert_eq!(
            s3.list("pyo-backups/").await.unwrap(),
            ["pyo-backups/pyo-20261112T000000Z-bkp_e2e.pyobak"]
        );
        s3.delete("pyo-backups/pyo-20261112T000000Z-bkp_e2e.pyobak")
            .await
            .unwrap();
        let (a, b) = (&teams["team_a"], &teams["team_b"]);
        let (pyo_on_a, ps) = client(a["bucket"].as_str().unwrap(), &key, &secret);
        denied(
            S3 {
                cfg: &pyo_on_a,
                secret: ps,
                http: reqwest::Client::new(),
                reach: None,
            }
            .put("x", b"x".to_vec())
            .await,
            "Pyo's key on a team's bucket",
        );
        let team = |t: &Value, bucket: &str| {
            client(
                bucket,
                t["key"].as_str().unwrap(),
                t["secret"].as_str().unwrap(),
            )
        };
        let (own, s) = team(a, a["bucket"].as_str().unwrap());
        let own = S3 {
            cfg: &own,
            secret: s,
            http: reqwest::Client::new(),
            reach: None,
        };
        own.put("hello", b"a".to_vec()).await.unwrap();
        assert_eq!(own.list("").await.unwrap(), ["hello"]);
        own.delete("hello").await.unwrap();
        for bucket in [b["bucket"].as_str().unwrap(), PYO_BUCKET] {
            let (other, s) = team(a, bucket);
            let other = S3 {
                cfg: &other,
                secret: s,
                http: reqwest::Client::new(),
                reach: None,
            };
            denied(
                other.put("x", b"x".to_vec()).await,
                &format!("team A's key on {bucket} (put)"),
            );
            denied(
                other.list("").await.map(|_| ()),
                &format!("team A's key on {bucket} (list)"),
            );
        }
        // Something in team B's bucket for its removal to clear.
        let (own_b, s) = team(b, b["bucket"].as_str().unwrap());
        let own_b = S3 {
            cfg: &own_b,
            secret: s,
            http: reqwest::Client::new(),
            reach: None,
        };
        for i in 0..1100 {
            own_b
                .put(&format!("data/{i:04}"), vec![1u8; 64])
                .await
                .unwrap();
        }
    }

    #[test]
    fn utc_stamp() {
        assert_eq!(stamp(0), "19700101T000000Z");
        assert_eq!(stamp(1_793_408_723_000), "20261031T010523Z");
        assert_eq!(stamp(951_782_400_000), "20000229T000000Z");
    }

    #[test]
    fn sigv4_encoding() {
        assert_eq!(enc("a b/c~d", true), "a%20b/c~d");
        assert_eq!(enc("a/b", false), "a%2Fb");
    }

    /// A bucket someone entered never reaches this machine, cloud metadata
    /// or the cluster's own network (the backup store's admin API among it), and
    /// a team's no private network unless the installation allows it.
    #[tokio::test]
    async fn entered_buckets_stay_off_internal_addresses() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let hits = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let h = hits.clone();
        tokio::spawn(async move {
            while let Ok((mut c, _)) = listener.accept().await {
                h.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let mut buf = [0u8; 4096];
                let _ = c.read(&mut buf).await;
                let _ = c
                    .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n")
                    .await;
            }
        });
        let lan = Reach { private: true };
        for (endpoint, reach) in [
            (format!("http://127.0.0.1:{port}"), lan),
            (format!("http://localhost:{port}"), lan),
            ("http://169.254.169.254".to_owned(), lan),
            ("http://10.43.0.20:3900".to_owned(), lan),
            (
                "https://192.168.1.10:9000".to_owned(),
                Reach { private: false },
            ),
            ("http://1.1.1.1".to_owned(), lan),
        ] {
            let cfg = Settings {
                destination: "s3".into(),
                endpoint: endpoint.clone(),
                bucket: "b".into(),
                access_key: "k".into(),
                ..Default::default()
            };
            let s3 = S3 {
                cfg: &cfg,
                secret: "s".into(),
                http: crate::outbound::client(reach, Duration::from_secs(5)).unwrap(),
                reach: Some(reach),
            };
            let err = s3
                .put("pyo-backups/.pyo-probe", b"ok".to_vec())
                .await
                .unwrap_err();
            assert!(
                err.downcast_ref::<crate::outbound::Refused>().is_some(),
                "{endpoint}: {err:#}"
            );
        }
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 0);
    }
}
