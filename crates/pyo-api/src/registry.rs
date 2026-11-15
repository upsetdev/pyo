//! Logins for private container registries: which one an image needs, and
//! checking a login against the registry before it's saved.

use std::time::Duration;

use crate::outbound::{self, Reach};

/// The registry host of an image reference ("docker.io" when it names none).
pub fn image_host(image: &str) -> String {
    let first = image.split('/').next().unwrap_or("");
    let host = if image.contains('/')
        && (first.contains('.') || first.contains(':') || first == "localhost")
    {
        first
    } else {
        "docker.io"
    };
    match host {
        "index.docker.io" | "registry-1.docker.io" => "docker.io".into(),
        h => h.to_lowercase(),
    }
}

/// Where the registry's API answers (Docker Hub's isn't at docker.io).
fn api_base(host: &str) -> String {
    match host {
        "docker.io" => "https://registry-1.docker.io".into(),
        h => format!("https://{h}"),
    }
}

/// Whether the registry accepts the login: Ok(true) yes, Ok(false) it
/// couldn't be reached or checked, Err when it refused.
///
/// The host comes from a team's admin, so the check only goes where
/// [`crate::outbound`] lets a team's addresses go: a registry elsewhere
/// (this machine, the cluster's network, a private one unless the
/// installation allows those) isn't checked, and its login is saved
/// unchecked. The token service a registry names (`realm`) is held to the
/// same, and must be https.
pub async fn verify(host: &str, username: &str, password: &str) -> Result<bool, String> {
    verify_with(host, username, password, Reach::team()).await
}

async fn verify_with(
    host: &str,
    username: &str,
    password: &str,
    reach: Reach,
) -> Result<bool, String> {
    let Ok(http) = outbound::client(reach, Duration::from_secs(10)) else {
        return Ok(false);
    };
    let Ok(base) = outbound::parse(&format!("{}/v2/", api_base(host))) else {
        return Ok(false);
    };
    if outbound::check(&base, reach).await.is_err() {
        return Ok(false);
    }
    let Ok(res) = http.get(base.clone()).send().await else {
        return Ok(false);
    };
    if res.status().is_success() {
        // Open registry: nothing to check against.
        return Ok(false);
    }
    let challenge = res
        .headers()
        .get(reqwest::header::WWW_AUTHENTICATE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_owned();
    let refused = |s: reqwest::StatusCode| {
        Err(format!(
            "{host} didn't accept that login ({s}). Check the username and token."
        ))
    };
    if let Some(params) = challenge.strip_prefix("Bearer ") {
        // Token auth (ghcr.io, Docker Hub, GitLab, Quay): ask for a token with the login.
        let field = |k: &str| {
            params
                .split(',')
                .find_map(|p| p.trim().strip_prefix(&format!("{k}=\"")))
                .map(|v| v.trim_end_matches('"').to_owned())
        };
        let Some(realm) = field("realm") else {
            return Ok(false);
        };
        let Some(realm) = realm_url(&realm, reach).await else {
            return Ok(false);
        };
        let mut req = http
            .get(realm)
            .basic_auth(username, Some(password))
            .query(&[("account", username)]);
        if let Some(service) = field("service") {
            req = req.query(&[("service", service)]);
        }
        return match req.send().await {
            Ok(r) if r.status().is_success() => Ok(true),
            Ok(r)
                if r.status() == reqwest::StatusCode::UNAUTHORIZED
                    || r.status() == reqwest::StatusCode::FORBIDDEN =>
            {
                refused(r.status())
            }
            _ => Ok(false),
        };
    }
    // Basic auth registries.
    match http
        .get(base)
        .basic_auth(username, Some(password))
        .send()
        .await
    {
        Ok(r) if r.status().is_success() => Ok(true),
        Ok(r) if r.status() == reqwest::StatusCode::UNAUTHORIZED => refused(r.status()),
        _ => Ok(false),
    }
}

/// The token service a registry names, if Pyo may send the login there:
/// https, and an address a team's may be.
async fn realm_url(realm: &str, reach: Reach) -> Option<reqwest::Url> {
    let url = outbound::parse(realm).ok()?;
    (url.scheme() == "https" && outbound::check(&url, reach).await.is_ok()).then_some(url)
}

/// An image reference's repository on its registry and its tag or digest:
/// "nginx" → ("library/nginx", "latest"), "ghcr.io/acme/api:1.2" →
/// ("acme/api", "1.2"), "acme/api@sha256:…" → ("acme/api", "sha256:…").
fn repo_and_reference(image: &str, host: &str) -> Option<(String, String)> {
    let image = image.trim();
    let rest = image
        .strip_prefix(&format!("{host}/"))
        .or_else(|| {
            ["index.docker.io/", "registry-1.docker.io/", "docker.io/"]
                .iter()
                .find_map(|p| image.strip_prefix(p))
        })
        .unwrap_or(image);
    let (name, reference) = match rest.split_once('@') {
        Some((n, d)) => (n, d.to_owned()),
        None => match rest.rsplit_once(':') {
            Some((n, t)) if !t.contains('/') => (n, t.to_owned()),
            _ => (rest, "latest".to_owned()),
        },
    };
    let name = if host == "docker.io" && !name.contains('/') {
        format!("library/{name}")
    } else {
        name.to_owned()
    };
    let ok = |c: char| c.is_ascii_alphanumeric() || "._-/".contains(c);
    let ok_ref = |c: char| c.is_ascii_alphanumeric() || "._-:".contains(c);
    (!name.is_empty()
        && name.chars().all(ok)
        && !reference.is_empty()
        && reference.chars().all(ok_ref))
    .then_some((name.to_lowercase(), reference))
}

/// The TCP ports an image config EXPOSEs, lowest first.
fn config_ports(cfg: &serde_json::Value) -> Vec<u16> {
    let mut ports: Vec<u16> = cfg["config"]["ExposedPorts"]
        .as_object()
        .map(|o| {
            o.keys()
                .filter(|k| k.ends_with("/tcp") || !k.contains('/'))
                .filter_map(|k| k.split('/').next()?.parse().ok())
                .collect()
        })
        .unwrap_or_default();
    ports.sort_unstable();
    ports.dedup();
    ports
}

/// From an image index: the linux/amd64 image's manifest digest, else the
/// first Linux one's.
fn platform_digest(index: &serde_json::Value) -> Option<String> {
    let all = index["manifests"].as_array()?;
    let linux = |m: &&serde_json::Value| m["platform"]["os"] == "linux";
    all.iter()
        .filter(linux)
        .find(|m| m["platform"]["architecture"] == "amd64")
        .or_else(|| all.iter().find(linux))
        .and_then(|m| m["digest"].as_str())
        .map(str::to_owned)
}

const MANIFEST_TYPES: &str = "application/vnd.oci.image.index.v1+json, application/vnd.docker.distribution.manifest.list.v2+json, application/vnd.oci.image.manifest.v1+json, application/vnd.docker.distribution.manifest.v2+json";

/// The ports a public image EXPOSEs, read from its config in the registry
/// without a login. Empty when it exposes none, is private, or the
/// registry can't be reached in a few seconds: the caller keeps its guess.
/// Held to [`crate::outbound`] like the login check.
pub async fn exposed_ports(image: &str) -> Vec<u16> {
    let reach = Reach::team();
    let host = image_host(image);
    let Some((repo, reference)) = repo_and_reference(image, &host) else {
        return vec![];
    };
    let Ok(http) = outbound::client(reach, Duration::from_secs(5)) else {
        return vec![];
    };
    let base = api_base(&host);
    let mut token: Option<String> = None;
    let manifest = |r: &str| format!("{base}/v2/{repo}/manifests/{r}");
    let Some(mut m) = registry_json(&http, reach, &manifest(&reference), &repo, &mut token).await
    else {
        return vec![];
    };
    if m["manifests"].is_array() {
        let Some(d) = platform_digest(&m) else {
            return vec![];
        };
        let Some(next) = registry_json(&http, reach, &manifest(&d), &repo, &mut token).await else {
            return vec![];
        };
        m = next;
    }
    let Some(cfg) = m["config"]["digest"].as_str() else {
        return vec![];
    };
    let url = format!("{base}/v2/{repo}/blobs/{cfg}");
    match registry_json(&http, reach, &url, &repo, &mut token).await {
        Some(c) => config_ports(&c),
        None => vec![],
    }
}

/// A registry API answer as JSON: anonymous, with a pull token from the
/// registry's token service when it asks for one, following a blob's
/// redirect to its storage (without the token).
async fn registry_json(
    http: &reqwest::Client,
    reach: Reach,
    url: &str,
    repo: &str,
    token: &mut Option<String>,
) -> Option<serde_json::Value> {
    let mut url = outbound::parse(url).ok()?;
    let origin = url.host_str()?.to_owned();
    for _ in 0..4 {
        outbound::check(&url, reach).await.ok()?;
        let mut req = http
            .get(url.clone())
            .header(reqwest::header::ACCEPT, MANIFEST_TYPES);
        if let Some(t) = token.as_deref()
            && url.host_str() == Some(origin.as_str())
        {
            req = req.bearer_auth(t);
        }
        let res = req.send().await.ok()?;
        let status = res.status();
        if status.is_redirection() {
            let to = res
                .headers()
                .get(reqwest::header::LOCATION)?
                .to_str()
                .ok()?;
            url = url.join(to).ok()?;
            if url.scheme() != "https" {
                return None;
            }
            continue;
        }
        if status == reqwest::StatusCode::UNAUTHORIZED && token.is_none() {
            let challenge = res
                .headers()
                .get(reqwest::header::WWW_AUTHENTICATE)?
                .to_str()
                .ok()?
                .to_owned();
            *token = Some(anonymous_token(http, reach, &challenge, repo).await?);
            continue;
        }
        if !status.is_success() {
            return None;
        }
        let body = res.bytes().await.ok()?;
        if body.len() > 4 << 20 {
            return None;
        }
        return serde_json::from_slice(&body).ok();
    }
    None
}

/// A pull token for `repo` without a login, from a Bearer challenge.
async fn anonymous_token(
    http: &reqwest::Client,
    reach: Reach,
    challenge: &str,
    repo: &str,
) -> Option<String> {
    let params = challenge.strip_prefix("Bearer ")?;
    let field = |k: &str| {
        params
            .split(',')
            .find_map(|p| p.trim().strip_prefix(&format!("{k}=\"")))
            .map(|v| v.trim_end_matches('"').to_owned())
    };
    let realm = realm_url(&field("realm")?, reach).await?;
    let mut req = http
        .get(realm)
        .query(&[("scope", format!("repository:{repo}:pull"))]);
    if let Some(service) = field("service") {
        req = req.query(&[("service", service)]);
    }
    let v: serde_json::Value = req.send().await.ok()?.json().await.ok()?;
    v["token"]
        .as_str()
        .or_else(|| v["access_token"].as_str())
        .map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn references() {
        let r = |i: &str| repo_and_reference(i, &image_host(i));
        assert_eq!(r("nginx"), Some(("library/nginx".into(), "latest".into())));
        assert_eq!(
            r("nginx:1.27"),
            Some(("library/nginx".into(), "1.27".into()))
        );
        assert_eq!(
            r("docker.io/acme/api:2"),
            Some(("acme/api".into(), "2".into()))
        );
        assert_eq!(
            r("ghcr.io/me/api"),
            Some(("me/api".into(), "latest".into()))
        );
        assert_eq!(
            r("registry.example.com:5000/team/web:v1"),
            Some(("team/web".into(), "v1".into()))
        );
        assert_eq!(
            r("ghcr.io/me/api@sha256:abc"),
            Some(("me/api".into(), "sha256:abc".into()))
        );
        assert_eq!(r("ghcr.io/me/api?x=1"), None);
    }

    #[test]
    fn exposed_ports_from_config() {
        let cfg =
            json!({ "config": { "ExposedPorts": { "3000/tcp": {}, "53/udp": {}, "80": {} } } });
        assert_eq!(config_ports(&cfg), vec![80, 3000]);
        assert!(config_ports(&json!({ "config": {} })).is_empty());
    }

    #[test]
    fn index_picks_linux_amd64() {
        let index = json!({ "manifests": [
            { "digest": "sha256:arm", "platform": { "os": "linux", "architecture": "arm64" } },
            { "digest": "sha256:win", "platform": { "os": "windows", "architecture": "amd64" } },
            { "digest": "sha256:amd", "platform": { "os": "linux", "architecture": "amd64" } },
        ] });
        assert_eq!(platform_digest(&index).as_deref(), Some("sha256:amd"));
    }

    #[test]
    fn hosts() {
        assert_eq!(image_host("nginx"), "docker.io");
        assert_eq!(image_host("docker.io/library/nginx"), "docker.io");
        assert_eq!(image_host("valkey/valkey"), "docker.io");
        assert_eq!(image_host("ghcr.io/acme/web"), "ghcr.io");
        assert_eq!(image_host("127.0.0.1:5005/pyo/app"), "127.0.0.1:5005");
        assert_eq!(image_host("localhost/app"), "localhost");
    }

    /// A one-request HTTP server on this machine: answers `reply`, and
    /// counts what it was asked.
    async fn serve(
        reply: &'static str,
    ) -> (String, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let hits = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let h = hits.clone();
        tokio::spawn(async move {
            while let Ok((mut c, _)) = listener.accept().await {
                h.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let mut buf = [0u8; 4096];
                let _ = c.read(&mut buf).await;
                let _ = c.write_all(reply.as_bytes()).await;
            }
        });
        (addr, hits)
    }

    /// Never this machine (nor metadata, nor the cluster's network), even
    /// where private networks are allowed: the login is saved unchecked.
    #[tokio::test]
    async fn reserved_registries_arent_called() {
        let (addr, hits) = serve("HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: Basic realm=\"x\"\r\nContent-Length: 0\r\n\r\n").await;
        for host in [
            addr.as_str(),
            "169.254.169.254",
            "10.43.0.1",
            "[fd00:ec2::254]",
        ] {
            assert_eq!(
                verify_with(host, "u", "p", Reach { private: true }).await,
                Ok(false),
                "{host}"
            );
        }
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn realms_are_held_to_the_same() {
        let lan = Reach { private: true };
        for bad in [
            "http://1.1.1.1/token",
            "https://169.254.169.254/latest/meta-data/",
            "https://127.0.0.1:8080/token",
            "https://10.42.0.7/token",
            "file:///etc/passwd",
            "https://u:p@1.1.1.1/token",
        ] {
            assert!(realm_url(bad, lan).await.is_none(), "{bad}");
        }
        assert!(
            realm_url("https://192.168.1.10/token", Reach { private: false })
                .await
                .is_none()
        );
        assert!(realm_url("https://192.168.1.10/token", lan).await.is_some());
        assert!(
            realm_url("https://1.1.1.1/token", Reach { private: false })
                .await
                .is_some()
        );
    }

    /// A private registry is left unchecked unless the installation allows
    /// private networks.
    #[tokio::test]
    async fn private_registries_only_when_allowed() {
        assert_eq!(
            verify_with("192.168.255.254:1", "u", "p", Reach { private: false }).await,
            Ok(false)
        );
    }
}
