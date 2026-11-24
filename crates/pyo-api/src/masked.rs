//! Values a read shows hidden: the MCP tools show environment variables as
//! `•••`, and a database's connection details hide its password as
//! `••••••••`. A client that saves back what it read (an assistant copying
//! `get_state` into `save_service`, say) must never store the mask as the
//! value. A hidden value sent back keeps the saved one. A value with a
//! hidden part is refused, unless it's the saved value as it was.

use serde_json::Value;

use crate::error::{ApiError, ApiResult};

const BULLET: char = '•';
/// The shortest mask: what the MCP tools show.
const MASK: &str = "•••";

/// The whole value is hidden: bullets only (`•••`, `••••••••`).
pub(crate) fn is_mask(v: &str) -> bool {
    let t = v.trim();
    t.chars().count() >= 3 && t.chars().all(|c| c == BULLET)
}

/// Part of the value is hidden, like a connection URL's password.
pub(crate) fn has_mask(v: &str) -> bool {
    v.contains(MASK)
}

/// The saved variable a sent one stands for: by id, else by key.
fn saved(e: &Value, old: &[Value]) -> Option<usize> {
    old.iter()
        .position(|o| o["id"].is_string() && o["id"] == e["id"])
        .or_else(|| old.iter().position(|o| o["key"] == e["key"]))
}

/// One list of variables. `old` is as stored (sealed values), `opened` the
/// same list with its values in the clear. Returns the keys of secrets made
/// plain without a new value, as keep_secret_values does.
fn keep(env: &mut [Value], old: &[Value], opened: &[Value]) -> Result<Vec<String>, String> {
    let mut unhidden = vec![];
    for e in env.iter_mut() {
        let Some(v) = e["value"].as_str().filter(|v| has_mask(v)) else {
            continue;
        };
        let key = e["key"].as_str().unwrap_or("").to_owned();
        let at = saved(e, old);
        if is_mask(v) {
            let Some(o) = at.map(|i| &old[i]) else {
                return Err(format!(
                    "{key}'s value is hidden ({MASK}). Send its real value."
                ));
            };
            if o["secret"] == Value::Bool(true) && e["secret"] != Value::Bool(true) {
                unhidden.push(key);
            }
            e["value"] = Value::String(o["value"].as_str().unwrap_or("").to_owned());
        } else if at
            .and_then(|i| opened.get(i))
            .and_then(|o| o["value"].as_str())
            != Some(v)
        {
            return Err(format!(
                "{key} has a hidden part ({MASK}). Send its full value."
            ));
        }
    }
    Ok(unhidden)
}

/// A service's variables and its previews' as a client sent them, against
/// the saved spec (`Value::Null` for a new service): a hidden value keeps
/// the saved one, and a value with a hidden part is refused.
pub(crate) async fn keep_hidden_env(spec: &mut Value, old: &Value) -> ApiResult<Vec<String>> {
    let mut unhidden = vec![];
    for path in ["/env", "/previews/env"] {
        let Some(env) = spec.pointer_mut(path).and_then(Value::as_array_mut) else {
            continue;
        };
        let old_env = old
            .pointer(path)
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let partial = env.iter().any(|e| {
            e["value"]
                .as_str()
                .is_some_and(|v| has_mask(v) && !is_mask(v))
        });
        // Opened only when a value has to be compared in the clear.
        let opened = if partial && !old_env.is_empty() {
            let mut doc = serde_json::json!({ "env": old_env.clone() });
            crate::vault::get().open_env(&mut doc).await?;
            doc["env"].as_array().cloned().unwrap_or_default()
        } else {
            old_env.clone()
        };
        unhidden.extend(keep(env, &old_env, &opened).map_err(ApiError::bad_request)?);
    }
    Ok(unhidden)
}

/// Credentials an operation takes. Reads never show them, so one sent
/// hidden is refused: there's no value of it a client could have read.
const CREDENTIALS: [&str; 9] = [
    "password",
    "adminPassword",
    "token",
    "secret",
    "secretKey",
    "privateKey",
    "credential",
    "accessKey",
    "webhookUrl",
];

/// An operation's credentials are never a hidden value (`•••`).
pub(crate) fn no_hidden_credentials(op: &Value) -> ApiResult<()> {
    for (k, v) in op.as_object().into_iter().flatten() {
        if CREDENTIALS.contains(&k.as_str()) && v.as_str().is_some_and(has_mask) {
            return Err(ApiError::bad_request(format!(
                "{k} is hidden ({MASK}). Send the real value."
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn masks() {
        for m in ["•••", "••••••••", " ••• "] {
            assert!(is_mask(m), "{m}");
        }
        for v in ["", "•", "••", "a•••", "secret", "***"] {
            assert!(!is_mask(v), "{v}");
        }
        assert!(has_mask("postgres://app:••••••••@db:5432/app"));
        assert!(!has_mask("price • 3"));
    }

    fn env(v: Value) -> Vec<Value> {
        v.as_array().unwrap().clone()
    }

    #[test]
    fn a_hidden_value_keeps_the_saved_one() {
        let old = env(json!([
            { "id": "e1", "key": "API_URL", "value": "pyo1:k:sealed-url" },
            { "id": "e2", "key": "TOKEN", "value": "pyo1:k:sealed-token", "secret": true },
            { "id": "e3", "key": "EMPTY", "value": "" },
        ]));
        // As get_state shows them, saved back: by id, by key, secret or not.
        let mut sent = env(json!([
            { "id": "e1", "key": "API_URL", "value": "•••" },
            { "key": "TOKEN", "value": "•••", "secret": true },
            { "id": "e3", "key": "EMPTY", "value": "••••••••" },
            { "id": "e4", "key": "NEW", "value": "real" },
        ]));
        assert!(keep(&mut sent, &old, &old).unwrap().is_empty());
        let values: Vec<&str> = sent.iter().map(|e| e["value"].as_str().unwrap()).collect();
        assert_eq!(
            values,
            ["pyo1:k:sealed-url", "pyo1:k:sealed-token", "", "real"]
        );
    }

    #[test]
    fn a_hidden_secret_made_plain_is_reported() {
        let old = env(json!([{ "id": "e1", "key": "TOKEN", "value": "pyo1:k:x", "secret": true }]));
        let mut sent =
            env(json!([{ "id": "e1", "key": "TOKEN", "value": "•••", "secret": false }]));
        assert_eq!(keep(&mut sent, &old, &old).unwrap(), ["TOKEN"]);
        assert_eq!(sent[0]["value"], "pyo1:k:x");
    }

    #[test]
    fn a_hidden_value_with_nothing_saved_is_refused() {
        let mut sent = env(json!([{ "key": "API_KEY", "value": "•••" }]));
        assert_eq!(
            keep(&mut sent, &[], &[]).unwrap_err(),
            "API_KEY's value is hidden (•••). Send its real value."
        );
    }

    #[test]
    fn a_value_with_a_hidden_part_is_refused_unless_unchanged() {
        let old = env(json!([{ "id": "e1", "key": "DATABASE_URL", "value": "pyo1:k:sealed" }]));
        let opened = env(
            json!([{ "id": "e1", "key": "DATABASE_URL", "value": "postgres://a:••••••••@db/a" }]),
        );
        // A connection URL copied with its password hidden.
        let mut sent = env(
            json!([{ "id": "e1", "key": "DATABASE_URL", "value": "postgres://a:••••••••@db/b" }]),
        );
        assert_eq!(
            keep(&mut sent, &old, &opened).unwrap_err(),
            "DATABASE_URL has a hidden part (•••). Send its full value."
        );
        // The saved value as it was (it happens to have bullets) is fine.
        let mut sent = env(
            json!([{ "id": "e1", "key": "DATABASE_URL", "value": "postgres://a:••••••••@db/a" }]),
        );
        assert!(keep(&mut sent, &old, &opened).is_ok());
        assert_eq!(sent[0]["value"], "postgres://a:••••••••@db/a");
    }

    #[test]
    fn a_hidden_credential_is_refused() {
        let e = no_hidden_credentials(
            &json!({ "op": "saveRegistryLogin", "host": "ghcr.io", "username": "u", "password": "••••••••" }),
        )
        .unwrap_err();
        assert_eq!(e.message, "password is hidden (•••). Send the real value.");
        assert!(
            no_hidden_credentials(&json!({ "op": "connectCloudflare", "token": "real-token" }))
                .is_ok()
        );
        // A service's variables are kept or refused by keep_hidden_env.
        assert!(no_hidden_credentials(&json!({ "op": "saveService", "service": { "env": [{ "key": "A", "value": "•••" }] } })).is_ok());
    }

    #[tokio::test]
    async fn previews_variables_too() {
        let old = json!({ "env": [], "previews": { "env": [{ "id": "p1", "key": "STAGE", "value": "pyo1:k:p" }] } });
        let mut spec = json!({ "env": [], "previews": { "env": [{ "id": "p1", "key": "STAGE", "value": "•••" }] } });
        keep_hidden_env(&mut spec, &old).await.unwrap();
        assert_eq!(spec["previews"]["env"][0]["value"], "pyo1:k:p");
        // A new service has nothing saved to keep.
        let mut spec = json!({ "env": [{ "key": "STAGE", "value": "•••" }] });
        assert!(keep_hidden_env(&mut spec, &Value::Null).await.is_err());
    }
}
