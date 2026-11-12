//! Valkey modules a Database loads (`spec.valkey.modules`), from the "kv"
//! catalog in `extensions.json` (the API and the dashboard read the same
//! file). They come with the valkey-bundle image, so an instance runs it
//! while it loads any, and `valkey.sh` writes a `loadmodule` line for each
//! (`MODULES`) on every instance: the primary, its replicas and the read
//! replicas. Sentinels load none.
//!
//! Changing them changes the instances' pod template, which restarts them in
//! order (`Kube::roll` in valkey.rs). Whether a module is on is each
//! instance's own `MODULE LIST`.
//!
//! Valkey refuses to start from data that holds a module's keys (or, for
//! Search, its indexes) without that module, so one being turned off stays
//! loaded while the primary's INFO counts any (`dataInfo`). Once it counts
//! none, it's unloaded, and each instance rewrites its append-only file
//! before it restarts, so the module's old commands aren't replayed.

use std::{collections::BTreeSet, sync::OnceLock};

use serde::Deserialize;

use crate::crd::ExtensionStatus;

#[derive(Deserialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct Module {
    pub name: String,
    /// The file `loadmodule` loads.
    pub module: String,
    /// Its name in `MODULE LIST`.
    pub loaded: String,
    /// The INFO field counting what it holds, and what that is.
    pub data_info: String,
    pub data: String,
}

pub fn catalog() -> &'static [Module] {
    #[derive(Deserialize)]
    struct Catalog {
        kv: Vec<Module>,
    }
    static C: OnceLock<Vec<Module>> = OnceLock::new();
    C.get_or_init(|| {
        serde_json::from_str::<Catalog>(include_str!("../extensions.json"))
            .expect("extensions.json")
            .kv
    })
}

fn find(name: &str) -> Option<&'static Module> {
    catalog().iter().find(|m| m.name == name)
}

/// The files to load, in the catalog's order (JSON before Search, which
/// indexes JSON documents when JSON is loaded first).
pub fn files(load: &[String]) -> Vec<&'static str> {
    catalog()
        .iter()
        .filter(|m| load.contains(&m.name))
        .map(|m| m.module.as_str())
        .collect()
}

/// The module names in a `MODULE LIST` reply.
pub fn loaded(reply: &pyo_resp::Value) -> BTreeSet<String> {
    let pyo_resp::Value::Array(mods) = reply else {
        return BTreeSet::new();
    };
    mods.iter()
        .filter_map(|m| {
            let pyo_resp::Value::Array(kv) = m else {
                return None;
            };
            kv.chunks(2)
                .find(|p| p.len() == 2 && p[0].text() == "name")
                .map(|p| p[1].text())
        })
        .collect()
}

/// What the instances have now.
#[derive(Default)]
pub struct Have {
    /// What each instance that answered has loaded.
    pub members: Vec<BTreeSet<String>>,
    /// How many instances there are (one that didn't answer may be
    /// restarting: a module isn't on until every one has it).
    pub instances: usize,
    /// The primary's `INFO` (None: it didn't answer).
    pub primary_info: Option<String>,
}

/// What to load (catalog names) and each module's state.
#[derive(Debug, PartialEq)]
pub struct Plan {
    pub load: Vec<String>,
    pub status: Vec<ExtensionStatus>,
}

const UNLOAD: &str = "Restarts the database to unload it.";

impl Plan {
    /// Whether a module is on its way out (instances rewrite their
    /// append-only file before they restart).
    pub fn unloading(&self) -> bool {
        self.status
            .iter()
            .any(|s| s.state == "disabling" && s.message == UNLOAD)
    }
}

/// From what's wanted, whether this version has modules at all, what the
/// instances have, and the status before (a module already being unloaded
/// stays so while the primary can't be asked, e.g. while it restarts).
pub fn plan(wanted: &[String], available: bool, have: &Have, before: &[ExtensionStatus]) -> Plan {
    // On every instance; one that didn't answer counts only for a module
    // that wasn't on before (it may be restarting without it).
    let was_on = |m: &Module| {
        before
            .iter()
            .any(|s| s.name == m.name && s.state == "enabled")
    };
    let on_all = |m: &Module| {
        !have.members.is_empty()
            && (have.members.len() >= have.instances || was_on(m))
            && have.members.iter().all(|l| l.contains(&m.loaded))
    };
    let on_any = |m: &Module| have.members.iter().any(|l| l.contains(&m.loaded));
    let st = |name: &str, state: &str, message: String| ExtensionStatus {
        name: name.into(),
        state: state.into(),
        message,
    };
    let mut load = vec![];
    let mut status = vec![];
    for w in wanted {
        status.push(match find(w) {
            None => st(w, "failed", "Pyo doesn't offer this one.".into()),
            Some(_) if !available => st(w, "failed", "Modules need Valkey 8.1.".into()),
            Some(m) => {
                load.push(m.name.clone());
                if on_all(m) {
                    st(w, "enabled", String::new())
                } else {
                    st(w, "restarting", "Restarts the database to load it.".into())
                }
            }
        });
    }
    // Being turned off: still loaded somewhere, or was being turned off and
    // not every instance answered.
    let was_leaving = |m: &Module| {
        have.members.len() < have.instances
            && before
                .iter()
                .any(|s| s.name == m.name && s.state == "disabling")
    };
    for m in catalog()
        .iter()
        .filter(|m| !wanted.contains(&m.name) && (on_any(m) || was_leaving(m)))
    {
        // How many it holds: none when the primary doesn't report it (it
        // isn't loaded there).
        let held = have.primary_info.as_deref().map(|i| {
            i.lines()
                .find_map(|l| {
                    l.trim_end()
                        .strip_prefix(m.data_info.as_str())?
                        .strip_prefix(':')?
                        .parse::<u64>()
                        .ok()
                })
                .unwrap_or(0)
        });
        let was = before
            .iter()
            .find(|s| s.name == m.name && s.state == "disabling");
        let message = match held {
            Some(0) => UNLOAD.to_owned(),
            Some(n) => format!(
                "Delete its {} first (the database has {n}), then it turns off.",
                m.data
            ),
            None => was.map(|s| s.message.clone()).unwrap_or_default(),
        };
        if message != UNLOAD {
            load.push(m.name.clone());
        }
        status.push(st(&m.name, "disabling", message));
    }
    load.sort_by_key(|n| catalog().iter().position(|m| &m.name == n));
    Plan { load, status }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(xs: &[&str]) -> BTreeSet<String> {
        xs.iter().map(|s| s.to_string()).collect()
    }

    fn have(members: &[&[&str]], info: Option<&str>) -> Have {
        Have {
            members: members.iter().map(|m| set(m)).collect(),
            instances: members.len(),
            primary_info: info.map(str::to_owned),
        }
    }

    #[test]
    fn the_catalog() {
        let names: Vec<&str> = catalog().iter().map(|m| m.name.as_str()).collect();
        assert_eq!(names, ["valkey-json", "valkey-bloom", "valkey-search"]);
        assert_eq!(
            files(&["valkey-search".into(), "valkey-json".into()]),
            ["/usr/lib/valkey/libjson.so", "/usr/lib/valkey/libsearch.so"]
        );
        assert!(files(&["nope".into()]).is_empty());
    }

    #[test]
    fn turning_on() {
        let w = vec!["valkey-bloom".to_owned(), "valkey-json".to_owned()];
        let p = plan(
            &w,
            true,
            &have(&[&["json", "bf"], &["json"]], Some("")),
            &[],
        );
        assert_eq!(p.load, ["valkey-json", "valkey-bloom"]);
        assert_eq!(
            (p.status[0].state.as_str(), p.status[1].state.as_str()),
            ("restarting", "enabled")
        );
        // Nobody answered, or one didn't: not on yet.
        assert_eq!(
            plan(&w, true, &have(&[], None), &[]).status[0].state,
            "restarting"
        );
        let some = Have {
            instances: 3,
            ..have(&[&["json", "bf"], &["json", "bf"]], None)
        };
        assert_eq!(plan(&w, true, &some, &[]).status[1].state, "restarting");
        let on = plan(&w, true, &have(&[&["json", "bf"]], None), &[]).status;
        assert_eq!(
            plan(&w, true, &some, &on).status[1].state,
            "enabled",
            "already on: stays so while one restarts"
        );
        let p = plan(&w, false, &have(&[], None), &[]);
        assert!(p.load.is_empty() && p.status[0].state == "failed");
        assert_eq!(
            plan(&["x".into()], true, &have(&[], None), &[]).status[0].message,
            "Pyo doesn't offer this one."
        );
        assert!(plan(&[], true, &have(&[&[]], None), &[]).status.is_empty());
    }

    #[test]
    fn turning_off_waits_for_its_data() {
        // Search has an index: it stays loaded, and says why.
        let p = plan(
            &[],
            true,
            &have(
                &[&["search"]],
                Some("# search\r\nsearch_number_of_indexes:2\r\n"),
            ),
            &[],
        );
        assert_eq!(p.load, ["valkey-search"]);
        assert_eq!(p.status[0].state, "disabling");
        assert_eq!(
            p.status[0].message,
            "Delete its Search indexes first (the database has 2), then it turns off."
        );
        assert!(!p.unloading());
        // None left: unloaded.
        let p = plan(
            &[],
            true,
            &have(&[&["search"]], Some("search_number_of_indexes:0\r\n")),
            &[],
        );
        assert!(p.load.is_empty() && p.unloading());
        // The primary can't be asked (restarting): as before.
        let p2 = plan(&[], true, &have(&[&["search"]], None), &p.status);
        assert!(p2.load.is_empty() && p2.unloading());
        let p3 = plan(&[], true, &have(&[&["search"]], None), &[]);
        assert_eq!(p3.load, ["valkey-search"], "unknown: kept");
        // Only some answered, none with it: still on its way out.
        let few = Have {
            instances: 3,
            ..have(&[&[]], None)
        };
        assert!(plan(&[], true, &few, &p.status).unloading());
        assert!(
            plan(&[], true, &have(&[&[]], None), &p.status)
                .status
                .is_empty(),
            "gone everywhere"
        );
        // The primary no longer has it loaded: nothing to count.
        assert!(
            plan(
                &[],
                true,
                &have(&[&["search"], &[]], Some("role:master\r\n")),
                &[]
            )
            .load
            .is_empty()
        );
    }

    #[test]
    fn module_list() {
        use pyo_resp::Value::{Array, Bulk, Int};
        let b = |s: &str| Bulk(s.as_bytes().to_vec());
        let r = Array(vec![
            Array(vec![
                b("name"),
                b("json"),
                b("ver"),
                Int(10003),
                b("path"),
                b("/usr/lib/valkey/libjson.so"),
                b("args"),
                Array(vec![]),
            ]),
            Array(vec![b("name"), b("bf"), b("ver"), Int(10001)]),
        ]);
        assert_eq!(loaded(&r), set(&["json", "bf"]));
        assert!(loaded(&b("ERR")).is_empty());
    }
}
