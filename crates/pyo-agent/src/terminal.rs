//! A shell in one of a service's replicas, for the dashboard's terminal: exec
//! with a TTY, relayed over a WebSocket the agent dials to the API.

use futures::{SinkExt, StreamExt};
use k8s_openapi::api::core::v1::Pod;
use kube::{
    Api, ResourceExt,
    api::{AttachParams, ListParams, TerminalSize},
};
use pyo_proto::{ServiceRef, TerminalFrame};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_tungstenite::tungstenite::Message;

use crate::{Agent, observe::SERVICE_LABEL};

/// The command that starts the chosen shell, with TERM so editors and colors
/// work. "auto" is bash when the image has it, else sh.
fn shell_command(shell: &str) -> String {
    match shell {
        "bash" | "sh" | "zsh" | "ash" => format!("export TERM=xterm-256color; exec {shell}"),
        _ => "export TERM=xterm-256color; if command -v bash >/dev/null 2>&1; then exec bash; else exec sh; fi".into(),
    }
}

fn frame(f: &TerminalFrame) -> Message {
    Message::text(serde_json::to_string(f).unwrap_or_default())
}

pub async fn run(
    agent: Agent,
    session_id: String,
    service: ServiceRef,
    pod: String,
    shell: String,
) {
    let mut ws = match agent.api.terminal_socket(&session_id).await {
        Ok(ws) => ws,
        Err(e) => {
            return tracing::warn!(%session_id, error = %e, "terminal: couldn't reach the API");
        }
    };
    let result = open(&agent, &service, &pod, &shell, &mut ws).await;
    if let Err(e) = result {
        let _ = ws
            .send(frame(&TerminalFrame::Error {
                message: format!("{e:#}"),
            }))
            .await;
    }
    let _ = ws.close(None).await;
    tracing::info!(%session_id, service = %service.name, "terminal closed");
}

async fn open<S>(
    agent: &Agent,
    service: &ServiceRef,
    want_pod: &str,
    shell: &str,
    ws: &mut S,
) -> anyhow::Result<()>
where
    S: futures::Stream<Item = Result<Message, tokio_tungstenite::tungstenite::Error>>
        + futures::Sink<Message, Error = tokio_tungstenite::tungstenite::Error>
        + Unpin,
{
    let pods: Api<Pod> = Api::namespaced(agent.kube.clone(), &service.namespace);
    let list = pods
        .list(&ListParams::default().labels(&format!("{SERVICE_LABEL}={}", service.service_id)))
        .await?;
    let ready = |p: &Pod| {
        p.metadata.deletion_timestamp.is_none()
            && p.status
                .as_ref()
                .and_then(|s| s.conditions.as_ref())
                .is_some_and(|cs| cs.iter().any(|c| c.type_ == "Ready" && c.status == "True"))
    };
    let pod = if want_pod.is_empty() {
        list.items.iter().find(|p| ready(p)).ok_or_else(|| {
            anyhow::anyhow!(
                "{} has no running replica to open a terminal in. Start or deploy it first.",
                service.name
            )
        })?
    } else {
        let p = list.items.iter().find(|p| p.name_any() == want_pod).ok_or_else(|| anyhow::anyhow!("Replica {want_pod} is gone (replaced by a deploy or restart). Pick another one."))?;
        if !ready(p) {
            anyhow::bail!("Replica {want_pod} isn't running right now. Pick another one.");
        }
        p
    };
    let name = pod.name_any();

    // A named shell must exist in the image: say so rather than a bare exit.
    if shell != "auto" && !shell.is_empty() {
        let mut probe = pods
            .exec(
                &name,
                ["sh", "-c", &format!("command -v {shell}")],
                &AttachParams::default().stdout(true).stderr(false),
            )
            .await
            .map_err(|e| no_sh(&name, e))?;
        let found = match probe.take_status() {
            Some(s) => s
                .await
                .is_some_and(|st| st.status.as_deref() == Some("Success")),
            None => false,
        };
        if !found {
            anyhow::bail!("{shell} isn't installed in this image. Try sh, or Auto.");
        }
    }

    let mut proc = pods
        .exec(
            &name,
            ["sh".to_owned(), "-c".to_owned(), shell_command(shell)],
            &AttachParams::interactive_tty(),
        )
        .await
        .map_err(|e| no_sh(&name, e))?;
    let mut stdin = proc.stdin().ok_or_else(|| anyhow::anyhow!("no stdin"))?;
    let mut stdout = proc.stdout().ok_or_else(|| anyhow::anyhow!("no stdout"))?;
    let mut size = proc
        .terminal_size()
        .ok_or_else(|| anyhow::anyhow!("no tty"))?;
    let status = proc.take_status();
    ws.send(frame(&TerminalFrame::Ready { pod: name.clone() }))
        .await?;

    let mut buf = vec![0u8; 16 * 1024];
    let mut exited = Box::pin(async move {
        if let Some(s) = status {
            s.await;
        }
    });
    loop {
        tokio::select! {
            n = stdout.read(&mut buf) => match n {
                Ok(0) | Err(_) => break,
                Ok(n) => ws.send(Message::binary(buf[..n].to_vec())).await?,
            },
            msg = ws.next() => match msg {
                Some(Ok(Message::Binary(b))) => stdin.write_all(&b).await?,
                Some(Ok(Message::Text(t))) => {
                    if let Ok(TerminalFrame::Resize { cols, rows }) = serde_json::from_str(t.as_str()) {
                        let _ = size.send(TerminalSize { width: cols, height: rows }).await;
                    }
                }
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => return Ok(()),
                _ => {}
            },
            _ = &mut exited => break,
        }
    }
    // Output written just before exit.
    while let Ok(Ok(n)) =
        tokio::time::timeout(std::time::Duration::from_millis(200), stdout.read(&mut buf)).await
    {
        if n == 0 {
            break;
        }
        ws.send(Message::binary(buf[..n].to_vec())).await?;
    }
    ws.send(frame(&TerminalFrame::Exit)).await?;
    Ok(())
}

fn no_sh(pod: &str, e: kube::Error) -> anyhow::Error {
    anyhow::anyhow!(
        "Couldn't start a shell in {pod}: {e}. Images without sh (distroless, scratch) have no shell to open."
    )
}
