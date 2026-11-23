//! A release's signed release.json: who checks its signature, and the part
//! of it the verifier reads.

use crate::{sys::Problem, version::Version};

/// What a good signature says besides "yes".
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Signed {
    /// When Sigstore's transparency log took the signature (seconds since
    /// 1970), signed by the log itself.
    pub logged_at: i64,
}

/// Checks that `manifest` is the release.json Pyo's release workflow signed
/// at the tag of exactly `version`.
pub trait ReleaseVerifier {
    fn verify(&self, manifest: &[u8], bundle: &[u8], version: &Version) -> Result<Signed, Problem>;
}

/// The only verifier the binary has: Sigstore's public instance, as built
/// into pyo-sigstore. It has no fields, and nothing (no flag, no setting, no
/// environment variable) can give the binary another trust root. Tests that
/// need releases nobody signed use a verifier that exists only in test
/// builds (run.rs).
pub struct Sigstore;

const _: () = assert!(std::mem::size_of::<Sigstore>() == 0);

impl ReleaseVerifier for Sigstore {
    fn verify(&self, manifest: &[u8], bundle: &[u8], version: &Version) -> Result<Signed, Problem> {
        let refused = |why: String| {
            Problem::with(
                format!(
                    "Release {version} is not signed by Pyo's release workflow. Nothing was changed."
                ),
                why,
            )
        };
        let bundle = std::str::from_utf8(bundle).map_err(|e| refused(e.to_string()))?;
        let signer = pyo_sigstore::release_signer_for(&version.to_string());
        pyo_sigstore::verify_logged(manifest, bundle, &signer.signer())
            .map(|v| Signed {
                logged_at: v.integrated_time,
            })
            .map_err(|e| refused(format!("{e:#}")))
    }
}

/// What the verifier reads from a release.json. Everything else in it is
/// for other readers.
#[derive(Debug, PartialEq)]
pub struct Manifest {
    pub version: String,
    /// The SHA-256 of this release's verifier, for this server's processor.
    /// None: the release has no verifier.
    pub verifier_sha256: Option<String>,
}

/// The name of the verifier for a processor, in `verifier` and as the
/// release's file `pyo-verify-<name>`.
pub fn platform(arch: &str) -> String {
    format!("linux-{arch}")
}

pub fn parse_manifest(bytes: &[u8], arch: &str) -> Result<Manifest, Problem> {
    let unreadable = || Problem::new("The signed release could not be read. Nothing was changed.");
    let v: serde_json::Value = serde_json::from_slice(bytes).map_err(|_| unreadable())?;
    let o = v.as_object().ok_or_else(unreadable)?;
    let version = o
        .get("version")
        .and_then(|v| v.as_str())
        .ok_or_else(unreadable)?
        .to_owned();
    let verifier_sha256 = match o.get("verifier") {
        None | Some(serde_json::Value::Null) => None,
        Some(v) => {
            let map = v.as_object().ok_or_else(unreadable)?;
            match map.get(&platform(arch)) {
                None => {
                    return Err(Problem::new(format!(
                        "Release {version} has no verifier for this server's processor. Run the connect command from Pyo to update this server."
                    )));
                }
                Some(sum) => {
                    let sum = sum.as_str().ok_or_else(unreadable)?;
                    let hex = sum.len() == 64
                        && sum
                            .bytes()
                            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
                    if !hex {
                        return Err(unreadable());
                    }
                    Some(sum.to_owned())
                }
            }
        }
    };
    Ok(Manifest {
        version,
        verifier_sha256,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_manifest_is_read_strictly() {
        let sum = "ab".repeat(32);
        let m = parse_manifest(
            format!(r#"{{"version":"0.52.0","verifier":{{"linux-x86_64":"{sum}","linux-aarch64":"{sum}"}},"more":1}}"#).as_bytes(),
            "x86_64",
        )
        .unwrap();
        assert_eq!(m.version, "0.52.0");
        assert_eq!(m.verifier_sha256.as_deref(), Some(sum.as_str()));
        // No verifier in the release.
        let m = parse_manifest(br#"{"version":"0.5.4"}"#, "x86_64").unwrap();
        assert_eq!(m.verifier_sha256, None);
        // Not for this processor.
        let e = parse_manifest(
            format!(r#"{{"version":"0.52.0","verifier":{{"linux-x86_64":"{sum}"}}}}"#).as_bytes(),
            "aarch64",
        )
        .unwrap_err();
        assert!(e.plain.contains("this server's processor"), "{}", e.plain);
        for bad in [
            "".to_owned(),
            "[]".to_owned(),
            r#"{"version":51}"#.to_owned(),
            r#"{"verifier":{}}"#.to_owned(),
            r#"{"version":"0.52.0","verifier":"x"}"#.to_owned(),
            r#"{"version":"0.52.0","verifier":{"linux-x86_64":"abc"}}"#.to_owned(),
            r#"{"version":"0.52.0","verifier":{"linux-x86_64":7}}"#.to_owned(),
            format!(
                r#"{{"version":"0.52.0","verifier":{{"linux-x86_64":"{}"}}}}"#,
                "AB".repeat(32)
            ),
            format!(r#"{{"version":"0.52.0","verifier":{{"linux-x86_64":"{sum}"}}}} trailing"#),
        ] {
            assert!(parse_manifest(bad.as_bytes(), "x86_64").is_err(), "{bad}");
        }
    }
}
