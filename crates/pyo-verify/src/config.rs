//! `/etc/pyo/verify.conf`: `key=value` lines, written by `install` and
//! edited by the server's owner.
//!
//! ```text
//! url=https://pyo.example.com
//! hold=false
//! channel=stable
//! # source=/srv/pyo-releases
//! ```
//!
//! A key the verifier does not know, or a value it cannot read, stops it:
//! a typo in `hold` must not read as "go on". There is no key for what the
//! verifier trusts.

use crate::{
    fetch::{Source, origin},
    sys::{Env, Problem, read_limited},
    version::Channel,
};

pub const PATH: &str = "/etc/pyo/verify.conf";
const DAY: i64 = 24 * 3600;

#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    /// The control plane's public URL, without a trailing slash.
    pub url: String,
    pub hold: bool,
    pub channel: Channel,
    pub source: Source,
    /// How old a release's signature has to be before it is used.
    pub soak_secs: i64,
}

fn bad(what: &str) -> Problem {
    Problem::new(format!(
        "{PATH} has a line the verifier cannot use ({what}). Nothing was changed."
    ))
}

pub fn parse(text: &str) -> Result<Config, Problem> {
    let (mut url, mut hold, mut channel, mut source, mut soak_hours) =
        (None, false, Channel::Stable, Source::GitHub, None);
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (key, value) = line.split_once('=').ok_or_else(|| bad("no ="))?;
        let key = key.trim().to_ascii_lowercase();
        let value = value.trim();
        let value = value
            .strip_prefix('"')
            .and_then(|v| v.strip_suffix('"'))
            .unwrap_or(value);
        match key.as_str() {
            "url" => {
                let value = value.trim_end_matches('/');
                origin(value).ok_or_else(|| bad("url"))?;
                url = Some(value.to_owned());
            }
            "hold" => {
                hold = match value.to_ascii_lowercase().as_str() {
                    "true" | "yes" | "on" | "1" => true,
                    "false" | "no" | "off" | "0" => false,
                    _ => return Err(bad("hold")),
                }
            }
            "channel" => {
                channel = match value {
                    "stable" => Channel::Stable,
                    "rc" => Channel::Rc,
                    _ => return Err(bad("channel")),
                }
            }
            "source" => {
                let value = value.trim_end_matches('/');
                source = if value.starts_with('/') {
                    Source::Dir(value.into())
                } else if origin(value).is_some() {
                    Source::Mirror(value.to_owned())
                } else {
                    return Err(bad("source"));
                }
            }
            "soak_hours" => {
                let hours: i64 = value.parse().map_err(|_| bad("soak_hours"))?;
                if !(0..=24).contains(&hours) {
                    return Err(bad("soak_hours"));
                }
                soak_hours = Some(hours);
            }
            _ => return Err(bad(&key.chars().take(20).collect::<String>())),
        }
    }
    let url = url.ok_or_else(|| bad("no url"))?;
    // Only test servers shorten the wait or talk to Pyo without TLS.
    if channel != Channel::Rc {
        if soak_hours.is_some() {
            return Err(bad("soak_hours needs channel=rc"));
        }
        if !url.starts_with("https://") {
            return Err(bad("url must be https"));
        }
    }
    Ok(Config {
        url,
        hold,
        channel,
        source,
        soak_secs: soak_hours.map_or(DAY, |h| h * 3600),
    })
}

pub fn load(env: &Env) -> Result<Config, Problem> {
    let text = read_limited(&env.path(PATH), 64 * 1024)
        .ok()
        .and_then(|b| String::from_utf8(b).ok())
        .ok_or_else(|| {
            Problem::new(
                "The verifier is not set up on this server. Run the connect command from Pyo again.",
            )
        })?;
    parse(&text)
}

/// What `install` writes when there is no file yet.
pub fn fresh(url: &str, channel: Channel) -> String {
    format!(
        "# Pyo's verifier on this server. hold=true stops updates.\nurl={url}\nhold=false\nchannel={}\n",
        match channel {
            Channel::Stable => "stable",
            Channel::Rc => "rc",
        }
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_config_file_reads() {
        let c = parse("# comment\n\nurl = https://pyo.example.com/\nHOLD=\"true\"\n").unwrap();
        assert_eq!(c.url, "https://pyo.example.com");
        assert!(c.hold);
        assert_eq!(c.channel, Channel::Stable);
        assert_eq!(c.source, Source::GitHub);
        assert_eq!(c.soak_secs, DAY);

        let c = parse("url=http://10.0.0.2:8000\nchannel=rc\nsoak_hours=0\nsource=/srv/rel/\n")
            .unwrap();
        assert_eq!(c.channel, Channel::Rc);
        assert_eq!(c.soak_secs, 0);
        assert_eq!(c.source, Source::Dir("/srv/rel".into()));
        assert_eq!(
            parse("url=https://a.example\nsource=https://mirror.example/pyo\n")
                .unwrap()
                .source,
            Source::Mirror("https://mirror.example/pyo".into())
        );
        assert_eq!(
            parse(&fresh("https://a.example", Channel::Rc))
                .unwrap()
                .channel,
            Channel::Rc
        );
    }

    #[test]
    fn a_config_it_cannot_read_stops_it() {
        for bad in [
            "",
            "hold=false",
            "url=pyo.example.com",
            "url=https://a.example\nhold=maybe",
            "url=https://a.example\nhodl=true",
            "url=https://a.example\nchannel=beta",
            "url=https://a.example\nsource=relative/dir",
            "url=https://a.example\njust words",
            // The wait only shortens on the rc channel, and never grows.
            "url=https://a.example\nsoak_hours=1",
            "url=https://a.example\nchannel=rc\nsoak_hours=25",
            "url=https://a.example\nchannel=rc\nsoak_hours=-1",
            // Plain HTTP to Pyo is for test servers.
            "url=http://a.example",
        ] {
            assert!(parse(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn nothing_in_the_config_names_what_to_trust() {
        for key in [
            "trust_root",
            "root",
            "fulcio",
            "rekor",
            "signer",
            "identity",
            "issuer",
            "key",
            "insecure",
            "verify",
            "skip_verify",
            "floor",
        ] {
            assert!(
                parse(&format!("url=https://a.example\n{key}=x")).is_err(),
                "{key}"
            );
        }
    }
}
