//! `pyo login`: approve the CLI in the browser (it listens on localhost
//! for the answer), or pass an API key for CI and machines without one.

use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};

use crate::{api::Api, config::Credentials, ui};

pub async fn run(url: Option<String>, token: Option<String>, expires: &str) -> Result<()> {
    let saved = Credentials::load()?;
    let url = match url.or(saved.url) {
        Some(u) => u,
        None if !std::io::IsTerminal::is_terminal(&std::io::stdin()) => {
            bail!("No terminal to ask on: pass your Pyo's URL with --url.")
        }
        None => ui::ask("Pyo URL (e.g. https://pyo.example.com): ")?,
    };
    let url = normalize(&url)?;

    let token = match token {
        Some(t) => t,
        None => browser_login(&url, expires).await?,
    };
    let api = Api::new(&url, &token)?;
    let me = api.me().await.context("That key doesn't work here")?;
    let email = me["email"].as_str().unwrap_or("").to_owned();
    Credentials {
        url: Some(url.clone()),
        token: Some(token),
        email: Some(email.clone()),
    }
    .save()?;
    ui::ok(&format!("Signed in to {url} as {email}"));
    Ok(())
}

fn normalize(url: &str) -> Result<String> {
    let u = url.trim().trim_end_matches('/');
    if u.is_empty() {
        bail!("The URL is empty.");
    }
    Ok(if u.starts_with("http://") || u.starts_with("https://") {
        u.to_owned()
    } else {
        format!("https://{u}")
    })
}

fn random_hex(n: usize) -> String {
    let mut b = vec![0u8; n];
    rand::fill(&mut b[..]);
    hex::encode(b)
}

fn machine_name() -> String {
    std::fs::read_to_string("/etc/hostname")
        .ok()
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .or_else(|| std::env::var("HOSTNAME").ok())
        .or_else(|| std::env::var("COMPUTERNAME").ok())
        .unwrap_or_default()
}

/// Open the approval page and wait for its answer on localhost. The key
/// expires after `expires` ("30d", "90d", "1y" or "never").
async fn browser_login(url: &str, expires: &str) -> Result<String> {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .context("Can't listen on localhost")?;
    let port = listener.local_addr()?.port();
    let verifier = random_hex(32);
    let challenge = hex::encode(Sha256::digest(verifier.as_bytes()));
    let state = random_hex(16);
    let page = format!(
        "{url}/cli-login?{}",
        [
            ("port", port.to_string()),
            ("state", state.clone()),
            ("challenge", challenge),
            ("name", machine_name())
        ]
        .iter()
        .map(|(k, v)| format!("{k}={}", encode(v)))
        .collect::<Vec<_>>()
        .join("&")
    );
    println!("Opening your browser to approve this computer…");
    println!("If it doesn't open, go to:\n  {page}\n");
    ui::open(&page);

    let code = tokio::time::timeout(Duration::from_secs(300), wait_for_code(&listener, &state))
        .await
        .context(
            "Timed out waiting for approval. Run pyo login again, or use: pyo login --token pyo_…",
        )??;

    let res: Value = reqwest::Client::new()
        .post(format!("{url}/api/cli/token"))
        .json(&json!({ "code": code, "verifier": verifier, "expires": expires }))
        .send()
        .await
        .with_context(|| format!("Can't reach {url}"))?
        .json()
        .await?;
    res["token"].as_str().map(str::to_owned).with_context(|| {
        res["error"]
            .as_str()
            .unwrap_or("Sign-in failed.")
            .to_owned()
    })
}

/// The browser comes back to http://127.0.0.1:<port>/callback?state=…&code=…
async fn wait_for_code(listener: &TcpListener, state: &str) -> Result<String> {
    loop {
        let (mut sock, _) = listener.accept().await?;
        let mut buf = vec![0u8; 8192];
        let n = sock.read(&mut buf).await.unwrap_or(0);
        let req = String::from_utf8_lossy(&buf[..n]);
        let Some(target) = req.lines().next().and_then(|l| l.split_whitespace().nth(1)) else {
            continue;
        };
        let Some(query) = target.strip_prefix("/callback?") else {
            let _ = sock
                .write_all(
                    b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .await;
            continue;
        };
        let params: Vec<(String, String)> = query
            .split('&')
            .filter_map(|kv| kv.split_once('='))
            .map(|(k, v)| (k.to_owned(), decode(v)))
            .collect();
        let get = |k: &str| params.iter().find(|(x, _)| x == k).map(|(_, v)| v.clone());
        let (title, result) = if get("state").as_deref() != Some(state) {
            (
                "This sign-in doesn't match",
                Err(anyhow::anyhow!(
                    "The answer didn't match this login. Run pyo login again."
                )),
            )
        } else if get("error").is_some() {
            ("Cancelled", Err(anyhow::anyhow!("Sign-in was cancelled.")))
        } else if let Some(code) = get("code") {
            ("You're signed in. You can close this tab.", Ok(code))
        } else {
            continue;
        };
        let body = format!(
            "<!doctype html><meta charset=utf-8><title>Pyo CLI</title><body style=\"font:15px system-ui;display:grid;place-items:center;height:90vh\"><p>{title}</p>"
        );
        let _ = sock
            .write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes())
            .await;
        return result;
    }
}

fn encode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

fn decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < b.len() => {
                match u8::from_str_radix(std::str::from_utf8(&b[i + 1..i + 3]).unwrap_or(""), 16) {
                    Ok(v) => {
                        out.push(v);
                        i += 2;
                    }
                    Err(_) => out.push(b'%'),
                }
            }
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    #[test]
    fn url_coding() {
        assert_eq!(super::encode("a b/c"), "a%20b%2Fc");
        assert_eq!(super::decode("a%20b%2Fc"), "a b/c");
        assert_eq!(super::decode("100%"), "100%");
        assert_eq!(
            super::normalize("pyo.example.com/").unwrap(),
            "https://pyo.example.com"
        );
    }
}
