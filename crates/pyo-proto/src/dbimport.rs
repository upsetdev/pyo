//! Importing into a built-in Postgres or MySQL database: what the API asks
//! the agent to check before an import (`CheckDatabaseImport`), and the
//! words the API, the agent and pyo-db-controller share.
//!
//! The import itself is the Database's `spec.import` (pyo-db-controller's
//! import.rs): a CloudNativePG import for a new Postgres database copied
//! from a URL, otherwise a Job in the database's namespace.

use serde::{Deserialize, Serialize};

use crate::EnvVar;

/// The label (and value) on an import's pods, the check's and the copy's:
/// an isolated project lets these in from its team's databases namespace,
/// so a source inside a project can be copied. The databases' own pods
/// don't have it.
pub const POD_LABEL: (&str, &str) = ("pyo.dev/component", "import");
/// CloudNativePG's label on its import Job's pods (a new Postgres database
/// copied from a URL), let in the same way.
pub const CNPG_POD_LABEL: (&str, &str) = ("cnpg.io/jobRole", "import");

/// What to check before an import, in the team's databases namespace.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ImportCheck {
    pub namespace: String,
    /// "postgres" or "mysql".
    pub engine: String,
    /// The database to copy from; None for a dump file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<Connection>,
    /// The database to import into, when it exists already: it must be
    /// empty. Its host and the Secret with its `username` and `password`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<Target>,
    /// The source's password, as `password`. Called env so it's sealed at
    /// rest in the command queue.
    #[serde(default)]
    pub env: Vec<EnvVar>,
}

/// A database to connect to, without its password.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Connection {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub database: String,
    /// One of [`TLS_MODES`]; empty: "prefer".
    #[serde(default)]
    pub tls: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Target {
    pub host: String,
    pub secret: String,
}

/// TLS to the source, as libpq's sslmode names it (MySQL's ssl-mode maps
/// onto it: DISABLED, PREFERRED, REQUIRED, VERIFY_CA, VERIFY_IDENTITY).
pub const TLS_MODES: [&str; 5] = ["disable", "prefer", "require", "verify-ca", "verify-full"];

/// The ssl-mode MySQL's clients take for one of [`TLS_MODES`].
pub fn mysql_ssl_mode(tls: &str) -> &'static str {
    match tls {
        "disable" => "DISABLED",
        "require" => "REQUIRED",
        "verify-ca" => "VERIFY_CA",
        "verify-full" => "VERIFY_IDENTITY",
        _ => "PREFERRED",
    }
}

/// A libpq connection string value, quoted.
pub fn conninfo_value(v: &str) -> String {
    format!("'{}'", v.replace('\\', "\\\\").replace('\'', "\\'"))
}

/// The libpq connection string for a source, without its password (that
/// comes from PGPASSWORD). Verifying a certificate uses the system's CAs.
pub fn pg_conninfo(c: &Connection) -> String {
    let tls = if c.tls.is_empty() {
        "prefer"
    } else {
        c.tls.as_str()
    };
    let mut s = format!(
        "host={} port={} user={} dbname={} sslmode={} connect_timeout=15 application_name=pyo-import",
        conninfo_value(&c.host),
        c.port,
        conninfo_value(&c.user),
        conninfo_value(&c.database),
        conninfo_value(tls)
    );
    if tls.starts_with("verify") {
        s.push_str(" sslrootcert=system");
    }
    s
}

/// SQL counting what a Postgres database has of its own: tables, views,
/// sequences and functions outside the system's schemas and not an
/// extension's. 0: empty.
pub const PG_OBJECTS_SQL: &str = "SELECT (SELECT count(*) FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
 WHERE n.nspname NOT IN ('pg_catalog', 'information_schema') AND n.nspname NOT LIKE 'pg\\_toast%' AND n.nspname NOT LIKE 'pg\\_temp%' \
 AND c.relkind IN ('r', 'p', 'v', 'm', 'S', 'f') \
 AND NOT EXISTS (SELECT 1 FROM pg_depend d WHERE d.classid = 'pg_class'::regclass AND d.objid = c.oid AND d.deptype = 'e')) \
 + (SELECT count(*) FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace \
 WHERE n.nspname NOT IN ('pg_catalog', 'information_schema') \
 AND NOT EXISTS (SELECT 1 FROM pg_depend d WHERE d.classid = 'pg_proc'::regclass AND d.objid = p.oid AND d.deptype = 'e'))";

/// SQL counting the tables, views, routines and events in MySQL's `app`.
pub const MYSQL_OBJECTS_SQL: &str = "SELECT (SELECT COUNT(*) FROM information_schema.TABLES WHERE TABLE_SCHEMA = 'app') \
 + (SELECT COUNT(*) FROM information_schema.ROUTINES WHERE ROUTINE_SCHEMA = 'app') \
 + (SELECT COUNT(*) FROM information_schema.EVENTS WHERE EVENT_SCHEMA = 'app')";

/// Why a connection failed, in plain words: (kind, message). Kinds:
/// "unreachable", "auth", "database", "tls", "privileges", "other".
pub fn explain_connect_error(
    engine: &str,
    host: &str,
    port: u16,
    database: &str,
    out: &str,
) -> (&'static str, String) {
    let out = strip_warnings(out);
    let out = out.as_str();
    let o = out.to_ascii_lowercase();
    let has = |words: &[&str]| words.iter().any(|w| o.contains(w));
    let at = format!("{host}:{port}");
    if has(&[
        "could not translate host name",
        "name or service not known",
        "unknown mysql server host",
        "temporary failure in name resolution",
        "nodename nor servname",
    ]) {
        return (
            "unreachable",
            format!("Can't find {host}: the cluster can't resolve that name."),
        );
    }
    if has(&["ssl", "tls", "certificate", "no encryption", "encryption"])
        && !has(&["password authentication failed", "access denied"])
    {
        return (
            "tls",
            format!(
                "TLS to {at} failed: {}. Check the URL's sslmode (or ssl-mode).",
                last_line(out)
            ),
        );
    }
    if has(&[
        "password authentication failed",
        "access denied",
        "authentication failed",
        "no password supplied",
        "role \"",
        "plugin caching_sha2_password could not be loaded",
    ]) {
        return (
            "auth",
            format!("{at} refused the user and password. Check them in the URL."),
        );
    }
    if has(&["pg_hba.conf"]) {
        return (
            "auth",
            format!(
                "{at} doesn't accept connections from the cluster's address (its pg_hba.conf, or the provider's allowed addresses)."
            ),
        );
    }
    if (engine == "postgres" && o.contains("database \"") && o.contains("does not exist"))
        || o.contains("unknown database")
    {
        return ("database", format!("{at} has no database {database}."));
    }
    if has(&[
        "connection refused",
        "timeout expired",
        "timed out",
        "no route to host",
        "network is unreachable",
        "can't connect to mysql server",
        "could not connect",
        "connection reset",
        "lost connection",
    ]) {
        if let Some((service, project)) = in_cluster(host) {
            return (
                "unreachable",
                format!(
                    "Can't reach {service} in project {project}. Check it's running and listening on port {port}."
                ),
            );
        }
        if is_service_ip(host) {
            return (
                "unreachable",
                format!(
                    "Can't reach {at}, a service address in the cluster. Check it's running and listening on port {port}."
                ),
            );
        }
        return (
            "unreachable",
            format!(
                "Can't reach {at} from the cluster. The source must accept connections from the cluster's servers."
            ),
        );
    }
    ("other", last_line(out))
}

/// A service in the cluster, by its DNS name (`<service>.<namespace>.svc`,
/// or with `.cluster.local`): (service, namespace, i.e. its project).
fn in_cluster(host: &str) -> Option<(&str, &str)> {
    let host = host.trim_end_matches('.');
    let rest = host
        .strip_suffix(".svc.cluster.local")
        .or_else(|| host.strip_suffix(".svc"))?;
    let (service, ns) = rest.split_once('.')?;
    (!service.is_empty() && !ns.is_empty() && !ns.contains('.')).then_some((service, ns))
}

/// An address in k3s's default range for services (10.43.0.0/16).
fn is_service_ip(host: &str) -> bool {
    host.parse::<std::net::Ipv4Addr>()
        .is_ok_and(|ip| ip.octets()[..2] == [10, 43])
}

/// Without the clients' warnings ("WARNING: no verification of server
/// certificate will be done...", "mysql: [Warning] ..."): they aren't why
/// it failed.
fn strip_warnings(out: &str) -> String {
    let mut s = out.to_owned();
    for w in [
        "WARNING: no verification of server certificate will be done. Use --ssl-mode=VERIFY_CA or VERIFY_IDENTITY.",
        "mysql: [Warning] Using a password on the command line interface can be insecure.",
    ] {
        s = s.replace(w, "");
    }
    s.lines()
        .filter(|l| !l.contains("[Warning]"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn last_line(out: &str) -> String {
    let line = out
        .lines()
        .map(str::trim)
        .rfind(|l| !l.is_empty())
        .unwrap_or("it failed");
    let line = line.strip_prefix("psql: ").unwrap_or(line);
    let line = line.strip_prefix("error: ").unwrap_or(line);
    line.chars().take(400).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connection_strings() {
        let c = Connection {
            host: "db.example.com".into(),
            port: 5432,
            user: "o'neil".into(),
            database: "shop".into(),
            tls: "verify-full".into(),
        };
        let s = pg_conninfo(&c);
        assert!(
            s.contains("user='o\\'neil'")
                && s.contains("sslmode='verify-full'")
                && s.ends_with("sslrootcert=system"),
            "{s}"
        );
        assert!(
            !pg_conninfo(&Connection {
                tls: String::new(),
                ..c
            })
            .contains("sslrootcert")
        );
        assert_eq!(mysql_ssl_mode("require"), "REQUIRED");
        assert_eq!(mysql_ssl_mode(""), "PREFERRED");
    }

    #[test]
    fn errors_in_plain_words() {
        let e = |engine: &str, out: &str| {
            explain_connect_error(engine, "db.example.com", 5432, "shop", out).0
        };
        assert_eq!(
            e(
                "postgres",
                "psql: error: connection to server at \"db.example.com\" (1.2.3.4), port 5432 failed: FATAL:  password authentication failed for user \"app\""
            ),
            "auth"
        );
        assert_eq!(
            e(
                "postgres",
                "psql: error: could not translate host name \"nope\" to address: Name or service not known"
            ),
            "unreachable"
        );
        assert_eq!(
            e(
                "postgres",
                "psql: error: connection to server at \"x\" failed: Connection refused"
            ),
            "unreachable"
        );
        assert_eq!(
            e(
                "postgres",
                "psql: error: connection to server failed: timeout expired"
            ),
            "unreachable"
        );
        assert_eq!(
            e(
                "postgres",
                "FATAL:  no pg_hba.conf entry for host \"10.0.0.1\", user \"app\", database \"shop\", no encryption"
            ),
            "tls"
        );
        assert_eq!(
            e(
                "postgres",
                "psql: error: server does not support SSL, but SSL was required"
            ),
            "tls"
        );
        assert_eq!(
            e("postgres", "FATAL:  database \"shop\" does not exist"),
            "database"
        );
        assert_eq!(
            e(
                "mysql",
                "ERROR 1045 (28000): Access denied for user 'app'@'10.42.0.5' (using password: YES)"
            ),
            "auth"
        );
        assert_eq!(
            e(
                "mysql",
                "ERROR 2005 (HY000): Unknown MySQL server host 'nope' (-2)"
            ),
            "unreachable"
        );
        assert_eq!(
            e(
                "mysql",
                "ERROR 2003 (HY000): Can't connect to MySQL server on 'db:3306' (111)"
            ),
            "unreachable"
        );
        assert_eq!(
            e("mysql", "ERROR 1049 (42000): Unknown database 'shop'"),
            "database"
        );
        assert_eq!(
            e(
                "mysql",
                "ERROR 2026 (HY000): SSL connection error: error:0A000086:SSL routines::certificate verify failed"
            ),
            "tls"
        );
        assert_eq!(
            explain_connect_error("mysql", "h", 1, "d", "psql: error: weird\n").1,
            "weird"
        );
        // A source inside a project: said so.
        let refused = "psql: error: connection to server failed: Connection refused";
        assert_eq!(
            explain_connect_error(
                "postgres",
                "srcpg.shop.svc.cluster.local",
                5432,
                "app",
                refused
            )
            .1,
            "Can't reach srcpg in project shop. Check it's running and listening on port 5432."
        );
        assert_eq!(
            explain_connect_error("postgres", "srcpg.shop.svc", 5432, "app", refused).1,
            "Can't reach srcpg in project shop. Check it's running and listening on port 5432."
        );
        assert!(
            explain_connect_error("postgres", "10.43.7.9", 5432, "app", refused)
                .1
                .contains("a service address in the cluster")
        );
        assert!(
            explain_connect_error("postgres", "db.svc.example.com", 5432, "app", refused)
                .1
                .contains("from the cluster's servers")
        );
        assert!(
            explain_connect_error("postgres", "db.example.com", 5432, "app", refused)
                .1
                .contains("from the cluster's servers")
        );
        // A client's warning about certificates isn't a TLS failure.
        assert_eq!(
            e(
                "mysql",
                "WARNING: no verification of server certificate will be done. Use --ssl-mode=VERIFY_CA or VERIFY_IDENTITY. ERROR 2003 (HY000): Can't connect to MySQL server on 'db:3306' (111)"
            ),
            "unreachable"
        );
    }
}
