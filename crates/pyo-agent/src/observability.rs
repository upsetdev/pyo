//! Website analytics and server metrics on this cluster: the gateway
//! (Envoy) writes a JSON access log, and a Vector DaemonSet forwards those lines, plus each
//! server's own load, disk I/O and network (Vector's host_metrics), to the
//! Pyo API with this agent's token.
//!
//! Observability is an installation-wide setting, sent with each heartbeat
//! reply. On: the agent (re)applies Vector at its first reply, so upgrading
//! the agent upgrades the pipeline, and on the cluster Pyo runs on it starts
//! ClickHouse. Off: Vector is removed and ClickHouse stopped, its volume
//! (and so the history) kept for when it's back on.

use k8s_openapi::api::{
    apps::v1::{DaemonSet, StatefulSet},
    core::v1::{Secret, ServiceAccount},
};
use kube::{
    Api, Client,
    api::{DeleteParams, Patch, PatchParams},
};
use serde_json::json;

use crate::MANAGER;

const NS: &str = "pyo-system";
const VECTOR_IMAGE: &str = "timberio/vector:0.60.0-debian";

/// Reads the proxy's container log files directly (CRI format: "<time>
/// <stream> <F|P> <line>"), so Vector needs no Kubernetes API access.
/// host_metrics reads the server's /proc and /sys (mounted read-only from the
/// host) every 30 seconds; only real disks, network cards and filesystems
/// are kept, and CPU and memory are dropped (the heartbeat has them).
const VECTOR_CONFIG: &str = r#"data_dir: /vector-data-dir
sources:
  # The gateway's access log (Envoy, JSON; fields in pyo-proto's
  # system.rs).
  proxy:
    type: file
    include:
      - /var/log/pods/pyo-gateway_envoy-pyo-*/envoy/*.log
      # The WAF's findings (pyo-waf: one JSON line per request).
      - /var/log/pods/pyo-waf_pyo-waf-*/waf/*.log
    # A file it has read before goes on from its checkpoint (data_dir, on
    # the server). One it hasn't is read from its start, also when it was
    # written while Vector was down (a reboot, an upgrade): a WAF or proxy
    # pod that started meanwhile would otherwise lose its first lines.
    # Files untouched for a day are older than anything worth sending.
    read_from: beginning
    ignore_older_secs: 86400
  host:
    type: host_metrics
    scrape_interval_secs: 30
    namespace: host
    collectors: [load, cpu, memory, filesystem, disk, network]
    filesystem:
      filesystems:
        excludes: [overlay, tmpfs, devtmpfs, squashfs, nsfs, proc, sysfs, cgroup, cgroup2, autofs, devpts, mqueue, securityfs, debugfs, tracefs, fuse.lxcfs, rpc_pipefs, bpf]
      mountpoints:
        excludes: ["/var/lib/kubelet/*", "/var/lib/rancher/*", "/run/*", "/sys/*", "/proc/*", "/dev/*", "/boot/efi"]
    disk:
      devices:
        excludes: ["loop*", "ram*", "sr*", "zram*", "nbd*"]
    network:
      devices:
        excludes: ["lo", "veth*", "cni*", "flannel*", "cali*", "docker*", "br-*", "kube-*", "tunl*", "vxlan*", "lxc*", "virbr*"]
transforms:
  host_kept:
    type: remap
    inputs: [host]
    drop_on_abort: true
    source: |
      # CPU and memory come with the heartbeat; keep the rest small.
      if !includes(["load1", "load5", "load15", "filesystem_used_bytes", "filesystem_total_bytes", "disk_read_bytes_total", "disk_written_bytes_total", "network_receive_bytes_total", "network_transmit_bytes_total"], .name) { abort }
      .tags.node = get_env_var("PYO_NODE_NAME") ?? ""
  access:
    type: remap
    inputs: [proxy]
    drop_on_error: true
    drop_on_abort: true
    source: |
      # Lines that aren't JSON (the proxy's own start-up messages, a line
      # split by the runtime) are skipped quietly, not counted as errors.
      parts, err = parse_regex(string(.message) ?? "", r'^\S+ \S+ F (?P<msg>\{.*)$')
      if err != null { abort }
      parsed, err = parse_json(parts.msg)
      if err != null || !is_object(parsed) { abort }
      . = object!(parsed)
      # Access log lines only, not the proxy's own messages.
      if !exists(.start_time) && !exists(.waf) { abort }
      # The server whose proxy answered: traffic per region.
      .PyoNode = get_env_var("PYO_NODE_NAME") ?? ""
sinks:
  pyo_metrics:
    type: http
    inputs: [host_kept]
    uri: "__API_URL__/api/agent/v1/host-metrics"
    method: post
    encoding:
      codec: json
    framing:
      method: newline_delimited
    auth:
      strategy: bearer
      token: "__TOKEN__"
    batch:
      max_events: 500
      timeout_secs: 30
    request:
      retry_attempts: 3
  pyo:
    type: http
    inputs: [access]
    uri: "__API_URL__/api/agent/v1/access-logs"
    method: post
    encoding:
      codec: json
    framing:
      method: newline_delimited
    auth:
      strategy: bearer
      token: "__TOKEN__"
    batch:
      max_events: 1000
      timeout_secs: 5
    request:
      retry_attempts: 10
"#;

fn pp() -> PatchParams {
    PatchParams::apply(MANAGER).force()
}

/// Pyo's ClickHouse (platform.yaml), on the cluster Pyo runs on.
const CLICKHOUSE: &str = "pyo-clickhouse";

/// Turn observability on or off here: Vector (the proxy's access log is in
/// its pinned settings, see pyo-proto's system.rs), and with `pyo_cluster`
/// ClickHouse. An error is for the caller to log and retry: analytics are
/// optional, deploys are not.
pub async fn set(client: &Client, on: bool, pyo_cluster: bool) -> anyhow::Result<()> {
    if pyo_cluster {
        clickhouse(client, on).await?;
    }
    if on {
        vector(client).await
    } else {
        remove_vector(client).await
    }
}

/// ClickHouse's replicas: 1 or 0. Its volume stays either way. Applied by
/// this agent's field manager, which then owns the field: the upgrader's
/// platform.yaml leaves it out, so an update never changes it back.
async fn clickhouse(client: &Client, on: bool) -> anyhow::Result<()> {
    let api = Api::<StatefulSet>::namespaced(client.clone(), NS);
    if api.get_opt(CLICKHOUSE).await?.is_none() {
        return Ok(());
    }
    api.patch(
        CLICKHOUSE,
        &pp(),
        &Patch::Apply(json!({
            "apiVersion": "apps/v1", "kind": "StatefulSet",
            "metadata": { "name": CLICKHOUSE },
            "spec": { "replicas": if on { 1 } else { 0 } },
        })),
    )
    .await?;
    Ok(())
}

async fn remove_vector(client: &Client) -> anyhow::Result<()> {
    let gone = |r: kube::Result<_>| match r {
        Err(kube::Error::Api(e)) if e.code == 404 => Ok(()),
        r => r.map(|_| ()),
    };
    gone(
        Api::<DaemonSet>::namespaced(client.clone(), NS)
            .delete("pyo-vector", &DeleteParams::default())
            .await
            .map(|_| ()),
    )?;
    gone(
        Api::<Secret>::namespaced(client.clone(), NS)
            .delete("pyo-vector", &DeleteParams::default())
            .await
            .map(|_| ()),
    )?;
    Ok(())
}

async fn vector(client: &Client) -> anyhow::Result<()> {
    let labels =
        json!({ "app.kubernetes.io/name": "pyo-vector", "app.kubernetes.io/managed-by": MANAGER });
    Api::<ServiceAccount>::namespaced(client.clone(), NS)
        .patch("pyo-vector", &pp(), &Patch::Apply(json!({ "apiVersion": "v1", "kind": "ServiceAccount", "metadata": { "name": "pyo-vector", "labels": labels } })))
        .await?;
    // Vector won't expand env vars in the sink URI, so the config carries the
    // URL and token itself, and lives in a Secret.
    let url = std::env::var("PYO_API_URL")?
        .trim_end_matches('/')
        .to_owned();
    let token = std::env::var("PYO_AGENT_TOKEN")?;
    let config = VECTOR_CONFIG
        .replace("__API_URL__", &url)
        .replace("__TOKEN__", &token);
    Api::<Secret>::namespaced(client.clone(), NS)
        .patch(
            "pyo-vector",
            &pp(),
            &Patch::Apply(json!({ "apiVersion": "v1", "kind": "Secret", "metadata": { "name": "pyo-vector", "labels": labels }, "type": "Opaque", "stringData": { "vector.yaml": config } })),
        )
        .await?;
    Api::<DaemonSet>::namespaced(client.clone(), NS)
        .patch(
            "pyo-vector",
            &pp(),
            &Patch::Apply(json!({
                "apiVersion": "apps/v1", "kind": "DaemonSet",
                "metadata": { "name": "pyo-vector", "labels": labels },
                "spec": {
                    "selector": { "matchLabels": { "app.kubernetes.io/name": "pyo-vector" } },
                    // Every server's copy moves at once: one on a server that's
                    // down (it can't be replaced there) would otherwise hold
                    // the others on their old config for good. A copy is only
                    // gone for seconds, and picks up where it stopped.
                    "updateStrategy": { "type": "RollingUpdate", "rollingUpdate": { "maxUnavailable": "100%" } },
                    "template": {
                        "metadata": {
                            "labels": labels,
                            // Restart Vector when its config changes.
                            "annotations": { "pyo/config-hash": format!("{:x}", fxhash(&config)) },
                        },
                        "spec": {
                            "serviceAccountName": "pyo-vector",
                            "tolerations": [{ "operator": "Exists" }],
                            "containers": [{
                                "name": "vector",
                                "image": VECTOR_IMAGE,
                                // Also re-read the config when its Secret changes, in
                                // case a restart is held up.
                                "args": ["--config", "/etc/vector/vector.yaml", "--watch-config"],
                                "env": [
                                    { "name": "VECTOR_LOG", "value": "warn" },
                                    { "name": "PYO_NODE_NAME", "valueFrom": { "fieldRef": { "fieldPath": "spec.nodeName" } } },
                                    // host_metrics reads the server's, not the container's.
                                    { "name": "PROCFS_ROOT", "value": "/host/proc" },
                                    { "name": "SYSFS_ROOT", "value": "/host/sys" },
                                ],
                                "resources": { "requests": { "cpu": "10m", "memory": "32Mi" }, "limits": { "memory": "256Mi" } },
                                "volumeMounts": [
                                    { "name": "config", "mountPath": "/etc/vector", "readOnly": true },
                                    { "name": "data", "mountPath": "/vector-data-dir" },
                                    { "name": "var-log", "mountPath": "/var/log", "readOnly": true },
                                    { "name": "host-proc", "mountPath": "/host/proc", "readOnly": true },
                                    { "name": "host-sys", "mountPath": "/host/sys", "readOnly": true },
                                ],
                            }],
                            "volumes": [
                                { "name": "config", "secret": { "secretName": "pyo-vector" } },
                                { "name": "data", "hostPath": { "path": "/var/lib/pyo-vector", "type": "DirectoryOrCreate" } },
                                { "name": "var-log", "hostPath": { "path": "/var/log" } },
                                { "name": "host-proc", "hostPath": { "path": "/proc" } },
                                { "name": "host-sys", "hostPath": { "path": "/sys" } },
                            ],
                        },
                    },
                },
            })),
        )
        .await?;
    Ok(())
}

/// Small stable hash for the config annotation.
fn fxhash(s: &str) -> u64 {
    s.bytes().fold(0xcbf29ce484222325u64, |h, b| {
        (h ^ b as u64).wrapping_mul(0x100000001b3)
    })
}
