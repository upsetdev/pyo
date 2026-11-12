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

#[cfg(test)]
mod tests {
    use super::*;

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
