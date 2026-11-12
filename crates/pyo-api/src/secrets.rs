//! Sealing installation credentials at rest (GitHub app key, registry and
//! SMTP passwords, Tailscale keys, deploy hook tokens, certificate keys).
//! XSalsa20-Poly1305 (NaCl secretbox) with PYO_SECRET_KEY (64 hex chars);
//! each value gets its own random nonce. Stored as `sb1:` + base64(nonce ||
//! ciphertext). Teams' own values use envelope encryption instead (vault.rs).

use base64::{Engine, engine::general_purpose::STANDARD};
use crypto_secretbox::{Key as BoxKey, Nonce as BoxNonce, XSalsa20Poly1305};

const PREFIX: &str = "sb1:";

pub struct Sealer {
    secretbox: Option<XSalsa20Poly1305>,
}

impl Sealer {
    pub fn from_env() -> Self {
        let key = std::env::var("PYO_SECRET_KEY")
            .ok()
            .and_then(|k| hex::decode(k.trim()).ok())
            .filter(|k| k.len() == 32);
        if key.is_none() {
            tracing::warn!(
                "PYO_SECRET_KEY isn't set (64 hex chars); saving integration keys is disabled"
            );
        }
        Self {
            secretbox: key
                .as_ref()
                .and_then(|k| <[u8; 32]>::try_from(k.as_slice()).ok())
                .map(|k| {
                    <XSalsa20Poly1305 as crypto_secretbox::aead::KeyInit>::new(&BoxKey::from(k))
                }),
        }
    }

    pub fn seal(&self, plain: &str) -> anyhow::Result<String> {
        use crypto_secretbox::aead::Aead as _;
        let cipher = self
            .secretbox
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("no PYO_SECRET_KEY"))?;
        let mut nonce = [0u8; 24];
        rand::fill(&mut nonce);
        let sealed = cipher
            .encrypt(&BoxNonce::from(nonce), plain.as_bytes())
            .map_err(|_| anyhow::anyhow!("encryption failed"))?;
        let mut out = nonce.to_vec();
        out.extend(sealed);
        Ok(format!("{PREFIX}{}", STANDARD.encode(out)))
    }

    pub fn open(&self, sealed: &str) -> anyhow::Result<String> {
        use crypto_secretbox::aead::Aead as _;
        let body = sealed
            .strip_prefix(PREFIX)
            .ok_or_else(|| anyhow::anyhow!("not a sealed value"))?;
        let cipher = self
            .secretbox
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("no PYO_SECRET_KEY"))?;
        let raw = STANDARD.decode(body)?;
        anyhow::ensure!(raw.len() > 24, "sealed value too short");
        let (nonce, body) = raw.split_at(24);
        let nonce: [u8; 24] = nonce.try_into()?;
        let plain = cipher
            .decrypt(&BoxNonce::from(nonce), body)
            .map_err(|_| anyhow::anyhow!("can't decrypt (was PYO_SECRET_KEY changed?)"))?;
        Ok(String::from_utf8(plain)?)
    }

    pub fn enabled(&self) -> bool {
        self.secretbox.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let key = [7u8; 32];
        let s = Sealer {
            secretbox: Some(<XSalsa20Poly1305 as crypto_secretbox::aead::KeyInit>::new(
                &BoxKey::from(key),
            )),
        };
        let sealed = s.seal("tskey_secret").unwrap();
        assert!(sealed.starts_with(PREFIX));
        assert_eq!(s.open(&sealed).unwrap(), "tskey_secret");
        // A fresh nonce each time.
        assert_ne!(s.seal("x").unwrap(), s.seal("x").unwrap());
        assert!(s.open("bm90IHNlYWxlZA==").is_err(), "unsealed");
    }
}
