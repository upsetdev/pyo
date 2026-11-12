//! The names of the objects a database makes in its team's namespace
//! (`pyo-db-<team>`), shared by every engine there: pyo-db-controller's own,
//! and those CloudNativePG, MySQL's operator (with the empty name prefix)
//! and the StatefulSets and Deployments make for it. Two databases whose
//! names overlap would fight over the same objects, so the API refuses a
//! name that `conflicts` with another of the team's.
//!
//! A name is a pattern of `-`-separated parts: `#` is one part of digits
//! (an instance's ordinal, a backup's time), and a last `*` is one or more
//! parts of anything. Not listed: names with a dot (MySQL's ConfigMaps),
//! which never meet a database's, and names with a random end (a Job's or
//! a Deployment's pods, ReplicaSets), which Kubernetes picks around those
//! taken. Kinds are ignored: the same name in two kinds is refused too.

/// Names in a databases namespace that aren't any database's.
pub const RESERVED: &[&str] = &[
    // Every namespace's service account.
    "default",
    // The namespace's NetworkPolicy.
    crate::ISOLATION_POLICY,
    // KV backups' Jobs and their pods.
    "pyo-backup-*",
    // CloudNativePG copies its default metrics queries in.
    "cnpg-default-monitoring",
];

/// One CloudNativePG Cluster's objects.
fn cnpg_cluster(c: &str, out: &mut Vec<String>) {
    // The Cluster, its service account, role, binding and disruption budget.
    out.push(c.to_owned());
    for s in [
        "primary",
        "rw",
        "ro",
        "r",
        "app",
        "superuser",
        "ca",
        "server",
        "replication",
    ] {
        out.push(format!("{c}-{s}"));
    }
    // Instances (pods and volumes) and their Jobs.
    out.push(format!("{c}-#"));
    out.push(format!("{c}-#-wal"));
    for job in [
        "initdb",
        "join",
        "pgbasebackup",
        "full-recovery",
        "snapshot-recovery",
        "import",
        "major-upgrade",
    ] {
        out.push(format!("{c}-#-{job}"));
    }
}

/// The names (patterns) a database of `engine` called `name` makes, with
/// every option on (a pooler, read replicas, backups, REST...): the ones it
/// may make later count too.
pub fn derived(engine: &str, name: &str) -> Vec<String> {
    let n = name;
    // Pyo's own: the Database, its login, its backups' credentials.
    let mut out = vec![
        n.to_owned(),
        format!("{n}-credentials"),
        format!("{n}-backup"),
    ];
    match engine {
        "postgres" => {
            cnpg_cluster(n, &mut out);
            cnpg_cluster(&format!("{n}-read"), &mut out);
            out.extend([
                // Poolers: each one's Deployment, Service and Secret too.
                format!("{n}-pooler"),
                format!("{n}-read-pooler"),
                format!("{n}-monitoring"),
                format!("{n}-read-monitoring"),
                // The extensions' Database.
                format!("{n}-db"),
                // ScheduledBackup's Backups; the region Secrets.
                format!("{n}-backup-#"),
                format!("{n}-backup-region-*"),
            ]);
        }
        "mysql" => out.extend([
            // StatefulSet, headless Service, service account, disruption
            // budget, users' Secret; instances; Services.
            format!("{n}-#"),
            format!("{n}-primary"),
            format!("{n}-replica"),
            format!("{n}-grpc"),
            format!("{n}-mysql"),
            format!("my-cnf-{n}"),
            format!("mysql-data-{n}-#"),
            // Backup CronJob, role and binding; its Jobs.
            format!("backup-{n}"),
            format!("backup-{n}-#"),
            format!("backup-{n}-first"),
            // Restore Job, role and binding.
            format!("restore-{n}"),
        ]),
        "valkey" => out.extend([
            format!("{n}-#"),
            format!("{n}-headless"),
            format!("{n}-primary"),
            format!("{n}-replicas"),
            format!("{n}-scripts"),
            format!("{n}-sentinel"),
            format!("{n}-sentinel-#"),
            format!("{n}-read"),
            format!("{n}-read-#"),
            format!("{n}-rest"),
            format!("data-{n}-#"),
            format!("data-{n}-read-#"),
            // pyo-kv's Deployment and disruption budget.
            format!("{n}-kv"),
        ]),
        _ => {}
    }
    out
}

fn digits(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())
}

fn part_meets(a: &str, b: &str) -> bool {
    a == b || (a == "#" && (b == "#" || digits(b))) || (b == "#" && digits(a))
}

fn parts_meet(a: &[&str], b: &[&str]) -> bool {
    match (a, b) {
        (["*"], rest) | (rest, ["*"]) => !rest.is_empty(),
        ([], []) => true,
        ([x, a @ ..], [y, b @ ..]) => part_meets(x, y) && parts_meet(a, b),
        _ => false,
    }
}

/// Whether two patterns can be the same name.
pub fn overlap(a: &str, b: &str) -> bool {
    parts_meet(
        &a.split('-').collect::<Vec<_>>(),
        &b.split('-').collect::<Vec<_>>(),
    )
}

fn meet(a: &[String], b: &[impl AsRef<str>]) -> bool {
    a.iter().any(|x| b.iter().any(|y| overlap(x, y.as_ref())))
}

/// Why a new database can't be called `name`: `Some("")` when the name is
/// reserved, `Some(other)` when it overlaps the database `other` (one of
/// `existing`: each one's engine and name), None when it's free.
pub fn conflicts<'a>(
    engine: &str,
    name: &str,
    existing: impl IntoIterator<Item = (&'a str, &'a str)>,
) -> Option<&'a str> {
    let mine = derived(engine, name);
    if meet(&mine, RESERVED) {
        return Some("");
    }
    existing
        .into_iter()
        .find(|(e, n)| meet(&mine, &derived(e, n)))
        .map(|(_, n)| n)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clash(engine: &str, name: &str, other_engine: &str, other: &str) -> bool {
        conflicts(engine, name, [(other_engine, other)]).is_some()
    }

    #[test]
    fn patterns() {
        assert!(overlap("shop-#", "shop-0"));
        assert!(overlap("shop-0", "shop-#"));
        assert!(overlap("shop-#", "shop-#"));
        assert!(!overlap("shop-#", "shop-a"));
        assert!(!overlap("shop-#", "shop"));
        assert!(overlap("shop-kv-*", "shop-kv-7d9f-abcde"));
        assert!(overlap("shop-kv-*", "shop-kv-#"));
        assert!(!overlap("shop-kv-*", "shop-kv"));
        assert!(!overlap("shop", "shops"));
    }

    #[test]
    fn suffixes_of_another_database() {
        for (engine, other) in [
            ("mysql", "shop-primary"),
            ("mysql", "shop-grpc"),
            ("mysql", "shop-replica"),
            ("mysql", "shop-0"),
            ("mysql", "backup-shop"),
            ("postgres", "shop-rw"),
            ("postgres", "shop-ro"),
            ("postgres", "shop-r"),
            ("postgres", "shop-read"),
            ("postgres", "shop-pooler"),
            ("postgres", "shop-read-pooler"),
            ("postgres", "shop-1"),
            ("valkey", "shop-kv"),
            ("valkey", "shop-sentinel"),
            ("valkey", "shop-primary"),
            ("valkey", "shop-rest"),
            ("valkey", "shop-read"),
        ] {
            // Whatever the new one's engine, and either way round.
            for new in ["postgres", "mysql", "valkey"] {
                assert!(
                    clash(new, other, engine, "shop"),
                    "{new} {other} beside {engine} shop"
                );
                assert!(
                    clash(engine, "shop", new, other),
                    "{engine} shop beside {new} {other}"
                );
            }
        }
    }

    #[test]
    fn across_engines() {
        // KV shop's shop-primary is MySQL shop's too.
        assert!(clash("valkey", "shop", "mysql", "shop"));
        // Postgres shop's read cluster is a KV shop-read's StatefulSet.
        assert!(clash("postgres", "shop", "valkey", "shop-read"));
        // A Postgres shop-read would be Postgres shop's read cluster.
        assert!(clash("postgres", "shop-read", "postgres", "shop"));
    }

    #[test]
    fn names_that_dont_meet() {
        for (a, b) in [
            ("shop", "shops"),
            ("shop", "shop2"),
            ("shop", "my-shop"),
            ("shop", "shop-orders"),
            ("shop-a", "shop-b"),
            ("orders", "users"),
        ] {
            for x in ["postgres", "mysql", "valkey"] {
                for y in ["postgres", "mysql", "valkey"] {
                    assert!(!clash(x, a, y, b), "{x} {a} beside {y} {b}");
                    assert!(!clash(y, b, x, a), "{y} {b} beside {x} {a}");
                }
            }
        }
    }

    #[test]
    fn reserved() {
        for (engine, name) in [
            ("mysql", "default"),
            ("valkey", "pyo-isolation"),
            ("postgres", "pyo-backup"),
            ("postgres", "cnpg-default"),
        ] {
            assert_eq!(conflicts(engine, name, []), Some(""), "{engine} {name}");
        }
        assert_eq!(
            conflicts(
                "postgres",
                "shop",
                [("valkey", "cache"), ("mysql", "shop-primary")]
            ),
            Some("shop-primary")
        );
        assert_eq!(conflicts("postgres", "shop", [("valkey", "cache")]), None);
    }
}
