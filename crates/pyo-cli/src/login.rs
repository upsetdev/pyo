//! `pyo login`: approve the CLI in the browser with the code it shows, or
//! pass an API key for CI and machines without one.

use std::time::Duration;

use crate::{api::Api, config::Credentials, ui};
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

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
    crate::upgrade::warn_if_pyo_is_older(&url).await;
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

/// POST JSON to Pyo: its answer, or the error it gives.
async fn post(http: &reqwest::Client, url: &str, body: Value) -> Result<Value> {
    let res = http.post(url).json(&body).send().await?;
    let ok = res.status().is_success();
    let v: Value = res.json().await.unwrap_or(Value::Null);
    if !ok {
        bail!(
            "{}",
            v["error"]
                .as_str()
                .unwrap_or("Sign-in failed. Run pyo login again.")
        );
    }
    Ok(v)
}

/// Ask Pyo for a sign-in, show its code, open the approval page and wait
/// for the answer there. Nothing comes back from the browser: the CLI
/// polls Pyo with a verifier only it knows, so this works over SSH too.
/// The key expires after `expires` ("30d", "90d", "1y" or "never").
async fn browser_login(url: &str, expires: &str) -> Result<String> {
    let http = reqwest::Client::builder()
        .user_agent(concat!("pyo-cli/", env!("CARGO_PKG_VERSION")))
        .timeout(Duration::from_secs(20))
        .build()?;
    let verifier = random_hex(32);
    let started = post(
        &http,
        &format!("{url}/api/cli/start"),
        json!({
            "challenge": hex::encode(Sha256::digest(verifier.as_bytes())),
            "name": machine_name(),
            "client": format!("pyo {} on {}", env!("CARGO_PKG_VERSION"), std::env::consts::OS),
            "expires": expires,
        }),
    )
    .await
    .map_err(|e| match e.downcast_ref::<reqwest::Error>() {
        Some(_) => anyhow::anyhow!("Can't reach {url}"),
        None => e,
    })?;
    let (Some(id), Some(code)) = (started["id"].as_str(), started["code"].as_str()) else {
        bail!("{url} didn't answer like a Pyo.");
    };
    let wait = Duration::from_secs(started["expiresIn"].as_u64().unwrap_or(600));
    let every = Duration::from_secs(started["interval"].as_u64().unwrap_or(2).clamp(1, 10));
    let page = format!("{url}/cli-login?id={}", encode(id));
    println!("Your code: {}", ui::bold(code));
    println!("Type it in your browser to approve this computer.");
    println!("If it doesn't open, go to:\n  {page}\n");
    ui::open(&page);

    let deadline = tokio::time::Instant::now() + wait;
    while tokio::time::Instant::now() < deadline {
        tokio::time::sleep(every).await;
        let res = match http
            .post(format!("{url}/api/cli/token"))
            .json(&json!({ "id": id, "verifier": verifier }))
            .send()
            .await
        {
            Ok(r) => r,
            // Offline for a moment: ask again.
            Err(_) => continue,
        };
        let status = res.status();
        let v: Value = res.json().await.unwrap_or(Value::Null);
        if let Some(token) = v["token"].as_str() {
            return Ok(token.to_owned());
        }
        if status.is_success() || status.as_u16() == 429 || status.is_server_error() {
            continue;
        }
        bail!(
            "{}",
            v["error"]
                .as_str()
                .unwrap_or("Sign-in failed. Run pyo login again.")
        );
    }
    bail!("This sign-in expired. Run pyo login again, or use: pyo login --token pyo_…")
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

#[cfg(test)]
mod tests {
    #[test]
    fn url_coding() {
        assert_eq!(super::encode("a b/c"), "a%20b%2Fc");
        assert_eq!(super::encode("clg_x-1"), "clg_x-1");
        assert_eq!(
            super::normalize("pyo.example.com/").unwrap(),
            "https://pyo.example.com"
        );
    }
}
