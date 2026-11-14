//! Commands on one service: which one comes from `--service`, else the
//! nearest pyo.toml. Changes save the service's whole configuration the
//! way the dashboard does (`saveService`); secrets' values never come back
//! from the API, and the API keeps them when they're sent back blank.

use std::{collections::HashSet, io::IsTerminal, path::PathBuf, time::Duration};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use crate::{
    api::{Api, State, new_id, now_ms},
    config::{self, ServiceFile},
    ui,
};

/// The service a command acts on, with the pyo.toml that named it.
pub struct Target {
    pub state: State,
    pub id: String,
    pub file: Option<(PathBuf, ServiceFile)>,
}

impl Target {
    pub fn service(&self) -> &Value {
        self.state.service(&self.id).expect("resolved")
    }
}

pub async fn target(api: &Api, service: Option<&str>) -> Result<Target> {
    let file = match config::find() {
        Some(p) => Some((p.clone(), config::load(&p)?)),
        None => None,
    };
    let state = api.state().await?;
    let id = match service {
        Some(s) => resolve(&state, s)?,
        None => match file.as_ref().and_then(|(_, f)| f.service.clone()) {
            Some(id) => id,
            None => bail!("No service linked here. Run: pyo link"),
        },
    };
    state.service(&id)?;
    Ok(Target { state, id, file })
}

/// An id, or a name: "service", "project/service" or "team/project/service".
pub fn resolve(state: &State, s: &str) -> Result<String> {
    let s = s.trim_matches('/');
    if state.services().iter().any(|a| a["id"] == json!(s)) {
        return Ok(s.to_owned());
    }
    let hits: Vec<&Value> = state
        .services()
        .iter()
        .filter(|a| matches(&state.path_of(a), s))
        .collect();
    match hits.len() {
        0 => bail!("No service called \"{s}\". See them all: pyo link"),
        1 => Ok(hits[0]["id"].as_str().unwrap_or("").to_owned()),
        _ => {
            let names: Vec<String> = hits.iter().map(|a| state.path_of(a)).collect();
            let i = ui::choose(&format!("More than one service is called \"{s}\":"), &names)?;
            Ok(hits[i]["id"].as_str().unwrap_or("").to_owned())
        }
    }
}

fn matches(path: &str, s: &str) -> bool {
    path == s || path.ends_with(&format!("/{s}"))
}

// ── link ───────────────────────────────────────────────────────────────

pub async fn link(api: &Api, service: Option<String>) -> Result<()> {
    let state = api.state().await?;
    if state.services().is_empty() {
        bail!(
            "You have no services yet. Create one on the dashboard: {}",
            api.base
        );
    }
    let id = match service {
        Some(s) => resolve(&state, &s)?,
        None => {
            let mut services: Vec<&Value> = state.services().iter().collect();
            services.sort_by_key(|a| state.path_of(a));
            let names: Vec<String> = services.iter().map(|a| state.path_of(a)).collect();
            let i = ui::choose("Which service does this folder deploy?", &names)?;
            services[i]["id"].as_str().unwrap_or("").to_owned()
        }
    };
    let service = state.service(&id)?;
    let label = state.path_of(service);
    let path = config::write_service(config::find().as_deref(), &id, &label)?;
    ui::ok(&format!(
        "Linked {} in {}",
        ui::bold(&label),
        path.display()
    ));
    println!("{}", ui::dim("Deploy it with: pyo deploy"));
    Ok(())
}

// ── deploy ─────────────────────────────────────────────────────────────

/// A yes to a [y/N] question.
fn confirmed(answer: &str) -> bool {
    matches!(answer.trim().to_lowercase().as_str(), "y" | "yes")
}

pub async fn deploy(api: &Api, service: Option<&str>, detach: bool, yes: bool) -> Result<()> {
    let t = target(api, service).await?;
    let mut service = t.service().clone();
    let changes = match &t.file {
        Some((_, f)) => apply(&mut service, f)?,
        None => vec![],
    };
    let dep = new_id("dpl");
    let done = if changes.is_empty() {
        api.op(
            "deploy",
            json!({ "serviceId": t.id, "deploymentId": dep, "trigger": "manual" }),
        )
        .await?
    } else {
        for c in &changes {
            println!("{} {c}", ui::dim("·"));
        }
        let save = |confirm: bool| {
            api.op(
                "saveService",
                json!({ "service": service, "deploymentId": dep, "trigger": "manual", "confirm": confirm }),
            )
        };
        match save(yes).await {
            Err(e) if e.downcast_ref::<crate::api::NeedsConfirm>().is_some() => {
                // A very large replica change: asked here, or -y.
                if !confirmed(&ui::ask(&format!("{e} [y/N] "))?) {
                    bail!("Not deployed.");
                }
                save(true).await?
            }
            r => r?,
        }
    };
    // Replicas that don't fit on the servers wait for room.
    if let Some(w) = done["warning"].as_str() {
        println!("{}", ui::yellow(w));
    }
    let url = t.state.deployment_url(&api.base, &service, &dep);
    println!(
        "Deploying {} {}",
        ui::bold(service["name"].as_str().unwrap_or("")),
        ui::dim(&url)
    );
    if detach {
        return Ok(());
    }
    watch(api, &dep).await
}

/// Follow a deployment to the end: its stages and build log. Fails when it does.
pub async fn watch(api: &Api, dep: &str) -> Result<()> {
    println!(
        "{}",
        ui::dim("Ctrl-C stops watching. The deploy carries on.")
    );
    let mut seen: HashSet<String> = HashSet::new();
    let mut stage = String::new();
    let started = now_ms();
    loop {
        tokio::time::sleep(Duration::from_millis(1500)).await;
        let state = api.state().await?;
        let Some(d) = state.deployment(dep) else {
            if now_ms() - started > 30_000 {
                bail!("The deployment didn't show up. Check the dashboard.");
            }
            continue;
        };
        for l in d["logs"].as_array().into_iter().flatten() {
            let key = l["id"]
                .as_str()
                .map(str::to_owned)
                .unwrap_or_else(|| format!("{}{}", l["at"], l["message"]));
            if seen.insert(key) {
                print_line(l);
            }
        }
        let status = d["status"].as_str().unwrap_or("").to_owned();
        if status != stage {
            stage = status.clone();
            match status.as_str() {
                "succeeded" => {
                    ui::ok(&format!(
                        "Live: {}",
                        d["revisionLabel"]
                            .as_str()
                            .filter(|s| !s.is_empty())
                            .unwrap_or("new version")
                    ));
                    return Ok(());
                }
                "failed" => {
                    let why = d["failure"]["message"].as_str().unwrap_or("It failed.");
                    let back = if d["autoRolledBack"] == json!(true) {
                        " The previous version is back."
                    } else {
                        ""
                    };
                    bail!("Deploy failed: {why}{back}");
                }
                "cancelled" => bail!("Deploy cancelled."),
                s => println!("{} {}…", ui::bold("→"), stage_label(s)),
            }
        }
    }
}

/// A deployment's stage in words, as the dashboard says it (its
/// `STAGE_LABEL`).
fn stage_label(stage: &str) -> String {
    let known = match stage {
        "queued" => "Queued",
        "building" => "Building",
        "pushing" => "Pushing image",
        "waiting_database" => "Waiting for database",
        "applying" => "Applying settings",
        "scheduling" => "Finding a server",
        "pulling" => "Downloading image",
        "starting" => "Starting",
        "waiting_ready" => "Running health check",
        "rolling_back" => "Rolling back",
        _ => "",
    };
    if !known.is_empty() {
        return known.to_owned();
    }
    // One the CLI doesn't know yet: its words, not its code.
    let s = stage.replace('_', " ");
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
        None => String::new(),
    }
}

fn print_line(l: &Value) {
    let msg = l["message"].as_str().unwrap_or("");
    let level = l["level"].as_str().unwrap_or("info");
    let at = ui::dim(&ui::clock(l["at"].as_i64().unwrap_or(0)));
    let msg = ui::log_text(msg, level);
    println!("{at} {msg}");
}

/// Put pyo.toml's settings on the service; what it changed, in words.
pub fn apply(service: &mut Value, f: &ServiceFile) -> Result<Vec<String>> {
    let mut changes = vec![];
    let kind = service["source"]["type"].as_str().unwrap_or("").to_owned();

    if let Some(image) = &f.image {
        if kind != "image" {
            bail!(
                "pyo.toml: `image` is for services deployed from an image. This one is from {kind}."
            );
        }
        let (name, reference) = split_image(image);
        set(
            &mut changes,
            &mut service["source"]["image"],
            json!(name),
            format!("image {image}"),
        );
        set(
            &mut changes,
            &mut service["source"]["reference"],
            json!(reference),
            format!("image {image}"),
        );
    }
    let b = &f.build;
    if b.branch.is_some() || b.dockerfile.is_some() || b.context.is_some() || !b.args.is_empty() {
        if kind != "git" {
            bail!("pyo.toml: [build] is for services built from Git. This one is from {kind}.");
        }
        if let Some(v) = &b.branch {
            set(
                &mut changes,
                &mut service["source"]["branch"],
                json!(v),
                format!("branch {v}"),
            );
        }
        if let Some(v) = &b.dockerfile {
            set(
                &mut changes,
                &mut service["source"]["dockerfilePath"],
                json!(v),
                format!("dockerfile {v}"),
            );
        }
        if let Some(v) = &b.context {
            set(
                &mut changes,
                &mut service["source"]["buildContext"],
                json!(v),
                format!("build context {v}"),
            );
        }
        for (key, v) in &b.args {
            let value = match v {
                toml::Value::String(s) => s.clone(),
                toml::Value::Integer(_) | toml::Value::Float(_) | toml::Value::Boolean(_) => {
                    v.to_string()
                }
                _ => bail!("pyo.toml: build argument {key} must be text, a number or true/false."),
            };
            if key.is_empty() || key.contains(['=', ' ']) {
                bail!("pyo.toml: build argument {key:?} isn't a name.");
            }
            let src = &mut service["source"];
            if !src["buildArgs"].is_array() {
                src["buildArgs"] = json!([]);
            }
            let args = src["buildArgs"]
                .as_array_mut()
                .context("no build arguments list")?;
            match args.iter_mut().find(|a| a["key"] == json!(key)) {
                Some(a) => set(
                    &mut changes,
                    &mut a["value"],
                    json!(value),
                    format!("build argument {key}"),
                ),
                None => {
                    args.push(json!({ "key": key, "value": value }));
                    changes.push(format!("build argument {key}"));
                }
            }
        }
    }

    let d = &f.deploy;
    if let Some(n) = d.replicas {
        if n < 0 {
            bail!("pyo.toml: replicas can't be below 0.");
        }
        if n > i64::from(i32::MAX) {
            bail!(
                "pyo.toml: that's more replicas than Kubernetes can count. The most is 2147483647."
            );
        }
        set(
            &mut changes,
            &mut service["replicas"],
            json!(n),
            format!("replicas {n}"),
        );
    }
    if let Some(port) = d.port {
        if !(1..=65535).contains(&port) {
            bail!("pyo.toml: port is 1 to 65535.");
        }
        let ports = service["ports"]
            .as_array_mut()
            .context("service has no ports list")?;
        match ports.iter_mut().find(|p| p["protocol"] == json!("HTTP")) {
            Some(p) => set(
                &mut changes,
                &mut p["containerPort"],
                json!(port),
                format!("port {port}"),
            ),
            None => {
                ports.push(json!({ "id": new_id("port"), "name": "http", "containerPort": port, "protocol": "HTTP", "exposure": "internal" }));
                changes.push(format!("port {port}"));
            }
        }
    }
    if let Some(path) = &d.health {
        let kind = match path.as_str() {
            "tcp" | "none" => path.as_str(),
            p if p.starts_with('/') => "http",
            _ => bail!("pyo.toml: health is a path like \"/healthz\", or \"tcp\" or \"none\"."),
        };
        let port = d
            .port
            .or_else(|| {
                service["ports"]
                    .as_array()?
                    .iter()
                    .find(|p| p["protocol"] == json!("HTTP"))?["containerPort"]
                    .as_i64()
            })
            .unwrap_or(80);
        let h = &mut service["healthCheck"];
        // Set here, so Pyo no longer picks it (no `auto`).
        let new = json!({
            "type": kind, "path": if kind == "http" { path.as_str() } else { "" }, "port": port, "command": h["command"].as_str().unwrap_or(""),
            "initialDelaySeconds": h["initialDelaySeconds"].as_i64().unwrap_or(5), "periodSeconds": h["periodSeconds"].as_i64().unwrap_or(10),
        });
        set(&mut changes, h, new, format!("health check {path}"));
    }
    if let Some(on) = d.auto_rollback {
        set(
            &mut changes,
            &mut service["autoRollback"],
            json!(on),
            format!("auto rollback {}", if on { "on" } else { "off" }),
        );
    }

    let r = &f.resources;
    if let Some(cpu) = &r.cpu {
        let m = config::parse_cpu(cpu)?;
        let req = service["resources"]["cpuRequest"]
            .as_i64()
            .unwrap_or(m)
            .min(m);
        set(
            &mut changes,
            &mut service["resources"]["cpuLimit"],
            json!(m),
            format!("cpu {cpu}"),
        );
        service["resources"]["cpuRequest"] = json!(req);
    }
    if let Some(mem) = &r.memory {
        let mi = config::parse_memory(mem)?;
        let req = service["resources"]["memoryRequest"]
            .as_i64()
            .unwrap_or(mi)
            .min(mi);
        set(
            &mut changes,
            &mut service["resources"]["memoryLimit"],
            json!(mi),
            format!("memory {mem}"),
        );
        service["resources"]["memoryRequest"] = json!(req);
    }

    for (key, v) in &f.env {
        let value = match v {
            toml::Value::String(s) => s.clone(),
            toml::Value::Integer(_) | toml::Value::Float(_) | toml::Value::Boolean(_) => {
                v.to_string()
            }
            _ => bail!("pyo.toml: env {key} must be text, a number or true/false."),
        };
        let env = service["env"]
            .as_array_mut()
            .context("service has no env list")?;
        match env.iter_mut().find(|e| e["key"] == json!(key)) {
            Some(e) if e["secret"] == json!(true) => {
                eprintln!(
                    "{} {key} is a secret in Pyo, so pyo.toml doesn't change it.",
                    ui::yellow("!")
                );
            }
            Some(e) => {
                if e["value"] != json!(value) {
                    e["value"] = json!(value);
                    e["updatedAt"] = json!(now_ms());
                    changes.push(format!("env {key}"));
                }
            }
            None => {
                env.push(json!({ "id": new_id("env"), "key": key, "value": value, "secret": false, "updatedAt": now_ms() }));
                changes.push(format!("env {key}"));
            }
        }
    }
    Ok(changes)
}

fn set(changes: &mut Vec<String>, v: &mut Value, new: Value, what: String) {
    if *v != new {
        *v = new;
        if !changes.contains(&what) {
            changes.push(what);
        }
    }
}

/// "ghcr.io/acme/api:1.2" → ("ghcr.io/acme/api", "1.2"); a digest after @.
fn split_image(s: &str) -> (String, String) {
    if let Some((n, d)) = s.split_once('@') {
        return (n.to_owned(), d.to_owned());
    }
    let slash = s.rfind('/').map(|i| i + 1).unwrap_or(0);
    match s[slash..].rfind(':') {
        Some(i) => (s[..slash + i].to_owned(), s[slash + i + 1..].to_owned()),
        None => (s.to_owned(), "latest".to_owned()),
    }
}

// ── status ─────────────────────────────────────────────────────────────

pub async fn status(api: &Api, service: Option<&str>, as_json: bool) -> Result<()> {
    let t = target(api, service).await?;
    let a = t.service();
    if as_json {
        println!("{}", serde_json::to_string_pretty(a)?);
        return Ok(());
    }
    let row = |k: &str, v: String| println!("{}  {v}", ui::dim(&format!("{k:<9}")));
    println!(
        "{}  {}",
        ui::bold(a["name"].as_str().unwrap_or("")),
        ui::dim(&t.state.path_of(a))
    );
    let ready = a["runtime"]["readyReplicas"].as_i64().unwrap_or(0);
    row(
        "Status",
        format!(
            "{} · {ready}/{} ready",
            a["status"].as_str().unwrap_or("?"),
            a["replicas"].as_i64().unwrap_or(0)
        ),
    );
    if let Some(d) = a["currentDeploymentId"]
        .as_str()
        .and_then(|id| t.state.deployment(id))
    {
        let when = d["finishedAt"]
            .as_i64()
            .or(d["createdAt"].as_i64())
            .map(ui::ago)
            .unwrap_or_default();
        row(
            "Live",
            format!(
                "{} · {when}",
                d["revisionLabel"]
                    .as_str()
                    .filter(|s| !s.is_empty())
                    .unwrap_or("?")
            ),
        );
    }
    if a["pendingChanges"] == json!(true) {
        row("Pending", "Saved changes not deployed yet".into());
    }
    if let Some(c) = t.state.cluster(a["clusterId"].as_str().unwrap_or("")) {
        row("Cluster", c["name"].as_str().unwrap_or("").to_owned());
    }
    let domains: Vec<String> = a["domains"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|d| d["hostname"].as_str())
        .map(|h| format!("https://{h}"))
        .collect();
    if !domains.is_empty() {
        row("Domains", domains.join(", "));
    }
    row("Dashboard", t.state.service_url(&api.base, a));
    Ok(())
}

// ── logs ───────────────────────────────────────────────────────────────

pub async fn logs(api: &Api, service: Option<&str>, follow: bool) -> Result<()> {
    let t = target(api, service).await?;
    let path = format!("/api/v1/services/{}/logs", t.id);
    let mut last = 0i64;
    let mut seen: HashSet<String> = HashSet::new();
    loop {
        let r = api.get(&path).await?;
        for l in r["logs"].as_array().into_iter().flatten() {
            let at = l["at"].as_i64().unwrap_or(0);
            let key = format!("{at}{}{}", l["source"], l["message"]);
            if at < last || !seen.insert(key) {
                continue;
            }
            last = last.max(at);
            let src = l["source"].as_str().unwrap_or("");
            let msg = ui::log_text(
                l["message"].as_str().unwrap_or(""),
                l["level"].as_str().unwrap_or("info"),
            );
            println!("{} {} {msg}", ui::dim(&ui::clock(at)), ui::dim(src));
        }
        if !follow {
            return Ok(());
        }
        // Only lines at the newest timestamp can repeat.
        seen.retain(|k| k.starts_with(&last.to_string()));
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

// ── env ────────────────────────────────────────────────────────────────

pub async fn env_list(api: &Api, service: Option<&str>) -> Result<()> {
    let t = target(api, service).await?;
    for e in t.service()["env"].as_array().into_iter().flatten() {
        let k = e["key"].as_str().unwrap_or("");
        if e["secret"] == json!(true) {
            println!("{k}={}", ui::dim("•••••••• (secret)"));
        } else {
            println!("{k}={}", e["value"].as_str().unwrap_or(""));
        }
    }
    Ok(())
}

pub async fn env_set(
    api: &Api,
    service: Option<&str>,
    pairs: &[String],
    secret: bool,
    deploy_now: bool,
) -> Result<()> {
    let t = target(api, service).await?;
    let mut service = t.service().clone();
    let env = service["env"]
        .as_array_mut()
        .context("service has no env list")?;
    let mut keys = vec![];
    for p in pairs {
        let (k, v) = p
            .split_once('=')
            .with_context(|| format!("\"{p}\" isn't KEY=value"))?;
        let k = k.trim();
        if k.is_empty()
            || !k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
            || k.starts_with(|c: char| c.is_ascii_digit())
        {
            bail!("\"{k}\" isn't a valid name: letters, digits and _.");
        }
        match env.iter_mut().find(|e| e["key"] == json!(k)) {
            Some(e) => {
                e["value"] = json!(v);
                e["updatedAt"] = json!(now_ms());
                if secret {
                    e["secret"] = json!(true);
                }
            }
            None => env.push(json!({ "id": new_id("env"), "key": k, "value": v, "secret": secret, "updatedAt": now_ms() })),
        }
        keys.push(k.to_owned());
    }
    save(
        api,
        &t,
        service,
        deploy_now,
        &format!("Set {}", keys.join(", ")),
    )
    .await
}

pub async fn env_unset(
    api: &Api,
    service: Option<&str>,
    keys: &[String],
    deploy_now: bool,
) -> Result<()> {
    let t = target(api, service).await?;
    let mut service = t.service().clone();
    let env = service["env"]
        .as_array_mut()
        .context("service has no env list")?;
    let before = env.len();
    env.retain(|e| !keys.iter().any(|k| e["key"] == json!(k)));
    if env.len() == before {
        bail!("None of those are set.");
    }
    save(
        api,
        &t,
        service,
        deploy_now,
        &format!("Removed {}", keys.join(", ")),
    )
    .await
}

/// Write the variables to a .env file; secrets are listed, not written.
pub async fn env_pull(api: &Api, service: Option<&str>, file: &str, force: bool) -> Result<()> {
    let t = target(api, service).await?;
    if std::path::Path::new(file).exists() && !force {
        bail!("{file} exists. Overwrite it with --force.");
    }
    let mut out = format!("# From Pyo: {}\n", t.state.path_of(t.service()));
    let mut secrets = 0;
    for e in t.service()["env"].as_array().into_iter().flatten() {
        let k = e["key"].as_str().unwrap_or("");
        if e["secret"] == json!(true) {
            out.push_str(&format!("# {k} is secret\n"));
            secrets += 1;
        } else {
            let v = e["value"].as_str().unwrap_or("");
            let quoted = if v.chars().any(|c| c.is_whitespace() || "#\"'$".contains(c)) {
                format!("\"{}\"", v.replace('\\', "\\\\").replace('"', "\\\""))
            } else {
                v.to_owned()
            };
            out.push_str(&format!("{k}={quoted}\n"));
        }
    }
    std::fs::write(file, out)?;
    ui::ok(&format!(
        "Wrote {file}{}",
        if secrets > 0 {
            format!(
                " ({secrets} secret{} left out)",
                if secrets == 1 { "" } else { "s" }
            )
        } else {
            String::new()
        }
    ));
    Ok(())
}

/// How `env push` treats keys the service has already.
#[derive(Clone, Copy, PartialEq)]
pub enum Existing {
    Ask,
    Overwrite,
    Skip,
}

/// Set the variables of a .env file: new keys are added (as plain
/// variables, or secrets when asked), keys set already are overwritten or
/// skipped, a secret staying a secret. Values are never printed.
pub struct Push {
    pub existing: Existing,
    /// Keys to make secrets.
    pub secret: Vec<String>,
    /// Make every key a secret.
    pub secrets: bool,
    pub dry_run: bool,
    pub deploy: bool,
}

pub async fn env_push(api: &Api, service: Option<&str>, file: &str, o: Push) -> Result<()> {
    let Push {
        existing,
        secret: ref secret_keys,
        secrets: all_secret,
        dry_run,
        deploy: deploy_now,
    } = o;
    let text = std::fs::read_to_string(file).with_context(|| format!("Can't read {file}"))?;
    let parsed = crate::dotenv::parse(&text);
    let (entries, repeated) = crate::dotenv::last_wins(parsed.entries);
    if !parsed.invalid.is_empty() {
        let n = parsed.invalid.len();
        eprintln!(
            "{} Ignored {n} line{} that {} KEY=value.",
            ui::yellow("!"),
            if n == 1 { "" } else { "s" },
            if n == 1 { "isn't" } else { "aren't" }
        );
    }
    if !repeated.is_empty() {
        eprintln!(
            "{} {} set more than once. Used the last value.",
            ui::yellow("!"),
            repeated.join(", ")
        );
    }
    if entries.is_empty() {
        bail!("No variables in {file}.");
    }
    if let Some(k) = secret_keys
        .iter()
        .find(|k| !entries.iter().any(|(e, _)| e == *k))
    {
        bail!("--secret {k}: it isn't in {file}.");
    }

    let t = target(api, service).await?;
    let mut service = t.service().clone();
    let env = service["env"]
        .as_array_mut()
        .context("service has no env list")?;
    let find = |env: &[Value], k: &str| env.iter().position(|e| e["key"] == json!(k));
    // Keys set already, unless the value is the same (a secret's can't be compared).
    let dupes: Vec<&str> = entries
        .iter()
        .filter(|(k, v)| {
            find(env, k)
                .is_some_and(|i| env[i]["secret"] == json!(true) || env[i]["value"] != json!(v))
        })
        .map(|(k, _)| k.as_str())
        .collect();
    let interactive = std::io::stdin().is_terminal() && !dry_run;
    if existing == Existing::Ask && !dupes.is_empty() && !interactive && !dry_run {
        bail!(
            "{} {} set already. Pass --overwrite or --skip-existing.",
            dupes.join(", "),
            if dupes.len() == 1 { "is" } else { "are" }
        );
    }

    let (mut added, mut updated, mut skipped, mut same) = (0, 0, 0, 0);
    let mut plan: Vec<String> = vec![];
    let mut all: Option<bool> = None;
    for (k, v) in &entries {
        let secret = all_secret || secret_keys.contains(k);
        let Some(i) = find(env, k) else {
            env.push(json!({ "id": new_id("env"), "key": k, "value": v, "secret": secret, "updatedAt": now_ms() }));
            added += 1;
            plan.push(format!(
                "{} {k}{}",
                ui::green("+"),
                if secret {
                    ui::dim(" (secret)")
                } else {
                    String::new()
                }
            ));
            continue;
        };
        if !dupes.contains(&k.as_str()) {
            same += 1;
            continue;
        }
        let was_secret = env[i]["secret"] == json!(true);
        let overwrite = match (existing, all) {
            (Existing::Overwrite, _) => true,
            (Existing::Skip, _) => false,
            (Existing::Ask, Some(a)) => a,
            (Existing::Ask, None) if !interactive => {
                plan.push(format!(
                    "{} {k} {}",
                    ui::yellow("?"),
                    ui::dim("set already: --overwrite or --skip-existing")
                ));
                continue;
            }
            (Existing::Ask, None) => {
                loop {
                    let what = if was_secret { "a secret" } else { "a variable" };
                    match ui::ask(&format!("{} is {what} already. [o]verwrite, [s]kip, overwrite [a]ll, skip all [n]: ", ui::bold(k)))?.as_str() {
                    "o" | "O" => break true,
                    "s" | "S" => break false,
                    "a" | "A" => {
                        all = Some(true);
                        break true;
                    }
                    "n" | "N" => {
                        all = Some(false);
                        break false;
                    }
                    _ => println!("{}", ui::yellow("o, s, a or n.")),
                }
                }
            }
        };
        if overwrite {
            let e = &mut env[i];
            e["value"] = json!(v);
            e["updatedAt"] = json!(now_ms());
            let secret = secret || was_secret;
            e["secret"] = json!(secret);
            updated += 1;
            plan.push(format!(
                "{} {k}{}",
                ui::yellow("~"),
                if secret {
                    ui::dim(" (secret)")
                } else {
                    String::new()
                }
            ));
        } else {
            skipped += 1;
            plan.push(format!("{} {k} {}", ui::dim("-"), ui::dim("skipped")));
        }
    }

    let mut summary = vec![];
    for (n, what) in [
        (added, "added"),
        (updated, "updated"),
        (skipped, "skipped"),
        (same, "unchanged"),
    ] {
        if n > 0 {
            summary.push(format!("{what} {n}"));
        }
    }
    let summary = summary.join(", ");
    if dry_run {
        for p in &plan {
            println!("{p}");
        }
        println!(
            "{}",
            ui::dim(&format!("Dry run, nothing saved: {summary}."))
        );
        return Ok(());
    }
    let summary = summary[..1].to_uppercase() + &summary[1..];
    if added + updated == 0 {
        ui::ok(&format!("Nothing to change. {summary}."));
        return Ok(());
    }
    save(api, &t, service, deploy_now, &summary).await
}

async fn save(api: &Api, t: &Target, service: Value, deploy_now: bool, what: &str) -> Result<()> {
    if deploy_now {
        let dep = new_id("dpl");
        api.op(
            "saveService",
            json!({ "service": service, "deploymentId": dep, "trigger": "manual" }),
        )
        .await?;
        ui::ok(what);
        watch(api, &dep).await
    } else {
        api.op("saveService", json!({ "service": service })).await?;
        ui::ok(&format!(
            "{what}. It applies on the next deploy: pyo deploy"
        ));
        let _ = t;
        Ok(())
    }
}

// ── rollback, restart, open ────────────────────────────────────────────

pub async fn rollback(api: &Api, service: Option<&str>, to: Option<String>) -> Result<()> {
    let t = target(api, service).await?;
    let deps = t.state.deployments_of(&t.id);
    let current = t.service()["currentDeploymentId"].as_str().unwrap_or("");
    let target_dep = match to {
        Some(id) => deps
            .iter()
            .find(|d| d["id"] == json!(id))
            .copied()
            .with_context(|| format!("No deployment {id} for this service."))?,
        None => {
            let cur_at = deps
                .iter()
                .find(|d| d["id"] == json!(current))
                .and_then(|d| d["createdAt"].as_i64())
                .unwrap_or(i64::MAX);
            deps.iter()
                .find(|d| {
                    d["status"] == json!("succeeded")
                        && d["id"] != json!(current)
                        && d["createdAt"].as_i64().unwrap_or(0) < cur_at
                })
                .copied()
                .context("No earlier version to roll back to.")?
        }
    };
    let dep = new_id("dpl");
    api.op("deploy", json!({ "serviceId": t.id, "deploymentId": dep, "trigger": "rollback", "rollbackOf": target_dep["id"] })).await?;
    println!(
        "Rolling back to {}",
        ui::bold(
            target_dep["revisionLabel"]
                .as_str()
                .filter(|s| !s.is_empty())
                .unwrap_or("the previous version")
        )
    );
    watch(api, &dep).await
}

pub async fn restart(api: &Api, service: Option<&str>) -> Result<()> {
    let t = target(api, service).await?;
    api.op("restartService", json!({ "id": t.id })).await?;
    ui::ok(&format!(
        "Restarting {}, one replica at a time",
        t.service()["name"].as_str().unwrap_or("")
    ));
    Ok(())
}

pub async fn open(api: &Api, service: Option<&str>) -> Result<()> {
    let t = target(api, service).await?;
    let url = t.state.service_url(&api.base, t.service());
    println!("{url}");
    ui::open(&url);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn service() -> Value {
        json!({
            "source": { "type": "git", "branch": "main", "dockerfilePath": "Dockerfile", "buildContext": "." },
            "replicas": 1,
            "ports": [{ "id": "port_x", "name": "http", "containerPort": 3000, "protocol": "HTTP", "exposure": "http" }],
            "healthCheck": { "type": "none", "path": "", "port": 3000, "command": "", "initialDelaySeconds": 5, "periodSeconds": 10 },
            "resources": { "cpuRequest": 250, "cpuLimit": 1000, "memoryRequest": 256, "memoryLimit": 512 },
            "env": [{ "id": "env_a", "key": "TOKEN", "value": "", "secret": true, "hasValue": true }],
        })
    }

    #[test]
    fn stages_in_words() {
        assert_eq!(stage_label("waiting_ready"), "Running health check");
        assert_eq!(stage_label("pulling"), "Downloading image");
        assert_eq!(stage_label("some_new_stage"), "Some new stage");
    }

    #[test]
    fn applies_only_whats_set() {
        let mut a = service();
        let f: ServiceFile = toml::from_str("[deploy]\nreplicas = 1\n").unwrap();
        assert!(apply(&mut a, &f).unwrap().is_empty());

        let f: ServiceFile = toml::from_str(
            "[deploy]\nreplicas = 3\nport = 8080\nhealth = \"/healthz\"\n[resources]\nmemory = \"128Mi\"\n[env]\nTOKEN = \"x\"\nLOG = \"info\"\n",
        )
        .unwrap();
        let c = apply(&mut a, &f).unwrap();
        assert_eq!(a["replicas"], json!(3));
        assert_eq!(a["ports"][0]["containerPort"], json!(8080));
        assert_eq!(a["healthCheck"]["port"], json!(8080));
        assert_eq!(a["resources"]["memoryLimit"], json!(128));
        assert_eq!(a["resources"]["memoryRequest"], json!(128));
        // A secret isn't overwritten from the file.
        assert_eq!(a["env"][0]["value"], json!(""));
        assert_eq!(a["env"][1]["key"], json!("LOG"));
        assert_eq!(c.len(), 5);
    }

    #[test]
    fn health_can_be_tcp_or_none() {
        let mut a = service();
        a["healthCheck"]["auto"] = json!("static");
        let f: ServiceFile = toml::from_str("[deploy]\nhealth = \"tcp\"\n").unwrap();
        apply(&mut a, &f).unwrap();
        assert_eq!(a["healthCheck"]["type"], json!("tcp"));
        assert_eq!(a["healthCheck"]["port"], json!(3000));
        assert!(a["healthCheck"].get("auto").is_none());
        let f: ServiceFile = toml::from_str("[deploy]\nhealth = \"none\"\n").unwrap();
        apply(&mut a, &f).unwrap();
        assert_eq!(a["healthCheck"]["type"], json!("none"));
        let f: ServiceFile = toml::from_str("[deploy]\nhealth = \"healthz\"\n").unwrap();
        assert!(apply(&mut a, &f).is_err());
    }

    #[test]
    fn sets_build_arguments() {
        let mut a = service();
        let f: ServiceFile =
            toml::from_str("[build.args]\nRAILPACK_SPA_OUTPUT_DIR = \"dist\"\nNODE_VERSION = 22\n")
                .unwrap();
        let c = apply(&mut a, &f).unwrap();
        assert_eq!(c.len(), 2);
        assert_eq!(
            a["source"]["buildArgs"],
            json!([{ "key": "NODE_VERSION", "value": "22" }, { "key": "RAILPACK_SPA_OUTPUT_DIR", "value": "dist" }])
        );
        // Again: nothing changes.
        assert!(apply(&mut a, &f).unwrap().is_empty());
        let mut img = service();
        img["source"]["type"] = json!("image");
        assert!(apply(&mut img, &f).is_err());
    }

    #[test]
    fn only_a_yes_confirms() {
        for y in ["y", "Y", "yes", " YES "] {
            assert!(confirmed(y), "{y}");
        }
        for n in ["", "n", "no", "maybe"] {
            assert!(!confirmed(n), "{n}");
        }
    }

    #[test]
    fn replicas_have_no_cap_but_kubernetes_limit() {
        let mut a = service();
        let f: ServiceFile = toml::from_str("[deploy]\nreplicas = 500\n").unwrap();
        apply(&mut a, &f).unwrap();
        assert_eq!(a["replicas"], json!(500));
        let f: ServiceFile = toml::from_str("[deploy]\nreplicas = -1\n").unwrap();
        assert!(apply(&mut a, &f).is_err());
        let f: ServiceFile = toml::from_str("[deploy]\nreplicas = 3000000000\n").unwrap();
        assert!(apply(&mut a, &f).is_err());
    }

    #[test]
    fn image_names() {
        assert_eq!(
            split_image("ghcr.io/a/b:1.2"),
            ("ghcr.io/a/b".into(), "1.2".into())
        );
        assert_eq!(
            split_image("localhost:5000/b"),
            ("localhost:5000/b".into(), "latest".into())
        );
        assert_eq!(
            split_image("b@sha256:abc"),
            ("b".into(), "sha256:abc".into())
        );
    }

    #[test]
    fn names_match_from_the_end() {
        assert!(matches("acme/shop/api", "api"));
        assert!(matches("acme/shop/api", "shop/api"));
        assert!(!matches("acme/shop/api", "pi"));
    }
}
