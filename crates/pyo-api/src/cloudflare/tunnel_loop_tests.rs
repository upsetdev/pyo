//! The tunnel loop's steps against the fake Cloudflare account: the tunnel
//! made, taken back or made again, records switched and put back exactly,
//! only Pyo's own records deleted, the certificate issued, and the health
//! it shows.

use serde_json::json;

use super::*;
use crate::cloudflare::tests::{Dns, Fake, MARK, rec};

const TARGET: &str = "00000000-0000-4000-8000-000000000001.cfargotunnel.com";

fn row() -> Row {
    Row {
        cluster_id: "cl_1".into(),
        team_id: "team_1".into(),
        account_id: "acct1".into(),
        tunnel_name: "pyo-cl_1-abc123".into(),
        state: "creating".into(),
        ..Default::default()
    }
}

#[tokio::test]
async fn a_tunnel_is_made_taken_back_or_made_again() {
    let cf = Fake::new(&[("z1", "example.com", Dns::Edit)]);
    let mut r = row();
    let (t, again) = ensure_tunnel(&cf, &r, "abc123").await.unwrap();
    assert_eq!(t.name, "pyo-cl_1-abc123");
    assert!(!again);
    // A pass that stopped before keeping its id: the same tunnel, not a second.
    let (same, _) = ensure_tunnel(&cf, &r, "abc123").await.unwrap();
    assert_eq!(same.id, t.id);
    assert_eq!(cf.tunnel_list().len(), 1);
    // Kept: read, not made.
    r.tunnel_id = Some(t.id.clone());
    let (kept, again) = ensure_tunnel(&cf, &r, "abc123").await.unwrap();
    assert_eq!((kept.id.as_str(), again), (t.id.as_str(), false));
    // Deleted by hand in Cloudflare: a new one, with a name of its own.
    cf.tunnels.lock().unwrap()[0].1.deleted = true;
    let (new, again) = ensure_tunnel(&cf, &r, "abc123").await.unwrap();
    assert!(again);
    assert_ne!(new.id, t.id);
    // Its name is free again once the old one is deleted.
    assert_eq!(new.name, "pyo-cl_1-abc123");
    // Taken by a live one (a pass that stopped halfway): the next name.
    r.tunnel_id = Some("00000000-0000-4000-8000-00000000ffff".into());
    let (third, again) = ensure_tunnel(&cf, &r, "abc123").await.unwrap();
    assert!(again);
    assert_eq!(third.name, "pyo-cl_1-abc123-2");
}

#[tokio::test]
async fn someone_elses_tunnel_is_never_taken() {
    let cf = Fake::new(&[("z1", "example.com", Dns::Edit)]);
    cf.tunnels.lock().unwrap().push((
        "acct1".into(),
        tunnel::Tunnel {
            id: "11111111-2222-4333-8444-555555555555".into(),
            name: "pyo-cl_1-zzz999".into(),
            ..Default::default()
        },
    ));
    let (t, _) = ensure_tunnel(&cf, &row(), "abc123").await.unwrap();
    assert_eq!(t.name, "pyo-cl_1-abc123");
    assert_ne!(t.id, "11111111-2222-4333-8444-555555555555");
}

#[tokio::test]
async fn a_free_name_switches_without_asking() {
    let cf = Fake::new(&[("z1", "example.com", Dns::Edit)]);
    let out = switch_record(&cf, "z1", "blog.example.com", TARGET, false, MARK)
        .await
        .unwrap();
    let Switched::Done { record, previous } = out else {
        panic!("{out:?}")
    };
    assert_eq!(
        (record.kind.as_str(), record.content.as_str(), record.proxied),
        ("CNAME", TARGET, true)
    );
    assert_eq!(record.comment.as_deref(), Some(MARK));
    assert!(previous.is_empty());
    // Again: nothing more to do.
    let again = switch_record(&cf, "z1", "blog.example.com", TARGET, false, MARK)
        .await
        .unwrap();
    assert!(matches!(again, Switched::Done { ref previous, .. } if previous.is_empty()));
    assert_eq!(cf.at("z1").len(), 1);
}

#[tokio::test]
async fn pyos_own_direct_record_goes_without_asking_and_isnt_kept() {
    let mine = rec("m1", "A", "blog.example.com", "203.0.113.7", Some(MARK));
    let cf = Fake::new(&[("z1", "example.com", Dns::Edit)]).with("z1", mine);
    let out = switch_record(&cf, "z1", "blog.example.com", TARGET, false, MARK)
        .await
        .unwrap();
    assert!(matches!(out, Switched::Done { ref previous, .. } if previous.is_empty()));
    let now = cf.at("z1");
    assert_eq!(now.len(), 1);
    assert_eq!(now[0].kind, "CNAME");
}

#[tokio::test]
async fn someone_elses_record_asks_first_then_is_kept_and_put_back_exactly() {
    let mut theirs = rec("t1", "A", "blog.example.com", "198.51.100.9", Some("by hand"));
    theirs.proxied = true;
    let mx = rec("x1", "MX", "blog.example.com", "mail.example.com", None);
    let other = rec("o1", "A", "shop.example.com", "198.51.100.9", None);
    let cf = Fake::new(&[("z1", "example.com", Dns::Edit)])
        .with("z1", theirs.clone())
        .with("z1", mx.clone())
        .with("z1", other.clone());
    cf.ttls.lock().unwrap().insert("t1".into(), 300);
    // Asked first: nothing changes.
    let out = switch_record(&cf, "z1", "blog.example.com", TARGET, false, MARK)
        .await
        .unwrap();
    assert_eq!(out, Switched::Confirm(vec![theirs.clone()]));
    assert_eq!(cf.at("z1").len(), 3);
    // Replaced: kept as it was, every field.
    let Switched::Done { record, previous } =
        switch_record(&cf, "z1", "blog.example.com", TARGET, true, MARK)
            .await
            .unwrap()
    else {
        panic!("not switched")
    };
    assert_eq!(previous.len(), 1);
    let k = &previous[0];
    assert_eq!(
        (k.kind.as_str(), k.content.as_str(), k.proxied, k.ttl, k.comment.as_deref()),
        ("A", "198.51.100.9", true, 300, Some("by hand"))
    );
    // Other types and names stay.
    let names: Vec<String> = cf.at("z1").iter().map(|r| r.id.clone()).collect();
    assert!(names.contains(&"x1".to_owned()) && names.contains(&"o1".to_owned()));
    // Switch back: Pyo's CNAME goes, the kept record comes back as it was.
    let made = revert_record(&cf, "z1", "blog.example.com", Some(&record.id), &previous, Some("203.0.113.7"), MARK)
        .await
        .unwrap();
    assert_eq!(made, None, "a kept record wins over the public IP");
    let back: Vec<Record> = cf
        .at("z1")
        .into_iter()
        .filter(|r| r.name == "blog.example.com" && r.kind != "MX")
        .collect();
    assert_eq!(back.len(), 1);
    assert_eq!(
        (back[0].kind.as_str(), back[0].content.as_str(), back[0].proxied, back[0].comment.as_deref()),
        ("A", "198.51.100.9", true, Some("by hand"))
    );
    assert_eq!(cf.ttls.lock().unwrap().get(&back[0].id), Some(&300));
}

#[tokio::test]
async fn switching_back_without_a_kept_record() {
    let cf = Fake::new(&[("z1", "example.com", Dns::Edit)]);
    let Switched::Done { record, .. } =
        switch_record(&cf, "z1", "blog.example.com", TARGET, false, MARK)
            .await
            .unwrap()
    else {
        panic!("not switched")
    };
    // Public ports: Pyo's own A record to the cluster.
    let made = revert_record(&cf, "z1", "blog.example.com", Some(&record.id), &[], Some("203.0.113.7"), MARK)
        .await
        .unwrap()
        .expect("an A record");
    assert_eq!((made.kind.as_str(), made.content.as_str()), ("A", "203.0.113.7"));
    assert_eq!(made.comment.as_deref(), Some(MARK));
    assert_eq!(cf.at("z1").len(), 1);
    // Neither: the name is left empty.
    let cf = Fake::new(&[("z1", "example.com", Dns::Edit)]);
    let Switched::Done { record, .. } =
        switch_record(&cf, "z1", "blog.example.com", TARGET, false, MARK)
            .await
            .unwrap()
    else {
        panic!("not switched")
    };
    assert_eq!(
        revert_record(&cf, "z1", "blog.example.com", Some(&record.id), &[], None, MARK)
            .await
            .unwrap(),
        None
    );
    assert!(cf.at("z1").is_empty());
}

#[tokio::test]
async fn a_cname_changed_by_hand_is_left_when_switching_back() {
    let cf = Fake::new(&[("z1", "example.com", Dns::Edit)]);
    let Switched::Done { record, .. } =
        switch_record(&cf, "z1", "blog.example.com", TARGET, false, MARK)
            .await
            .unwrap()
    else {
        panic!("not switched")
    };
    // Someone took the comment off: no longer Pyo's.
    for (_, r) in cf.records.lock().unwrap().iter_mut() {
        r.comment = Some("mine now".into());
    }
    revert_record(&cf, "z1", "blog.example.com", Some(&record.id), &[], None, MARK)
        .await
        .unwrap();
    assert_eq!(cf.at("z1").len(), 1, "not Pyo's to delete");
}

#[tokio::test]
async fn a_cname_to_an_old_tunnel_is_repointed() {
    let old = rec("c1", "CNAME", "blog.example.com", "old.cfargotunnel.com", Some(MARK));
    let cf = Fake::new(&[("z1", "example.com", Dns::Edit)]).with("z1", old);
    let out = switch_record(&cf, "z1", "blog.example.com", TARGET, false, MARK)
        .await
        .unwrap();
    assert!(matches!(out, Switched::Done { .. }));
    let now = cf.at("z1");
    assert_eq!(now.len(), 1);
    assert_eq!(now[0].content, TARGET);
}

#[tokio::test]
async fn the_certificate_is_issued_for_the_names_with_a_key_kept_here() {
    let cf = Fake::new(&[("z1", "example.com", Dns::Edit)]);
    let names = vec!["*.example.com".to_owned(), "example.com".to_owned()];
    let (cert, key) = issue(&cf, &names).await.unwrap();
    assert!(key.contains("PRIVATE KEY"));
    let issued = cf.certs.lock().unwrap().clone();
    assert_eq!(issued, vec![(cert.id.clone(), names.clone(), false)]);
    // Without SSL and Certificates:Edit: said plainly.
    let mut cf = Fake::new(&[("z1", "example.com", Dns::Edit)]);
    cf.no_certs = vec!["z1".into()];
    let e = issue(&cf, &names).await.unwrap_err();
    assert_eq!(
        e.to_string(),
        "Cloudflare refused the token: give the token SSL and Certificates:Edit on the tunnel's zones."
    );
}

#[test]
fn health_from_both_views() {
    let agent = |w: i64, r: i64| json!({ "replicas": w, "readyReplicas": r });
    // No read of Cloudflare's view yet: starting while it's new, then Pyo
    // can't reach Cloudflare (a pass failed, or none read it in time).
    assert_eq!(health(&json!({}), &json!({}), true, false), "starting");
    assert_eq!(health(&json!({}), &agent(2, 2), true, true), "unknown");
    assert_eq!(health(&json!({}), &agent(2, 2), false, false), "unknown");
    assert_eq!(health(&json!({ "status": "healthy" }), &agent(2, 2), false, false), "connected");
    assert_eq!(health(&json!({ "status": "healthy" }), &agent(2, 2), true, true), "connected");
    assert_eq!(health(&json!({ "status": "healthy" }), &agent(2, 1), false, false), "degraded");
    assert_eq!(health(&json!({ "status": "degraded" }), &agent(2, 2), false, false), "degraded");
    assert_eq!(health(&json!({ "status": "inactive" }), &agent(2, 0), true, false), "starting");
    assert_eq!(health(&json!({ "status": "down" }), &agent(2, 0), false, false), "down");
    assert_eq!(health(&json!({ "status": "inactive" }), &json!({}), false, false), "down");
}

#[test]
fn a_new_tunnel_shows_starting_until_cloudflare_is_read() {
    let row = |young: bool, error: Value| {
        json!({ "state": "creating", "error": error, "cfStatus": {}, "agentStatus": {}, "young": young })
    };
    // Just turned on: starting, never "can't reach Cloudflare".
    assert_eq!(shown(&row(true, Value::Null))["health"], "starting");
    // Pyo tried and Cloudflare said no, or no read came in 5 minutes.
    assert_eq!(shown(&row(true, json!("Cloudflare refused the token.")))["health"], "unknown");
    assert_eq!(shown(&row(false, Value::Null))["health"], "unknown");
    // Being removed: says so, whatever Cloudflare said.
    let mut r = row(true, Value::Null);
    r["state"] = json!("removing");
    assert_eq!(shown(&r)["health"], "removing");
}

#[test]
fn connectors_say_why_in_plain_words() {
    let pods = json!([
        // An older agent sent Kubernetes' own word.
        { "server": "server-1", "ready": false, "reason": "ImagePullBackOff" },
        { "server": "server-2", "ready": false, "reason": pyo_proto::KEEPS_STOPPING },
        { "server": "server-3", "ready": false, "reason": "OOMKilled" },
        { "server": "server-4", "ready": false, "reason": "ContainerCreating" },
        { "server": "server-5", "ready": true },
    ]);
    let row = json!({ "state": "ready", "error": null, "cfStatus": {}, "agentStatus": { "pods": pods } });
    let reasons: Vec<Value> = shown(&row)["connectors"].as_array().unwrap().iter().map(|c| c["reason"].clone()).collect();
    assert_eq!(
        reasons,
        [
            json!(pyo_proto::CANT_DOWNLOAD_IMAGE),
            json!(pyo_proto::KEEPS_STOPPING),
            json!(pyo_proto::NOT_RUNNING_YET),
            Value::Null,
            Value::Null,
        ]
    );
}

#[test]
fn domains_take_the_tunnel_certificate_and_give_it_back() {
    // Switching with a certificate already issued: kept, both paths answer.
    let mut d = json!({ "id": "dom_1", "hostname": "blog.example.com", "tlsMode": "auto", "tls": "issued", "tunnel": "switching" });
    assert!(!take_certificate(&mut d, "crt_t"));
    assert_eq!(d["tlsMode"], "auto");
    // Switched: the tunnel's, the old one kept.
    d["tunnel"] = json!("on");
    assert!(take_certificate(&mut d, "crt_t"));
    assert_eq!((d["tlsMode"].as_str(), d["certificateId"].as_str()), (Some("custom"), Some("crt_t")));
    assert_eq!(d["tlsModeBefore"], "auto");
    assert!(!take_certificate(&mut d, "crt_t"), "already");
    leave_tunnel(&mut d);
    assert_eq!(d["tlsMode"], "auto");
    assert!(d.get("certificateId").is_none() && d.get("tunnel").is_none() && d.get("tlsModeBefore").is_none());
    // Not issued yet (a new domain): the tunnel's at once, even while switching.
    let mut d = json!({ "id": "dom_2", "hostname": "new.example.com", "tlsMode": "auto", "tls": "pending", "tunnel": "switching" });
    assert!(take_certificate(&mut d, "crt_t"));
    // An uploaded one comes back with its own id.
    let mut d = json!({ "id": "dom_3", "hostname": "up.example.com", "tlsMode": "custom", "certificateId": "crt_mine", "tunnel": "on" });
    assert!(take_certificate(&mut d, "crt_t"));
    leave_tunnel(&mut d);
    assert_eq!((d["tlsMode"].as_str(), d["certificateId"].as_str()), (Some("custom"), Some("crt_mine")));
}

#[test]
fn a_domain_live_through_the_tunnel_drops_its_old_error() {
    // The direct DNS check's error from before the switch.
    let mut d = json!({ "id": "dom_1", "hostname": "blog.example.com", "status": "pending_dns", "tunnel": "on",
                        "error": "blog.example.com doesn't resolve yet. Point it at the cluster's ingress." });
    live(&mut d);
    assert_eq!(d["status"], "active");
    assert!(d.get("error").is_none(), "{d}");
}

#[test]
fn free_names_are_one_label_under_the_default_domain() {
    assert!(under_default("web-shop.example.com", "example.com"));
    assert!(under_default("Web-Shop.Example.com", "example.com"));
    assert!(!under_default("a.web-shop.example.com", "example.com"), "the wildcard covers one level");
    assert!(!under_default("example.com", "example.com"));
    assert!(!under_default("web.notexample.com", "example.com"));
    assert!(!under_default("web.example.com.evil.net", "example.com"));
}

#[test]
fn free_names_get_their_own_route_and_no_wildcard() {
    // A free name under the default domain is routed by name, like one
    // switched by hand. Nothing routes the rest of the default domain.
    let routes = tunnel::ingress(&["web-shop.example.com".into(), "blog.example.com".into()]);
    let names: Vec<&str> = routes["ingress"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|r| r["hostname"].as_str())
        .collect();
    assert_eq!(names, ["web-shop.example.com", "blog.example.com"]);
}
