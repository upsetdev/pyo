//! A shell in one of a service's replicas, for the dashboard's terminal: exec
//! with a TTY, relayed over a WebSocket the agent dials to the API. Also one
//! command (`pyo exec -- cmd`), with or without a TTY: without one, each
//! output frame starts with its stream, and the exit code comes back.

use futures::{SinkExt, StreamExt};
use k8s_openapi::api::core::v1::Pod;
use kube::{
    Api, ResourceExt,
    api::{AttachParams, ListParams, TerminalSize},
};
use pyo_proto::{STREAM_STDERR, STREAM_STDOUT, ServiceRef, TerminalFrame};
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

/// What to run: a shell, or one command, with or without a TTY.
pub struct Run {
    pub pod: String,
    pub shell: String,
    pub command: Vec<String>,
    pub tty: bool,
}

pub async fn run(agent: Agent, session_id: String, service: ServiceRef, what: Run) {
    let mut ws = match agent.api.terminal_socket(&session_id).await {
        Ok(ws) => ws,
        Err(e) => {
            return tracing::warn!(%session_id, error = %e, "terminal: couldn't reach the API");
        }
    };
    let result = open(&agent, &service, &what, &mut ws).await;
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

/// The exit code an exec's status gives: 0 for success, the code of a
/// command that failed, none when it didn't run (and why).
fn exit_code(
    status: Option<&k8s_openapi::apimachinery::pkg::apis::meta::v1::Status>,
) -> Result<i32, String> {
    let Some(st) = status else {
        return Err("The command's status is unknown.".into());
    };
    if st.status.as_deref() == Some("Success") {
        return Ok(0);
    }
    st.details
        .as_ref()
        .and_then(|d| d.causes.as_ref())
        .and_then(|cs| cs.iter().find(|c| c.reason.as_deref() == Some("ExitCode")))
        .and_then(|c| c.message.as_deref()?.trim().parse().ok())
        .ok_or_else(|| {
            st.message
                .clone()
                .filter(|m| !m.is_empty())
                .unwrap_or_else(|| "The command didn't run.".into())
        })
}

async fn open<S>(agent: &Agent, service: &ServiceRef, what: &Run, ws: &mut S) -> anyhow::Result<()>
where
    S: futures::Stream<Item = Result<Message, tokio_tungstenite::tungstenite::Error>>
        + futures::Sink<Message, Error = tokio_tungstenite::tungstenite::Error>
        + Unpin,
{
    let (want_pod, shell) = (what.pod.as_str(), what.shell.as_str());
    let pods: Api<Pod> = Api::namespaced(agent.kube.clone(), &service.namespace);
    let lp = ListParams::default()
        .labels(&format!("{SERVICE_LABEL}={}", service.service_id))
        .fields(crate::pods::LIVE);
    let list = crate::pods::list_pods(&pods, &lp).await?;
    let ready = |p: &Pod| {
        p.metadata.deletion_timestamp.is_none()
            && p.status
                .as_ref()
                .and_then(|s| s.conditions.as_ref())
                .is_some_and(|cs| cs.iter().any(|c| c.type_ == "Ready" && c.status == "True"))
    };
    let pod = if want_pod.is_empty() {
        list.iter().find(|p| ready(p)).ok_or_else(|| {
            anyhow::anyhow!(
                "{} has no running replica to open a terminal in. Start or deploy it first.",
                service.name
            )
        })?
    } else {
        let p = list.iter().find(|p| p.name_any() == want_pod).ok_or_else(|| anyhow::anyhow!("Replica {want_pod} is gone (replaced by a deploy or restart). Pick another one."))?;
        if !ready(p) {
            anyhow::bail!("Replica {want_pod} isn't running right now. Pick another one.");
        }
        p
    };
    let name = pod.name_any();

    if !what.command.is_empty() {
        return command(&pods, &name, what, ws).await;
    }

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
    // A ping every 30 seconds keeps a quiet session open through proxies
    // that close idle connections (Cloudflare: after 100 seconds).
    let mut keepalive = tokio::time::interval(std::time::Duration::from_secs(30));
    keepalive.tick().await;
    loop {
        tokio::select! {
            _ = keepalive.tick() => ws.send(Message::Ping(Default::default())).await?,
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
    ws.send(frame(&TerminalFrame::Exit { code: None })).await?;
    Ok(())
}

/// One command, as it is (no shell), until it exits: stdin from the
/// socket until `StdinEnd`, its output back, then its exit code.
async fn command<S>(pods: &Api<Pod>, name: &str, what: &Run, ws: &mut S) -> anyhow::Result<()>
where
    S: futures::Stream<Item = Result<Message, tokio_tungstenite::tungstenite::Error>>
        + futures::Sink<Message, Error = tokio_tungstenite::tungstenite::Error>
        + Unpin,
{
    let params = if what.tty {
        AttachParams::interactive_tty()
    } else {
        AttachParams::default()
            .stdin(true)
            .stdout(true)
            .stderr(true)
    };
    let mut proc = pods
        .exec(name, what.command.clone(), &params)
        .await
        .map_err(|e| anyhow::anyhow!("Couldn't run the command in {name}: {e}"))?;
    let mut stdin = proc.stdin();
    let mut stdout = proc.stdout().ok_or_else(|| anyhow::anyhow!("no stdout"))?;
    let mut stderr = proc.stderr();
    let mut size = proc.terminal_size();
    let status = proc.take_status();
    ws.send(frame(&TerminalFrame::Ready {
        pod: name.to_owned(),
    }))
    .await?;
    // Output frames carry their stream without a TTY.
    let tag = |stream: u8, b: &[u8]| {
        if what.tty {
            b.to_vec()
        } else {
            let mut v = Vec::with_capacity(b.len() + 1);
            v.push(stream);
            v.extend_from_slice(b);
            v
        }
    };
    let mut out = vec![0u8; 16 * 1024];
    let mut err = vec![0u8; 16 * 1024];
    let (mut out_open, mut err_open) = (true, stderr.is_some());
    let mut keepalive = tokio::time::interval(std::time::Duration::from_secs(30));
    keepalive.tick().await;
    while out_open || err_open {
        tokio::select! {
            _ = keepalive.tick() => ws.send(Message::Ping(Default::default())).await?,
            n = stdout.read(&mut out), if out_open => match n {
                Ok(0) | Err(_) => out_open = false,
                Ok(n) => ws.send(Message::binary(tag(STREAM_STDOUT, &out[..n]))).await?,
            },
            n = async { stderr.as_mut().expect("open").read(&mut err).await }, if err_open => match n {
                Ok(0) | Err(_) => err_open = false,
                Ok(n) => ws.send(Message::binary(tag(STREAM_STDERR, &err[..n]))).await?,
            },
            msg = ws.next() => match msg {
                Some(Ok(Message::Binary(b))) => {
                    if let Some(w) = stdin.as_mut() {
                        w.write_all(&b).await?;
                    }
                }
                Some(Ok(Message::Text(t))) => match serde_json::from_str(t.as_str()) {
                    // Dropping the writer closes the command's stdin.
                    Ok(TerminalFrame::StdinEnd) => drop(stdin.take()),
                    Ok(TerminalFrame::Resize { cols, rows }) => {
                        if let Some(s) = size.as_mut() {
                            let _ = s.send(TerminalSize { width: cols, height: rows }).await;
                        }
                    }
                    _ => {}
                },
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => return Ok(()),
                _ => {}
            },
        }
    }
    drop(stdin);
    let st = match status {
        Some(s) => s.await,
        None => None,
    };
    match exit_code(st.as_ref()) {
        Ok(code) => {
            ws.send(frame(&TerminalFrame::Exit { code: Some(code) }))
                .await?
        }
        Err(why) => anyhow::bail!(why),
    }
    Ok(())
}

fn no_sh(pod: &str, e: kube::Error) -> anyhow::Error {
    anyhow::anyhow!(
        "Couldn't start a shell in {pod}: {e}. Images without sh (distroless, scratch) have no shell to open."
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use k8s_openapi::apimachinery::pkg::apis::meta::v1::{Status, StatusCause, StatusDetails};

    #[test]
    fn exit_codes_from_the_status() {
        let ok = Status {
            status: Some("Success".into()),
            ..Default::default()
        };
        assert_eq!(exit_code(Some(&ok)), Ok(0));
        let failed = Status {
            status: Some("Failure".into()),
            reason: Some("NonZeroExitCode".into()),
            details: Some(StatusDetails {
                causes: Some(vec![StatusCause {
                    reason: Some("ExitCode".into()),
                    message: Some("3".into()),
                    ..Default::default()
                }]),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(exit_code(Some(&failed)), Ok(3));
        let missing = Status {
            status: Some("Failure".into()),
            message: Some("exec: \"nope\": executable file not found in $PATH".into()),
            ..Default::default()
        };
        assert!(exit_code(Some(&missing)).unwrap_err().contains("not found"));
        assert!(exit_code(None).is_err());
    }
}
