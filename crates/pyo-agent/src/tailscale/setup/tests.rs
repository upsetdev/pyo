//! A mock Tailscale API, and the setup run against it.

use super::*;
use std::sync::{Arc, Mutex};

use axum::{
    Router,
    body::Bytes,
    extract::State,
    http::{HeaderMap, Method, StatusCode, Uri},
    response::{IntoResponse, Response},
};

const TOKEN: &str = "tskey-api-kTok1CNTRL-0123456789abcdefghijSECRET";
const TOKEN_SECRET: &str = "0123456789abcdefghijSECRET";

/// A new tailnet's policy, as Tailscale writes it (grants, comments,
/// trailing commas, tabs).
const DEFAULT_POLICY: &str = r#"// Example/default ACLs for unrestricted connections.
{
	// Declare static groups of users. Use autogroups for all users or users with a specific role.
	// "groups": {
	//  	"group:example": ["alice@example.com", "bob@example.com"],
	// },

	// Define the tags which can be applied to devices and by which users.
	// "tagOwners": {
	//  	"tag:example": ["autogroup:admin"],
	// },

	// Define grants that govern access for users, groups, autogroups, tags,
	// Tailscale IP addresses, and subnet ranges.
	"grants": [
		// Allow all connections.
		// Comment this section out if you want to define specific restrictions.
		{"src": ["*"], "dst": ["*"], "ip": ["*"]},
	],

	// Define users and devices that can use Tailscale SSH.
	"ssh": [
		// Allow all users to SSH into their own devices in check mode.
		// Comment this section out if you want to define specific restrictions.
		{
			"action": "check",
			"src":    ["autogroup:member"],
			"dst":    ["autogroup:self"],
			"users":  ["autogroup:nonroot", "root"],
		},
	],

	// Test access rules every time they're saved.
	// "tests": [
	//  	{
	//  		"src": "alice@example.com",
	//  		"accept": ["tag:example"],
	//  		"deny": ["100.101.102.103:443"],
	//  	},
	// ],
}
"#;

/// A restrictive policy with tag owners of its own, and comments.
const RESTRICTED_POLICY: &str = r#"{
  // Who owns which tags.
  "tagOwners": {
    "tag:prod": ["group:eng"], // production servers
  },
  "groups": { "group:eng": ["alice@example.com"] },
  /* Only engineers, only SSH to production. */
  "acls": [
    { "action": "accept", "src": ["group:eng"], "dst": ["tag:prod:22"] },
  ],
}
"#;

/// A policy that has everything Pyo needs.
const SET_UP_POLICY: &str = r#"{
  "tagOwners": { "tag:pyo": ["autogroup:admin"] },
  "autoApprovers": { "services": { "tag:pyo": ["tag:pyo"] } },
  "acls": [{ "action": "accept", "src": ["*"], "dst": ["*:*"] }],
}
"#;

#[derive(Default)]
struct Mock {
    policy: String,
    etag: u32,
    gitops: bool,
    magic_dns: bool,
    nameservers: Vec<String>,
    /// Stored HTTPS certificates setting.
    https: bool,
    /// The settings' name for it this Tailscale knows: "httpsEnabled"
    /// (default) or "httpsCertificates".
    https_field: Option<&'static str>,
    /// Policy writes that lose to someone else's edit first.
    races: u32,
    refuse_client: bool,
    refuse_revoke: bool,
    /// Error messages carry the request's Authorization header.
    echo_token: bool,
    clients: Vec<Value>,
    deleted: Vec<String>,
    /// OAuth clients made before (the team's old ones), by ID.
    old: Vec<String>,
    /// The token can't delete OAuth clients (only itself).
    refuse_delete_clients: bool,
    /// Machines on the tailnet, by full name.
    devices: Vec<String>,
    /// "METHOD /path" of every request.
    log: Vec<String>,
    /// Policy texts written.
    written: Vec<String>,
}

type Shared = Arc<Mutex<Mock>>;

/// One test against the mock at a time, so the log test sees every line
/// (tracing caches what's logged per call site, across threads).
static ONE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn err(m: &Mock, status: StatusCode, msg: &str, auth: &str) -> Response {
    let message = if m.echo_token {
        format!("{msg} (request had {auth})")
    } else {
        msg.to_owned()
    };
    (status, axum::Json(json!({ "message": message }))).into_response()
}

async fn handle(
    State(st): State<Shared>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let mut m = st.lock().unwrap();
    let path = uri.path().to_owned();
    m.log.push(format!("{method} {path}"));
    let auth = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_owned();
    if auth != format!("Bearer {TOKEN}") {
        return err(&m, StatusCode::UNAUTHORIZED, "API token invalid", &auth);
    }
    let body = String::from_utf8_lossy(&body).to_string();
    let field = m.https_field.unwrap_or("httpsEnabled");
    let settings = |m: &Mock| json!({ "aclsExternallyManagedOn": m.gitops, field: m.https });
    match (method.as_str(), path.as_str()) {
        ("GET", "/tailnet/-/settings") => axum::Json(settings(&m)).into_response(),
        ("PATCH", "/tailnet/-/settings") => {
            let v: Value = serde_json::from_str(&body).unwrap();
            for other in ["httpsEnabled", "httpsCertificates"] {
                if other != field && v.get(other).is_some() {
                    return err(&m, StatusCode::BAD_REQUEST, "unknown field", &auth);
                }
            }
            if let Some(b) = v[field].as_bool() {
                m.https = b;
            }
            axum::Json(settings(&m)).into_response()
        }
        ("GET", "/tailnet/-/acl") => {
            assert_eq!(headers["accept"], "application/hujson");
            ([("etag", format!("\"e{}\"", m.etag))], m.policy.clone()).into_response()
        }
        ("POST", "/tailnet/-/acl/validate") => {
            assert_eq!(headers["content-type"], "application/hujson");
            match jsonc_parser::parse_to_serde_value::<Value>(&body, &hujson()) {
                Ok(_) => axum::Json(json!({})).into_response(),
                Err(e) => axum::Json(json!({ "message": e.to_string() })).into_response(),
            }
        }
        ("POST", "/tailnet/-/acl") => {
            assert_eq!(headers["content-type"], "application/hujson");
            if m.races > 0 {
                // Someone else saves first.
                m.races -= 1;
                m.etag += 1;
                let edited = m.policy.replacen('{', "{\n  // edited meanwhile", 1);
                m.policy = edited;
            }
            let want = format!("\"e{}\"", m.etag);
            if headers.get("if-match").and_then(|v| v.to_str().ok()) != Some(want.as_str()) {
                return err(
                    &m,
                    StatusCode::PRECONDITION_FAILED,
                    "precondition failed, invalid old hash",
                    &auth,
                );
            }
            m.etag += 1;
            m.policy = body.clone();
            m.written.push(body.clone());
            body.into_response()
        }
        ("GET", "/tailnet/-/dns/preferences") => {
            axum::Json(json!({ "magicDNS": m.magic_dns })).into_response()
        }
        ("POST", "/tailnet/-/dns/preferences") => {
            if m.nameservers.is_empty() {
                return err(
                    &m,
                    StatusCode::BAD_REQUEST,
                    "need at least one nameserver",
                    &auth,
                );
            }
            m.magic_dns = true;
            axum::Json(json!({ "magicDNS": true })).into_response()
        }
        ("GET", "/tailnet/-/dns/nameservers") => {
            axum::Json(json!({ "dns": m.nameservers })).into_response()
        }
        ("POST", "/tailnet/-/keys") => {
            if m.refuse_client {
                return err(
                    &m,
                    StatusCode::FORBIDDEN,
                    "not allowed to create OAuth clients",
                    &auth,
                );
            }
            let v: Value = serde_json::from_str(&body).unwrap();
            let id = format!("kPyo{}CNTRL", m.clients.len() + 1);
            let k = json!({
                "id": id, "keyType": v["keyType"], "scopes": v["scopes"], "tags": v["tags"],
                "description": v["description"],
                "key": format!("tskey-client-{id}-clientsecret0123456789"),
            });
            m.clients.push(k.clone());
            axum::Json(k).into_response()
        }
        ("GET", p) if p.starts_with("/tailnet/-/keys/") => {
            let id = &p["/tailnet/-/keys/".len()..];
            match m.clients.iter().find(|k| k["id"] == id) {
                Some(k) if !m.deleted.iter().any(|d| d == id) => {
                    axum::Json(json!({ "id": id, "keyType": "client", "scopes": k["scopes"] }))
                        .into_response()
                }
                _ => err(&m, StatusCode::NOT_FOUND, "key not found", &auth),
            }
        }
        ("GET", "/tailnet/-/devices") => {
            let list: Vec<Value> = m.devices.iter().map(|n| json!({ "name": n })).collect();
            axum::Json(json!({ "devices": list })).into_response()
        }
        ("DELETE", p) if p.starts_with("/tailnet/-/keys/") && p != "/tailnet/-/keys/kTok1CNTRL" => {
            let id = p["/tailnet/-/keys/".len()..].to_owned();
            let known = m.old.contains(&id) || m.clients.iter().any(|k| k["id"] == id.as_str());
            if !known || m.deleted.contains(&id) {
                return err(&m, StatusCode::NOT_FOUND, "key not found", &auth);
            }
            if m.refuse_delete_clients {
                return err(&m, StatusCode::FORBIDDEN, "insufficient permissions", &auth);
            }
            m.deleted.push(id);
            axum::Json(json!({})).into_response()
        }
        ("DELETE", p) if p.starts_with("/tailnet/-/keys/") => {
            let id = p["/tailnet/-/keys/".len()..].to_owned();
            if m.refuse_revoke && id == "kTok1CNTRL" {
                return err(
                    &m,
                    StatusCode::FORBIDDEN,
                    "cannot delete the key in use",
                    &auth,
                );
            }
            m.deleted.push(id);
            axum::Json(json!({})).into_response()
        }
        _ => err(&m, StatusCode::NOT_FOUND, "no such endpoint", &auth),
    }
}

async fn serve(m: Mock) -> (String, Shared) {
    let st: Shared = Arc::new(Mutex::new(m));
    let app = Router::new().fallback(handle).with_state(st.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}"), st)
}

/// The cluster, as the setup sees it.
#[derive(Default)]
struct FakeHost {
    no_key: bool,
    /// The saved client that signs in.
    client: Mutex<Option<String>>,
    /// What was saved: (client ID, secret).
    saved: Mutex<Vec<(String, String)>>,
    save_fails: bool,
}

impl Host for FakeHost {
    fn can_store(&self) -> bool {
        !self.no_key
    }
    async fn working_client(&self) -> Option<String> {
        self.client.lock().unwrap().clone()
    }
    async fn save_client(&self, id: &str, secret: &str) -> Result<Value, String> {
        if self.save_fails {
            return Err("The client didn't sign in.".into());
        }
        self.saved
            .lock()
            .unwrap()
            .push((id.to_owned(), secret.to_owned()));
        *self.client.lock().unwrap() = Some(id.to_owned());
        Ok(
            json!({ "ok": true, "checks": [{ "name": "client", "ok": true, "message": "Signed in." }] }),
        )
    }
}

fn api(base: &str) -> Api {
    Api::at(base, Token::new(TOKEN))
}

/// Plans, then sets up what was reviewed, as the dashboard does.
async fn set_up(base: &str, host: &FakeHost) -> Outcome {
    let p = plan_with(&api(base), host).await.unwrap();
    apply_with(&api(base), host, &p.version, &[]).await
}

fn fresh() -> Mock {
    Mock {
        policy: DEFAULT_POLICY.into(),
        magic_dns: true,
        nameservers: vec!["1.1.1.1".into()],
        ..Default::default()
    }
}

fn comments(text: &str) -> Vec<&str> {
    text.lines()
        .filter_map(|l| {
            let l = l.trim();
            (l.starts_with("//") || l.starts_with("/*") || l.contains(" // "))
                .then_some(l.split("//").last().unwrap_or(l))
        })
        .collect()
}

fn writes(m: &Mock) -> Vec<String> {
    m.log
        .iter()
        .filter(|l| !l.starts_with("GET ") && !l.ends_with("/acl/validate"))
        .cloned()
        .collect()
}

// ── The policy, on its own ──────────────────────────────────────────────────

#[test]
fn the_patch_keeps_comments_and_formatting() {
    let n = needs(&parse(DEFAULT_POLICY).unwrap());
    assert_eq!(
        n,
        Needs {
            tag_owner: true,
            reach: Reach::Yes,
            approver: true
        }
    );
    let out = patch(DEFAULT_POLICY, &n).unwrap();
    for c in comments(DEFAULT_POLICY) {
        assert!(out.contains(c), "lost the comment {c:?}:\n{out}");
    }
    // Every line it had is still there, as it was.
    for l in DEFAULT_POLICY.lines() {
        assert!(
            out.lines().any(|o| o == l),
            "changed the line {l:?}:\n{out}"
        );
    }
    let v = parse(&out).unwrap();
    assert_eq!(v["tagOwners"][TAG], json!(["autogroup:admin"]));
    assert_eq!(v["autoApprovers"]["services"][TAG], json!([TAG]));
    assert_eq!(v["grants"], parse(DEFAULT_POLICY).unwrap()["grants"]);
    assert!(!needs(&v).any());
    // What the diff shows is only what's new.
    let d = diff(DEFAULT_POLICY, &out);
    assert!(d.iter().all(|l| l.starts_with("+ ")), "{d:?}");
    assert!(
        d.iter()
            .any(|l| l.contains("\"tag:pyo\": [\"autogroup:admin\"]")),
        "{d:?}"
    );
}

#[test]
fn the_patch_adds_to_tag_owners_already_there() {
    let n = needs(&parse(RESTRICTED_POLICY).unwrap());
    assert_eq!(
        n,
        Needs {
            tag_owner: true,
            reach: Reach::No,
            approver: true
        }
    );
    let out = patch(RESTRICTED_POLICY, &n).unwrap();
    for c in [
        "// Who owns which tags.",
        "// production servers",
        "/* Only engineers, only SSH to production. */",
    ] {
        assert!(out.contains(c), "lost {c:?}:\n{out}");
    }
    let v = parse(&out).unwrap();
    assert_eq!(v["tagOwners"]["tag:prod"], json!(["group:eng"]));
    assert_eq!(v["tagOwners"][TAG], json!(["autogroup:admin"]));
    assert_eq!(
        v["grants"],
        json!([{ "src": ["autogroup:member"], "dst": [TAG], "ip": ["tcp:80", "tcp:443"] }])
    );
    assert_eq!(v["acls"], parse(RESTRICTED_POLICY).unwrap()["acls"]);
    assert_eq!(reach(&v), Reach::Yes);
}

#[test]
fn approvers_for_tag_pyo_get_tag_pyo_added() {
    let p = r#"{ "autoApprovers": { "services": { "tag:pyo": ["group:ops"] } }, "tagOwners": { "tag:pyo": [] }, "grants": [{ "src": ["*"], "dst": ["*"], "ip": ["*"] }] }"#;
    let n = needs(&parse(p).unwrap());
    assert!(n.approver && !n.tag_owner && n.grant().is_none());
    let v = parse(&patch(p, &n).unwrap()).unwrap();
    assert_eq!(
        v["autoApprovers"]["services"][TAG],
        json!(["group:ops", TAG])
    );
}

fn src(src: &[&str], posture: &[&str]) -> Source {
    Source {
        src: src.iter().map(|s| (*s).to_owned()).collect(),
        posture: posture.iter().map(|s| (*s).to_owned()).collect(),
    }
}

#[test]
fn what_lets_the_tailnet_reach_pyo() {
    let r = |p: Value| reach(&p);
    for yes in [
        json!({ "grants": [{ "src": ["*"], "dst": ["*"], "ip": ["*"] }] }),
        json!({ "acls": [{ "action": "accept", "src": ["*"], "dst": ["*:*"] }] }),
        json!({ "acls": [{ "action": "accept", "users": ["*"], "ports": ["*:*"] }] }),
        json!({ "grants": [{ "src": ["autogroup:member"], "dst": [TAG], "ip": ["tcp:80", "443"] }] }),
        json!({ "acls": [{ "action": "accept", "src": ["*"], "dst": ["tag:pyo:80,443"] }] }),
        json!({ "acls": [{ "action": "accept", "src": ["*"], "dst": ["tag:pyo:1-1024"] }] }),
        // Split over two rules.
        json!({ "grants": [
            { "src": ["*"], "dst": [TAG], "ip": ["tcp:80"] },
            { "src": ["*"], "dst": ["*"], "ip": ["tcp:443"] },
        ] }),
        // Only some people, or only with a posture: the admin's choice, kept.
        json!({ "grants": [{ "src": ["group:eng"], "dst": ["*"], "ip": ["*"] }] }),
        json!({ "grants": [{ "src": ["*"], "dst": ["*"], "ip": ["*"], "srcPosture": ["posture:x"] }] }),
    ] {
        assert_eq!(r(yes.clone()), Reach::Yes, "{yes}");
    }
    // A port missing, or UDP only: those same sources, and no one else.
    assert_eq!(
        r(json!({ "grants": [{ "src": ["group:eng"], "dst": [TAG], "ip": ["tcp:443"] }] })),
        Reach::Partly(vec![src(&["group:eng"], &[])])
    );
    assert_eq!(
        r(json!({ "grants": [{ "src": ["*"], "dst": [TAG], "ip": ["udp:*"] }] })),
        Reach::Partly(vec![src(&["*"], &[])])
    );
    assert_eq!(
        r(
            json!({ "acls": [{ "action": "accept", "src": ["*"], "proto": "udp", "dst": ["*:*"] }] })
        ),
        Reach::Partly(vec![src(&["*"], &[])])
    );
    assert_eq!(
        r(
            json!({ "acls": [{ "action": "accept", "src": ["group:eng"], "srcPosture": ["posture:x"], "dst": ["*:22"] }] })
        ),
        Reach::Partly(vec![src(&["group:eng"], &["posture:x"])])
    );
    // Nothing reaches tag:pyo, or there are no rules yet.
    assert_eq!(
        r(
            json!({ "acls": [{ "action": "accept", "src": ["group:eng"], "dst": ["tag:prod:22"] }] })
        ),
        Reach::No
    );
    assert_eq!(r(json!({ "acls": [] })), Reach::No);
    assert_eq!(r(json!({ "tagOwners": {} })), Reach::NoRules);
}

#[test]
fn access_limited_to_a_group_is_never_widened() {
    // The group reaches everything: nothing to add, and no grant at all.
    let p = r#"{ "tagOwners": { "tag:pyo": [] }, "grants": [{ "src": ["group:eng"], "dst": ["*"], "ip": ["*"] }] }"#;
    let n = needs(&parse(p).unwrap());
    assert_eq!(n.reach, Reach::Yes);
    let out = patch(
        p,
        &Needs {
            approver: true,
            ..n
        },
    )
    .unwrap();
    assert_eq!(parse(&out).unwrap()["grants"], parse(p).unwrap()["grants"]);
    assert!(!out.contains(MEMBERS), "{out}");

    // The group reaches tag:pyo on 443 only: 80 is added for that group,
    // with its posture, and never for anyone else.
    let p = r#"{
  "tagOwners": { "tag:pyo": [] },
  "grants": [
    { "src": ["group:eng"], "dst": ["tag:pyo"], "ip": ["tcp:443"], "srcPosture": ["posture:latest"] },
  ],
}"#;
    let n = needs(&parse(p).unwrap());
    assert!(n.required());
    let out = patch(p, &n).unwrap();
    let v = parse(&out).unwrap();
    assert_eq!(
        v["grants"][1],
        json!({ "src": ["group:eng"], "dst": [TAG], "ip": ["tcp:80", "tcp:443"], "srcPosture": ["posture:latest"] })
    );
    assert_eq!(v["grants"].as_array().unwrap().len(), 2);
    assert!(!out.contains(MEMBERS), "{out}");
    assert_eq!(reach(&v), Reach::Yes);
}

#[test]
fn the_token_shape_is_checked_and_its_id_found() {
    assert!(check_shape(&Token::new(TOKEN)).is_ok());
    assert!(
        check_shape(&Token::new("tskey-client-k1CNTRL-abcdefabcdef"))
            .unwrap_err()
            .contains("OAuth client secret")
    );
    assert!(
        check_shape(&Token::new("tskey-auth-k1CNTRL-abcdefabcdef"))
            .unwrap_err()
            .contains("auth key")
    );
    assert!(check_shape(&Token::new("hello")).is_err());
    assert_eq!(Token::new(TOKEN).key_id(), Some("kTok1CNTRL"));
    assert_eq!(Token::new("tskey-api-").key_id(), None);
    assert_eq!(format!("{:?}", Token::new(TOKEN)), "Token(•••)");
}

#[test]
fn the_plan_uses_the_policy_as_parsed_once() {
    // plan_of never reads the text again: what the policy reaches comes
    // from the one parse, so even text it couldn't parse doesn't change it.
    let s = Survey {
        gitops: false,
        policy: Policy {
            text: "not a policy".into(),
            etag: None,
            needs: Needs {
                reach: Reach::Yes,
                ..Default::default()
            },
            patched: None,
        },
        magic_dns: true,
        https: Some(true),
        client: true,
    };
    let p = plan_of(&s);
    assert!(p.changes.is_empty(), "{p:?}");
    assert!(
        p.done
            .contains(&"Your tailnet can reach tag:pyo".to_owned())
    );
}

// ── Against the mock API ────────────────────────────────────────────────────

#[tokio::test]
async fn a_fresh_tailnet_with_the_default_policy() {
    let _one = ONE.lock().await;
    let (base, st) = serve(fresh()).await;
    let host = FakeHost::default();
    let p = plan_with(&api(&base), &host).await.unwrap();
    assert_eq!(
        p.changes,
        [
            "Turn on HTTPS certificates",
            "Add tag:pyo to your policy",
            "Approve Pyo's Services on tag:pyo servers by policy",
            "Create the Pyo OAuth client",
        ]
    );
    assert_eq!(p.done, ["MagicDNS is on", "Your tailnet can reach tag:pyo"]);
    assert!(p.blocked.is_none());
    assert!(p.diff.iter().any(|l| l.contains("tag:pyo")));
    assert!(
        writes(&st.lock().unwrap()).is_empty(),
        "the plan changes nothing"
    );

    let out = apply_with(&api(&base), &host, &p.version, &[]).await;
    assert!(out.ok, "{out:?}");
    assert_eq!(
        out.changed,
        [
            "Turned on HTTPS certificates",
            "Added tag:pyo to your policy",
            "Approved Pyo's Services on tag:pyo servers by policy",
            "Created the Pyo OAuth client",
        ]
    );
    assert!(out.token_deleted && out.todo.is_empty() && out.error.is_none());
    let m = st.lock().unwrap();
    assert!(m.https);
    // The comments are all still there.
    for c in comments(DEFAULT_POLICY) {
        assert!(m.policy.contains(c), "lost {c:?}");
    }
    // The client has exactly the scopes the manual path asks for.
    assert_eq!(m.clients.len(), 1);
    assert_eq!(m.clients[0]["keyType"], "client");
    assert_eq!(
        m.clients[0]["scopes"],
        json!(["devices:core", "auth_keys", "services"])
    );
    assert_eq!(m.clients[0]["tags"], json!([TAG]));
    assert_eq!(
        *host.saved.lock().unwrap(),
        [(
            "kPyo1CNTRL".to_owned(),
            "tskey-client-kPyo1CNTRL-clientsecret0123456789".to_owned()
        )]
    );
    // The token went last.
    assert_eq!(m.deleted, ["kTok1CNTRL"]);
    assert_eq!(
        writes(&m).last().unwrap(),
        "DELETE /tailnet/-/keys/kTok1CNTRL"
    );
}

#[tokio::test]
async fn a_restricted_policy_with_its_own_tag_owners() {
    let _one = ONE.lock().await;
    let (base, st) = serve(Mock {
        policy: RESTRICTED_POLICY.into(),
        https: true,
        ..fresh()
    })
    .await;
    let host = FakeHost::default();
    let p = plan_with(&api(&base), &host).await.unwrap();
    assert_eq!(
        p.changes,
        [
            "Add tag:pyo to your policy",
            "Let your tailnet reach tag:pyo on ports 80 and 443",
            "Approve Pyo's Services on tag:pyo servers by policy",
            "Create the Pyo OAuth client",
        ]
    );
    let out = apply_with(&api(&base), &host, &p.version, &[]).await;
    assert!(out.ok, "{out:?}");
    let m = st.lock().unwrap();
    assert!(
        m.policy
            .contains("\"tag:prod\": [\"group:eng\"], // production servers")
    );
    assert!(
        m.policy
            .contains("/* Only engineers, only SSH to production. */")
    );
    assert_eq!(
        parse(&m.policy).unwrap()["tagOwners"][TAG],
        json!(["autogroup:admin"])
    );
    assert_eq!(m.written.len(), 1);
}

#[tokio::test]
async fn an_already_set_up_tailnet_changes_nothing() {
    let _one = ONE.lock().await;
    let (base, st) = serve(Mock {
        policy: SET_UP_POLICY.into(),
        https: true,
        clients: vec![json!({ "id": "kOld1CNTRL", "keyType": "client", "scopes": SCOPES })],
        ..fresh()
    })
    .await;
    let host = FakeHost {
        client: Mutex::new(Some("kOld1CNTRL".into())),
        ..Default::default()
    };
    let p = plan_with(&api(&base), &host).await.unwrap();
    assert!(p.changes.is_empty(), "{p:?}");
    assert!(p.diff.is_empty());
    assert_eq!(p.done.len(), 6);
    let out = apply_with(&api(&base), &host, &p.version, &[]).await;
    assert!(
        out.ok && out.changed.is_empty() && out.token_deleted,
        "{out:?}"
    );
    let m = st.lock().unwrap();
    assert_eq!(writes(&m), ["DELETE /tailnet/-/keys/kTok1CNTRL"]);
    assert!(host.saved.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_saved_client_from_another_tailnet_is_replaced() {
    let _one = ONE.lock().await;
    let (base, st) = serve(Mock {
        policy: SET_UP_POLICY.into(),
        https: true,
        ..fresh()
    })
    .await;
    let host = FakeHost {
        client: Mutex::new(Some("kElsewhereCNTRL".into())),
        ..Default::default()
    };
    let p = plan_with(&api(&base), &host).await.unwrap();
    assert_eq!(p.changes, ["Create the Pyo OAuth client"]);
    let out = apply_with(&api(&base), &host, &p.version, &[]).await;
    assert!(out.ok);
    assert_eq!(st.lock().unwrap().clients.len(), 1);
}

#[tokio::test]
async fn a_policy_managed_in_git_is_never_written() {
    let _one = ONE.lock().await;
    let (base, st) = serve(Mock {
        gitops: true,
        ..fresh()
    })
    .await;
    let host = FakeHost::default();
    let p = plan_with(&api(&base), &host).await.unwrap();
    assert!(p.blocked.as_deref().unwrap().contains("managed outside"));
    assert!(
        p.diff
            .iter()
            .any(|l| l.contains("\"tag:pyo\": [\"autogroup:admin\"]"))
    );
    // Set up anyway: nothing is written, and the token stays for a new review.
    let out = apply_with(&api(&base), &host, &p.version, &[]).await;
    assert!(!out.ok && out.review_again && !out.token_deleted, "{out:?}");
    assert!(writes(&st.lock().unwrap()).is_empty());

    // Once Git has the lines, the rest goes on, and the policy is left alone.
    let (base, st) = serve(Mock {
        gitops: true,
        policy: r#"{ "tagOwners": { "tag:pyo": [] }, "grants": [{ "src": ["*"], "dst": ["*"], "ip": ["*"] }] }"#.into(),
        ..fresh()
    })
    .await;
    let p = plan_with(&api(&base), &host).await.unwrap();
    assert!(p.blocked.is_none() && p.diff.is_empty(), "{p:?}");
    assert!(!p.changes.iter().any(|c| c.contains("Approve")));
    let out = apply_with(&api(&base), &host, &p.version, &[]).await;
    assert!(out.ok, "{out:?}");
    assert!(st.lock().unwrap().written.is_empty());
}

#[tokio::test]
async fn a_policy_changed_while_setting_up_is_reviewed_again() {
    let _one = ONE.lock().await;
    let (base, st) = serve(Mock {
        races: 1,
        ..fresh()
    })
    .await;
    let host = FakeHost::default();
    let p = plan_with(&api(&base), &host).await.unwrap();
    let out = apply_with(&api(&base), &host, &p.version, &[]).await;
    // Saving would lose the other edit or save what wasn't reviewed: stop.
    assert!(!out.ok && out.review_again, "{out:?}");
    assert_eq!(out.changed, ["Turned on HTTPS certificates"]);
    assert_eq!(out.error.as_deref(), Some(REVIEW_AGAIN));
    // The token stays for the new review, and nothing after the policy ran.
    assert!(!out.token_deleted && out.todo.is_empty());
    {
        let m = st.lock().unwrap();
        assert!(m.written.is_empty() && m.clients.is_empty() && m.deleted.is_empty());
    }

    // The new review shows the policy as it is now, and sets up from it.
    let again = plan_with(&api(&base), &host).await.unwrap();
    assert_ne!(again.version, p.version);
    assert!(again.done.contains(&"HTTPS certificates are on".to_owned()));
    let out = apply_with(&api(&base), &host, &again.version, &[]).await;
    assert!(out.ok && out.token_deleted, "{out:?}");
    let m = st.lock().unwrap();
    // The other edit is kept, under Pyo's.
    assert!(m.policy.contains("// edited meanwhile"));
    assert!(parse(&m.policy).unwrap()["tagOwners"].get(TAG).is_some());
    assert_eq!(m.written.len(), 1);
}

#[tokio::test]
async fn a_policy_changed_after_the_review_changes_nothing() {
    let _one = ONE.lock().await;
    let (base, st) = serve(fresh()).await;
    let host = FakeHost::default();
    let p = plan_with(&api(&base), &host).await.unwrap();
    {
        let mut m = st.lock().unwrap();
        m.policy = m.policy.replacen('{', "{\n  // edited after the review", 1);
        m.etag += 1;
    }
    let out = apply_with(&api(&base), &host, &p.version, &[]).await;
    assert!(
        !out.ok && out.review_again && out.changed.is_empty(),
        "{out:?}"
    );
    assert!(writes(&st.lock().unwrap()).is_empty(), "nothing changed");
}

#[tokio::test]
async fn https_under_its_other_name() {
    let _one = ONE.lock().await;
    let (base, st) = serve(Mock {
        https_field: Some("httpsCertificates"),
        ..fresh()
    })
    .await;
    let out = set_up(&base, &FakeHost::default()).await;
    assert!(
        out.ok
            && out
                .changed
                .contains(&"Turned on HTTPS certificates".to_owned()),
        "{out:?}"
    );
    let m = st.lock().unwrap();
    assert!(m.https);
    assert_eq!(
        m.log
            .iter()
            .filter(|l| *l == "PATCH /tailnet/-/settings")
            .count(),
        2
    );
}

#[tokio::test]
async fn a_refused_client_says_what_changed() {
    let _one = ONE.lock().await;
    let (base, st) = serve(Mock {
        refuse_client: true,
        https: true,
        ..fresh()
    })
    .await;
    let host = FakeHost::default();
    let out = set_up(&base, &host).await;
    assert!(!out.ok);
    assert_eq!(
        out.changed,
        [
            "Added tag:pyo to your policy",
            "Approved Pyo's Services on tag:pyo servers by policy",
        ]
    );
    let e = out.error.clone().unwrap();
    assert!(
        e.contains("didn't let Pyo create its OAuth client") && e.contains("not allowed"),
        "{e}"
    );
    assert!(host.saved.lock().unwrap().is_empty());
    // The setup is over, so the token goes all the same.
    assert!(out.token_deleted && out.todo.is_empty(), "{out:?}");
    assert_eq!(st.lock().unwrap().deleted, ["kTok1CNTRL"]);
}

#[tokio::test]
async fn a_client_that_cant_be_saved_is_deleted_again() {
    let _one = ONE.lock().await;
    let (base, st) = serve(Mock {
        https: true,
        policy: SET_UP_POLICY.into(),
        ..fresh()
    })
    .await;
    let host = FakeHost {
        save_fails: true,
        ..Default::default()
    };
    let out = set_up(&base, &host).await;
    assert!(!out.ok && out.changed.is_empty(), "{out:?}");
    let e = out.error.clone().unwrap();
    assert!(e.contains("couldn't save its OAuth client"), "{e}");
    assert!(out.token_deleted);
    assert_eq!(st.lock().unwrap().deleted, ["kPyo1CNTRL", "kTok1CNTRL"]);
}

#[tokio::test]
async fn a_token_that_cant_delete_itself_is_left_to_the_admin() {
    let _one = ONE.lock().await;
    let (base, _) = serve(Mock {
        refuse_revoke: true,
        ..fresh()
    })
    .await;
    let out = set_up(&base, &FakeHost::default()).await;
    assert!(out.ok && !out.token_deleted);
    assert_eq!(out.todo, [DELETE_TOKEN]);
}

#[tokio::test]
async fn magic_dns_without_nameservers_changes_nothing() {
    let _one = ONE.lock().await;
    let (base, st) = serve(Mock {
        magic_dns: false,
        nameservers: vec![],
        ..fresh()
    })
    .await;
    let host = FakeHost::default();
    let p = plan_with(&api(&base), &host).await.unwrap();
    assert_eq!(p.changes[0], "Turn on MagicDNS");
    let out = apply_with(&api(&base), &host, &p.version, &[]).await;
    assert!(!out.ok && out.changed.is_empty(), "{out:?}");
    let e = out.error.clone().unwrap();
    assert!(e.contains("needs a DNS nameserver"), "{e}");
    // Stopped at the start, and the token deleted all the same.
    assert!(out.token_deleted && out.todo.is_empty());
    assert_eq!(
        writes(&st.lock().unwrap()),
        [
            "POST /tailnet/-/dns/preferences",
            "DELETE /tailnet/-/keys/kTok1CNTRL"
        ]
    );
}

#[tokio::test]
async fn a_stop_with_a_token_that_cant_delete_itself_says_to_delete_it() {
    let _one = ONE.lock().await;
    let (base, _) = serve(Mock {
        magic_dns: false,
        nameservers: vec![],
        refuse_revoke: true,
        ..fresh()
    })
    .await;
    let out = set_up(&base, &FakeHost::default()).await;
    assert!(
        !out.ok && out.error.is_some() && !out.token_deleted,
        "{out:?}"
    );
    assert_eq!(out.todo, [DELETE_TOKEN]);
}

#[tokio::test]
async fn a_token_tailscale_no_longer_takes_needs_no_deleting() {
    let _one = ONE.lock().await;
    let (base, _) = serve(fresh()).await;
    let wrong = Api::at(
        &base,
        Token::new("tskey-api-kOther1CNTRL-zzzzzzzzzzzzzzzzzzzz"),
    );
    let out = apply_with(&wrong, &FakeHost::default(), "", &[]).await;
    assert_eq!(out.error.as_deref(), Some(BAD_TOKEN));
    assert!(
        !out.ok && !out.token_deleted && out.todo.is_empty(),
        "{out:?}"
    );
}

#[tokio::test]
async fn a_wrong_token_or_no_secret_key() {
    let _one = ONE.lock().await;
    let (base, _) = serve(fresh()).await;
    let wrong = Api::at(
        &base,
        Token::new("tskey-api-kOther1CNTRL-zzzzzzzzzzzzzzzzzzzz"),
    );
    let e = plan_with(&wrong, &FakeHost::default()).await.unwrap_err();
    assert_eq!(e, BAD_TOKEN);
    let host = FakeHost {
        no_key: true,
        ..Default::default()
    };
    assert_eq!(plan_with(&api(&base), &host).await.unwrap_err(), NO_KEY);
}

// ── The token never shows ───────────────────────────────────────────────────

#[derive(Clone, Default)]
struct Captured(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Captured {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[tokio::test]
async fn the_token_never_shows_in_logs_errors_or_saved_settings() {
    let _one = ONE.lock().await;
    let logs = Captured::default();
    let w = logs.clone();
    let _guard = tracing::subscriber::set_default(
        tracing_subscriber::fmt()
            .with_max_level(tracing::Level::TRACE)
            .with_writer(move || w.clone())
            .finish(),
    );
    tracing::callsite::rebuild_interest_cache();
    let leaks = |s: &str| s.contains(TOKEN) || s.contains(TOKEN_SECRET);
    let mut seen = vec![];

    // Tailscale echoing it back in its errors, on every failure path.
    for m in [
        Mock {
            refuse_client: true,
            ..fresh()
        },
        Mock {
            races: 2,
            ..fresh()
        },
        Mock {
            magic_dns: false,
            nameservers: vec!["1.1.1.1".into()],
            refuse_revoke: true,
            ..fresh()
        },
        Mock {
            refuse_revoke: true,
            ..fresh()
        },
    ] {
        let (base, _) = serve(Mock {
            echo_token: true,
            ..m
        })
        .await;
        let host = FakeHost::default();
        let p = plan_with(&api(&base), &host).await;
        seen.push(format!("{p:?}"));
        let reviewed = p.as_ref().map(|p| p.version.clone()).unwrap_or_default();
        seen.push(serde_json::to_string(&p.ok()).unwrap());
        let out = apply_with(&api(&base), &host, &reviewed, &[]).await;
        seen.push(format!("{out:?}"));
        seen.push(serde_json::to_string(&out).unwrap());
        for (id, secret) in host.saved.lock().unwrap().iter() {
            seen.push(format!("{id} {secret}"));
        }
    }
    // Errors straight from Tailscale's answers.
    let (base, _) = serve(Mock {
        echo_token: true,
        ..fresh()
    })
    .await;
    let a = api(&base);
    let e = a.get("/nowhere").await.unwrap_err();
    assert!(
        e.message.contains("•••"),
        "the mock echoed it: {}",
        e.message
    );
    seen.push(e.to_string());
    seen.push(format!("{e:?}"));
    seen.push(say(&e, "do it"));
    // Unreachable Tailscale.
    let e = Api::at("http://127.0.0.1:9", Token::new(TOKEN))
        .get("/x")
        .await
        .unwrap_err();
    seen.push(e.to_string());
    seen.push(format!(
        "{:?}",
        Api::at("http://x", Token::new(TOKEN)).token
    ));

    for s in &seen {
        assert!(!leaks(s), "the token shows in: {s}");
    }
    let logs = String::from_utf8(logs.0.lock().unwrap().clone()).unwrap();
    assert!(
        logs.contains("tailscale setup planned"),
        "the logs were captured: {}",
        &logs[..logs.len().min(2000)]
    );
    assert!(!leaks(&logs), "the token shows in the logs");
}

#[tokio::test]
async fn a_policy_pyo_cant_read_stops_the_plan_with_why() {
    let _one = ONE.lock().await;
    let (base, st) = serve(Mock {
        policy: "{ \"tagOwners\": ".into(),
        ..fresh()
    })
    .await;
    let e = plan_with(&api(&base), &FakeHost::default())
        .await
        .unwrap_err();
    assert!(
        e.starts_with("Pyo couldn't read your tailnet policy"),
        "{e}"
    );
    assert!(writes(&st.lock().unwrap()).is_empty());
}

/// The team's old OAuth clients go with the pasted token once the setup is
/// done, before the token: one deleted, one Tailscale doesn't know, and
/// never the one the cluster keeps now.
#[tokio::test]
async fn old_clients_are_deleted_before_the_token() {
    let _one = ONE.lock().await;
    let (base, st) = serve(Mock {
        old: vec!["kOld1".into(), "kOld2".into()],
        devices: vec!["web-1.tail9.ts.net".into()],
        ..fresh()
    })
    .await;
    let host = FakeHost::default();
    let p = plan_with(&api(&base), &host).await.unwrap();
    let ids = ["kOld1", "kGone", "kOld2", "kPyo1CNTRL"].map(str::to_owned);
    let out = apply_with(&api(&base), &host, &p.version, &ids).await;
    assert!(out.ok && out.token_deleted, "{out:?}");
    let states: Vec<(&str, &str)> = out
        .old_clients
        .iter()
        .map(|c| (c.client_id.as_str(), c.state.as_str()))
        .collect();
    assert_eq!(
        states,
        [
            ("kOld1", "deleted"),
            ("kGone", "missing"),
            ("kOld2", "deleted"),
            ("kPyo1CNTRL", "kept")
        ]
    );
    assert_eq!(out.tailnet.as_deref(), Some("tail9.ts.net"));
    let m = st.lock().unwrap();
    assert_eq!(
        m.deleted,
        ["kOld1", "kOld2", "kTok1CNTRL"],
        "the token last"
    );
    // What Pyo reads.
    let v = serde_json::to_value(&out).unwrap();
    assert_eq!(
        v["oldClients"][1],
        json!({ "clientId": "kGone", "state": "missing" })
    );
}

/// A token that can't delete OAuth clients: the setup is done anyway, and
/// each old client says why it's still there.
#[tokio::test]
async fn a_token_that_cant_delete_clients_leaves_them() {
    let _one = ONE.lock().await;
    let (base, st) = serve(Mock {
        old: vec!["kOld1".into()],
        refuse_delete_clients: true,
        ..fresh()
    })
    .await;
    let host = FakeHost::default();
    let p = plan_with(&api(&base), &host).await.unwrap();
    let out = apply_with(&api(&base), &host, &p.version, &["kOld1".to_owned()]).await;
    assert!(out.ok && out.token_deleted, "{out:?}");
    assert_eq!(out.old_clients[0].state, "failed");
    assert_eq!(
        out.old_clients[0].reason.as_deref(),
        Some(CANT_DELETE_CLIENTS)
    );
    assert_eq!(st.lock().unwrap().deleted, ["kTok1CNTRL"]);
}

/// A setup that stops deletes no old client: the team may still need it.
#[tokio::test]
async fn a_failed_setup_deletes_no_old_client() {
    let _one = ONE.lock().await;
    let (base, st) = serve(Mock {
        old: vec!["kOld1".into()],
        refuse_client: true,
        ..fresh()
    })
    .await;
    let host = FakeHost::default();
    let p = plan_with(&api(&base), &host).await.unwrap();
    let out = apply_with(&api(&base), &host, &p.version, &["kOld1".to_owned()]).await;
    assert!(!out.ok);
    assert!(out.old_clients.is_empty());
    assert!(!st.lock().unwrap().deleted.contains(&"kOld1".to_owned()));
}
