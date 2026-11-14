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

/// A static site generator's build that Pyo has Railpack serve as static
/// files (Caddy, a few MiB) instead of running its start script, which
/// would be the generator's development or preview server (a full Node.js
/// process, 100 MiB and more).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StaticSite {
    /// "Eleventy", "Astro", "Hugo" or "Vite".
    pub framework: &'static str,
    /// The build's output folder, relative to the app.
    pub output: String,
    /// Its start script (a development server), or empty when it has none.
    pub start: String,
}

impl StaticSite {
    /// Railpack's setting that serves the output (its SPA mode), for
    /// `railpack prepare`. Its plan reads every setting given to `prepare`
    /// as a build secret, so the build is given it too.
    pub fn railpack_setting(&self) -> (&'static str, String) {
        ("RAILPACK_SPA_OUTPUT_DIR", self.output.clone())
    }

    /// A generated site has real pages: an unknown path is a 404, not the
    /// home page (an app built with Vite routes in the browser, so it keeps
    /// Railpack's fallback to index.html).
    pub fn index_fallback(&self) -> bool {
        self.framework == "Vite"
    }
}

/// Build arguments that already say how to run it: Pyo leaves it to them.
/// RAILPACK_NO_SPA=1 runs the start script.
const RUN_SETTINGS: [&str; 4] = [
    "RAILPACK_SPA_OUTPUT_DIR",
    "RAILPACK_NO_SPA",
    "RAILPACK_START_CMD",
    "RAILPACK_STATIC_FILE_ROOT",
];

/// Commands that serve a site for development or preview, not production.
const DEV_SERVERS: [&str; 14] = [
    "eleventy --serve",
    "eleventy --watch",
    "astro dev",
    "astro preview",
    "vite dev",
    "vite preview",
    "vite serve",
    "hugo server",
    "hugo serve",
    "serve ",
    "http-server",
    "live-server",
    "browser-sync",
    "lite-server",
];

/// Whether a script runs a development or preview server.
fn dev_server(script: &str) -> bool {
    let s = format!("{} ", script.trim());
    s.split(['&', ';', '|'])
        .map(str::trim)
        .filter(|c| !c.is_empty())
        .any(|c| {
            let c = c.replace("@11ty/eleventy", "eleventy");
            let c = c
                .trim_start_matches("npx ")
                .trim_start_matches("bunx ")
                .trim_start_matches("pnpm exec ")
                .trim_start_matches("cross-env NODE_ENV=production ");
            let c = format!("{c} ");
            c == "vite "
                || c == "astro "
                || DEV_SERVERS
                    .iter()
                    .any(|d| c.starts_with(d) || c.contains(&format!(" {d}")))
        })
}

/// A script's command, following one "npm run x" (or pnpm, yarn, bun) to x.
fn resolve<'a>(scripts: &'a Value, name: &str) -> &'a str {
    let s = scripts[name].as_str().unwrap_or("").trim();
    for prefix in [
        "npm run ",
        "pnpm run ",
        "pnpm ",
        "yarn run ",
        "yarn ",
        "bun run ",
    ] {
        if let Some(other) = s.strip_prefix(prefix) {
            let other = other.split_whitespace().next().unwrap_or("");
            if other != name
                && let Some(t) = scripts[other].as_str()
            {
                return t.trim();
            }
        }
    }
    s
}

/// The value after `flag` ("--out x" or "--out=x") in a command.
fn flag_value(cmd: &str, flags: &[&str]) -> Option<String> {
    let words: Vec<&str> = cmd.split_whitespace().collect();
    for (i, w) in words.iter().enumerate() {
        for f in flags {
            if let Some(v) = w.strip_prefix(&format!("{f}=")) {
                return Some(v.trim_matches(['"', '\'']).to_owned());
            }
            if w == f {
                return words
                    .get(i + 1)
                    .map(|v| v.trim_matches(['"', '\'']).to_owned());
            }
        }
    }
    None
}

/// `key: "value"` in the first of `files` that's there.
fn config_value(app_dir: &Path, files: &[&str], key: &str) -> Option<String> {
    let text = files
        .iter()
        .find_map(|f| std::fs::read_to_string(app_dir.join(f)).ok())?;
    let at = text.find(&format!("{key}:"))? + key.len() + 1;
    let rest = text[at..].trim_start();
    let quote = rest
        .chars()
        .next()
        .filter(|c| matches!(c, '"' | '\'' | '`'))?;
    let value = rest[1..].split(quote).next()?;
    (!value.is_empty()).then(|| value.to_owned())
}

/// A relative folder in the app, or None.
fn folder(v: String) -> Option<String> {
    let v = v
        .trim()
        .trim_start_matches("./")
        .trim_end_matches('/')
        .to_owned();
    let p = Path::new(&v);
    (!v.is_empty() && v != "." && p.components().all(|c| matches!(c, Component::Normal(_))))
        .then_some(v)
}

/// When the app is a static site generator's (Eleventy, Astro without a
/// server adapter, Hugo through npm, Vite) with a build script, and Railpack
/// would run its start script though that's a development or preview
/// server (or, for Eleventy and Hugo, there's none): the output to serve as
/// static files instead. None when a build argument, a Procfile or
/// railpack.json already says how to run it.
pub fn static_site(app_dir: &Path, build_args: &[String]) -> Option<StaticSite> {
    if build_args
        .iter()
        .any(|k| RUN_SETTINGS.contains(&k.as_str()))
        || app_dir.join("Procfile").exists()
    {
        return None;
    }
    if let Ok(text) = std::fs::read_to_string(app_dir.join("railpack.json")) {
        let cfg: Value = serde_json::from_str(&text).unwrap_or_default();
        if cfg["deploy"]["startCommand"]
            .as_str()
            .is_some_and(|s| !s.trim().is_empty())
        {
            return None;
        }
    }
    let pkg: Value =
        serde_json::from_str(&std::fs::read_to_string(app_dir.join("package.json")).ok()?).ok()?;
    let scripts = &pkg["scripts"];
    let build = resolve(scripts, "build");
    if build.is_empty() || dev_server(build) {
        return None;
    }
    let start = resolve(scripts, "start").to_owned();
    let has = |dep: &str| {
        pkg["dependencies"].get(dep).is_some() || pkg["devDependencies"].get(dep).is_some()
    };
    let (framework, output) = if has("astro") && build.contains("astro build") {
        let config = [
            "astro.config.mjs",
            "astro.config.ts",
            "astro.config.js",
            "astro.config.mts",
        ];
        let server = config_value(app_dir, &config, "output").is_some_and(|o| o == "server");
        let adapter = [
            "@astrojs/node",
            "@astrojs/vercel",
            "@astrojs/netlify",
            "@astrojs/cloudflare",
        ]
        .iter()
        .any(|a| has(a));
        if server || adapter {
            return None;
        }
        (
            "Astro",
            config_value(app_dir, &config, "outDir").unwrap_or_else(|| "dist".into()),
        )
    } else if has("@11ty/eleventy") && build.contains("eleventy") {
        let config = [
            "eleventy.config.js",
            "eleventy.config.mjs",
            "eleventy.config.cjs",
            "eleventy.config.ts",
            ".eleventy.js",
        ];
        let out =
            flag_value(build, &["--output"]).or_else(|| config_value(app_dir, &config, "output"));
        ("Eleventy", out.unwrap_or_else(|| "_site".into()))
    } else if build.split_whitespace().any(|w| w == "hugo") {
        (
            "Hugo",
            flag_value(build, &["-d", "--destination"]).unwrap_or_else(|| "public".into()),
        )
    } else if has("vite") && build.contains("vite build") {
        // Apps that build a server of their own.
        if [
            "@sveltejs/kit",
            "@tanstack/react-start",
            "@react-router/dev",
            "@remix-run/dev",
            "nuxt",
            "next",
        ]
        .iter()
        .any(|d| has(d))
        {
            return None;
        }
        let config = [
            "vite.config.js",
            "vite.config.ts",
            "vite.config.mjs",
            "vite.config.mts",
        ];
        let out =
            flag_value(build, &["--outDir"]).or_else(|| config_value(app_dir, &config, "outDir"));
        ("Vite", out.unwrap_or_else(|| "dist".into()))
    } else {
        return None;
    };
    let output = folder(output)?;
    // Vite and Astro without a start script: Railpack serves them already.
    let no_start_ok = matches!(framework, "Eleventy" | "Hugo");
    if !(dev_server(&start) || (start.is_empty() && no_start_ok)) {
        return None;
    }
    Some(StaticSite {
        framework,
        output,
        start,
    })
}

/// What the build log says about it.
pub fn static_site_note(s: &StaticSite) -> String {
    let why = if s.start.is_empty() {
        "it has no start script".to_owned()
    } else {
        format!("its start script ({}) is a development server", s.start)
    };
    format!(
        "Serving the {} site's {} folder as static files: {why}. To run the start script instead, add the build argument RAILPACK_NO_SPA=1.",
        s.framework, s.output
    )
}

/// Railpack's index.html fallback off for a generated site, through a
/// Staticfile (its only switch), unless the app has one.
pub fn write_staticfile(app_dir: &Path, s: &StaticSite) -> std::io::Result<()> {
    let path = app_dir.join("Staticfile");
    if s.index_fallback() || std::fs::symlink_metadata(&path).is_ok() {
        return Ok(());
    }
    std::fs::write(path, "index_fallback: false\n")
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

    fn app(name: &str, files: &[(&str, &str)]) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("pyo-ssg-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for (f, text) in files {
            std::fs::write(dir.join(f), text).unwrap();
        }
        dir
    }

    #[test]
    fn eleventys_dev_server_becomes_static_files() {
        let pkg = r#"{"scripts":{"build":"eleventy","start":"eleventy --serve"},"devDependencies":{"@11ty/eleventy":"3"}}"#;
        let dir = app("11ty", &[("package.json", pkg)]);
        let s = static_site(&dir, &[]).unwrap();
        assert_eq!(
            s,
            StaticSite {
                framework: "Eleventy",
                output: "_site".into(),
                start: "eleventy --serve".into()
            }
        );
        assert_eq!(
            s.railpack_setting(),
            ("RAILPACK_SPA_OUTPUT_DIR", "_site".to_owned())
        );
        assert!(static_site_note(&s).contains("RAILPACK_NO_SPA=1"));
        // Its output folder from the config, or the build's --output.
        std::fs::write(
            dir.join("eleventy.config.js"),
            "export default function () { return { dir: { input: \"src\", output: \"dist\" } } }",
        )
        .unwrap();
        assert_eq!(static_site(&dir, &[]).unwrap().output, "dist");
        let pkg = r#"{"scripts":{"build":"npx @11ty/eleventy --output=www","start":"npm run dev","dev":"npx @11ty/eleventy --serve"},"dependencies":{"@11ty/eleventy":"3"}}"#;
        std::fs::write(dir.join("package.json"), pkg).unwrap();
        assert_eq!(static_site(&dir, &[]).unwrap().output, "www");
        // No start script at all: Railpack would have nothing to run.
        std::fs::write(
            dir.join("package.json"),
            r#"{"scripts":{"build":"eleventy"},"devDependencies":{"@11ty/eleventy":"3"}}"#,
        )
        .unwrap();
        assert_eq!(static_site(&dir, &[]).unwrap().start, "");
        // The escape hatch, and other ways it's already said.
        for arg in [
            "RAILPACK_NO_SPA",
            "RAILPACK_SPA_OUTPUT_DIR",
            "RAILPACK_START_CMD",
        ] {
            assert_eq!(static_site(&dir, &[arg.to_owned()]), None, "{arg}");
        }
        std::fs::write(dir.join("Procfile"), "web: node server.js\n").unwrap();
        assert_eq!(static_site(&dir, &[]), None);
        // A generated site's unknown pages are 404s.
        std::fs::remove_file(dir.join("Procfile")).unwrap();
        write_staticfile(&dir, &s).unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.join("Staticfile")).unwrap(),
            "index_fallback: false\n"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn astro_vite_and_hugo() {
        // Astro's dev server as its start script, static output.
        let dir = app(
            "astro",
            &[
                (
                    "package.json",
                    r#"{"scripts":{"build":"astro build","start":"astro dev"},"dependencies":{"astro":"5"}}"#,
                ),
                (
                    "astro.config.mjs",
                    "export default defineConfig({ outDir: './build' })",
                ),
            ],
        );
        assert_eq!(
            static_site(&dir, &[]).unwrap(),
            StaticSite {
                framework: "Astro",
                output: "build".into(),
                start: "astro dev".into()
            }
        );
        // With a server adapter it's a server.
        std::fs::write(dir.join("package.json"), r#"{"scripts":{"build":"astro build","start":"astro preview"},"dependencies":{"astro":"5","@astrojs/node":"9"}}"#).unwrap();
        assert_eq!(static_site(&dir, &[]), None);
        // Without a start script Railpack serves it already.
        std::fs::write(
            dir.join("package.json"),
            r#"{"scripts":{"build":"astro build"},"dependencies":{"astro":"5"}}"#,
        )
        .unwrap();
        assert_eq!(static_site(&dir, &[]), None);
        let _ = std::fs::remove_dir_all(&dir);

        let dir = app(
            "vite",
            &[(
                "package.json",
                r#"{"scripts":{"build":"tsc && vite build","start":"vite preview --port $PORT"},"devDependencies":{"vite":"6"}}"#,
            )],
        );
        let s = static_site(&dir, &[]).unwrap();
        assert_eq!((s.framework, s.output.as_str()), ("Vite", "dist"));
        assert!(s.index_fallback(), "an app routes in the browser");
        write_staticfile(&dir, &s).unwrap();
        assert!(!dir.join("Staticfile").exists());
        // A real server as the start script stays.
        std::fs::write(dir.join("package.json"), r#"{"scripts":{"build":"vite build","start":"node server.js"},"devDependencies":{"vite":"6"}}"#).unwrap();
        assert_eq!(static_site(&dir, &[]), None);
        std::fs::write(dir.join("package.json"), r#"{"scripts":{"build":"vite build","start":"vite"},"devDependencies":{"vite":"6","@sveltejs/kit":"2"}}"#).unwrap();
        assert_eq!(static_site(&dir, &[]), None, "SvelteKit builds a server");
        let _ = std::fs::remove_dir_all(&dir);

        let dir = app(
            "hugo",
            &[(
                "package.json",
                r#"{"scripts":{"build":"hugo --minify -d site","start":"hugo server"},"devDependencies":{"hugo-bin":"0.140"}}"#,
            )],
        );
        assert_eq!(static_site(&dir, &[]).unwrap().output, "site");
        // Not a folder in the app.
        std::fs::write(
            dir.join("package.json"),
            r#"{"scripts":{"build":"hugo -d ../out","start":"hugo server"}}"#,
        )
        .unwrap();
        assert_eq!(static_site(&dir, &[]), None);
        let _ = std::fs::remove_dir_all(&dir);

        // Not a generator, or no package.json.
        let dir = app(
            "express",
            &[(
                "package.json",
                r#"{"scripts":{"build":"tsc","start":"node dist/index.js"},"dependencies":{"express":"5"}}"#,
            )],
        );
        assert_eq!(static_site(&dir, &[]), None);
        std::fs::remove_file(dir.join("package.json")).unwrap();
        assert_eq!(static_site(&dir, &[]), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn dev_servers_are_told_from_servers() {
        for s in [
            "eleventy --serve",
            "npx @11ty/eleventy --serve --port 8080",
            "astro dev",
            "vite",
            "vite preview --host",
            "npx serve -s dist",
            "http-server ./public",
            "hugo server -D",
            "npm run build && vite preview",
        ] {
            assert!(dev_server(s), "{s}");
        }
        for s in [
            "node server.js",
            "eleventy",
            "vite build",
            "astro build",
            "next start",
            "",
            "node ./serve.js",
        ] {
            assert!(!dev_server(s), "{s}");
        }
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
