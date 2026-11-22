#!/usr/bin/env bash
# Checks Cloudflare Tunnel's routes against a real Envoy, the version
# Envoy Gateway runs (system.rs ENVOY_GATEWAY_VERSION's default proxy image).
#
# A tunnel domain's listener reads the visitor from X-Forwarded-For only on
# connections from the tunnel's connectors, cloudflared's pods (gateway.rs,
# the "tunnel" client policy with their addresses). While its record moves
# ("switching") anyone gets through as themselves. Once it's switched
# ("on") the firewall's behind gate (firewall.rs `BEHIND_GATE_CEL`) goes
# first, so only the connectors' requests do, and a request Cloudflare took
# over plain HTTP (`cf-visitor: {"scheme":"http"}`) goes to HTTPS
# (gateway.rs `tunnel_redirect_rule`). This builds the same Envoy config
# Envoy Gateway does for that and checks, with real connections:
#
#   - switched: a direct client is refused, whatever X-Forwarded-For or
#     CF-Connecting-IP it sends;
#   - switched: the connector's visitor gets in as themselves, the last
#     address the connector appended, not one the visitor made up;
#   - the service's own rules apply to that visitor;
#   - a plain-HTTP visitor at Cloudflare is sent to HTTPS;
#   - switching: a direct client gets through as itself, and can't pick
#     its address with X-Forwarded-For.
#
# TLS is left out (plain HTTP to the listener): only the visitor and the
# gate are checked here.
#
# Run it after every Envoy Gateway upgrade: ./scripts/tunnel-gate-e2e.sh
# Needs docker. Leaves nothing behind.
set -euo pipefail

ENVOY_IMAGE="${ENVOY_IMAGE:-docker.io/envoyproxy/envoy:distroless-v1.39.5}"
CURL_IMAGE="${CURL_IMAGE:-docker.io/curlimages/curl:8.17.0}"
NET="pyo-tunnel-gate-$$"
SUBNET="172.30.$((RANDOM % 200 + 20)).0/24"
PREFIX="${SUBNET%.0/24}"
ENVOY_IP="$PREFIX.2"
CONNECTOR_IP="$PREFIX.10" # stands in for a cloudflared pod
DIRECT_IP="$PREFIX.20"    # another pod, or a visitor reaching the server
DIR="$(mktemp -d)"

cleanup() {
  docker rm -f "$NET-envoy" >/dev/null 2>&1 || true
  docker network rm "$NET" >/dev/null 2>&1 || true
  rm -rf "$DIR"
}
trap cleanup EXIT

# The gate (firewall.rs behind_gate_rule: source.port != 0, as Envoy
# Gateway parses it) and a service's own rule (block 198.51.100.0/24), as
# an RBAC matcher list. Switched routes get the gate first, switching ones
# only the service's own rule.
rbac() {
  local gate="$1"
  cat <<EOF
              "@type": type.googleapis.com/envoy.extensions.filters.http.rbac.v3.RBACPerRoute
              rbac:
                matcher:
                  matcher_list:
                    matchers:
EOF
  if [ "$gate" = yes ]; then
    cat <<EOF
                    - predicate:
                        single_predicate:
                          input: { name: cel, typed_config: { "@type": type.googleapis.com/xds.type.matcher.v3.HttpAttributesCelMatchInput } }
                          custom_match:
                            name: cel_matcher
                            typed_config:
                              "@type": type.googleapis.com/xds.type.matcher.v3.CelMatcher
                              expr_match:
                                cel_expr_parsed:
                                  expr:
                                    id: 3
                                    call_expr:
                                      function: "_!=_"
                                      args:
                                      - id: 2
                                        select_expr: { operand: { id: 1, ident_expr: { name: source } }, field: port }
                                      - id: 4
                                        const_expr: { int64_value: 0 }
                      on_match: { action: { name: tunnel, typed_config: { "@type": type.googleapis.com/envoy.config.rbac.v3.Action, name: DENY, action: DENY } } }
EOF
  fi
  cat <<EOF
                    - predicate:
                        single_predicate:
                          input: { name: source_ip, typed_config: { "@type": type.googleapis.com/envoy.extensions.matching.common_inputs.network.v3.SourceIPInput } }
                          custom_match:
                            name: ip_matcher
                            typed_config:
                              "@type": type.googleapis.com/envoy.extensions.matching.input_matchers.ip.v3.Ip
                              stat_prefix: block
                              cidr_ranges: [ { address_prefix: "198.51.100.0", prefix_len: 24 } ]
                      on_match: { action: { name: block, typed_config: { "@type": type.googleapis.com/envoy.config.rbac.v3.Action, name: DENY, action: DENY } } }
                  on_no_match: { action: { name: default, typed_config: { "@type": type.googleapis.com/envoy.config.rbac.v3.Action, name: ALLOW, action: ALLOW } } }
EOF
}

cat >"$DIR/envoy.yaml" <<EOF
static_resources:
  listeners:
  - name: tunnel
    address: { socket_address: { address: 0.0.0.0, port_value: 10443 } }
    filter_chains:
    - filters:
      - name: envoy.filters.network.http_connection_manager
        typed_config:
          "@type": type.googleapis.com/envoy.extensions.filters.network.http_connection_manager.v3.HttpConnectionManager
          stat_prefix: tunnel
          # The tunnel client policy: X-Forwarded-For from the connectors only.
          use_remote_address: false
          original_ip_detection_extensions:
          - name: envoy.extensions.http.original_ip_detection.xff
            typed_config:
              "@type": type.googleapis.com/envoy.extensions.http.original_ip_detection.xff.v3.XffConfig
              xff_trusted_cidrs: { cidrs: [ { address_prefix: "$CONNECTOR_IP", prefix_len: 32 } ] }
              skip_xff_append: false
          route_config:
            virtual_hosts:
            - name: switched
              domains: ["done.example.com"]
              typed_per_filter_config:
                envoy.filters.http.rbac:
$(rbac yes)
              routes:
              # gateway.rs tunnel_redirect_rule
              - match:
                  prefix: "/"
                  headers: [ { name: cf-visitor, string_match: { exact: '{"scheme":"http"}' } } ]
                redirect: { scheme_redirect: https, response_code: MOVED_PERMANENTLY }
              - match: { prefix: "/" }
                direct_response: { status: 200, body: { inline_string: "ok" } }
                response_headers_to_add:
                - header: { key: x-visitor, value: "%DOWNSTREAM_REMOTE_ADDRESS_WITHOUT_PORT%" }
            - name: switching
              domains: ["moving.example.com"]
              typed_per_filter_config:
                envoy.filters.http.rbac:
$(rbac no)
              routes:
              - match: { prefix: "/" }
                direct_response: { status: 200, body: { inline_string: "ok" } }
                response_headers_to_add:
                - header: { key: x-visitor, value: "%DOWNSTREAM_REMOTE_ADDRESS_WITHOUT_PORT%" }
          http_filters:
          - name: envoy.filters.http.rbac
            typed_config:
              "@type": type.googleapis.com/envoy.extensions.filters.http.rbac.v3.RBAC
          - name: envoy.filters.http.router
            typed_config:
              "@type": type.googleapis.com/envoy.extensions.filters.http.router.v3.Router
EOF

docker network create --subnet "$SUBNET" "$NET" >/dev/null
docker run -d --memory 512m --name "$NET-envoy" --network "$NET" --ip "$ENVOY_IP" \
  -v "$DIR/envoy.yaml:/etc/envoy/envoy.yaml:ro" "$ENVOY_IMAGE" \
  -c /etc/envoy/envoy.yaml --log-level warn >/dev/null

# From an address, for a host: "<status> <visitor or redirect>".
ask() {
  local from="$1" host="$2"
  shift 2
  docker run --rm --memory 128m --network "$NET" --ip "$from" "$CURL_IMAGE" -s -o /dev/null \
    -w '%{http_code} %header{x-visitor}%header{location}' -H "Host: $host" "$@" "http://$ENVOY_IP:10443/"
}

for _ in $(seq 1 30); do
  if [ "$(ask "$DIRECT_IP" moving.example.com | cut -d' ' -f1)" = 200 ]; then break; fi
  sleep 1
done

fail=0
check() {
  local what="$1" want="$2" got="$3"
  if [[ "$got" == $want ]]; then
    echo "ok    $what ($got)"
  else
    echo "FAIL  $what: want $want, got $got"
    fail=1
  fi
}

check "switched: a direct client is refused" "403*" "$(ask "$DIRECT_IP" done.example.com)"
check "switched: a direct client naming a visitor is refused" "403*" \
  "$(ask "$DIRECT_IP" done.example.com -H 'X-Forwarded-For: 203.0.113.9')"
check "switched: a direct client sending CF-Connecting-IP is refused" "403*" \
  "$(ask "$DIRECT_IP" done.example.com -H 'CF-Connecting-IP: 203.0.113.9')"
check "switched: the connector's visitor gets in as themselves" "200 203.0.113.9" \
  "$(ask "$CONNECTOR_IP" done.example.com -H 'X-Forwarded-For: 203.0.113.9')"
check "switched: a visitor's own X-Forwarded-For doesn't count" "200 203.0.113.9" \
  "$(ask "$CONNECTOR_IP" done.example.com -H 'X-Forwarded-For: 10.9.9.9, 203.0.113.9')"
check "switched: the service's rules see the connector's visitor" "403*" \
  "$(ask "$CONNECTOR_IP" done.example.com -H 'X-Forwarded-For: 198.51.100.7')"
check "switched: plain HTTP at Cloudflare goes to HTTPS" "301 https://done.example.com/" \
  "$(ask "$CONNECTOR_IP" done.example.com -H 'X-Forwarded-For: 203.0.113.9' -H 'cf-visitor: {"scheme":"http"}')"
check "switched: HTTPS at Cloudflare isn't redirected" "200*" \
  "$(ask "$CONNECTOR_IP" done.example.com -H 'X-Forwarded-For: 203.0.113.9' -H 'cf-visitor: {"scheme":"https"}')"
check "switching: a direct client gets through as itself" "200 $DIRECT_IP" \
  "$(ask "$DIRECT_IP" moving.example.com)"
check "switching: a direct client can't pick its address" "200 $DIRECT_IP" \
  "$(ask "$DIRECT_IP" moving.example.com -H 'X-Forwarded-For: 203.0.113.9')"
check "switching: the connector's visitor gets in as themselves" "200 203.0.113.9" \
  "$(ask "$CONNECTOR_IP" moving.example.com -H 'X-Forwarded-For: 203.0.113.9')"

exit "$fail"
