//! `pyo-verify install` and `uninstall`: the config, the binary in its
//! place and the systemd timer. Pyo's connect command calls these while it
//! holds the lock, so they do not take it.

use crate::{
    config, state,
    status::{self, KUBECTL_TIMEOUT},
    sys::{Env, Problem, read_limited, write_atomic},
    update,
    version::Channel,
};

pub const BINARY: &str = "/usr/local/bin/pyo-verify";
pub const SERVICE: &str = "/etc/systemd/system/pyo-verify.service";
pub const TIMER: &str = "/etc/systemd/system/pyo-verify.timer";

const SERVICE_UNIT: &str = "\
[Unit]
Description=Pyo verifier (takes signed Pyo releases for this server)
Wants=network-online.target
After=network-online.target

[Service]
Type=oneshot
ExecStart=/usr/local/bin/pyo-verify run
TimeoutStartSec=15min
NoNewPrivileges=yes
PrivateTmp=yes
";

/// About every 15 minutes, spread out so servers do not ask at once. A run
/// missed while the server was off happens after it starts.
const TIMER_UNIT: &str = "\
[Unit]
Description=Runs the Pyo verifier

[Timer]
OnCalendar=*:0/15
RandomizedDelaySec=5min
Persistent=true

[Install]
WantedBy=timers.target
";

fn cannot(what: &str, e: impl std::fmt::Display) -> Problem {
    Problem::with(
        format!(
            "The verifier could not be set up on this server ({what}). It needs to run as root."
        ),
        e.to_string(),
    )
}

fn systemctl(env: &Env, args: &[&str]) -> bool {
    let mut argv = vec!["systemctl"];
    argv.extend(args);
    env.runner.run(&argv, None).is_ok_and(|o| o.ok)
}

/// Sets the verifier up. An existing config is kept as it is. Without one,
/// `url` (the control plane's public URL) is needed.
pub fn install(env: &Env, url: Option<&str>, channel: Option<Channel>) -> Result<String, Problem> {
    let conf = env.path(config::PATH);
    if conf.exists() {
        // Kept, and it has to be one the verifier can use.
        config::load(env)?;
    } else {
        let url = url.ok_or_else(|| {
            Problem::new(
                "The verifier needs the address of Pyo. Run pyo-verify install --url <address>.",
            )
        })?;
        let unusable =
            || Problem::new("The address of Pyo has to be an https address. Nothing was changed.");
        // One line of the config, and nothing more.
        crate::fetch::origin(url).ok_or_else(unusable)?;
        let text = config::fresh(
            url.trim_end_matches('/'),
            channel.unwrap_or(Channel::Stable),
        );
        config::parse(&text).map_err(|_| unusable())?;
        write_atomic(&conf, text.as_bytes(), 0o644).map_err(|e| cannot("config", e))?;
    }

    let binary = env.path(BINARY);
    if env.exe != binary {
        let bytes =
            read_limited(&env.exe, crate::fetch::MAX_BINARY).map_err(|e| cannot("binary", e))?;
        write_atomic(&binary, &bytes, 0o755).map_err(|e| cannot("binary", e))?;
    }
    std::fs::create_dir_all(env.path(state::DIR)).map_err(|e| cannot("state", e))?;
    write_atomic(&env.path(SERVICE), SERVICE_UNIT.as_bytes(), 0o644)
        .map_err(|e| cannot("service", e))?;
    write_atomic(&env.path(TIMER), TIMER_UNIT.as_bytes(), 0o644).map_err(|e| cannot("timer", e))?;
    if !(systemctl(env, &["daemon-reload"])
        && systemctl(env, &["enable", "--now", "pyo-verify.timer"]))
    {
        return Err(cannot("timer", "systemctl failed"));
    }
    Ok("The verifier is set up on this server.".into())
}

/// Removes everything `install` and the verifier's runs left: the timer and
/// service, the config, the state, the status ConfigMap and the binary with
/// the one it kept.
pub fn uninstall(env: &Env) -> Result<String, Problem> {
    // Missing pieces are fine: it may be half there.
    systemctl(env, &["disable", "--now", "pyo-verify.timer"]);
    let binary = env.path(BINARY);
    let previous = update::previous(&binary);
    let mut left = Vec::new();
    for path in [
        env.path(TIMER),
        env.path(SERVICE),
        env.path(config::PATH),
        previous,
        binary,
    ] {
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => left.push(format!("{}: {e}", path.display())),
        }
    }
    match std::fs::remove_dir_all(env.path(state::DIR)) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => left.push(format!("{}: {e}", state::DIR)),
    }
    systemctl(env, &["daemon-reload"]);
    // A cluster that does not answer keeps the ConfigMap, which nothing
    // reads once the verifier is gone.
    let _ = env.runner.run(
        &[
            "k3s",
            "kubectl",
            KUBECTL_TIMEOUT,
            "-n",
            status::NAMESPACE,
            "delete",
            "configmap",
            status::CONFIGMAP,
            "--ignore-not-found",
        ],
        None,
    );
    if !left.is_empty() {
        return Err(cannot("removing it", left.join(", ")));
    }
    Ok("The verifier is removed from this server.".into())
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use super::*;
    use crate::{
        release::Sigstore,
        sys::{CmdOut, Curl, Exec, Runner, SystemClock},
        testdir::TestDir,
        version::Version,
    };

    #[derive(Default)]
    struct Recorder(RefCell<Vec<String>>);

    impl Runner for Recorder {
        fn run(&self, argv: &[&str], _: Option<&[u8]>) -> std::io::Result<CmdOut> {
            self.0.borrow_mut().push(argv.join(" "));
            Ok(CmdOut {
                ok: true,
                stdout: Vec::new(),
            })
        }
    }

    /// A server's files in `dir`, with this binary somewhere else on it.
    fn env<'a>(dir: &TestDir, runner: &'a Recorder) -> Env<'a> {
        let exe = dir.path().join("root/pyo-verify-download");
        std::fs::create_dir_all(exe.parent().unwrap()).unwrap();
        std::fs::write(&exe, b"this binary").unwrap();
        Env {
            root: dir.path().to_owned(),
            runner,
            // Never reached: install and uninstall make no requests.
            http: &Curl,
            clock: &SystemClock,
            handover: &Exec,
            verifier: &Sigstore,
            own: Version::own(),
            arch: "x86_64",
            exe,
        }
    }

    #[test]
    fn install_sets_up_the_config_the_binary_and_the_timer() {
        let (dir, runner) = (TestDir::new(), Recorder::default());
        let e = env(&dir, &runner);
        install(&e, Some("https://pyo.example/"), None).unwrap();
        let read = |p: &str| std::fs::read_to_string(e.path(p)).unwrap();
        let conf = config::load(&e).unwrap();
        assert_eq!(conf.url, "https://pyo.example");
        assert!(!conf.hold);
        assert_eq!(conf.channel, Channel::Stable);
        assert_eq!(read(BINARY), "this binary");
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(e.path(BINARY))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o755);
        assert!(read(SERVICE).contains("ExecStart=/usr/local/bin/pyo-verify run\n"));
        let timer = read(TIMER);
        assert!(timer.contains("OnCalendar=*:0/15\n"), "{timer}");
        assert!(timer.contains("RandomizedDelaySec=5min\n"));
        assert!(timer.contains("Persistent=true\n"));
        assert!(e.path(state::DIR).is_dir());
        assert_eq!(
            *runner.0.borrow(),
            [
                "systemctl daemon-reload",
                "systemctl enable --now pyo-verify.timer"
            ]
        );
    }

    #[test]
    fn install_keeps_an_existing_config() {
        let (dir, runner) = (TestDir::new(), Recorder::default());
        let e = env(&dir, &runner);
        let mine = "url=https://pyo.example\nhold=true\nchannel=rc\n";
        std::fs::create_dir_all(e.path("/etc/pyo")).unwrap();
        std::fs::write(e.path(config::PATH), mine).unwrap();
        install(&e, Some("https://other.example"), Some(Channel::Stable)).unwrap();
        assert_eq!(std::fs::read_to_string(e.path(config::PATH)).unwrap(), mine);
        // Again, also without an address: the same.
        install(&e, None, None).unwrap();
        assert_eq!(std::fs::read_to_string(e.path(config::PATH)).unwrap(), mine);
    }

    #[test]
    fn install_needs_a_usable_address() {
        for url in [
            None,
            Some("pyo.example"),
            Some("http://pyo.example"),
            Some("https://pyo.example\nhold=true"),
        ] {
            let (dir, runner) = (TestDir::new(), Recorder::default());
            let e = env(&dir, &runner);
            assert!(install(&e, url, None).is_err(), "{url:?}");
            assert!(!e.path(config::PATH).exists());
            assert!(!e.path(TIMER).exists());
            assert!(runner.0.borrow().is_empty());
        }
        // Test servers may use plain HTTP.
        let (dir, runner) = (TestDir::new(), Recorder::default());
        let e = env(&dir, &runner);
        install(&e, Some("http://10.0.0.2:8000"), Some(Channel::Rc)).unwrap();
        assert_eq!(config::load(&e).unwrap().channel, Channel::Rc);
    }

    #[test]
    fn uninstall_removes_what_install_and_the_runs_left() {
        let (dir, runner) = (TestDir::new(), Recorder::default());
        let e = env(&dir, &runner);
        install(&e, Some("https://pyo.example"), None).unwrap();
        std::fs::write(e.path(state::PATH), "floor=0.51.0\n").unwrap();
        std::fs::write(e.path(status::PATH), "{}").unwrap();
        std::fs::write(update::previous(&e.path(BINARY)), "older").unwrap();
        runner.0.borrow_mut().clear();
        uninstall(&e).unwrap();
        for gone in [
            config::PATH,
            BINARY,
            SERVICE,
            TIMER,
            state::DIR,
            "/usr/local/bin/pyo-verify.previous",
        ] {
            assert!(!e.path(gone).exists(), "{gone}");
        }
        // Pyo's other files in /etc/pyo are not its to remove.
        assert!(e.path("/etc/pyo").is_dir());
        assert_eq!(
            *runner.0.borrow(),
            [
                "systemctl disable --now pyo-verify.timer",
                "systemctl daemon-reload",
                "k3s kubectl --request-timeout=15s -n pyo-system delete configmap pyo-verifier --ignore-not-found",
            ]
        );
        // Twice is fine.
        uninstall(&e).unwrap();
    }
}
