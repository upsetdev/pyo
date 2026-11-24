//! Who signs sandbox servers' commands (docs/cloud-sandbox-servers.md).
//!
//! - A hosted Pyo: its build's signer (Pyo Cloud's, `RemoteSigner`).
//! - A self-hosted Pyo: [`Local`], a key of its own kept in
//!   `pyo.sandbox_command_keys`, sealed with PYO_SECRET_KEY. It signs
//!   sandbox commands only. It's never the build's command signer, so
//!   agents keep their token auth and nothing changes for clusters.
//!
//! The key is never the sandbox token key (keys.rs): tokens and commands
//! both sign `pyosbx1.<claims>`, so one key for both could let a token's
//! signature pass for a command's.

use std::sync::{Arc, OnceLock};

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use futures::future::BoxFuture;
use pyo_proto::signing::{SandboxClaims, Signer as KeySigner, TTL_MS};
use sqlx::PgPool;

use crate::{AppState, CommandSigner, ReplyToSign, Signature, ToSign, secrets::Sealer};

/// The team an installation's sandbox server (no team, self-hosted only)
/// has in its commands' claims and its config. Team ids start with
/// `team_`, so it's never a team's.
pub const INSTALLATION_TEAM: &str = "installation";

/// The team a server's commands are signed for: its own, or
/// [`INSTALLATION_TEAM`].
pub fn claims_team(team_id: Option<&str>) -> &str {
    team_id.unwrap_or(INSTALLATION_TEAM)
}

static SIGNER: OnceLock<Arc<dyn CommandSigner>> = OnceLock::new();

/// The signer for sandbox servers' commands, once [`init`] found one.
pub fn get() -> Option<Arc<dyn CommandSigner>> {
    SIGNER.get().cloned()
}

/// Picks the signer at start: the build's on a hosted Pyo, else [`Local`]
/// (made on first start). None without PYO_SECRET_KEY: such a Pyo takes
/// no sandbox servers.
pub async fn init(s: &AppState) {
    let signer: Option<Arc<dyn CommandSigner>> = if crate::hosted() {
        crate::command_signer().cloned()
    } else if s.sealer.enabled() {
        match Local::load(&s.db, &s.sealer).await {
            Ok(l) => Some(Arc::new(l)),
            Err(e) => {
                tracing::warn!(error = %format!("{e:#}"), "sandbox servers: couldn't load the command key");
                None
            }
        }
    } else {
        None
    };
    if let Some(x) = signer {
        let _ = SIGNER.set(x);
    }
}

/// A self-hosted Pyo's signer for its sandbox servers' commands.
pub struct Local {
    db: PgPool,
    key: KeySigner,
}

impl Local {
    /// The active key, or a new one on first use. Two processes starting
    /// at once make one.
    pub async fn load(db: &PgPool, sealer: &Sealer) -> anyhow::Result<Local> {
        anyhow::ensure!(sealer.enabled(), "no PYO_SECRET_KEY to seal the key with");
        let mut tx = db.begin().await?;
        sqlx::query("SELECT pg_advisory_xact_lock($1)")
            .bind(crate::bg::LOCK_SANDBOX_COMMAND_KEY)
            .execute(&mut *tx)
            .await?;
        let row: Option<String> = sqlx::query_scalar(
            "SELECT pkcs8_sealed FROM pyo.sandbox_command_keys WHERE retired_at IS NULL ORDER BY created_at DESC LIMIT 1",
        )
        .fetch_optional(&mut *tx)
        .await?;
        let key = match row {
            Some(sealed) => {
                let pkcs8 = B64.decode(sealer.open(&sealed)?.trim())?;
                KeySigner::from_pkcs8(&pkcs8).map_err(anyhow::Error::msg)?
            }
            None => {
                let pkcs8 = KeySigner::generate_pkcs8().map_err(anyhow::Error::msg)?;
                let key = KeySigner::from_pkcs8(&pkcs8).map_err(anyhow::Error::msg)?;
                sqlx::query(
                    "INSERT INTO pyo.sandbox_command_keys (kid, pkcs8_sealed, public_key) VALUES ($1, $2, $3)",
                )
                .bind(key.kid())
                .bind(sealer.seal(&B64.encode(&pkcs8))?)
                .bind(key.public_key())
                .execute(&mut *tx)
                .await?;
                tracing::info!(kid = key.kid(), "made the sandbox command key");
                key
            }
        };
        tx.commit().await?;
        Ok(Local { db: db.clone(), key })
    }

    /// Claims for `items`, numbered from `first`.
    fn sign_all(&self, server_id: &str, team: &str, first: u64, items: &[ToSign]) -> Vec<Signature> {
        let now = pyo_proto::now_ms() as u64;
        items
            .iter()
            .zip(first..)
            .map(|(t, seq)| {
                let (claims, sig) = self.key.sign_sandbox(&SandboxClaims {
                    v: 1,
                    id: t.id.clone(),
                    server: server_id.to_owned(),
                    team: team.to_owned(),
                    seq,
                    iat: now,
                    exp: now + TTL_MS,
                    kind: t.kind.clone(),
                    sha256: t.sha256.clone(),
                });
                Signature {
                    claims,
                    sig,
                    kid: self.key.kid().to_owned(),
                    refused: None,
                    delegation: None,
                }
            })
            .collect()
    }
}

impl CommandSigner for Local {
    fn sign<'a>(&'a self, _: &'a str, _: &'a [ToSign]) -> BoxFuture<'a, anyhow::Result<Vec<Signature>>> {
        Box::pin(async { anyhow::bail!("this signer signs sandbox server commands only") })
    }

    fn sign_reply<'a>(&'a self, _: &'a str, _: &'a ReplyToSign) -> BoxFuture<'a, anyhow::Result<Signature>> {
        Box::pin(async { anyhow::bail!("this signer signs sandbox server commands only") })
    }

    fn public_keys(&self) -> String {
        self.key.public_key()
    }

    /// Takes the next numbers from the server's row, in the statement that
    /// raises them, and only for an outbound server of that team.
    fn sign_sandbox<'a>(
        &'a self,
        server_id: &'a str,
        team_id: &'a str,
        items: &'a [ToSign],
    ) -> BoxFuture<'a, anyhow::Result<Vec<Signature>>> {
        Box::pin(async move {
            if items.is_empty() {
                return Ok(vec![]);
            }
            let last: Option<i64> = sqlx::query_scalar(
                "UPDATE pyo.sandbox_servers SET command_seq = command_seq + $2
                  WHERE id = $1 AND outbound AND coalesce(team_id, $4) = $3
                 RETURNING command_seq",
            )
            .bind(server_id)
            .bind(items.len() as i64)
            .bind(team_id)
            .bind(INSTALLATION_TEAM)
            .fetch_optional(&self.db)
            .await?;
            let Some(last) = last else {
                anyhow::bail!("{server_id} isn't an outbound sandbox server of {team_id}");
            };
            let first = (last as u64).saturating_sub(items.len() as u64) + 1;
            Ok(self.sign_all(server_id, team_id, first, items))
        })
    }
}
