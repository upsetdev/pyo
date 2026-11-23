//! The agent's install manifest: `kubectl apply -f <api>/install/agent.yaml?token=…`.
//!
//! The token in a connect command is a connect token ([`new_connect_token`]):
//! one cluster's, made for its team, good for an hour and for one agent.
//! Fetching the manifest with it makes the agent's own token, which goes
//! only into the manifest (a command run again within the hour, after a
//! failed try, makes another). Once that agent reports in, the connect
//! token is used up. The agent's own token also fetches the manifest
//! again, unchanged: the refresh command reads it from the agent there.

use crate::error::Query;
use axum::{extract::State, http::header, response::IntoResponse};
use serde::Deserialize;

use crate::{
    AppState,
    error::{ApiError, ApiResult},
    ids::sha256_hex,
};

#[derive(Deserialize)]
pub struct InstallQuery {
    token: String,
    /// "busy": something else on the machine has ports 80 and 443 (k3s's
    /// Traefik, nginx…), as connect.sh found before installing anything.
    #[serde(default, rename = "webPorts")]
    web_ports: Option<String>,
}

/// How long a connect command works.
pub const CONNECT_TOKEN_HOURS: i32 = 1;

/// Said when a connect command ran after its hour.
pub const CONNECT_EXPIRED: &str =
    "This connect command expired. Get a new one in Pyo: open the cluster and show its install command.";
/// Said when its agent already connected with it.
pub const CONNECT_USED: &str =
    "This connect command was used already. Get a new one in Pyo: open the cluster and show its install command.";

/// A new connect command's token for a cluster of `team_id`: good for
/// [`CONNECT_TOKEN_HOURS`], once. One before it stops working.
pub async fn new_connect_token(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    cluster_id: &str,
    team_id: &str,
) -> ApiResult<String> {
    let token = crate::ids::secret_token();
    sqlx::query(
        "UPDATE pyo.clusters SET connect_token_hash = $2, connect_token_team = $3,
                connect_token_expires_at = now() + make_interval(hours => $4), connect_token_used_at = NULL
          WHERE id = $1",
    )
    .bind(cluster_id)
    .bind(sha256_hex(&token))
    .bind(team_id)
    .bind(CONNECT_TOKEN_HOURS)
    .execute(&mut **tx)
    .await?;
    Ok(token)
}

/// The cluster's agent reported in: its connect command is used up. (The
/// agent before it was cut off when that command was made.)
pub async fn agent_reported(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    cluster_id: &str,
) -> ApiResult<()> {
    sqlx::query(
        "UPDATE pyo.clusters SET connect_token_used_at = now()
          WHERE id = $1 AND connect_token_hash IS NOT NULL AND connect_token_used_at IS NULL",
    )
    .bind(cluster_id)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// Which cluster a manifest fetch is for, and the agent token its manifest
/// carries: a new one for a connect token, the same one for the agent's own.
async fn token_for_manifest(db: &sqlx::PgPool, token: &str) -> ApiResult<(String, String)> {
    if !token.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(ApiError::not_found("Install token"));
    }
    let hash = sha256_hex(token);
    let mut tx = db.begin().await?;
    let row: Option<(String, bool, bool, bool, bool)> = sqlx::query_as(
        "SELECT id, agent_token_hash = $1, coalesce(connect_token_team = team_id, false),
                coalesce(connect_token_expires_at > now(), false), connect_token_used_at IS NOT NULL
           FROM pyo.clusters WHERE agent_token_hash = $1 OR connect_token_hash = $1 FOR UPDATE",
    )
    .bind(&hash)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((cluster, agent_token, same_team, fresh, used)) = row else {
        return Err(ApiError::not_found("Install token"));
    };
    if agent_token {
        return Ok((cluster, token.to_owned()));
    }
    // A connect token, bound to the team it was made for.
    if !same_team {
        return Err(ApiError::not_found("Install token"));
    }
    if used {
        return Err(ApiError::conflict(CONNECT_USED));
    }
    if !fresh {
        return Err(ApiError::new(axum::http::StatusCode::GONE, CONNECT_EXPIRED));
    }
    // The agent's own token, shown nowhere. Its keys and certificate come
    // with it (enrolled once, with this token).
    let agent = crate::ids::secret_token();
    sqlx::query(
        "UPDATE pyo.clusters SET agent_token_hash = $2, agent_token_used_hash = NULL WHERE id = $1",
    )
    .bind(&cluster)
    .bind(sha256_hex(&agent))
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    tracing::info!(%cluster, "connect command fetched the agent's manifest");
    Ok((cluster, agent))
}

/// Ports 80 and 443 are taken on the machine a cluster is being connected
/// from: it takes no public traffic, so the gateway publishes nothing there
/// and the sites already on it keep working. Only before its agent first
/// reports in (the reply to that first heartbeat carries it, before the
/// gateway is set up). A cluster that's running keeps its setting.
async fn web_ports_busy(db: &sqlx::PgPool, cluster: &str) -> ApiResult<()> {
    let off = sqlx::query(
        "UPDATE pyo.clusters SET traffic = 'tailscale'
          WHERE id = $1 AND last_seen_at IS NULL AND traffic = 'public'",
    )
    .bind(cluster)
    .execute(db)
    .await?
    .rows_affected();
    if off > 0 {
        tracing::info!(%cluster, "ports 80 and 443 are in use where it's being connected: public traffic off");
    }
    Ok(())
}

pub async fn manifest(
    State(s): State<AppState>,
    headers: axum::http::HeaderMap,
    Query(q): Query<InstallQuery>,
) -> ApiResult<impl IntoResponse> {
    let (cluster, token) = token_for_manifest(&s.db, q.token.trim()).await?;
    if q.web_ports.as_deref() == Some("busy") {
        web_ports_busy(&s.db, &cluster).await?;
    }
    // Pyo Cloud: the agent dials the mTLS endpoint, trusts only its CA and
    // runs only commands the signer signed for this cluster (agent_tls.rs,
    // the agent's trust.rs).
    if let Some(endpoint) = crate::agent_tls::endpoint() {
        let signed = crate::agent_auth::mode() == crate::agent_auth::Mode::Signed;
        // mTLS: Pyo's agent CA. Signed requests: the CA of the endpoint's
        // certificate only when it isn't a public one (PYO_AGENT_ENDPOINT_CA_FILE).
        let ca = if signed {
            std::env::var("PYO_AGENT_ENDPOINT_CA_FILE")
                .ok()
                .filter(|f| !f.trim().is_empty())
                .and_then(|f| std::fs::read_to_string(f.trim()).ok())
                .unwrap_or_default()
        } else {
            crate::agent_tls::ca().map(|c| c.pem().to_owned()).unwrap_or_default()
        };
        let mut env = vec![
            ("PYO_AGENT_AUTH", if signed { "signed" } else { "mtls" }.to_owned()),
            ("PYO_CLUSTER_ID", cluster.clone()),
        ];
        if !ca.is_empty() {
            env.push(("PYO_AGENT_CA", ca));
        }
        if let Some(signer) = crate::command_signer() {
            env.push(("PYO_COMMAND_KEY", signer.public_keys()));
        }
        let yaml = render_with(&endpoint, &s.config.agent_image, &token, &env);
        return Ok((
            [
                (header::CONTENT_TYPE, "application/yaml"),
                (header::CACHE_CONTROL, "no-store"),
            ],
            yaml,
        ));
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
    let yaml = render(&api, &s.config.agent_image, &token);
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
/// brings its roles and its admission policy up to date with Pyo by
/// itself. On unless `PYO_AGENT_SELF_UPDATE` is false: with it, whoever
/// controls this Pyo can give the agent any permission on those clusters
/// (pyo-proto's rbac.rs says why that was so before it wrote roles too),
/// so a Pyo serving other people's clusters (Pyo Cloud) turns it off. A
/// cluster's owner can also delete its `pyo-agent-self-update` binding.
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

/// The hash of the install a cluster without the self-update role gets
/// from this Pyo (not the cluster Pyo runs on): [`parts`]`(false)`'s on a
/// Pyo that hands out no self-update role (Pyo Cloud).
pub fn verifier_install() -> &'static str {
    use pyo_proto::system::{db_images_from_env, geoip_image_from_env, waf_image_from_env};
    static HASH: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    HASH.get_or_init(|| {
        pyo_proto::rbac::InstallParts::new(
            false,
            false,
            &waf_image_from_env(),
            &geoip_image_from_env(),
            &db_images_from_env(),
        )
        .hash
    })
}

/// How long an answer of [`version`] may be kept, in seconds.
const VERSION_MAX_AGE: &str = "public, max-age=60";

/// GET /install/version: the version this Pyo runs and the install a
/// cluster without the self-update role gets from it
/// ([`verifier_install`]), as `{"version":"X.Y.Z","install":"<hash>"}`.
///
/// For the verifier on a cluster's first server (`pyo-verify`): it applies
/// only what the signed release of this version defines, so this names a
/// version and nothing more. No key, as `/api/v1/meta` (which says the
/// version too) and the install scripts beside it: nothing here is secret,
/// nothing is read from the database, and the answer may be kept a minute.
pub async fn version() -> impl IntoResponse {
    (
        [(header::CACHE_CONTROL, VERSION_MAX_AGE)],
        crate::error::Json(version_answer(
            env!("CARGO_PKG_VERSION"),
            verifier_install(),
        )),
    )
}

fn version_answer(version: &str, install: &str) -> serde_json::Value {
    serde_json::json!({ "version": version, "install": install })
}

/// The agent's manifest. On the cluster Pyo itself runs on (an in-cluster
/// API address), its agent also runs Pyo's upgrader.
pub fn render(api_url: &str, image: &str, token: &str) -> String {
    render_with(api_url, image, token, &[])
}

/// [`render`], with more of the agent's env (Pyo Cloud's trust settings).
pub fn render_with(api_url: &str, image: &str, token: &str, extra_env: &[(&str, String)]) -> String {
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
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .chain(extra_env.iter().map(|(k, v)| (*k, v.as_str())))
        .map(|(k, v)| {
            format!(
                "            - name: {k}\n              value: {}\n",
                serde_json::Value::from(v)
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
#   with exactly the content this release writes (never its Secrets' content),
#   and its failed pods, deleted. Its role there names no Service, network
#   policy or ConfigMap it writes: the admission policy at the end does, by
#   name and content. On Kubernetes older than 1.30 there is no admission
#   policy, so the agent could change or delete any of those three kinds in
#   pyo-gateway (Envoy Gateway's own included);
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
# - read pyo-system/pyo-verifier, where your server's verifier says how
#   automatic updates stand (when you set them up), never write it;
# - with pyo-agent-self-update bound (on unless this Pyo turns it off):
#   when Pyo is updated, update its own roles and bindings (the ones below,
#   by name, never a new one) and rewrite its own admission policy, so the
#   cluster needs no command after a release. With it, whoever controls
#   this Pyo can give the agent any permission here, up to cluster-admin.
#   That was already so while this role only rewrote the admission policy:
#   the policy is what keeps the agent's roles from privileged pods and
#   from writing other roles. Delete the pyo-agent-self-update
#   ClusterRoleBinding to take all of it away. The agent's roles then
#   change only when you run the connect command. A role the agent can't
#   update (a new one, say) waits for that command, and the rest of a
#   refresh goes on without it.
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
      # It keeps running on a server short of disk or memory, and starts
      # there after an update: it reports that state, and cleans up what
      # the evictions leave. It may also run on a GPU server kept for GPU
      # work. A refresh brings these two to an older install.
      priorityClassName: {priority}
      tolerations: {tolerations}
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
        priority = pyo_proto::rbac::AGENT_PRIORITY,
        tolerations = pyo_proto::rbac::agent_tolerations(),
    )
}

/// The agent's token as its pod template carries it: not the token, nor
/// the hash Pyo looks it up by.
fn token_stamp(token: &str) -> String {
    sha256_hex(&format!("pyo-agent-token\n{token}"))[..16].to_owned()
}

// ── Keeping a cluster's install current ────────────────────────────────────

/// What an outdated install needs, as the cluster's settings say it (where
/// the command is): shown with its "Update needed", and by what waits for it.
pub const OUTDATED: &str = "This cluster's agent needs an update. Run the command under Refresh this cluster in the cluster's settings.";

/// An outdated install that refused the client policies removing
/// client-sent visitor headers: its agent wrote them without that
/// (`AgentScope::headers_kept`).
pub const HEADERS_KEPT: &str = "This cluster's install is older than Pyo's. Run the command under Refresh this cluster in the cluster's settings so client-sent visitor headers are removed.";

/// What an install that's current except for some of its roles needs (a
/// refresh brought the admission policy and the image pins, and couldn't
/// write those roles): shown with its "Update needed", and by the features
/// that wait for them.
pub const ROLES: &str = "This cluster's agent needs an update for some features. Run the command under Refresh this cluster in the cluster's settings.";

/// Whether the system bundles (the databases system, the WAF, Pyo Static)
/// can be installed and updated on a cluster whose install is in `state`:
/// when it's current, or current except for roles they don't need
/// (`bundles`, from `agentInstall.bundles`). Never while it's outdated or
/// updating: its older admission policy refuses this Pyo's bundles.
pub fn bundles_ready(state: Option<&str>, bundles: Option<bool>) -> bool {
    match state {
        None | Some("current") => true,
        Some("roles") => bundles == Some(true),
        _ => false,
    }
}

/// Why an outdated install needs its connect command, when nothing more
/// particular is known.
fn outdated_message(scope: &pyo_proto::rbac::AgentScope) -> &'static str {
    if scope.headers_kept {
        HEADERS_KEPT
    } else {
        OUTDATED
    }
}

/// A deploy that needs what an outdated install can't do.
pub fn outdated_deploy() -> String {
    format!("{OUTDATED} Then deploy again.")
}

/// Whether an error the agent reported is the cluster's install refusing
/// the write: its admission policy, or its roles (Kubernetes' "forbidden").
pub fn refused_by_install(message: &str) -> bool {
    message.contains("ValidatingAdmissionPolicy")
        || message.contains(pyo_proto::bundles::POLICY_REFUSED)
        || (message.contains(" is forbidden: ") && message.contains(" cannot "))
}

/// A command's error as the team reads it: a refusal while the cluster's
/// install (`install`, its state) is outdated, or current except for some
/// roles, is [`OUTDATED`]'s to fix.
pub fn shown(message: Option<&str>, install: &str) -> Option<String> {
    match message {
        Some(m) if matches!(install, "outdated" | "roles") && refused_by_install(m) => {
            Some(OUTDATED.into())
        }
        m => m.map(str::to_owned),
    }
}

/// The cluster's install as its last heartbeat found: "current",
/// "updating", "outdated" or "roles" (current when it never said).
pub async fn state_of(db: impl sqlx::PgExecutor<'_>, cluster_id: &str) -> ApiResult<String> {
    Ok(sqlx::query_scalar::<_, Option<String>>(
        "SELECT capabilities->'agentInstall'->>'state' FROM pyo.clusters WHERE id = $1",
    )
    .bind(cluster_id)
    .fetch_optional(db)
    .await?
    .flatten()
    .unwrap_or_else(|| "current".into()))
}

/// The last refresh Pyo sent a cluster for the install it has now.
#[derive(Debug, Clone, PartialEq)]
pub struct LastRefresh {
    /// The command's: "pending", "sent", "done" or "failed".
    pub status: String,
    pub message: String,
    pub age_secs: i64,
    /// How many refreshes of this install Pyo has sent the cluster.
    pub attempts: i64,
    /// It failed with another version of the agent than the one running
    /// now (an update replaced it): the new agent may well make it.
    pub agent_changed: bool,
}

/// A refresh sent (or done) this long without the install turning current
/// is sent again: after a minute, then twice as long each time, up to
/// [`REFRESH_PATIENCE_MAX_SECS`]. A refresh lost with an agent that
/// restarted is handed out again as soon as the new one polls (agent.rs).
const REFRESH_PATIENCE_SECS: i64 = 60;
const REFRESH_PATIENCE_MAX_SECS: i64 = 10 * 60;
/// One that failed is tried again after this long, or as soon as another
/// version of the agent runs.
const REFRESH_RETRY_SECS: i64 = 30 * 60;

/// Where a cluster's install stands, and whether to send it a refresh:
/// ("current" | "updating" | "outdated" | "roles", why it isn't current,
/// send). "roles": current except for some of its roles.
pub fn install_state(
    scope: &pyo_proto::rbac::AgentScope,
    expected: &str,
    last: Option<&LastRefresh>,
) -> (&'static str, String, bool) {
    if pyo_proto::rbac::install_current(scope, expected) {
        return ("current", String::new(), false);
    }
    // A refresh brought this Pyo's admission policy and image pins, and
    // left roles it couldn't write. Another refresh would leave the same
    // ones, so none is sent: the connect command brings them.
    if scope.admission == "enforced" && scope.install == expected && !scope.roles_older.is_empty() {
        return ("roles", ROLES.into(), false);
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
        // Its roles aren't this Pyo's: the same agent can't make this
        // refresh however often it's sent, so the connect command it is.
        Some(l) if l.status == "failed" && roles_older(&l.message) && !l.agent_changed => {
            ("outdated", String::new(), false)
        }
        Some(l) if l.status == "failed" => {
            if l.age_secs < REFRESH_RETRY_SECS && !l.agent_changed {
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

/// Whether a refresh failed because the agent's roles aren't this Pyo's
/// (pyo-agent's install.rs says so in these words): no agent can change
/// its own roles, so only the cluster's connect command gets it further.
/// A release that changes the roles leaves every cluster there.
pub fn roles_older(message: &str) -> bool {
    message.contains(pyo_proto::rbac::ROLES_OLDER)
}

/// How long the `attempts`th refresh of an install has before another is sent.
fn refresh_patience(attempts: i64) -> i64 {
    let doublings = attempts.clamp(1, 11) - 1;
    (REFRESH_PATIENCE_SECS << doublings).min(REFRESH_PATIENCE_MAX_SECS)
}

/// The latest refresh of `hash` sent to the cluster, if any, against the
/// agent version running now (`agent`).
async fn last_refresh(
    db: impl sqlx::PgExecutor<'_>,
    cluster_id: &str,
    hash: &str,
    agent: &str,
) -> ApiResult<Option<LastRefresh>> {
    let last: Option<(String, String, i64, i64, Option<String>)> = sqlx::query_as(
        "SELECT status, coalesce(result->>'message', ''), extract(epoch FROM now() - created_at)::bigint,
                count(*) OVER (), result->>'agentVersion' FROM pyo.commands
          WHERE cluster_id = $1 AND command->>'kind' = 'refresh_install' AND command->>'hash' = $2 ORDER BY created_at DESC LIMIT 1",
    )
    .bind(cluster_id)
    .bind(hash)
    .fetch_optional(db)
    .await?;
    Ok(
        last.map(|(status, message, age_secs, attempts, by)| LastRefresh {
            status,
            message,
            age_secs,
            attempts,
            agent_changed: agent_changed(by.as_deref(), agent),
        }),
    )
}

/// Whether the agent that answered a refresh (`by`) isn't the one running
/// now. Unknown on either side: no.
fn agent_changed(by: Option<&str>, agent: &str) -> bool {
    by.is_some_and(|by| !by.is_empty() && !agent.is_empty() && by != agent)
}

/// A refresh's answer: which agent version gave it, as the cluster's
/// heartbeat last said, so a failed one is sent again once another runs.
pub async fn answered(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    command_id: &str,
    cluster_id: &str,
) -> ApiResult<()> {
    sqlx::query(
        "UPDATE pyo.commands c SET result = c.result || jsonb_build_object('agentVersion', k.agent_version)
           FROM pyo.clusters k
          WHERE c.id = $1 AND c.cluster_id = $2 AND k.id = c.cluster_id AND k.agent_version IS NOT NULL",
    )
    .bind(command_id)
    .bind(cluster_id)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// Whether the agent is bringing the cluster's install up to this Pyo's
/// (`scope` as it last reported it): its image is moved only after that
/// (updates.rs), so the new agent starts under the admission policy it's
/// made for.
pub async fn refreshing(
    db: &sqlx::PgPool,
    cluster_id: &str,
    scope: &pyo_proto::rbac::AgentScope,
    agent: &str,
    same_cluster: bool,
) -> ApiResult<bool> {
    let hash = &parts(same_cluster).hash;
    let last = last_refresh(db, cluster_id, hash, agent).await?;
    Ok(install_state(scope, hash, last.as_ref()).0 == "updating")
}

/// From a heartbeat: where the cluster's install stands against this Pyo's,
/// as `capabilities.agentInstall` (`{state, message, command}`, and in
/// state "roles" `bundles`: whether the roles left behind leave the system
/// bundles free, see [`bundles_ready`]), sending
/// the agent (version `agent`) a refresh when it can make one. Returns it, and whether a
/// command was queued. `command`, in every state, is the connect command
/// run again on the cluster's first server: it also applies Pyo's latest
/// host settings (the memory reserve for k3s), which no agent can.
pub async fn sync(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    cluster_id: &str,
    scope: Option<&pyo_proto::rbac::AgentScope>,
    agent: &str,
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
    let last = last_refresh(&mut **tx, cluster_id, &parts.hash, agent).await?;
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
    // The cluster Pyo runs on: its server runs the command by itself.
    let host = host_refreshes(same_cluster, self_update(), scope, state);
    let stamp = host_stamp(&parts.hash);
    let (state, message) = if host && host_at_work(tx, &stamp).await? {
        ("updating", String::new())
    } else {
        (state, message)
    };
    let out = install_shown(
        scope,
        state,
        &message,
        command(same_cluster),
        host.then_some(stamp.as_str()),
        unix_now(),
    );
    Ok((out, send))
}

/// `capabilities.agentInstall` for a cluster whose install is in `state`
/// (see [`sync`]): `host_stamp` when its own server refreshes it.
/// `autoUpdate` ([`auto_update`]) is added last and is only ever shown:
/// everything else here is the same whatever the verifier's status says.
fn install_shown(
    scope: &pyo_proto::rbac::AgentScope,
    state: &str,
    message: &str,
    command: String,
    host_stamp: Option<&str>,
    now: i64,
) -> serde_json::Value {
    use serde_json::json;
    let mut out = json!({ "state": state, "command": command });
    if let Some(stamp) = host_stamp {
        out["hostRefresh"] = json!(stamp);
    }
    if state == "roles" {
        out["bundles"] = json!(!pyo_proto::rbac::bundles_held_back(&scope.roles_older));
    }
    if !message.is_empty() {
        out["message"] = json!(message);
    } else if state == "outdated" {
        out["message"] = json!(outdated_message(scope));
    }
    if let Some(auto) = auto_update(scope, state, now) {
        out["autoUpdate"] = auto;
    }
    out
}

// ── Automatic updates, as the server's verifier reports them ───────────────
//
// A cluster without the self-update role (Pyo Cloud's) gets a release's
// roles, admission policy and image pins only from its connect command.
// With `--auto-update` that command also sets up the verifier
// (`pyo-verify`) on the cluster's first server: a root program on a timer
// that asks GET /install/version which version this Pyo runs, checks that
// version's signed release, and applies only what that release defines.
// Pyo can name a version. It can't turn the verifier on or off, make it
// run, or hand it anything to apply: the customer's switches are theirs
// (`updates: "hold"` in pyo-system/pyo-policy, /etc/pyo/verify.conf).
//
// 0.51.0 ships the verifier's bootstrap only: it checks the signed
// release, never moves backwards, replaces itself with that release's
// verifier, and writes its status. It applies nothing to the cluster yet,
// so an outdated install still shows "Update needed" with its command.
//
// Its status reaches Pyo through the agent (`AgentScope::verifier`), so an
// agent that was broken into can say anything there. It only ever adds
// `autoUpdate` to what the cluster's settings show ([`auto_update`]). The
// install's state, the refresh Pyo sends, what waits for the connect
// command and what the host does all follow from the rest of the scope.

/// A status the verifier last wrote longer ago than this counts as no
/// verifier: its timer stopped, or it was removed.
pub const VERIFIER_STALE_SECS: i64 = 48 * 60 * 60;

pub const AUTO_UPDATE_ON: &str = "Automatic updates are set up on this cluster";
pub const AUTO_UPDATE_NEEDS_COMMAND: &str = "This update still needs the command.";
pub const AUTO_UPDATE_HOLD_SERVER: &str =
    "Automatic updates are on hold. They were turned off on the server.";
pub const AUTO_UPDATE_HOLD_POLICY: &str =
    "Automatic updates are on hold. They were turned off in this cluster's policy.";
pub const AUTO_UPDATE_PROBLEM: &str = "Automatic updates reported a problem:";
pub const AUTO_UPDATE_FALLBACK: &str = "Run the command to update this cluster.";

/// Seconds since 1970 of an RFC 3339 time ("2026-11-23T10:00:00Z", with or
/// without a fraction, `Z` or an offset). None when it isn't one.
fn rfc3339_secs(s: &str) -> Option<i64> {
    let s = s.trim();
    if !s.is_ascii() || s.len() < 20 {
        return None;
    }
    let b = s.as_bytes();
    let num = |from: usize, to: usize| -> Option<i64> {
        let part = s.get(from..to)?;
        part.bytes()
            .all(|c| c.is_ascii_digit())
            .then(|| part.parse::<i64>().ok())
            .flatten()
    };
    if b[4] != b'-' || b[7] != b'-' || !matches!(b[10], b'T' | b't' | b' ') || b[13] != b':' || b[16] != b':' {
        return None;
    }
    let (y, m, d) = (num(0, 4)?, num(5, 7)?, num(8, 10)?);
    let (h, min, sec) = (num(11, 13)?, num(14, 16)?, num(17, 19)?);
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) || h > 23 || min > 59 || sec > 60 {
        return None;
    }
    let mut rest = &s[19..];
    if let Some(fraction) = rest.strip_prefix('.') {
        let digits = fraction.bytes().take_while(u8::is_ascii_digit).count();
        if digits == 0 {
            return None;
        }
        rest = &fraction[digits..];
    }
    let offset = match rest {
        "Z" | "z" => 0,
        o if o.len() == 6 && o.as_bytes()[3] == b':' => {
            let sign = match o.as_bytes()[0] {
                b'+' => 1,
                b'-' => -1,
                _ => return None,
            };
            let part = |r: std::ops::Range<usize>| -> Option<i64> {
                let p = o.get(r)?;
                p.bytes().all(|c| c.is_ascii_digit()).then(|| p.parse::<i64>().ok()).flatten()
            };
            let (oh, om) = (part(1..3)?, part(4..6)?);
            if oh > 23 || om > 59 {
                return None;
            }
            sign * (oh * 3600 + om * 60)
        }
        _ => return None,
    };
    // Days since 1970-01-01 of a date in the Gregorian calendar.
    let (y, m) = if m <= 2 { (y - 1, m + 9) } else { (y, m - 3) };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let doy = (153 * m + 2) / 5 + d - 1;
    let days = era * 146_097 + yoe * 365 + yoe / 4 - yoe / 100 + doy - 719_468;
    Some(days * 86_400 + h * 3600 + min * 60 + sec - offset)
}

/// The verifier's status when it counts: the cluster has no self-update
/// role (its agent refreshes it otherwise), and the verifier checked
/// within [`VERIFIER_STALE_SECS`] of `now`. A time that can't be read is
/// as good as none.
fn verifier_of(
    scope: &pyo_proto::rbac::AgentScope,
    now: i64,
) -> Option<&pyo_proto::rbac::VerifierStatus> {
    let v = scope.verifier.as_ref().filter(|_| !scope.self_update)?;
    let checked = rfc3339_secs(&v.checked)?;
    (now - checked <= VERIFIER_STALE_SECS).then_some(v)
}

/// Text from the verifier, as shown: one line, no control characters, at
/// most `max` characters.
fn shown_text(s: &str, max: usize) -> String {
    let line = s
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect::<String>();
    let line = line.split_whitespace().collect::<Vec<_>>().join(" ");
    match line.char_indices().nth(max) {
        Some((at, _)) => format!("{}…", line[..at].trim_end()),
        None => line,
    }
}

/// A version as the verifier names it, when it looks like one.
fn shown_version(s: &str) -> Option<&str> {
    let s = s.trim().trim_start_matches('v');
    (!s.is_empty()
        && s.len() <= 32
        && s.starts_with(|c: char| c.is_ascii_digit())
        && s.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'+')))
    .then_some(s)
}

/// How automatic updates stand on a cluster without the self-update role,
/// for its settings: `{state, message, version?}`, none without a
/// verifier that checked lately ([`verifier_of`]). `state`:
///
/// - "on": the verifier runs. While the install waits for the connect
///   command (`install`: its state), the message says the command is
///   still needed: the bootstrap applies nothing to the cluster.
/// - "hold": turned off by the customer, on the server or in the policy.
/// - "error": the verifier's last check failed, with its own words.
///
/// Only ever shown: see the section's note.
pub fn auto_update(
    scope: &pyo_proto::rbac::AgentScope,
    install: &str,
    now: i64,
) -> Option<serde_json::Value> {
    let v = verifier_of(scope, now)?;
    let waits = matches!(install, "outdated" | "roles");
    let version = shown_version(&v.version);
    let error = shown_text(&v.error, 300);
    let (state, message) = if scope.policy.updates_hold {
        ("hold", AUTO_UPDATE_HOLD_POLICY.to_owned())
    } else if v.hold {
        ("hold", AUTO_UPDATE_HOLD_SERVER.to_owned())
    } else if !error.is_empty() {
        let end = if error.ends_with(['.', '!', '?', '…']) { "" } else { "." };
        let fallback = if waits { format!(" {AUTO_UPDATE_FALLBACK}") } else { String::new() };
        ("error", format!("{AUTO_UPDATE_PROBLEM} {error}{end}{fallback}"))
    } else {
        let on = match version {
            Some(v) => format!("{AUTO_UPDATE_ON} (version {v})."),
            None => format!("{AUTO_UPDATE_ON}."),
        };
        ("on", if waits { format!("{on} {AUTO_UPDATE_NEEDS_COMMAND}") } else { on })
    };
    let mut out = serde_json::json!({ "state": state, "message": message });
    if let Some(version) = version {
        out["version"] = serde_json::json!(version);
    }
    Some(out)
}

// ── The cluster Pyo runs on, refreshed by its own server ───────────────────
//
// What no agent can bring (a new role object, a chart installed with the
// agent, host settings) comes with the connect command. On the server Pyo
// runs on, the host's timer (install.sh `host_units`) runs that command by
// itself: it asks GET /install/this-machine every minute, which says
// `"refresh": true` while this is so, and reports each step to POST
// /install/this-machine/refresh. Meanwhile the cluster shows "updating",
// not "Update needed". The host runs it once per install ([`host_stamp`]),
// again about hourly after a failure, and a few times a day at most.

/// Whether the server Pyo runs on refreshes its cluster's install by
/// itself: the install waits for the connect command (`state`), and both
/// this installation (`self_update`, off on Pyo Cloud) and the cluster's
/// owner (the self-update role is bound, the cluster isn't read only) let
/// Pyo keep it current.
pub fn host_refreshes(
    same_cluster: bool,
    self_update: bool,
    scope: &pyo_proto::rbac::AgentScope,
    state: &str,
) -> bool {
    same_cluster && host_may_refresh(self_update, scope) && matches!(state, "outdated" | "roles")
}

/// Whether the server Pyo runs on may run the connect command for its
/// cluster by itself at all (see [`host_refreshes`]).
fn host_may_refresh(self_update: bool, scope: &pyo_proto::rbac::AgentScope) -> bool {
    self_update && scope.self_update && !scope.policy.read_only
}

/// What the host counts its automatic runs by: this Pyo's version and the
/// install (`hash`) it brings. One run each, unless it failed.
pub fn host_stamp(hash: &str) -> String {
    format!("{}-{hash}", env!("CARGO_PKG_VERSION"))
}

/// What the host last said of its refresh (`pyo.settings` 'host_refresh').
#[derive(Debug, Clone, PartialEq)]
pub struct HostRefresh {
    /// The [`host_stamp`] it's about.
    pub install: String,
    /// "waiting" (Pyo's own, until the host says more), "running", "done",
    /// "failed" or "held" (it ran for this install already, or too often
    /// today).
    pub status: String,
    pub age_secs: i64,
}

/// The host takes this long to start (its timer runs every minute; after
/// an update its scripts come first), and after it's done the agent this
/// long to say the install is current.
const HOST_WAIT_SECS: i64 = 10 * 60;
/// A run takes minutes, more when k3s restarts.
const HOST_RUN_SECS: i64 = 30 * 60;

/// Whether the host is about to refresh the install `stamp`, or at it
/// (`last`: its last word). Not once it failed, gave up, or took too long:
/// the cluster then says "Update needed" with its command, as any other.
/// A server without the timer (set up before Pyo 0.4.2 and never since)
/// gets there after [`HOST_WAIT_SECS`].
pub fn host_working(stamp: &str, last: Option<&HostRefresh>) -> bool {
    match last {
        Some(l) if l.install == stamp => match l.status.as_str() {
            "waiting" | "done" => l.age_secs < HOST_WAIT_SECS,
            "running" => l.age_secs < HOST_RUN_SECS,
            _ => false,
        },
        // Nothing said of this install yet: asked from now on.
        _ => true,
    }
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

/// [`HostRefresh`] from its setting.
fn host_refresh_from(value: &serde_json::Value, now: i64) -> Option<HostRefresh> {
    Some(HostRefresh {
        install: value["install"].as_str()?.to_owned(),
        status: value["status"].as_str()?.to_owned(),
        age_secs: (now - value["at"].as_i64()?).max(0),
    })
}

/// The setting's next value when the host (or Pyo, "waiting") says
/// `status` of `install`: none when it said the same before, so the time
/// stays the first one's.
fn host_refresh_next(
    last: Option<&HostRefresh>,
    install: &str,
    status: &str,
    now: i64,
) -> Option<serde_json::Value> {
    if last.is_some_and(|l| l.install == install && l.status == status) {
        return None;
    }
    Some(serde_json::json!({ "install": install, "status": status, "at": now }))
}

async fn host_refresh_get(db: impl sqlx::PgExecutor<'_>) -> ApiResult<Option<HostRefresh>> {
    let v: Option<serde_json::Value> =
        sqlx::query_scalar("SELECT value FROM pyo.settings WHERE key = 'host_refresh'")
            .fetch_optional(db)
            .await?;
    Ok(v.and_then(|v| host_refresh_from(&v, unix_now())))
}

async fn host_refresh_set(db: impl sqlx::PgExecutor<'_>, value: serde_json::Value) -> ApiResult<()> {
    sqlx::query("INSERT INTO pyo.settings (key, value) VALUES ('host_refresh', $1) ON CONFLICT (key) DO UPDATE SET value = excluded.value")
        .bind(value)
        .execute(db)
        .await?;
    Ok(())
}

/// [`host_working`] for the install `stamp`, noting when Pyo first waited
/// for the host to refresh it.
async fn host_at_work(db: &mut sqlx::PgConnection, stamp: &str) -> ApiResult<bool> {
    let last = host_refresh_get(&mut *db).await?;
    if last.as_ref().is_none_or(|l| l.install != stamp) {
        if let Some(v) = host_refresh_next(last.as_ref(), stamp, "waiting", unix_now()) {
            host_refresh_set(&mut *db, v).await?;
        }
        return Ok(true);
    }
    Ok(host_working(stamp, last.as_ref()))
}

/// What GET /install/this-machine adds for a connected machine: `install`,
/// this Pyo's [`host_stamp`], and `refresh` when the host should run the
/// connect command now (`asked`: the `hostRefresh` its cluster's last
/// heartbeat left, for this Pyo's install or an earlier one). Nothing when
/// the host may not refresh its cluster (`may`, [`host_may_refresh`]):
/// install.sh then leaves the cluster's install alone after an update too.
fn host_answer(may: bool, stamp: &str, asked: Option<&str>) -> serde_json::Value {
    let mut out = serde_json::json!({ "state": "connected" });
    if may {
        out["install"] = serde_json::json!(stamp);
        if asked == Some(stamp) {
            out["refresh"] = serde_json::json!(true);
        }
    }
    out
}

/// A [`host_stamp`], as the host sends it back: a version and a hash.
fn stamp_like(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 100
        && s.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'+' | b'_'))
}

#[derive(Deserialize)]
pub struct ThisMachineRefresh {
    key: String,
    #[serde(default)]
    install: String,
    #[serde(default)]
    status: String,
}

/// POST /install/this-machine/refresh (form: key, install, status): how
/// the host's refresh of the install `install` goes ("running", "done",
/// "failed" or "held"), from its timer, with the bootstrap key as for
/// [`this_machine`]. Only what the cluster's settings show follows from it.
pub async fn this_machine_refresh(
    State(s): State<AppState>,
    axum::Form(f): axum::Form<ThisMachineRefresh>,
) -> ApiResult<crate::error::Json<serde_json::Value>> {
    bootstrap_key(&f.key)?;
    if !stamp_like(&f.install) || !matches!(f.status.as_str(), "running" | "done" | "failed" | "held") {
        return Err(ApiError::bad_request("Not a refresh's status."));
    }
    let last = host_refresh_get(&s.db).await?;
    if let Some(v) = host_refresh_next(last.as_ref(), &f.install, &f.status, unix_now()) {
        host_refresh_set(&s.db, v).await?;
        tracing::info!(install = %f.install, status = %f.status, "this machine: its refresh");
    }
    Ok(crate::error::Json(serde_json::json!({})))
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
            agent_changed: false,
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

    /// A cluster whose install refused the client policies that remove
    /// visitor headers says so, and what to run.
    #[test]
    fn an_install_that_keeps_visitor_headers_says_so() {
        let mut s = scope("old", false);
        assert_eq!(install_state(&s, "h1", None).0, "outdated");
        assert_eq!(outdated_message(&s), OUTDATED);
        s.headers_kept = true;
        assert_eq!(outdated_message(&s), HEADERS_KEPT);
        assert!(HEADERS_KEPT.contains("Refresh this cluster"));
        assert!(!HEADERS_KEPT.contains(';'));
        // As the agent sends it, and as older agents (without it) do.
        let sent: AgentScope = serde_json::from_value(serde_json::json!({
            "access": "limited", "admission": "enforced", "headersKept": true,
        }))
        .unwrap();
        assert!(sent.headers_kept);
        let older: AgentScope =
            serde_json::from_value(serde_json::json!({ "access": "limited", "admission": "enforced" }))
                .unwrap();
        assert!(!older.headers_kept);
        assert!(serde_json::to_value(&older).unwrap().get("headersKept").is_none());
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

    /// The refusals a v0.2 install gave a v0.3 Pyo's deploys, and errors
    /// that aren't the install's.
    #[test]
    fn refusals_by_the_install_are_told_apart() {
        assert!(refused_by_install(
            "HTTPRoute web-tn: admission webhook \"validation.policy\" denied the request: ValidatingAdmissionPolicy 'pyo-agent' with binding 'pyo-agent' denied request: failed expression: gatewayAllowed"
        ));
        assert!(refused_by_install(
            "clienttrafficpolicies.gateway.envoyproxy.io \"web-cdn\" is forbidden: User \"system:serviceaccount:pyo-system:pyo-agent\" cannot patch resource \"clienttrafficpolicies\""
        ));
        assert!(!refused_by_install("ImagePullBackOff: pull access denied"));
        assert!(!refused_by_install(
            "namespaces \"p\" is forbidden: unable to create new content in namespace p because it is being terminated"
        ));
        assert!(refused_by_install(&format!(
            "Deployment pyo-static: {}. Run the cluster's connect command again.",
            pyo_proto::bundles::POLICY_REFUSED
        )));
        assert!(outdated_deploy().starts_with(OUTDATED));
        let refused = "ValidatingAdmissionPolicy 'pyo-agent' denied request";
        assert_eq!(shown(Some(refused), "outdated").as_deref(), Some(OUTDATED));
        assert_eq!(shown(Some(refused), "current").as_deref(), Some(refused));
        assert_eq!(
            shown(Some("timed out"), "outdated").as_deref(),
            Some("timed out")
        );
        assert_eq!(shown(None, "outdated"), None);
        assert!(OUTDATED.contains("Refresh this cluster"));
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
    fn a_failed_refresh_is_sent_again_once_another_agent_runs() {
        let s = scope("h0", true);
        let failed = LastRefresh {
            agent_changed: true,
            ..last("failed", 60)
        };
        assert_eq!(
            install_state(&s, "h1", Some(&failed)),
            ("updating", String::new(), true)
        );
        assert!(agent_changed(Some("0.3.1"), "0.3.2"));
        assert!(!agent_changed(Some("0.3.2"), "0.3.2"));
        assert!(!agent_changed(None, "0.3.2"));
        assert!(!agent_changed(Some("0.3.1"), ""));
    }

    /// A release that changes the agent's roles (0.51.0's Cloudflare
    /// Tunnel Services): the refresh fails on them, on any cluster. It
    /// isn't sent again every half hour, the install stays outdated (never
    /// "updating" in between) and the cluster's settings say what to run,
    /// in Pyo's words.
    #[test]
    fn a_refresh_that_failed_on_the_roles_waits_for_the_connect_command() {
        let s = scope("h0", true);
        let on_roles = |age_secs: i64| LastRefresh {
            message: "this agent's roles are older than Pyo's (Role pyo-agent differs); run the cluster's connect command to bring them up to date".into(),
            ..last("failed", age_secs)
        };
        for age in [5, 60, REFRESH_RETRY_SECS, 30 * REFRESH_RETRY_SECS] {
            assert_eq!(
                install_state(&s, "h1", Some(&on_roles(age))),
                ("outdated", String::new(), false),
                "{age}"
            );
        }
        // With no message of its own, the cluster's settings say Pyo's.
        assert_eq!(outdated_message(&s), OUTDATED);
        assert!(OUTDATED.contains("Refresh this cluster") && !OUTDATED.contains(';'));
        // Another version of the agent runs: it gets one try.
        let other = LastRefresh {
            agent_changed: true,
            ..on_roles(60)
        };
        assert_eq!(
            install_state(&s, "h1", Some(&other)),
            ("updating", String::new(), true)
        );
        // Any other failure is still tried again later.
        assert!(roles_older(&on_roles(0).message));
        assert!(!roles_older("couldn't read its Role pyo-agent (timed out); run the cluster's connect command to bring them up to date"));
        assert!(install_state(&s, "h1", Some(&last("failed", REFRESH_RETRY_SECS))).2);
        // The install the connect command brings is current again.
        assert_eq!(install_state(&scope("h1", true), "h1", Some(&on_roles(60))).0, "current");
    }

    fn partial(install: &str, self_update: bool, older: &[&str]) -> AgentScope {
        AgentScope {
            roles_older: older.iter().map(|s| s.to_string()).collect(),
            ..scope(install, self_update)
        }
    }

    /// A refresh that brought all but some roles: its own state, with its
    /// own message, and no refresh sent again whatever came before.
    #[test]
    fn an_install_current_except_its_roles_says_so_and_gets_no_refresh() {
        let s = partial("h1", true, &["Role/pyo-gateway/pyo-agent"]);
        for l in [
            None,
            Some(last("done", 5)),
            Some(last("done", 30 * REFRESH_RETRY_SECS)),
            Some(last("failed", 30 * REFRESH_RETRY_SECS)),
            Some(LastRefresh { agent_changed: true, ..last("failed", 5) }),
            Some(LastRefresh { attempts: 9, ..last("sent", REFRESH_PATIENCE_MAX_SECS) }),
        ] {
            assert_eq!(install_state(&s, "h1", l.as_ref()), ("roles", ROLES.to_owned(), false), "{l:?}");
        }
        assert!(ROLES.contains("Refresh this cluster") && !ROLES.contains(';') && !ROLES.contains(" - "));
        // The same without the self-update role, or read only.
        assert_eq!(install_state(&partial("h1", false, &["x"]), "h1", None).0, "roles");
        // Pyo moves on: the install is older again, and refreshed as any.
        assert_eq!(install_state(&s, "h2", None), ("updating", String::new(), true));
        assert_eq!(install_state(&partial("h1", false, &["x"]), "h2", None), ("outdated", String::new(), false));
        // The connect command brings the roles: current.
        assert_eq!(install_state(&scope("h1", true), "h1", None).0, "current");
        // What its roles refuse reads as the connect command's to fix.
        let forbidden = "services \"pyo-tunnel-origin\" is forbidden: User \"system:serviceaccount:pyo-system:pyo-agent\" cannot patch resource";
        assert_eq!(shown(Some(forbidden), "roles").as_deref(), Some(OUTDATED));
        assert_eq!(shown(Some("image not found"), "roles").as_deref(), Some("image not found"));
        // The bundles go on when the roles left behind aren't theirs.
        assert!(bundles_ready(None, None));
        assert!(bundles_ready(Some("current"), None));
        assert!(bundles_ready(Some("roles"), Some(true)));
        assert!(!bundles_ready(Some("roles"), Some(false)));
        assert!(!bundles_ready(Some("roles"), None));
        assert!(!bundles_ready(Some("outdated"), Some(true)));
        assert!(!bundles_ready(Some("updating"), None));
    }

    /// The whole way from 0.5.5, as each heartbeat sees it. Nothing loops,
    /// one command brings it home, and the release after needs none.
    #[test]
    fn from_0_5_5_one_command_then_none() {
        let was = "this agent's roles are older than Pyo's (Role pyo-agent differs); run the cluster's connect command to bring them up to date";
        // Pyo is now 0.51.0 (install h1). 0.5.5's agent gets one refresh.
        let old = scope("h0", true);
        assert_eq!(install_state(&old, "h1", None), ("updating", String::new(), true));
        // It never writes roles: it fails, and isn't sent another.
        let failed = LastRefresh { message: was.into(), ..last("failed", 5) };
        assert_eq!(install_state(&old, "h1", Some(&failed)), ("outdated", String::new(), false));
        // So the agent's image moves on (updates.rs waits only while "updating").
        // 0.51.0's agent runs: one more try.
        let moved = LastRefresh { agent_changed: true, ..failed.clone() };
        assert_eq!(install_state(&old, "h1", Some(&moved)), ("updating", String::new(), true));
        // 0.5.5's self-update role may not write roles, so that refresh
        // brings the policy and the pins and leaves the roles: said once,
        // with nothing more sent, however long it stays so.
        let left = partial("h1", true, &["Role/pyo-gateway/pyo-agent", "ClusterRole/pyo-agent-self-update"]);
        for age in [5, REFRESH_PATIENCE_MAX_SECS, 30 * REFRESH_RETRY_SECS] {
            assert_eq!(install_state(&left, "h1", Some(&last("done", age))), ("roles", ROLES.to_owned(), false));
        }
        // Its bundles' roles are as they were, so the databases system goes on.
        assert!(!pyo_proto::rbac::bundles_held_back(&left.roles_older));
        // The owner runs the connect command, once.
        let home = scope("h1", true);
        assert_eq!(install_state(&home, "h1", Some(&last("done", 600))).0, "current");
        // A later release (h2) with other rules in the same roles: one
        // refresh, which the agent makes whole. No command.
        assert_eq!(install_state(&home, "h2", None), ("updating", String::new(), true));
        assert_eq!(install_state(&scope("h2", true), "h2", Some(&last("done", 5))).0, "current");
        // On Pyo Cloud (no self-update role) every step is the command's:
        // no refresh is ever sent, so no partial one either.
        let cloud = scope("h0", false);
        for l in [None, Some(last("done", 5)), Some(moved.clone())] {
            assert_eq!(install_state(&cloud, "h1", l.as_ref()), ("outdated", String::new(), false));
        }
    }

    /// Pyo's own server runs the connect command for its cluster when the
    /// install waits for it, and nowhere else.
    #[test]
    fn only_pyo_s_own_cluster_is_refreshed_by_its_server() {
        let s = scope("h0", true);
        for state in ["outdated", "roles"] {
            assert!(host_refreshes(true, true, &s, state), "{state}");
            // Another cluster, or Pyo Cloud (self-update off).
            assert!(!host_refreshes(false, true, &s, state), "{state}");
            assert!(!host_refreshes(true, false, &s, state), "{state}");
            // Its owner took the self-update role away, or made it read only.
            assert!(!host_refreshes(true, true, &scope("h0", false), state), "{state}");
            let mut read_only = scope("h0", true);
            read_only.policy.read_only = true;
            assert!(!host_refreshes(true, true, &read_only, state), "{state}");
        }
        // Current, or the agent is still at its own refresh.
        for state in ["current", "updating"] {
            assert!(!host_refreshes(true, true, &s, state), "{state}");
        }
        // A missing admission policy is the command's too.
        let mut missing = scope("h0", true);
        missing.admission = "missing".into();
        let (state, _, send) = install_state(&missing, "h1", None);
        assert!(!send && host_refreshes(true, true, &missing, state));
    }

    fn said(install: &str, status: &str, age_secs: i64) -> HostRefresh {
        HostRefresh { install: install.into(), status: status.into(), age_secs }
    }

    /// The cluster says "updating" while its server is about to run the
    /// command or runs it, and "Update needed" once that failed, was given
    /// up, or never came.
    #[test]
    fn the_cluster_waits_for_its_server_then_asks_for_the_command() {
        // Nothing said yet, or only of an earlier install: it will.
        assert!(host_working("1-h1", None));
        assert!(host_working("1-h1", Some(&said("1-h0", "failed", 5))));
        assert!(host_working("1-h1", Some(&said("1-h0", "held", 99_999))));
        // Asked, running, done: for a while each.
        assert!(host_working("1-h1", Some(&said("1-h1", "waiting", HOST_WAIT_SECS - 1))));
        assert!(!host_working("1-h1", Some(&said("1-h1", "waiting", HOST_WAIT_SECS))));
        assert!(host_working("1-h1", Some(&said("1-h1", "running", HOST_RUN_SECS - 1))));
        assert!(!host_working("1-h1", Some(&said("1-h1", "running", HOST_RUN_SECS))));
        assert!(host_working("1-h1", Some(&said("1-h1", "done", HOST_WAIT_SECS - 1))));
        assert!(!host_working("1-h1", Some(&said("1-h1", "done", HOST_WAIT_SECS))));
        // Failed, or not run again: the command's pointer at once.
        for status in ["failed", "held", "something else"] {
            assert!(!host_working("1-h1", Some(&said("1-h1", status, 0))), "{status}");
        }
    }

    /// The host's word is kept with the time it was first said, so a
    /// status repeated every minute doesn't start its wait again.
    #[test]
    fn the_host_s_word_keeps_its_first_time() {
        let v = host_refresh_next(None, "1-h1", "waiting", 100).unwrap();
        assert_eq!(v, serde_json::json!({ "install": "1-h1", "status": "waiting", "at": 100 }));
        let last = host_refresh_from(&v, 160).unwrap();
        assert_eq!(last, said("1-h1", "waiting", 60));
        assert_eq!(host_refresh_next(Some(&last), "1-h1", "waiting", 160), None);
        // Another status, or another install: from now.
        assert_eq!(
            host_refresh_next(Some(&last), "1-h1", "running", 160).unwrap()["at"],
            serde_json::json!(160)
        );
        assert!(host_refresh_next(Some(&last), "1-h2", "waiting", 160).is_some());
        // A clock set back never makes it negative, and other shapes are none.
        assert_eq!(host_refresh_from(&v, 40).unwrap().age_secs, 0);
        assert_eq!(host_refresh_from(&serde_json::json!({ "install": "x" }), 0), None);
    }

    /// What the host's timer reads: `refresh` only for this Pyo's install,
    /// and nothing at all on an installation that doesn't self-update.
    #[test]
    fn the_host_is_told_to_refresh_only_this_pyo_s_install() {
        use serde_json::json;
        let stamp = host_stamp("abc123");
        assert_eq!(stamp, concat!(env!("CARGO_PKG_VERSION"), "-abc123"));
        assert!(stamp_like(&stamp));
        assert_eq!(
            host_answer(true, &stamp, Some(&stamp)),
            json!({ "state": "connected", "install": stamp, "refresh": true })
        );
        // Current, or asked by the Pyo before this one.
        for asked in [None, Some("0.0.1-abc123")] {
            assert_eq!(
                host_answer(true, &stamp, asked),
                json!({ "state": "connected", "install": stamp })
            );
        }
        // Pyo Cloud, or a cluster its owner keeps Pyo from changing: as before.
        assert_eq!(host_answer(false, &stamp, Some(&stamp)), json!({ "state": "connected" }));
        assert!(host_may_refresh(true, &scope("h0", true)));
        assert!(!host_may_refresh(false, &scope("h0", true)));
        assert!(!host_may_refresh(true, &scope("h0", false)));
        // The host's script reads these as plain text.
        let text = host_answer(true, &stamp, Some(&stamp)).to_string();
        assert!(text.contains(r#""state":"connected""#), "{text}");
        assert!(text.contains(r#""refresh":true"#), "{text}");
        assert!(text.contains(&format!(r#""install":"{stamp}""#)), "{text}");
        // And only a stamp comes back from it.
        assert!(stamp_like("0.51.0-rc.1+b_2-0a1b"));
        for bad in ["", "a b", "a\"b", "a/b", "$(x)", &"a".repeat(101)] {
            assert!(!stamp_like(bad), "{bad}");
        }
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

    // ── The verifier ───────────────────────────────────────────────────

    /// 2026-11-23T10:00:00Z.
    const NOW: i64 = 1_795_428_000;

    fn verifier(checked: &str) -> pyo_proto::rbac::VerifierStatus {
        pyo_proto::rbac::VerifierStatus {
            version: "0.51.0".into(),
            live: "0.51.0".into(),
            checked: checked.into(),
            error: String::new(),
            hold: false,
            host: "server-1".into(),
        }
    }

    /// A cluster without the self-update role whose verifier checked an
    /// hour ago.
    fn verified(install: &str) -> AgentScope {
        AgentScope {
            verifier: Some(verifier("2026-11-23T09:00:00Z")),
            ..scope(install, false)
        }
    }

    #[test]
    fn rfc_3339_times_are_read() {
        assert_eq!(rfc3339_secs("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(rfc3339_secs("2026-11-23T10:00:00Z"), Some(NOW));
        assert_eq!(rfc3339_secs(" 2026-11-23T10:00:00.123456789Z "), Some(NOW));
        assert_eq!(rfc3339_secs("2026-11-23T17:00:00+07:00"), Some(NOW));
        assert_eq!(rfc3339_secs("2026-11-23T05:30:00-04:30"), Some(NOW));
        assert_eq!(rfc3339_secs("2026-11-23t10:00:00z"), Some(NOW));
        assert_eq!(rfc3339_secs("2024-02-29T00:00:00Z"), Some(1_709_164_800));
        assert_eq!(rfc3339_secs("2000-03-01T00:00:00Z"), Some(951_868_800));
        for bad in [
            "",
            "yesterday",
            "2026-11-23",
            "2026-11-23T10:00:00",
            "2026-11-23T10:00:00.Z",
            "2026-13-01T00:00:00Z",
            "2026-11-23T24:00:00Z",
            "2026-11-23T10:00:00+0700",
            "2026-11-23T10:00:00*07:00",
            "+026-11-23T10:00:00Z",
            "2026-11-23T10:00:00Zjunk",
            "２０２６-11-23T10:00:00Z",
            "1795428000",
        ] {
            assert_eq!(rfc3339_secs(bad), None, "{bad}");
        }
    }

    /// A status older than 48 hours counts as no verifier, and so does one
    /// whose time can't be read. A cluster with the self-update role is
    /// kept current by its agent, so its status isn't shown either.
    #[test]
    fn a_verifier_that_last_checked_two_days_ago_counts_as_none() {
        let at = |checked: &str| {
            let mut s = verified("h");
            s.verifier = Some(verifier(checked));
            auto_update(&s, "current", NOW)
        };
        assert!(at("2026-11-23T09:59:59Z").is_some());
        // Exactly 48 hours: still counts. A second more: gone.
        assert!(at("2026-11-21T10:00:00Z").is_some());
        assert!(at("2026-11-21T09:59:59Z").is_none());
        assert!(at("2026-01-01T00:00:00Z").is_none());
        // The server's clock ahead of Pyo's: fine.
        assert!(at("2026-11-23T10:05:00Z").is_some());
        for unread in ["", "soon", "2026-11-23"] {
            assert!(at(unread).is_none(), "{unread}");
        }
        assert_eq!(VERIFIER_STALE_SECS, 48 * 3600);
        // No verifier at all.
        assert_eq!(auto_update(&scope("h", false), "current", NOW), None);
        // With the self-update role: the agent's job.
        let mut own = verified("h");
        own.self_update = true;
        assert_eq!(auto_update(&own, "current", NOW), None);
    }

    /// What the cluster's settings say of automatic updates. 0.51.0's
    /// verifier applies nothing to the cluster, so an outdated install is
    /// told it still needs the command, never that it updates by itself.
    #[test]
    fn automatic_updates_are_shown_as_the_verifier_says() {
        use serde_json::json;
        // Set up, the install current.
        assert_eq!(
            auto_update(&verified("h"), "current", NOW),
            Some(json!({ "state": "on", "version": "0.51.0", "message": "Automatic updates are set up on this cluster (version 0.51.0)." }))
        );
        // Set up, the install outdated or short of roles: the command.
        for waits in ["outdated", "roles"] {
            assert_eq!(
                auto_update(&verified("old"), waits, NOW),
                Some(json!({ "state": "on", "version": "0.51.0", "message": "Automatic updates are set up on this cluster (version 0.51.0). This update still needs the command." }))
            );
        }
        // A version that isn't one isn't shown.
        let mut odd = verified("h");
        odd.verifier.as_mut().unwrap().version = "<b>9</b>".into();
        assert_eq!(
            auto_update(&odd, "current", NOW),
            Some(json!({ "state": "on", "message": "Automatic updates are set up on this cluster." }))
        );
        // On hold on the server.
        let mut held = verified("h");
        held.verifier.as_mut().unwrap().hold = true;
        assert_eq!(
            auto_update(&held, "current", NOW),
            Some(json!({ "state": "hold", "version": "0.51.0", "message": "Automatic updates are on hold. They were turned off on the server." }))
        );
        // On hold in the policy, whatever the verifier noticed of it yet.
        let mut policy = verified("h");
        policy.policy.updates_hold = true;
        assert_eq!(
            auto_update(&policy, "outdated", NOW),
            Some(json!({ "state": "hold", "version": "0.51.0", "message": "Automatic updates are on hold. They were turned off in this cluster's policy." }))
        );
        // The policy alone, without a verifier: nothing to show.
        let mut alone = scope("h", false);
        alone.policy.updates_hold = true;
        assert_eq!(auto_update(&alone, "current", NOW), None);
        // An error, in the verifier's words, with the command when the
        // install waits for it. Hold comes first: a held verifier's last
        // error isn't news.
        let mut failed = verified("h");
        failed.verifier.as_mut().unwrap().error = "The release's signature couldn't be checked".into();
        assert_eq!(
            auto_update(&failed, "current", NOW),
            Some(json!({ "state": "error", "version": "0.51.0", "message": "Automatic updates reported a problem: The release's signature couldn't be checked." }))
        );
        assert_eq!(
            auto_update(&failed, "outdated", NOW).unwrap()["message"],
            "Automatic updates reported a problem: The release's signature couldn't be checked. Run the command to update this cluster."
        );
        failed.verifier.as_mut().unwrap().hold = true;
        assert_eq!(auto_update(&failed, "current", NOW).unwrap()["state"], "hold");
        // Its words are shown as one plain line, cut short.
        let mut noisy = verified("h");
        noisy.verifier.as_mut().unwrap().error = format!("GitHub didn't answer.\n\t\u{1b}[31m{}", "x".repeat(1000));
        let message = auto_update(&noisy, "current", NOW).unwrap()["message"].as_str().unwrap().to_owned();
        assert!(message.starts_with("Automatic updates reported a problem: GitHub didn't answer. [31mxxx"), "{message}");
        assert!(message.ends_with("x…") && message.chars().count() < 350, "{message}");
        assert!(!message.contains(char::is_control));
        // No copy with a semicolon or a dash between clauses.
        for text in [AUTO_UPDATE_ON, AUTO_UPDATE_NEEDS_COMMAND, AUTO_UPDATE_HOLD_SERVER, AUTO_UPDATE_HOLD_POLICY, AUTO_UPDATE_PROBLEM, AUTO_UPDATE_FALLBACK] {
            assert!(!text.contains(';') && !text.contains(" - ") && !text.contains('—'), "{text}");
        }
    }

    /// The verifier's status comes through the agent, so a broken-into
    /// agent can make it say anything. Whatever it says, nothing but
    /// `autoUpdate` changes: not the install's state, not whether a
    /// refresh is sent, not what waits for the connect command, not what
    /// the host does, not the command shown.
    #[test]
    fn a_lying_verifier_status_only_changes_what_is_shown() {
        use pyo_proto::rbac::VerifierStatus;
        let lies = [
            None,
            Some(verifier("2026-11-23T09:00:00Z")),
            Some(VerifierStatus { version: "99.0.0".into(), live: "99.0.0".into(), ..verifier("2026-11-23T09:59:00Z") }),
            Some(VerifierStatus { hold: true, ..verifier("2026-11-23T09:00:00Z") }),
            Some(VerifierStatus { error: "all is lost".into(), ..verifier("2026-11-23T09:00:00Z") }),
            Some(VerifierStatus { checked: "9999-12-31T23:59:59Z".into(), ..verifier("") }),
            Some(VerifierStatus { checked: "never".into(), ..verifier("") }),
            Some(VerifierStatus::default()),
        ];
        let failed = LastRefresh { message: "nope".into(), ..last("failed", 10) };
        let bases = [
            scope("h", true),
            scope("old", true),
            scope("h", false),
            scope("old", false),
            partial("h", true, &["ClusterRole/pyo-agent-bundles"]),
            partial("h", false, &["ClusterRole/pyo-agent-read"]),
            AgentScope { admission: "missing".into(), ..scope("old", false) },
            AgentScope { headers_kept: true, ..scope("old", false) },
        ];
        for base in &bases {
            for last in [None, Some(&failed)] {
                let honest = install_state(base, "h", last);
                let honest_shown = install_shown(base, honest.0, &honest.1, "cmd".into(), Some("stamp"), NOW);
                assert!(honest_shown.get("autoUpdate").is_none());
                for (lie, hold) in lies.iter().flat_map(|l| [(l, false), (l, true)]) {
                    let mut lying = base.clone();
                    lying.verifier = lie.clone();
                    lying.policy.updates_hold = hold;
                    let what = format!("{base:?} {lie:?} hold={hold}");
                    assert_eq!(install_state(&lying, "h", last), honest, "{what}");
                    assert_eq!(
                        pyo_proto::rbac::install_current(&lying, "h"),
                        pyo_proto::rbac::install_current(base, "h"),
                        "{what}"
                    );
                    for (same, own) in [(true, true), (true, false), (false, true)] {
                        assert_eq!(
                            host_refreshes(same, own, &lying, honest.0),
                            host_refreshes(same, own, base, honest.0),
                            "{what}"
                        );
                        assert_eq!(host_may_refresh(own, &lying), host_may_refresh(own, base), "{what}");
                    }
                    let mut shown = install_shown(&lying, honest.0, &honest.1, "cmd".into(), Some("stamp"), NOW);
                    if let Some(o) = shown.as_object_mut() {
                        o.remove("autoUpdate");
                    }
                    assert_eq!(shown, honest_shown, "{what}");
                    // What the gates read of it: the state and `bundles`.
                    assert_eq!(
                        bundles_ready(shown["state"].as_str(), shown["bundles"].as_bool()),
                        bundles_ready(honest_shown["state"].as_str(), honest_shown["bundles"].as_bool()),
                        "{what}"
                    );
                }
            }
        }
        // An outdated install stays "Update needed", with its command and
        // its message, beside a verifier that says all is well.
        let outdated = verified("old");
        let (state, message, send) = install_state(&outdated, "h", None);
        assert_eq!((state, send), ("outdated", false));
        let shown = install_shown(&outdated, state, &message, "cmd".into(), None, NOW);
        assert_eq!(shown["state"], "outdated");
        assert_eq!(shown["command"], "cmd");
        assert_eq!(shown["message"], OUTDATED);
        assert_eq!(shown["autoUpdate"]["state"], "on");
    }

    /// GET /install/version: the version and the install a cluster
    /// without the self-update role gets, and nothing else, kept a minute.
    #[tokio::test]
    async fn the_version_route_names_this_pyo_and_its_install() {
        let res = version().await.into_response();
        assert_eq!(res.status(), axum::http::StatusCode::OK);
        assert_eq!(res.headers()[header::CACHE_CONTROL], "public, max-age=60");
        assert!(res.headers()[header::CONTENT_TYPE].to_str().unwrap().starts_with("application/json"));
        let body = axum::body::to_bytes(res.into_body(), 4096).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(
            v,
            serde_json::json!({ "version": env!("CARGO_PKG_VERSION"), "install": verifier_install() })
        );
        assert_eq!(v.as_object().unwrap().len(), 2);
        // The install: without the self-update role, on a cluster that
        // isn't Pyo's own, with this installation's images.
        use pyo_proto::system::{db_images_from_env, geoip_image_from_env, waf_image_from_env};
        let new = |self_update| {
            pyo_proto::rbac::InstallParts::new(false, self_update, &waf_image_from_env(), &geoip_image_from_env(), &db_images_from_env()).hash
        };
        assert_eq!(verifier_install(), new(false));
        assert_ne!(verifier_install(), new(true));
        assert_eq!(verifier_install().len(), 16);
        // What a Pyo without self-update hands its clusters.
        if !self_update() {
            assert_eq!(verifier_install(), parts(false).hash);
        }
        assert_eq!(
            version_answer("0.51.0", "abc").to_string(),
            r#"{"install":"abc","version":"0.51.0"}"#
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

    /// A server short of disk neither evicts the agent nor keeps a new one
    /// from starting, so it goes on reporting and cleaning up there.
    #[test]
    fn the_agent_stays_on_a_server_short_of_disk() {
        let pod = render("https://pyo.example.com", "img@sha256:00", "abc")
            .split("\n---")
            .filter_map(|d| serde_norway::from_str::<serde_json::Value>(d).ok())
            .find(|d| d["kind"] == "Deployment" && d["metadata"]["name"] == "pyo-agent")
            .expect("the agent's Deployment")["spec"]["template"]["spec"]
            .clone();
        assert_eq!(pod["priorityClassName"], "system-cluster-critical");
        // Exactly what a refresh brings, and the admission policy allows.
        assert_eq!(pod["priorityClassName"], pyo_proto::rbac::AGENT_PRIORITY);
        assert_eq!(pod["tolerations"], pyo_proto::rbac::agent_tolerations());
        let tolerated: Vec<&str> = pod["tolerations"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|t| t["operator"] == "Exists" && t["effect"] == "NoSchedule")
            .filter_map(|t| t["key"].as_str())
            .collect();
        for taint in [
            "node.kubernetes.io/disk-pressure",
            "node.kubernetes.io/memory-pressure",
            "nvidia.com/gpu",
        ] {
            assert!(tolerated.contains(&taint), "{taint}: {tolerated:?}");
        }
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
/// yet, so the host asks again later. Answers `{"state": "connected"}`
/// (with `install` and `refresh`, see [`host_answer`]),
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
) -> ApiResult<crate::error::Json<serde_json::Value>> {
    use serde_json::json;
    let key = bootstrap_key(&q.key)?;
    Ok(crate::error::Json(match hand_over(&s.db, &key).await? {
        Handover::Connected => {
            // What its cluster's last heartbeat asked of the host, and
            // what its agent may do there.
            let own: Option<(Option<String>, Option<serde_json::Value>)> = sqlx::query_as(
                "SELECT capabilities->'agentInstall'->>'hostRefresh', capabilities->'agentScope' FROM pyo.clusters WHERE same_cluster LIMIT 1",
            )
            .fetch_optional(&s.db)
            .await?;
            let (asked, scope) = own.unwrap_or_default();
            let may = scope
                .and_then(|v| serde_json::from_value::<pyo_proto::rbac::AgentScope>(v).ok())
                .is_some_and(|scope| host_may_refresh(self_update(), &scope));
            host_answer(may, &host_stamp(&parts(true).hash), asked.as_deref())
        }
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

/// The installation's bootstrap key, when `given` is it.
fn bootstrap_key(given: &str) -> ApiResult<String> {
    let key = std::env::var("PYO_BOOTSTRAP_KEY").unwrap_or_default();
    if key.len() < 32 || !crate::util::constant_time_eq(key.as_bytes(), given.as_bytes()) {
        return Err(ApiError::not_found("Install key"));
    }
    Ok(key)
}

/// GET /install/first-sign-in?key=…: whether the first admin
/// (admin@pyo.local) hasn't signed in and chosen their own email and
/// password yet, for the installer, which then prints that login again
/// when it runs again. `key` as for this_machine. Answers
/// `{"pending": true|false}`.
pub async fn first_sign_in(
    State(s): State<AppState>,
    Query(q): Query<ThisMachineQuery>,
) -> ApiResult<crate::error::Json<serde_json::Value>> {
    bootstrap_key(&q.key)?;
    let pending = first_admin_pending(&s.db).await?;
    Ok(crate::error::Json(
        serde_json::json!({ "pending": pending }),
    ))
}

#[derive(Deserialize)]
pub struct ResetLogin {
    key: String,
    /// The account's email. Empty: the installation's admin, when it has one.
    #[serde(default)]
    email: String,
}

/// POST /install/reset-login (form: key, email): a one-time sign-in link for
/// an account, for `pyo-reset-login` on the host (install.sh), which proves
/// itself with the bootstrap key as for [`this_machine`]. Without an email,
/// for the installation's admin, or a list of admins to pick from. Answers
/// `{"email", "path", "minutes"}`. The link replaces the password, so the
/// only admin who lost theirs gets back in.
pub async fn reset_login(
    State(s): State<AppState>,
    axum::Form(f): axum::Form<ResetLogin>,
) -> ApiResult<crate::error::Json<serde_json::Value>> {
    bootstrap_key(&f.key)?;
    let email = f.email.trim().to_lowercase();
    let mut conn = s.db.acquire().await?;
    let users: Vec<(String, String)> = if email.is_empty() {
        sqlx::query_as("SELECT id, email FROM pyo.users WHERE is_admin ORDER BY created_at")
            .fetch_all(&mut *conn)
            .await?
    } else {
        sqlx::query_as("SELECT id, email FROM pyo.users WHERE email = $1")
            .bind(&email)
            .fetch_all(&mut *conn)
            .await?
    };
    let (id, email) = match users.as_slice() {
        [one] => one.clone(),
        [] if !email.is_empty() => {
            return Err(ApiError::new(
                axum::http::StatusCode::NOT_FOUND,
                format!("No account has the email {email}."),
            ));
        }
        [] => return Err(ApiError::bad_request("Pyo has no admin yet.")),
        many => {
            let list: Vec<&str> = many.iter().map(|(_, e)| e.as_str()).collect();
            return Err(ApiError::bad_request(format!(
                "Pyo has more than one admin: {}. Name the account, like: pyo-reset-login {}",
                list.join(", "),
                list[0]
            )));
        }
    };
    // Made on the server: it also turns two-factor off, and for an admin
    // where admins may sign in from (recovery.rs).
    let token = crate::recovery::new_host_link(&mut conn, &id).await?;
    tracing::info!(user = %id, "sign-in link made on the host");
    Ok(crate::error::Json(serde_json::json!({
        "email": email,
        "path": crate::recovery::link_path(&token),
        "minutes": crate::recovery::LINK_MINUTES,
    })))
}

/// The first admin is there and hasn't chosen their own login yet.
async fn first_admin_pending(db: impl sqlx::PgExecutor<'_>) -> ApiResult<bool> {
    Ok(sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM pyo.users WHERE email = $1 AND must_set_up)",
    )
    .bind(crate::auth::FIRST_ADMIN_EMAIL)
    .fetch_one(db)
    .await?)
}

#[derive(Deserialize)]
pub struct ThisMachineTailnet {
    key: String,
    #[serde(default)]
    ip: String,
    #[serde(default)]
    name: String,
    /// Moving k3s onto the tailnet: "moving", "done" or "failed".
    #[serde(default)]
    k3s: String,
    /// Why it failed.
    #[serde(default)]
    error: String,
}

/// POST /install/this-machine/tailnet (form: key, ip, name, k3s, error): this machine's
/// Tailscale, from its timer, with the bootstrap key as for
/// [`this_machine`]. Answers what it does next (tailscale.rs `host_report`).
pub async fn this_machine_tailnet(
    State(s): State<AppState>,
    axum::Form(f): axum::Form<ThisMachineTailnet>,
) -> ApiResult<crate::error::Json<serde_json::Value>> {
    let key = std::env::var("PYO_BOOTSTRAP_KEY").unwrap_or_default();
    if key.len() < 32 || !crate::util::constant_time_eq(key.as_bytes(), f.key.as_bytes()) {
        return Err(ApiError::not_found("Install key"));
    }
    Ok(crate::error::Json(
        crate::tailscale::host_report(&s, &f.ip, &f.name, &f.k3s, &f.error).await?,
    ))
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

    /// The installer prints the first login again until it's been used:
    /// while admin@pyo.local is there and hasn't chosen their own. In a
    /// transaction that's rolled back.
    #[tokio::test]
    #[ignore]
    async fn first_sign_in_is_pending_until_the_admin_sets_up() {
        let db = db().await;
        let mut tx = db.begin().await.unwrap();
        sqlx::query(
            "UPDATE pyo.users SET email = id || '@moved.test' WHERE email = 'admin@pyo.local'",
        )
        .execute(&mut *tx)
        .await
        .unwrap();
        assert!(!first_admin_pending(&mut *tx).await.unwrap());
        sqlx::query("INSERT INTO pyo.users (id, email, name, password_hash, is_admin, must_set_up) VALUES ('usr_firstsignin', 'admin@pyo.local', 'Admin', 'x', true, true)")
            .execute(&mut *tx)
            .await
            .unwrap();
        assert!(first_admin_pending(&mut *tx).await.unwrap());
        sqlx::query("UPDATE pyo.users SET email = 'me@example.com', must_set_up = false WHERE id = 'usr_firstsignin'")
            .execute(&mut *tx)
            .await
            .unwrap();
        assert!(!first_admin_pending(&mut *tx).await.unwrap());
        tx.rollback().await.unwrap();
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

#[cfg(test)]
mod connect_token_tests {
    use super::*;

    /// Against a throwaway Postgres: a connect command's token fetches the
    /// manifest (with an agent token of its own) until its agent reports
    /// in or its hour is over; another team's, or a used or expired one,
    /// gets nothing. The agent's own token fetches the same manifest again.
    #[tokio::test]
    #[ignore = "needs DATABASE_URL (a throwaway Postgres)"]
    async fn connect_tokens_work_once_within_the_hour() {
        let db = sqlx::PgPool::connect(&std::env::var("DATABASE_URL").unwrap()).await.unwrap();
        crate::migrate::run(&db).await.unwrap();
        let run = crate::ids::random_string(8);
        let (team, other, cl) = (format!("team_c{run}"), format!("team_x{run}"), format!("cl_c{run}"));
        for t in [&team, &other] {
            sqlx::query("INSERT INTO pyo.teams (id, name, slug) VALUES ($1, $1, $1)").bind(t).execute(&db).await.unwrap();
        }
        sqlx::query("INSERT INTO pyo.clusters (id, team_id, name, agent_token_hash) VALUES ($1, $2, $1, $3)")
            .bind(&cl)
            .bind(&team)
            .bind(sha256_hex(&crate::ids::secret_token()))
            .execute(&db)
            .await
            .unwrap();
        let mut tx = db.begin().await.unwrap();
        let connect = new_connect_token(&mut tx, &cl, &team).await.unwrap();
        tx.commit().await.unwrap();
        let agent_hash = || async {
            sqlx::query_scalar::<_, String>("SELECT agent_token_hash FROM pyo.clusters WHERE id = $1").bind(&cl).fetch_one(&db).await.unwrap()
        };

        // Its manifest carries an agent token of its own, never the connect token.
        let (c, first) = token_for_manifest(&db, &connect).await.unwrap();
        assert_eq!(c, cl);
        assert_ne!(first, connect);
        assert_eq!(agent_hash().await, sha256_hex(&first));
        // Run again before the agent reports in (a failed try): another one.
        let (_, second) = token_for_manifest(&db, &connect).await.unwrap();
        assert_ne!(second, first);
        assert_eq!(agent_hash().await, sha256_hex(&second));
        // The agent's own token: the same manifest again (the refresh command).
        assert_eq!(token_for_manifest(&db, &second).await.unwrap().1, second);
        assert!(token_for_manifest(&db, &first).await.is_err());

        // Its agent reports in: the command is used up.
        let mut tx = db.begin().await.unwrap();
        agent_reported(&mut tx, &cl).await.unwrap();
        tx.commit().await.unwrap();
        let used = token_for_manifest(&db, &connect).await.unwrap_err();
        assert_eq!((used.status, used.message.as_str()), (axum::http::StatusCode::CONFLICT, CONNECT_USED));
        assert_eq!(token_for_manifest(&db, &second).await.unwrap().1, second);

        // A new command, after its hour.
        let mut tx = db.begin().await.unwrap();
        let late = new_connect_token(&mut tx, &cl, &team).await.unwrap();
        tx.commit().await.unwrap();
        assert!(token_for_manifest(&db, &connect).await.is_err(), "the command before it stops working");
        sqlx::query("UPDATE pyo.clusters SET connect_token_expires_at = now() - interval '1 minute' WHERE id = $1").bind(&cl).execute(&db).await.unwrap();
        let expired = token_for_manifest(&db, &late).await.unwrap_err();
        assert_eq!((expired.status, expired.message.as_str()), (axum::http::StatusCode::GONE, CONNECT_EXPIRED));

        // Made for another team (the cluster isn't its): nothing.
        let mut tx = db.begin().await.unwrap();
        let foreign = new_connect_token(&mut tx, &cl, &other).await.unwrap();
        tx.commit().await.unwrap();
        assert_eq!(token_for_manifest(&db, &foreign).await.unwrap_err().status, axum::http::StatusCode::NOT_FOUND);
        assert_eq!(token_for_manifest(&db, "not-hex").await.unwrap_err().status, axum::http::StatusCode::NOT_FOUND);
        assert_eq!(agent_hash().await, sha256_hex(&second), "nothing above changed the agent's token");
    }

    /// Against a throwaway Postgres: connect.sh found ports 80 and 443 in
    /// use. A cluster whose agent hasn't reported in yet takes no public
    /// traffic. One that's running keeps what it has.
    #[tokio::test]
    #[ignore = "needs DATABASE_URL (a throwaway Postgres)"]
    async fn busy_web_ports_turn_public_traffic_off_before_the_first_heartbeat() {
        let db = sqlx::PgPool::connect(&std::env::var("DATABASE_URL").unwrap()).await.unwrap();
        crate::migrate::run(&db).await.unwrap();
        let run = crate::ids::random_string(8);
        let team = format!("team_w{run}");
        sqlx::query("INSERT INTO pyo.teams (id, name, slug) VALUES ($1, $1, $1)").bind(&team).execute(&db).await.unwrap();
        let (new, running) = (format!("cl_wn{run}"), format!("cl_wr{run}"));
        for (cl, seen) in [(&new, false), (&running, true)] {
            sqlx::query(
                "INSERT INTO pyo.clusters (id, team_id, name, agent_token_hash, last_seen_at)
                 VALUES ($1, $2, $1, $3, CASE WHEN $4 THEN now() END)",
            )
            .bind(cl)
            .bind(&team)
            .bind(sha256_hex(&crate::ids::secret_token()))
            .bind(seen)
            .execute(&db)
            .await
            .unwrap();
        }
        let public = |cl: String| {
            let db = db.clone();
            async move {
                sqlx::query_scalar::<_, bool>("SELECT public_traffic FROM pyo.clusters WHERE id = $1")
                    .bind(cl)
                    .fetch_one(&db)
                    .await
                    .unwrap()
            }
        };
        assert!(public(new.clone()).await, "public by default");
        web_ports_busy(&db, &new).await.unwrap();
        assert!(!public(new.clone()).await);
        // Again (a second try of the command): still off.
        web_ports_busy(&db, &new).await.unwrap();
        assert!(!public(new.clone()).await);
        web_ports_busy(&db, &running).await.unwrap();
        assert!(public(running.clone()).await, "a running cluster keeps its setting");
        // The query connect.sh sends.
        let q: InstallQuery = serde_json::from_value(serde_json::json!({ "token": "ab", "webPorts": "busy" })).unwrap();
        assert_eq!(q.web_ports.as_deref(), Some("busy"));
        let q: InstallQuery = serde_json::from_value(serde_json::json!({ "token": "ab" })).unwrap();
        assert_eq!(q.web_ports, None);
        sqlx::query("DELETE FROM pyo.teams WHERE id = $1").bind(&team).execute(&db).await.unwrap();
    }
}
