//! Outgoing email through the installation's SMTP server, set up by an
//! admin in Settings → Email. The password is sealed like integration keys;
//! the dashboard only ever learns whether one is saved.
//!
//! Team invites are the first sender: an invite mails a link to sign up (or,
//! for an existing account, to the team).

use std::time::Duration;

use lettre::{
    AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor,
    message::{Mailbox, MultiPart},
    transport::smtp::authentication::Credentials,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::PgPool;

use crate::{
    AppState,
    error::{ApiError, ApiResult},
};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct EmailSettings {
    pub host: String,
    pub port: u16,
    /// "starttls" (usually 587), "tls" (465) or "none" (a local relay).
    pub security: String,
    pub username: String,
    /// Sealed with the installation's secret key.
    pub password_sealed: Option<String>,
    pub from_email: String,
    pub from_name: String,
    /// Where people open the dashboard, for links in emails.
    pub public_url: String,
}

impl EmailSettings {
    /// For the dashboard: everything but the password.
    pub fn public(&self) -> Value {
        json!({
            "host": self.host, "port": self.port, "security": self.security, "username": self.username,
            "passwordSet": self.password_sealed.is_some(), "fromEmail": self.from_email, "fromName": self.from_name,
            "publicUrl": self.public_url,
        })
    }

    pub fn link(&self, path: &str) -> String {
        format!("{}{path}", self.public_url.trim_end_matches('/'))
    }
}

pub async fn settings(db: &PgPool) -> ApiResult<Option<EmailSettings>> {
    let v: Option<Value> = sqlx::query_scalar("SELECT value FROM pyo.settings WHERE key = 'email'")
        .fetch_optional(db)
        .await?;
    Ok(v.and_then(|v| serde_json::from_value(v).ok()))
}

pub fn validate(s: &EmailSettings) -> ApiResult<()> {
    if s.host.trim().is_empty() || s.host.contains(char::is_whitespace) {
        return Err(ApiError::bad_request("Enter the SMTP server's host name."));
    }
    if s.port == 0 {
        return Err(ApiError::bad_request("Enter the SMTP port, usually 587."));
    }
    if !matches!(s.security.as_str(), "starttls" | "tls" | "none") {
        return Err(ApiError::bad_request("Pick STARTTLS, TLS or none."));
    }
    if s.from_email.parse::<lettre::Address>().is_err() {
        return Err(ApiError::bad_request("Enter a valid From address."));
    }
    if !(s.public_url.starts_with("http://") || s.public_url.starts_with("https://")) {
        return Err(ApiError::bad_request(
            "The dashboard URL starts with http:// or https://.",
        ));
    }
    Ok(())
}

/// Send one email with a plain-text and an HTML part.
pub async fn send(
    state: &AppState,
    cfg: &EmailSettings,
    to: &str,
    subject: &str,
    text: String,
    html: String,
) -> anyhow::Result<()> {
    let from = Mailbox::new(
        if cfg.from_name.trim().is_empty() {
            None
        } else {
            Some(cfg.from_name.trim().to_owned())
        },
        cfg.from_email.parse()?,
    );
    let msg = Message::builder()
        .from(from)
        .to(to.parse()?)
        .subject(subject)
        .multipart(MultiPart::alternative_plain_html(text, html))?;
    let host = cfg.host.trim();
    let mut b = match cfg.security.as_str() {
        "tls" => AsyncSmtpTransport::<Tokio1Executor>::relay(host)?,
        "starttls" => AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(host)?,
        _ => AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous(host),
    }
    .port(cfg.port)
    .timeout(Some(Duration::from_secs(15)));
    if !cfg.username.is_empty() {
        let password = cfg
            .password_sealed
            .as_deref()
            .map(|p| state.sealer.open(p))
            .transpose()?
            .unwrap_or_default();
        b = b.credentials(Credentials::new(cfg.username.clone(), password));
    }
    b.build().send(msg).await?;
    Ok(())
}

/// A short, plain message: a heading, a paragraph and one button.
fn layout(heading: &str, body: &str, button: &str, link: &str, footer: &str) -> String {
    let esc = |s: &str| {
        s.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
            .replace('"', "&quot;")
    };
    format!(
        r#"<!doctype html><html><body style="margin:0;background:#f6f6f7;font-family:-apple-system,BlinkMacSystemFont,'Segoe UI',Roboto,sans-serif;color:#18181b">
<table role="presentation" width="100%" cellpadding="0" cellspacing="0"><tr><td align="center" style="padding:40px 16px">
<table role="presentation" width="100%" cellpadding="0" cellspacing="0" style="max-width:480px;background:#ffffff;border:1px solid #e4e4e7;border-radius:12px">
<tr><td style="padding:32px">
<div style="width:28px;height:28px;border-radius:7px;background:#2465ff;color:#fff;font-weight:700;font-size:15px;line-height:28px;text-align:center">D</div>
<h1 style="margin:24px 0 8px;font-size:20px;line-height:28px">{h}</h1>
<p style="margin:0 0 24px;font-size:14px;line-height:22px;color:#52525b">{b}</p>
<a href="{l}" style="display:inline-block;background:#2465ff;color:#ffffff;text-decoration:none;font-size:14px;font-weight:600;padding:10px 18px;border-radius:8px">{btn}</a>
<p style="margin:24px 0 0;font-size:12px;line-height:18px;color:#71717a">Or open this link: <a href="{l}" style="color:#2465ff;word-break:break-all">{l}</a></p>
</td></tr></table>
<p style="margin:16px 0 0;font-size:12px;color:#a1a1aa">{f}</p>
</td></tr></table></body></html>"#,
        h = esc(heading),
        b = esc(body),
        l = esc(link),
        btn = esc(button),
        f = esc(footer),
    )
}

/// (subject, text, html) of a team invite.
pub fn invite(
    inviter: &str,
    team: &str,
    role: &str,
    link: &str,
    has_account: bool,
) -> (String, String, String) {
    let subject = format!("{inviter} invited you to {team} on Pyo");
    let body = if has_account {
        format!(
            "{inviter} added you to {team} as {}. It's in your team switcher the next time you open Pyo.",
            article(role)
        )
    } else {
        format!(
            "{inviter} invited you to join {team} on Pyo as {}. Create your account with this email address to join.",
            article(role)
        )
    };
    let button = if has_account {
        "Open the team"
    } else {
        "Accept invite"
    };
    let footer = "You got this because someone invited this address. If you didn't expect it, you can ignore it.";
    let text = format!("{body}\n\n{button}: {link}\n\n{footer}\n");
    (
        subject,
        text,
        layout(&format!("Join {team}"), &body, button, link, footer),
    )
}

/// (subject, text, html) of the settings page's test message.
pub fn test(link: &str) -> (String, String, String) {
    let body = "Pyo can send email through this SMTP server. Team invites go out this way now.";
    let footer = "Sent by Pyo, from its email settings.";
    (
        "Pyo test email".into(),
        format!("{body}\n\n{link}\n"),
        layout("Email works", body, "Open Pyo", link, footer),
    )
}

fn article(role: &str) -> String {
    match role {
        "admin" => "an admin".into(),
        other => format!("a {other}"),
    }
}
