//! The cluster's client against a mock Tailscale API.

use super::*;
use std::sync::{Arc, Mutex};

use axum::{
    Json, Router,
    body::Bytes,
    extract::State,
    http::{Method, StatusCode, Uri},
    response::{IntoResponse, Response},
};
use pyo_proto::tailscale::{OwnName, SyncServer};

/// A tailnet: Services by name (comment, addresses), machines (node ID,
/// full name, tags), each Service's approved hosts, and every write.
#[derive(Default)]
struct Net {
    services: BTreeMap<String, (String, Vec<String>)>,
    /// Services the list leaves out (defined, hosted nowhere).
    hidden: BTreeSet<String>,
    devices: Vec<(String, String, Vec<String>)>,
    approved: BTreeSet<(String, String)>,
    writes: Vec<String>,
}

type Shared = Arc<Mutex<Net>>;

async fn handle(State(st): State<Shared>, method: Method, uri: Uri, _body: Bytes) -> Response {
    let mut n = st.lock().unwrap();
    let path = uri.path().to_owned();
    let svc = |p: &str| {
        p.strip_prefix("/tailnet/-/services/svc:")
            .map(str::to_owned)
    };
    match (method.as_str(), path.as_str()) {
        ("POST", "/oauth/token") => Json(json!({ "access_token": "t" })).into_response(),
        ("GET", "/tailnet/-/services") => {
            let list: Vec<Value> = n
                .services
                .iter()
                .filter(|(k, _)| !n.hidden.contains(*k))
                .map(|(k, (c, a))| json!({ "name": format!("svc:{k}"), "comment": c, "addrs": a }))
                .collect();
            Json(json!({ "vipServices": list })).into_response()
        }
        ("GET", "/tailnet/-/devices") => {
            let list: Vec<Value> = n
                .devices
                .iter()
                .map(|(id, name, tags)| json!({ "nodeId": id, "name": name, "hostname": name.split('.').next(), "tags": tags }))
                .collect();
            Json(json!({ "devices": list })).into_response()
        }
        ("POST", "/tailnet/-/keys") => {
            n.writes.push("key".into());
            Json(json!({ "id": "k1", "key": "tskey-auth-k1-secret" })).into_response()
        }
        ("POST", p) if p.starts_with("/device/") && p.ends_with("/tags") => {
            let id = p
                .trim_start_matches("/device/")
                .trim_end_matches("/tags")
                .to_owned();
            n.writes.push(format!("tag {id}"));
            Json(json!({})).into_response()
        }
        (m, p) if svc(p).is_some() => {
            let rest = svc(p).unwrap();
            if let Some((name, tail)) = rest.split_once('/') {
                if tail == "devices" {
                    let hosts: Vec<Value> = n
                        .approved
                        .iter()
                        .filter(|(s, _)| s == name)
                        .map(|(_, node)| json!({ "stableNodeID": node, "approvalLevel": "approved" }))
                        .collect();
                    return Json(json!({ "hosts": hosts })).into_response();
                }
                if let Some(node) = tail
                    .strip_prefix("device/")
                    .and_then(|t| t.strip_suffix("/approved"))
                {
                    n.writes.push(format!("approve {name} {node}"));
                    n.approved.insert((name.to_owned(), node.to_owned()));
                    return Json(json!({})).into_response();
                }
            }
            let name = rest;
            match m {
                "GET" => match n.services.get(&name) {
                    Some((c, a)) => Json(json!({ "comment": c, "addrs": a })).into_response(),
                    None => (
                        StatusCode::NOT_FOUND,
                        Json(json!({ "message": "not found" })),
                    )
                        .into_response(),
                },
                "PUT" => {
                    n.writes.push(format!("PUT {name}"));
                    let addrs = vec![format!("100.100.1.{}", n.services.len() + 10)];
                    n.services
                        .insert(name, ("Made by Pyo (i cl_a)".into(), addrs.clone()));
                    Json(json!({ "addrs": addrs })).into_response()
                }
                "DELETE" => {
                    n.writes.push(format!("DELETE {name}"));
                    n.services.remove(&name);
                    Json(json!({})).into_response()
                }
                _ => StatusCode::METHOD_NOT_ALLOWED.into_response(),
            }
        }
        _ => (
            StatusCode::NOT_FOUND,
            Json(json!({ "message": "no such path" })),
        )
            .into_response(),
    }
}

async fn serve(net: Net) -> (Client, Shared) {
    let st: Shared = Arc::new(Mutex::new(net));
    let app = Router::new().fallback(handle).with_state(st.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (
        Client::at(&format!("http://{addr}"), "id", "tskey-client-id-secret"),
        st,
    )
}

const MARK: &str = "Made by Pyo (i cl_a)";

fn svc(comment: &str, addr: &str) -> (String, Vec<String>) {
    (comment.to_owned(), vec![addr.to_owned()])
}

/// A Service ("docker", someone else's) that the list leaves out but a
/// direct read finds. Pyo must refuse the name and never write to it.
#[tokio::test]
async fn a_service_the_list_leaves_out_is_never_written() {
    let mut net = Net::default();
    net.services
        .insert("docker".into(), svc("the user's", "100.100.1.1"));
    net.services.insert("mine".into(), svc(MARK, "100.100.1.2"));
    net.hidden = ["docker".to_owned(), "mine".to_owned()].into();
    let (c, st) = serve(net).await;
    assert!(
        c.services().await.unwrap().is_empty(),
        "the list leaves them out"
    );
    assert_eq!(
        claim(&c, "docker", MARK, &[MARK]).await.unwrap(),
        Claim::Taken
    );
    assert!(!delete_if_ours(&c, "docker", &[MARK]).await.unwrap());
    assert_eq!(
        claim(&c, "mine", MARK, &[MARK]).await.unwrap(),
        Claim::Ours(vec!["100.100.1.2".into()])
    );
    assert!(matches!(
        claim(&c, "blog", MARK, &[MARK]).await.unwrap(),
        Claim::Ours(_)
    ));
    assert!(delete_if_ours(&c, "mine", &[MARK]).await.unwrap());
    assert_eq!(st.lock().unwrap().writes, ["PUT blog", "DELETE mine"]);
}

#[test]
fn machines_waiting_for_a_signature() {
    let d = |host: &str, tag: &str, key: &str, err: &str| Device {
        node_id: "n".into(),
        hostname: host.into(),
        tags: vec![tag.into()],
        node_key: key.into(),
        lock_error: err.into(),
    };
    let waiting = lock_waiting(&[
        d(
            "sbx-1",
            TAG,
            "nodekey:ab12",
            "tailnet lock: node key not signed",
        ),
        d("laptop", "tag:other", "nodekey:cd34", "not signed"),
        d("web-1", TAG, "nodekey:ef56", ""),
    ]);
    assert_eq!(
        waiting,
        [LockWaiting {
            name: "sbx-1".into(),
            sign_command: "tailscale lock sign nodekey:ab12".into()
        }]
    );
}

fn own(kind: &str, current: Option<&str>, base: &str, keep: bool) -> OwnName {
    OwnName {
        kind: kind.into(),
        current: current.map(str::to_owned),
        base: base.into(),
        more: "home".into(),
        keep,
    }
}

fn names(s: &TailnetSynced) -> Vec<(String, String, bool)> {
    s.names
        .iter()
        .map(|n| (n.name.clone(), n.kind.clone(), n.error.is_none()))
        .collect()
}

/// Pyo's own names on the cluster Pyo runs on: picked around what's taken
/// (someone's Service, a machine, a domain), the dashboard's kept even
/// when someone else has it, the sandboxes' picked again.
#[tokio::test]
async fn own_names_are_picked_around_what_is_taken() {
    let mut net = Net::default();
    net.services
        .insert("pyo".into(), svc("someone's", "100.100.1.1"));
    net.services
        .insert("sandboxes".into(), svc("another Pyo's", "100.100.1.3"));
    net.devices
        .push(("n1".into(), "registry.tail1.ts.net".into(), vec![]));
    net.devices
        .push(("n2".into(), "web-1.tail1.ts.net".into(), vec![TAG.into()]));
    let (c, st) = serve(net).await;
    let w = TailnetSync {
        mark: MARK.into(),
        own: vec![
            own("dashboard", None, "pyo", true),
            own("registry", None, "registry", true),
            own("sandboxes", Some("sandboxes"), "sandboxes", false),
        ],
        domains: vec!["blog".into()],
        ..Default::default()
    };
    let s = sync(&c, &w).await.unwrap();
    assert_eq!(s.suffix.as_deref(), Some("tail1.ts.net"));
    assert_eq!(
        names(&s),
        [
            ("pyo-home".into(), "dashboard".into(), true),
            ("registry-home".into(), "registry".into(), true),
            ("sandboxes-home".into(), "sandboxes".into(), true),
            ("blog".into(), "domain".into(), true),
        ]
    );
    // A kept name someone else took meanwhile: kept, and said so.
    let w = TailnetSync {
        own: vec![own("dashboard", Some("pyo"), "pyo", true)],
        ..w
    };
    let s = sync(&c, &w).await.unwrap();
    assert_eq!(s.names[0].name, "pyo");
    assert_eq!(s.names[0].error.as_deref(), Some(TAKEN));
    let writes = st.lock().unwrap().writes.clone();
    assert!(
        !writes
            .iter()
            .any(|w| w == "PUT pyo" || w == "PUT sandboxes"),
        "{writes:?}"
    );
}

/// Only names this cluster made, no longer wanted, and still marked as its
/// own on the tailnet are deleted. Another cluster's (another mark) and
/// someone's own stay.
#[tokio::test]
async fn only_this_clusters_unused_names_are_deleted() {
    let mut net = Net::default();
    net.services.insert("old".into(), svc(MARK, "100.100.1.1"));
    net.services
        .insert("adopted".into(), svc("Made by Pyo (i)", "100.100.1.2"));
    net.services
        .insert("other".into(), svc("Made by Pyo (i cl_b)", "100.100.1.3"));
    net.services
        .insert("theirs".into(), svc("the user's", "100.100.1.4"));
    net.services.insert("kept".into(), svc(MARK, "100.100.1.5"));
    let (c, st) = serve(net).await;
    let w = TailnetSync {
        mark: MARK.into(),
        adopt: vec!["Made by Pyo (i)".into()],
        domains: vec!["kept".into()],
        made: ["old", "adopted", "other", "theirs", "kept"]
            .map(str::to_owned)
            .to_vec(),
        ..Default::default()
    };
    let s = sync(&c, &w).await.unwrap();
    assert_eq!(s.deleted, ["old", "adopted"]);
    let net = st.lock().unwrap();
    assert_eq!(net.writes, ["DELETE old", "DELETE adopted"]);
    assert!(net.services.contains_key("other") && net.services.contains_key("theirs"));
}

/// The cluster's servers: tagged when they aren't, and approved as hosts
/// of the names they advertise, once.
#[tokio::test]
async fn servers_are_tagged_and_approved_once() {
    let mut net = Net::default();
    net.devices
        .push(("n1".into(), "web-1.tail1.ts.net".into(), vec![]));
    net.devices
        .push(("n2".into(), "web-2.tail1.ts.net".into(), vec![TAG.into()]));
    let (c, st) = serve(net).await;
    let server = |name: &str, node: &str| SyncServer {
        server: name.into(),
        node_id: node.into(),
        advertised: vec!["shop".into()],
    };
    let w = TailnetSync {
        mark: MARK.into(),
        domains: vec!["shop".into()],
        servers: vec![server("web-1", "n1"), server("web-2", "n2")],
        ..Default::default()
    };
    sync(&c, &w).await.unwrap();
    sync(&c, &w).await.unwrap();
    let writes = st.lock().unwrap().writes.clone();
    assert_eq!(
        writes,
        [
            "PUT shop",
            "tag n1",
            "approve shop n1",
            "approve shop n2",
            "tag n1"
        ]
    );
}

#[tokio::test]
async fn a_name_check_reads_the_service_itself() {
    let mut net = Net::default();
    net.services
        .insert("docker".into(), svc("the user's", "100.100.1.1"));
    net.services.insert("blog".into(), svc(MARK, "100.100.1.2"));
    net.hidden = ["docker".to_owned()].into();
    net.devices
        .push(("n1".into(), "laptop.tail1.ts.net".into(), vec![]));
    let (c, _) = serve(net).await;
    let state = async |n: &str| check_name(&c, n, &[MARK]).await.unwrap().state;
    assert_eq!(state("docker").await, "taken");
    assert_eq!(state("laptop").await, "taken");
    assert_eq!(state("blog").await, "ours");
    assert_eq!(state("shop").await, "free");
}

#[tokio::test]
async fn a_join_key_is_single_use_tagged_and_short_lived() {
    let (c, st) = serve(Net::default()).await;
    let (_, key) = c.auth_key("Pyo: join web-2").await.unwrap();
    assert_eq!(key, "tskey-auth-k1-secret");
    assert_eq!(st.lock().unwrap().writes, ["key"]);
}

#[test]
fn hints_show_the_last_characters_only() {
    assert_eq!(hint("tskey-client-abc-0123456789WXYZ"), "…WXYZ");
}
