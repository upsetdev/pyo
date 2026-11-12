//! Postgres extensions a Database turns on (`spec.postgres.extensions`), from
//! the catalog in `extensions.json` (the API and the dashboard read the same
//! file). The "managed" ones are Pyo's to turn on and off, as superuser:
//!
//! - Libraries some need loaded at start go in the Cluster's
//!   `shared_preload_libraries`, with their settings; CloudNativePG restarts
//!   the instances (one at a time with high availability, the primary last
//!   by a switchover).
//! - The extension itself is created through CloudNativePG's declarative
//!   `Database` (`<name>-db`, the app database, `databaseReclaimPolicy:
//!   retain`), only once its libraries are loaded. Turning one off sets it
//!   `absent`: CloudNativePG drops it without CASCADE, so Postgres refuses
//!   while other objects depend on it, and that refusal is the status.
//!   Its libraries stay loaded until it's dropped.
//! - Whether a library is loaded is the instances' own word: a custom
//!   monitoring query (`cnpg_pyo_preload_loaded`) reports each one's
//!   `shared_preload_libraries` as it runs.
//!
//! The "additional" ones the database owner creates itself (trusted), so
//! there's nothing here for them.

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::OnceLock,
};

use serde::Deserialize;
use serde_json::{Value, json};

use crate::crd::ExtensionStatus;

#[derive(Deserialize, Clone, Debug, Default)]
#[serde(rename_all = "camelCase")]
pub struct Ext {
    pub name: String,
    pub section: String,
    /// Turning it on or off restarts the database (for the dashboard; here
    /// it's what `libraries` says, checked by the tests).
    #[serde(default)]
    #[cfg_attr(not(test), allow(dead_code))]
    pub restart: bool,
    #[serde(default)]
    pub preload: Vec<String>,
    #[serde(default)]
    pub parameters: BTreeMap<String, String>,
    /// CloudNativePG preloads it itself for the parameters.
    #[serde(default)]
    pub library: String,
    #[serde(default)]
    pub create: bool,
    #[serde(default)]
    pub requires: Vec<String>,
    #[serde(default)]
    pub schema_owner: String,
    #[serde(default)]
    pub fdw: bool,
}

impl Ext {
    /// The libraries that must be loaded for it to be on.
    pub fn libraries(&self) -> Vec<&str> {
        let mut out: Vec<&str> = self.preload.iter().map(String::as_str).collect();
        if !self.library.is_empty() {
            out.push(&self.library);
        }
        out
    }
}

#[derive(Deserialize)]
struct Catalog {
    extensions: Vec<Ext>,
}

pub fn catalog() -> &'static [Ext] {
    static C: OnceLock<Vec<Ext>> = OnceLock::new();
    C.get_or_init(|| {
        serde_json::from_str::<Catalog>(include_str!("../extensions.json"))
            .expect("extensions.json")
            .extensions
    })
}

/// One Pyo turns on and off.
pub fn managed(name: &str) -> Option<&'static Ext> {
    catalog()
        .iter()
        .find(|e| e.name == name && e.section == "managed")
}

/// The CloudNativePG Database through which extensions are made.
pub fn database_object_name(name: &str) -> String {
    format!("{name}-db")
}

/// The libraries an instance has loaded, from its metrics (the
/// `cnpg_pyo_preload_loaded{libraries="a,b"}` line); None when it doesn't
/// report them (yet).
pub fn loaded_libraries(metrics: &str) -> Option<BTreeSet<String>> {
    let line = metrics
        .lines()
        .find(|l| l.starts_with("cnpg_pyo_preload_loaded{"))?;
    let libs = line.split("libraries=\"").nth(1)?.split('"').next()?;
    Some(
        libs.split(',')
            .map(|l| l.trim().trim_matches('"').to_owned())
            .filter(|l| !l.is_empty())
            .collect(),
    )
}

/// The monitoring query behind `cnpg_pyo_preload_loaded`.
pub const PRELOAD_QUERY: &str = r#"pyo_preload:
  query: |
    SELECT current_setting('shared_preload_libraries') AS libraries, 1 AS loaded
  metrics:
    - libraries:
        usage: LABEL
        description: The libraries this instance loaded at start
    - loaded:
        usage: GAUGE
        description: Always 1
"#;

/// What the cluster has now: what the CloudNativePG Database lists and
/// reports, and the libraries every instance has loaded (None: unknown).
#[derive(Default, Debug)]
pub struct Have {
    /// Its `spec.extensions`: name and whether it's wanted present.
    pub listed: Vec<(String, bool)>,
    /// Its `status.extensions`: name, applied, message.
    pub applied: BTreeMap<String, (bool, String)>,
    pub loaded: Option<BTreeSet<String>>,
}

impl Have {
    pub fn from(object: Option<&Value>, loaded: Option<BTreeSet<String>>) -> Self {
        let mut h = Have {
            loaded,
            ..Default::default()
        };
        if let Some(o) = object {
            for e in o["spec"]["extensions"].as_array().into_iter().flatten() {
                h.listed.push((
                    e["name"].as_str().unwrap_or("").to_owned(),
                    e["ensure"].as_str() != Some("absent"),
                ));
            }
            for e in o["status"]["extensions"].as_array().into_iter().flatten() {
                h.applied.insert(
                    e["name"].as_str().unwrap_or("").to_owned(),
                    (
                        e["applied"].as_bool().unwrap_or(false),
                        e["message"].as_str().unwrap_or("").to_owned(),
                    ),
                );
            }
        }
        h
    }

    fn is_loaded(&self, e: &Ext) -> bool {
        self.loaded
            .as_ref()
            .is_some_and(|l| e.libraries().iter().all(|x| l.contains(*x)))
    }

    fn listed_present(&self, name: &str) -> bool {
        self.listed.iter().any(|(n, present)| n == name && *present)
    }

    fn listed_absent(&self, name: &str) -> bool {
        self.listed.iter().any(|(n, present)| n == name && !present)
    }

    /// Listed absent, and CloudNativePG did it.
    fn dropped(&self, name: &str) -> bool {
        self.listed_absent(name) && self.applied.get(name).is_some_and(|(ok, _)| *ok)
    }
}

/// What to write: the Cluster's preload libraries and settings, the
/// CloudNativePG Database's spec (None: nothing to write), and each
/// extension's status.
#[derive(Debug, PartialEq)]
pub struct Plan {
    pub preload: Vec<String>,
    pub parameters: BTreeMap<String, String>,
    pub database: Option<Value>,
    pub status: Vec<ExtensionStatus>,
}

fn fill(v: &str, database: &str, owner: &str) -> String {
    v.replace("{database}", database).replace("{owner}", owner)
}

/// A refusal from Postgres, in plain words.
pub fn plain(message: &str) -> String {
    if message.contains("depend on it") {
        "Other objects in the database depend on it, so it wasn't dropped. Drop or change them first, then turn it off again.".into()
    } else if message.contains("must be loaded via") || message.contains("shared_preload_libraries")
    {
        "It needs its library loaded first, so the database restarts.".into()
    } else {
        message.trim().to_owned()
    }
}

/// From what's wanted (`spec.postgres.extensions`) and what the cluster has.
pub fn plan(wanted: &[String], have: &Have, database: &str, owner: &str) -> Plan {
    let mut status = vec![];
    let wanted_exts: Vec<&Ext> = wanted.iter().filter_map(|w| managed(w)).collect();
    for w in wanted.iter().filter(|w| managed(w).is_none()) {
        status.push(ExtensionStatus {
            name: w.clone(),
            state: "failed".into(),
            message: "Pyo doesn't offer this one.".into(),
        });
    }
    let is_wanted = |n: &str| wanted_exts.iter().any(|e| e.name == n);
    // Being turned off: still created (listed present, or absent and not
    // dropped yet), or still loaded.
    let leaving: Vec<&Ext> = catalog()
        .iter()
        .filter(|e| e.section == "managed" && !is_wanted(&e.name))
        .filter(|e| {
            let created = e.create
                && (have.listed_present(&e.name)
                    || (have.listed_absent(&e.name) && !have.dropped(&e.name)));
            created || (!e.libraries().is_empty() && have.is_loaded(e))
        })
        .collect();

    let mut preload = BTreeSet::new();
    let mut parameters = BTreeMap::new();
    for e in &wanted_exts {
        preload.extend(e.preload.iter().cloned());
        parameters.extend(
            e.parameters
                .iter()
                .map(|(k, v)| (k.clone(), fill(v, database, owner))),
        );
    }
    // A leaving one keeps its library and settings until it's dropped (the
    // ones without an extension go at once).
    for e in &leaving {
        if e.create && !have.dropped(&e.name) {
            preload.extend(e.preload.iter().cloned());
            parameters.extend(
                e.parameters
                    .iter()
                    .map(|(k, v)| (k.clone(), fill(v, database, owner))),
            );
        }
    }

    let mut extensions = vec![];
    let mut schemas = vec![];
    let mut fdws = vec![];
    let mut present = BTreeSet::new();
    for e in &wanted_exts {
        let loaded = e.libraries().is_empty() || have.is_loaded(e);
        if e.create && (loaded || have.listed_present(&e.name)) {
            for r in &e.requires {
                if present.insert(r.clone()) {
                    extensions.push(json!({ "name": r, "ensure": "present" }));
                }
            }
            if present.insert(e.name.clone()) {
                extensions.push(json!({ "name": e.name, "ensure": "present" }));
            }
        }
        let applied = have.applied.get(&e.name);
        let made = applied.is_some_and(|(ok, _)| *ok) && have.listed_present(&e.name);
        if made && !e.schema_owner.is_empty() {
            schemas.push(json!({ "name": e.schema_owner, "owner": owner, "ensure": "present" }));
        }
        if made && e.fdw {
            fdws.push(json!({ "name": e.name, "ensure": "present", "usage": [{ "name": owner, "type": "grant" }] }));
        }
        let (state, message) = if !loaded {
            ("restarting", "Restarts the database to load it.".to_owned())
        } else if !e.create || made {
            ("enabled", String::new())
        } else {
            match applied {
                Some((false, m)) if !m.is_empty() => ("failed", plain(m)),
                _ => ("pending", String::new()),
            }
        };
        status.push(ExtensionStatus {
            name: e.name.clone(),
            state: state.into(),
            message,
        });
    }
    for e in &leaving {
        if e.create && !have.dropped(&e.name) {
            extensions.push(json!({ "name": e.name, "ensure": "absent" }));
        }
        let message = match have.applied.get(&e.name) {
            Some((false, m)) if !m.is_empty() && have.listed_absent(&e.name) => plain(m),
            _ if e.create && !have.dropped(&e.name) => String::new(),
            _ => "Restarts the database to unload it.".into(),
        };
        status.push(ExtensionStatus {
            name: e.name.clone(),
            state: "disabling".into(),
            message,
        });
    }
    let database = (!extensions.is_empty() || !have.listed.is_empty()).then(|| {
        json!({
            "name": database,
            "owner": owner,
            "databaseReclaimPolicy": "retain",
            "extensions": extensions,
            "schemas": schemas,
            "fdws": fdws,
        })
    });
    Plan {
        preload: preload.into_iter().collect(),
        parameters,
        database,
        status,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn have(
        listed: &[(&str, bool)],
        applied: &[(&str, bool, &str)],
        loaded: Option<&[&str]>,
    ) -> Have {
        Have {
            listed: listed.iter().map(|(n, p)| (n.to_string(), *p)).collect(),
            applied: applied
                .iter()
                .map(|(n, a, m)| (n.to_string(), (*a, m.to_string())))
                .collect(),
            loaded: loaded.map(|l| l.iter().map(|s| s.to_string()).collect()),
        }
    }

    #[test]
    fn the_catalog_is_whole() {
        let names: BTreeSet<&str> = catalog().iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names.len(), catalog().len(), "a name twice");
        assert!(
            catalog()
                .iter()
                .all(|e| e.section == "managed" || e.section == "additional")
        );
        for e in catalog().iter().filter(|e| e.section == "managed") {
            assert!(
                e.restart == !e.libraries().is_empty(),
                "{}: restarts exactly when it loads a library",
                e.name
            );
            assert!(
                e.create || !e.parameters.is_empty() || !e.preload.is_empty(),
                "{} does something",
                e.name
            );
        }
        for n in [
            "postgis",
            "pg_squeeze",
            "file_fdw",
            "pg_surgery",
            "pageinspect",
            "plpgsql",
        ] {
            assert!(!names.contains(n), "{n} isn't offered");
        }
        assert!(managed("vector").is_some() && managed("citext").is_none());
    }

    #[test]
    fn a_library_loads_before_the_extension_is_made() {
        let w = vec!["pg_cron".to_owned()];
        // Not loaded yet: preloaded, nothing created, a restart pending.
        let p = plan(&w, &have(&[], &[], Some(&[])), "app", "app");
        assert_eq!(p.preload, ["pg_cron"]);
        assert_eq!(p.parameters["cron.database_name"], "app");
        assert!(p.database.is_none());
        assert_eq!(p.status[0].state, "restarting");
        // Loaded: created, then its schema goes to the owner.
        let p = plan(&w, &have(&[], &[], Some(&["pg_cron"])), "app", "app");
        assert_eq!(
            p.database.as_ref().unwrap()["extensions"],
            json!([{ "name": "pg_cron", "ensure": "present" }])
        );
        assert_eq!(p.status[0].state, "pending");
        let p = plan(
            &w,
            &have(
                &[("pg_cron", true)],
                &[("pg_cron", true, "")],
                Some(&["pg_cron"]),
            ),
            "app",
            "app",
        );
        assert_eq!(
            p.database.as_ref().unwrap()["schemas"],
            json!([{ "name": "cron", "owner": "app", "ensure": "present" }])
        );
        assert_eq!(
            p.database.as_ref().unwrap()["databaseReclaimPolicy"],
            "retain"
        );
        assert_eq!(p.status[0].state, "enabled");
    }

    #[test]
    fn turning_off_drops_first_then_unloads() {
        // Created, now unwanted: absent, library kept.
        let p = plan(
            &[],
            &have(
                &[("timescaledb", true)],
                &[("timescaledb", true, "")],
                Some(&["timescaledb"]),
            ),
            "app",
            "app",
        );
        assert_eq!(
            p.database.as_ref().unwrap()["extensions"],
            json!([{ "name": "timescaledb", "ensure": "absent" }])
        );
        assert_eq!(p.preload, ["timescaledb"]);
        assert_eq!(p.status[0].state, "disabling");
        // Refused: dependent objects; still loaded, said plainly.
        let p = plan(
            &[],
            &have(
                &[("timescaledb", false)],
                &[(
                    "timescaledb",
                    false,
                    "ERROR: cannot drop extension timescaledb because other objects depend on it",
                )],
                Some(&["timescaledb"]),
            ),
            "app",
            "app",
        );
        assert_eq!(p.preload, ["timescaledb"]);
        assert!(
            p.status[0]
                .message
                .starts_with("Other objects in the database depend on it")
        );
        // Dropped: unloaded, gone from the list.
        let p = plan(
            &[],
            &have(
                &[("timescaledb", false)],
                &[("timescaledb", true, "")],
                Some(&["timescaledb"]),
            ),
            "app",
            "app",
        );
        assert!(p.preload.is_empty());
        assert_eq!(p.database.as_ref().unwrap()["extensions"], json!([]));
        assert_eq!(p.status[0].message, "Restarts the database to unload it.");
        // And once it's unloaded, nothing's left to say.
        let p = plan(&[], &have(&[], &[], Some(&[])), "app", "app");
        assert!(p.status.is_empty() && p.database.is_none());
    }

    #[test]
    fn settings_only_and_plain_ones() {
        let p = plan(
            &[
                "pg_stat_statements".into(),
                "vector".into(),
                "earthdistance".into(),
                "nope".into(),
            ],
            &have(&[], &[], Some(&[])),
            "app",
            "owner",
        );
        assert_eq!(p.parameters["pg_stat_statements.track"], "top");
        assert!(
            p.preload.is_empty(),
            "CloudNativePG preloads it for its settings"
        );
        let ext = &p.database.as_ref().unwrap()["extensions"];
        assert_eq!(
            ext,
            &json!([{ "name": "vector", "ensure": "present" }, { "name": "cube", "ensure": "present" }, { "name": "earthdistance", "ensure": "present" }])
        );
        let st: BTreeMap<&str, &str> = p
            .status
            .iter()
            .map(|s| (s.name.as_str(), s.state.as_str()))
            .collect();
        assert_eq!(st["pg_stat_statements"], "restarting");
        assert_eq!(st["vector"], "pending");
        assert_eq!(st["nope"], "failed");
        let p = plan(
            &["pg_partman_bgw".into()],
            &have(&[], &[], Some(&["pg_partman_bgw"])),
            "app",
            "owner",
        );
        assert_eq!(p.parameters["pg_partman_bgw.role"], "owner");
        assert_eq!(p.status[0].state, "enabled");
        let p = plan(
            &["postgres_fdw".into()],
            &have(
                &[("postgres_fdw", true)],
                &[("postgres_fdw", true, "")],
                Some(&[]),
            ),
            "app",
            "owner",
        );
        assert_eq!(
            p.database.as_ref().unwrap()["fdws"][0]["usage"],
            json!([{ "name": "owner", "type": "grant" }])
        );
    }

    #[test]
    fn libraries_from_metrics() {
        let m = "# HELP x\ncnpg_pyo_preload_loaded{libraries=\"pg_cron,timescaledb\"} 1\n";
        assert_eq!(
            loaded_libraries(m).unwrap(),
            BTreeSet::from(["pg_cron".to_owned(), "timescaledb".to_owned()])
        );
        assert_eq!(
            loaded_libraries("cnpg_pyo_preload_loaded{libraries=\"\"} 1\n").unwrap(),
            BTreeSet::new()
        );
        assert!(loaded_libraries("other 1").is_none());
    }
}
