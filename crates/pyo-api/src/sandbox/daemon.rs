//! Pyo's calls to a sandbox server's daemon. Pyo never connects to a
//! daemon itself: each call is queued as a signed command the server polls
//! for over its outbound channel, and its answer awaited
//! (outbound/channel.rs `ask`). A server that joined at Pyo's tailnet door
//! before 0.52.0 isn't on the channel until it's updated, so every call to
//! it fails with [`super::NEEDS_UPDATE`].

use std::time::Duration;

use pyo_proto::sandbox::{
    DaemonCommand, DaemonError, DaemonResult, ErrorCode, ExecRequest, ExecResult, SandboxStatus,
    outbound,
};

/// Calls that aren't exec wait this long at most.
const TIMEOUT: Duration = Duration::from_secs(20);

pub struct Daemon {
    /// The server's id.
    server_id: String,
}

/// Pyo's state, for the channel (set at start).
static STATE: std::sync::OnceLock<crate::AppState> = std::sync::OnceLock::new();

/// Calls to sandbox servers go through their channel from now on.
pub fn init(s: crate::AppState) {
    let _ = STATE.set(s);
}

/// The error for a call to a server that joined the old way.
pub fn needs_update() -> DaemonError {
    DaemonError::new(ErrorCode::Conflict, super::NEEDS_UPDATE)
}

impl Daemon {
    /// The daemon of the server `server_id`.
    pub fn new(server_id: &str) -> Daemon {
        Daemon {
            server_id: server_id.to_owned(),
        }
    }

    /// Pyo's state, once this server is known to be on the channel.
    async fn channel(&self) -> Result<&'static crate::AppState, DaemonError> {
        let Some(s) = STATE.get() else {
            return Err(DaemonError::new(
                ErrorCode::Internal,
                "Pyo isn't ready to reach sandbox servers yet.",
            ));
        };
        let outbound: Option<bool> =
            sqlx::query_scalar("SELECT outbound FROM pyo.sandbox_servers WHERE id = $1")
                .bind(&self.server_id)
                .fetch_optional(&s.db)
                .await
                .map_err(|e| DaemonError::new(ErrorCode::Internal, e.to_string()))?;
        match outbound {
            Some(true) => Ok(s),
            Some(false) => Err(needs_update()),
            None => Err(DaemonError::new(
                ErrorCode::NotFound,
                "The sandbox server was removed.",
            )),
        }
    }

    /// A command through the server's channel: its answer.
    async fn ask(&self, cmd: &DaemonCommand, wait: Duration) -> Result<DaemonResult, DaemonError> {
        let s = self.channel().await?;
        match super::outbound::channel::ask(s, &self.server_id, cmd, wait).await? {
            DaemonResult::Error { error } => Err(error),
            r => Ok(r),
        }
    }

    pub async fn command(&self, cmd: &DaemonCommand) -> Result<DaemonResult, DaemonError> {
        self.ask(cmd, TIMEOUT).await
    }

    /// A command whose answer is a sandbox.
    pub async fn sandbox(&self, cmd: &DaemonCommand) -> Result<SandboxStatus, DaemonError> {
        match self.command(cmd).await? {
            DaemonResult::Sandbox { sandbox } => Ok(sandbox),
            other => Err(something_else(other)),
        }
    }

    /// Its report now, asked over the channel. Servers post theirs every
    /// 15 seconds, so only tests ask.
    #[cfg(test)]
    pub async fn report(&self) -> Result<pyo_proto::sandbox::ServerReport, DaemonError> {
        match self.ask(&DaemonCommand::Report, TIMEOUT).await? {
            DaemonResult::Report { report } => Ok(*report),
            other => Err(something_else(other)),
        }
    }

    pub async fn exec(&self, id: &str, req: &ExecRequest) -> Result<ExecResult, DaemonError> {
        let ms = req.timeout_ms.unwrap_or(60_000);
        // The server stops it by the channel's longest.
        let ms = if ms == 0 {
            outbound::EXEC_MAX_MS
        } else {
            ms.min(outbound::EXEC_MAX_MS)
        };
        let cmd = DaemonCommand::Exec {
            id: id.to_owned(),
            request: req.clone(),
        };
        match self
            .ask(&cmd, Duration::from_millis(ms) + Duration::from_secs(30))
            .await?
        {
            DaemonResult::Exec { result } => Ok(result),
            other => Err(something_else(other)),
        }
    }

    pub async fn read_file(&self, id: &str, path: &str) -> Result<Vec<u8>, DaemonError> {
        use base64::Engine as _;
        let cmd = DaemonCommand::ReadFile {
            id: id.to_owned(),
            path: path.to_owned(),
        };
        match self.ask(&cmd, TIMEOUT).await {
            Ok(DaemonResult::File { data }) => base64::engine::general_purpose::STANDARD
                .decode(data)
                .map_err(|_| {
                    DaemonError::new(
                        ErrorCode::Internal,
                        "The sandbox server's answer didn't parse.",
                    )
                }),
            Ok(other) => Err(something_else(other)),
            Err(e) => Err(over_the_channel(e)),
        }
    }

    pub async fn write_file(&self, id: &str, path: &str, data: Vec<u8>) -> Result<u64, DaemonError> {
        use base64::Engine as _;
        if data.len() > outbound::FILE_CAP {
            return Err(over_the_channel(DaemonError::new(ErrorCode::InvalidSpec, "")));
        }
        let cmd = DaemonCommand::WriteFile {
            id: id.to_owned(),
            path: path.to_owned(),
            data: base64::engine::general_purpose::STANDARD.encode(data),
        };
        match self.ask(&cmd, TIMEOUT).await {
            Ok(DaemonResult::Written { bytes }) => Ok(bytes),
            Ok(other) => Err(something_else(other)),
            Err(e) => Err(over_the_channel(e)),
        }
    }
}

fn something_else(r: DaemonResult) -> DaemonError {
    DaemonError::new(
        ErrorCode::Internal,
        format!("The sandbox server answered with something else: {r:?}"),
    )
}

/// A file too big for the channel says where bigger ones go.
fn over_the_channel(e: DaemonError) -> DaemonError {
    // The daemon says "The file is bigger than 2 MiB."
    if e.code == ErrorCode::InvalidSpec
        && (e.message.is_empty() || e.message.starts_with("The file is bigger than"))
    {
        return DaemonError::new(
            ErrorCode::InvalidSpec,
            format!(
                "Through Pyo, files go up to {} MiB on this sandbox server. Move bigger ones from a device on your tailnet.",
                outbound::FILE_CAP >> 20
            ),
        );
    }
    e
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_server_that_joined_the_old_way_says_to_update_it() {
        let e = needs_update();
        assert_eq!(e.code, ErrorCode::Conflict);
        assert_eq!(e.message, "This server needs an update. Press Update to reconnect it.");
        let api = super::super::daemon_error(e);
        assert_eq!(api.status, axum::http::StatusCode::CONFLICT);
        assert_eq!(api.message, super::super::NEEDS_UPDATE);
    }

    #[test]
    fn big_files_say_where_they_go() {
        let e = over_the_channel(DaemonError::new(ErrorCode::InvalidSpec, "The file is bigger than 2 MiB."));
        assert!(e.message.contains("tailnet"), "{e}");
        let e = over_the_channel(DaemonError::new(ErrorCode::NotFound, "gone"));
        assert_eq!(e.message, "gone");
    }
}
