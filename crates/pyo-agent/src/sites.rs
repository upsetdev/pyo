//! Pyo Static on this cluster: the static sites `pyo-static` serves
//! (pyo-proto's site.rs).
//!
//! - Its bundle's login Secret gets Pyo's address and the cluster's token
//!   when the bundle is applied ([`fill_login`]).
//! - A static deploy has `pyo-static` fetch its folder first ([`warm`]),
//!   then moves the site's routes there (gateway.rs sends them to
//!   `pyo-static` with the folder in a header), and only then removes the
//!   workload it ran as before, if any (rollout.rs).
//! - While a site is served as files, a small Secret in its namespace says
//!   so ([`marker`]): its routes are a live service's (gateway.rs's
//!   `workload_exists`). A deploy that runs it as a service again keeps
//!   the routes on `pyo-static` until its replicas are ready.

use std::time::Duration;

use k8s_openapi::api::{apps::v1::Deployment, core::v1::Secret};
use kube::{
    Api, Client, ResourceExt,
    api::{DeleteParams, LogParams, Patch, PatchParams},
};
use pyo_proto::{
    DeploySpec, LogLine, ServiceRef,
    bundles::SystemBundle,
    system::{STATIC_LOGIN, STATIC_NAME, STATIC_NS, STATIC_PORT},
};
use serde_json::json;

use crate::{Agent, MANAGER, observe::SERVICE_LABEL};

/// On the marker: the deployment whose folder is served.
pub const MARKER_LABEL: &str = "pyo.dev/static-site";

/// The marker's name, beside the service's other Secrets
/// (`<name>-pull`, `<name>-env-<deployment>`).
pub fn marker_name(service: &str) -> String {
    format!("{service}-site")
}

/// Puts Pyo's address and the cluster's token into the bundle's login
/// Secret (only its name is pinned). Without them `pyo-static` serves
/// only what it has cached, so a failure is logged, not fatal.
pub async fn fill_login(agent: &Agent, bundle: &mut SystemBundle) {
    let login = match agent.api.static_login().await {
        Ok(l) => l,
        Err(e) => {
            tracing::warn!(error = %format!("{e:#}"), "couldn't get pyo-static's login");
            return;
        }
    };
    for o in &mut bundle.objects {
        if o["kind"] == "Secret" && o["metadata"]["name"] == STATIC_LOGIN {
            o["stringData"] = json!({ "url": login.0, "token": login.1 });
        }
    }
}

/// Whether `pyo-static` is up (its Deployment has a ready copy), waiting up
/// to `wait`.
pub async fn wait_ready(client: &Client, wait: Duration) -> bool {
    let api: Api<Deployment> = Api::namespaced(client.clone(), STATIC_NS);
    let until = tokio::time::Instant::now() + wait;
    loop {
        let ready = api
            .get_opt(STATIC_NAME)
            .await
            .ok()
            .flatten()
            .and_then(|d| d.status)
            .and_then(|s| s.available_replicas)
            .unwrap_or(0)
            > 0;
        if ready || tokio::time::Instant::now() >= until {
            return ready;
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

/// Where the agent reaches `pyo-static`.
fn base() -> String {
    format!("http://{STATIC_NAME}.{STATIC_NS}.svc.cluster.local:{STATIC_PORT}")
}

/// Has `pyo-static` fetch a deploy's folder: (files, fetched now).
pub async fn warm(site: &str, deploy: &str) -> anyhow::Result<(u64, u64)> {
    let http = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(5))
        .build()?;
    let res = http
        .post(format!("{}/_pyo/warm?site={site}&deploy={deploy}", base()))
        .timeout(Duration::from_secs(600))
        .send()
        .await?;
    let status = res.status();
    let v: serde_json::Value = res.json().await.unwrap_or_default();
    if !status.is_success() {
        anyhow::bail!(
            "{}",
            v["error"]
                .as_str()
                .unwrap_or("the static file server didn't answer")
        );
    }
    Ok((
        v["files"].as_u64().unwrap_or(0),
        v["fetched"].as_u64().unwrap_or(0),
    ))
}

/// The marker: the service is a static site, serving this deployment's
/// folder.
pub async fn mark(client: &Client, spec: &DeploySpec, deploy: &str) -> anyhow::Result<()> {
    let name = marker_name(&spec.service.name);
    Api::<Secret>::namespaced(client.clone(), &spec.service.namespace)
        .patch(
            &name,
            &PatchParams::apply(MANAGER).force(),
            &Patch::Apply(json!({
                "apiVersion": "v1", "kind": "Secret", "type": "Opaque",
                "metadata": { "name": name, "labels": {
                    "app.kubernetes.io/managed-by": MANAGER, SERVICE_LABEL: spec.service.service_id, MARKER_LABEL: deploy,
                } },
            })),
        )
        .await?;
    Ok(())
}

/// Whether the service is served as static files now: its marker is there
/// (and its own).
pub async fn marked(client: &Client, service: &ServiceRef) -> anyhow::Result<bool> {
    let s = Api::<Secret>::namespaced(client.clone(), &service.namespace)
        .get_opt(&marker_name(&service.name))
        .await?;
    Ok(s.is_some_and(|s| {
        s.metadata.deletion_timestamp.is_none()
            && s.labels().get(SERVICE_LABEL) == Some(&service.service_id)
            && s.labels().contains_key(MARKER_LABEL)
    }))
}

/// The service runs as a service again (or is deleted): no marker.
pub async fn unmark(client: &Client, service: &ServiceRef) -> anyhow::Result<()> {
    if !marked(client, service).await? {
        return Ok(());
    }
    match Api::<Secret>::namespaced(client.clone(), &service.namespace)
        .delete(&marker_name(&service.name), &DeleteParams::default())
        .await
    {
        Ok(_) => Ok(()),
        Err(kube::Error::Api(e)) if e.code == 404 => Ok(()),
        Err(e) => Err(e.into()),
    }
}

/// A site's requests from `pyo-static`'s output (one JSON line each), as
/// its logs.
pub async fn logs(
    client: &Client,
    service_id: &str,
    tail_lines: i64,
) -> anyhow::Result<Vec<LogLine>> {
    use k8s_openapi::api::core::v1::Pod;
    let pods: Api<Pod> = Api::namespaced(client.clone(), STATIC_NS);
    let list = pods
        .list(
            &kube::api::ListParams::default()
                .labels(&format!("app.kubernetes.io/name={STATIC_NAME}")),
        )
        .await?;
    let mut out = vec![];
    for p in &list.items {
        let lp = LogParams {
            // Every site's lines are in it: read more to find this one's.
            tail_lines: Some(20_000),
            timestamps: true,
            ..Default::default()
        };
        let Ok(text) = pods.logs(&p.name_any(), &lp).await else {
            continue;
        };
        out.extend(site_lines(&text, service_id));
    }
    out.sort_by_key(|l| l.at);
    let skip = out.len().saturating_sub(tail_lines.clamp(1, 2000) as usize);
    Ok(out.into_iter().skip(skip).collect())
}

/// This site's request lines in `pyo-static`'s output (with timestamps).
pub fn site_lines(text: &str, service_id: &str) -> Vec<LogLine> {
    let mut out = vec![];
    for l in text.lines() {
        let Some((ts, rest)) = l.split_once(' ') else {
            continue;
        };
        let Ok(v) = serde_json::from_str::<serde_json::Value>(rest) else {
            continue;
        };
        if v["site"] != json!(service_id) {
            continue;
        }
        let at = ts
            .parse::<k8s_openapi::jiff::Timestamp>()
            .map(|t| t.as_millisecond())
            .unwrap_or_else(|_| pyo_proto::now_ms());
        let status = v["status"].as_u64().unwrap_or(0);
        out.push(LogLine {
            at,
            level: if status >= 500 {
                "error"
            } else if status >= 400 {
                "warn"
            } else {
                "info"
            }
            .into(),
            source: STATIC_NAME.into(),
            message: format!(
                "{} {} {} {}ms",
                v["method"].as_str().unwrap_or(""),
                v["path"].as_str().unwrap_or(""),
                status,
                v["ms"].as_u64().unwrap_or(0)
            ),
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_sites_logs_are_its_requests() {
        let text = "2026-11-15T10:00:00Z {\"site\":\"svc_1\",\"deploy\":\"d\",\"method\":\"GET\",\"path\":\"/\",\"status\":200,\"ms\":1}\n\
                    2026-11-15T10:00:01Z {\"site\":\"svc_2\",\"method\":\"GET\",\"path\":\"/x\",\"status\":200,\"ms\":1}\n\
                    2026-11-15T10:00:02Z not json\n\
                    2026-11-15T10:00:03Z {\"site\":\"svc_1\",\"method\":\"GET\",\"path\":\"/gone\",\"status\":404,\"ms\":2}\n";
        let lines = site_lines(text, "svc_1");
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].message, "GET / 200 1ms");
        assert_eq!(lines[1].level, "warn");
        assert!(lines[0].at < lines[1].at);
        assert_eq!(marker_name("web"), "web-site");
    }
}
