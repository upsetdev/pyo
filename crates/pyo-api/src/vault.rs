//! Envelope encryption for what teams entrust to Pyo, starting with
//! environment variables.
//!
//! Each team has a data key (32 random bytes) that seals its values with
//! XSalsa20-Poly1305 (NaCl's secretbox). Data keys are stored wrapped by a
//! key-encryption key from a [`Provider`]: the local master key
//! (`PYO_SECRET_KEY`), or OpenBao/Vault Transit. Rotating the master key
//! rewraps the data keys; the values themselves never need re-sealing.
//!
//! A sealed value is `pyo1:<data key id>:<base64(nonce || ciphertext)>`.
//! Deleting a team deletes its data keys, so its sealed values are gone too.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex, OnceLock},
};

use anyhow::{Context, anyhow, bail, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use crypto_secretbox::{
    Key, Nonce, XSalsa20Poly1305,
    aead::{Aead, KeyInit},
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use zeroize::Zeroizing;

const PREFIX: &str = "pyo1:";

static VAULT: OnceLock<Vault> = OnceLock::new();

/// The process's vault; set once at startup.
pub fn get() -> &'static Vault {
    VAULT.get().expect("vault isn't initialised")
}

pub fn init(v: Vault) {
    let _ = VAULT.set(v);
}

/// Seals one value with a secretbox key: base64(nonce || ciphertext).
fn box_seal(key: &[u8; 32], plain: &[u8]) -> anyhow::Result<String> {
    let cipher = XSalsa20Poly1305::new(&Key::from(*key));
    let mut nonce = [0u8; 24];
    rand::fill(&mut nonce);
    let sealed = cipher
        .encrypt(&Nonce::from(nonce), plain)
        .map_err(|_| anyhow!("encryption failed"))?;
    let mut out = nonce.to_vec();
    out.extend(sealed);
    Ok(STANDARD.encode(out))
}

fn box_open(key: &[u8; 32], sealed: &str) -> anyhow::Result<Zeroizing<Vec<u8>>> {
    let raw = STANDARD.decode(sealed)?;
    ensure!(raw.len() > 24, "sealed value too short");
    let (nonce, body) = raw.split_at(24);
    let nonce: [u8; 24] = nonce.try_into()?;
    let cipher = XSalsa20Poly1305::new(&Key::from(*key));
    Ok(Zeroizing::new(
        cipher
            .decrypt(&Nonce::from(nonce), body)
            .map_err(|_| anyhow!("can't decrypt"))?,
    ))
}

fn fingerprint(key: &[u8]) -> String {
    hex::encode(&Sha256::digest(key)[..6])
}

fn hex_key(name: &str) -> Option<[u8; 32]> {
    let k = hex::decode(std::env::var(name).ok()?.trim()).ok()?;
    k.try_into().ok()
}

/// Where the key-encryption key lives.
pub enum Provider {
    /// `PYO_SECRET_KEY` (64 hex chars). `PYO_SECRET_KEY_PREVIOUS` still
    /// unwraps data keys from before a rotation; they're rewrapped at startup.
    Local {
        current: Zeroizing<[u8; 32]>,
        previous: Option<Zeroizing<[u8; 32]>>,
    },
    /// OpenBao (or Vault) Transit: the master key never leaves it.
    Transit {
        addr: String,
        token: String,
        mount: String,
        key: String,
        http: reqwest::Client,
    },
}

impl Provider {
    /// `PYO_KMS=openbao` (with `OPENBAO_ADDR`/`VAULT_ADDR`, a token, and
    /// optionally `PYO_KMS_MOUNT`/`PYO_KMS_KEY`), else the local key.
    pub fn from_env() -> Option<Self> {
        let var = |names: &[&str]| {
            names
                .iter()
                .find_map(|n| std::env::var(n).ok().filter(|v| !v.trim().is_empty()))
        };
        match var(&["PYO_KMS"]).as_deref() {
            Some("openbao" | "vault") => {
                let addr = var(&["OPENBAO_ADDR", "BAO_ADDR", "VAULT_ADDR"])?;
                let token = var(&["OPENBAO_TOKEN", "BAO_TOKEN", "VAULT_TOKEN"])?;
                Some(Self::Transit {
                    addr: addr.trim_end_matches('/').to_owned(),
                    token,
                    mount: var(&["PYO_KMS_MOUNT"]).unwrap_or_else(|| "transit".into()),
                    key: var(&["PYO_KMS_KEY"]).unwrap_or_else(|| "pyo".into()),
                    http: reqwest::Client::new(),
                })
            }
            _ => Some(Self::Local {
                current: Zeroizing::new(hex_key("PYO_SECRET_KEY")?),
                previous: hex_key("PYO_SECRET_KEY_PREVIOUS").map(Zeroizing::new),
            }),
        }
    }

    /// Names the key-encryption key, so a data key records what wrapped it.
    fn kek_id(&self) -> String {
        match self {
            Self::Local { current, .. } => format!("local:{}", fingerprint(&current[..])),
            Self::Transit { mount, key, .. } => format!("transit:{mount}/{key}"),
        }
    }

    pub fn describe(&self) -> &'static str {
        match self {
            Self::Local { .. } => "local master key",
            Self::Transit { .. } => "OpenBao Transit",
        }
    }

    async fn wrap(&self, dek: &[u8; 32]) -> anyhow::Result<String> {
        match self {
            Self::Local { current, .. } => box_seal(current, dek),
            Self::Transit { .. } => {
                let r = self
                    .transit("encrypt", json!({ "plaintext": STANDARD.encode(dek) }))
                    .await?;
                r["data"]["ciphertext"]
                    .as_str()
                    .map(str::to_owned)
                    .ok_or_else(|| anyhow!("Transit returned no ciphertext"))
            }
        }
    }

    async fn unwrap(&self, kek_id: &str, wrapped: &str) -> anyhow::Result<Zeroizing<[u8; 32]>> {
        let raw = match self {
            Self::Local { current, previous } => {
                let key = if kek_id == self.kek_id() {
                    current
                } else {
                    previous
                        .as_ref()
                        .filter(|p| kek_id == format!("local:{}", fingerprint(&p[..])))
                        .with_context(|| format!("data key was wrapped by {kek_id}; set PYO_SECRET_KEY_PREVIOUS to that key to rotate"))?
                };
                box_open(key, wrapped)?
            }
            Self::Transit { .. } => {
                let r = self
                    .transit("decrypt", json!({ "ciphertext": wrapped }))
                    .await?;
                let b64 = r["data"]["plaintext"]
                    .as_str()
                    .ok_or_else(|| anyhow!("Transit returned no plaintext"))?;
                Zeroizing::new(STANDARD.decode(b64)?)
            }
        };
        let key: [u8; 32] = raw
            .as_slice()
            .try_into()
            .map_err(|_| anyhow!("data key has the wrong length"))?;
        Ok(Zeroizing::new(key))
    }

    async fn transit(&self, op: &str, body: Value) -> anyhow::Result<Value> {
        let Self::Transit {
            addr,
            token,
            mount,
            key,
            http,
        } = self
        else {
            bail!("not Transit")
        };
        let res = http
            .post(format!("{addr}/v1/{mount}/{op}/{key}"))
            .header("X-Vault-Token", token)
            .json(&body)
            .send()
            .await?;
        let status = res.status();
        let v: Value = res.json().await.unwrap_or(Value::Null);
        ensure!(
            status.is_success(),
            "Transit {op} failed ({status}): {}",
            v["errors"]
        );
        Ok(v)
    }
}

pub struct Vault {
    db: PgPool,
    provider: Option<Provider>,
    /// Unwrapped data keys by id, and each team's current one.
    keys: Mutex<HashMap<String, Arc<Zeroizing<[u8; 32]>>>>,
    active: Mutex<HashMap<String, String>>,
}

impl Vault {
    pub fn new(db: PgPool, provider: Option<Provider>) -> Self {
        match &provider {
            Some(p) => {
                tracing::info!(kek = %p.kek_id(), "envelope encryption with the {}", p.describe())
            }
            None => tracing::warn!(
                "no master key (PYO_SECRET_KEY or PYO_KMS): environment variables are stored unencrypted"
            ),
        }
        Self {
            db,
            provider,
            keys: Default::default(),
            active: Default::default(),
        }
    }

    pub fn is_sealed(v: &str) -> bool {
        v.starts_with(PREFIX)
    }

    /// The team's current data key, made on first use.
    async fn team_key(&self, team_id: &str) -> anyhow::Result<(String, Arc<Zeroizing<[u8; 32]>>)> {
        let cached = self.active.lock().unwrap().get(team_id).cloned();
        if let Some(id) = cached
            && let Some(k) = self.keys.lock().unwrap().get(&id).cloned()
        {
            return Ok((id, k));
        }
        let provider = self
            .provider
            .as_ref()
            .ok_or_else(|| anyhow!("no master key"))?;
        let row: Option<(String, String, String)> =
            sqlx::query_as("SELECT id, kek_id, wrapped FROM pyo.data_keys WHERE team_id = $1 AND retired_at IS NULL").bind(team_id).fetch_optional(&self.db).await?;
        let (id, key) = match row {
            Some((id, kek, wrapped)) => {
                let key = provider.unwrap(&kek, &wrapped).await?;
                (id, key)
            }
            None => {
                let mut dek = Zeroizing::new([0u8; 32]);
                rand::fill(&mut dek[..]);
                let id = crate::ids::new_id("dek");
                let wrapped = provider.wrap(&dek).await?;
                // Two first uses at once: one insert wins; use whichever did.
                let won: Option<String> = sqlx::query_scalar(
                    "INSERT INTO pyo.data_keys (id, team_id, kek_id, wrapped) VALUES ($1, $2, $3, $4)
                     ON CONFLICT (team_id) WHERE retired_at IS NULL DO NOTHING RETURNING id",
                )
                .bind(&id)
                .bind(team_id)
                .bind(provider.kek_id())
                .bind(&wrapped)
                .fetch_optional(&self.db)
                .await?;
                if won.is_none() {
                    return Box::pin(self.team_key(team_id)).await;
                }
                (id, dek)
            }
        };
        let key = Arc::new(key);
        self.keys.lock().unwrap().insert(id.clone(), key.clone());
        self.active
            .lock()
            .unwrap()
            .insert(team_id.to_owned(), id.clone());
        Ok((id, key))
    }

    async fn key_by_id(&self, id: &str) -> anyhow::Result<Arc<Zeroizing<[u8; 32]>>> {
        if let Some(k) = self.keys.lock().unwrap().get(id).cloned() {
            return Ok(k);
        }
        let provider = self
            .provider
            .as_ref()
            .ok_or_else(|| anyhow!("no master key to open sealed values"))?;
        let (kek, wrapped): (String, String) =
            sqlx::query_as("SELECT kek_id, wrapped FROM pyo.data_keys WHERE id = $1")
                .bind(id)
                .fetch_optional(&self.db)
                .await?
                .ok_or_else(|| anyhow!("data key {id} is gone (its team was deleted)"))?;
        let key = Arc::new(provider.unwrap(&kek, &wrapped).await?);
        self.keys.lock().unwrap().insert(id.to_owned(), key.clone());
        Ok(key)
    }

    /// The team whose data key sealed `value` (None: not sealed, or its key
    /// is gone).
    async fn sealed_by(&self, value: &str) -> anyhow::Result<Option<String>> {
        let Some(id) = value
            .strip_prefix(PREFIX)
            .and_then(|r| r.split_once(':'))
            .map(|(id, _)| id)
        else {
            return Ok(None);
        };
        Ok(
            sqlx::query_scalar("SELECT team_id FROM pyo.data_keys WHERE id = $1")
                .bind(id)
                .fetch_optional(&self.db)
                .await?,
        )
    }

    /// A value that already looks sealed may only be stored for the team
    /// whose key sealed it: another team's ciphertext (from a leaked backup,
    /// say) pasted in as a value would otherwise be opened for this team.
    async fn check_owner(&self, team_id: &str, value: &str) -> anyhow::Result<()> {
        if self.sealed_by(value).await?.as_deref() != Some(team_id) {
            bail!("A value can't start with {PREFIX}");
        }
        Ok(())
    }

    /// Seal a value with its team's data key. Without a master key, values
    /// stay as they are (and startup said so).
    pub async fn seal(&self, team_id: &str, plain: &str) -> anyhow::Result<String> {
        if Self::is_sealed(plain) {
            self.check_owner(team_id, plain).await?;
            return Ok(plain.to_owned());
        }
        if self.provider.is_none() {
            return Ok(plain.to_owned());
        }
        let (id, key) = self.team_key(team_id).await?;
        Ok(format!(
            "{PREFIX}{id}:{}",
            box_seal(&key, plain.as_bytes())?
        ))
    }

    /// Open a sealed value; anything else is returned as it is.
    pub async fn open(&self, value: &str) -> anyhow::Result<String> {
        let Some(rest) = value.strip_prefix(PREFIX) else {
            return Ok(value.to_owned());
        };
        let (id, body) = rest
            .split_once(':')
            .ok_or_else(|| anyhow!("malformed sealed value"))?;
        let key = self.key_by_id(id).await?;
        Ok(String::from_utf8(box_open(&key, body)?.to_vec())?)
    }

    /// Seal every environment variable value in a JSON document (a service's
    /// spec, a deployment's spec, an agent command, its fallback included).
    pub async fn seal_env(&self, team_id: &str, doc: &mut Value) -> anyhow::Result<()> {
        let mut plain = Vec::new();
        let mut sealed = Vec::new();
        visit_env(doc, &mut |v| {
            if let Some(s) = v.as_str()
                && !s.is_empty()
            {
                if Self::is_sealed(s) {
                    sealed.push(s.to_owned())
                } else {
                    plain.push(s.to_owned())
                }
            }
        });
        // Values kept from before: this team's own, never another's.
        let mut checked: Vec<&str> = vec![];
        for v in &sealed {
            let key = v.split(':').nth(1).unwrap_or("");
            if !checked.contains(&key) {
                self.check_owner(team_id, v).await?;
                checked.push(key);
            }
        }
        if plain.is_empty() || self.provider.is_none() {
            return Ok(());
        }
        let mut sealed = Vec::with_capacity(plain.len());
        for p in &plain {
            sealed.push(self.seal(team_id, p).await?);
        }
        let mut it = sealed.into_iter();
        visit_env(doc, &mut |v| {
            if v.as_str()
                .is_some_and(|s| !s.is_empty() && !Self::is_sealed(s))
                && let Some(s) = it.next()
            {
                *v = Value::String(s);
            }
        });
        Ok(())
    }

    /// Open every sealed environment variable value in a JSON document.
    pub async fn open_env(&self, doc: &mut Value) -> anyhow::Result<()> {
        let mut sealed = Vec::new();
        visit_env(doc, &mut |v| {
            if let Some(s) = v.as_str().filter(|s| Self::is_sealed(s)) {
                sealed.push(s.to_owned());
            }
        });
        let mut opened = Vec::with_capacity(sealed.len());
        for s in &sealed {
            opened.push(self.open(s).await?);
        }
        let mut it = opened.into_iter();
        visit_env(doc, &mut |v| {
            if v.as_str().is_some_and(Self::is_sealed)
                && let Some(s) = it.next()
            {
                *v = Value::String(s);
            }
        });
        Ok(())
    }

    /// A new data key for the team. The old one is kept (retired) so values
    /// it sealed still open; the team's stored values are re-sealed now.
    pub async fn rotate(&self, team_id: &str) -> anyhow::Result<usize> {
        ensure!(
            self.provider.is_some(),
            "Set PYO_SECRET_KEY (or PYO_KMS) on the API to encrypt values."
        );
        sqlx::query(
            "UPDATE pyo.data_keys SET retired_at = now() WHERE team_id = $1 AND retired_at IS NULL",
        )
        .bind(team_id)
        .execute(&self.db)
        .await?;
        self.active.lock().unwrap().remove(team_id);
        let mut resealed = 0;
        for table in ["services", "deployments"] {
            let rows: Vec<(String, Value)> = sqlx::query_as(sqlx::AssertSqlSafe(format!("SELECT id, spec FROM pyo.{table} WHERE team_id = $1 AND spec::text LIKE '%{PREFIX}%'")))
                .bind(team_id)
                .fetch_all(&self.db)
                .await?;
            for (id, mut spec) in rows {
                self.open_env(&mut spec).await?;
                self.seal_env(team_id, &mut spec).await?;
                sqlx::query(sqlx::AssertSqlSafe(format!(
                    "UPDATE pyo.{table} SET spec = $2 WHERE id = $1"
                )))
                .bind(&id)
                .bind(&spec)
                .execute(&self.db)
                .await?;
                resealed += 1;
            }
        }
        Ok(resealed)
    }

    /// For the security overview: how values are protected, and when each
    /// team's key last changed.
    pub async fn status(&self, team_ids: &[String]) -> anyhow::Result<Value> {
        let keys: Vec<(String, i64, Option<i64>, String)> = sqlx::query_as(
            "SELECT team_id, pyo.ms(created_at), pyo.ms(rotated_at), kek_id FROM pyo.data_keys WHERE team_id = ANY($1) AND retired_at IS NULL",
        )
        .bind(team_ids)
        .fetch_all(&self.db)
        .await?;
        Ok(json!({
            "enabled": self.provider.is_some(),
            "cipher": "XSalsa20-Poly1305 (secretbox)",
            "provider": self.provider.as_ref().map(|p| match p { Provider::Local { .. } => "local", Provider::Transit { .. } => "openbao" }),
            "kekId": self.provider.as_ref().map(Provider::kek_id),
            "teams": keys.into_iter().map(|(team, created, rewrapped, kek)| json!({
                "teamId": team, "keyCreatedAt": created, "keyRewrappedAt": rewrapped, "kekId": kek,
            })).collect::<Vec<_>>(),
        }))
    }

    /// Startup upkeep: rewrap data keys a previous master key wrapped, then
    /// seal values stored before envelope encryption (services, deployments, and
    /// commands still waiting for an agent).
    pub async fn upkeep(&self) -> anyhow::Result<()> {
        let Some(provider) = &self.provider else {
            return Ok(());
        };
        let current = provider.kek_id();
        let stale: Vec<(String, String, String)> =
            sqlx::query_as("SELECT id, kek_id, wrapped FROM pyo.data_keys WHERE kek_id <> $1")
                .bind(&current)
                .fetch_all(&self.db)
                .await?;
        for (id, kek, wrapped) in stale {
            match provider.unwrap(&kek, &wrapped).await {
                Ok(dek) => {
                    let rewrapped = provider.wrap(&dek).await?;
                    sqlx::query("UPDATE pyo.data_keys SET kek_id = $2, wrapped = $3, rotated_at = now() WHERE id = $1").bind(&id).bind(&current).bind(&rewrapped).execute(&self.db).await?;
                    tracing::info!(%id, from = %kek, to = %current, "rewrapped data key");
                }
                Err(e) => {
                    tracing::error!(%id, error = %e, "can't rewrap data key; values it sealed can't be opened")
                }
            }
        }

        let mut sealed = 0usize;
        for (table, team_sql) in [
            (
                "services",
                "SELECT id, team_id, spec FROM pyo.services WHERE spec::text LIKE '%\"env\"%'",
            ),
            (
                "deployments",
                "SELECT id, team_id, spec FROM pyo.deployments WHERE spec::text LIKE '%\"env\"%'",
            ),
            (
                "commands",
                "SELECT c.id, cl.team_id, c.command FROM pyo.commands c JOIN pyo.clusters cl ON cl.id = c.cluster_id WHERE c.command::text LIKE '%\"env\"%'",
            ),
        ] {
            let rows: Vec<(String, String, Value)> = sqlx::query_as(sqlx::AssertSqlSafe(team_sql))
                .fetch_all(&self.db)
                .await?;
            for (id, team, mut doc) in rows {
                let before = doc.clone();
                self.seal_env(&team, &mut doc).await?;
                if doc != before {
                    let col = if table == "commands" {
                        "command"
                    } else {
                        "spec"
                    };
                    sqlx::query(sqlx::AssertSqlSafe(format!(
                        "UPDATE pyo.{table} SET {col} = $2 WHERE id = $1"
                    )))
                    .bind(&id)
                    .bind(&doc)
                    .execute(&self.db)
                    .await?;
                    sealed += 1;
                }
            }
        }
        if sealed > 0 {
            tracing::info!(
                rows = sealed,
                "sealed environment variables stored before envelope encryption"
            );
        }
        Ok(())
    }
}

/// Calls `f` on the `value` of every item of every `env` array in `doc`.
fn visit_env(doc: &mut Value, f: &mut impl FnMut(&mut Value)) {
    match doc {
        Value::Object(map) => {
            for (k, v) in map.iter_mut() {
                if k == "env"
                    && let Value::Array(items) = v
                {
                    for item in items.iter_mut() {
                        if let Some(val) = item.get_mut("value") {
                            f(val);
                        }
                    }
                } else {
                    visit_env(v, f);
                }
            }
        }
        Value::Array(items) => items.iter_mut().for_each(|v| visit_env(v, f)),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secretbox_round_trip() {
        let key = [9u8; 32];
        let a = box_seal(&key, b"postgres://u:p@db/app").unwrap();
        assert_ne!(
            a,
            box_seal(&key, b"postgres://u:p@db/app").unwrap(),
            "a fresh nonce each time"
        );
        assert_eq!(&box_open(&key, &a).unwrap()[..], b"postgres://u:p@db/app");
        assert!(
            box_open(&[8u8; 32], &a).is_err(),
            "another key can't open it"
        );
    }

    /// Against a real Postgres (DATABASE_URL, a throwaway one): one team's
    /// sealed value can't be stored as another team's.
    #[tokio::test]
    #[ignore = "needs DATABASE_URL (a throwaway Postgres)"]
    async fn a_sealed_value_stays_with_its_team() {
        let db = PgPool::connect(&std::env::var("DATABASE_URL").unwrap())
            .await
            .unwrap();
        crate::migrate::run(&db).await.unwrap();
        let v = Vault::new(
            db.clone(),
            Some(Provider::Local {
                current: Zeroizing::new([5u8; 32]),
                previous: None,
            }),
        );
        let run = crate::ids::new_id("t");
        let (a, b) = (format!("va_{run}"), format!("vb_{run}"));
        for t in [&a, &b] {
            sqlx::query("INSERT INTO pyo.teams (id, name, slug) VALUES ($1, $1, $1)")
                .bind(t)
                .execute(&db)
                .await
                .unwrap();
        }
        let sealed = v.seal(&a, "hunter2").await.unwrap();
        assert!(Vault::is_sealed(&sealed));
        // Its own team keeps it as it is; another team can't take it in.
        assert_eq!(v.seal(&a, &sealed).await.unwrap(), sealed);
        assert!(v.seal(&b, &sealed).await.is_err());
        let mut doc = json!({ "env": [{ "key": "X", "value": sealed.clone() }, { "key": "Y", "value": "plain" }] });
        v.seal_env(&a, &mut doc).await.unwrap();
        assert_eq!(doc["env"][0]["value"], json!(sealed));
        assert!(Vault::is_sealed(doc["env"][1]["value"].as_str().unwrap()));
        let mut stolen = json!({ "env": [{ "key": "X", "value": sealed.clone() }] });
        assert!(v.seal_env(&b, &mut stolen).await.is_err());
        // Made up: no such key.
        assert!(v.seal(&b, "pyo1:dek_nope:AAAA").await.is_err());
        assert_eq!(v.open(&sealed).await.unwrap(), "hunter2");
        sqlx::query("DELETE FROM pyo.teams WHERE id = ANY($1)")
            .bind(vec![a, b])
            .execute(&db)
            .await
            .unwrap();
    }

    #[test]
    fn finds_env_values_at_any_depth() {
        let mut doc = json!({
            "env": [{ "key": "A", "value": "1" }],
            "kind": "deploy",
            "fallback": { "env": [{ "key": "B", "value": "2" }] },
            "other": { "value": "not env" },
        });
        let mut seen = vec![];
        visit_env(&mut doc, &mut |v| seen.push(v.as_str().unwrap().to_owned()));
        assert_eq!(seen, ["1", "2"]);
    }
}
