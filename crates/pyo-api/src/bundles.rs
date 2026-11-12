//! System bundles on a cluster: the WAF and the databases system, Pyo's own
//! parts that run per cluster. One installer for both: the cluster's agent
//! applies the bundle (pyo-proto's bundles.rs; pyo-agent's bundles.rs), and
//! its state lives in `pyo.cluster_bundles`, one row per installed bundle.
//!
//! - The WAF: installed when the first service on the cluster turns its
//!   WAF on, removed when the last one turns it off or is deleted
//!   (`sync_waf`, from firewall.rs's `sync`), with the installation's
//!   images, and updated when they change.
//! - Databases: set up on a cluster when a team makes its first database
//!   there, and updated with Pyo (`sync_databases`, every heartbeat). Never
//!   removed.

use std::collections::{HashMap, HashSet};

use pyo_proto::{
    Command,
    bundles::{
        BundleReport, DATABASES_ID, ObjectRef, POLICY_REFUSED, ReadyCheck, SystemBundle, WAF_ID,
        never_pruned,
    },
};
use serde_json::{Value, json};
use sqlx::{PgPool, Postgres, Transaction};

use crate::{deploy, error::ApiResult, ops::log_activity};

/// The WAF's bundle, with this installation's images.
fn waf_bundle() -> SystemBundle {
    pyo_proto::bundles::waf_bundle(
        &pyo_proto::system::waf_image_from_env(),
        &pyo_proto::system::geoip_image_from_env(),
    )
}

/// The databases system's bundle, with this installation's images.
fn databases_bundle() -> SystemBundle {
    pyo_proto::bundles::databases_bundle(&pyo_proto::system::db_images_from_env())
}

fn known(id: &str) -> Option<SystemBundle> {
    match id {
        WAF_ID => Some(waf_bundle()),
        DATABASES_ID => Some(databases_bundle()),
        _ => None,
    }
}

/// Its ready checks, each with its namespace.
fn ready_checks(b: &SystemBundle) -> Vec<ReadyCheck> {
    b.ready
        .iter()
        .map(|r| ReadyCheck {
            namespace: Some(r.namespace.clone().unwrap_or_else(|| b.namespace.clone())),
            ..r.clone()
        })
        .collect()
}

/// A row's state as shown: an applied bundle is "ready" once every ready
/// check's workload is healthy (from the agent's last heartbeat).
fn shown_state(state: &str, message: &str, ready: &Value, workloads: &Value) -> (String, String) {
    if state != "applied" {
        return (state.to_owned(), message.to_owned());
    }
    let checks: Vec<ReadyCheck> = serde_json::from_value(ready.clone()).unwrap_or_default();
    let ws = workloads.as_array().cloned().unwrap_or_default();
    for c in &checks {
        let w = ws.iter().find(|w| {
            w["kind"] == json!(c.kind)
                && w["name"] == json!(c.name)
                && w["namespace"] == json!(c.namespace.clone().unwrap_or_default())
        });
        // MySQL's operator goes by what it is, not its project's name.
        let what = if c.name == "moco-controller" {
            "the MySQL engine"
        } else {
            c.name.as_str()
        };
        match w {
            Some(w) if w["status"] == "healthy" => {}
            Some(_) => return ("installing".into(), format!("Starting {what}.")),
            None => return ("installing".into(), format!("Waiting for {what}.")),
        }
    }
    ("ready".into(), String::new())
}

/// Whether a bundle is installed and ready on a cluster.
pub async fn ready_on(
    tx: &mut Transaction<'_, Postgres>,
    cluster_id: &str,
    bundle: &str,
) -> ApiResult<bool> {
    let row: Option<(String, String, Value, Value)> = sqlx::query_as(
        "SELECT a.state, a.message, a.ready, c.workloads FROM pyo.cluster_bundles a JOIN pyo.clusters c ON c.id = a.cluster_id WHERE a.cluster_id = $1 AND a.bundle = $2",
    )
    .bind(cluster_id)
    .bind(bundle)
    .fetch_optional(&mut **tx)
    .await?;
    Ok(row.is_some_and(|(state, message, ready, workloads)| {
        shown_state(&state, &message, &ready, &workloads).0 == "ready"
    }))
}

/// Where the WAF's engine is on each of these clusters: "installing",
/// "ready", "failed" (with a message) or "removing". None where it isn't
/// installed.
pub async fn waf_state(
    db: &PgPool,
    cluster_ids: &[String],
) -> ApiResult<HashMap<String, (String, String)>> {
    let rows: Vec<(String, String, String, Value, Value)> = sqlx::query_as(
        "SELECT a.cluster_id, a.state, a.message, a.ready, c.workloads
           FROM pyo.cluster_bundles a JOIN pyo.clusters c ON c.id = a.cluster_id WHERE a.bundle = $1 AND a.cluster_id = ANY($2)",
    )
    .bind(WAF_ID)
    .bind(cluster_ids)
    .fetch_all(db)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(c, state, message, ready, workloads)| {
            let mut shown = shown_state(&state, &message, &ready, &workloads);
            // The dashboard says "Starting" while its processor starts.
            if state == "applied" && shown.0 == "installing" {
                shown.1.clear();
            }
            (c, shown)
        })
        .collect())
}

/// An install sent while the bundle's removal still runs: the agent runs
/// it once the removal is done.
const WAITING_FOR_REMOVAL: &str = "Waiting for removal";

/// A service's WAF as shown, for a service with it on: where the engine is
/// on its cluster (none yet: about to be installed), and not "ready" while
/// the gateway refuses the service's policy (the agent's heartbeat names
/// those services): its site passes unfiltered; nor while its policy isn't
/// in effect yet (`starting`: a firewall sync on its way to the cluster,
/// and the gateway hasn't accepted the service's policy).
pub fn waf_shown(
    engine: Option<&(String, String)>,
    rejected: bool,
    starting: bool,
) -> (String, String) {
    match engine {
        None => ("installing".into(), String::new()),
        // Turned on again while it's being removed: installed after.
        Some((st, _)) if st == "removing" => ("installing".into(), WAITING_FOR_REMOVAL.into()),
        Some((st, _)) if st == "ready" && rejected => (
            "failed".into(),
            "The gateway refused its policy, so requests aren't checked.".into(),
        ),
        Some((st, _)) if st == "ready" && starting => ("installing".into(), String::new()),
        Some(shown) => shown.clone(),
    }
}

/// Clusters with a firewall sync not done yet (sent or waiting): their
/// services' routes and WAF policies are about to change.
pub async fn syncing_firewall(
    db: &PgPool,
    cluster_ids: &[String],
) -> ApiResult<std::collections::HashSet<String>> {
    let rows: Vec<String> = sqlx::query_scalar("SELECT DISTINCT cluster_id FROM pyo.commands WHERE cluster_id = ANY($1) AND status IN ('pending', 'sent') AND command->>'kind' = 'sync_firewall'")
        .bind(cluster_ids)
        .fetch_all(db)
        .await?;
    Ok(rows.into_iter().collect())
}

type Installed = (String, String, Value, i32, bool);

/// The cluster's row for a bundle: version, state, objects, in use, and
/// whether a command for it has been out for over ten minutes.
async fn installed(
    tx: &mut Transaction<'_, Postgres>,
    cluster_id: &str,
    bundle: &str,
) -> ApiResult<Option<Installed>> {
    Ok(sqlx::query_as("SELECT version, state, objects, in_use, updated_at < now() - interval '10 minutes' FROM pyo.cluster_bundles WHERE cluster_id = $1 AND bundle = $2 FOR UPDATE")
        .bind(cluster_id)
        .bind(bundle)
        .fetch_optional(&mut **tx)
        .await?)
}

/// Whether the cluster's install (its agent's admission policy) is this
/// Pyo's, as its last heartbeat found (install.rs): an older one refuses
/// this Pyo's bundles, so updates wait for it.
async fn install_current(tx: &mut Transaction<'_, Postgres>, cluster_id: &str) -> ApiResult<bool> {
    Ok(sqlx::query_scalar("SELECT coalesce(capabilities->'agentInstall'->>'state', 'current') = 'current' FROM pyo.clusters WHERE id = $1")
        .bind(cluster_id)
        .fetch_optional(&mut **tx)
        .await?
        .unwrap_or(true))
}

fn busy(state: &str, stale: bool) -> bool {
    matches!(state, "installing" | "removing") && !stale
}

/// What the version before had (its row's objects) and this one doesn't:
/// pruned by the update, but never a CRD or the namespace.
fn dropped(b: &SystemBundle, objects: Value) -> Vec<ObjectRef> {
    let keep: HashSet<ObjectRef> = b.refs().into_iter().collect();
    let old: Vec<ObjectRef> = serde_json::from_value(objects).unwrap_or_default();
    old.into_iter()
        .filter(|r| !keep.contains(r) && !never_pruned(r))
        .collect()
}

/// Send a bundle and record it.
async fn send(
    tx: &mut Transaction<'_, Postgres>,
    cluster_id: &str,
    b: &SystemBundle,
    prune: Vec<ObjectRef>,
) -> ApiResult<()> {
    let command = deploy::enqueue(
        tx,
        cluster_id,
        Command::ApplyBundle {
            bundle: Box::new(b.clone()),
            prune,
        },
    )
    .await?;
    let objects = serde_json::to_value(b.refs()).map_err(anyhow::Error::from)?;
    let ready = serde_json::to_value(ready_checks(b)).map_err(anyhow::Error::from)?;
    sqlx::query(
        "INSERT INTO pyo.cluster_bundles (cluster_id, bundle, version, state, objects, ready, command_id) VALUES ($1, $2, $3, 'installing', $4, $5, $6)
         ON CONFLICT (cluster_id, bundle) DO UPDATE SET version = $3, state = 'installing', message = '', objects = $4, ready = $5, command_id = $6, updated_at = now()",
    )
    .bind(cluster_id)
    .bind(&b.id)
    .bind(&b.version)
    .bind(objects)
    .bind(ready)
    .bind(command)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// The WAF's bundle as the cluster's services need it: installed (and
/// current with this installation's images) while a service has its WAF
/// on, removed once none has.
pub async fn sync_waf(
    tx: &mut Transaction<'_, Postgres>,
    cluster_id: &str,
    on: bool,
) -> ApiResult<()> {
    let row: Option<(String, String, Value)> = sqlx::query_as("SELECT version, state, objects FROM pyo.cluster_bundles WHERE cluster_id = $1 AND bundle = $2").bind(cluster_id).bind(WAF_ID).fetch_optional(&mut **tx).await?;
    if on {
        let b = waf_bundle();
        match row {
            Some((v, st, _)) if v == b.version && st != "failed" && st != "removing" => {}
            // An update waits for the install that allows it (sent once it's there).
            Some((_, st, _)) if st != "removing" && !install_current(tx, cluster_id).await? => {}
            Some((_, st, objects)) => {
                send(tx, cluster_id, &b, dropped(&b, objects)).await?;
                // Wanted again while it's still being removed: the agent
                // runs the install after the removal (pyo-agent's
                // `ordered_keys`), and says so meanwhile.
                if st == "removing" {
                    sqlx::query("UPDATE pyo.cluster_bundles SET message = $3 WHERE cluster_id = $1 AND bundle = $2").bind(cluster_id).bind(WAF_ID).bind(WAITING_FOR_REMOVAL).execute(&mut **tx).await?;
                }
            }
            None => send(tx, cluster_id, &b, vec![]).await?,
        }
    } else if let Some((_, _, objects)) = row.filter(|(_, st, _)| st != "removing") {
        let objects: Vec<ObjectRef> = serde_json::from_value(objects).unwrap_or_default();
        let command = deploy::enqueue(
            tx,
            cluster_id,
            Command::RemoveBundle {
                id: WAF_ID.to_owned(),
                objects,
            },
        )
        .await?;
        sqlx::query("UPDATE pyo.cluster_bundles SET state = 'removing', message = '', command_id = $3, updated_at = now() WHERE cluster_id = $1 AND bundle = $2").bind(cluster_id).bind(WAF_ID).bind(command).execute(&mut **tx).await?;
    }
    Ok(())
}

/// The databases system on a cluster, as this Pyo has it: updated when it's
/// there and older (or failed over two minutes ago), once the cluster's
/// install is this Pyo's (right after an update of Pyo, the agent's
/// admission policy is still the old one's until it's refreshed), installed
/// when `install` (a team's first database there). Returns whether it's
/// ready.
pub async fn sync_databases(
    tx: &mut Transaction<'_, Postgres>,
    cluster_id: &str,
    install: bool,
) -> ApiResult<bool> {
    let b = databases_bundle();
    match installed(tx, cluster_id, DATABASES_ID).await? {
        None if install => {
            send(tx, cluster_id, &b, vec![]).await?;
            Ok(false)
        }
        None => Ok(false),
        Some((version, state, objects, _, stale)) => {
            let current = version == b.version && state != "failed";
            let retry: bool = sqlx::query_scalar("SELECT updated_at < now() - interval '2 minutes' FROM pyo.cluster_bundles WHERE cluster_id = $1 AND bundle = $2").bind(cluster_id).bind(DATABASES_ID).fetch_one(&mut **tx).await?;
            if !current
                && !busy(&state, stale)
                && (state != "failed" || retry)
                && install_current(tx, cluster_id).await?
            {
                send(tx, cluster_id, &b, dropped(&b, objects)).await?;
                return Ok(false);
            }
            Ok(current && ready_on(tx, cluster_id, DATABASES_ID).await?)
        }
    }
}

/// Where the databases system is on each of these clusters, for the
/// dashboard: "ready", "installing" (being set up) or "failed" (with a
/// message). Ready once it runs a database there, so an update with Pyo
/// doesn't count as setting up. None where it was never set up.
pub async fn databases_state(
    db: &PgPool,
    cluster_ids: &[String],
) -> ApiResult<HashMap<String, (String, String)>> {
    let rows: Vec<(String, String, String, Value, Value, bool)> = sqlx::query_as(
        "SELECT a.cluster_id, a.state, a.message, a.ready, c.workloads,
                EXISTS (SELECT 1 FROM pyo.databases m WHERE m.cluster_id = a.cluster_id AND m.status ? 'phase')
           FROM pyo.cluster_bundles a JOIN pyo.clusters c ON c.id = a.cluster_id WHERE a.bundle = 'databases' AND a.cluster_id = ANY($1)",
    )
    .bind(cluster_ids)
    .fetch_all(db)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(c, state, message, ready, workloads, running)| {
            (
                c,
                if running {
                    ("ready".to_owned(), String::new())
                } else {
                    shown_state(&state, &message, &ready, &workloads)
                },
            )
        })
        .collect())
}

/// The agent finished a bundle's command (only the latest one counts).
pub async fn finished(
    tx: &mut Transaction<'_, Postgres>,
    cluster_id: &str,
    command_id: &str,
    command: &Value,
    ok: bool,
    message: Option<&str>,
) -> ApiResult<()> {
    let (bundle, removing) = match command["kind"].as_str() {
        Some("apply_bundle") => (command["bundle"]["id"].as_str().unwrap_or(""), false),
        Some("remove_bundle") => (command["id"].as_str().unwrap_or(""), true),
        _ => return Ok(()),
    };
    let current: Option<Option<String>> = sqlx::query_scalar(
        "SELECT command_id FROM pyo.cluster_bundles WHERE cluster_id = $1 AND bundle = $2",
    )
    .bind(cluster_id)
    .bind(bundle)
    .fetch_optional(&mut **tx)
    .await?;
    if current.flatten().as_deref() != Some(command_id) {
        // A removal an install was waiting for: the install runs now.
        if removing {
            sqlx::query("UPDATE pyo.cluster_bundles SET message = '' WHERE cluster_id = $1 AND bundle = $2 AND state = 'installing' AND message = $3")
                .bind(cluster_id)
                .bind(bundle)
                .bind(WAITING_FOR_REMOVAL)
                .execute(&mut **tx)
                .await?;
        }
        return Ok(());
    }
    let message = message.map(plain);
    // In the activity feed, like the request that started it.
    let (team_id, name): (String, String) =
        sqlx::query_as("SELECT team_id, name FROM pyo.clusters WHERE id = $1")
            .bind(cluster_id)
            .fetch_one(&mut **tx)
            .await?;
    let label = known(bundle).map_or(bundle.to_owned(), |b| b.name);
    // Databases are Pyo's own doing: only a failure to set them up is news
    // (an update that fails is tried again, and says why in the log).
    let first_setup = bundle == DATABASES_ID
        && {
            let reported: Option<Option<String>> = sqlx::query_scalar("SELECT reported_version FROM pyo.cluster_bundles WHERE cluster_id = $1 AND bundle = $2").bind(cluster_id).bind(bundle).fetch_optional(&mut **tx).await?;
            reported.flatten().is_none_or(|v| v.is_empty())
        };
    let message = match message {
        Some(m) if bundle == DATABASES_ID && m.contains(POLICY_REFUSED) => Some(
            "This cluster's agent was installed by an older Pyo. Run the cluster's connect command again, then databases update by themselves.",
        ),
        m => m,
    };
    let (status, title) = if bundle == DATABASES_ID {
        let why = message
            .filter(|m| !m.is_empty())
            .map(|m| format!(": {m}"))
            .unwrap_or_default();
        ("error", format!("Couldn't set up databases on {name}{why}"))
    } else {
        outcome(removing, ok, &label, &name, message)
    };
    if bundle == DATABASES_ID && !ok && !first_setup {
        tracing::warn!(
            cluster = cluster_id,
            message = message.unwrap_or(""),
            "couldn't update databases; trying again shortly"
        );
    }
    if bundle != DATABASES_ID || (!ok && first_setup) {
        log_activity(
            tx,
            &team_id,
            "Pyo",
            "cluster.updated",
            status,
            &title,
            json!({ "kind": "cluster", "id": cluster_id, "label": name }),
            json!({ "clusterId": cluster_id, "bundle": bundle }),
        )
        .await?;
    }
    if ok && removing {
        sqlx::query("DELETE FROM pyo.cluster_bundles WHERE cluster_id = $1 AND bundle = $2")
            .bind(cluster_id)
            .bind(bundle)
            .execute(&mut **tx)
            .await?;
        return Ok(());
    }
    let (state, message) = if ok {
        ("applied", String::new())
    } else {
        (
            "failed",
            message.unwrap_or("The agent couldn't do it.").to_owned(),
        )
    };
    sqlx::query("UPDATE pyo.cluster_bundles SET state = $3, message = $4, updated_at = now() WHERE cluster_id = $1 AND bundle = $2")
        .bind(cluster_id)
        .bind(bundle)
        .bind(state)
        .bind(message)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

/// A bundle command's message as shown: the agent's own words, never a
/// Kubernetes API error's text (that stays in the agent's log).
fn plain(message: &str) -> &str {
    if [
        "ApiError",
        "ErrorResponse",
        "Status {",
        "HyperError",
        "ServiceError",
    ]
    .iter()
    .any(|raw| message.contains(raw))
    {
        "The agent couldn't do it. Its log has the details."
    } else {
        message
    }
}

/// A bundle command's outcome in the activity feed: (status, title).
fn outcome(
    removing: bool,
    ok: bool,
    bundle: &str,
    cluster: &str,
    message: Option<&str>,
) -> (&'static str, String) {
    let why = message
        .filter(|m| !m.is_empty())
        .map(|m| format!(": {m}"))
        .unwrap_or_default();
    match (removing, ok) {
        (false, true) => ("success", format!("Installed {bundle} on {cluster}")),
        (false, false) => (
            "error",
            format!("Couldn't install {bundle} on {cluster}{why}"),
        ),
        (true, true) => ("success", format!("Removed {bundle} from {cluster}")),
        (true, false) => (
            "error",
            format!("Couldn't remove {bundle} from {cluster}{why}"),
        ),
    }
}

/// What a heartbeat found: each bundle's version and how much uses it.
pub async fn reported(
    tx: &mut Transaction<'_, Postgres>,
    cluster_id: &str,
    reports: &[BundleReport],
) -> ApiResult<()> {
    for r in reports {
        sqlx::query("UPDATE pyo.cluster_bundles SET reported_version = $3, in_use = $4 WHERE cluster_id = $1 AND bundle = $2")
            .bind(cluster_id)
            .bind(&r.id)
            .bind(&r.version)
            .bind(i32::try_from(r.in_use).unwrap_or(i32::MAX))
            .execute(&mut **tx)
            .await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_services_waf_shows_its_engine_and_its_policy() {
        let st = |s: &str, m: &str| (s.to_owned(), m.to_owned());
        // Not installed yet: about to be.
        assert_eq!(waf_shown(None, false, false), st("installing", ""));
        assert_eq!(
            waf_shown(Some(&st("installing", "")), false, false),
            st("installing", "")
        );
        assert_eq!(
            waf_shown(Some(&st("removing", "")), false, false),
            st("installing", "Waiting for removal")
        );
        assert_eq!(
            waf_shown(Some(&st("ready", "")), false, false),
            st("ready", "")
        );
        // The gateway refused this service's policy: not active here.
        assert_eq!(waf_shown(Some(&st("ready", "")), true, false).0, "failed");
        assert_eq!(
            waf_shown(Some(&st("installing", "")), true, false).0,
            "installing"
        );
        assert_eq!(
            waf_shown(
                Some(&st("failed", "Couldn't pull the image.")),
                false,
                false
            ),
            st("failed", "Couldn't pull the image.")
        );
        // The engine runs, but the service's policy isn't in effect yet.
        assert_eq!(
            waf_shown(Some(&st("ready", "")), false, true),
            st("installing", "")
        );
    }

    #[test]
    fn ready_once_its_workloads_are() {
        let b = known("databases").unwrap();
        let ready = serde_json::to_value(ready_checks(&b)).unwrap();
        assert!(
            ready
                .as_array()
                .unwrap()
                .iter()
                .all(|r| r["namespace"] == "pyo-databases")
        );
        let w = |name: &str, status: &str| json!({ "kind": "Deployment", "name": name, "namespace": "pyo-databases", "status": status });
        let none = json!([]);
        assert_eq!(shown_state("applied", "", &ready, &none).0, "installing");
        let half = json!([
            w("cnpg-controller-manager", "healthy"),
            w("moco-controller", "healthy"),
            w("pyo-db-controller", "progressing")
        ]);
        assert_eq!(
            shown_state("applied", "", &ready, &half),
            (
                "installing".to_owned(),
                "Starting pyo-db-controller.".to_owned()
            )
        );
        let mysql = json!([
            w("cnpg-controller-manager", "healthy"),
            w("moco-controller", "progressing")
        ]);
        assert_eq!(
            shown_state("applied", "", &ready, &mysql),
            (
                "installing".to_owned(),
                "Starting the MySQL engine.".to_owned()
            )
        );
        let all = json!([
            w("cnpg-controller-manager", "healthy"),
            w("moco-controller", "healthy"),
            w("pyo-db-controller", "healthy")
        ]);
        assert_eq!(shown_state("applied", "", &ready, &all).0, "ready");
        assert_eq!(
            shown_state("failed", "nope", &ready, &all),
            ("failed".to_owned(), "nope".to_owned())
        );
    }

    #[test]
    fn outcomes_say_what_happened() {
        assert_eq!(
            outcome(false, true, "Databases", "server-1", Some("Applied")),
            ("success", "Installed Databases on server-1".to_owned())
        );
        assert_eq!(
            outcome(false, false, "Databases", "server-1", Some("forbidden")),
            (
                "error",
                "Couldn't install Databases on server-1: forbidden".to_owned()
            )
        );
        assert_eq!(
            outcome(true, true, "Databases", "server-1", None),
            ("success", "Removed Databases from server-1".to_owned())
        );
        assert_eq!(
            outcome(true, false, "Databases", "server-1", Some("")),
            (
                "error",
                "Couldn't remove Databases from server-1".to_owned()
            )
        );
    }

    #[test]
    fn kubernetes_errors_arent_shown() {
        let raw = r#"ConfigMap pyo-waf: ApiError: configmaps "pyo-waf" is forbidden: unable to create new content in namespace pyo-waf because it is being terminated (ErrorResponse { status: "Failure", reason: "Forbidden", code: 403 })"#;
        assert_eq!(
            plain(raw),
            "The agent couldn't do it. Its log has the details."
        );
        let ours = "The previous removal hasn't finished yet. Try again in a minute.";
        assert_eq!(plain(ours), ours);
    }

    /// Only the WAF and databases, in their own namespaces.
    #[test]
    fn only_pyos_own_bundles() {
        assert_eq!(known("waf").unwrap().namespace, "pyo-waf");
        assert_eq!(known("databases").unwrap().namespace, "pyo-databases");
        assert!(known("storage").is_none());
    }
}
