//! In-cluster builders: a pod pinned to one server with BuildKit and
//! pyo-builder side by side, talking over a shared socket.
//!
//! Every build step runs in a BuildKit sandbox container, never on the
//! server. BuildKit itself needs a privileged container (rootless BuildKit
//! needs user namespaces, which Ubuntu restricts by default). pyo-builder,
//! which checks out the code and runs Railpack's detection, runs as an
//! unprivileged user with no capabilities and a read-only filesystem.
//!
//! BuildKit runs only while there are builds: its container starts it when
//! pyo-builder asks (a `wanted` file in the shared socket directory) and
//! stops it when pyo-builder takes the file away after a while without
//! builds. Its cache stays on the server's disk in between.

use k8s_openapi::api::{
    apps::v1::Deployment,
    core::v1::{Node, Secret},
};
use kube::{
    Api,
    api::{DeleteParams, Patch, PatchParams},
};
use pyo_proto::{BuilderSpec, memory::Tier};
use serde_json::{Value, json};

use crate::{Agent, MANAGER};

const NS: &str = "pyo-system";
/// BuildKit, from Pyo's mirror (deploy/mirrors.txt).
const BUILDKIT_IMAGE: &str = "ghcr.io/upsetdev/pyo-mirror/moby/buildkit:v0.35.1@sha256:baf6138eb8c37644800c7c913fba051e840ebb9ba13eaf92d92ef9a1712d0b4c";

fn name(builder_id: &str) -> String {
    format!("pyo-builder-{}", builder_id.replace('_', "-"))
}

/// BuildKit's settings.
fn config_name(name: &str) -> String {
    format!("{name}-config")
}

/// The BuildKit container's command: runs buildkitd (with the arguments
/// after the script) while `/run/buildkit/wanted` exists, and marks it
/// started with `/run/buildkit/up`. A buildkitd that exits on its own is
/// started again while it's wanted.
const ON_DEMAND: &str = r#"run=/run/buildkit
pid=
stop() {
  rm -f "$run/up"
  if [ -n "$pid" ]; then kill -TERM "$pid" 2>/dev/null; wait "$pid"; fi
  pid=
}
trap 'stop; exit 0' TERM INT
rm -f "$run/up"
while :; do
  if [ -e "$run/wanted" ]; then
    if [ -z "$pid" ]; then
      buildkitd "$@" &
      pid=$!
      touch "$run/up"
    elif ! kill -0 "$pid" 2>/dev/null; then
      wait "$pid"
      pid=
      rm -f "$run/up"
      sleep 2
    fi
  elif [ -n "$pid" ]; then
    stop
  fi
  sleep 1
done
"#;

/// BuildKit's memory (its container's resources): every build step runs in
/// its container, so a big build can't take the whole server. Its limit
/// is a share of its server's memory (`server_mib`), by the server's
/// memory tier: half of a small server's, 60% of a normal one's, three
/// quarters of a large one's. Its request stays 8Mi: BuildKit sleeps
/// between builds, so nothing is set aside for it. The spec's own request
/// and limit win (Pyo's `PYO_BUILDKIT_MEMORY_REQUEST` and
/// `PYO_BUILDKIT_MEMORY_LIMIT`). With no server memory and no limit set,
/// there's no limit.
pub fn buildkit_resources(server_mib: Option<u64>, request: &str, limit: &str) -> Value {
    let valid = |q: &str| crate::observe::bytes(q) >= 1_048_576.0;
    let request = match request.trim() {
        r if valid(r) => r.to_owned(),
        _ => "8Mi".to_owned(),
    };
    let limit = match limit.trim() {
        l if valid(l) => Some(l.to_owned()),
        _ => server_mib.map(|m| {
            let percent = match pyo_proto::memory::tier(m) {
                Tier::Small => 50,
                Tier::Normal => 60,
                Tier::Large => 75,
            };
            format!("{}Mi", m * percent / 100)
        }),
    }
    // Never under the request (Kubernetes refuses that).
    .map(|l| {
        if crate::observe::bytes(&l) < crate::observe::bytes(&request) {
            request.clone()
        } else {
            l
        }
    });
    let mut r = json!({ "requests": { "cpu": "10m", "memory": request } });
    if let Some(l) = limit {
        r["limits"] = json!({ "memory": l });
    }
    r
}

/// A server's memory in MiB, from its node.
async fn server_mib(c: &kube::Client, server: &str) -> Option<u64> {
    let n = Api::<Node>::all(c.clone()).get(server).await.ok()?;
    let q = n.status?.capacity?.get("memory")?.0.clone();
    Some((crate::observe::bytes(&q) / 1_048_576.0) as u64).filter(|&m| m > 0)
}

pub async fn ensure(agent: &Agent, spec: &BuilderSpec) -> anyhow::Result<String> {
    let c = agent.kube.clone();
    let name = name(&spec.builder_id);
    let pp = PatchParams::apply(MANAGER).force();
    let labels = json!({ "app.kubernetes.io/name": "pyo-builder", "app.kubernetes.io/managed-by": MANAGER, "pyo/builder-id": spec.builder_id });

    // Its own key, made here and registered with Pyo: it signs its
    // requests with it. The token is only its registry door login.
    let mut env = serde_json::Map::new();
    env.insert("PYO_API_URL".into(), json!(spec.api_url));
    if !spec.token.is_empty() {
        env.insert("PYO_BUILDER_TOKEN".into(), json!(spec.token));
    }
    match crate::trust::component_key(&agent.api, "builder", Some(&spec.builder_id)).await {
        Ok((key_id, pkcs8)) => {
            env.insert("PYO_BUILDER_KEY_ID".into(), json!(key_id));
            env.insert("PYO_BUILDER_KEY".into(), json!(pkcs8));
        }
        Err(e) => tracing::warn!(builder = %spec.builder_id, error = %format!("{e:#}"), "couldn't register the builder's key"),
    }
    // Restarted on a new login: it reads them at start.
    let login_stamp = pyo_proto::token_hash(&format!(
        "{}\n{}",
        env.get("PYO_BUILDER_KEY_ID").and_then(|v| v.as_str()).unwrap_or(""),
        pyo_proto::token_hash(&spec.token)
    ))[..16]
        .to_owned();
    Api::<Secret>::namespaced(c.clone(), NS)
        .patch(
            &name,
            &pp,
            &Patch::Apply(json!({
                "apiVersion": "v1", "kind": "Secret", "metadata": { "name": name, "labels": labels }, "type": "Opaque",
                "stringData": env,
            })),
        )
        .await?;
    // A Secret, though nothing in it is: the agent has no ConfigMaps to write.
    Api::<Secret>::namespaced(c.clone(), NS)
        .patch(
            &config_name(&name),
            &pp,
            &Patch::Apply(json!({
                "apiVersion": "v1", "kind": "Secret", "metadata": { "name": config_name(&name), "labels": labels }, "type": "Opaque",
                // Pyo's registry is plain HTTP on the tailnet.
                "stringData": { "buildkitd.toml": format!("[registry.\"{}\"]\n  http = true\n  insecure = true\n", spec.registry_host) },
            })),
        )
        .await?;
    let mib = server_mib(&c, &spec.server).await;
    if mib.is_none() && spec.buildkit_memory_limit.is_empty() {
        tracing::warn!(builder = %spec.builder_id, server = %spec.server, "couldn't read the server's memory: BuildKit runs without a memory limit");
    }
    let buildkit = buildkit_resources(
        mib,
        &spec.buildkit_memory_request,
        &spec.buildkit_memory_limit,
    );
    Api::<Deployment>::namespaced(c, NS)
        .patch(
            &name,
            &pp,
            &Patch::Apply(json!({
                "apiVersion": "apps/v1", "kind": "Deployment",
                "metadata": { "name": name, "labels": labels },
                "spec": {
                    "replicas": 1,
                    "strategy": { "type": "Recreate" },
                    "selector": { "matchLabels": { "pyo/builder-id": spec.builder_id } },
                    "template": {
                        "metadata": { "labels": labels, "annotations": { "pyo.dev/login": login_stamp } },
                        "spec": {
                            "nodeSelector": { "kubernetes.io/hostname": spec.server },
                            // Build-only servers (join.sh --builder) are tainted for everything else.
                            "tolerations": [{ "key": pyo_proto::BUILDER_NODE_LABEL, "operator": "Exists", "effect": "NoSchedule" }, pyo_proto::gpu_toleration()],
                            "containers": [
                                {
                                    "name": "buildkitd",
                                    "image": BUILDKIT_IMAGE,
                                    // The socket is group-owned by the builder's group, so the
                                    // unprivileged builder can talk to it.
                                    // Started on demand (ON_DEMAND); the rest are buildkitd's.
                                    "command": ["/bin/sh", "-c", ON_DEMAND, "buildkitd-on-demand"],
                                    "args": ["--addr", "unix:///run/buildkit/buildkitd.sock", "--group", "65532", "--config", "/etc/buildkit/buildkitd.toml"],
                                    "securityContext": { "privileged": true },
                                    "volumeMounts": [
                                        { "name": "run", "mountPath": "/run/buildkit" },
                                        { "name": "config", "mountPath": "/etc/buildkit" },
                                        { "name": "cache", "mountPath": "/var/lib/buildkit" },
                                    ],
                                    // Asleep between builds: a shell and its sleep.
                                    // A build's memory isn't set aside, up to its limit.
                                    "resources": buildkit,
                                },
                                {
                                    "name": "builder",
                                    "image": spec.image,
                                    "imagePullPolicy": "Always",
                                    "envFrom": [{ "secretRef": { "name": name } }],
                                    "env": [
                                        { "name": "BUILDKIT_HOST", "value": "unix:///run/buildkit/buildkitd.sock" },
                                        // Where it wakes BuildKit (see ON_DEMAND).
                                        { "name": "PYO_BUILDKIT_WAKE", "value": "/run/buildkit" },
                                        // git and railpack keep their state under $HOME.
                                        { "name": "HOME", "value": "/tmp" },
                                        { "name": "TMPDIR", "value": "/tmp" },
                                    ],
                                    "securityContext": {
                                        "runAsUser": 65532, "runAsGroup": 65532, "runAsNonRoot": true,
                                        "allowPrivilegeEscalation": false, "readOnlyRootFilesystem": true,
                                        "capabilities": { "drop": ["ALL"] }, "seccompProfile": { "type": "RuntimeDefault" },
                                    },
                                    "volumeMounts": [{ "name": "run", "mountPath": "/run/buildkit" }, { "name": "tmp", "mountPath": "/tmp" }],
                                    // Checkouts and Railpack live in /tmp while a build runs.
                                    // A few MiB between builds (measured).
                                    "resources": { "requests": { "cpu": "10m", "memory": "16Mi" }, "limits": { "memory": "1Gi" } },
                                },
                            ],
                            "volumes": [
                                { "name": "run", "emptyDir": {} },
                                { "name": "tmp", "emptyDir": { "sizeLimit": "10Gi" } },
                                { "name": "config", "secret": { "secretName": config_name(&name) } },
                                // Build cache that survives restarts of the pod.
                                { "name": "cache", "hostPath": { "path": format!("/var/lib/{name}"), "type": "DirectoryOrCreate" } },
                            ],
                        },
                    },
                },
            })),
        )
        .await?;
    Ok(format!("Builder {name} scheduled on {}", spec.server))
}

pub async fn remove(agent: &Agent, builder_id: &str) -> anyhow::Result<String> {
    let c = agent.kube.clone();
    let name = name(builder_id);
    let dp = DeleteParams::background();
    for r in [
        Api::<Deployment>::namespaced(c.clone(), NS)
            .delete(&name, &dp)
            .await
            .map(|_| ()),
        Api::<Secret>::namespaced(c.clone(), NS)
            .delete(&name, &dp)
            .await
            .map(|_| ()),
        Api::<Secret>::namespaced(c, NS)
            .delete(&config_name(&name), &dp)
            .await
            .map(|_| ()),
    ] {
        match r {
            Ok(()) | Err(kube::Error::Api(_)) => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(format!("Removed {name}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buildkit_gets_a_share_of_its_servers_memory() {
        let limit = |mib: Option<u64>| buildkit_resources(mib, "", "")["limits"]["memory"].clone();
        // Small, normal and large servers (pyo_proto::memory's tiers).
        assert_eq!(limit(Some(2048)), "1024Mi");
        assert_eq!(limit(Some(4096)), "2457Mi");
        assert_eq!(limit(Some(32768)), "24576Mi");
        // Nothing set aside while it sleeps.
        assert_eq!(
            buildkit_resources(Some(4096), "", "")["requests"]["memory"],
            "8Mi"
        );
        // Unknown memory: no limit.
        assert!(buildkit_resources(None, "", "")["limits"].is_null());
        // Pyo's own settings win; a bad one is left out.
        let r = buildkit_resources(Some(4096), "1Gi", "6Gi");
        assert_eq!(
            (
                r["requests"]["memory"].as_str(),
                r["limits"]["memory"].as_str()
            ),
            (Some("1Gi"), Some("6Gi"))
        );
        let r = buildkit_resources(Some(4096), "lots", "x");
        assert_eq!(
            (
                r["requests"]["memory"].as_str(),
                r["limits"]["memory"].as_str()
            ),
            (Some("8Mi"), Some("2457Mi"))
        );
        // A limit is never under the request.
        let r = buildkit_resources(Some(2048), "3Gi", "");
        assert_eq!(r["limits"]["memory"], "3Gi");
    }
}
