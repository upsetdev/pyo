//! The installation's sandbox signing key, and the tokens it signs.
//!
//! One Ed25519 key per installation, made on first use and kept in
//! `pyo.sandbox_keys` with its seed sealed by PYO_SECRET_KEY. Its `kid` is
//! in every token, so a new key can take over while tokens of the old one
//! run out. Sandbox servers get only the public keys ([`verifying_keys`]).
//!
//! - Client tokens ([`client_token`]): one sandbox, about an hour, for exec,
//!   terminals, files and ports. Pause, resume and delete go through Pyo, so
//!   its records stay right.
//! - Pyo signs no tokens for its own calls: they go to the server over its
//!   channel, signed as commands (outbound/signer.rs). Until 0.52.0 Pyo
//!   called daemons with admin and create tokens. Daemons refuse those now.
//! - Registry tokens ([`registry_token`]): pull one team's images from
//!   Pyo's registry door for an hour, while a server makes a template.
//!   No daemon takes them.

use std::sync::{Arc, Mutex};

use pyo_proto::sandbox::token::{Claims, SigningKey, VerifyingKey};
use pyo_proto::sandbox::wire::Scope;
use sqlx::PgPool;

use crate::error::{ApiError, ApiResult};
use crate::secrets::Sealer;

/// How long a client token lives. The SDK asks for a new one before.
pub const CLIENT_TTL_SECS: u64 = 3600;
/// How long a sandbox server may pull a template's image from Pyo's
/// registry: long enough for a big image.
pub const REGISTRY_TTL_SECS: u64 = 3600;

/// What a client token allows.
pub const CLIENT_SCOPES: [Scope; 4] = [Scope::Exec, Scope::Pty, Scope::Files, Scope::Ports];

static KEY: Mutex<Option<Arc<SigningKey>>> = Mutex::new(None);

fn now_secs() -> u64 {
    pyo_proto::now_ms() as u64 / 1000
}

/// The installation's signing key: the active one, or a new one on first
/// use. Kept in memory after the first read.
pub async fn signing_key(db: &PgPool, sealer: &Sealer) -> ApiResult<Arc<SigningKey>> {
    if let Some(k) = KEY.lock().unwrap_or_else(|p| p.into_inner()).clone() {
        return Ok(k);
    }
    if !sealer.enabled() {
        return Err(ApiError::new(
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            "This installation has no PYO_SECRET_KEY, so it can't sign sandbox tokens.",
        ));
    }
    let mut tx = db.begin().await?;
    // Two processes asking at once make one key.
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(crate::bg::LOCK_SANDBOX_KEY)
        .execute(&mut *tx)
        .await?;
    let row: Option<(String, String)> = sqlx::query_as(
        "SELECT kid, seed_sealed FROM pyo.sandbox_keys WHERE retired_at IS NULL ORDER BY created_at DESC LIMIT 1",
    )
    .fetch_optional(&mut *tx)
    .await?;
    let key = match row {
        Some((kid, sealed)) => {
            let seed = sealer.open(&sealed)?;
            SigningKey::from_seed_b64(kid, &seed)
                .map_err(|e| anyhow::anyhow!("the sandbox signing key: {e}"))?
        }
        None => {
            let key = SigningKey::generate(crate::ids::new_id("sbk"));
            sqlx::query(
                "INSERT INTO pyo.sandbox_keys (kid, seed_sealed, public_key) VALUES ($1, $2, $3)",
            )
            .bind(key.kid())
            .bind(sealer.seal(&key.seed_b64())?)
            .bind(&key.verifying_key().public_key)
            .execute(&mut *tx)
            .await?;
            tracing::info!(kid = key.kid(), "made the sandbox signing key");
            key
        }
    };
    tx.commit().await?;
    let key = Arc::new(key);
    *KEY.lock().unwrap_or_else(|p| p.into_inner()) = Some(key.clone());
    Ok(key)
}

/// Every key a daemon should accept: the active one and retired ones.
pub async fn verifying_keys(db: &PgPool, sealer: &Sealer) -> ApiResult<Vec<VerifyingKey>> {
    // Makes the key when there's none yet.
    signing_key(db, sealer).await?;
    let rows: Vec<(String, String)> = sqlx::query_as(
        "SELECT kid, public_key FROM pyo.sandbox_keys ORDER BY created_at DESC LIMIT 4",
    )
    .fetch_all(db)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(kid, public_key)| VerifyingKey { kid, public_key })
        .collect())
}

/// A token and when it runs out (epoch milliseconds).
#[derive(Debug, Clone, serde::Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct Token {
    pub token: String,
    pub expires_at: u64,
}

fn sign(
    key: &SigningKey,
    sbx: &str,
    team: &str,
    server: &str,
    scopes: &[Scope],
    ttl: u64,
) -> Token {
    let iat = now_secs();
    let exp = iat + ttl;
    Token {
        token: key.sign(&Claims {
            kid: String::new(),
            sbx: sbx.into(),
            team: team.into(),
            srv: server.into(),
            iat,
            exp,
            scopes: scopes.to_vec(),
        }),
        expires_at: exp * 1000,
    }
}

/// A client's token for one sandbox on its server.
pub fn client_token(key: &SigningKey, sandbox: &str, team: &str, server: &str) -> Token {
    sign(key, sandbox, team, server, &CLIENT_SCOPES, CLIENT_TTL_SECS)
}

/// The team a registry token names for an installation's template: it may
/// pull Pyo's own images (`pyo/<name>`) and no team's.
pub const PYO_IMAGES_ONLY: &str = "pyo";

/// A sandbox server's login at Pyo's registry door (password, with the
/// username [`crate::registrydoor::SANDBOX_USER`]) to pull a template's
/// image of `team` (or, with [`PYO_IMAGES_ONLY`], one of Pyo's own).
pub fn registry_token(key: &SigningKey, template: &str, team: &str) -> String {
    sign(
        key,
        template,
        team,
        pyo_proto::sandbox::token::REGISTRY_AUDIENCE,
        &[Scope::Registry],
        REGISTRY_TTL_SECS,
    )
    .token
}

/// Checks a registry token at the door: the team it may pull for.
pub fn check_registry_token(keys: &[VerifyingKey], token: &str) -> Option<String> {
    use pyo_proto::sandbox::token::{Expect, verify, verify_signature};
    let unchecked = verify_signature(token, keys).ok()?;
    let c = verify(
        token,
        keys,
        Expect {
            server: pyo_proto::sandbox::token::REGISTRY_AUDIENCE,
            sandbox: Some(&unchecked.sbx),
            scope: Scope::Registry,
            now: now_secs(),
        },
    )
    .ok()?;
    // Only a token made for this, never one that has it through admin.
    (c.scopes == [Scope::Registry] && !c.team.is_empty()).then_some(c.team)
}

#[cfg(test)]
pub(crate) fn reset_cache() {
    *KEY.lock().unwrap_or_else(|p| p.into_inner()) = None;
}

#[cfg(test)]
mod tests {
    use super::*;
    use pyo_proto::sandbox::token::{Expect, TokenError, verify};

    const SBX: &str = "sbx_abcdefghijklmnop";

    fn expect(sandbox: Option<&str>, scope: Scope) -> Expect<'_> {
        Expect {
            server: "sbs_one",
            sandbox,
            scope,
            now: now_secs(),
        }
    }

    #[test]
    fn client_tokens_open_one_sandbox_for_an_hour() {
        let k = SigningKey::generate("sbk_test");
        let keys = [k.verifying_key()];
        let t = client_token(&k, SBX, "tm_one", "sbs_one");
        let c = verify(&t.token, &keys, expect(Some(SBX), Scope::Exec)).unwrap();
        assert_eq!((c.kid.as_str(), c.team.as_str()), ("sbk_test", "tm_one"));
        assert_eq!(c.exp - c.iat, CLIENT_TTL_SECS);
        assert_eq!(t.expires_at, c.exp * 1000);
        for s in [Scope::Pty, Scope::Files, Scope::Ports] {
            verify(&t.token, &keys, expect(Some(SBX), s)).unwrap();
        }
        // Pyo keeps the lifecycle, and the server.
        assert_eq!(
            verify(&t.token, &keys, expect(Some(SBX), Scope::Lifecycle)),
            Err(TokenError::MissingScope(Scope::Lifecycle))
        );
        assert_eq!(
            verify(&t.token, &keys, expect(Some(SBX), Scope::Create)),
            Err(TokenError::MissingScope(Scope::Create))
        );
        assert_eq!(
            verify(&t.token, &keys, expect(None, Scope::Admin)),
            Err(TokenError::MissingScope(Scope::Admin))
        );
        assert_eq!(
            verify(
                &t.token,
                &keys,
                expect(Some("sbx_otherotherother"), Scope::Exec)
            ),
            Err(TokenError::WrongSandbox)
        );
        let mut other = expect(Some(SBX), Scope::Exec);
        other.server = "sbs_two";
        assert_eq!(verify(&t.token, &keys, other), Err(TokenError::WrongServer));
        // Another installation's key.
        let stranger = [SigningKey::generate("sbk_test").verifying_key()];
        assert_eq!(
            verify(&t.token, &stranger, expect(Some(SBX), Scope::Exec)),
            Err(TokenError::BadSignature)
        );
    }

    #[test]
    fn a_refresh_is_a_new_token_that_runs_longer() {
        let k = SigningKey::generate("sbk_test");
        let a = client_token(&k, SBX, "tm_one", "sbs_one");
        std::thread::sleep(std::time::Duration::from_millis(1100));
        let b = client_token(&k, SBX, "tm_one", "sbs_one");
        assert_ne!(a.token, b.token);
        assert!(b.expires_at > a.expires_at);
    }

    #[test]
    fn registry_tokens_only_open_the_registry() {
        let k = SigningKey::generate("sbk_test");
        let keys = [k.verifying_key()];
        let t = registry_token(&k, "stp_one", "tm_one");
        assert_eq!(check_registry_token(&keys, &t).as_deref(), Some("tm_one"));
        // No sandbox server takes it.
        assert_eq!(
            verify(&t, &keys, expect(None, Scope::Admin)),
            Err(TokenError::WrongServer)
        );
        // Other tokens don't open the registry.
        let admin = sign(
            &k,
            pyo_proto::sandbox::token::ANY_SANDBOX,
            "",
            "sbs_one",
            &[Scope::Admin],
            300,
        );
        assert_eq!(check_registry_token(&keys, &admin.token), None);
        let c = client_token(&k, SBX, "tm_one", "sbs_one");
        assert_eq!(check_registry_token(&keys, &c.token), None);
        let other = SigningKey::generate("sbk_other");
        assert_eq!(
            check_registry_token(&keys, &registry_token(&other, "stp_one", "tm_one")),
            None
        );
        assert_eq!(
            check_registry_token(&keys, &registry_token(&k, "stp_one", "")),
            None
        );
        // An installation's template: Pyo's own images, which the door
        // tells apart from a team's.
        assert_eq!(
            check_registry_token(&keys, &registry_token(&k, "stp_desktop", PYO_IMAGES_ONLY))
                .as_deref(),
            Some(PYO_IMAGES_ONLY)
        );
        assert!(
            !PYO_IMAGES_ONLY.contains('_'),
            "never a team's id (prefix_random)"
        );
    }

    /// Against a throwaway Postgres: `DATABASE_URL=… cargo test -p pyo-api
    /// sandbox::keys -- --ignored`.
    #[tokio::test]
    #[ignore = "needs DATABASE_URL (a throwaway Postgres)"]
    async fn one_key_is_made_and_kept_sealed() {
        let db = PgPool::connect(&std::env::var("DATABASE_URL").expect("DATABASE_URL"))
            .await
            .unwrap();
        crate::migrate::run(&db).await.unwrap();
        let sealer = Sealer::for_tests();
        reset_cache();
        let a = signing_key(&db, &sealer).await.unwrap();
        reset_cache();
        let b = signing_key(&db, &sealer).await.unwrap();
        assert_eq!(
            a.verifying_key(),
            b.verifying_key(),
            "read back, not made again"
        );
        let sealed: String =
            sqlx::query_scalar("SELECT seed_sealed FROM pyo.sandbox_keys WHERE kid = $1")
                .bind(a.kid())
                .fetch_one(&db)
                .await
                .unwrap();
        assert!(sealed.starts_with("sb1:"));
        assert!(!sealed.contains(&a.seed_b64()));
        let keys = verifying_keys(&db, &sealer).await.unwrap();
        assert!(keys.contains(&a.verifying_key()));
        reset_cache();
    }
}
