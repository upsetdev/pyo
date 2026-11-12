//! Prefixed random ids ("prj_k3x9q2mf") and validation of ids the dashboard
//! generates itself (it creates them optimistically, then sends them here).

use sha2::{Digest, Sha256};

const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";

pub fn random_string(len: usize) -> String {
    let mut bytes = vec![0u8; len];
    rand::fill(&mut bytes[..]);
    bytes
        .iter()
        .map(|b| ALPHABET[*b as usize % ALPHABET.len()] as char)
        .collect()
}

pub fn new_id(prefix: &str) -> String {
    format!("{prefix}_{}", random_string(12))
}

/// A dashboard-made id: the expected prefix, then 4-40 of [a-z0-9_-].
pub fn valid_client_id(id: &str, prefix: &str) -> bool {
    let Some(rest) = id.strip_prefix(prefix).and_then(|r| r.strip_prefix('_')) else {
        return false;
    };
    (4..=40).contains(&rest.len())
        && rest
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
}

/// 32 random bytes, hex: session cookies and agent tokens.
pub fn secret_token() -> String {
    let mut bytes = [0u8; 32];
    rand::fill(&mut bytes);
    hex::encode(bytes)
}

pub fn sha256_hex(s: &str) -> String {
    hex::encode(Sha256::digest(s.as_bytes()))
}
