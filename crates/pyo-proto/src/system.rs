//! What Pyo puts outside its projects, from one place, so the admission
//! policy (rbac.rs) can hold the agent to exactly this content.
//!
//! - The HelmCharts of cert-manager and Envoy Gateway (the gateway):
//!   in the install manifest, applied by whoever connects the cluster, never
//!   by the agent (k3s's helm controller installs charts with full rights).
//!   Envoy Gateway's CRDs (and the Gateway API's, where the cluster has
//!   none) are applied by connect.sh too ([`ENVOY_GATEWAY_VALUES`]).
//! - The proxy's settings (EnvoyProxy), its GatewayClass, the shared HTTP
//!   Gateway, the client traffic policies, the issuers and the GPU
//!   RuntimeClass: written by the agent once their APIs exist, pinned to this
//!   content.
//!
//! The gateway: Envoy Gateway's controller runs in `pyo-gateway` and
//! may only read that namespace (its Secrets included: certificates live
//! there); the Envoy proxies that face the internet run there too, as a
//! DaemonSet on every server but build-only ones ([`gateway_affinity`]),
//! with no cluster credentials (`automountServiceAccountToken: false`): they
//! get their configuration and certificates from the controller. Whether
//! they're published on the servers' addresses is the cluster's public
//! traffic setting ([`envoy_proxy`]).

use serde_json::{Value, json};

/// cert-manager release (Helm chart from charts.jetstack.io).
pub const CERT_MANAGER_VERSION: &str = "v1.21.2";

/// Envoy Gateway release (Helm chart oci://docker.io/envoyproxy/gateway-helm).
/// v1.9.3: the newest patch of the newest minor with one (1.10.0 is a day old).
pub const ENVOY_GATEWAY_VERSION: &str = "v1.9.3";

/// The gateway's namespace: its controller, its proxies, the Gateways,
/// routes and certificates.
pub const GATEWAY_NS: &str = "pyo-gateway";

/// The shared Gateway: one HTTP listener on :80 for every domain (redirects,
/// plain HTTP and Let's Encrypt's challenges).
pub const HTTP_GATEWAY: &str = "pyo";

/// Label on Pyo's Gateways: "http" (the shared one) or "https" (a service's).
pub const GATEWAY_LABEL: &str = "pyo.dev/gateway";

/// Prefix of certificate Secrets in `pyo-gateway`.
pub const TLS_SECRET_PREFIX: &str = "pyo-tls-";

/// Proxies trust X-Forwarded-For from these: tailnet, private, local (a
/// tunnel, ngrok's agent, k3s's load balancer, a CDN on a private link).
pub const TRUSTED_PROXY_CIDRS: [&str; 7] = [
    "127.0.0.1/32",
    "10.0.0.0/8",
    "172.16.0.0/12",
    "192.168.0.0/16",
    "100.64.0.0/10",
    "::1/128",
    "fc00::/7",
];

/// The access log the proxies write (JSON on stdout), field by field: what
/// analytics reads (pyo-api's observability.rs maps it).
pub const ACCESS_LOG_FIELDS: [(&str, &str); 20] = [
    ("start_time", "%START_TIME%"),
    ("host", "%REQ(:AUTHORITY)%"),
    ("method", "%REQ(:METHOD)%"),
    ("path", "%REQ(X-ENVOY-ORIGINAL-PATH?:PATH)%"),
    ("status", "%RESPONSE_CODE%"),
    ("duration_ms", "%DURATION%"),
    ("bytes", "%BYTES_SENT%"),
    // The TCP peer; the visitor is worked out from it and the headers.
    ("peer", "%DOWNSTREAM_DIRECT_REMOTE_ADDRESS_WITHOUT_PORT%"),
    ("x_forwarded_for", "%REQ(X-FORWARDED-FOR)%"),
    ("x_real_ip", "%REQ(X-REAL-IP)%"),
    ("user_agent", "%REQ(USER-AGENT)%"),
    ("referer", "%REQ(REFERER)%"),
    // The visitor as the proxy sees them: the PROXY protocol header's
    // address when one came, else the TCP peer (or, for HTTP, the address
    // worked out from X-Forwarded-For behind a trusted proxy).
    ("remote", "%DOWNSTREAM_REMOTE_ADDRESS_WITHOUT_PORT%"),
    // TCP and UDP ports (connections): where they came in, what they sent,
    // which route took them (`tcproute/pyo-gateway/<route>/rule/0`), and
    // how they ended.
    ("local_port", "%DOWNSTREAM_LOCAL_PORT%"),
    ("bytes_in", "%BYTES_RECEIVED%"),
    ("upstream", "%UPSTREAM_CLUSTER%"),
    ("upstream_host", "%UPSTREAM_HOST%"),
    ("flags", "%RESPONSE_FLAGS%"),
    ("termination", "%CONNECTION_TERMINATION_DETAILS%"),
    // Why the proxy answered itself: a firewall rule
    // (`rbac_access_denied_matched_policy[<rule>]`) or the rate limit.
    ("details", "%RESPONSE_CODE_DETAILS%"),
];

/// Also logged: the response's type (a page view is HTML).
pub const ACCESS_LOG_CONTENT_TYPE: (&str, &str) = ("content_type", "%RESP(CONTENT-TYPE)%");

/// The GeoIP country database the proxies place visitors with, for the
/// firewall's country rules (GeoLite2 Country, as analytics uses GeoLite2
/// City): an image with just the file (deploy/images/geoip.Dockerfile,
/// built with each release), mounted read-only as an image volume, so the
/// proxies need no credentials or downloads for it.
///
/// Its image comes from the installation (`PYO_GEOIP_IMAGE`), like the
/// WAF's: the agent's install manifest gives it to the agent and pins it in
/// the admission policy. This is the default where none is set: a local build.
pub const GEOIP_IMAGE_DEFAULT: &str = "127.0.0.1:5005/pyo/geoip:dev";
pub const GEOIP_PATH: &str = "/geoip/GeoLite2-Country.mmdb";

/// What a website's visitor gets when its firewall refuses them (a route
/// with access rules, its BackendTrafficPolicy): a short plain message, like
/// the WAF's, not Envoy's "RBAC: access denied". Only that refusal: it's
/// the proxy's own 403 (not the service's) whose body is exactly those 19
/// bytes, so the WAF's 403 keeps its own message. A rate limit's 429 is left
/// as it is: an override would drop its x-ratelimit-* headers.
pub fn firewall_response_override() -> Value {
    json!([{
        "source": "Local",
        "match": {
            "statusCodes": [{ "type": "Value", "value": 403 }],
            "responseHeaders": [{ "name": "content-length", "value": { "type": "Exact", "value": "19" } }],
        },
        "response": { "contentType": "text/plain", "body": { "type": "Inline", "inline": "Blocked by the firewall.\n" } },
    }])
}

/// The firewall for UDP ports through the proxy: a network policy on the
/// proxy's pods (pyo-agent's firewall.rs), which see the visitor's address.
pub const FIREWALL_POLICY: &str = "pyo-firewall-udp";

/// The proxy's pods, as Envoy Gateway labels them.
pub fn proxy_pods() -> Value {
    json!({ "matchLabels": { "app.kubernetes.io/component": "proxy", "app.kubernetes.io/managed-by": "envoy-gateway", "gateway.envoyproxy.io/owning-gatewayclass": "pyo" } })
}

/// The web application firewall (built in, set per service; its engine runs
/// on a cluster while a service there has it on): Coraza with the
/// OWASP Core Rule Set as an external processor beside the proxies (the
/// repository's `waf/`, image `deploy/images/waf.Dockerfile`). Measured cost: about 70 MiB and 2 ms a
/// request, against some 300 MiB for the same rules inside Envoy as Wasm.
///
/// Its image comes from the installation (`PYO_WAF_IMAGE`, like the agent's
/// and builder's): the API sends it with the WAF's settings, the agent's
/// install manifest gives it to the agent, and the admission policy pins
/// it. This is the default where none is set: a local build.
pub const WAF_IMAGE_DEFAULT: &str = "127.0.0.1:5005/pyo/waf:dev";

/// The WAF's image: `PYO_WAF_IMAGE`, else the default.
pub fn waf_image_from_env() -> String {
    image_from_env("PYO_WAF_IMAGE", WAF_IMAGE_DEFAULT)
}

/// The GeoIP database's image: `PYO_GEOIP_IMAGE`, else the default.
pub fn geoip_image_from_env() -> String {
    image_from_env("PYO_GEOIP_IMAGE", GEOIP_IMAGE_DEFAULT)
}

/// The databases system's own images (bundles.rs's `databases_bundle`):
/// pyo-db-controller, pyo-kv, Pyo's Postgres per major version and the
/// MySQL operator and its backup image (MOCO with Pyo's patches), built
/// with each Pyo release (deploy/images/pyo.Dockerfile's db-controller and
/// kv targets, postgres.Dockerfile, moco/moco.Dockerfile). Like the
/// WAF's, they come from the installation (`PYO_DB_CONTROLLER_IMAGE`,
/// `PYO_KV_IMAGE`, `PYO_POSTGRES_IMAGE_<major>`, `PYO_MYSQL_OPERATOR_IMAGE`,
/// `PYO_MYSQL_BACKUP_IMAGE`): the API and the agent's install manifest both
/// have them, and the admission policy pins them. The defaults are a local
/// build.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DbImages {
    pub controller: String,
    pub kv: String,
    /// Postgres 16, 17 and 18, in that order ([`POSTGRES_MAJORS`]). Their
    /// tags start with the version ("postgres:18-0.1.0"): CloudNativePG
    /// reads the version from the tag.
    pub postgres: [String; 3],
    /// The MySQL operator (`--target operator`) and the image of its
    /// backup and restore Jobs (`--target backup`).
    pub mysql_operator: String,
    pub mysql_backup: String,
}

/// The Postgres versions Pyo has an image for.
pub const POSTGRES_MAJORS: [&str; 3] = ["16", "17", "18"];

pub const DB_CONTROLLER_IMAGE_DEFAULT: &str = "127.0.0.1:5005/pyo/db-controller:dev";
pub const KV_IMAGE_DEFAULT: &str = "127.0.0.1:5005/pyo/kv:dev";
pub const MYSQL_OPERATOR_IMAGE_DEFAULT: &str = "127.0.0.1:5005/pyo/mysql-operator:dev";
pub const MYSQL_BACKUP_IMAGE_DEFAULT: &str = "127.0.0.1:5005/pyo/mysql-backup:dev";

/// The databases system's images: from the environment, else the defaults.
pub fn db_images_from_env() -> DbImages {
    DbImages {
        controller: image_from_env("PYO_DB_CONTROLLER_IMAGE", DB_CONTROLLER_IMAGE_DEFAULT),
        kv: image_from_env("PYO_KV_IMAGE", KV_IMAGE_DEFAULT),
        postgres: POSTGRES_MAJORS.map(|m| {
            image_from_env(
                &format!("PYO_POSTGRES_IMAGE_{m}"),
                &format!("127.0.0.1:5005/pyo/postgres:{m}-dev"),
            )
        }),
        mysql_operator: image_from_env("PYO_MYSQL_OPERATOR_IMAGE", MYSQL_OPERATOR_IMAGE_DEFAULT),
        mysql_backup: image_from_env("PYO_MYSQL_BACKUP_IMAGE", MYSQL_BACKUP_IMAGE_DEFAULT),
    }
}

fn image_from_env(var: &str, default: &str) -> String {
    std::env::var(var)
        .ok()
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| default.to_owned())
}
/// Its DaemonSet, Service and settings ConfigMap, in [`WAF_NS`] (a
/// bundle's namespace: bundles.rs's `waf_bundle`).
pub const WAF_NAME: &str = "pyo-waf";
pub const WAF_NS: &str = "pyo-waf";
pub const WAF_PORT: i32 = 9002;

/// The Backend (in the gateway's namespace, named [`WAF_NAME`]) the routes'
/// WAF policies send to: its Service by cluster DNS name, like services'
/// routes, since Envoy Gateway only reads its own namespace (a reference to
/// the Service in [`WAF_NS`] never resolves). The agent writes it with the
/// gateway's fixed objects; the admission policy pins it.
pub fn waf_backend() -> Value {
    json!({
        "apiVersion": "gateway.envoyproxy.io/v1alpha1", "kind": "Backend",
        "metadata": { "name": WAF_NAME, "namespace": GATEWAY_NS, "labels": { "app.kubernetes.io/managed-by": "pyo" } },
        "spec": waf_backend_spec(),
    })
}

pub fn waf_backend_spec() -> Value {
    let mut spec = fqdn_backend_spec(&format!("{WAF_NAME}.{WAF_NS}.svc.cluster.local"), WAF_PORT);
    // gRPC: HTTP/2 without TLS.
    spec["appProtocols"] = json!(["gateway.envoyproxy.io/h2c"]);
    spec
}

/// How the proxy calls it, on each route with a domain that has the WAF on:
/// requests go to it first. When it can't be reached or doesn't answer in
/// time, a service in block mode refuses the request (a WAF that's down
/// mustn't let attacks through), and any other passes it uninspected (an
/// extra, not a gate: log only, or custom rules alone).
///
/// Timeouts, measured on a test cluster (Envoy Gateway 1.9):
/// - Connecting: 1 s (Envoy's default is 10). The WAF is on the proxy's own
///   server, so a connection takes well under a millisecond; one that's
///   not answered means the copy there is gone (with
///   internalTrafficPolicy Local its packets are dropped, not refused).
///   Block mode answers 503 after it, the rest pass after the message
///   timeout.
/// - Each message (headers, then the body): block mode waits up to
///   [`WAF_BLOCK_TIMEOUT`] and answers 504 after it, so a slow or hung WAF
///   never holds a request longer; the rest keep [`WAF_PASS_TIMEOUT`], so a
///   slow WAF costs a request little. (Their OWASP rules, which only log,
///   don't hold a body at all: the WAF runs them after answering. That
///   matters: streamed, Envoy cancels a message's timeout when it gets the
///   answer to an earlier piece, so the last piece would otherwise wait
///   for the WAF however long it took.)
///
/// The WAF inspects a body's first 128 KiB (custom rules and OWASP rules;
/// pyo-waf's `inspectBody`): the OWASP rules' time grows faster than a
/// field's length, and must stay well under block mode's timeout. The rest
/// passes uninspected.
///
/// What doesn't need the body (custom rules on the request line, headers,
/// address and country; block mode's OWASP rules on the request line and
/// headers) is decided on the headers, so such a 403 never waits for the
/// body and nothing of the request reaches the service.
///
/// The body, in block mode: `BufferedPartial`. The proxy holds the whole
/// request, headers included, until the WAF has answered on the body's
/// first [`WAF_BODY_LIMIT`] (the proxy's buffer, [`client_policy_spec`]),
/// sent as one message: nothing reaches the service before the verdict.
/// A larger body (an upload) isn't refused: it goes on after that. Not
/// `Buffered`: that answers 413 to any body over the buffer, and a buffer
/// big enough for uploads would hold each one whole in the proxy's memory.
///
/// In any other mode: `Streamed`, a piece at a time, each passed on once
/// the WAF has answered it, so a request isn't slowed down. Custom rules
/// on the body decide when the body ends; the service may get the headers
/// and the first pieces before that (a custom rule on the body holds back
/// only the end of it). The service gets the body chunked (no
/// Content-Length).
pub fn waf_ext_proc(block: bool) -> Value {
    json!({
        "backendRefs": [{ "group": "gateway.envoyproxy.io", "kind": "Backend", "name": WAF_NAME }],
        "backendSettings": { "timeout": { "tcp": { "connectTimeout": "1s" } } },
        "failOpen": !block,
        "messageTimeout": if block { WAF_BLOCK_TIMEOUT } else { WAF_PASS_TIMEOUT },
        "statusOnError": 503,
        "processingMode": { "request": { "body": if block { "BufferedPartial" } else { "Streamed" }, "attributes": ["source.address"] } },
    })
}

/// How much of a request's body the proxies hold (their per-connection
/// buffer, [`client_policy_spec`]): what block mode's WAF gets before the
/// request goes on ([`waf_ext_proc`]). More than the 128 KiB the WAF
/// inspects, so a body the proxy cuts short is cut there too (Envoy
/// Gateway's default, 32 KiB, isn't); and a slow client can't make a proxy
/// hold more than this per connection.
pub const WAF_BODY_LIMIT: &str = "256Ki";

/// How long the proxy waits for the WAF's answer to one message, for a
/// service in block mode ([`waf_ext_proc`]).
pub const WAF_BLOCK_TIMEOUT: &str = "5s";
/// The same, for any other.
pub const WAF_PASS_TIMEOUT: &str = "500ms";

/// A route's WAF policy refuses requests when the WAF is unavailable: its
/// service blocks ([`waf_ext_proc`]).
pub fn waf_fails_closed(route: &WafRoute) -> bool {
    route.settings.mode == "block"
}

/// On a route's WAF policy: its hosts and the service's WAF (JSON,
/// [`WafRoute`]), from which the WAF's settings are rebuilt.
pub const WAF_ANNOTATION: &str = "pyo.dev/waf";

/// What a block-mode route's WAF annotation (its [`WafRoute`] as JSON)
/// always contains and no other's can: the settings come right after the
/// hosts, the mode first, and a quote inside a host or rule value is
/// escaped. The admission policy matches failOpen against it.
pub const WAF_BLOCK_MARK: &str = r#""settings":{"mode":"block""#;

pub fn waf_labels() -> Value {
    json!({ "app.kubernetes.io/name": WAF_NAME, "app.kubernetes.io/managed-by": "pyo" })
}

/// Which servers run a proxy: every one but build-only servers (labelled
/// [`crate::BUILDER_NODE_LABEL`]; GPU servers too). The WAF's processor
/// runs on exactly the same ones.
pub fn gateway_affinity() -> Value {
    json!({ "nodeAffinity": { "requiredDuringSchedulingIgnoredDuringExecution": { "nodeSelectorTerms": [
        { "matchExpressions": [{ "key": crate::BUILDER_NODE_LABEL, "operator": "NotIn", "values": ["true"] }] },
    ] } } })
}

/// Its DaemonSet: one copy on every server that runs a proxy, which only
/// ever calls the copy on its own server (the Service's
/// internalTrafficPolicy Local), so the WAF adds no network hop. Updated
/// new-before-old, and a copy that's stopping finishes the requests it has
/// and sends new ones to its replacement first (pyo-waf's
/// `stopGracefully`), so block-mode services see no errors. Non-root,
/// read-only, no cluster credentials, settings from the ConfigMap, and the
/// proxies' GeoIP database for country conditions.
pub fn waf_daemonset(image: &str, geoip_image: &str) -> Value {
    json!({
        "apiVersion": "apps/v1", "kind": "DaemonSet",
        "metadata": { "name": WAF_NAME, "namespace": WAF_NS, "labels": waf_labels() },
        "spec": {
            "selector": { "matchLabels": { "app.kubernetes.io/name": WAF_NAME } },
            "updateStrategy": { "type": "RollingUpdate", "rollingUpdate": { "maxSurge": 1, "maxUnavailable": 0 } },
            "template": {
                "metadata": { "labels": waf_labels() },
                "spec": {
                    // Its token reads its settings' ConfigMap, nothing else ([`waf_access`]).
                    "serviceAccountName": WAF_NAME,
                    "automountServiceAccountToken": true,
                    "securityContext": { "runAsNonRoot": true, "runAsUser": 65532, "runAsGroup": 65532, "seccompProfile": { "type": "RuntimeDefault" } },
                    "affinity": gateway_affinity(),
                    "tolerations": [{ "key": crate::GPU_RESOURCE, "operator": "Exists", "effect": "NoSchedule" }],
                    "containers": [{
                        "name": "waf", "image": image,
                        "args": ["-listen", format!(":{WAF_PORT}"), "-config", "/etc/pyo-waf/config.json", "-configmap", WAF_NAME, "-geoip", GEOIP_PATH],
                        "ports": [{ "name": "grpc", "containerPort": WAF_PORT }],
                        "resources": { "requests": { "cpu": "50m", "memory": "96Mi" }, "limits": { "memory": "384Mi" } },
                        "securityContext": { "allowPrivilegeEscalation": false, "readOnlyRootFilesystem": true, "capabilities": { "drop": ["ALL"] } },
                        // Often: routes point at it once it's ready, and a copy
                        // that's stopping (it reports not ready first) is left.
                        "readinessProbe": { "grpc": { "port": WAF_PORT }, "periodSeconds": 2 },
                        // Coraza checks it can write a temporary file at start.
                        "volumeMounts": [{ "name": "settings", "mountPath": "/etc/pyo-waf", "readOnly": true }, { "name": "tmp", "mountPath": "/tmp" }, { "name": "geoip", "mountPath": "/geoip", "readOnly": true }],
                    }],
                    "volumes": [{ "name": "settings", "configMap": { "name": WAF_NAME } }, { "name": "tmp", "emptyDir": { "sizeLimit": "16Mi" } }, { "name": "geoip", "image": { "reference": geoip_image, "pullPolicy": "IfNotPresent" } }],
                },
            },
        },
    })
}

/// What the processor may read: its settings' ConfigMap, followed through
/// the API (a mounted ConfigMap reaches the pod only on the kubelet's next
/// sync, a minute or more after a change: so long a service whose WAF was
/// just turned on would pass uninspected). Its account, role and binding.
pub fn waf_access() -> [Value; 3] {
    let meta = json!({ "name": WAF_NAME, "namespace": WAF_NS, "labels": waf_labels() });
    [
        json!({ "apiVersion": "v1", "kind": "ServiceAccount", "metadata": meta }),
        json!({
            "apiVersion": "rbac.authorization.k8s.io/v1", "kind": "Role", "metadata": meta,
            "rules": [{ "apiGroups": [""], "resources": ["configmaps"], "resourceNames": [WAF_NAME], "verbs": ["get", "list", "watch"] }],
        }),
        json!({
            "apiVersion": "rbac.authorization.k8s.io/v1", "kind": "RoleBinding", "metadata": meta,
            "roleRef": { "apiGroup": "rbac.authorization.k8s.io", "kind": "Role", "name": WAF_NAME },
            "subjects": [{ "kind": "ServiceAccount", "name": WAF_NAME, "namespace": WAF_NS }],
        }),
    ]
}

/// Its Service. The proxies reach it by its address (routingType Service),
/// and with internalTrafficPolicy Local only ever the copy on their own
/// server; when that one is down, requests there pass uninspected (or are
/// refused, for a service in block mode: [`waf_ext_proc`]), never a slower
/// trip to another server.
pub fn waf_service() -> Value {
    json!({
        "apiVersion": "v1", "kind": "Service",
        "metadata": { "name": WAF_NAME, "namespace": WAF_NS, "labels": waf_labels() },
        "spec": {
            "selector": { "app.kubernetes.io/name": WAF_NAME },
            "internalTrafficPolicy": "Local",
            "ports": [{ "name": "grpc", "port": WAF_PORT, "targetPort": WAF_PORT, "appProtocol": "kubernetes.io/h2c" }],
        },
    })
}

/// What a route's WAF policy carries (as an annotation): its hostnames and
/// the service's WAF, from which the processor's settings are rebuilt.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WafRoute {
    pub hosts: Vec<String>,
    #[serde(default)]
    pub settings: crate::WafSettings,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rules: Vec<crate::WafRule>,
}

/// Its settings: each distinct combination of OWASP settings once (compiled
/// once in pyo-waf) and each distinct list of custom rules once, and which
/// host uses which. A host with neither isn't inspected.
pub fn waf_config(routes: &[WafRoute]) -> Value {
    // Sets by what's compiled.
    type Key<'a> = (&'a str, u8, &'a [u32], u32);
    fn key(d: &crate::WafSettings) -> Key<'_> {
        (
            d.mode.as_str(),
            d.level,
            d.exceptions.as_slice(),
            d.threshold,
        )
    }
    let mut sets: Vec<Key> = routes
        .iter()
        .map(|r| &r.settings)
        .filter(|d| d.on())
        .map(key)
        .collect();
    sets.sort();
    sets.dedup();
    let mut lists: Vec<&[crate::WafRule]> = routes
        .iter()
        .map(|r| r.rules.as_slice())
        .filter(|l| !l.is_empty())
        .collect();
    lists.sort();
    lists.dedup();
    let mut set_map = serde_json::Map::new();
    for (i, (mode, level, exceptions, threshold)) in sets.iter().enumerate() {
        set_map.insert(format!("s{i}"), json!({ "engine": if *mode == "block" { "On" } else { "DetectionOnly" }, "level": (*level).clamp(1, 4), "exceptions": exceptions, "threshold": threshold }));
    }
    let list_map: serde_json::Map<String, Value> = lists
        .iter()
        .enumerate()
        .map(|(i, l)| (format!("l{i}"), json!(l)))
        .collect();
    let (mut hosts, mut host_rules) = (serde_json::Map::new(), serde_json::Map::new());
    for r in routes {
        for h in &r.hosts {
            let h = h.to_lowercase();
            if r.settings.on() {
                let i = sets
                    .iter()
                    .position(|x| *x == key(&r.settings))
                    .unwrap_or(0);
                hosts.insert(h.clone(), json!(format!("s{i}")));
            }
            if !r.rules.is_empty() {
                let i = lists
                    .iter()
                    .position(|x| *x == r.rules.as_slice())
                    .unwrap_or(0);
                host_rules.insert(h, json!(format!("l{i}")));
            }
        }
    }
    json!({ "sets": set_map, "hosts": hosts, "lists": list_map, "hostRules": host_rules })
}

/// Envoy Gateway's chart values: it watches only `pyo-gateway` (so its roles
/// are Roles there, and it reads no other namespace's Secrets), with the
/// Backend API (routes reach services by their cluster DNS name, see
/// `gateway.rs` in the agent; the WAF's policies too, [`waf_backend`]).
/// Anything it must resolve has to be in `pyo-gateway`: a reference into
/// another namespace is never found.
///
/// Without the chart's CRDs (`crds.enabled: false`): they come with the
/// Gateway API's experimental channel, which k3s 1.37+ refuses on top of
/// the standard one it brings (its safe-upgrades admission policy), so the
/// chart never installed there. Pyo uses only standard kinds (Gateway,
/// HTTPRoute, TCPRoute, UDPRoute), so connect.sh applies Envoy Gateway's own
/// CRDs, and the Gateway API's standard channel where the cluster has none
/// (k3s before 1.37), before this chart: pyo-api's
/// `/install/gateway-crds.yaml`. Envoy Gateway only watches kinds whose CRDs
/// exist when it starts.
pub const ENVOY_GATEWAY_VALUES: &str = r#"crds:
  enabled: false
config:
  envoyGateway:
    gateway:
      controllerName: gateway.envoyproxy.io/gatewayclass-controller
    provider:
      type: Kubernetes
      kubernetes:
        watch:
          type: Namespaces
          namespaces: [pyo-gateway]
    extensionApis:
      enableBackend: true
    logging:
      level:
        default: info
topologyInjector:
  enabled: false
deployment:
  envoyGateway:
    resources:
      limits: { memory: 512Mi }
      requests: { cpu: 50m, memory: 64Mi }
"#;

/// The HelmChart that installs Envoy Gateway in `pyo-gateway`.
pub fn envoy_gateway_chart() -> Value {
    json!({
        "apiVersion": "helm.cattle.io/v1", "kind": "HelmChart",
        "metadata": { "name": "envoy-gateway", "namespace": "kube-system", "labels": { "app.kubernetes.io/managed-by": "pyo" } },
        "spec": {
            "chart": "oci://docker.io/envoyproxy/gateway-helm", "version": ENVOY_GATEWAY_VERSION,
            "targetNamespace": GATEWAY_NS, "createNamespace": true,
            "valuesContent": ENVOY_GATEWAY_VALUES,
        },
    })
}

/// The proxies: a DaemonSet on every server but build-only ones (GPU
/// servers too, [`gateway_affinity`]). With `public` (the cluster takes
/// public traffic), behind a LoadBalancer Service with externalTrafficPolicy
/// Local, so ServiceLB publishes their ports on those servers and hands a
/// request to the proxy on the server it reached. Without, a ClusterIP
/// Service: nothing is published on the servers, and only the tailnet
/// listeners' own Service (the host's, with its Tailscale address) reaches
/// them from outside. One proxy fleet for every Gateway (mergeGateways).
/// Backends are reached through their Service (routingType Service), so
/// "keep traffic in its region" still works. `geoip_image`: the
/// installation's GeoIP database image.
pub fn envoy_proxy(geoip_image: &str, public: bool) -> Value {
    let mut log = serde_json::Map::new();
    for (k, v) in ACCESS_LOG_FIELDS.iter().chain([&ACCESS_LOG_CONTENT_TYPE]) {
        log.insert((*k).into(), json!(v));
    }
    json!({
        "apiVersion": "gateway.envoyproxy.io/v1alpha1", "kind": "EnvoyProxy",
        "metadata": { "name": "pyo", "namespace": GATEWAY_NS, "labels": { "app.kubernetes.io/managed-by": "pyo" } },
        "spec": {
            "mergeGateways": true,
            "routingType": "Service",
            // The default, written out: the admission policy compares it all.
            "logging": { "level": { "default": "warn" } },
            "provider": {
                "type": "Kubernetes",
                "kubernetes": {
                    "envoyDaemonSet": {
                        "pod": {
                            "affinity": gateway_affinity(),
                            "tolerations": [{ "key": crate::GPU_RESOURCE, "operator": "Exists", "effect": "NoSchedule" }],
                            "volumes": [{ "name": "geoip", "image": { "reference": geoip_image, "pullPolicy": "IfNotPresent" } }],
                        },
                        "container": {
                            // ~30 MiB at rest (measured); traffic and routes add to it.
                            "resources": { "requests": { "cpu": "50m", "memory": "48Mi" }, "limits": { "memory": "512Mi" } },
                            "volumeMounts": [{ "name": "geoip", "mountPath": "/geoip", "readOnly": true }],
                        },
                        // No cluster credentials in the pods that face the internet.
                        // Envoy Gateway's shutdown manager asks for 32 MiB and holds a few.
                        "patch": { "type": "StrategicMerge", "value": { "spec": { "template": { "spec": {
                            "automountServiceAccountToken": false,
                            "containers": [{ "name": "shutdown-manager", "resources": { "requests": { "cpu": "10m", "memory": "16Mi" } } }],
                        } } } } },
                    },
                    "envoyService": envoy_service(public),
                },
            },
            // The firewall's country rules place visitors with this.
            "geoIP": { "provider": { "type": "MaxMind", "maxMind": { "countryDbSource": { "local": { "path": GEOIP_PATH } } } } },
            "telemetry": { "accessLog": { "settings": [{
                "format": { "type": "JSON", "json": log },
                "sinks": [{ "type": "File", "file": { "path": "/dev/stdout" } }],
            }] } },
        },
    })
}

/// The proxies' Service: published on the servers or not. Envoy Gateway's
/// schema gives externalTrafficPolicy Local either way (it's only used for
/// a LoadBalancer), written out so the admission policy compares it all.
///
/// It never carries the tailnet listeners' ports ([`TAILNET_PORTS`]): one
/// proxy fleet serves every Gateway, so Envoy Gateway would list them on
/// this Service too, and ServiceLB would then publish them on every
/// address of the server, the Tailscale one included. A connection there
/// would go through ServiceLB's pod (arriving from that pod, which the
/// tailnet-only policy refuses), competing with `TAILNET_SERVICE`, which
/// alone takes them. The patch drops them from what Envoy Gateway renders.
fn envoy_service(public: bool) -> Value {
    let drop: Vec<Value> = TAILNET_PORTS
        .iter()
        .map(|p| json!({ "port": p, "$patch": "delete" }))
        .collect();
    json!({
        "type": if public { "LoadBalancer" } else { "ClusterIP" },
        "externalTrafficPolicy": "Local",
        "patch": { "type": "StrategicMerge", "value": { "spec": { "ports": drop } } },
    })
}

/// Whether the proxies' settings (an EnvoyProxy's spec) publish them.
pub fn envoy_proxy_public(spec: &Value) -> bool {
    spec["provider"]["kubernetes"]["envoyService"]["type"] != "ClusterIP"
}

pub fn gateway_class() -> Value {
    json!({
        "apiVersion": "gateway.networking.k8s.io/v1", "kind": "GatewayClass",
        "metadata": { "name": "pyo", "labels": { "app.kubernetes.io/managed-by": "pyo" } },
        "spec": {
            "controllerName": "gateway.envoyproxy.io/gatewayclass-controller",
            "parametersRef": { "group": "gateway.envoyproxy.io", "kind": "EnvoyProxy", "name": "pyo", "namespace": GATEWAY_NS },
        },
    })
}

/// The shared Gateway's spec: plain HTTP on :80, routes from `pyo-gateway`.
pub fn http_gateway_spec() -> Value {
    json!({
        "gatewayClassName": "pyo",
        "listeners": [{ "name": "http", "port": 80, "protocol": "HTTP", "allowedRoutes": { "namespaces": { "from": "Same" } } }],
    })
}

pub fn http_gateway() -> Value {
    json!({
        "apiVersion": "gateway.networking.k8s.io/v1", "kind": "Gateway",
        "metadata": { "name": HTTP_GATEWAY, "namespace": GATEWAY_NS, "labels": { "app.kubernetes.io/managed-by": "pyo", GATEWAY_LABEL: "http" } },
        "spec": http_gateway_spec(),
    })
}

// ── Pyo itself over HTTPS, on the tailnet only ──────────────────────────────
//
// On the cluster Pyo runs on, its agent puts the dashboard and the
// registry's door behind the gateway with Tailscale's certificate for the
// host's tailnet name (written into TAILNET_TLS by the host's renew timer).
// Each on a port of its own, never 80/443 (the websites'), and only for
// tailnet addresses and the servers themselves (`tailnet_server_cidrs`).
//
// Their ports are kept off the proxies' own Service ([`envoy_proxy`]), so
// ServiceLB never publishes them (it would, on every address of the server,
// and hand those connections over from its own pod, which the policy below
// refuses). The host's Tailscale address gets a Service of its own instead
// (`TAILNET_SERVICE`, externalIPs, externalTrafficPolicy Local: kube-proxy
// takes a connection straight to the proxy, which sees the device's own
// address), written by the host's tailnet-cert timer (deploy/install.sh,
// scripts/pyo-up.sh) with the certificate: not by the agent. The
// SecurityPolicy still refuses everyone but tailnet addresses and the
// servers, whatever Host or SNI they send.

/// The Gateway of Pyo's own HTTPS listeners.
pub const TAILNET_GATEWAY: &str = "pyo-tailnet";
/// Tailscale's certificate for the host's tailnet name (pyo-gateway).
pub const TAILNET_TLS: &str = "pyo-tls-tailnet";
/// The Service that takes the tailnet listeners' connections on the host's
/// Tailscale address (pyo-gateway; the host writes it).
pub const TAILNET_SERVICE: &str = "pyo-tailnet";
/// The dashboard (and API) over HTTPS.
pub const DASHBOARD_PORT: i32 = 8443;
/// Pyo registry's door for teams' own images.
pub const REGISTRY_PORT: i32 = 5443;
/// Kept for S3 (object storage, to be built in): its listener and route
/// will come with it, as a third entry in `tailnet_listener`'s list and
/// `tailnet_objects`.
pub const S3_PORT: i32 = 3443;
/// Ports of Pyo's own HTTPS listeners; services can't publish them.
pub const TAILNET_PORTS: [i32; 3] = [DASHBOARD_PORT, REGISTRY_PORT, S3_PORT];
/// Tailscale's address ranges: the only sources the listeners admit.
pub const TAILNET_CIDRS: [&str; 2] = ["100.64.0.0/10", "fd7a:115c:a1e0::/48"];

fn tailnet_listener(name: &str, port: i32) -> Value {
    json!({
        "name": name, "port": port, "protocol": "HTTPS",
        "tls": { "mode": "Terminate", "certificateRefs": [{ "group": "", "kind": "Secret", "name": TAILNET_TLS }] },
        "allowedRoutes": { "namespaces": { "from": "Same" } },
    })
}

pub fn tailnet_gateway_spec() -> Value {
    json!({
        "gatewayClassName": "pyo",
        "listeners": [tailnet_listener("dashboard", DASHBOARD_PORT), tailnet_listener("registry", REGISTRY_PORT)],
    })
}

/// (route and backend name, listener, in-cluster host, port)
pub const TAILNET_ROUTES: [(&str, &str, &str, i32); 2] = [
    (
        "pyo-dashboard",
        "dashboard",
        "pyo.pyo-system.svc.cluster.local",
        8000,
    ),
    (
        "pyo-registry-door",
        "registry",
        "pyo-registry-door.pyo-system.svc.cluster.local",
        5006,
    ),
];

/// A Backend's spec as the API server keeps it: with the `type` Envoy
/// Gateway's schema fills in, so the admission policy can compare the
/// stored object to it exactly.
pub fn fqdn_backend_spec(host: &str, port: i32) -> Value {
    json!({ "endpoints": [{ "fqdn": { "hostname": host, "port": port } }], "type": "Endpoints" })
}

pub fn tailnet_backend_spec(host: &str, port: i32) -> Value {
    fqdn_backend_spec(host, port)
}

/// The servers themselves: a server's own connections to its tailnet name
/// (its browser, its docker pushing to the registry) don't keep their
/// address. kube-proxy masquerades the host's traffic to a Service's
/// external address, so it reaches the proxy from the server's end of the
/// pod network: the bridge (cni0, the first address of the server's pod
/// range) when the proxy runs there, flannel's tunnel (its network address)
/// when it runs on another server. Pods get neither: their own connections
/// keep their pod address. So each server's pod range gives its first two
/// addresses, as one /31 (/127 for IPv6), and only private IPv4 ranges or
/// IPv6 ranges whose network address ends in `::` qualify: what
/// [`TAILNET_SERVER_CIDR_V4`] / [`TAILNET_SERVER_CIDR_V6`] admit.
pub fn tailnet_server_cidrs<'a>(pod_cidrs: impl IntoIterator<Item = &'a str>) -> Vec<String> {
    let mut out: Vec<String> = pod_cidrs
        .into_iter()
        .filter_map(|c| {
            let (ip, len) = c.trim().split_once('/')?;
            let len: u32 = len.parse().ok()?;
            match ip.parse::<std::net::IpAddr>().ok()? {
                std::net::IpAddr::V4(a) if len <= 30 => {
                    let net = std::net::Ipv4Addr::from(
                        u32::from(a) & u32::MAX.checked_shl(32 - len).unwrap_or(0),
                    );
                    let o = net.octets();
                    let private = o[0] == 10
                        || (o[0] == 172 && (16..32).contains(&o[1]))
                        || (o[0] == 192 && o[1] == 168);
                    (private && o[3] == 0).then(|| format!("{net}/31"))
                }
                std::net::IpAddr::V6(a) if len <= 126 => {
                    let net = std::net::Ipv6Addr::from(
                        u128::from(a) & u128::MAX.checked_shl(128 - len).unwrap_or(0),
                    )
                    .to_string();
                    (net.ends_with("::") && net != "::").then(|| format!("{net}/127"))
                }
                _ => None,
            }
        })
        .collect();
    out.sort();
    out.dedup();
    out
}

/// What the admission policy lets the servers' rule hold (CEL `matches`,
/// RE2): the first /31 of a private IPv4 range that starts at `.0`, the
/// first /127 of an IPv6 range.
pub const TAILNET_SERVER_CIDR_V4: &str =
    r"^(10\.[0-9]{1,3}|172\.(1[6-9]|2[0-9]|3[01])|192\.168)\.[0-9]{1,3}\.0/31$";
pub const TAILNET_SERVER_CIDR_V6: &str = r"^[0-9a-f]{1,4}(:[0-9a-f]{1,4}){0,6}::/127$";

/// Name of the policy's rule for the servers themselves.
pub const TAILNET_SERVERS_RULE: &str = "servers";

/// The tailnet listeners' policy: Tailscale's ranges, and the servers
/// themselves ([`tailnet_server_cidrs`], from the servers' pod ranges).
pub fn tailnet_policy_spec(servers: &[String]) -> Value {
    let mut rules = vec![
        json!({ "name": "tailnet", "action": "Allow", "principal": { "clientCIDRs": TAILNET_CIDRS } }),
    ];
    if !servers.is_empty() {
        rules.push(json!({ "name": TAILNET_SERVERS_RULE, "action": "Allow", "principal": { "clientCIDRs": servers } }));
    }
    json!({
        "targetRefs": [{ "group": "gateway.networking.k8s.io", "kind": "Gateway", "name": TAILNET_GATEWAY }],
        "authorization": { "defaultAction": "Deny", "rules": rules },
    })
}

/// The tailnet listeners' SecurityPolicy.
pub fn tailnet_policy(servers: &[String]) -> Value {
    json!({
        "apiVersion": "gateway.envoyproxy.io/v1alpha1", "kind": "SecurityPolicy",
        "metadata": { "name": TAILNET_GATEWAY, "namespace": GATEWAY_NS, "labels": { "app.kubernetes.io/managed-by": "pyo" } },
        "spec": tailnet_policy_spec(servers),
    })
}

/// Everything the agent applies for them, in pyo-gateway (`servers`:
/// [`tailnet_server_cidrs`]).
pub fn tailnet_objects(servers: &[String]) -> Vec<Value> {
    let meta = |name: &str| json!({ "name": name, "namespace": GATEWAY_NS, "labels": { "app.kubernetes.io/managed-by": "pyo" } });
    let mut out = vec![
        json!({ "apiVersion": "gateway.networking.k8s.io/v1", "kind": "Gateway", "metadata": meta(TAILNET_GATEWAY), "spec": tailnet_gateway_spec() }),
        tailnet_policy(servers),
    ];
    for (name, section, host, port) in TAILNET_ROUTES {
        out.push(json!({ "apiVersion": "gateway.envoyproxy.io/v1alpha1", "kind": "Backend", "metadata": meta(name), "spec": tailnet_backend_spec(host, port) }));
        out.push(json!({
            "apiVersion": "gateway.networking.k8s.io/v1", "kind": "HTTPRoute", "metadata": meta(name),
            "spec": {
                "parentRefs": [{ "name": TAILNET_GATEWAY, "sectionName": section }],
                // No request timeout: image layers and long-polls take their time.
                "rules": [{ "backendRefs": [{ "group": "gateway.envoyproxy.io", "kind": "Backend", "name": name }], "timeouts": { "request": "0s" } }],
            },
        }));
    }
    out
}

// ── Websites on the servers' Tailscale addresses ────────────────────────────
//
// When a cluster's free domains point at its Tailscale IP
// ("<name>.100.x.y.z.nip.io"), the agent publishes the websites' ports (80
// and 443; 10080 and 10443 in the proxy) on every server's Tailscale
// address that runs a proxy, whether or not the cluster takes public
// traffic: a Service of its own, `TAILNET_WEB_SERVICE` (the host's
// `TAILNET_SERVICE` stays the host's, with only the listeners' ports). Like
// that one: externalIPs, externalTrafficPolicy Local, so kube-proxy takes a
// connection straight to the proxy on that server and the proxy sees the
// visitor's own tailnet address: a service's access rules and WAF apply as
// on the internet. The admission policy pins it: this name, these ports,
// the proxies' pods, and Tailscale's addresses only.

/// The agent's Service for websites on the servers' Tailscale addresses.
pub const TAILNET_WEB_SERVICE: &str = "pyo-tailnet-web";
/// Its ports: the websites' (port, the proxy's port).
pub const TAILNET_WEB_PORTS: [(&str, i32, i32); 2] = [("http", 80, 10080), ("https", 443, 10443)];
/// A Tailscale IPv4 address (100.64.0.0/10), as the admission policy checks it.
pub const TAILNET_IPV4_RE: &str = r"^100\.(6[4-9]|[7-9][0-9]|1[01][0-9]|12[0-7])\.(25[0-5]|2[0-4][0-9]|1[0-9][0-9]|[1-9]?[0-9])\.(25[0-5]|2[0-4][0-9]|1[0-9][0-9]|[1-9]?[0-9])$";
/// A Tailscale IPv6 address (fd7a:115c:a1e0::/48).
pub const TAILNET_IPV6_RE: &str = r"^fd7a:115c:a1e0:[0-9a-f:]*$";

/// Its ports, as the API server keeps them (protocol filled in).
pub fn tailnet_web_ports() -> Value {
    Value::Array(
        TAILNET_WEB_PORTS
            .iter()
            .map(|(name, port, target)| json!({ "name": name, "port": port, "targetPort": target, "protocol": "TCP" }))
            .collect(),
    )
}

/// The Service, on these Tailscale addresses (`ips`, at least one).
pub fn tailnet_web_service(ips: &[String]) -> Value {
    json!({
        "apiVersion": "v1", "kind": "Service",
        "metadata": { "name": TAILNET_WEB_SERVICE, "namespace": GATEWAY_NS, "labels": { "app.kubernetes.io/managed-by": "pyo" } },
        "spec": {
            "type": "ClusterIP",
            "externalIPs": ips,
            "externalTrafficPolicy": "Local",
            "selector": proxy_pods()["matchLabels"],
            "ports": tailnet_web_ports(),
        },
    })
}

/// Spec fields the API server may fill in on it, besides those it sets.
pub const TAILNET_WEB_DEFAULTED: [&str; 6] = [
    "clusterIP",
    "clusterIPs",
    "ipFamilies",
    "ipFamilyPolicy",
    "sessionAffinity",
    "internalTrafficPolicy",
];

fn client_ip_detection() -> Value {
    json!({ "xForwardedFor": { "trustedCIDRs": TRUSTED_PROXY_CIDRS } })
}

/// How the proxies place a website's visitor, for the client policies.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct WebSettings {
    /// HTTPS requires the CDN's client certificate.
    pub origin_pull: bool,
    /// A PROXY protocol header is read when one is sent.
    pub proxy_protocol: bool,
    /// The visitor is the connection's address (or the PROXY header's),
    /// never X-Forwarded-For: on while the firewall has country rules, which
    /// Envoy Gateway only allows with this.
    pub direct: bool,
}

/// Client traffic policies: the visitor's address behind trusted proxies
/// (or, with `direct`, the connection's address), on every Pyo Gateway; and
/// on HTTPS, with origin pull on, a client certificate from the CDN's CAs
/// (ConfigMap `cdn-origin-pull`). With `proxy_protocol`, the visitor's
/// address from a PROXY protocol header when one is sent (optional: plain
/// connections still work). Each connection's buffer is [`WAF_BODY_LIMIT`]:
/// the part of a body block mode's WAF inspects before the request goes on.
/// Its other setting is written out as Envoy Gateway's CRD defaults it, so
/// the stored spec is exactly one the admission policy lists.
pub fn client_policy_spec(https: bool, w: WebSettings) -> Value {
    let mut spec = json!({
        "targetSelectors": [{ "group": "gateway.networking.k8s.io", "kind": "Gateway", "matchLabels": { GATEWAY_LABEL: if https { "https" } else { "http" } } }],
        "clientIPDetection": if w.direct { json!({ "directSourceIP": {} }) } else { client_ip_detection() },
        "connection": { "bufferLimit": WAF_BODY_LIMIT, "maxAcceptPerSocketEvent": 1 },
    });
    let (origin_pull, proxy_protocol) = (w.origin_pull, w.proxy_protocol);
    if https && origin_pull {
        spec["tls"] = json!({ "clientValidation": { "caCertificateRefs": [{ "group": "", "kind": "ConfigMap", "name": ORIGIN_PULL_SECRET }] } });
    }
    if proxy_protocol {
        spec["proxyProtocol"] = json!({ "optional": true });
    }
    spec
}

pub fn client_policy(https: bool, w: WebSettings) -> Value {
    json!({
        "apiVersion": "gateway.envoyproxy.io/v1alpha1", "kind": "ClientTrafficPolicy",
        "metadata": { "name": if https { "pyo-https" } else { "pyo-http" }, "namespace": GATEWAY_NS, "labels": { "app.kubernetes.io/managed-by": "pyo" } },
        "spec": client_policy_spec(https, w),
    })
}

/// Every client policy spec the agent may write (the admission policy's list).
pub fn client_policy_variants(https: bool) -> Vec<Value> {
    let mut v = vec![];
    for origin_pull in [false, true].into_iter().filter(|p| https || !p) {
        for proxy_protocol in [false, true] {
            for direct in [false, true] {
                v.push(client_policy_spec(
                    https,
                    WebSettings {
                        origin_pull,
                        proxy_protocol,
                        direct,
                    },
                ));
            }
        }
    }
    v
}

/// Label value on a service's Gateway for its public TCP and UDP ports.
pub const L4_GATEWAY: &str = "ports";

/// The most addresses one port's IP rule holds.
pub const MAX_IP_FILTER: usize = 100;

/// NVIDIA's device plugin, run on GPU servers.
pub const DEVICE_PLUGIN_IMAGE: &str = "nvcr.io/nvidia/k8s-device-plugin:v0.20.1";

/// k3s's CoreDNS Deployment in kube-system. With two or more servers the
/// agent runs 2 of it (its scale only: k3s's manifest already spreads them
/// one per server and leaves the count alone), with this budget.
pub const COREDNS: &str = "coredns";
pub const COREDNS_BUDGET: &str = "pyo-coredns";

/// The disruption budget that keeps one CoreDNS up while a server drains.
/// A pod that isn't ready (on a frozen server) never holds a drain up.
pub fn coredns_budget() -> Value {
    json!({
        "apiVersion": "policy/v1", "kind": "PodDisruptionBudget",
        "metadata": { "name": COREDNS_BUDGET, "namespace": "kube-system", "labels": { "app.kubernetes.io/managed-by": "pyo" } },
        "spec": {
            "minAvailable": 1,
            "selector": { "matchLabels": { "k8s-app": "kube-dns" } },
            "unhealthyPodEvictionPolicy": "AlwaysAllow",
        },
    })
}

/// cert-manager's chart values. Gateway API: Let's Encrypt's HTTP
/// challenge goes through the proxy. Owner references: a Certificate owns
/// its Secret, so the Secret goes when the Certificate is deleted (a
/// deleted service's in `pyo-gateway`, a deleted MySQL database's in
/// `pyo-databases`); without it cert-manager keeps every one.
pub const CERT_MANAGER_VALUES: &str = "crds:
  enabled: true
enableCertificateOwnerRef: true
config:
  apiVersion: controller.config.cert-manager.io/v1alpha1
  kind: ControllerConfiguration
  enableGatewayAPI: true
";

/// The HelmChart that installs cert-manager, with its CRDs.
pub fn cert_manager_chart() -> Value {
    json!({
        "apiVersion": "helm.cattle.io/v1", "kind": "HelmChart",
        "metadata": { "name": "cert-manager", "namespace": "kube-system", "labels": { "app.kubernetes.io/managed-by": "pyo" } },
        "spec": {
            "repo": "https://charts.jetstack.io", "chart": "cert-manager", "version": CERT_MANAGER_VERSION,
            "targetNamespace": "cert-manager", "createNamespace": true,
            "valuesContent": CERT_MANAGER_VALUES,
        },
    })
}

/// The two ClusterIssuers: Let's Encrypt over HTTP-01 through the gateway, and
/// self-signed for private domains. (name, spec)
pub fn cluster_issuers() -> [(&'static str, Value); 2] {
    [
        (
            "pyo-letsencrypt",
            json!({ "acme": {
                "server": "https://acme-v02.api.letsencrypt.org/directory",
                "privateKeySecretRef": { "name": "pyo-letsencrypt-account" },
                "solvers": [{ "http01": { "gatewayHTTPRoute": { "parentRefs": [{ "group": "gateway.networking.k8s.io", "kind": "Gateway", "name": HTTP_GATEWAY, "namespace": GATEWAY_NS, "sectionName": "http" }] } } }],
            } }),
        ),
        ("pyo-selfsigned", json!({ "selfSigned": {} })),
    ]
}

/// The CDN's CA certificates (a ConfigMap in `pyo-gateway`) the HTTPS
/// listeners check client certificates against, with origin pull on.
pub const ORIGIN_PULL_SECRET: &str = "cdn-origin-pull";

/// The GPU RuntimeClass's scheduling: only GPU servers.
pub fn gpu_runtime_scheduling() -> Value {
    json!({ "nodeSelector": { crate::GPU_LABEL: "nvidia" } })
}

/// A JSON value as a CEL literal, to compare an object's fields with.
pub fn cel_literal(v: &Value) -> String {
    match v {
        Value::Null => "null".into(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        Value::String(s) => {
            let mut out = String::from("'");
            for c in s.chars() {
                match c {
                    '\\' => out.push_str("\\\\"),
                    '\'' => out.push_str("\\'"),
                    '\n' => out.push_str("\\n"),
                    '\r' => out.push_str("\\r"),
                    '\t' => out.push_str("\\t"),
                    c => out.push(c),
                }
            }
            out.push('\'');
            out
        }
        // dyn(): CEL's maps and lists hold one type, and these mix them.
        Value::Array(a) => format!(
            "[{}]",
            a.iter()
                .map(|v| format!("dyn({})", cel_literal(v)))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Value::Object(m) => format!(
            "{{{}}}",
            m.iter()
                .map(|(k, v)| format!("{}: dyn({})", cel_literal(&json!(k)), cel_literal(v)))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

#[cfg(test)]
mod tests {

    /// The chart installs no CRDs: its own would bring the experimental
    /// Gateway API, which k3s 1.37+ refuses (connect.sh applies them).
    #[test]
    fn envoy_gateway_chart_installs_no_crds() {
        let chart = super::envoy_gateway_chart();
        let values: serde_json::Value =
            serde_norway::from_str(chart["spec"]["valuesContent"].as_str().unwrap()).unwrap();
        assert_eq!(values["crds"]["enabled"], false);
        assert_eq!(
            values["config"]["envoyGateway"]["provider"]["kubernetes"]["watch"]["namespaces"],
            serde_json::json!([super::GATEWAY_NS])
        );
        assert_eq!(
            values["config"]["envoyGateway"]["extensionApis"]["enableBackend"],
            true
        );
        assert_eq!(chart["spec"]["version"], super::ENVOY_GATEWAY_VERSION);
    }

    /// A Certificate owns its Secret, so deleting one (a service's domain,
    /// a MySQL database's) leaves no Secret behind; and cert-manager still
    /// answers Let's Encrypt's challenge through the gateway.
    #[test]
    fn cert_manager_secrets_go_with_their_certificates() {
        let chart = super::cert_manager_chart();
        let values: serde_json::Value =
            serde_norway::from_str(chart["spec"]["valuesContent"].as_str().unwrap()).unwrap();
        assert_eq!(values["enableCertificateOwnerRef"], true);
        assert_eq!(values["crds"]["enabled"], true);
        assert_eq!(values["config"]["enableGatewayAPI"], true);
        assert_eq!(chart["spec"]["version"], super::CERT_MANAGER_VERSION);
    }

    /// The proxies' Service (the public LoadBalancer when public traffic is
    /// on) never carries the tailnet listeners' ports, so ServiceLB never
    /// publishes them: Envoy Gateway renders one port per listener of every
    /// Gateway, and the patch (a strategic merge on the Service, keyed by
    /// port) deletes exactly the tailnet ones, keeping the websites' and
    /// teams' own TCP/UDP ports.
    #[test]
    fn public_service_never_publishes_the_tailnet_ports() {
        for public in [true, false] {
            let p = envoy_proxy("g", public);
            let svc = &p["spec"]["provider"]["kubernetes"]["envoyService"];
            assert_eq!(svc["patch"]["type"], "StrategicMerge");
            let patch = svc["patch"]["value"]["spec"]["ports"].as_array().unwrap();
            assert_eq!(svc["patch"]["value"].as_object().unwrap().len(), 1);
            assert_eq!(svc["patch"]["value"]["spec"].as_object().unwrap().len(), 1);
            // Only delete directives, one per tailnet port.
            assert_eq!(patch.len(), TAILNET_PORTS.len());
            assert!(
                patch
                    .iter()
                    .all(|e| e["$patch"] == "delete" && e.as_object().unwrap().len() == 2)
            );
            // What Envoy Gateway renders for the listeners, as merged.
            let rendered = [80, 443, DASHBOARD_PORT, REGISTRY_PORT, S3_PORT, 2222, 5353];
            let kept: Vec<i32> = rendered
                .into_iter()
                .filter(|port| !patch.iter().any(|e| e["port"] == *port))
                .collect();
            assert_eq!(kept, [80, 443, 2222, 5353]);
            assert!(TAILNET_PORTS.iter().all(|t| !kept.contains(t)));
        }
    }

    /// Pyo's own HTTPS listeners: their own ports, tailnet sources only.
    /// Public clients reach the same proxies, so the policy on the Gateway
    /// refuses them, whatever Host or SNI they send: it matches on the
    /// client's address, which the proxies keep (externalTrafficPolicy
    /// Local) through `TAILNET_SERVICE` (see the section's notes).
    #[test]
    fn tailnet_listeners_admit_the_tailnet_only() {
        let objs = tailnet_objects(&[]);
        let gw = objs.iter().find(|o| o["kind"] == "Gateway").unwrap();
        let ports: Vec<i64> = gw["spec"]["listeners"]
            .as_array()
            .unwrap()
            .iter()
            .map(|l| l["port"].as_i64().unwrap())
            .collect();
        assert_eq!(ports, [8443, 5443]);
        assert!(
            gw["spec"]["listeners"]
                .as_array()
                .unwrap()
                .iter()
                .all(|l| l["protocol"] == "HTTPS"
                    && l["tls"]["certificateRefs"][0]["name"] == TAILNET_TLS)
        );
        let sp = objs.iter().find(|o| o["kind"] == "SecurityPolicy").unwrap();
        assert_eq!(
            sp["spec"]["targetRefs"],
            json!([{ "group": "gateway.networking.k8s.io", "kind": "Gateway", "name": TAILNET_GATEWAY }]),
            "the whole Gateway: every listener and route"
        );
        assert_eq!(sp["spec"]["authorization"]["defaultAction"], "Deny");
        assert_eq!(
            sp["spec"]["authorization"]["rules"],
            json!([{ "name": "tailnet", "action": "Allow", "principal": { "clientCIDRs": ["100.64.0.0/10", "fd7a:115c:a1e0::/48"] } }])
        );
        // Only these routes attach to it, each to its own listener.
        let routes: Vec<&Value> = objs.iter().filter(|o| o["kind"] == "HTTPRoute").collect();
        assert_eq!(routes.len(), 2);
        for r in routes {
            assert_eq!(r["spec"]["parentRefs"][0]["name"], TAILNET_GATEWAY);
        }
        // Websites' listeners stay on 80/443, and services can't take these ports.
        assert!(!TAILNET_PORTS.contains(&443) && !TAILNET_PORTS.contains(&80));
        for p in TAILNET_PORTS {
            assert!(crate::reserved_public_port("TCP", p).is_some(), "{p}");
        }
    }

    /// The servers themselves: their own connections to their tailnet name
    /// reach the proxy from the bridge's (`.1`) or flannel's tunnel's (`.0`)
    /// address of their pod range, so the first /31 (/127) of each; pods
    /// (from `.2`) and anything else stay out.
    #[test]
    fn the_servers_own_addresses_are_their_pod_ranges_first_two() {
        let got = tailnet_server_cidrs([
            "10.42.1.0/24",
            "10.42.0.0/24",
            "10.42.0.0/24",
            "10.42.2.7/24",
            " 172.20.4.0/22 ",
            "192.168.8.0/24",
            "fd00:42:0:1::/64",
            "2001:db8:42:100::/56",
            // Public, too narrow, not starting at .0, or not a range: none.
            "8.8.8.0/24",
            "10.42.3.0/31",
            "10.42.3.128/25",
            "10.42.4.0",
            "x/24",
            "::/0",
            "fd00::1/127",
        ]);
        assert_eq!(
            got,
            [
                "10.42.0.0/31",
                "10.42.1.0/31",
                "10.42.2.0/31",
                "172.20.4.0/31",
                "192.168.8.0/31",
                "2001:db8:42:100::/127",
                "fd00:42:0:1::/127"
            ]
        );
        // Exactly what the admission policy lets the rule hold.
        let v4 = regex::Regex::new(TAILNET_SERVER_CIDR_V4).unwrap();
        let v6 = regex::Regex::new(TAILNET_SERVER_CIDR_V6).unwrap();
        for c in &got {
            assert!(v4.is_match(c) || v6.is_match(c), "{c}");
        }
        for c in [
            "10.42.0.2/32",
            "10.42.0.0/24",
            "10.42.0.1/31",
            "10.42.0.0/31 ",
            "100.64.0.0/31",
            "172.32.0.0/31",
            "11.0.0.0/31",
            "0.0.0.0/0",
            "::/0",
            "fd00::/64",
            "fd00::1/127",
            "FD00::/127",
        ] {
            assert!(!v4.is_match(c) && !v6.is_match(c), "{c}");
        }

        let spec = tailnet_policy_spec(&got[..2]);
        assert_eq!(spec["authorization"]["defaultAction"], "Deny");
        assert_eq!(
            spec["authorization"]["rules"],
            json!([
                { "name": "tailnet", "action": "Allow", "principal": { "clientCIDRs": TAILNET_CIDRS } },
                { "name": "servers", "action": "Allow", "principal": { "clientCIDRs": ["10.42.0.0/31", "10.42.1.0/31"] } },
            ])
        );
        let objs = tailnet_objects(&got);
        let sp = objs.iter().find(|o| o["kind"] == "SecurityPolicy").unwrap();
        assert_eq!(sp["spec"], tailnet_policy_spec(&got));
        assert_eq!(sp["metadata"]["name"], TAILNET_GATEWAY);

        // Admission: the bare policy, or with the servers' rule whose
        // addresses must each match one of the two patterns.
        let policy = crate::rbac::admission("w", "g")[0].to_string();
        assert!(policy.contains(&format!(
            "object.spec == {} ||",
            cel_literal(&tailnet_policy_spec(&[]))
        )));
        assert!(
            policy.contains(
                "'clientCIDRs': dyn(object.spec.authorization.rules[1].principal.clientCIDRs)"
            ) && !policy.contains("SERVERS")
        );
        let matches = format!(
            "c.matches({}) || c.matches({})",
            cel_literal(&json!(TAILNET_SERVER_CIDR_V4)),
            cel_literal(&json!(TAILNET_SERVER_CIDR_V6))
        );
        assert!(
            policy.contains(json!(matches).to_string().trim_matches('"')),
            "{matches}"
        );
    }

    /// The host's timer writes the Service that brings tailnet connections
    /// to the proxies with their own address: the listeners' ports, onto
    /// the proxies' pods, Local, on the Tailscale address only. Both copies
    /// of the timer's script (the install's and the dev setup's) agree.
    #[test]
    fn the_hosts_tailnet_service_matches_the_listeners() {
        for script in [
            include_str!("../../../deploy/install.sh"),
            include_str!("../../../scripts/pyo-up.sh"),
        ] {
            let start = script.find("kind: Service\n").expect("the Service");
            let end = start + script[start..].find("\nYAML\n").expect("its end");
            let doc: Value = serde_norway::from_str(
                &format!("apiVersion: v1\n{}", &script[start..end]).replace("$ip4", "100.64.0.1"),
            )
            .unwrap();
            assert_eq!(doc["metadata"]["name"], TAILNET_SERVICE);
            assert_eq!(doc["metadata"]["namespace"], GATEWAY_NS);
            assert_eq!(doc["spec"]["type"], "ClusterIP");
            assert_eq!(doc["spec"]["externalTrafficPolicy"], "Local");
            assert_eq!(doc["spec"]["externalIPs"], json!(["100.64.0.1"]));
            assert_eq!(doc["spec"]["selector"], proxy_pods()["matchLabels"]);
            let ports: Vec<(i64, i64)> = doc["spec"]["ports"]
                .as_array()
                .unwrap()
                .iter()
                .map(|p| {
                    (
                        p["port"].as_i64().unwrap(),
                        p["targetPort"].as_i64().unwrap(),
                    )
                })
                .collect();
            assert_eq!(
                ports,
                [
                    (DASHBOARD_PORT as i64, DASHBOARD_PORT as i64),
                    (REGISTRY_PORT as i64, REGISTRY_PORT as i64)
                ]
            );
            assert!(
                script[..start].contains("ip4=$(tailscale ip -4"),
                "the Tailscale address"
            );
        }
    }

    /// The agent's Service for websites on the servers' Tailscale
    /// addresses: the websites' ports onto the proxies' (10000 higher), on
    /// those addresses only, Local (the proxy sees the visitor's address).
    /// The host's Service keeps only the listeners' ports.
    #[test]
    fn websites_on_the_tailnet_are_the_agents_own_service() {
        let ips = vec!["100.64.0.1".to_owned(), "100.101.67.2".to_owned()];
        let svc = tailnet_web_service(&ips);
        assert_eq!(svc["metadata"]["name"], TAILNET_WEB_SERVICE);
        assert_ne!(TAILNET_WEB_SERVICE, TAILNET_SERVICE);
        assert_eq!(svc["metadata"]["namespace"], GATEWAY_NS);
        assert_eq!(
            svc["spec"],
            json!({
                "type": "ClusterIP", "externalIPs": ips, "externalTrafficPolicy": "Local",
                "selector": proxy_pods()["matchLabels"],
                "ports": [
                    { "name": "http", "port": 80, "targetPort": 10080, "protocol": "TCP" },
                    { "name": "https", "port": 443, "targetPort": 10443, "protocol": "TCP" },
                ],
            })
        );
        let v4 = regex::Regex::new(TAILNET_IPV4_RE).unwrap();
        let v6 = regex::Regex::new(TAILNET_IPV6_RE).unwrap();
        for ip in [
            "100.64.0.0",
            "100.101.67.2",
            "100.127.255.255",
            "100.99.0.9",
        ] {
            assert!(v4.is_match(ip), "{ip}");
        }
        for ip in [
            "100.63.255.255",
            "100.128.0.1",
            "10.0.0.1",
            "129.226.158.31",
            "100.64.0.256",
            "100.64.0.01",
            "100.64.0",
            " 100.64.0.1",
        ] {
            assert!(!v4.is_match(ip), "{ip}");
        }
        assert!(v6.is_match("fd7a:115c:a1e0::6930:4303"));
        assert!(!v6.is_match("fd7a:115c:a1e1::1") && !v6.is_match("2001:db8::1"));
        // The host's Service: still only the listeners' ports.
        for script in [
            include_str!("../../../deploy/install.sh"),
            include_str!("../../../scripts/pyo-up.sh"),
        ] {
            assert!(!script.contains("targetPort: 10080") && !script.contains("targetPort: 10443"));
            assert!(!script.contains(TAILNET_WEB_SERVICE));
        }
    }

    /// The API server keeps a Backend with the `type` Envoy Gateway's schema
    /// fills in, and admission sees it so: the specs the policy compares
    /// exactly must carry it, or the agent's own writes are refused.
    #[test]
    fn pinned_backends_carry_what_the_api_server_fills_in() {
        for (_, _, host, port) in TAILNET_ROUTES {
            assert_eq!(tailnet_backend_spec(host, port)["type"], "Endpoints");
        }
        assert_eq!(waf_backend_spec()["type"], "Endpoints");
        let policy = crate::rbac::admission("w", "g")[0].to_string();
        assert!(policy.contains(&format!(
            "object.spec == {}",
            cel_literal(&tailnet_backend_spec(
                TAILNET_ROUTES[0].2,
                TAILNET_ROUTES[0].3
            ))
        )));
        assert!(policy.contains(&format!(
            "object.spec == {}",
            cel_literal(&waf_backend_spec())
        )));
    }

    /// Envoy Gateway reads only its own namespace: the routes' WAF policies
    /// send to a Backend there, which reaches the WAF's Service by name.
    #[test]
    fn the_waf_is_reached_from_the_gateways_namespace() {
        let b = waf_backend();
        assert_eq!(b["metadata"]["namespace"], GATEWAY_NS);
        assert_eq!(
            b["spec"]["endpoints"],
            json!([{ "fqdn": { "hostname": "pyo-waf.pyo-waf.svc.cluster.local", "port": WAF_PORT } }])
        );
        let r = &waf_ext_proc(false)["backendRefs"];
        assert_eq!(
            r,
            &json!([{ "group": "gateway.envoyproxy.io", "kind": "Backend", "name": b["metadata"]["name"] }])
        );
        let policy = crate::rbac::admission("w", "g")[0].to_string();
        assert!(policy.contains(
            "variables.objName == 'pyo-waf' && (request.operation == 'DELETE' || object.spec == "
        ));
        assert!(policy.contains("b.kind == 'Backend' && b.group == 'gateway.envoyproxy.io'"));
    }

    /// A block-mode service's routes refuse requests when the WAF is
    /// unavailable, every other's pass them; the admission policy lets the
    /// agent write only that pairing, with this release's timeouts.
    #[test]
    fn waf_fails_closed_in_block_mode_only() {
        use crate::{WafCondition, WafRule, WafSettings};
        let route = |mode: &str, rules: Vec<WafRule>| WafRoute {
            hosts: vec!["shop.example.com".into()],
            settings: WafSettings {
                mode: mode.into(),
                level: 2,
                ..Default::default()
            },
            rules,
        };
        // A value that spells the mark out stays escaped in the JSON.
        let tricky = WafRule {
            id: 1_000_001,
            name: r#""settings":{"mode":"block""#.into(),
            matching: "all".into(),
            conditions: vec![WafCondition {
                field: "path".into(),
                op: "contains".into(),
                values: vec![r#""settings":{"mode":"block""#.into()],
                ..Default::default()
            }],
            action: "block".into(),
        };
        for (mode, closed) in [("block", true), ("log", false), ("off", false)] {
            let r = route(mode, vec![tricky.clone()]);
            assert_eq!(waf_fails_closed(&r), closed, "{mode}");
            assert_eq!(
                serde_json::to_string(&r).unwrap().contains(WAF_BLOCK_MARK),
                closed,
                "{mode}"
            );
            assert_eq!(
                serde_json::to_string(&route(mode, vec![]))
                    .unwrap()
                    .contains(WAF_BLOCK_MARK),
                closed,
                "{mode}"
            );
            let p = waf_ext_proc(closed);
            assert_eq!(p["failOpen"], !closed, "{mode}");
            assert_eq!(
                p["messageTimeout"],
                if closed {
                    WAF_BLOCK_TIMEOUT
                } else {
                    WAF_PASS_TIMEOUT
                }
            );
            assert_eq!(
                p["backendSettings"]["timeout"]["tcp"]["connectTimeout"],
                "1s"
            );
            // Block mode holds each request until the verdict on its body's
            // first part; the rest stream it.
            assert_eq!(
                p["processingMode"]["request"]["body"],
                if closed {
                    "BufferedPartial"
                } else {
                    "Streamed"
                },
                "{mode}"
            );
            assert_eq!(
                p["processingMode"]["request"]["attributes"],
                json!(["source.address"])
            );
        }
        // The part of a body block mode inspects is what the proxies buffer:
        // every client policy the agent may write sets it.
        for https in [false, true] {
            assert!(
                client_policy_variants(https)
                    .iter()
                    .all(|s| s["connection"]["bufferLimit"] == WAF_BODY_LIMIT)
            );
        }
        let spec = &crate::rbac::admission("w", "g")[0]["spec"];
        let all: String = ["variables", "validations"]
            .iter()
            .flat_map(|k| {
                spec[k]
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter_map(|v| v["expression"].as_str())
            })
            .collect();
        assert!(all.contains(r#"object.metadata.annotations[?'pyo.dev/waf'].orValue('').contains('"settings":{"mode":"block"')) ? {"#), "{all}");
        assert!(all.contains("'failOpen': dyn(false)") && all.contains("'failOpen': dyn(true)"));
        assert!(
            all.contains("'messageTimeout': dyn('5s')")
                && all.contains("'messageTimeout': dyn('500ms')")
        );
        assert!(all.contains("'statusOnError': dyn(503)"));
        assert!(
            all.contains("dyn('BufferedPartial')") && all.contains("dyn('Streamed')"),
            "the body mode is pinned per mode"
        );
        assert!(all.contains("[dyn('backendSettings'), dyn('failOpen'), dyn('messageTimeout'), dyn('processingMode'), dyn('statusOnError')].all(k, k in object.spec.extProc[0] && "));
        assert!(!all.contains("failOpen == true"), "no longer open always");
    }

    #[test]
    fn admission_keeps_the_tailnet_gateway_to_itself() {
        let policy = crate::rbac::admission("w", "g")[0].to_string();
        // The exact objects, and no other route may attach to the Gateway.
        assert!(policy.contains("variables.objName == 'pyo-tailnet' && (request.operation == 'DELETE' || object.spec == "));
        assert!(policy.contains("p.name != 'pyo-tailnet'"));
        assert!(policy.contains("variables.objName == 'pyo-dashboard'"));
        assert!(policy.contains("pyo.pyo-system.svc.cluster.local"));
        // Other Backends still can't point into Pyo's namespaces.
        assert!(
            policy.contains(
                "!(variables.objName in [dyn('pyo-dashboard'), dyn('pyo-registry-door')])"
            )
        );
    }

    use super::*;

    /// Fills in what a CRD's schema defaults (as the API server does
    /// before admission) where the object leaves it out.
    fn default_from(schema: &serde_json::Value, v: &mut Value) {
        match v {
            Value::Object(m) => {
                if let Some(props) = schema["properties"].as_object() {
                    for (k, sub) in props {
                        if !m.contains_key(k) && !sub["default"].is_null() {
                            m.insert(k.clone(), sub["default"].clone());
                        }
                        if let Some(x) = m.get_mut(k) {
                            default_from(sub, x);
                        }
                    }
                } else if schema["additionalProperties"].is_object() {
                    m.values_mut()
                        .for_each(|x| default_from(&schema["additionalProperties"], x));
                }
            }
            Value::Array(a) => a.iter_mut().for_each(|x| default_from(&schema["items"], x)),
            _ => {}
        }
    }

    /// The objects the admission policy pins to exactly Pyo's spec come
    /// back from the API server unchanged: none leaves out a field the
    /// CRDs default (one that did would be refused on every write after).
    #[test]
    fn pinned_objects_survive_crd_defaults() {
        use serde::Deserialize;
        let mut schemas = std::collections::BTreeMap::new();
        for file in ["envoy-gateway-crds.yaml", "gateway-api-crds.yaml"] {
            let text = std::fs::read_to_string(format!(
                "{}/../pyo-api/assets/gateway/{file}",
                env!("CARGO_MANIFEST_DIR")
            ))
            .unwrap();
            for doc in serde_norway::Deserializer::from_str(&text) {
                let Ok(crd) = Value::deserialize(doc) else {
                    continue;
                };
                let kind = crd["spec"]["names"]["kind"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string();
                for v in crd["spec"]["versions"].as_array().into_iter().flatten() {
                    let version = format!(
                        "{}/{}",
                        crd["spec"]["group"].as_str().unwrap_or_default(),
                        v["name"].as_str().unwrap_or_default()
                    );
                    schemas.insert(
                        (version, kind.clone()),
                        v["schema"]["openAPIV3Schema"]["properties"]["spec"].clone(),
                    );
                }
            }
        }
        let mut pinned = vec![
            envoy_proxy(GEOIP_IMAGE_DEFAULT, true),
            envoy_proxy(GEOIP_IMAGE_DEFAULT, false),
            gateway_class(),
            http_gateway(),
            waf_backend(),
        ];
        for https in [false, true] {
            pinned.extend(client_policy_variants(https).into_iter().map(|spec| json!({ "apiVersion": "gateway.envoyproxy.io/v1alpha1", "kind": "ClientTrafficPolicy", "spec": spec })));
        }
        pinned.extend(
            tailnet_objects(&tailnet_server_cidrs(["10.42.0.0/24"]))
                .into_iter()
                .filter(|o| o["kind"] != "HTTPRoute"),
        );
        for o in pinned {
            let key = (
                o["apiVersion"].as_str().unwrap().to_string(),
                o["kind"].as_str().unwrap().to_string(),
            );
            let schema = schemas
                .get(&key)
                .unwrap_or_else(|| panic!("no CRD for {key:?}"));
            let mut stored = o["spec"].clone();
            default_from(schema, &mut stored);
            assert_eq!(stored, o["spec"], "{key:?} {}", o["metadata"]["name"]);
        }
    }

    /// The fixed objects, for checking them against a cluster:
    /// `cargo test -p pyo-proto print_objects -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn print_objects() {
        let issuers: Vec<Value> = cluster_issuers().iter().map(|(n, spec)| json!({ "apiVersion": "cert-manager.io/v1", "kind": "ClusterIssuer", "metadata": { "name": n }, "spec": spec })).collect();
        for o in [
            envoy_proxy(GEOIP_IMAGE_DEFAULT, true),
            envoy_proxy(GEOIP_IMAGE_DEFAULT, false),
            gateway_class(),
            http_gateway(),
            client_policy(false, WebSettings::default()),
            client_policy(
                true,
                WebSettings {
                    origin_pull: true,
                    proxy_protocol: true,
                    direct: true,
                },
            ),
        ]
        .into_iter()
        .chain(issuers)
        .chain(tailnet_objects(&tailnet_server_cidrs(["10.42.0.0/24"])))
        .chain([cert_manager_chart(), envoy_gateway_chart()])
        {
            println!("{o}");
        }
    }

    #[test]
    fn cel_literals() {
        assert_eq!(
            cel_literal(&json!({ "a": ["x'y", 1] })),
            "{'a': dyn([dyn('x\\'y'), dyn(1)])}"
        );
        assert_eq!(cel_literal(&json!("a\nb\\c")), "'a\\nb\\\\c'");
    }
}
