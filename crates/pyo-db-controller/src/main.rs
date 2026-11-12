//! pyo-db-controller runs Pyo's built-in databases on a cluster: Postgres
//! through CloudNativePG, Valkey with Sentinel and MySQL through MOCO, from
//! the Database resources the Pyo agent writes.
//!
//! ```text
//! PYO_DB_POSTGRES_IMAGE     Postgres image repository; the version is the tag (ghcr.io/cloudnative-pg/postgresql)
//! PYO_DB_POSTGRES_IMAGE_<major>  Pyo's Postgres image for that version (16, 17, 18), used instead
//! PYO_DB_VALKEY_IMAGE       Valkey image repository (docker.io/valkey/valkey)
//! PYO_DB_VALKEY_BUNDLE_IMAGE  Valkey 8.1 with its modules, run while a Database loads any (docker.io/valkey/valkey-bundle:8.1.11@sha256:...)
//! PYO_DB_PYO_KV_IMAGE       pyo-kv image (ghcr.io/upsetdev/pyo-kv:latest)
//! PYO_DB_MYSQL_IMAGE        MySQL 8.4's mysqld image (ghcr.io/cybozu-go/moco/mysql:8.4.10.1)
//! PYO_DB_DEFAULT_POSTGRES   version when a Database doesn't say (18)
//! PYO_DB_DEFAULT_VALKEY     version when a Database doesn't say (8.1)
//! PYO_DB_CLUSTER_DOMAIN     the cluster's DNS domain (cluster.local)
//! PYO_DB_PROBE_ADDR         health endpoint (0.0.0.0:8081)
//! POD_NAME, POD_NAMESPACE   this replica and where its Lease lives (leader election)
//! ```

mod controller;
mod crd;
mod extensions;
mod import;
mod leader;
mod modules;
mod mysql;
mod postgres;
mod valkey;

use std::sync::Arc;

use futures::StreamExt;
use k8s_openapi::api::apps::v1::{Deployment, StatefulSet};
use kube::{
    Api, Client, CustomResourceExt,
    runtime::{
        Controller, Predicate, WatchStreamExt,
        events::{Recorder, Reporter},
        predicates, reflector, watcher,
    },
};
use tokio::io::AsyncWriteExt;

/// valkey-bundle 8.1.11 (Valkey 8.1.11 with valkey-json 1.0.3, valkey-bloom
/// 1.0.1 and valkey-search 1.0.3), the multi-platform index.
const VALKEY_BUNDLE: &str = "docker.io/valkey/valkey-bundle:8.1.11@sha256:3f57cc71a102e0e66991ae203b069038d8c3f6214ca887ff32f9a381091af8de";

fn env(k: &str, def: &str) -> String {
    std::env::var(k)
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| def.to_owned())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    if std::env::args().nth(1).as_deref() == Some("print-crd") {
        // The schema derived from the Rust types (deploy/crd.yaml adds the
        // validation rules on top).
        println!("{}", serde_json::to_string_pretty(&crd::Database::crd())?);
        return Ok(());
    }
    rustls::crypto::aws_lc_rs::default_provider()
        .install_default()
        .ok();
    tracing_subscriber::fmt()
        .json()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let client = Client::try_default().await?;
    let pod = env("POD_NAME", "pyo-db-controller");
    let ctx = Arc::new(controller::Ctx {
        client: client.clone(),
        recorder: Recorder::new(
            client.clone(),
            Reporter {
                controller: controller::OWNER.into(),
                instance: Some(pod.clone()),
            },
        ),
        announced: Default::default(),
        pruned: Default::default(),
        handed_over: Default::default(),
        started: postgres::rfc3339(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_secs() as i64),
        ),
        images: controller::Images {
            postgres_repo: env("PYO_DB_POSTGRES_IMAGE", "ghcr.io/cloudnative-pg/postgresql"),
            postgres: ["16", "17", "18"]
                .iter()
                .filter_map(|m| {
                    std::env::var(format!("PYO_DB_POSTGRES_IMAGE_{m}"))
                        .ok()
                        .filter(|v| !v.is_empty())
                        .map(|v| (m.to_string(), v))
                })
                .collect(),
            valkey_repo: env("PYO_DB_VALKEY_IMAGE", "docker.io/valkey/valkey"),
            valkey_bundle: env("PYO_DB_VALKEY_BUNDLE_IMAGE", VALKEY_BUNDLE),
            pyo_kv: env("PYO_DB_PYO_KV_IMAGE", "ghcr.io/upsetdev/pyo-kv:latest"),
            mysql_84: env(
                "PYO_DB_MYSQL_IMAGE",
                "ghcr.io/cybozu-go/moco/mysql:8.4.10.1",
            ),
        },
        default_postgres: env("PYO_DB_DEFAULT_POSTGRES", "18"),
        default_valkey: env("PYO_DB_DEFAULT_VALKEY", "8.1"),
        domain: env("PYO_DB_CLUSTER_DOMAIN", "cluster.local"),
    });
    tokio::spawn(probes(env("PYO_DB_PROBE_ADDR", "0.0.0.0:8081")));
    tracing::info!(images = ?ctx.images, "starting");

    // Replicas take turns: only the one holding the lease reconciles.
    let leader = leader::Leader::new(client.clone(), &env("POD_NAMESPACE", "pyo-databases"), pod);
    leader.acquire().await;
    tracing::info!("leading");

    let ours = watcher::Config::default().labels(&format!(
        "{}={}",
        controller::LABEL_MANAGED_BY,
        controller::OWNER
    ));
    // Reconcile on spec changes, not on the status this controller writes:
    // read replicas' lag changes all the time, and each status write would
    // otherwise start the next reconcile at once.
    let (reader, writer) = reflector::store();
    let databases = watcher(
        Api::<crd::Database>::all(client.clone()),
        watcher::Config::default(),
    )
    .default_backoff()
    .reflect(writer)
    .applied_objects()
    .predicate_filter(
        predicates::generation
            .combine(predicates::labels)
            .combine(predicates::annotations),
        Default::default(),
    );
    let run = Controller::for_stream(databases, reader)
        .owns(Api::<StatefulSet>::all(client.clone()), ours.clone())
        .owns(Api::<Deployment>::all(client), ours)
        .shutdown_on_signal()
        .run(controller::reconcile, controller::error_policy, ctx)
        .for_each(|r| async move {
            if let Err(e) = r {
                tracing::debug!(error = %e, "reconcile");
            }
        });
    tokio::select! {
        _ = run => {}
        _ = leader.hold() => {
            // Another replica leads now: stop at once, and start over as a
            // follower (the pod restarts).
            tracing::warn!("lost the lease");
            std::process::exit(1);
        }
    }
    Ok(())
}

/// Answers 200 to anything: the process is up and its loop is running.
async fn probes(addr: String) {
    let Ok(l) = tokio::net::TcpListener::bind(&addr).await else {
        tracing::warn!(%addr, "probe listener");
        return;
    };
    while let Ok((mut s, _)) = l.accept().await {
        tokio::spawn(async move {
            let _ = s
                .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n\r\nok")
                .await;
        });
    }
}
