//! The databases system's operators at rest: on a small cluster, each of
//! CloudNativePG, the MySQL operator and pyo-db-controller (pyo-proto's
//! [`OPERATORS`]) runs only while it has work there. Together they hold
//! about 72 MB, a few lease writes a second and dozens of watches.
//!
//! - **Small:** every server under [`SMALL_MIB`] (memory.rs, the line the
//!   installers draw). A cluster with one bigger server keeps them running,
//!   so it never waits for one to start.
//! - **In use:** a Database of one of its engines exists (imports,
//!   restores, backups and branches are Databases too), or any object of its
//!   own kinds does, even one being deleted (their finalizers are its to
//!   remove). Kinds that aren't on the cluster count as none.
//! - **Starting:** right before the agent writes a Database ([`start_for`]),
//!   and every pass while one is in use. pyo-db-controller makes the first
//!   object of an engine only once its operator is ready (the operator's
//!   webhooks refuse its kinds while it's stopped).
//! - **Stopping:** once nothing has used it for [`GRACE`], so a database
//!   deleted and made again doesn't stop and start it. The grace starts over
//!   when the agent starts.
//!
//! Only the replica count changes (the Deployments' scale, which admission
//! holds to 0 or 1 for these three by name); the bundle leaves it out, so
//! an update of the bundle keeps a stopped operator stopped.

use std::{
    collections::HashMap,
    sync::Mutex,
    time::{Duration, Instant},
};

use k8s_openapi::api::{apps::v1::Deployment, core::v1::Node};
use kube::{
    Api, Client, ResourceExt,
    api::{DynamicObject, ListParams, Patch, PatchParams},
};
use pyo_proto::{
    bundles::{OPERATORS, ObjectRef, Operator},
    memory::SMALL_MIB,
};
use serde_json::json;

/// Where the operators run (the databases bundle's namespace).
pub const NS: &str = "pyo-databases";
/// How long an operator nothing uses keeps running.
pub const GRACE: Duration = Duration::from_secs(10 * 60);
/// How often the agent looks.
const EVERY: Duration = Duration::from_secs(30);
/// The field manager of its scale changes.
const MANAGER: &str = "pyo-agent-operators";

/// Whether a cluster is small enough to stop its idle operators: every
/// server under [`SMALL_MIB`] of memory. None read: not small.
pub fn cluster_is_small(servers_mib: &[u64]) -> bool {
    !servers_mib.is_empty() && servers_mib.iter().all(|m| *m < SMALL_MIB)
}

/// What an operator has on the cluster.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Usage {
    /// Databases of its engines, those being deleted included.
    pub databases: usize,
    /// Objects of its own kinds, those being deleted included.
    pub objects: usize,
}

impl Usage {
    pub fn in_use(&self) -> bool {
        self.databases > 0 || self.objects > 0
    }
}

/// How many replicas an operator should run: one on a cluster that isn't
/// small or while it's in use; a stopped one stays stopped; a running one
/// stops once nothing has used it for `grace` ([`GRACE`]).
pub fn replicas(
    small: bool,
    usage: Usage,
    running: i32,
    idle_for: Duration,
    grace: Duration,
) -> i32 {
    if !small || usage.in_use() {
        1
    } else if running == 0 || idle_for >= grace {
        0
    } else {
        1
    }
}

/// Databases per operator, from each Database's engine.
pub fn databases_per_operator(engines: &[String]) -> HashMap<&'static str, usize> {
    OPERATORS
        .iter()
        .map(|o| {
            (
                o.deployment,
                engines
                    .iter()
                    .filter(|e| o.engines.contains(&e.as_str()))
                    .count(),
            )
        })
        .collect()
}

/// When each operator was last in use, as this agent saw it.
static LAST_USED: Mutex<Option<HashMap<&'static str, Instant>>> = Mutex::new(None);

fn last_used(deployment: &'static str, now: Instant, in_use: bool) -> Instant {
    let mut g = LAST_USED.lock().unwrap_or_else(|e| e.into_inner());
    let m = g.get_or_insert_with(HashMap::new);
    let at = m.entry(deployment).or_insert(now);
    if in_use {
        *at = now;
    }
    *at
}

fn deployments(client: &Client) -> Api<Deployment> {
    Api::namespaced(client.clone(), NS)
}

async fn set_replicas(client: &Client, deployment: &str, n: i32) -> anyhow::Result<()> {
    let pp = PatchParams {
        field_manager: Some(MANAGER.into()),
        ..Default::default()
    };
    deployments(client)
        .patch_scale(
            deployment,
            &pp,
            &Patch::Merge(json!({ "spec": { "replicas": n } })),
        )
        .await?;
    Ok(())
}

fn replicas_of(d: &Deployment) -> i32 {
    d.spec.as_ref().and_then(|s| s.replicas).unwrap_or(1)
}

/// Start what a database of `engine` needs, before the agent writes it.
/// Doesn't wait: pyo-db-controller waits for the engine's operator.
pub async fn start_for(client: &Client, engine: &str) {
    let now = Instant::now();
    for o in OPERATORS.iter().filter(|o| o.engines.contains(&engine)) {
        last_used(o.deployment, now, true);
        match deployments(client).get_opt(o.deployment).await {
            Ok(Some(d)) if replicas_of(&d) == 0 => {
                match set_replicas(client, o.deployment, 1).await {
                    Ok(()) => {
                        tracing::info!(operator = o.deployment, "starting for a new database")
                    }
                    Err(e) => {
                        tracing::warn!(operator = o.deployment, error = %format!("{e:#}"), "couldn't start it")
                    }
                }
            }
            Ok(_) => {}
            Err(e) => {
                tracing::warn!(operator = o.deployment, error = %format!("{e:#}"), "couldn't read it")
            }
        }
    }
}

/// Every server's memory in MiB.
async fn servers_mib(client: &Client) -> anyhow::Result<Vec<u64>> {
    Ok(Api::<Node>::all(client.clone())
        .list(&ListParams::default())
        .await?
        .items
        .iter()
        .filter_map(|n| {
            let q = n.status.as_ref()?.capacity.as_ref()?.get("memory")?;
            Some((crate::observe::bytes(&q.0) / 1_048_576.0) as u64)
        })
        .collect())
}

/// How many objects of an operator's kinds exist. A kind that isn't there
/// is none; any other error counts as some (it's kept running).
async fn objects_of(client: &Client, o: &Operator) -> usize {
    let mut n = 0;
    for (api_version, kind) in o.kinds {
        let r = ObjectRef {
            api_version: (*api_version).into(),
            kind: (*kind).into(),
            namespace: None,
            name: String::new(),
        };
        match crate::bundles::api_for(client, &r)
            .list(&ListParams::default())
            .await
        {
            Ok(l) => n += l.items.len(),
            Err(kube::Error::Api(e)) if e.code == 404 => {}
            Err(e) => {
                tracing::warn!(kind, error = %e, "couldn't count them; keeping the operator");
                n += 1;
            }
        }
    }
    n
}

/// One look: each operator started or stopped as it should be.
pub async fn pass(client: &Client) -> anyhow::Result<()> {
    let small = cluster_is_small(&servers_mib(client).await?);
    pass_as(client, small, GRACE).await
}

/// [`pass`] for a cluster that is `small` or not, with this grace.
async fn pass_as(client: &Client, small: bool, grace: Duration) -> anyhow::Result<()> {
    let have = deployments(client)
        .list(&ListParams::default())
        .await?
        .items;
    let running: HashMap<String, i32> = have
        .iter()
        .filter(|d| OPERATORS.iter().any(|o| o.deployment == d.name_any()))
        .map(|d| (d.name_any(), replicas_of(d)))
        .collect();
    if running.is_empty() {
        // Databases aren't set up here.
        return Ok(());
    }
    let dbs = Api::<DynamicObject>::all_with(client.clone(), &crate::databases::resource())
        .list(&ListParams::default())
        .await?;
    let engines: Vec<String> = dbs
        .items
        .iter()
        .map(|d| {
            d.data["spec"]["engine"]
                .as_str()
                .unwrap_or_default()
                .to_owned()
        })
        .collect();
    let per = databases_per_operator(&engines);
    let now = Instant::now();
    for o in &OPERATORS {
        let Some(&have) = running.get(o.deployment) else {
            continue;
        };
        let databases = per.get(o.deployment).copied().unwrap_or(0);
        // Its own objects only matter once no database needs it.
        let objects = if small && databases == 0 {
            objects_of(client, o).await
        } else {
            0
        };
        let usage = Usage { databases, objects };
        let since = last_used(o.deployment, now, usage.in_use());
        let want = replicas(small, usage, have, now.duration_since(since), grace);
        if want != have {
            set_replicas(client, o.deployment, want).await?;
            tracing::info!(
                operator = o.deployment,
                replicas = want,
                small,
                databases,
                objects,
                "{}",
                if want == 0 {
                    "stopped while nothing uses it"
                } else {
                    "started"
                }
            );
        }
    }
    Ok(())
}

/// Every [`EVERY`], for as long as the agent runs.
pub async fn run(client: Client) {
    let mut tick = tokio::time::interval(EVERY);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tick.tick().await;
        if let Err(e) = pass(&client).await {
            tracing::warn!(error = %format!("{e:#}"), "couldn't check the database operators");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIN: Duration = Duration::from_secs(60);

    #[test]
    fn small_is_every_server_small() {
        assert!(cluster_is_small(&[1945]));
        assert!(cluster_is_small(&[1945, 3583]));
        // One bigger server: room for them.
        assert!(!cluster_is_small(&[1945, 3584]));
        assert!(!cluster_is_small(&[7900]));
        // Nothing read: as if not small.
        assert!(!cluster_is_small(&[]));
    }

    #[test]
    fn in_use_while_databases_or_objects_exist() {
        let none = Usage::default();
        assert!(!none.in_use());
        // A database (one being imported, restored or deleted too).
        assert!(
            Usage {
                databases: 1,
                objects: 0
            }
            .in_use()
        );
        // The operator's own objects left: a cluster or backup being
        // deleted, waiting on its finalizer.
        assert!(
            Usage {
                databases: 0,
                objects: 1
            }
            .in_use()
        );
    }

    #[test]
    fn stopped_after_the_grace_period_on_small_clusters_only() {
        let idle = Usage::default();
        let used = Usage {
            databases: 1,
            objects: 0,
        };
        let leftovers = Usage {
            databases: 0,
            objects: 2,
        };
        // Not small: always running, and started again if it was stopped
        // (a bigger server joined).
        assert_eq!(replicas(false, idle, 1, 60 * MIN, GRACE), 1);
        assert_eq!(replicas(false, idle, 0, 60 * MIN, GRACE), 1);
        // Small and in use: started.
        assert_eq!(replicas(true, used, 0, Duration::ZERO, GRACE), 1);
        assert_eq!(replicas(true, leftovers, 0, 60 * MIN, GRACE), 1);
        assert_eq!(replicas(true, leftovers, 1, 60 * MIN, GRACE), 1);
        // Small, idle: kept for the grace period, then stopped.
        assert_eq!(replicas(true, idle, 1, Duration::ZERO, GRACE), 1);
        assert_eq!(replicas(true, idle, 1, 9 * MIN, GRACE), 1);
        assert_eq!(replicas(true, idle, 1, GRACE, GRACE), 0);
        // A stopped one stays stopped (also right after the agent starts).
        assert_eq!(replicas(true, idle, 0, Duration::ZERO, GRACE), 0);
    }

    #[test]
    fn each_engine_has_its_operator() {
        let per = databases_per_operator(&["valkey".into(), "valkey".into(), "mysql".into()]);
        assert_eq!(per["cnpg-controller-manager"], 0);
        assert_eq!(per["moco-controller"], 1);
        // pyo-db-controller runs every engine.
        assert_eq!(per["pyo-db-controller"], 3);
        let pg = databases_per_operator(&["postgres".into()]);
        assert_eq!(
            (
                pg["cnpg-controller-manager"],
                pg["moco-controller"],
                pg["pyo-db-controller"]
            ),
            (1, 0, 1)
        );
        // A KV database alone: the Postgres and MySQL operators stop,
        // pyo-db-controller runs.
        let kv = databases_per_operator(&["valkey".into()]);
        let want = |name: &str| {
            replicas(
                true,
                Usage {
                    databases: kv[name],
                    objects: 0,
                },
                1,
                GRACE,
                GRACE,
            )
        };
        assert_eq!(want("cnpg-controller-manager"), 0);
        assert_eq!(want("moco-controller"), 0);
        assert_eq!(want("pyo-db-controller"), 1);
    }

    #[test]
    fn the_grace_starts_over_with_each_use() {
        let t0 = Instant::now();
        // First seen idle: the grace starts then.
        assert_eq!(last_used("test-a", t0, false), t0);
        let t1 = t0 + 5 * MIN;
        assert_eq!(last_used("test-a", t1, false), t0);
        // Used again: from then on.
        assert_eq!(last_used("test-a", t1, true), t1);
        assert_eq!(last_used("test-a", t1 + MIN, false), t1);
    }

    /// Against a throwaway cluster (1.30+), as the agent's own
    /// ServiceAccount under its admission policy: the operators stop while
    /// nothing uses them, a bundle update keeps them stopped, a new
    /// Postgres database starts its operator and comes up, and its operator
    /// stops again a grace period after it's deleted. It needs CloudNativePG's
    /// image and `ghcr.io/cloudnative-pg/postgresql:18` on the cluster, and
    /// pyo-db-controller (this build) running against it, e.g. on the host.
    /// Prints the measured times.
    ///
    /// PYO_TEST_KUBECONFIG=<throwaway kubeconfig> cargo test -p pyo-agent -- --ignored --nocapture stopped_and_started_against
    #[tokio::test]
    #[ignore = "needs a throwaway Kubernetes API server and pyo-db-controller"]
    async fn stopped_and_started_against_a_cluster() {
        use k8s_openapi::api::{authentication::v1::TokenRequest, core::v1::ServiceAccount};
        use kube::api::PostParams;
        use pyo_proto::rbac;
        let Ok(path) = std::env::var("PYO_TEST_KUBECONFIG") else {
            panic!("set PYO_TEST_KUBECONFIG to a throwaway cluster's kubeconfig")
        };
        let kc = kube::config::Kubeconfig::read_from(&path).unwrap();
        let config = kube::Config::from_custom_kubeconfig(kc, &Default::default())
            .await
            .unwrap();
        let admin = Client::try_from(config.clone()).unwrap();
        // The images the admission policy allows (this process's env; the
        // defaults can't be pulled, so pyo-db-controller runs elsewhere).
        let db = pyo_proto::system::db_images_from_env();
        let (waf_image, geoip_image) =
            ("127.0.0.1:5005/pyo/waf:dev", "127.0.0.1:5005/pyo/geoip:dev");
        let install = rbac::InstallParts::new(false, true, waf_image, geoip_image, &db);
        let pp = PatchParams::apply("pyo-test").force();
        for o in install
            .access
            .iter()
            .chain(install.admission.iter())
            .filter(|o| o["kind"] != "HelmChart")
        {
            let r = ObjectRef::of(o);
            crate::bundles::api_for(&admin, &r)
                .patch(&r.name, &pp, &Patch::Apply(o))
                .await
                .unwrap_or_else(|e| panic!("{}: {e}", r.name));
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
        let token = Api::<ServiceAccount>::namespaced(admin.clone(), rbac::NS)
            .create_token_request(
                rbac::SA,
                &PostParams::default(),
                &serde_json::from_value::<TokenRequest>(
                    json!({ "spec": { "audiences": [], "expirationSeconds": 3600 } }),
                )
                .unwrap(),
            )
            .await
            .unwrap()
            .status
            .unwrap()
            .token
            .unwrap();
        let mut sa_config = config.clone();
        sa_config.auth_info = kube::config::AuthInfo {
            token: Some(token.into()),
            ..Default::default()
        };
        let sa = Client::try_from(sa_config).unwrap();

        // The databases bundle as the agent applies it, but MySQL's
        // certificates (this cluster has no cert-manager).
        let mut bundle = pyo_proto::bundles::databases_bundle(&db);
        bundle.objects.retain(|o| {
            !o["apiVersion"]
                .as_str()
                .unwrap_or("")
                .starts_with("cert-manager.io/")
        });
        crate::bundles::apply(&sa, &bundle, &[])
            .await
            .expect("installed");
        let deployments = deployments(&admin);
        let replicas = |name: &'static str| {
            let deployments = deployments.clone();
            async move { replicas_of(&deployments.get(name).await.unwrap()) }
        };
        let ready = |name: &'static str| {
            let deployments = deployments.clone();
            async move {
                deployments
                    .get(name)
                    .await
                    .unwrap()
                    .status
                    .and_then(|s| s.ready_replicas)
                    .unwrap_or(0)
                    > 0
            }
        };
        let wait = |what: &str, secs: u64| {
            let what = what.to_owned();
            move |done: bool, started: Instant| {
                assert!(
                    started.elapsed() < Duration::from_secs(secs),
                    "{what}: not within {secs}s"
                );
                done
            }
        };
        let pg = "cnpg-controller-manager";
        let t = Instant::now();
        let check = wait("CloudNativePG ready after the install", 300);
        while !check(ready(pg).await, t) {
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        println!("installed: CloudNativePG ready in {:?}", t.elapsed());

        // A short grace, and the agent's pass every 5 seconds.
        let grace = Duration::from_secs(45);
        let passes = tokio::spawn({
            let sa = sa.clone();
            async move {
                loop {
                    if let Err(e) = pass_as(&sa, true, grace).await {
                        panic!("pass: {e:#}");
                    }
                    tokio::time::sleep(Duration::from_secs(5)).await;
                }
            }
        });
        let t = Instant::now();
        let check = wait("stopped while nothing uses them", 120);
        loop {
            let mut stopped = true;
            for o in OPERATORS {
                stopped &= replicas(o.deployment).await == 0;
            }
            if check(stopped, t) {
                break;
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        println!(
            "idle: all three stopped {:?} after the first pass (grace {grace:?})",
            t.elapsed()
        );

        // Only 0 or 1, and only these.
        let sa_deployments = super::deployments(&sa);
        let two = sa_deployments
            .patch_scale(
                pg,
                &PatchParams::default(),
                &Patch::Merge(json!({ "spec": { "replicas": 2 } })),
            )
            .await;
        assert!(two.is_err(), "two replicas refused: {two:?}");
        // An update of the bundle keeps them stopped.
        crate::bundles::apply(&sa, &bundle, &[])
            .await
            .expect("updated");
        for o in OPERATORS {
            assert_eq!(
                replicas(o.deployment).await,
                0,
                "{} after an update",
                o.deployment
            );
        }

        // A Postgres database: its operator starts, then it comes up.
        let ns = "pyo-db-optest";
        let spec = pyo_proto::DatabaseSpec {
            namespace: ns.into(),
            name: "shop".into(),
            database: json!({
                "apiVersion": "pyo.dev/v1alpha1", "kind": "Database",
                "metadata": { "name": "shop", "namespace": ns, "labels": { "app.kubernetes.io/managed-by": "pyo" } },
                "spec": { "engine": "postgres", "version": "18", "instances": 1, "storage": "1Gi", "cpu": "250m", "memory": "512Mi", "credentialsSecret": "shop-credentials",
                    "postgres": { "database": "app", "owner": "app", "pooler": false } },
            }),
            secret_name: "shop-credentials".into(),
            secret_type: "kubernetes.io/basic-auth".into(),
            env: vec![
                pyo_proto::EnvVar {
                    key: "username".into(),
                    value: "app".into(),
                },
                pyo_proto::EnvVar {
                    key: "password".into(),
                    value: "not-a-secret-just-a-test".into(),
                },
            ],
            secrets: vec![],
            remove_secrets: vec![],
            team_id: String::new(),
        };
        let databases =
            Api::<DynamicObject>::namespaced_with(admin.clone(), ns, &crate::databases::resource());
        let t = Instant::now();
        crate::databases::apply(&sa, &spec).await.expect("applied");
        println!(
            "created: Database written {:?} after the command",
            t.elapsed()
        );
        let (mut operator_at, mut messages) = (None, vec![]);
        let check = wait("the database ready", 420);
        loop {
            if operator_at.is_none() && ready(pg).await {
                operator_at = Some(t.elapsed());
                println!(
                    "created: CloudNativePG ready {:?} after the command",
                    t.elapsed()
                );
            }
            let d = databases.get("shop").await.unwrap();
            let st = &d.data["status"];
            let said = format!(
                "{} {}",
                st["phase"].as_str().unwrap_or(""),
                st["message"].as_str().unwrap_or("")
            );
            if messages.last() != Some(&said) {
                println!("  {:>6.1}s status: {said}", t.elapsed().as_secs_f64());
                messages.push(said);
            }
            if check(st["phase"] == "Ready", t) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        println!(
            "created: Database ready {:?} after the command",
            t.elapsed()
        );
        assert!(operator_at.is_some());
        assert!(
            messages
                .iter()
                .any(|m| m.contains("Starting the Postgres operator")),
            "{messages:?}"
        );
        assert_eq!(
            replicas("moco-controller").await,
            0,
            "the MySQL operator stays stopped"
        );

        // Deleted: stopped again once its cluster is gone and the grace ran out.
        let t = Instant::now();
        crate::databases::delete(&sa, ns, "shop")
            .await
            .expect("deleted");
        println!("deleted: Database gone {:?} after the command", t.elapsed());
        let clusters = crate::bundles::api_for(
            &admin,
            &ObjectRef {
                api_version: "postgresql.cnpg.io/v1".into(),
                kind: "Cluster".into(),
                namespace: Some(ns.into()),
                name: String::new(),
            },
        );
        let mut gone_at = None;
        let check = wait("stopped after the delete", 300);
        let stopped_at = loop {
            if gone_at.is_none()
                && clusters
                    .list(&ListParams::default())
                    .await
                    .unwrap()
                    .items
                    .is_empty()
            {
                gone_at = Some(t.elapsed());
                println!(
                    "deleted: its CloudNativePG cluster gone {:?} after the command",
                    t.elapsed()
                );
            }
            if replicas(pg).await == 0 {
                break t.elapsed();
            }
            check(false, t);
            tokio::time::sleep(Duration::from_secs(1)).await;
        };
        println!(
            "deleted: CloudNativePG stopped {:?} after the command",
            t.elapsed()
        );
        let gone_at = gone_at.expect("its cluster gone before it stopped");
        assert!(
            stopped_at >= gone_at + grace - Duration::from_secs(6),
            "stopped {stopped_at:?}, its cluster gone {gone_at:?}"
        );
        tokio::time::sleep(Duration::from_secs(10)).await;
        assert_eq!(replicas("pyo-db-controller").await, 0);
        passes.abort();

        // This host isn't small: the real pass starts them all again.
        pass(&sa).await.expect("pass");
        for o in OPERATORS {
            assert_eq!(
                replicas(o.deployment).await,
                1,
                "{} on a cluster that isn't small",
                o.deployment
            );
        }
    }
}
