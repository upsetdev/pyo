use crate::error::{ApiError, ApiResult};

/// Kubernetes object and namespace names: RFC 1123 labels.
pub fn dns_label(s: &str) -> bool {
    let b = s.as_bytes();
    !b.is_empty()
        && b.len() <= 63
        && b.iter()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-')
        && b[0] != b'-'
        && b[b.len() - 1] != b'-'
}

pub fn require_dns_label(s: &str, what: &str) -> ApiResult<()> {
    if dns_label(s) {
        Ok(())
    } else {
        Err(ApiError::bad_request(format!(
            "{what} must be lowercase letters, digits and dashes (max 63)."
        )))
    }
}

pub fn require_text(s: &str, what: &str, max: usize) -> ApiResult<String> {
    let t = s.trim();
    if t.is_empty() {
        return Err(ApiError::bad_request(format!("{what} is required.")));
    }
    if t.chars().count() > max {
        return Err(ApiError::bad_request(format!(
            "{what} is too long (max {max})."
        )));
    }
    Ok(t.to_owned())
}

pub fn slugify(s: &str) -> String {
    let mut out = String::new();
    for c in s.to_lowercase().chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c);
        } else if !out.ends_with('-') && !out.is_empty() {
            out.push('-');
        }
    }
    out.trim_matches('-').chars().take(40).collect()
}

/// Hostnames for HTTP routes: labels separated by dots, no scheme or port.
pub fn hostname(s: &str) -> bool {
    s.len() <= 253
        && s.contains('.')
        && s.split('.').all(|l| {
            !l.is_empty()
                && l.len() <= 63
                && l.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-')
        })
}

/// Equal, in time that doesn't depend on where they differ (secrets).
pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |d, (x, y)| d | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels() {
        assert!(dns_label("web-1"));
        assert!(!dns_label("-web"));
        assert!(!dns_label("Web"));
        assert!(!dns_label(""));
    }

    #[test]
    fn slugs() {
        assert_eq!(slugify("Acme Platform!"), "acme-platform");
        assert_eq!(slugify("  --x--  "), "x");
    }

    #[test]
    fn hosts() {
        assert!(hostname("app.example.com"));
        assert!(!hostname("localhost"));
        assert!(!hostname("https://a.b"));
    }
}
