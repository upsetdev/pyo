//! The ACME flow against a mock ACME server on 127.0.0.1, which checks the
//! DNS-01 record in a fake Cloudflare account before it signs, and the
//! wildcard record against the same fake. Nothing here reaches Let's
//! Encrypt or Cloudflare.

use std::sync::{Arc, Mutex};

use axum::{
    Json, Router,
    extract::{Path, State},
    http::{HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD as B64};
use sha2::{Digest, Sha256};

use super::*;

const MARK: &str = "Made by Pyo (abc123)";
const ZONE: &str = "z1";

// ── A Cloudflare account ───────────────────────────────────────────────────

#[derive(Default)]
struct Cf {
    records: Mutex<Vec<Record>>,
    next: Mutex<u32>,
    /// Every create and delete, in order.
    calls: Mutex<Vec<String>>,
}

impl Cf {
    fn with(self, kind: &str, name: &str, content: &str, comment: Option<&str>) -> Self {
        let id = self.id();
        self.records.lock().unwrap().push(Record {
            id,
            kind: kind.into(),
            name: name.into(),
            content: content.into(),
            proxied: false,
            comment: comment.map(str::to_owned),
        });
        self
    }

    fn id(&self) -> String {
        let mut n = self.next.lock().unwrap();
        *n += 1;
        format!("r{n}")
    }

    fn named(&self, name: &str) -> Vec<Record> {
        self.records
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r.name == name)
            .cloned()
            .collect()
    }

    fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }
}

impl Cloudflare for Arc<Cf> {
    async fn zones(&self) -> Result<Vec<(String, String)>, CfError> {
        Ok(vec![(ZONE.into(), "example.com".into())])
    }
    async fn records(&self, zone_id: &str, name: &str) -> Result<Vec<Record>, CfError> {
        assert_eq!(zone_id, ZONE);
        Ok(self.named(name))
    }
    async fn record(&self, _: &str, id: &str) -> Result<Record, CfError> {
        self.records
            .lock()
            .unwrap()
            .iter()
            .find(|r| r.id == id)
            .cloned()
            .ok_or(CfError::NotFound)
    }
    async fn create(&self, zone_id: &str, r: &NewRecord) -> Result<Record, CfError> {
        assert_eq!(zone_id, ZONE);
        let rec = Record {
            id: self.id(),
            kind: r.kind.clone(),
            name: r.name.clone(),
            content: r.content.clone(),
            proxied: r.proxied,
            comment: Some(r.comment.clone()),
        };
        self.calls
            .lock()
            .unwrap()
            .push(format!("create {} {}", r.kind, r.name));
        self.records.lock().unwrap().push(rec.clone());
        Ok(rec)
    }
    async fn set_proxied(&self, _: &str, _: &str, _: bool) -> Result<(), CfError> {
        unreachable!("edge records are never proxied")
    }
    async fn delete(&self, _: &str, id: &str) -> Result<(), CfError> {
        self.calls.lock().unwrap().push(format!("delete {id}"));
        let mut rs = self.records.lock().unwrap();
        let n = rs.len();
        rs.retain(|r| r.id != id);
        if rs.len() == n {
            return Err(CfError::NotFound);
        }
        Ok(())
    }
}

/// No waiting: the fake has the record at once.
struct Now;

impl Propagation for Now {
    async fn wait(&self, _: &str, _: &str) {}
}

// ── A mock ACME server ─────────────────────────────────────────────────────

struct Acme {
    base: String,
    cf: Arc<Cf>,
    /// The account key's JWK thumbprint, from newAccount.
    thumbprint: Option<String>,
    domain: Option<String>,
    authz: &'static str,
    order: &'static str,
    error: Option<String>,
    cert: Option<String>,
    /// The TXT value the challenge found.
    seen: Option<String>,
    ca: (rcgen::CertificateParams, rcgen::KeyPair),
    nonce: u64,
}

type Shared = Arc<Mutex<Acme>>;

/// The payload of a JWS, and its protected header.
fn jws(body: &[u8]) -> (Value, Value) {
    let v: Value = serde_json::from_slice(body).unwrap();
    let part = |k: &str| -> Value {
        let b = B64.decode(v[k].as_str().unwrap()).unwrap();
        if b.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&b).unwrap()
        }
    };
    (part("protected"), part("payload"))
}

/// RFC 7638's thumbprint of an EC key.
fn thumbprint(jwk: &Value) -> String {
    let canonical = format!(
        r#"{{"crv":"{}","kty":"{}","x":"{}","y":"{}"}}"#,
        jwk["crv"].as_str().unwrap(),
        jwk["kty"].as_str().unwrap(),
        jwk["x"].as_str().unwrap(),
        jwk["y"].as_str().unwrap()
    );
    B64.encode(Sha256::digest(canonical.as_bytes()))
}

fn reply(st: &Shared, status: StatusCode, location: Option<String>, body: Value) -> Response {
    let mut a = st.lock().unwrap();
    a.nonce += 1;
    let mut h = HeaderMap::new();
    h.insert(
        "replay-nonce",
        HeaderValue::from_str(&format!("n{}", a.nonce)).unwrap(),
    );
    if let Some(l) = location {
        h.insert("location", HeaderValue::from_str(&l).unwrap());
    }
    (status, h, Json(body)).into_response()
}

fn order_json(a: &Acme) -> Value {
    json!({
        "status": a.order,
        "identifiers": [{ "type": "dns", "value": format!("*.{}", a.domain.as_deref().unwrap_or("")) }],
        "authorizations": [format!("{}/authz/1", a.base)],
        "finalize": format!("{}/finalize/1", a.base),
        "certificate": a.cert.as_ref().map(|_| format!("{}/cert/1", a.base)),
        "error": a.error.as_ref().map(|d| json!({ "type": "urn:ietf:params:acme:error:unauthorized", "detail": d })),
    })
}

fn authz_json(a: &Acme) -> Value {
    json!({
        "status": a.authz,
        "identifier": { "type": "dns", "value": a.domain },
        "wildcard": true,
        "challenges": [
            { "type": "http-01", "url": format!("{}/chall/0", a.base), "token": "nope", "status": "pending" },
            { "type": "dns-01", "url": format!("{}/chall/1", a.base), "token": "tok-1", "status": if a.authz == "valid" { "valid" } else { "pending" } },
        ],
    })
}

async fn mock_acme(cf: Arc<Cf>) -> (String, Shared) {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", l.local_addr().unwrap());
    let ca_key = rcgen::KeyPair::generate().unwrap();
    let mut ca = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap();
    ca.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    ca.distinguished_name
        .push(rcgen::DnType::CommonName, "Mock ACME CA");
    let st: Shared = Arc::new(Mutex::new(Acme {
        base: base.clone(),
        cf,
        thumbprint: None,
        domain: None,
        authz: "pending",
        order: "pending",
        error: None,
        cert: None,
        seen: None,
        ca: (ca, ca_key),
        nonce: 0,
    }));
    let app = Router::new()
        .route(
            "/directory",
            get(|State(st): State<Shared>| async move {
                let b = st.lock().unwrap().base.clone();
                Json(json!({
                    "newNonce": format!("{b}/nonce"),
                    "newAccount": format!("{b}/account"),
                    "newOrder": format!("{b}/order"),
                }))
            }),
        )
        .route(
            "/nonce",
            get(|State(st): State<Shared>| async move { reply(&st, StatusCode::OK, None, json!({})) })
                .head(|State(st): State<Shared>| async move {
                    reply(&st, StatusCode::OK, None, json!({}))
                }),
        )
        .route(
            "/account",
            post(|State(st): State<Shared>, body: Bytes| async move {
                let (protected, _) = jws(&body);
                let base = {
                    let mut a = st.lock().unwrap();
                    a.thumbprint = Some(thumbprint(&protected["jwk"]));
                    a.base.clone()
                };
                reply(
                    &st,
                    StatusCode::CREATED,
                    Some(format!("{base}/acct/1")),
                    json!({ "status": "valid" }),
                )
            }),
        )
        .route(
            "/order",
            post(|State(st): State<Shared>, body: Bytes| async move {
                let (protected, payload) = jws(&body);
                assert!(protected["kid"].as_str().unwrap().ends_with("/acct/1"));
                let id = payload["identifiers"][0]["value"].as_str().unwrap();
                let domain = id.strip_prefix("*.").expect("a wildcard order").to_owned();
                let (base, o) = {
                    let mut a = st.lock().unwrap();
                    a.domain = Some(domain);
                    (a.base.clone(), order_json(&a))
                };
                reply(&st, StatusCode::CREATED, Some(format!("{base}/order/1")), o)
            }),
        )
        .route(
            "/order/1",
            post(|State(st): State<Shared>| async move {
                let o = order_json(&st.lock().unwrap());
                reply(&st, StatusCode::OK, None, o)
            }),
        )
        .route(
            "/authz/1",
            post(|State(st): State<Shared>| async move {
                let v = authz_json(&st.lock().unwrap());
                reply(&st, StatusCode::OK, None, v)
            }),
        )
        .route(
            "/chall/{n}",
            post(|State(st): State<Shared>, Path(n): Path<u32>| async move {
                assert_eq!(n, 1, "only the DNS challenge is answered");
                let v = {
                    let mut a = st.lock().unwrap();
                    let domain = a.domain.clone().unwrap();
                    let expected = B64.encode(Sha256::digest(
                        format!("tok-1.{}", a.thumbprint.as_ref().unwrap()).as_bytes(),
                    ));
                    // What a resolver would find at _acme-challenge.<domain>.
                    let found = a
                        .cf
                        .named(&format!("_acme-challenge.{domain}"))
                        .into_iter()
                        .filter(|r| r.kind == "TXT")
                        .map(|r| r.content.trim_matches('"').to_owned())
                        .find(|c| *c == expected);
                    a.seen = found.clone();
                    if found.is_some() {
                        (a.authz, a.order) = ("valid", "ready");
                    } else {
                        (a.authz, a.order) = ("invalid", "invalid");
                        a.error = Some(format!("No TXT record found at _acme-challenge.{domain}"));
                    }
                    json!({ "type": "dns-01", "url": format!("{}/chall/1", a.base), "token": "tok-1", "status": "processing" })
                };
                reply(&st, StatusCode::OK, None, v)
            }),
        )
        .route(
            "/finalize/1",
            post(|State(st): State<Shared>, body: Bytes| async move {
                let (_, payload) = jws(&body);
                let csr = B64.decode(payload["csr"].as_str().unwrap()).unwrap();
                let o = {
                    let mut a = st.lock().unwrap();
                    assert_eq!(a.order, "ready");
                    let req =
                        rcgen::CertificateSigningRequestParams::from_der(&csr.into()).unwrap();
                    let wanted = format!("*.{}", a.domain.as_ref().unwrap());
                    assert!(
                        req.params.subject_alt_names.iter().any(|n| matches!(n, rcgen::SanType::DnsName(d) if d.as_str() == wanted)),
                        "{:?}",
                        req.params.subject_alt_names
                    );
                    let (ca, ca_key) = &a.ca;
                    let ca_pem = ca.self_signed(ca_key).unwrap().pem();
                    let issuer = rcgen::Issuer::new(ca.clone(), ca_key);
                    let leaf = req.signed_by(&issuer).unwrap();
                    a.cert = Some(format!("{}{ca_pem}", leaf.pem()));
                    a.order = "valid";
                    order_json(&a)
                };
                reply(&st, StatusCode::OK, None, o)
            }),
        )
        .route(
            "/cert/1",
            post(|State(st): State<Shared>| async move {
                let mut a = st.lock().unwrap();
                a.nonce += 1;
                (
                    [
                        ("replay-nonce", format!("n{}", a.nonce)),
                        ("content-type", "application/pem-certificate-chain".into()),
                    ],
                    a.cert.clone().unwrap(),
                )
                    .into_response()
            }),
        )
        .with_state(st.clone());
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    (format!("{base}/directory"), st)
}

async fn new_account(directory: &str) -> Account {
    Account::builder_with_http(Box::new(Reqwest(reqwest::Client::new())))
        .create(
            &NewAccount {
                contact: &[],
                terms_of_service_agreed: true,
                only_return_existing: false,
            },
            directory.to_owned(),
            None,
        )
        .await
        .unwrap()
        .0
}

fn quick() -> RetryPolicy {
    RetryPolicy::new()
        .initial_delay(Duration::from_millis(10))
        .timeout(Duration::from_secs(5))
}

// ── Tests ──────────────────────────────────────────────────────────────────

#[tokio::test]
async fn a_wildcard_certificate_through_dns01() {
    // A record a cut-off attempt left, and someone else's: only Pyo's goes.
    let cf = Arc::new(
        Cf::default()
            .with(
                "TXT",
                "_acme-challenge.sbx.example.com",
                "\"old\"",
                Some(MARK),
            )
            .with("TXT", "_acme-challenge.sbx.example.com", "\"theirs\"", None),
    );
    let (dir, acme) = mock_acme(cf.clone()).await;
    let account = new_account(&dir).await;
    let issued = obtain(&account, &cf, ZONE, MARK, "sbx.example.com", &Now, &quick())
        .await
        .unwrap();

    // The CA saw the right TXT value, and signed a key only Pyo has.
    assert!(acme.lock().unwrap().seen.is_some());
    let p = check(&issued, "sbx.example.com", pyo_proto::now_ms() / 1000).unwrap();
    assert_eq!(p.hostnames, vec!["*.sbx.example.com"]);
    assert!(issued.key.contains("PRIVATE KEY"));
    assert_eq!(issued.chain.matches("BEGIN CERTIFICATE").count(), 2);
    assert!(check(&issued, "other.example.com", 0).is_err());

    // Only the other's TXT record is left.
    let left = cf.named("_acme-challenge.sbx.example.com");
    assert_eq!(left.len(), 1, "{left:?}");
    assert_eq!(left[0].content, "\"theirs\"");
    let calls = cf.calls();
    assert_eq!(calls[0], "delete r1", "{calls:?}");
    assert_eq!(calls[1], "create TXT _acme-challenge.sbx.example.com");
    assert!(calls[2].starts_with("delete "), "{calls:?}");
}

/// A Cloudflare account that takes the record but stores another value,
/// so the CA doesn't find it.
struct Dropping(Arc<Cf>);

impl Cloudflare for Dropping {
    async fn zones(&self) -> Result<Vec<(String, String)>, CfError> {
        self.0.zones().await
    }
    async fn records(&self, z: &str, n: &str) -> Result<Vec<Record>, CfError> {
        self.0.records(z, n).await
    }
    async fn record(&self, z: &str, id: &str) -> Result<Record, CfError> {
        self.0.record(z, id).await
    }
    async fn create(&self, z: &str, r: &NewRecord) -> Result<Record, CfError> {
        let mut wrong = r.clone();
        wrong.content = "\"something else\"".into();
        self.0.create(z, &wrong).await
    }
    async fn set_proxied(&self, z: &str, id: &str, p: bool) -> Result<(), CfError> {
        self.0.set_proxied(z, id, p).await
    }
    async fn delete(&self, z: &str, id: &str) -> Result<(), CfError> {
        self.0.delete(z, id).await
    }
}

#[tokio::test]
async fn a_failed_challenge_still_cleans_up() {
    let cf = Arc::new(Cf::default());
    let (dir, _) = mock_acme(cf.clone()).await;
    let account = new_account(&dir).await;
    let e = obtain(
        &account,
        &Dropping(cf.clone()),
        ZONE,
        MARK,
        "sbx.example.com",
        &Now,
        &quick(),
    )
    .await
    .err()
    .unwrap();
    assert!(e.contains("No TXT record found"), "{e}");
    assert!(cf.named("_acme-challenge.sbx.example.com").is_empty());
}

/// A token Cloudflare doesn't take.
struct Refusing;

impl Cloudflare for Refusing {
    async fn zones(&self) -> Result<Vec<(String, String)>, CfError> {
        Ok(vec![])
    }
    async fn records(&self, _: &str, _: &str) -> Result<Vec<Record>, CfError> {
        Ok(vec![])
    }
    async fn record(&self, _: &str, _: &str) -> Result<Record, CfError> {
        Err(CfError::NotFound)
    }
    async fn create(&self, _: &str, _: &NewRecord) -> Result<Record, CfError> {
        Err(CfError::Denied("Authentication error".into()))
    }
    async fn set_proxied(&self, _: &str, _: &str, _: bool) -> Result<(), CfError> {
        Ok(())
    }
    async fn delete(&self, _: &str, _: &str) -> Result<(), CfError> {
        Ok(())
    }
}

#[tokio::test]
async fn cloudflare_refusing_the_record_is_said() {
    let (dir, _) = mock_acme(Arc::new(Cf::default())).await;
    let account = new_account(&dir).await;
    let e = obtain(
        &account,
        &Refusing,
        ZONE,
        MARK,
        "sbx.example.com",
        &Now,
        &quick(),
    )
    .await
    .err()
    .unwrap();
    assert_eq!(e, "Cloudflare refused the token: Authentication error");
}

#[tokio::test]
async fn the_wildcard_record() {
    let name = "*.sbx.example.com";
    let both = vec![
        ("A".to_owned(), "203.0.114.7".to_owned()),
        ("AAAA".to_owned(), "2a01:4f8::7".to_owned()),
    ];
    // Nothing there: both made, DNS-only, with Pyo's comment.
    let cf = Arc::new(Cf::default());
    let out = ensure_wildcard(&cf, ZONE, name, &both, MARK).await.unwrap();
    assert_eq!(out.status, "created");
    assert_eq!(out.made.len(), 2);
    assert!(
        cf.named(name)
            .iter()
            .all(|r| !r.proxied && r.comment.as_deref() == Some(MARK))
    );
    // Again: nothing to do.
    let again = ensure_wildcard(&cf, ZONE, name, &both, MARK).await.unwrap();
    assert_eq!((again.status, again.made.len()), ("exists", 0));

    // The A record there already: only the AAAA is made.
    let cf = Arc::new(Cf::default().with("A", name, "203.0.114.7", None));
    let out = ensure_wildcard(&cf, ZONE, name, &both, MARK).await.unwrap();
    assert_eq!(out.status, "created");
    assert_eq!(out.made.len(), 1);
    assert_eq!(out.made[0].kind, "AAAA");

    // Another address or a CNAME on the name: nothing changes.
    for (kind, content) in [("A", "198.51.100.1"), ("CNAME", "elsewhere.example.net")] {
        let cf = Arc::new(Cf::default().with(kind, name, content, None));
        let out = ensure_wildcard(&cf, ZONE, name, &both, MARK).await.unwrap();
        assert_eq!(out.status, "conflict");
        assert_eq!(out.existing[0].content, content);
        assert!(cf.calls().is_empty(), "{:?}", cf.calls());
    }
    // A TXT record on the name is no conflict.
    let cf = Arc::new(Cf::default().with("TXT", name, "\"x\"", None));
    let out = ensure_wildcard(&cf, ZONE, name, &both[..1], MARK)
        .await
        .unwrap();
    assert_eq!(out.status, "created");
}

#[test]
fn directories_and_renewals() {
    assert_eq!(
        directory_from(None),
        "https://acme-v02.api.letsencrypt.org/directory"
    );
    assert_eq!(
        directory_from(Some("staging")),
        "https://acme-staging-v02.api.letsencrypt.org/directory"
    );
    assert_eq!(
        directory_from(Some("https://pebble.test/dir")),
        "https://pebble.test/dir"
    );
    assert_eq!(
        challenge_name("sbx.example.com"),
        "_acme-challenge.sbx.example.com"
    );
    let day = 86_400;
    // 90 days: renewed 30 days before the end.
    assert!(!due(0, 90 * day, 59 * day));
    assert!(due(0, 90 * day, 60 * day));
    // 45 days: a third, 15 days before.
    assert!(!due(0, 45 * day, 29 * day));
    assert!(due(0, 45 * day, 30 * day));
    // Run out: due.
    assert!(due(0, 90 * day, 91 * day));
}
