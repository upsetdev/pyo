//! The agent CA's certificates (pyo-api's mTLS agent endpoint): what they
//! hold, whatever holds the CA's key. Pyo's own CA keeps a key file;
//! Pyo Cloud's signer keeps the key in a KMS. Both issue through here, so
//! an agent's certificate looks the same either way.
//!
//! - The CA: "Pyo agent CA", may sign certificates but not other CAs, ten
//!   years.
//! - An agent's client certificate: its cluster as a URI name
//!   (`pyo://team/<team>/cluster/<cluster>`), client authentication only,
//!   [`CERT_HOURS`] long, a random serial.
//! - The endpoint's server certificate: its DNS names or IPs, server
//!   authentication only, a year.
//!
//! The key behind an [`Issuer`] is any [`SigningKey`]: rcgen's own, or one
//! that asks a KMS.

pub use rcgen::{Issuer, PublicKeyData, SigningKey, SubjectPublicKeyInfo};
use rcgen::{
    CertificateParams, CertificateSigningRequestParams, DistinguishedName, DnType, ExtendedKeyUsagePurpose, IsCa,
    KeyUsagePurpose, SanType, SerialNumber,
};

/// A client certificate's lifetime.
pub const CERT_HOURS: i64 = 24;

/// A certificate the CA issued.
#[derive(Debug, Clone)]
pub struct Issued {
    /// Hex, without leading zero bytes (as agent_tls reads it back).
    pub serial: String,
    pub pem: String,
    /// When it expires, ms since the epoch.
    pub not_after_ms: i64,
}

fn err(e: impl std::fmt::Display) -> String {
    e.to_string()
}

/// The CA's own certificate (PEM), self-signed by its key.
pub fn ca_cert(key: &impl SigningKey) -> Result<String, String> {
    let mut p = CertificateParams::new(Vec::<String>::new()).map_err(err)?;
    let mut dn = DistinguishedName::new();
    dn.push(DnType::CommonName, "Pyo agent CA");
    p.distinguished_name = dn;
    p.is_ca = IsCa::Ca(rcgen::BasicConstraints::Constrained(0));
    p.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    let now = time::OffsetDateTime::now_utc();
    p.not_before = now - time::Duration::hours(1);
    p.not_after = now + time::Duration::days(3650);
    Ok(p.self_signed(key).map_err(err)?.pem())
}

/// The issuer for the CA certificate `ca_pem`, signing with `key`.
pub fn issuer<S: SigningKey>(ca_pem: &str, key: S) -> Result<Issuer<'static, S>, String> {
    Issuer::from_ca_cert_pem(ca_pem, key).map_err(err)
}

/// The public key (SubjectPublicKeyInfo, DER) of a certificate (PEM).
pub fn cert_public_key(pem: &str) -> Result<Vec<u8>, String> {
    let der = cert_der(pem)?;
    let (_, cert) = x509_parser::parse_x509_certificate(&der).map_err(err)?;
    Ok(cert.public_key().raw.to_vec())
}

/// A certificate's DER from its PEM.
pub fn cert_der(pem: &str) -> Result<Vec<u8>, String> {
    let (_, p) = x509_parser::pem::parse_x509_pem(pem.as_bytes()).map_err(err)?;
    if p.label != "CERTIFICATE" {
        return Err("not a certificate".into());
    }
    Ok(p.contents)
}

/// A team or cluster id as certificates carry them.
fn plain_id(s: &str) -> bool {
    (1..=64).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

fn random_serial() -> Result<Vec<u8>, String> {
    let mut serial = [0u8; 16];
    ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut serial).map_err(|_| "no randomness".to_owned())?;
    // Positive, as X.509 wants.
    serial[0] &= 0x7f;
    Ok(serial.to_vec())
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// A client certificate for a cluster's agent, from its CSR (the agent's
/// own key; the CSR's own names are ignored).
pub fn issue_client(issuer: &Issuer<'_, impl SigningKey>, csr_pem: &str, team: &str, cluster: &str) -> Result<Issued, String> {
    if !plain_id(team) || !plain_id(cluster) {
        return Err("a team and cluster id are letters, digits, _ and -".into());
    }
    let mut csr = CertificateSigningRequestParams::from_pem(csr_pem).map_err(err)?;
    let mut p = CertificateParams::new(Vec::<String>::new()).map_err(err)?;
    p.subject_alt_names = vec![SanType::URI(format!("pyo://team/{team}/cluster/{cluster}").try_into().map_err(err)?)];
    let mut dn = DistinguishedName::new();
    dn.push(DnType::CommonName, cluster);
    p.distinguished_name = dn;
    let now = time::OffsetDateTime::now_utc();
    p.not_before = now - time::Duration::minutes(5);
    let not_after = now + time::Duration::hours(CERT_HOURS);
    p.not_after = not_after;
    let serial = random_serial()?;
    let serial_hex = hex(&serial).trim_start_matches("00").to_owned();
    p.serial_number = Some(SerialNumber::from(serial));
    p.key_usages = vec![KeyUsagePurpose::DigitalSignature];
    p.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
    csr.params = p;
    let cert = csr.signed_by(issuer).map_err(err)?;
    Ok(Issued { serial: serial_hex, pem: cert.pem(), not_after_ms: (not_after.unix_timestamp_nanos() / 1_000_000) as i64 })
}

/// The agent endpoint's own certificate for `names` (DNS names or IPs),
/// from its CSR.
pub fn issue_server(issuer: &Issuer<'_, impl SigningKey>, csr_pem: &str, names: &[String]) -> Result<String, String> {
    if names.is_empty() {
        return Err("no names".into());
    }
    let mut csr = CertificateSigningRequestParams::from_pem(csr_pem).map_err(err)?;
    let mut p = CertificateParams::new(Vec::<String>::new()).map_err(err)?;
    for n in names {
        p.subject_alt_names.push(match n.parse::<std::net::IpAddr>() {
            Ok(ip) => SanType::IpAddress(ip),
            Err(_) => SanType::DnsName(n.as_str().try_into().map_err(err)?),
        });
    }
    let mut dn = DistinguishedName::new();
    dn.push(DnType::CommonName, names[0].as_str());
    p.distinguished_name = dn;
    let now = time::OffsetDateTime::now_utc();
    p.not_before = now - time::Duration::hours(1);
    p.not_after = now + time::Duration::days(365);
    p.serial_number = Some(SerialNumber::from(random_serial()?));
    p.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
    csr.params = p;
    Ok(csr.signed_by(issuer).map_err(err)?.pem())
}

/// A CSR (PEM) for `key`, for [`issue_client`] or [`issue_server`].
pub fn csr(key: &rcgen::KeyPair) -> Result<String, String> {
    CertificateParams::new(Vec::<String>::new()).map_err(err)?.serialize_request(key).map_err(err)?.pem().map_err(err)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn certificates_chain_to_the_ca_and_name_their_cluster() {
        let ca_key = rcgen::KeyPair::generate().unwrap();
        let ca = ca_cert(&ca_key).unwrap();
        assert_eq!(cert_public_key(&ca).unwrap(), ca_key.subject_public_key_info());
        let iss = issuer(&ca, &ca_key).unwrap();
        let agent = rcgen::KeyPair::generate().unwrap();
        let c = issue_client(&iss, &csr(&agent).unwrap(), "team_a", "cl_1").unwrap();
        let der = cert_der(&c.pem).unwrap();
        let (_, x) = x509_parser::parse_x509_certificate(&der).unwrap();
        assert_eq!(hex(x.raw_serial()).trim_start_matches("00"), c.serial);
        assert!(format!("{:?}", x.subject_alternative_name()).contains("pyo://team/team_a/cluster/cl_1"));
        assert_eq!(x.public_key().raw, agent.subject_public_key_info());
        // Ids that would bend the URI are refused.
        assert!(issue_client(&iss, &csr(&agent).unwrap(), "team_a", "cl_1/cluster/cl_2").is_err());
        assert!(issue_server(&iss, &csr(&agent).unwrap(), &["agent.pyo.dev".into(), "10.0.0.1".into()]).is_ok());
        assert!(issue_server(&iss, &csr(&agent).unwrap(), &[]).is_err());
    }
}
