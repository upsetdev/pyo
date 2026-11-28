//! A fake Cloudflare API over HTTP, for tests (docs/cloudflare-tunnel.md
//! §17.3): zones with their accounts, DNS records, tunnels with their
//! routes, tokens and connections, and Origin CA certificates signed by a
//! test CA. It answers in Cloudflare's envelope (`success`, `errors`,
//! `result`), so the real client ([`super::Http`]) runs against it.
//!
//! [`FakeCf::start`] serves it on a free local port; the client points at
//! [`FakeCf::base`] (`Http::at`, or `PYO_DEV_CLOUDFLARE_API` in a debug
//! build). Every call is kept, as "METHOD /path".

use std::sync::{Arc, Mutex};

use axum::{
    Router,
    body::Bytes,
    extract::State,
    http::{HeaderMap, Method, StatusCode, Uri},
    response::{IntoResponse, Response},
};
use serde_json::{Value, json};

/// What the fake account holds.
#[derive(Default)]
pub struct Account {
    /// The token it takes.
    pub token: String,
    /// (id, name, account id, account name).
    pub zones: Vec<(String, String, String, String)>,
    /// Accounts whose tunnels the token may not touch.
    pub no_tunnels: Vec<String>,
    /// (zone id, record).
    pub records: Vec<(String, Value)>,
    /// (account id, tunnel).
    pub tunnels: Vec<(String, Value)>,
    /// tunnel id → (version, config).
    pub configs: Vec<(String, i64, Value)>,
    /// (id, hostnames, revoked).
    pub certs: Vec<(String, Vec<String>, bool)>,
    pub calls: Vec<String>,
    next: u64,
}

#[derive(Clone)]
pub struct FakeCf {
    pub state: Arc<Mutex<Account>>,
    pub base: String,
}

fn ok(result: Value) -> Response {
    axum::Json(json!({ "success": true, "errors": [], "messages": [], "result": result, "result_info": { "page": 1, "total_pages": 1 } }))
        .into_response()
}

fn err(code: StatusCode, message: &str) -> Response {
    (
        code,
        axum::Json(json!({ "success": false, "errors": [{ "code": 1000 + code.as_u16(), "message": message }], "result": null })),
    )
        .into_response()
}

fn query(uri: &Uri, key: &str) -> Option<String> {
    uri.query()?.split('&').find_map(|kv| {
        let (k, v) = kv.split_once('=')?;
        (k == key).then(|| v.replace("%2A", "*").replace("%2a", "*"))
    })
}

impl FakeCf {
    /// Serve a fake account with this token and zones (id, name, account).
    pub async fn start(token: &str, zones: &[(&str, &str, &str)]) -> FakeCf {
        let state = Arc::new(Mutex::new(Account {
            token: token.into(),
            zones: zones
                .iter()
                .map(|(i, n, a)| (i.to_string(), n.to_string(), a.to_string(), format!("{a} Co")))
                .collect(),
            ..Default::default()
        }));
        let app = Router::new()
            .fallback(handle)
            .with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        FakeCf {
            state,
            base: format!("http://{addr}/client/v4"),
        }
    }

    pub fn calls(&self) -> Vec<String> {
        self.state.lock().unwrap().calls.clone()
    }

    pub fn records(&self, zone: &str) -> Vec<Value> {
        self.state
            .lock()
            .unwrap()
            .records
            .iter()
            .filter(|(z, _)| z == zone)
            .map(|(_, r)| r.clone())
            .collect()
    }

    pub fn tunnels(&self) -> Vec<Value> {
        self.state
            .lock()
            .unwrap()
            .tunnels
            .iter()
            .map(|(_, t)| t.clone())
            .collect()
    }

    /// Add a tunnel that's already in the account, made `age_mins` ago.
    pub fn put_tunnel(&self, account: &str, name: &str, age_mins: i64) {
        let mut a = self.state.lock().unwrap();
        a.next += 1;
        let made = jiff::Timestamp::now() - jiff::SignedDuration::from_mins(age_mins);
        let t = json!({ "id": uuid(a.next), "name": name, "status": "inactive", "created_at": made.to_string(), "deleted_at": null, "connections": [], "secret": "first" });
        a.tunnels.push((account.into(), t));
    }

    /// Add a record someone made by hand.
    pub fn put_record(&self, zone: &str, r: Value) {
        self.state.lock().unwrap().records.push((zone.into(), r));
    }

    /// Set a tunnel's connectors by their ids (UUIDs), two connections each.
    pub fn set_connectors(&self, tunnel: &str, clients: &[&str]) {
        for (_, t) in self.state.lock().unwrap().tunnels.iter_mut() {
            if t["id"] == tunnel {
                t["status"] = json!(if clients.is_empty() { "inactive" } else { "healthy" });
                t["connections"] = json!(clients.iter().flat_map(|c| ["FRA", "AMS"].map(|colo| json!({ "colo_name": colo, "client_id": c, "client_version": "2026.10.7", "opened_at": "2026-11-22T00:00:00Z", "origin_ip": "203.0.113.7" }))).collect::<Vec<_>>());
            }
        }
    }

    /// The ids of a tunnel's connectors, as Cloudflare lists them now.
    pub fn connected(&self, tunnel: &str) -> Vec<String> {
        let mut out: Vec<String> = self
            .tunnels()
            .iter()
            .filter(|t| t["id"] == tunnel)
            .flat_map(|t| t["connections"].as_array().cloned().unwrap_or_default())
            .filter_map(|c| c["client_id"].as_str().map(str::to_owned))
            .collect();
        out.dedup();
        out
    }

    /// Set a tunnel's status, as its connectors make it.
    pub fn set_status(&self, tunnel: &str, status: &str, colos: &[&str]) {
        for (_, t) in self.state.lock().unwrap().tunnels.iter_mut() {
            if t["id"] == tunnel {
                t["status"] = json!(status);
                t["connections"] = json!(colos.iter().map(|c| json!({ "colo_name": c, "client_id": "c1", "client_version": "2026.10.7", "opened_at": "2026-11-22T00:00:00Z", "origin_ip": "203.0.113.7" })).collect::<Vec<_>>());
            }
        }
    }
}

/// A uuid-shaped id, as Cloudflare makes for tunnels.
fn uuid(n: u64) -> String {
    format!("00000000-0000-4000-8000-{n:012x}")
}

/// Sign a CSR with a test CA, as Cloudflare's Origin CA does.
fn sign(csr: &str, days: i64) -> Option<String> {
    use rcgen::{CertificateParams, CertificateSigningRequestParams, IsCa, Issuer, KeyPair};
    let ca_key = KeyPair::generate().ok()?;
    let mut ca = CertificateParams::new(vec![]).ok()?;
    ca.is_ca = IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    let issuer = Issuer::new(ca, ca_key);
    let mut req = CertificateSigningRequestParams::from_pem(csr).ok()?;
    let now = time::OffsetDateTime::now_utc();
    req.params.not_before = now - time::Duration::days(1);
    req.params.not_after = now + time::Duration::days(days);
    Some(req.signed_by(&issuer).ok()?.pem())
}

async fn handle(
    State(s): State<Arc<Mutex<Account>>>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let mut a = s.lock().unwrap();
    let path = uri.path().trim_start_matches("/client/v4").to_owned();
    a.calls.push(format!("{method} {path}"));
    let bearer = headers
        .get("authorization")
        .and_then(|h| h.to_str().ok())
        .and_then(|h| h.strip_prefix("Bearer "))
        .unwrap_or("");
    if bearer != a.token {
        return err(StatusCode::FORBIDDEN, "Authentication error");
    }
    let body: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let seg: Vec<&str> = path.trim_matches('/').split('/').collect();
    let m = method.as_str();
    a.next += 1;
    let n = a.next;
    match (m, seg.as_slice()) {
        ("GET", ["zones"]) => ok(json!(a
            .zones
            .iter()
            .map(|(i, n, ai, an)| json!({ "id": i, "name": n, "account": { "id": ai, "name": an } }))
            .collect::<Vec<_>>())),
        ("GET", ["zones", zone, "dns_records"]) => {
            let name = query(&uri, "name");
            ok(json!(a
                .records
                .iter()
                .filter(|(z, r)| z == zone && name.as_deref().is_none_or(|n| r["name"] == n))
                .map(|(_, r)| r.clone())
                .collect::<Vec<_>>()))
        }
        ("POST", ["zones", zone, "dns_records"]) => {
            let mut r = body.clone();
            r["id"] = json!(format!("{n:032x}"));
            if r.get("ttl").is_none() {
                r["ttl"] = json!(1);
            }
            a.records.push((zone.to_string(), r.clone()));
            ok(r)
        }
        ("GET" | "PATCH" | "DELETE", ["zones", zone, "dns_records", id]) => {
            let Some(i) = a.records.iter().position(|(z, r)| z == zone && r["id"] == *id) else {
                return err(StatusCode::NOT_FOUND, "Record not found");
            };
            match m {
                "GET" => ok(a.records[i].1.clone()),
                "PATCH" => {
                    if let Some(o) = body.as_object() {
                        for (k, v) in o {
                            a.records[i].1[k] = v.clone();
                        }
                    }
                    ok(a.records[i].1.clone())
                }
                _ => {
                    a.records.remove(i);
                    ok(json!({ "id": id }))
                }
            }
        }
        (_, ["accounts", account, "cfd_tunnel", ..]) if a.no_tunnels.iter().any(|x| x == account) => {
            err(StatusCode::FORBIDDEN, "Authentication error")
        }
        ("GET", ["accounts", account, "cfd_tunnel"]) => {
            let name = query(&uri, "name").unwrap_or_default();
            ok(json!(a
                .tunnels
                .iter()
                .filter(|(ac, t)| ac == account && t["name"].as_str().unwrap_or("").starts_with(&name) && t["deleted_at"].is_null())
                .map(|(_, t)| t.clone())
                .collect::<Vec<_>>()))
        }
        ("POST", ["accounts", account, "cfd_tunnel"]) => {
            let t = json!({ "id": uuid(n), "name": body["name"], "status": "inactive", "created_at": jiff::Timestamp::now().to_string(), "deleted_at": null, "connections": [], "secret": "first" });
            a.tunnels.push((account.to_string(), t.clone()));
            ok(t)
        }
        (_, ["accounts", account, "cfd_tunnel", id, rest @ ..]) => {
            let Some(i) = a.tunnels.iter().position(|(ac, t)| ac == account && t["id"] == *id) else {
                return err(StatusCode::NOT_FOUND, "Tunnel not found");
            };
            match (m, rest) {
                ("GET", []) => ok(a.tunnels[i].1.clone()),
                ("PATCH", []) => {
                    a.tunnels[i].1["secret"] = body["tunnel_secret"].clone();
                    ok(a.tunnels[i].1.clone())
                }
                ("DELETE", []) => {
                    if a.tunnels[i].1["connections"].as_array().is_some_and(|c| !c.is_empty()) {
                        return err(StatusCode::BAD_REQUEST, "Cannot delete tunnel because it has active connections");
                    }
                    a.tunnels[i].1["deleted_at"] = json!("2026-11-22T00:00:00Z");
                    ok(a.tunnels[i].1.clone())
                }
                ("GET", ["token"]) => {
                    let secret = a.tunnels[i].1["secret"].as_str().unwrap_or("").to_owned();
                    ok(json!(format!("eyJ{id}{secret}")))
                }
                ("PUT", ["configurations"]) => {
                    let id = id.to_string();
                    let v = a.configs.iter().find(|c| c.0 == id).map_or(1, |c| c.1 + 1);
                    a.configs.retain(|c| c.0 != id);
                    a.configs.push((id.clone(), v, body["config"].clone()));
                    ok(json!({ "tunnel_id": id, "version": v, "config": body["config"] }))
                }
                ("GET", ["configurations"]) => {
                    let c = a.configs.iter().find(|c| c.0 == *id).cloned();
                    ok(c.map_or(json!({ "version": 0, "config": null }), |c| json!({ "version": c.1, "config": c.2 })))
                }
                ("DELETE", ["connections"]) => {
                    // One connector (client_id), or every one without it.
                    let client = query(&uri, "client_id");
                    if let Some(c) = &client
                        && let Some(last) = a.calls.last_mut()
                    {
                        last.push_str(&format!("?client_id={c}"));
                    }
                    let t = &mut a.tunnels[i].1;
                    let left: Vec<Value> = match &client {
                        Some(c) => t["connections"]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .filter(|x| x["client_id"] != json!(c))
                            .cloned()
                            .collect(),
                        None => vec![],
                    };
                    if left.is_empty() {
                        t["status"] = json!("inactive");
                    }
                    t["connections"] = json!(left);
                    ok(json!(null))
                }
                _ => err(StatusCode::NOT_FOUND, "No route"),
            }
        }
        ("GET", ["certificates"]) => ok(json!([])),
        ("POST", ["certificates"]) => {
            let hosts: Vec<String> = body["hostnames"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|h| h.as_str().map(str::to_owned))
                .collect();
            let days = body["requested_validity"].as_i64().unwrap_or(365);
            let Some(pem) = sign(body["csr"].as_str().unwrap_or(""), days) else {
                return err(StatusCode::BAD_REQUEST, "Invalid CSR");
            };
            let id = format!("{}", 100_000 + n);
            a.certs.push((id.clone(), hosts.clone(), false));
            ok(json!({ "id": id, "certificate": pem, "hostnames": hosts, "request_type": body["request_type"] }))
        }
        ("DELETE", ["certificates", id]) => match a.certs.iter_mut().find(|c| c.0 == *id) {
            Some(c) => {
                c.2 = true;
                ok(json!({ "id": id }))
            }
            None => err(StatusCode::NOT_FOUND, "Certificate not found"),
        },
        _ => err(StatusCode::NOT_FOUND, "No route"),
    }
}
