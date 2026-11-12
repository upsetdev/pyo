//! Variable references between the services of a project:
//! `${{postgres.DATABASE_URL}}` in one service's variable is filled in with
//! that value of the `postgres` service when it deploys. Done here, on the
//! server, so the browser only ever sees the reference; the result is sealed
//! with the rest of the deployment's env (vault.rs).
//!
//! What a service offers (keep in step with dash/src/lib/pyo/refs.ts):
//! - HOST, PORT and URL: its address inside the cluster;
//! - PUBLIC_DOMAIN and PUBLIC_URL, when it has a domain;
//! - its own variables, by key;
//! - for the database templates, their connection details: USER, PASSWORD,
//!   DATABASE and DATABASE_URL (REDIS_URL for Valkey).

use std::collections::{BTreeMap, BTreeSet};

use pyo_proto::EnvVar;
use serde_json::Value;
use sqlx::{Postgres, Transaction};

use crate::error::{ApiError, ApiResult};

/// One `${{service.VAR}}` in a value, with where it sits.
#[derive(Debug, PartialEq)]
pub struct Ref<'a> {
    pub service: &'a str,
    pub var: &'a str,
    pub start: usize,
    pub end: usize,
}

fn service_name(s: &str) -> bool {
    !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

fn var_name(s: &str) -> bool {
    let mut b = s.bytes();
    matches!(b.next(), Some(c) if c.is_ascii_alphabetic() || c == b'_')
        && b.all(|c| c.is_ascii_alphanumeric() || c == b'_')
}

/// The references in a value. Anything else that looks like `${{…}}` is
/// left alone, as text.
pub fn parse(value: &str) -> Vec<Ref<'_>> {
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(i) = value[from..].find("${{") {
        let start = from + i;
        let Some(j) = value[start + 3..].find("}}") else {
            break;
        };
        let end = start + 3 + j + 2;
        let inner = value[start + 3..end - 2].trim();
        if let Some((service, var)) = inner.split_once('.')
            && service_name(service)
            && var_name(var)
        {
            out.push(Ref {
                service,
                var,
                start,
                end,
            });
            from = end;
        } else {
            from = start + 3;
        }
    }
    out
}

/// Percent-encodes what can't go in a URL's user or password as is.
fn enc(s: &str) -> String {
    s.bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

/// A database template's connection variables: which of its own keys hold
/// the user, password and database, and the URL's scheme and name.
struct Conn {
    user: Option<&'static str>,
    default_user: &'static str,
    password: &'static str,
    database: Option<&'static str>,
    scheme: &'static str,
    url: &'static str,
    query: &'static str,
}

fn conn(template: &str) -> Option<Conn> {
    Some(match template {
        "postgresql" => Conn {
            user: Some("POSTGRES_USER"),
            default_user: "postgres",
            password: "POSTGRES_PASSWORD",
            database: Some("POSTGRES_DB"),
            scheme: "postgresql",
            url: "DATABASE_URL",
            query: "",
        },
        "mysql" => Conn {
            user: Some("MYSQL_USER"),
            default_user: "root",
            password: "MYSQL_PASSWORD",
            database: Some("MYSQL_DATABASE"),
            scheme: "mysql",
            url: "DATABASE_URL",
            query: "",
        },
        "mariadb" => Conn {
            user: Some("MARIADB_USER"),
            default_user: "root",
            password: "MARIADB_PASSWORD",
            database: Some("MARIADB_DATABASE"),
            scheme: "mysql",
            url: "DATABASE_URL",
            query: "",
        },
        "mongodb" => Conn {
            user: Some("MONGO_INITDB_ROOT_USERNAME"),
            default_user: "root",
            password: "MONGO_INITDB_ROOT_PASSWORD",
            database: None,
            scheme: "mongodb",
            url: "DATABASE_URL",
            query: "?authSource=admin",
        },
        "valkey" => Conn {
            user: None,
            default_user: "",
            password: "VALKEY_PASSWORD",
            database: None,
            scheme: "redis",
            url: "REDIS_URL",
            query: "",
        },
        _ => return None,
    })
}

/// Everything a service offers to the others, from its spec with values opened.
pub fn offered(spec: &Value, namespace: &str) -> BTreeMap<String, String> {
    let s = |v: &Value| v.as_str().unwrap_or_default().to_owned();
    let name = s(&spec["name"]);
    let mut out: BTreeMap<String, String> = BTreeMap::new();
    for e in spec["env"].as_array().into_iter().flatten() {
        let k = s(&e["key"]);
        if !k.is_empty() {
            out.insert(k, s(&e["value"]));
        }
    }
    let own = out.clone();
    let host = format!("{name}.{namespace}.svc.cluster.local");
    let port = spec["ports"]
        .as_array()
        .and_then(|p| p.first())
        .and_then(|p| p["containerPort"].as_i64());
    out.insert("HOST".into(), host.clone());
    if let Some(p) = port {
        out.insert("PORT".into(), p.to_string());
        out.insert("URL".into(), format!("http://{host}:{p}"));
    }
    if let Some(d) = spec["domains"]
        .as_array()
        .and_then(|d| d.first())
        .map(|d| s(&d["hostname"]))
        .filter(|d| !d.is_empty())
    {
        out.insert("PUBLIC_URL".into(), format!("https://{d}"));
        out.insert("PUBLIC_DOMAIN".into(), d);
    }
    let template = s(&spec["source"]["templateId"]);
    if let Some(c) = conn(&template) {
        let get = |k: &str| own.get(k).cloned().unwrap_or_default();
        let user = c
            .user
            .map(get)
            .filter(|u| !u.is_empty())
            .unwrap_or_else(|| c.default_user.to_owned());
        let password = get(c.password);
        let database = c
            .database
            .map(get)
            .filter(|d| !d.is_empty())
            .unwrap_or_else(|| {
                if c.database.is_some() {
                    user.clone()
                } else {
                    String::new()
                }
            });
        let auth = match (user.is_empty(), password.is_empty()) {
            (true, true) => String::new(),
            (true, false) => format!(":{}@", enc(&password)),
            (false, true) => format!("{}@", enc(&user)),
            (false, false) => format!("{}:{}@", enc(&user), enc(&password)),
        };
        let path = if database.is_empty() {
            "/".to_owned()
        } else {
            format!("/{database}")
        };
        let p = port.map(|p| format!(":{p}")).unwrap_or_default();
        out.insert(
            c.url.into(),
            format!(
                "{}://{auth}{host}{p}{}{}",
                c.scheme,
                if c.scheme == "redis" { "" } else { &path },
                c.query
            ),
        );
        if c.user.is_some() {
            out.insert("USER".into(), user);
        }
        out.insert("PASSWORD".into(), password);
        if c.database.is_some() {
            out.insert("DATABASE".into(), database);
        }
    }
    out
}

/// Fills in one value; the error names the reference that couldn't be.
pub fn substitute(
    value: &str,
    offers: &BTreeMap<String, BTreeMap<String, String>>,
) -> Result<String, String> {
    let refs = parse(value);
    if refs.is_empty() {
        return Ok(value.to_owned());
    }
    let mut out = String::with_capacity(value.len());
    let mut at = 0;
    for r in refs {
        let Some(vars) = offers.get(r.service) else {
            return Err(format!(
                "there's no service called {} in this project",
                r.service
            ));
        };
        let Some(v) = vars.get(r.var) else {
            return Err(format!("{} has no variable {}", r.service, r.var));
        };
        out.push_str(&value[at..r.start]);
        out.push_str(v);
        at = r.end;
    }
    out.push_str(&value[at..]);
    Ok(out)
}

/// Fills in the references of a deploy's env from the project's services.
pub async fn resolve(
    tx: &mut Transaction<'_, Postgres>,
    project_id: &str,
    namespace: &str,
    env: &mut [EnvVar],
) -> ApiResult<()> {
    let names: BTreeSet<String> = env
        .iter()
        .flat_map(|e| parse(&e.value).into_iter().map(|r| r.service.to_owned()))
        .collect();
    if names.is_empty() {
        return Ok(());
    }
    let specs: Vec<Value> = sqlx::query_scalar(
        "SELECT spec FROM pyo.services WHERE project_id = $1 AND name = ANY($2) AND NOT deleting",
    )
    .bind(project_id)
    .bind(names.iter().cloned().collect::<Vec<_>>())
    .fetch_all(&mut **tx)
    .await?;
    let mut offers = BTreeMap::new();
    for mut spec in specs {
        crate::vault::get().open_env(&mut spec).await?;
        let name = spec["name"].as_str().unwrap_or_default().to_owned();
        offers.insert(name, offered(&spec, namespace));
    }
    for e in env.iter_mut() {
        e.value = substitute(&e.value, &offers).map_err(|why| {
            ApiError::bad_request(format!("{} can't be filled in: {why}.", e.key))
        })?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_references() {
        let r = parse("postgres://${{ db.USER }}:${{db.PASSWORD}}@x ${{Bad.X}} ${{a.1x}} ${{");
        assert_eq!(
            r.iter().map(|r| (r.service, r.var)).collect::<Vec<_>>(),
            vec![("db", "USER"), ("db", "PASSWORD")]
        );
        assert!(parse("plain").is_empty());
    }

    #[test]
    fn offers_connection_variables() {
        let spec = json!({
            "name": "postgres", "source": { "type": "template", "templateId": "postgresql" },
            "env": [{ "key": "POSTGRES_USER", "value": "app" }, { "key": "POSTGRES_PASSWORD", "value": "p@ss/word" }, { "key": "POSTGRES_DB", "value": "shop" }],
            "ports": [{ "containerPort": 5432 }], "domains": []
        });
        let o = offered(&spec, "shop");
        assert_eq!(o["HOST"], "postgres.shop.svc.cluster.local");
        assert_eq!(
            o["DATABASE_URL"],
            "postgresql://app:p%40ss%2Fword@postgres.shop.svc.cluster.local:5432/shop"
        );
        assert_eq!(o["PASSWORD"], "p@ss/word");
        assert!(!o.contains_key("PUBLIC_URL"));
        let v = offered(
            &json!({ "name": "cache", "source": { "templateId": "valkey" }, "env": [{ "key": "VALKEY_PASSWORD", "value": "s3cret" }], "ports": [{ "containerPort": 6379 }] }),
            "shop",
        );
        assert_eq!(
            v["REDIS_URL"],
            "redis://:s3cret@cache.shop.svc.cluster.local:6379"
        );
    }

    #[test]
    fn substitutes_and_names_what_is_missing() {
        let mut offers = BTreeMap::new();
        offers.insert(
            "api".to_owned(),
            BTreeMap::from([("URL".to_owned(), "http://api:80".to_owned())]),
        );
        assert_eq!(
            substitute("x=${{api.URL}}/v1", &offers).unwrap(),
            "x=http://api:80/v1"
        );
        assert_eq!(
            substitute("${{api.NOPE}}", &offers).unwrap_err(),
            "api has no variable NOPE"
        );
        assert_eq!(
            substitute("${{db.URL}}", &offers).unwrap_err(),
            "there's no service called db in this project"
        );
    }
}
