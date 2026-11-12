//! What a service deployed from a template records in its `source`, so it
//! can be upgraded later: the template's own version (`templateVersion`),
//! the app's (`appVersion`), each Compose service's image as the template
//! pinned it (`images`, repository:tag@digest by service name; the
//! template's own service is under the template's id), and the deploy
//! form's values (`fields`), never a secret one's. A companion the template
//! deployed next to it (an image service) has `companionOf` instead.
//!
//! The dashboard records these when it creates the service; saves keep them,
//! and whether the image has moved off the template's (`customized`) is
//! worked out for each response, never stored.

use serde_json::{Map, Value, json};

use crate::error::{ApiError, ApiResult};

/// A template service's record, in its `source`.
const RECORD: [&str; 4] = ["templateVersion", "appVersion", "images", "fields"];

/// A save keeps what the service was deployed from. The dashboard, MCP and
/// the CLI send the whole service, and one that leaves the record out
/// mustn't drop it; it goes only when the service stops coming from that
/// template (another template, or another kind of source).
pub fn keep_record(spec: &mut Value, old: &Value) {
    let old = &old["source"];
    let Some(new) = spec.get_mut("source").and_then(Value::as_object_mut) else {
        return;
    };
    let keys: &[&str] = match (
        old["type"].as_str(),
        new.get("type").and_then(Value::as_str),
    ) {
        (Some("template"), Some("template")) if new.get("templateId") == old.get("templateId") => {
            &RECORD
        }
        (Some("image"), Some("image")) => &["companionOf"],
        _ => return,
    };
    for k in keys {
        if !new.contains_key(*k)
            && let Some(v) = old.get(*k)
        {
            new.insert((*k).into(), v.clone());
        }
    }
}

/// The deploy form's values are stored in the clear. The dashboard records
/// a secret one only as set or not; this makes sure of it for every client:
/// a value that's one of the service's secret variables becomes that mark,
/// naming the variable, and a mark carries nothing else.
pub fn scrub_fields(spec: &mut Value) {
    let secrets: Vec<(String, String)> = spec["env"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|e| e["secret"] == json!(true))
        .filter_map(|e| {
            Some((
                e["key"].as_str()?.to_owned(),
                e["value"].as_str().filter(|v| !v.is_empty())?.to_owned(),
            ))
        })
        .collect();
    let Some(fields) = spec
        .get_mut("source")
        .and_then(|s| s.get_mut("fields"))
        .and_then(Value::as_object_mut)
    else {
        return;
    };
    let mark = |set: bool, env: Option<&str>| {
        let mut m = Map::new();
        m.insert("secret".into(), json!(true));
        m.insert("set".into(), json!(set));
        if let Some(env) = env {
            m.insert("env".into(), json!(env));
        }
        Value::Object(m)
    };
    for v in fields.values_mut() {
        let next = match &*v {
            Value::String(s) => match secrets.iter().find(|(_, value)| value == s) {
                Some((key, _)) => mark(true, Some(key)),
                None => continue,
            },
            Value::Object(o) => mark(
                o.get("set") == Some(&json!(true)),
                o.get("env")
                    .and_then(Value::as_str)
                    .filter(|e| is_env_key(e)),
            ),
            Value::Number(_) | Value::Bool(_) => continue,
            Value::Null | Value::Array(_) => Value::Null,
        };
        *v = next;
    }
    fields.retain(|_, v| !v.is_null());
}

fn is_env_key(k: &str) -> bool {
    !k.is_empty()
        && k.len() <= 128
        && !k.starts_with(|c: char| c.is_ascii_digit())
        && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// The record's shape and size, so it stays a small note on the service.
pub fn check(source: &Value) -> ApiResult<()> {
    let bad = |what: &str| {
        Err(ApiError::bad_request(format!(
            "The source's {what} isn't valid."
        )))
    };
    for k in ["templateVersion", "appVersion"] {
        if let Some(v) = source.get(k)
            && !v.as_str().is_some_and(|s| !s.is_empty() && s.len() <= 40)
        {
            return bad(k);
        }
    }
    if let Some(images) = source.get("images")
        && !images.as_object().is_some_and(|m| {
            m.len() <= 16
                && m.values()
                    .all(|v| v.as_str().is_some_and(|s| s.len() <= 512))
        })
    {
        return bad("images");
    }
    if let Some(fields) = source.get("fields")
        && !fields.as_object().is_some_and(|m| {
            m.len() <= 64
                && m.values()
                    .all(|v| v.as_str().is_none_or(|s| s.len() <= 4096))
        })
    {
        return bad("fields");
    }
    if let Some(c) = source.get("companionOf")
        && !(c.is_object()
            && ["templateId", "service", "image"].iter().all(|k| {
                c[k].as_str()
                    .is_some_and(|s| !s.is_empty() && s.len() <= 512)
            }))
    {
        return bad("companionOf");
    }
    Ok(())
}

/// Whether a template's service (or companion) no longer runs the image the
/// template gave it: another image, tag or digest. None for other services
/// and ones without a record.
pub fn customized(source: &Value) -> Option<bool> {
    let given = match source["type"].as_str()? {
        "template" => source["images"][source["templateId"].as_str()?].as_str()?,
        "image" => source["companionOf"]["image"].as_str()?,
        _ => return None,
    };
    let (named, digest) = match given.split_once('@') {
        Some((n, d)) => (n, Some(d)),
        None => (given, None),
    };
    // The tag is after the last ':' that's past the last '/' (a registry's port isn't one).
    let (repo, tag) = match named
        .rfind(':')
        .filter(|&i| i > named.rfind('/').unwrap_or(0))
    {
        Some(i) => (&named[..i], &named[i + 1..]),
        None => (named, ""),
    };
    let image = source["image"].as_str().unwrap_or("").trim();
    let reference = source["reference"].as_str().unwrap_or("").trim();
    let same = image == repo
        && if reference.starts_with("sha256:") {
            Some(reference) == digest
        } else {
            reference == tag
        };
    Some(!same)
}

/// A service as the API shows it: with `source.customized` when it has a record.
pub fn show(service: &mut Value) {
    if let Some(c) = customized(&service["source"])
        && let Some(src) = service.get_mut("source").and_then(Value::as_object_mut)
    {
        src.insert("customized".into(), json!(c));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const D: &str = "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    fn template() -> Value {
        json!({
            "name": "status",
            "source": {
                "type": "template", "templateId": "uptime-kuma", "image": "docker.io/louislam/uptime-kuma", "reference": "1.23.16",
                "templateVersion": "1.0.0", "appVersion": "1.23.16",
                "images": { "uptime-kuma": format!("docker.io/louislam/uptime-kuma:1.23.16@{D}") },
                "fields": { "retention": "15d", "admin": { "secret": true, "set": true, "env": "ADMIN_PASSWORD" } },
            },
            "env": [{ "key": "ADMIN_PASSWORD", "value": "hunter22", "secret": true }],
        })
    }

    #[test]
    fn a_save_without_the_record_keeps_it() {
        let old = template();
        let mut spec = json!({ "source": { "type": "template", "templateId": "uptime-kuma", "image": "docker.io/louislam/uptime-kuma", "reference": "2.0.0" } });
        keep_record(&mut spec, &old);
        for k in RECORD {
            assert_eq!(spec["source"][k], old["source"][k], "{k}");
        }
        assert_eq!(spec["source"]["reference"], "2.0.0");
        // One that sends its own keeps that.
        let mut spec = json!({ "source": { "type": "template", "templateId": "uptime-kuma", "templateVersion": "1.1.0" } });
        keep_record(&mut spec, &old);
        assert_eq!(spec["source"]["templateVersion"], "1.1.0");
        assert_eq!(spec["source"]["images"], old["source"]["images"]);
    }

    #[test]
    fn the_record_goes_with_the_template() {
        let old = template();
        for source in [
            json!({ "type": "image", "image": "docker.io/library/nginx", "reference": "1.27" }),
            json!({ "type": "template", "templateId": "grafana" }),
        ] {
            let mut spec = json!({ "source": source });
            keep_record(&mut spec, &old);
            assert!(
                spec["source"].get("templateVersion").is_none()
                    && spec["source"].get("fields").is_none()
            );
        }
    }

    #[test]
    fn a_companion_keeps_what_it_came_with() {
        let old = json!({ "source": { "type": "image", "image": "docker.io/clickhouse/clickhouse-server", "reference": "24.8",
            "companionOf": { "templateId": "plausible", "templateVersion": "1.0.0", "service": "plausible-events", "image": format!("docker.io/clickhouse/clickhouse-server:24.8@{D}") } } });
        let mut spec = json!({ "source": { "type": "image", "image": "docker.io/clickhouse/clickhouse-server", "reference": "24.9" } });
        keep_record(&mut spec, &old);
        assert_eq!(spec["source"]["companionOf"], old["source"]["companionOf"]);
        assert_eq!(customized(&old["source"]), Some(false));
        assert_eq!(customized(&spec["source"]), Some(true));
    }

    #[test]
    fn secret_field_values_are_never_stored() {
        let mut spec = template();
        // A client that sent the secret's value in the clear.
        spec["source"]["fields"]["admin"] = json!("hunter22");
        // And a mark with something extra in it.
        spec["source"]["fields"]["token"] =
            json!({ "secret": true, "set": true, "value": "leaked", "env": "not a key" });
        spec["source"]["fields"]["list"] = json!(["x"]);
        scrub_fields(&mut spec);
        let fields = &spec["source"]["fields"];
        assert_eq!(
            fields["admin"],
            json!({ "secret": true, "set": true, "env": "ADMIN_PASSWORD" })
        );
        assert_eq!(fields["token"], json!({ "secret": true, "set": true }));
        assert_eq!(fields["retention"], "15d");
        assert!(fields.get("list").is_none());
        assert!(
            !spec["source"].to_string().contains("hunter22")
                && !spec["source"].to_string().contains("leaked")
        );
    }

    #[test]
    fn customized_is_the_image_moving_off_the_templates() {
        let s = template()["source"].clone();
        assert_eq!(customized(&s), Some(false));
        let with = |k: &str, v: &str| {
            let mut s = s.clone();
            s[k] = json!(v);
            customized(&s)
        };
        assert_eq!(with("reference", "2.0.0"), Some(true));
        assert_eq!(with("reference", D), Some(false));
        assert_eq!(with("reference", "sha256:bbbb"), Some(true));
        assert_eq!(with("image", "ghcr.io/someone/uptime-kuma"), Some(true));
        // A registry with a port isn't a tag.
        let mut p = s.clone();
        p["image"] = json!("registry.local:5000/kuma");
        p["images"]["uptime-kuma"] = json!(format!("registry.local:5000/kuma:1.23.16@{D}"));
        assert_eq!(customized(&p), Some(false));
        // Nothing to compare with.
        assert_eq!(
            customized(
                &json!({ "type": "template", "templateId": "x", "image": "a", "reference": "1" })
            ),
            None
        );
        assert_eq!(customized(&json!({ "type": "git" })), None);
        let mut shown = json!({ "source": s });
        show(&mut shown);
        assert_eq!(shown["source"]["customized"], false);
    }

    #[test]
    fn the_record_is_checked() {
        assert!(check(&template()["source"]).is_ok());
        assert!(check(&json!({ "type": "image" })).is_ok());
        assert!(check(&json!({ "templateVersion": 1 })).is_err());
        assert!(check(&json!({ "images": ["x"] })).is_err());
        assert!(check(&json!({ "fields": { "a": "x".repeat(5000) } })).is_err());
        assert!(check(&json!({ "companionOf": { "templateId": "plausible" } })).is_err());
    }
}
