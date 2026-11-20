//! A registry's door: who may push and pull what, by a team's registry
//! token. Pyo's own door (pyo-api registrydoor.rs) and a cluster's own
//! registry's door (pyo-agent registrydoor.rs, Pyo Cloud's clusters, which
//! keep the images they build) go by the same rules: a team's token pushes
//! and pulls `<team>/<project>/<service>` of its own team only, and a pull
//! token only pulls.
//!
//! A cluster never sees a token: Pyo sends it the tokens' SHA-256 (hex)
//! with what each may do ([`RegistryTokens`], a signed command), and the
//! cluster's door hashes what a client logs in with.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// The cluster's team's registry tokens, as its door checks them. All of
/// them each time: the door keeps exactly these.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RegistryTokens {
    /// The team's slug: its images are under it.
    pub team_slug: String,
    pub tokens: Vec<RegistryToken>,
}

/// One registry token, never the token itself.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RegistryToken {
    /// Lowercase hex SHA-256 of the token.
    pub sha256: String,
    /// It may push too (else only pull).
    pub push: bool,
    /// Unix seconds it stops working; never when None.
    #[serde(default)]
    pub expires_at: Option<i64>,
}

impl RegistryTokens {
    /// The token a client logged in with, while it works (unix seconds
    /// `now`). Compared in constant time.
    pub fn find(&self, password: &str, now: i64) -> Option<&RegistryToken> {
        let got = hex(&Sha256::digest(password.as_bytes()));
        self.tokens.iter().find(|t| {
            t.sha256.len() == got.len()
                && t.sha256
                    .bytes()
                    .zip(got.bytes())
                    .fold(0u8, |a, (x, y)| a | (x ^ y))
                    == 0
                && t.expires_at.is_none_or(|e| e > now)
        })
    }
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// A team's own repository: "<team>/<project>/<service>", its three parts.
pub fn parts(name: &str) -> Option<(&str, &str, &str)> {
    let mut it = name.split('/');
    let (t, p, s) = (it.next()?, it.next()?, it.next()?);
    let label = |x: &str| {
        !x.is_empty()
            && x.len() <= 63
            && x.bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    };
    (it.next().is_none() && label(t) && label(p) && label(s) && t != "pyo").then_some((t, p, s))
}

/// What a request asks for: the repository it names (None for /v2/ itself)
/// and whether it writes.
#[derive(Debug, PartialEq, Eq)]
pub enum Wants {
    Root,
    Repo { name: String, write: bool },
    Other,
}

fn reads(method: &str) -> bool {
    matches!(method, "GET" | "HEAD")
}

pub fn wants(method: &str, path: &str) -> Wants {
    let Some(rest) = path.strip_prefix("/v2/") else {
        return Wants::Other;
    };
    if rest.is_empty() {
        return Wants::Root;
    }
    let name = ["/manifests/", "/blobs/", "/tags/list"]
        .iter()
        .filter_map(|m| rest.find(m))
        .min()
        .map(|i| &rest[..i]);
    match name {
        Some(n) if !n.is_empty() => Wants::Repo {
            name: n.to_owned(),
            write: !reads(method),
        },
        _ => Wants::Other,
    }
}

/// Whether a token for `slug` (with `push` or only pull) may do this, by
/// the path alone.
pub fn allowed(slug: &str, push: bool, method: &str, name: &str) -> bool {
    let ours = parts(name).is_some_and(|(t, _, _)| t == slug);
    match method {
        "GET" | "HEAD" => ours,
        // Pushing: uploads, then the manifest. Deleting isn't offered.
        "POST" | "PUT" | "PATCH" => ours && push,
        _ => false,
    }
}

/// A query without a cross-repository mount from outside the team (the
/// registry then takes a plain upload instead).
pub fn scoped_query(query: &str, slug: &str) -> String {
    let prefix = format!("{slug}/");
    // (Another repository of its own team is fine to mount from.)
    let pairs: Vec<(&str, &str)> = query
        .split('&')
        .filter(|p| !p.is_empty())
        .map(|p| p.split_once('=').unwrap_or((p, "")))
        .collect();
    let foreign = pairs
        .iter()
        .any(|(k, v)| *k == "from" && !decode(v).starts_with(&prefix));
    pairs
        .into_iter()
        .filter(|(k, _)| !(foreign && (*k == "from" || *k == "mount")))
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("&")
}

/// Percent-decoding of a query value ("+" is a space).
fn decode(v: &str) -> String {
    let b = v.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'%' if i + 2 < b.len() => match std::str::from_utf8(&b[i + 1..i + 3])
                .ok()
                .and_then(|h| u8::from_str_radix(h, 16).ok())
            {
                Some(x) => {
                    out.push(x);
                    i += 3;
                    continue;
                }
                None => out.push(b'%'),
            },
            b'+' => out.push(b' '),
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_token_by_its_hash_while_it_works() {
        let t = RegistryTokens {
            team_slug: "acme".into(),
            tokens: vec![
                RegistryToken {
                    sha256: hex(&Sha256::digest(b"pyo_reg_a")),
                    push: true,
                    expires_at: None,
                },
                RegistryToken {
                    sha256: hex(&Sha256::digest(b"pyo_reg_b")),
                    push: false,
                    expires_at: Some(100),
                },
            ],
        };
        assert!(t.find("pyo_reg_a", 1_000).unwrap().push);
        assert!(!t.find("pyo_reg_b", 99).unwrap().push);
        assert!(t.find("pyo_reg_b", 100).is_none(), "expired");
        assert!(t.find("pyo_reg_c", 0).is_none());
        assert!(t.find("", 0).is_none());
    }

    #[test]
    fn a_teams_own_repositories_only() {
        assert_eq!(wants("GET", "/v2/"), Wants::Root);
        assert_eq!(
            wants("PUT", "/v2/acme/shop/api/manifests/1"),
            Wants::Repo {
                name: "acme/shop/api".into(),
                write: true
            }
        );
        assert_eq!(
            wants("HEAD", "/v2/acme/shop/api/blobs/sha256:x"),
            Wants::Repo {
                name: "acme/shop/api".into(),
                write: false
            }
        );
        assert_eq!(wants("GET", "/v2/_catalog"), Wants::Other);
        assert!(allowed("acme", true, "PUT", "acme/shop/api"));
        assert!(allowed("acme", false, "GET", "acme/shop/api"));
        assert!(!allowed("acme", false, "PUT", "acme/shop/api"), "pull only");
        assert!(!allowed("acme", true, "GET", "other/shop/api"));
        assert!(!allowed("acme", true, "DELETE", "acme/shop/api"));
        assert!(!allowed("pyo", true, "GET", "pyo/shop/api"));
        assert!(parts("acme/shop").is_none() && parts("acme/shop/api/x").is_none());
        assert_eq!(
            scoped_query("mount=sha256:1&from=other%2Fx%2Fy", "acme"),
            ""
        );
        assert_eq!(
            scoped_query("mount=sha256:1&from=acme%2Fx%2Fy", "acme"),
            "mount=sha256:1&from=acme%2Fx%2Fy"
        );
    }
}
