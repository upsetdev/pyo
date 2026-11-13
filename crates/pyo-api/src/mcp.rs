//! Pyo for AI assistants: a Model Context Protocol server at `/api/mcp`.
//!
//! Streamable HTTP transport, answered with plain JSON (no server-initiated
//! messages, so no event stream). Clients sign in with OAuth (oauth.rs): the
//! user approves them in the dashboard, choosing read, deploy or full
//! access, and they act as that user with those limits, the same as an API
//! key's.
//!
//! The tools are everything the API does: a few written for assistants
//! (find a service by name, create one from an image, add a domain…), then
//! one tool per endpoint and per operation of the API reference
//! (openapi.rs), with its description and input schema, run through the
//! same handlers and `/ops` code with the same checks. Only the tools the
//! connection's access allows are listed.

use axum::{
    Json, Router,
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::post,
};
use std::sync::OnceLock;

use serde_json::{Map, Value, json};

use crate::{
    AppState,
    auth::{KeyAccess, SessionUser, User},
    error::{ApiError, ApiResult},
    ids::new_id,
    oauth::OAuthUser,
    ops, snapshot,
    util::slugify,
};

/// Protocol revisions this server speaks; it answers with the client's when
/// it's one of these, else the newest.
const VERSIONS: [&str; 3] = ["2025-11-25", "2025-06-18", "2025-03-26"];

pub fn routes() -> Router<AppState> {
    Router::new().route("/", post(handle).get(no_stream).delete(no_stream))
}

/// No server-initiated messages, so no event stream and no sessions to end.
async fn no_stream() -> StatusCode {
    StatusCode::METHOD_NOT_ALLOWED
}

async fn handle(
    State(s): State<AppState>,
    OAuthUser(user, client): OAuthUser,
    Json(body): Json<Value>,
) -> Response {
    let who = (&user, client.as_str());
    match body {
        Value::Array(batch) => {
            let mut out = vec![];
            for msg in batch {
                if let Some(r) = message(&s, who, msg).await {
                    out.push(r);
                }
            }
            if out.is_empty() {
                StatusCode::ACCEPTED.into_response()
            } else {
                Json(Value::Array(out)).into_response()
            }
        }
        msg => match message(&s, who, msg).await {
            Some(r) => Json(r).into_response(),
            // Notifications and responses get no answer.
            None => StatusCode::ACCEPTED.into_response(),
        },
    }
}

/// One JSON-RPC message; `None` for notifications.
async fn message(s: &AppState, who: (&User, &str), msg: Value) -> Option<Value> {
    let id = msg.get("id").cloned()?;
    let method = msg["method"].as_str().unwrap_or("");
    let params = msg.get("params").cloned().unwrap_or(json!({}));
    let result = match method {
        "initialize" => Ok(initialize(&params)),
        "ping" => Ok(json!({})),
        "tools/list" => list_tools(who.0, &params),
        "tools/call" => Ok(call(s, who, &params).await),
        other => Err((-32601, format!("Method not found: {other}"))),
    };
    Some(match result {
        Ok(r) => json!({ "jsonrpc": "2.0", "id": id, "result": r }),
        Err((code, message)) => {
            json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
        }
    })
}

fn initialize(params: &Value) -> Value {
    let asked = params["protocolVersion"].as_str().unwrap_or("");
    let version = VERSIONS
        .iter()
        .find(|v| **v == asked)
        .copied()
        .unwrap_or(VERSIONS[0]);
    json!({
        "protocolVersion": version,
        "capabilities": { "tools": { "listChanged": false } },
        "serverInfo": { "name": "pyo", "title": "Pyo", "version": env!("CARGO_PKG_VERSION") },
        "instructions": "Pyo deploys services (websites, APIs, databases, workers) to the user's own servers. Start with list_services to find a service's id, \
                         or get_state for everything the user can see. Deploys and rollbacks are asynchronous. They return a deployment id to check with get_deployment. \
                         Every other change has its own tool, named after the operation (save_service, add_volume, create_cluster…). New ids are chosen by you: \
                         the kind's prefix and an underscore, then 4 to 40 lowercase letters or digits (svc_, prj_, dpl_…).",
    })
}

fn schema(props: Value, required: &[&str]) -> Value {
    json!({ "type": "object", "properties": props, "required": required })
}

/// The tools written for assistants.
fn curated() -> Vec<Value> {
    let service = json!({ "type": "string", "description": "Service id, e.g. svc_0dakmctwrd (from list_services)" });
    let team = json!({ "type": "string", "description": "Team slug or id. The user's first team when omitted" });
    let confirm = json!({ "type": "boolean", "description": "Go ahead with a very large replica change. Only after the user said yes to it" });
    let read = json!({ "readOnlyHint": true, "destructiveHint": false, "idempotentHint": true });
    let write = json!({ "readOnlyHint": false, "destructiveHint": false, "idempotentHint": false });
    vec![
        json!({ "name": "list_teams", "title": "List teams", "description": "Teams the user belongs to.", "inputSchema": schema(json!({}), &[]), "annotations": read }),
        json!({ "name": "list_services", "title": "List services", "description": "Services of a team: status, replicas, domains, live deployment.", "inputSchema": schema(json!({ "team": team }), &[]), "annotations": read }),
        json!({ "name": "get_service", "title": "Get service", "description": "One service in detail: source, settings, domains, resources, runtime, recent deployments.", "inputSchema": schema(json!({ "service_id": service }), &["service_id"]), "annotations": read }),
        json!({ "name": "list_deployments", "title": "List deployments", "description": "A service's deployments, newest first.", "inputSchema": schema(json!({ "service_id": service, "limit": { "type": "integer", "minimum": 1, "maximum": 50 } }), &["service_id"]), "annotations": read }),
        json!({ "name": "get_deployment", "title": "Get deployment", "description": "A deployment's status, why it failed, and its build and rollout logs.", "inputSchema": schema(json!({ "deployment_id": { "type": "string" } }), &["deployment_id"]), "annotations": read }),
        json!({ "name": "get_service_logs", "title": "Get service logs", "description": "Recent output of the service's running replicas.", "inputSchema": schema(json!({ "service_id": service }), &["service_id"]), "annotations": read }),
        json!({ "name": "list_servers", "title": "List servers", "description": "Clusters and servers of a team, with CPU and memory.", "inputSchema": schema(json!({ "team": team }), &[]), "annotations": read }),
        json!({ "name": "deploy_service", "title": "Deploy service", "description": "Deploy the service with its current settings (Git services build their branch head), or retry a failed deployment. Returns the deployment id.", "inputSchema": schema(json!({ "service_id": service, "retry_of": { "type": "string", "description": "A failed deployment to try again" } }), &["service_id"]), "annotations": write }),
        json!({ "name": "rollback_service", "title": "Roll back service", "description": "Go back to an earlier successful deployment (the previous one when omitted).", "inputSchema": schema(json!({ "service_id": service, "to_deployment_id": { "type": "string" } }), &["service_id"]), "annotations": write }),
        json!({ "name": "scale_service", "title": "Scale service", "description": "Set how many replicas run (1 or more), without redeploying. Replicas that don't fit on the servers wait until there's room, and the result has a warning saying so. A very large change (20 or more replicas, and over 10 times the current count or twice what fits) is refused until confirmed: ask the user first, then call again with confirm: true.", "inputSchema": schema(json!({ "service_id": service, "replicas": { "type": "integer", "minimum": 1 }, "confirm": confirm }), &["service_id", "replicas"]), "annotations": { "readOnlyHint": false, "destructiveHint": false, "idempotentHint": true } }),
        json!({ "name": "restart_service", "title": "Restart service", "description": "Restart the service's replicas one at a time.", "inputSchema": schema(json!({ "service_id": service }), &["service_id"]), "annotations": write }),
        json!({ "name": "create_project", "title": "Create project", "description": "A new, empty project (a group of services). Returns its id.", "inputSchema": schema(json!({ "team": team, "name": { "type": "string" }, "description": { "type": "string" }, "namespace": { "type": "string", "description": "Its slug (lowercase letters, digits and dashes), used in service addresses. Made from the name when omitted" } }), &["name"]), "annotations": write }),
        json!({ "name": "create_service", "title": "Create service", "description": "Create a service from a container image and deploy it. The project is found by name or id, or created when it doesn't exist. Returns the service id, deployment id and URL. 20 or more replicas is refused until confirmed: ask the user first, then call again with confirm: true.", "inputSchema": schema(json!({
            "team": team,
            "project": { "type": "string", "description": "Project name or id. Created when no project has that name" },
            "name": { "type": "string", "description": "Service name: lowercase letters, digits and dashes" },
            "image": { "type": "string", "description": "Container image, e.g. nginx:latest or ghcr.io/owner/app:1.2" },
            "port": { "type": "integer", "description": "Port the container listens on (default 80)" },
            "replicas": { "type": "integer", "minimum": 1 },
            "confirm": confirm,
            "env": { "type": "object", "additionalProperties": { "type": "string" }, "description": "Environment variables" },
            "free_domain": { "type": "boolean", "description": "Give it a free nip.io domain (no DNS needed)" },
            "domain": { "type": "string", "description": "A custom hostname to serve it on" },
            "region": { "type": "string", "description": "Where to run it: a region of the team's servers (e.g. \"US East\" or \"Singapore, SG\"), a country code (DE, SG) or a city. The team's default cluster when omitted" },
            "server_tags": { "type": "array", "items": { "type": "string" }, "description": "Run only on servers that have all of these tags (see list_servers)" },
        }), &["name", "image"]), "annotations": write }),
        json!({ "name": "add_domain", "title": "Add domain", "description": "Serve a service on a hostname, or on a free nip.io domain. It goes live in place, without a redeploy (a service never deployed gets it with its first deploy).", "inputSchema": schema(json!({ "service_id": service, "hostname": { "type": "string" }, "free_domain": { "type": "boolean" } }), &["service_id"]), "annotations": write }),
        json!({ "name": "stop_service", "title": "Stop service", "description": "Stop the service (scale to zero). Settings and data are kept.", "inputSchema": schema(json!({ "service_id": service }), &["service_id"]), "annotations": { "readOnlyHint": false, "destructiveHint": true, "idempotentHint": true } }),
    ]
}

// ── The catalog: written tools, then the API reference's ─────────────────────

/// How many tools one page of tools/list carries; the next page's cursor
/// comes with it. The written tools come first, so a client that reads only
/// one page still has them.
const PAGE: usize = 50;

/// Spec operations and endpoints a written tool does instead, so there's one
/// tool each: (operation or operationId, tool).
const COVERED: &[(&str, &str)] = &[
    ("deploy", "deploy_service"),
    ("scaleService", "scale_service"),
    ("restartService", "restart_service"),
    ("stopService", "stop_service"),
    ("createProject", "create_project"),
    ("getServiceLogs", "get_service_logs"),
];

/// Endpoints with no tool: the dashboard's own helpers. getMyAddress is the
/// address a request comes from, which for an assistant is its own server's,
/// not the person's.
// Database logs stay out of MCP for now.
// A dump's file is sent as a request body, which a tool can't send: an
// assistant imports from a URL.
const NOT_TOOLS: &[&str] = &[
    "getMyAddress",
    "getDatabaseLogs",
    "createDatabaseDumpUpload",
    "uploadDatabaseDump",
    // They take another platform's API token: kept out of assistants.
    "previewPlatformImport",
    "startPlatformImport",
];

/// Tools whose generated name would clash or say too little.
const RENAMED: &[(&str, &str)] = &[
    ("createService", "create_service_from_spec"),
    ("cancel", "cancel_deployment"),
    ("setServerLocation", "set_server_region"),
];

/// The operation or endpoint behind each written tool: it needs that one's
/// access.
const CURATED_RUNS: &[(&str, &str)] = &[
    ("list_teams", "getState"),
    ("list_services", "getState"),
    ("get_service", "getState"),
    ("list_deployments", "getState"),
    ("get_deployment", "getState"),
    ("get_service_logs", "getServiceLogs"),
    ("list_servers", "getState"),
    ("deploy_service", "deploy"),
    ("rollback_service", "deploy"),
    ("scale_service", "scaleService"),
    ("restart_service", "restartService"),
    ("stop_service", "stopService"),
    ("create_project", "createProject"),
    ("create_service", "createService"),
    ("add_domain", "saveService"),
];

/// Operations that hand back a secret: said so in their description. They
/// stay audited like in the API (the operation records who saw it).
const SECRETS: &[&str] = &["revealSecret", "revealDeployHook", "revealBackupPassword"];

#[derive(Clone, Debug, PartialEq)]
enum Run {
    /// Written in this file (run_tool).
    Written,
    /// A `/ops` operation, by its `op`.
    Op(String),
    /// An endpoint, by its operationId.
    Endpoint(String),
}

#[derive(Clone, Debug)]
pub(crate) struct Tool {
    def: Value,
    access: KeyAccess,
    run: Run,
}

impl Tool {
    fn name(&self) -> &str {
        self.def["name"].as_str().unwrap_or("")
    }
}

/// "scaleService" → "scale_service".
fn snake(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        if c.is_uppercase() {
            out.push('_');
            out.extend(c.to_lowercase());
        } else {
            out.push(c);
        }
    }
    out
}

fn access_of(a: &str) -> KeyAccess {
    match a {
        "full" | "varies" => KeyAccess::Full,
        "deploy" => KeyAccess::Deploy,
        _ => KeyAccess::Read,
    }
}

/// A schema with its `$ref`s written out (clients don't resolve them), and
/// without what's only for the docs.
fn inline(v: &Value, schemas: &Value) -> Value {
    match v {
        Value::Object(m) => {
            if let Some(r) = m.get("$ref").and_then(Value::as_str) {
                let mut out = inline(&schemas[r.rsplit('/').next().unwrap_or("")], schemas);
                if let (Some(o), Some(d)) = (out.as_object_mut(), m.get("description")) {
                    o.insert("description".into(), d.clone());
                }
                return out;
            }
            Value::Object(
                m.iter()
                    .filter(|(k, _)| {
                        !k.starts_with("x-") && *k != "example" && *k != "examples" && *k != "title"
                    })
                    .map(|(k, v)| (k.clone(), inline(v, schemas)))
                    .collect(),
            )
        }
        Value::Array(a) => Value::Array(a.iter().map(|v| inline(v, schemas)).collect()),
        v => v.clone(),
    }
}

/// Plain text for a tool: the reference's Markdown without bold.
fn plain(s: &str) -> String {
    s.replace("**", "")
}

fn op_annotations(op: &str) -> Value {
    let starts = |ps: &[&str]| ps.iter().any(|p| op.starts_with(p));
    let read_only = matches!(op, "githubInstallUrl" | "checkSecretImport");
    json!({
        "readOnlyHint": read_only,
        // Undoing a server's removal takes nothing away.
        // A volume's restore replaces its data.
        "destructiveHint": !read_only && op != "cancelServerRemoval" && (starts(&["delete", "remove", "stop", "discard", "rotate", "cancel", "revoke"]) || op == "restoreVolume"),
        "idempotentHint": read_only || starts(&["set", "update", "save", "scale", "remove", "delete", "stop", "verify", "replace", "detach", "attach", "resize"]),
    })
}

fn build_catalog() -> Vec<Tool> {
    let spec: Value =
        serde_json::from_str(&crate::openapi::spec_json()).expect("the API reference");
    let schemas = &spec["components"]["schemas"];
    let covered = |id: &str| COVERED.iter().any(|(o, _)| *o == id);
    let renamed = |id: &str| {
        RENAMED
            .iter()
            .find(|(o, _)| *o == id)
            .map(|(_, n)| (*n).to_owned())
            .unwrap_or_else(|| snake(id))
    };

    // Endpoints by operationId: their access and their tool.
    let mut endpoints: Vec<(String, Value)> = vec![];
    for (path, item) in spec["paths"].as_object().into_iter().flatten() {
        for (method, op) in item.as_object().into_iter().flatten() {
            let mut op = op.clone();
            op["x-path"] = json!(path);
            op["x-method"] = json!(method);
            endpoints.push((op["operationId"].as_str().unwrap_or("").to_owned(), op));
        }
    }
    let ops: Vec<(String, Value)> = schemas["Op"]["discriminator"]["mapping"]
        .as_object()
        .into_iter()
        .flatten()
        .map(|(name, r)| {
            (
                name.clone(),
                schemas[r.as_str().unwrap_or("").rsplit('/').next().unwrap_or("")].clone(),
            )
        })
        .collect();
    let access_for = |id: &str| -> KeyAccess {
        if let Some((_, o)) = ops.iter().find(|(n, _)| n == id) {
            return access_of(o["x-pyo-access"].as_str().unwrap_or("full"));
        }
        let (_, e) = endpoints
            .iter()
            .find(|(n, _)| n == id)
            .unwrap_or_else(|| panic!("{id} isn't in the API reference"));
        access_of(e["x-pyo-access"].as_str().unwrap_or("full"))
    };

    let mut tools: Vec<Tool> = curated()
        .into_iter()
        .map(|def| {
            let name = def["name"].as_str().unwrap_or("").to_owned();
            let behind = CURATED_RUNS
                .iter()
                .find(|(t, _)| *t == name)
                .unwrap_or_else(|| panic!("{name} has no entry in CURATED_RUNS"))
                .1;
            Tool {
                def,
                access: access_for(behind),
                run: Run::Written,
            }
        })
        .collect();

    // Endpoints (but not /ops: its operations are tools of their own).
    for (id, e) in &endpoints {
        if id == "runOp" || covered(id) || NOT_TOOLS.contains(&id.as_str()) {
            continue;
        }
        let mut props = Map::new();
        let mut required = vec![];
        for p in e["parameters"].as_array().into_iter().flatten() {
            let name = p["name"].as_str().unwrap_or("").to_owned();
            let mut schema = inline(&p["schema"], schemas);
            if let (Some(o), Some(d)) = (schema.as_object_mut(), p["description"].as_str()) {
                o.insert("description".into(), json!(d));
            }
            if p["required"] == json!(true) {
                required.push(json!(name));
            }
            props.insert(name, schema);
        }
        let mut input = json!({ "type": "object", "properties": props, "required": required });
        if let Some(body) = e["requestBody"]["content"]["application/json"]["schema"].as_object() {
            input = inline(&Value::Object(body.clone()), schemas);
        }
        // Reads change nothing, POST or not (a Compose preview creates nothing).
        let get = e["x-method"] == "get" || e["x-pyo-access"] == "read";
        let description = format!(
            "{}\n\n{}",
            e["summary"].as_str().unwrap_or(""),
            plain(e["description"].as_str().unwrap_or(""))
        );
        tools.push(Tool {
            def: json!({
                "name": renamed(id),
                "title": e["summary"],
                "description": description.trim(),
                "inputSchema": input,
                "annotations": { "readOnlyHint": get, "destructiveHint": e["x-method"] == "delete", "idempotentHint": get || e["x-method"] == "delete" },
            }),
            access: access_of(e["x-pyo-access"].as_str().unwrap_or("full")),
            run: Run::Endpoint(id.clone()),
        });
    }

    // Operations, in the reference's order of folders.
    let order: Vec<&str> = spec["tags"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|t| t["name"].as_str())
        .collect();
    let mut op_tools: Vec<(usize, Tool)> = vec![];
    for (name, o) in &ops {
        if covered(name) {
            continue;
        }
        let mut input = inline(o, schemas);
        if let Some(m) = input.as_object_mut() {
            m.remove("description");
            if let Some(p) = m.get_mut("properties").and_then(Value::as_object_mut) {
                p.remove("op");
            }
            if let Some(r) = m.get_mut("required").and_then(Value::as_array_mut) {
                r.retain(|x| x != "op");
            }
        }
        let mut description = plain(o["description"].as_str().unwrap_or(""));
        if SECRETS.contains(&name.as_str()) {
            description = format!(
                "Returns a secret in plain text. Only call this when the user asks to see it. Who saw it is recorded in the team's activity.\n\n{description}"
            );
        }
        let tag = o["x-tags"][0].as_str().unwrap_or("");
        let annotations = if SECRETS.contains(&name.as_str()) {
            // Not read-only: it's recorded in the activity log.
            json!({ "readOnlyHint": false, "destructiveHint": false, "idempotentHint": true })
        } else {
            op_annotations(name)
        };
        op_tools.push((
            order.iter().position(|t| *t == tag).unwrap_or(usize::MAX),
            Tool {
                def: json!({ "name": renamed(name), "title": o["title"], "description": description, "inputSchema": input, "annotations": annotations }),
                access: access_of(o["x-pyo-access"].as_str().unwrap_or("full")),
                run: Run::Op(name.clone()),
            },
        ));
    }
    op_tools.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.name().cmp(b.1.name())));
    tools.extend(op_tools.into_iter().map(|(_, t)| t));
    tools
}

pub(crate) fn catalog() -> &'static [Tool] {
    static C: OnceLock<Vec<Tool>> = OnceLock::new();
    C.get_or_init(build_catalog)
}

/// What this connection may run: its grant's access (full for a session).
fn allowed(user: &User) -> KeyAccess {
    user.key.as_ref().map_or(KeyAccess::Full, |k| k.access)
}

/// tools/list: the tools the connection's access allows, a page at a time.
fn list_tools(user: &User, params: &Value) -> Result<Value, (i64, String)> {
    let mine: Vec<&Tool> = catalog()
        .iter()
        .filter(|t| t.access <= allowed(user))
        .collect();
    let start = match params["cursor"].as_str() {
        None => 0,
        Some(c) => c
            .parse::<usize>()
            .ok()
            .filter(|n| *n < mine.len())
            .ok_or((-32602, "Invalid cursor".to_owned()))?,
    };
    let end = (start + PAGE).min(mine.len());
    let mut out =
        json!({ "tools": mine[start..end].iter().map(|t| t.def.clone()).collect::<Vec<_>>() });
    if end < mine.len() {
        out["nextCursor"] = json!(end.to_string());
    }
    Ok(out)
}

/// Env values out of a snapshot: listings show names only, like get_service.
fn without_env_values(mut snap: Value) -> Value {
    for a in snap["services"].as_array_mut().into_iter().flatten() {
        for e in a["env"].as_array_mut().into_iter().flatten() {
            if e.get("value").is_some() {
                e["value"] = json!("•••");
            }
        }
    }
    snap
}

/// Every string in `v` as plain text: container output (deployment logs,
/// cron runs' output) without its ANSI colors and control characters.
fn plain_text(mut v: Value) -> Value {
    fn walk(v: &mut Value) {
        match v {
            Value::String(s) => {
                if let std::borrow::Cow::Owned(p) = pyo_proto::ansi::strip(s) {
                    *s = p;
                }
            }
            Value::Array(a) => a.iter_mut().for_each(walk),
            Value::Object(o) => o.values_mut().for_each(walk),
            _ => {}
        }
    }
    walk(&mut v);
    v
}

fn args_as<T: serde::de::DeserializeOwned>(args: &Value) -> Result<T, String> {
    serde_json::from_value(args.clone()).map_err(|e| format!("Invalid arguments: {e}"))
}

fn json_of<T: serde::Serialize>(r: ApiResult<Json<T>>) -> Result<Value, String> {
    r.map_err(err)
        .and_then(|Json(v)| serde_json::to_value(v).map_err(|e| e.to_string()))
}

/// An endpoint's tool: the endpoint's own handler, as this user.
async fn run_endpoint(s: &AppState, user: &User, id: &str, args: &Value) -> Result<Value, String> {
    use axum::extract::{Path, Query};
    let st = || State(s.clone());
    let me = || SessionUser(user.clone());
    let path_id = || arg(args, "id").map(|i| Path(i.to_owned()));
    match id {
        "getState" => json_of(ops::state(st(), me()).await)
            .map(without_env_values)
            .map(plain_text),
        "getMe" => serde_json::to_value(ops::me(me()).await.0).map_err(|e| e.to_string()),
        "getMeta" => serde_json::to_value(ops::meta().await.0).map_err(|e| e.to_string()),
        "getServicePreview" => json_of(ops::service_preview(st(), me(), path_id()?).await),
        "getServiceTraffic" => json_of(crate::traffic::get(st(), me(), path_id()?).await),
        "listVolumeSnapshots" => json_of(crate::volumes::snapshots(st(), me(), path_id()?).await),
        "deleteVolumeSnapshot" => json_of(
            crate::volumes::delete_snapshot(
                st(),
                me(),
                Path((
                    arg(args, "id")?.to_owned(),
                    arg(args, "snapshotId")?.to_owned(),
                )),
            )
            .await,
        ),
        "listCronRuns" => {
            json_of(crate::cronruns::list(st(), me(), Query(args_as(args)?)).await).map(plain_text)
        }
        "getMetrics" => json_of(ops::metrics(st(), me(), Query(args_as(args)?)).await),
        "getObservabilitySettings" => json_of(ops::observability_settings(st(), me()).await),
        "getObservability" => json_of(ops::observability(st(), me(), Query(args_as(args)?)).await),
        "getAccessLogs" => json_of(ops::access_logs(st(), me(), Query(args_as(args)?)).await),
        "listImages" => json_of(crate::images::list(st(), me(), Query(args_as(args)?)).await),
        "getRegistry" => json_of(crate::images::registry(st(), me(), Query(args_as(args)?)).await),
        "listDatabases" => json_of(crate::databases::list(st(), me(), Query(args_as(args)?)).await),
        "getDatabase" => {
            json_of(crate::databases::get(st(), me(), Path(arg(args, "id")?.to_owned())).await)
        }
        "listDatabaseExtensions" => json_of(crate::databases::extensions(me()).await),
        "getDatabaseAnalytics" => json_of(
            crate::database_analytics::analytics(
                st(),
                me(),
                Path(arg(args, "id")?.to_owned()),
                Query(args_as(args)?),
            )
            .await,
        ),
        "listGithubRepos" => json_of(crate::github::repos(st(), me(), Query(args_as(args)?)).await),
        "getGithubBuildFile" => {
            json_of(crate::github::build_file(st(), me(), Query(args_as(args)?)).await)
        }
        "addDatabaseBackup" => {
            json_of(crate::database_backups::add(st(), me(), Json(args_as(args)?)).await)
        }
        "checkDatabaseImport" => {
            json_of(crate::database_imports::check(st(), me(), Json(args_as(args)?)).await)
        }
        "startDatabaseImport" => {
            json_of(crate::database_imports::start(st(), me(), Json(args_as(args)?)).await)
        }
        "retryDatabaseImport" => {
            json_of(crate::database_imports::retry(st(), me(), path_id()?).await)
        }
        "previewCompose" => {
            json_of(crate::compose::preview(st(), me(), Json(args_as(args)?)).await)
        }
        "getServiceConnections" => {
            json_of(ops::connections(st(), me(), path_id()?, Query(args_as(args)?)).await)
        }
        "getFirewallBlocked" => {
            json_of(ops::firewall_blocked(st(), me(), Query(args_as(args)?)).await)
        }
        "getWafHits" => json_of(ops::waf_hits(st(), me(), Query(args_as(args)?)).await),
        other => Err(format!("No tool runs {other}")),
    }
}

/// A tool by name, if this connection may run it.
fn find_tool(user: &User, name: &str) -> Result<&'static Tool, String> {
    let t = catalog()
        .iter()
        .find(|t| t.name() == name)
        .ok_or_else(|| format!("Unknown tool {name}"))?;
    if t.access > allowed(user) {
        return Err(match allowed(user) {
            KeyAccess::Read => "This connection can only read. Reconnect the assistant with more access to do this.".to_owned(),
            _ => "This connection can only deploy, roll back, restart, stop and scale services. Reconnect the assistant with full access to do this.".to_owned(),
        });
    }
    Ok(t)
}

async fn dispatch(s: &AppState, user: &User, name: &str, args: &Value) -> Result<Value, String> {
    let t = find_tool(user, name)?;
    match &t.run {
        Run::Written => run_tool(s, user, t.name(), args).await,
        Run::Op(op) => {
            let mut body = args.as_object().cloned().unwrap_or_default();
            body.insert("op".into(), json!(op));
            ops::run_json(s, user, Value::Object(body))
                .await
                .map_err(err)
        }
        Run::Endpoint(id) => run_endpoint(s, user, id, args).await,
    }
}

/// A tool's answer: JSON as text (and as structured content), or an error
/// the model can read and act on. Every call is logged for the MCP page.
async fn call(s: &AppState, (user, client): (&User, &str), params: &Value) -> Value {
    let name = params["name"].as_str().unwrap_or("");
    let args = params.get("arguments").cloned().unwrap_or(json!({}));
    let started = std::time::Instant::now();
    let result = dispatch(s, user, name, &args).await;
    log_call(
        s,
        user,
        client,
        name,
        &args,
        result.as_ref().err(),
        started.elapsed(),
    )
    .await;
    match result {
        Ok(v) => {
            json!({ "content": [{ "type": "text", "text": serde_json::to_string_pretty(&v).unwrap_or_default() }], "structuredContent": structured(name, v), "isError": false })
        }
        Err(msg) => json!({ "content": [{ "type": "text", "text": msg }], "isError": true }),
    }
}

async fn log_call(
    s: &AppState,
    user: &User,
    client: &str,
    tool: &str,
    args: &Value,
    error: Option<&String>,
    took: std::time::Duration,
) {
    let r = sqlx::query("INSERT INTO pyo.mcp_calls (user_id, client, tool, args, ok, error, duration_ms) VALUES ($1, $2, $3, $4, $5, $6, $7)")
        .bind(&user.id)
        .bind(client)
        .bind(tool)
        .bind(redacted(args))
        .bind(error.is_none())
        .bind(error)
        .bind(took.as_millis().min(i32::MAX as u128) as i32)
        .execute(&s.db)
        .await;
    if let Err(e) = r {
        tracing::warn!(%tool, error = %e, "MCP call not logged");
    }
}

/// Arguments as logged: environment variable values, passwords, keys and
/// the like may be secrets.
fn redacted(args: &Value) -> Value {
    const SECRET: [&str; 7] = [
        "password",
        "adminPassword",
        "secret",
        "secretKey",
        "privateKey",
        "credential",
        "accessKey",
    ];
    /// `in_env`: inside a service's environment variables, whose `value`s go.
    fn walk(v: &mut Value, in_env: bool) {
        match v {
            Value::Object(m) => {
                for (k, x) in m.iter_mut() {
                    if k == "env" {
                        match x.as_object_mut() {
                            // create_service: {"NAME": "value"}.
                            Some(env) => env.values_mut().for_each(|e| *e = json!("•••")),
                            // A service: [{"key": …, "value": …}].
                            None => walk(x, true),
                        }
                    } else if !x.is_null()
                        && (SECRET.contains(&k.as_str()) || (in_env && k == "value"))
                    {
                        *x = json!("•••");
                    } else {
                        walk(x, false);
                    }
                }
            }
            Value::Array(a) => a.iter_mut().for_each(|x| walk(x, in_env)),
            _ => {}
        }
    }
    let mut a = args.clone();
    walk(&mut a, false);
    a
}

/// Your assistants' recent tool calls, newest first.
pub async fn calls(
    State(s): State<AppState>,
    SessionUser(user): SessionUser,
) -> ApiResult<Json<Value>> {
    let rows: Vec<Value> = sqlx::query_scalar(
        "SELECT jsonb_build_object('id', id, 'client', client, 'tool', tool, 'args', args, 'ok', ok, 'error', error,
                                  'durationMs', duration_ms, 'createdAt', pyo.ms(created_at))
           FROM pyo.mcp_calls WHERE user_id = $1 ORDER BY created_at DESC LIMIT 100",
    )
    .bind(&user.id)
    .fetch_all(&s.db)
    .await?;
    Ok(Json(json!({ "calls": rows })))
}

fn arg<'a>(args: &'a Value, key: &str) -> Result<&'a str, String> {
    args[key]
        .as_str()
        .filter(|v| !v.is_empty())
        .ok_or_else(|| format!("{key} is required"))
}

/// The service a tool acts on.
fn service_id(args: &Value) -> Result<&str, String> {
    arg(args, "service_id")
}

fn err(e: ApiError) -> String {
    if e.needs_confirm {
        return format!(
            "{} Nothing changed. Ask the user whether to go ahead, and only if they say yes, call again with confirm: true.",
            e.message
        );
    }
    e.message
}

/// MCP wants structured results as an object: lists go under a key named
/// after the tool (`list_services` gives `{"services": [...]}`).
fn structured(tool: &str, v: Value) -> Value {
    if v.is_object() {
        return v;
    }
    let key = tool
        .strip_prefix("list_")
        .or_else(|| tool.strip_prefix("get_"))
        .unwrap_or("result");
    json!({ key: v })
}

async fn run_tool(s: &AppState, user: &User, name: &str, args: &Value) -> Result<Value, String> {
    let snap = snapshot::build(&s.db, user, &s.config.agent_api_url)
        .await
        .map_err(err)?;
    let list = |k: &str| snap[k].as_array().cloned().unwrap_or_default();
    let team_id = |args: &Value| -> Result<String, String> {
        let teams = list("teams");
        match args["team"].as_str().filter(|t| !t.is_empty()) {
            Some(t) => teams
                .iter()
                .find(|x| x["slug"] == json!(t) || x["id"] == json!(t))
                .and_then(|x| x["id"].as_str())
                .map(str::to_owned)
                .ok_or_else(|| format!("No team {t} among yours")),
            None => teams
                .first()
                .and_then(|x| x["id"].as_str())
                .map(str::to_owned)
                .ok_or_else(|| "You aren't in any team".to_owned()),
        }
    };
    let find_service = |id: &str| {
        list("services")
            .into_iter()
            .find(|a| a["id"] == json!(id))
            .ok_or_else(|| format!("No service {id} (list_services shows the ids)"))
    };
    let deps_of = |service_id: &str| {
        let mut d: Vec<Value> = list("deployments")
            .into_iter()
            .filter(|d| d["serviceId"] == json!(service_id))
            .collect();
        d.sort_by_key(|d| std::cmp::Reverse(d["createdAt"].as_i64().unwrap_or(0)));
        d
    };
    let brief_dep = |d: &Value| {
        json!({
            "id": d["id"], "status": d["status"], "trigger": d["trigger"], "by": d["actor"], "revision": d["revisionLabel"],
            "createdAt": d["createdAt"], "finishedAt": d["finishedAt"], "failure": d["failure"]["message"],
        })
    };
    let op = |v: Value| async move { ops::run_json(s, user, v).await.map_err(err) };

    match name {
        "list_teams" => Ok(json!(
            list("teams")
                .iter()
                .map(|t| json!({ "id": t["id"], "slug": t["slug"], "name": t["name"] }))
                .collect::<Vec<_>>()
        )),
        "list_services" => {
            let team = team_id(args)?;
            let projects = list("projects");
            Ok(json!(list("services")
                .iter()
                .filter(|a| a["teamId"] == json!(team))
                .map(|a| json!({
                    "id": a["id"], "name": a["name"],
                    "project": projects.iter().find(|p| p["id"] == a["projectId"]).map(|p| p["name"].clone()),
                    "status": a["status"], "replicas": a["replicas"], "readyReplicas": a["runtime"]["readyReplicas"],
                    "domains": a["domains"].as_array().map(|ds| ds.iter().map(|d| d["hostname"].clone()).collect::<Vec<_>>()),
                    "source": a["source"]["type"], "liveDeployment": a["currentDeploymentId"],
                }))
                .collect::<Vec<_>>()))
        }
        "get_service" => {
            let id = service_id(args)?;
            let a = find_service(id)?;
            let mut out = a.clone();
            // Variable names only: values may be secrets.
            out["env"] = json!(
                a["env"]
                    .as_array()
                    .map(|e| e.iter().map(|x| x["key"].clone()).collect::<Vec<_>>())
            );
            out["recentDeployments"] = json!(
                deps_of(id)
                    .iter()
                    .take(5)
                    .map(brief_dep)
                    .collect::<Vec<_>>()
            );
            Ok(out)
        }
        "list_deployments" => {
            let id = service_id(args)?;
            find_service(id)?;
            let n = args["limit"].as_u64().unwrap_or(10).clamp(1, 50) as usize;
            Ok(json!(
                deps_of(id)
                    .iter()
                    .take(n)
                    .map(brief_dep)
                    .collect::<Vec<_>>()
            ))
        }
        "get_deployment" => {
            let id = arg(args, "deployment_id")?;
            let d = list("deployments")
                .into_iter()
                .find(|d| d["id"] == json!(id))
                .ok_or_else(|| format!("No deployment {id}"))?;
            // Straight from the log table: the snapshot only carries recent deployments' logs.
            let rows: Vec<(String, String, String)> = sqlx::query_as(
                "SELECT level, source, message FROM (SELECT id, level, source, message FROM pyo.deployment_logs WHERE deployment_id = $1 ORDER BY id DESC LIMIT 150) t ORDER BY id",
            )
            .bind(id)
            .fetch_all(&s.db)
            .await
            .map_err(|e| e.to_string())?;
            let tail: Vec<String> = rows
                .into_iter()
                .map(|(level, source, message)| {
                    format!("{level} {source} {}", pyo_proto::ansi::strip(&message))
                })
                .collect();
            let mut out = brief_dep(&d);
            out["serviceId"] = d["serviceId"].clone();
            out["stages"] = d["stages"].clone();
            out["autoRolledBack"] = d["autoRolledBack"].clone();
            out["logs"] = json!(tail);
            Ok(out)
        }
        "get_service_logs" => {
            let id = service_id(args)?;
            let v = ops::fetch_service_logs(s, user, id).await.map_err(err)?;
            let lines: Vec<String> = v["logs"]
                .as_array()
                .cloned()
                .unwrap_or_default()
                .iter()
                .map(|l| {
                    format!(
                        "{} {}",
                        l["source"].as_str().unwrap_or(""),
                        pyo_proto::ansi::strip(l["message"].as_str().unwrap_or(""))
                    )
                })
                .collect();
            Ok(json!({ "lines": lines }))
        }
        "list_servers" => {
            let team = team_id(args)?;
            let clusters: Vec<Value> = list("clusters")
                .into_iter()
                .filter(|c| c["teamId"] == json!(team))
                .collect();
            let servers = list("servers");
            Ok(json!(clusters
                .iter()
                .map(|c| json!({
                    "cluster": c["name"], "status": c["status"],
                    "servers": servers.iter().filter(|n| n["clusterId"] == c["id"]).map(|n| json!({
                        "name": n["name"], "status": n["status"], "ip": n["internalIP"],
                        "cpu": { "cores": n["capacity"]["cpu"], "used": n["usage"]["cpu"] },
                        "memoryGb": { "total": n["capacity"]["memoryGi"], "used": n["usage"]["memoryGi"] },
                    })).collect::<Vec<_>>(),
                }))
                .collect::<Vec<_>>()))
        }
        "deploy_service" => {
            let id = service_id(args)?;
            find_service(id)?;
            let dep = new_id("dpl");
            let retry = args["retry_of"].as_str().filter(|r| !r.is_empty());
            let r = op(json!({ "op": "deploy", "serviceId": id, "deploymentId": dep, "trigger": if retry.is_some() { "retry" } else { "redeploy" }, "retryOf": retry })).await?;
            let mut out = json!({ "deploymentId": dep, "status": "queued", "next": "Check progress with get_deployment." });
            if let Some(w) = r.get("warning") {
                out["warning"] = w.clone();
            }
            Ok(out)
        }
        "rollback_service" => {
            let id = service_id(args)?;
            let a = find_service(id)?;
            let deps = deps_of(id);
            let live = deps
                .iter()
                .find(|d| d["id"] == a["currentDeploymentId"])
                .cloned();
            let to = match args["to_deployment_id"].as_str().filter(|v| !v.is_empty()) {
                Some(t) => deps
                    .iter()
                    .find(|d| d["id"] == json!(t))
                    .cloned()
                    .ok_or_else(|| format!("No deployment {t} of this service"))?,
                None => deps
                    .iter()
                    .find(|d| {
                        d["status"] == json!("succeeded")
                            && d["imageExpired"] != json!(true)
                            && Some(&d["id"]) != live.as_ref().map(|l| &l["id"])
                            && d["createdAt"].as_i64()
                                < live.as_ref().and_then(|l| l["createdAt"].as_i64())
                    })
                    .cloned()
                    .ok_or_else(|| {
                        "There's no earlier successful deployment to go back to".to_owned()
                    })?,
            };
            let dep = new_id("dpl");
            op(json!({ "op": "deploy", "serviceId": id, "deploymentId": dep, "trigger": "rollback", "rollbackOf": to["id"] })).await?;
            Ok(
                json!({ "deploymentId": dep, "rollingBackTo": to["id"], "revision": to["revisionLabel"], "next": "Check progress with get_deployment." }),
            )
        }
        "scale_service" => {
            let id = service_id(args)?;
            let n = args["replicas"].as_i64().ok_or("replicas is required")?;
            let confirm = args["confirm"].as_bool().unwrap_or(false);
            let r =
                op(json!({ "op": "scaleService", "id": id, "replicas": n, "confirm": confirm }))
                    .await?;
            let mut out = json!({ "ok": true, "replicas": n });
            if let Some(w) = r.get("warning") {
                out["warning"] = w.clone();
            }
            Ok(out)
        }
        "restart_service" => {
            op(json!({ "op": "restartService", "id": service_id(args)? })).await?;
            Ok(json!({ "ok": true }))
        }
        "stop_service" => {
            op(json!({ "op": "stopService", "id": service_id(args)? })).await?;
            Ok(json!({ "ok": true }))
        }
        "create_project" => {
            let team = team_id(args)?;
            let p = new_project_with(
                s,
                user,
                &team,
                &list("projects"),
                arg(args, "name")?,
                args["namespace"].as_str().filter(|n| !n.is_empty()),
                args["description"].as_str().unwrap_or(""),
            )
            .await?;
            Ok(p)
        }
        "create_service" => {
            let team = team_id(args)?;
            let name = slugify(arg(args, "name")?);
            let projects = list("projects");
            let project = match args["project"].as_str().filter(|p| !p.is_empty()) {
                Some(p) => match projects.iter().find(|x| {
                    x["teamId"] == json!(team)
                        && (x["id"] == json!(p)
                            || x["name"]
                                .as_str()
                                .is_some_and(|n| n.eq_ignore_ascii_case(p)))
                }) {
                    Some(x) => x.clone(),
                    None => new_project(s, user, &team, &projects, p).await?,
                },
                None => new_project(s, user, &team, &projects, &name).await?,
            };
            let (cluster, pin) = match args["region"].as_str().filter(|l| !l.is_empty()) {
                Some(l) => cluster_at(&snap, &team, l)?,
                None => (team_cluster(&snap, &team)?, String::new()),
            };
            let (image, reference) = split_image(arg(args, "image")?);
            let port = args["port"].as_i64().unwrap_or(80);
            let replicas = args["replicas"].as_i64().unwrap_or(1).max(1);
            let now = now_ms();
            let env: Vec<Value> = args["env"]
                .as_object()
                .map(|m| m.iter().map(|(k, v)| json!({ "id": new_id("env"), "key": k, "value": v.as_str().map(str::to_owned).unwrap_or_else(|| v.to_string()), "secret": false, "updatedAt": now })).collect())
                .unwrap_or_default();
            let host = match (
                args["domain"].as_str().filter(|d| !d.is_empty()),
                args["free_domain"].as_bool().unwrap_or(false),
            ) {
                (Some(d), _) => Some(d.trim().to_lowercase()),
                (None, true) => Some(free_hostname(
                    &cluster,
                    &name,
                    project["namespace"].as_str().unwrap_or(""),
                )?),
                _ => None,
            };
            let domains: Vec<Value> = host.iter().map(|h| domain(h, now)).collect();
            let service_id = new_id("svc");
            let dep = new_id("dpl");
            let service = json!({
                "id": service_id, "teamId": team, "projectId": project["id"], "clusterId": cluster["id"],
                "name": name, "description": "", "kind": "application", "workloadKind": "Deployment",
                "location": if pin.is_empty() { Value::Null } else { json!(pin) },
                "serverTags": args["server_tags"].as_array().map(|ts| ts.iter().filter_map(|t| t.as_str().and_then(pyo_proto::valid_tag)).collect::<Vec<_>>()).unwrap_or_default(),
                "source": { "type": "image", "image": image, "reference": reference, "registryId": "reg_dockerhub", "private": false },
                "env": env,
                "ports": [{ "id": new_id("port"), "name": "http", "protocol": "HTTP", "containerPort": port, "exposure": if domains.is_empty() { "internal" } else { "http" } }],
                "domains": domains,
                "resources": { "cpuRequest": 100, "cpuLimit": 500, "memoryRequest": 128, "memoryLimit": 256 },
                "replicas": replicas,
                "healthCheck": { "type": "tcp", "port": port, "path": "", "command": "", "periodSeconds": 10, "initialDelaySeconds": 5 },
            });
            let confirm = args["confirm"].as_bool().unwrap_or(false);
            let r = op(json!({ "op": "createService", "service": service, "deploymentId": dep, "confirm": confirm }))
                .await?;
            let mut out = json!({
                "serviceId": service_id, "deploymentId": dep, "project": project["name"],
                "url": host.map(|h| format!("{}://{h}", if crate::deploy::private_host(&h) { "http" } else { "https" })),
                "next": "Check progress with get_deployment.",
            });
            if let Some(w) = r.get("warning") {
                out["warning"] = w.clone();
            }
            Ok(out)
        }
        "add_domain" => {
            let id = service_id(args)?;
            let mut a = find_service(id)?;
            let host = match args["hostname"].as_str().filter(|h| !h.is_empty()) {
                Some(h) => h.trim().to_lowercase(),
                None if args["free_domain"].as_bool().unwrap_or(false) => {
                    let cluster = list("clusters")
                        .into_iter()
                        .find(|c| c["id"] == a["clusterId"])
                        .ok_or("The service's cluster is gone")?;
                    let ns = list("projects")
                        .into_iter()
                        .find(|p| p["id"] == a["projectId"])
                        .and_then(|p| p["namespace"].as_str().map(str::to_owned))
                        .unwrap_or_default();
                    free_hostname(&cluster, a["name"].as_str().unwrap_or(""), &ns)?
                }
                None => return Err("Give a hostname, or free_domain: true".into()),
            };
            if a["domains"]
                .as_array()
                .is_some_and(|ds| ds.iter().any(|d| d["hostname"] == json!(host)))
            {
                return Err(format!("{host} is already on this service"));
            }
            let ports = a["ports"]
                .as_array_mut()
                .ok_or("The service has no ports to serve")?;
            let Some(port) = ports
                .iter_mut()
                .find(|p| p["protocol"] != json!("TCP") && p["protocol"] != json!("UDP"))
            else {
                return Err("The service has no HTTP port to serve".into());
            };
            port["exposure"] = json!("http");
            let port_name = port["name"].clone();
            let mut d = domain(&host, now_ms());
            d["portName"] = port_name;
            a["domains"]
                .as_array_mut()
                .map(|ds| ds.push(d.clone()))
                .ok_or("Bad service")?;
            let url = format!(
                "{}://{host}",
                if crate::deploy::private_host(&host) {
                    "http"
                } else {
                    "https"
                }
            );
            // Domains go live in place, without a redeploy; a service never
            // deployed gets them with its first deploy.
            op(json!({ "op": "saveService", "service": a })).await?;
            let next = if deps_of(id).is_empty() {
                "Saved. It goes live with the service's first deploy (deploy_service)."
            } else if crate::deploy::private_host(&host) || d["tlsMode"] == json!("off") {
                "Live in a few seconds, once the cluster has applied it."
            } else {
                "Live in a few seconds, once the cluster has applied it. HTTPS starts once its certificate is issued (get_service shows it)."
            };
            Ok(json!({ "hostname": host, "url": url, "next": next }))
        }
        other => Err(format!("Unknown tool {other}")),
    }
}

/// A project with a free namespace, named like the dashboard does.
async fn new_project(
    s: &AppState,
    user: &User,
    team: &str,
    projects: &[Value],
    name: &str,
) -> Result<Value, String> {
    new_project_with(s, user, team, projects, name, None, "").await
}

async fn new_project_with(
    s: &AppState,
    user: &User,
    team: &str,
    projects: &[Value],
    name: &str,
    namespace: Option<&str>,
    description: &str,
) -> Result<Value, String> {
    let base = {
        let b = slugify(namespace.unwrap_or(name));
        if b.is_empty() {
            "project".to_owned()
        } else {
            b
        }
    };
    let taken = |ns: &str| {
        ns == "default"
            || ns == "pyo-system"
            || ns.starts_with("kube-")
            || projects
                .iter()
                .any(|p| p["teamId"] == json!(team) && p["namespace"] == json!(ns))
    };
    let mut ns = base.clone();
    let mut i = 2;
    while taken(&ns) {
        ns = format!("{}-{i}", &base[..base.len().min(36)]);
        i += 1;
    }
    let id = new_id("prj");
    // A namespace asked for is used as is: taken, it's the API's error.
    let ns = if namespace.is_some() { base } else { ns };
    ops::run_json(s, user, json!({ "op": "createProject", "id": id, "teamId": team, "name": name.trim(), "namespace": ns, "description": description })).await.map_err(err)?;
    Ok(json!({ "id": id, "name": name.trim(), "namespace": ns }))
}

/// The team's default cluster, else its first.
fn team_cluster(snap: &Value, team: &str) -> Result<Value, String> {
    let clusters: Vec<Value> = snap["clusters"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter(|c| c["teamId"] == json!(team))
        .collect();
    let default = snap["teams"]
        .as_array()
        .and_then(|ts| ts.iter().find(|t| t["id"] == json!(team)))
        .map(|t| t["defaultClusterId"].clone());
    clusters
        .iter()
        .find(|c| Some(&c["id"]) == default.as_ref())
        .or(clusters.first())
        .cloned()
        .ok_or_else(|| "This team has no cluster to deploy to. Add a server first.".to_owned())
}

/// A team cluster with servers in a place (country code like "DE", or a
/// city), and the location to pin the service to when the cluster spans several.
fn cluster_at(snap: &Value, team: &str, place: &str) -> Result<(Value, String), String> {
    let want = place.trim().to_lowercase();
    let clusters: Vec<Value> = snap["clusters"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter(|c| c["teamId"] == json!(team))
        .collect();
    let servers = snap["servers"].as_array().cloned().unwrap_or_default();
    let fits = |loc: &str| {
        let l = loc.to_lowercase();
        let (city, cc) = l
            .rsplit_once(',')
            .map(|(a, b)| (a.trim().to_owned(), b.trim().to_owned()))
            .unwrap_or((String::new(), l.trim().to_owned()));
        // A region by its whole name too ("us east").
        l.trim() == want || cc == want || (!city.is_empty() && city == want)
    };
    for c in &clusters {
        let mut places: Vec<String> = servers
            .iter()
            .filter(|n| n["clusterId"] == c["id"])
            .filter_map(|n| n["location"].as_str().map(str::to_owned))
            .collect();
        if places.is_empty() {
            places.push(c["location"].as_str().unwrap_or("").to_owned());
        }
        places.sort();
        places.dedup();
        if let Some(hit) = places.iter().find(|l| fits(l)) {
            return Ok((
                c.clone(),
                if places.len() > 1 {
                    hit.clone()
                } else {
                    String::new()
                },
            ));
        }
    }
    let known: Vec<String> = clusters
        .iter()
        .map(|c| {
            format!(
                "{} ({})",
                c["location"].as_str().unwrap_or("?"),
                c["name"].as_str().unwrap_or("")
            )
        })
        .collect();
    Err(format!(
        "No servers in the region {place}. The team's clusters: {}.",
        if known.is_empty() {
            "none".into()
        } else {
            known.join(", ")
        }
    ))
}

/// "nginx-web.100-64-0-10.nip.io": resolves to the cluster's ingress IP.
fn free_hostname(cluster: &Value, service: &str, namespace: &str) -> Result<String, String> {
    let ip = cluster["capabilities"]["loadBalancerIPs"]
        .as_array()
        .and_then(|ips| ips.iter().filter_map(|i| i.as_str()).find(|i| i.split('.').count() == 4 && i.split('.').all(|p| p.parse::<u8>().is_ok())))
        .ok_or("The cluster has no ingress IP yet, so there's no free domain for it. Use a custom hostname.")?;
    let label: String = format!("{service}-{namespace}").chars().take(63).collect();
    Ok(format!(
        "{}.{}.nip.io",
        label.trim_end_matches('-'),
        ip.replace('.', "-")
    ))
}

/// A new domain on the service's "http" port; HTTPS unless only a private network reaches it.
fn domain(host: &str, now: i64) -> Value {
    let private = crate::deploy::private_host(host);
    json!({ "id": new_id("dom"), "hostname": host, "portName": "http", "status": "pending_dns", "tls": "pending", "tlsMode": "auto", "redirectHttps": !private, "createdAt": now })
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// "nginx:1.27" → ("nginx", "1.27"); a digest stays a digest.
fn split_image(s: &str) -> (String, String) {
    let s = s.trim();
    if let Some((i, d)) = s.split_once('@') {
        return (i.to_owned(), d.to_owned());
    }
    match s.rsplit_once(':') {
        Some((i, t)) if !t.contains('/') => (i.to_owned(), t.to_owned()),
        _ => (s.to_owned(), "latest".to_owned()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn images() {
        assert_eq!(split_image("nginx"), ("nginx".into(), "latest".into()));
        assert_eq!(split_image("nginx:1.27"), ("nginx".into(), "1.27".into()));
        assert_eq!(
            split_image("localhost:5000/app"),
            ("localhost:5000/app".into(), "latest".into())
        );
        assert_eq!(
            split_image("ghcr.io/o/a@sha256:ab"),
            ("ghcr.io/o/a".into(), "sha256:ab".into())
        );
    }

    #[test]
    fn output_is_plain() {
        let v = plain_text(
            json!({ "deployments": [{ "logs": [{ "message": "\x1b[32mok\x1b[0m\x1b[2K", "at": 1 }] }], "runs": [{ "output": "a\nb\x1b[1m" }] }),
        );
        assert_eq!(
            v,
            json!({ "deployments": [{ "logs": [{ "message": "ok", "at": 1 }] }], "runs": [{ "output": "a\nb" }] })
        );
    }

    #[test]
    fn env_values_hidden() {
        let a = redacted(&json!({ "name": "web", "env": { "DATABASE_URL": "postgres://secret" } }));
        assert_eq!(
            a,
            json!({ "name": "web", "env": { "DATABASE_URL": "•••" } })
        );
    }

    /// The tool list for the docs: `cargo test -p pyo-api print_tools -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn print_tools() {
        for t in catalog() {
            let kind = match &t.run {
                Run::Written => "written".to_owned(),
                Run::Op(o) => format!("op {o}"),
                Run::Endpoint(e) => format!("endpoint {e}"),
            };
            println!(
                "{}\t{:?}\t{}\t{}\t{}",
                t.name(),
                t.access,
                kind,
                t.def["title"].as_str().unwrap_or(""),
                t.def["annotations"]
            );
        }
    }

    #[test]
    fn state_shows_env_names_only() {
        let snap = without_env_values(
            json!({ "services": [{ "id": "svc_1", "env": [{ "key": "API_URL", "value": "https://x" }, { "key": "TOKEN", "value": "", "secret": true, "hasValue": true }] }] }),
        );
        assert_eq!(
            snap["services"][0]["env"][0],
            json!({ "key": "API_URL", "value": "•••" })
        );
        assert_eq!(snap["services"][0]["env"][1]["key"], "TOKEN");
    }

    #[test]
    fn secrets_hidden_in_the_log() {
        let a = redacted(
            &json!({ "service": { "name": "web", "env": [{ "key": "A", "value": "x", "secret": true }] }, "password": "p", "privateKey": "k", "name": "n" }),
        );
        assert_eq!(a["service"]["env"][0]["value"], "•••");
        assert_eq!(a["service"]["env"][0]["key"], "A");
        assert_eq!(
            (
                a["password"].as_str(),
                a["privateKey"].as_str(),
                a["name"].as_str()
            ),
            (Some("•••"), Some("•••"), Some("n"))
        );
    }

    fn user(access: Option<KeyAccess>) -> User {
        User {
            id: "usr_1".into(),
            name: "Ada".into(),
            email: "ada@example.com".into(),
            timezone: "UTC".into(),
            is_admin: false,
            must_set_up: false,
            key: access.map(|access| crate::auth::KeyScope {
                access,
                teams: None,
            }),
        }
    }

    /// Every operation API keys can run has a tool (its own, or a written
    /// one that covers it); the others have none. A new operation fails
    /// this until it has one.
    #[test]
    fn every_operation_has_a_tool() {
        let tools = catalog();
        let names: Vec<&str> = tools.iter().map(Tool::name).collect();
        let mut seen = std::collections::HashSet::new();
        for n in &names {
            assert!(seen.insert(*n), "two tools are called {n}");
        }
        for (op, access) in crate::openapi::all_ops() {
            let has = tools.iter().any(|t| t.run == Run::Op(op.clone()))
                || COVERED.iter().any(|(o, t)| *o == op && names.contains(t));
            match access {
                Some(_) => assert!(has, "the operation {op} has no MCP tool"),
                None => assert!(!has, "{op} isn't for API keys, so no tool should run it"),
            }
        }
        // And every endpoint but /ops.
        let spec: Value = serde_json::from_str(&crate::openapi::spec_json()).unwrap();
        for item in spec["paths"].as_object().unwrap().values() {
            for op in item.as_object().unwrap().values() {
                let id = op["operationId"].as_str().unwrap();
                let has = tools.iter().any(|t| t.run == Run::Endpoint(id.into()))
                    || COVERED.iter().any(|(o, _)| *o == id);
                assert!(
                    id == "runOp" || NOT_TOOLS.contains(&id) || has,
                    "the endpoint {id} has no MCP tool"
                );
            }
        }
        // And run_endpoint runs every endpoint tool (and nothing else).
        let src = include_str!("mcp.rs");
        let start = src.find("async fn run_endpoint(").unwrap();
        let body = &src[start..start + src[start..].find("\n}\n").unwrap()];
        let arms: std::collections::HashSet<&str> = body
            .lines()
            .filter_map(|l| {
                l.trim()
                    .strip_prefix('"')?
                    .split_once("\" =>")
                    .map(|(id, _)| id)
            })
            .collect();
        let endpoint_tools: std::collections::HashSet<&str> = tools
            .iter()
            .filter_map(|t| {
                if let Run::Endpoint(id) = &t.run {
                    Some(id.as_str())
                } else {
                    None
                }
            })
            .collect();
        for id in &endpoint_tools {
            assert!(
                arms.contains(id),
                "run_endpoint has no case for {id}, so its tool can't run"
            );
        }
        for id in &arms {
            assert!(
                endpoint_tools.contains(id),
                "run_endpoint runs {id}, which has no endpoint tool"
            );
        }
        assert!(names.len() > 80, "{} tools", names.len());
    }

    #[test]
    fn tools_are_valid() {
        let mut schemas = boon::Schemas::new();
        let mut compiler = boon::Compiler::new();
        for (i, t) in catalog().iter().enumerate() {
            let d = &t.def;
            let name = t.name();
            assert!(
                name.chars().all(|c| c.is_ascii_lowercase() || c == '_') && name.len() <= 64,
                "{name}"
            );
            assert!(
                d["title"].as_str().is_some_and(|x| !x.is_empty()),
                "{name} has no title"
            );
            assert!(
                d["description"].as_str().is_some_and(|x| x.len() > 15),
                "{name} has no description"
            );
            let input = &d["inputSchema"];
            assert_eq!(input["type"], "object", "{name}");
            assert!(
                !input.to_string().contains("$ref"),
                "{name} refers to a schema"
            );
            let props = input["properties"]
                .as_object()
                .unwrap_or_else(|| panic!("{name} has no properties"));
            for r in input["required"].as_array().into_iter().flatten() {
                assert!(
                    props.contains_key(r.as_str().unwrap()),
                    "{name} requires {r}, which it doesn't describe"
                );
            }
            assert!(!props.contains_key("op"), "{name}");
            for k in ["readOnlyHint", "destructiveHint"] {
                assert!(d["annotations"][k].is_boolean(), "{name} has no {k}");
            }
            // A valid JSON Schema (2020-12), which compiles.
            let url = format!("http://tools.test/{i}.json");
            let mut schema = input.clone();
            schema["$schema"] = json!("https://json-schema.org/draft/2020-12/schema");
            compiler.add_resource(&url, schema).unwrap();
            compiler
                .compile(&url, &mut schemas)
                .unwrap_or_else(|e| panic!("{name}: {e}"));
        }
        let by = |n: &str| {
            catalog()
                .iter()
                .find(|t| t.name() == n)
                .unwrap_or_else(|| panic!("no {n}"))
        };
        assert_eq!(by("get_state").def["annotations"]["readOnlyHint"], true);
        assert_eq!(
            by("delete_service").def["annotations"]["destructiveHint"],
            true
        );
        assert_eq!(
            by("save_service").def["annotations"]["idempotentHint"],
            true
        );
        assert!(
            by("reveal_secret").def["description"]
                .as_str()
                .unwrap()
                .starts_with("Returns a secret")
        );
        assert_eq!(by("cancel_deployment").run, Run::Op("cancel".into()));
        assert_eq!(
            by("create_service_from_spec").run,
            Run::Op("createService".into())
        );
        assert_eq!(by("create_service").run, Run::Written);
        assert_eq!(by("remove_server").run, Run::Op("removeServer".into()));
        assert_eq!(
            by("cancel_server_removal").def["annotations"]["destructiveHint"],
            false
        );
        // An example from the reference is a valid input for its tool.
        let input = &by("add_volume").def["inputSchema"];
        assert!(
            input["required"]
                .as_array()
                .unwrap()
                .contains(&json!("serviceId"))
        );
        // Volumes: detaching keeps the data; deleting doesn't.
        assert_eq!(
            by("detach_volume").def["annotations"]["destructiveHint"],
            false
        );
        assert_eq!(
            by("delete_volume").def["annotations"]["destructiveHint"],
            true
        );
        assert_eq!(
            by("restore_volume").def["annotations"]["destructiveHint"],
            true
        );
        assert_eq!(
            by("resize_volume").def["annotations"]["destructiveHint"],
            false
        );
        assert_eq!(
            by("list_volume_snapshots").def["annotations"]["readOnlyHint"],
            true
        );
        assert_eq!(
            by("delete_volume_snapshot").def["annotations"]["destructiveHint"],
            true
        );
        assert_eq!(by("delete_volume_snapshot").access, KeyAccess::Full);
        assert!(
            by("attach_volume").def["inputSchema"]["required"]
                .as_array()
                .unwrap()
                .contains(&json!("mountPath"))
        );
    }

    #[test]
    fn database_imports_are_tools() {
        let by = |n: &str| {
            catalog()
                .iter()
                .find(|t| t.name() == n)
                .unwrap_or_else(|| panic!("no tool {n}"))
        };
        for n in [
            "check_database_import",
            "start_database_import",
            "retry_database_import",
        ] {
            assert_eq!(by(n).access, KeyAccess::Full, "{n}");
        }
        assert!(
            by("start_database_import").def["inputSchema"]["properties"]
                .get("url")
                .is_some()
        );
        // Another platform's API token never passes through an assistant.
        assert!(
            !catalog()
                .iter()
                .any(|t| t.name() == "preview_platform_import"
                    || t.name() == "start_platform_import")
        );
        // A dump's file is sent as a request body: not a tool.
        assert!(!catalog().iter().any(
            |t| t.name() == "upload_database_dump" || t.name() == "create_database_dump_upload"
        ));
    }

    #[test]
    fn access_decides_what_is_listed_and_run() {
        let names = |a: Option<KeyAccess>| -> Vec<String> {
            let u = user(a);
            let mut out = vec![];
            let mut cursor: Option<String> = None;
            loop {
                let page = list_tools(&u, &json!({ "cursor": cursor })).unwrap();
                assert!(page["tools"].as_array().unwrap().len() <= PAGE);
                out.extend(
                    page["tools"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|t| t["name"].as_str().unwrap().to_owned()),
                );
                match page["nextCursor"].as_str() {
                    Some(c) => cursor = Some(c.to_owned()),
                    None => break,
                }
            }
            out
        };
        let read = names(Some(KeyAccess::Read));
        let deploy = names(Some(KeyAccess::Deploy));
        let full = names(Some(KeyAccess::Full));
        assert_eq!(full.len(), catalog().len());
        assert_eq!(names(None).len(), catalog().len(), "a session sees all");
        assert!(
            read.contains(&"get_state".into())
                && read.contains(&"list_services".into())
                && read.contains(&"get_meta".into())
        );
        assert!(
            !read.contains(&"scale_service".into()) && !read.contains(&"delete_service".into())
        );
        assert!(
            deploy.contains(&"scale_service".into())
                && deploy.contains(&"run_cron_job".into())
                && deploy.contains(&"cancel_deployment".into())
        );
        assert!(
            !deploy.contains(&"delete_service".into())
                && !deploy.contains(&"create_service".into())
        );
        assert!(full.contains(&"delete_service".into()));
        // The written tools come first.
        assert_eq!(full[0], "list_teams");
        // Not listed means not run, with the reason.
        let e = find_tool(&user(Some(KeyAccess::Read)), "scale_service").unwrap_err();
        assert!(e.contains("can only read"), "{e}");
        let e = find_tool(&user(Some(KeyAccess::Deploy)), "delete_service").unwrap_err();
        assert!(e.contains("can only deploy"), "{e}");
        assert!(find_tool(&user(Some(KeyAccess::Read)), "get_metrics").is_ok());
        assert!(list_tools(&user(None), &json!({ "cursor": "x" })).is_err());
    }

    #[test]
    fn free_hosts() {
        let snap = json!({ "clusters": [{ "id": "c1", "teamId": "t", "name": "eu", "location": "Frankfurt, DE" }] });
        assert_eq!(cluster_at(&snap, "t", "de").unwrap().0["id"], "c1");
        assert_eq!(cluster_at(&snap, "t", "Frankfurt").unwrap().0["id"], "c1");
        assert!(cluster_at(&snap, "t", "SG").is_err());
        // A cluster spanning two places: picking one pins the service there.
        let snap = json!({ "clusters": [{ "id": "c1", "teamId": "t", "name": "mix", "location": "Yogyakarta, ID" }],
                           "servers": [{ "clusterId": "c1", "location": "Yogyakarta, ID" }, { "clusterId": "c1", "location": "Frankfurt, DE" }] });
        assert_eq!(cluster_at(&snap, "t", "DE").unwrap().1, "Frankfurt, DE");
        let c = json!({ "capabilities": { "loadBalancerIPs": ["fd00::1", "100.64.0.10"] } });
        assert_eq!(
            free_hostname(&c, "nginx", "web").unwrap(),
            "nginx-web.100-64-0-10.nip.io"
        );
    }
}
