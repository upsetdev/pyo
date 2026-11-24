//! A Kubernetes API server in memory, for tests that run the agent's
//! writes end to end without a cluster: objects by path, server-side apply
//! as a replace, merge patches, deletes, lists with label selectors, and an
//! admission check each test sets (an older admission policy, say). A dry
//! run is checked and not stored.

use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};

use axum::{
    Router,
    body::Bytes,
    extract::State,
    http::{HeaderMap, Method, StatusCode, Uri, header},
    response::{IntoResponse, Response},
};
use serde_json::{Value, json};

/// A write the server was asked for.
#[derive(Debug, Clone)]
pub struct Write {
    /// "PATCH", "POST", "PUT" or "DELETE".
    pub method: String,
    /// The resource, as in its path ("clienttrafficpolicies").
    pub plural: String,
    pub name: String,
    pub body: Value,
}

/// The admission check: None lets a write through, Some((code, message))
/// refuses it with that status.
pub type Admit = Arc<dyn Fn(&Write) -> Option<(u16, String)> + Send + Sync>;

/// (API prefix such as "apis/apps/v1", resource, namespace, name).
type Key = (String, String, String, String);

struct Inner {
    objects: BTreeMap<Key, Value>,
    refused: Vec<Write>,
    admit: Admit,
    /// Answer anything in a namespace that isn't there with 403, as the
    /// agent's roles (bound per namespace) get from Kubernetes.
    forbid_missing_namespaces: bool,
}

#[derive(Clone)]
pub struct FakeKube(Arc<Mutex<Inner>>);

/// A path's (prefix, namespace, resource, name).
fn parse(path: &str) -> Option<(String, String, String, String)> {
    let seg: Vec<&str> = path.trim_matches('/').split('/').collect();
    let (prefix, rest) = match *seg.first()? {
        "api" if seg.len() >= 2 => (seg[..2].join("/"), &seg[2..]),
        "apis" if seg.len() >= 3 => (seg[..3].join("/"), &seg[3..]),
        _ => return None,
    };
    let at = |i: usize| rest.get(i).map(|s| s.to_string()).unwrap_or_default();
    Some(match rest {
        ["namespaces", _, _, ..] => (prefix, at(1), at(2), at(3)),
        _ => (prefix, String::new(), at(0), at(1)),
    })
}

fn decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = vec![];
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'%' if i + 2 < b.len() => {
                let hex = std::str::from_utf8(&b[i + 1..i + 3]).unwrap_or("");
                match u8::from_str_radix(hex, 16) {
                    Ok(v) => {
                        out.push(v);
                        i += 3;
                        continue;
                    }
                    Err(_) => out.push(b'%'),
                }
            }
            b'+' => out.push(b' '),
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn query(uri: &Uri, key: &str) -> Option<String> {
    uri.query()?.split('&').find_map(|kv| {
        let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
        (decode(k) == key).then(|| decode(v))
    })
}

/// Whether an object's labels match a label selector (`a=b`, `a!=b`, `a`,
/// `!a`, joined by commas).
fn selected(obj: &Value, selector: &str) -> bool {
    let labels = &obj["metadata"]["labels"];
    selector.split(',').filter(|t| !t.is_empty()).all(|t| {
        if let Some((k, v)) = t.split_once("!=") {
            labels[k.trim()].as_str() != Some(v.trim())
        } else if let Some((k, v)) = t.split_once('=') {
            labels[k.trim()].as_str() == Some(v.trim_start_matches('=').trim())
        } else if let Some(k) = t.strip_prefix('!') {
            labels.get(k.trim()).is_none()
        } else {
            labels.get(t.trim()).is_some()
        }
    })
}

fn json_response(code: u16, body: Value) -> Response {
    (
        StatusCode::from_u16(code).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
        [(header::CONTENT_TYPE, "application/json")],
        body.to_string(),
    )
        .into_response()
}

fn status(code: u16, reason: &str, message: &str) -> Response {
    json_response(
        code,
        json!({ "kind": "Status", "apiVersion": "v1", "metadata": {}, "status": if code < 400 { "Success" } else { "Failure" }, "message": message, "reason": reason, "code": code }),
    )
}

/// RFC 7386: `patch` merged into `into`.
fn merge(into: &mut Value, patch: &Value) {
    match (into, patch) {
        (Value::Object(a), Value::Object(b)) => {
            for (k, v) in b {
                if v.is_null() {
                    a.remove(k);
                } else {
                    merge(a.entry(k.clone()).or_insert(Value::Null), v);
                }
            }
        }
        (into, patch) => *into = patch.clone(),
    }
}

async fn handle(
    State(fake): State<FakeKube>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if uri.path() == "/version" {
        return json_response(
            200,
            json!({ "major": "1", "minor": "33", "gitVersion": "v1.33.0" }),
        );
    }
    let Some((prefix, ns, plural, name)) = parse(uri.path()) else {
        return status(404, "NotFound", "no such path");
    };
    let mut inner = fake.0.lock().unwrap();
    let ns_key = (
        "api/v1".to_owned(),
        "namespaces".to_owned(),
        String::new(),
        ns.clone(),
    );
    if inner.forbid_missing_namespaces && !ns.is_empty() && !inner.objects.contains_key(&ns_key) {
        return status(
            403,
            "Forbidden",
            &format!("{plural} is forbidden: the agent can't reach namespace \"{ns}\""),
        );
    }
    let key = (prefix.clone(), plural.clone(), ns.clone(), name.clone());
    if method == Method::GET {
        if !name.is_empty() {
            return match inner.objects.get(&key) {
                Some(o) => json_response(200, o.clone()),
                None => status(404, "NotFound", &format!("{plural} \"{name}\" not found")),
            };
        }
        let selector = query(&uri, "labelSelector").unwrap_or_default();
        let items: Vec<Value> = inner
            .objects
            .iter()
            .filter(|((p, r, n, _), _)| *p == prefix && *r == plural && (ns.is_empty() || *n == ns))
            .map(|(_, o)| o.clone())
            .filter(|o| selected(o, &selector))
            .collect();
        return json_response(
            200,
            json!({ "apiVersion": "v1", "kind": "List", "metadata": { "resourceVersion": "1" }, "items": items }),
        );
    }
    let mut obj: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let name = if name.is_empty() {
        obj["metadata"]["name"].as_str().unwrap_or("").to_owned()
    } else {
        name
    };
    let key = (prefix, plural.clone(), ns.clone(), name.clone());
    let write = Write {
        method: method.to_string(),
        plural: plural.clone(),
        name: name.clone(),
        body: obj.clone(),
    };
    if method == Method::DELETE {
        return match inner.objects.remove(&key) {
            Some(_) => status(200, "", "deleted"),
            None => status(404, "NotFound", &format!("{plural} \"{name}\" not found")),
        };
    }
    let merge_patch = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|t| t.contains("merge-patch"));
    if merge_patch {
        let Some(have) = inner.objects.get(&key) else {
            return status(404, "NotFound", &format!("{plural} \"{name}\" not found"));
        };
        let mut merged = have.clone();
        merge(&mut merged, &obj);
        obj = merged;
    }
    let refusal = (inner.admit)(&Write {
        body: obj.clone(),
        ..write.clone()
    });
    if let Some((code, message)) = refusal {
        inner.refused.push(write);
        return status(
            code,
            if code == 403 { "Forbidden" } else { "Invalid" },
            &message,
        );
    }
    obj["metadata"]["name"] = json!(name);
    if !ns.is_empty() {
        obj["metadata"]["namespace"] = json!(ns);
    }
    if obj["metadata"]["creationTimestamp"].is_null() {
        obj["metadata"]["creationTimestamp"] = json!("2026-01-01T00:00:00Z");
    }
    obj["metadata"]["resourceVersion"] = json!("1");
    // A dry run goes through admission and writes nothing.
    if query(&uri, "dryRun").is_none() {
        inner.objects.insert(key, obj.clone());
    }
    json_response(if method == Method::POST { 201 } else { 200 }, obj)
}

impl FakeKube {
    /// The server, and a client of it. Every write goes through until
    /// [`FakeKube::admit`] says otherwise.
    pub async fn start() -> (FakeKube, kube::Client) {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        let fake = FakeKube(Arc::new(Mutex::new(Inner {
            objects: BTreeMap::new(),
            refused: vec![],
            admit: Arc::new(|_| None),
            forbid_missing_namespaces: false,
        })));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let app = Router::new().fallback(handle).with_state(fake.clone());
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        let config = kube::Config::new(url.parse().unwrap());
        (fake, kube::Client::try_from(config).unwrap())
    }

    /// The admission check from now on.
    pub fn admit(&self, f: impl Fn(&Write) -> Option<(u16, String)> + Send + Sync + 'static) {
        self.0.lock().unwrap().admit = Arc::new(f);
    }

    /// From now on, anything in a namespace that isn't there is refused
    /// with 403 (the agent's rights), not 404.
    pub fn forbid_missing_namespaces(&self) {
        self.0.lock().unwrap().forbid_missing_namespaces = true;
    }

    /// Put an object at its path ("apis/apps/v1/namespaces/shop/deployments/web").
    pub fn put(&self, path: &str, mut obj: Value) {
        let (prefix, ns, plural, name) = parse(path).expect("a resource path");
        obj["metadata"]["name"] = json!(name);
        if !ns.is_empty() {
            obj["metadata"]["namespace"] = json!(ns);
        }
        self.0
            .lock()
            .unwrap()
            .objects
            .insert((prefix, plural, ns, name), obj);
    }

    /// The object at a path, as the server has it.
    pub fn get(&self, path: &str) -> Option<Value> {
        let (prefix, ns, plural, name) = parse(path)?;
        self.0
            .lock()
            .unwrap()
            .objects
            .get(&(prefix, plural, ns, name))
            .cloned()
    }

    /// The writes the admission check refused, oldest first.
    pub fn refused(&self) -> Vec<Write> {
        self.0.lock().unwrap().refused.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_and_selectors() {
        assert_eq!(
            parse(
                "/apis/gateway.envoyproxy.io/v1alpha1/namespaces/pyo-gateway/clienttrafficpolicies/pyo-http"
            ),
            Some((
                "apis/gateway.envoyproxy.io/v1alpha1".into(),
                "pyo-gateway".into(),
                "clienttrafficpolicies".into(),
                "pyo-http".into()
            ))
        );
        assert_eq!(
            parse("/api/v1/namespaces/shop/services"),
            Some((
                "api/v1".into(),
                "shop".into(),
                "services".into(),
                String::new()
            ))
        );
        assert_eq!(
            parse("/api/v1/namespaces/shop"),
            Some((
                "api/v1".into(),
                String::new(),
                "namespaces".into(),
                "shop".into()
            ))
        );
        assert_eq!(
            parse("/apis/admissionregistration.k8s.io/v1/validatingadmissionpolicies/pyo-agent"),
            Some((
                "apis/admissionregistration.k8s.io/v1".into(),
                String::new(),
                "validatingadmissionpolicies".into(),
                "pyo-agent".into()
            ))
        );
        assert_eq!(decode("a%3Db%2Cc"), "a=b,c");
        let o = json!({ "metadata": { "labels": { "a": "1", "b": "2" } } });
        assert!(selected(&o, "a=1,b"));
        assert!(selected(&o, "a==1,!c"));
        assert!(!selected(&o, "a!=1"));
        assert!(!selected(&o, "c"));
        assert!(selected(&o, ""));
    }
}
