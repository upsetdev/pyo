//! Zone matching, records made, found, in the way or replaced, deleting
//! only Pyo's own, who may do what, and the HTTP client's handling of
//! Cloudflare's answers. Cloudflare itself is a fake account.

use std::sync::Mutex;

use super::*;

const MARK: &str = "Made by Pyo (abc123)";

fn zone(id: &str, name: &str) -> Zone {
    Zone {
        id: id.into(),
        name: name.into(),
        can_edit: true,
        missing: None,
    }
}

fn rec(id: &str, kind: &str, name: &str, content: &str, comment: Option<&str>) -> Record {
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
enum Dns {
    None,
    Read,
    Edit,
}

/// A Cloudflare account: zones, the token's DNS permission in each, and
/// every call made.
struct Fake {
    zones: Vec<(String, String, Dns)>,
    zone_read: bool,
    records: Mutex<Vec<(String, Record)>>,
    calls: Mutex<Vec<String>>,
    next: Mutex<u32>,
}

impl Fake {
    fn new(zones: &[(&str, &str, Dns)]) -> Self {
        Self {
            zones: zones
                .iter()
                .map(|(i, n, d)| (i.to_string(), n.to_string(), *d))
                .collect(),
            zone_read: true,
            records: Mutex::new(vec![]),
            calls: Mutex::new(vec![]),
            next: Mutex::new(0),
        }
    }

    fn with(self, zone: &str, r: Record) -> Self {
        self.records.lock().unwrap().push((zone.into(), r));
        self
    }

    fn dns(&self, zone: &str) -> Dns {
        self.zones
            .iter()
            .find(|z| z.0 == zone)
            .map(|z| z.2)
            .unwrap_or(Dns::None)
    }

    fn log(&self, c: String) {
        self.calls.lock().unwrap().push(c);
    }

    fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }

    fn at(&self, zone: &str) -> Vec<Record> {
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

impl Cloudflare for Fake {
    async fn zones(&self) -> Result<Vec<(String, String)>, CfError> {
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
        let mut n = self.next.lock().unwrap();
        *n += 1;
        let made = Record {
            id: format!("r{n}"),
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
    let zones = inspect(&cf, MARK).await.unwrap();
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
            .contains(&"create z1 TXT _pyo-check.example.com \"pyo-check\"".to_string())
    );
}

#[tokio::test]
async fn a_token_without_zone_read_is_refused() {
    let mut cf = Fake::new(&[("z1", "example.com", Dns::Edit)]);
    cf.zone_read = false;
    let e = inspect(&cf, MARK).await.unwrap_err();
    assert!(matches!(e, CfError::Denied(_)));
    assert!(e.to_string().starts_with("Cloudflare refused the token"));
    let none = Fake::new(&[]);
    let zones = inspect(&none, MARK).await.unwrap();
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
            "\"pyo-check\"",
            Some(MARK),
        ),
    );
    inspect(&cf, MARK).await.unwrap();
    assert!(cf.at("z1").is_empty());
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
