//! The source of a build: a temporary checkout of the repository at the
//! commit, and how to build it. A Dockerfile (or a Containerfile) in the
//! repository wins; otherwise Railpack works out the language and framework (Node.js and
//! Next.js, Go, Python, PHP, Ruby, Java, Rust, static sites, …) and writes a
//! build plan that BuildKit runs through Railpack's frontend.
//!
//! The checkout lives under the system temp directory and is deleted when
//! the build ends, whatever the outcome.

use std::{
    path::{Component, Path, PathBuf},
    process::Stdio,
    time::Duration,
};

use anyhow::{Context, bail};
use serde_json::Value;
use tokio::process::Command;

/// Railpack's BuildKit frontend; must match the railpack CLI's version.
pub fn railpack_frontend() -> String {
    std::env::var("RAILPACK_FRONTEND")
        .unwrap_or_else(|_| "ghcr.io/railwayapp/railpack-frontend:v0.40.1".into())
}

/// A directory for one build, removed on drop.
pub struct Workspace {
    pub root: PathBuf,
}

impl Workspace {
    pub fn new(job_id: &str) -> anyhow::Result<Self> {
        let safe: String = job_id
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
            .collect();
        let root = std::env::temp_dir().join(format!("pyo-build-{safe}"));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("src"))?;
        std::fs::create_dir_all(root.join("plan"))?;
        Ok(Self { root })
    }
    pub fn src(&self) -> PathBuf {
        self.root.join("src")
    }
    pub fn plan(&self) -> PathBuf {
        self.root.join("plan")
    }
}

impl Drop for Workspace {
    fn drop(&mut self) {
        if let Err(e) = std::fs::remove_dir_all(&self.root) {
            tracing::warn!(path = %self.root.display(), error = %e, "couldn't remove build workspace");
        }
    }
}

async fn git(dir: &Path, args: &[&str], auth: Option<&str>) -> anyhow::Result<()> {
    let mut cmd = Command::new("git");
    // A token travels as an HTTP header set through the environment, so it's
    // never in the remote URL, the arguments or git's error messages.
    if let Some(header) = auth {
        cmd.env("GIT_CONFIG_COUNT", "1")
            .env("GIT_CONFIG_KEY_0", "http.extraHeader")
            .env("GIT_CONFIG_VALUE_0", header);
    }
    let out = tokio::time::timeout(
        Duration::from_secs(300),
        cmd.args(args)
            .current_dir(dir)
            // Never wait for a username or password prompt.
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_ASKPASS", "true")
            .stdin(Stdio::null())
            .output(),
    )
    .await
    .context("git took longer than 5 minutes")?
    .context("couldn't run git")?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        let last = err
            .lines()
            .rev()
            .find(|l| !l.trim().is_empty())
            .unwrap_or("git failed")
            .trim();
        bail!("{}", last.trim_start_matches("fatal: "));
    }
    Ok(())
}

/// Check out `commit` of `repo` into `dir`. Fetches only that commit when
/// the server allows it (GitHub, GitLab, Gitea do); otherwise the branch.
/// With a `subdir` (a service inside a monorepo), only that folder's files are
/// downloaded. `token`: a GitHub App installation token, for private repositories.
pub async fn checkout(
    repo: &str,
    branch: &str,
    commit: &str,
    subdir: &str,
    dir: &Path,
    token: Option<&str>,
) -> anyhow::Result<()> {
    use base64::Engine;
    let header = token.map(|t| {
        format!(
            "Authorization: Basic {}",
            base64::engine::general_purpose::STANDARD.encode(format!("x-access-token:{t}"))
        )
    });
    let auth = header.as_deref();
    git(dir, &["init", "-q"], None).await?;
    git(dir, &["remote", "add", "origin", repo], None).await?;
    let sub = subdir.trim().trim_matches('/').trim_start_matches("./");
    let sparse = !sub.is_empty() && sub != ".";
    if sparse {
        // A partial clone: trees now, file contents only for the folder.
        git(dir, &["sparse-checkout", "set", "--cone", sub], None).await?;
        git(dir, &["config", "remote.origin.promisor", "true"], None).await?;
        git(
            dir,
            &["config", "remote.origin.partialclonefilter", "blob:none"],
            None,
        )
        .await?;
    }
    let shallow: &[&str] = if sparse {
        &[
            "fetch",
            "-q",
            "--depth",
            "1",
            "--filter=blob:none",
            "origin",
            commit,
        ]
    } else {
        &["fetch", "-q", "--depth", "1", "origin", commit]
    };
    if git(dir, shallow, auth).await.is_err()
        && git(
            dir,
            &["fetch", "-q", "--depth", "1", "origin", commit],
            auth,
        )
        .await
        .is_err()
    {
        git(dir, &["fetch", "-q", "origin", branch], auth)
            .await
            .with_context(|| format!("couldn't fetch {repo}"))?;
    }
    // Checking out downloads the folder's files, so it may need the token too.
    git(
        dir,
        &["-c", "advice.detachedHead=false", "checkout", "-q", commit],
        auth,
    )
    .await
    .with_context(|| format!("commit {commit} isn't in {repo}"))?;
    Ok(())
}

/// `rel` inside `base`, refusing absolute paths, "..", and a symbolic link
/// in the repository that leads outside it (to the builder's own files).
pub fn inside(base: &Path, rel: &str) -> anyhow::Result<PathBuf> {
    let rel = rel.trim().trim_start_matches("./");
    let p = Path::new(rel);
    if p.components().any(|c| {
        matches!(
            c,
            Component::ParentDir | Component::RootDir | Component::Prefix(_)
        )
    }) {
        bail!("{rel} points outside the repository");
    }
    let path = if rel.is_empty() || rel == "." {
        base.to_path_buf()
    } else {
        base.join(p)
    };
    if !stays_inside(base, &path) {
        bail!("{rel} is a symbolic link to outside the repository");
    }
    Ok(path)
}

/// Whether `path`, with every symbolic link on the way followed, is still
/// in `base`. One that isn't there (nothing to follow) is.
fn stays_inside(base: &Path, path: &Path) -> bool {
    let Ok(root) = base.canonicalize() else {
        return true;
    };
    match path.canonicalize() {
        Ok(real) => real.starts_with(&root),
        // A dangling link: where it would lead.
        Err(_) => std::fs::symlink_metadata(path).is_err() || link_target_inside(&root, path),
    }
}

/// Where a link (one that may lead nowhere) points, worked out from its
/// text, is in `root`.
fn link_target_inside(root: &Path, link: &Path) -> bool {
    let Ok(target) = std::fs::read_link(link) else {
        return false;
    };
    if target.is_absolute() {
        return false;
    }
    let Some(parent) = link.parent().and_then(|p| p.canonicalize().ok()) else {
        return false;
    };
    let mut out = PathBuf::new();
    for c in parent.join(target).components() {
        match c {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            c => out.push(c),
        }
    }
    out.starts_with(root)
}

/// Removes the symbolic links under `dir` that lead outside `root` (or
/// would, if they lead nowhere), and returns them relative to `root`.
/// For Railpack, which reads the code following links: a link in the
/// repository mustn't let it read the builder's own files. (BuildKit sends
/// a link as a link, so a Dockerfile build keeps them.)
pub fn remove_outside_links(root: &Path, dir: &Path) -> anyhow::Result<Vec<PathBuf>> {
    let canonical_root = root.canonicalize()?;
    let mut removed = vec![];
    let mut todo = vec![dir.to_path_buf()];
    while let Some(d) = todo.pop() {
        for entry in std::fs::read_dir(&d)? {
            let entry = entry?;
            let path = entry.path();
            let kind = entry.file_type()?;
            if kind.is_symlink() {
                let inside = match path.canonicalize() {
                    Ok(real) => real.starts_with(&canonical_root),
                    Err(_) => link_target_inside(&canonical_root, &path),
                };
                if !inside {
                    std::fs::remove_file(&path)?;
                    removed.push(path.strip_prefix(root).unwrap_or(&path).to_path_buf());
                }
            } else if kind.is_dir() && !(d == root && entry.file_name() == ".git") {
                todo.push(path);
            }
        }
    }
    removed.sort();
    Ok(removed)
}

/// Whether the service's Dockerfile path is the default, so Pyo picks the file
/// (Dockerfile, then Containerfile) or, without either, Railpack.
pub fn default_path(path: &str) -> bool {
    matches!(path.trim().trim_start_matches("./"), "" | "Dockerfile")
}

/// The file to build with: the path set in the service's settings, or by
/// default the build context's Dockerfile, else its Containerfile. None when
/// it isn't there.
pub fn build_file(app_dir: &Path, path: &str) -> anyhow::Result<Option<PathBuf>> {
    if default_path(path) {
        for f in pyo_proto::BUILD_FILES {
            let p = app_dir.join(f);
            if std::fs::symlink_metadata(&p).is_ok() && !stays_inside(app_dir, &p) {
                bail!("{f} is a symbolic link to outside the repository");
            }
        }
        return Ok(pyo_proto::BUILD_FILES
            .iter()
            .map(|f| app_dir.join(f))
            .find(|p| p.is_file()));
    }
    let f = inside(app_dir, path)?;
    Ok(f.is_file().then_some(f))
}

/// Whether Railpack's image only serves files: its static file provider, or
/// a Node app it builds as a single-page app. Both run a web server that
/// answers `/` and has no /healthz.
pub fn is_static(info: &Value) -> bool {
    info["detectedProviders"].get(0).and_then(Value::as_str) == Some("staticfile")
        || info["metadata"]["nodeIsSPA"].as_str() == Some("true")
}

/// What Railpack found, as people name it: "Next.js", "Go", "Python", …
pub fn label(info: &Value, app_dir: &Path) -> Option<String> {
    let provider = info["detectedProviders"]
        .as_array()?
        .first()?
        .as_str()?
        .to_owned();
    if provider == "node" {
        let pkg: Value = std::fs::read_to_string(app_dir.join("package.json"))
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        let has = |name: &str| {
            pkg["dependencies"].get(name).is_some() || pkg["devDependencies"].get(name).is_some()
        };
        for (dep, name) in [
            ("next", "Next.js"),
            ("nuxt", "Nuxt"),
            ("@remix-run/node", "Remix"),
            ("astro", "Astro"),
            ("@sveltejs/kit", "SvelteKit"),
            ("@nestjs/core", "NestJS"),
            ("vite", "Vite"),
            ("express", "Express"),
        ] {
            if has(dep) {
                return Some(name.into());
            }
        }
    }
    Some(
        match provider.as_str() {
            "node" => "Node.js",
            "golang" | "go" => "Go",
            "python" => "Python",
            "php" => "PHP",
            "ruby" => "Ruby",
            "java" => "Java",
            "rust" => "Rust",
            "deno" => "Deno",
            "elixir" => "Elixir",
            "gleam" => "Gleam",
            "dotnet" => ".NET",
            "cpp" => "C++",
            "staticfile" => "Static site",
            "shell" => "Shell script",
            other => other,
        }
        .to_owned(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subdirectories_stay_inside() {
        let base = Path::new("/w/src");
        assert_eq!(inside(base, ".").unwrap(), base);
        assert_eq!(inside(base, "./apps/web").unwrap(), base.join("apps/web"));
        assert!(inside(base, "../etc").is_err());
        assert!(inside(base, "/etc").is_err());
    }

    #[test]
    fn picks_the_build_file() {
        let dir = std::env::temp_dir().join(format!("pyo-build-file-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("docker")).unwrap();
        // Neither: zero config.
        assert_eq!(build_file(&dir, "Dockerfile").unwrap(), None);
        assert_eq!(build_file(&dir, "").unwrap(), None);
        // Only a Containerfile: the default finds it.
        std::fs::write(dir.join("Containerfile"), "FROM scratch\n").unwrap();
        assert_eq!(
            build_file(&dir, "Dockerfile").unwrap(),
            Some(dir.join("Containerfile"))
        );
        assert_eq!(
            build_file(&dir, "./").unwrap(),
            Some(dir.join("Containerfile"))
        );
        // Both: the Dockerfile wins.
        std::fs::write(dir.join("Dockerfile"), "FROM scratch\n").unwrap();
        assert_eq!(
            build_file(&dir, "Dockerfile").unwrap(),
            Some(dir.join("Dockerfile"))
        );
        assert_eq!(
            build_file(&dir, "./Dockerfile").unwrap(),
            Some(dir.join("Dockerfile"))
        );
        // A path set in the settings is used as is, and nothing else.
        assert_eq!(
            build_file(&dir, "Containerfile").unwrap(),
            Some(dir.join("Containerfile"))
        );
        assert_eq!(build_file(&dir, "docker/prod.Dockerfile").unwrap(), None);
        std::fs::write(dir.join("docker/prod.Dockerfile"), "FROM scratch\n").unwrap();
        assert_eq!(
            build_file(&dir, "docker/prod.Dockerfile").unwrap(),
            Some(dir.join("docker/prod.Dockerfile"))
        );
        assert!(build_file(&dir, "../Dockerfile").is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A symbolic link in the repository never takes the build outside the
    /// checkout: not as the build context, the Dockerfile or a path in the
    /// settings, and Railpack doesn't get to read through one.
    #[cfg(unix)]
    #[test]
    fn links_dont_lead_outside() {
        use std::os::unix::fs::symlink;
        let ws = std::env::temp_dir().join(format!("pyo-build-link-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&ws);
        let src = ws.join("src");
        std::fs::create_dir_all(src.join("apps/web")).unwrap();
        std::fs::create_dir_all(ws.join("secret")).unwrap();
        std::fs::write(ws.join("secret/Dockerfile"), "FROM scratch\n").unwrap();
        std::fs::write(src.join("apps/web/real.Dockerfile"), "FROM scratch\n").unwrap();
        symlink("/", src.join("root")).unwrap();
        symlink("../secret", src.join("up")).unwrap();
        symlink("../../secret/Dockerfile", src.join("apps/Dockerfile")).unwrap();
        symlink("web/real.Dockerfile", src.join("apps/ok.Dockerfile")).unwrap();
        symlink("apps/web", src.join("web")).unwrap();
        symlink("/nowhere/at/all", src.join("apps/web/dangling")).unwrap();
        symlink("missing-but-inside", src.join("apps/web/dangling-inside")).unwrap();
        symlink("/proc/self/environ", src.join("apps/web/package.json")).unwrap();

        // As the build context.
        assert!(inside(&src, "root").is_err());
        assert!(inside(&src, "up").is_err());
        assert!(inside(&src, "root/etc").is_err());
        assert_eq!(
            inside(&src, "web").unwrap(),
            src.join("web"),
            "a link inside the repository is fine"
        );
        assert_eq!(inside(&src, "apps/web").unwrap(), src.join("apps/web"));
        // As the build file, set or found.
        assert!(build_file(&src.join("apps"), "Dockerfile").is_err());
        assert!(build_file(&src, "apps/Dockerfile").is_err());
        assert!(build_file(&src, "up/Dockerfile").is_err());
        assert_eq!(
            build_file(&src.join("apps"), "ok.Dockerfile").unwrap(),
            Some(src.join("apps/ok.Dockerfile"))
        );

        // Railpack's view: links out are gone, links within stay.
        let removed = remove_outside_links(&src, &src).unwrap();
        let removed: Vec<String> = removed.iter().map(|p| p.display().to_string()).collect();
        assert_eq!(
            removed,
            [
                "apps/Dockerfile",
                "apps/web/dangling",
                "apps/web/package.json",
                "root",
                "up"
            ]
        );
        assert!(std::fs::symlink_metadata(src.join("web")).is_ok());
        assert!(std::fs::symlink_metadata(src.join("apps/ok.Dockerfile")).is_ok());
        assert!(std::fs::symlink_metadata(src.join("apps/web/dangling-inside")).is_ok());
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn labels_providers() {
        let dir = std::env::temp_dir().join("pyo-label-test");
        let _ = std::fs::create_dir_all(&dir);
        std::fs::write(
            dir.join("package.json"),
            r#"{"dependencies":{"next":"15","react":"19"}}"#,
        )
        .unwrap();
        assert_eq!(
            label(&serde_json::json!({ "detectedProviders": ["node"] }), &dir).as_deref(),
            Some("Next.js")
        );
        assert_eq!(
            label(
                &serde_json::json!({ "detectedProviders": ["golang"] }),
                &dir
            )
            .as_deref(),
            Some("Go")
        );
        assert_eq!(label(&serde_json::json!({}), &dir), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn static_sites_and_spas_are_static() {
        use serde_json::json;
        assert!(is_static(&json!({ "detectedProviders": ["staticfile"] })));
        assert!(is_static(&json!({
            "detectedProviders": ["node"],
            "metadata": { "nodeIsSPA": "true", "nodeRuntime": "vite" }
        })));
        // A Node server, and Railpack leaves out a false flag.
        assert!(!is_static(&json!({
            "detectedProviders": ["node"],
            "metadata": { "nodeRuntime": "node" }
        })));
        assert!(!is_static(&json!({ "detectedProviders": ["golang"] })));
        assert!(!is_static(&json!({})));
    }
}
