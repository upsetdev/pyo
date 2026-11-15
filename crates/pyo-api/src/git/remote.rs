//! A public repository's default branch, read from Git's smart HTTP
//! advertisement ("info/refs"), which names it in the `symref=HEAD:…`
//! capability. Works the same on GitHub, GitLab, Gitea and others, with no
//! account. Goes through the guarded client ([`crate::outbound`]): the
//! address is the team's.

use std::time::Duration;

use crate::outbound::{self, Reach};

/// The branch HEAD points at, or None when the repository is private,
/// can't be reached in a few seconds, or doesn't say.
pub async fn default_branch(repo_url: &str) -> Option<String> {
    let reach = Reach::team();
    let base = repo_url.trim().trim_end_matches('/');
    let url = outbound::parse(&format!("{base}/info/refs?service=git-upload-pack")).ok()?;
    outbound::check(&url, reach).await.ok()?;
    let http = outbound::client(reach, Duration::from_secs(8)).ok()?;
    let res = http
        .get(url)
        .header("git-protocol", "version=1")
        .send()
        .await
        .ok()?;
    if !res.status().is_success() {
        return None;
    }
    let body = res.bytes().await.ok()?;
    head_branch(&body[..body.len().min(1 << 20)])
}

/// The `symref=HEAD:refs/heads/<branch>` capability of an advertisement.
fn head_branch(body: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(body);
    let caps = text.split('\0').nth(1)?;
    caps.split([' ', '\n'])
        .find_map(|c| c.strip_prefix("symref=HEAD:refs/heads/"))
        .map(str::to_owned)
        .filter(|b| !b.is_empty() && !b.contains(['\0', ' ', ':', '#']) && !b.contains(".."))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_head_symref() {
        let body = b"001e# service=git-upload-pack\n0000\
            00f9a1b2c3d4e5f6a7b8c9d0e1f2a3b4c5d6e7f8a9b0 HEAD\0multi_ack thin-pack side-band symref=HEAD:refs/heads/master agent=git/github\n\
            003fa1b2c3d4e5f6a7b8c9d0e1f2a3b4c5d6e7f8a9b0 refs/heads/master\n0000";
        assert_eq!(head_branch(body).as_deref(), Some("master"));
        let trunk = b"0010HEAD\0symref=HEAD:refs/heads/trunk\n";
        assert_eq!(head_branch(trunk).as_deref(), Some("trunk"));
        assert_eq!(head_branch(b"0010HEAD\0agent=x\n"), None);
        assert_eq!(head_branch(b"no refs"), None);
    }
}
