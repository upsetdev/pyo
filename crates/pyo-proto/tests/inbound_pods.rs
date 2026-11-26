//! The pods every request to a cluster passes through restart when their
//! settings change: Pyo Gateway's proxies (Envoy), the WAF and the
//! Cloudflare Tunnel connector. A restart of the proxies closes open
//! WebSockets, so it has to be meant, and the release notes have to say so.
//!
//! `testdata/inbound-pods.json` holds what the last recorded change left
//! under `pods`: the settings Pyo writes for those pods, and the tags of
//! the two images a release doesn't tag with its version
//! (`scripts/image-tags.sh`). This test fails while the code gives anything
//! else. When the change is meant, record it with the reason:
//!
//! ```sh
//! PYO_INBOUND_PODS_NOTE='Envoy 1.39.6' cargo test -p pyo-proto --test inbound_pods
//! ```
//!
//! That rewrites `pods` and adds the note, with today's date, to `changes`.
//! Commit the file with the change, and say in the release notes which pods
//! restart ([`RESTARTS`]).

use pyo_proto::system;
use serde_json::{Map, Value, json};
use std::path::{Path, PathBuf};

const NOTE: &str = "PYO_INBOUND_PODS_NOTE";

/// Each part of `pods`, and what a change to it restarts.
const RESTARTS: &[(&str, &str)] = &[
    (
        "images",
        "a new geoip tag restarts Pyo Gateway's proxies, the WAF and the traffic store, a new waf tag the WAF",
    ),
    (
        "envoyGateway",
        "Pyo Gateway's proxies restart, and open WebSockets reconnect",
    ),
    (
        "proxies",
        "Pyo Gateway's proxies restart, and open WebSockets reconnect",
    ),
    ("waf", "the WAF restarts"),
    ("tunnel", "the Cloudflare Tunnel connector restarts"),
];

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn snapshot() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata/inbound-pods.json")
}

/// An image's tag from `scripts/image-tags.sh`.
fn tag(image: &str) -> String {
    let out = std::process::Command::new("bash")
        .arg(root().join("scripts/image-tags.sh"))
        .arg(image)
        .output()
        .expect("bash runs");
    assert!(
        out.status.success(),
        "scripts/image-tags.sh {image}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

/// What the code gives now. The WAF's and the GeoIP database's images come
/// from the installation, so their place holds a name here and `images`
/// holds their tags.
fn pods() -> Value {
    let mut proxies = system::envoy_proxy("<geoip>", true)["spec"].clone();
    // The proxies' Service isn't part of their pods.
    proxies["provider"]["kubernetes"]
        .as_object_mut()
        .expect("the proxies' settings")
        .remove("envoyService");
    sorted(&json!({
        "images": { "geoip": tag("geoip"), "waf": tag("waf") },
        // Envoy Gateway writes the proxies' DaemonSet from `proxies`, and
        // its own image is their shutdown manager's.
        "envoyGateway": { "version": system::ENVOY_GATEWAY_VERSION, "image": system::ENVOY_GATEWAY_IMAGE },
        "proxies": proxies,
        "waf": system::waf_daemonset("<waf>", "<geoip>")["spec"]["template"],
        "tunnel": system::tunnel_deployment(system::CLOUDFLARED_IMAGE, "<token hash>")["spec"]["template"],
    }))
}

/// The same value with every object's keys in order, however serde_json
/// was built.
fn sorted(v: &Value) -> Value {
    match v {
        Value::Object(m) => {
            let mut keys: Vec<&String> = m.keys().collect();
            keys.sort();
            let mut out = Map::new();
            for k in keys {
                out.insert(k.clone(), sorted(&m[k]));
            }
            Value::Object(out)
        }
        Value::Array(a) => Value::Array(a.iter().map(sorted).collect()),
        other => other.clone(),
    }
}

/// Where two values differ, as paths: "proxies.shutdown.drainTimeout".
fn differences(path: &str, was: &Value, now: &Value, out: &mut Vec<String>) {
    match (was, now) {
        (Value::Object(a), Value::Object(b)) => {
            let mut keys: Vec<&String> = a.keys().chain(b.keys()).collect();
            keys.sort();
            keys.dedup();
            for k in keys {
                let at = if path.is_empty() {
                    k.clone()
                } else {
                    format!("{path}.{k}")
                };
                differences(
                    &at,
                    a.get(k).unwrap_or(&Value::Null),
                    b.get(k).unwrap_or(&Value::Null),
                    out,
                );
            }
        }
        (Value::Array(a), Value::Array(b)) if a.len() == b.len() => {
            for (i, (x, y)) in a.iter().zip(b).enumerate() {
                differences(&format!("{path}[{i}]"), x, y, out);
            }
        }
        _ if was != now => out.push(format!("  {path}\n    was {was}\n    now {now}")),
        _ => {}
    }
}

#[test]
fn inbound_pods_change_only_on_purpose() {
    let file = snapshot();
    let text = std::fs::read_to_string(&file).unwrap_or_else(|e| panic!("{}: {e}", file.display()));
    let mut recorded: Value = serde_json::from_str(&text).expect("inbound-pods.json is JSON");
    let now = pods();
    let changes = recorded["changes"].as_array().cloned().unwrap_or_default();
    assert!(
        changes
            .iter()
            .all(|c| c["date"].as_str().is_some_and(|d| d.len() == 10)
                && c["note"].as_str().is_some_and(|n| !n.trim().is_empty())),
        "every change in inbound-pods.json has a date and a note"
    );
    if recorded["pods"] == now {
        assert!(!changes.is_empty(), "inbound-pods.json records no change");
        return;
    }

    let mut where_ = vec![];
    differences("", &recorded["pods"], &now, &mut where_);
    let restarts: Vec<String> = RESTARTS
        .iter()
        .filter(|(part, _)| recorded["pods"][part] != now[part])
        .map(|(part, what)| format!("  {part}: {what}"))
        .collect();

    let note = std::env::var(NOTE).unwrap_or_default();
    if !note.trim().is_empty() {
        let today = jiff::Zoned::now().date().to_string();
        let mut changes = changes;
        changes.push(json!({ "date": today, "note": note.trim() }));
        recorded["changes"] = Value::Array(changes);
        recorded["pods"] = now;
        let mut out = serde_json::to_string_pretty(&sorted(&recorded)).expect("it serializes");
        out.push('\n');
        std::fs::write(&file, out).unwrap_or_else(|e| panic!("{}: {e}", file.display()));
        eprintln!(
            "Recorded in {}:\n{}\nSay in the release notes what restarts:\n{}",
            file.display(),
            where_.join("\n"),
            restarts.join("\n")
        );
        return;
    }
    panic!(
        "The pods incoming traffic passes through changed, so they restart on every cluster with the next release:\n\
         {}\n\nWhat changed:\n{}\n\n\
         If that isn't meant, undo it. If it is, record it with the reason and commit the file:\n  \
         {NOTE}='why they restart' cargo test -p pyo-proto --test inbound_pods\n\
         Then say in the release notes what restarts.",
        restarts.join("\n"),
        where_.join("\n")
    );
}
