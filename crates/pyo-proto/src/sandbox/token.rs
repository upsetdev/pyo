//! Short-lived tokens that let a client talk to a sandbox server directly.
//!
//! Pyo signs a token when a sandbox is created and on request after that.
//! The daemon checks it offline, so sandboxes keep working while Pyo
//! restarts.
//!
//! ```text
//! pyosbx1.<base64url(claims JSON)>.<base64url(Ed25519 signature)>
//! ```
//!
//! The signature covers `pyosbx1.<claims>`, the prefix included, so a token
//! signature can't be reused for anything else Pyo signs.
//!
//! ## Why Ed25519 and not HMAC
//!
//! A sandbox server runs untrusted code by design. With HMAC the daemon
//! would hold the key that makes tokens, so anyone who broke out of a VM
//! could mint tokens. With a shared key that means every server and every
//! team. With Ed25519 the daemon holds only Pyo's public key, which grants
//! nothing. Pyo keeps the private key. Verifying costs about 50 µs, and
//! `ring` is already built for every target Pyo ships through rustls, so it
//! adds no dependency. `kid` lets Pyo rotate keys while old tokens run out.

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use ring::signature::{ED25519, Ed25519KeyPair, KeyPair, UnparsedPublicKey};
use serde::{Deserialize, Serialize};

use super::wire::Scope;

pub const TOKEN_PREFIX: &str = "pyosbx1";
/// The longest token the daemon looks at.
pub const MAX_TOKEN_LEN: usize = 4096;
/// The longest lifetime a token may have. Pyo gives out tokens for an hour
/// and the SDK asks for a new one before that runs out.
pub const MAX_TOKEN_TTL_SECS: u64 = 24 * 60 * 60;
/// Clock difference allowed between Pyo and the server.
pub const LEEWAY_SECS: u64 = 30;
/// `sbx` of a token for every sandbox on the server. Only valid with the
/// admin scope.
pub const ANY_SANDBOX: &str = "*";

/// What a token grants.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Claims {
    /// The key that signed it.
    pub kid: String,
    /// The sandbox ID, or [`ANY_SANDBOX`].
    pub sbx: String,
    pub team: String,
    /// The sandbox server it's for. A token works on that server only.
    pub srv: String,
    /// Issued at, epoch seconds.
    pub iat: u64,
    /// Expires at, epoch seconds.
    pub exp: u64,
    pub scopes: Vec<Scope>,
}

impl Claims {
    pub fn has(&self, scope: Scope) -> bool {
        self.scopes.contains(&scope) || self.scopes.contains(&Scope::Admin)
    }
}

/// Pyo's private key.
pub struct SigningKey {
    kid: String,
    pair: Ed25519KeyPair,
    seed: [u8; 32],
}

impl std::fmt::Debug for SigningKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SigningKey")
            .field("kid", &self.kid)
            .finish_non_exhaustive()
    }
}

impl SigningKey {
    pub fn from_seed(kid: impl Into<String>, seed: [u8; 32]) -> SigningKey {
        let pair = Ed25519KeyPair::from_seed_unchecked(&seed).expect("any 32 bytes are a seed");
        SigningKey {
            kid: kid.into(),
            pair,
            seed,
        }
    }

    /// A new key from the system's random source.
    pub fn generate(kid: impl Into<String>) -> SigningKey {
        use ring::rand::SecureRandom as _;
        let mut seed = [0u8; 32];
        ring::rand::SystemRandom::new()
            .fill(&mut seed)
            .expect("system random source");
        SigningKey::from_seed(kid, seed)
    }

    /// The seed in base64url, for storing the key.
    pub fn seed_b64(&self) -> String {
        B64.encode(self.seed)
    }

    pub fn from_seed_b64(kid: impl Into<String>, seed: &str) -> Result<SigningKey, TokenError> {
        let bytes = B64.decode(seed.trim()).map_err(|_| TokenError::Malformed)?;
        let seed: [u8; 32] = bytes.try_into().map_err(|_| TokenError::Malformed)?;
        Ok(SigningKey::from_seed(kid, seed))
    }

    pub fn kid(&self) -> &str {
        &self.kid
    }

    pub fn verifying_key(&self) -> VerifyingKey {
        VerifyingKey {
            kid: self.kid.clone(),
            public_key: B64.encode(self.pair.public_key().as_ref()),
        }
    }

    /// Signs `claims`. Their `kid` is set to this key's.
    pub fn sign(&self, claims: &Claims) -> String {
        let mut claims = claims.clone();
        claims.kid = self.kid.clone();
        let body = B64.encode(serde_json::to_vec(&claims).expect("claims serialize"));
        let signed = format!("{TOKEN_PREFIX}.{body}");
        let sig = self.pair.sign(signed.as_bytes());
        format!("{signed}.{}", B64.encode(sig.as_ref()))
    }
}

/// Pyo's public key, as a daemon has it in its config.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VerifyingKey {
    pub kid: String,
    /// 32 bytes in base64url.
    pub public_key: String,
}

/// What the daemon needs from a token for one call.
#[derive(Debug, Clone, Copy)]
pub struct Expect<'a> {
    /// This server's ID.
    pub server: &'a str,
    /// The sandbox the call is about. `None` for calls about the whole
    /// server, which need the admin scope.
    pub sandbox: Option<&'a str>,
    pub scope: Scope,
    /// Epoch seconds.
    pub now: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TokenError {
    Malformed,
    UnknownKey,
    BadSignature,
    Expired,
    NotYetValid,
    LifetimeTooLong,
    WrongServer,
    WrongSandbox,
    MissingScope(Scope),
}

impl std::fmt::Display for TokenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TokenError::Malformed => f.write_str("The sandbox token is malformed."),
            TokenError::UnknownKey => {
                f.write_str("The sandbox token is signed with an unknown key.")
            }
            TokenError::BadSignature => f.write_str("The sandbox token signature is wrong."),
            TokenError::Expired => f.write_str("The sandbox token has expired."),
            TokenError::NotYetValid => f.write_str("The sandbox token isn't valid yet."),
            TokenError::LifetimeTooLong => {
                f.write_str("The sandbox token lives longer than allowed.")
            }
            TokenError::WrongServer => f.write_str("The sandbox token is for another server."),
            TokenError::WrongSandbox => f.write_str("The sandbox token is for another sandbox."),
            TokenError::MissingScope(s) => {
                write!(f, "The sandbox token doesn't allow {}.", scope_name(*s))
            }
        }
    }
}

impl std::error::Error for TokenError {}

impl TokenError {
    /// Whether the client should get 401 (who are you) rather than 403
    /// (you may not).
    pub fn is_unauthenticated(&self) -> bool {
        !matches!(
            self,
            TokenError::WrongServer | TokenError::WrongSandbox | TokenError::MissingScope(_)
        )
    }
}

fn scope_name(s: Scope) -> &'static str {
    match s {
        Scope::Create => "creating sandboxes",
        Scope::Exec => "running commands",
        Scope::Pty => "terminals",
        Scope::Files => "files",
        Scope::Ports => "ports",
        Scope::Lifecycle => "pausing, resuming or deleting",
        Scope::Admin => "server-wide calls",
    }
}

/// Checks the signature and every claim against `expect`.
pub fn verify(
    token: &str,
    keys: &[VerifyingKey],
    expect: Expect<'_>,
) -> Result<Claims, TokenError> {
    let claims = verify_signature(token, keys)?;
    if claims.iat > expect.now + LEEWAY_SECS {
        return Err(TokenError::NotYetValid);
    }
    if expect.now > claims.exp + LEEWAY_SECS {
        return Err(TokenError::Expired);
    }
    if claims.exp.saturating_sub(claims.iat) > MAX_TOKEN_TTL_SECS {
        return Err(TokenError::LifetimeTooLong);
    }
    if claims.srv != expect.server {
        return Err(TokenError::WrongServer);
    }
    let any = claims.sbx == ANY_SANDBOX;
    match expect.sandbox {
        Some(id) if claims.sbx == id => {}
        Some(_) if any && claims.has(Scope::Admin) => {}
        Some(_) => return Err(TokenError::WrongSandbox),
        None if any && claims.has(Scope::Admin) => {}
        None => return Err(TokenError::MissingScope(Scope::Admin)),
    }
    if !claims.has(expect.scope) {
        return Err(TokenError::MissingScope(expect.scope));
    }
    Ok(claims)
}

/// Checks only the signature and returns the claims. [`verify`] is what
/// the daemon uses. This is for checking scopes again per request on a
/// connection that was already let in.
pub fn verify_signature(token: &str, keys: &[VerifyingKey]) -> Result<Claims, TokenError> {
    if token.len() > MAX_TOKEN_LEN {
        return Err(TokenError::Malformed);
    }
    let mut parts = token.split('.');
    let (Some(prefix), Some(body), Some(sig), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(TokenError::Malformed);
    };
    if prefix != TOKEN_PREFIX {
        return Err(TokenError::Malformed);
    }
    let body_bytes = B64.decode(body).map_err(|_| TokenError::Malformed)?;
    let sig = B64.decode(sig).map_err(|_| TokenError::Malformed)?;
    // Read the kid to pick the key. The claims are trusted only after the
    // signature checks out.
    let claims: Claims = serde_json::from_slice(&body_bytes).map_err(|_| TokenError::Malformed)?;
    let key = keys
        .iter()
        .find(|k| k.kid == claims.kid)
        .ok_or(TokenError::UnknownKey)?;
    let public = B64
        .decode(&key.public_key)
        .map_err(|_| TokenError::UnknownKey)?;
    let signed_len = prefix.len() + 1 + body.len();
    UnparsedPublicKey::new(&ED25519, public)
        .verify(&token.as_bytes()[..signed_len], &sig)
        .map_err(|_| TokenError::BadSignature)?;
    Ok(claims)
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: u64 = 1_800_000_000;

    fn key() -> SigningKey {
        SigningKey::from_seed("k1", [7u8; 32])
    }

    fn claims() -> Claims {
        Claims {
            kid: String::new(),
            sbx: "sbx_abcdefghijklmnop".into(),
            team: "team1".into(),
            srv: "srv1".into(),
            iat: NOW,
            exp: NOW + 3600,
            scopes: vec![Scope::Exec, Scope::Files],
        }
    }

    fn expect(scope: Scope) -> Expect<'static> {
        Expect {
            server: "srv1",
            sandbox: Some("sbx_abcdefghijklmnop"),
            scope,
            now: NOW + 10,
        }
    }

    #[test]
    fn sign_and_verify() {
        let k = key();
        let t = k.sign(&claims());
        assert!(t.starts_with("pyosbx1."));
        let c = verify(&t, &[k.verifying_key()], expect(Scope::Exec)).unwrap();
        assert_eq!(c.kid, "k1");
        assert_eq!(c.team, "team1");
    }

    #[test]
    fn seeds_round_trip_and_keys_are_stable() {
        let k = key();
        let k2 = SigningKey::from_seed_b64("k1", &k.seed_b64()).unwrap();
        assert_eq!(k.verifying_key(), k2.verifying_key());
        let g = SigningKey::generate("k2");
        assert_ne!(g.verifying_key().public_key, k.verifying_key().public_key);
        assert!(!format!("{k:?}").contains(&k.seed_b64()));
    }

    #[test]
    fn refuses_tampering_and_unknown_keys() {
        let k = key();
        let keys = [k.verifying_key()];
        let t = k.sign(&claims());
        // Swap in claims for another team, keep the signature.
        let mut other = claims();
        other.team = "team2".into();
        other.kid = "k1".into();
        let parts: Vec<&str> = t.split('.').collect();
        let forged = format!(
            "{}.{}.{}",
            parts[0],
            B64.encode(serde_json::to_vec(&other).unwrap()),
            parts[2]
        );
        assert_eq!(
            verify(&forged, &keys, expect(Scope::Exec)),
            Err(TokenError::BadSignature)
        );

        let stranger = SigningKey::from_seed("k1", [8u8; 32]).sign(&claims());
        assert_eq!(
            verify(&stranger, &keys, expect(Scope::Exec)),
            Err(TokenError::BadSignature)
        );
        let other_kid = SigningKey::from_seed("k9", [7u8; 32]).sign(&claims());
        assert_eq!(
            verify(&other_kid, &keys, expect(Scope::Exec)),
            Err(TokenError::UnknownKey)
        );

        for bad in [
            "",
            "pyosbx1",
            "pyosbx1.a.b.c",
            "jwt.a.b",
            &format!("pyosbx2{}", &t[7..]),
            &"x".repeat(5000),
        ] {
            assert!(verify(bad, &keys, expect(Scope::Exec)).is_err(), "{bad}");
        }
    }

    #[test]
    fn checks_time() {
        let k = key();
        let keys = [k.verifying_key()];
        let t = k.sign(&claims());
        let at = |now| Expect {
            now,
            ..expect(Scope::Exec)
        };
        assert!(verify(&t, &keys, at(NOW + 3600 + LEEWAY_SECS)).is_ok());
        assert_eq!(
            verify(&t, &keys, at(NOW + 3600 + LEEWAY_SECS + 1)),
            Err(TokenError::Expired)
        );
        assert_eq!(
            verify(&t, &keys, at(NOW - LEEWAY_SECS - 1)),
            Err(TokenError::NotYetValid)
        );
        let long = Claims {
            exp: NOW + MAX_TOKEN_TTL_SECS + 1,
            ..claims()
        };
        assert_eq!(
            verify(&k.sign(&long), &keys, at(NOW)),
            Err(TokenError::LifetimeTooLong)
        );
    }

    #[test]
    fn checks_server_sandbox_and_scope() {
        let k = key();
        let keys = [k.verifying_key()];
        let t = k.sign(&claims());
        let e = expect(Scope::Exec);
        assert_eq!(
            verify(
                &t,
                &keys,
                Expect {
                    server: "srv2",
                    ..e
                }
            ),
            Err(TokenError::WrongServer)
        );
        assert_eq!(
            verify(
                &t,
                &keys,
                Expect {
                    sandbox: Some("sbx_zzzzzzzzzzzzzzzz"),
                    ..e
                }
            ),
            Err(TokenError::WrongSandbox)
        );
        assert_eq!(
            verify(&t, &keys, expect(Scope::Pty)),
            Err(TokenError::MissingScope(Scope::Pty))
        );
        assert_eq!(
            verify(&t, &keys, Expect { sandbox: None, ..e }),
            Err(TokenError::MissingScope(Scope::Admin))
        );
        assert!(
            TokenError::MissingScope(Scope::Pty)
                .to_string()
                .contains("terminals")
        );
        assert!(!TokenError::WrongSandbox.is_unauthenticated());
        assert!(TokenError::Expired.is_unauthenticated());
    }

    #[test]
    fn admin_tokens_cover_the_server() {
        let k = key();
        let keys = [k.verifying_key()];
        let admin = Claims {
            sbx: ANY_SANDBOX.into(),
            team: String::new(),
            scopes: vec![Scope::Admin],
            ..claims()
        };
        let t = k.sign(&admin);
        assert!(verify(&t, &keys, expect(Scope::Lifecycle)).is_ok());
        assert!(
            verify(
                &t,
                &keys,
                Expect {
                    sandbox: None,
                    ..expect(Scope::Admin)
                }
            )
            .is_ok()
        );
        // A wildcard without admin is nothing.
        let wild = Claims {
            sbx: ANY_SANDBOX.into(),
            scopes: vec![Scope::Exec],
            ..claims()
        };
        assert_eq!(
            verify(&k.sign(&wild), &keys, expect(Scope::Exec)),
            Err(TokenError::WrongSandbox)
        );
    }
}
