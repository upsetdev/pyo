//! Where the images of Pyo's built-in sandbox templates ([`super::base`])
//! come from: upsetdev/pyo-sandbox-images. Every build there publishes
//! `base` and `desktop` (Ubuntu) and a release whose images.json names
//! each by digest, with the image contract they're made for, signed by
//! that repo's release workflow on main.
//!
//! - Pyo reads the latest release's images.json when a worker starts and
//!   every hour after. It uses it only when the signature checks out
//!   (sigstore.rs) and its contract is one Pyo runs
//!   ([`pyo_proto::sandbox::CONTRACTS`]). It keeps it in pyo.settings, so
//!   every replica takes the same one.
//! - A newer build there gives `base` and `desktop` a new revision. It
//!   never goes back to an older build (by its date).
//! - Pyo ships the images.json of when it was made
//!   (assets/sandbox-images/), so a fresh install has images before it
//!   reaches GitHub, and one that never does still has some.
//! - `PYO_UPDATE_CHECK=off` stops the reading too, as it stops every check
//!   of GitHub. Pyo then uses the images.json it shipped with.
//! - `PYO_SANDBOX_BASE_REF` and `PYO_SANDBOX_DESKTOP_REF` override both,
//!   for dev builds and pinning ([`super::base::from_env`]).

use std::collections::BTreeMap;
use std::sync::LazyLock;
use std::time::Duration;

use pyo_proto::pinned_image;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::PgPool;

use crate::AppState;

/// Where the latest release's files are.
pub const LATEST: &str = "https://github.com/upsetdev/pyo-sandbox-images/releases/latest/download";
/// How often Pyo looks for a newer build.
const EVERY: Duration = Duration::from_secs(3600);
/// Its key in pyo.settings.
const KEY: &str = "sandbox_images";
/// images.json is small, and so is its signature.
const MAX: usize = 1 << 20;

/// What Pyo shipped with: the images.json of when it was made, and its
/// signature (a test checks it).
const SHIPPED_JSON: &[u8] = include_bytes!("../../assets/sandbox-images/images.json");

/// One image in images.json.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Image {
    pub repository: String,
    pub digest: String,
    /// `repository@digest`.
    #[serde(rename = "ref")]
    pub reference: String,
    /// Each tag's digest when it was built.
    #[serde(default)]
    pub tags: BTreeMap<String, String>,
}

/// images.json.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Images {
    /// The image contract they're made for.
    pub contract: u32,
    /// Ubuntu's version, like "24.04".
    pub ubuntu: String,
    /// The day it was built, YYYYMMDD.
    pub date: String,
    /// The commit it was built from.
    pub commit: String,
    pub images: BTreeMap<String, Image>,
}

impl Images {
    /// Reads and checks images.json. Its signature is [`verified`]'s.
    pub fn parse(json: &[u8]) -> anyhow::Result<Images> {
        let i: Images = serde_json::from_slice(json)
            .map_err(|e| anyhow::anyhow!("images.json can't be read: {e}"))?;
        i.check()?;
        Ok(i)
    }

    fn check(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            pyo_proto::sandbox::CONTRACTS.contains(&self.contract),
            "images.json is for image contract {}, which this Pyo doesn't run. A newer Pyo does.",
            self.contract
        );
        anyhow::ensure!(
            self.date.len() == 8 && self.date.bytes().all(|b| b.is_ascii_digit()),
            "images.json has no build date"
        );
        for b in super::base::ALL {
            let image = self
                .images
                .get(b.name)
                .ok_or_else(|| anyhow::anyhow!("images.json has no {} image", b.name))?;
            anyhow::ensure!(
                pinned_image(&image.reference)
                    && image.reference == format!("{}@{}", image.repository, image.digest),
                "images.json's {} image isn't pinned by digest: {}",
                b.name,
                image.reference
            );
        }
        Ok(())
    }

    /// The image of template `name`, pinned by digest.
    pub fn image(&self, name: &str) -> Option<&str> {
        self.images.get(name).map(|i| i.reference.as_str())
    }

    /// What the images run, like "Ubuntu 24.04".
    pub fn os(&self) -> String {
        format!("Ubuntu {}", self.ubuntu)
    }
}

/// The images.json Pyo shipped with. None only if it's broken, which a
/// test rules out.
pub fn shipped() -> Option<&'static Images> {
    static SHIPPED: LazyLock<Option<Images>> = LazyLock::new(|| {
        match Images::parse(SHIPPED_JSON) {
            Ok(i) => Some(i),
            Err(e) => {
                tracing::error!(error = %format!("{e:#}"), "the sandbox images Pyo shipped with can't be used");
                None
            }
        }
    });
    SHIPPED.as_ref()
}

/// images.json, once its signature checks out: signed by
/// pyo-sandbox-images' release workflow on main.
pub fn verified(json: &[u8], bundle: &str) -> anyhow::Result<Images> {
    crate::sigstore::verify(json, bundle, &crate::sigstore::SANDBOX_IMAGES_SIGNER).map_err(
        |e| {
            anyhow::anyhow!(
                "images.json isn't signed by pyo-sandbox-images' release workflow: {e:#}"
            )
        },
    )?;
    Images::parse(json)
}

/// What Pyo keeps of the last read.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Stored {
    /// The newest images.json read so far that checked out.
    pub images: Option<Images>,
    pub checked_at: Option<i64>,
    /// Why the last read failed.
    pub error: Option<String>,
}

impl Stored {
    /// Takes in a read: a newer images.json replaces the one kept, an older
    /// one doesn't, and a failed read keeps it and says why. True when the
    /// images changed.
    pub fn take(&mut self, read: anyhow::Result<Images>, now_ms: i64) -> bool {
        self.checked_at = Some(now_ms);
        match read {
            Ok(new) => {
                self.error = None;
                let newer = match &self.images {
                    None => true,
                    Some(have) => new.date >= have.date && &new != have,
                };
                if newer {
                    self.images = Some(new);
                }
                newer
            }
            Err(e) => {
                self.error = Some(format!("{e:#}"));
                false
            }
        }
    }
}

/// The newer of two by build date, `a` when they're as new.
pub fn newest<'a>(a: Option<&'a Images>, b: Option<&'a Images>) -> Option<&'a Images> {
    match (a, b) {
        (Some(a), Some(b)) if b.date > a.date => Some(b),
        (Some(a), _) => Some(a),
        (None, b) => b,
    }
}

pub async fn stored(db: &PgPool) -> sqlx::Result<Stored> {
    let v: Option<Value> = sqlx::query_scalar("SELECT value FROM pyo.settings WHERE key = $1")
        .bind(KEY)
        .fetch_optional(db)
        .await?;
    Ok(v.and_then(|v| serde_json::from_value::<Stored>(v).ok())
        .map(|mut s| {
            // Kept by a Pyo that ran another contract: not for this one.
            if s.images.as_ref().is_some_and(|i| i.check().is_err()) {
                s.images = None;
            }
            s
        })
        .unwrap_or_default())
}

/// The images Pyo uses now: the newest of the last one read and the one it
/// shipped with.
pub async fn current(db: &PgPool) -> Option<Images> {
    let kept = stored(db).await.ok().and_then(|s| s.images);
    newest(kept.as_ref(), shipped()).cloned()
}

/// The latest release's images.json from GitHub, signature checked.
pub async fn fetch() -> anyhow::Result<Images> {
    let http = crate::updates::client()?;
    let get = |name: &'static str| {
        let http = http.clone();
        async move {
            let res = http.get(format!("{LATEST}/{name}")).send().await?;
            anyhow::ensure!(
                res.status().is_success(),
                "GitHub answered {} for {name}",
                res.status()
            );
            crate::updates::body_limited(res, MAX).await
        }
    };
    let json = get("images.json").await?;
    let bundle = get("images.json.sigstore.json").await?;
    verified(&json, &String::from_utf8_lossy(&bundle))
}

/// Reads the latest images.json and keeps it when it's newer. True when
/// the images changed. Nothing is read with `PYO_UPDATE_CHECK=off`.
pub async fn refresh(db: &PgPool) -> anyhow::Result<bool> {
    if crate::updates::checks_off_by_env() {
        return Ok(false);
    }
    let read = fetch().await;
    if let Err(e) = &read {
        tracing::warn!(error = %format!("{e:#}"), "sandbox images: couldn't read the latest images.json, keeping the ones Pyo has");
    }
    let mut s = stored(db).await?;
    let changed = s.take(read, pyo_proto::now_ms());
    if changed && let Some(i) = &s.images {
        tracing::info!(date = %i.date, commit = %i.commit, "sandbox images: a newer build");
    }
    sqlx::query(
        "INSERT INTO pyo.settings (key, value) VALUES ($1, $2) ON CONFLICT (key) DO UPDATE SET value = excluded.value",
    )
    .bind(KEY)
    .bind(serde_json::to_value(&s)?)
    .execute(db)
    .await?;
    Ok(changed)
}

/// On a worker: reads images.json every hour, and moves the built-in
/// templates when there's a newer build. [`super::base::seed`] reads it
/// once at start.
pub async fn runner(s: AppState) {
    loop {
        tokio::time::sleep(EVERY).await;
        if s.stopping() {
            return;
        }
        match refresh(&s.db).await {
            Ok(true) => {
                if let Err(e) = super::base::ensure_all(&s).await {
                    tracing::warn!(error = %format!("{e:#}"), "sandbox templates: couldn't move Pyo's to the newer images");
                }
            }
            Ok(false) => {}
            Err(e) => {
                tracing::warn!(error = %format!("{e:#}"), "sandbox images: couldn't keep the latest")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BUNDLE: &str = include_str!("../../assets/sandbox-images/images.json.sigstore.json");

    fn with(date: &str, digest: char) -> Images {
        let mut i = Images::parse(SHIPPED_JSON).unwrap();
        i.date = date.into();
        for img in i.images.values_mut() {
            img.digest = format!("sha256:{}", digest.to_string().repeat(64));
            img.reference = format!("{}@{}", img.repository, img.digest);
        }
        i
    }

    #[test]
    fn what_pyo_shipped_with_is_signed_and_usable() {
        let i = verified(SHIPPED_JSON, BUNDLE).unwrap();
        assert_eq!(Some(&i), shipped());
        assert_eq!(i.contract, 1);
        assert!(i.os().starts_with("Ubuntu "), "{}", i.os());
        for b in super::super::base::ALL {
            let image = i.image(b.name).unwrap();
            assert!(pinned_image(image), "{image}");
            assert!(
                image.starts_with(&format!("ghcr.io/upsetdev/pyo-sandbox-images/{}@", b.name)),
                "{image}"
            );
        }
    }

    #[test]
    fn a_signature_that_doesnt_check_out_is_refused() {
        // Another file under the same signature.
        let mut other = SHIPPED_JSON.to_vec();
        let at = other.iter().position(|&b| b == b'7').unwrap();
        other[at] = b'8';
        let e = format!("{:#}", verified(&other, BUNDLE).unwrap_err());
        assert!(e.contains("isn't signed by pyo-sandbox-images"), "{e}");
        // No signature, or not a bundle.
        assert!(verified(SHIPPED_JSON, "").is_err());
        assert!(verified(SHIPPED_JSON, "{}").is_err());
        // Signed by Pyo's own release workflow instead: not this one.
        assert!(
            crate::sigstore::verify(SHIPPED_JSON, BUNDLE, &crate::sigstore::RELEASE_SIGNER)
                .is_err()
        );
    }

    #[test]
    fn only_contracts_pyo_runs() {
        let mut v: Value = serde_json::from_slice(SHIPPED_JSON).unwrap();
        v["contract"] = 2.into();
        let e = format!("{:#}", Images::parse(v.to_string().as_bytes()).unwrap_err());
        assert_eq!(
            e,
            "images.json is for image contract 2, which this Pyo doesn't run. A newer Pyo does."
        );
        assert!(!e.contains(';'));
    }

    #[test]
    fn every_image_pinned() {
        let mut v: Value = serde_json::from_slice(SHIPPED_JSON).unwrap();
        v["images"]["desktop"]["ref"] = "ghcr.io/upsetdev/pyo-sandbox-images/desktop:24.04".into();
        let e = format!("{:#}", Images::parse(v.to_string().as_bytes()).unwrap_err());
        assert!(e.contains("desktop image isn't pinned"), "{e}");
        let mut v: Value = serde_json::from_slice(SHIPPED_JSON).unwrap();
        v["images"].as_object_mut().unwrap().remove("base");
        let e = format!("{:#}", Images::parse(v.to_string().as_bytes()).unwrap_err());
        assert_eq!(e, "images.json has no base image");
        let mut v: Value = serde_json::from_slice(SHIPPED_JSON).unwrap();
        v["date"] = "2026-11-16".into();
        assert!(Images::parse(v.to_string().as_bytes()).is_err());
        assert!(Images::parse(b"<html>").is_err());
    }

    #[test]
    fn a_read_moves_forward_only() {
        let mut s = Stored::default();
        let (a, b) = (with("20261116", 'a'), with("20261123", 'b'));
        assert!(s.take(Ok(a.clone()), 1), "the first");
        assert!(!s.take(Ok(a.clone()), 2), "the same again");
        assert!(s.take(Ok(b.clone()), 3), "a newer build");
        assert!(!s.take(Ok(a.clone()), 4), "never back to an older one");
        assert_eq!(s.images.as_ref(), Some(&b));
        // A rebuild the same day replaces it.
        let b2 = with("20261123", 'c');
        assert!(s.take(Ok(b2.clone()), 5));
        // A failed read keeps what it had, and says why.
        assert!(!s.take(Err(anyhow::anyhow!("GitHub answered 503")), 6));
        assert_eq!(s.images.as_ref(), Some(&b2));
        assert_eq!(s.error.as_deref(), Some("GitHub answered 503"));
        assert_eq!(s.checked_at, Some(6));
        assert!(!s.take(Ok(b2.clone()), 7));
        assert_eq!(s.error, None, "cleared by the next read");
        // It survives a round trip through pyo.settings.
        let back: Stored = serde_json::from_value(serde_json::to_value(&s).unwrap()).unwrap();
        assert_eq!(back, s);
    }

    #[test]
    fn the_newest_wins() {
        let (old, new) = (with("20261116", 'a'), with("20261201", 'b'));
        assert_eq!(newest(Some(&old), Some(&new)), Some(&new));
        assert_eq!(newest(Some(&new), Some(&old)), Some(&new));
        // As new: the one read wins over the one shipped.
        let same_day = with("20261116", 'c');
        assert_eq!(newest(Some(&same_day), Some(&old)), Some(&same_day));
        assert_eq!(newest(None, Some(&old)), Some(&old));
        assert_eq!(newest(Some(&old), None), Some(&old));
        assert_eq!(newest(None, None), None);
    }

    /// The real thing, from GitHub.
    #[tokio::test]
    #[ignore = "reads GitHub"]
    async fn reads_the_latest_images_json() {
        let i = fetch().await.unwrap();
        assert!(i.date >= shipped().unwrap().date, "{i:?}");
    }
}
