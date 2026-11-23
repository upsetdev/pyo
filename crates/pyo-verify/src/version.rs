//! Release versions as the verifier takes them: `X.Y.Z`, and `X.Y.Z-rc.N`
//! on the rc channel. One spelling per version (no leading zeros), since the
//! version becomes the release tag the signature has to name exactly.

use std::{cmp::Ordering, fmt};

/// Which versions a server takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Channel {
    Stable,
    /// Release candidates too: for testing.
    Rc,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Version {
    major: u32,
    minor: u32,
    patch: u32,
    /// `-rc.N`: before the release of the same numbers.
    rc: Option<u32>,
}

/// A number without a sign, spaces or leading zeros.
fn number(s: &str) -> Option<u32> {
    if s.is_empty() || s.len() > 9 || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    if s.len() > 1 && s.starts_with('0') {
        return None;
    }
    s.parse().ok()
}

impl Version {
    /// `X.Y.Z` or `X.Y.Z-rc.N`, and nothing else.
    pub fn parse(s: &str) -> Option<Version> {
        let (numbers, rc) = match s.split_once('-') {
            Some((n, pre)) => (n, Some(number(pre.strip_prefix("rc.")?)?)),
            None => (s, None),
        };
        let mut parts = numbers.split('.');
        let v = Version {
            major: number(parts.next()?)?,
            minor: number(parts.next()?)?,
            patch: number(parts.next()?)?,
            rc,
        };
        parts.next().is_none().then_some(v)
    }

    pub fn is_rc(&self) -> bool {
        self.rc.is_some()
    }

    /// This build's own version.
    pub fn own() -> Version {
        Version::parse(env!("CARGO_PKG_VERSION")).expect("the crate's version is X.Y.Z")
    }

    fn key(&self) -> (u32, u32, u32, u64) {
        (
            self.major,
            self.minor,
            self.patch,
            self.rc.map_or(u64::MAX, u64::from),
        )
    }
}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> Ordering {
        self.key().cmp(&other.key())
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)?;
        match self.rc {
            Some(n) => write!(f, "-rc.{n}"),
            None => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &str) -> Version {
        Version::parse(s).unwrap()
    }

    #[test]
    fn only_plain_versions_parse() {
        for ok in ["0.51.0", "1.2.3", "10.20.30", "0.51.0-rc.1", "0.0.0"] {
            assert_eq!(v(ok).to_string(), ok);
        }
        for bad in [
            "",
            "0.51",
            "0.51.0.1",
            "v0.51.0",
            "0.51.0 ",
            " 0.51.0",
            "0.51.0\n",
            "00.51.0",
            "0.051.0",
            "0.51.0-rc1",
            "0.51.0-rc.",
            "0.51.0-rc.01",
            "0.51.0-beta.1",
            "0.51.0-rc.1-rc.2",
            "0.51.0+build",
            "0.51.x",
            "-1.0.0",
            "+1.0.0",
            "0.51.0/../x",
            "1.2.3; rm -rf /",
            "99999999999.0.0",
            "٠.٥١.٠",
        ] {
            assert!(Version::parse(bad).is_none(), "{bad:?}");
        }
    }

    #[test]
    fn versions_order_by_number_and_rc_comes_first() {
        assert!(v("0.51.0") > v("0.5.5"));
        assert!(v("0.52.0") > v("0.51.9"));
        assert!(v("1.0.0") > v("0.99.99"));
        assert!(v("0.51.0") > v("0.51.0-rc.9"));
        assert!(v("0.51.0-rc.2") > v("0.51.0-rc.1"));
        assert!(v("0.51.0-rc.1") > v("0.50.9"));
        assert_eq!(v("0.51.0"), v("0.51.0"));
    }

    #[test]
    fn this_build_has_a_version() {
        assert_eq!(Version::own().to_string(), env!("CARGO_PKG_VERSION"));
    }
}
