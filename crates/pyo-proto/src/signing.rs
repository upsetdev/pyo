//! Signed commands: a command the agent runs only when a signer it trusts
//! signed it for its own cluster.
//!
//! A hosted Pyo (Pyo Cloud) keeps its signing key out of the API: a separate
//! signer signs each command. The agent is told the signer's public key at
//! install (`PYO_COMMAND_KEY`) and refuses anything else.
//!
//! What signing protects against: a stolen copy of the database (it holds
//! no signing key), a command changed or played again on its way to the
//! agent, a command signed for one cluster run on another, kinds that would
//! widen the agent's roles (the signer refuses them), and agent images that
//! weren't released (`update_agent`, when the signer has
//! `SIGNER_AGENT_IMAGES`).
//!
//! What it doesn't protect against: a write to the database, or an API that
//! was broken into. When an agent polls, the API reads its waiting commands
//! from `pyo.commands` and asks the signer to sign them. So such a write can
//! get any command signed that the signer doesn't refuse.
//!
//! ```text
//! sig = Ed25519("pyocmd1." + base64url(claims JSON))
//! ```
//!
//! The claims name the command (its id and kind), the cluster it's for, a
//! sequence number that only grows per cluster, when it was signed and when
//! it expires, and the SHA-256 of the command's exact JSON. The signer sees
//! only the command's kind and hash, so the secrets in it (sealed env values
//! opened for the agent) never reach it. The exception is `update_agent`,
//! which carries no secrets: the signer reads it whole to check the image.
//!
//! Answers the agent acts on (the heartbeat's reply: labels, taints, public
//! traffic, observability; a certificate; a registry login) are signed the
//! same way, under their own prefix so neither passes for the other:
//!
//! ```text
//! sig = Ed25519("pyoreply1." + base64url(reply claims JSON))
//! ```
//!
//! The reply claims name the cluster, the nonce the agent sent with its
//! request ([`crate::REPLY_NONCE_HEADER`]), when it was signed and when it
//! expires, and the SHA-256 of the reply's exact JSON. The nonce ties an
//! answer to one request: an old answer can't be played back for a new one.
//!
//! A trusted key may hand signing to a session key for a while (at most
//! [`MAX_DELEGATION_MS`]), so a key kept in a KMS signs a few times an hour
//! rather than every heartbeat's reply:
//!
//! ```text
//! sig = Ed25519("pyodel1." + base64url(delegation claims JSON))
//! ```
//!
//! The delegation names the session key, what it may sign ([`PURPOSE_REPLIES`],
//! [`PURPOSE_COMMANDS`]) and when. A command or reply signed by the session
//! key carries the delegation; the agent checks the delegation against its
//! trusted keys, then the signature against the session key, and takes it
//! only when it was signed and expires inside the delegation's window.
//! Kinds in [`ROOT_ONLY_KINDS`] are never taken from a session key.

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
pub const REPLY_PREFIX: &str = "pyoreply1";
/// How long a signed reply is good for (it answers a request made now).
pub const REPLY_TTL_MS: u64 = 60 * 1000;
pub const DELEGATION_PREFIX: &str = "pyodel1";
/// The longest a session key may sign for, from one delegation.
pub const MAX_DELEGATION_MS: u64 = 2 * 60 * 60 * 1000;
/// A session key may sign replies.
pub const PURPOSE_REPLIES: &str = "replies";
/// A session key may sign commands (but never [`ROOT_ONLY_KINDS`]).
pub const PURPOSE_COMMANDS: &str = "commands";
/// Command kinds an agent takes only with a trusted key's own signature:
/// an agent update names the image every customer's agent will run.
pub const ROOT_ONLY_KINDS: &[&str] = &["update_agent"];

/// What a trusted key signs to let a session key sign.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DelegationClaims {
    pub v: u32,
    /// The session key's public half, base64url.
    pub key: String,
    /// What it may sign ([`PURPOSE_REPLIES`], [`PURPOSE_COMMANDS`]).
    pub purposes: Vec<String>,
    /// Good from, and until (ms since the epoch). What the session key
    /// signs must be signed and expire inside this window.
    pub nbf: u64,
    pub exp: u64,
    /// Grows with every delegation the signer makes (for its audit log).
    pub serial: u64,
}

impl DelegationClaims {
    /// The claims as sent (base64url JSON), and the exact bytes signed.
    pub fn signing_input(&self) -> (String, Vec<u8>) {
        let c = B64.encode(serde_json::to_vec(self).unwrap_or_default());
        let m = format!("{DELEGATION_PREFIX}.{c}").into_bytes();
        (c, m)
    }
}

impl Claims {
    /// The claims as sent (base64url JSON), and the exact bytes signed.
    /// For signers whose key is elsewhere (a KMS, a session key).
    pub fn signing_input(&self) -> (String, Vec<u8>) {
        let c = B64.encode(serde_json::to_vec(self).unwrap_or_default());
        let m = message(&c);
        (c, m)
    }
}

impl ReplyClaims {
    /// The claims as sent (base64url JSON), and the exact bytes signed.
    pub fn signing_input(&self) -> (String, Vec<u8>) {
        let c = B64.encode(serde_json::to_vec(self).unwrap_or_default());
        let m = reply_message(&c);
        (c, m)
    }
}

/// Whether `sig` is `public_key`'s Ed25519 signature over `message`.
pub fn verify_ed25519(public_key: &[u8], message: &[u8], sig: &[u8]) -> bool {
    UnparsedPublicKey::new(&ED25519, public_key).verify(message, sig).is_ok()
}

/// A public key's base64url form, as `PYO_COMMAND_KEY` and delegations hold it.
pub fn encode_key(public_key: &[u8]) -> String {
    B64.encode(public_key)
}

/// What the signer signs for a reply.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReplyClaims {
    pub v: u32,
    /// The cluster it's for.
    pub cluster: String,
    /// The nonce of the request it answers.
    pub nonce: String,
    /// Signed at, and expires at (ms since the epoch).
    pub iat: u64,
    pub exp: u64,
    /// Hex SHA-256 of [`crate::SignedReplyWire::reply`].
    pub sha256: String,
}

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

/// A new reply nonce: 16 random bytes, base64url.
pub fn new_nonce() -> String {
    let mut n = [0u8; 16];
    let _ = ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut n);
    B64.encode(n)
}

fn reply_message(claims_b64: &str) -> Vec<u8> {
    format!("{REPLY_PREFIX}.{claims_b64}").into_bytes()
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

    /// The public key's raw bytes.
    pub fn public_key_bytes(&self) -> &[u8] {
        self.key.public_key().as_ref()
    }

    /// Signs claims for a command whose JSON hashes to `claims.sha256`.
    /// The command itself is attached by the caller ([`SignedCommand::command`]).
    pub fn sign(&self, claims: &Claims) -> (String, String) {
        let (c, m) = claims.signing_input();
        (c, B64.encode(self.key.sign(&m).as_ref()))
    }

    /// Signs a reply's claims (its JSON hashes to `claims.sha256`).
    pub fn sign_reply(&self, claims: &ReplyClaims) -> (String, String) {
        let (c, m) = claims.signing_input();
        (c, B64.encode(self.key.sign(&m).as_ref()))
    }

    /// Lets another key sign for a while ([`DelegationClaims`]).
    pub fn delegate(&self, claims: &DelegationClaims) -> crate::DelegationWire {
        let (c, m) = claims.signing_input();
        crate::DelegationWire { claims: c, sig: B64.encode(self.key.sign(&m).as_ref()), kid: self.kid.clone() }
    }

    /// The raw Ed25519 signature over `message`.
    pub fn sign_raw(&self, message: &[u8]) -> Vec<u8> {
        self.key.sign(message).as_ref().to_vec()
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
    /// A reply to another request (its nonce isn't the one sent).
    OtherRequest,
    /// Signed by a session key whose delegation isn't a trusted key's (or
    /// can't be read, or is longer than [`MAX_DELEGATION_MS`]).
    BadDelegation,
    /// Signed by a session key whose delegation expired (or isn't good yet).
    DelegationExpired,
    /// Signed by a session key outside its delegation's window.
    OutsideDelegation,
    /// Signed by a session key that may not sign this.
    NotDelegated,
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
            Refused::OtherRequest => write!(f, "it answers another request"),
            Refused::BadDelegation => write!(f, "its session key wasn't handed over by a key this cluster trusts"),
            Refused::DelegationExpired => write!(f, "its session key's time is over"),
            Refused::OutsideDelegation => write!(f, "it was signed outside its session key's time"),
            Refused::NotDelegated => write!(f, "its session key may not sign it"),
        }
    }
}

/// The keys an agent trusts, from `PYO_COMMAND_KEY` (base64url public keys,
/// comma separated, so a new key can be added before the old one goes).
#[derive(Clone, Debug, Default)]
pub struct TrustedKeys {
    keys: Vec<(String, Vec<u8>)>,
    /// Delegations already checked against `keys` (the latest few). Their
    /// windows are still checked on every use.
    delegations: std::sync::Arc<std::sync::Mutex<Vec<Checked>>>,
}

/// A delegation checked against a trusted key: its claims and session key.
type Checked = (crate::DelegationWire, DelegationClaims, Vec<u8>);

/// A signature's checked origin: a trusted key, or a session key it
/// handed signing to (with that delegation's claims).
enum Origin {
    Trusted,
    Session(DelegationClaims),
}

/// How many checked delegations an agent keeps.
const DELEGATION_CACHE: usize = 4;

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
        Ok(Self { keys, delegations: Default::default() })
    }

    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    fn trusted(&self, kid: &str) -> Option<&[u8]> {
        self.keys.iter().find(|(k, _)| k == kid).map(|(_, k)| k.as_slice())
    }

    /// A delegation's claims and session key, once it checks out against
    /// a trusted key (from the cache when it was checked before).
    fn delegation(&self, d: &crate::DelegationWire) -> Result<(DelegationClaims, Vec<u8>), Refused> {
        if let Ok(cache) = self.delegations.lock()
            && let Some((_, c, k)) = cache.iter().find(|(w, _, _)| w == d)
        {
            return Ok((c.clone(), k.clone()));
        }
        let root = self.trusted(&d.kid).ok_or(Refused::BadDelegation)?;
        let sig = B64.decode(&d.sig).map_err(|_| Refused::BadDelegation)?;
        let msg = format!("{DELEGATION_PREFIX}.{}", d.claims).into_bytes();
        if !verify_ed25519(root, &msg, &sig) {
            return Err(Refused::BadDelegation);
        }
        let raw = B64.decode(&d.claims).map_err(|_| Refused::BadDelegation)?;
        let c: DelegationClaims = serde_json::from_slice(&raw).map_err(|_| Refused::BadDelegation)?;
        let key = B64.decode(&c.key).map_err(|_| Refused::BadDelegation)?;
        if c.v != 1 || key.len() != 32 || c.exp <= c.nbf || c.exp - c.nbf > MAX_DELEGATION_MS {
            return Err(Refused::BadDelegation);
        }
        if let Ok(mut cache) = self.delegations.lock() {
            if cache.len() >= DELEGATION_CACHE {
                cache.remove(0);
            }
            cache.push((d.clone(), c.clone(), key.clone()));
        }
        Ok((c, key))
    }

    /// Checks `sig` over `msg` from the key `kid`: a trusted key, or a
    /// session key whose delegation lets it sign for `purpose` now.
    fn check_signature(
        &self,
        kid: &str,
        delegation: Option<&crate::DelegationWire>,
        purpose: &str,
        msg: &[u8],
        sig: &str,
        now_ms: u64,
    ) -> Result<Origin, Refused> {
        let sig = B64.decode(sig).map_err(|_| Refused::BadSignature)?;
        let Some(d) = delegation else {
            let key = self.trusted(kid).ok_or(Refused::UnknownKey)?;
            return verify_ed25519(key, msg, &sig).then_some(Origin::Trusted).ok_or(Refused::BadSignature);
        };
        let (c, key) = self.delegation(d)?;
        if key_id(&key) != kid {
            return Err(Refused::UnknownKey);
        }
        if !c.purposes.iter().any(|p| p == purpose) {
            return Err(Refused::NotDelegated);
        }
        if now_ms > c.exp.saturating_add(LEEWAY_MS) || c.nbf > now_ms.saturating_add(LEEWAY_MS) {
            return Err(Refused::DelegationExpired);
        }
        if !verify_ed25519(&key, msg, &sig) {
            return Err(Refused::BadSignature);
        }
        Ok(Origin::Session(c))
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
        let origin =
            self.check_signature(&s.kid, s.delegation.as_ref(), PURPOSE_COMMANDS, &message(&s.claims), &s.sig, now_ms)?;
        let raw = B64.decode(&s.claims).map_err(|_| Refused::BadClaims)?;
        let c: Claims = serde_json::from_slice(&raw).map_err(|_| Refused::BadClaims)?;
        if c.v != 1 {
            return Err(Refused::BadClaims);
        }
        if let Origin::Session(d) = &origin {
            if ROOT_ONLY_KINDS.contains(&c.kind.as_str()) {
                return Err(Refused::NotDelegated);
            }
            if c.iat < d.nbf || c.exp > d.exp {
                return Err(Refused::OutsideDelegation);
            }
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

    /// Checks a signed reply for `cluster`, to the request sent with
    /// `nonce`. On success, the reply's JSON (what was signed).
    pub fn verify_reply<'a>(
        &self,
        signed: Option<&'a crate::SignedReplyWire>,
        cluster: &str,
        nonce: &str,
        now_ms: u64,
    ) -> Result<&'a str, Refused> {
        let s = signed.ok_or(Refused::Unsigned)?;
        let origin =
            self.check_signature(&s.kid, s.delegation.as_ref(), PURPOSE_REPLIES, &reply_message(&s.claims), &s.sig, now_ms)?;
        let raw = B64.decode(&s.claims).map_err(|_| Refused::BadClaims)?;
        let c: ReplyClaims = serde_json::from_slice(&raw).map_err(|_| Refused::BadClaims)?;
        if c.v != 1 {
            return Err(Refused::BadClaims);
        }
        if let Origin::Session(d) = &origin
            && (c.iat < d.nbf || c.exp > d.exp)
        {
            return Err(Refused::OutsideDelegation);
        }
        if c.cluster != cluster {
            return Err(Refused::OtherCluster);
        }
        if nonce.is_empty() || c.nonce != nonce {
            return Err(Refused::OtherRequest);
        }
        if sha256_hex(s.reply.as_bytes()) != c.sha256 {
            return Err(Refused::Tampered);
        }
        if now_ms > c.exp.saturating_add(LEEWAY_MS) {
            return Err(Refused::Expired);
        }
        if c.iat > now_ms.saturating_add(LEEWAY_MS) {
            return Err(Refused::NotYetValid);
        }
        Ok(&s.reply)
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
        SignedCommand { claims: c, sig, kid: s.kid().into(), command: command.into(), delegation: None }
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

    fn reply(s: &Signer, cluster: &str, nonce: &str, body: &str, now: u64) -> crate::SignedReplyWire {
        let claims = ReplyClaims {
            v: 1,
            cluster: cluster.into(),
            nonce: nonce.into(),
            iat: now,
            exp: now + REPLY_TTL_MS,
            sha256: sha256_hex(body.as_bytes()),
        };
        let (c, sig) = s.sign_reply(&claims);
        crate::SignedReplyWire { claims: c, sig, kid: s.kid().into(), reply: body.into(), delegation: None }
    }

    #[test]
    fn a_good_reply_passes() {
        let (s, t) = setup();
        let now = 1_000_000;
        let r = reply(&s, "cl_a", "nonce-1234", r#"{"publicTraffic":true}"#, now);
        assert_eq!(t.verify_reply(Some(&r), "cl_a", "nonce-1234", now + 5_000), Ok(r#"{"publicTraffic":true}"#));
    }

    #[test]
    fn bad_replies_are_refused() {
        let (s, t) = setup();
        let now = 1_000_000;
        let r = reply(&s, "cl_a", "nonce-1234", r#"{"publicTraffic":false}"#, now);
        // Unsigned, where signing is required.
        assert_eq!(t.verify_reply(None, "cl_a", "nonce-1234", now), Err(Refused::Unsigned));
        // Another cluster's.
        assert_eq!(t.verify_reply(Some(&r), "cl_b", "nonce-1234", now), Err(Refused::OtherCluster));
        // Played back for a later request.
        assert_eq!(t.verify_reply(Some(&r), "cl_a", "nonce-5678", now), Err(Refused::OtherRequest));
        assert_eq!(t.verify_reply(Some(&r), "cl_a", "", now), Err(Refused::OtherRequest));
        // Too old.
        assert_eq!(
            t.verify_reply(Some(&r), "cl_a", "nonce-1234", now + REPLY_TTL_MS + LEEWAY_MS + 1),
            Err(Refused::Expired)
        );
        // Dated ahead.
        let ahead = reply(&s, "cl_a", "nonce-1234", "{}", now + LEEWAY_MS + 60_000);
        assert_eq!(t.verify_reply(Some(&ahead), "cl_a", "nonce-1234", now), Err(Refused::NotYetValid));
        // Changed after signing.
        let mut tampered = r.clone();
        tampered.reply = r#"{"publicTraffic":true}"#.into();
        assert_eq!(t.verify_reply(Some(&tampered), "cl_a", "nonce-1234", now), Err(Refused::Tampered));
        // Signed by a key the cluster doesn't trust.
        let (other, _) = setup();
        let mut forged = reply(&other, "cl_a", "nonce-1234", "{}", now);
        assert_eq!(t.verify_reply(Some(&forged), "cl_a", "nonce-1234", now), Err(Refused::UnknownKey));
        forged.kid = s.kid().into();
        assert_eq!(t.verify_reply(Some(&forged), "cl_a", "nonce-1234", now), Err(Refused::BadSignature));
    }

    #[test]
    fn a_reply_signature_is_no_command_signature() {
        let (s, t) = setup();
        let now = 1_000_000;
        let body = r#"{"kind":"restart"}"#;
        // A reply's claims and signature passed off as a command's.
        let r = reply(&s, "cl_a", "nonce-1234", body, now);
        let as_command = SignedCommand { claims: r.claims.clone(), sig: r.sig.clone(), kid: r.kid.clone(), command: body.into(), delegation: None };
        assert_eq!(t.verify(Some(&as_command), "cmd_1", "cl_a", 0, now), Err(Refused::BadSignature));
        // And a command's as a reply's.
        let c = signed(&s, "cl_a", 1, body, now);
        let as_reply = crate::SignedReplyWire { claims: c.claims, sig: c.sig, kid: c.kid, reply: body.into(), delegation: None };
        assert_eq!(t.verify_reply(Some(&as_reply), "cl_a", "nonce-1234", now), Err(Refused::BadSignature));
    }

    /// A session key, and the root's delegation to it.
    fn session(root: &Signer, purposes: &[&str], nbf: u64, exp: u64) -> (Signer, crate::DelegationWire) {
        let k = Signer::from_pkcs8(&Signer::generate_pkcs8().unwrap()).unwrap();
        let d = root.delegate(&DelegationClaims {
            v: 1,
            key: k.public_key(),
            purposes: purposes.iter().map(|p| p.to_string()).collect(),
            nbf,
            exp,
            serial: 1,
        });
        (k, d)
    }

    const HOUR: u64 = 60 * 60 * 1000;

    #[test]
    fn a_session_key_signs_replies_and_commands_it_was_given() {
        let (root, t) = setup();
        let now = 10 * HOUR;
        let (k, d) = session(&root, &[PURPOSE_REPLIES, PURPOSE_COMMANDS], now - 1000, now + HOUR);
        let mut r = reply(&k, "cl_a", "nonce-1234", "{}", now);
        r.delegation = Some(d.clone());
        assert_eq!(t.verify_reply(Some(&r), "cl_a", "nonce-1234", now), Ok("{}"));
        // Checked once, then from the cache: the same answer.
        assert_eq!(t.verify_reply(Some(&r), "cl_a", "nonce-1234", now + 1), Ok("{}"));
        let mut c = signed(&k, "cl_a", 5, r#"{"kind":"restart"}"#, now);
        c.delegation = Some(d);
        assert_eq!(t.verify(Some(&c), "cmd_1", "cl_a", 4, now).map(|c| c.seq), Ok(5));
        // Without its delegation, the session key is just an unknown key.
        c.delegation = None;
        assert_eq!(t.verify(Some(&c), "cmd_1", "cl_a", 4, now), Err(Refused::UnknownKey));
    }

    #[test]
    fn an_expired_delegation_is_refused() {
        let (root, t) = setup();
        let now = 10 * HOUR;
        let (k, d) = session(&root, &[PURPOSE_REPLIES], now - 2 * HOUR + 1000, now - HOUR);
        // Signed while the delegation held, checked after it ended.
        let mut r = reply(&k, "cl_a", "nonce-1234", "{}", now - HOUR - REPLY_TTL_MS);
        r.delegation = Some(d.clone());
        assert_eq!(t.verify_reply(Some(&r), "cl_a", "nonce-1234", now), Err(Refused::DelegationExpired));
        // And one that isn't good yet.
        let (k, d) = session(&root, &[PURPOSE_REPLIES], now + HOUR, now + 2 * HOUR);
        let mut r = reply(&k, "cl_a", "nonce-1234", "{}", now + HOUR);
        r.delegation = Some(d);
        assert_eq!(t.verify_reply(Some(&r), "cl_a", "nonce-1234", now), Err(Refused::DelegationExpired));
    }

    #[test]
    fn a_delegation_not_signed_by_a_trusted_key_is_refused() {
        let (_, t) = setup();
        let (other, _) = setup();
        let now = 10 * HOUR;
        let (k, d) = session(&other, &[PURPOSE_REPLIES], now - 1000, now + HOUR);
        let mut r = reply(&k, "cl_a", "nonce-1234", "{}", now);
        r.delegation = Some(d.clone());
        assert_eq!(t.verify_reply(Some(&r), "cl_a", "nonce-1234", now), Err(Refused::BadDelegation));
        // Under a trusted key's id.
        let (root, t) = setup();
        let mut forged = d;
        forged.kid = root.kid().into();
        r.delegation = Some(forged.clone());
        assert_eq!(t.verify_reply(Some(&r), "cl_a", "nonce-1234", now), Err(Refused::BadDelegation));
        // A trusted delegation changed after signing (a longer window).
        let (k, d) = session(&root, &[PURPOSE_REPLIES], now - 1000, now + HOUR);
        let mut c: DelegationClaims = serde_json::from_slice(&B64.decode(&d.claims).unwrap()).unwrap();
        c.exp = now + 100 * HOUR;
        let changed = crate::DelegationWire { claims: c.signing_input().0, ..d };
        let mut r = reply(&k, "cl_a", "nonce-1234", "{}", now);
        r.delegation = Some(changed);
        assert_eq!(t.verify_reply(Some(&r), "cl_a", "nonce-1234", now), Err(Refused::BadDelegation));
        // Signed by the root, but longer than an agent accepts.
        let (k, d) = session(&root, &[PURPOSE_REPLIES], now - 1000, now + MAX_DELEGATION_MS);
        let mut r = reply(&k, "cl_a", "nonce-1234", "{}", now);
        r.delegation = Some(d);
        assert_eq!(t.verify_reply(Some(&r), "cl_a", "nonce-1234", now), Err(Refused::BadDelegation));
        // Another session key's signature under this delegation.
        let (_, d) = session(&root, &[PURPOSE_REPLIES], now - 1000, now + HOUR);
        let (k2, _) = session(&root, &[PURPOSE_REPLIES], now - 1000, now + HOUR);
        let mut r = reply(&k2, "cl_a", "nonce-1234", "{}", now);
        r.delegation = Some(d);
        assert_eq!(t.verify_reply(Some(&r), "cl_a", "nonce-1234", now), Err(Refused::UnknownKey));
    }

    #[test]
    fn a_session_key_is_refused_outside_its_window() {
        let (root, t) = setup();
        let now = 10 * HOUR;
        let (k, d) = session(&root, &[PURPOSE_REPLIES, PURPOSE_COMMANDS], now, now + HOUR);
        // Dated before the delegation starts.
        let mut r = reply(&k, "cl_a", "nonce-1234", "{}", now - 30_000);
        r.delegation = Some(d.clone());
        assert_eq!(t.verify_reply(Some(&r), "cl_a", "nonce-1234", now), Err(Refused::OutsideDelegation));
        // Good past the delegation's end.
        let mut c = signed(&k, "cl_a", 1, "{}", now + HOUR - 60_000);
        c.delegation = Some(d);
        assert_eq!(t.verify(Some(&c), "cmd_1", "cl_a", 0, now + HOUR - 60_000), Err(Refused::OutsideDelegation));
    }

    #[test]
    fn a_session_key_signs_only_what_it_was_given() {
        let (root, t) = setup();
        let now = 10 * HOUR;
        let (k, d) = session(&root, &[PURPOSE_REPLIES], now - 1000, now + HOUR);
        // Replies only: no commands.
        let mut c = signed(&k, "cl_a", 1, "{}", now);
        c.delegation = Some(d);
        assert_eq!(t.verify(Some(&c), "cmd_1", "cl_a", 0, now), Err(Refused::NotDelegated));
        // Commands only: no replies.
        let (k, d) = session(&root, &[PURPOSE_COMMANDS], now - 1000, now + HOUR);
        let mut r = reply(&k, "cl_a", "nonce-1234", "{}", now);
        r.delegation = Some(d.clone());
        assert_eq!(t.verify_reply(Some(&r), "cl_a", "nonce-1234", now), Err(Refused::NotDelegated));
        // Never an agent update, which only the trusted key signs.
        let body = r#"{"kind":"update_agent","image":"evil.example/agent:1"}"#;
        let claims = Claims {
            v: 1,
            id: "cmd_1".into(),
            cluster: "cl_a".into(),
            seq: 1,
            iat: now,
            exp: now + TTL_MS,
            kind: "update_agent".into(),
            sha256: sha256_hex(body.as_bytes()),
        };
        let (c, sig) = k.sign(&claims);
        let update = SignedCommand { claims: c, sig, kid: k.kid().into(), command: body.into(), delegation: Some(d) };
        assert_eq!(t.verify(Some(&update), "cmd_1", "cl_a", 0, now), Err(Refused::NotDelegated));
        let (c, sig) = root.sign(&claims);
        let update = SignedCommand { claims: c, sig, kid: root.kid().into(), command: body.into(), delegation: None };
        assert!(t.verify(Some(&update), "cmd_1", "cl_a", 0, now).is_ok());
    }
}
