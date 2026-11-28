//! `pyo-verify run`: what the timer calls.
//!
//! In order, and on any failure nothing is changed:
//! 1. take the lock the connect command also takes;
//! 2. read the config, and stop on a hold (the config's or the cluster's);
//! 3. ask Pyo's control plane which version is live;
//! 4. go forward only: that version against the floor;
//! 5. fetch that release's release.json and check its signature for
//!    exactly that version;
//! 6. wait until the signature is old enough;
//! 7. replace this binary with that release's verifier and start it;
//! 8. write the status, whatever happened.
//!
//! Two parties have to agree before step 7: the release workflow signed the
//! release, and the control plane names it. Either alone changes nothing.
//!
//! This release stops there: it applies nothing to the cluster. The next
//! one adds an `apply` module, called from `steps` where a run finds the
//! live version to be its own, with the release.json it verified the same
//! way. Because step 7 comes first, the code that applies a release is
//! always that release's own.

use std::fs::File;

use crate::{
    config::{self, Config},
    fetch::{self, MAX_ANSWER, MAX_BUNDLE, MAX_MANIFEST},
    release::parse_manifest,
    state,
    status::{self, KUBECTL_TIMEOUT, Status},
    sys::{Env, Problem},
    update,
    version::{Channel, Version},
};

/// Shared with Pyo's connect command (deploy/install.sh takes it with
/// `flock` on fd 9): a run and a connect never overlap.
pub const LOCK: &str = "/run/pyo-connect.lock";
/// The customer's switches (pyo-proto's rbac.rs): read, never written.
const POLICY_CONFIGMAP: &str = "pyo-policy";

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Outcome {
    /// Something else holds the lock: nothing read, nothing written.
    Skipped,
    /// The live version is this server's.
    Current,
    Hold,
    /// A good release that is not used yet.
    Waiting,
    /// The live release has no verifier: the connect command is needed.
    NoVerifier,
    /// The new verifier was started in this one's place.
    HandedOver,
    Failed,
}

impl Outcome {
    /// For the process: a failure shows as a failed unit.
    pub fn exit_code(self) -> u8 {
        match self {
            Outcome::Failed | Outcome::NoVerifier => 1,
            _ => 0,
        }
    }
}

pub struct Report {
    pub outcome: Outcome,
    /// One plain line.
    pub message: String,
    /// For the journal.
    pub detail: Option<String>,
}

/// What a run learned on its way, for the status.
#[derive(Default)]
struct Seen {
    live: Option<Version>,
    hold: bool,
    /// The version whose verifier now is this binary's file.
    next: Option<Version>,
}

enum Lock {
    Taken(#[allow(dead_code)] File),
    Held,
    Failed(std::io::Error),
}

/// Not waited for. Closed (and so released) when this process ends or
/// hands over.
fn lock(env: &Env) -> Lock {
    let path = env.path(LOCK);
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path);
    match file {
        Err(e) => Lock::Failed(e),
        Ok(f) => match f.try_lock() {
            Ok(()) => Lock::Taken(f),
            Err(std::fs::TryLockError::WouldBlock) => Lock::Held,
            Err(std::fs::TryLockError::Error(e)) => Lock::Failed(e),
        },
    }
}

/// The cluster's `updates` switch says hold: `hold`, as Pyo reads it
/// (trimmed, any case). Empty or `auto` go on, anything else stops.
fn policy_hold(env: &Env) -> Result<bool, Problem> {
    let unreadable = || {
        Problem::new(
            "The verifier could not read this cluster's pyo-policy settings. Nothing was changed.",
        )
    };
    let out = env
        .runner
        .run(
            &[
                "k3s",
                "kubectl",
                KUBECTL_TIMEOUT,
                "-n",
                status::NAMESPACE,
                "get",
                "configmap",
                POLICY_CONFIGMAP,
                "--ignore-not-found",
                "-o",
                "jsonpath={.data.updates}",
            ],
            None,
        )
        .ok()
        .filter(|o| o.ok)
        .ok_or_else(unreadable)?;
    match String::from_utf8_lossy(&out.stdout)
        .trim()
        .to_ascii_lowercase()
        .as_str()
    {
        // What Pyo shows as on hold (pyo-proto's `Policy::updates_hold`).
        "hold" => Ok(true),
        "" | "auto" => Ok(false),
        // A value it does not know is not read as "go on".
        _ => Err(Problem::new(
            "The updates setting in this cluster's pyo-policy is not one the verifier knows. Nothing was changed.",
        )),
    }
}

/// The live version, from the control plane's answer: an untrusted string
/// until it parses as a version.
fn parse_answer(body: &[u8], channel: Channel) -> Result<Version, Problem> {
    let unusable = || {
        Problem::new("Pyo's answer about its live version could not be used. Nothing was changed.")
    };
    let v: serde_json::Value = serde_json::from_slice(body).map_err(|_| unusable())?;
    let version = v
        .as_object()
        .and_then(|o| o.get("version"))
        .and_then(|v| v.as_str())
        .and_then(Version::parse)
        .ok_or_else(unusable)?;
    if version.is_rc() && channel != Channel::Rc {
        return Err(Problem::new(
            "Pyo names a test version and this server only takes stable versions. Nothing was changed.",
        ));
    }
    Ok(version)
}

fn ask(env: &Env, cfg: &Config) -> Result<Version, Problem> {
    // No redirects: the answer comes from the address in the config.
    let body = fetch::get(
        env,
        &format!("{}/install/version", cfg.url),
        MAX_ANSWER,
        &[],
    )?;
    parse_answer(&body, cfg.channel)
}

fn steps(
    env: &Env,
    handed_over_from: Option<&str>,
    seen: &mut Seen,
) -> Result<(Outcome, String), Problem> {
    let cfg = config::load(env)?;
    if let Some(from) = handed_over_from {
        // Started by the verifier this one replaced. Never by itself or a
        // newer one: that would be a loop.
        Version::parse(from).filter(|f| *f < env.own).ok_or_else(|| {
            Problem::new(
                "The verifier was started by one that is not older than itself. Nothing was changed.",
            )
        })?;
        state::raise(env, env.own)?;
    }
    if cfg.hold {
        seen.hold = true;
        return Ok((
            Outcome::Hold,
            format!("Updates are on hold in {}.", config::PATH),
        ));
    }
    if policy_hold(env)? {
        seen.hold = true;
        return Ok((
            Outcome::Hold,
            "Updates are on hold in this cluster's pyo-policy settings.".into(),
        ));
    }

    let live = ask(env, &cfg)?;
    seen.live = Some(live);
    let floor = state::floor(env)?.map_or(env.own, |f| f.max(env.own));
    if live == floor {
        return Ok((
            Outcome::Current,
            format!("This server is on version {live}."),
        ));
    }
    if live < floor {
        return Err(Problem::new(format!(
            "Pyo names version {live} and this server is already on {floor}. Nothing was changed."
        )));
    }

    let manifest = cfg.source.fetch(env, &live, "release.json", MAX_MANIFEST)?;
    let bundle = cfg
        .source
        .fetch(env, &live, "release.json.sigstore.json", MAX_BUNDLE)?;
    // Nothing of the release is read before its signature checks out.
    let signed = env.verifier.verify(&manifest, &bundle, &live)?;
    let release = parse_manifest(&manifest, env.arch)?;
    if release.version != live.to_string() {
        return Err(Problem::new(format!(
            "The signed release names another version than {live}. Nothing was changed."
        )));
    }

    // The age is the log's time against this server's clock. A clock that
    // runs behind makes the wait longer, one that runs ahead shorter.
    let age = env.clock.now() - signed.logged_at;
    if age < 0 {
        return Ok((
            Outcome::Waiting,
            format!(
                "Release {live} is dated after this server's clock. Check the clock of this server."
            ),
        ));
    }
    if age < cfg.soak_secs {
        return Ok((
            Outcome::Waiting,
            format!(
                "Release {live} was signed {} hours ago. This server takes it once it is {} hours old.",
                age / 3600,
                cfg.soak_secs / 3600
            ),
        ));
    }

    let Some(sha256) = release.verifier_sha256 else {
        return Ok((
            Outcome::NoVerifier,
            format!(
                "Release {live} has no verifier. Run the connect command from Pyo to update this server."
            ),
        ));
    };
    if handed_over_from.is_some() {
        // One hand-over per run.
        return Ok((
            Outcome::Waiting,
            format!("Release {live} is newer than this verifier. It is taken on the next run."),
        ));
    }
    update::replace(env, &cfg.source, &live, &sha256)?;
    seen.next = Some(live);
    Ok((
        Outcome::HandedOver,
        format!("The verifier is updating itself to {live}."),
    ))
}

fn status_of(env: &Env, seen: &Seen, outcome: Outcome, message: &str) -> Status {
    let failed = outcome.exit_code() != 0;
    Status {
        verifier_version: env.own.to_string(),
        live_version: seen.live.map(|v| v.to_string()).unwrap_or_default(),
        last_check: status::rfc3339(env.clock.now()),
        state: match outcome {
            Outcome::Current => "current",
            Outcome::Hold => "hold",
            Outcome::Waiting => "waiting",
            Outcome::HandedOver => "updating",
            Outcome::Failed | Outcome::NoVerifier | Outcome::Skipped => "error",
        }
        .into(),
        message: message.to_owned(),
        last_error: if failed {
            message.to_owned()
        } else {
            String::new()
        },
        hold: seen.hold,
        host: status::host(env),
    }
}

/// One run. `handed_over_from`: the version of the verifier that started
/// this one in its place (`run --handed-over-from <version>`).
pub fn run(env: &Env, handed_over_from: Option<&str>) -> Report {
    let _lock = match lock(env) {
        Lock::Taken(f) => f,
        Lock::Held => {
            return Report {
                outcome: Outcome::Skipped,
                message: "Another Pyo setup step is running on this server. The verifier tries again later.".into(),
                detail: None,
            };
        }
        Lock::Failed(e) => {
            return Report {
                outcome: Outcome::Failed,
                message: "The verifier needs to run as root.".into(),
                detail: Some(e.to_string()),
            };
        }
    };

    let mut seen = Seen::default();
    let (mut outcome, mut message, mut detail) = match steps(env, handed_over_from, &mut seen) {
        Ok((outcome, message)) => (outcome, message, None),
        Err(p) => (Outcome::Failed, p.plain, p.detail),
    };
    status::write(env, &status_of(env, &seen, outcome, &message));

    if let Some(next) = seen.next {
        let own = env.own.to_string();
        // Said here: after a hand-over this process is gone. The lock is
        // released by the exec, and the new verifier takes it again.
        println!("{message}");
        let started = if next > env.own {
            env.handover
                .exec(&env.exe, &["run", "--handed-over-from", &own])
        } else {
            Err(std::io::Error::other("not a newer version"))
        };
        if let Err(e) = started {
            let back = update::roll_back(env);
            outcome = Outcome::Failed;
            message = format!(
                "The verifier of release {next} could not be started. This server keeps verifier {own}."
            );
            detail = Some(format!("{e} (previous binary back: {})", back.is_ok()));
            status::write(env, &status_of(env, &seen, outcome, &message));
        }
    }
    Report {
        outcome,
        message,
        detail,
    }
}

#[cfg(test)]
mod tests {
    use std::{
        cell::{Cell, RefCell},
        collections::HashMap,
        path::{Path, PathBuf},
    };

    use sha2::{Digest, Sha256};

    use super::*;
    use crate::{
        release::{ReleaseVerifier, Signed, Sigstore},
        sys::{Clock, CmdOut, Handover, HttpError, Response, Runner, Transport},
        testdir::TestDir,
    };

    const GITHUB: &str = "https://github.com/upsetdev/pyo/releases/download";
    const PYO: &str = "https://pyo.example/install/version";
    const NOW: i64 = 2_000_000_000;
    const DAY: i64 = 86_400;
    /// A real release: its release.json and the bundle the release workflow
    /// made for it.
    const REAL_MANIFEST: &[u8] =
        include_bytes!("../../pyo-sigstore/assets/testdata/release-0.5.4/release.json");
    const REAL_BUNDLE: &[u8] = include_bytes!(
        "../../pyo-sigstore/assets/testdata/release-0.5.4/release.json.sigstore.json"
    );
    /// When Rekor logged it.
    const REAL_LOGGED: i64 = 1_795_294_220;

    /// Stands in for Sigstore where a test needs a release nobody signed.
    /// It only exists in tests: the binary's verifier is `Sigstore`, built
    /// in main.rs, and cannot be exchanged.
    struct TestSigner;

    fn test_bundle(tag: &str, manifest: &[u8], logged_at: i64) -> Vec<u8> {
        serde_json::json!({
            "tag": tag,
            "sha256": hex::encode(Sha256::digest(manifest)),
            "loggedAt": logged_at,
        })
        .to_string()
        .into_bytes()
    }

    impl ReleaseVerifier for TestSigner {
        fn verify(
            &self,
            manifest: &[u8],
            bundle: &[u8],
            version: &Version,
        ) -> Result<Signed, Problem> {
            let refused = || {
                Problem::new(format!(
                    "Release {version} is not signed by Pyo's release workflow. Nothing was changed."
                ))
            };
            let b: serde_json::Value = serde_json::from_slice(bundle).map_err(|_| refused())?;
            if b["tag"] != format!("v{version}")
                || b["sha256"] != hex::encode(Sha256::digest(manifest))
            {
                return Err(refused());
            }
            Ok(Signed {
                logged_at: b["loggedAt"].as_i64().ok_or_else(refused)?,
            })
        }
    }

    /// A status, where it redirects to, and a body.
    type Answer = (u16, Option<String>, Vec<u8>);

    #[derive(Default)]
    struct FakeHttp {
        routes: RefCell<HashMap<String, Answer>>,
        calls: RefCell<Vec<String>>,
    }

    impl Transport for FakeHttp {
        fn get(&self, url: &str, max: usize) -> Result<Response, HttpError> {
            self.calls.borrow_mut().push(url.to_owned());
            match self.routes.borrow().get(url) {
                Some((_, _, body)) if body.len() > max => Err(HttpError::TooLarge),
                Some((status, redirect, body)) => Ok(Response {
                    status: *status,
                    redirect: redirect.clone(),
                    body: body.clone(),
                }),
                None => Ok(Response {
                    status: 404,
                    redirect: None,
                    body: b"Not Found".to_vec(),
                }),
            }
        }
    }

    struct FakeRunner {
        k3s_up: Cell<bool>,
        /// `updates` in pyo-policy.
        policy: RefCell<String>,
        /// What a downloaded verifier answers to `version`.
        new_says: RefCell<String>,
        applied: RefCell<Vec<String>>,
        calls: RefCell<Vec<Vec<String>>>,
    }

    impl Runner for FakeRunner {
        fn run(&self, argv: &[&str], stdin: Option<&[u8]>) -> std::io::Result<CmdOut> {
            self.calls
                .borrow_mut()
                .push(argv.iter().map(|a| a.to_string()).collect());
            let out = |ok: bool, s: &str| {
                Ok(CmdOut {
                    ok,
                    stdout: s.as_bytes().to_vec(),
                })
            };
            if argv[0] == "k3s" {
                if !self.k3s_up.get() {
                    return out(false, "");
                }
                if argv.contains(&"get") {
                    return out(true, &self.policy.borrow());
                }
                if argv.contains(&"apply") {
                    self.applied
                        .borrow_mut()
                        .push(String::from_utf8(stdin.unwrap().to_vec()).unwrap());
                }
                return out(true, "");
            }
            if argv.len() == 2 && argv[1] == "version" {
                assert!(Path::new(argv[0]).exists(), "the staged binary is there");
                return out(true, &format!("{}\n", self.new_says.borrow()));
            }
            out(true, "")
        }
    }

    struct FakeClock(Cell<i64>);

    impl Clock for FakeClock {
        fn now(&self) -> i64 {
            self.0.get()
        }
    }

    #[derive(Default)]
    struct FakeExec {
        fails: Cell<bool>,
        calls: RefCell<Vec<(PathBuf, Vec<String>)>>,
    }

    impl Handover for FakeExec {
        fn exec(&self, program: &Path, args: &[&str]) -> std::io::Result<()> {
            self.calls.borrow_mut().push((
                program.to_owned(),
                args.iter().map(|a| a.to_string()).collect(),
            ));
            if self.fails.get() {
                return Err(std::io::Error::other("exec format error"));
            }
            Ok(())
        }
    }

    /// A server: its files in a temporary directory, and fakes for the rest.
    struct World {
        dir: TestDir,
        http: FakeHttp,
        runner: FakeRunner,
        clock: FakeClock,
        exec: FakeExec,
        own: Version,
        /// Check signatures for real (Sigstore) instead of `TestSigner`.
        real: bool,
        _quiet: crate::testdir::Quiet,
    }

    fn v(s: &str) -> Version {
        Version::parse(s).unwrap()
    }

    impl World {
        /// A server whose verifier is version `own`, set up for
        /// https://pyo.example.
        fn new(own: &str) -> World {
            let w = World {
                dir: TestDir::new(),
                http: FakeHttp::default(),
                runner: FakeRunner {
                    k3s_up: Cell::new(true),
                    policy: RefCell::new(String::new()),
                    new_says: RefCell::new(String::new()),
                    applied: RefCell::default(),
                    calls: RefCell::default(),
                },
                clock: FakeClock(Cell::new(NOW)),
                exec: FakeExec::default(),
                own: v(own),
                real: false,
                _quiet: crate::testdir::Quiet::new(),
            };
            w.config("url=https://pyo.example\n");
            w.write("usr/local/bin/pyo-verify", b"old binary");
            w.write("proc/sys/kernel/hostname", b"web-1\n");
            w
        }

        fn write(&self, rel: &str, bytes: &[u8]) {
            let p = self.dir.path().join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, bytes).unwrap();
        }

        fn read(&self, rel: &str) -> Option<Vec<u8>> {
            std::fs::read(self.dir.path().join(rel)).ok()
        }

        fn config(&self, text: &str) {
            self.write("etc/pyo/verify.conf", text.as_bytes());
        }

        fn exe(&self) -> PathBuf {
            self.dir.path().join("usr/local/bin/pyo-verify")
        }

        fn env(&self) -> Env<'_> {
            Env {
                root: self.dir.path().to_owned(),
                runner: &self.runner,
                http: &self.http,
                clock: &self.clock,
                handover: &self.exec,
                verifier: if self.real { &Sigstore } else { &TestSigner },
                own: self.own,
                arch: "x86_64",
                exe: self.exe(),
            }
        }

        fn route(&self, url: &str, body: &[u8]) {
            self.http
                .routes
                .borrow_mut()
                .insert(url.to_owned(), (200, None, body.to_vec()));
        }

        fn redirect(&self, url: &str, to: &str) {
            self.http
                .routes
                .borrow_mut()
                .insert(url.to_owned(), (302, Some(to.to_owned()), Vec::new()));
        }

        /// The control plane names this version.
        fn live(&self, version: &str) {
            self.route(
                PYO,
                format!(r#"{{"version":"{version}","install":"abc123"}}"#).as_bytes(),
            );
        }

        /// A release on GitHub, signed (by `TestSigner`) a week ago, with a
        /// verifier binary of these bytes.
        fn release(&self, version: &str, binary: Option<&[u8]>) {
            self.release_logged(version, binary, NOW - 7 * DAY);
        }

        fn release_logged(&self, version: &str, binary: Option<&[u8]>, logged_at: i64) {
            let manifest = match binary {
                Some(b) => {
                    self.route(&format!("{GITHUB}/v{version}/pyo-verify-linux-x86_64"), b);
                    *self.runner.new_says.borrow_mut() = version.to_owned();
                    serde_json::json!({ "version": version, "verifier": {
                        "linux-x86_64": hex::encode(Sha256::digest(b)),
                        "linux-aarch64": "0".repeat(64),
                    }})
                }
                None => serde_json::json!({ "version": version }),
            }
            .to_string();
            self.signed_manifest(version, manifest.as_bytes(), logged_at);
        }

        fn signed_manifest(&self, version: &str, manifest: &[u8], logged_at: i64) {
            self.route(&format!("{GITHUB}/v{version}/release.json"), manifest);
            self.route(
                &format!("{GITHUB}/v{version}/release.json.sigstore.json"),
                &test_bundle(&format!("v{version}"), manifest, logged_at),
            );
        }

        /// The real release 0.5.4, served as release `at`.
        fn real_release(&self, at: &str, manifest: &[u8]) {
            self.route(&format!("{GITHUB}/v{at}/release.json"), manifest);
            self.route(
                &format!("{GITHUB}/v{at}/release.json.sigstore.json"),
                REAL_BUNDLE,
            );
        }

        fn run(&self) -> Report {
            run(&self.env(), None)
        }

        fn status(&self) -> serde_json::Value {
            serde_json::from_slice(
                &self
                    .read("var/lib/pyo-verify/status.json")
                    .expect("a status file"),
            )
            .unwrap()
        }

        fn asked_github(&self) -> bool {
            self.http
                .calls
                .borrow()
                .iter()
                .any(|u| u.contains("github"))
        }

        /// Nothing on the server moved: the binary, no hand-over.
        fn assert_unchanged(&self) {
            assert_eq!(
                self.read("usr/local/bin/pyo-verify").unwrap(),
                b"old binary"
            );
            assert!(self.read("usr/local/bin/pyo-verify.new").is_none());
            assert!(self.exec.calls.borrow().is_empty());
        }
    }

    #[test]
    fn the_live_version_being_this_one_changes_nothing() {
        let w = World::new("0.51.0");
        w.live("0.51.0");
        let r = w.run();
        assert_eq!(r.outcome, Outcome::Current, "{}", r.message);
        assert_eq!(r.message, "This server is on version 0.51.0.");
        assert!(!w.asked_github());
        w.assert_unchanged();
        let s = w.status();
        assert_eq!(s["state"], "current");
        assert_eq!(s["verifierVersion"], "0.51.0");
        assert_eq!(s["liveVersion"], "0.51.0");
        assert_eq!(s["lastError"], "");
        assert_eq!(s["host"], "web-1");
        assert_eq!(s["hold"], false);
    }

    #[test]
    fn a_lower_version_is_refused_and_reported() {
        let w = World::new("0.51.0");
        w.live("0.50.9");
        w.release("0.50.9", Some(b"older"));
        let r = w.run();
        assert_eq!(r.outcome, Outcome::Failed);
        assert_eq!(
            r.message,
            "Pyo names version 0.50.9 and this server is already on 0.51.0. Nothing was changed."
        );
        assert!(!w.asked_github());
        w.assert_unchanged();
        assert_eq!(w.status()["state"], "error");
        assert_eq!(w.status()["lastError"], r.message);
    }

    #[test]
    fn the_recorded_floor_holds_under_an_older_binary() {
        // An older verifier was put back on a server that had moved on.
        let w = World::new("0.51.0");
        w.write("var/lib/pyo-verify/state", b"floor=0.53.0\n");
        w.live("0.52.0");
        w.release("0.52.0", Some(b"new"));
        let r = w.run();
        assert_eq!(r.outcome, Outcome::Failed);
        assert!(r.message.contains("already on 0.53.0"), "{}", r.message);
        w.assert_unchanged();
        w.live("0.53.0");
        assert_eq!(w.run().outcome, Outcome::Current);
        // Above the floor it goes on.
        w.live("0.54.0");
        w.release("0.54.0", Some(b"newer"));
        assert_eq!(w.run().outcome, Outcome::HandedOver);
    }

    #[test]
    fn a_state_file_it_cannot_read_stops_it() {
        let w = World::new("0.51.0");
        w.write("var/lib/pyo-verify/state", b"floor=lots\n");
        w.live("0.52.0");
        w.release("0.52.0", Some(b"new"));
        assert_eq!(w.run().outcome, Outcome::Failed);
        w.assert_unchanged();
    }

    #[test]
    fn an_answer_that_is_not_a_version_is_refused() {
        for answer in [
            r#"{"version":"v0.52.0"}"#,
            r#"{"version":"0.52"}"#,
            r#"{"version":"latest"}"#,
            r#"{"version":"0.52.0\n"}"#,
            r#"{"version":"0.52.0 --insecure"}"#,
            r#"{"version":"../../0.52.0"}"#,
            r#"{"version":52}"#,
            r#"{"version":null}"#,
            r#"{"install":"abc"}"#,
            r#"["0.52.0"]"#,
            r#""0.52.0""#,
            "0.52.0",
            "",
            "<html>502 Bad Gateway</html>",
            r#"{"version":"0.52.0"} {"version":"0.53.0"}"#,
        ] {
            let w = World::new("0.51.0");
            w.route(PYO, answer.as_bytes());
            w.release("0.52.0", Some(b"new"));
            let r = w.run();
            assert_eq!(r.outcome, Outcome::Failed, "{answer}");
            assert_eq!(
                r.message,
                "Pyo's answer about its live version could not be used. Nothing was changed.",
                "{answer}"
            );
            assert!(!w.asked_github(), "{answer}");
            w.assert_unchanged();
            assert_eq!(w.status()["liveVersion"], "", "{answer}");
        }
        // More fields than it knows are fine: the route may grow.
        let w = World::new("0.51.0");
        w.route(
            PYO,
            br#"{"version":"0.51.0","install":"abc","more":{"x":1}}"#,
        );
        assert_eq!(w.run().outcome, Outcome::Current);
    }

    #[test]
    fn rc_versions_need_the_rc_channel() {
        let w = World::new("0.51.0");
        w.live("0.52.0-rc.1");
        w.release("0.52.0-rc.1", Some(b"rc binary"));
        let r = w.run();
        assert_eq!(r.outcome, Outcome::Failed);
        assert!(
            r.message.contains("only takes stable versions"),
            "{}",
            r.message
        );
        assert!(!w.asked_github());
        w.assert_unchanged();

        w.config("url=https://pyo.example\nchannel=rc\n");
        let r = w.run();
        assert_eq!(r.outcome, Outcome::HandedOver, "{}", r.message);
        assert_eq!(w.read("usr/local/bin/pyo-verify").unwrap(), b"rc binary");

        // An rc is older than its release, and newer than the one before.
        let w = World::new("0.52.0");
        w.config("url=https://pyo.example\nchannel=rc\n");
        w.live("0.52.0-rc.2");
        assert!(w.run().message.contains("already on 0.52.0"));
    }

    #[test]
    fn hold_in_the_config_stops_everything_but_the_status() {
        let w = World::new("0.51.0");
        w.config("url=https://pyo.example\nhold=true\n");
        w.live("0.52.0");
        w.release("0.52.0", Some(b"new"));
        let r = w.run();
        assert_eq!(r.outcome, Outcome::Hold);
        assert_eq!(r.message, "Updates are on hold in /etc/pyo/verify.conf.");
        assert!(w.http.calls.borrow().is_empty(), "nobody was asked");
        w.assert_unchanged();
        let s = w.status();
        assert_eq!(s["state"], "hold");
        assert_eq!(s["hold"], true);
        assert_eq!(s["lastError"], "");
        // The status also went to the cluster.
        let applied = w.runner.applied.borrow();
        let cm: serde_json::Value = serde_json::from_str(&applied[0]).unwrap();
        assert_eq!(cm["data"]["hold"], "true");
        // And `status` still answers.
        assert!(status::show(&w.env()).contains("State hold"));
    }

    #[test]
    fn hold_in_the_cluster_stops_it() {
        for value in ["hold", "Hold\n", " HOLD "] {
            let w = World::new("0.51.0");
            *w.runner.policy.borrow_mut() = value.into();
            w.live("0.52.0");
            w.release("0.52.0", Some(b"new"));
            let r = w.run();
            assert_eq!(r.outcome, Outcome::Hold, "{value}");
            assert_eq!(
                r.message,
                "Updates are on hold in this cluster's pyo-policy settings."
            );
            assert!(w.http.calls.borrow().is_empty(), "{value}");
            w.assert_unchanged();
            assert_eq!(w.status()["hold"], true);
        }
        // Empty or `auto`: it goes on.
        for value in ["", "auto", " Auto\n"] {
            let w = World::new("0.51.0");
            *w.runner.policy.borrow_mut() = value.into();
            w.live("0.52.0");
            w.release("0.52.0", Some(b"new"));
            assert_eq!(w.run().outcome, Outcome::HandedOver, "{value:?}");
            assert_eq!(w.status()["hold"], false, "{value:?}");
        }
        // The switch is read, never written.
        let w = World::new("0.51.0");
        w.live("0.51.0");
        w.run();
        let calls = w.runner.calls.borrow();
        let get = calls
            .iter()
            .find(|c| c.contains(&"pyo-policy".to_owned()))
            .unwrap();
        assert_eq!(
            get.join(" "),
            "k3s kubectl --request-timeout=15s -n pyo-system get configmap pyo-policy --ignore-not-found -o jsonpath={.data.updates}"
        );
    }

    #[test]
    fn a_hold_switch_it_cannot_read_stops_it() {
        // A value it does not know, a word Pyo doesn't show as on hold
        // among them: neither hold nor go.
        for value in ["sometimes", "paused", "off", "false", "on", "true", "holding"] {
            let w = World::new("0.51.0");
            *w.runner.policy.borrow_mut() = value.into();
            w.live("0.52.0");
            w.release("0.52.0", Some(b"new"));
            let r = w.run();
            assert_eq!(r.outcome, Outcome::Failed, "{value}");
            assert_eq!(
                r.message,
                "The updates setting in this cluster's pyo-policy is not one the verifier knows. Nothing was changed.",
                "{value}"
            );
            assert!(w.http.calls.borrow().is_empty(), "{value}");
            w.assert_unchanged();
        }

        // k3s is not answering: nothing happens, and the status is the file.
        let w = World::new("0.51.0");
        w.runner.k3s_up.set(false);
        w.live("0.52.0");
        w.release("0.52.0", Some(b"new"));
        let r = w.run();
        assert_eq!(r.outcome, Outcome::Failed);
        assert_eq!(
            r.message,
            "The verifier could not read this cluster's pyo-policy settings. Nothing was changed."
        );
        assert!(w.http.calls.borrow().is_empty());
        w.assert_unchanged();
        assert_eq!(w.status()["lastError"], r.message);
        assert!(w.runner.applied.borrow().is_empty());
    }

    #[test]
    fn a_newer_signed_release_replaces_the_verifier_and_hands_over() {
        let w = World::new("0.51.0");
        w.live("0.52.0");
        w.release("0.52.0", Some(b"the 0.52.0 verifier"));
        let r = w.run();
        assert_eq!(
            r.outcome,
            Outcome::HandedOver,
            "{} {:?}",
            r.message,
            r.detail
        );
        assert_eq!(r.message, "The verifier is updating itself to 0.52.0.");
        // The new binary is in place, the one it replaced is kept.
        assert_eq!(
            w.read("usr/local/bin/pyo-verify").unwrap(),
            b"the 0.52.0 verifier"
        );
        assert_eq!(
            w.read("usr/local/bin/pyo-verify.previous").unwrap(),
            b"old binary"
        );
        assert!(w.read("usr/local/bin/pyo-verify.new").is_none());
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(w.exe()).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o755);
        // Started once, in this one's place, told where it came from.
        let calls = w.exec.calls.borrow();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, w.exe());
        assert_eq!(calls[0].1, ["run", "--handed-over-from", "0.51.0"]);
        let s = w.status();
        assert_eq!(s["state"], "updating");
        assert_eq!(s["liveVersion"], "0.52.0");
        // The floor is the new verifier's to record.
        assert!(w.read("var/lib/pyo-verify/state").is_none());
    }

    #[test]
    fn a_handed_over_run_records_the_floor() {
        let w = World::new("0.52.0");
        w.live("0.52.0");
        let r = run(&w.env(), Some("0.51.0"));
        assert_eq!(r.outcome, Outcome::Current, "{}", r.message);
        assert_eq!(
            w.read("var/lib/pyo-verify/state").unwrap(),
            b"floor=0.52.0\n"
        );
        assert_eq!(w.status()["verifierVersion"], "0.52.0");
        assert_eq!(w.status()["state"], "current");
        w.assert_unchanged();
        // Also when Pyo cannot be asked right then.
        let w = World::new("0.52.0");
        let r = run(&w.env(), Some("0.51.0"));
        assert_eq!(r.outcome, Outcome::Failed);
        assert_eq!(
            w.read("var/lib/pyo-verify/state").unwrap(),
            b"floor=0.52.0\n"
        );
    }

    #[test]
    fn there_is_no_exec_loop() {
        // A verifier never takes a hand-over from its own version, a newer
        // one, or something that is not a version.
        for from in ["0.52.0", "0.53.0", "", "0.52", "--help"] {
            let w = World::new("0.52.0");
            w.live("0.53.0");
            w.release("0.53.0", Some(b"new"));
            let r = run(&w.env(), Some(from));
            assert_eq!(r.outcome, Outcome::Failed, "{from:?}");
            assert!(w.http.calls.borrow().is_empty(), "{from:?}");
            assert!(w.read("var/lib/pyo-verify/state").is_none());
            w.assert_unchanged();
        }
        // One hand-over per run: a handed-over verifier that finds an even
        // newer release leaves it to the next run.
        let w = World::new("0.52.0");
        w.live("0.53.0");
        w.release("0.53.0", Some(b"new"));
        let r = run(&w.env(), Some("0.51.0"));
        assert_eq!(r.outcome, Outcome::Waiting);
        assert_eq!(
            r.message,
            "Release 0.53.0 is newer than this verifier. It is taken on the next run."
        );
        w.assert_unchanged();
        assert!(
            !w.http
                .calls
                .borrow()
                .iter()
                .any(|u| u.contains("pyo-verify-linux"))
        );
        // The next run, started by the timer, takes it.
        assert_eq!(w.run().outcome, Outcome::HandedOver);
        assert_eq!(w.exec.calls.borrow().len(), 1);
    }

    #[test]
    fn a_real_signed_release_is_accepted() {
        // Sigstore for real: release 0.5.4 as the release workflow signed
        // it. It has no verifier yet, which is as far as a run can get with
        // it, past the signature, the version and the wait.
        let mut w = World::new("0.5.3");
        w.real = true;
        w.clock.0.set(REAL_LOGGED + 2 * DAY);
        w.live("0.5.4");
        w.real_release("0.5.4", REAL_MANIFEST);
        let r = w.run();
        assert_eq!(
            r.outcome,
            Outcome::NoVerifier,
            "{} {:?}",
            r.message,
            r.detail
        );
        assert_eq!(
            r.message,
            "Release 0.5.4 has no verifier. Run the connect command from Pyo to update this server."
        );
        assert_eq!(w.status()["state"], "error");
        assert_eq!(w.status()["lastError"], r.message);
        w.assert_unchanged();
        // Its wait counts from the time Rekor signed.
        w.clock.0.set(REAL_LOGGED + DAY - 1);
        assert_eq!(w.run().outcome, Outcome::Waiting);
    }

    #[test]
    fn a_release_signed_at_another_tag_is_refused() {
        // The real 0.5.4, offered as other versions: its signature names
        // the tag v0.5.4 and no other.
        for (own, live, channel) in [
            ("0.5.3", "0.5.5", "stable"),
            ("0.5.3", "0.5.40", "stable"),
            ("0.5.3", "0.5.4-rc.1", "rc"),
            ("0.4.0", "0.5.0", "stable"),
        ] {
            let mut w = World::new(own);
            w.real = true;
            w.config(&format!("url=https://pyo.example\nchannel={channel}\n"));
            w.clock.0.set(REAL_LOGGED + 2 * DAY);
            w.live(live);
            w.real_release(live, REAL_MANIFEST);
            let r = w.run();
            assert_eq!(r.outcome, Outcome::Failed, "{live}");
            assert_eq!(
                r.message,
                format!(
                    "Release {live} is not signed by Pyo's release workflow. Nothing was changed."
                )
            );
            assert!(r.detail.unwrap().contains("refs/tags/v0.5.4"), "{live}");
            w.assert_unchanged();
        }
    }

    #[test]
    fn a_tampered_release_is_refused() {
        // Against Sigstore: one changed byte in the real release.json.
        let mut w = World::new("0.5.3");
        w.real = true;
        w.clock.0.set(REAL_LOGGED + 2 * DAY);
        w.live("0.5.4");
        let mut manifest = REAL_MANIFEST.to_vec();
        let at = manifest.windows(6).position(|w| w == b"sha256").unwrap();
        manifest[at + 10] ^= 1;
        w.real_release("0.5.4", &manifest);
        let r = w.run();
        assert_eq!(r.outcome, Outcome::Failed);
        assert!(
            r.message
                .contains("is not signed by Pyo's release workflow"),
            "{}",
            r.message
        );
        w.assert_unchanged();

        // A signature that is no bundle, or missing.
        for bundle in [&b"{}"[..], b"", b"not json"] {
            let mut w = World::new("0.5.3");
            w.real = true;
            w.live("0.5.4");
            w.route(&format!("{GITHUB}/v0.5.4/release.json"), REAL_MANIFEST);
            w.route(
                &format!("{GITHUB}/v0.5.4/release.json.sigstore.json"),
                bundle,
            );
            assert_eq!(w.run().outcome, Outcome::Failed);
            w.assert_unchanged();
        }
        let mut w = World::new("0.5.3");
        w.real = true;
        w.live("0.5.4");
        w.route(&format!("{GITHUB}/v0.5.4/release.json"), REAL_MANIFEST);
        assert_eq!(w.run().outcome, Outcome::Failed);

        // And a release whose verifier checksum was swapped after signing.
        let w = World::new("0.51.0");
        w.live("0.52.0");
        w.release("0.52.0", Some(b"new"));
        let evil = serde_json::json!({ "version": "0.52.0", "verifier": {
            "linux-x86_64": hex::encode(Sha256::digest(b"evil")) } })
        .to_string();
        w.route(&format!("{GITHUB}/v0.52.0/release.json"), evil.as_bytes());
        w.route(
            &format!("{GITHUB}/v0.52.0/pyo-verify-linux-x86_64"),
            b"evil",
        );
        assert_eq!(w.run().outcome, Outcome::Failed);
        w.assert_unchanged();
    }

    #[test]
    fn a_release_that_names_another_version_is_refused() {
        let w = World::new("0.51.0");
        w.live("0.52.0");
        w.route(&format!("{GITHUB}/v0.52.0/pyo-verify-linux-x86_64"), b"new");
        let manifest = serde_json::json!({ "version": "0.53.0", "verifier": {
            "linux-x86_64": hex::encode(Sha256::digest(b"new")) } })
        .to_string();
        w.signed_manifest("0.52.0", manifest.as_bytes(), NOW - 7 * DAY);
        let r = w.run();
        assert_eq!(r.outcome, Outcome::Failed);
        assert_eq!(
            r.message,
            "The signed release names another version than 0.52.0. Nothing was changed."
        );
        w.assert_unchanged();
    }

    #[test]
    fn a_young_release_waits() {
        let w = World::new("0.51.0");
        w.live("0.52.0");
        w.release_logged("0.52.0", Some(b"new"), NOW - 23 * 3600);
        let r = w.run();
        assert_eq!(r.outcome, Outcome::Waiting);
        assert_eq!(
            r.message,
            "Release 0.52.0 was signed 23 hours ago. This server takes it once it is 24 hours old."
        );
        w.assert_unchanged();
        assert_eq!(w.status()["state"], "waiting");
        assert_eq!(w.status()["lastError"], "");
        assert!(
            !w.http
                .calls
                .borrow()
                .iter()
                .any(|u| u.contains("pyo-verify-linux"))
        );
        // An hour later it is old enough.
        w.clock.0.set(NOW + 3600);
        assert_eq!(w.run().outcome, Outcome::HandedOver);

        // Signed after this server's clock: the clock is off.
        let w = World::new("0.51.0");
        w.live("0.52.0");
        w.release_logged("0.52.0", Some(b"new"), NOW + 600);
        let r = w.run();
        assert_eq!(r.outcome, Outcome::Waiting);
        assert!(
            r.message.contains("Check the clock of this server"),
            "{}",
            r.message
        );
        w.assert_unchanged();

        // Test servers may shorten the wait.
        let w = World::new("0.51.0");
        w.config("url=https://pyo.example\nchannel=rc\nsoak_hours=1\n");
        w.live("0.52.0-rc.1");
        w.release_logged("0.52.0-rc.1", Some(b"new"), NOW - 1800);
        assert_eq!(w.run().outcome, Outcome::Waiting);
        w.release_logged("0.52.0-rc.1", Some(b"new"), NOW - 3600);
        assert_eq!(w.run().outcome, Outcome::HandedOver);
    }

    #[test]
    fn a_release_without_a_verifier_stops_cleanly() {
        let w = World::new("0.51.0");
        w.live("0.52.0");
        w.release("0.52.0", None);
        let r = w.run();
        assert_eq!(r.outcome, Outcome::NoVerifier);
        assert_eq!(r.outcome.exit_code(), 1);
        w.assert_unchanged();
        assert!(
            !w.http
                .calls
                .borrow()
                .iter()
                .any(|u| u.contains("pyo-verify-linux"))
        );
        assert!(w.read("var/lib/pyo-verify/state").is_none());
        assert_eq!(w.status()["lastError"], r.message);
    }

    #[test]
    fn a_bad_checksum_is_refused_and_the_binary_kept() {
        let w = World::new("0.51.0");
        w.live("0.52.0");
        w.release("0.52.0", Some(b"the signed verifier"));
        w.route(
            &format!("{GITHUB}/v0.52.0/pyo-verify-linux-x86_64"),
            b"another binary",
        );
        let r = w.run();
        assert_eq!(r.outcome, Outcome::Failed);
        assert_eq!(
            r.message,
            "The verifier of release 0.52.0 does not match its signed checksum. Nothing was changed."
        );
        w.assert_unchanged();
        assert!(w.read("usr/local/bin/pyo-verify.previous").is_none());
        // It was never run.
        assert!(
            !w.runner
                .calls
                .borrow()
                .iter()
                .any(|c| c.get(1).is_some_and(|a| a == "version"))
        );
        assert_eq!(w.status()["lastError"], r.message);
    }

    #[test]
    fn a_verifier_that_does_not_run_is_not_put_in_place() {
        let w = World::new("0.51.0");
        w.live("0.52.0");
        w.release("0.52.0", Some(b"new"));
        *w.runner.new_says.borrow_mut() = "0.51.0".into();
        let r = w.run();
        assert_eq!(r.outcome, Outcome::Failed);
        assert!(
            r.message.contains("does not run on this server"),
            "{}",
            r.message
        );
        w.assert_unchanged();
    }

    #[test]
    fn a_verifier_that_does_not_start_is_rolled_back() {
        let w = World::new("0.51.0");
        w.live("0.52.0");
        w.release("0.52.0", Some(b"new"));
        w.exec.fails.set(true);
        let r = w.run();
        assert_eq!(r.outcome, Outcome::Failed);
        assert_eq!(
            r.message,
            "The verifier of release 0.52.0 could not be started. This server keeps verifier 0.51.0."
        );
        assert_eq!(w.read("usr/local/bin/pyo-verify").unwrap(), b"old binary");
        assert_eq!(w.status()["state"], "error");
        assert_eq!(w.status()["lastError"], r.message);
    }

    #[test]
    fn the_previous_binary_survives_each_replace() {
        let w = World::new("0.51.0");
        w.write("usr/local/bin/pyo-verify.previous", b"an even older one");
        w.write("usr/local/bin/pyo-verify.new", b"left over from a crash");
        w.live("0.52.0");
        w.release("0.52.0", Some(b"new"));
        assert_eq!(w.run().outcome, Outcome::HandedOver);
        assert_eq!(w.read("usr/local/bin/pyo-verify").unwrap(), b"new");
        assert_eq!(
            w.read("usr/local/bin/pyo-verify.previous").unwrap(),
            b"old binary"
        );
        // Replaced by a rename: the old binary's file was never written to.
        let w = World::new("0.51.0");
        use std::os::unix::fs::MetadataExt;
        let before = std::fs::metadata(w.exe()).unwrap().ino();
        w.live("0.52.0");
        w.release("0.52.0", Some(b"new"));
        w.run();
        assert_ne!(std::fs::metadata(w.exe()).unwrap().ino(), before);
        let kept = w.dir.path().join("usr/local/bin/pyo-verify.previous");
        assert_eq!(std::fs::metadata(kept).unwrap().ino(), before);
    }

    #[test]
    fn the_status_is_written_on_every_outcome() {
        type Setup = fn(&World);
        let cases: [(&str, Setup, Outcome, &str); 7] = [
            ("current", |w| w.live("0.51.0"), Outcome::Current, "current"),
            (
                "hold",
                |w| w.config("url=https://pyo.example\nhold=yes\n"),
                Outcome::Hold,
                "hold",
            ),
            ("pyo is down", |_| {}, Outcome::Failed, "error"),
            (
                "no config",
                |w| std::fs::remove_file(w.dir.path().join("etc/pyo/verify.conf")).unwrap(),
                Outcome::Failed,
                "error",
            ),
            (
                "waiting",
                |w| {
                    w.live("0.52.0");
                    w.release_logged("0.52.0", Some(b"new"), NOW - 60);
                },
                Outcome::Waiting,
                "waiting",
            ),
            (
                "updating",
                |w| {
                    w.live("0.52.0");
                    w.release("0.52.0", Some(b"new"));
                },
                Outcome::HandedOver,
                "updating",
            ),
            ("no release", |w| w.live("0.52.0"), Outcome::Failed, "error"),
        ];
        for (name, setup, outcome, state) in cases {
            let w = World::new("0.51.0");
            setup(&w);
            let r = w.run();
            assert_eq!(r.outcome, outcome, "{name}: {}", r.message);
            let s = w.status();
            assert_eq!(s["state"], state, "{name}");
            assert_eq!(s["message"], r.message, "{name}");
            assert_eq!(s["lastCheck"], "2033-05-18T03:33:20Z", "{name}");
            assert_eq!(s["verifierVersion"], "0.51.0", "{name}");
            assert_eq!(s["host"], "web-1", "{name}");
            // One line, in plain words.
            assert!(
                !r.message.contains(['\n', ';', '—', '–']),
                "{name}: {}",
                r.message
            );
            assert!(!r.message.contains(" - "), "{name}: {}", r.message);
            assert!(!r.message.to_lowercase().contains("node"), "{name}");
            // The same in the cluster.
            let applied = w.runner.applied.borrow();
            let cm: serde_json::Value = serde_json::from_str(applied.last().unwrap()).unwrap();
            assert_eq!(cm["metadata"]["name"], "pyo-verifier", "{name}");
            assert_eq!(cm["data"]["state"], state, "{name}");
            assert_eq!(cm["data"]["message"], r.message, "{name}");
        }
    }

    #[test]
    fn downloads_have_a_size_limit() {
        let large = |n: usize| vec![b' '; n];
        // The control plane's answer.
        let w = World::new("0.51.0");
        let mut answer = br#"{"version":"0.52.0"}"#.to_vec();
        answer.extend(large(MAX_ANSWER));
        w.route(PYO, &answer);
        let r = w.run();
        assert_eq!(r.outcome, Outcome::Failed);
        assert_eq!(
            r.message,
            "The answer from pyo.example is larger than expected. Nothing was changed."
        );
        // release.json, its signature and the binary.
        for file in [
            "release.json",
            "release.json.sigstore.json",
            "pyo-verify-linux-x86_64",
        ] {
            let w = World::new("0.51.0");
            w.live("0.52.0");
            w.release("0.52.0", Some(b"new"));
            let max = match file {
                "release.json" => MAX_MANIFEST,
                "release.json.sigstore.json" => MAX_BUNDLE,
                _ => fetch::MAX_BINARY,
            };
            w.route(&format!("{GITHUB}/v0.52.0/{file}"), &large(max + 1));
            let r = w.run();
            assert_eq!(r.outcome, Outcome::Failed, "{file}");
            assert_eq!(
                r.message,
                "The answer from github.com is larger than expected. Nothing was changed.",
                "{file}"
            );
            w.assert_unchanged();
        }
        // A transport that hands back more than it was asked for is not
        // believed either.
        struct Sloppy;
        impl Transport for Sloppy {
            fn get(&self, _: &str, max: usize) -> Result<Response, HttpError> {
                Ok(Response {
                    status: 200,
                    redirect: None,
                    body: vec![b'x'; max + 1],
                })
            }
        }
        let w = World::new("0.51.0");
        let env = Env {
            http: &Sloppy,
            ..w.env()
        };
        assert!(fetch::get(&env, "https://a.example/x", 10, &[]).is_err());
        // A release folder has the same limits.
        let w = World::new("0.51.0");
        let folder = w.dir.path().join("releases");
        w.config(&format!(
            "url=https://pyo.example\nsource={}\n",
            folder.display()
        ));
        w.live("0.52.0");
        w.write("releases/v0.52.0/release.json", &large(MAX_MANIFEST + 1));
        assert_eq!(w.run().outcome, Outcome::Failed);
        w.assert_unchanged();
    }

    #[test]
    fn a_held_lock_means_skip() {
        let w = World::new("0.51.0");
        w.live("0.52.0");
        w.release("0.52.0", Some(b"new"));
        // A connect command is running.
        w.write("run/pyo-connect.lock", b"");
        let held = std::fs::File::open(w.dir.path().join("run/pyo-connect.lock")).unwrap();
        held.lock().unwrap();
        let r = w.run();
        assert_eq!(r.outcome, Outcome::Skipped);
        assert_eq!(r.outcome.exit_code(), 0);
        assert!(w.http.calls.borrow().is_empty());
        assert!(w.runner.calls.borrow().is_empty());
        assert!(w.read("var/lib/pyo-verify/status.json").is_none());
        w.assert_unchanged();
        // Once it is done, the next run goes on.
        drop(held);
        assert_eq!(w.run().outcome, Outcome::HandedOver);
    }

    #[test]
    fn redirects_only_go_to_githubs_own_hosts() {
        let asset = format!("{GITHUB}/v0.52.0/pyo-verify-linux-x86_64");
        // GitHub sends release files to its asset host.
        let w = World::new("0.51.0");
        w.live("0.52.0");
        w.release("0.52.0", Some(b"new"));
        let cdn = "https://release-assets.githubusercontent.com/github-production-release-asset/1/2?sig=3";
        w.redirect(&asset, cdn);
        w.route(cdn, b"new");
        assert_eq!(w.run().outcome, Outcome::HandedOver);

        // Anywhere else, plain HTTP, or in circles: refused.
        for to in [
            "https://evil.example/pyo-verify-linux-x86_64",
            "https://github.com.evil.example/x",
            "https://github.com@evil.example/x",
            "http://release-assets.githubusercontent.com/x",
            "https://release-assets.githubusercontent.com:8443/x",
            "file:///usr/bin/true",
            asset.as_str(),
        ] {
            let w = World::new("0.51.0");
            w.live("0.52.0");
            w.release("0.52.0", Some(b"new"));
            w.redirect(&asset, to);
            w.route("https://evil.example/pyo-verify-linux-x86_64", b"new");
            let r = w.run();
            assert_eq!(r.outcome, Outcome::Failed, "{to}");
            w.assert_unchanged();
            assert!(
                !w.http.calls.borrow().iter().any(|u| u.contains("evil")),
                "{to}"
            );
        }

        // The control plane's answer comes from its own address only.
        let w = World::new("0.51.0");
        w.redirect(PYO, "https://github.com/x");
        w.route("https://github.com/x", br#"{"version":"0.52.0"}"#);
        w.release("0.52.0", Some(b"new"));
        assert_eq!(w.run().outcome, Outcome::Failed);
        w.assert_unchanged();
    }

    #[test]
    fn release_json_comes_from_api_pyo_dev_when_github_fails() {
        const COPY: &str = "https://api.pyo.dev/v1/releases/download";
        let w = World::new("0.51.0");
        w.live("0.52.0");
        w.release("0.52.0", Some(b"new"));
        // GitHub is down for release.json and its signature: api.pyo.dev
        // has the same bytes.
        for file in ["release.json", "release.json.sigstore.json"] {
            let mut routes = w.http.routes.borrow_mut();
            let answer = routes.remove(&format!("{GITHUB}/v0.52.0/{file}")).unwrap();
            routes.insert(format!("{COPY}/v0.52.0/{file}"), answer);
        }
        let r = w.run();
        assert_eq!(r.outcome, Outcome::HandedOver, "{}", r.message);
        let calls = w.http.calls.borrow();
        assert!(
            calls
                .iter()
                .any(|u| u == &format!("{COPY}/v0.52.0/release.json"))
        );
        // The binary isn't copied: it comes from GitHub only.
        assert!(
            !calls
                .iter()
                .any(|u| u.starts_with(COPY) && u.contains("pyo-verify"))
        );
        drop(calls);

        // Nowhere: GitHub's reason is shown, and nothing moves.
        let w = World::new("0.51.0");
        w.live("0.52.0");
        w.route(&format!("{GITHUB}/v0.52.0/pyo-verify-linux-x86_64"), b"new");
        let r = w.run();
        assert_eq!(r.outcome, Outcome::Failed);
        assert!(r.message.contains("github.com"), "{}", r.message);
        w.assert_unchanged();
    }

    #[test]
    fn a_mirror_or_a_folder_can_stand_in_for_github() {
        // A folder (air gaps, tests). It is as untrusted as the network.
        let w = World::new("0.51.0");
        let folder = w.dir.path().join("releases");
        w.config(&format!(
            "url=https://pyo.example\nsource={}\n",
            folder.display()
        ));
        w.live("0.52.0");
        let manifest = serde_json::json!({ "version": "0.52.0", "verifier": {
            "linux-x86_64": hex::encode(Sha256::digest(b"from the folder")) } })
        .to_string();
        w.write("releases/v0.52.0/release.json", manifest.as_bytes());
        w.write(
            "releases/v0.52.0/release.json.sigstore.json",
            &test_bundle("v0.52.0", manifest.as_bytes(), NOW - 2 * DAY),
        );
        w.write(
            "releases/v0.52.0/pyo-verify-linux-x86_64",
            b"from the folder",
        );
        *w.runner.new_says.borrow_mut() = "0.52.0".into();
        let r = w.run();
        assert_eq!(
            r.outcome,
            Outcome::HandedOver,
            "{} {:?}",
            r.message,
            r.detail
        );
        assert_eq!(
            w.read("usr/local/bin/pyo-verify").unwrap(),
            b"from the folder"
        );
        assert!(!w.asked_github());
        // A release.json changed in the folder is refused like any other.
        let w2 = World::new("0.51.0");
        w2.config(&format!(
            "url=https://pyo.example\nsource={}\n",
            folder.display()
        ));
        w2.live("0.52.0");
        w.write(
            "releases/v0.52.0/release.json",
            manifest.replace("0.52.0", "0.52.1").as_bytes(),
        );
        assert_eq!(w2.run().outcome, Outcome::Failed);
        w2.assert_unchanged();

        // A mirror by URL.
        let w = World::new("0.51.0");
        w.config("url=https://pyo.example\nsource=https://mirror.example/pyo\n");
        w.live("0.52.0");
        w.route(
            "https://mirror.example/pyo/v0.52.0/release.json",
            manifest.as_bytes(),
        );
        w.route(
            "https://mirror.example/pyo/v0.52.0/release.json.sigstore.json",
            &test_bundle("v0.52.0", manifest.as_bytes(), NOW - 2 * DAY),
        );
        w.route(
            "https://mirror.example/pyo/v0.52.0/pyo-verify-linux-x86_64",
            b"from the folder",
        );
        *w.runner.new_says.borrow_mut() = "0.52.0".into();
        assert_eq!(w.run().outcome, Outcome::HandedOver);
        assert!(!w.asked_github());
    }

    #[test]
    fn it_touches_nothing_in_the_cluster_but_its_own_status() {
        let w = World::new("0.51.0");
        w.live("0.52.0");
        w.release("0.52.0", Some(b"new"));
        assert_eq!(w.run().outcome, Outcome::HandedOver);
        let new = format!("{}.new", w.exe().display());
        for call in w.runner.calls.borrow().iter() {
            let line = call.join(" ");
            let known = line.starts_with(
                "k3s kubectl --request-timeout=15s -n pyo-system get configmap pyo-policy ",
            ) || line == "k3s kubectl --request-timeout=15s apply -f -"
                || line == format!("{new} version");
            assert!(known, "{line}");
        }
        for manifest in w.runner.applied.borrow().iter() {
            let cm: serde_json::Value = serde_json::from_str(manifest).unwrap();
            assert_eq!(cm["kind"], "ConfigMap");
            assert_eq!(cm["metadata"]["namespace"], "pyo-system");
            assert_eq!(cm["metadata"]["name"], "pyo-verifier");
        }
        // Only the control plane and the release were asked, with no
        // credentials in the address.
        for url in w.http.calls.borrow().iter() {
            assert!(
                url == PYO || url.starts_with(&format!("{GITHUB}/v0.52.0/")),
                "{url}"
            );
        }
    }
}
