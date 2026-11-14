//! The API reference (OpenAPI 3.1), made from the code: the handlers'
//! `#[utoipa::path]` attributes, the request and response types, and the
//! operations of `/api/v1/ops` with their doc comments and the API key access
//! `key_access_for` says each needs. `cargo run -p pyo-api --bin openapi`
//! writes it for the docs.
//!
//! Only what an API key (`Authorization: Bearer pyo_…`) can call is in it:
//! not the agent's and builders' endpoints, sign-in, OAuth, MCP, terminals or
//! webhooks.

use serde_json::{Map, Value, json};
use utoipa::{
    Modify, OpenApi,
    openapi::{
        Discriminator, OpenApi as Spec, RefOr, Required, Response, Schema,
        extensions::ExtensionsBuilder,
        schema::{OneOf, Ref},
        security::{HttpAuthScheme, HttpBuilder, SecurityScheme},
    },
};

use crate::{auth::KeyAccess, error::ErrorBody, ops::Op};

#[derive(OpenApi)]
#[openapi(
    info(
        title = "Pyo API",
        description = "Everything the dashboard does, for scripts. Read with GET, and change things with one operation per POST to `/ops`.\n\nAuthenticate with an API key from the dashboard's API keys page: `Authorization: Bearer pyo_…`. A key acts as you, with your role in each team, and is limited further by its access: **read** (look, never change), **deploy** (also deploy, roll back, restart, stop and scale services) or **full** (everything you can do in its teams). Each endpoint and operation says the access it needs in `x-pyo-access`.\n\nErrors come back as `{\"error\": \"…\"}` with a status code. Times are milliseconds since 1970 unless they say otherwise.",
        license(name = "Apache-2.0", identifier = "Apache-2.0"),
    ),
    servers((url = "https://{host}/api/v1", description = "Your Pyo", variables(("host" = (default = "your-pyo.example.com", description = "Where your Pyo runs."))))),
    paths(
        crate::ops::state,
        crate::ops::me,
        crate::ops::meta,
        crate::ops::run,
        crate::ops::service_logs,
        crate::volumes::snapshots,
        crate::volumes::delete_snapshot,
        crate::ops::service_preview,
        crate::traffic::get,
        crate::cronruns::list,
        crate::ops::metrics,
        crate::ops::observability_settings,
        crate::ops::observability,
        crate::ops::access_logs,
        crate::ops::connections,
        crate::ops::firewall_blocked,
        crate::ops::firewall_me,
        crate::ops::waf_hits,
        crate::images::list,
        crate::images::registry,
        crate::databases::list,
        crate::databases::get,
        crate::databases::extensions,
        crate::database_logs::logs,
        crate::database_analytics::analytics,
        crate::database_backups::add,
        crate::database_imports::check,
        crate::database_imports::start,
        crate::database_imports::retry,
        crate::database_imports::create_upload,
        crate::database_imports::upload_content,
        crate::github::repos,
        crate::github::build_file,
        crate::compose::preview,
        crate::platform_import::preview,
        crate::platform_import::apply,
        crate::platform_import::get_run,
        crate::platform_import::list_runs,
    ),
    components(schemas(ErrorBody, Op)),
    modifiers(&ApiKey),
    security(("apiKey" = [])),
    // In the order the docs list them. Operations of /ops are filed under
    // these too (`x-tags`), by `ops::op_doc`.
    tags(
        (name = "Account", description = "Who you are, and what this Pyo is."),
        (name = "State", description = "Everything you can see, in one read."),
        (name = "Operations", description = "Every change: one operation per request to `/ops`."),
        (name = "Teams", description = "Teams, their members and invites."),
        (name = "Projects", description = "Projects group a team's services."),
        (name = "Services", description = "Create, change, scale and stop services, and read their logs and traffic."),
        (name = "Deployments", description = "Deploy, roll back and cancel, and manage deploy hooks."),
        (name = "Cron jobs", description = "Commands a service runs on a schedule, and their runs."),
        (name = "Domains", description = "Domains and their certificates."),
        (name = "Volumes", description = "Persistent storage for services."),
        (name = "Databases", description = "Postgres, MySQL and KV databases Pyo runs, and databases it backs up."),
        (name = "Backups", description = "A team's backups of its volumes and databases."),
        (name = "Clusters", description = "Groups of servers that run services."),
        (name = "Firewall", description = "Per service: who may reach it, and its web application firewall."),
        (name = "Servers", description = "Add, tag, pause and remove servers."),
        (name = "Builders", description = "Where images are built from Git."),
        (name = "Images", description = "The images a team deployed, and logins for private registries."),
        (name = "GitHub", description = "Repositories to deploy from."),
        (name = "Integrations", description = "Keys for outside services, and secret stores."),
        (name = "Observability", description = "Usage of servers and services, website traffic and request logs."),
    ),
)]
struct Doc;

struct ApiKey;

impl Modify for ApiKey {
    fn modify(&self, spec: &mut Spec) {
        let c = spec.components.get_or_insert_with(Default::default);
        let scheme = HttpBuilder::new()
            .scheme(HttpAuthScheme::Bearer)
            .bearer_format("pyo_…")
            .description(Some("An API key from the dashboard's API keys page: `Authorization: Bearer pyo_…`. Keys can expire, and an expired one gets a 401."))
            .build();
        c.add_security_scheme("apiKey", SecurityScheme::Http(scheme));
    }
}

/// serde's `rename_all_fields = "camelCase"`, which the schema derive doesn't
/// apply to an enum's variants.
fn camel(s: &str) -> String {
    let mut out = String::new();
    let mut up = false;
    for c in s.chars() {
        if c == '_' {
            up = true;
        } else if up {
            out.extend(c.to_uppercase());
            up = false;
        } else {
            out.push(c);
        }
    }
    out
}

fn pascal(s: &str) -> String {
    let mut c = s.chars();
    c.next()
        .map(|f| f.to_uppercase().chain(c).collect())
        .unwrap_or_default()
}

/// The smallest JSON a schema accepts, to deserialize each operation with
/// serde and ask `key_access_for` about it.
fn sample(schema: &Value, components: &Value) -> Value {
    if let Some(r) = schema["$ref"].as_str() {
        let name = r.rsplit('/').next().unwrap_or("");
        return sample(&components[name], components);
    }
    if let Some(e) = schema["enum"].as_array().and_then(|e| e.first()) {
        return e.clone();
    }
    if let Some(first) = schema["oneOf"].as_array().and_then(|o| o.first()) {
        return sample(first, components);
    }
    let ty = match &schema["type"] {
        Value::Array(ts) => ts
            .iter()
            .find(|t| *t != "null")
            .cloned()
            .unwrap_or(Value::Null),
        t => t.clone(),
    };
    match ty.as_str() {
        Some("string") => json!("x"),
        Some("integer") | Some("number") => json!(1),
        Some("boolean") => json!(false),
        Some("array") => json!([]),
        Some("object") => {
            let mut m = Map::new();
            for r in schema["required"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
            {
                m.insert(r.to_owned(), sample(&schema["properties"][r], components));
            }
            Value::Object(m)
        }
        _ => Value::Null,
    }
}

/// "scaleService" → "Scale service".
fn title(op: &str) -> String {
    // Named after servers' regions, not the operation's id.
    match op {
        "setServerLocation" => return "Set server region".into(),
        "importCompose" => return "Import Docker Compose".into(),
        _ => {}
    }
    let mut words: Vec<String> = vec![];
    for c in op.chars() {
        if c.is_uppercase() || words.is_empty() {
            words.push(String::new());
        }
        words.last_mut().expect("a word").extend(c.to_lowercase());
    }
    let words: Vec<String> = words
        .into_iter()
        .map(|w| match w.as_str() {
            "github" => "GitHub".into(),
            "url" => "URL".into(),
            "api" => "API".into(),
            _ => w,
        })
        .collect();
    let t = words.join(" ");
    let mut c = t.chars();
    c.next()
        .map(|f| f.to_uppercase().chain(c).collect())
        .unwrap_or_default()
}

/// An id of the kind `prefix` names, for examples.
fn example_id(prefix: &str) -> Value {
    json!(format!("{prefix}_8kq2x7ad"))
}

/// A plausible request: every required field, and the optional ones too
/// when only an id is required (an update). Values follow the field's name,
/// or the first value its description quotes.
fn example(op: &str, schema: &Value, components: &Value, tag: &str, id_prefix: &str) -> Value {
    let required: Vec<&str> = schema["required"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    let update = required.iter().all(|r| *r == "op" || *r == "id");
    let mut m = Map::new();
    for (k, p) in schema["properties"].as_object().into_iter().flatten() {
        if k == "op" {
            m.insert(k.clone(), json!(op));
            continue;
        }
        if !required.contains(&k.as_str()) && !update {
            continue;
        }
        m.insert(k.clone(), example_value(k, p, components, tag, id_prefix));
    }
    Value::Object(m)
}

fn example_value(k: &str, p: &Value, components: &Value, tag: &str, id_prefix: &str) -> Value {
    if let Some(one) = p["oneOf"]
        .as_array()
        .and_then(|o| o.iter().find(|x| x["type"] != "null"))
    {
        return example_value(k, one, components, tag, id_prefix);
    }
    if let Some(r) = p["$ref"].as_str() {
        let s = &components[r.rsplit('/').next().unwrap_or("")];
        let mut m = Map::new();
        for (k, p) in s["properties"].as_object().into_iter().flatten() {
            m.insert(k.clone(), example_value(k, p, components, tag, id_prefix));
        }
        return Value::Object(m);
    }
    let ty = match &p["type"] {
        Value::Array(ts) => ts
            .iter()
            .find(|t| *t != "null")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned(),
        t => t.as_str().unwrap_or("").to_owned(),
    };
    let quoted = p["description"]
        .as_str()
        .and_then(|d| d.split('"').nth(1))
        .map(str::to_owned);
    let id = |prefix: &str| example_id(prefix);
    match (k, ty.as_str()) {
        ("id", "integer") => json!(51234567),
        ("id", _) => id(id_prefix),
        ("teamId", _) => id("team"),
        ("projectId", _) => id("prj"),
        ("serviceId", _) => id("svc"),
        ("clusterId" | "defaultClusterId", _) => id("cl"),
        ("deploymentId" | "rollbackOf" | "retryOf", _) => id("dpl"),
        ("serverId", _) => id("srv"),
        ("storeId", _) => id("sst"),
        ("domainId", _) => id("dom"),
        ("envId", _) => id("env"),
        ("target", _) => id("vol"),
        ("certificateIds", _) => json!([example_id("crt")]),
        ("teamIds", _) => json!([example_id("team")]),
        ("exceptions", _) => json!([example_id("svc")]),
        ("tags", _) => json!(["gpu"]),
        ("service", _) => json!({
            "id": example_id("svc"), "projectId": example_id("prj"), "clusterId": example_id("cl"), "name": "web",
            "source": { "type": "image", "image": "nginx:1.29" }, "replicas": 2,
            "ports": [{ "name": "http", "containerPort": 80, "protocol": "TCP", "exposure": "http" }, { "name": "redis", "containerPort": 6379, "protocol": "TCP", "exposure": "loadbalancer", "publicPort": 6379, "proxy": true, "ipFilter": { "mode": "allow", "cidrs": ["203.0.113.0/24"] } }],
            "env": [], "domains": [],
        }),
        ("name", _) => json!(match tag {
            "Teams" => "Acme",
            "Projects" => "Shop",
            "Clusters" => "edge",
            "Volumes" => "data",
            "Builders" => "builder-1",
            "Domains" => "shop.example.com",
            "Images" => "GitHub packages",
            "Cron jobs" => "nightly-cleanup",
            "Deployments" => "Deploy from CI",
            _ => "web",
        }),
        ("serverName", _) => json!("sg-3"),
        ("server", _) => json!("sg-1"),
        ("email", _) => json!("ada@example.com"),
        ("location", _) => json!("Singapore, SG"),
        ("namespace", _) => json!("shop"),
        ("schedule", _) if tag == "Cron jobs" => json!("0 3 * * *"),
        ("schedule", _) => json!("daily"),
        ("role", _) => json!("developer"),
        ("kind", _) if tag == "Integrations" => json!("tailscale"),
        ("endpoint", _) => json!("https://s3.eu-central-1.amazonaws.com"),
        ("region", _) => json!("eu-central-1"),
        ("bucket", _) => json!("acme-backups"),
        ("prefix", _) => json!("pyo/"),
        ("accessKey", _) => json!("AKIAEXAMPLE"),
        ("secretKey", _) => json!("an-s3-secret-key"),
        ("command", _) => json!("python manage.py clearsessions"),
        ("mountPath", _) => json!("/data"),
        ("host", _) => json!("ghcr.io"),
        ("username", _) => json!("ci-bot"),
        ("password", _) => json!("a-registry-token"),
        ("secret", _) => json!("tskey-auth-k1a2b3c4d5"),
        ("path", _) => json!("secret/shop"),
        ("commonName", _) => json!("cdn.example.com"),
        ("certificate", _) => json!("-----BEGIN CERTIFICATE-----\n…\n-----END CERTIFICATE-----"),
        ("privateKey", _) => json!("-----BEGIN PRIVATE KEY-----\n…\n-----END PRIVATE KEY-----"),
        ("description", _) => json!("Storefront and its workers"),
        ("replicas", _) => json!(3),
        ("hour", _) => json!(2),
        ("keep", _) => json!(14),
        ("sizeGi", _) => json!(10),
        ("years", _) => json!(1),
        ("port", _) => json!(5432),
        ("min", _) => json!(1),
        ("max", _) => json!(5),
        ("targetCpu", _) => json!(70),
        ("targetMemory", _) => json!(80),
        ("force" | "thisMachine" | "paused", _) => json!(false),
        (_, "string") => json!(quoted.unwrap_or_else(|| "example".into())),
        (_, "integer" | "number") => json!(1),
        (_, "boolean") => json!(true),
        (_, "array") => json!([]),
        (_, "object") => json!({}),
        _ => Value::Null,
    }
}

fn access_name(a: Option<KeyAccess>) -> Option<&'static str> {
    a.map(|a| match a {
        KeyAccess::Read => "read",
        KeyAccess::Deploy => "deploy",
        KeyAccess::Full => "full",
    })
}

/// Each operation of `/ops` as its own named schema (`CreateServiceOp`…)
/// with its API key access, and `Op` as their `oneOf` on `op`. Operations
/// API keys can't run are left out.
fn split_ops(spec: &mut Spec) {
    let components = spec.components.as_mut().expect("components");
    let for_sampling = serde_json::to_value(&components.schemas).expect("schemas");
    let Some(RefOr::T(Schema::OneOf(all))) = components.schemas.remove("Op") else {
        panic!("Op isn't a oneOf")
    };
    let mut one_of = OneOf::new();
    one_of.description = all.description.clone();
    let mut discriminator = Discriminator::new("op");
    for item in all.items {
        let RefOr::T(Schema::Object(mut o)) = item else {
            panic!("an operation isn't an object")
        };
        // Fields in camelCase, `op` first.
        let props = std::mem::take(&mut o.properties);
        let op_schema = props.get("op").cloned().expect("op");
        o.properties.insert("op".into(), op_schema);
        for (k, v) in props.into_iter().filter(|(k, _)| k != "op") {
            o.properties.insert(camel(&k), v);
        }
        o.required = std::iter::once("op".to_owned())
            .chain(o.required.iter().filter(|r| *r != "op").map(|r| camel(r)))
            .collect();
        let value = serde_json::to_value(&o).expect("op schema");
        let name = value["properties"]["op"]["enum"][0]
            .as_str()
            .expect("op name")
            .to_owned();
        let op: Op = serde_json::from_value(sample(&value, &for_sampling))
            .unwrap_or_else(|e| panic!("{name}: the schema's own sample doesn't parse: {e}"));
        let Some(access) = access_name(crate::ops::key_access_for(&op)) else {
            continue;
        };
        let (tag, id_prefix) = crate::ops::op_doc(&op);
        let first = o.description.take().unwrap_or_default();
        o.title = Some(title(&name));
        o.description = Some(format!("{}\n\nAPI key access: **{access}**.", first.trim()));
        let ex = example(&name, &value, &for_sampling, tag, id_prefix);
        o.example = Some(ex.clone());
        o.examples = vec![ex];
        o.extensions = Some(
            ExtensionsBuilder::new()
                .add("x-pyo-access", access)
                .add("x-tags", json!([tag]))
                .build(),
        );
        let schema_name = format!("{}Op", pascal(&name));
        let r = format!("#/components/schemas/{schema_name}");
        discriminator.mapping.insert(name, r.clone());
        components
            .schemas
            .insert(schema_name, RefOr::T(Schema::Object(o)));
        one_of.items.push(RefOr::Ref(Ref::new(r)));
    }
    one_of.discriminator = Some(discriminator);
    components
        .schemas
        .insert("Op".into(), RefOr::T(Schema::OneOf(one_of)));
}

/// What every operation can answer besides its own: no or a bad key, and
/// Pyo failing. Then the access in words in each description.
fn common_responses(spec: &mut Spec) {
    let err = |d: &str| {
        let mut r = Response::new(d);
        r.content.insert(
            "application/json".into(),
            utoipa::openapi::ContentBuilder::new()
                .schema(Some(Ref::from_schema_name("ErrorBody")))
                .build(),
        );
        RefOr::T(r)
    };
    for item in spec.paths.paths.values_mut() {
        for op in [
            &mut item.get,
            &mut item.post,
            &mut item.put,
            &mut item.delete,
        ]
        .into_iter()
        .flatten()
        {
            let open = op.security.as_ref().is_some_and(|s| {
                s.iter().all(|r| {
                    serde_json::to_value(r).is_ok_and(|v| v.as_object().is_some_and(Map::is_empty))
                })
            });
            if !open {
                op.responses
                    .responses
                    .entry("401".into())
                    .or_insert_with(|| {
                        err("No API key, or not a valid one (revoked, expired or mistyped).")
                    });
            }
            op.responses
                .responses
                .entry("500".into())
                .or_insert_with(|| err("Something went wrong on Pyo's side. It's logged."));
            let access = op
                .extensions
                .as_ref()
                .and_then(|e| e.get("x-pyo-access"))
                .and_then(Value::as_str)
                .unwrap_or("");
            let words = match access {
                "none" => "No API key needed.".to_owned(),
                "varies" => {
                    "API key access: each operation says what it needs (**deploy** or **full**)."
                        .to_owned()
                }
                a => format!("API key access: **{a}**."),
            };
            op.description = Some(match op.description.take() {
                Some(d) if !d.trim().is_empty() => format!("{}\n\n{words}", d.trim()),
                _ => words,
            });
            if let Some(body) = op.request_body.as_mut() {
                body.required = Some(Required::True);
            }
        }
    }
}

/// Drop schemas nothing refers to any more (those only operations API keys
/// can't run used).
fn prune(spec: &mut Spec) {
    loop {
        let text = serde_json::to_string(&spec).expect("spec");
        let components = spec.components.as_mut().expect("components");
        let unused: Vec<String> = components
            .schemas
            .keys()
            .filter(|k| !text.contains(&format!("\"#/components/schemas/{k}\"")))
            .cloned()
            .collect();
        if unused.is_empty() {
            return;
        }
        for k in unused {
            components.schemas.remove(&k);
        }
    }
}

/// Every operation of the `Op` enum, and the API key access it needs (None:
/// keys can't run it). For tests that check each one is somewhere.
#[cfg(test)]
pub(crate) fn all_ops() -> Vec<(String, Option<KeyAccess>)> {
    let spec = Doc::openapi();
    let components = spec.components.expect("components");
    let for_sampling = serde_json::to_value(&components.schemas).expect("schemas");
    let Some(RefOr::T(Schema::OneOf(all))) = components.schemas.get("Op") else {
        panic!("Op isn't a oneOf")
    };
    all.items
        .iter()
        .map(|item| {
            let mut v = serde_json::to_value(item).expect("op");
            let props: Map<String, Value> = v["properties"]
                .as_object()
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .map(|(k, x)| (camel(&k), x))
                .collect();
            v["properties"] = Value::Object(props);
            v["required"] = json!(
                v["required"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default()
                    .iter()
                    .filter_map(Value::as_str)
                    .map(camel)
                    .collect::<Vec<_>>()
            );
            let name = v["properties"]["op"]["enum"][0]
                .as_str()
                .expect("op name")
                .to_owned();
            let op: Op = serde_json::from_value(sample(&v, &for_sampling))
                .unwrap_or_else(|e| panic!("{name}: {e}"));
            (name, crate::ops::key_access_for(&op))
        })
        .collect()
}

/// The API reference.
pub fn spec() -> Spec {
    let mut spec = Doc::openapi();
    spec.info.version = env!("CARGO_PKG_VERSION").to_owned();
    spec.info.contact = None;
    split_ops(&mut spec);
    common_responses(&mut spec);
    prune(&mut spec);
    spec
}

/// JSON that keeps its keys' order, so the reference reads in the order
/// it's written; extensions (`x-…`), which the spec keeps in a hash map, go
/// last and sorted, so the output is the same on every run.
enum Node {
    Obj(Vec<(String, Node)>),
    Arr(Vec<Node>),
    Leaf(Value),
}

impl<'de> serde::Deserialize<'de> for Node {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> serde::de::Visitor<'de> for V {
            type Value = Node;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("JSON")
            }
            fn visit_map<A: serde::de::MapAccess<'de>>(self, mut m: A) -> Result<Node, A::Error> {
                let mut out = vec![];
                while let Some((k, v)) = m.next_entry::<String, Node>()? {
                    out.push((k, v));
                }
                let (mut ext, rest): (Vec<_>, Vec<_>) =
                    out.into_iter().partition(|(k, _)| k.starts_with("x-"));
                ext.sort_by(|a, b| a.0.cmp(&b.0));
                Ok(Node::Obj(rest.into_iter().chain(ext).collect()))
            }
            fn visit_seq<A: serde::de::SeqAccess<'de>>(self, mut s: A) -> Result<Node, A::Error> {
                let mut out = vec![];
                while let Some(v) = s.next_element::<Node>()? {
                    out.push(v);
                }
                Ok(Node::Arr(out))
            }
            fn visit_bool<E>(self, v: bool) -> Result<Node, E> {
                Ok(Node::Leaf(v.into()))
            }
            fn visit_i64<E>(self, v: i64) -> Result<Node, E> {
                Ok(Node::Leaf(v.into()))
            }
            fn visit_u64<E>(self, v: u64) -> Result<Node, E> {
                Ok(Node::Leaf(v.into()))
            }
            fn visit_f64<E>(self, v: f64) -> Result<Node, E> {
                Ok(Node::Leaf(v.into()))
            }
            fn visit_str<E>(self, v: &str) -> Result<Node, E> {
                Ok(Node::Leaf(v.into()))
            }
            fn visit_unit<E>(self) -> Result<Node, E> {
                Ok(Node::Leaf(Value::Null))
            }
        }
        d.deserialize_any(V)
    }
}

impl serde::Serialize for Node {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::{SerializeMap, SerializeSeq};
        match self {
            Node::Obj(kv) => {
                let mut m = s.serialize_map(Some(kv.len()))?;
                for (k, v) in kv {
                    m.serialize_entry(k, v)?;
                }
                m.end()
            }
            Node::Arr(a) => {
                let mut q = s.serialize_seq(Some(a.len()))?;
                for v in a {
                    q.serialize_element(v)?;
                }
                q.end()
            }
            Node::Leaf(v) => v.serialize(s),
        }
    }
}

/// The API reference as the docs keep it: pretty JSON, ending in a newline.
pub fn spec_json() -> String {
    let raw = spec().to_json().expect("the spec serializes");
    let node: Node = serde_json::from_str(&raw).expect("the spec parses");
    let mut s = serde_json::to_string_pretty(&node).expect("the spec serializes");
    s.push('\n');
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_operation_is_in_the_spec_and_parses() {
        let v: Value = serde_json::from_str(&spec_json()).unwrap();
        let schemas = &v["components"]["schemas"];
        let mapping = schemas["Op"]["discriminator"]["mapping"]
            .as_object()
            .unwrap();
        assert!(mapping.len() > 50, "{} operations", mapping.len());
        for (name, r) in mapping {
            let s = &schemas[r.as_str().unwrap().rsplit('/').next().unwrap()];
            assert_eq!(s["properties"]["op"]["enum"][0], json!(name));
            assert!(
                s["x-pyo-access"] == "deploy" || s["x-pyo-access"] == "full",
                "{name}"
            );
            assert!(
                s["description"].as_str().is_some_and(|d| d.len() > 30),
                "{name} has no description"
            );
            for k in s["properties"].as_object().unwrap().keys() {
                assert!(!k.contains('_'), "{name}.{k} isn't camelCase");
            }
        }
        let tags: Vec<&str> = v["tags"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        for (name, r) in mapping {
            let s = &schemas[r.as_str().unwrap().rsplit('/').next().unwrap()];
            let ex = &s["example"];
            let op: Op = serde_json::from_value(ex.clone())
                .unwrap_or_else(|e| panic!("{name}'s example doesn't parse: {e}"));
            assert_eq!(
                access_name(crate::ops::key_access_for(&op)),
                s["x-pyo-access"].as_str(),
                "{name}"
            );
            assert!(
                tags.contains(&s["x-tags"][0].as_str().unwrap()),
                "{name}'s tag isn't defined"
            );
            assert!(
                s["title"]
                    .as_str()
                    .is_some_and(|t| !t.is_empty() && t.chars().next().unwrap().is_uppercase()),
                "{name}"
            );
        }
        assert_eq!(schemas["ScaleServiceOp"]["title"], "Scale service");
        assert_eq!(schemas["ScaleServiceOp"]["x-tags"], json!(["Services"]));
        // Account and installation operations aren't for API keys.
        assert!(
            !mapping.contains_key("createApiToken") && !mapping.contains_key("saveEmailSettings")
        );
        assert_eq!(schemas["DeployOp"]["x-pyo-access"], "deploy");
        assert_eq!(schemas["CreateServiceOp"]["x-pyo-access"], "full");
        assert!(
            schemas["DeployOp"]["required"]
                .as_array()
                .unwrap()
                .contains(&json!("serviceId"))
        );
        // A request the docs show parses as the same operation.
        let op: Op = serde_json::from_value(
            json!({ "op": "scaleService", "id": "svc_abcdef", "replicas": 3 }),
        )
        .unwrap();
        assert_eq!(crate::ops::key_access_for(&op), Some(KeyAccess::Deploy));
    }

    #[test]
    fn paths_and_errors() {
        let v: Value = serde_json::from_str(&spec_json()).unwrap();
        assert_eq!(v["openapi"], "3.1.0");
        assert_eq!(v["servers"][0]["url"], "https://{host}/api/v1");
        for (path, item) in v["paths"].as_object().unwrap() {
            for (method, op) in item.as_object().unwrap() {
                let access = op["x-pyo-access"]
                    .as_str()
                    .unwrap_or_else(|| panic!("{method} {path} has no x-pyo-access"));
                assert!(["none", "read", "deploy", "full", "varies"].contains(&access));
                assert!(op["responses"]["500"].is_object(), "{method} {path}");
                assert!(
                    op["operationId"].is_string() && op["tags"][0].is_string(),
                    "{method} {path}"
                );
            }
        }
        assert!(
            v["paths"]["/meta"]["get"]["responses"]["401"].is_null(),
            "meta needs no key"
        );
        assert!(v["paths"]["/state"]["get"]["responses"]["401"].is_object());
    }

    #[test]
    fn plain_words() {
        fn texts(v: &Value, out: &mut Vec<String>) {
            match v {
                Value::Object(m) => m.iter().for_each(|(k, v)| match v {
                    Value::String(s) if k == "description" || k == "summary" || k == "title" => {
                        out.push(s.to_lowercase())
                    }
                    v => texts(v, out),
                }),
                Value::Array(a) => a.iter().for_each(|v| texts(v, out)),
                _ => {}
            }
        }
        let mut all = vec![];
        texts(&serde_json::from_str(&spec_json()).unwrap(), &mut all);
        for t in &all {
            for w in [
                "kubernetes",
                "k3s",
                "kubectl",
                "namespace",
                " pod",
                "taint",
                "daemonset",
                "statefulset",
            ] {
                assert!(!t.contains(w), "the reference says {w:?}: {t}");
            }
        }
    }

    /// The docs' copy is current, where there is one (pyo-docs beside pyo).
    #[test]
    fn docs_copy_is_current() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../../pyo-docs/openapi/pyo.json");
        let Ok(have) = std::fs::read_to_string(&path) else {
            return;
        };
        assert!(
            have == spec_json(),
            "{} is out of date: run `cargo run -p pyo-api --bin openapi`",
            path.display()
        );
    }
}
