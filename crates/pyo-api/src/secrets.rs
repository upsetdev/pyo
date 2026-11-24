//! Sealing the installation's own credentials at rest (GitHub App key,
//! SMTP password, Tailscale keys, Pyo's backup store, sandbox signing
//! keys). XSalsa20-Poly1305 (NaCl secretbox) with PYO_SECRET_KEY (64 hex
//! chars); each value gets its own random nonce. Stored as `sb1:` +
//! base64(nonce || ciphertext). Teams' credentials are sealed with each
//! team's own data key instead (vault.rs `seal_credential`). Releases
//! before 0.5 sealed some of them here, and they still open.

use base64::{Engine, engine::general_purpose::STANDARD};
use crypto_secretbox::{Key as BoxKey, Nonce as BoxNonce, XSalsa20Poly1305};

pub(crate) const PREFIX: &str = "sb1:";

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

    /// A sealer with a fixed key, for tests.
    #[cfg(test)]
    pub fn for_tests() -> Self {
        Self {
            secretbox: Some(<XSalsa20Poly1305 as crypto_secretbox::aead::KeyInit>::new(
                &BoxKey::from([7u8; 32]),
            )),
        }
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

    /// The installation's key seals only the installation's own
    /// credentials. A team's (a token, password or key it gave Pyo) is
    /// sealed with the team's data key: vault.rs `seal_credential`. A new
    /// use here gets a place in this list with its reason.
    #[test]
    fn the_installation_key_seals_only_the_installations_credentials() {
        let allowed: &[(&str, usize, &str)] = &[
            ("github.rs", 1, "the installation's GitHub App"),
            (
                "sandbox/keys.rs",
                1,
                "the installation's sandbox signing keys",
            ),
            (
                "sandbox/edgecert.rs",
                2,
                "the installation's ACME account and its own sandbox servers' certificates",
            ),
            (
                "sandbox/outbound/signer.rs",
                1,
                "the key a self-hosted Pyo signs its sandbox servers' commands with",
            ),
            (
                "sandbox/outbound/channel.rs",
                1,
                "commands to the installation's own sandbox servers, which belong to no team",
            ),
            (
                "cloud/bootstrap.rs",
                1,
                "a new cloud server's one-time join material, burned when used or after 15 minutes",
            ),
            (
                "twofactor.rs",
                1,
                "people's authenticator app secrets: they belong to accounts, not to a team",
            ),
            (
                "ops.rs",
                3,
                "Pyo's own backup bucket, its SMTP password, and an invite's link in the email retry job",
            ),
        ];
        let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut found: Vec<(String, usize)> = vec![];
        let mut dirs = vec![src.clone()];
        while let Some(d) = dirs.pop() {
            for e in std::fs::read_dir(&d).unwrap() {
                let path = e.unwrap().path();
                if path.is_dir() {
                    dirs.push(path);
                    continue;
                }
                let text = std::fs::read_to_string(&path).unwrap_or_default();
                // Tests seal what they like.
                let text = text.split("#[cfg(test)]").next().unwrap_or("");
                // `s.sealer\n.seal(` too.
                let squashed: String = text.chars().filter(|c| !c.is_whitespace()).collect();
                let n = squashed.matches("sealer.seal(").count();
                let name = path
                    .strip_prefix(&src)
                    .unwrap()
                    .to_string_lossy()
                    .into_owned();
                if n > 0 && name != "secrets.rs" {
                    found.push((name, n));
                }
            }
        }
        found.sort();
        let mut want: Vec<(String, usize)> = allowed
            .iter()
            .map(|(f, n, _)| (f.to_string(), *n))
            .collect();
        want.sort();
        assert_eq!(
            found, want,
            "a team's credential goes through vault.rs seal_credential"
        );
    }
}
