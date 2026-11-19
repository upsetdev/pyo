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

/// The installation, as the setup sees it.
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
            grant: false,
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
            grant: true,
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
    assert_eq!(reaches_pyo(&v), Some(true));
}

#[test]
fn approvers_for_tag_pyo_get_tag_pyo_added() {
    let p = r#"{ "autoApprovers": { "services": { "tag:pyo": ["group:ops"] } }, "tagOwners": { "tag:pyo": [] }, "grants": [{ "src": ["*"], "dst": ["*"], "ip": ["*"] }] }"#;
    let n = needs(&parse(p).unwrap());
    assert!(n.approver && !n.tag_owner && !n.grant);
    let v = parse(&patch(p, &n).unwrap()).unwrap();
    assert_eq!(
        v["autoApprovers"]["services"][TAG],
        json!(["group:ops", TAG])
    );
}

#[test]
fn what_lets_the_tailnet_reach_pyo() {
    let r = |p: Value| reaches_pyo(&p);
    assert_eq!(
        r(json!({ "grants": [{ "src": ["*"], "dst": ["*"], "ip": ["*"] }] })),
        Some(true)
    );
    assert_eq!(
        r(json!({ "acls": [{ "action": "accept", "src": ["*"], "dst": ["*:*"] }] })),
        Some(true)
    );
    assert_eq!(
        r(json!({ "acls": [{ "action": "accept", "users": ["*"], "ports": ["*:*"] }] })),
        Some(true)
    );
    assert_eq!(
        r(
            json!({ "grants": [{ "src": ["autogroup:member"], "dst": [TAG], "ip": ["tcp:80", "443"] }] })
        ),
        Some(true)
    );
    assert_eq!(
        r(json!({ "acls": [{ "action": "accept", "src": ["*"], "dst": ["tag:pyo:80,443"] }] })),
        Some(true)
    );
    assert_eq!(
        r(json!({ "acls": [{ "action": "accept", "src": ["*"], "dst": ["tag:pyo:1-1024"] }] })),
        Some(true)
    );
    // Split over two rules.
    assert_eq!(
        r(json!({ "grants": [
            { "src": ["*"], "dst": [TAG], "ip": ["tcp:80"] },
            { "src": ["*"], "dst": ["*"], "ip": ["tcp:443"] },
        ] })),
        Some(true)
    );
    // Only some people, a port missing, UDP, posture: not plainly open.
    assert_eq!(
        r(json!({ "grants": [{ "src": ["group:eng"], "dst": ["*"], "ip": ["*"] }] })),
        Some(false)
    );
    assert_eq!(
        r(json!({ "grants": [{ "src": ["*"], "dst": [TAG], "ip": ["tcp:443"] }] })),
        Some(false)
    );
    assert_eq!(
        r(json!({ "grants": [{ "src": ["*"], "dst": [TAG], "ip": ["udp:*"] }] })),
        Some(false)
    );
    assert_eq!(
        r(
            json!({ "grants": [{ "src": ["*"], "dst": ["*"], "ip": ["*"], "srcPosture": ["posture:x"] }] })
        ),
        Some(false)
    );
    assert_eq!(
        r(
            json!({ "acls": [{ "action": "accept", "src": ["*"], "proto": "udp", "dst": ["*:*"] }] })
        ),
        Some(false)
    );
    assert_eq!(r(json!({ "acls": [] })), Some(false));
    // No rules at all: Pyo doesn't write the first one.
    assert_eq!(r(json!({ "tagOwners": {} })), None);
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
            needs: Needs::default(),
            reaches: Some(true),
            patched: None,
        },
        magic_dns: true,
        https: Some(true),
        client: true,
    };
    let p = plan_of(&s);
    assert!(p.changes.is_empty(), "{p:?}");
    assert!(p.done.contains(&"Your tailnet can reach tag:pyo".to_owned()));
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

    let out = apply_with(&api(&base), &host).await.unwrap();
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
    let out = apply_with(&api(&base), &host).await.unwrap();
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
    let out = apply_with(&api(&base), &host).await.unwrap();
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
    let out = apply_with(&api(&base), &host).await.unwrap();
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
    let e = apply_with(&api(&base), &host).await.unwrap_err();
    assert!(e.contains("managed outside"), "{e}");
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
    let out = apply_with(&api(&base), &host).await.unwrap();
    assert!(out.ok, "{out:?}");
    assert!(st.lock().unwrap().written.is_empty());
}

#[tokio::test]
async fn a_policy_changed_meanwhile_is_read_again_once() {
    let _one = ONE.lock().await;
    let (base, st) = serve(Mock {
        races: 1,
        https: true,
        ..fresh()
    })
    .await;
    let out = apply_with(&api(&base), &FakeHost::default()).await.unwrap();
    assert!(out.ok, "{out:?}");
    let m = st.lock().unwrap();
    // The other edit is kept, under Pyo's.
    assert!(m.policy.contains("// edited meanwhile"));
    assert!(parse(&m.policy).unwrap()["tagOwners"].get(TAG).is_some());
    assert_eq!(
        m.log.iter().filter(|l| *l == "POST /tailnet/-/acl").count(),
        2
    );
}

#[tokio::test]
async fn a_policy_that_keeps_changing_stops_with_what_was_done() {
    let _one = ONE.lock().await;
    let (base, st) = serve(Mock {
        races: 2,
        ..fresh()
    })
    .await;
    let out = apply_with(&api(&base), &FakeHost::default()).await.unwrap();
    assert!(!out.ok);
    assert_eq!(out.changed, ["Turned on HTTPS certificates"]);
    assert!(
        out.error
            .unwrap()
            .contains("changed while Pyo was editing it")
    );
    let m = st.lock().unwrap();
    assert!(m.written.is_empty() && m.clients.is_empty() && m.deleted.is_empty());
}

#[tokio::test]
async fn https_under_its_other_name() {
    let _one = ONE.lock().await;
    let (base, st) = serve(Mock {
        https_field: Some("httpsCertificates"),
        ..fresh()
    })
    .await;
    let out = apply_with(&api(&base), &FakeHost::default()).await.unwrap();
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
    let out = apply_with(&api(&base), &host).await.unwrap();
    assert!(!out.ok);
    assert_eq!(
        out.changed,
        [
            "Added tag:pyo to your policy",
            "Approved Pyo's Services on tag:pyo servers by policy",
        ]
    );
    let e = out.error.unwrap();
    assert!(
        e.contains("didn't let Pyo create its OAuth client") && e.contains("not allowed"),
        "{e}"
    );
    assert!(!out.token_deleted);
    assert!(host.saved.lock().unwrap().is_empty());
    // The token stays for another try.
    assert!(st.lock().unwrap().deleted.is_empty());
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
    let e = apply_with(&api(&base), &host).await.unwrap_err();
    assert!(e.contains("couldn't save its OAuth client"), "{e}");
    assert_eq!(st.lock().unwrap().deleted, ["kPyo1CNTRL"]);
}

#[tokio::test]
async fn a_token_that_cant_delete_itself_is_left_to_the_admin() {
    let _one = ONE.lock().await;
    let (base, _) = serve(Mock {
        refuse_revoke: true,
        ..fresh()
    })
    .await;
    let out = apply_with(&api(&base), &FakeHost::default()).await.unwrap();
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
    let e = apply_with(&api(&base), &host).await.unwrap_err();
    assert!(e.contains("needs a DNS nameserver"), "{e}");
    assert_eq!(
        writes(&st.lock().unwrap()),
        ["POST /tailnet/-/dns/preferences"]
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
        seen.push(serde_json::to_string(&p.ok()).unwrap());
        let out = apply_with(&api(&base), &host).await;
        seen.push(format!("{out:?}"));
        if let Ok(o) = &out {
            seen.push(serde_json::to_string(o).unwrap());
        }
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
    assert!(e.starts_with("Pyo couldn't read your tailnet policy"), "{e}");
    assert!(writes(&st.lock().unwrap()).is_empty());
}
