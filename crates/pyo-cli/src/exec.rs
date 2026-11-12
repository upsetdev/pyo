//! `pyo exec`: a shell in one of the service's replicas, over the same
//! WebSocket as the dashboard's terminal. Keys go up as binary frames, the
//! shell's output comes down as binary, and JSON text frames carry resize,
//! ready, error and exit (pyo_proto::TerminalFrame).

use anyhow::{Context, Result, bail};
use futures::{SinkExt, StreamExt};
use pyo_proto::TerminalFrame;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_tungstenite::tungstenite::{Message, client::IntoClientRequest, http::HeaderValue};

use crate::{api::Api, service};

/// Puts the terminal back however exec ends.
struct Raw;

impl Raw {
    fn on() -> Result<Self> {
        crossterm::terminal::enable_raw_mode().context("This needs an interactive terminal")?;
        Ok(Raw)
    }
}

impl Drop for Raw {
    fn drop(&mut self) {
        let _ = crossterm::terminal::disable_raw_mode();
    }
}

pub async fn run(
    api: &Api,
    service: Option<&str>,
    replica: Option<String>,
    shell: Option<String>,
) -> Result<()> {
    let t = service::target(api, service).await?;
    let ws_base = api
        .base
        .replacen("https://", "wss://", 1)
        .replacen("http://", "ws://", 1);
    let mut url = format!(
        "{ws_base}/api/v1/services/{}/terminal?shell={}",
        t.id,
        shell.as_deref().unwrap_or("auto")
    );
    if let Some(r) = &replica {
        url.push_str(&format!("&replica={r}"));
    }
    let mut req = url.into_client_request()?;
    req.headers_mut().insert(
        "authorization",
        HeaderValue::from_str(&format!("Bearer {}", api.token()))?,
    );
    let (ws, _) = tokio_tungstenite::connect_async(req)
        .await
        .context("Can't open a terminal on the service")?;
    let (mut up, mut down) = ws.split();

    eprintln!(
        "Connecting to {}…",
        t.service()["name"].as_str().unwrap_or("the service")
    );
    let _raw = Raw::on()?;
    if let Ok((cols, rows)) = crossterm::terminal::size() {
        up.send(Message::text(serde_json::to_string(
            &TerminalFrame::Resize { cols, rows },
        )?))
        .await?;
    }

    let mut stdin = tokio::io::stdin();
    let mut stdout = tokio::io::stdout();
    let mut buf = [0u8; 4096];
    let mut size = crossterm::terminal::size().unwrap_or((80, 24));
    let mut tick = tokio::time::interval(std::time::Duration::from_millis(500));
    loop {
        tokio::select! {
            n = stdin.read(&mut buf) => {
                let n = n?;
                if n == 0 { break; }
                up.send(Message::binary(buf[..n].to_vec())).await?;
            }
            m = down.next() => match m {
                Some(Ok(Message::Binary(b))) => { stdout.write_all(&b).await?; stdout.flush().await?; }
                Some(Ok(Message::Text(t))) => match serde_json::from_str::<TerminalFrame>(t.as_str()) {
                    Ok(TerminalFrame::Exit) => break,
                    Ok(TerminalFrame::Error { message }) => { drop(_raw); bail!(message); }
                    _ => {}
                },
                Some(Ok(Message::Close(_))) | None => break,
                Some(Ok(_)) => {}
                Some(Err(e)) => return Err(e.into()),
            },
            // Follow the window's size (no SIGWINCH on every platform).
            _ = tick.tick() => {
                if let Ok(s) = crossterm::terminal::size() && s != size {
                    size = s;
                    up.send(Message::text(serde_json::to_string(&TerminalFrame::Resize { cols: s.0, rows: s.1 })?)).await?;
                }
            }
        }
    }
    Ok(())
}
