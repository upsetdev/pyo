//! `pyo exec`: a shell in one of the service's replicas, or one command
//! (`pyo exec -- cmd args`), over the same WebSocket as the dashboard's
//! terminal. Keys or stdin go up as binary frames, output comes down as
//! binary (without a TTY, each frame starts with its stream), and JSON text
//! frames carry resize, end of input, ready, error and exit
//! (pyo_proto::TerminalFrame).

use std::io::IsTerminal;

use anyhow::{Context, Result, bail};
use futures::{SinkExt, StreamExt};
use pyo_proto::{STREAM_STDERR, TerminalFrame};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_tungstenite::tungstenite::{Message, client::IntoClientRequest, http::HeaderValue};

use crate::{api::Api, service};

/// Puts the terminal back however exec ends.
pub(crate) struct Raw;

impl Raw {
    pub(crate) fn on() -> Result<Self> {
        crossterm::terminal::enable_raw_mode().context("This needs an interactive terminal")?;
        Ok(Raw)
    }
}

impl Drop for Raw {
    fn drop(&mut self) {
        let _ = crossterm::terminal::disable_raw_mode();
    }
}

/// A command ended with this exit code: `pyo` exits with it too.
#[derive(Debug)]
pub struct Exited(pub i32);

impl std::fmt::Display for Exited {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "exited with {}", self.0)
    }
}

impl std::error::Error for Exited {}

/// What `pyo exec` runs, and how.
#[derive(Default)]
pub struct Opts {
    pub replica: Option<String>,
    pub shell: Option<String>,
    /// A command instead of a shell.
    pub command: Vec<String>,
    /// A TTY or not; by default one when stdin and stdout are terminals.
    pub tty: Option<bool>,
    /// Send stdin even when it's a terminal (without a TTY).
    pub stdin: bool,
}

/// Whether a command gets a TTY: as asked, else when both ends are terminals.
pub fn wants_tty(asked: Option<bool>, stdin_tty: bool, stdout_tty: bool) -> bool {
    asked.unwrap_or(stdin_tty && stdout_tty)
}

/// The terminal endpoint's URL, its query encoded.
pub fn socket_url(base: &str, service_id: &str, o: &Opts, tty: bool) -> Result<String> {
    let ws_base = base
        .replacen("https://", "wss://", 1)
        .replacen("http://", "ws://", 1);
    let mut url = reqwest::Url::parse(&format!("{ws_base}/api/v1/services/{service_id}/terminal"))
        .context("Pyo's address isn't a URL")?;
    {
        let mut q = url.query_pairs_mut();
        q.append_pair("shell", o.shell.as_deref().unwrap_or("auto"));
        if let Some(r) = &o.replica {
            q.append_pair("replica", r);
        }
        if !o.command.is_empty() {
            q.append_pair("command", &serde_json::to_string(&o.command)?);
            if !tty {
                q.append_pair("tty", "false");
            }
        }
    }
    Ok(url.to_string())
}

/// Why Pyo turned the terminal down: its message, else what failed.
fn refused(e: tokio_tungstenite::tungstenite::Error) -> anyhow::Error {
    use tokio_tungstenite::tungstenite::Error;
    if let Error::Http(res) = &e {
        let msg = res
            .body()
            .as_deref()
            .and_then(|b| serde_json::from_slice::<serde_json::Value>(b).ok())
            .and_then(|v| v["error"].as_str().map(str::to_owned));
        if let Some(m) = msg {
            return anyhow::anyhow!(m);
        }
    }
    anyhow::Error::from(e).context("Can't open a terminal on the service")
}

pub async fn run(api: &Api, service: Option<&str>, o: Opts) -> Result<()> {
    let t = service::target(api, service).await?;
    let tty = o.command.is_empty()
        || wants_tty(
            o.tty,
            std::io::stdin().is_terminal(),
            std::io::stdout().is_terminal(),
        );
    let mut req = socket_url(&api.base, &t.id, &o, tty)?.into_client_request()?;
    req.headers_mut().insert(
        "authorization",
        HeaderValue::from_str(&format!("Bearer {}", api.token()))?,
    );
    let (ws, _) = tokio_tungstenite::connect_async(req)
        .await
        .map_err(refused)?;
    let (mut up, mut down) = ws.split();

    let name = t.service()["name"].as_str().unwrap_or("the service");
    if o.command.is_empty() {
        eprintln!("Connecting to {name}…");
    }
    let raw = if tty { Some(Raw::on()?) } else { None };
    if tty && let Ok((cols, rows)) = crossterm::terminal::size() {
        up.send(Message::text(serde_json::to_string(
            &TerminalFrame::Resize { cols, rows },
        )?))
        .await?;
    }
    // Without a TTY, stdin goes up when it's a pipe or file (or -i), then
    // its end. A terminal's stdin isn't read: the command gets none.
    let mut send_stdin = tty || o.stdin || !std::io::stdin().is_terminal();
    if !send_stdin {
        up.send(Message::text(serde_json::to_string(
            &TerminalFrame::StdinEnd,
        )?))
        .await?;
    }

    let mut stdin = tokio::io::stdin();
    let mut stdout = tokio::io::stdout();
    let mut stderr = tokio::io::stderr();
    let mut buf = vec![0u8; 32 * 1024];
    let mut size = crossterm::terminal::size().unwrap_or((80, 24));
    let mut tick = tokio::time::interval(std::time::Duration::from_millis(500));
    let code: Option<i32> = loop {
        tokio::select! {
            n = stdin.read(&mut buf), if send_stdin => {
                let n = n?;
                if n == 0 {
                    // End of input: a shell's session ends, a command's stdin closes.
                    if tty && o.command.is_empty() { break None; }
                    send_stdin = false;
                    up.send(Message::text(serde_json::to_string(&TerminalFrame::StdinEnd)?)).await?;
                    continue;
                }
                up.send(Message::binary(buf[..n].to_vec())).await?;
            }
            m = down.next() => match m {
                Some(Ok(Message::Binary(b))) => {
                    if tty {
                        stdout.write_all(&b).await?;
                        stdout.flush().await?;
                    } else if let Some((stream, rest)) = b.split_first() {
                        if *stream == STREAM_STDERR {
                            stderr.write_all(rest).await?;
                            stderr.flush().await?;
                        } else {
                            stdout.write_all(rest).await?;
                            stdout.flush().await?;
                        }
                    }
                }
                Some(Ok(Message::Text(t))) => match serde_json::from_str::<TerminalFrame>(t.as_str()) {
                    Ok(TerminalFrame::Exit { code }) => break code,
                    Ok(TerminalFrame::Error { message }) => { drop(raw); bail!(message); }
                    _ => {}
                },
                Some(Ok(Message::Close(_))) | None => {
                    if o.command.is_empty() { break None; }
                    drop(raw);
                    bail!("The connection closed before the command finished.");
                }
                Some(Ok(_)) => {}
                Some(Err(e)) => return Err(e.into()),
            },
            // Follow the window's size (no SIGWINCH on every platform).
            _ = tick.tick(), if tty => {
                if let Ok(s) = crossterm::terminal::size() && s != size {
                    size = s;
                    up.send(Message::text(serde_json::to_string(&TerminalFrame::Resize { cols: s.0, rows: s.1 })?)).await?;
                }
            }
        }
    };
    drop(raw);
    let _ = up.close().await;
    match code {
        Some(c) if c != 0 => Err(Exited(c).into()),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tty_only_between_terminals_unless_asked() {
        assert!(wants_tty(None, true, true));
        assert!(!wants_tty(None, false, true), "piped input");
        assert!(!wants_tty(None, true, false), "output to a file");
        assert!(wants_tty(Some(true), false, false));
        assert!(!wants_tty(Some(false), true, true));
    }

    #[test]
    fn the_command_goes_in_the_query_as_json() {
        let o = Opts {
            command: vec!["sh".into(), "-c".into(), "echo a && echo b".into()],
            replica: Some("web-1".into()),
            ..Default::default()
        };
        let u = socket_url("https://pyo.example.com", "svc_1", &o, false).unwrap();
        assert!(u.starts_with(
            "wss://pyo.example.com/api/v1/services/svc_1/terminal?shell=auto&replica=web-1&command="
        ));
        let parsed = reqwest::Url::parse(&u).unwrap();
        let cmd = parsed
            .query_pairs()
            .find(|(k, _)| k == "command")
            .unwrap()
            .1;
        assert_eq!(
            serde_json::from_str::<Vec<String>>(&cmd).unwrap(),
            o.command
        );
        assert!(u.ends_with("&tty=false"));
        let shell = socket_url("http://localhost:8000", "svc_1", &Opts::default(), true).unwrap();
        assert_eq!(
            shell,
            "ws://localhost:8000/api/v1/services/svc_1/terminal?shell=auto"
        );
    }
}
