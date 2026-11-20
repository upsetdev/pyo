//! Signed agent requests: who an agent is, carried in each request, so it
//! survives a proxy that ends TLS (Cloudflare in front of agent.pyo.dev).
//!
//! The agent makes an Ed25519 key in its cluster at enrollment and sends
//! Pyo only the public half, for a key id. Every request then carries:
//!
//! ```text
//! pyo-agent-key: <key id>
//! pyo-agent-signature: t=<unix seconds>,n=<nonce>,s=<base64url Ed25519 signature>
//! ```
//!
//! over `pyo-agent-req1`, the key id, the method, the path with its query,
//! the time, the nonce and the SHA-256 of the body, one per line. Pyo checks
//! it against the key it registered, refuses a time more than
//! [`MAX_SKEW_SECS`] off and a nonce it has seen, and takes the cluster from
//! the key: never from anything a proxy could add.
//!
//! The cluster's other components sign the same way with keys of their
//! own (Pyo's builders, pyo-static), which the agent makes in the cluster
//! and registers. A body too large to hold (a file a builder uploads) is
//! signed by the hash it declares in [`BODY_SHA256_HEADER`]: Pyo checks the
//! signature against that, and the body against it as it streams.

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use ring::signature::{ED25519, Ed25519KeyPair, KeyPair, UnparsedPublicKey};
use sha2::{Digest, Sha256};

pub const KEY_HEADER: &str = "pyo-agent-key";
pub const SIGNATURE_HEADER: &str = "pyo-agent-signature";
/// The hex SHA-256 of a body signed by its hash ([`RequestKey::sign_hashed`]).
pub const BODY_SHA256_HEADER: &str = "x-pyo-sha256";
/// How far a request's time may be from Pyo's clock.
pub const MAX_SKEW_SECS: i64 = 60;
const PREFIX: &str = "pyo-agent-req1";

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// What gets signed.
pub fn message(key_id: &str, method: &str, path: &str, t: i64, nonce: &str, body: &[u8]) -> Vec<u8> {
    message_hashed(key_id, method, path, t, nonce, &body_sha256(body))
}

/// What gets signed, given the body's hex SHA-256.
pub fn message_hashed(key_id: &str, method: &str, path: &str, t: i64, nonce: &str, body_sha256: &str) -> Vec<u8> {
    format!(
        "{PREFIX}\n{key_id}\n{}\n{path}\n{t}\n{nonce}\n{}",
        method.to_ascii_uppercase(),
        body_sha256.to_ascii_lowercase()
    )
    .into_bytes()
}

/// A body's hex SHA-256.
pub fn body_sha256(body: &[u8]) -> String {
    hex(&Sha256::digest(body))
}

/// An agent's request key.
pub struct RequestKey {
    key: Ed25519KeyPair,
}

impl RequestKey {
    pub fn generate_pkcs8() -> Result<Vec<u8>, String> {
        let rng = ring::rand::SystemRandom::new();
        Ed25519KeyPair::generate_pkcs8(&rng)
            .map(|d| d.as_ref().to_vec())
            .map_err(|e| format!("couldn't make a key: {e}"))
    }

    pub fn from_pkcs8(pkcs8: &[u8]) -> Result<Self, String> {
        Ed25519KeyPair::from_pkcs8(pkcs8)
            .map(|key| Self { key })
            .map_err(|e| format!("not an Ed25519 key: {e}"))
    }

    /// The public key, base64url: what enrollment sends.
    pub fn public_key(&self) -> String {
        B64.encode(self.key.public_key().as_ref())
    }

    /// The two headers for a request, signed at `t` (unix seconds).
    pub fn sign(&self, key_id: &str, method: &str, path: &str, body: &[u8], t: i64) -> [(&'static str, String); 2] {
        self.sign_hashed(key_id, method, path, &body_sha256(body), t)
    }

    /// The same for a body given by its hex SHA-256 (one that's streamed:
    /// send that hash in [`BODY_SHA256_HEADER`] too).
    pub fn sign_hashed(&self, key_id: &str, method: &str, path: &str, body_sha256: &str, t: i64) -> [(&'static str, String); 2] {
        let mut n = [0u8; 16];
        let _ = ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut n);
        let nonce = B64.encode(n);
        let sig = self.key.sign(&message_hashed(key_id, method, path, t, &nonce, body_sha256));
        [
            (KEY_HEADER, key_id.to_owned()),
            (SIGNATURE_HEADER, format!("t={t},n={nonce},s={}", B64.encode(sig.as_ref()))),
        ]
    }
}

/// A signature header's parts: time, nonce and signature.
pub fn parse(header: &str) -> Option<(i64, String, Vec<u8>)> {
    let (mut t, mut n, mut s) = (None, None, None);
    for part in header.split(',') {
        match part.trim().split_once('=')? {
            ("t", v) => t = v.parse().ok(),
            ("n", v) if (8..=64).contains(&v.len()) => n = Some(v.to_owned()),
            ("s", v) => s = B64.decode(v).ok(),
            _ => {}
        }
    }
    Some((t?, n?, s?))
}

/// Why a request's signature doesn't hold.
#[derive(Debug, Clone, PartialEq)]
pub enum Bad {
    Malformed,
    Stale,
    Signature,
}

impl std::fmt::Display for Bad {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Bad::Malformed => "The request's signature can't be read.",
            Bad::Stale => "The request's time is too far from Pyo's clock.",
            Bad::Signature => "The request's signature doesn't match its key.",
        })
    }
}

/// Checks a request against the registered public key (base64url). On
/// success, its nonce, which the caller must not have seen before.
pub fn verify(
    public_key: &str,
    key_id: &str,
    method: &str,
    path: &str,
    body: &[u8],
    header: &str,
    now: i64,
) -> Result<String, Bad> {
    verify_hashed(public_key, key_id, method, path, &body_sha256(body), header, now)
}

/// [`verify`], given the body's hex SHA-256 (the caller then checks the
/// body against it).
pub fn verify_hashed(
    public_key: &str,
    key_id: &str,
    method: &str,
    path: &str,
    body_sha256: &str,
    header: &str,
    now: i64,
) -> Result<String, Bad> {
    let (t, nonce, sig) = parse(header).ok_or(Bad::Malformed)?;
    if (now - t).abs() > MAX_SKEW_SECS {
        return Err(Bad::Stale);
    }
    let key = B64.decode(public_key).map_err(|_| Bad::Signature)?;
    UnparsedPublicKey::new(&ED25519, key)
        .verify(&message_hashed(key_id, method, path, t, &nonce, body_sha256), &sig)
        .map_err(|_| Bad::Signature)?;
    Ok(nonce)
}

/// A public key as enrollment sends it: 32 bytes, base64url.
pub fn valid_public_key(k: &str) -> bool {
    B64.decode(k.trim()).is_ok_and(|b| b.len() == 32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signed_requests_check_out_and_nothing_else_does() {
        let k = RequestKey::from_pkcs8(&RequestKey::generate_pkcs8().unwrap()).unwrap();
        let pk = k.public_key();
        let now = 1_800_000_000;
        let [_, (_, sig)] = k.sign("key_1", "post", "/api/agent/v1/heartbeat", b"{}", now);
        assert!(verify(&pk, "key_1", "POST", "/api/agent/v1/heartbeat", b"{}", &sig, now + 30).is_ok());
        assert_eq!(verify(&pk, "key_1", "POST", "/api/agent/v1/heartbeat", b"{}", &sig, now + 61), Err(Bad::Stale));
        assert_eq!(verify(&pk, "key_1", "POST", "/api/agent/v1/heartbeat", b"{ }", &sig, now), Err(Bad::Signature));
        assert_eq!(verify(&pk, "key_1", "POST", "/api/agent/v1/commands", b"{}", &sig, now), Err(Bad::Signature));
        assert_eq!(verify(&pk, "key_2", "POST", "/api/agent/v1/heartbeat", b"{}", &sig, now), Err(Bad::Signature));
        assert_eq!(verify(&pk, "key_1", "GET", "/api/agent/v1/heartbeat", b"{}", &sig, now), Err(Bad::Signature));
        let other = RequestKey::from_pkcs8(&RequestKey::generate_pkcs8().unwrap()).unwrap();
        assert_eq!(verify(&other.public_key(), "key_1", "POST", "/api/agent/v1/heartbeat", b"{}", &sig, now), Err(Bad::Signature));
        assert_eq!(verify(&pk, "key_1", "POST", "/x", b"", "t=1", now), Err(Bad::Malformed));
        assert!(valid_public_key(&pk));
    }

    #[test]
    fn a_streamed_body_is_signed_by_its_hash() {
        let k = RequestKey::from_pkcs8(&RequestKey::generate_pkcs8().unwrap()).unwrap();
        let pk = k.public_key();
        let now = 1_800_000_000;
        let file = b"<h1>hi</h1>";
        let path = "/api/builder/v1/jobs/j1/site/files/index.html";
        let [_, (_, sig)] = k.sign_hashed("key_1", "PUT", path, &body_sha256(file), now);
        assert!(verify_hashed(&pk, "key_1", "PUT", path, &body_sha256(file), &sig, now).is_ok());
        // The same as signing the body itself.
        assert!(verify(&pk, "key_1", "PUT", path, file, &sig, now).is_ok());
        assert_eq!(verify_hashed(&pk, "key_1", "PUT", path, &body_sha256(b"other"), &sig, now), Err(Bad::Signature));
    }
}
