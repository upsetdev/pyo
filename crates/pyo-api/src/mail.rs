//! Outgoing email through the installation's SMTP server, set up by an
//! admin in Settings → Email. The password is sealed like integration keys;
//! the dashboard only ever learns whether one is saved.
//!
//! Pyo mails team invites, sign-in links, security notices (two-factor
//! turned off, a new API key) and the settings page's test. Every email
//! shares one layout ([`Email::html`]): the logo, a card with a heading,
//! short paragraphs and one button, and a footer with the install's
//! address and why the person got it. A plain-text part goes with it.

use std::time::Duration;

use lettre::{
    AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor,
    message::{Attachment, Mailbox, MultiPart, SinglePart, header::ContentType},
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

/// Send one email: a plain-text part, and an HTML part with the logo.
pub async fn send(
    state: &AppState,
    cfg: &EmailSettings,
    to: &str,
    email: &Email,
) -> anyhow::Result<()> {
    let from = Mailbox::new(
        if cfg.from_name.trim().is_empty() {
            None
        } else {
            Some(cfg.from_name.trim().to_owned())
        },
        cfg.from_email.parse()?,
    );
    let msg = email.message(from, to.parse()?, &cfg.public_url)?;
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

/// The duck on a dark tile, 96px for a 32px spot. Sent inline (a CID part),
/// so it shows without loading anything: a Pyo on a private network has no
/// image URL a mail client could reach, and clients block remote images.
const LOGO: &[u8] = include_bytes!("../assets/email/logo.png");
const LOGO_CID: &str = "logo@pyo";

/// A part of a paragraph: plain text, or a value set in bold.
enum Seg {
    Text(String),
    Strong(String),
}

/// One email: a heading, short paragraphs and one button, in the shared
/// layout. The footer says which Pyo sent it and why.
pub struct Email {
    pub subject: String,
    heading: String,
    body: Vec<Vec<Seg>>,
    button: String,
    link: String,
    /// Why the person got it, at the bottom.
    reason: String,
}

impl Email {
    fn new(subject: impl Into<String>, heading: impl Into<String>) -> Self {
        Email {
            subject: subject.into(),
            heading: heading.into(),
            body: Vec::new(),
            button: String::new(),
            link: String::new(),
            reason: String::new(),
        }
    }

    fn para(mut self, segs: Vec<Seg>) -> Self {
        self.body.push(segs);
        self
    }

    fn button(mut self, label: &str, link: &str) -> Self {
        self.button = label.into();
        self.link = link.into();
        self
    }

    fn reason(mut self, reason: impl Into<String>) -> Self {
        self.reason = reason.into();
        self
    }

    fn para_text(segs: &[Seg]) -> String {
        segs.iter()
            .map(|s| match s {
                Seg::Text(t) | Seg::Strong(t) => t.as_str(),
            })
            .collect()
    }

    /// The plain-text part.
    pub fn text(&self, public_url: &str) -> String {
        let mut out = String::new();
        for p in &self.body {
            out.push_str(&Self::para_text(p));
            out.push_str("\n\n");
        }
        out.push_str(&format!("{}: {}\n\n", self.button, self.link));
        out.push_str(&format!(
            "{}\nSent by Pyo at {}\n",
            self.reason,
            site(public_url)
        ));
        out
    }

    /// The HTML part. Tables and inline styles for every client; a style
    /// block adds dark colors and phone padding where it's supported.
    pub fn html(&self, public_url: &str) -> String {
        let paras: String = self
            .body
            .iter()
            .map(|p| {
                let inner: String = p
                    .iter()
                    .map(|s| match s {
                        Seg::Text(t) => esc(t),
                        Seg::Strong(t) => format!(r#"<strong class="fg" style="color:{FG};font-weight:600">{}</strong>"#, esc(t)),
                    })
                    .collect();
                format!(r#"<p class="fg" style="margin:0 0 16px;font-size:15px;line-height:24px;color:{FG}">{inner}</p>"#)
            })
            .collect();
        let preview = self
            .body
            .first()
            .map(|p| Self::para_text(p))
            .unwrap_or_default();
        format!(
            r##"<!doctype html>
<html lang="en" xmlns:v="urn:schemas-microsoft-com:vml" xmlns:o="urn:schemas-microsoft-com:office:office">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<meta name="x-apple-disable-message-reformatting">
<meta name="format-detection" content="telephone=no,date=no,address=no,email=no,url=no">
<meta name="color-scheme" content="light dark">
<meta name="supported-color-schemes" content="light dark">
<title>{subject}</title>
<!--[if mso]><noscript><xml><o:OfficeDocumentSettings><o:PixelsPerInch>96</o:PixelsPerInch></o:OfficeDocumentSettings></xml></noscript><style>body,table,td,h1,p,a,strong{{font-family:'Segoe UI',Arial,sans-serif!important}}</style><![endif]-->
<style>
:root{{color-scheme:light dark;supported-color-schemes:light dark}}
body{{margin:0;padding:0;width:100%;-webkit-text-size-adjust:100%;-ms-text-size-adjust:100%}}
table{{border-collapse:separate}}
img{{border:0;outline:none;text-decoration:none}}
@media (max-width:600px){{
.outer{{padding:24px 12px!important}}
.px{{padding:28px 24px!important}}
}}
@media (prefers-color-scheme:dark){{
.bg{{background-color:{D_BG}!important}}
.frame{{background-color:{D_FRAME}!important;border-color:{D_FRAME_EDGE}!important}}
.card{{background-color:{D_CARD}!important;border-color:{D_CARD_EDGE}!important}}
.rule{{border-color:{D_RULE}!important}}
.fg{{color:{D_FG}!important}}
.muted{{color:{D_MUTED}!important}}
.link{{color:{D_LINK}!important}}
.btn{{background-color:{D_BLUE}!important;border-color:{D_BLUE}!important}}
}}
[data-ogsb] .bg{{background-color:{D_BG}!important}}
[data-ogsb] .frame{{background-color:{D_FRAME}!important}}
[data-ogsb] .card{{background-color:{D_CARD}!important}}
[data-ogsc] .fg{{color:{D_FG}!important}}
[data-ogsc] .muted{{color:{D_MUTED}!important}}
[data-ogsc] .link{{color:{D_LINK}!important}}
</style>
</head>
<body class="bg" style="margin:0;padding:0;background-color:{BG}">
<div style="display:none;max-height:0;max-width:0;overflow:hidden;opacity:0;mso-hide:all;font-size:1px;line-height:1px;color:{BG}">{preview}&#8199;&#847;&#8199;&#847;&#8199;&#847;&#8199;&#847;&#8199;&#847;&#8199;&#847;&#8199;&#847;&#8199;&#847;</div>
<table role="presentation" class="bg" width="100%" cellpadding="0" cellspacing="0" border="0" bgcolor="{BG}" style="background-color:{BG}">
<tr><td class="outer" align="center" style="padding:48px 16px">
<!--[if mso]><table role="presentation" width="560" align="center" cellpadding="0" cellspacing="0" border="0"><tr><td><![endif]-->
<table role="presentation" width="100%" cellpadding="0" cellspacing="0" border="0" style="max-width:560px;margin:0 auto;font-family:{FONT}">
<tr><td style="padding:0 8px 20px">
<table role="presentation" cellpadding="0" cellspacing="0" border="0"><tr>
<td style="padding:0 10px 0 0;vertical-align:middle"><img src="cid:{cid}" width="28" height="28" alt="" style="display:block;width:28px;height:28px;border:0;border-radius:7px"></td>
<td class="fg" style="vertical-align:middle;font-family:{FONT};font-size:18px;line-height:28px;font-weight:700;letter-spacing:-0.3px;color:{FG}">Pyo</td>
</tr></table>
</td></tr>
<tr><td class="frame" bgcolor="{FRAME}" style="background-color:{FRAME};border:1px solid {FRAME_EDGE};border-radius:20px;padding:6px">
<table role="presentation" class="card" width="100%" cellpadding="0" cellspacing="0" border="0" bgcolor="{CARD}" style="background-color:{CARD};border:1px solid {CARD_EDGE};border-radius:14px">
<tr><td class="px" style="padding:36px 40px;font-family:{FONT}">
<h1 class="fg" style="margin:0 0 16px;font-family:{FONT};font-size:22px;line-height:30px;font-weight:600;letter-spacing:-0.4px;color:{FG}">{heading}</h1>
{paras}<table role="presentation" cellpadding="0" cellspacing="0" border="0" style="margin:12px 0 0"><tr><td>{button}</td></tr></table>
<table role="presentation" width="100%" cellpadding="0" cellspacing="0" border="0" style="margin:32px 0 0"><tr><td class="rule" style="border-top:1px solid {RULE};padding:20px 0 0;font-family:{FONT};font-size:13px;line-height:20px">
<p class="muted" style="margin:0 0 4px;color:{MUTED}">Or open this link:</p>
<a class="link" href="{link}" style="color:{BLUE};text-decoration:none;word-break:break-all">{link}</a>
</td></tr></table>
</td></tr>
</table>
</td></tr>
<tr><td style="padding:20px 8px 0;font-family:{FONT};font-size:12px;line-height:19px">
<p class="muted" style="margin:0 0 6px;color:{MUTED}">{reason}</p>
<p class="muted" style="margin:0;color:{MUTED}">Sent by Pyo at <a class="link" href="{home}" style="color:{BLUE};text-decoration:none">{site}</a></p>
</td></tr>
</table>
<!--[if mso]></td></tr></table><![endif]-->
</td></tr>
</table>
</body>
</html>
"##,
            subject = esc(&self.subject),
            preview = esc(&preview),
            cid = LOGO_CID,
            heading = esc(&self.heading),
            button = button(&self.button, &self.link),
            link = esc(&self.link),
            reason = esc(&self.reason),
            home = esc(public_url),
            site = esc(&site(public_url)),
        )
    }

    /// The message: text and HTML as alternatives, the HTML with its logo.
    pub fn message(&self, from: Mailbox, to: Mailbox, public_url: &str) -> anyhow::Result<Message> {
        let logo = Attachment::new_inline(LOGO_CID.into())
            .body(LOGO.to_vec(), ContentType::parse("image/png")?);
        Ok(Message::builder()
            .from(from)
            .to(to)
            .subject(&self.subject)
            .multipart(
                MultiPart::alternative()
                    .singlepart(SinglePart::plain(self.text(public_url)))
                    .multipart(
                        MultiPart::related()
                            .singlepart(SinglePart::html(self.html(public_url)))
                            .singlepart(logo),
                    ),
            )?)
    }
}

// Colors from the dashboard's light and dark tokens (dash/src/styles.css).
const FONT: &str =
    "'Inter',-apple-system,BlinkMacSystemFont,'Segoe UI',Roboto,Helvetica,Arial,sans-serif";
const BG: &str = "#ffffff";
const FRAME: &str = "#f5f5f5";
const FRAME_EDGE: &str = "#ebebeb";
const CARD: &str = "#ffffff";
const CARD_EDGE: &str = "#dddddd";
const RULE: &str = "#e9e9e9";
const FG: &str = "#171717";
const MUTED: &str = "#666666";
const BLUE: &str = "#004dff";
const D_BG: &str = "#0b0b0b";
const D_FRAME: &str = "#131313";
const D_FRAME_EDGE: &str = "#1e1e1e";
const D_CARD: &str = "#0f0f0f";
const D_CARD_EDGE: &str = "#333333";
const D_RULE: &str = "#282828";
const D_FG: &str = "#f5f5f5";
const D_MUTED: &str = "#ababab";
const D_BLUE: &str = "#2465ff";
const D_LINK: &str = "#6b97ff";

/// A pill button that also works in Outlook for Windows, which draws the
/// VML shape instead (it ignores padding and rounded corners on links).
fn button(label: &str, link: &str) -> String {
    // Outlook's shape needs a width: about 8px a letter at 15px, plus padding.
    let width = label.chars().count() * 8 + 56;
    let (l, h) = (esc(label), esc(link));
    format!(
        r#"<!--[if mso]><v:roundrect xmlns:v="urn:schemas-microsoft-com:vml" xmlns:w="urn:schemas-microsoft-com:office:word" href="{h}" style="height:44px;v-text-anchor:middle;width:{width}px" arcsize="50%" stroke="f" fillcolor="{BLUE}"><w:anchorlock/><center style="color:#ffffff;font-family:'Segoe UI',Arial,sans-serif;font-size:15px;font-weight:600">{l}</center></v:roundrect><![endif]--><!--[if !mso]><!--><a class="btn" href="{h}" style="display:inline-block;background-color:{BLUE};border:1px solid {BLUE};border-radius:999px;color:#ffffff;font-family:{FONT};font-size:15px;font-weight:600;line-height:20px;padding:12px 24px;text-decoration:none;-webkit-text-size-adjust:none">{l}</a><!--<![endif]-->"#
    )
}

/// What the footer shows for the install's address: its host and path.
fn site(public_url: &str) -> String {
    let u = public_url.trim().trim_end_matches('/');
    u.strip_prefix("https://")
        .or_else(|| u.strip_prefix("http://"))
        .unwrap_or(u)
        .to_owned()
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

fn t(s: impl Into<String>) -> Seg {
    Seg::Text(s.into())
}

fn b(s: impl Into<String>) -> Seg {
    Seg::Strong(s.into())
}

/// A team invite.
pub fn invite(inviter: &str, team: &str, role: &str, link: &str, has_account: bool) -> Email {
    let subject = format!("{inviter} invited you to {team} on Pyo");
    let reason = "You got this because someone invited this address. If you didn't expect it, you can ignore it.";
    if has_account {
        Email::new(subject, format!("You're in {team}"))
            .para(vec![
                b(inviter),
                t(" added you to "),
                b(team),
                t(format!(
                    " as {}. It's in your team switcher the next time you open Pyo.",
                    article(role)
                )),
            ])
            .button("Open the team", link)
            .reason(reason)
    } else {
        Email::new(subject, format!("Join {team}"))
            .para(vec![
                b(inviter),
                t(" invited you to join "),
                b(team),
                t(format!(" on Pyo as {}.", article(role))),
            ])
            .para(vec![t(
                "Create your account with this email address to join.",
            )])
            .button("Accept invite", link)
            .reason(reason)
    }
}

/// The settings page's test message.
pub fn test(link: &str) -> Email {
    Email::new("Pyo test email", "Email works")
        .para(vec![t(
            "Pyo can send email through this SMTP server. Team invites and sign-in emails go out this way.",
        )])
        .button("Open Pyo", link)
        .reason("You got this because you sent a test from Pyo's email settings.")
}

/// A one-time sign-in link (recovery.rs). `owner`: the team owner who
/// reset the member's sign-in and their team, when it wasn't the person.
pub fn sign_in_link(link: &str, minutes: i32, owner: Option<(&str, &str)>) -> Email {
    let works = format!("It works once, for {minutes} minutes.");
    let e = Email::new("Choose a new Pyo password", "Choose a new password");
    match owner {
        Some((owner, team)) => e
            .para(vec![
                b(owner),
                t(" from "),
                b(team),
                t(format!(
                    " reset your sign-in. Open this link to choose a new password for Pyo. {works}"
                )),
            ])
            .button("Choose a new password", link)
            .reason(format!(
                "You got this because an owner of {team} reset your sign-in."
            )),
        None => e
            .para(vec![t(format!(
                "Open this link to choose a new password for Pyo. {works}"
            ))])
            .button("Choose a new password", link)
            .reason("If you didn't ask for this, you can ignore it. Your password stays as it is."),
    }
}

/// Two-factor was turned off on the account.
pub fn two_factor_off(link: &str) -> Email {
    Email::new("Two-factor authentication was turned off", "Two-factor authentication is off")
        .para(vec![t(
            "Two-factor authentication was turned off on your Pyo account. If that wasn't you, sign in, change your password and turn it on again.",
        )])
        .button("Open your profile", link)
        .reason("You get this email for every change to how you sign in.")
}

/// An admin sign-in with the right credentials, refused because of where
/// it came from (admin_sign_in.rs).
pub fn sign_in_refused(address: &str, link: &str) -> Email {
    Email::new("A Pyo sign-in was refused", "Sign-in refused")
        .para(vec![
            t("Someone used your sign-in details for your Pyo admin account from "),
            b(address),
            t(". Pyo refused it because admins can't sign in from there. If that wasn't you, change your password."),
        ])
        .button("Open security settings", link)
        .reason("You get this email for every refused sign-in to an admin account.")
}

/// A new API key was made on the account.
pub fn new_api_key(name: &str, link: &str) -> Email {
    Email::new("A new Pyo API key was made", "New API key")
        .para(vec![
            t("A new API key named "),
            b(name),
            t(" was made on your Pyo account. If that wasn't you, revoke it and change your password."),
        ])
        .button("See your API keys", link)
        .reason("You get this email for every new API key.")
}

fn article(role: &str) -> String {
    match role {
        "admin" => "an admin".into(),
        other => format!("a {other}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> EmailSettings {
        EmailSettings {
            public_url: "https://pyo.example.com/".into(),
            ..Default::default()
        }
    }

    /// Every email Pyo sends, with links made the way the callers make them.
    fn all(inviter: &str, team: &str, key: &str) -> Vec<(&'static str, Email)> {
        let c = cfg();
        vec![
            (
                "invite",
                invite(inviter, team, "member", &c.link("/invite/abc"), false),
            ),
            (
                "invite-existing",
                invite(inviter, team, "admin", &c.link("/acme"), true),
            ),
            ("test", test(&c.link("/"))),
            (
                "sign-in-link",
                sign_in_link(&c.link("/reset-password?token=t1"), 30, None),
            ),
            (
                "sign-in-reset",
                sign_in_link(
                    &c.link("/reset-password?token=t2"),
                    30,
                    Some((inviter, team)),
                ),
            ),
            ("two-factor-off", two_factor_off(&c.link("/profile"))),
            ("new-api-key", new_api_key(key, &c.link("/api-keys"))),
            ("sign-in-refused", sign_in_refused(key, &c.link("/security"))),
        ]
    }

    #[test]
    fn html_escapes_values() {
        let evil = r#"<script>alert("x")</script> & 'y'"#;
        for (name, e) in all(evil, evil, evil) {
            let html = e.html(&cfg().public_url);
            assert!(!html.contains("<script>"), "{name}: raw tag in the HTML");
            assert!(!html.contains(r#"("x")"#), "{name}: raw quotes in the HTML");
            if name != "test" && name != "two-factor-off" && name != "sign-in-link" {
                assert!(
                    html.contains(
                        "&lt;script&gt;alert(&quot;x&quot;)&lt;/script&gt; &amp; &#39;y&#39;"
                    ),
                    "{name}: value not escaped"
                );
            }
            // The text part keeps values as they are.
            if name.starts_with("invite") {
                assert!(e.text(&cfg().public_url).contains(evil), "{name}");
            }
        }
        let html = test(r#"https://x.example/?a=1&b="2""#).html("https://x.example");
        assert!(html.contains(r#"href="https://x.example/?a=1&amp;b=&quot;2&quot;""#));
    }

    #[test]
    fn every_email_has_text_and_html() {
        for (name, e) in all("Ana", "Acme", "deploy") {
            let msg = e
                .message(
                    "Pyo <pyo@example.com>".parse().unwrap(),
                    "a@example.com".parse().unwrap(),
                    &cfg().public_url,
                )
                .unwrap();
            let raw = String::from_utf8(msg.formatted()).unwrap();
            for part in [
                "multipart/alternative",
                "multipart/related",
                "Content-Type: text/plain",
                "Content-Type: text/html",
                "Content-Type: image/png",
                "Content-ID: <logo@pyo>",
            ] {
                assert!(raw.contains(part), "{name}: no {part}");
            }
            let text = e.text(&cfg().public_url);
            assert!(
                text.contains(&e.button) && text.contains(&e.link),
                "{name}: no link in the text"
            );
            assert!(
                e.html(&cfg().public_url).contains("cid:logo@pyo"),
                "{name}: no logo"
            );
        }
    }

    #[test]
    fn links_use_public_url() {
        for (name, e) in all("Ana", "Acme", "deploy") {
            let (text, html) = (e.text(&cfg().public_url), e.html(&cfg().public_url));
            assert!(
                e.link.starts_with("https://pyo.example.com/"),
                "{name}: {}",
                e.link
            );
            assert!(
                html.contains(&format!(r#"href="{}""#, esc(&e.link))),
                "{name}: button"
            );
            assert!(
                html.contains(r#"href="https://pyo.example.com/""#),
                "{name}: footer link"
            );
            assert!(html.contains("Sent by Pyo at <a"), "{name}: footer");
            assert!(
                text.contains("Sent by Pyo at pyo.example.com\n"),
                "{name}: text footer"
            );
            for s in [&text, &html] {
                assert!(!s.contains("Pyo Cloud"), "{name}");
            }
        }
    }

    #[test]
    fn copy_is_plain() {
        for (name, e) in all("Ana", "Acme", "deploy") {
            let text = e.text(&cfg().public_url);
            let words = text.replace(&e.link, "");
            for bad in [";", " - ", " — ", " – ", "node", " app "] {
                assert!(!words.contains(bad), "{name}: {bad:?} in {words}");
            }
            assert!(!e.reason.is_empty() && !e.button.is_empty(), "{name}");
        }
    }

    /// Writes every email's HTML and text to $PYO_EMAIL_PREVIEWS, the logo
    /// inlined so a browser shows it: `PYO_EMAIL_PREVIEWS=dir cargo test
    /// -p pyo-api mail::tests::write_previews -- --ignored`.
    #[test]
    #[ignore]
    fn write_previews() {
        use base64::Engine;
        let dir = std::path::PathBuf::from(
            std::env::var("PYO_EMAIL_PREVIEWS").expect("PYO_EMAIL_PREVIEWS"),
        );
        std::fs::create_dir_all(&dir).unwrap();
        let logo = format!(
            "data:image/png;base64,{}",
            base64::engine::general_purpose::STANDARD.encode(LOGO)
        );
        for (name, e) in all("Ana Lima", "Acme Studio", "deploy-bot") {
            let html = e.html(&cfg().public_url).replace("cid:logo@pyo", &logo);
            std::fs::write(dir.join(format!("{name}.html")), html).unwrap();
            std::fs::write(
                dir.join(format!("{name}.txt")),
                format!("Subject: {}\n\n{}", e.subject, e.text(&cfg().public_url)),
            )
            .unwrap();
        }
    }
}
