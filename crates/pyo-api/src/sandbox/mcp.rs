//! The assistants' sandbox tools (mcp.rs lists them only once a team can
//! use a sandbox server). Unlike the SDK, these go through Pyo: it calls
//! the sandbox server with a token for that one sandbox.

use pyo_proto::sandbox::{ExecRequest, ExecResult, wire::Cmd};
use serde_json::{Value, json};

use super::Need;
use crate::{AppState, auth::User, error::ApiError};

/// The tools, in the order they're listed.
pub const TOOLS: [&str; 10] = [
    "create_sandbox",
    "run_code",
    "exec",
    "read_file",
    "write_file",
    "open_port",
    "close_port",
    "rename_sandbox",
    "pause_sandbox",
    "delete_sandbox",
];

/// The most of a file read_file returns.
const READ_CAP: usize = 256 << 10;

fn schema(props: Value, required: &[&str]) -> Value {
    json!({ "type": "object", "properties": props, "required": required })
}

/// The tool definitions.
pub fn defs() -> Vec<Value> {
    let sandbox =
        json!({ "type": "string", "description": "Sandbox id, sbx_… (from create_sandbox)" });
    let team = json!({ "type": "string", "description": "Team slug or id. May be left out when the user has one team" });
    let timeout = json!({ "type": "integer", "minimum": 1, "maximum": 600000, "description": "Kill it after this many milliseconds (60000 when omitted)" });
    let write = json!({ "readOnlyHint": false, "destructiveHint": false, "idempotentHint": false });
    let read = json!({ "readOnlyHint": true, "destructiveHint": false, "idempotentHint": true });
    vec![
        json!({ "name": "create_sandbox", "title": "Create sandbox", "description": "Create an isolated machine (a sandbox) on the team's sandbox servers, to run code, commands and files in. It ends after its lifetime (5 minutes when omitted). Returns its id.", "inputSchema": schema(json!({
            "team": team,
            "name": { "type": "string", "description": "A name to tell it apart, like checkout tests. Need not be unique. A short one like quiet-river when omitted" },
            "template": { "type": "string", "description": "A sandbox template name, like node-22 (list_sandbox_templates shows them). base when omitted: Ubuntu 24.04 with git, Python, uv, Node.js, pnpm, Bun, Deno, the GitHub and Docker CLIs, Claude Code, Codex and opencode. desktop adds a desktop and a browser: drive it over CDP at 127.0.0.1:9222 inside the sandbox, and people see the screen in the dashboard. A server makes base or desktop the first time it's used there, which takes a minute or so" },
            "server": { "type": "string", "description": "A sandbox server's name or id to run it on. Pyo picks one when omitted" },
            "cpus": { "type": "integer", "minimum": 1, "maximum": 32 },
            "memory_mib": { "type": "integer", "minimum": 128 },
            "ttl": { "type": "string", "description": "How long it lives, like 30m, 1h or 7d (365d at most, and within the team's limit). none: it runs until it's deleted, when the team allows it" },
            "env": { "type": "object", "additionalProperties": { "type": "string" } },
            "network": { "type": "string", "enum": ["internet", "none"] },
        }), &[]), "annotations": write }),
        json!({ "name": "run_code", "title": "Run code", "description": "Run a piece of code in a sandbox and wait for it: its output and exit code.", "inputSchema": schema(json!({
            "sandbox_id": sandbox,
            "code": { "type": "string" },
            "language": { "type": "string", "enum": ["python", "node", "bash"], "description": "python when omitted" },
            "timeout_ms": timeout,
        }), &["sandbox_id", "code"]), "annotations": write }),
        json!({ "name": "exec", "title": "Run a command in a sandbox", "description": "Run a shell command in a sandbox and wait for it: its output and exit code.", "inputSchema": schema(json!({
            "sandbox_id": sandbox,
            "command": { "type": "string", "description": "Run with /bin/sh -c" },
            "cwd": { "type": "string" },
            "timeout_ms": timeout,
        }), &["sandbox_id", "command"]), "annotations": write }),
        json!({ "name": "read_file", "title": "Read a file in a sandbox", "description": "A file's content from a sandbox, as text (the first 256 KiB).", "inputSchema": schema(json!({
            "sandbox_id": sandbox,
            "path": { "type": "string", "description": "An absolute path, like /app/index.js" },
        }), &["sandbox_id", "path"]), "annotations": read }),
        json!({ "name": "write_file", "title": "Write a file in a sandbox", "description": "Write text to a file in a sandbox. Its folders are made, and a file there is replaced.", "inputSchema": schema(json!({
            "sandbox_id": sandbox,
            "path": { "type": "string", "description": "An absolute path, like /app/index.js" },
            "content": { "type": "string" },
        }), &["sandbox_id", "path", "content"]), "annotations": { "readOnlyHint": false, "destructiveHint": true, "idempotentHint": true } }),
        json!({ "name": "open_port", "title": "Open a sandbox port", "description": "A URL for a port a sandbox listens on.", "inputSchema": schema(json!({
            "sandbox_id": sandbox,
            "port": { "type": "integer", "minimum": 1, "maximum": 65535 },
            "public": { "type": "boolean", "description": "Anyone can open the URL. False needs a sandbox token. Default true." },
        }), &["sandbox_id", "port"]), "annotations": { "readOnlyHint": false, "destructiveHint": false, "idempotentHint": true } }),
        json!({ "name": "close_port", "title": "Close a sandbox port", "description": "Stop serving a sandbox port at its URL.", "inputSchema": schema(json!({
            "sandbox_id": sandbox,
            "port": { "type": "integer", "minimum": 1, "maximum": 65535 },
        }), &["sandbox_id", "port"]), "annotations": { "readOnlyHint": false, "destructiveHint": true, "idempotentHint": true } }),
        json!({ "name": "rename_sandbox", "title": "Rename sandbox", "description": "Give a sandbox another name. Its id stays the same.", "inputSchema": schema(json!({
            "sandbox_id": sandbox,
            "name": { "type": "string", "description": "Up to 63 letters, digits, spaces, dashes, underscores and dots" },
        }), &["sandbox_id", "name"]), "annotations": { "readOnlyHint": false, "destructiveHint": false, "idempotentHint": true } }),
        json!({ "name": "pause_sandbox", "title": "Pause sandbox", "description": "Pause a sandbox: its memory and files are kept, and it resumes where it was.", "inputSchema": schema(json!({ "sandbox_id": sandbox }), &["sandbox_id"]), "annotations": { "readOnlyHint": false, "destructiveHint": false, "idempotentHint": true } }),
        json!({ "name": "delete_sandbox", "title": "Delete sandbox", "description": "End a sandbox and throw away its files.", "inputSchema": schema(json!({ "sandbox_id": sandbox }), &["sandbox_id"]), "annotations": { "readOnlyHint": false, "destructiveHint": true, "idempotentHint": true } }),
    ]
}

fn err(e: ApiError) -> String {
    e.message
}

fn arg<'a>(args: &'a Value, key: &str) -> Result<&'a str, String> {
    args[key]
        .as_str()
        .filter(|v| !v.is_empty())
        .ok_or_else(|| format!("{key} is required"))
}

/// The team a tool names, else the user's only one (as the API does).
async fn team_id(s: &AppState, user: &User, args: &Value) -> Result<String, String> {
    let ids = super::teams_of(&s.db, user)
        .await
        .map_err(|e| e.to_string())?;
    let Some(t) = args["team"].as_str().filter(|t| !t.is_empty()) else {
        return super::only_team(&ids, "team");
    };
    let teams: Vec<(String, String)> =
        sqlx::query_as("SELECT id, slug FROM pyo.teams WHERE id = ANY($1) ORDER BY created_at")
            .bind(&ids)
            .fetch_all(&s.db)
            .await
            .map_err(|e| e.to_string())?;
    teams
        .iter()
        .find(|(id, slug)| id == t || slug == t)
        .map(|(id, _)| id.clone())
        .ok_or_else(|| format!("No team {t} among yours"))
}

fn exec_out(r: ExecResult) -> Value {
    json!({
        "exitCode": r.exit_code, "stdout": r.stdout, "stderr": r.stderr,
        "timedOut": r.timed_out, "truncated": r.truncated, "durationMs": r.duration_ms,
    })
}

async fn exec(
    s: &AppState,
    user: &User,
    args: &Value,
    cmd: Cmd,
    stdin: Option<String>,
) -> Result<Value, String> {
    let f = super::find(s, user, arg(args, "sandbox_id")?, Need::Use)
        .await
        .map_err(err)?;
    let (d, _) = f.daemon().map_err(err)?;
    let req = ExecRequest {
        cmd,
        cwd: args["cwd"].as_str().map(str::to_owned),
        env: Default::default(),
        stdin,
        timeout_ms: Some(args["timeout_ms"].as_u64().unwrap_or(60_000).min(600_000)),
    };
    d.exec(&f.id, &req)
        .await
        .map(exec_out)
        .map_err(|e| super::daemon_error(e).message)
}

/// Runs a sandbox tool.
pub async fn run(s: &AppState, user: &User, name: &str, args: &Value) -> Result<Value, String> {
    match name {
        "create_sandbox" => {
            let team = team_id(s, user, args).await?;
            let mut req = json!({ "teamId": team });
            for (from, to) in [
                ("name", "name"),
                ("template", "template"),
                ("server", "server"),
                ("cpus", "cpus"),
                ("memory_mib", "memoryMib"),
                ("ttl", "ttl"),
                ("env", "env"),
                ("network", "network"),
            ] {
                if !args[from].is_null() {
                    req[to] = args[from].clone();
                }
            }
            let req = serde_json::from_value(req).map_err(|e| format!("Invalid arguments: {e}"))?;
            let out = super::create(s, user, req).await.map_err(err)?;
            let x = &out["sandbox"];
            Ok(
                json!({ "id": x["id"], "name": x["name"], "template": x["template"], "state": x["state"], "runtime": x["runtime"], "expiresAt": x["expiresAt"] }),
            )
        }
        "run_code" => {
            let argv: &[&str] = match args["language"].as_str().unwrap_or("python") {
                "python" => &["python3", "-"],
                "node" => &["node", "-"],
                "bash" => &["bash", "-s"],
                other => {
                    return Err(format!(
                        "{other} isn't a language run_code knows. Use python, node or bash."
                    ));
                }
            };
            let code = arg(args, "code")?.to_owned();
            exec(
                s,
                user,
                args,
                Cmd::Argv(argv.iter().map(|a| (*a).to_owned()).collect()),
                Some(code),
            )
            .await
        }
        "exec" => {
            exec(
                s,
                user,
                args,
                Cmd::Shell(arg(args, "command")?.to_owned()),
                None,
            )
            .await
        }
        "read_file" => {
            let f = super::find(s, user, arg(args, "sandbox_id")?, Need::Use)
                .await
                .map_err(err)?;
            let (d, _) = f.daemon().map_err(err)?;
            let data = d
                .read_file(&f.id, arg(args, "path")?)
                .await
                .map_err(|e| super::daemon_error(e).message)?;
            let truncated = data.len() > READ_CAP;
            let text = String::from_utf8_lossy(&data[..data.len().min(READ_CAP)]).into_owned();
            Ok(json!({ "content": text, "bytes": data.len(), "truncated": truncated }))
        }
        "write_file" => {
            let f = super::find(s, user, arg(args, "sandbox_id")?, Need::Use)
                .await
                .map_err(err)?;
            let (d, _) = f.daemon().map_err(err)?;
            let content = args["content"].as_str().ok_or("content is required")?;
            let bytes = d
                .write_file(
                    &f.id,
                    arg(args, "path")?,
                    content.as_bytes().to_vec(),
                )
                .await
                .map_err(|e| super::daemon_error(e).message)?;
            Ok(json!({ "ok": true, "bytes": bytes }))
        }
        "open_port" => {
            let port = args["port"]
                .as_u64()
                .and_then(|p| u16::try_from(p).ok())
                .filter(|p| *p > 0)
                .ok_or("port must be 1 to 65535")?;
            let public = args["public"].as_bool().unwrap_or(true);
            super::open_port(s, user, arg(args, "sandbox_id")?, port, public)
                .await
                .map_err(err)
        }
        "close_port" => {
            let port = args["port"]
                .as_u64()
                .and_then(|p| u16::try_from(p).ok())
                .filter(|p| *p > 0)
                .ok_or("port must be 1 to 65535")?;
            super::close_port(s, user, arg(args, "sandbox_id")?, port)
                .await
                .map_err(err)
        }
        "rename_sandbox" => super::rename(s, user, arg(args, "sandbox_id")?, arg(args, "name")?)
            .await
            .map_err(err),
        "pause_sandbox" => super::pause(s, user, arg(args, "sandbox_id")?)
            .await
            .map_err(err),
        "delete_sandbox" => super::delete(s, user, arg(args, "sandbox_id")?)
            .await
            .map_err(err),
        other => Err(format!("Unknown tool {other}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_tool_has_a_definition() {
        let defs = defs();
        let names: Vec<&str> = defs.iter().filter_map(|d| d["name"].as_str()).collect();
        assert_eq!(names, TOOLS);
        for d in &defs {
            let desc = d["description"].as_str().unwrap();
            assert!(desc.ends_with('.') && !desc.contains(';'), "{desc}");
            assert!(d["inputSchema"]["properties"].is_object());
        }
    }
}
