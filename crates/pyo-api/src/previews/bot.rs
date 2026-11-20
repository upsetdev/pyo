//! What the bot says on a pull request, worked out from the previews alone
//! (no calls here): the comment, the check, each deployment's state.

use serde_json::{Value, json};

/// A preview as people see it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shown {
    /// Over the service's limit: starts when another preview goes.
    Waiting,
    Building,
    Ready,
    Failed,
    /// Its copy is going.
    Removing,
    Removed,
}

impl Shown {
    /// From a preview's state and its latest deployment's status.
    pub fn of(state: &str, deployment: Option<&str>, copy_left: bool) -> Shown {
        match state {
            "waiting" => Shown::Waiting,
            "failed" => Shown::Failed,
            "removed" if copy_left => Shown::Removing,
            "removed" => Shown::Removed,
            _ => match deployment {
                Some("succeeded") => Shown::Ready,
                Some("failed" | "cancelled") => Shown::Failed,
                // Queued, building, rolling out, or not deployed yet.
                _ => Shown::Building,
            },
        }
    }

    /// The dashboard's word (`Preview.status`).
    pub fn key(self) -> &'static str {
        match self {
            Shown::Waiting => "waiting",
            Shown::Building => "building",
            Shown::Ready => "ready",
            Shown::Failed => "failed",
            Shown::Removing => "removing",
            Shown::Removed => "removed",
        }
    }

    pub fn word(self) -> &'static str {
        match self {
            Shown::Waiting => "Waiting",
            Shown::Building => "Building",
            Shown::Ready => "Ready",
            Shown::Failed => "Failed",
            Shown::Removing | Shown::Removed => "Removed",
        }
    }

    fn gone(self) -> bool {
        matches!(self, Shown::Removing | Shown::Removed)
    }

    /// GitHub's deployment status for it.
    pub fn github_deployment(self) -> &'static str {
        match self {
            Shown::Waiting => "queued",
            Shown::Building => "in_progress",
            Shown::Ready => "success",
            Shown::Failed => "failure",
            Shown::Removing | Shown::Removed => "inactive",
        }
    }

    /// GitLab's deployment status for it (none while it waits).
    pub fn gitlab_deployment(self) -> Option<&'static str> {
        match self {
            Shown::Waiting => None,
            Shown::Building => Some("running"),
            Shown::Ready => Some("success"),
            Shown::Failed => Some("failed"),
            Shown::Removing | Shown::Removed => Some("canceled"),
        }
    }
}

/// One line of the bot's table.
#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    pub service: String,
    pub shown: Shown,
    /// https://… when it has a URL.
    pub url: Option<String>,
    /// Why it waits, failed or was removed.
    pub message: Option<String>,
    /// When it last changed (ms).
    pub updated_ms: i64,
    /// Its deployment in the dashboard.
    pub logs: Option<String>,
}

/// The hidden mark that finds the bot's comment again.
pub fn marker(team_id: &str) -> String {
    format!("<!-- pyo-previews:{team_id} -->")
}

/// "Nov 20, 14:05 UTC".
fn when(ms: i64) -> String {
    match jiff::Timestamp::from_millisecond(ms) {
        Ok(t) => t.strftime("%b %-d, %H:%M UTC").to_string(),
        Err(_) => String::new(),
    }
}

/// Text that can't break the table: no pipes, no new lines, kept short.
fn cell(s: &str) -> String {
    let one: String = s
        .replace(['\r', '\n'], " ")
        .replace('|', "\\|")
        .chars()
        .take(160)
        .collect();
    one.trim().to_owned()
}

/// The comment: a table of the previews, or "Preview removed" once the pull
/// request closed or none is left.
pub fn comment(team_id: &str, rows: &[Row], open: bool) -> String {
    let mut out = format!("{}\n", marker(team_id));
    let live: Vec<&Row> = rows.iter().filter(|r| !r.shown.gone()).collect();
    if !open || live.is_empty() {
        out.push_str("**Preview removed**\n");
        if !rows.is_empty() {
            let names: Vec<String> = rows
                .iter()
                .map(|r| format!("`{}`", cell(&r.service)))
                .collect();
            out.push_str(&format!("\nServices: {}\n", names.join(", ")));
        }
        return out;
    }
    out.push_str(
        "**Preview**\n\n| Service | Status | URL | Updated | |\n| :-- | :-- | :-- | :-- | :-- |\n",
    );
    for r in live {
        let status = match (&r.message, r.shown) {
            (Some(m), Shown::Failed | Shown::Waiting) if !m.is_empty() => {
                format!("{} ({})", r.shown.word(), cell(m))
            }
            _ => r.shown.word().to_owned(),
        };
        let url = match (&r.url, r.shown) {
            (Some(u), Shown::Ready) => format!("[{}]({u})", u.trim_start_matches("https://")),
            (Some(u), _) => u.trim_start_matches("https://").to_owned(),
            (None, _) => String::new(),
        };
        let logs = r
            .logs
            .as_ref()
            .map(|l| format!("[Logs]({l})"))
            .unwrap_or_default();
        out.push_str(&format!(
            "| {} | {status} | {url} | {} | {logs} |\n",
            cell(&r.service),
            when(r.updated_ms)
        ));
    }
    out
}

/// The check run ("Pyo · Preview") for the head: GitHub's status and
/// conclusion, with a title and a summary.
#[derive(Debug, Clone, PartialEq)]
pub struct Check {
    /// "in_progress" or "completed".
    pub status: &'static str,
    /// "success", "failure" or "neutral" once completed.
    pub conclusion: Option<&'static str>,
    pub title: String,
    pub summary: String,
}

pub const CHECK_NAME: &str = "Pyo · Preview";

pub fn check(rows: &[Row]) -> Check {
    let live: Vec<&Row> = rows.iter().filter(|r| !r.shown.gone()).collect();
    let count = |s: Shown| live.iter().filter(|r| r.shown == s).count();
    let (building, failed, ready) = (
        count(Shown::Building),
        count(Shown::Failed),
        count(Shown::Ready),
    );
    let (status, conclusion, title) = if building > 0 {
        ("in_progress", None, "Building".to_owned())
    } else if failed > 0 {
        let names: Vec<&str> = live
            .iter()
            .filter(|r| r.shown == Shown::Failed)
            .map(|r| r.service.as_str())
            .collect();
        (
            "completed",
            Some("failure"),
            format!("Failed: {}", names.join(", ")),
        )
    } else if ready > 0 {
        ("completed", Some("success"), "Ready".to_owned())
    } else if live.is_empty() {
        ("completed", Some("neutral"), "Removed".to_owned())
    } else {
        ("completed", Some("neutral"), "Waiting".to_owned())
    };
    let summary = live
        .iter()
        .map(|r| {
            let mut line = format!("- {}: {}", r.service, r.shown.word());
            if let Some(u) = &r.url {
                line.push_str(&format!(" ({u})"));
            }
            if let Some(m) = r.message.as_ref().filter(|m| !m.is_empty())
                && matches!(r.shown, Shown::Failed | Shown::Waiting)
            {
                line.push_str(&format!(". {}", m.trim_end_matches('.')));
            }
            line
        })
        .collect::<Vec<_>>()
        .join("\n");
    Check {
        status,
        conclusion,
        title,
        summary,
    }
}

impl Check {
    /// GitHub's check run body (`details_url`: the first logs link).
    pub fn github(&self, sha: &str, details: Option<&str>) -> Value {
        let mut v = json!({
            "name": CHECK_NAME,
            "head_sha": sha,
            "status": self.status,
            "output": { "title": self.title, "summary": if self.summary.is_empty() { "No previews." } else { &self.summary } },
        });
        if let Some(c) = self.conclusion {
            v["conclusion"] = json!(c);
        }
        if let Some(d) = details {
            v["details_url"] = json!(d);
        }
        v
    }

    /// GitLab's commit status state.
    pub fn gitlab_state(&self) -> &'static str {
        match (self.status, self.conclusion) {
            ("in_progress", _) => "running",
            (_, Some("success")) => "success",
            (_, Some("failure")) => "failed",
            _ => "skipped",
        }
    }
}

/// A short hash of what's said, to say it only when it changed.
pub fn hash(v: &str) -> String {
    crate::ids::sha256_hex(v)[..16].to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(service: &str, shown: Shown) -> Row {
        Row {
            service: service.into(),
            shown,
            url: Some(format!("https://{service}-pr-42.203.0.113.7.nip.io")),
            message: None,
            updated_ms: 1_795_000_000_000,
            logs: Some(format!(
                "https://pyo.example.com/acme/deployments/dpl_{service}"
            )),
        }
    }

    #[test]
    fn statuses_from_deployments() {
        assert_eq!(Shown::of("active", None, true), Shown::Building);
        assert_eq!(Shown::of("active", Some("queued"), true), Shown::Building);
        assert_eq!(Shown::of("active", Some("building"), true), Shown::Building);
        assert_eq!(Shown::of("active", Some("succeeded"), true), Shown::Ready);
        assert_eq!(Shown::of("active", Some("failed"), true), Shown::Failed);
        assert_eq!(Shown::of("active", Some("cancelled"), true), Shown::Failed);
        assert_eq!(Shown::of("failed", None, false), Shown::Failed);
        assert_eq!(Shown::of("waiting", None, false), Shown::Waiting);
        assert_eq!(
            Shown::of("removed", Some("succeeded"), true),
            Shown::Removing
        );
        assert_eq!(
            Shown::of("removed", Some("succeeded"), false),
            Shown::Removed
        );
    }

    #[test]
    fn deployment_states_on_each_provider() {
        assert_eq!(Shown::Building.github_deployment(), "in_progress");
        assert_eq!(Shown::Ready.github_deployment(), "success");
        assert_eq!(Shown::Failed.github_deployment(), "failure");
        assert_eq!(Shown::Removed.github_deployment(), "inactive");
        assert_eq!(Shown::Waiting.github_deployment(), "queued");
        assert_eq!(Shown::Building.gitlab_deployment(), Some("running"));
        assert_eq!(Shown::Ready.gitlab_deployment(), Some("success"));
        assert_eq!(Shown::Failed.gitlab_deployment(), Some("failed"));
        assert_eq!(Shown::Removing.gitlab_deployment(), Some("canceled"));
        assert_eq!(Shown::Waiting.gitlab_deployment(), None);
    }

    #[test]
    fn the_comment_is_a_table() {
        let mut failed = row("api", Shown::Failed);
        failed.message = Some("The build failed | exit 1\nsee logs".into());
        let c = comment(
            "team_1",
            &[row("web", Shown::Ready), failed, row("old", Shown::Removed)],
            true,
        );
        assert!(c.starts_with("<!-- pyo-previews:team_1 -->\n"), "{c}");
        assert!(c.contains("| web | Ready | [web-pr-42.203.0.113.7.nip.io](https://web-pr-42.203.0.113.7.nip.io) |"), "{c}");
        // A message can't break the table.
        assert!(
            c.contains("| api | Failed (The build failed \\| exit 1 see logs) |"),
            "{c}"
        );
        assert!(
            c.contains("[Logs](https://pyo.example.com/acme/deployments/dpl_web)"),
            "{c}"
        );
        assert!(!c.contains("| old |"), "removed ones aren't listed: {c}");
        assert!(c.contains("Nov "), "{c}");
        assert_eq!(
            c.lines().filter(|l| l.starts_with("| ")).count(),
            4,
            "header, rule and two rows"
        );
    }

    #[test]
    fn closed_says_removed() {
        let rows = [row("web", Shown::Ready), row("api", Shown::Building)];
        let c = comment("team_1", &rows, false);
        assert!(c.contains("**Preview removed**"), "{c}");
        assert!(c.contains("`web`, `api`"), "{c}");
        assert!(!c.contains("| web |"), "{c}");
        // All removed while open: the same.
        let gone = [row("web", Shown::Removed)];
        assert!(comment("team_1", &gone, true).contains("**Preview removed**"));
    }

    #[test]
    fn check_states() {
        let c = check(&[row("web", Shown::Ready), row("api", Shown::Building)]);
        assert_eq!((c.status, c.conclusion), ("in_progress", None));
        assert_eq!(c.gitlab_state(), "running");
        let c = check(&[row("web", Shown::Ready), row("api", Shown::Failed)]);
        assert_eq!((c.status, c.conclusion), ("completed", Some("failure")));
        assert_eq!(c.title, "Failed: api");
        assert_eq!(c.gitlab_state(), "failed");
        let c = check(&[row("web", Shown::Ready)]);
        assert_eq!(c.conclusion, Some("success"));
        assert_eq!(c.gitlab_state(), "success");
        let c = check(&[row("web", Shown::Waiting)]);
        assert_eq!(c.conclusion, Some("neutral"));
        assert_eq!(c.gitlab_state(), "skipped");
        let body = check(&[row("web", Shown::Ready)]).github("abc", Some("https://x"));
        assert_eq!(body["name"], "Pyo · Preview");
        assert_eq!(body["conclusion"], "success");
        assert_eq!(body["details_url"], "https://x");
        let building = check(&[row("web", Shown::Building)]).github("abc", None);
        assert!(building.get("conclusion").is_none());
    }
}
