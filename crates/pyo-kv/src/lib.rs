//! Valkey over HTTP: a Redis REST API, so the common Redis REST SDKs (and
//! plain fetch) work against a Pyo KV database:
//!
//! ```text
//! POST /                ["SET","k","v"]            -> {"result":"OK"}
//! GET  /set/k/v?EX=60                              -> {"result":"OK"}
//! POST /set/k  (the body is the last argument)     -> {"result":"OK"}
//! POST /pipeline        [["INCR","n"],["GET","n"]] -> [{"result":1},{"result":"1"}]
//! POST /multi-exec      the same, as a transaction
//! ```
//!
//! Auth is `Authorization: Bearer <token>` or `?_token=`. With the base64
//! encoding header the SDKs send, strings come back base64-encoded the way
//! they expect.
//!
//! With read replicas (see `reads`), a request whose commands are all
//! read-only goes to a healthy replica that is close behind the primary;
//! writes, transactions and anything else go to the primary.
//! `Pyo-Read-From: primary` sends a request to the primary regardless, and
//! `Pyo-Served-By` in the response says which answered.

pub mod metrics;
pub mod proxy;
pub mod reads;

use std::{
    collections::HashSet,
    sync::{Arc, LazyLock},
    time::Duration,
};

use axum::{
    Router,
    body::Bytes,
    extract::{DefaultBodyLimit, State},
    http::{HeaderMap, HeaderValue, Method, StatusCode, Uri, header},
    response::{IntoResponse, Response},
};
use base64::{Engine, engine::general_purpose::STANDARD as B64};
use percent_encoding::percent_decode_str;
use pyo_resp::{Pool, Value};
use serde_json::{Value as Json, json};
use subtle::ConstantTimeEq;

pub struct Config {
    pub pool: Pool,
    /// Allows every command.
    pub token: String,
    /// Allows only reads (optional).
    pub readonly_token: Option<String>,
    /// Bounds one request's time on Valkey.
    pub timeout: Duration,
    pub max_body: usize,
    /// Read replicas, when the database has them.
    pub reads: Option<Arc<reads::Reads>>,
}

pub type Cmd = Vec<Vec<u8>>;

/// Request header: `primary` sends the request to the primary.
pub const READ_FROM: &str = "pyo-read-from";
/// Response header: `primary` or `replica`.
pub const SERVED_BY: &str = "pyo-served-by";

/// Where a request runs.
#[derive(Clone, Copy)]
struct Target<'a> {
    pool: &'a Pool,
    timeout: Duration,
    replica: Option<&'a reads::Replica>,
}

pub fn router(cfg: Config) -> Router {
    let max = cfg.max_body;
    Router::new()
        .route("/_ready", axum::routing::get(ready))
        .fallback(handle)
        .layer(DefaultBodyLimit::max(max))
        .with_state(Arc::new(cfg))
}

/// Only /_ready and /_health: the REST API is off.
pub fn health_router(cfg: Config) -> Router {
    Router::new()
        .route("/_ready", axum::routing::get(ready))
        .route(
            "/_health",
            axum::routing::get(|State(cfg): State<Arc<Config>>| async move { health(&cfg).await }),
        )
        .with_state(Arc::new(cfg))
}

/// The process answers (whatever Valkey is doing): for readiness, so a
/// failover never leaves the service without endpoints.
async fn ready() -> Response {
    reply(StatusCode::OK, json!({ "result": "OK" }))
}

fn reply(code: StatusCode, body: Json) -> Response {
    let mut r = (code, body.to_string()).into_response();
    let h = r.headers_mut();
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    h.insert(
        header::ACCESS_CONTROL_ALLOW_ORIGIN,
        HeaderValue::from_static("*"),
    );
    r
}

fn error(code: StatusCode, msg: impl Into<String>) -> Response {
    reply(code, json!({ "error": msg.into() }))
}

async fn handle(
    State(cfg): State<Arc<Config>>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if method == Method::OPTIONS {
        return preflight();
    }
    let path = uri.path().trim_matches('/');
    let query = uri.query().unwrap_or("");
    if path == "_health" {
        return health(&cfg).await;
    }
    if method != Method::GET && method != Method::POST {
        return error(StatusCode::METHOD_NOT_ALLOWED, "Method not allowed");
    }
    let Some(read_only) = auth(&cfg, &headers, query) else {
        return error(StatusCode::UNAUTHORIZED, "Unauthorized");
    };
    let enc = Encoder {
        b64: headers
            .get("upstash-encoding")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.eq_ignore_ascii_case("base64")),
    };
    let post = method == Method::POST;
    let primary_only = headers
        .get(READ_FROM)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.eq_ignore_ascii_case("primary"));
    let (cmds, kind) = if post && (path == "pipeline" || path == "multi-exec") {
        match parse_commands(&body).and_then(|c| check(&c, read_only).map(|_| c)) {
            Ok(c) => (
                c,
                if path == "pipeline" {
                    Kind::Pipeline
                } else {
                    Kind::Multi
                },
            ),
            Err(e) => return error(StatusCode::BAD_REQUEST, e),
        }
    } else {
        let cmd = if path.is_empty() && post {
            parse_command(&body)
        } else {
            path_command(path, query, &body)
        };
        match cmd.and_then(|c| check(std::slice::from_ref(&c), read_only).map(|_| c)) {
            Ok(c) => (vec![c], Kind::Single),
            Err(e) => return error(StatusCode::BAD_REQUEST, e),
        }
    };
    use metrics::{COUNTERS as C, add};
    add(&C.commands_http, cmds.len());
    // Reads are counted with read replicas only.
    let reads = cfg
        .reads
        .as_ref()
        .is_some_and(|r| kind != Kind::Multi && r.routable(&cmds));
    let primary = Target {
        pool: &cfg.pool,
        timeout: cfg.timeout,
        replica: None,
    };
    // Transactions, and anything that isn't a known read, run on the primary.
    let replica = match &cfg.reads {
        Some(r) if !primary_only && kind != Kind::Multi => {
            if r.routable(&cmds) {
                r.pick().map(|rep| Target {
                    pool: &rep.pool,
                    timeout: r.timeout,
                    replica: Some(rep),
                })
            } else {
                r.wrote();
                None
            }
        }
        _ => None,
    };
    let mut res = match replica {
        Some(t) => match run(t, kind, &cmds, enc).await {
            // Any trouble on a replica: the primary answers instead (these
            // are reads, so running them again is harmless).
            Ok(r) if !r.1 => {
                add(&C.reads_replica, cmds.len());
                Ok((r.0, "replica"))
            }
            _ => {
                add(&C.fallbacks_http, 1);
                if reads {
                    add(&C.reads_primary, cmds.len());
                }
                if let Some(rep) = t.replica {
                    tracing::warn!(replica = %rep.name, "read replica failed a request; the primary answers");
                    rep.fail();
                }
                run(primary, kind, &cmds, enc)
                    .await
                    .map(|r| (r.0, "primary"))
            }
        },
        None => {
            if reads {
                add(&C.reads_primary, cmds.len());
            }
            run(primary, kind, &cmds, enc)
                .await
                .map(|r| (r.0, "primary"))
        }
    };
    if res.is_err() {
        add(&C.errors_http, 1);
    }
    if cfg.reads.is_none() {
        // No read replicas: no header, exactly as before.
        return res
            .map(|r| r.0)
            .unwrap_or_else(|e| error(StatusCode::SERVICE_UNAVAILABLE, e));
    }
    if let Ok((r, by)) = &mut res {
        let h = r.headers_mut();
        h.insert(SERVED_BY, HeaderValue::from_static(by));
        h.insert(
            header::ACCESS_CONTROL_EXPOSE_HEADERS,
            HeaderValue::from_static("Pyo-Served-By"),
        );
    }
    res.map(|r| r.0)
        .unwrap_or_else(|e| error(StatusCode::SERVICE_UNAVAILABLE, e))
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Single,
    Pipeline,
    Multi,
}

/// Runs a request on t: the response, and whether a replica couldn't serve
/// it (still loading, or cut off from the primary).
async fn run(
    t: Target<'_>,
    kind: Kind,
    cmds: &[Cmd],
    enc: Encoder,
) -> Result<(Response, bool), String> {
    match kind {
        Kind::Single => single(t, cmds[0].clone(), enc).await,
        Kind::Pipeline => pipeline(t, cmds.to_vec(), enc).await,
        Kind::Multi => multi_exec(t, cmds.to_vec(), enc).await.map(|r| (r, false)),
    }
}

/// A replica's error that means "ask the primary". An unknown command is
/// one the primary knows (only those go to replicas): a module being turned
/// on or off, which restarts the replicas before the primary.
fn unready(v: &Value) -> bool {
    match v {
        Value::Error(e) => {
            e.starts_with(b"LOADING")
                || e.starts_with(b"MASTERDOWN")
                || e.starts_with(b"ERR unknown command")
        }
        Value::Array(a) => a.iter().any(unready),
        _ => false,
    }
}

fn preflight() -> Response {
    let mut r = StatusCode::NO_CONTENT.into_response();
    let h = r.headers_mut();
    h.insert(
        header::ACCESS_CONTROL_ALLOW_ORIGIN,
        HeaderValue::from_static("*"),
    );
    h.insert(
        header::ACCESS_CONTROL_ALLOW_METHODS,
        HeaderValue::from_static("GET, POST, OPTIONS"),
    );
    h.insert(
        header::ACCESS_CONTROL_ALLOW_HEADERS,
        HeaderValue::from_static("Authorization, Content-Type, Upstash-Encoding, Upstash-Sync-Token, Upstash-Telemetry-Runtime, Upstash-Telemetry-Platform, Upstash-Telemetry-Sdk, Upstash-Telemetry-Retry, Pyo-Read-From"),
    );
    h.insert(
        header::ACCESS_CONTROL_MAX_AGE,
        HeaderValue::from_static("86400"),
    );
    r
}

/// Some(read_only) when the token is good.
fn auth(cfg: &Config, headers: &HeaderMap, query: &str) -> Option<bool> {
    let tok = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::to_owned)
        .or_else(|| query_value(query, "_token"))?;
    let eq = |want: &str| !want.is_empty() && bool::from(tok.as_bytes().ct_eq(want.as_bytes()));
    if eq(&cfg.token) {
        return Some(false);
    }
    cfg.readonly_token
        .as_deref()
        .is_some_and(eq)
        .then_some(true)
}

async fn health(cfg: &Config) -> Response {
    let t = Target {
        pool: &cfg.pool,
        timeout: cfg.timeout,
        replica: None,
    };
    match exec(t, true, |c| {
        Box::pin(async move { c.cmd(&[b"PING"]).await })
    })
    .await
    {
        Ok(v) if !v.is_error() => reply(StatusCode::OK, json!({ "result": "OK" })),
        _ => error(StatusCode::SERVICE_UNAVAILABLE, "unavailable"),
    }
}

async fn single(t: Target<'_>, cmd: Cmd, enc: Encoder) -> Result<(Response, bool), String> {
    let retry = idempotent(&cmd);
    let v = exec(t, retry, |c| {
        let cmd = cmd.clone();
        Box::pin(async move { c.cmd(&cmd).await })
    })
    .await?;
    let unready = t.replica.is_some() && unready(&v);
    Ok((
        match v {
            Value::Error(e) => error(StatusCode::BAD_REQUEST, String::from_utf8_lossy(&e)),
            v => reply(StatusCode::OK, json!({ "result": enc.top(&v) })),
        },
        unready,
    ))
}

async fn pipeline(t: Target<'_>, cmds: Vec<Cmd>, enc: Encoder) -> Result<(Response, bool), String> {
    let retry = cmds.iter().all(idempotent);
    let out = exec(t, retry, |c| {
        let cmds = cmds.clone();
        Box::pin(async move {
            let refs: Vec<&[Vec<u8>]> = cmds.iter().map(|c| c.as_slice()).collect();
            c.send(&refs).await?;
            let mut out = Vec::with_capacity(cmds.len());
            for _ in &cmds {
                out.push(c.read().await?);
            }
            Ok(Value::Array(out))
        })
    })
    .await?;
    let unready = t.replica.is_some() && unready(&out);
    let Value::Array(out) = out else {
        unreachable!()
    };
    Ok((reply(StatusCode::OK, results(&out, enc)), unready))
}

async fn multi_exec(t: Target<'_>, cmds: Vec<Cmd>, enc: Encoder) -> Result<Response, String> {
    let mut all: Vec<Cmd> = Vec::with_capacity(cmds.len() + 2);
    all.push(vec![b"MULTI".to_vec()]);
    all.extend(cmds);
    all.push(vec![b"EXEC".to_vec()]);
    let out = exec(t, false, |c| {
        let all = all.clone();
        Box::pin(async move {
            let refs: Vec<&[Vec<u8>]> = all.iter().map(|c| c.as_slice()).collect();
            c.send(&refs).await?;
            let mut out = Vec::with_capacity(all.len());
            for _ in &all {
                out.push(c.read().await?);
            }
            Ok(Value::Array(out))
        })
    })
    .await?;
    let Value::Array(mut out) = out else {
        unreachable!()
    };
    let exec_reply = out.pop().unwrap_or(Value::Nil);
    // A command that couldn't be queued aborts the whole transaction.
    if let Some(Value::Error(e)) = out.iter().find(|v| v.is_error()) {
        return Ok(error(StatusCode::BAD_REQUEST, String::from_utf8_lossy(e)));
    }
    Ok(match exec_reply {
        Value::Array(items) => reply(StatusCode::OK, results(&items, enc)),
        Value::Error(e) => error(StatusCode::BAD_REQUEST, String::from_utf8_lossy(&e)),
        _ => error(StatusCode::BAD_REQUEST, "ERR transaction was aborted"),
    })
}

fn results(vs: &[Value], enc: Encoder) -> Json {
    Json::Array(
        vs.iter()
            .map(|v| match v {
                Value::Error(e) => json!({ "error": String::from_utf8_lossy(e) }),
                v => json!({ "result": enc.top(v) }),
            })
            .collect(),
    )
}

const UNAVAILABLE: &str = "ERR the cache is unavailable, try again";

type Fut<'a> = std::pin::Pin<Box<dyn Future<Output = std::io::Result<Value>> + Send + 'a>>;

/// Runs f on a pooled connection. A reused connection that fails before any
/// reply arrived is retried once on a fresh one when retry is set: the
/// server never answered, and for these commands running twice is harmless.
async fn exec<F>(t: Target<'_>, retry: bool, f: F) -> Result<Value, String>
where
    F: for<'a> Fn(&'a mut pyo_resp::Conn) -> Fut<'a>,
{
    let pool = t.pool;
    for attempt in 0.. {
        // Getting a connection counts against the deadline too: connecting
        // to a frozen replica must not hold a request.
        let mut p = match tokio::time::timeout(t.timeout, pool.get()).await {
            Ok(Ok(p)) => p,
            Ok(Err(e)) => {
                tracing::warn!(error = %e, addr = %pool.addr(), "valkey connect");
                return Err(UNAVAILABLE.into());
            }
            Err(_) => {
                tracing::warn!(addr = %pool.addr(), "valkey connect timed out");
                return Err(UNAVAILABLE.into());
            }
        };
        match tokio::time::timeout(t.timeout, f(&mut p.conn)).await {
            Ok(Ok(v)) => {
                // A primary turned replica: drop every connection so the next
                // ones reach the new primary.
                let moved = matches!(&v, Value::Error(e) if e.starts_with(b"READONLY"));
                if moved {
                    pool.reset();
                }
                pool.put(p, moved);
                return Ok(v);
            }
            Ok(Err(e)) => {
                let (reused, answered) = (p.reused, p.conn.answered());
                pool.put(p, true);
                // Every pooled connection goes to the same primary: when one
                // breaks, the rest likely are too (it died or moved).
                pool.reset();
                if reused && !answered && attempt == 0 && retry {
                    continue;
                }
                tracing::warn!(error = %e, "valkey");
                return Err(UNAVAILABLE.into());
            }
            Err(_) => {
                pool.put(p, true);
                pool.reset();
                return Err("ERR the command timed out".into());
            }
        }
    }
    unreachable!()
}

/// Turns replies into JSON.
#[derive(Clone, Copy, Default)]
pub struct Encoder {
    pub b64: bool,
}

impl Encoder {
    /// A whole result. The SDK decodes every string except a bare "OK", and
    /// inside a top-level array it decodes every string, so only "OK" status
    /// replies outside depth 1 stay as they are.
    pub fn top(&self, v: &Value) -> Json {
        self.value(v, 0)
    }

    fn value(&self, v: &Value, depth: usize) -> Json {
        let text = |b: &[u8]| {
            if self.b64 {
                Json::String(B64.encode(b))
            } else {
                Json::String(String::from_utf8_lossy(b).into_owned())
            }
        };
        match v {
            Value::Int(n) => json!(n),
            Value::Nil => Json::Null,
            Value::Array(a) => Json::Array(a.iter().map(|x| self.value(x, depth + 1)).collect()),
            Value::Simple(s) if !self.b64 || (depth != 1 && s == b"OK") => {
                Json::String(String::from_utf8_lossy(s).into_owned())
            }
            Value::Simple(b) | Value::Bulk(b) | Value::Error(b) => text(b),
        }
    }
}

/// ["SET","k","v"]. Numbers and booleans become their text.
pub fn parse_command(body: &[u8]) -> Result<Cmd, String> {
    let v: Json = serde_json::from_slice(body)
        .map_err(|_| "ERR the body should be a JSON array of the command and its arguments")?;
    match v {
        Json::Array(a) => to_args(a),
        _ => Err("ERR the body should be a JSON array of the command and its arguments".into()),
    }
}

/// [["SET","k","v"],["GET","k"]].
pub fn parse_commands(body: &[u8]) -> Result<Vec<Cmd>, String> {
    const MSG: &str = "ERR the body should be a JSON array of commands, each an array";
    let Json::Array(cmds) = serde_json::from_slice(body).map_err(|_| MSG)? else {
        return Err(MSG.into());
    };
    if cmds.is_empty() {
        return Err("ERR no commands".into());
    }
    cmds.into_iter()
        .map(|c| match c {
            Json::Array(a) => to_args(a),
            _ => Err(MSG.into()),
        })
        .collect()
}

fn to_args(a: Vec<Json>) -> Result<Cmd, String> {
    if a.is_empty() {
        return Err("ERR empty command".into());
    }
    a.into_iter()
        .map(|v| match v {
            Json::String(s) => Ok(s.into_bytes()),
            Json::Number(n) => Ok(n.to_string().into_bytes()),
            Json::Bool(b) => Ok(b.to_string().into_bytes()),
            Json::Null => Err("ERR null isn't a valid argument".into()),
            _ => Err("ERR arguments must be strings or numbers".into()),
        })
        .collect()
}

/// /set/k/v?EX=60: path segments, then query pairs in order, then the body
/// (if any) as the last argument.
pub fn path_command(path: &str, query: &str, body: &[u8]) -> Result<Cmd, String> {
    if path.is_empty() {
        return Err("ERR no command".into());
    }
    let mut args: Cmd = path
        .split('/')
        .map(|s| percent_decode_str(s).collect())
        .collect();
    for kv in query.split('&').filter(|s| !s.is_empty()) {
        let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
        if k == "_token" {
            continue;
        }
        args.push(form_decode(k));
        if !v.is_empty() {
            args.push(form_decode(v));
        }
    }
    if !body.is_empty() {
        args.push(body.to_vec());
    }
    Ok(args)
}

fn form_decode(s: &str) -> Vec<u8> {
    percent_decode_str(&s.replace('+', " ")).collect()
}

fn query_value(query: &str, key: &str) -> Option<String> {
    query.split('&').find_map(|kv| {
        let (k, v) = kv.split_once('=')?;
        (k == key).then(|| String::from_utf8_lossy(&form_decode(v)).into_owned())
    })
}

/// Refuses commands that would break a shared connection or the server.
pub fn check(cmds: &[Cmd], read_only: bool) -> Result<(), String> {
    for c in cmds {
        let name = String::from_utf8_lossy(&c[0]).to_ascii_uppercase();
        if let Some(why) = denied(&name) {
            return Err(format!("ERR {name} isn't available over REST: {why}"));
        }
        if read_only && !READS.contains(name.as_str()) {
            return Err(format!(
                "NOPERM this token can only read, and {name} writes"
            ));
        }
    }
    Ok(())
}

fn denied(name: &str) -> Option<&'static str> {
    const STATE: &str = "it changes the connection's state";
    const ADMIN: &str = "it's an admin command";
    const TX: &str = "use /multi-exec";
    Some(match name {
        "AUTH" | "HELLO" | "SELECT" | "CLIENT" | "RESET" | "QUIT" | "READONLY" | "READWRITE"
        | "SUBSCRIBE" | "PSUBSCRIBE" | "SSUBSCRIBE" | "UNSUBSCRIBE" | "PUNSUBSCRIBE"
        | "SUNSUBSCRIBE" | "MONITOR" => STATE,
        "MULTI" | "EXEC" | "DISCARD" | "WATCH" | "UNWATCH" => TX,
        "SYNC" | "PSYNC" | "REPLCONF" | "SHUTDOWN" | "DEBUG" | "CONFIG" | "MODULE"
        | "REPLICAOF" | "SLAVEOF" | "FAILOVER" | "CLUSTER" | "ACL" | "SAVE" | "BGSAVE"
        | "BGREWRITEAOF" | "MIGRATE" | "SENTINEL" | "LATENCY" | "SLOWLOG" => ADMIN,
        _ => return None,
    })
}

/// Commands a read-only token may run. Replicas answer those among them
/// Valkey flags readonly and not blocking (see `reads`).
pub static READS: LazyLock<HashSet<&'static str>> = LazyLock::new(|| {
    "PING ECHO TIME DBSIZE TYPE TTL PTTL EXPIRETIME PEXPIRETIME EXISTS KEYS SCAN RANDOMKEY STRLEN GET MGET \
     GETRANGE SUBSTR GETBIT BITCOUNT BITPOS LCS HGET HMGET HGETALL HKEYS HVALS HLEN HEXISTS HSTRLEN HSCAN \
     HRANDFIELD LRANGE LINDEX LLEN LPOS SMEMBERS SISMEMBER SMISMEMBER SCARD SRANDMEMBER SSCAN SINTER SINTERCARD \
     SUNION SDIFF ZRANGE ZRANGEBYSCORE ZREVRANGE ZREVRANGEBYSCORE ZRANGEBYLEX ZREVRANGEBYLEX ZSCORE ZMSCORE \
     ZRANK ZREVRANK ZCARD ZCOUNT ZLEXCOUNT ZSCAN ZRANDMEMBER ZINTER ZINTERCARD ZUNION ZDIFF XRANGE XREVRANGE \
     XLEN XREAD XINFO XPENDING PFCOUNT GEOPOS GEODIST GEOHASH GEOSEARCH GEORADIUS_RO GEORADIUSBYMEMBER_RO \
     OBJECT MEMORY DUMP TOUCH SORT_RO EVAL_RO EVALSHA_RO FCALL_RO JSON.GET JSON.MGET JSON.TYPE JSON.STRLEN \
     JSON.OBJKEYS JSON.OBJLEN JSON.ARRLEN JSON.ARRINDEX JSON.RESP BF.EXISTS BF.MEXISTS BF.CARD BF.INFO \
     FT.SEARCH FT.INFO"
        .split_whitespace()
        .collect()
});

/// Reads are safe to send again when the first try got no answer; a write
/// might have run, so it isn't.
fn idempotent(cmd: &Cmd) -> bool {
    READS.contains(
        String::from_utf8_lossy(&cmd[0])
            .to_ascii_uppercase()
            .as_str(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strs(c: &Cmd) -> Vec<String> {
        c.iter()
            .map(|b| String::from_utf8_lossy(b).into_owned())
            .collect()
    }

    #[test]
    fn parses_json_commands() {
        let c = parse_command(br#"["SET","k",12,"EX",1.5,true]"#).unwrap();
        assert_eq!(strs(&c), ["SET", "k", "12", "EX", "1.5", "true"]);
        for bad in [
            &b"[]"[..],
            br#"{"a":1}"#,
            br#"["SET",null]"#,
            br#"["SET",["x"]]"#,
            b"nope",
        ] {
            assert!(
                parse_command(bad).is_err(),
                "{}",
                String::from_utf8_lossy(bad)
            );
        }
        assert_eq!(
            parse_commands(br#"[["INCR","n"],["GET","n"]]"#)
                .unwrap()
                .len(),
            2
        );
    }

    #[test]
    fn parses_path_commands() {
        let c = path_command("set/a%2Fb/hello%20world", "EX=60&NX&_token=secret", b"body").unwrap();
        assert_eq!(
            strs(&c),
            ["set", "a/b", "hello world", "EX", "60", "NX", "body"]
        );
        assert_eq!(
            query_value("a=1&_token=t%2Bk", "_token").as_deref(),
            Some("t+k")
        );
    }

    #[test]
    fn encodes_like_the_sdk_expects() {
        let ok = Value::Simple(b"OK".to_vec());
        let arr = Value::Array(vec![
            ok.clone(),
            Value::Bulk(vec![0xff, b'a']),
            Value::Int(3),
            Value::Nil,
        ]);
        let b64 = Encoder { b64: true };
        assert_eq!(b64.top(&ok), json!("OK"));
        assert_eq!(b64.top(&Value::Simple(b"PONG".to_vec())), json!("UE9ORw=="));
        // Inside a top-level array the SDK decodes every string, "OK" too.
        assert_eq!(b64.top(&arr), json!(["T0s=", "/2E=", 3, null]));
        assert_eq!(
            Encoder::default().top(&arr),
            json!(["OK", "\u{fffd}a", 3, null])
        );
    }

    #[test]
    fn checks_commands() {
        let get = vec![b"get".to_vec(), b"k".to_vec()];
        let set = vec![b"set".to_vec(), b"k".to_vec(), b"v".to_vec()];
        assert!(check(&[get], true).is_ok());
        assert!(check(&[set], true).unwrap_err().starts_with("NOPERM"));
        assert!(check(&[vec![b"subscribe".to_vec()]], false).is_err());
    }
}
