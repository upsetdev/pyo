//! `pyo sandbox`: isolated machines on the team's sandbox servers. Create,
//! list, pause and delete them through Pyo's API, one command per endpoint.
//! `exec`, `shell` and `cp` then talk to the sandbox's server directly with
//! the client token Pyo signs (`createSandboxToken`), as the SDK does. The
//! server answers on its tailnet address, so those need the tailnet.

use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::Subcommand;
use futures::{SinkExt, StreamExt};
use pyo_proto::sandbox::wire::{Cmd, Frame, Kind, PtySpec, Request, Response};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_tungstenite::tungstenite::{Message, client::IntoClientRequest, http::HeaderValue};

use crate::{api::Api, ui};

#[derive(Subcommand)]
pub enum Command {
    /// Make a sandbox from a sandbox template and print its id.
    Create {
        /// The sandbox template, like node-22 (see pyo sandbox templates ls).
        /// Default base: Debian 13 with git, Python, uv, Node.js and pnpm.
        /// desktop adds a desktop and Chromium: CDP on port 9222, the screen
        /// on port 6080. A server makes these the first time they're used
        /// there.
        template: Option<String>,
        /// A name to tell it apart. Pyo makes up a short one when left out.
        #[arg(long)]
        name: Option<String>,
        /// The sandbox server to run it on, by name or id. Pyo picks one
        /// when left out.
        #[arg(long)]
        server: Option<String>,
        /// CPUs. Default 1 (2 for desktop).
        #[arg(long)]
        cpus: Option<u32>,
        /// Memory in MiB. Default 512 (2048 for desktop).
        #[arg(long)]
        memory: Option<u32>,
        /// Writable disk in MiB. Default 2048.
        #[arg(long)]
        disk: Option<u32>,
        /// How long it lives, like 30m, 1h or 7d, up to 365d. "none" keeps it
        /// until you delete it. Default 5m.
        #[arg(long)]
        ttl: Option<String>,
        /// Pause after this many seconds without use.
        #[arg(long, value_name = "SECS")]
        pause_when_idle: Option<u64>,
        /// An environment variable, KEY=VALUE. Repeat for more.
        #[arg(long = "env", short = 'e', value_name = "KEY=VALUE")]
        env: Vec<String>,
        /// A label, key=value. Repeat for more.
        #[arg(long = "label", value_name = "KEY=VALUE")]
        labels: Vec<String>,
        /// internet or none.
        #[arg(long, value_parser = ["internet", "none"])]
        network: Option<String>,
        /// auto (Firecracker where the server has KVM, gVisor elsewhere) or vm (Firecracker only).
        #[arg(long, value_parser = ["auto", "vm"])]
        isolation: Option<String>,
        /// A project of the team (id), as a label.
        #[arg(long)]
        project: Option<String>,
        /// Print the sandbox and its connection as JSON.
        #[arg(long)]
        json: bool,
    },
    /// The team's sandboxes.
    Ls {
        /// Ended ones too (the latest 500).
        #[arg(long, short)]
        all: bool,
        /// Print it as JSON.
        #[arg(long)]
        json: bool,
    },
    /// One sandbox.
    Get {
        /// The sandbox's id, sbx_….
        id: String,
        /// Print it as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Give a sandbox another name. Its id stays the same.
    Rename {
        /// The sandbox's id, sbx_….
        id: String,
        /// The new name.
        name: String,
    },
    /// Delete a sandbox and its files.
    Rm {
        /// The sandbox's id, sbx_….
        id: String,
    },
    /// Pause a sandbox. It resumes where it was.
    Pause {
        /// The sandbox's id, sbx_….
        id: String,
    },
    /// Resume a paused sandbox.
    Resume {
        /// The sandbox's id, sbx_….
        id: String,
    },
    /// Make a sandbox end this long from now.
    Extend {
        /// The sandbox's id, sbx_….
        id: String,
        /// Like 30m, 1h or 7d. "none" keeps it until you delete it.
        ttl: String,
    },
    /// A new token for the sandbox's server, valid an hour.
    Token {
        /// The sandbox's id, sbx_….
        id: String,
        /// Print it as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Run a command in a sandbox and print its output. Exits with its code.
    Exec {
        /// The sandbox's id, sbx_….
        id: String,
        /// One argument runs with /bin/sh -c, more run as they are.
        #[arg(required = true, trailing_var_arg = true, allow_hyphen_values = true)]
        command: Vec<String>,
        /// The working directory.
        #[arg(long)]
        cwd: Option<String>,
        /// Kill the command after this many seconds. Default 60.
        #[arg(long, value_name = "SECS")]
        timeout: Option<u64>,
    },
    /// A terminal in a sandbox.
    Shell {
        /// The sandbox's id, sbx_….
        id: String,
    },
    /// Copy a file into or out of a sandbox: <ID>:<PATH> on one side. Up to 64 MB.
    Cp {
        /// A local file, - for stdin, or <ID>:<PATH>.
        from: String,
        /// A local file or folder, - for stdout, or <ID>:<PATH>.
        to: String,
    },
    /// Serve a sandbox's ports at their own URLs.
    Ports {
        #[command(subcommand)]
        command: PortsCommand,
    },
    /// The servers sandboxes run on.
    Servers {
        #[command(subcommand)]
        command: ServersCommand,
    },
    /// What sandboxes start from.
    Templates {
        #[command(subcommand)]
        command: TemplatesCommand,
    },
}

#[derive(Subcommand)]
pub enum PortsCommand {
    /// Serve a port the sandbox listens on and print its URL.
    Open {
        /// The sandbox's id, sbx_….
        id: String,
        /// The port, like 3000.
        port: u16,
        /// Only answer with a sandbox token.
        #[arg(long)]
        private: bool,
    },
    /// Stop serving a port.
    Close {
        /// The sandbox's id, sbx_….
        id: String,
        /// The port, like 3000.
        port: u16,
    },
    /// The sandbox's open ports.
    Ls {
        /// The sandbox's id, sbx_….
        id: String,
        /// Print them as JSON.
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
pub enum ServersCommand {
    /// Add a sandbox server: prints the command to run on it. Owners and admins.
    Add {
        /// Its name, lowercase letters, digits and dashes.
        name: String,
        /// Serve sandbox ports at <port>-<sandbox>.DOMAIN. Point *.DOMAIN at the server.
        #[arg(long, value_name = "DOMAIN")]
        edge_domain: Option<String>,
    },
    /// Buy a sandbox server at Hetzner. Pyo sets it up over your tailnet,
    /// and it runs sandboxes on gVisor. Owners and admins.
    Create {
        /// Its name, lowercase letters, digits and dashes.
        name: String,
        /// A location, e.g. fsn1 (see pyo server types).
        #[arg(long)]
        location: String,
        /// A server type, e.g. cx22 (see pyo server types).
        #[arg(long = "type")]
        server_type: String,
        /// Serve sandbox ports at <port>-<sandbox>.DOMAIN. Point *.DOMAIN at the server.
        #[arg(long, value_name = "DOMAIN")]
        edge_domain: Option<String>,
        /// Your SSH public key file, to sign in over your tailnet.
        #[arg(long, value_name = "FILE")]
        ssh_key: Option<PathBuf>,
        /// Don't ask to confirm the price.
        #[arg(long, short = 'y')]
        yes: bool,
        /// Start it and exit, without following it.
        #[arg(long)]
        detach: bool,
    },
    /// The sandbox servers the team can use.
    Ls {
        /// Print it as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Remove a sandbox server (name or id). Its sandboxes end.
    Rm {
        /// Its name or id.
        server: String,
        /// Don't ask.
        #[arg(long, short = 'y')]
        yes: bool,
    },
}

#[derive(Subcommand)]
pub enum TemplatesCommand {
    /// The sandbox templates the team can use.
    Ls {
        /// Print it as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Make a sandbox template from an image.
    Create {
        /// Lowercase letters, digits, dashes, dots and underscores, like node-22.
        name: String,
        /// The image, like node:22.
        image: String,
    },
}

pub async fn run(api: &Api, team: Option<&str>, cmd: Command) -> Result<()> {
    match cmd {
        Command::Create {
            template,
            name,
            server,
            cpus,
            memory,
            disk,
            ttl,
            pause_when_idle,
            env,
            labels,
            network,
            isolation,
            project,
            json,
        } => {
            let team = team_id(api, team).await?;
            let body = create_body(CreateArgs {
                team: &team,
                template: template.as_deref(),
                name: name.as_deref(),
                server: server.as_deref(),
                cpus,
                memory,
                disk,
                ttl: ttl.as_deref(),
                pause_when_idle,
                env: &env,
                labels: &labels,
                network: network.as_deref(),
                isolation: isolation.as_deref(),
                project: project.as_deref(),
            })?;
            let r = api.post("/api/v1/sandboxes", &body).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&r)?);
            } else {
                println!("{}", r["sandbox"]["id"].as_str().unwrap_or(""));
            }
            Ok(())
        }
        Command::Ls { all, json } => {
            let team = team_id(api, team).await?;
            let state = if all { "all" } else { "live" };
            let r = api
                .get(&format!("/api/v1/sandboxes?team={team}&state={state}"))
                .await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&r["sandboxes"])?);
            } else {
                print_sandboxes(&r["sandboxes"]);
            }
            Ok(())
        }
        Command::Get { id, json } => {
            let r = api.get(&format!("/api/v1/sandboxes/{}", seg(&id))).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&r)?);
            } else {
                print_sandbox(&r);
            }
            Ok(())
        }
        Command::Rename { id, name } => {
            let r = api
                .patch(
                    &format!("/api/v1/sandboxes/{}", seg(&id)),
                    &json!({ "name": name }),
                )
                .await?;
            ui::ok(&format!(
                "{id} is now {}.",
                r["name"].as_str().unwrap_or(&name)
            ));
            Ok(())
        }
        Command::Rm { id } => {
            let r = api
                .delete(&format!("/api/v1/sandboxes/{}", seg(&id)))
                .await?;
            ui::ok(&format!(
                "{id} is {}.",
                r["state"].as_str().unwrap_or("stopping")
            ));
            Ok(())
        }
        Command::Pause { id } => lifecycle(api, &id, "pause", "Paused").await,
        Command::Resume { id } => lifecycle(api, &id, "resume", "Resumed").await,
        Command::Extend { id, ttl } => {
            let r = api
                .post(
                    &format!("/api/v1/sandboxes/{}/extend", seg(&id)),
                    &json!({ "ttl": ttl }),
                )
                .await?;
            ui::ok(&match r["expiresAt"].as_i64() {
                Some(at) => format!("{id} ends in {}.", left(Some(at), crate::api::now_ms())),
                None => format!("{id} runs until you delete it."),
            });
            Ok(())
        }
        Command::Token { id, json } => {
            let c = connection(api, &id).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&c.raw)?);
            } else {
                println!("{}", c.token);
            }
            Ok(())
        }
        Command::Exec {
            id,
            command,
            cwd,
            timeout,
        } => {
            let c = connection(api, &id).await?;
            let mut body = json!({ "cmd": exec_cmd(command) });
            if let Some(cwd) = cwd {
                body["cwd"] = json!(cwd);
            }
            if let Some(t) = timeout {
                body["timeoutMs"] = json!(t * 1000);
            }
            let r = c.call(reqwest::Method::POST, "/exec", &[]).json(&body);
            let r = daemon_json(r).await?;
            print!("{}", r["stdout"].as_str().unwrap_or(""));
            eprint!("{}", r["stderr"].as_str().unwrap_or(""));
            if r["truncated"] == json!(true) {
                eprintln!("{}", ui::yellow("(output cut short)"));
            }
            if r["timedOut"] == json!(true) {
                bail!("The command timed out.");
            }
            let code = exit_code(&r);
            if code != 0 {
                use std::io::Write;
                let _ = std::io::stdout().flush();
                std::process::exit(code);
            }
            Ok(())
        }
        Command::Shell { id } => shell(api, &id).await,
        Command::Cp { from, to } => cp(api, &from, &to).await,
        Command::Ports { command } => ports(api, command).await,
        Command::Servers { command } => servers(api, team, command).await,
        Command::Templates { command } => templates(api, team, command).await,
    }
}

// ── Pyo's API ──────────────────────────────────────────────────────────────

/// A path segment: ids are `sbx_…`, but nothing else reaches the URL as is.
fn seg(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-'))
        .collect()
}

async fn team_id(api: &Api, team: Option<&str>) -> Result<String> {
    let state = api.state().await?;
    let teams = state.teams();
    let t = match team {
        Some(t) => teams
            .iter()
            .find(|x| x["slug"] == json!(t) || x["id"] == json!(t))
            .with_context(|| format!("No team {t} among yours"))?,
        None => teams.first().context("You aren't in any team")?,
    };
    Ok(t["id"].as_str().unwrap_or("").to_owned())
}

async fn lifecycle(api: &Api, id: &str, what: &str, done: &str) -> Result<()> {
    api.post(&format!("/api/v1/sandboxes/{}/{what}", seg(id)), &json!({}))
        .await?;
    ui::ok(&format!("{done} {id}."));
    Ok(())
}

pub struct CreateArgs<'a> {
    pub team: &'a str,
    pub template: Option<&'a str>,
    pub name: Option<&'a str>,
    pub server: Option<&'a str>,
    pub cpus: Option<u32>,
    pub memory: Option<u32>,
    pub disk: Option<u32>,
    pub ttl: Option<&'a str>,
    pub pause_when_idle: Option<u64>,
    pub env: &'a [String],
    pub labels: &'a [String],
    pub network: Option<&'a str>,
    pub isolation: Option<&'a str>,
    pub project: Option<&'a str>,
}

/// The createSandbox body. Left-out values take the API's defaults.
pub fn create_body(a: CreateArgs) -> Result<Value> {
    let mut b = json!({ "teamId": a.team });
    let mut set = |k: &str, v: Value| {
        if !v.is_null() {
            b[k] = v;
        }
    };
    set("template", json!(a.template));
    set("name", json!(a.name));
    set("server", json!(a.server));
    set("cpus", json!(a.cpus));
    set("memoryMib", json!(a.memory));
    set("diskMib", json!(a.disk));
    set("ttl", json!(a.ttl));
    set("pauseWhenIdleSecs", json!(a.pause_when_idle));
    set("network", json!(a.network));
    set("isolation", json!(a.isolation));
    set("projectId", json!(a.project));
    if !a.env.is_empty() {
        set("env", json!(pairs(a.env, "--env")?));
    }
    if !a.labels.is_empty() {
        set("labels", json!(pairs(a.labels, "--label")?));
    }
    Ok(b)
}

fn pairs(items: &[String], flag: &str) -> Result<BTreeMap<String, String>> {
    items
        .iter()
        .map(|p| match p.split_once('=') {
            Some((k, v)) if !k.is_empty() => Ok((k.to_owned(), v.to_owned())),
            _ => bail!("{flag} takes KEY=VALUE, not {p:?}."),
        })
        .collect()
}

/// "45m", "2h 10m", or "ended".
/// Time left until `at`. None: the sandbox has no lifetime.
fn left(at: Option<i64>, now: i64) -> String {
    let Some(at) = at else {
        return "never".into();
    };
    let s = (at - now) / 1000;
    match s {
        ..=0 => "ended".into(),
        1..60 => format!("{s}s"),
        60..3600 => format!("{}m", s / 60),
        3600..172800 if s % 3600 / 60 == 0 => format!("{}h", s / 3600),
        3600..172800 => format!("{}h {}m", s / 3600, s % 3600 / 60),
        _ => format!("{}d", s / 86400),
    }
}

fn size(v: &Value) -> String {
    let mib = v["memoryMib"].as_u64().unwrap_or(0);
    let mem = if mib >= 1024 {
        format!("{} GB", mib as f64 / 1024.0)
    } else {
        format!("{mib} MB")
    };
    format!("{} CPU, {mem}", v["cpus"].as_u64().unwrap_or(0))
}

fn print_sandboxes(list: &Value) {
    let list = list.as_array().map(Vec::as_slice).unwrap_or(&[]);
    if list.is_empty() {
        println!("No sandboxes. Make one: pyo sandbox create");
        return;
    }
    let now = crate::api::now_ms();
    let rows: Vec<[String; 7]> = list
        .iter()
        .map(|x| {
            let s = |k: &str| x[k].as_str().unwrap_or("").to_owned();
            let ended = matches!(x["state"].as_str(), Some("stopped" | "failed"));
            [
                s("id"),
                s("name"),
                s("template"),
                s("state"),
                size(x),
                if ended {
                    "-".into()
                } else {
                    left(x["expiresAt"].as_i64(), now)
                },
                ui::ago(x["createdAt"].as_i64().unwrap_or(now)),
            ]
        })
        .collect();
    table(
        &["ID", "NAME", "TEMPLATE", "STATE", "SIZE", "LEFT", "CREATED"],
        &rows,
    );
}

fn table<const N: usize>(head: &[&str; N], rows: &[[String; N]]) {
    let mut w = head.map(str::len);
    for r in rows {
        for (i, c) in r.iter().enumerate() {
            w[i] = w[i].max(c.chars().count());
        }
    }
    let line = |cells: Vec<&str>| {
        cells
            .iter()
            .enumerate()
            .map(|(i, c)| format!("{c:<w$}", w = w[i]))
            .collect::<Vec<_>>()
            .join("  ")
            .trim_end()
            .to_owned()
    };
    println!("{}", ui::dim(&line(head.to_vec())));
    for r in rows {
        println!("{}", line(r.iter().map(String::as_str).collect()));
    }
}

fn print_sandbox(x: &Value) {
    let now = crate::api::now_ms();
    let s = |k: &str| x[k].as_str().unwrap_or("-").to_owned();
    let env: Vec<&str> = x["env"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    let labels: Vec<String> = x["labels"]
        .as_object()
        .into_iter()
        .flatten()
        .map(|(k, v)| format!("{k}={}", v.as_str().unwrap_or("")))
        .collect();
    let rows = [
        ("id", s("id")),
        ("name", s("name")),
        ("state", s("state")),
        ("template", s("template")),
        ("size", size(x)),
        (
            "ends in",
            if matches!(x["state"].as_str(), Some("stopped" | "failed")) {
                "-".into()
            } else {
                left(x["expiresAt"].as_i64(), now)
            },
        ),
        ("server", s("server")),
        ("runtime", s("runtime")),
        ("network", s("network")),
        ("address", s("url")),
        ("env", env.join(", ")),
        ("labels", labels.join(", ")),
        ("created by", s("createdBy")),
    ];
    for (k, v) in rows {
        if !v.is_empty() {
            println!("{}  {v}", ui::dim(&format!("{k:<10}")));
        }
    }
    if let Some(e) = x["error"].as_str() {
        println!("{}", ui::red(e));
    }
}

// ── Ports ──────────────────────────────────────────────────────────────────

async fn ports(api: &Api, cmd: PortsCommand) -> Result<()> {
    match cmd {
        PortsCommand::Open { id, port, private } => {
            let r = api
                .post(
                    &format!("/api/v1/sandboxes/{}/ports", seg(&id)),
                    &json!({ "port": port, "public": !private }),
                )
                .await?;
            println!("{}", r["url"].as_str().unwrap_or(""));
            Ok(())
        }
        PortsCommand::Close { id, port } => {
            api.delete(&format!("/api/v1/sandboxes/{}/ports/{port}", seg(&id)))
                .await?;
            ui::ok(&format!("Closed port {port} of {id}."));
            Ok(())
        }
        PortsCommand::Ls { id, json } => {
            let r = api.get(&format!("/api/v1/sandboxes/{}", seg(&id))).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&r["ports"])?);
                return Ok(());
            }
            let list = r["ports"].as_array().map(Vec::as_slice).unwrap_or(&[]);
            if list.is_empty() {
                println!("No open ports. Open one: pyo sandbox ports open {id} <port>");
                return Ok(());
            }
            let rows: Vec<[String; 3]> = list
                .iter()
                .map(|p| {
                    [
                        p["port"].as_u64().unwrap_or(0).to_string(),
                        if p["public"] == json!(false) {
                            "private"
                        } else {
                            "public"
                        }
                        .to_owned(),
                        p["url"].as_str().unwrap_or("-").to_owned(),
                    ]
                })
                .collect();
            table(&["PORT", "ACCESS", "URL"], &rows);
            Ok(())
        }
    }
}

// ── Servers and templates ──────────────────────────────────────────────────

async fn servers(api: &Api, team: Option<&str>, cmd: ServersCommand) -> Result<()> {
    let team = team_id(api, team).await?;
    match cmd {
        ServersCommand::Add { name, edge_domain } => {
            let mut body = json!({ "teamId": team, "name": name });
            if let Some(d) = edge_domain {
                body["edgeDomain"] = json!(d);
            }
            let r = api.post("/api/v1/sandbox-servers", &body).await?;
            let mins = (r["expiresAt"].as_i64().unwrap_or(0) - crate::api::now_ms()) / 60_000;
            println!("Run this on the server as root. It works once, for {mins} minutes:\n");
            println!("{}", r["command"].as_str().unwrap_or(""));
            Ok(())
        }
        ServersCommand::Create {
            name,
            location,
            server_type,
            edge_domain,
            ssh_key,
            yes,
            detach,
        } => {
            let ssh_key = match ssh_key {
                Some(p) => Some(
                    std::fs::read_to_string(&p)
                        .with_context(|| format!("Can't read {}", p.display()))?
                        .trim()
                        .to_owned(),
                ),
                None => None,
            };
            let mut body = json!({
                "teamId": team, "name": name, "location": location,
                "serverType": server_type, "sshKey": ssh_key,
            });
            if let Some(d) = edge_domain {
                body["edgeDomain"] = json!(d);
            }
            let path = "/api/v1/sandbox-servers/cloud";
            let r = match api.post(path, &body).await {
                Err(e) if e.downcast_ref::<crate::api::NeedsConfirm>().is_some() => {
                    println!("{e}");
                    if !yes && !crate::server::yes_to("Go ahead? [y/N] ")? {
                        bail!("Nothing was changed.");
                    }
                    body["confirm"] = json!(true);
                    api.post(path, &body).await?
                }
                r => r?,
            };
            ui::ok(&format!(
                "Creating {name} at Hetzner. It runs sandboxes on gVisor."
            ));
            if detach {
                return Ok(());
            }
            let id = r["cloudServer"]["id"].as_str().unwrap_or("").to_owned();
            crate::server::follow(api, &id).await
        }
        ServersCommand::Ls { json } => {
            let r = api
                .get(&format!("/api/v1/sandbox-servers?team={team}"))
                .await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&r["servers"])?);
                return Ok(());
            }
            let list = r["servers"].as_array().map(Vec::as_slice).unwrap_or(&[]);
            if list.is_empty() {
                println!("No sandbox servers. Add one: pyo sandbox servers add <name>");
                return Ok(());
            }
            let rows: Vec<[String; 5]> = list
                .iter()
                .map(|x| {
                    let c = &x["capability"];
                    // A server Pyo is still making at Hetzner says how far it is.
                    let state = match x["cloud"]["state"].as_str() {
                        Some(cs) if cs != "ready" => format!("{cs} (Hetzner)"),
                        _ => x["state"].as_str().unwrap_or("").to_owned(),
                    };
                    [
                        x["name"].as_str().unwrap_or("").to_owned(),
                        state,
                        x["runtime"]
                            .as_str()
                            .or(c["runtime"].as_str())
                            .unwrap_or("-")
                            .to_owned(),
                        match (c["cpus"].as_u64(), c["memory"].as_u64()) {
                            (Some(cpu), Some(m)) => format!("{cpu} CPU, {} GB", m >> 30),
                            _ => "-".into(),
                        },
                        x["sandboxes"].as_u64().unwrap_or(0).to_string(),
                    ]
                })
                .collect();
            table(&["NAME", "STATE", "RUNTIME", "SIZE", "SANDBOXES"], &rows);
            Ok(())
        }
        ServersCommand::Rm { server, yes } => {
            let r = api
                .get(&format!("/api/v1/sandbox-servers?team={team}"))
                .await?;
            let x = r["servers"]
                .as_array()
                .into_iter()
                .flatten()
                .find(|x| x["id"] == json!(server) || x["name"] == json!(server))
                .with_context(|| format!("No sandbox server {server}"))?;
            let name = x["name"].as_str().unwrap_or(&server).to_owned();
            if !yes {
                let n = x["sandboxes"].as_u64().unwrap_or(0);
                let q = format!("Remove {name}? Its {n} sandboxes end. [y/N] ");
                if !matches!(ui::ask(&q)?.trim(), "y" | "Y" | "yes") {
                    bail!("Not removed.");
                }
            }
            let id = x["id"].as_str().unwrap_or("");
            let path = format!("/api/v1/sandbox-servers/{}", seg(id));
            // One Pyo made at Hetzner is deleted there: its price first.
            match api.delete(&path).await {
                Err(e) if e.downcast_ref::<crate::api::NeedsConfirm>().is_some() => {
                    println!("{e}");
                    if !yes && !crate::server::yes_to("Go ahead? [y/N] ")? {
                        bail!("Nothing was changed.");
                    }
                    api.delete(&format!("{path}?confirm=true")).await?;
                    ui::ok(&format!("Deleting {name} at Hetzner."));
                }
                r => {
                    r?;
                    ui::ok(&format!(
                        "Removed {name}. Run sandbox.sh --uninstall on it to take Pyo off."
                    ));
                }
            }
            Ok(())
        }
    }
}

/// A template's image as the list shows it. Pyo's built-in one says what
/// it runs and leaves out its version: Debian 13 · ghcr.io/upsetdev/pyo/sandbox-base.
fn template_image(t: &Value) -> String {
    let image = t["source"]["image"].as_str();
    match (t["source"]["os"].as_str(), image) {
        (Some(os), Some(image)) if t["builtin"] == true => {
            format!("{os} · {}", pyo_proto::image_repo(image))
        }
        (Some(os), None) if t["builtin"] == true => os.to_owned(),
        _ => image.unwrap_or("-").to_owned(),
    }
}

async fn templates(api: &Api, team: Option<&str>, cmd: TemplatesCommand) -> Result<()> {
    let team = team_id(api, team).await?;
    match cmd {
        TemplatesCommand::Ls { json } => {
            let r = api
                .get(&format!("/api/v1/sandbox-templates?team={team}"))
                .await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&r["templates"])?);
                return Ok(());
            }
            let list = r["templates"].as_array().map(Vec::as_slice).unwrap_or(&[]);
            if list.is_empty() {
                println!(
                    "No sandbox templates. Make one: pyo sandbox templates create <name> <image>"
                );
                return Ok(());
            }
            let rows: Vec<[String; 3]> = list
                .iter()
                .map(|t| {
                    [
                        t["name"].as_str().unwrap_or("").to_owned(),
                        template_image(t),
                        t["state"].as_str().unwrap_or("").to_owned(),
                    ]
                })
                .collect();
            table(&["NAME", "IMAGE", "STATE"], &rows);
            Ok(())
        }
        TemplatesCommand::Create { name, image } => {
            api.post(
                "/api/v1/sandbox-templates",
                &json!({ "teamId": team, "name": name, "image": image }),
            )
            .await?;
            ui::ok(&format!("Added {name}. It's ready once Pyo has built it."));
            Ok(())
        }
    }
}

// ── The sandbox server ─────────────────────────────────────────────────────

/// The sandbox's server and a token for it, from createSandboxToken.
struct Connection {
    url: String,
    sandbox: String,
    token: String,
    raw: Value,
    http: reqwest::Client,
}

async fn connection(api: &Api, id: &str) -> Result<Connection> {
    let raw = api
        .post(&format!("/api/v1/sandboxes/{}/token", seg(id)), &json!({}))
        .await?;
    Ok(Connection {
        url: raw["url"]
            .as_str()
            .unwrap_or("")
            .trim_end_matches('/')
            .to_owned(),
        sandbox: raw["sandboxId"].as_str().unwrap_or(id).to_owned(),
        token: raw["token"].as_str().unwrap_or("").to_owned(),
        raw,
        http: reqwest::Client::builder()
            .user_agent(concat!("pyo-cli/", env!("CARGO_PKG_VERSION")))
            .build()?,
    })
}

impl Connection {
    /// A call to `/v1/sandboxes/<id><path>` on the server.
    fn call(
        &self,
        method: reqwest::Method,
        path: &str,
        query: &[(&str, &str)],
    ) -> reqwest::RequestBuilder {
        self.http
            .request(
                method,
                format!("{}/v1/sandboxes/{}{path}", self.url, self.sandbox),
            )
            .query(query)
            .bearer_auth(&self.token)
    }
}

const OFF_TAILNET: &str = "Can't reach the sandbox server. It answers only on your tailnet for now";

async fn daemon_send(req: reqwest::RequestBuilder) -> Result<reqwest::Response> {
    let res = req.send().await.context(OFF_TAILNET)?;
    if res.status().is_success() {
        return Ok(res);
    }
    let status = res.status();
    let body: Value = res.json().await.unwrap_or(Value::Null);
    match body["error"]["message"].as_str() {
        Some(m) => bail!("{m}"),
        None => bail!("The sandbox server answered {status}."),
    }
}

async fn daemon_json(req: reqwest::RequestBuilder) -> Result<Value> {
    Ok(daemon_send(req).await?.json().await?)
}

/// One argument: a shell line. More: the argv as given.
pub fn exec_cmd(mut args: Vec<String>) -> Cmd {
    if args.len() == 1 {
        Cmd::Shell(args.remove(0))
    } else {
        Cmd::Argv(args)
    }
}

/// The command's exit code, 128 + the signal when a signal ended it.
fn exit_code(r: &Value) -> i32 {
    match (r["exitCode"].as_i64(), r["signal"].as_i64()) {
        (Some(c), _) => c as i32,
        (None, Some(s)) => 128 + s as i32,
        _ => 1,
    }
}

/// One side of `cp`.
#[derive(Debug, PartialEq, Eq)]
pub enum Place {
    Local(PathBuf),
    Stdio,
    Sandbox { id: String, path: String },
}

pub fn place(s: &str) -> Place {
    if s == "-" {
        return Place::Stdio;
    }
    match s.split_once(':') {
        Some((id, path)) if id.starts_with("sbx_") && !id.contains('/') => Place::Sandbox {
            id: id.to_owned(),
            path: if path.is_empty() {
                "/".into()
            } else {
                path.to_owned()
            },
        },
        _ => Place::Local(PathBuf::from(s)),
    }
}

/// The file name a copy keeps when it lands in a folder.
fn base_name(path: &str) -> &str {
    path.trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or(path)
}

async fn cp(api: &Api, from: &str, to: &str) -> Result<()> {
    match (place(from), place(to)) {
        (Place::Sandbox { id, path }, dest @ (Place::Local(_) | Place::Stdio)) => {
            let c = connection(api, &id).await?;
            let res =
                daemon_send(c.call(reqwest::Method::GET, "/files", &[("path", &path)])).await?;
            let bytes = res.bytes().await?;
            match dest {
                Place::Stdio => tokio::io::stdout().write_all(&bytes).await?,
                Place::Local(p) => {
                    let p = if p.is_dir() {
                        p.join(base_name(&path))
                    } else {
                        p
                    };
                    tokio::fs::write(&p, &bytes)
                        .await
                        .with_context(|| format!("Can't write {}", p.display()))?;
                    ui::ok(&format!("Copied {} bytes to {}.", bytes.len(), p.display()));
                }
                Place::Sandbox { .. } => unreachable!(),
            }
            Ok(())
        }
        (src @ (Place::Local(_) | Place::Stdio), Place::Sandbox { id, path }) => {
            let (bytes, name) = match &src {
                Place::Local(p) => (
                    tokio::fs::read(p)
                        .await
                        .with_context(|| format!("Can't read {}", p.display()))?,
                    p.file_name().map(|n| n.to_string_lossy().into_owned()),
                ),
                _ => {
                    let mut b = Vec::new();
                    tokio::io::stdin().read_to_end(&mut b).await?;
                    (b, None)
                }
            };
            // A path ending in / is a folder: the file keeps its name.
            let path = match (path.ends_with('/'), name) {
                (true, Some(n)) => format!("{path}{n}"),
                (true, None) => bail!("Give the file's path in the sandbox, not a folder."),
                _ => path,
            };
            let c = connection(api, &id).await?;
            let n = bytes.len();
            daemon_json(
                c.call(reqwest::Method::PUT, "/files", &[("path", &path)])
                    .body(bytes),
            )
            .await?;
            ui::ok(&format!("Copied {n} bytes to {id}:{path}."));
            Ok(())
        }
        (Place::Sandbox { .. }, Place::Sandbox { .. }) => {
            bail!("Copy between two sandboxes through your computer: one side is local.")
        }
        _ => bail!("One side is a sandbox: <ID>:<PATH>, like sbx_abc:/app/main.py."),
    }
}

/// The stream the terminal runs on.
const PTY: u32 = 1;

async fn shell(api: &Api, id: &str) -> Result<()> {
    let c = connection(api, id).await?;
    let ws_url = format!(
        "{}/v1/sandboxes/{}/connect",
        c.url
            .replacen("https://", "wss://", 1)
            .replacen("http://", "ws://", 1),
        c.sandbox
    );
    let mut req = ws_url.into_client_request()?;
    req.headers_mut().insert(
        "authorization",
        HeaderValue::from_str(&format!("Bearer {}", c.token))?,
    );
    let (ws, _) = tokio_tungstenite::connect_async(req)
        .await
        .context(OFF_TAILNET)?;
    let (mut up, mut down) = ws.split();
    let send = |f: Frame| Message::binary(f.encode());

    let raw = crate::exec::Raw::on()?;
    let mut size = crossterm::terminal::size().unwrap_or((80, 24));
    let mut stdin = tokio::io::stdin();
    let mut stdout = tokio::io::stdout();
    let mut buf = [0u8; 4096];
    let mut tick = tokio::time::interval(std::time::Duration::from_millis(500));
    let mut started = false;
    let mut code = 0;
    loop {
        tokio::select! {
            n = stdin.read(&mut buf), if started => {
                let n = n?;
                if n == 0 {
                    up.send(send(Frame::eof(PTY))).await?;
                    continue;
                }
                up.send(send(Frame::new(Kind::Stdin, PTY, buf[..n].to_vec()))).await?;
            }
            m = down.next() => match m {
                Some(Ok(Message::Binary(b))) => {
                    let f = Frame::decode(&b).context("The sandbox server sent a bad frame")?;
                    match f.kind {
                        Kind::Hello => {
                            let pty = Request::Pty(PtySpec { cmd: None, cwd: None, env: BTreeMap::new(), cols: size.0, rows: size.1 });
                            up.send(send(Frame::request(PTY, &pty))).await?;
                        }
                        Kind::Stdout | Kind::Stderr if f.stream == PTY => {
                            stdout.write_all(&f.payload).await?;
                            stdout.flush().await?;
                        }
                        Kind::Response if f.stream == PTY => match f.parse::<Response>()? {
                            Response::Started { .. } => started = true,
                            Response::Exit { code: c, signal } => {
                                code = c.unwrap_or(128 + signal.unwrap_or(0));
                                break;
                            }
                            Response::Error { message, .. } => {
                                drop(raw);
                                bail!(message);
                            }
                            _ => {}
                        },
                        _ => {}
                    }
                }
                // A refused connection closes at once with the reason.
                Some(Ok(Message::Close(Some(c)))) if !started && !c.reason.is_empty() => {
                    drop(raw);
                    bail!("{}", c.reason);
                }
                Some(Ok(Message::Close(_))) | None => break,
                Some(Ok(_)) => {}
                Some(Err(e)) => return Err(e.into()),
            },
            // Follow the window's size (no SIGWINCH on every platform).
            _ = tick.tick(), if started => {
                if let Ok(s) = crossterm::terminal::size() && s != size {
                    size = s;
                    let r = Request::Resize { cols: s.0, rows: s.1 };
                    up.send(send(Frame::request(PTY, &r))).await?;
                }
            }
        }
    }
    drop(raw);
    let _ = up.close().await;
    if code != 0 {
        std::process::exit(code);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_sends_only_what_was_given() {
        let env = vec!["A=1".to_owned(), "B=x=y".to_owned()];
        let b = create_body(CreateArgs {
            team: "tm_1",
            template: Some("node-22"),
            name: Some("checkout tests"),
            server: Some("sandbox-1"),
            cpus: Some(2),
            memory: None,
            disk: None,
            ttl: Some("1h"),
            pause_when_idle: None,
            env: &env,
            labels: &[],
            network: Some("none"),
            isolation: None,
            project: None,
        })
        .unwrap();
        assert_eq!(
            b,
            json!({
                "teamId": "tm_1",
                "template": "node-22",
                "name": "checkout tests",
                "server": "sandbox-1",
                "cpus": 2,
                "ttl": "1h",
                "network": "none",
                "env": { "A": "1", "B": "x=y" },
            })
        );
    }

    #[test]
    fn no_template_leaves_it_to_pyo() {
        let b = create_body(CreateArgs {
            team: "tm_1",
            template: None,
            name: None,
            server: None,
            cpus: None,
            memory: None,
            disk: None,
            ttl: None,
            pause_when_idle: None,
            env: &[],
            labels: &[],
            network: None,
            isolation: None,
            project: None,
        })
        .unwrap();
        assert_eq!(b, json!({ "teamId": "tm_1" }), "Pyo uses base");
    }

    #[test]
    fn the_builtin_template_shows_what_it_runs() {
        let base = json!({ "builtin": true, "source": { "image": "ghcr.io/upsetdev/pyo/sandbox-base:0.4.1@sha256:ab", "os": "Debian 13" } });
        assert_eq!(
            template_image(&base),
            "Debian 13 · ghcr.io/upsetdev/pyo/sandbox-base"
        );
        let own = json!({ "builtin": false, "source": { "image": "node:22" } });
        assert_eq!(template_image(&own), "node:22");
    }

    #[test]
    fn env_and_labels_need_a_key() {
        let bad = vec!["=1".to_owned()];
        let e = create_body(CreateArgs {
            team: "t",
            template: Some("x"),
            name: None,
            server: None,
            cpus: None,
            memory: None,
            disk: None,
            ttl: None,
            pause_when_idle: None,
            env: &[],
            labels: &bad,
            network: None,
            isolation: None,
            project: None,
        })
        .unwrap_err();
        assert!(e.to_string().contains("--label takes KEY=VALUE"), "{e}");
    }

    #[test]
    fn one_argument_is_a_shell_line() {
        assert_eq!(
            exec_cmd(vec!["ls -la | wc -l".into()]),
            Cmd::Shell("ls -la | wc -l".into())
        );
        assert_eq!(
            exec_cmd(vec!["ls".into(), "-la".into()]),
            Cmd::Argv(vec!["ls".into(), "-la".into()])
        );
    }

    #[test]
    fn cp_sides() {
        assert_eq!(
            place("sbx_abc:/app/main.py"),
            Place::Sandbox {
                id: "sbx_abc".into(),
                path: "/app/main.py".into()
            }
        );
        assert_eq!(
            place("sbx_abc:"),
            Place::Sandbox {
                id: "sbx_abc".into(),
                path: "/".into()
            }
        );
        assert_eq!(place("-"), Place::Stdio);
        assert_eq!(place("./main.py"), Place::Local("./main.py".into()));
        // A local path with a colon isn't a sandbox.
        assert_eq!(place("C:/x"), Place::Local("C:/x".into()));
        assert_eq!(place("dir/sbx_a:b"), Place::Local("dir/sbx_a:b".into()));
        assert_eq!(base_name("/app/main.py"), "main.py");
        assert_eq!(base_name("/app/"), "app");
    }

    #[test]
    fn exit_codes() {
        assert_eq!(exit_code(&json!({ "exitCode": 0 })), 0);
        assert_eq!(exit_code(&json!({ "exitCode": 3 })), 3);
        assert_eq!(exit_code(&json!({ "exitCode": null, "signal": 9 })), 137);
    }

    #[test]
    fn time_left() {
        let now = 1_000_000;
        assert_eq!(left(Some(now + 45 * 60_000), now), "45m");
        assert_eq!(left(Some(now + 2 * 3_600_000), now), "2h");
        assert_eq!(left(Some(now + 130 * 60_000), now), "2h 10m");
        assert_eq!(left(Some(now + 3 * 86_400_000), now), "3d");
        assert_eq!(left(Some(now - 1), now), "ended");
        assert_eq!(left(None, now), "never");
    }

    #[test]
    fn ids_stay_one_path_segment() {
        assert_eq!(seg("sbx_abc"), "sbx_abc");
        assert_eq!(seg("../x?y"), "xy");
    }
}
