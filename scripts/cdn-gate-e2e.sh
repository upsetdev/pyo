#!/usr/bin/env bash
# Checks the "Behind Cloudflare" gate against a real Envoy, the version
# Envoy Gateway runs (system.rs ENVOY_GATEWAY_VERSION's default proxy image).
#
# A website behind a CDN reads the visitor from X-Forwarded-For only on
# connections from the CDN's ranges (system.rs `cdn_client_policy_spec`),
# and refuses any request whose visitor wasn't read that way
# (firewall.rs `BEHIND_GATE_CEL`: an address read from the header has port
# 0). This builds the same Envoy config Envoy Gateway does for that (the
# xff original IP detection with trusted CIDRs, and RBAC as a matcher list
# with the CEL rule) and checks, with real connections:
#
#   - a direct client is refused, whatever X-Forwarded-For it sends,
#     a CDN address included;
#   - a connection from the CDN gets the visitor it forwards (the last
#     address that isn't the CDN's), not one the visitor made up;
#   - the service's own rules apply to that visitor;
#   - Let's Encrypt's challenge path gets through from anyone.
#
# Run it after every Envoy Gateway upgrade: ./scripts/cdn-gate-e2e.sh
# Needs docker. Leaves nothing behind.
set -euo pipefail

ENVOY_IMAGE="${ENVOY_IMAGE:-docker.io/envoyproxy/envoy:distroless-v1.39.5}"
CURL_IMAGE="${CURL_IMAGE:-docker.io/curlimages/curl:8.17.0}"
NET="pyo-cdn-gate-$$"
SUBNET="172.30.$((RANDOM % 200 + 20)).0/24"
PREFIX="${SUBNET%.0/24}"
ENVOY_IP="$PREFIX.2"
CDN_IP="$PREFIX.10"     # stands in for a Cloudflare edge server
DIRECT_IP="$PREFIX.20"  # anyone else on the internet
DIR="$(mktemp -d)"

cleanup() {
  docker rm -f "$NET-envoy" >/dev/null 2>&1 || true
  docker network rm "$NET" >/dev/null 2>&1 || true
  rm -rf "$DIR"
}
trap cleanup EXIT

cat >"$DIR/envoy.yaml" <<EOF
static_resources:
  listeners:
  - name: web
    address: { socket_address: { address: 0.0.0.0, port_value: 10080 } }
    filter_chains:
    - filters:
      - name: envoy.filters.network.http_connection_manager
        typed_config:
          "@type": type.googleapis.com/envoy.extensions.filters.network.http_connection_manager.v3.HttpConnectionManager
          stat_prefix: web
          # As Envoy Gateway sets it with a detection extension.
          use_remote_address: false
          original_ip_detection_extensions:
          - name: envoy.extensions.http.original_ip_detection.xff
            typed_config:
              "@type": type.googleapis.com/envoy.extensions.http.original_ip_detection.xff.v3.XffConfig
              xff_trusted_cidrs: { cidrs: [ { address_prefix: "$CDN_IP", prefix_len: 32 } ] }
              skip_xff_append: false
          route_config:
            virtual_hosts:
            - name: all
              domains: ["*"]
              routes:
              - match: { prefix: "/" }
                direct_response: { status: 200, body: { inline_string: "ok" } }
                response_headers_to_add:
                - header: { key: x-visitor, value: "%DOWNSTREAM_REMOTE_ADDRESS_WITHOUT_PORT%" }
          http_filters:
          - name: envoy.filters.http.rbac
            typed_config:
              "@type": type.googleapis.com/envoy.extensions.filters.http.rbac.v3.RBAC
              matcher:
                matcher_list:
                  matchers:
                  # firewall.rs acme_rule
                  - predicate:
                      single_predicate:
                        input: { name: path, typed_config: { "@type": type.googleapis.com/envoy.type.matcher.v3.HttpRequestHeaderMatchInput, header_name: ":path" } }
                        value_match: { prefix: "/.well-known/acme-challenge/" }
                    on_match: { action: { name: acme, typed_config: { "@type": type.googleapis.com/envoy.config.rbac.v3.Action, name: ALLOW, action: ALLOW } } }
                  # firewall.rs behind_gate_rule: source.port != 0, parsed
                  # as Envoy Gateway parses it (cel_expr_parsed)
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
                    on_match: { action: { name: cdn, typed_config: { "@type": type.googleapis.com/envoy.config.rbac.v3.Action, name: DENY, action: DENY } } }
                  # A service's own rule: block 198.51.100.0/24 (a clientCIDRs
                  # principal, as Envoy Gateway matches it)
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
          - name: envoy.filters.http.router
            typed_config:
              "@type": type.googleapis.com/envoy.extensions.filters.http.router.v3.Router
EOF

docker network create --subnet "$SUBNET" "$NET" >/dev/null
docker run -d --name "$NET-envoy" --network "$NET" --ip "$ENVOY_IP" \
  -v "$DIR/envoy.yaml:/etc/envoy/envoy.yaml:ro" "$ENVOY_IMAGE" \
  -c /etc/envoy/envoy.yaml --log-level warn >/dev/null

# From an address: "<status> <visitor>".
ask() {
  local from="$1" path="$2"
  shift 2
  docker run --rm --network "$NET" --ip "$from" "$CURL_IMAGE" -s -o /dev/null \
    -w '%{http_code} %header{x-visitor}' "$@" "http://$ENVOY_IP:10080$path"
}

for _ in $(seq 1 30); do
  if [ "$(ask "$DIRECT_IP" /.well-known/acme-challenge/ready | cut -d' ' -f1)" = 200 ]; then break; fi
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

check "a direct client is refused" "403*" "$(ask "$DIRECT_IP" /)"
check "a direct client naming a CDN address is refused" "403*" \
  "$(ask "$DIRECT_IP" / -H 'X-Forwarded-For: 104.16.0.1')"
check "a direct client naming a visitor is refused" "403*" \
  "$(ask "$DIRECT_IP" / -H 'X-Forwarded-For: 203.0.113.9')"
check "a direct client sending CF-Connecting-IP is refused" "403*" \
  "$(ask "$DIRECT_IP" / -H 'CF-Connecting-IP: 104.16.0.1')"
check "the CDN's visitor gets in as themselves" "200 203.0.113.9" \
  "$(ask "$CDN_IP" / -H 'X-Forwarded-For: 203.0.113.9')"
check "a visitor's own X-Forwarded-For doesn't count" "200 203.0.113.9" \
  "$(ask "$CDN_IP" / -H 'X-Forwarded-For: 10.9.9.9, 203.0.113.9')"
check "the service's rules see the CDN's visitor" "403*" \
  "$(ask "$CDN_IP" / -H 'X-Forwarded-For: 198.51.100.7')"
check "a CDN connection without a visitor is refused" "403*" "$(ask "$CDN_IP" /)"
check "Let's Encrypt's challenges get through from anyone" "200*" \
  "$(ask "$DIRECT_IP" /.well-known/acme-challenge/token)"

exit "$fail"
