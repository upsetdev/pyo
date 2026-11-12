//! In-cluster builders: a pod pinned to one server with BuildKit and
//! pyo-builder side by side, talking over a shared socket.
//!
//! Every build step runs in a BuildKit sandbox container, never on the
//! server. BuildKit itself needs a privileged container (rootless BuildKit
//! needs user namespaces, which Ubuntu restricts by default). pyo-builder,
//! which checks out the code and runs Railpack's detection, runs as an
//! unprivileged user with no capabilities and a read-only filesystem.

use k8s_openapi::api::{apps::v1::Deployment, core::v1::Secret};
use kube::{
    Api,
    api::{DeleteParams, Patch, PatchParams},
};
use pyo_proto::BuilderSpec;
use serde_json::json;

use crate::{Agent, MANAGER};

const NS: &str = "pyo-system";
const BUILDKIT_IMAGE: &str = "moby/buildkit:v0.34.1";

fn name(builder_id: &str) -> String {
    format!("pyo-builder-{}", builder_id.replace('_', "-"))
}

/// BuildKit's settings.
fn config_name(name: &str) -> String {
    format!("{name}-config")
}

pub async fn ensure(agent: &Agent, spec: &BuilderSpec) -> anyhow::Result<String> {
    let c = agent.kube.clone();
    let name = name(&spec.builder_id);
    let pp = PatchParams::apply(MANAGER).force();
    let labels = json!({ "app.kubernetes.io/name": "pyo-builder", "app.kubernetes.io/managed-by": MANAGER, "pyo/builder-id": spec.builder_id });

    Api::<Secret>::namespaced(c.clone(), NS)
        .patch(
            &name,
            &pp,
            &Patch::Apply(json!({
                "apiVersion": "v1", "kind": "Secret", "metadata": { "name": name, "labels": labels }, "type": "Opaque",
                "stringData": { "PYO_API_URL": spec.api_url, "PYO_BUILDER_TOKEN": spec.token },
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
                        "metadata": { "labels": labels },
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
                                    "args": ["--addr", "unix:///run/buildkit/buildkitd.sock", "--group", "65532", "--config", "/etc/buildkit/buildkitd.toml"],
                                    "securityContext": { "privileged": true },
                                    "volumeMounts": [
                                        { "name": "run", "mountPath": "/run/buildkit" },
                                        { "name": "config", "mountPath": "/etc/buildkit" },
                                        { "name": "cache", "mountPath": "/var/lib/buildkit" },
                                    ],
                                    "resources": { "requests": { "cpu": "100m", "memory": "256Mi" } },
                                },
                                {
                                    "name": "builder",
                                    "image": spec.image,
                                    "imagePullPolicy": "Always",
                                    "envFrom": [{ "secretRef": { "name": name } }],
                                    "env": [
                                        { "name": "BUILDKIT_HOST", "value": "unix:///run/buildkit/buildkitd.sock" },
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
                                    "resources": { "requests": { "cpu": "10m", "memory": "64Mi" }, "limits": { "memory": "1Gi" } },
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
