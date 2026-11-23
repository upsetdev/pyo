//! `/var/lib/pyo-verify/state`: the highest version this server has moved
//! to (`floor=X.Y.Z`). The verifier never goes below it, also when an older
//! verifier binary is put back.

use crate::{
    sys::{Env, Problem, read_limited, write_atomic},
    version::Version,
};

pub const DIR: &str = "/var/lib/pyo-verify";
pub const PATH: &str = "/var/lib/pyo-verify/state";

/// The recorded floor. None when nothing was recorded yet. A file that is
/// there and does not read stops the verifier: it does not guess.
pub fn floor(env: &Env) -> Result<Option<Version>, Problem> {
    let path = env.path(PATH);
    if !path.exists() {
        return Ok(None);
    }
    let unreadable = || {
        Problem::new(format!(
            "{PATH} could not be read. Run the connect command from Pyo again."
        ))
    };
    let text = read_limited(&path, 4096)
        .ok()
        .and_then(|b| String::from_utf8(b).ok())
        .ok_or_else(unreadable)?;
    text.lines()
        .find_map(|l| l.trim().strip_prefix("floor="))
        .and_then(Version::parse)
        .map(Some)
        .ok_or_else(unreadable)
}

/// Records `version` as the floor, unless the floor is already higher.
pub fn raise(env: &Env, version: Version) -> Result<(), Problem> {
    if floor(env)?.is_some_and(|f| f >= version) {
        return Ok(());
    }
    write_atomic(
        &env.path(PATH),
        format!("floor={version}\n").as_bytes(),
        0o644,
    )
    .map_err(|e| {
        Problem::with(
            format!("{PATH} could not be written. Nothing was changed."),
            e.to_string(),
        )
    })
}
