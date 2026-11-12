//! Resolve a branch or tag to a commit without a git binary: the "info/refs"
//! advertisement of Git's smart HTTP protocol, or a plain file listing when
//! the server only speaks the dumb protocol.

use std::time::Duration;

/// The commit `name` (a branch or tag) points at in a public repository.
pub async fn resolve(repo_url: &str, name: &str) -> anyhow::Result<String> {
    let base = repo_url.trim_end_matches('/');
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .build()?;
    let res = http
        .get(format!("{base}/info/refs?service=git-upload-pack"))
        .header("git-protocol", "version=1")
        .send()
        .await?;
    let status = res.status();
    if status == reqwest::StatusCode::UNAUTHORIZED
        || status == reqwest::StatusCode::FORBIDDEN
        || status == reqwest::StatusCode::NOT_FOUND
    {
        anyhow::bail!(
            "Can't read {repo_url}: it doesn't exist or isn't public. Private repositories aren't supported yet."
        );
    }
    if !status.is_success() {
        anyhow::bail!("{repo_url} answered {status}");
    }
    let smart = res
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|c| c.contains("git-upload-pack-advertisement"));
    let body = res.bytes().await?;
    let refs = if smart {
        parse_smart(&body)
    } else {
        parse_dumb(&String::from_utf8_lossy(&body))
    };
    pick(&refs, name)
        .ok_or_else(|| anyhow::anyhow!("{repo_url} has no branch or tag named {name}."))
}

/// (commit, ref name) pairs.
pub fn parse_smart(body: &[u8]) -> Vec<(String, String)> {
    let mut out = vec![];
    let mut i = 0;
    while i + 4 <= body.len() {
        let Ok(len) =
            usize::from_str_radix(std::str::from_utf8(&body[i..i + 4]).unwrap_or("0000"), 16)
        else {
            break;
        };
        if len == 0 {
            i += 4;
            continue;
        }
        if len < 4 || i + len > body.len() {
            break;
        }
        let pkt = String::from_utf8_lossy(&body[i + 4..i + len]);
        let pkt = pkt.trim_end_matches('\n');
        // "<sha> <ref>\0<capabilities>" on the first ref, "<sha> <ref>" after.
        let first = pkt.split('\0').next().unwrap_or("");
        if let Some((sha, name)) = first.split_once(' ')
            && sha.len() == 40
            && sha.bytes().all(|b| b.is_ascii_hexdigit())
        {
            out.push((sha.to_owned(), name.to_owned()));
        }
        i += len;
    }
    out
}

pub fn parse_dumb(body: &str) -> Vec<(String, String)> {
    body.lines()
        .filter_map(|l| l.split_once('\t'))
        .map(|(sha, name)| (sha.trim().to_owned(), name.trim().to_owned()))
        .collect()
}

fn pick(refs: &[(String, String)], name: &str) -> Option<String> {
    let find = |r: &str| refs.iter().find(|(_, n)| n == r).map(|(s, _)| s.clone());
    if name.is_empty() || name == "HEAD" {
        return find("HEAD");
    }
    // An annotated tag's commit is the peeled "^{}" entry.
    find(&format!("refs/heads/{name}"))
        .or_else(|| find(&format!("refs/tags/{name}^{{}}")))
        .or_else(|| find(&format!("refs/tags/{name}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn smart_advertisement() {
        let a = "a".repeat(40);
        let b = "b".repeat(40);
        let first = format!("{a} HEAD\0multi_ack symref=HEAD:refs/heads/main\n");
        let second = format!("{b} refs/heads/main\n");
        let pkt = |s: &str| format!("{:04x}{s}", s.len() + 4);
        let body = format!(
            "{}0000{}{}0000",
            pkt("# service=git-upload-pack\n"),
            pkt(&first),
            pkt(&second)
        );
        let refs = parse_smart(body.as_bytes());
        assert_eq!(pick(&refs, "main"), Some(b.clone()));
        assert_eq!(pick(&refs, "HEAD"), Some(a));
        assert_eq!(pick(&refs, "nope"), None);
    }

    #[test]
    fn dumb_listing_and_peeled_tags() {
        let refs = parse_dumb("111\trefs/heads/dev\n222\trefs/tags/v1\n333\trefs/tags/v1^{}\n");
        assert_eq!(pick(&refs, "dev"), Some("111".into()));
        assert_eq!(pick(&refs, "v1"), Some("333".into()));
    }
}
