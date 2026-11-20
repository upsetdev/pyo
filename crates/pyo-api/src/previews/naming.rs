//! Names of previews: `<service>-pr-<n>`, as the copy's name in its project
//! and as the first label of its URL.

use crate::ids::sha256_hex;

/// A DNS label is at most 63 characters.
pub const LABEL_MAX: usize = 63;
/// A service's name is at most 40 (util.rs `slugify`).
pub const NAME_MAX: usize = 40;

/// `<service>-pr-<n>`, at most `max` characters. A longer one keeps the
/// start of the service's name and a short hash of the whole, so two long
/// names that start alike stay apart: "a-very-long-…-3f9a1c-pr-42".
pub fn label(service: &str, number: i64, max: usize) -> String {
    let full = format!("{service}-pr-{number}");
    if full.len() <= max {
        return full;
    }
    let tail = format!("-pr-{number}");
    let hash = &sha256_hex(&full)[..6];
    let keep = max.saturating_sub(tail.len() + 1 + hash.len());
    let head: String = service.chars().take(keep).collect();
    let head = head.trim_end_matches('-');
    if head.is_empty() {
        format!("{hash}{tail}")
    } else {
        format!("{head}-{hash}{tail}")
    }
}

/// The label again, told apart by a short hash of `salt` (the service's
/// id), for when another service already has the plain one.
pub fn label_salted(service: &str, number: i64, max: usize, salt: &str) -> String {
    let h = &sha256_hex(salt)[..4];
    label(&format!("{service}-{h}"), number, max)
}

/// A host under `suffix` ("203.0.113.7.nip.io", "tail1234.ts.net",
/// "preview.example.com").
pub fn host(label: &str, suffix: &str) -> String {
    format!("{label}.{}", suffix.trim_matches('.').to_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_names_stay_as_they_are() {
        assert_eq!(label("web", 42, LABEL_MAX), "web-pr-42");
        assert_eq!(
            host("web-pr-42", "203.0.113.7.nip.io"),
            "web-pr-42.203.0.113.7.nip.io"
        );
        assert_eq!(
            host("web-pr-42", ".Tail1234.ts.net."),
            "web-pr-42.tail1234.ts.net"
        );
    }

    #[test]
    fn long_names_are_cut_with_a_hash() {
        let long = "a-service-with-a-really-quite-long-name-for-testing-labels";
        let l = label(long, 1234, LABEL_MAX);
        assert!(l.len() <= LABEL_MAX, "{l}");
        assert!(l.ends_with("-pr-1234"), "{l}");
        assert!(l.starts_with("a-service-with"), "{l}");
        // Stable, and apart from another long name that starts the same.
        assert_eq!(l, label(long, 1234, LABEL_MAX));
        assert_ne!(l, label(&format!("{long}-two"), 1234, LABEL_MAX));
        // Service names are at most 40 characters.
        let n = label(long, 7, NAME_MAX);
        assert!(n.len() <= NAME_MAX, "{n}");
        assert!(n.ends_with("-pr-7"));
        // Exactly at the limit is kept whole.
        let exact = "x".repeat(LABEL_MAX - "-pr-1".len());
        assert_eq!(label(&exact, 1, LABEL_MAX), format!("{exact}-pr-1"));
        assert_ne!(
            label(&format!("{exact}y"), 1, LABEL_MAX),
            format!("{exact}y-pr-1")
        );
    }

    #[test]
    fn no_dash_before_the_hash_twice() {
        // A cut that ends on a dash doesn't leave "--".
        let l = label("abcdefghij-klmnopqrst", 1, 20);
        assert!(!l.contains("--"), "{l}");
        assert!(l.len() <= 20, "{l}");
    }

    #[test]
    fn salted_labels_differ() {
        let a = label_salted("web", 42, LABEL_MAX, "svc_a");
        let b = label_salted("web", 42, LABEL_MAX, "svc_b");
        assert_ne!(a, b);
        assert!(a.starts_with("web-") && a.ends_with("-pr-42"), "{a}");
    }
}
