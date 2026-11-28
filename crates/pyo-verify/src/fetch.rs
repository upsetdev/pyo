//! Downloads. Every answer is untrusted: a size limit on each, and a
//! redirect is only followed to a host on a short list.

use std::path::PathBuf;

use crate::{
    sys::{Env, HttpError, Problem, read_limited},
    version::Version,
};

/// Where GitHub serves a release's files from, after its own redirect.
pub const GITHUB_HOSTS: &[&str] = &[
    "github.com",
    "objects.githubusercontent.com",
    "release-assets.githubusercontent.com",
];
const GITHUB_RELEASES: &str = "https://github.com/upsetdev/pyo/releases/download";
/// api.pyo.dev's copies of each release's release.json and its signature,
/// laid out like GitHub's, for when GitHub's download fails. As untrusted
/// as GitHub: the signature is checked the same way.
const RELEASE_COPIES: &str = "https://api.pyo.dev/v1/releases/download";
const COPY_HOSTS: &[&str] = &["api.pyo.dev"];
/// The files api.pyo.dev keeps a copy of.
const COPIED_FILES: &[&str] = &["release.json", "release.json.sigstore.json"];
const MAX_REDIRECTS: usize = 5;

pub const MAX_ANSWER: usize = 4 * 1024;
pub const MAX_MANIFEST: usize = 4 * 1024 * 1024;
pub const MAX_BUNDLE: usize = 256 * 1024;
pub const MAX_BINARY: usize = 64 * 1024 * 1024;

/// The scheme and host (with its port, when it has one) of a URL the
/// verifier is willing to ask. None for anything else.
pub fn origin(url: &str) -> Option<(bool, String)> {
    if !url.is_ascii() || url.bytes().any(|b| b <= b' ' || b == 0x7f) {
        return None;
    }
    let (https, rest) = match (url.strip_prefix("https://"), url.strip_prefix("http://")) {
        (Some(r), _) => (true, r),
        (_, Some(r)) => (false, r),
        _ => return None,
    };
    let host = rest.split(['/', '?', '#']).next().unwrap_or("");
    let ok = !host.is_empty()
        && host
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b':' | b'[' | b']'));
    ok.then(|| (https, host.to_ascii_lowercase()))
}

/// A GET that follows redirects only to `hosts`, and only over HTTPS when
/// it started there.
pub fn get(env: &Env, url: &str, max: usize, hosts: &[&str]) -> Result<Vec<u8>, Problem> {
    let (https, first) = origin(url).ok_or_else(|| Problem::new("The address is not a URL."))?;
    let mut at = url.to_owned();
    for _ in 0..=MAX_REDIRECTS {
        let r = env.http.get(&at, max).map_err(|e| match e {
            HttpError::TooLarge => Problem::new(format!(
                "The answer from {first} is larger than expected. Nothing was changed."
            )),
            HttpError::Failed(why) => {
                Problem::with(format!("{first} did not answer. Nothing was changed."), why)
            }
        })?;
        if r.body.len() > max {
            return Err(Problem::new(format!(
                "The answer from {first} is larger than expected. Nothing was changed."
            )));
        }
        match (r.status, r.redirect) {
            (200, _) => return Ok(r.body),
            (300..=399, Some(to)) => {
                let allowed = origin(&to)
                    .is_some_and(|(s, host)| (s || !https) && hosts.contains(&host.as_str()));
                if !allowed {
                    return Err(Problem::with(
                        format!(
                            "{first} sent the download to an address the verifier does not use. Nothing was changed."
                        ),
                        to,
                    ));
                }
                at = to;
            }
            (status, _) => {
                return Err(Problem::with(
                    format!(
                        "{first} did not have what the verifier asked for. Nothing was changed."
                    ),
                    format!("HTTP {status} for {at}"),
                ));
            }
        }
    }
    Err(Problem::new(format!(
        "{first} redirected too many times. Nothing was changed."
    )))
}

/// Where a release's files come from. All of them are untrusted: what
/// counts is the signature on release.json and the checksums in it.
#[derive(Debug, Clone, PartialEq)]
pub enum Source {
    GitHub,
    /// A base URL laid out like GitHub's: `<base>/v<version>/<file>`.
    Mirror(String),
    /// A directory laid out the same way.
    Dir(PathBuf),
}

impl Source {
    pub fn fetch(
        &self,
        env: &Env,
        version: &Version,
        file: &str,
        max: usize,
    ) -> Result<Vec<u8>, Problem> {
        match self {
            Source::GitHub => {
                let github = get(
                    env,
                    &format!("{GITHUB_RELEASES}/v{version}/{file}"),
                    max,
                    GITHUB_HOSTS,
                );
                match github {
                    Err(e) if COPIED_FILES.contains(&file) => {
                        get(
                            env,
                            &format!("{RELEASE_COPIES}/v{version}/{file}"),
                            max,
                            COPY_HOSTS,
                        )
                        // Both failed: GitHub's reason is the one to show.
                        .map_err(|_| e)
                    }
                    other => other,
                }
            }
            Source::Mirror(base) => {
                let host = origin(base).map(|(_, h)| h).unwrap_or_default();
                get(env, &format!("{base}/v{version}/{file}"), max, &[&host])
            }
            Source::Dir(dir) => {
                let path = dir.join(format!("v{version}")).join(file);
                read_limited(&path, max).map_err(|e| {
                    Problem::with(
                        format!("{file} of release {version} could not be read from the release folder. Nothing was changed."),
                        format!("{}: {e}", path.display()),
                    )
                })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_plain_urls_have_an_origin() {
        assert_eq!(
            origin("https://GitHub.com/a/b?c#d"),
            Some((true, "github.com".into()))
        );
        assert_eq!(
            origin("http://10.0.0.5:8080"),
            Some((false, "10.0.0.5:8080".into()))
        );
        for bad in [
            "",
            "github.com/x",
            "ftp://github.com/x",
            "file:///etc/passwd",
            "https://",
            "https:///x",
            "https://user@github.com/x",
            "https://github.com@evil.example/x",
            "https://github.com /x",
            "https://github.com/x\n--output=/etc/passwd",
            "https://gíthub.com/x",
            "-o/etc/passwd",
        ] {
            assert_eq!(origin(bad), None, "{bad:?}");
        }
    }
}
