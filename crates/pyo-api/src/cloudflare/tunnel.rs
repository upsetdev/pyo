//! Cloudflare Tunnel: what Pyo asks of Cloudflare for a cluster's tunnel
//! (docs/cloudflare-tunnel.md §6). The tunnel itself, its routes (remotely
//! managed ingress), its token for the cluster's connector, the Origin CA
//! certificate Pyo Gateway shows cloudflared, and the token checks that say
//! which zones can use a tunnel.
//!
//! Everything goes through [`Http`]'s pinned client to `api.cloudflare.com`
//! only, with the team's token. The token never leaves the control plane:
//! the cluster only ever gets the tunnel's own token.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use super::{CfError, Http, Record, id_ok};
use reqwest::Method;

/// The permission a tunnel needs in the account.
pub const TUNNEL_EDIT: &str = "Cloudflare Tunnel:Edit";
/// The permission an Origin CA certificate needs in the zone.
pub const CERTS_EDIT: &str = "SSL and Certificates:Edit";

/// What a probe tunnel is named: removed right after the check, and any
/// left behind by a check that stopped halfway is removed by the next.
pub const CHECK_PREFIX: &str = "pyo-check-";

/// How long the Origin CA certificate is valid: 15 years, Cloudflare's
/// longest (decision 2). Never renewed by age.
pub const CERT_DAYS: u32 = 5475;

/// The most names one Origin CA certificate takes.
pub const MAX_CERT_NAMES: usize = 200;

/// Where cloudflared sends requests: Pyo's own Service in front of the
/// Gateway's HTTPS listener (`system::TUNNEL_ORIGIN_SERVICE`).
pub const ORIGIN: &str = "https://pyo-tunnel-origin.pyo-gateway.svc.cluster.local:443";

/// A zone as Cloudflare lists it, with the account it's in.
#[derive(Debug, Clone, PartialEq)]
pub struct CfZone {
    pub id: String,
    pub name: String,
    pub account_id: String,
    pub account_name: String,
}

/// A tunnel at Cloudflare.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Tunnel {
    pub id: String,
    pub name: String,
    /// "inactive", "degraded", "healthy" or "down".
    pub status: String,
    /// Deleted at Cloudflare (it stays listed for a while).
    pub deleted: bool,
    pub connections: Vec<Connection>,
}

/// One of a tunnel's connections to Cloudflare's edge.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Connection {
    /// The connector it belongs to (one cloudflared runs four).
    pub client_id: String,
    pub colo: String,
    pub client_version: String,
    pub opened_at: String,
    pub origin_ip: String,
}

/// A record as it was, kept to be put back (`dns_records.previous`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Kept {
    #[serde(default, skip_serializing)]
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub name: String,
    pub content: String,
    #[serde(default)]
    pub proxied: bool,
    #[serde(default = "auto_ttl")]
    pub ttl: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub comment: Option<String>,
}

fn auto_ttl() -> u32 {
    1
}

impl Kept {
    pub fn record(&self) -> Record {
        Record {
            id: self.id.clone(),
            kind: self.kind.clone(),
            name: self.name.clone(),
            content: self.content.clone(),
            proxied: self.proxied,
            comment: self.comment.clone(),
        }
    }
}

/// An Origin CA certificate Cloudflare issued.
#[derive(Debug, Clone, PartialEq)]
pub struct OriginCert {
    pub id: String,
    pub pem: String,
}

/// What Pyo asks of Cloudflare for tunnels and their certificates, on top
/// of DNS ([`super::Cloudflare`]). [`Http`] is the real one; the tests use
/// a fake account.
pub(crate) trait TunnelApi {
    /// Every zone the token can read, with its account.
    async fn zone_accounts(&self) -> Result<Vec<CfZone>, CfError>;
    /// The records with exactly this name, with what's needed to put them
    /// back as they were.
    async fn records_kept(&self, zone_id: &str, name: &str) -> Result<Vec<Kept>, CfError>;
    /// Create a record as it was.
    async fn create_kept(&self, zone_id: &str, r: &Kept) -> Result<Record, CfError>;
    /// The account's tunnels named `name` that aren't deleted.
    async fn tunnels_named(&self, account: &str, name: &str) -> Result<Vec<Tunnel>, CfError>;
    /// A remotely managed tunnel (its routes are set through the API).
    async fn create_tunnel(&self, account: &str, name: &str) -> Result<Tunnel, CfError>;
    async fn tunnel(&self, account: &str, id: &str) -> Result<Tunnel, CfError>;
    async fn delete_tunnel(&self, account: &str, id: &str) -> Result<(), CfError>;
    /// The token a connector runs the tunnel with.
    async fn tunnel_token(&self, account: &str, id: &str) -> Result<String, CfError>;
    /// Replace the tunnel's routes. Cloudflare's version of them comes back.
    async fn put_config(&self, account: &str, id: &str, config: &Value) -> Result<i64, CfError>;
    /// Cloudflare's version of the tunnel's routes, and the routes.
    async fn config(&self, account: &str, id: &str) -> Result<(i64, Value), CfError>;
    /// Give the tunnel a new secret (so a new token).
    async fn rotate_secret(&self, account: &str, id: &str, secret: &str) -> Result<(), CfError>;
    /// Drop the tunnel's connections (they come back with a current token).
    async fn drop_connections(&self, account: &str, id: &str) -> Result<(), CfError>;
    /// The zone's Origin CA certificates (proves the token may read them).
    async fn origin_certs(&self, zone_id: &str) -> Result<Vec<String>, CfError>;
    /// Issue an Origin CA certificate for a CSR.
    async fn issue_origin_cert(
        &self,
        csr_pem: &str,
        hostnames: &[String],
    ) -> Result<OriginCert, CfError>;
    async fn revoke_origin_cert(&self, id: &str) -> Result<(), CfError>;
}

/// Tunnel ids are UUIDs.
pub fn uuid_ok(id: &str) -> bool {
    id.len() == 36
        && id.bytes().enumerate().all(|(i, b)| match i {
            8 | 13 | 18 | 23 => b == b'-',
            _ => b.is_ascii_hexdigit(),
        })
}

/// Origin CA certificate ids: digits (a long serial), nothing else.
fn cert_id_ok(id: &str) -> bool {
    !id.is_empty() && id.len() <= 80 && id.bytes().all(|b| b.is_ascii_digit())
}

fn tunnel_path(account: &str, id: Option<&str>, rest: &str) -> Result<String, CfError> {
    if !id_ok(account) || id.is_some_and(|i| !uuid_ok(i)) {
        return Err(CfError::NotFound);
    }
    Ok(match id {
        Some(i) => format!("/accounts/{account}/cfd_tunnel/{i}{rest}"),
        None => format!("/accounts/{account}/cfd_tunnel{rest}"),
    })
}

fn unreadable() -> CfError {
    CfError::Refused("an answer Pyo can't read".into())
}

/// A tunnel as Cloudflare answers with it. Its connections are listed per
/// connection (`connections`, each with its connector's `client_id`).
pub fn tunnel_from(v: &Value) -> Result<Tunnel, CfError> {
    let id = v["id"].as_str().filter(|i| uuid_ok(i)).ok_or_else(unreadable)?;
    let s = |v: &Value, k: &str| v[k].as_str().unwrap_or("").to_owned();
    Ok(Tunnel {
        id: id.to_owned(),
        name: s(v, "name"),
        status: s(v, "status"),
        deleted: v["deleted_at"].as_str().is_some_and(|d| !d.is_empty()),
        connections: v["connections"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|c| Connection {
                client_id: s(c, "client_id"),
                colo: s(c, "colo_name"),
                client_version: s(c, "client_version"),
                opened_at: s(c, "opened_at"),
                origin_ip: s(c, "origin_ip"),
            })
            .collect(),
    })
}

fn kept_from(v: &Value) -> Result<Kept, CfError> {
    serde_json::from_value::<Kept>(v.clone())
        .map(|mut k| {
            k.id = v["id"].as_str().unwrap_or("").to_owned();
            k
        })
        .map_err(|_| unreadable())
}

impl TunnelApi for Http {
    async fn zone_accounts(&self) -> Result<Vec<CfZone>, CfError> {
        let mut out = vec![];
        for page in 1..=20 {
            let mut url = self.url("/zones");
            url.query_pairs_mut()
                .append_pair("per_page", "50")
                .append_pair("page", &page.to_string());
            let v = self.call(Method::GET, url, None).await?;
            for z in v["result"].as_array().into_iter().flatten() {
                if let (Some(id), Some(name)) = (z["id"].as_str(), z["name"].as_str())
                    && id_ok(id)
                {
                    let account = z["account"]["id"].as_str().filter(|a| id_ok(a));
                    out.push(CfZone {
                        id: id.to_owned(),
                        name: name.to_owned(),
                        account_id: account.unwrap_or("").to_owned(),
                        account_name: z["account"]["name"]
                            .as_str()
                            .unwrap_or("")
                            .chars()
                            .take(100)
                            .collect(),
                    });
                }
            }
            if page >= v["result_info"]["total_pages"].as_i64().unwrap_or(1) {
                break;
            }
        }
        Ok(out)
    }

    async fn records_kept(&self, zone_id: &str, name: &str) -> Result<Vec<Kept>, CfError> {
        let mut url = self.url(&Self::record_path(zone_id, None)?);
        url.query_pairs_mut()
            .append_pair("name", name)
            .append_pair("per_page", "100");
        let v = self.call(Method::GET, url, None).await?;
        v["result"]
            .as_array()
            .into_iter()
            .flatten()
            .map(kept_from)
            .collect()
    }

    async fn create_kept(&self, zone_id: &str, r: &Kept) -> Result<Record, CfError> {
        let url = self.url(&Self::record_path(zone_id, None)?);
        let mut body = json!({
            "type": r.kind, "name": r.name, "content": r.content,
            "ttl": r.ttl, "proxied": r.proxied,
        });
        if let Some(c) = &r.comment {
            body["comment"] = json!(c);
        }
        super::record_from(&self.call(Method::POST, url, Some(body)).await?["result"])
    }

    async fn tunnels_named(&self, account: &str, name: &str) -> Result<Vec<Tunnel>, CfError> {
        let mut url = self.url(&tunnel_path(account, None, "")?);
        url.query_pairs_mut()
            .append_pair("name", name)
            .append_pair("is_deleted", "false")
            .append_pair("per_page", "50");
        let v = self.call(Method::GET, url, None).await?;
        v["result"]
            .as_array()
            .into_iter()
            .flatten()
            .map(tunnel_from)
            .collect()
    }

    async fn create_tunnel(&self, account: &str, name: &str) -> Result<Tunnel, CfError> {
        let url = self.url(&tunnel_path(account, None, "")?);
        let body = json!({ "name": name, "config_src": "cloudflare" });
        tunnel_from(&self.call(Method::POST, url, Some(body)).await?["result"])
    }

    async fn tunnel(&self, account: &str, id: &str) -> Result<Tunnel, CfError> {
        let url = self.url(&tunnel_path(account, Some(id), "")?);
        tunnel_from(&self.call(Method::GET, url, None).await?["result"])
    }

    async fn delete_tunnel(&self, account: &str, id: &str) -> Result<(), CfError> {
        let url = self.url(&tunnel_path(account, Some(id), "")?);
        self.call(Method::DELETE, url, None).await.map(|_| ())
    }

    async fn tunnel_token(&self, account: &str, id: &str) -> Result<String, CfError> {
        let url = self.url(&tunnel_path(account, Some(id), "/token")?);
        let v = self.call(Method::GET, url, None).await?;
        v["result"]
            .as_str()
            .filter(|t| !t.is_empty() && t.len() <= 4096 && t.bytes().all(|b| b.is_ascii_graphic()))
            .map(str::to_owned)
            .ok_or_else(unreadable)
    }

    async fn put_config(&self, account: &str, id: &str, config: &Value) -> Result<i64, CfError> {
        let url = self.url(&tunnel_path(account, Some(id), "/configurations")?);
        let v = self
            .call(Method::PUT, url, Some(json!({ "config": config })))
            .await?;
        Ok(v["result"]["version"].as_i64().unwrap_or(0))
    }

    async fn config(&self, account: &str, id: &str) -> Result<(i64, Value), CfError> {
        let url = self.url(&tunnel_path(account, Some(id), "/configurations")?);
        let v = self.call(Method::GET, url, None).await?;
        Ok((
            v["result"]["version"].as_i64().unwrap_or(0),
            v["result"]["config"].clone(),
        ))
    }

    async fn rotate_secret(&self, account: &str, id: &str, secret: &str) -> Result<(), CfError> {
        let url = self.url(&tunnel_path(account, Some(id), "")?);
        self.call(Method::PATCH, url, Some(json!({ "tunnel_secret": secret })))
            .await
            .map(|_| ())
    }

    async fn drop_connections(&self, account: &str, id: &str) -> Result<(), CfError> {
        let url = self.url(&tunnel_path(account, Some(id), "/connections")?);
        self.call(Method::DELETE, url, None).await.map(|_| ())
    }

    async fn origin_certs(&self, zone_id: &str) -> Result<Vec<String>, CfError> {
        if !id_ok(zone_id) {
            return Err(CfError::NotFound);
        }
        let mut url = self.url("/certificates");
        url.query_pairs_mut().append_pair("zone_id", zone_id);
        let v = self.call(Method::GET, url, None).await?;
        Ok(v["result"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|c| c["id"].as_str().map(str::to_owned))
            .collect())
    }

    async fn issue_origin_cert(
        &self,
        csr_pem: &str,
        hostnames: &[String],
    ) -> Result<OriginCert, CfError> {
        let url = self.url("/certificates");
        let body = json!({
            "csr": csr_pem, "hostnames": hostnames,
            "request_type": "origin-ecc", "requested_validity": CERT_DAYS,
        });
        let v = self.call(Method::POST, url, Some(body)).await?;
        let r = &v["result"];
        match (r["id"].as_str(), r["certificate"].as_str()) {
            (Some(id), Some(pem)) if cert_id_ok(id) => Ok(OriginCert {
                id: id.to_owned(),
                pem: pem.to_owned(),
            }),
            _ => Err(unreadable()),
        }
    }

    async fn revoke_origin_cert(&self, id: &str) -> Result<(), CfError> {
        if !cert_id_ok(id) {
            return Err(CfError::NotFound);
        }
        let url = self.url(&format!("/certificates/{id}"));
        self.call(Method::DELETE, url, None).await.map(|_| ())
    }
}

// ── Checking a token ───────────────────────────────────────────────────────

/// Whether the token may make tunnels in an account: it makes a probe
/// tunnel and deletes it again (a token without Cloudflare Tunnel:Edit is
/// refused the first step). Probes left behind are deleted first. The
/// permission it lacks, or None.
pub(crate) async fn tunnel_missing<C: TunnelApi>(
    cf: &C,
    account: &str,
) -> Result<Option<String>, CfError> {
    let left = match cf.tunnels_named(account, CHECK_PREFIX).await {
        Ok(t) => t,
        Err(CfError::Denied(_)) => return Ok(Some(TUNNEL_EDIT.into())),
        Err(e) => return Err(e),
    };
    for t in left.iter().filter(|t| t.name.starts_with(CHECK_PREFIX)) {
        let _ = cf.delete_tunnel(account, &t.id).await;
    }
    let mut b = [0u8; 3];
    rand::fill(&mut b);
    match cf
        .create_tunnel(account, &format!("{CHECK_PREFIX}{}", hex::encode(b)))
        .await
    {
        Ok(t) => {
            cf.delete_tunnel(account, &t.id).await?;
            Ok(None)
        }
        Err(CfError::Denied(_)) => Ok(Some(TUNNEL_EDIT.into())),
        Err(e) => Err(e),
    }
}

/// Whether the token may read a zone's Origin CA certificates. Edit can't
/// be proven without issuing one, so the first issue says plainly when it's
/// missing. The permission it lacks, or None.
pub(crate) async fn certs_missing<C: TunnelApi>(
    cf: &C,
    zone_id: &str,
) -> Result<Option<String>, CfError> {
    match cf.origin_certs(zone_id).await {
        Ok(_) => Ok(None),
        Err(CfError::Denied(_)) => Ok(Some(CERTS_EDIT.into())),
        Err(e) => Err(e),
    }
}

// ── The routes ─────────────────────────────────────────────────────────────

/// One route rule for a hostname, to Pyo Gateway over HTTPS. The visitor's
/// host goes as SNI, so the Origin CA certificate is checked against it
/// (one shape for exact names and the wildcard).
fn rule(hostname: &str) -> Value {
    json!({
        "hostname": hostname,
        "service": ORIGIN,
        "originRequest": { "matchSNItoHost": true, "connectTimeout": 10, "keepAliveTimeout": 90 },
    })
}

/// The tunnel's routes: each hostname, exact names first (longest first,
/// then by name), the default domain's wildcard last, then the catch-all.
/// cloudflared matches top down.
pub fn ingress(hostnames: &[String], default_domain: Option<&str>) -> Value {
    let mut exact: Vec<String> = hostnames
        .iter()
        .map(|h| h.trim().trim_end_matches('.').to_ascii_lowercase())
        .filter(|h| !h.is_empty())
        .collect();
    exact.sort_by(|a, b| b.len().cmp(&a.len()).then_with(|| a.cmp(b)));
    exact.dedup();
    let wildcard = default_domain.map(|d| format!("*.{}", d.to_ascii_lowercase()));
    exact.retain(|h| Some(h) != wildcard.as_ref());
    let mut rules: Vec<Value> = exact.iter().map(|h| rule(h)).collect();
    if let Some(w) = &wildcard {
        rules.push(rule(w));
    }
    rules.push(json!({ "service": "http_status:404" }));
    json!({ "ingress": rules })
}

/// Only the catch-all: what's left while a tunnel is removed.
pub fn empty_ingress() -> Value {
    ingress(&[], None)
}

/// The routes' hash, to PUT them only when they change. serde_json keeps
/// keys sorted, so the same routes always hash the same.
pub fn config_hash(config: &Value) -> String {
    hex::encode(Sha256::digest(config.to_string().as_bytes()))
}

// ── The certificate's names ────────────────────────────────────────────────

/// The name that covers a tunnel hostname on the certificate: the zone's
/// apex itself, else a wildcard one level up (`blog.example.com` →
/// `*.example.com`, `a.b.example.com` → `*.b.example.com`).
pub fn cert_name(host: &str, zone: &str) -> String {
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    let zone = zone.trim_end_matches('.').to_ascii_lowercase();
    if host == zone {
        return host;
    }
    match host.split_once('.') {
        Some((_, parent)) => format!("*.{parent}"),
        None => host,
    }
}

/// Every name the tunnel's certificate needs: one per hostname (each with
/// its zone), the default domain's wildcard. Sorted, deduped. Too many for
/// one certificate: Err.
pub fn cert_names(
    hosts: &[(String, String)],
    default_domain: Option<&str>,
) -> Result<Vec<String>, String> {
    let mut names: Vec<String> = hosts.iter().map(|(h, z)| cert_name(h, z)).collect();
    if let Some(d) = default_domain {
        names.push(format!("*.{}", d.to_ascii_lowercase()));
    }
    names.sort();
    names.dedup();
    if names.len() > MAX_CERT_NAMES {
        return Err("This tunnel has too many names for one certificate.".into());
    }
    Ok(names)
}

/// Whether a certificate with `have` names covers all of `want`.
pub fn covers_all(have: &[String], want: &[String]) -> bool {
    want.iter().all(|w| have.contains(w))
}

/// A new key and its CSR for the tunnel's certificate: ECDSA P-256. The
/// key stays on the control plane, sealed with the team's certificates
/// (decision 1). (CSR PEM, key PEM).
pub fn key_and_csr(names: &[String]) -> anyhow::Result<(String, String)> {
    use rcgen::{CertificateParams, DistinguishedName, DnType, KeyPair, PKCS_ECDSA_P256_SHA256};
    let key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256)?;
    let mut p = CertificateParams::new(names.to_vec())?;
    let mut dn = DistinguishedName::new();
    dn.push(
        DnType::CommonName,
        names.first().cloned().unwrap_or_default(),
    );
    p.distinguished_name = dn;
    let csr = p.serialize_request(&key)?;
    Ok((csr.pem()?, key.serialize_pem()))
}

/// A new tunnel secret, as Cloudflare takes it: 32 random bytes, base64.
pub fn new_secret() -> String {
    use base64::Engine;
    let mut b = [0u8; 32];
    rand::fill(&mut b);
    base64::engine::general_purpose::STANDARD.encode(b)
}

/// The tunnel's name at Cloudflare: the cluster and this installation, so
/// two installations sharing an account never take each other's.
pub fn tunnel_name(cluster_id: &str, install_id: &str, attempt: u32) -> String {
    match attempt {
        0 | 1 => format!("pyo-{cluster_id}-{install_id}"),
        n => format!("pyo-{cluster_id}-{install_id}-{n}"),
    }
}

/// Where a tunnel's CNAME points.
pub fn cname_target(tunnel_id: &str) -> String {
    format!("{tunnel_id}.cfargotunnel.com")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routes_go_exact_names_first_then_the_wildcard_then_404() {
        let hosts = vec![
            "blog.example.com".to_owned(),
            "a.b.example.com".to_owned(),
            "API.example.com.".to_owned(),
            "blog.example.com".to_owned(),
            "*.example.com".to_owned(),
        ];
        let c = ingress(&hosts, Some("example.com"));
        let names: Vec<Value> = c["ingress"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["hostname"].clone())
            .collect();
        assert_eq!(
            names,
            [
                json!("blog.example.com"),
                json!("a.b.example.com"),
                json!("api.example.com"),
                json!("*.example.com"),
                Value::Null
            ]
        );
        let first = &c["ingress"][0];
        assert_eq!(first["service"], ORIGIN);
        assert_eq!(first["originRequest"]["matchSNItoHost"], true);
        assert_eq!(c["ingress"][4], json!({ "service": "http_status:404" }));
        // The same routes always hash the same, whatever order they came in.
        let mut again = hosts.clone();
        again.reverse();
        assert_eq!(
            config_hash(&ingress(&again, Some("example.com"))),
            config_hash(&c)
        );
        assert_ne!(
            config_hash(&ingress(&hosts[..1], Some("example.com"))),
            config_hash(&c)
        );
        assert_eq!(
            empty_ingress(),
            json!({ "ingress": [{ "service": "http_status:404" }] })
        );
    }

    #[test]
    fn certificate_names_cover_each_host_one_level_up() {
        assert_eq!(cert_name("example.com", "example.com"), "example.com");
        assert_eq!(cert_name("blog.example.com", "example.com"), "*.example.com");
        assert_eq!(cert_name("a.b.example.com", "example.com"), "*.b.example.com");
        assert_eq!(cert_name("Shop.Example.com.", "example.com"), "*.example.com");
        let hosts = vec![
            ("blog.example.com".to_owned(), "example.com".to_owned()),
            ("shop.example.com".to_owned(), "example.com".to_owned()),
            ("other.org".to_owned(), "other.org".to_owned()),
            ("x.y.other.org".to_owned(), "other.org".to_owned()),
        ];
        assert_eq!(
            cert_names(&hosts, Some("example.com")).unwrap(),
            ["*.example.com", "*.y.other.org", "other.org"]
        );
        let many: Vec<(String, String)> = (0..=MAX_CERT_NAMES)
            .map(|i| (format!("a.s{i}.example.com"), "example.com".to_owned()))
            .collect();
        assert_eq!(
            cert_names(&many, None).unwrap_err(),
            "This tunnel has too many names for one certificate."
        );
        let have = vec!["*.example.com".to_owned(), "other.org".to_owned()];
        assert!(covers_all(&have, &["*.example.com".to_owned()]));
        assert!(!covers_all(&have, &["*.b.example.com".to_owned()]));
    }

    #[test]
    fn a_csr_names_every_host_and_the_key_stays_here() {
        let names = vec!["*.example.com".to_owned(), "example.com".to_owned()];
        let (csr, key) = key_and_csr(&names).unwrap();
        assert!(csr.starts_with("-----BEGIN CERTIFICATE REQUEST-----"));
        assert!(key.contains("PRIVATE KEY"));
        assert!(!csr.contains("PRIVATE KEY"));
    }

    #[test]
    fn names_secrets_and_targets() {
        assert_eq!(tunnel_name("cl_1", "abc123", 0), "pyo-cl_1-abc123");
        assert_eq!(tunnel_name("cl_1", "abc123", 2), "pyo-cl_1-abc123-2");
        assert_eq!(
            cname_target("c1744f8b-faa1-48a4-9e5c-02ac921467fa"),
            "c1744f8b-faa1-48a4-9e5c-02ac921467fa.cfargotunnel.com"
        );
        use base64::Engine;
        let s = new_secret();
        assert_eq!(
            base64::engine::general_purpose::STANDARD
                .decode(&s)
                .unwrap()
                .len(),
            32
        );
        assert_ne!(new_secret(), s);
        assert!(cert_id_ok("123456789012345678901234567890"));
        assert!(!cert_id_ok("12/34"));
        assert!(!cert_id_ok(""));
    }

    #[test]
    fn kept_records_round_trip_without_their_id() {
        let k = kept_from(&json!({
            "id": "372e67954025e0ba6aaa6d586b9e0b59", "type": "A", "name": "blog.example.com",
            "content": "203.0.113.7", "proxied": true, "ttl": 300, "comment": "by hand",
        }))
        .unwrap();
        assert_eq!(k.id, "372e67954025e0ba6aaa6d586b9e0b59");
        assert_eq!(k.ttl, 300);
        let stored = serde_json::to_value(&k).unwrap();
        assert!(stored.get("id").is_none());
        assert_eq!(stored["type"], "A");
        let back: Kept = serde_json::from_value(stored).unwrap();
        assert_eq!(back.ttl, 300);
        assert_eq!(back.comment.as_deref(), Some("by hand"));
        let auto: Kept = serde_json::from_value(
            json!({ "type": "CNAME", "name": "a.example.com", "content": "x.example.net" }),
        )
        .unwrap();
        assert_eq!((auto.ttl, auto.proxied), (1, false));
    }
}
