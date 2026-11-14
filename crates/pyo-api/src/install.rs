//! The agent's install manifest: `kubectl apply -f <api>/install/agent.yaml?token=…`.
//!
//! The token is the cluster's agent credential; it's only valid for the
//! cluster it was issued to and can be rotated from the dashboard.

use axum::{
    extract::{Query, State},
    http::header,
    response::IntoResponse,
};
use serde::Deserialize;

use crate::{
    AppState,
    error::{ApiError, ApiResult},
    ids::sha256_hex,
};

#[derive(Deserialize)]
pub struct InstallQuery {
    token: String,
}

pub async fn manifest(
    State(s): State<AppState>,
    headers: axum::http::HeaderMap,
    Query(q): Query<InstallQuery>,
) -> ApiResult<impl IntoResponse> {
    let known: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM pyo.clusters WHERE agent_token_hash = $1)",
    )
    .bind(sha256_hex(&q.token))
    .fetch_one(&s.db)
    .await?;
    if !known || !q.token.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(ApiError::not_found("Install token"));
    }
    // The agent reaches Pyo the way the installer just did, unless that was
    // localhost (meaningless inside a pod): then the in-cluster address.
    let base = crate::oauth::base_url(&headers);
    let host = base.split("://").nth(1).unwrap_or("");
    let api = if host.starts_with("localhost") || host.starts_with("127.") || host.is_empty() {
        s.config.agent_api_url.clone()
    } else {
        base
    };
    let yaml = render(&api, &s.config.agent_image, &q.token);
    Ok((
        [
            (header::CONTENT_TYPE, "application/yaml"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        yaml,
    ))
}

/// Envoy Gateway's own CRDs, for its version (pyo-proto's
/// `ENVOY_GATEWAY_VERSION`): its chart installs none (`ENVOY_GATEWAY_VALUES`).
const ENVOY_GATEWAY_CRDS: &str = include_str!("../assets/gateway/envoy-gateway-crds.yaml");
/// The Gateway API's standard channel, as that Envoy Gateway bundles it.
const GATEWAY_API_CRDS: &str = include_str!("../assets/gateway/gateway-api-crds.yaml");

#[derive(Deserialize)]
pub struct GatewayCrdsQuery {
    /// With the Gateway API too: for a cluster that has none (k3s before
    /// 1.37), or has the copy Pyo applied.
    #[serde(default, rename = "gatewayApi")]
    gateway_api: Option<String>,
}

/// `/install/gateway-crds.yaml[?gatewayApi=1]`: what connect.sh applies
/// (server-side) before the install manifest, so Envoy Gateway finds its
/// APIs when it starts. The same for everyone: no token.
pub async fn gateway_crds(Query(q): Query<GatewayCrdsQuery>) -> impl IntoResponse {
    let with_api = q
        .gateway_api
        .as_deref()
        .is_some_and(|v| matches!(v, "1" | "true"));
    (
        [
            (header::CONTENT_TYPE, "application/yaml"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        gateway_crds_yaml(with_api),
    )
}

fn gateway_crds_yaml(with_api: bool) -> String {
    if with_api {
        format!("{ENVOY_GATEWAY_CRDS}\n---\n{GATEWAY_API_CRDS}")
    } else {
        ENVOY_GATEWAY_CRDS.to_owned()
    }
}

/// Whether new installs bind the self-update role: their agent then
/// brings its admission policy up to date with Pyo by itself. On unless
/// `PYO_AGENT_SELF_UPDATE` is false: with it, whoever controls this Pyo can
/// loosen what the agent may do on those clusters, so a Pyo serving other
/// people's clusters (Pyo Cloud) turns it off. A cluster's owner can also
/// delete its `pyo-agent-self-update` binding.
pub fn self_update() -> bool {
    self_update_from(std::env::var("PYO_AGENT_SELF_UPDATE").ok().as_deref())
}

fn self_update_from(v: Option<&str>) -> bool {
    !v.is_some_and(|v| {
        matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "0" | "false" | "off" | "no"
        )
    })
}

/// The install this Pyo hands out (rbac.rs), for the cluster Pyo runs on
/// (`upgrades`) or another. Its images come from this installation's env.
pub fn parts(upgrades: bool) -> &'static pyo_proto::rbac::InstallParts {
    use pyo_proto::system::{db_images_from_env, geoip_image_from_env, waf_image_from_env};
    static PARTS: std::sync::OnceLock<[pyo_proto::rbac::InstallParts; 2]> =
        std::sync::OnceLock::new();
    &PARTS.get_or_init(|| {
        [false, true].map(|u| {
            pyo_proto::rbac::InstallParts::new(
                u,
                self_update(),
                &waf_image_from_env(),
                &geoip_image_from_env(),
                &db_images_from_env(),
            )
        })
    })[usize::from(upgrades)]
}

/// The agent's manifest. On the cluster Pyo itself runs on (an in-cluster
/// API address), its agent also runs Pyo's upgrader.
pub fn render(api_url: &str, image: &str, token: &str) -> String {
    let parts = parts(pyo_proto::in_cluster_url(api_url));
    let docs = |objects: &[serde_json::Value]| {
        objects
            .iter()
            .map(|o| format!("{o}\n---\n"))
            .collect::<String>()
    };
    let access = docs(&parts.access);
    let admission = docs(&parts.admission);
    let hash = &parts.hash;
    // The image pins, as the admission policy allows them.
    let pins: String = parts
        .pins
        .iter()
        .map(|(k, v)| {
            format!(
                "            - name: {k}\n              value: {}\n",
                serde_json::Value::from(v.as_str())
            )
        })
        .collect();
    format!(
        r#"# Pyo agent. Dials out to {api_url}; nothing in the cluster is exposed.
#
# What it may do (crates/pyo-proto/src/rbac.rs):
# - read nodes, namespaces and workloads across the cluster (never Secrets);
# - write only in namespaces labelled pyo.dev/managed=true: pyo-system and
#   the projects' namespaces it creates, each with a RoleBinding to
#   pyo-agent-write, the only role it may bind;
# - in kube-system, only join tokens, the GPU device plugin and cluster
#   DNS's scale;
# - the gateway's routes, certificates and settings in pyo-gateway,
#   with exactly the content this release writes (never its Secrets' content);
# - the databases system in pyo-databases (CloudNativePG, the MySQL
#   operator and pyo-db-controller), once a team makes its first database
#   here, with exactly the content this release defines;
# - cert-manager and the gateway (Envoy Gateway) are installed by this
#   manifest (their HelmCharts below), not by the agent; Envoy Gateway's CRDs
#   by connect.sh, before it. Envoy Gateway only reads pyo-gateway, and its
#   proxies hold no cluster credentials;
# - servers (pyo-agent-nodes) and their disk and network figures
#   (pyo-agent-stats): delete those two bindings to take that away;
# - read your pyo-system/pyo-policy ConfigMap, never write it;
# - with pyo-agent-self-update bound (on unless this Pyo turns it off):
#   rewrite its own admission policy when Pyo is updated, once its roles are
#   already as Pyo's; delete that binding to take that away.
# The admission policy at the end holds it to this, and refuses privileged
# containers and the host's network, processes and files, except for Pyo's
# own build, metrics and GPU components.
{access}apiVersion: v1
kind: Secret
metadata:
  name: pyo-agent
  namespace: pyo-system
type: Opaque
stringData:
  PYO_API_URL: "{api_url}"
  PYO_AGENT_TOKEN: "{token}"
---
apiVersion: apps/v1
kind: Deployment
metadata:
  name: pyo-agent
  namespace: pyo-system
  labels:
    app.kubernetes.io/name: pyo-agent
  annotations:
    {annotation}: "{hash}"
spec:
  replicas: 1
  strategy:
    type: Recreate
  selector:
    matchLabels:
      app.kubernetes.io/name: pyo-agent
  template:
    metadata:
      labels:
        app.kubernetes.io/name: pyo-agent
      annotations:
        # Which token it runs with: a new one (rotated, then the connect
        # command run again) restarts it on that one.
        pyo.dev/token: "{token_stamp}"
    spec:
      serviceAccountName: pyo-agent
      # It may run on a GPU server kept for GPU work.
      tolerations:
        - {{ key: nvidia.com/gpu, operator: Exists, effect: NoSchedule }}
      containers:
        - name: agent
          image: {image}
          imagePullPolicy: Always
          envFrom:
            - secretRef:
                name: pyo-agent
          env:
            - name: RUST_LOG
              value: pyo_agent=info
            # The WAF's, the proxies' GeoIP database's and the databases
            # system's images: the ones this manifest's admission policy allows.
{pins}          resources:
            requests: {{ cpu: 20m, memory: 48Mi }}
            limits: {{ cpu: 500m, memory: 256Mi }}
          securityContext:
            runAsNonRoot: true
            runAsUser: 65532
            allowPrivilegeEscalation: false
            readOnlyRootFilesystem: true
            capabilities: {{ drop: [ALL] }}
---
# Kubernetes 1.30 or later: the agent reports whether this is in place.
{admission}"#,
        annotation = pyo_proto::rbac::INSTALL_ANNOTATION,
        token_stamp = token_stamp(token),
    )
}

/// The agent's token as its pod template carries it: not the token, nor
/// the hash Pyo looks it up by.
fn token_stamp(token: &str) -> String {
    sha256_hex(&format!("pyo-agent-token\n{token}"))[..16].to_owned()
}

// ── Keeping a cluster's install current ────────────────────────────────────

/// The last refresh Pyo sent a cluster for the install it has now.
#[derive(Debug, Clone, PartialEq)]
pub struct LastRefresh {
    /// The command's: "pending", "sent", "done" or "failed".
    pub status: String,
    pub message: String,
    pub age_secs: i64,
    /// How many refreshes of this install Pyo has sent the cluster.
    pub attempts: i64,
}

/// A refresh sent (or done) this long without the install turning current
/// is sent again: after a minute, then twice as long each time, up to
/// [`REFRESH_PATIENCE_MAX_SECS`]. A refresh lost with an agent that
/// restarted is handed out again as soon as the new one polls (agent.rs).
const REFRESH_PATIENCE_SECS: i64 = 60;
const REFRESH_PATIENCE_MAX_SECS: i64 = 10 * 60;
/// One that failed is tried again after this long.
const REFRESH_RETRY_SECS: i64 = 30 * 60;

/// Where a cluster's install stands, and whether to send it a refresh:
/// ("current" | "updating" | "outdated", why it's outdated, send).
pub fn install_state(
    scope: &pyo_proto::rbac::AgentScope,
    expected: &str,
    last: Option<&LastRefresh>,
) -> (&'static str, String, bool) {
    if pyo_proto::rbac::install_current(scope, expected) {
        return ("current", String::new(), false);
    }
    // The agent may only update its admission policy, never make it: the
    // cluster's connect command puts a missing one back.
    if scope.admission == "missing" {
        return ("outdated", "Its admission policy is missing.".into(), false);
    }
    // Without the self-update role (or with the cluster read only), its
    // connect command brings it up to date.
    if !scope.self_update || scope.policy.read_only {
        return ("outdated", String::new(), false);
    }
    match last {
        None => ("updating", String::new(), true),
        Some(l) if l.status == "failed" => {
            if l.age_secs < REFRESH_RETRY_SECS {
                ("outdated", l.message.clone(), false)
            } else {
                ("updating", String::new(), true)
            }
        }
        // Still queued: the agent gets it when it next polls.
        Some(l) if l.status == "pending" => ("updating", String::new(), false),
        Some(l) => (
            "updating",
            String::new(),
            l.age_secs >= refresh_patience(l.attempts),
        ),
    }
}

/// How long the `attempts`th refresh of an install has before another is sent.
fn refresh_patience(attempts: i64) -> i64 {
    let doublings = attempts.clamp(1, 11) - 1;
    (REFRESH_PATIENCE_SECS << doublings).min(REFRESH_PATIENCE_MAX_SECS)
}

/// The latest refresh of `hash` sent to the cluster, if any.
async fn last_refresh(
    db: impl sqlx::PgExecutor<'_>,
    cluster_id: &str,
    hash: &str,
) -> ApiResult<Option<LastRefresh>> {
    let last: Option<(String, String, i64, i64)> = sqlx::query_as(
        "SELECT status, coalesce(result->>'message', ''), extract(epoch FROM now() - created_at)::bigint,
                count(*) OVER () FROM pyo.commands
          WHERE cluster_id = $1 AND command->>'kind' = 'refresh_install' AND command->>'hash' = $2 ORDER BY created_at DESC LIMIT 1",
    )
    .bind(cluster_id)
    .bind(hash)
    .fetch_optional(db)
    .await?;
    Ok(
        last.map(|(status, message, age_secs, attempts)| LastRefresh {
            status,
            message,
            age_secs,
            attempts,
        }),
    )
}

/// Whether the agent is bringing the cluster's install up to this Pyo's
/// (`scope` as it last reported it): its image is moved only after that
/// (updates.rs), so the new agent starts under the admission policy it's
/// made for.
pub async fn refreshing(
    db: &sqlx::PgPool,
    cluster_id: &str,
    scope: &pyo_proto::rbac::AgentScope,
    same_cluster: bool,
) -> ApiResult<bool> {
    let hash = &parts(same_cluster).hash;
    let last = last_refresh(db, cluster_id, hash).await?;
    Ok(install_state(scope, hash, last.as_ref()).0 == "updating")
}

/// From a heartbeat: where the cluster's install stands against this Pyo's,
/// as `capabilities.agentInstall` (`{state, message, command}`), sending
/// the agent a refresh when it can make one. Returns it, and whether a
/// command was queued. `command`, in every state, is the connect command
/// run again on the cluster's first server: it also applies Pyo's latest
/// host settings (the memory reserve for k3s), which no agent can.
pub async fn sync(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    cluster_id: &str,
    scope: Option<&pyo_proto::rbac::AgentScope>,
    same_cluster: bool,
) -> ApiResult<(serde_json::Value, bool)> {
    use serde_json::json;
    // Agents that don't say: as they are.
    let Some(scope) = scope else {
        return Ok((
            json!({ "state": "current", "command": command(same_cluster) }),
            false,
        ));
    };
    let parts = parts(same_cluster);
    let last = last_refresh(&mut **tx, cluster_id, &parts.hash).await?;
    let (state, message, send) = install_state(scope, &parts.hash, last.as_ref());
    if send {
        crate::deploy::enqueue(
            tx,
            cluster_id,
            pyo_proto::Command::RefreshInstall(Box::new(parts.clone())),
        )
        .await?;
        tracing::info!(cluster = cluster_id, install = %parts.hash, "sent the agent its install's refresh");
    }
    let mut out = json!({ "state": state, "command": command(same_cluster) });
    if !message.is_empty() {
        out["message"] = json!(message);
    }
    Ok((out, send))
}

/// The connect command again, run on the cluster's first server: it reads
/// Pyo's address and the token from the agent already there. (The cluster
/// Pyo runs on: from Pyo on that machine.)
fn command(same_cluster: bool) -> String {
    let base = if same_cluster {
        crate::join::local_scripts_url()
    } else {
        crate::join::scripts_url(None)
    };
    crate::join::refresh_command(&base)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pyo_proto::rbac::AgentScope;

    fn scope(install: &str, self_update: bool) -> AgentScope {
        AgentScope {
            access: "limited".into(),
            admission: "enforced".into(),
            install: install.into(),
            self_update,
            ..Default::default()
        }
    }

    fn last(status: &str, age_secs: i64) -> LastRefresh {
        LastRefresh {
            status: status.into(),
            message: "no".into(),
            age_secs,
            attempts: 1,
        }
    }

    #[test]
    fn current_installs_are_left_alone() {
        assert_eq!(
            install_state(&scope("h1", true), "h1", None),
            ("current", String::new(), false)
        );
        // Nothing to refresh where the policy isn't enforced.
        let loose = AgentScope {
            admission: "unsupported".into(),
            ..scope("", true)
        };
        assert_eq!(install_state(&loose, "h1", None).0, "current");
    }

    #[test]
    fn a_missing_admission_policy_waits_for_the_connect_command() {
        let missing = AgentScope {
            admission: "missing".into(),
            ..scope("", true)
        };
        let (state, message, send) = install_state(&missing, "h1", None);
        assert_eq!((state, send), ("outdated", false));
        assert!(message.contains("admission policy"));
    }

    #[test]
    fn without_self_update_it_waits_for_the_connect_command() {
        assert_eq!(
            install_state(&scope("h0", false), "h1", None),
            ("outdated", String::new(), false)
        );
        let mut ro = scope("h0", true);
        ro.policy.read_only = true;
        assert!(!install_state(&ro, "h1", None).2);
    }

    #[test]
    fn refreshes_are_sent_once_then_again_when_stuck_or_failed_long_ago() {
        let s = scope("h0", true);
        assert_eq!(
            install_state(&s, "h1", None),
            ("updating", String::new(), true)
        );
        assert_eq!(
            install_state(&s, "h1", Some(&last("sent", 30))),
            ("updating", String::new(), false)
        );
        assert!(!install_state(&s, "h1", Some(&last("done", 30))).2);
        // Unanswered (lost with an agent that restarted, say): again after a minute.
        assert!(install_state(&s, "h1", Some(&last("sent", REFRESH_PATIENCE_SECS))).2);
        assert!(install_state(&s, "h1", Some(&last("done", REFRESH_PATIENCE_SECS))).2);
        // Still queued: it's delivered when the agent polls.
        assert!(!install_state(&s, "h1", Some(&last("pending", REFRESH_PATIENCE_MAX_SECS))).2);
        assert_eq!(
            install_state(&s, "h1", Some(&last("failed", 60))),
            ("outdated", "no".to_owned(), false)
        );
        assert_eq!(
            install_state(&s, "h1", Some(&last("failed", REFRESH_RETRY_SECS))),
            ("updating", String::new(), true)
        );
    }

    #[test]
    fn unanswered_refreshes_are_sent_again_sooner_then_less_often() {
        assert_eq!(REFRESH_PATIENCE_SECS, 60);
        let waits: Vec<i64> = (0..8).map(refresh_patience).collect();
        assert_eq!(waits, [60, 60, 120, 240, 480, 600, 600, 600]);
        let s = scope("h0", true);
        let third = LastRefresh {
            attempts: 3,
            ..last("sent", 200)
        };
        assert!(!install_state(&s, "h1", Some(&third)).2);
        assert!(
            install_state(
                &s,
                "h1",
                Some(&LastRefresh {
                    age_secs: 240,
                    ..third
                })
            )
            .2
        );
    }

    #[test]
    fn self_update_is_on_unless_turned_off() {
        assert!(self_update_from(None));
        assert!(self_update_from(Some("")));
        assert!(self_update_from(Some("true")));
        for off in ["false", "0", "off", " No "] {
            assert!(!self_update_from(Some(off)), "{off}");
        }
    }

    #[test]
    fn the_manifest_grants_self_update_only_when_on() {
        let on = pyo_proto::rbac::InstallParts::new(
            false,
            true,
            "w",
            "g",
            &pyo_proto::system::db_images_from_env(),
        );
        let off = pyo_proto::rbac::InstallParts::new(
            false,
            false,
            "w",
            "g",
            &pyo_proto::system::db_images_from_env(),
        );
        let has = |p: &pyo_proto::rbac::InstallParts, kind: &str| {
            p.access.iter().any(|o| {
                o["kind"] == kind && o["metadata"]["name"] == pyo_proto::rbac::SELF_UPDATE_ROLE
            })
        };
        assert!(has(&on, "ClusterRole") && has(&on, "ClusterRoleBinding"));
        assert!(!has(&off, "ClusterRole") && !has(&off, "ClusterRoleBinding"));
        // Rendered: as this installation has it (on, unless the env says otherwise).
        let yaml = render("https://pyo.example.com", "img@sha256:00", "abc");
        assert_eq!(yaml.contains(&format!("\"kind\":\"ClusterRoleBinding\",\"metadata\":{{\"labels\":{{\"app.kubernetes.io/managed-by\":\"pyo\",\"app.kubernetes.io/part-of\":\"pyo-agent\"}},\"name\":\"{}\"", pyo_proto::rbac::SELF_UPDATE_ROLE)), self_update());
    }

    #[test]
    fn a_cluster_without_the_binding_gets_the_command() {
        // The agent reports it can't self-update (binding deleted): no refresh, the command instead.
        let (state, _, send) = install_state(&scope("h0", false), "h1", Some(&last("sent", 30)));
        assert_eq!((state, send), ("outdated", false));
    }

    #[test]
    fn a_new_token_restarts_the_agent() {
        let a = render("https://pyo.example.com", "img@sha256:00", "abc");
        let b = render("https://pyo.example.com", "img@sha256:00", "abd");
        let stamp = |y: &str| {
            y.lines()
                .find(|l| l.contains("pyo.dev/token:"))
                .map(str::to_owned)
        };
        assert!(
            stamp(&a).is_some_and(|l| l.starts_with("        pyo.dev/token: ")),
            "on the pod template"
        );
        assert_ne!(stamp(&a), stamp(&b));
        assert_eq!(
            stamp(&a),
            stamp(&render("https://pyo.example.com", "img@sha256:00", "abc"))
        );
        assert!(!a.contains(&sha256_hex("abc")), "not the lookup hash");
    }

    #[test]
    fn the_manifest_is_stamped_with_its_install() {
        let yaml = render("https://pyo.example.com", "img@sha256:00", "abc");
        let p = parts(false);
        assert!(
            yaml.contains(&format!("pyo.dev/install: \"{}\"", p.hash)),
            "on the Deployment"
        );
        assert!(
            yaml.contains(&format!(
                "\"annotations\":{{\"pyo.dev/install\":\"{}\"}}",
                p.hash
            )),
            "on the admission policy"
        );
        for (k, v) in &p.pins {
            assert!(
                yaml.contains(&format!("- name: {k}\n              value: \"{v}\"")),
                "{k}"
            );
        }
        // The cluster Pyo runs on has the upgrader's role, so another install.
        assert_ne!(parts(true).hash, p.hash);
        assert!(
            !yaml.contains("\"name\":\"pyo-agent-upgrade\",\"namespace\""),
            "no upgrader's role here"
        );
    }
}

// ── This machine, connected by its own setup ───────────────────────────────

#[derive(Deserialize)]
pub struct ThisMachineQuery {
    key: String,
}

/// GET /install/this-machine?key=…: the command that connects the machine
/// Pyo runs on as a cluster, for the host's own setup (pyo-up.sh /
/// install.sh, and their timer until it's done). `key` is the installation's
/// bootstrap key (`PYO_BOOTSTRAP_KEY`, a Secret only the host reads). The
/// cluster goes to the first team; before anyone has signed up there's none
/// yet, so the host asks again later. Answers `{"state": "connected"}`,
/// `{"state": "waiting"}` (no team yet) or `{"state": "connect", "command"}`.
///
/// Until its agent reports in, every ask gets the same command, whichever
/// replica answers and however many runs ask at once: its token is derived
/// from the bootstrap key and the cluster (`this_machine_token`), and asks
/// take turns (an advisory lock), so the host's setup and its timer can't
/// make two clusters or leave the agent with a token Pyo no longer knows.
pub async fn this_machine(
    State(s): State<AppState>,
    Query(q): Query<ThisMachineQuery>,
) -> ApiResult<axum::Json<serde_json::Value>> {
    use serde_json::json;
    let key = std::env::var("PYO_BOOTSTRAP_KEY").unwrap_or_default();
    if key.len() < 32 || !crate::util::constant_time_eq(key.as_bytes(), q.key.as_bytes()) {
        return Err(ApiError::not_found("Install key"));
    }
    Ok(axum::Json(match hand_over(&s.db, &key).await? {
        Handover::Connected => json!({ "state": "connected" }),
        Handover::Waiting => json!({ "state": "waiting" }),
        Handover::Connect { cluster_id, token } => {
            // The same command "Add cluster › This machine" gives.
            // From Pyo on this machine: connecting it needs no internet.
            let command = crate::join::connect_command(
                &crate::join::local_scripts_url(),
                &crate::join::local_api_url(),
                &token,
                None,
                None,
            );
            tracing::info!(cluster = %cluster_id, "this machine: connect command handed to its setup");
            json!({ "state": "connect", "command": command })
        }
    }))
}

#[derive(Debug, PartialEq)]
enum Handover {
    /// The cluster is there (its agent reported in).
    Connected,
    /// No team yet to put it in.
    Waiting,
    /// Connect it with this token.
    Connect { cluster_id: String, token: String },
}

/// This machine's cluster (made on the first ask), and its token while its
/// agent hasn't reported in. `key`: the bootstrap key.
async fn hand_over(db: &sqlx::PgPool, key: &str) -> ApiResult<Handover> {
    let mut tx = db.begin().await?;
    // One ask at a time across replicas: two asks at once would otherwise
    // both find no cluster and make one each.
    sqlx::query("SELECT pg_advisory_xact_lock(hashtext('pyo.install.this_machine'))")
        .execute(&mut *tx)
        .await?;
    // Already there: connected, or made and waiting for its agent.
    let have: Option<(String, String, String)> =
        sqlx::query_as("SELECT id, status, agent_token_hash FROM pyo.clusters WHERE same_cluster OR from_host_setup ORDER BY same_cluster DESC, created_at LIMIT 1")
            .fetch_optional(&mut *tx)
            .await?;
    let (id, hash) = match have {
        // Its agent reported in: its token is left alone.
        Some((_, status, _)) if status != "pending" => return Ok(Handover::Connected),
        Some((id, _, hash)) => (id, Some(hash)),
        None => {
            let team: Option<String> =
                sqlx::query_scalar("SELECT id FROM pyo.teams ORDER BY created_at LIMIT 1")
                    .fetch_optional(&mut *tx)
                    .await?;
            let Some(team) = team else {
                return Ok(Handover::Waiting);
            };
            let id = crate::ids::new_id("cl");
            // "local" until its agent reports the server's hostname (agent.rs).
            let local_taken: bool = sqlx::query_scalar(
                "SELECT EXISTS (SELECT 1 FROM pyo.clusters WHERE team_id = $1 AND name = 'local')",
            )
            .bind(&team)
            .fetch_one(&mut *tx)
            .await?;
            let name = if local_taken {
                let tail: String = crate::ids::secret_token()
                    .chars()
                    .filter(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
                    .take(6)
                    .collect();
                format!("local-{tail}")
            } else {
                "local".to_owned()
            };
            sqlx::query(
                "INSERT INTO pyo.clusters (id, team_id, name, description, location, agent_token_hash, tags, name_from_host, from_host_setup)
                 VALUES ($1, $2, $3, '', '', $4, '{}', true, true)",
            )
            .bind(&id)
            .bind(&team)
            .bind(&name)
            .bind(sha256_hex(&this_machine_token(key, &id)))
            .execute(&mut *tx)
            .await?;
            sqlx::query("UPDATE pyo.teams SET default_cluster_id = coalesce(default_cluster_id, $2) WHERE id = $1").bind(&team).bind(&id).execute(&mut *tx).await?;
            crate::ops::log_activity(
                &mut tx,
                &team,
                "Pyo",
                "cluster.registered",
                "info",
                "Connecting the server Pyo runs on",
                serde_json::json!({ "kind": "cluster", "id": id, "label": name }),
                serde_json::json!({ "clusterId": id }),
            )
            .await?;
            (id, None)
        }
    };
    let token = this_machine_token(key, &id);
    // Rotated meanwhile (or made before): this machine's token again.
    if hash.is_some_and(|h| h != sha256_hex(&token)) {
        sqlx::query(
            "UPDATE pyo.clusters SET agent_token_hash = $2 WHERE id = $1 AND status = 'pending'",
        )
        .bind(&id)
        .bind(sha256_hex(&token))
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(Handover::Connect {
        cluster_id: id,
        token,
    })
}

/// The agent token of this machine's cluster: an HMAC of the cluster's id
/// under the bootstrap key, so every replica (and every ask) hands over the
/// same one without keeping it anywhere. Only whoever reads the bootstrap
/// key can make it: root on this machine, who can read the agent's Secret
/// anyway. Hex, like `ids::secret_token`; rotating it from the dashboard
/// replaces it with a random one as for any cluster.
fn this_machine_token(key: &str, cluster_id: &str) -> String {
    use hmac::{Hmac, KeyInit, Mac};
    let mut mac =
        <Hmac<sha2::Sha256> as KeyInit>::new_from_slice(key.as_bytes()).expect("any key length");
    mac.update(b"pyo-this-machine-agent-token\n");
    mac.update(cluster_id.as_bytes());
    hex::encode(mac.finalize().into_bytes())
}

#[cfg(test)]
mod this_machine_tests {
    use super::*;

    const KEY: &str = "0123456789abcdef0123456789abcdef";

    #[test]
    fn the_token_is_the_same_for_every_ask_and_its_own_per_cluster_and_key() {
        let t = this_machine_token(KEY, "cl_a");
        assert_eq!(t, this_machine_token(KEY, "cl_a"));
        assert_ne!(t, this_machine_token(KEY, "cl_b"));
        assert_ne!(
            t,
            this_machine_token("fedcba9876543210fedcba9876543210", "cl_a")
        );
        // What connect.sh and the install manifest accept.
        assert_eq!(t.len(), 64);
        assert!(t.bytes().all(|b| b.is_ascii_hexdigit()));
        // Not the host proof of any token, nor the key itself.
        assert_ne!(t, pyo_proto::host_proof(KEY, &sha256_hex(&t)));
        assert!(!t.contains(KEY));
    }

    /// The tests below share this machine's cluster: one at a time.
    static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    async fn db() -> sqlx::PgPool {
        let db = sqlx::PgPool::connect(&std::env::var("DATABASE_URL").expect("DATABASE_URL"))
            .await
            .unwrap();
        crate::migrate::run(&db).await.unwrap();
        db
    }

    async fn reset(db: &sqlx::PgPool) {
        sqlx::query("DELETE FROM pyo.clusters WHERE same_cluster OR from_host_setup")
            .execute(db)
            .await
            .unwrap();
    }

    async fn team(db: &sqlx::PgPool) {
        let have: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM pyo.teams)")
            .fetch_one(db)
            .await
            .unwrap();
        if !have {
            sqlx::query("INSERT INTO pyo.teams (id, name, slug) VALUES ('tm_thismachine', 'Test', 'test-this-machine')").execute(db).await.unwrap();
        }
    }

    async fn hash_of(db: &sqlx::PgPool, id: &str) -> String {
        sqlx::query_scalar("SELECT agent_token_hash FROM pyo.clusters WHERE id = $1")
            .bind(id)
            .fetch_one(db)
            .await
            .unwrap()
    }

    /// Against a throwaway Postgres: `DATABASE_URL=… cargo test -p pyo-api
    /// this_machine -- --ignored`.
    #[tokio::test]
    #[ignore]
    async fn every_ask_gets_the_same_command_until_the_agent_reports_in() {
        let _serial = SERIAL.lock().await;
        let db = db().await;
        reset(&db).await;
        let teams: i64 = sqlx::query_scalar("SELECT count(*) FROM pyo.teams")
            .fetch_one(&db)
            .await
            .unwrap();
        if teams == 0 {
            assert_eq!(hand_over(&db, KEY).await.unwrap(), Handover::Waiting);
        }
        team(&db).await;

        let Handover::Connect { cluster_id, token } = hand_over(&db, KEY).await.unwrap() else {
            panic!("expected a command")
        };
        assert_eq!(hash_of(&db, &cluster_id).await, sha256_hex(&token));
        // Asked again (the timer, or the other replica: no memory involved).
        assert_eq!(
            hand_over(&db, KEY).await.unwrap(),
            Handover::Connect {
                cluster_id: cluster_id.clone(),
                token: token.clone()
            }
        );
        assert_eq!(hash_of(&db, &cluster_id).await, sha256_hex(&token));

        // Rotated while pending: this machine's token again.
        sqlx::query("UPDATE pyo.clusters SET agent_token_hash = 'other' WHERE id = $1")
            .bind(&cluster_id)
            .execute(&db)
            .await
            .unwrap();
        assert_eq!(
            hand_over(&db, KEY).await.unwrap(),
            Handover::Connect {
                cluster_id: cluster_id.clone(),
                token: token.clone()
            }
        );
        assert_eq!(hash_of(&db, &cluster_id).await, sha256_hex(&token));

        // Its agent reported in (then had its token rotated): left alone.
        sqlx::query("UPDATE pyo.clusters SET status = 'connected', agent_token_hash = 'rotated' WHERE id = $1").bind(&cluster_id).execute(&db).await.unwrap();
        assert_eq!(hand_over(&db, KEY).await.unwrap(), Handover::Connected);
        assert_eq!(hash_of(&db, &cluster_id).await, "rotated");
        reset(&db).await;
    }

    /// Two replicas asked at once (the host's setup and its timer): one
    /// cluster, one token, the same for both.
    #[tokio::test]
    #[ignore]
    async fn asks_at_once_agree() {
        let _serial = SERIAL.lock().await;
        let db = db().await;
        reset(&db).await;
        team(&db).await;
        // Separate pools: as two replicas would be.
        let replicas = [
            db.clone(),
            sqlx::PgPool::connect(&std::env::var("DATABASE_URL").unwrap())
                .await
                .unwrap(),
        ];
        let tasks: Vec<_> = (0..16)
            .map(|i| {
                let db = replicas[i % 2].clone();
                tokio::spawn(async move { hand_over(&db, KEY).await.unwrap() })
            })
            .collect();
        let mut got = Vec::new();
        for t in tasks {
            got.push(t.await.unwrap());
        }
        let Handover::Connect { cluster_id, token } = &got[0] else {
            panic!("expected a command")
        };
        assert!(got.iter().all(|h| h == &got[0]), "{got:?}");
        let made: i64 =
            sqlx::query_scalar("SELECT count(*) FROM pyo.clusters WHERE from_host_setup")
                .fetch_one(&db)
                .await
                .unwrap();
        assert_eq!(made, 1);
        assert_eq!(hash_of(&db, cluster_id).await, sha256_hex(token));
        reset(&db).await;
    }
}

#[cfg(test)]
mod gateway_crds_tests {
    use super::*;

    fn docs(yaml: &str) -> Vec<serde_json::Value> {
        serde_norway::Deserializer::from_str(yaml)
            .map(|d| <serde_json::Value as serde::Deserialize>::deserialize(d).unwrap())
            .filter(|v| !v.is_null())
            .collect()
    }

    fn crd_names(v: &[serde_json::Value]) -> Vec<String> {
        v.iter()
            .map(|d| d["metadata"]["name"].as_str().unwrap().to_owned())
            .collect()
    }

    /// Envoy Gateway's own CRDs, for the version Pyo installs: every kind
    /// the agent writes, nothing of the Gateway API's group.
    #[test]
    fn envoy_gateway_crds_are_its_own_and_its_version() {
        assert!(
            ENVOY_GATEWAY_CRDS
                .lines()
                .next()
                .unwrap()
                .contains(pyo_proto::system::ENVOY_GATEWAY_VERSION),
            "update the CRDs with the chart"
        );
        let d = docs(ENVOY_GATEWAY_CRDS);
        assert!(d.iter().all(|c| c["kind"] == "CustomResourceDefinition"
            && c["spec"]["group"] == "gateway.envoyproxy.io"
            && c["metadata"]["labels"]["app.kubernetes.io/managed-by"] == "pyo"));
        let names = crd_names(&d);
        for kind in [
            "backends",
            "envoyproxies",
            "securitypolicies",
            "clienttrafficpolicies",
            "backendtrafficpolicies",
            "envoyextensionpolicies",
        ] {
            assert!(
                names.contains(&format!("{kind}.gateway.envoyproxy.io")),
                "{kind}"
            );
        }
        assert!(
            !ENVOY_GATEWAY_CRDS.contains("{{"),
            "no Helm templating left"
        );
    }

    /// The Gateway API: standard channel only (k3s 1.37+'s admission policy
    /// refuses experimental CRDs over standard ones), with every kind Pyo
    /// writes served at v1, no admission policy of its own, and owned like
    /// k3s's own copy (its `gateway-api-crd` release in kube-system), so
    /// k3s adopts it when the cluster is upgraded to 1.37+.
    #[test]
    fn gateway_api_crds_are_the_standard_channel() {
        assert!(GATEWAY_API_CRDS.contains(pyo_proto::system::ENVOY_GATEWAY_VERSION));
        let d = docs(GATEWAY_API_CRDS);
        assert!(d.iter().all(|c| c["kind"] == "CustomResourceDefinition"
            && c["spec"]["group"] == "gateway.networking.k8s.io"
            && c["metadata"]["annotations"]["gateway.networking.k8s.io/channel"] == "standard"
            && c["metadata"]["labels"]["pyo.dev/gateway-api"] == "true"
            // k3s 1.37+'s chart takes them over on upgrade.
            && c["metadata"]["labels"]["app.kubernetes.io/managed-by"] == "Helm"
            && c["metadata"]["annotations"]["meta.helm.sh/release-name"] == "gateway-api-crd"
            && c["metadata"]["annotations"]["meta.helm.sh/release-namespace"] == "kube-system"
            && c["metadata"]["annotations"]["helm.sh/resource-policy"] == "keep"));
        for kind in [
            "gatewayclasses",
            "gateways",
            "httproutes",
            "tcproutes",
            "udproutes",
        ] {
            let crd = d
                .iter()
                .find(|c| {
                    c["metadata"]["name"] == format!("{kind}.gateway.networking.k8s.io").as_str()
                })
                .unwrap_or_else(|| panic!("{kind}"));
            assert!(
                crd["spec"]["versions"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|v| v["name"] == "v1" && v["served"] == true),
                "{kind} v1"
            );
        }
        assert!(!GATEWAY_API_CRDS.contains("{{"), "no Helm templating left");
    }

    #[test]
    fn the_gateway_api_comes_only_when_asked() {
        let envoy_only = docs(&gateway_crds_yaml(false));
        assert!(
            envoy_only
                .iter()
                .all(|c| c["spec"]["group"] == "gateway.envoyproxy.io")
        );
        let both = docs(&gateway_crds_yaml(true));
        assert_eq!(both.len(), envoy_only.len() + docs(GATEWAY_API_CRDS).len());
    }
}
