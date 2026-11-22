//! Compose sync: a Compose file applied again to the project made from it
//! (or linked to it) changes only what the file says differently. The plan
//! lists each service as added, changed (and what), unchanged, or no longer
//! in the file. `syncCompose` works the plan out again in its own
//! transaction and applies it through the same operations the dashboard
//! uses, with their checks.
//!
//! - Only what the file sets is compared (`PlanService::managed`). A setting
//!   changed in Pyo that the file doesn't mention stays as it is.
//! - Variables, ports, domains and volumes the file lists are added or
//!   changed. Ones it doesn't list stay.
//! - A secret keeps its value in Pyo, whatever the file says.
//! - What can't change in place (a volume that would move, another kind of
//!   source) is a manual step: listed, and left as it is.
//! - A service no longer in the file is only listed. With `prune` it's
//!   deleted (its volumes are kept, detached), and only when it was made
//!   from this file.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::{Postgres, Transaction};

use crate::compose::{self, ComposePlan, PlanService, PlanVolume, PlanWarning};
use crate::error::{ApiError, ApiResult};

/// What a sync would change.
#[derive(Debug, Clone, Default, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct SyncPlan {
    /// The project it syncs. None when there's none for this file yet: then
    /// `importCompose` makes it from `file`.
    pub project: Option<SyncProject>,
    /// The file's services in deploy order, then the project's services that
    /// aren't in the file.
    pub services: Vec<SyncService>,
    /// What the file has that Pyo doesn't carry over.
    pub warnings: Vec<PlanWarning>,
    /// The file as Pyo reads it: pass it to `syncCompose` (or `importCompose`).
    pub file: ComposePlan,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct SyncProject {
    pub id: String,
    pub name: String,
    pub namespace: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct SyncService {
    /// Its name in Pyo.
    pub name: String,
    /// Its name in the file. None when it isn't in the file.
    #[serde(default)]
    pub compose_name: Option<String>,
    /// None for a service the sync would add.
    #[serde(default)]
    pub service_id: Option<String>,
    /// "added", "changed", "unchanged" or "notInFile".
    pub status: String,
    pub changes: Vec<SyncChange>,
    /// What stays as it is in Pyo, and why.
    pub notes: Vec<SyncNote>,
    /// What can't change in place. It's left as it is: change it by hand.
    pub manual: Vec<SyncNote>,
    /// The sync deploys it.
    pub deploy: bool,
    /// For a service not in the file: it was made from this file, so
    /// `prune` deletes it.
    #[serde(default)]
    pub from_file: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, utoipa::ToSchema, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SyncChange {
    /// What changes, e.g. "image", "replicas", "env LOG_LEVEL", "domain app.example.com".
    pub field: String,
    /// As it is now. Variables' values are never shown.
    #[serde(default)]
    pub from: Option<String>,
    #[serde(default)]
    pub to: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, utoipa::ToSchema, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SyncNote {
    pub field: String,
    pub message: String,
}

/// A service of the project, as the sync sees it.
#[derive(Debug, Clone, Default)]
pub struct Existing {
    pub id: String,
    pub name: String,
    /// As stored: secrets sealed.
    pub spec: Value,
    pub behind_cdn: Option<String>,
    /// Its volumes' data: name, mountPath, sizeGi.
    pub volumes: Vec<Value>,
}

impl Existing {
    /// The Compose service it was made from.
    fn compose_name(&self) -> Option<&str> {
        self.spec["compose"]["service"].as_str()
    }
}

/// One service brought in line with the file.
#[derive(Debug, Default)]
pub struct Merged {
    /// Its new spec (secrets still sealed).
    pub spec: Value,
    pub changes: Vec<SyncChange>,
    pub notes: Vec<SyncNote>,
    pub manual: Vec<SyncNote>,
    pub deploy: bool,
    /// A new CDN setting.
    pub behind_cdn: Option<Option<String>>,
    /// Volumes to add.
    pub volumes: Vec<PlanVolume>,
    /// Nothing changes but what it remembers of the file.
    pub record_only: bool,
}

impl Merged {
    fn change(&mut self, field: impl Into<String>, from: Option<String>, to: Option<String>) {
        self.changes.push(SyncChange {
            field: field.into(),
            from,
            to,
        });
    }
}

fn note(field: impl Into<String>, message: impl Into<String>) -> SyncNote {
    SyncNote {
        field: field.into(),
        message: message.into(),
    }
}

fn image_words(image: &str, reference: &str) -> String {
    if reference.starts_with("sha256:") {
        format!("{image}@{reference}")
    } else {
        format!("{image}:{reference}")
    }
}

fn source_words(kind: &str) -> String {
    match kind {
        "image" => "deployed from an image".into(),
        "git" => "built from Git".into(),
        "upload" => "built from a folder".into(),
        "template" => "deployed from a template".into(),
        k => format!("from {k}"),
    }
}

fn health_words(h: &Value) -> String {
    let port = h["port"].as_i64().unwrap_or(0);
    match h["type"].as_str().unwrap_or("none") {
        "http" => format!("{} on {port}", h["path"].as_str().unwrap_or("/")),
        "tcp" => format!("tcp on {port}"),
        "exec" => "a command".into(),
        _ => "none".into(),
    }
}

fn placement_words(p: &crate::placement::Placement) -> String {
    if p.any() {
        return "any server".into();
    }
    let mut parts = vec![];
    if !p.servers.is_empty() {
        parts.push(p.servers.join(", "));
    }
    if !p.tags.is_empty() {
        parts.push(format!(
            "{} {}",
            if p.tags.len() == 1 { "tag" } else { "tags" },
            p.tags.join(", ")
        ));
    }
    parts.join(" and ")
}

fn port_words(exposure: &str, public_port: Option<u64>) -> String {
    match (exposure, public_port) {
        ("http", _) => "public".into(),
        ("loadbalancer", Some(p)) => format!("public on {p}"),
        ("loadbalancer" | "nodeport", None) => "public".into(),
        _ => "inside the project".into(),
    }
}

fn sorted(mut p: crate::placement::Placement) -> crate::placement::Placement {
    p.servers.sort();
    p.tags.sort();
    p
}

/// A name for a new port that none of the service's ports has.
fn free_port_name(ports: &[Value], want: &str, protocol: &str, port: u16) -> String {
    let taken = |n: &str| ports.iter().any(|p| p["name"] == json!(n));
    if !taken(want) {
        return want.to_owned();
    }
    let base = format!("{}-{port}", protocol.to_lowercase());
    let mut name = base.clone();
    let mut n = 2;
    while taken(&name) {
        name = format!("{base}-{n}");
        n += 1;
    }
    name
}

/// A volume's name without its service's in front, as `addVolume` takes it.
pub fn volume_label(service: &str, volume: &str) -> String {
    volume
        .strip_prefix(&format!("{service}-"))
        .filter(|l| !l.is_empty())
        .unwrap_or(volume)
        .to_owned()
}

/// `old` brought in line with what the file sets for it (`want`).
pub fn merge(
    old: &Existing,
    want: &PlanService,
    file: &str,
    now: i64,
    new_id: &dyn Fn(&str) -> String,
) -> Merged {
    let mut m = Merged::default();
    let mut spec = old.spec.clone();
    if !spec.is_object() {
        spec = json!({});
    }

    // Its source.
    let kind = spec["source"]["type"].as_str().unwrap_or("").to_owned();
    match (kind.as_str(), want.source.kind.as_str()) {
        ("image", "image") => {
            let src = &mut spec["source"];
            let from = image_words(
                src["image"].as_str().unwrap_or(""),
                src["reference"].as_str().unwrap_or(""),
            );
            let to = image_words(&want.source.image, &want.source.reference);
            if from != to {
                src["image"] = json!(want.source.image);
                src["reference"] = json!(want.source.reference);
                m.change("image", Some(from), Some(to));
                m.deploy = true;
            }
        }
        ("git" | "upload", "git") => {
            let mut fields = vec![
                ("buildContext", "build context", &want.source.build_context),
                ("dockerfilePath", "Dockerfile", &want.source.dockerfile_path),
            ];
            if kind == "git" && !want.source.repo_url.is_empty() {
                fields.push(("repoUrl", "repository", &want.source.repo_url));
                fields.push(("branch", "branch", &want.source.branch));
            }
            for (key, label, to) in fields {
                let src = &mut spec["source"];
                let from = src[key].as_str().unwrap_or("").to_owned();
                if &from != to {
                    src[key] = json!(to);
                    m.change(label, Some(from), Some(to.clone()));
                    m.deploy = true;
                }
            }
            for a in &want.source.build_args {
                let src = &mut spec["source"];
                if !src["buildArgs"].is_array() {
                    src["buildArgs"] = json!([]);
                }
                let Some(args) = src["buildArgs"].as_array_mut() else {
                    continue;
                };
                match args.iter_mut().find(|x| x["key"] == json!(a.key)) {
                    Some(x) if x["value"] == json!(a.value) => {}
                    Some(x) => {
                        x["value"] = json!(a.value);
                        m.change(format!("build argument {}", a.key), None, None);
                        m.deploy = true;
                    }
                    None => {
                        args.push(json!({ "key": a.key, "value": a.value }));
                        m.change(
                            format!("build argument {}", a.key),
                            None,
                            Some("added".into()),
                        );
                        m.deploy = true;
                    }
                }
            }
        }
        (from, to) => m.manual.push(note(
            "source",
            format!(
                "It's {} in Pyo and {} in the file. Change its source in the dashboard.",
                source_words(from),
                source_words(to)
            ),
        )),
    }

    // Variables: the file's are added or changed, a secret's value stays.
    if !spec["env"].is_array() {
        spec["env"] = json!([]);
    }
    for e in &want.env {
        let field = format!("env {}", e.key);
        let Some(env) = spec["env"].as_array_mut() else {
            break;
        };
        match env.iter_mut().find(|o| o["key"] == json!(e.key)) {
            Some(o) if o["secret"] == json!(true) => {
                if !e.value.is_empty() {
                    m.notes
                        .push(note(field, "It's a secret in Pyo, so its value stays."));
                }
            }
            Some(o) if o["value"].as_str() == Some(e.value.as_str()) => {}
            Some(o) => {
                o["value"] = json!(e.value);
                o["updatedAt"] = json!(now);
                m.change(field, None, None);
                m.deploy = true;
            }
            None => {
                env.push(json!({ "id": new_id("env"), "key": e.key, "value": e.value, "secret": e.secret, "updatedAt": now }));
                m.change(field, None, Some("added".into()));
                m.deploy = true;
            }
        }
    }
    for k in &want.secrets {
        let field = format!("secret {k}");
        let Some(env) = spec["env"].as_array_mut() else {
            break;
        };
        match env.iter().find(|o| o["key"] == json!(k)) {
            Some(o) if o["secret"] == json!(true) => {}
            Some(_) => m.manual.push(note(
                field,
                "It's a plain variable in Pyo. Make it a secret in the dashboard.",
            )),
            None => {
                env.push(json!({ "id": new_id("env"), "key": k, "value": "", "secret": true, "updatedAt": now }));
                m.change(field.clone(), None, Some("added".into()));
                m.notes.push(note(field, "Set its value in Pyo, then deploy."));
            }
        }
    }

    // Ports, matched by number (and UDP or not).
    if want.manages("ports") {
        if !spec["ports"].is_array() {
            spec["ports"] = json!([]);
        }
        for p in &want.ports {
            let field = format!("port {}", p.container_port);
            let udp = p.protocol == "UDP";
            let want_exposure = compose::exposure(p);
            let Some(ports) = spec["ports"].as_array_mut() else {
                break;
            };
            let found = ports.iter().position(|o| {
                o["containerPort"].as_u64() == Some(u64::from(p.container_port))
                    && (o["protocol"] == json!("UDP")) == udp
            });
            let Some(i) = found else {
                let name = free_port_name(ports, &p.name, &p.protocol, p.container_port);
                let mut v = compose::port_json(p, new_id("port"));
                v["name"] = json!(name);
                ports.push(v);
                m.change(
                    field,
                    None,
                    Some(port_words(want_exposure, p.public_port.map(u64::from))),
                );
                m.deploy = true;
                continue;
            };
            let o = &mut ports[i];
            if !udp && o["protocol"] != json!(p.protocol) {
                let from = o["protocol"].as_str().unwrap_or("").to_owned();
                o["protocol"] = json!(p.protocol);
                m.change(field.clone(), Some(from), Some(p.protocol.clone()));
                m.deploy = true;
            }
            let exposure = o["exposure"].as_str().unwrap_or("internal").to_owned();
            let from = port_words(&exposure, o["publicPort"].as_u64());
            let to = port_words(want_exposure, p.public_port.map(u64::from));
            match (exposure.as_str(), want_exposure) {
                ("loadbalancer", "loadbalancer") => {
                    if let Some(pp) = p.public_port
                        && o["publicPort"].as_u64() != Some(u64::from(pp))
                    {
                        o["publicPort"] = json!(pp);
                        m.change(field, Some(from), Some(to));
                        m.deploy = true;
                    }
                }
                (a, b) if a == b => {}
                ("http" | "loadbalancer" | "nodeport", "internal") => m
                    .notes
                    .push(note(field, "It's public in Pyo, so it stays public.")),
                (_, "http") => {
                    // A website's port: its domains go live in place.
                    o["exposure"] = json!("http");
                    if let Some(x) = o.as_object_mut() {
                        x.remove("publicPort");
                        x.remove("proxy");
                    }
                    m.change(field, Some(from), Some(to));
                    if exposure != "internal" {
                        m.deploy = true;
                    }
                }
                _ => {
                    o["exposure"] = json!(want_exposure);
                    o["proxy"] = json!(true);
                    if let Some(pp) = p.public_port {
                        o["publicPort"] = json!(pp);
                    }
                    m.change(field, Some(from), Some(to));
                    m.deploy = true;
                }
            }
        }
    }

    // Domains: the file's are added on its web port.
    if want.manages("domains") {
        if !spec["domains"].is_array() {
            spec["domains"] = json!([]);
        }
        let ports = spec["ports"].as_array().cloned().unwrap_or_default();
        // The port the file serves them on, else its first website port.
        let wanted_port = want
            .ports
            .iter()
            .find(|p| p.public && p.protocol == "HTTP")
            .map(|p| p.container_port);
        let web = ports
            .iter()
            .find(|o| {
                o["exposure"] == json!("http")
                    && wanted_port
                        .is_some_and(|w| o["containerPort"].as_u64() == Some(u64::from(w)))
            })
            .or_else(|| ports.iter().find(|o| o["exposure"] == json!("http")))
            .and_then(|o| o["name"].as_str())
            .map(str::to_owned);
        for d in &want.domains {
            let Some(domains) = spec["domains"].as_array_mut() else {
                break;
            };
            if domains.iter().any(|o| {
                o["hostname"]
                    .as_str()
                    .is_some_and(|h| h.eq_ignore_ascii_case(d))
            }) {
                continue;
            }
            match &web {
                Some(port) => {
                    domains.push(compose::domain_json(d, port, new_id("dom"), now));
                    m.change(format!("domain {d}"), None, Some("added".into()));
                }
                None => m.manual.push(note(
                    format!("domain {d}"),
                    "It has no public web port in Pyo to serve it on. Make one public in the dashboard.",
                )),
            }
        }
    }

    // Health check.
    if want.manages("health") {
        let old_h = spec["healthCheck"].clone();
        let mut h = want.health_check.clone();
        if !want.manages("healthTiming") {
            for k in ["initialDelaySeconds", "periodSeconds"] {
                if let Some(v) = old_h.get(k) {
                    h[k] = v.clone();
                }
            }
        }
        let same = old_h.get("auto").is_none_or(Value::is_null)
            && [
                "type",
                "path",
                "port",
                "command",
                "initialDelaySeconds",
                "periodSeconds",
            ]
            .iter()
            .all(|k| old_h[*k] == h[*k]);
        if !same {
            m.change(
                "health check",
                Some(health_words(&old_h)),
                Some(health_words(&h)),
            );
            spec["healthCheck"] = h;
            m.deploy = true;
        }
    }

    // Replicas, unless autoscaling sets them.
    if want.manages("replicas") {
        if spec.get("autoscale").is_some_and(|a| !a.is_null()) {
            if spec["replicas"].as_i64() != Some(i64::from(want.replicas)) {
                m.notes.push(note(
                    "replicas",
                    "Autoscaling sets them, so they stay as they are.",
                ));
            }
        } else if spec["replicas"].as_i64() != Some(i64::from(want.replicas)) {
            m.change(
                "replicas",
                spec["replicas"].as_i64().map(|n| n.to_string()),
                Some(want.replicas.to_string()),
            );
            spec["replicas"] = json!(want.replicas);
            m.deploy = true;
        }
    }

    // Resources the file sets.
    let r = &want.resources;
    let mut set_resource = false;
    for (field, key, value, label) in [
        ("resources.cpuLimit", "cpuLimit", r.cpu_limit, "cpu limit"),
        (
            "resources.memoryLimit",
            "memoryLimit",
            r.memory_limit,
            "memory limit",
        ),
        ("resources.cpuRequest", "cpuRequest", r.cpu_request, "cpu request"),
        (
            "resources.memoryRequest",
            "memoryRequest",
            r.memory_request,
            "memory request",
        ),
        ("resources.gpu", "gpu", r.gpu, "GPUs"),
    ] {
        if !want.manages(field) {
            continue;
        }
        if !spec["resources"].is_object() {
            spec["resources"] = json!({});
        }
        let now_v = spec["resources"][key].as_i64();
        if now_v != Some(value) {
            let unit = |n: i64| match key {
                "cpuLimit" | "cpuRequest" => format!("{n}m"),
                "gpu" => n.to_string(),
                _ => format!("{n} MiB"),
            };
            m.change(label, now_v.map(unit), Some(unit(value)));
            spec["resources"][key] = json!(value);
            set_resource = true;
        }
    }
    if set_resource {
        // A request is never above its limit.
        for (req, lim) in [("cpuRequest", "cpuLimit"), ("memoryRequest", "memoryLimit")] {
            let res = &mut spec["resources"];
            if let (Some(a), Some(b)) = (res[req].as_i64(), res[lim].as_i64())
                && a > b
            {
                res[req] = json!(b);
            }
        }
        m.deploy = true;
    }

    // How it starts.
    let c = &want.container;
    let mut container = spec
        .get("container")
        .cloned()
        .filter(Value::is_object)
        .unwrap_or_else(|| json!({}));
    let mut set_container = false;
    for (field, key, value, label) in [
        (
            "container.entrypoint",
            "entrypoint",
            json!(c.entrypoint),
            "entrypoint",
        ),
        ("container.command", "command", json!(c.command), "command"),
        (
            "container.workingDir",
            "workingDir",
            json!(c.working_dir),
            "working directory",
        ),
    ] {
        if !want.manages(field) {
            continue;
        }
        let now_v = match container.get(key) {
            None | Some(Value::Null) if value.is_array() => json!([]),
            None | Some(Value::Null) => json!(""),
            Some(v) => v.clone(),
        };
        if now_v != value {
            m.change(label, None, None);
            container[key] = value;
            set_container = true;
        }
    }
    if want.manages("container.user") {
        for (key, value) in [("runAsUser", c.run_as_user), ("runAsGroup", c.run_as_group)] {
            if container.get(key).and_then(Value::as_i64) != value {
                container[key] = json!(value);
                set_container = true;
                if !m.changes.iter().any(|x| x.field == "user") {
                    m.change("user", None, None);
                }
            }
        }
    }
    if set_container {
        // Empty ones keep the image's own.
        if let Some(o) = container.as_object_mut() {
            o.retain(|_, v| {
                !(v.is_null()
                    || v.as_array().is_some_and(Vec::is_empty)
                    || v.as_str().is_some_and(str::is_empty))
            });
        }
        if container.as_object().is_some_and(|o| o.is_empty()) {
            if let Some(o) = spec.as_object_mut() {
                o.remove("container");
            }
        } else {
            spec["container"] = container;
        }
        m.deploy = true;
    }

    // Where it runs: changed in place.
    if want.manages("placement") {
        let wanted = json!({ "placement": want.placement.clone().unwrap_or(Value::Null) });
        let (from, to) = (
            sorted(crate::placement::Placement::of(&spec)),
            sorted(crate::placement::Placement::of(&wanted)),
        );
        if from != to {
            m.change(
                "placement",
                Some(placement_words(&from)),
                Some(placement_words(&to)),
            );
            if let Some(o) = spec.as_object_mut() {
                o.remove("servers");
                o.remove("serverTags");
                o.insert("placement".into(), to.to_value());
            }
        }
    }

    // Volumes, matched by where they're mounted.
    if want.manages("volumes") {
        for v in &want.volumes {
            if old
                .volumes
                .iter()
                .any(|o| o["mountPath"] == json!(v.mount_path))
            {
                continue;
            }
            let label = volume_label(&old.name, &v.name);
            let full = format!("{}-{label}", old.name);
            let named = old.volumes.iter().find(|o| {
                o["name"] == json!(v.name) || o["name"] == json!(full)
            });
            match named {
                Some(o) => m.manual.push(note(
                    format!("volume {}", o["name"].as_str().unwrap_or(&v.name)),
                    format!(
                        "It's mounted at {} in Pyo and {} in the file. Detach it and attach it at {} to move it.",
                        o["mountPath"].as_str().unwrap_or(""),
                        v.mount_path,
                        v.mount_path
                    ),
                )),
                None => {
                    m.change(
                        format!("volume {full}"),
                        None,
                        Some(format!("added at {}", v.mount_path)),
                    );
                    m.volumes.push(v.clone());
                    m.deploy = true;
                }
            }
        }
    }

    // Behind a CDN: changed in place.
    if want.manages("behindCdn") && old.behind_cdn != want.behind_cdn {
        m.change(
            "behind CDN",
            Some(old.behind_cdn.clone().unwrap_or_else(|| "none".into())),
            Some(want.behind_cdn.clone().unwrap_or_else(|| "none".into())),
        );
        m.behind_cdn = Some(want.behind_cdn.clone());
    }

    // What it remembers of the file.
    let record = compose::record(file, &want.compose_name);
    if spec.get("compose") != Some(&record) {
        spec["compose"] = record;
        m.record_only = m.changes.is_empty();
    }
    m.spec = spec;
    m
}

/// The file's services and the project's, paired: a service made from a
/// Compose service matches it by that name, any other by its own name.
fn pair<'a>(
    file: &'a ComposePlan,
    existing: &'a [Existing],
) -> Vec<(Option<&'a PlanService>, Option<&'a Existing>)> {
    let mut used = vec![false; existing.len()];
    let mut out = vec![];
    for want in &file.services {
        let by_record = existing
            .iter()
            .enumerate()
            .position(|(i, e)| !used[i] && e.compose_name() == Some(want.compose_name.as_str()));
        let by_name = || {
            existing.iter().enumerate().position(|(i, e)| {
                !used[i]
                    && e.name == want.name
                    && e.compose_name().is_none_or(|c| {
                        !file.services.iter().any(|s| s.compose_name == c)
                    })
            })
        };
        match by_record.or_else(by_name) {
            Some(i) => {
                used[i] = true;
                out.push((Some(want), Some(&existing[i])));
            }
            None => out.push((Some(want), None)),
        }
    }
    for (i, e) in existing.iter().enumerate() {
        if !used[i] {
            out.push((None, Some(e)));
        }
    }
    out
}

/// The file names a service's record and the file are compared by.
fn same_file(a: &str, b: &str) -> bool {
    let base = |s: &str| s.trim().rsplit('/').next().unwrap_or("").to_owned();
    let (a, b) = (
        if a.trim().is_empty() { "compose.yaml" } else { a },
        if b.trim().is_empty() { "compose.yaml" } else { b },
    );
    base(a) == base(b)
}

/// What syncing `file` into a project with `existing` services would do.
pub fn plan_of(project: Option<SyncProject>, file: ComposePlan, existing: &[Existing]) -> SyncPlan {
    let now = pyo_proto::now_ms();
    let new_id = |p: &str| crate::ids::new_id(p);
    let mut services = vec![];
    for (want, old) in pair(&file, existing) {
        match (want, old) {
            (Some(w), None) => services.push(SyncService {
                name: w.name.clone(),
                compose_name: Some(w.compose_name.clone()),
                status: "added".into(),
                notes: w
                    .secrets
                    .iter()
                    .filter(|k| !w.env.iter().any(|e| &e.key == *k))
                    .map(|k| note(format!("secret {k}"), "Set its value in Pyo, then deploy."))
                    .collect(),
                deploy: true,
                ..Default::default()
            }),
            (Some(w), Some(o)) => {
                let m = merge(o, w, &file.project.file, now, &new_id);
                services.push(SyncService {
                    name: o.name.clone(),
                    compose_name: Some(w.compose_name.clone()),
                    service_id: Some(o.id.clone()),
                    status: if m.changes.is_empty() {
                        "unchanged"
                    } else {
                        "changed"
                    }
                    .into(),
                    deploy: m.deploy,
                    changes: m.changes,
                    notes: m.notes,
                    manual: m.manual,
                    from_file: true,
                });
            }
            (None, Some(o)) => {
                let made_from = o.spec["compose"]["file"].as_str();
                let from_file = made_from.is_some_and(|f| same_file(f, &file.project.file));
                let notes = match made_from {
                    Some(f) if !from_file => {
                        vec![note("source", format!("It was made from {f}."))]
                    }
                    None => vec![note("source", "It wasn't made from a Compose file.")],
                    _ => vec![],
                };
                services.push(SyncService {
                    name: o.name.clone(),
                    service_id: Some(o.id.clone()),
                    status: "notInFile".into(),
                    notes,
                    from_file,
                    ..Default::default()
                });
            }
            (None, None) => {}
        }
    }
    SyncPlan {
        project,
        services,
        warnings: file.warnings.clone(),
        file,
    }
}

// ── Reading the project ──────────────────────────────────────────────────────

/// The project's services a sync compares: not previews, not being deleted.
pub async fn existing(
    tx: &mut Transaction<'_, Postgres>,
    project_id: &str,
) -> ApiResult<Vec<Existing>> {
    let rows: Vec<(String, String, Value, Option<String>)> = sqlx::query_as(
        "SELECT id, name, spec, behind_cdn FROM pyo.services
          WHERE project_id = $1 AND NOT deleting AND preview_of IS NULL
          ORDER BY created_at, id",
    )
    .bind(project_id)
    .fetch_all(&mut **tx)
    .await?;
    let volumes: Vec<(String, Value)> = sqlx::query_as(
        "SELECT service_id, data FROM pyo.volumes
          WHERE project_id = $1 AND service_id IS NOT NULL AND NOT deleting",
    )
    .bind(project_id)
    .fetch_all(&mut **tx)
    .await?;
    let mut out = vec![];
    for (id, name, mut spec, behind_cdn) in rows {
        // Variables are compared in the clear. Secrets never are: they're
        // left blank, so a sync never opens them, and a save keeps them.
        blank_secrets(&mut spec);
        crate::vault::get().open_env(&mut spec).await?;
        out.push(Existing {
            volumes: volumes
                .iter()
                .filter(|(s, _)| *s == id)
                .map(|(_, d)| d.clone())
                .collect(),
            id,
            name,
            spec,
            behind_cdn,
        });
    }
    Ok(out)
}

/// The project a sync goes to: the one named, else the team's project made
/// from a Compose file of this name (its namespace).
async fn find_project(
    tx: &mut Transaction<'_, Postgres>,
    team_id: &str,
    project_id: Option<&str>,
    namespace: &str,
) -> ApiResult<Option<SyncProject>> {
    let row: Option<(String, String, String)> = match project_id.filter(|p| !p.is_empty()) {
        Some(id) => Some(
            sqlx::query_as(
                "SELECT id, name, namespace FROM pyo.projects WHERE id = $1 AND team_id = $2",
            )
            .bind(id)
            .bind(team_id)
            .fetch_optional(&mut **tx)
            .await?
            .ok_or_else(|| ApiError::not_found("Project"))?,
        ),
        None => {
            sqlx::query_as(
                "SELECT p.id, p.name, p.namespace FROM pyo.projects p
                  WHERE p.team_id = $1 AND p.namespace = $2
                    AND EXISTS (SELECT 1 FROM pyo.services a
                                 WHERE a.project_id = p.id AND NOT a.deleting AND a.spec ? 'compose')",
            )
            .bind(team_id)
            .bind(namespace)
            .fetch_optional(&mut **tx)
            .await?
        }
    };
    Ok(row.map(|(id, name, namespace)| SyncProject {
        id,
        name,
        namespace,
    }))
}

// ── The plan endpoint ────────────────────────────────────────────────────────

/// A Compose file to sync: its text, or a file in one of the team's GitHub
/// repositories, and the project it goes to.
#[derive(Debug, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct SyncBody {
    /// The team it's for.
    pub team_id: String,
    /// The project to sync. Without it: the team's project made from a
    /// Compose file of the same name, when there's one.
    #[serde(default)]
    pub project_id: Option<String>,
    /// The Compose file's text, when it isn't read from a repository.
    #[serde(default)]
    pub file: Option<String>,
    /// A .env file's text, for `${VAR}`s and `env_file: .env`.
    #[serde(default)]
    pub env: Option<String>,
    /// Or: a repository the team's GitHub App installations can see.
    #[serde(default)]
    pub repo: Option<compose::RepoInput>,
    /// The cluster new services go to: their public web ports get a free
    /// domain on it.
    #[serde(default)]
    pub cluster_id: Option<String>,
    /// The project's name, when the file has none.
    #[serde(default)]
    pub name: Option<String>,
    /// The file's name, e.g. "compose.yaml". Services remember it.
    #[serde(default)]
    pub file_name: Option<String>,
}

/// Plan a Docker Compose sync
///
/// What applying a Compose file to the project made from it would change,
/// service by service: added, changed (and what), unchanged, or not in the
/// file. Only what the file sets is compared, so settings changed in Pyo
/// that the file doesn't mention stay. A secret's value is never changed.
/// What can't change in place is listed under `manual` and left as it is.
/// Nothing changes. Pass `file` to the `syncCompose` operation to apply it,
/// or to `importCompose` when there's no `project` yet.
///
/// Besides Compose, each service's `x-pyo` can set `domains`, `behindCdn`
/// ("cloudflare" or "none"), `health` (a path like "/healthz", "tcp" or
/// "none"), `replicas`, `placement` (`{"servers": [...], "tags": [...]}`)
/// and `secrets` (variable names; their values are set in Pyo).
#[utoipa::path(post, path = "/compose/sync-plan", operation_id = "planComposeSync", tag = "Projects",
    request_body = SyncBody,
    responses(
        (status = 200, description = "The plan.", body = SyncPlan),
        (status = 400, description = "Not a Compose file Pyo can read, nothing importable, or no file given.", body = crate::error::ErrorBody),
        (status = 404, description = "No such team, project, cluster or file.", body = crate::error::ErrorBody),
        (status = 502, description = "GitHub couldn't be read.", body = crate::error::ErrorBody),
    ),
    extensions(("x-pyo-access" = json!("read"))))]
pub async fn plan(
    axum::extract::State(s): axum::extract::State<crate::AppState>,
    crate::auth::SessionUser(user): crate::auth::SessionUser,
    crate::error::Json(b): crate::error::Json<SyncBody>,
) -> ApiResult<crate::error::Json<SyncPlan>> {
    let file = compose::read_plan(
        &s,
        &user,
        &compose::PreviewBody {
            team_id: b.team_id.clone(),
            file: b.file,
            env: b.env,
            repo: b.repo,
            cluster_id: b.cluster_id,
            name: b.name,
            file_name: b.file_name,
        },
    )
    .await?;
    let mut tx = crate::rls::begin(&s.db, &user).await?;
    let project = find_project(
        &mut tx,
        &b.team_id,
        b.project_id.as_deref(),
        &file.project.namespace,
    )
    .await?;
    let existing = match &project {
        Some(p) => existing(&mut tx, &p.id).await?,
        None => vec![],
    };
    tx.commit().await?;
    Ok(crate::error::Json(plan_of(project, file, &existing)))
}

// ── Applying it ──────────────────────────────────────────────────────────────

/// A secret goes back blank, so the save keeps its sealed value.
fn blank_secrets(spec: &mut Value) {
    let blank = |env: &mut Value| {
        for e in env.as_array_mut().into_iter().flatten() {
            if e["secret"] == json!(true) {
                e["value"] = json!("");
            }
        }
    };
    blank(&mut spec["env"]);
    if let Some(env) = spec.get_mut("previews").and_then(|p| p.get_mut("env")) {
        blank(env);
    }
}

/// `syncCompose`: the plan worked out again, then applied through the
/// operations it stands for.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn apply(
    s: &crate::AppState,
    tx: &mut Transaction<'_, Postgres>,
    user: &crate::auth::User,
    project_id: &str,
    cluster_id: Option<String>,
    file: ComposePlan,
    prune: bool,
    confirm: bool,
    wake: &mut Vec<String>,
) -> ApiResult<Value> {
    use crate::ops::{Op, Role, nested, require_role, team_of};
    let team_id = team_of(tx, "projects", project_id).await?;
    require_role(tx, user, &team_id, Role::Developer).await?;
    if file.services.is_empty() || file.services.len() > 40 {
        return Err(ApiError::bad_request(
            "A Compose file has 1 to 40 services.",
        ));
    }
    for svc in &file.services {
        crate::util::require_dns_label(&svc.name, "Service name")?;
    }
    let (name, namespace): (String, String) =
        sqlx::query_as("SELECT name, namespace FROM pyo.projects WHERE id = $1")
            .bind(project_id)
            .fetch_one(&mut **tx)
            .await?;
    let existing = existing(tx, project_id).await?;
    let plan = plan_of(
        Some(SyncProject {
            id: project_id.to_owned(),
            name: name.clone(),
            namespace,
        }),
        file.clone(),
        &existing,
    );
    let now = pyo_proto::now_ms();
    let new_id = |p: &str| crate::ids::new_id(p);
    let mut cluster: Option<String> = None;
    let mut done = vec![];
    for ss in &plan.services {
        let want = ss
            .compose_name
            .as_ref()
            .and_then(|c| file.services.iter().find(|w| &w.compose_name == c));
        let old = ss
            .service_id
            .as_ref()
            .and_then(|id| existing.iter().find(|e| &e.id == id));
        match (ss.status.as_str(), want, old) {
            ("added", Some(w), _) => {
                if w.source.kind == "git" && w.source.repo_url.trim().is_empty() {
                    return Err(ApiError::bad_request(format!(
                        "{} is built from source: sync from its repository, or give it an image.",
                        w.name
                    )));
                }
                // Where new services run: the cluster given, else where the
                // project's services run, else the team's default.
                if cluster.is_none() {
                    let c = match cluster_id.clone().filter(|c| !c.is_empty()) {
                        Some(c) => c,
                        None => sqlx::query_scalar::<_, Option<String>>(
                            "SELECT COALESCE(
                                (SELECT cluster_id FROM pyo.services WHERE project_id = $1 AND NOT deleting
                                  GROUP BY cluster_id ORDER BY count(*) DESC, cluster_id LIMIT 1),
                                (SELECT default_cluster_id FROM pyo.teams WHERE id = $2))",
                        )
                        .bind(project_id)
                        .bind(&team_id)
                        .fetch_one(&mut **tx)
                        .await?
                        .ok_or_else(|| {
                            ApiError::bad_request(
                                "This team has no cluster to deploy to. Add a server first.",
                            )
                        })?,
                    };
                    if team_of(tx, "clusters", &c).await? != team_id {
                        return Err(ApiError::not_found("Cluster"));
                    }
                    cluster = Some(c);
                }
                let cluster_id = cluster.clone().unwrap_or_default();
                let id = new_id("svc");
                let deployment_id = new_id("dpl");
                let service = compose::service_spec(
                    w,
                    Some(&file.project.file),
                    &id,
                    &team_id,
                    project_id,
                    &cluster_id,
                    now,
                    &new_id,
                );
                let volumes = compose::volume_specs(w, &new_id);
                let r = nested(
                    s,
                    tx,
                    user,
                    Op::CreateService {
                        service,
                        deployment_id: deployment_id.clone(),
                        volumes,
                        databases: vec![],
                        confirm,
                        upload_id: None,
                    },
                    wake,
                )
                .await?;
                if let Some(cdn) = &w.behind_cdn {
                    nested(
                        s,
                        tx,
                        user,
                        Op::SetServiceBehindCdn {
                            service_id: id.clone(),
                            cdn: Some(cdn.clone()),
                        },
                        wake,
                    )
                    .await?;
                }
                done.push(json!({ "name": w.name, "id": id, "status": "added", "deploymentId": deployment_id, "warning": r.get("warning") }));
            }
            ("changed" | "unchanged", Some(w), Some(o)) => {
                let m = merge(o, w, &file.project.file, now, &new_id);
                if m.changes.is_empty() && !m.record_only {
                    continue;
                }
                for v in &m.volumes {
                    nested(
                        s,
                        tx,
                        user,
                        Op::AddVolume {
                            id: new_id("vol"),
                            service_id: o.id.clone(),
                            name: volume_label(&o.name, &v.name),
                            mount_path: v.mount_path.clone(),
                            size_gi: v.size_gi,
                        },
                        wake,
                    )
                    .await?;
                }
                let mut spec = m.spec;
                blank_secrets(&mut spec);
                spec["id"] = json!(o.id);
                let deployment_id = m.deploy.then(|| new_id("dpl"));
                let r = nested(
                    s,
                    tx,
                    user,
                    Op::SaveService {
                        service: spec,
                        deployment_id: deployment_id.clone(),
                        trigger: None,
                        confirm,
                    },
                    wake,
                )
                .await?;
                if let Some(cdn) = m.behind_cdn {
                    nested(
                        s,
                        tx,
                        user,
                        Op::SetServiceBehindCdn {
                            service_id: o.id.clone(),
                            cdn,
                        },
                        wake,
                    )
                    .await?;
                }
                if !m.changes.is_empty() {
                    done.push(json!({ "name": o.name, "id": o.id, "status": "changed", "deploymentId": deployment_id, "warning": r.get("warning") }));
                }
            }
            ("notInFile", _, Some(o)) if prune && ss.from_file => {
                nested(
                    s,
                    tx,
                    user,
                    Op::DeleteService {
                        id: o.id.clone(),
                        delete_volumes: false,
                    },
                    wake,
                )
                .await?;
                done.push(json!({ "name": o.name, "id": o.id, "status": "deleted" }));
            }
            _ => {}
        }
    }
    if !done.is_empty() {
        let file_name = compose::record(&file.project.file, "")["file"]
            .as_str()
            .unwrap_or("compose.yaml")
            .to_owned();
        crate::ops::log_activity(
            tx,
            &team_id,
            &user.name,
            "project.updated",
            "info",
            &format!("Synced {name} with {file_name}"),
            json!({ "kind": "project", "id": project_id, "label": name }),
            json!({ "projectId": project_id }),
        )
        .await?;
    }
    Ok(json!({ "ok": true, "projectId": project_id, "services": done, "plan": plan }))
}

#[cfg(test)]
#[path = "compose_sync_tests.rs"]
mod tests;
