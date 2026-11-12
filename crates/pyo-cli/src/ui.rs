//! Terminal output: short lines, color only on a terminal (and never with
//! NO_COLOR), and the few prompts the CLI asks.

use std::io::{IsTerminal, Write};

use anyhow::{Result, bail};

fn color() -> bool {
    std::env::var_os("NO_COLOR").is_none() && std::io::stdout().is_terminal()
}

fn paint(code: &str, s: &str) -> String {
    if color() {
        format!("\x1b[{code}m{s}\x1b[0m")
    } else {
        s.to_owned()
    }
}

pub fn dim(s: &str) -> String {
    paint("2", s)
}

pub fn bold(s: &str) -> String {
    paint("1", s)
}

pub fn green(s: &str) -> String {
    paint("32", s)
}

pub fn red(s: &str) -> String {
    paint("31", s)
}

pub fn yellow(s: &str) -> String {
    paint("33", s)
}

/// A log line's text. On a terminal: its own ANSI colors (nothing else
/// that moves the cursor or retitles the window), else tinted by its
/// level. Otherwise plain text.
pub fn log_text(msg: &str, level: &str) -> String {
    if !color() {
        return pyo_proto::ansi::strip(msg).into_owned();
    }
    let kept = pyo_proto::ansi::sgr_only(msg);
    if kept.contains('\x1b') {
        // Reset after, so an unclosed color doesn't run on.
        return format!("{kept}\x1b[0m");
    }
    match level {
        "error" => red(&kept),
        "warn" => yellow(&kept),
        _ => kept.into_owned(),
    }
}

pub fn ok(msg: &str) {
    println!("{} {msg}", green("✓"));
}

pub fn ask(prompt: &str) -> Result<String> {
    if !std::io::stdin().is_terminal() {
        bail!("{}", no_terminal(prompt));
    }
    print!("{prompt}");
    std::io::stdout().flush()?;
    let mut s = String::new();
    std::io::stdin().read_line(&mut s)?;
    Ok(s.trim().to_owned())
}

/// Why a question can't be asked without a terminal, and what to do:
/// every yes/no question here has -y.
fn no_terminal(prompt: &str) -> String {
    match prompt.trim_end().strip_suffix("[y/N]") {
        Some(q) => format!(
            "{} No terminal to ask on: pass -y to confirm.",
            q.trim_end()
        ),
        None => format!(
            "{} (no terminal to ask on)",
            prompt.trim_end_matches([' ', ':'])
        ),
    }
}

/// Pick one of `items` by number.
pub fn choose(title: &str, items: &[String]) -> Result<usize> {
    if items.is_empty() {
        bail!("Nothing to choose from.");
    }
    println!("{}", bold(title));
    for (i, it) in items.iter().enumerate() {
        println!("  {} {it}", dim(&format!("{:>2}.", i + 1)));
    }
    if !std::io::stdin().is_terminal() {
        bail!("Pass one of these names in full.");
    }
    loop {
        let a = ask(&format!("Number (1-{}): ", items.len()))?;
        match a.parse::<usize>() {
            Ok(n) if (1..=items.len()).contains(&n) => return Ok(n - 1),
            _ => println!("{}", yellow("Pick one of the numbers.")),
        }
    }
}

/// Open a URL in the browser; quietly does nothing where there's none.
pub fn open(url: &str) {
    let cmd = if cfg!(target_os = "macos") {
        ("open", vec![url])
    } else if cfg!(windows) {
        ("cmd", vec!["/C", "start", "", url])
    } else {
        ("xdg-open", vec![url])
    };
    let _ = std::process::Command::new(cmd.0)
        .args(cmd.1)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

/// "3m ago", from a timestamp in milliseconds.
pub fn ago(ms: i64) -> String {
    let s = (crate::api::now_ms() - ms).max(0) / 1000;
    match s {
        0..60 => "just now".into(),
        60..3600 => format!("{}m ago", s / 60),
        3600..86400 => format!("{}h ago", s / 3600),
        _ => format!("{}d ago", s / 86400),
    }
}

/// "14:03:22", local time isn't known without a time zone library: UTC.
pub fn clock(ms: i64) -> String {
    let s = (ms / 1000).rem_euclid(86400);
    format!("{:02}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60)
}

#[cfg(test)]
mod tests {
    #[test]
    fn says_which_flag_confirms() {
        assert_eq!(
            super::no_terminal("Remove a.example.com? It stops serving api right away. [y/N] "),
            "Remove a.example.com? It stops serving api right away. No terminal to ask on: pass -y to confirm."
        );
        assert_eq!(
            super::no_terminal("Number (1-3): "),
            "Number (1-3) (no terminal to ask on)"
        );
    }
}
