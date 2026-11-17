//! Signed commands: a command the agent runs only when a signer it trusts
//! signed it for its own cluster.
//!
//! A hosted Pyo (Pyo Cloud) keeps its signing key out of the API: a separate
//! signer signs each command, so a database write or a bug in the API alone
//! can't send work to a customer's cluster. The agent is told the signer's
//! public key at install (`PYO_COMMAND_KEY`) and refuses anything else.
//!
//! ```text
//! sig = Ed25519("pyocmd1." + base64url(claims JSON))
//! ```
//!
//! The claims name the command (its id and kind), the cluster it's for, a
//! sequence number that only grows per cluster, when it was signed and when
//! it expires, and the SHA-256 of the command's exact JSON. The signer never
//! sees the command itself, so the secrets in it (sealed env values opened
//! for the agent) never reach it.

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use ring::signature::{ED25519, Ed25519KeyPair, KeyPair, UnparsedPublicKey};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const PREFIX: &str = "pyocmd1";
/// How long a signed command may wait to be run.
pub const TTL_MS: u64 = 10 * 60 * 1000;
/// Clock difference allowed between the signer and the cluster.
pub const LEEWAY_MS: u64 = 2 * 60 * 1000;

/// What the signer signs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Claims {
    pub v: u32,
    /// The command's id (pyo.commands).
    pub id: String,
    /// The cluster it's for.
    pub cluster: String,
    /// Grows with every command signed for this cluster.
    pub seq: u64,
    /// Signed at, and expires at (ms since the epoch).
    pub iat: u64,
    pub exp: u64,
    /// The command's `kind`, so the signer can refuse kinds by policy.
    pub kind: String,
    /// Hex SHA-256 of [`SignedCommand::command`].
    pub sha256: String,
}

/// A command with its signature, as the API hands it to the agent.
pub use crate::SignedCommandWire as SignedCommand;

/// A short id for a public key: hex of the first 8 bytes of its SHA-256.
pub fn key_id(public_key: &[u8]) -> String {
    hex8(&Sha256::digest(public_key))
}

fn hex8(b: &[u8]) -> String {
    b.iter().take(8).map(|x| format!("{x:02x}")).collect()
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes).iter().map(|x| format!("{x:02x}")).collect()
}

fn message(claims_b64: &str) -> Vec<u8> {
    format!("{PREFIX}.{claims_b64}").into_bytes()
}

/// A signing key (the signer's), from its PKCS#8 document.
pub struct Signer {
    key: Ed25519KeyPair,
    kid: String,
}

impl Signer {
    pub fn from_pkcs8(pkcs8: &[u8]) -> Result<Self, String> {
        let key = Ed25519KeyPair::from_pkcs8(pkcs8).map_err(|e| format!("not an Ed25519 key: {e}"))?;
        let kid = key_id(key.public_key().as_ref());
        Ok(Self { key, kid })
    }

    /// A new key's PKCS#8 document.
    pub fn generate_pkcs8() -> Result<Vec<u8>, String> {
        let rng = ring::rand::SystemRandom::new();
        Ed25519KeyPair::generate_pkcs8(&rng)
            .map(|d| d.as_ref().to_vec())
            .map_err(|e| format!("couldn't make a key: {e}"))
    }

    /// The public key, base64url (what `PYO_COMMAND_KEY` holds).
    pub fn public_key(&self) -> String {
        B64.encode(self.key.public_key().as_ref())
    }

    pub fn kid(&self) -> &str {
        &self.kid
    }

    /// Signs claims for a command whose JSON hashes to `claims.sha256`.
    /// The command itself is attached by the caller ([`SignedCommand::command`]).
    pub fn sign(&self, claims: &Claims) -> (String, String) {
        let c = B64.encode(serde_json::to_vec(claims).unwrap_or_default());
        let sig = self.key.sign(&message(&c));
        (c, B64.encode(sig.as_ref()))
    }
}

/// Why a command was refused.
#[derive(Debug, Clone, PartialEq)]
pub enum Refused {
    Unsigned,
    UnknownKey,
    BadSignature,
    BadClaims,
    OtherCluster,
    Replayed { seq: u64, last: u64 },
    Expired,
    NotYetValid,
    Tampered,
    IdMismatch,
}

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Refused::Unsigned => write!(f, "it isn't signed"),
            Refused::UnknownKey => write!(f, "it's signed with a key this cluster doesn't trust"),
            Refused::BadSignature => write!(f, "its signature doesn't match"),
            Refused::BadClaims => write!(f, "its signed details can't be read"),
            Refused::OtherCluster => write!(f, "it was signed for another cluster"),
            Refused::Replayed { seq, last } => write!(f, "it was already used (number {seq}, last run {last})"),
            Refused::Expired => write!(f, "it expired"),
            Refused::NotYetValid => write!(f, "it's dated in the future"),
            Refused::Tampered => write!(f, "it was changed after it was signed"),
            Refused::IdMismatch => write!(f, "its id doesn't match what was signed"),
        }
    }
}

/// The keys an agent trusts, from `PYO_COMMAND_KEY` (base64url public keys,
/// comma separated, so a new key can be added before the old one goes).
#[derive(Clone, Debug, Default)]
pub struct TrustedKeys(Vec<(String, Vec<u8>)>);

impl TrustedKeys {
    pub fn parse(v: &str) -> Result<Self, String> {
        let mut keys = vec![];
        for k in v.split(',').map(str::trim).filter(|k| !k.is_empty()) {
            let raw = B64.decode(k).map_err(|_| format!("{k} isn't base64url"))?;
            if raw.len() != 32 {
                return Err(format!("{k} isn't an Ed25519 public key"));
            }
            keys.push((key_id(&raw), raw));
        }
        if keys.is_empty() {
            return Err("no keys".into());
        }
        Ok(Self(keys))
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Checks a signed command for `cluster`, given the last sequence number
    /// this cluster ran. On success, the claims and the command's JSON.
    pub fn verify(
        &self,
        signed: Option<&SignedCommand>,
        envelope_id: &str,
        cluster: &str,
        last_seq: u64,
        now_ms: u64,
    ) -> Result<Claims, Refused> {
        let s = signed.ok_or(Refused::Unsigned)?;
        let key = self
            .0
            .iter()
            .find(|(kid, _)| *kid == s.kid)
            .map(|(_, k)| k)
            .ok_or(Refused::UnknownKey)?;
        let sig = B64.decode(&s.sig).map_err(|_| Refused::BadSignature)?;
        UnparsedPublicKey::new(&ED25519, key)
            .verify(&message(&s.claims), &sig)
            .map_err(|_| Refused::BadSignature)?;
        let raw = B64.decode(&s.claims).map_err(|_| Refused::BadClaims)?;
        let c: Claims = serde_json::from_slice(&raw).map_err(|_| Refused::BadClaims)?;
        if c.v != 1 {
            return Err(Refused::BadClaims);
        }
        if c.cluster != cluster {
            return Err(Refused::OtherCluster);
        }
        if c.id != envelope_id {
            return Err(Refused::IdMismatch);
        }
        if sha256_hex(s.command.as_bytes()) != c.sha256 {
            return Err(Refused::Tampered);
        }
        if now_ms > c.exp.saturating_add(LEEWAY_MS) {
            return Err(Refused::Expired);
        }
        if c.iat > now_ms.saturating_add(LEEWAY_MS) {
            return Err(Refused::NotYetValid);
        }
        if c.seq <= last_seq {
            return Err(Refused::Replayed { seq: c.seq, last: last_seq });
        }
        Ok(c)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup() -> (Signer, TrustedKeys) {
        let s = Signer::from_pkcs8(&Signer::generate_pkcs8().unwrap()).unwrap();
        let t = TrustedKeys::parse(&s.public_key()).unwrap();
        (s, t)
    }

    fn signed(s: &Signer, cluster: &str, seq: u64, command: &str, now: u64) -> SignedCommand {
        let claims = Claims {
            v: 1,
            id: "cmd_1".into(),
            cluster: cluster.into(),
            seq,
            iat: now,
            exp: now + TTL_MS,
            kind: "restart".into(),
            sha256: sha256_hex(command.as_bytes()),
        };
        let (c, sig) = s.sign(&claims);
        SignedCommand { claims: c, sig, kid: s.kid().into(), command: command.into() }
    }

    #[test]
    fn a_good_command_passes() {
        let (s, t) = setup();
        let x = signed(&s, "cl_a", 5, r#"{"kind":"restart"}"#, 1_000_000);
        let c = t.verify(Some(&x), "cmd_1", "cl_a", 4, 1_000_000).unwrap();
        assert_eq!(c.seq, 5);
    }

    #[test]
    fn everything_else_is_refused() {
        let (s, t) = setup();
        let now = 1_000_000;
        let x = signed(&s, "cl_a", 5, r#"{"kind":"restart"}"#, now);
        assert_eq!(t.verify(None, "cmd_1", "cl_a", 0, now), Err(Refused::Unsigned));
        assert_eq!(t.verify(Some(&x), "cmd_1", "cl_b", 0, now), Err(Refused::OtherCluster));
        assert_eq!(t.verify(Some(&x), "cmd_2", "cl_a", 0, now), Err(Refused::IdMismatch));
        assert_eq!(t.verify(Some(&x), "cmd_1", "cl_a", 5, now), Err(Refused::Replayed { seq: 5, last: 5 }));
        assert_eq!(t.verify(Some(&x), "cmd_1", "cl_a", 0, now + TTL_MS + LEEWAY_MS + 1), Err(Refused::Expired));
        let mut tampered = x.clone();
        tampered.command = r#"{"kind":"delete_namespace"}"#.into();
        assert_eq!(t.verify(Some(&tampered), "cmd_1", "cl_a", 0, now), Err(Refused::Tampered));
        let mut forged = x.clone();
        forged.claims = B64.encode(br#"{"v":1}"#);
        assert_eq!(t.verify(Some(&forged), "cmd_1", "cl_a", 0, now), Err(Refused::BadSignature));
        let (other, _) = setup();
        let mut wrong_key = signed(&other, "cl_a", 5, r#"{"kind":"restart"}"#, now);
        assert_eq!(t.verify(Some(&wrong_key), "cmd_1", "cl_a", 0, now), Err(Refused::UnknownKey));
        // Another key's signature under a trusted key id.
        wrong_key.kid = s.kid().into();
        assert_eq!(t.verify(Some(&wrong_key), "cmd_1", "cl_a", 0, now), Err(Refused::BadSignature));
    }
}
