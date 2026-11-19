//! Setting up Tailscale from one API access token (Integrations › Tailscale).
//!
//! An admin pastes a short-lived API access token (Settings › Keys in the
//! Tailscale console). [`plan`] reads the tailnet and says what's missing,
//! changing nothing. [`apply`] then makes those changes:
//!
//! - **MagicDNS** and **HTTPS certificates** on.
//! - **The policy**: `tag:pyo` in `tagOwners`, a grant to reach it on ports
//!   80 and 443 when the policy doesn't allow that already, and
//!   `autoApprovers.services` for it. The policy is HuJSON and is edited in
//!   place, so its comments and formatting stay. It's validated first and
//!   written with `If-Match`, so a change made meanwhile is never lost.
//! - **Pyo's own OAuth client**, with the scopes the manual path asks for,
//!   saved and checked like one pasted by hand ([`crate::tailscale::save_client`]).
//! - **The pasted token is deleted** last, once nothing else needs it.
//!
//! The token lives in memory for one request only: it's never stored,
//! logged or put in an error ([`Token`], [`Api::scrub`]). A tailnet that's
//! already set up gets no changes. A policy managed in Git isn't written:
//! the lines to add come back instead.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use zeroize::Zeroize;

use crate::AppState;
use crate::error::{ApiError, ApiResult};
use crate::tailscale::{Reply, TAG, TsError};

/// The scopes Pyo's own client gets: what the manual path asks for.
const SCOPES: [&str; 3] = ["devices:core", "auth_keys", "services"];
/// Who may apply tag:pyo by hand (Owners, Admins and Network admins can anyway).
const OWNERS: &str = "autogroup:admin";

const BAD_TOKEN: &str =
    "Tailscale didn't accept the token. It may be wrong or expired. Generate a new one.";
const NOT_ADMIN: &str =
    "That token can't change your tailnet's settings. Use a token from a tailnet Owner or Admin.";
const DELETE_TOKEN: &str = "Delete the token under Settings › Keys in the Tailscale console. It expires within a day anyway.";

// ── The token ───────────────────────────────────────────────────────────────

/// A pasted API access token. It never prints, and is wiped when dropped.
#[derive(Deserialize)]
#[serde(transparent)]
pub struct Token(String);

impl Token {
    #[cfg(test)]
    pub fn new(s: &str) -> Self {
        Self(s.to_owned())
    }

    /// The key ID in "tskey-api-<id>-<secret>": what deletes it.
    fn key_id(&self) -> Option<&str> {
        let id = self.0.strip_prefix("tskey-api-")?.split('-').next()?;
        (!id.is_empty() && id.bytes().all(|b| b.is_ascii_alphanumeric())).then_some(id)
    }
}

impl std::fmt::Debug for Token {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Token(•••)")
    }
}

impl Drop for Token {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

/// The token's first check: the shape of an API access token.
fn check_shape(t: &Token) -> Result<(), String> {
    let s = t.0.trim();
    if s.starts_with("tskey-client-") {
        return Err("That's an OAuth client secret. Paste an API access token, or set up with an OAuth client instead.".into());
    }
    if s.starts_with("tskey-auth-") {
        return Err(
            "That's an auth key. Paste an API access token. It starts with tskey-api-.".into(),
        );
    }
    if !s.starts_with("tskey-api-") || s.len() < 20 || s.contains(char::is_whitespace) {
        return Err("Paste an API access token. It starts with tskey-api-.".into());
    }
    Ok(())
}

// ── Tailscale's API, as the token's admin ───────────────────────────────────

/// What goes in a request.
enum Body {
    None,
    Json(Value),
    HuJson(String),
}

/// The answer's body as JSON (Null when it isn't).
fn json_of(r: &Reply) -> Value {
    serde_json::from_str(&r.text).unwrap_or(Value::Null)
}

/// Tailscale's API with the pasted token.
pub struct Api {
    base: String,
    token: Token,
}

impl Api {
    pub fn new(token: Token) -> Self {
        Self::at(crate::tailscale::API, token)
    }

    fn at(base: &str, mut token: Token) -> Self {
        let t = token.0.trim().to_owned();
        token.0.zeroize();
        token.0 = t;
        Self {
            base: base.trim_end_matches('/').to_owned(),
            token,
        }
    }

    /// `s` with the token taken out, whatever Tailscale or the network said.
    fn scrub(&self, s: &str) -> String {
        let mut out = s.replace(&self.token.0, "•••");
        // Its secret part alone, too.
        if let Some(secret) = self.token.0.rsplit('-').next().filter(|x| x.len() >= 8) {
            out = out.replace(secret, "•••");
        }
        out
    }

    /// A request with the token, through the same path as Pyo's own client
    /// ([`crate::tailscale::send`]), with the token scrubbed from any error.
    async fn send(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Body,
        headers: &[(&str, &str)],
    ) -> Result<Reply, TsError> {
        let mut req = crate::tailscale::http()
            .request(method, format!("{}{path}", self.base))
            .bearer_auth(&self.token.0);
        for (k, v) in headers {
            req = req.header(*k, *v);
        }
        req = match body {
            Body::None => req,
            Body::Json(v) => req.json(&v),
            Body::HuJson(t) => req
                .header(reqwest::header::CONTENT_TYPE, "application/hujson")
                .body(t),
        };
        crate::tailscale::send(req).await.map_err(|e| TsError {
            status: e.status,
            message: self.scrub(&e.message),
        })
    }

    async fn get(&self, path: &str) -> Result<Value, TsError> {
        Ok(json_of(
            &self
                .send(reqwest::Method::GET, path, Body::None, &[])
                .await?,
        ))
    }

    /// The policy file as written, and its ETag.
    async fn policy(&self) -> Result<(String, Option<String>), TsError> {
        let r = self
            .send(
                reqwest::Method::GET,
                "/tailnet/-/acl",
                Body::None,
                &[("accept", "application/hujson")],
            )
            .await?;
        Ok((r.text, r.etag))
    }
}

/// A plain sentence for what went wrong while Pyo tried `what` ("read your
/// tailnet policy").
fn say(e: &TsError, what: &str) -> String {
    match e.status {
        401 => BAD_TOKEN.into(),
        403 => NOT_ADMIN.into(),
        0 => e.message.clone(),
        _ => {
            let m = e.message.trim().trim_end_matches('.');
            if m.is_empty() {
                format!("Couldn't {what}. Tailscale answered {}.", e.status)
            } else {
                format!("Couldn't {what}. Tailscale said: {m}.")
            }
        }
    }
}

// ── The policy ──────────────────────────────────────────────────────────────

/// What HuJSON allows beyond JSON: comments and trailing commas.
fn hujson() -> jsonc_parser::ParseOptions {
    jsonc_parser::ParseOptions {
        allow_comments: true,
        allow_trailing_commas: true,
        allow_loose_object_property_names: false,
        allow_missing_commas: false,
        allow_single_quoted_strings: false,
        allow_hexadecimal_numbers: false,
        allow_unary_plus_numbers: false,
        allow_bare_decimal_point_numbers: false,
        allow_non_finite_numbers: false,
        allow_extended_string_escapes: false,
    }
}

fn parse(text: &str) -> Result<Value, String> {
    jsonc_parser::parse_to_serde_value::<Option<Value>>(text, &hujson())
        .map_err(|e| format!("Pyo couldn't read your tailnet policy: {e}"))?
        .filter(Value::is_object)
        .ok_or_else(|| "Pyo couldn't read your tailnet policy: it isn't an object.".into())
}

/// What the policy is missing for Pyo.
#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct Needs {
    /// `tag:pyo` in `tagOwners`: needed before anything can carry the tag.
    pub tag_owner: bool,
    /// A grant to tag:pyo on ports 80 and 443: only when the policy doesn't
    /// already let the tailnet reach it.
    pub grant: bool,
    /// `autoApprovers.services` for tag:pyo. Pyo approves its Services'
    /// hosts through the API anyway, so this only adds resilience.
    pub approver: bool,
}

impl Needs {
    fn any(&self) -> bool {
        self.tag_owner || self.grant || self.approver
    }
    /// Without these the setup can't work.
    fn required(&self) -> bool {
        self.tag_owner || self.grant
    }
}

fn strs(v: &Value) -> impl Iterator<Item = &str> {
    v.as_array().into_iter().flatten().filter_map(Value::as_str)
}

pub fn needs(p: &Value) -> Needs {
    Needs {
        tag_owner: p["tagOwners"].get(TAG).is_none(),
        grant: reaches_pyo(p) == Some(false),
        approver: !strs(&p["autoApprovers"]["services"][TAG]).any(|s| s == TAG),
    }
}

/// Whether "80", "*", "80,443" or "1-1024" takes in `port`.
fn ports_cover(ports: &str, port: u16) -> bool {
    ports.split(',').map(str::trim).any(|p| {
        if p == "*" {
            return true;
        }
        match p.split_once('-') {
            Some((a, b)) => matches!((a.parse::<u16>(), b.parse::<u16>()), (Ok(a), Ok(b)) if a <= port && port <= b),
            None => p.parse::<u16>() == Ok(port),
        }
    })
}

/// Whether everyone on the tailnet can reach tag:pyo on ports 80 and 443,
/// by a rule that plainly says so: from "*" or autogroup:member, to "*" or
/// tag:pyo, with no posture or route conditions. None when the policy has
/// neither `acls` nor `grants`, so Pyo doesn't add the first rule there.
pub fn reaches_pyo(p: &Value) -> Option<bool> {
    if p.get("acls").is_none() && p.get("grants").is_none() {
        return None;
    }
    let everyone = |v: &Value| strs(v).any(|s| s == "*" || s == "autogroup:member");
    let plain = |r: &Value| r.get("srcPosture").is_none() && r.get("via").is_none();
    let mut open = [false, false];
    let mut take = |ports: &str| {
        for (i, port) in [80, 443].into_iter().enumerate() {
            open[i] |= ports_cover(ports, port);
        }
    };
    for r in p["acls"].as_array().into_iter().flatten() {
        let tcp = match &r["proto"] {
            Value::Null => true,
            v => matches!(v.as_str(), Some("tcp" | "6")),
        };
        if r["action"].as_str() != Some("accept")
            || !(everyone(&r["src"]) || everyone(&r["users"]))
            || !tcp
            || !plain(r)
        {
            continue;
        }
        for d in strs(&r["dst"]).chain(strs(&r["ports"])) {
            if let Some((host, ports)) = d.rsplit_once(':')
                && (host == "*" || host == TAG)
            {
                take(ports);
            }
        }
    }
    for g in p["grants"].as_array().into_iter().flatten() {
        if !everyone(&g["src"]) || !plain(g) || !strs(&g["dst"]).any(|d| d == "*" || d == TAG) {
            continue;
        }
        for ip in strs(&g["ip"]) {
            let (proto, ports) = match ip.split_once(':') {
                Some(x) => x,
                None if ip.bytes().next().is_some_and(|b| b.is_ascii_digit()) => ("*", ip),
                None => (ip, "*"),
            };
            if matches!(proto, "*" | "tcp" | "6") {
                take(ports);
            }
        }
    }
    Some(open == [true, true])
}

fn strings(xs: &[&str]) -> jsonc_parser::cst::CstInputValue {
    jsonc_parser::cst::CstInputValue::Array(
        xs.iter()
            .map(|s| jsonc_parser::cst::CstInputValue::String((*s).to_owned()))
            .collect(),
    )
}

/// `text` with what `needs` says added, edited in place: comments,
/// order and formatting stay as they were.
pub fn patch(text: &str, needs: &Needs) -> Result<String, String> {
    use jsonc_parser::cst::{CstInputValue, CstRootNode};
    let cant = |what: &str| format!("Pyo can't edit your tailnet policy: {what} isn't an object.");
    let root = CstRootNode::parse(text, &hujson())
        .map_err(|e| format!("Pyo couldn't read your tailnet policy: {e}"))?;
    let obj = root.object_value().ok_or_else(|| cant("the policy"))?;
    if needs.tag_owner {
        obj.object_value_or_create("tagOwners")
            .ok_or_else(|| cant("tagOwners"))?
            .append(TAG, strings(&[OWNERS]));
    }
    if needs.grant {
        obj.array_value_or_create("grants")
            .ok_or_else(|| "Pyo can't edit your tailnet policy: grants isn't a list.".to_owned())?
            .append(CstInputValue::Object(vec![
                ("src".into(), strings(&["autogroup:member"])),
                ("dst".into(), strings(&[TAG])),
                ("ip".into(), strings(&["tcp:80", "tcp:443"])),
            ]));
    }
    if needs.approver {
        let services = obj
            .object_value_or_create("autoApprovers")
            .ok_or_else(|| cant("autoApprovers"))?
            .object_value_or_create("services")
            .ok_or_else(|| cant("autoApprovers.services"))?;
        match services.get(TAG) {
            None => {
                services.append(TAG, strings(&[TAG]));
            }
            Some(_) => {
                services
                    .array_value(TAG)
                    .ok_or_else(|| {
                        "Pyo can't edit your tailnet policy: its tag:pyo approvers aren't a list."
                            .to_owned()
                    })?
                    .append(CstInputValue::String(TAG.into()));
            }
        }
    }
    let out = root.to_string();
    // What it did, read back.
    if needs.any() && self::needs(&parse(&out)?).required() {
        return Err("Pyo couldn't edit your tailnet policy.".into());
    }
    Ok(out)
}

/// The lines `new` adds to `old` ("+ …") and the ones it changes ("- …"),
/// in order.
pub fn diff(old: &str, new: &str) -> Vec<String> {
    let a: Vec<&str> = old.lines().collect();
    let b: Vec<&str> = new.lines().collect();
    // Policy files are small. A huge one just lists what's new.
    if a.len() * b.len() > 4_000_000 {
        return b
            .iter()
            .filter(|l| !a.contains(l))
            .map(|l| format!("+ {l}"))
            .collect();
    }
    let (n, m) = (a.len(), b.len());
    let mut lcs = vec![vec![0u32; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            lcs[i][j] = if a[i] == b[j] {
                lcs[i + 1][j + 1] + 1
            } else {
                lcs[i + 1][j].max(lcs[i][j + 1])
            };
        }
    }
    let (mut i, mut j, mut out) = (0, 0, vec![]);
    while i < n || j < m {
        if i < n && j < m && a[i] == b[j] {
            i += 1;
            j += 1;
        } else if j < m && (i == n || lcs[i][j + 1] >= lcs[i + 1][j]) {
            out.push(format!("+ {}", b[j]));
            j += 1;
        } else {
            out.push(format!("- {}", a[i]));
            i += 1;
        }
    }
    out
}

// ── Reading the tailnet ─────────────────────────────────────────────────────

/// What the installation does for the setup: the AppState, or a stand-in
/// in tests.
pub(crate) trait Host {
    /// It can seal a client secret (PYO_SECRET_KEY is set).
    fn can_store(&self) -> bool;
    /// The saved OAuth client's ID, when it signs in.
    async fn working_client(&self) -> Option<String>;
    /// Saves a new client like one pasted by hand: `{ ok, checks }`.
    async fn save_client(&self, id: &str, secret: &str) -> Result<Value, String>;
}

impl Host for AppState {
    fn can_store(&self) -> bool {
        self.sealer.enabled()
    }

    async fn working_client(&self) -> Option<String> {
        let (cfg, c) = crate::tailscale::saved_client(self).await.ok()??;
        c.access_token().await.ok()?;
        Some(cfg.client_id)
    }

    async fn save_client(&self, id: &str, secret: &str) -> Result<Value, String> {
        crate::tailscale::save_client(self, id, Some(secret))
            .await
            .map_err(|e| e.message)
    }
}

/// Whether HTTPS certificates are on. The API reference names the setting
/// both `httpsEnabled` and `httpsCertificates`, so either counts.
fn https_on(settings: &Value) -> Option<bool> {
    ["httpsEnabled", "httpsCertificates"]
        .iter()
        .find_map(|k| match &settings[*k] {
            Value::Bool(b) => Some(*b),
            Value::String(s) => Some(matches!(s.as_str(), "on" | "enabled" | "true")),
            _ => None,
        })
}

/// The tailnet policy as Pyo read it, parsed once.
struct Policy {
    text: String,
    etag: Option<String>,
    needs: Needs,
    /// What [`reaches_pyo`] says of it.
    reaches: Option<bool>,
    /// The policy with what it needs, when it needs something.
    patched: Option<String>,
}

/// The tailnet as Pyo found it.
struct Survey {
    /// The policy is managed outside the console (GitOps, Terraform).
    gitops: bool,
    policy: Policy,
    magic_dns: bool,
    /// None when Tailscale doesn't say.
    https: Option<bool>,
    /// Pyo's saved client is on this tailnet and signs in.
    client: bool,
}

async fn read_policy(api: &Api) -> Result<Policy, String> {
    let (text, etag) = api
        .policy()
        .await
        .map_err(|e| say(&e, "read your tailnet policy"))?;
    let parsed = parse(&text)?;
    let needs = needs(&parsed);
    let patched = if needs.any() {
        Some(patch(&text, &needs)?)
    } else {
        None
    };
    Ok(Policy {
        reaches: reaches_pyo(&parsed),
        text,
        etag,
        needs,
        patched,
    })
}

async fn survey(api: &Api, host: &impl Host) -> Result<Survey, String> {
    // Nothing here depends on anything else, so it's all read at once.
    let settings = async {
        api.get("/tailnet/-/settings")
            .await
            .map_err(|e| say(&e, "read your tailnet's settings"))
    };
    let dns = async {
        api.get("/tailnet/-/dns/preferences")
            .await
            .map_err(|e| say(&e, "read your tailnet's DNS settings"))
    };
    let client = async {
        Ok::<_, String>(match host.working_client().await {
            None => false,
            // On this tailnet: Tailscale knows it by its ID. A client it can't
            // show this token still counts, since it signs in.
            Some(id) => match api.get(&format!("/tailnet/-/keys/{id}")).await {
                Ok(k) => k["invalid"].as_bool() != Some(true),
                Err(e) => e.status != 404,
            },
        })
    };
    let (settings, policy, dns, client) =
        tokio::try_join!(settings, read_policy(api), dns, client)?;
    Ok(Survey {
        gitops: settings["aclsExternallyManagedOn"].as_bool() == Some(true),
        policy,
        magic_dns: dns["magicDNS"].as_bool() == Some(true),
        https: https_on(&settings),
        client,
    })
}

const GITOPS: &str = "Your tailnet policy is managed outside the Tailscale console. Add these lines there, then check again.";

/// What the plan says, step by step.
mod words {
    pub const MAGIC_DNS: [&str; 3] = ["Turn on MagicDNS", "Turned on MagicDNS", "MagicDNS is on"];
    pub const HTTPS: [&str; 3] = [
        "Turn on HTTPS certificates",
        "Turned on HTTPS certificates",
        "HTTPS certificates are on",
    ];
    pub const TAG_OWNER: [&str; 3] = [
        "Add tag:pyo to your policy",
        "Added tag:pyo to your policy",
        "Your policy has tag:pyo",
    ];
    pub const GRANT: [&str; 3] = [
        "Let your tailnet reach tag:pyo on ports 80 and 443",
        "Let your tailnet reach tag:pyo on ports 80 and 443",
        "Your tailnet can reach tag:pyo",
    ];
    pub const APPROVER: [&str; 3] = [
        "Approve Pyo's Services on tag:pyo servers by policy",
        "Approved Pyo's Services on tag:pyo servers by policy",
        "Your policy approves Pyo's Services",
    ];
    pub const CLIENT: [&str; 3] = [
        "Create the Pyo OAuth client",
        "Created the Pyo OAuth client",
        "The Pyo OAuth client works",
    ];
}

/// What [`plan`] returns.
#[derive(Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Plan {
    /// What Pyo will change, in order. Empty when the tailnet is set up.
    pub changes: Vec<String>,
    /// What's set up already.
    pub done: Vec<String>,
    /// The policy lines added ("+ …") or changed ("- …").
    pub diff: Vec<String>,
    /// Why Pyo can't go on by itself, with `diff` as the lines to add.
    pub blocked: Option<String>,
}

fn plan_of(s: &Survey) -> Plan {
    let n = &s.policy.needs;
    let mut p = Plan::default();
    let mut step = |todo: bool, t: [&str; 3]| {
        if todo {
            p.changes.push(t[0].into());
        } else {
            p.done.push(t[2].into());
        }
    };
    step(!s.magic_dns, words::MAGIC_DNS);
    step(s.https != Some(true), words::HTTPS);
    step(n.tag_owner, words::TAG_OWNER);
    if n.grant || s.policy.reaches == Some(true) {
        step(n.grant, words::GRANT);
    }
    // A policy in Git that has what's required is left as it is.
    let writes_policy = !s.gitops || n.required();
    if writes_policy || !n.approver {
        step(n.approver, words::APPROVER);
    }
    step(!s.client, words::CLIENT);
    if let Some(new) = &s.policy.patched
        && writes_policy
    {
        p.diff = diff(&s.policy.text, new);
    }
    if s.gitops && n.required() {
        p.blocked = Some(GITOPS.into());
    }
    p
}

/// Reads the tailnet and says what setting it up would change. Changes
/// nothing.
pub(crate) async fn plan_with(api: &Api, host: &impl Host) -> Result<Plan, String> {
    if !host.can_store() {
        return Err(NO_KEY.into());
    }
    let s = survey(api, host).await?;
    let p = plan_of(&s);
    tracing::info!(
        changes = p.changes.len(),
        blocked = p.blocked.is_some(),
        "tailscale setup planned"
    );
    Ok(p)
}

const NO_KEY: &str =
    "This installation has no PYO_SECRET_KEY, so it can't store Pyo's OAuth client.";

// ── Making the changes ──────────────────────────────────────────────────────

/// What [`apply`] did.
#[derive(Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Outcome {
    /// Everything went through.
    pub ok: bool,
    /// What changed, in order.
    pub changed: Vec<String>,
    /// What the admin still has to do by hand.
    pub todo: Vec<String>,
    /// What stopped it, after `changed`.
    pub error: Option<String>,
    /// The saved client's checks, as Test shows them.
    pub checks: Vec<Value>,
    /// The pasted token was deleted.
    pub token_deleted: bool,
}

enum Https {
    On,
    Off,
    Unknown,
}

/// Turns HTTPS certificates on, trying the setting's name in the schema
/// first and then the one in its description, and reading it back.
async fn turn_on_https(api: &Api) -> Result<Https, TsError> {
    let read = async || api.get("/tailnet/-/settings").await.map(|v| https_on(&v));
    for field in ["httpsEnabled", "httpsCertificates"] {
        match api
            .send(
                reqwest::Method::PATCH,
                "/tailnet/-/settings",
                Body::Json(json!({ field: true })),
                &[],
            )
            .await
        {
            Ok(r) if https_on(&json_of(&r)) == Some(true) => return Ok(Https::On),
            Ok(_) => {
                if read().await? == Some(true) {
                    return Ok(Https::On);
                }
            }
            // Not a setting by that name.
            Err(e) if e.status == 400 => {}
            Err(e) => return Err(e),
        }
    }
    Ok(match read().await? {
        Some(true) => Https::On,
        Some(false) => Https::Off,
        None => Https::Unknown,
    })
}

/// Validates the new policy, without saving it.
async fn validate(api: &Api, text: &str) -> Result<(), String> {
    let r = api
        .send(
            reqwest::Method::POST,
            "/tailnet/-/acl/validate",
            Body::HuJson(text.to_owned()),
            &[],
        )
        .await
        .map_err(|e| say(&e, "check the new tailnet policy"))?;
    let v = json_of(&r);
    let errors = v["data"]
        .as_array()
        .into_iter()
        .flatten()
        .any(|d| d["errors"].as_array().is_some_and(|e| !e.is_empty()));
    match v["message"].as_str().filter(|m| !m.is_empty()) {
        // Warnings don't stop it.
        Some(m) if errors || !m.starts_with("warning") => Err(format!(
            "Tailscale wouldn't take the new policy: {}.",
            api.scrub(m.trim_end_matches('.'))
        )),
        _ => Ok(()),
    }
}

/// The policy steps done, as the outcome lists them.
fn policy_changes(n: &Needs) -> Vec<String> {
    [
        (n.tag_owner, words::TAG_OWNER),
        (n.grant, words::GRANT),
        (n.approver, words::APPROVER),
    ]
    .into_iter()
    .filter(|(on, _)| *on)
    .map(|(_, t)| t[1].to_owned())
    .collect()
}

/// Writes the policy with what it needs. If someone changed it meanwhile
/// (412), reads it again and plans once more.
async fn write_policy(api: &Api, s: &Survey) -> Result<Vec<String>, String> {
    let (mut etag, mut needs, mut patched) = (
        s.policy.etag.clone(),
        s.policy.needs,
        s.policy.patched.clone(),
    );
    for attempt in 0..2 {
        let Some(text) = patched.as_deref() else {
            return Ok(vec![]);
        };
        // The first one was validated before anything changed.
        if attempt > 0 {
            validate(api, text).await?;
        }
        let mut headers = vec![("accept", "application/hujson")];
        if let Some(e) = etag.as_deref() {
            headers.push(("if-match", e));
        }
        match api
            .send(
                reqwest::Method::POST,
                "/tailnet/-/acl",
                Body::HuJson(text.to_owned()),
                &headers,
            )
            .await
        {
            Ok(_) => {
                tracing::info!("tailnet policy updated");
                return Ok(policy_changes(&needs));
            }
            Err(e) if e.status == 412 && attempt == 0 => {
                tracing::info!("tailnet policy changed meanwhile, planning again");
                let p = read_policy(api).await?;
                (etag, needs, patched) = (p.etag, p.needs, p.patched);
            }
            Err(e) if e.status == 412 => {
                return Err(
                    "Your tailnet policy changed while Pyo was editing it. Check again.".into(),
                );
            }
            Err(e) => return Err(say(&e, "save your tailnet policy")),
        }
    }
    unreachable!()
}

/// Makes Pyo's own OAuth client and saves it: (checks).
async fn make_client(api: &Api, host: &impl Host) -> Result<Vec<Value>, String> {
    let body = json!({
        "keyType": "client",
        "description": crate::tailscale::key_description("Pyo"),
        "scopes": SCOPES,
        "tags": [TAG],
    });
    let r = api
        .send(reqwest::Method::POST, "/tailnet/-/keys", Body::Json(body), &[])
        .await
        .map_err(|e| match e.status {
            401 => BAD_TOKEN.to_owned(),
            _ => {
                let m = e.message.trim().trim_end_matches('.');
                let why = if m.is_empty() { String::new() } else { format!(" It said: {m}.") };
                format!("Tailscale didn't let Pyo create its OAuth client.{why} Set up with an OAuth client instead.")
            }
        })?;
    let v = json_of(&r);
    let secret = v["key"].as_str().unwrap_or("");
    // The client ID is the key's ID, which its secret carries too.
    let id = v["id"].as_str().map(str::to_owned).or_else(|| {
        secret
            .strip_prefix("tskey-client-")
            .and_then(|s| s.split('-').next())
            .map(str::to_owned)
    });
    let Some(id) = id.filter(|_| secret.starts_with("tskey-client-")) else {
        return Err(
            "Tailscale made no OAuth client for Pyo. Set up with an OAuth client instead.".into(),
        );
    };
    tracing::info!(client = %id, "tailscale client made");
    match host.save_client(&id, secret).await {
        Ok(out) => Ok(out["checks"].as_array().cloned().unwrap_or_default()),
        Err(e) => {
            // Not left behind unused.
            let _ = api
                .send(
                    reqwest::Method::DELETE,
                    &format!("/tailnet/-/keys/{id}"),
                    Body::None,
                    &[],
                )
                .await;
            Err(format!("Pyo couldn't save its OAuth client. {e}"))
        }
    }
}

/// Sets the tailnet up: what [`plan_with`] says, in that order, and the
/// pasted token deleted last. Fails with nothing changed, or says what
/// changed before it stopped.
pub(crate) async fn apply_with(api: &Api, host: &impl Host) -> Result<Outcome, String> {
    if !host.can_store() {
        return Err(NO_KEY.into());
    }
    let s = survey(api, host).await?;
    if s.gitops && s.policy.needs.required() {
        return Err(GITOPS.into());
    }
    let writes_policy = !s.gitops && s.policy.patched.is_some();
    // Before anything changes.
    if writes_policy && let Some(p) = &s.policy.patched {
        validate(api, p).await?;
    }
    let mut out = Outcome::default();
    let stop = |mut out: Outcome, e: String| {
        if out.changed.is_empty() {
            Err(e)
        } else {
            out.error = Some(e);
            Ok(out)
        }
    };
    if !s.magic_dns {
        if let Err(e) = api
            .send(
                reqwest::Method::POST,
                "/tailnet/-/dns/preferences",
                Body::Json(json!({ "magicDNS": true })),
                &[],
            )
            .await
        {
            let none = api
                .get("/tailnet/-/dns/nameservers")
                .await
                .is_ok_and(|v| strs(&v["dns"]).next().is_none());
            return stop(
                out,
                if none && e.status != 401 && e.status != 403 {
                    "MagicDNS needs a DNS nameserver first. Add one under DNS in the Tailscale console, then try again.".into()
                } else {
                    say(&e, "turn on MagicDNS")
                },
            );
        }
        out.changed.push(words::MAGIC_DNS[1].into());
    }
    if s.https != Some(true) {
        match turn_on_https(api).await {
            Ok(Https::On) => out.changed.push(words::HTTPS[1].into()),
            Ok(Https::Off) => out
                .todo
                .push("Turn on HTTPS Certificates under DNS in the Tailscale console.".into()),
            Ok(Https::Unknown) => out.todo.push(
                "Check that HTTPS Certificates is on under DNS in the Tailscale console.".into(),
            ),
            Err(e) => return stop(out, say(&e, "turn on HTTPS certificates")),
        }
    }
    if writes_policy {
        match write_policy(api, &s).await {
            Ok(done) => out.changed.extend(done),
            Err(e) => return stop(out, e),
        }
    }
    if !s.client {
        match make_client(api, host).await {
            Ok(checks) => {
                out.changed.push(words::CLIENT[1].into());
                out.checks = checks;
            }
            Err(e) => return stop(out, e),
        }
    }
    // Last: nothing needs the token after this.
    out.token_deleted = match api.token.key_id() {
        Some(id) => api
            .send(
                reqwest::Method::DELETE,
                &format!("/tailnet/-/keys/{id}"),
                Body::None,
                &[],
            )
            .await
            .is_ok(),
        None => false,
    };
    if !out.token_deleted {
        out.todo.push(DELETE_TOKEN.into());
    }
    out.ok = true;
    tracing::info!(
        changed = out.changed.len(),
        token_deleted = out.token_deleted,
        "tailscale set up"
    );
    Ok(out)
}

/// planTailscaleSetup.
pub async fn plan(s: &AppState, token: Token) -> ApiResult<Value> {
    check_shape(&token).map_err(ApiError::bad_request)?;
    let p = plan_with(&Api::new(token), s)
        .await
        .map_err(ApiError::bad_request)?;
    Ok(json!({ "ok": true, "plan": p }))
}

/// setUpTailscale.
pub async fn apply(s: &AppState, token: Token) -> ApiResult<Outcome> {
    check_shape(&token).map_err(ApiError::bad_request)?;
    apply_with(&Api::new(token), s)
        .await
        .map_err(ApiError::bad_request)
}

#[cfg(test)]
mod tests;
