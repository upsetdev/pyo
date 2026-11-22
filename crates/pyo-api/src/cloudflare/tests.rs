//! Zone matching, records made, found, in the way or replaced, deleting
//! only Pyo's own, who may do what, and the HTTP client's handling of
//! Cloudflare's answers. Cloudflare itself is a fake account.

use std::{collections::HashMap, sync::Mutex};

use super::tunnel::{CfZone, Connection, Kept, OriginCert, Tunnel};
use super::*;

pub(super) const MARK: &str = "Made by Pyo (abc123)";
/// The installation [`MARK`] is the comment of.
pub(super) const INSTALL: &str = "abc123";

pub(super) fn zone(id: &str, name: &str) -> Zone {
    Zone {
        id: id.into(),
        name: name.into(),
        can_edit: true,
        missing: None,
        account_id: "acct1".into(),
        account_name: "Acme".into(),
        tunnel: TunnelAccess {
            ready: true,
            missing: vec![],
        },
    }
}

pub(super) fn rec(id: &str, kind: &str, name: &str, content: &str, comment: Option<&str>) -> Record {
    Record {
        id: id.into(),
        kind: kind.into(),
        name: name.into(),
        content: content.into(),
        proxied: false,
        comment: comment.map(str::to_owned),
    }
}

#[derive(Clone, Copy, PartialEq)]
pub(super) enum Dns {
    None,
    Read,
    Edit,
}

/// A Cloudflare account: zones, the token's DNS permission in each, its
/// tunnels and Origin CA certificates, and every call made.
pub(super) struct Fake {
    pub zones: Vec<(String, String, Dns)>,
    pub zone_read: bool,
    pub records: Mutex<Vec<(String, Record)>>,
    /// Records' TTLs, by id (1, automatic, when not set).
    pub ttls: Mutex<HashMap<String, u32>>,
    pub calls: Mutex<Vec<String>>,
    next: Mutex<u32>,
    /// Each zone's account (acct1 "Acme" when not set).
    pub accounts: HashMap<String, (String, String)>,
    /// Accounts where the token can't make tunnels.
    pub no_tunnels: Vec<String>,
    /// Zones whose Origin CA certificates the token can't read or issue.
    pub no_certs: Vec<String>,
    /// (account, tunnel).
    pub tunnels: Mutex<Vec<(String, Tunnel)>>,
    /// Each tunnel's routes: (version, config).
    pub configs: Mutex<HashMap<String, (i64, Value)>>,
    /// Each tunnel's secret.
    pub secrets: Mutex<HashMap<String, String>>,
    /// Issued Origin CA certificates: (id, names, revoked).
    pub certs: Mutex<Vec<(String, Vec<String>, bool)>>,
}

impl Fake {
    pub fn new(zones: &[(&str, &str, Dns)]) -> Self {
        Self {
            zones: zones
                .iter()
                .map(|(i, n, d)| (i.to_string(), n.to_string(), *d))
                .collect(),
            zone_read: true,
            records: Mutex::new(vec![]),
            ttls: Mutex::new(HashMap::new()),
            calls: Mutex::new(vec![]),
            next: Mutex::new(0),
            accounts: HashMap::new(),
            no_tunnels: vec![],
            no_certs: vec![],
            tunnels: Mutex::new(vec![]),
            configs: Mutex::new(HashMap::new()),
            secrets: Mutex::new(HashMap::new()),
            certs: Mutex::new(vec![]),
        }
    }

    pub fn with(self, zone: &str, r: Record) -> Self {
        self.records.lock().unwrap().push((zone.into(), r));
        self
    }

    pub fn account_of(&self, zone: &str) -> (String, String) {
        self.accounts
            .get(zone)
            .cloned()
            .unwrap_or_else(|| ("acct1".into(), "Acme".into()))
    }

    fn id(&self, prefix: &str) -> String {
        let mut n = self.next.lock().unwrap();
        *n += 1;
        format!("{prefix}{n}")
    }

    /// A tunnel id as Cloudflare makes them (a UUID).
    fn uuid(&self) -> String {
        let n = self.id("");
        format!("00000000-0000-4000-8000-{:0>12}", n)
    }

    pub fn tunnel_list(&self) -> Vec<Tunnel> {
        self.tunnels
            .lock()
            .unwrap()
            .iter()
            .map(|(_, t)| t.clone())
            .collect()
    }

    /// Set a tunnel's status and connections, as Cloudflare reports them.
    pub fn connect(&self, id: &str, status: &str, colos: &[&str]) {
        for (_, t) in self.tunnels.lock().unwrap().iter_mut() {
            if t.id == id {
                t.status = status.into();
                t.connections = colos
                    .iter()
                    .enumerate()
                    .map(|(i, c)| Connection {
                        client_id: format!("c{}", i / 2),
                        colo: (*c).into(),
                        client_version: "2026.10.7".into(),
                        opened_at: "2026-11-22T00:00:00Z".into(),
                        origin_ip: "203.0.113.7".into(),
                    })
                    .collect();
            }
        }
    }

    pub fn dns(&self, zone: &str) -> Dns {
        self.zones
            .iter()
            .find(|z| z.0 == zone)
            .map(|z| z.2)
            .unwrap_or(Dns::None)
    }

    pub fn log(&self, c: String) {
        self.calls.lock().unwrap().push(c);
    }

    pub fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }

    pub fn at(&self, zone: &str) -> Vec<Record> {
        self.records
            .lock()
            .unwrap()
            .iter()
            .filter(|(z, _)| z == zone)
            .map(|(_, r)| r.clone())
            .collect()
    }

    fn denied() -> CfError {
        CfError::Denied("Authentication error".into())
    }
}

impl Fake {
    fn zone_list(&self) -> Result<Vec<(String, String)>, CfError> {
        self.log("zones".into());
        if !self.zone_read {
            return Err(Self::denied());
        }
        Ok(self
            .zones
            .iter()
            .map(|z| (z.0.clone(), z.1.clone()))
            .collect())
    }
}

impl Cloudflare for Fake {
    async fn records(&self, zone_id: &str, name: &str) -> Result<Vec<Record>, CfError> {
        self.log(format!("list {zone_id} {name}"));
        if self.dns(zone_id) == Dns::None {
            return Err(Self::denied());
        }
        Ok(self
            .at(zone_id)
            .into_iter()
            .filter(|r| r.name == name)
            .collect())
    }

    async fn record(&self, zone_id: &str, id: &str) -> Result<Record, CfError> {
        self.log(format!("get {zone_id} {id}"));
        self.at(zone_id)
            .into_iter()
            .find(|r| r.id == id)
            .ok_or(CfError::NotFound)
    }

    async fn create(&self, zone_id: &str, r: &NewRecord) -> Result<Record, CfError> {
        self.log(format!(
            "create {zone_id} {} {} {}",
            r.kind, r.name, r.content
        ));
        if self.dns(zone_id) != Dns::Edit {
            return Err(Self::denied());
        }
        let made = Record {
            id: self.id("r"),
            kind: r.kind.clone(),
            name: r.name.clone(),
            content: r.content.clone(),
            proxied: r.proxied,
            comment: Some(r.comment.clone()),
        };
        self.records
            .lock()
            .unwrap()
            .push((zone_id.into(), made.clone()));
        Ok(made)
    }

    async fn set_proxied(&self, zone_id: &str, id: &str, proxied: bool) -> Result<(), CfError> {
        self.log(format!("proxy {zone_id} {id} {proxied}"));
        for (z, r) in self.records.lock().unwrap().iter_mut() {
            if z == zone_id && r.id == id {
                r.proxied = proxied;
                return Ok(());
            }
        }
        Err(CfError::NotFound)
    }

    async fn delete(&self, zone_id: &str, id: &str) -> Result<(), CfError> {
        self.log(format!("delete {zone_id} {id}"));
        if self.dns(zone_id) != Dns::Edit {
            return Err(Self::denied());
        }
        let mut rs = self.records.lock().unwrap();
        let before = rs.len();
        rs.retain(|(z, r)| !(z == zone_id && r.id == id));
        if rs.len() == before {
            return Err(CfError::NotFound);
        }
        Ok(())
    }
}

// ── Zones ──────────────────────────────────────────────────────────────────

#[test]
fn a_hostname_goes_to_the_longest_zone_it_is_in() {
    let zones = [
        zone("z1", "example.com"),
        zone("z2", "eu.example.com"),
        zone("z3", "other.org"),
    ];
    let z = |h: &str| zone_for(h, &zones).map(|z| z.id.as_str());
    assert_eq!(z("shop.example.com"), Some("z1"));
    assert_eq!(z("example.com"), Some("z1"), "the apex");
    assert_eq!(z("a.b.example.com"), Some("z1"), "deeper subdomains");
    assert_eq!(z("shop.eu.example.com"), Some("z2"), "the longest suffix");
    assert_eq!(z("eu.example.com"), Some("z2"));
    assert_eq!(
        z("SHOP.Example.COM."),
        Some("z1"),
        "case and a trailing dot"
    );
    assert_eq!(
        z("notexample.com"),
        None,
        "a label boundary, not any suffix"
    );
    assert_eq!(z("example.com.evil.net"), None);
    assert_eq!(z(".example.com"), None);
    assert_eq!(z("shop.other.org"), Some("z3"));
    assert_eq!(z("shop.example.net"), None);
    assert_eq!(zone_for("a.example.com", &[]), None);
}

#[tokio::test]
async fn a_test_lists_zones_and_says_what_is_missing() {
    let cf = Fake::new(&[
        ("z1", "example.com", Dns::Edit),
        ("z2", "readonly.dev", Dns::Read),
        ("z3", "nodns.io", Dns::None),
    ]);
    let zones = inspect(&cf, INSTALL).await.unwrap();
    let got: Vec<(&str, bool, Option<&str>)> = zones
        .iter()
        .map(|z| (z.name.as_str(), z.can_edit, z.missing.as_deref()))
        .collect();
    assert_eq!(
        got,
        [
            ("example.com", true, None),
            ("nodns.io", false, Some("DNS:Edit")),
            ("readonly.dev", false, Some("DNS:Edit")),
        ]
    );
    assert_eq!(
        problems(&zones),
        [
            "nodns.io: give the token DNS:Edit.",
            "readonly.dev: give the token DNS:Edit."
        ]
    );
    // The check leaves nothing behind.
    assert!(cf.at("z1").is_empty());
    assert!(
        cf.calls()
            .contains(&"create z1 TXT _pyo-check.example.com \"pyo-check-abc123\"".to_string())
    );
}

#[tokio::test]
async fn a_token_without_zone_read_is_refused() {
    let mut cf = Fake::new(&[("z1", "example.com", Dns::Edit)]);
    cf.zone_read = false;
    let e = inspect(&cf, INSTALL).await.unwrap_err();
    assert!(matches!(e, CfError::Denied(_)));
    assert!(e.to_string().starts_with("Cloudflare refused the token"));
    let none = Fake::new(&[]);
    let zones = inspect(&none, INSTALL).await.unwrap();
    assert_eq!(
        problems(&zones),
        ["The token can't see any zones. Give it Zone:Read on the zones Pyo may use."]
    );
}

#[tokio::test]
async fn a_probe_left_behind_is_cleaned_up() {
    let cf = Fake::new(&[("z1", "example.com", Dns::Edit)]).with(
        "z1",
        rec(
            "old",
            "TXT",
            "_pyo-check.example.com",
            "\"pyo-check-abc123\"",
            Some(MARK),
        ),
    );
    inspect(&cf, INSTALL).await.unwrap();
    assert!(cf.at("z1").is_empty());
}

#[tokio::test]
async fn another_installations_probe_record_is_left_alone() {
    // Two installations share the zone. The other one's check stopped
    // halfway and left its probe: this one's check still passes (its own
    // record's text differs, so Cloudflare doesn't call it a duplicate),
    // and the other's record stays.
    let theirs = rec(
        "theirs",
        "TXT",
        "_pyo-check.example.com",
        "\"pyo-check-zzz999\"",
        Some("Made by Pyo (zzz999)"),
    );
    let cf = Fake::new(&[("z1", "example.com", Dns::Edit)]).with("z1", theirs.clone());
    let zones = inspect(&cf, INSTALL).await.unwrap();
    assert!(zones[0].can_edit);
    assert_eq!(cf.at("z1"), [theirs]);
    assert!(
        cf.calls()
            .contains(&"create z1 TXT _pyo-check.example.com \"pyo-check-abc123\"".to_string())
    );
}

// ── Records ────────────────────────────────────────────────────────────────

#[test]
fn plans() {
    let h = "shop.example.com";
    assert_eq!(plan("A", "203.0.113.7", h, &[]), Plan::Create);
    // Other names and types don't matter.
    let unrelated = [
        rec("1", "A", "www.example.com", "198.51.100.1", None),
        rec("2", "MX", h, "mail.example.com", None),
        rec("3", "TXT", h, "\"v=spf1 -all\"", None),
    ];
    assert_eq!(plan("A", "203.0.113.7", h, &unrelated), Plan::Create);
    let same = rec("4", "A", "Shop.Example.com", "203.0.113.7", None);
    assert_eq!(
        plan("A", "203.0.113.7", h, std::slice::from_ref(&same)),
        Plan::Have(same.clone())
    );
    let other = rec("5", "A", h, "198.51.100.9", None);
    let cname = rec("6", "CNAME", h, "elsewhere.example.net", None);
    let v6 = rec("7", "AAAA", h, "2001:db8::1", None);
    assert_eq!(
        plan("A", "203.0.113.7", h, std::slice::from_ref(&other)),
        Plan::Conflict(vec![other.clone()])
    );
    assert_eq!(
        plan("A", "203.0.113.7", h, std::slice::from_ref(&cname)),
        Plan::Conflict(vec![cname.clone()])
    );
    // An AAAA elsewhere sends IPv6 visitors elsewhere: in the way too.
    assert_eq!(
        plan("A", "203.0.113.7", h, &[same.clone(), v6.clone()]),
        Plan::Conflict(vec![v6])
    );
}

#[tokio::test]
async fn creates_the_record_when_the_name_is_free() {
    let cf = Fake::new(&[("z1", "example.com", Dns::Edit)]);
    let o = ensure(
        &cf,
        "z1",
        "shop.example.com",
        "203.0.113.7",
        false,
        false,
        MARK,
    )
    .await
    .unwrap();
    assert_eq!(o.status, "created");
    let r = o.record.unwrap();
    assert_eq!(
        (r.kind.as_str(), r.content.as_str(), r.proxied),
        ("A", "203.0.113.7", false)
    );
    assert_eq!(r.comment.as_deref(), Some(MARK));
    assert_eq!(cf.at("z1").len(), 1);
}

#[tokio::test]
async fn skips_a_record_already_pointing_there() {
    let theirs = rec("x1", "A", "shop.example.com", "203.0.113.7", None);
    let cf = Fake::new(&[("z1", "example.com", Dns::Edit)]).with("z1", theirs.clone());
    let o = ensure(
        &cf,
        "z1",
        "shop.example.com",
        "203.0.113.7",
        true,
        false,
        MARK,
    )
    .await
    .unwrap();
    assert_eq!(o.status, "exists", "someone else's: not Pyo's to delete");
    assert_eq!(o.record, Some(theirs));
    assert!(!cf.calls().iter().any(|c| c.starts_with("create")));

    // Pyo's own from before (the same domain added again): still Pyo's.
    let mine = rec("m1", "A", "shop.example.com", "203.0.113.7", Some(MARK));
    let cf = Fake::new(&[("z1", "example.com", Dns::Edit)]).with("z1", mine);
    let o = ensure(
        &cf,
        "z1",
        "shop.example.com",
        "203.0.113.7",
        false,
        false,
        MARK,
    )
    .await
    .unwrap();
    assert_eq!(o.status, "created");
    // Another installation's isn't.
    let theirs = rec(
        "m2",
        "A",
        "shop.example.com",
        "203.0.113.7",
        Some("Made by Pyo (other)"),
    );
    let cf = Fake::new(&[("z1", "example.com", Dns::Edit)]).with("z1", theirs);
    let o = ensure(
        &cf,
        "z1",
        "shop.example.com",
        "203.0.113.7",
        false,
        false,
        MARK,
    )
    .await
    .unwrap();
    assert_eq!(o.status, "exists");
}

#[tokio::test]
async fn never_overwrites_without_being_asked() {
    let old = rec("x1", "CNAME", "shop.example.com", "old.example.net", None);
    let cf = Fake::new(&[("z1", "example.com", Dns::Edit)]).with("z1", old.clone());
    let o = ensure(
        &cf,
        "z1",
        "shop.example.com",
        "203.0.113.7",
        false,
        false,
        MARK,
    )
    .await
    .unwrap();
    assert_eq!(o.status, "conflict");
    assert_eq!(o.existing, vec![old.clone()]);
    assert_eq!(cf.at("z1"), vec![old.clone()], "untouched");
    assert!(
        !cf.calls()
            .iter()
            .any(|c| c.starts_with("create") || c.starts_with("delete"))
    );

    // Asked to replace: the record in the way goes, Pyo's comes.
    let o = ensure(
        &cf,
        "z1",
        "shop.example.com",
        "203.0.113.7",
        false,
        true,
        MARK,
    )
    .await
    .unwrap();
    assert_eq!(o.status, "created");
    assert_eq!(o.replaced, vec![old]);
    let now = cf.at("z1");
    assert_eq!(now.len(), 1);
    assert_eq!(
        (now[0].kind.as_str(), now[0].content.as_str()),
        ("A", "203.0.113.7")
    );
}

#[tokio::test]
async fn replacing_keeps_a_matching_record_and_other_types() {
    let same = rec("s", "A", "shop.example.com", "203.0.113.7", None);
    let v6 = rec("v", "AAAA", "shop.example.com", "2001:db8::1", None);
    let mx = rec("m", "MX", "shop.example.com", "mail.example.com", None);
    let cf = Fake::new(&[("z1", "example.com", Dns::Edit)])
        .with("z1", same.clone())
        .with("z1", v6)
        .with("z1", mx.clone());
    let o = ensure(
        &cf,
        "z1",
        "shop.example.com",
        "203.0.113.7",
        false,
        true,
        MARK,
    )
    .await
    .unwrap();
    assert_eq!(o.status, "exists");
    assert_eq!(cf.at("z1"), vec![same, mx]);
}

#[tokio::test]
async fn a_token_that_cannot_edit_fails_with_the_reason() {
    let cf = Fake::new(&[("z1", "example.com", Dns::Read)]);
    let e = ensure(
        &cf,
        "z1",
        "shop.example.com",
        "203.0.113.7",
        false,
        false,
        MARK,
    )
    .await
    .unwrap_err();
    assert_eq!(
        e.to_string(),
        "Cloudflare refused the token: Authentication error"
    );
}

#[tokio::test]
async fn deletes_only_pyos_own_records() {
    let mine = rec("m1", "A", "shop.example.com", "203.0.113.7", Some(MARK));
    let theirs = rec("t1", "A", "blog.example.com", "203.0.113.7", None);
    let edited = rec(
        "e1",
        "A",
        "api.example.com",
        "203.0.113.7",
        Some("changed by hand"),
    );
    let other_install = rec(
        "o1",
        "A",
        "app.example.com",
        "203.0.113.7",
        Some("Made by Pyo (zzz)"),
    );
    let cf = Fake::new(&[("z1", "example.com", Dns::Edit)])
        .with("z1", mine)
        .with("z1", theirs.clone())
        .with("z1", edited.clone())
        .with("z1", other_install.clone());
    assert!(
        !delete_ours(&cf, "z1", "t1", "blog.example.com", MARK)
            .await
            .unwrap()
    );
    assert!(
        !delete_ours(&cf, "z1", "e1", "api.example.com", MARK)
            .await
            .unwrap()
    );
    assert!(
        !delete_ours(&cf, "z1", "o1", "app.example.com", MARK)
            .await
            .unwrap()
    );
    // The id of Pyo's record, but for another hostname: no.
    assert!(
        !delete_ours(&cf, "z1", "m1", "blog.example.com", MARK)
            .await
            .unwrap()
    );
    assert!(
        delete_ours(&cf, "z1", "m1", "shop.example.com", MARK)
            .await
            .unwrap()
    );
    assert_eq!(cf.at("z1"), vec![theirs, edited, other_install]);
    // Gone already: nothing to do.
    assert!(
        delete_ours(&cf, "z1", "m1", "shop.example.com", MARK)
            .await
            .unwrap()
    );
    assert_eq!(
        cf.calls()
            .iter()
            .filter(|c| c.starts_with("delete"))
            .count(),
        1
    );
}

#[test]
fn proxied_once_the_certificate_needs_no_direct_check() {
    let d = |mode: &str, tls: &str, host: &str| {
        proxy_now(&json!({ "hostname": host, "tlsMode": mode, "tls": tls }))
    };
    assert!(
        !d("auto", "pending", "shop.example.com"),
        "Let's Encrypt first"
    );
    assert!(d("auto", "issued", "shop.example.com"));
    assert!(!d("letsencrypt", "failed", "shop.example.com"));
    assert!(d("off", "disabled", "shop.example.com"));
    assert!(d("custom", "pending", "shop.example.com"));
    assert!(d("selfsigned", "pending", "shop.example.com"));
}

// ── Who may ────────────────────────────────────────────────────────────────

#[test]
fn only_owners_and_admins_manage_the_token() {
    assert_eq!(role_for(Action::ManageToken), Role::Admin);
    assert_eq!(role_for(Action::ReplaceRecord), Role::Admin);
    assert_eq!(role_for(Action::CreateRecord), Role::Developer);
    assert_eq!(role_for(Action::DeleteRecord), Role::Developer);
    assert_eq!(role_for(Action::ListZones), Role::Viewer);
    let row = json!({
        "teamId": "team_a", "zones": [{ "id": "z1", "name": "example.com", "canEdit": true }],
        "hint": "…wxyz", "createdBy": "Ada", "createdAt": 1, "testedAt": 2,
    });
    for r in [Role::Viewer, Role::Developer] {
        let v = shown(&row, r);
        assert!(v.get("hint").is_none() && v.get("createdBy").is_none());
        assert_eq!(v["zones"], row["zones"]);
    }
    for r in [Role::Admin, Role::Owner] {
        assert_eq!(shown(&row, r)["hint"], json!("…wxyz"));
    }
    // Never the token, for anyone.
    assert!(!shown(&row, Role::Owner).to_string().contains("token"));
}

#[test]
fn tokens() {
    assert!(token_ok("abcdefghijklmnopqrstuvwxyz0123456789_-AB"));
    assert!(!token_ok("short"));
    assert!(!token_ok("abcdefghijklmnopqrstuvwxyz 0123456789"));
    assert!(!token_ok("abcdefghijklmnopqrstuvwxyz\n0123456789"));
    assert_eq!(hint("abcdefghijklmnopqrstuvwxyz0123456789_-AB"), "…_-AB");
}

#[test]
fn the_client_never_prints_its_token() {
    let h = Http::new("secret-token-value-1234567890").unwrap();
    assert!(!format!("{h:?}").contains("secret"));
}

#[tokio::test]
async fn the_client_only_calls_cloudflare() {
    let h = Http::new("secret-token-value-1234567890").unwrap();
    let e = h
        .call(
            Method::GET,
            Url::parse("https://example.com/x").unwrap(),
            None,
        )
        .await
        .unwrap_err();
    assert_eq!(e, CfError::Unreachable);
    assert!(Http::record_path("../zones", None).is_err());
    assert!(Http::record_path("z1", Some("a/b")).is_err());
    assert_eq!(
        Http::record_path(
            "023e105f4ecef8ad9ca31a8372d0c353",
            Some("372e67954025e0ba6aaa6d586b9e0b59")
        )
        .unwrap(),
        "/zones/023e105f4ecef8ad9ca31a8372d0c353/dns_records/372e67954025e0ba6aaa6d586b9e0b59"
    );
}

#[test]
fn reads_cloudflares_answers() {
    // As Cloudflare documents its envelope and record.
    let v = json!({
        "success": false,
        "errors": [{ "code": 9109, "message": "Invalid access token" }],
        "messages": [], "result": null,
    });
    assert_eq!(message(&v), "Invalid access token");
    assert!(token_code(&v));
    assert!(!token_code(
        &json!({ "errors": [{ "code": 81057, "message": "Record already exists." }] })
    ));
    let r = record_from(&json!({
        "id": "372e67954025e0ba6aaa6d586b9e0b59", "type": "A", "name": "shop.example.com",
        "content": "203.0.113.7", "proxied": true, "ttl": 1, "comment": "Made by Pyo (abc123)",
        "tags": [], "created_on": "2026-01-01T00:00:00Z",
    }))
    .unwrap();
    assert!(ours(&r, MARK, "shop.example.com"));
    assert!(r.proxied);
    let no_comment = record_from(&json!({
        "id": "1", "type": "A", "name": "a.example.com", "content": "203.0.113.7", "proxied": false, "comment": null,
    }))
    .unwrap();
    assert!(!ours(&no_comment, MARK, "a.example.com"));
}

impl tunnel::TunnelApi for Fake {
    async fn zone_accounts(&self) -> Result<Vec<CfZone>, CfError> {
        Ok(self
            .zone_list()?
            .into_iter()
            .map(|(id, name)| {
                let (account_id, account_name) = self.account_of(&id);
                CfZone {
                    id,
                    name,
                    account_id,
                    account_name,
                }
            })
            .collect())
    }

    async fn records_kept(&self, zone_id: &str, name: &str) -> Result<Vec<Kept>, CfError> {
        let ttls = self.ttls.lock().unwrap().clone();
        Ok(self
            .records(zone_id, name)
            .await?
            .into_iter()
            .map(|r| Kept {
                ttl: ttls.get(&r.id).copied().unwrap_or(1),
                id: r.id,
                kind: r.kind,
                name: r.name,
                content: r.content,
                proxied: r.proxied,
                comment: r.comment,
            })
            .collect())
    }

    async fn create_kept(&self, zone_id: &str, r: &Kept) -> Result<Record, CfError> {
        self.log(format!(
            "create {zone_id} {} {} {}",
            r.kind, r.name, r.content
        ));
        if self.dns(zone_id) != Dns::Edit {
            return Err(Self::denied());
        }
        let made = Record {
            id: self.id("r"),
            ..r.record()
        };
        self.ttls.lock().unwrap().insert(made.id.clone(), r.ttl);
        self.records
            .lock()
            .unwrap()
            .push((zone_id.into(), made.clone()));
        Ok(made)
    }

    async fn tunnels_named(&self, account: &str, name: &str) -> Result<Vec<Tunnel>, CfError> {
        self.log(format!("tunnels {account} {name}"));
        if self.no_tunnels.iter().any(|a| a == account) {
            return Err(Self::denied());
        }
        Ok(self
            .tunnels
            .lock()
            .unwrap()
            .iter()
            .filter(|(a, t)| a == account && t.name.starts_with(name) && !t.deleted)
            .map(|(_, t)| t.clone())
            .collect())
    }

    async fn create_tunnel(&self, account: &str, name: &str) -> Result<Tunnel, CfError> {
        self.log(format!("create tunnel {account} {name}"));
        if self.no_tunnels.iter().any(|a| a == account) {
            return Err(Self::denied());
        }
        let t = Tunnel {
            id: self.uuid(),
            name: name.into(),
            status: "inactive".into(),
            created_at: jiff::Timestamp::now().to_string(),
            ..Default::default()
        };
        self.tunnels
            .lock()
            .unwrap()
            .push((account.into(), t.clone()));
        self.secrets
            .lock()
            .unwrap()
            .insert(t.id.clone(), "first".into());
        Ok(t)
    }

    async fn tunnel(&self, account: &str, id: &str) -> Result<Tunnel, CfError> {
        self.tunnels
            .lock()
            .unwrap()
            .iter()
            .find(|(a, t)| a == account && t.id == id)
            .map(|(_, t)| t.clone())
            .ok_or(CfError::NotFound)
    }

    async fn delete_tunnel(&self, account: &str, id: &str) -> Result<(), CfError> {
        self.log(format!("delete tunnel {id}"));
        let mut ts = self.tunnels.lock().unwrap();
        let Some((_, t)) = ts.iter_mut().find(|(a, t)| a == account && t.id == id) else {
            return Err(CfError::NotFound);
        };
        if !t.connections.is_empty() {
            return Err(CfError::Refused(
                "Cannot delete tunnel because it has active connections".into(),
            ));
        }
        t.deleted = true;
        Ok(())
    }

    async fn tunnel_token(&self, account: &str, id: &str) -> Result<String, CfError> {
        self.tunnel(account, id).await?;
        let secret = self
            .secrets
            .lock()
            .unwrap()
            .get(id)
            .cloned()
            .unwrap_or_default();
        Ok(format!("token-{id}-{secret}"))
    }

    async fn put_config(&self, account: &str, id: &str, config: &Value) -> Result<i64, CfError> {
        self.log(format!("put config {id}"));
        self.tunnel(account, id).await?;
        let mut cs = self.configs.lock().unwrap();
        let v = cs.get(id).map_or(1, |(v, _)| v + 1);
        cs.insert(id.into(), (v, config.clone()));
        Ok(v)
    }

    async fn config(&self, account: &str, id: &str) -> Result<(i64, Value), CfError> {
        self.tunnel(account, id).await?;
        Ok(self
            .configs
            .lock()
            .unwrap()
            .get(id)
            .cloned()
            .unwrap_or((0, Value::Null)))
    }

    async fn rotate_secret(&self, account: &str, id: &str, secret: &str) -> Result<(), CfError> {
        self.log(format!("rotate {id}"));
        self.tunnel(account, id).await?;
        self.secrets
            .lock()
            .unwrap()
            .insert(id.into(), secret.into());
        Ok(())
    }

    async fn drop_connections(&self, account: &str, id: &str) -> Result<(), CfError> {
        self.log(format!("drop connections {id}"));
        self.tunnel(account, id).await?;
        self.connect(id, "inactive", &[]);
        Ok(())
    }

    async fn origin_certs(&self, zone_id: &str) -> Result<Vec<String>, CfError> {
        if self.no_certs.iter().any(|z| z == zone_id) {
            return Err(Self::denied());
        }
        Ok(vec![])
    }

    async fn issue_origin_cert(
        &self,
        csr_pem: &str,
        hostnames: &[String],
    ) -> Result<OriginCert, CfError> {
        self.log(format!("issue {}", hostnames.join(",")));
        let zone_denied = self.zones.iter().any(|(id, name, _)| {
            self.no_certs.contains(id)
                && hostnames
                    .iter()
                    .any(|h| h.trim_start_matches("*.").ends_with(name.as_str()))
        });
        if zone_denied {
            return Err(Self::denied());
        }
        assert!(csr_pem.contains("CERTIFICATE REQUEST"), "{csr_pem}");
        let id = format!("{}", 1000 + self.certs.lock().unwrap().len());
        self.certs
            .lock()
            .unwrap()
            .push((id.clone(), hostnames.to_vec(), false));
        Ok(OriginCert {
            id,
            pem: String::new(),
        })
    }

    async fn revoke_origin_cert(&self, id: &str) -> Result<(), CfError> {
        self.log(format!("revoke {id}"));
        for c in self.certs.lock().unwrap().iter_mut() {
            if c.0 == id {
                c.2 = true;
                return Ok(());
            }
        }
        Err(CfError::NotFound)
    }
}

// ── Tunnel permissions ─────────────────────────────────────────────────────

#[tokio::test]
async fn a_test_says_which_zones_can_use_a_tunnel() {
    let mut cf = Fake::new(&[
        ("z1", "example.com", Dns::Edit),
        ("z2", "other.org", Dns::Edit),
        ("z3", "certless.dev", Dns::Edit),
        ("z4", "readonly.io", Dns::Read),
    ]);
    cf.accounts
        .insert("z2".into(), ("acct2".into(), "Other Co".into()));
    cf.no_tunnels = vec!["acct2".into()];
    cf.no_certs = vec!["z2".into(), "z3".into()];
    let zones = inspect(&cf, INSTALL).await.unwrap();
    let got: Vec<(&str, &str, bool, Vec<String>)> = zones
        .iter()
        .map(|z| {
            (
                z.name.as_str(),
                z.account_id.as_str(),
                z.tunnel_ready(),
                z.tunnel.missing.clone(),
            )
        })
        .collect();
    assert_eq!(
        got,
        [
            ("certless.dev", "acct1", false, vec![tunnel::CERTS_EDIT.to_owned()]),
            ("example.com", "acct1", true, vec![]),
            (
                "other.org",
                "acct2",
                false,
                vec![tunnel::TUNNEL_EDIT.to_owned(), tunnel::CERTS_EDIT.to_owned()]
            ),
            ("readonly.io", "acct1", false, vec![]),
        ]
    );
    assert_eq!(zones[1].account_name, "Acme");
    // A zone that can't edit DNS can't be tunnel-ready either.
    assert!(!zones[3].tunnel.ready);
    // DNS only (neither tunnel permission) is no problem. Half of them is.
    assert_eq!(
        problems(&zones),
        [
            "readonly.io: give the token DNS:Edit.",
            "certless.dev: give the token SSL and Certificates:Edit."
        ]
    );
    // The tunnel check is once per account, and leaves nothing behind.
    let checks = cf
        .calls()
        .iter()
        .filter(|c| c.starts_with("create tunnel acct1 pyo-check-abc123-"))
        .count();
    assert_eq!(checks, 1);
    assert!(cf.tunnel_list().iter().all(|t| t.deleted));
}

#[tokio::test]
async fn only_this_installations_old_probe_tunnels_are_deleted() {
    let cf = Fake::new(&[("z1", "example.com", Dns::Edit)]);
    let old = (jiff::Timestamp::now() - jiff::SignedDuration::from_mins(30)).to_string();
    let now = jiff::Timestamp::now().to_string();
    let put = |n: u32, name: &str, made: &str| {
        cf.tunnels.lock().unwrap().push((
            "acct1".into(),
            Tunnel {
                id: format!("11111111-2222-4333-8444-{n:012}"),
                name: name.into(),
                created_at: made.into(),
                ..Default::default()
            },
        ));
    };
    put(1, "pyo-check-abc123-a1b2c3", &old);
    put(2, "pyo-check-abc123-d4e5f6", &now);
    // Another installation's probe, and people's own tunnels: never touched,
    // however old.
    put(3, "pyo-check-zzz999-a1b2c3", &old);
    put(4, "pyo-check-mine", &old);
    put(5, "pyo-check-abc123-mine", &old);
    put(6, "pyo-check-a1b2c3", &old);
    put(7, "home-lab", &old);
    // One with a date Pyo can't read stays too.
    put(8, "pyo-check-abc123-0a0b0c", "");
    inspect(&cf, INSTALL).await.unwrap();
    let left: Vec<String> = cf
        .tunnel_list()
        .into_iter()
        .filter(|t| !t.deleted)
        .map(|t| t.name)
        .collect();
    assert_eq!(
        left,
        [
            "pyo-check-abc123-d4e5f6",
            "pyo-check-zzz999-a1b2c3",
            "pyo-check-mine",
            "pyo-check-abc123-mine",
            "pyo-check-a1b2c3",
            "home-lab",
            "pyo-check-abc123-0a0b0c",
        ]
    );
}

#[test]
fn zones_stored_before_the_tunnel_read_as_dns_only() {
    let z: Zone =
        serde_json::from_value(json!({ "id": "z1", "name": "example.com", "canEdit": true })).unwrap();
    assert!(!z.tunnel_ready());
    assert_eq!(z.account_id, "");
    let v = serde_json::to_value(zone("z1", "example.com")).unwrap();
    assert_eq!(v["accountId"], "acct1");
    assert_eq!(v["tunnel"], json!({ "ready": true }));
}

#[test]
fn tunnel_ids_are_uuids_before_they_go_into_paths() {
    use tunnel::uuid_ok;
    assert!(uuid_ok("c1744f8b-faa1-48a4-9e5c-02ac921467fa"));
    assert!(!uuid_ok("c1744f8b-faa1-48a4-9e5c-02ac921467f"));
    assert!(!uuid_ok("../../zones/x-faa1-48a4-9e5c-02ac921467fa"));
    assert!(!uuid_ok("c1744f8bxfaa1-48a4-9e5c-02ac921467fa"));
}

#[test]
fn reads_cloudflares_tunnels() {
    let t = tunnel::tunnel_from(&json!({
        "id": "c1744f8b-faa1-48a4-9e5c-02ac921467fa", "name": "pyo-cl_1-abc123",
        "status": "healthy", "deleted_at": null, "account_tag": "699d98642c564d2e855e9661899b7252",
        "connections": [{
            "colo_name": "FRA", "uuid": "1bedc50d-42b3-473c-b108-ff3d10c0d925", "id": "1bedc50d-42b3-473c-b108-ff3d10c0d925",
            "is_pending_reconnect": false, "origin_ip": "203.0.113.7", "opened_at": "2026-11-22T10:00:00Z",
            "client_id": "1bedc50d-42b3-473c-b108-ff3d10c0d925", "client_version": "2026.10.7",
        }],
    }))
    .unwrap();
    assert_eq!(t.status, "healthy");
    assert!(!t.deleted);
    assert_eq!(t.connections[0].colo, "FRA");
    let gone = tunnel::tunnel_from(&json!({
        "id": "c1744f8b-faa1-48a4-9e5c-02ac921467fa", "name": "x", "deleted_at": "2026-11-22T10:00:00Z",
    }))
    .unwrap();
    assert!(gone.deleted);
    assert!(tunnel::tunnel_from(&json!({ "id": "../x" })).is_err());
}
