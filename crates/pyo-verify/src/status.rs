//! What the verifier last did, for the server's owner and for Pyo's agent:
//! `/var/lib/pyo-verify/status.json`, and the same as the ConfigMap
//! `pyo-system/pyo-verifier`. The ConfigMap is the only cluster object the
//! verifier writes in this release.

use serde_json::json;

use crate::sys::{Env, read_limited, write_atomic};

pub const PATH: &str = "/var/lib/pyo-verify/status.json";
pub const NAMESPACE: &str = "pyo-system";
pub const CONFIGMAP: &str = "pyo-verifier";
/// Every kubectl call gives up after this.
pub const KUBECTL_TIMEOUT: &str = "--request-timeout=15s";

#[derive(Debug, Clone, PartialEq)]
pub struct Status {
    pub verifier_version: String,
    /// The version Pyo's control plane named. Empty when it was not asked
    /// or its answer was not usable.
    pub live_version: String,
    /// RFC 3339, UTC.
    pub last_check: String,
    /// `current`, `hold`, `waiting`, `updating` or `error`.
    pub state: String,
    /// One plain line about this run.
    pub message: String,
    /// The same line when the run failed, else empty.
    pub last_error: String,
    pub hold: bool,
    pub host: String,
}

impl Status {
    fn fields(&self) -> [(&'static str, String); 8] {
        [
            ("verifierVersion", self.verifier_version.clone()),
            ("liveVersion", self.live_version.clone()),
            ("lastCheck", self.last_check.clone()),
            ("state", self.state.clone()),
            ("message", self.message.clone()),
            ("lastError", self.last_error.clone()),
            ("hold", self.hold.to_string()),
            ("host", self.host.clone()),
        ]
    }

    fn file(&self) -> String {
        let mut v = serde_json::Map::new();
        for (k, s) in self.fields() {
            v.insert(k.into(), json!(s));
        }
        v.insert("hold".into(), json!(self.hold));
        format!("{:#}\n", serde_json::Value::Object(v))
    }

    /// The ConfigMap, as JSON (which kubectl takes like YAML). Its values
    /// are all strings, under the keys Pyo's agent reads ([`configmap_key`]).
    fn configmap(&self) -> String {
        let data: serde_json::Map<String, serde_json::Value> = self
            .fields()
            .into_iter()
            .map(|(k, s)| (configmap_key(k).to_owned(), json!(s)))
            .collect();
        json!({
            "apiVersion": "v1",
            "kind": "ConfigMap",
            "metadata": {
                "name": CONFIGMAP,
                "namespace": NAMESPACE,
                "labels": { "app.kubernetes.io/managed-by": "pyo-verify" },
            },
            "data": data,
        })
        .to_string()
    }
}

/// A status field's key in the ConfigMap: what Pyo's agent reads there
/// (pyo-proto's `VerifierStatus::from_data`: version, live, checked,
/// error, hold, host). The file keeps its own names.
fn configmap_key(field: &str) -> &str {
    match field {
        "verifierVersion" => "version",
        "liveVersion" => "live",
        "lastCheck" => "checked",
        "lastError" => "error",
        other => other,
    }
}

/// This server's name.
pub fn host(env: &Env) -> String {
    read_limited(&env.path("/proc/sys/kernel/hostname"), 256)
        .ok()
        .and_then(|b| String::from_utf8(b).ok())
        .map(|s| s.trim().to_owned())
        .unwrap_or_default()
}

/// Seconds since 1970 as `2026-11-23T13:42:00Z`.
pub fn rfc3339(secs: i64) -> String {
    let (days, rem) = (secs.div_euclid(86400), secs.rem_euclid(86400));
    // Days to a date (Howard Hinnant's civil_from_days).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

/// Writes the file, then the ConfigMap. A cluster that does not answer is
/// fine: the file is there. Returns whether each was written.
pub fn write(env: &Env, status: &Status) -> (bool, bool) {
    let file = write_atomic(&env.path(PATH), status.file().as_bytes(), 0o644).is_ok();
    let cm = env
        .runner
        .run(
            &["k3s", "kubectl", KUBECTL_TIMEOUT, "apply", "-f", "-"],
            Some(status.configmap().as_bytes()),
        )
        .is_ok_and(|o| o.ok);
    (file, cm)
}

/// The status file as a few plain lines, for `pyo-verify status`.
pub fn show(env: &Env) -> String {
    let v: Option<serde_json::Value> = read_limited(&env.path(PATH), 64 * 1024)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok());
    let Some(v) = v else {
        return "The verifier has not run on this server yet.".into();
    };
    let s = |k: &str| v[k].as_str().unwrap_or("").to_owned();
    let mut lines = vec![format!(
        "Verifier {} on {}",
        s("verifierVersion"),
        s("host")
    )];
    if !s("liveVersion").is_empty() {
        lines.push(format!("Pyo names version {}", s("liveVersion")));
    }
    lines.push(format!("Last check {}", s("lastCheck")));
    lines.push(format!("State {}", s("state")));
    lines.push(s("message"));
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn times_are_written_as_utc_dates() {
        assert_eq!(rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339(951_782_400), "2000-02-29T00:00:00Z");
        assert_eq!(rfc3339(1_795_294_220), "2026-11-21T20:50:20Z");
        assert_eq!(rfc3339(4_102_444_799), "2099-12-31T23:59:59Z");
    }

    #[test]
    fn the_configmap_is_only_the_status() {
        let s = Status {
            verifier_version: "0.51.0".into(),
            live_version: "0.52.0".into(),
            last_check: rfc3339(0),
            state: "error".into(),
            message: "A \"quoted\"\nline".into(),
            last_error: "A \"quoted\"\nline".into(),
            hold: false,
            host: "web-1".into(),
        };
        let cm: serde_json::Value = serde_json::from_str(&s.configmap()).unwrap();
        assert_eq!(cm["kind"], "ConfigMap");
        assert_eq!(cm["metadata"]["namespace"], "pyo-system");
        assert_eq!(cm["metadata"]["name"], "pyo-verifier");
        assert_eq!(cm["data"]["hold"], "false");
        // The keys Pyo's agent reads (pyo-proto's VerifierStatus::from_data).
        assert_eq!(cm["data"]["version"], "0.51.0");
        assert_eq!(cm["data"]["live"], "0.52.0");
        assert_eq!(cm["data"]["checked"], "1970-01-01T00:00:00Z");
        assert_eq!(cm["data"]["error"], "A \"quoted\"\nline");
        assert_eq!(cm["data"]["host"], "web-1");
        let mut keys: Vec<&str> = cm["data"]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            [
                "checked", "error", "hold", "host", "live", "message", "state", "version"
            ]
        );
        assert!(
            cm["data"]
                .as_object()
                .unwrap()
                .values()
                .all(|v| v.is_string())
        );
        let file: serde_json::Value = serde_json::from_str(&s.file()).unwrap();
        assert_eq!(file["hold"], false);
        assert_eq!(file["liveVersion"], "0.52.0");
    }
}
