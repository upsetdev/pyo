//! The `pyo-verify` binary: the real server around lib.rs.

use std::process::ExitCode;

use pyo_verify::{
    Command, USAGE, install, parse_args,
    release::Sigstore,
    run, status,
    sys::{Curl, Env, Exec, Problem, RealRunner, SystemClock},
    version::Version,
};

fn finish(result: Result<String, Problem>) -> ExitCode {
    match result {
        Ok(line) => {
            println!("{line}");
            ExitCode::SUCCESS
        }
        Err(p) => {
            println!("{}", p.plain);
            if let Some(detail) = p.detail {
                eprintln!("{detail}");
            }
            ExitCode::FAILURE
        }
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(command) = parse_args(&args) else {
        eprintln!("{USAGE}");
        return ExitCode::from(2);
    };
    if command == Command::Version {
        println!("{}", Version::own());
        return ExitCode::SUCCESS;
    }
    let Ok(exe) = std::env::current_exe() else {
        println!("The verifier could not find its own file.");
        return ExitCode::FAILURE;
    };
    // The one place the pieces are chosen. The verifier is Sigstore's, with
    // the trust root compiled into pyo-sigstore. Nothing here is read from
    // a flag, a file or the environment.
    let env = Env {
        root: "/".into(),
        runner: &RealRunner,
        http: &Curl,
        clock: &SystemClock,
        handover: &Exec,
        verifier: &Sigstore,
        own: Version::own(),
        arch: std::env::consts::ARCH,
        exe,
    };
    match command {
        Command::Version => ExitCode::SUCCESS,
        Command::Run { handed_over_from } => {
            let report = run::run(&env, handed_over_from.as_deref());
            println!("{}", report.message);
            if let Some(detail) = report.detail {
                eprintln!("{detail}");
            }
            ExitCode::from(report.outcome.exit_code())
        }
        Command::Status => {
            println!("{}", status::show(&env));
            ExitCode::SUCCESS
        }
        Command::Install { url, channel } => {
            finish(install::install(&env, url.as_deref(), channel))
        }
        Command::Uninstall => finish(install::uninstall(&env)),
    }
}
