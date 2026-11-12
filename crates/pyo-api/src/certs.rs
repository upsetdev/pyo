//! Uploaded TLS certificates: checked on upload (the key matches, it covers
//! hostnames, it hasn't expired), stored with the key sealed, and handed to
//! agents with the deploy spec of services whose domains use them.

use rustls::sign::CertifiedKey;
use rustls_pki_types::{CertificateDer, PrivateKeyDer, pem::PemObject};
use sha2::{Digest, Sha256};
use x509_parser::{extensions::GeneralName, prelude::*};

use serde_json::{Value, json};
use sqlx::{Postgres, Transaction};

use crate::{
    AppState,
    error::{ApiError, ApiResult},
};

/// What an upload says about itself.
#[derive(Debug)]
pub struct Parsed {
    pub hostnames: Vec<String>,
    pub issuer: String,
    pub not_before: i64,
    pub not_after: i64,
    pub fingerprint: String,
    /// Normalized PEM, leaf first.
    pub chain_pem: String,
    pub key_pem: String,
    /// Fine to save, but worth saying.
    pub warnings: Vec<String>,
}

fn bad(msg: impl Into<String>) -> ApiError {
    ApiError::bad_request(msg.into())
}

/// Check a certificate chain and its private key, both PEM.
pub fn parse(chain_pem: &str, key_pem: &str, now_secs: i64) -> ApiResult<Parsed> {
    let chain: Vec<CertificateDer<'static>> = CertificateDer::pem_slice_iter(chain_pem.as_bytes())
        .collect::<Result<_, _>>()
        .map_err(|_| {
            bad("The certificate isn't valid PEM. Paste the -----BEGIN CERTIFICATE----- block(s).")
        })?;
    let Some(leaf) = chain.first().cloned() else {
        return Err(bad(
            "No certificate found. Paste the -----BEGIN CERTIFICATE----- block(s).",
        ));
    };
    let key = PrivateKeyDer::from_pem_slice(key_pem.as_bytes()).map_err(|_| bad("The private key isn't valid PEM. Paste the -----BEGIN PRIVATE KEY----- block (it must not be password-protected)."))?;
    let key_out = pem_of(
        match &key {
            PrivateKeyDer::Pkcs1(_) => "RSA PRIVATE KEY",
            PrivateKeyDer::Sec1(_) => "EC PRIVATE KEY",
            _ => "PRIVATE KEY",
        },
        key.secret_der(),
    );
    match CertifiedKey::from_der(
        chain.clone(),
        key,
        &rustls::crypto::aws_lc_rs::default_provider(),
    ) {
        Ok(_) => {}
        Err(rustls::Error::InconsistentKeys(_)) => {
            return Err(bad("The private key doesn't belong to this certificate."));
        }
        Err(_) => {
            return Err(bad(
                "This kind of private key isn't supported. Use RSA, ECDSA (P-256/P-384) or Ed25519.",
            ));
        }
    }

    let (_, cert) =
        X509Certificate::from_der(&leaf).map_err(|_| bad("The certificate couldn't be read."))?;
    let mut hostnames: Vec<String> = match cert.subject_alternative_name() {
        Ok(Some(san)) => san
            .value
            .general_names
            .iter()
            .filter_map(|g| {
                if let GeneralName::DNSName(n) = g {
                    Some(n.to_lowercase())
                } else {
                    None
                }
            })
            .collect(),
        _ => vec![],
    };
    if hostnames.is_empty() {
        hostnames = cert
            .subject()
            .iter_common_name()
            .filter_map(|cn| cn.as_str().ok())
            .map(str::to_lowercase)
            .collect();
    }
    if hostnames.is_empty() {
        return Err(bad("The certificate names no hostnames."));
    }
    let not_before = cert.validity().not_before.timestamp();
    let not_after = cert.validity().not_after.timestamp();
    if not_after <= now_secs {
        return Err(bad("This certificate has expired."));
    }
    let mut warnings = vec![];
    if not_before > now_secs {
        warnings.push("It isn't valid yet. Browsers reject it until its start date.".into());
    }
    let self_signed = cert.issuer() == cert.subject();
    if self_signed {
        warnings.push("It's self-signed, so browsers warn about it.".into());
    } else if chain.len() == 1 {
        warnings.push("No intermediate certificates are included. Some browsers and tools won't trust it. Paste the full chain from your provider.".into());
    }
    let issuer = cert
        .issuer()
        .iter_organization()
        .chain(cert.issuer().iter_common_name())
        .find_map(|a| a.as_str().ok().map(str::to_owned))
        .unwrap_or_else(|| cert.issuer().to_string());
    Ok(Parsed {
        hostnames,
        issuer,
        not_before,
        not_after,
        fingerprint: hex::encode(Sha256::digest(leaf.as_ref())),
        chain_pem: chain
            .iter()
            .map(|c| pem_of("CERTIFICATE", c.as_ref()))
            .collect::<Vec<_>>()
            .join(""),
        key_pem: key_out,
        warnings,
    })
}

fn pem_of(label: &str, der: &[u8]) -> String {
    use base64::Engine;
    let b64 = base64::engine::general_purpose::STANDARD.encode(der);
    let body: Vec<&str> = b64
        .as_bytes()
        .chunks(64)
        .map(|c| std::str::from_utf8(c).unwrap_or(""))
        .collect();
    format!(
        "-----BEGIN {label}-----\n{}\n-----END {label}-----\n",
        body.join("\n")
    )
}

/// Whether a certificate's names cover a hostname: exactly, or by a
/// wildcard for one label (`*.example.com` covers `shop.example.com`).
pub fn covers(names: &[String], host: &str) -> bool {
    let host = host.to_lowercase();
    names.iter().any(|n| match n.strip_prefix("*.") {
        Some(base) => host
            .split_once('.')
            .is_some_and(|(label, rest)| !label.is_empty() && rest == base),
        None => *n == host,
    })
}

/// Domains of a service's spec that use an uploaded certificate must name one
/// of the team's, valid for their hostname.
pub async fn domains_ok(
    tx: &mut Transaction<'_, Postgres>,
    team_id: &str,
    spec: &Value,
) -> ApiResult<()> {
    for d in spec["domains"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|d| d["tlsMode"] == json!("custom"))
    {
        let host = d["hostname"].as_str().unwrap_or("");
        let id = d["certificateId"].as_str().unwrap_or("");
        if id.is_empty() {
            return Err(bad(format!(
                "Pick a certificate for {host}, or upload one."
            )));
        }
        let names: Option<(String, Vec<String>)> = sqlx::query_as(
            "SELECT name, hostnames FROM pyo.certificates WHERE id = $1 AND team_id = $2",
        )
        .bind(id)
        .bind(team_id)
        .fetch_optional(&mut **tx)
        .await?;
        let Some((name, names)) = names else {
            return Err(bad(format!(
                "The certificate for {host} isn't one of this team's."
            )));
        };
        if !covers(&names, host) {
            return Err(bad(format!(
                "\"{name}\" isn't valid for {host} (it covers {}).",
                names.join(", ")
            )));
        }
    }
    Ok(())
}

/// Hostnames using a certificate, in any of the team's services.
pub async fn used_by(
    tx: &mut Transaction<'_, Postgres>,
    id: &str,
) -> ApiResult<Vec<(String, String)>> {
    Ok(sqlx::query_as(
        "SELECT a.id, d->>'hostname' FROM pyo.services a, jsonb_array_elements(a.spec->'domains') d
          WHERE d->>'tlsMode' = 'custom' AND d->>'certificateId' = $1 AND NOT a.deleting",
    )
    .bind(id)
    .fetch_all(&mut **tx)
    .await?)
}

/// Notifications 30 and 7 days before an uploaded certificate expires (and
/// once it has), once each.
pub async fn warn_expiring(s: &AppState) -> ApiResult<()> {
    warn_expiring_in(s, "certificates", "upload a new one").await?;
    // Past its date, a CDN using this one is locked out of every site.
    warn_expiring_in(
        s,
        "origin_pull_certificates",
        "create a new one and update your CDN",
    )
    .await
}

async fn warn_expiring_in(s: &AppState, table: &str, fix: &str) -> ApiResult<()> {
    // `table` is one of the two names above, never user input.
    let sql = format!(
        "SELECT id, team_id, name, (CASE WHEN not_after <= now() THEN 0 WHEN not_after < now() + interval '7 days' THEN 7 ELSE 30 END)::int,
                ceil(extract(epoch FROM not_after - now()) / 86400)::bigint
           FROM pyo.{table}
          WHERE not_after < now() + interval '30 days'
            AND coalesce(warned_days, 999) > (CASE WHEN not_after <= now() THEN 0 WHEN not_after < now() + interval '7 days' THEN 7 ELSE 30 END)"
    );
    let due: Vec<(String, String, String, i32, i64)> = sqlx::query_as(sqlx::AssertSqlSafe(sql))
        .fetch_all(&s.db)
        .await?;
    for (id, team, name, step, days) in due {
        let mut tx = s.db.begin().await?;
        let title = if step == 0 {
            format!("Certificate {name} has expired; {fix}")
        } else {
            match days.max(1) {
                1 => format!("Certificate {name} expires in 1 day"),
                d => format!("Certificate {name} expires in {d} days"),
            }
        };
        crate::ops::log_activity(
            &mut tx,
            &team,
            "Pyo",
            "domain.certificate_expiring",
            "warning",
            &title,
            json!({ "kind": "certificate", "id": id, "label": name }),
            json!({}),
        )
        .await?;
        let mark = format!("UPDATE pyo.{table} SET warned_days = $2 WHERE id = $1");
        sqlx::query(sqlx::AssertSqlSafe(mark))
            .bind(&id)
            .bind(step)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
    }
    Ok(())
}

/// The agent finished applying a cluster's origin pull setting. An answer to
/// an older request than the latest one changes nothing.
pub async fn origin_pull_finished(
    tx: &mut Transaction<'_, Postgres>,
    cluster_id: &str,
    command: &Value,
    ok: bool,
    message: Option<&str>,
) -> ApiResult<()> {
    sqlx::query(
        "UPDATE pyo.clusters
            SET origin_pull = origin_pull || jsonb_build_object('state', $3::text, 'error', $4::text)
                  || CASE WHEN $3 = 'active' THEN jsonb_build_object('enforcedIds', origin_pull->'certificateIds') ELSE '{}' END
          WHERE id = $1 AND origin_pull->>'revision' = $2",
    )
    .bind(cluster_id)
    .bind(command["revision"].as_str().unwrap_or(""))
    .bind(if ok { "active" } else { "failed" })
    .bind(if ok { None } else { Some(message.unwrap_or("The agent couldn't apply this.")) })
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// A client certificate for a CDN, with the private CA that signed it.
pub struct OriginPull {
    pub ca_pem: String,
    pub certificate_pem: String,
    pub private_key_pem: String,
    pub fingerprint: String,
    pub not_after: i64,
}

/// Generate an Authenticated Origin Pulls certificate. Only the CDN needs the
/// keys, so the CA's goes when this returns: nobody, Pyo included, can sign
/// another certificate that a server trusting this CA would accept.
pub fn generate_origin_pull(
    common_name: &str,
    years: i64,
    now_secs: i64,
) -> anyhow::Result<OriginPull> {
    use cookie::time::{Duration, OffsetDateTime};
    use rcgen::{
        BasicConstraints, CertificateParams, DistinguishedName, DnType, ExtendedKeyUsagePurpose,
        IsCa, Issuer, KeyPair, KeyUsagePurpose,
    };

    let not_after = now_secs + years * 365 * 86_400;
    let params = |cn: String| -> anyhow::Result<CertificateParams> {
        let mut p = CertificateParams::default();
        let mut dn = DistinguishedName::new();
        dn.push(DnType::CommonName, cn);
        p.distinguished_name = dn;
        // A day's grace for clocks that disagree.
        p.not_before = OffsetDateTime::from_unix_timestamp(now_secs)? - Duration::days(1);
        p.not_after = OffsetDateTime::from_unix_timestamp(not_after)?;
        Ok(p)
    };

    let mut ca = params(format!("{common_name} CA"))?;
    ca.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
    ca.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    let ca_key = KeyPair::generate()?;
    let ca_cert = ca.self_signed(&ca_key)?;

    let mut client = params(common_name.to_owned())?;
    client.is_ca = IsCa::ExplicitNoCa;
    client.key_usages = vec![KeyUsagePurpose::DigitalSignature];
    client.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
    client.use_authority_key_identifier_extension = true;
    let client_key = KeyPair::generate()?;
    let client_cert = client.signed_by(&client_key, &Issuer::new(ca, ca_key))?;

    Ok(OriginPull {
        fingerprint: hex::encode(Sha256::digest(ca_cert.der())),
        ca_pem: ca_cert.pem(),
        certificate_pem: client_cert.pem(),
        private_key_pem: client_key.serialize_pem(),
        not_after,
    })
}

#[cfg(test)]
mod tests {
    #[test]
    fn origin_pull_certificates_are_for_clients_and_chain_to_their_ca() {
        use x509_parser::pem::parse_x509_pem;
        let g = super::generate_origin_pull("cdn.acme.example", 1, 1_800_000_000).unwrap();
        assert!(g.private_key_pem.contains("PRIVATE KEY"));
        let (_, ca_pem) = parse_x509_pem(g.ca_pem.as_bytes()).unwrap();
        let (_, client_pem) = parse_x509_pem(g.certificate_pem.as_bytes()).unwrap();
        let ca = ca_pem.parse_x509().unwrap();
        let client = client_pem.parse_x509().unwrap();
        assert!(ca.is_ca());
        assert!(!client.is_ca());
        assert_eq!(client.issuer(), ca.subject());
        assert_eq!(
            client
                .subject()
                .iter_common_name()
                .next()
                .unwrap()
                .as_str()
                .unwrap(),
            "cdn.acme.example"
        );
        let eku = client.extended_key_usage().unwrap().unwrap().value;
        assert!(eku.client_auth && !eku.server_auth);
        assert_eq!(
            client.validity().not_after.timestamp(),
            1_800_000_000 + 365 * 86_400
        );
    }

    use super::*;

    #[test]
    fn wildcards() {
        let names = vec!["*.example.com".to_owned(), "example.com".to_owned()];
        assert!(covers(&names, "shop.example.com"));
        assert!(covers(&names, "Example.com"));
        assert!(!covers(&names, "a.b.example.com"));
        assert!(!covers(&names, "example.org"));
    }

    #[test]
    fn rejects_garbage() {
        assert!(parse("hello", "world", 0).is_err());
    }
}
