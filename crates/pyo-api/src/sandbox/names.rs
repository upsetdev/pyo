//! Sandbox names: for people to tell sandboxes apart and find them. They
//! aren't unique and never address a sandbox: the id does, so a rename
//! breaks nothing. A sandbox made without one gets a short name like
//! `quiet-river`, picked from its id, so a sandbox from before names (whose
//! name is NULL) shows the same name every time.

const ADJECTIVES: [&str; 64] = [
    "amber", "bold", "brave", "bright", "brisk", "calm", "clear", "clever", "cosy", "crisp",
    "curious", "daring", "eager", "early", "fair", "fancy", "fast", "fresh", "friendly", "gentle",
    "glad", "golden", "good", "grand", "green", "happy", "honest", "jolly", "keen", "kind",
    "lively", "lucky", "merry", "mighty", "misty", "neat", "nimble", "noble", "patient", "plain",
    "polite", "proud", "quick", "quiet", "rapid", "ready", "rosy", "shiny", "silent", "silver",
    "simple", "sleepy", "smooth", "snowy", "steady", "sunny", "swift", "tidy", "tiny", "warm",
    "wild", "wise", "witty", "young",
];

const NOUNS: [&str; 64] = [
    "acorn", "badger", "beach", "bear", "birch", "bird", "breeze", "brook", "canyon", "cedar",
    "cloud", "comet", "coral", "creek", "dawn", "deer", "dune", "eagle", "falcon", "fern", "field",
    "finch", "forest", "fox", "garden", "glade", "harbor", "hawk", "hill", "island", "lake",
    "leaf", "maple", "meadow", "moon", "moss", "mountain", "oak", "ocean", "otter", "owl", "panda",
    "pebble", "pine", "planet", "pond", "rain", "raven", "reef", "river", "robin", "sea", "shore",
    "sky", "spark", "star", "stone", "storm", "sun", "tiger", "valley", "wave", "willow", "wolf",
];

/// The longest name, in characters.
pub const MAX_LEN: usize = 63;

/// The name a sandbox gets when it's made without one, from its id. The
/// same id always gives the same name.
pub fn generated(id: &str) -> String {
    // FNV-1a: stable across releases, unlike std's hasher.
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in id.bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    // Then mixed (splitmix64's finish), so ids a character apart spread too.
    h ^= h >> 30;
    h = h.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    h ^= h >> 27;
    h = h.wrapping_mul(0x94d0_49bb_1331_11eb);
    h ^= h >> 31;
    let adjective = ADJECTIVES[(h % 64) as usize];
    let noun = NOUNS[((h >> 32) % 64) as usize];
    format!("{adjective}-{noun}")
}

/// A name someone gave, trimmed, or why it can't be one.
pub fn check(name: &str) -> Result<String, String> {
    let name = name.trim();
    if name.is_empty() {
        return Err("Give the sandbox a name.".into());
    }
    if name.chars().count() > MAX_LEN {
        return Err(format!("A sandbox name is {MAX_LEN} characters at most."));
    }
    if !name
        .chars()
        .all(|c| c.is_alphanumeric() || matches!(c, ' ' | '-' | '_' | '.'))
    {
        return Err(
            "A sandbox name can have letters, digits, spaces, dashes, underscores and dots.".into(),
        );
    }
    Ok(name.to_owned())
}

/// Whether a sandbox's name or id has `q` in it, ignoring case.
pub fn matches(name: &str, id: &str, q: &str) -> bool {
    let q = q.to_lowercase();
    name.to_lowercase().contains(&q) || id.to_lowercase().contains(&q)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_names_are_short_plain_and_stable() {
        let a = generated("sbx_0123456789abcdef");
        assert_eq!(
            a,
            generated("sbx_0123456789abcdef"),
            "the same id, the same name"
        );
        let (adj, noun) = a.split_once('-').unwrap();
        assert!(ADJECTIVES.contains(&adj) && NOUNS.contains(&noun), "{a}");
        assert!(check(&a).is_ok());
        // Different ids spread over the words.
        let names: std::collections::HashSet<String> =
            (0..200).map(|i| generated(&format!("sbx_{i}"))).collect();
        assert!(names.len() > 150, "{} names", names.len());
    }

    #[test]
    fn word_lists_are_plain() {
        for w in ADJECTIVES.iter().chain(NOUNS.iter()) {
            assert!(w.chars().all(|c| c.is_ascii_lowercase()), "{w}");
        }
        let a: std::collections::HashSet<_> = ADJECTIVES.iter().collect();
        let n: std::collections::HashSet<_> = NOUNS.iter().collect();
        assert_eq!((a.len(), n.len()), (64, 64), "no word twice");
    }

    #[test]
    fn names_people_give() {
        assert_eq!(check("  my sandbox ").unwrap(), "my sandbox");
        assert_eq!(check("job-42_v1.2").unwrap(), "job-42_v1.2");
        assert_eq!(check("café").unwrap(), "café");
        assert_eq!(check(&"a".repeat(63)).unwrap().len(), 63);
        assert!(check(&"a".repeat(64)).is_err());
        assert!(check("").is_err());
        assert!(check("   ").is_err());
        for bad in ["a/b", "a;b", "<x>", "tab\there", "line\nbreak", "a@b"] {
            let e = check(bad).unwrap_err();
            assert!(!e.contains(';'), "{e}");
        }
    }

    #[test]
    fn search_takes_the_name_or_the_id() {
        assert!(matches("Quiet River", "sbx_abc", "river"));
        assert!(matches("quiet-river", "sbx_ABC", "abc"));
        assert!(!matches("quiet-river", "sbx_abc", "lake"));
    }
}
