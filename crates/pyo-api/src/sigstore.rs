//! Checks a Sigstore bundle (`cosign sign-blob --bundle`, keyless) for a
//! file: what the release workflow makes for release.json, so the
//! dashboard's updates use only a release Pyo's own workflow signed.
//!
//! The trust is Sigstore's public instance, as of its trusted root
//! (assets/sigstore/): Fulcio's root and intermediate (valid to 2031) and
//! Rekor v1's key. The release workflow signs with Rekor v1
//! (.github/release-signing-config.json), whose signed entry timestamp
//! (SET) this reads; checking is offline.
//!
//! What has to hold, all of it:
//! - the signing certificate chains to Fulcio's root (ECDSA signatures),
//!   is for code signing, and names the expected identity (its SAN) and
//!   OIDC issuer (Fulcio's extension);
//! - Rekor's log entry is signed by Rekor (its SET over the entry's body,
//!   time, log id and index), and records this certificate, this signature
//!   and the file's SHA-256;
//! - the certificate was valid when Rekor logged it (Fulcio's are valid ten
//!   minutes);
//! - the signature is the certificate key's, over the file.

use anyhow::{Context, anyhow, bail, ensure};
use base64::Engine;
use rustls::pki_types::{AlgorithmIdentifier, alg_id};
use serde_json::Value;
use sha2::{Digest, Sha256};
use x509_parser::{certificate::X509Certificate, prelude::FromDer};

const FULCIO_INTERMEDIATE: &[u8] = include_bytes!("../assets/sigstore/fulcio-intermediate.der");
const FULCIO_ROOT: &[u8] = include_bytes!("../assets/sigstore/fulcio-root.der");
/// Rekor v1's public key (SPKI, ECDSA P-256).
const REKOR_KEY: &[u8] = include_bytes!("../assets/sigstore/rekor.der");

/// Who must have signed: the certificate's SAN (a URI or email) starting
/// with `identity_prefix`, issued for a token from `issuer`.
pub struct Signer<'a> {
    pub identity_prefix: &'a str,
    pub issuer: &'a str,
}

/// Pyo's release workflow, at a version tag.
pub const RELEASE_SIGNER: Signer<'static> = Signer {
    identity_prefix: "https://github.com/upsetdev/pyo/.github/workflows/release.yml@refs/tags/v",
    issuer: "https://token.actions.githubusercontent.com",
};

const OID_EC_P256: &str = "1.2.840.10045.3.1.7";
const OID_EC_P384: &str = "1.3.132.0.34";
const OID_ECDSA_SHA256: &str = "1.2.840.10045.4.3.2";
const OID_ECDSA_SHA384: &str = "1.2.840.10045.4.3.3";
/// Fulcio's OIDC issuer extensions: v1 (raw string) and v2 (DER UTF8String).
const OID_ISSUER_V1: &str = "1.3.6.1.4.1.57264.1.1";
const OID_ISSUER_V2: &str = "1.3.6.1.4.1.57264.1.8";

fn b64(s: &str) -> anyhow::Result<Vec<u8>> {
    Ok(base64::engine::general_purpose::STANDARD.decode(s.trim())?)
}

/// An ECDSA signature check by aws-lc (rustls's provider): `key` is the
/// SPKI's point, `curve` its curve's OID, `hash` the signature's.
fn ecdsa_ok(curve: &str, hash: &str, key: &[u8], message: &[u8], signature: &[u8]) -> bool {
    let want: (AlgorithmIdentifier, AlgorithmIdentifier) = match (curve, hash) {
        (OID_EC_P256, "sha256") => (alg_id::ECDSA_P256, alg_id::ECDSA_SHA256),
        (OID_EC_P256, "sha384") => (alg_id::ECDSA_P256, alg_id::ECDSA_SHA384),
        (OID_EC_P384, "sha256") => (alg_id::ECDSA_P384, alg_id::ECDSA_SHA256),
        (OID_EC_P384, "sha384") => (alg_id::ECDSA_P384, alg_id::ECDSA_SHA384),
        _ => return false,
    };
    rustls::crypto::aws_lc_rs::default_provider()
        .signature_verification_algorithms
        .all
        .iter()
        .find(|a| a.public_key_alg_id() == want.0 && a.signature_alg_id() == want.1)
        .is_some_and(|a| a.verify_signature(key, message, signature).is_ok())
}

/// A certificate's public key: (curve OID, point).
fn key_of<'a>(cert: &'a X509Certificate) -> anyhow::Result<(String, &'a [u8])> {
    let spki = cert.public_key();
    let curve = spki
        .algorithm
        .parameters
        .as_ref()
        .and_then(|p| p.as_oid().ok())
        .map(|o| o.to_id_string())
        .ok_or_else(|| anyhow!("not an EC key"))?;
    Ok((curve, &spki.subject_public_key.data))
}

/// `cert` is signed by `issuer`'s key.
fn signed_by(cert: &X509Certificate, issuer: &X509Certificate) -> bool {
    let hash = match cert.signature_algorithm.algorithm.to_id_string().as_str() {
        OID_ECDSA_SHA256 => "sha256",
        OID_ECDSA_SHA384 => "sha384",
        _ => return false,
    };
    let Ok((curve, key)) = key_of(issuer) else {
        return false;
    };
    cert.issuer() == issuer.subject()
        && ecdsa_ok(
            &curve,
            hash,
            key,
            cert.tbs_certificate.as_ref(),
            &cert.signature_value.data,
        )
}

/// Checks `bundle` (a Sigstore bundle, JSON) for `blob`, signed by `signer`.
pub fn verify(blob: &[u8], bundle: &str, signer: &Signer) -> anyhow::Result<()> {
    let b: Value = serde_json::from_str(bundle).context("the signature isn't a Sigstore bundle")?;
    let vm = &b["verificationMaterial"];
    let leaf_der = match vm["certificate"]["rawBytes"].as_str() {
        Some(raw) => b64(raw)?,
        // Bundle v0.1/v0.2: a chain, leaf first.
        None => b64(vm["x509CertificateChain"]["certificates"][0]["rawBytes"]
            .as_str()
            .ok_or_else(|| anyhow!("the bundle has no certificate"))?)?,
    };
    let signature = b64(b["messageSignature"]["signature"]
        .as_str()
        .ok_or_else(|| anyhow!("the bundle has no signature"))?)?;
    let digest = Sha256::digest(blob);

    // The certificate: Fulcio's, for this identity.
    let (_, leaf) = X509Certificate::from_der(&leaf_der)
        .map_err(|e| anyhow!("the signing certificate doesn't parse: {e}"))?;
    let (_, inter) = X509Certificate::from_der(FULCIO_INTERMEDIATE).map_err(|e| anyhow!("{e}"))?;
    let (_, root) = X509Certificate::from_der(FULCIO_ROOT).map_err(|e| anyhow!("{e}"))?;
    ensure!(
        signed_by(&leaf, &inter) && signed_by(&inter, &root) && signed_by(&root, &root),
        "the signing certificate isn't Sigstore's (Fulcio)"
    );
    ensure!(
        leaf.extended_key_usage()
            .ok()
            .flatten()
            .is_some_and(|e| e.value.code_signing),
        "the signing certificate isn't for code signing"
    );
    let names: Vec<String> = leaf
        .subject_alternative_name()
        .ok()
        .flatten()
        .map(|san| {
            san.value
                .general_names
                .iter()
                .filter_map(|n| match n {
                    x509_parser::extensions::GeneralName::URI(u) => Some((*u).to_owned()),
                    x509_parser::extensions::GeneralName::RFC822Name(e) => Some((*e).to_owned()),
                    _ => None,
                })
                .collect()
        })
        .unwrap_or_default();
    ensure!(
        names.iter().any(|n| n.starts_with(signer.identity_prefix)),
        "signed by {}, not {}…",
        names.join(", "),
        signer.identity_prefix
    );
    let issuer = leaf
        .extensions()
        .iter()
        .find_map(|e| match e.oid.to_id_string().as_str() {
            OID_ISSUER_V1 => std::str::from_utf8(e.value).ok().map(str::to_owned),
            // DER UTF8String: tag 0x0c, a short length.
            OID_ISSUER_V2 => e
                .value
                .get(2..)
                .filter(|_| {
                    e.value.first() == Some(&0x0c)
                        && e.value
                            .get(1)
                            .is_some_and(|l| usize::from(*l) == e.value.len() - 2)
                })
                .and_then(|v| std::str::from_utf8(v).ok())
                .map(str::to_owned),
            _ => None,
        });
    ensure!(
        issuer.as_deref() == Some(signer.issuer),
        "signed with a token from {}, not {}",
        issuer.unwrap_or_default(),
        signer.issuer
    );

    // Rekor's entry: signed by Rekor, and about this signature.
    let entry = vm["tlogEntries"]
        .as_array()
        .and_then(|e| e.first())
        .ok_or_else(|| anyhow!("the signature isn't in Sigstore's transparency log"))?;
    let num = |v: &Value| {
        v.as_str()
            .and_then(|s| s.parse::<i64>().ok())
            .or_else(|| v.as_i64())
    };
    let integrated =
        num(&entry["integratedTime"]).ok_or_else(|| anyhow!("the log entry has no time"))?;
    let index = num(&entry["logIndex"]).ok_or_else(|| anyhow!("the log entry has no index"))?;
    let log_id = hex::encode(b64(entry["logId"]["keyId"].as_str().unwrap_or(""))?);
    let body = entry["canonicalizedBody"]
        .as_str()
        .ok_or_else(|| anyhow!("the log entry has no body"))?;
    let set = b64(entry["inclusionPromise"]["signedEntryTimestamp"]
        .as_str()
        .ok_or_else(|| anyhow!("the log entry isn't signed by Rekor (no SET)"))?)?;
    ensure!(
        log_id == hex::encode(Sha256::digest(REKOR_KEY)),
        "logged in another transparency log than Rekor's"
    );
    // The SET: Rekor's signature over this canonical JSON (keys sorted).
    let payload = format!(
        "{{\"body\":{},\"integratedTime\":{integrated},\"logID\":{},\"logIndex\":{index}}}",
        Value::from(body.trim()),
        Value::from(log_id.as_str())
    );
    let (_, rekor) =
        x509_parser::x509::SubjectPublicKeyInfo::from_der(REKOR_KEY).map_err(|e| anyhow!("{e}"))?;
    let rekor_curve = rekor
        .algorithm
        .parameters
        .as_ref()
        .and_then(|p| p.as_oid().ok())
        .map(|o| o.to_id_string())
        .unwrap_or_default();
    ensure!(
        ecdsa_ok(
            &rekor_curve,
            "sha256",
            &rekor.subject_public_key.data,
            payload.as_bytes(),
            &set
        ),
        "Rekor's signature over the log entry doesn't check out"
    );
    let rec: Value = serde_json::from_slice(&b64(body)?).context("the log entry's body")?;
    ensure!(
        rec["kind"] == "hashedrekord",
        "the log entry isn't for a signed file"
    );
    ensure!(
        rec["spec"]["data"]["hash"]["algorithm"] == "sha256"
            && rec["spec"]["data"]["hash"]["value"].as_str() == Some(hex::encode(digest).as_str()),
        "the log entry is for another file"
    );
    ensure!(
        b64(rec["spec"]["signature"]["content"].as_str().unwrap_or(""))? == signature,
        "the log entry is for another signature"
    );
    let logged_pem = String::from_utf8(b64(rec["spec"]["signature"]["publicKey"]["content"]
        .as_str()
        .unwrap_or(""))?)?;
    let logged_der = b64(&logged_pem
        .lines()
        .filter(|l| !l.starts_with("-----"))
        .collect::<String>())?;
    ensure!(
        logged_der == leaf_der,
        "the log entry is for another certificate"
    );
    let (from, to) = (
        leaf.validity().not_before.timestamp(),
        leaf.validity().not_after.timestamp(),
    );
    ensure!(
        (from..=to).contains(&integrated),
        "the certificate wasn't valid when the signature was logged"
    );

    // The signature itself, over the file.
    if let Some(d) = b["messageSignature"]["messageDigest"]["digest"].as_str() {
        ensure!(
            b64(d)? == digest.as_slice(),
            "the bundle is for another file"
        );
    }
    let (curve, key) = key_of(&leaf)?;
    if !ecdsa_ok(&curve, "sha256", key, blob, &signature) {
        bail!("the signature doesn't match the file");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const BLOB: &[u8] = include_bytes!("../assets/sigstore/testdata/cosign_checksums.txt");
    const BUNDLE: &str =
        include_str!("../assets/sigstore/testdata/cosign_checksums.txt.sigstore.json");
    /// cosign's own release (v3.1.4), signed keyless by its release account.
    const COSIGN: Signer<'static> = Signer {
        identity_prefix: "keyless@projectsigstore.iam.gserviceaccount.com",
        issuer: "https://accounts.google.com",
    };

    #[test]
    fn a_real_bundle_checks_out() {
        verify(BLOB, BUNDLE, &COSIGN).unwrap();
    }

    #[test]
    fn anything_else_doesnt() {
        let mut other = BLOB.to_vec();
        other[10] ^= 1;
        assert!(verify(&other, BUNDLE, &COSIGN).is_err(), "another file");
        assert!(
            format!("{:#}", verify(BLOB, BUNDLE, &RELEASE_SIGNER).unwrap_err())
                .contains("not https://github.com/upsetdev/pyo/"),
            "another signer"
        );
        assert!(
            verify(
                BLOB,
                BUNDLE,
                &Signer {
                    issuer: "https://token.actions.githubusercontent.com",
                    ..COSIGN
                }
            )
            .is_err(),
            "another issuer"
        );
        // Rekor's promise, changed.
        let mut b: Value = serde_json::from_str(BUNDLE).unwrap();
        let t = &mut b["verificationMaterial"]["tlogEntries"][0];
        t["integratedTime"] = Value::from(
            (t["integratedTime"]
                .as_str()
                .unwrap()
                .parse::<i64>()
                .unwrap()
                + 1)
            .to_string(),
        );
        assert!(
            verify(BLOB, &b.to_string(), &COSIGN).is_err(),
            "a log entry Rekor didn't sign"
        );
        // Without the log entry's promise.
        let mut b: Value = serde_json::from_str(BUNDLE).unwrap();
        b["verificationMaterial"]["tlogEntries"][0]["inclusionPromise"] = Value::Null;
        assert!(verify(BLOB, &b.to_string(), &COSIGN).is_err());
        // Another signature (a valid base64 one, but not this file's).
        let mut b: Value = serde_json::from_str(BUNDLE).unwrap();
        let sig = b["messageSignature"]["signature"]
            .as_str()
            .unwrap()
            .to_owned();
        let mut raw = b64(&sig).unwrap();
        let n = raw.len() - 3;
        raw[n] ^= 1;
        b["messageSignature"]["signature"] =
            Value::from(base64::engine::general_purpose::STANDARD.encode(raw));
        assert!(verify(BLOB, &b.to_string(), &COSIGN).is_err());
        assert!(verify(BLOB, "{}", &COSIGN).is_err());
        assert!(verify(BLOB, "not json", &COSIGN).is_err());
    }
}
