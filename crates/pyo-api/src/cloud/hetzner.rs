//! Hetzner Cloud (https://docs.hetzner.cloud): a team's project API token,
//! with read and write. Calls go only to [`API`], through Pyo's guarded
//! HTTP client (outbound.rs: public addresses, no redirects, no proxy).
//! The token is never logged, and nothing here prints a request.

use std::collections::BTreeMap;
use std::time::Duration;

use reqwest::Method;
use serde_json::{Value, json};

use super::{Catalog, CloudError, Create, Location, Provider, Remote, ServerType};

pub const API: &str = "https://api.hetzner.cloud/v1";
/// The image every server starts from.
pub const IMAGE: &str = "ubuntu-24.04";
/// Tailscale's port for direct connections (optional, but faster than relays).
pub const TAILSCALE_PORT: &str = "41641";

pub struct Hetzner {
    token: String,
    http: Http,
}

enum Http {
    Real(reqwest::Client),
    #[cfg(test)]
    Mock(std::sync::Arc<mock::Mock>),
}

/// A Hetzner token as its console makes them: 64 letters and digits.
pub fn valid_token(t: &str) -> bool {
    t.len() == 64 && t.bytes().all(|b| b.is_ascii_alphanumeric())
}

impl Hetzner {
    pub fn new(token: &str) -> Result<Self, CloudError> {
        let http = crate::outbound::client(
            crate::outbound::Reach { private: false },
            Duration::from_secs(30),
        )
        .map_err(|e| CloudError::Network(e.to_string()))?;
        Ok(Self {
            token: token.to_owned(),
            http: Http::Real(http),
        })
    }

    #[cfg(test)]
    pub fn mock(token: &str, m: std::sync::Arc<mock::Mock>) -> Self {
        Self {
            token: token.to_owned(),
            http: Http::Mock(m),
        }
    }

    async fn call(
        &self,
        method: Method,
        path: &str,
        body: Option<Value>,
    ) -> Result<Value, CloudError> {
        let (status, v) = match &self.http {
            Http::Real(http) => {
                let url = format!("{API}{path}");
                debug_assert!(url.starts_with("https://api.hetzner.cloud/"));
                let mut req = http.request(method, &url).bearer_auth(&self.token);
                if let Some(b) = &body {
                    req = req.json(b);
                }
                // The error says what failed, never the request (it carries the token).
                let r = req
                    .send()
                    .await
                    .map_err(|e| CloudError::Network(e.without_url().to_string()))?;
                let status = r.status().as_u16();
                let text = r.text().await.unwrap_or_default();
                (status, serde_json::from_str(&text).unwrap_or(Value::Null))
            }
            #[cfg(test)]
            Http::Mock(m) => m.handle(&self.token, &method, path, body.as_ref()),
        };
        if (200..300).contains(&status) {
            Ok(v)
        } else {
            Err(error_of(status, &v))
        }
    }

    /// Every page of a list: `key`'s items.
    async fn all(&self, path: &str, key: &str) -> Result<Vec<Value>, CloudError> {
        let mut out = Vec::new();
        let sep = if path.contains('?') { '&' } else { '?' };
        let mut page = 1;
        loop {
            let v = self
                .call(
                    Method::GET,
                    &format!("{path}{sep}per_page=50&page={page}"),
                    None,
                )
                .await?;
            out.extend(v[key].as_array().cloned().unwrap_or_default());
            match v["meta"]["pagination"]["next_page"].as_u64() {
                Some(n) if n > page && page < 20 => page = n,
                _ => return Ok(out),
            }
        }
    }
}

/// What Hetzner's error says, as people read it.
pub fn error_of(status: u16, body: &Value) -> CloudError {
    let code = body["error"]["code"].as_str().unwrap_or("");
    let message = body["error"]["message"]
        .as_str()
        .unwrap_or("")
        .chars()
        .take(300)
        .collect::<String>();
    match (status, code) {
        (401, _) | (_, "unauthorized") => CloudError::Unauthorized,
        (_, "forbidden") => CloudError::ReadOnly,
        (_, "resource_limit_exceeded") => CloudError::Quota(message),
        (
            _,
            "resource_unavailable" | "placement_error" | "unavailable" | "server_type_unavailable",
        ) => CloudError::Unavailable(message),
        (_, "uniqueness_error") => CloudError::NameTaken,
        (429, _) | (_, "rate_limit_exceeded") => CloudError::RateLimited,
        (404, _) | (_, "not_found") => CloudError::NotFound,
        _ => CloudError::Other {
            status,
            message: if message.is_empty() {
                format!("status {status}")
            } else {
                message
            },
        },
    }
}

fn s(v: &Value) -> String {
    v.as_str().unwrap_or("").to_owned()
}

/// Hetzner's prices are decimal strings ("3.7900000000").
fn price(v: &Value) -> Option<f64> {
    v.as_str().and_then(|p| p.parse::<f64>().ok())
}

fn remote(v: &Value) -> Remote {
    let id = match &v["id"] {
        Value::Number(n) => n.to_string(),
        other => s(other),
    };
    Remote {
        id,
        name: s(&v["name"]),
        status: s(&v["status"]),
        labels: v["labels"]
            .as_object()
            .map(|o| {
                o.iter()
                    .map(|(k, v)| (k.clone(), s(v)))
                    .collect::<BTreeMap<_, _>>()
            })
            .unwrap_or_default(),
        ipv4: v["public_net"]["ipv4"]["ip"].as_str().map(str::to_owned),
    }
}

/// A firewall's rules: the web from anywhere, and Tailscale's port for
/// direct connections. Everything else stays closed: the cluster talks
/// over the tailnet.
pub fn firewall_rules() -> Value {
    let anywhere = json!(["0.0.0.0/0", "::/0"]);
    json!([
        { "direction": "in", "protocol": "tcp", "port": "80", "source_ips": anywhere, "description": "Web" },
        { "direction": "in", "protocol": "tcp", "port": "443", "source_ips": anywhere, "description": "Web" },
        { "direction": "in", "protocol": "udp", "port": TAILSCALE_PORT, "source_ips": anywhere, "description": "Tailscale direct connections" },
    ])
}

impl Provider for Hetzner {
    fn title(&self) -> &'static str {
        "Hetzner"
    }

    async fn locations(&self) -> Result<Vec<Location>, CloudError> {
        let mut out: Vec<Location> = self
            .all("/locations", "locations")
            .await?
            .iter()
            .map(|l| Location {
                name: s(&l["name"]),
                city: s(&l["city"]),
                country: s(&l["country"]),
                network_zone: s(&l["network_zone"]),
            })
            .filter(|l| !l.name.is_empty())
            .collect();
        out.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(out)
    }

    async fn catalog(&self) -> Result<Catalog, CloudError> {
        let locations = self.locations().await?;
        let types = self.all("/server_types", "server_types").await?;
        // The IPv4 address each server gets costs extra.
        let pricing = self.call(Method::GET, "/pricing", None).await?;
        let currency = pricing["pricing"]["currency"]
            .as_str()
            .unwrap_or("EUR")
            .to_owned();
        let mut ipv4: BTreeMap<String, f64> = BTreeMap::new();
        for ip in pricing["pricing"]["primary_ips"]
            .as_array()
            .into_iter()
            .flatten()
        {
            if ip["type"] != "ipv4" {
                continue;
            }
            for p in ip["prices"].as_array().into_iter().flatten() {
                if let Some(m) = price(&p["price_monthly"]["net"]) {
                    ipv4.insert(s(&p["location"]), m);
                }
            }
        }
        let mut server_types: Vec<ServerType> = types
            .iter()
            .filter_map(|t| {
                // Each type lists the locations that sell it. A location
                // where Hetzner is phasing the type out has a `deprecation`
                // and is left out, the same as a type phased out everywhere.
                // `available` false means sold out for now.
                let sold: Vec<(String, bool)> = t["locations"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter(|l| l["deprecation"].is_null())
                    .map(|l| (s(&l["name"]), l["available"] == true))
                    .filter(|(name, _)| !name.is_empty())
                    .collect();
                if sold.is_empty() {
                    return None;
                }
                let mut available: Vec<String> = sold
                    .iter()
                    .filter(|(_, now)| *now)
                    .map(|(name, _)| name.clone())
                    .collect();
                available.sort();
                let architecture = s(&t["architecture"]);
                Some(ServerType {
                    name: s(&t["name"]),
                    description: s(&t["description"]),
                    cores: t["cores"].as_u64().unwrap_or(0) as u32,
                    memory_gb: t["memory"].as_f64().unwrap_or(0.0),
                    disk_gb: t["disk"].as_u64().unwrap_or(0),
                    cpu_type: s(&t["cpu_type"]),
                    supported: architecture == "x86",
                    architecture,
                    prices: t["prices"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(|p| {
                            let loc = s(&p["location"]);
                            if !sold.iter().any(|(name, _)| *name == loc) {
                                return None;
                            }
                            let m = price(&p["price_monthly"]["net"])?;
                            let ip = ipv4.get(&loc).copied().unwrap_or(0.0);
                            Some((loc, ((m + ip) * 100.0).round() / 100.0))
                        })
                        .collect(),
                    available,
                })
            })
            .collect();
        server_types.sort_by(|a, b| {
            let cheapest =
                |t: &ServerType| t.prices.values().copied().fold(f64::INFINITY, f64::min);
            cheapest(a)
                .total_cmp(&cheapest(b))
                .then_with(|| a.name.cmp(&b.name))
        });
        Ok(Catalog {
            locations,
            server_types,
            currency,
        })
    }

    async fn create(&self, r: &Create<'_>) -> Result<Remote, CloudError> {
        let mut body = json!({
            "name": r.name,
            "server_type": r.server_type,
            "location": r.location,
            "image": IMAGE,
            "user_data": r.user_data,
            "labels": r.labels,
            "start_after_create": true,
            "public_net": { "enable_ipv4": true, "enable_ipv6": true },
        });
        if let Some(fw) = r.firewall.and_then(|f| f.parse::<u64>().ok()) {
            body["firewalls"] = json!([{ "firewall": fw }]);
        }
        let v = self.call(Method::POST, "/servers", Some(body)).await?;
        // Without an SSH key Hetzner answers with a root password too. It's
        // dropped here, never kept or shown.
        Ok(remote(&v["server"]))
    }

    async fn server(&self, id: &str) -> Result<Option<Remote>, CloudError> {
        if !id.bytes().all(|b| b.is_ascii_digit()) || id.is_empty() {
            return Ok(None);
        }
        match self
            .call(Method::GET, &format!("/servers/{id}"), None)
            .await
        {
            Ok(v) => Ok(Some(remote(&v["server"]))),
            Err(CloudError::NotFound) => Ok(None),
            Err(e) => Err(e),
        }
    }

    async fn delete_unchecked(&self, id: &str) -> Result<(), CloudError> {
        if !id.bytes().all(|b| b.is_ascii_digit()) || id.is_empty() {
            return Err(CloudError::NotFound);
        }
        match self
            .call(Method::DELETE, &format!("/servers/{id}"), None)
            .await
        {
            Ok(_) | Err(CloudError::NotFound) => Ok(()),
            Err(e) => Err(e),
        }
    }

    async fn firewall(&self, install: &str) -> Result<String, CloudError> {
        let selector = format!("pyo%3D{install}");
        let found = self
            .all(
                &format!("/firewalls?label_selector={selector}"),
                "firewalls",
            )
            .await?;
        if let Some(id) = found
            .first()
            .map(|f| remote(f).id)
            .filter(|i| !i.is_empty())
        {
            return Ok(id);
        }
        let v = self
            .call(
                Method::POST,
                "/firewalls",
                Some(json!({
                    "name": format!("pyo-{install}"),
                    "labels": { super::LABEL: install },
                    "rules": firewall_rules(),
                })),
            )
            .await?;
        Ok(remote(&v["firewall"]).id)
    }
}

/// An in-memory Hetzner for tests: its servers, firewalls and limits.
#[cfg(test)]
pub mod mock {
    use std::sync::Mutex;

    use super::*;

    pub const TOKEN: &str = "a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6abcd";

    #[derive(Default)]
    pub struct State {
        pub servers: BTreeMap<u64, Value>,
        pub firewalls: BTreeMap<u64, Value>,
        pub next: u64,
        /// The project's server limit.
        pub limit: usize,
        /// Server types sold out.
        pub sold_out: Vec<String>,
        pub calls: Vec<String>,
        /// The last server's user data.
        pub user_data: String,
    }

    pub struct Mock(pub Mutex<State>);

    impl Mock {
        pub fn new() -> std::sync::Arc<Self> {
            std::sync::Arc::new(Mock(Mutex::new(State {
                limit: 5,
                next: 100,
                ..Default::default()
            })))
        }

        /// A server someone else made, with these labels.
        pub fn add_server(&self, labels: Value) -> u64 {
            let mut st = self.0.lock().unwrap();
            st.next += 1;
            let id = st.next;
            st.servers.insert(
                id,
                json!({ "id": id, "name": format!("other-{id}"), "status": "running", "labels": labels }),
            );
            id
        }

        fn err(status: u16, code: &str, message: &str) -> (u16, Value) {
            (
                status,
                json!({ "error": { "code": code, "message": message } }),
            )
        }

        pub fn handle(
            &self,
            token: &str,
            method: &Method,
            path: &str,
            body: Option<&Value>,
        ) -> (u16, Value) {
            let mut st = self.0.lock().unwrap();
            st.calls.push(format!("{method} {path}"));
            if token != TOKEN {
                return Self::err(401, "unauthorized", "unable to authenticate");
            }
            let route = path.split('?').next().unwrap_or("");
            let query = path.split_once('?').map(|(_, q)| q).unwrap_or("");
            let page_one = json!({ "pagination": { "page": 1, "next_page": null } });
            match (method.as_str(), route) {
                ("GET", "/locations") => (
                    200,
                    json!({ "locations": [
                    { "id": 1, "name": "fsn1", "city": "Falkenstein", "country": "DE", "network_zone": "eu-central" },
                    { "id": 2, "name": "ash", "city": "Ashburn, VA", "country": "US", "network_zone": "us-east" },
                ], "meta": page_one }),
                ),
                // The shape since 2026-11-02: availability and deprecation
                // per location, and no `deprecated` field.
                ("GET", "/server_types") => (
                    200,
                    json!({ "server_types": [
                    { "id": 104, "name": "cx23", "description": "CX23", "cores": 2, "memory": 4.0, "disk": 40, "cpu_type": "shared", "category": "Cost-Optimized", "architecture": "x86", "storage_type": "local", "deprecation": null,
                      "locations": [
                        { "id": 1, "name": "fsn1", "deprecation": null, "recommended": true, "available": true },
                        { "id": 2, "name": "ash", "deprecation": null, "recommended": false, "available": false } ],
                      "prices": [
                        { "location": "fsn1", "price_hourly": { "net": "0.0061000000", "gross": "0.0072590000" }, "price_monthly": { "net": "3.7900000000", "gross": "4.5101000000" }, "included_traffic": 21990232555520i64, "price_per_tb_traffic": { "net": "1.0000000000", "gross": "1.1900000000" } },
                        { "location": "ash", "price_hourly": { "net": "0.0080000000", "gross": "0.0080000000" }, "price_monthly": { "net": "4.9900000000", "gross": "4.9900000000" }, "included_traffic": 1099511627776i64, "price_per_tb_traffic": { "net": "1.0000000000", "gross": "1.0000000000" } } ] },
                    { "id": 45, "name": "cax11", "description": "CAX11", "cores": 2, "memory": 4.0, "disk": 40, "cpu_type": "shared", "category": "Cost-Optimized", "architecture": "arm", "storage_type": "local", "deprecation": null,
                      "locations": [ { "id": 1, "name": "fsn1", "deprecation": null, "recommended": false, "available": true } ],
                      "prices": [ { "location": "fsn1", "price_hourly": { "net": "0.0053000000", "gross": "0.0063070000" }, "price_monthly": { "net": "3.2900000000", "gross": "3.9151000000" }, "included_traffic": 21990232555520i64, "price_per_tb_traffic": { "net": "1.0000000000", "gross": "1.1900000000" } } ] },
                    { "id": 108, "name": "cpx22", "description": "CPX22", "cores": 2, "memory": 4.0, "disk": 80, "cpu_type": "shared", "category": "Regular Purpose", "architecture": "x86", "storage_type": "local", "deprecation": null,
                      "locations": [
                        { "id": 1, "name": "fsn1", "deprecation": { "announced": "2025-10-16T00:00:00Z", "unavailable_after": "2027-01-01T00:00:00Z" }, "recommended": false, "available": true },
                        { "id": 2, "name": "ash", "deprecation": null, "recommended": true, "available": true } ],
                      "prices": [
                        { "location": "fsn1", "price_hourly": { "net": "0.0100000000", "gross": "0.0119000000" }, "price_monthly": { "net": "6.4900000000", "gross": "7.7231000000" }, "included_traffic": 21990232555520i64, "price_per_tb_traffic": { "net": "1.0000000000", "gross": "1.1900000000" } },
                        { "location": "ash", "price_hourly": { "net": "0.0120000000", "gross": "0.0120000000" }, "price_monthly": { "net": "7.4900000000", "gross": "7.4900000000" }, "included_traffic": 1099511627776i64, "price_per_tb_traffic": { "net": "1.0000000000", "gross": "1.0000000000" } } ] },
                    { "id": 22, "name": "cx22", "description": "CX22", "cores": 2, "memory": 4.0, "disk": 40, "cpu_type": "shared", "category": "Cost-Optimized", "architecture": "x86", "storage_type": "local",
                      "deprecation": { "announced": "2025-10-16T00:00:00Z", "unavailable_after": "2026-12-31T00:00:00Z" },
                      "locations": [ { "id": 1, "name": "fsn1", "deprecation": { "announced": "2025-10-16T00:00:00Z", "unavailable_after": "2026-12-31T00:00:00Z" }, "recommended": false, "available": true } ],
                      "prices": [ { "location": "fsn1", "price_hourly": { "net": "0.0050000000", "gross": "0.0059500000" }, "price_monthly": { "net": "3.0000000000", "gross": "3.5700000000" }, "included_traffic": 21990232555520i64, "price_per_tb_traffic": { "net": "1.0000000000", "gross": "1.1900000000" } } ] },
                ], "meta": page_one }),
                ),
                // Removed on 2026-10-01: Hetzner answers 410 Gone.
                ("GET", r) if r.starts_with("/datacenters") => (
                    410,
                    json!({ "error": { "code": "deprecated_api_endpoint", "message": "API functionality was removed",
                        "details": { "announcement": "https://docs.hetzner.cloud/changelog" } } }),
                ),
                ("GET", "/pricing") => (
                    200,
                    json!({ "pricing": { "currency": "EUR", "primary_ips": [
                    { "type": "ipv4", "prices": [ { "location": "fsn1", "price_monthly": { "net": "0.5000000000" } } ] },
                ] } }),
                ),
                ("POST", "/servers") => {
                    let b = body.cloned().unwrap_or_default();
                    if st.servers.len() >= st.limit {
                        return Self::err(403, "resource_limit_exceeded", "server limit exceeded");
                    }
                    let ty = b["server_type"].as_str().unwrap_or("");
                    if st.sold_out.iter().any(|x| x == ty) {
                        return Self::err(
                            412,
                            "resource_unavailable",
                            "server type unavailable in location",
                        );
                    }
                    if st.servers.values().any(|v| v["name"] == b["name"]) {
                        return Self::err(409, "uniqueness_error", "server name is already used");
                    }
                    st.next += 1;
                    let id = st.next;
                    st.user_data = b["user_data"].as_str().unwrap_or("").to_owned();
                    let server = json!({ "id": id, "name": b["name"], "status": "initializing", "labels": b["labels"],
                        "public_net": { "ipv4": { "ip": "203.0.113.7" } } });
                    st.servers.insert(id, server.clone());
                    (
                        201,
                        json!({ "server": server, "root_password": "never-kept" }),
                    )
                }
                ("GET", r) if r.starts_with("/servers/") => {
                    let id: u64 = r["/servers/".len()..].parse().unwrap_or(0);
                    match st.servers.get(&id) {
                        Some(v) => (200, json!({ "server": v })),
                        None => Self::err(404, "not_found", "server not found"),
                    }
                }
                ("DELETE", r) if r.starts_with("/servers/") => {
                    let id: u64 = r["/servers/".len()..].parse().unwrap_or(0);
                    match st.servers.remove(&id) {
                        Some(_) => (200, json!({ "action": { "id": 1, "status": "running" } })),
                        None => Self::err(404, "not_found", "server not found"),
                    }
                }
                ("GET", "/firewalls") => {
                    let want = query
                        .split('&')
                        .find_map(|p| p.strip_prefix("label_selector="))
                        .map(|x| x.replace("%3D", "="))
                        .unwrap_or_default();
                    let (k, v) = want.split_once('=').unwrap_or(("", ""));
                    let list: Vec<Value> = st
                        .firewalls
                        .values()
                        .filter(|f| f["labels"][k] == v)
                        .cloned()
                        .collect();
                    (200, json!({ "firewalls": list, "meta": page_one }))
                }
                ("POST", "/firewalls") => {
                    st.next += 1;
                    let id = st.next;
                    let b = body.cloned().unwrap_or_default();
                    let fw = json!({ "id": id, "name": b["name"], "labels": b["labels"], "rules": b["rules"] });
                    st.firewalls.insert(id, fw.clone());
                    (201, json!({ "firewall": fw }))
                }
                _ => Self::err(404, "not_found", "no such route"),
            }
        }
    }
}
