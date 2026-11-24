//! What an outbound daemon keeps on disk, in its pool directory, root's
//! only (mode 0600):
//!
//! - `key`: its Ed25519 key (PKCS#8), the key's id at Pyo and when it
//!   stops working. Replaced whole on renewal.
//! - `outbound.json`: its server and team, the keys it trusts for
//!   commands (from the join command) and the last command number it ran.
//!
//! Each is written to a new file and renamed, so a crash leaves the old
//! one or the new one, never half of either.

use std::path::{Path, PathBuf};

use anyhow::Context;
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use serde::{Deserialize, Serialize};

pub const KEY_FILE: &str = "key";
pub const STATE_FILE: &str = "outbound.json";

/// The server's key, as kept.
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KeyFile {
    pub key_id: String,
    /// When Pyo stops taking it, and when it was issued (ms since the epoch).
    pub not_after: i64,
    pub issued_at: i64,
    /// The PKCS#8 document, base64url.
    pub pkcs8: String,
}

impl std::fmt::Debug for KeyFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KeyFile")
            .field("key_id", &self.key_id)
            .field("not_after", &self.not_after)
            .finish_non_exhaustive()
    }
}

impl KeyFile {
    pub fn new(key_id: &str, not_after: i64, issued_at: i64, pkcs8: &[u8]) -> KeyFile {
        KeyFile {
            key_id: key_id.to_owned(),
            not_after,
            issued_at,
            pkcs8: B64.encode(pkcs8),
        }
    }

    pub fn request_key(&self) -> anyhow::Result<pyo_proto::agentauth::RequestKey> {
        let raw = B64
            .decode(&self.pkcs8)
            .context("The key file isn't base64url.")?;
        pyo_proto::agentauth::RequestKey::from_pkcs8(&raw).map_err(anyhow::Error::msg)
    }
}

/// Who the server is and what it ran last.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct State {
    pub server_id: String,
    pub team_id: String,
    /// The keys commands are signed with (base64url, comma separated):
    /// the join command's, trusted on first use.
    pub command_keys: String,
    /// The sequence number of the last command it took. Only grows.
    pub last_seq: u64,
}

/// The files, in the daemon's pool directory.
#[derive(Debug, Clone)]
pub struct Store {
    pub dir: PathBuf,
}

impl Store {
    pub fn new(dir: &Path) -> Store {
        Store {
            dir: dir.to_owned(),
        }
    }

    pub fn key(&self) -> anyhow::Result<KeyFile> {
        read(&self.dir.join(KEY_FILE))
    }

    pub fn save_key(&self, k: &KeyFile) -> anyhow::Result<()> {
        write(&self.dir.join(KEY_FILE), k)
    }

    pub fn state(&self) -> anyhow::Result<State> {
        read(&self.dir.join(STATE_FILE))
    }

    pub fn save_state(&self, s: &State) -> anyhow::Result<()> {
        write(&self.dir.join(STATE_FILE), s)
    }

    /// Whether the server joined: both files are there.
    pub fn joined(&self) -> bool {
        self.dir.join(KEY_FILE).exists() && self.dir.join(STATE_FILE).exists()
    }
}

fn read<T: serde::de::DeserializeOwned>(p: &Path) -> anyhow::Result<T> {
    let text = std::fs::read(p).with_context(|| format!("reading {}", p.display()))?;
    serde_json::from_slice(&text).with_context(|| format!("parsing {}", p.display()))
}

fn write<T: Serialize>(p: &Path, v: &T) -> anyhow::Result<()> {
    let data = serde_json::to_vec_pretty(v)?;
    crate::edge::write_private(p, &data)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn files_are_roots_only_and_read_back() {
        let dir = std::env::temp_dir().join(format!("pyo-sbx-store-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let st = Store::new(&dir);
        assert!(!st.joined());
        let pkcs8 = pyo_proto::agentauth::RequestKey::generate_pkcs8().unwrap();
        let k = KeyFile::new("sbsk_1", 2, 1, &pkcs8);
        st.save_key(&k).unwrap();
        let s = State {
            server_id: "sbs_1".into(),
            team_id: "team_1".into(),
            command_keys: "AAAA".into(),
            last_seq: 7,
        };
        st.save_state(&s).unwrap();
        assert!(st.joined());
        assert_eq!(st.state().unwrap(), s);
        let back = st.key().unwrap();
        assert_eq!(back.key_id, "sbsk_1");
        assert!(back.request_key().is_ok());
        assert!(
            !format!("{back:?}").contains(&back.pkcs8),
            "the key never prints"
        );
        for f in [KEY_FILE, STATE_FILE] {
            let mode = std::fs::metadata(dir.join(f)).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "{f}");
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
