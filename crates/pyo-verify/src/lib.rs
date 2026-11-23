//! Pyo's verifier: a small root program on the first server of a cluster
//! that is connected to a Pyo it does not run itself (Pyo Cloud). A systemd
//! timer runs it.
//!
//! It takes only what a signed Pyo release defines, and only for the
//! version Pyo's control plane names as live. Two parties have to agree:
//! the release workflow signed the release (checked offline against
//! Sigstore's public trust root, compiled in), and the control plane says
//! that version is live. The control plane alone can only pick among
//! releases the workflow signed, never below what the server already has,
//! and never one younger than a day.
//!
//! This release is the bootstrap only: the verifier checks a release,
//! replaces itself with that release's verifier and hands over to it. It
//! applies nothing to the cluster. See run.rs.
//!
//! What it never does: write Secrets, write any cluster object besides its
//! own status ConfigMap, read or send a token, or take its trust root from
//! anywhere but its own binary.

pub mod config;
pub mod fetch;
pub mod install;
pub mod release;
pub mod run;
pub mod state;
pub mod status;
pub mod sys;
pub mod update;
pub mod version;

use version::Channel;

pub const USAGE: &str = "\
Usage: pyo-verify <command>

  run                      check for a signed release now (the timer runs this)
  status                   show what the last run found
  install [--url <address>] [--channel stable|rc]
                           set up the config and the timer
  uninstall                remove the verifier from this server
  version                  print this verifier's version";

#[derive(Debug, PartialEq)]
pub enum Command {
    Run {
        handed_over_from: Option<String>,
    },
    Status,
    Install {
        url: Option<String>,
        channel: Option<Channel>,
    },
    Uninstall,
    Version,
}

/// The command line, without the program's name. Anything it does not know
/// is refused: there are no hidden flags.
pub fn parse_args(args: &[String]) -> Option<Command> {
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    match args.as_slice() {
        ["run"] => Some(Command::Run {
            handed_over_from: None,
        }),
        ["run", "--handed-over-from", from] => Some(Command::Run {
            handed_over_from: Some((*from).to_owned()),
        }),
        ["status"] => Some(Command::Status),
        ["uninstall"] => Some(Command::Uninstall),
        ["version"] => Some(Command::Version),
        ["install", rest @ ..] => {
            let (mut url, mut channel) = (None, None);
            let mut rest = rest.iter();
            while let Some(flag) = rest.next() {
                match (*flag, rest.next()) {
                    ("--url", Some(v)) if url.is_none() => url = Some((*v).to_owned()),
                    ("--channel", Some(&"stable")) if channel.is_none() => {
                        channel = Some(Channel::Stable)
                    }
                    ("--channel", Some(&"rc")) if channel.is_none() => channel = Some(Channel::Rc),
                    _ => return None,
                }
            }
            Some(Command::Install { url, channel })
        }
        _ => None,
    }
}

#[cfg(test)]
pub(crate) mod testdir {
    use std::{
        cell::RefCell,
        path::{Path, PathBuf},
        sync::{
            RwLock, RwLockReadGuard, RwLockWriteGuard,
            atomic::{AtomicU32, Ordering},
        },
    };

    /// A directory of its own for one test, removed with it.
    pub struct TestDir(PathBuf);

    static NEXT: AtomicU32 = AtomicU32::new(0);

    /// Tests that start real programs hold this for writing, tests that
    /// take the verifier's lock for reading. A program started by another
    /// thread shares this process's open files until it execs, the lock
    /// file with its lock among them, which would make a run see the lock
    /// as held for a moment.
    static SPAWNING: RwLock<()> = RwLock::new(());

    thread_local! {
        /// This thread's hold on `SPAWNING`, and how many share it.
        static QUIET: RefCell<(u32, Option<RwLockReadGuard<'static, ()>>)> =
            const { RefCell::new((0, None)) };
    }

    pub fn spawning() -> RwLockWriteGuard<'static, ()> {
        SPAWNING.write().unwrap_or_else(|e| e.into_inner())
    }

    /// While one of these lives, no test starts a program.
    pub struct Quiet;

    impl Quiet {
        pub fn new() -> Quiet {
            QUIET.with_borrow_mut(|(count, guard)| {
                if *count == 0 {
                    *guard = Some(SPAWNING.read().unwrap_or_else(|e| e.into_inner()));
                }
                *count += 1;
            });
            Quiet
        }
    }

    impl Drop for Quiet {
        fn drop(&mut self) {
            QUIET.with_borrow_mut(|(count, guard)| {
                *count -= 1;
                if *count == 0 {
                    *guard = None;
                }
            });
        }
    }

    impl TestDir {
        pub fn new() -> TestDir {
            let dir = std::env::temp_dir().join(format!(
                "pyo-verify-test-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            TestDir(dir)
        }

        pub fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(line: &str) -> Vec<String> {
        line.split_whitespace().map(str::to_owned).collect()
    }

    #[test]
    fn the_commands_parse() {
        assert_eq!(
            parse_args(&args("run")),
            Some(Command::Run {
                handed_over_from: None
            })
        );
        assert_eq!(
            parse_args(&args("run --handed-over-from 0.51.0")),
            Some(Command::Run {
                handed_over_from: Some("0.51.0".into())
            })
        );
        assert_eq!(parse_args(&args("status")), Some(Command::Status));
        assert_eq!(parse_args(&args("uninstall")), Some(Command::Uninstall));
        assert_eq!(parse_args(&args("version")), Some(Command::Version));
        assert_eq!(
            parse_args(&args("install --channel rc --url https://pyo.example")),
            Some(Command::Install {
                url: Some("https://pyo.example".into()),
                channel: Some(Channel::Rc)
            })
        );
        assert_eq!(
            parse_args(&args("install")),
            Some(Command::Install {
                url: None,
                channel: None
            })
        );
    }

    #[test]
    fn anything_else_is_refused() {
        for line in [
            "",
            "apply",
            "run now",
            "run --handed-over-from",
            "run --force",
            "run --trust-root /tmp/root.pem",
            "run --insecure",
            "run --signer someone",
            "run --source /tmp",
            "run --url https://evil.example",
            "run --floor 0.1.0",
            "install --channel beta",
            "install --url",
            "install --url a --url b",
            "install --trust-root x",
            "status --json",
            "version --all",
            "--help",
        ] {
            assert_eq!(parse_args(&args(line)), None, "{line:?}");
        }
    }

    /// The trust root is the one compiled into pyo-sigstore, and what a run
    /// does comes from /etc/pyo/verify.conf alone. This holds the code to
    /// it: outside of tests, no source file reads an environment variable,
    /// and only main.rs builds the verifier, as `Sigstore`, a type without
    /// fields (release.rs asserts that when compiling). The config has no
    /// key for it (config.rs) and the command line no flag (above).
    #[test]
    fn nothing_can_swap_the_trust_root() {
        let sources = [
            ("lib.rs", include_str!("lib.rs")),
            ("main.rs", include_str!("main.rs")),
            ("config.rs", include_str!("config.rs")),
            ("fetch.rs", include_str!("fetch.rs")),
            ("install.rs", include_str!("install.rs")),
            ("release.rs", include_str!("release.rs")),
            ("run.rs", include_str!("run.rs")),
            ("state.rs", include_str!("state.rs")),
            ("status.rs", include_str!("status.rs")),
            ("sys.rs", include_str!("sys.rs")),
            ("update.rs", include_str!("update.rs")),
            ("version.rs", include_str!("version.rs")),
        ];
        // Every module is in the list.
        let mods = sources[0]
            .1
            .lines()
            .filter(|l| l.starts_with("pub mod "))
            .count();
        assert_eq!(mods + 2, sources.len());
        let var = ["env::", "var"].concat();
        let cfg_test = ["#[cfg(", "test)]"].concat();
        for (name, text) in sources {
            // What ships: the file up to its tests.
            let shipped = text.split(&cfg_test).next().unwrap();
            assert!(!shipped.contains(&var), "{name} reads the environment");
            assert!(!shipped.contains("option_env!"), "{name}");
            let builds_verifier = shipped.contains("= Env {");
            assert_eq!(builds_verifier, name == "main.rs", "{name}");
        }
        assert!(sources[1].1.contains("verifier: &Sigstore,"));
        // And the checker crate has one trust root, its own files.
        let sigstore = include_str!("../../pyo-sigstore/src/lib.rs");
        let shipped = sigstore.split(&cfg_test).next().unwrap();
        assert_eq!(shipped.matches("include_bytes!(\"../assets/").count(), 3);
        assert!(!shipped.contains(&var));
        for root in ["FULCIO_ROOT", "FULCIO_INTERMEDIATE", "REKOR_KEY"] {
            assert!(
                shipped.contains(&format!("const {root}: &[u8] = include_bytes!")),
                "{root}"
            );
            assert!(!shipped.contains(&format!("pub const {root}")), "{root}");
        }
    }
}
