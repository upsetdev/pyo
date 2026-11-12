//! .env parsing, the same as the dashboard's (dash/src/lib/pyo/dotenv.ts):
//!
//!   # comment            skipped, as are blank lines
//!   export KEY=value     "export " is dropped
//!   KEY=value # note     inline comment after an unquoted value
//!   KEY='literal $x\n'   single quotes: taken as is
//!   KEY="a\nb"           double quotes: \n \r \t \" \\ escapes, may span lines
//!   KEY=                 empty value

#[derive(Debug, Default, PartialEq)]
pub struct Parsed {
    pub entries: Vec<(String, String)>,
    /// Lines that aren't KEY=value (trimmed), comments and blanks excluded.
    pub invalid: Vec<String>,
}

/// Letters, digits and _, not starting with a digit.
pub fn valid_key(k: &str) -> bool {
    let mut c = k.chars();
    matches!(c.next(), Some(f) if f.is_ascii_alphabetic() || f == '_')
        && c.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Byte index of the closing `quote` in `s` from `from`.
fn closing(s: &str, quote: char, from: usize) -> Option<usize> {
    let mut it = s.char_indices().skip_while(|(i, _)| *i < from);
    while let Some((i, c)) = it.next() {
        if quote == '"' && c == '\\' {
            it.next();
        } else if c == quote {
            return Some(i);
        }
    }
    None
}

fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars();
    while let Some(c) = it.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match it.next() {
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some('t') => out.push('\t'),
            Some('"') => out.push('"'),
            Some('\\') => out.push('\\'),
            Some(o) => {
                out.push('\\');
                out.push(o);
            }
            None => out.push('\\'),
        }
    }
    out
}

/// "KEY = value" → ("KEY", "value"), "export " dropped; the key unchecked.
fn split(line: &str) -> Option<(&str, &str)> {
    let line = match line.strip_prefix("export") {
        Some(rest) if rest.starts_with(char::is_whitespace) && rest.contains('=') => {
            rest.trim_start()
        }
        _ => line,
    };
    let (k, v) = line.split_once('=')?;
    Some((k.trim_end(), v.trim_start()))
}

/// An unquoted value: up to a " #" comment, trimmed.
fn unquoted(v: &str) -> &str {
    let cut = v
        .char_indices()
        .find(|&(i, c)| c == '#' && i > 0 && v[..i].ends_with(char::is_whitespace))
        .map(|(i, _)| i);
    v[..cut.unwrap_or(v.len())].trim()
}

pub fn parse(text: &str) -> Parsed {
    let text = text.replace("\r\n", "\n").replace('\r', "\n");
    let lines: Vec<&str> = text.split('\n').collect();
    let mut out = Parsed::default();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i].trim();
        i += 1;
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, rest)) = split(line).filter(|(k, _)| valid_key(k)) else {
            out.invalid.push(line.to_owned());
            continue;
        };
        let quote = rest.chars().next();
        if let Some(q @ ('"' | '\'')) = quote {
            // A quoted value runs to its closing quote, over lines if need be.
            let mut rest = rest.to_owned();
            let mut end = closing(&rest, q, 1);
            let mut j = i;
            while end.is_none() && j < lines.len() {
                rest.push('\n');
                rest.push_str(lines[j]);
                j += 1;
                end = closing(&rest, q, 1);
            }
            let Some(end) = end else {
                out.invalid.push(line.to_owned());
                continue;
            };
            i = j;
            let body = &rest[1..end];
            out.entries.push((
                key.to_owned(),
                if q == '\'' {
                    body.to_owned()
                } else {
                    unescape(body)
                },
            ));
        } else {
            out.entries
                .push((key.to_owned(), unquoted(rest).to_owned()));
        }
    }
    out
}

/// One entry per key, the last one winning (in first-seen order); the
/// keys that repeated.
pub fn last_wins(entries: Vec<(String, String)>) -> (Vec<(String, String)>, Vec<String>) {
    let mut out: Vec<(String, String)> = vec![];
    let mut repeated: Vec<String> = vec![];
    for (k, v) in entries {
        match out.iter_mut().find(|(x, _)| *x == k) {
            Some(e) => {
                e.1 = v;
                if !repeated.contains(&k) {
                    repeated.push(k);
                }
            }
            None => out.push((k, v)),
        }
    }
    (out, repeated)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn e(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn plain_lines_comments_and_export() {
        let p = parse("# a comment\n\nA=1\nexport B=two\n  C = three  \nD=\n");
        assert_eq!(
            p.entries,
            e(&[("A", "1"), ("B", "two"), ("C", "three"), ("D", "")])
        );
        assert!(p.invalid.is_empty());
    }

    #[test]
    fn inline_comments_after_unquoted_values() {
        let p = parse("A=value # note\nB=a#b\nC=#x\nD=\"q # kept\" # note\n");
        assert_eq!(
            p.entries,
            e(&[("A", "value"), ("B", "a#b"), ("C", "#x"), ("D", "q # kept")])
        );
    }

    #[test]
    fn single_quotes_are_literal() {
        let p = parse("A='literal $x\\n'\nB='it''s'\n");
        assert_eq!(p.entries[0], ("A".into(), "literal $x\\n".into()));
        // Only up to the first closing quote.
        assert_eq!(p.entries[1], ("B".into(), "it".into()));
    }

    #[test]
    fn double_quotes_unescape() {
        let p = parse(r#"A="a\nb\tc\"d\\e\qf""#);
        assert_eq!(p.entries, e(&[("A", "a\nb\tc\"d\\e\\qf")]));
    }

    #[test]
    fn multi_line_double_quotes() {
        let p = parse("KEY=\"-----BEGIN-----\nabc\n-----END-----\"\nNEXT=1\n");
        assert_eq!(
            p.entries,
            e(&[
                ("KEY", "-----BEGIN-----\nabc\n-----END-----"),
                ("NEXT", "1")
            ])
        );
    }

    #[test]
    fn unclosed_quote_is_invalid() {
        let p = parse("A=\"never closed\nB=1\n");
        assert_eq!(p.invalid, vec!["A=\"never closed".to_string()]);
        assert_eq!(p.entries, e(&[("B", "1")]));
    }

    #[test]
    fn crlf() {
        let p = parse("A=1\r\nB=\"x\r\ny\"\r\nC=3\r");
        assert_eq!(p.entries, e(&[("A", "1"), ("B", "x\ny"), ("C", "3")]));
    }

    #[test]
    fn invalid_keys_and_lines() {
        let p = parse("1A=x\nA-B=y\njust text\n=v\nexport\n_OK=1\na1=2\n");
        assert_eq!(
            p.invalid,
            vec!["1A=x", "A-B=y", "just text", "=v", "export"]
        );
        assert_eq!(p.entries, e(&[("_OK", "1"), ("a1", "2")]));
    }

    #[test]
    fn export_as_a_key() {
        assert_eq!(parse("export=1").entries, e(&[("export", "1")]));
    }

    #[test]
    fn last_one_wins() {
        let (entries, repeated) = last_wins(e(&[("A", "1"), ("B", "2"), ("A", "3")]));
        assert_eq!(entries, e(&[("A", "3"), ("B", "2")]));
        assert_eq!(repeated, vec!["A".to_string()]);
    }

    #[test]
    fn keys() {
        assert!(valid_key("A_1") && valid_key("_x"));
        assert!(!valid_key("") && !valid_key("1A") && !valid_key("A B") && !valid_key("É"));
    }
}
