//! What Pyo sizes by the memory of the server it runs on. Every user reads
//! the server's current memory when it starts, so a server given more (or
//! less) memory gets the matching settings at its next restart:
//! - the installers' boot-time helper: k3s's GOMEMLIMIT and compressed swap
//! - Postgres's settings (platform.yaml's pyo-postgres)
//! - whether a fresh install starts with observability on (pyo-api), and
//!   whether the upgrader creates ClickHouse stopped
//!
//! Containers see the server's /proc/meminfo (k3s doesn't virtualize it),
//! so a pod reads the same figure the installers do.

/// Under this much memory (MiB), a server is small: the same line as the
/// installers' small_machine and platform.yaml's pyo-postgres.
pub const SMALL_MIB: u64 = 3584;
/// From this much memory (MiB), a server is large (platform.yaml's pyo-postgres).
pub const LARGE_MIB: u64 = 8192;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    Small,
    Normal,
    Large,
}

pub fn tier(mib: u64) -> Tier {
    if mib < SMALL_MIB {
        Tier::Small
    } else if mib < LARGE_MIB {
        Tier::Normal
    } else {
        Tier::Large
    }
}

/// MemTotal from a /proc/meminfo, in MiB.
pub fn parse_meminfo(text: &str) -> Option<u64> {
    text.lines()
        .find_map(|l| l.strip_prefix("MemTotal:"))
        .and_then(|rest| rest.split_whitespace().next())
        .and_then(|kb| kb.parse::<u64>().ok())
        .map(|kb| kb / 1024)
}

/// This server's memory in MiB (PYO_MEMINFO names another file, for tests).
pub fn server_mib() -> Option<u64> {
    let path = std::env::var("PYO_MEMINFO").unwrap_or_else(|_| "/proc/meminfo".into());
    parse_meminfo(&std::fs::read_to_string(path).ok()?)
}

/// This server's tier; Normal when its memory can't be read.
pub fn server_tier() -> Tier {
    server_mib().map(tier).unwrap_or(Tier::Normal)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tiers_follow_memory() {
        assert_eq!(tier(1945), Tier::Small);
        assert_eq!(tier(3583), Tier::Small);
        assert_eq!(tier(3584), Tier::Normal);
        assert_eq!(tier(7900), Tier::Normal);
        assert_eq!(tier(8192), Tier::Large);
        assert_eq!(
            parse_meminfo("MemTotal:        1992180 kB\nMemFree: 1 kB\n"),
            Some(1945)
        );
        assert_eq!(parse_meminfo("nothing"), None);
    }
}
