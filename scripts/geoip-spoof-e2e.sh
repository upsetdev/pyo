#!/usr/bin/env bash
# Checks country rules against a real Envoy, the version Envoy Gateway runs
# (system.rs ENVOY_GATEWAY_VERSION's default proxy image), with Pyo's
# GeoIP database (deploy/images/geoip.Dockerfile, its pinned download).
#
# Envoy Gateway's country rules read a header its GeoIP filter sets,
# x-eg-internal-geoip-country. The filter sets it only when the lookup
# finds a country. For an address the database doesn't place, a header the
# client sent itself would decide. Every Pyo client policy removes
# x-eg-internal-* as requests come in (system.rs `visitor_headers`, Envoy's
# early header mutation). Behind Cloudflare and through the tunnel the
# country is Cloudflare's CF-IPCountry, checked after the behind gate
# (firewall.rs `COUNTRY_HEADER`). This builds the same Envoy config Envoy
# Gateway does for those and checks, with real connections:
#
#   - without the removal, a client the database doesn't place gets past
#     "only DE" by claiming DE (the hole);
#   - with it, the claim changes nothing, and neither does CF-IPCountry;
#   - an address the database places always gets its own country;
#   - behind the gate, CF-IPCountry decides for the CDN's requests, and a
#     direct client's never counts.
#
# Run it after every Envoy Gateway upgrade: ./scripts/geoip-spoof-e2e.sh
# Needs docker, curl, sha256sum and tar. Leaves nothing behind.
set -euo pipefail
cd "$(dirname "$0")/.."

ENVOY_IMAGE="${ENVOY_IMAGE:-docker.io/envoyproxy/envoy:distroless-v1.39.5}"
CURL_IMAGE="${CURL_IMAGE:-docker.io/curlimages/curl:8.17.0}"
NET="pyo-geoip-spoof-$$"
SUBNET="172.30.$((RANDOM % 200 + 20)).0/24"
PREFIX="${SUBNET%.0/24}"
ENVOY_IP="$PREFIX.2"
CDN_IP="$PREFIX.10"     # stands in for a Cloudflare edge server or a connector
DIRECT_IP="$PREFIX.20"  # anyone else; a private address the database doesn't place
# Public addresses the pinned database places in Russia and Germany.
RU_IP=77.88.8.8
DE_IP=46.4.0.1
DIR="$(mktemp -d)"

cleanup() {
  docker rm -f "$NET-envoy" >/dev/null 2>&1 || true
  docker network rm "$NET" >/dev/null 2>&1 || true
  rm -rf "$DIR"
}
trap cleanup EXIT

# The pinned GeoLite2 Country database, as the geoip image gets it.
arg() { sed -n "s/^ARG $1=//p" deploy/images/geoip.Dockerfile; }
DATE=$(arg GEOIP_DATE) COMMIT=$(arg GEOIP_COMMIT) SHA=$(arg GEOIP_COUNTRY_SHA256)
echo "Downloading the GeoIP database of $DATE"
curl -fsSL --proto '=https' -m 120 -o "$DIR/country.tar.gz" \
  "https://raw.githubusercontent.com/GitSquared/node-geolite2-redist/$COMMIT/redist/GeoLite2-Country.tar.gz"
echo "$SHA  $DIR/country.tar.gz" | sha256sum -c --quiet
mkdir -p "$DIR/geo"
tar -xzf "$DIR/country.tar.gz" -C "$DIR" "GeoLite2-Country_${DATE//-/}/GeoLite2-Country.mmdb"
mv "$DIR/GeoLite2-Country_${DATE//-/}/GeoLite2-Country.mmdb" "$DIR/geo/"
chmod 644 "$DIR/geo/GeoLite2-Country.mmdb"

# A listener: NAME PORT EARLY(yes|no) DETECTION GEOIP RULES DEFAULT
# DETECTION: hcm lines for the visitor; GEOIP: the geoip filter's own
# lines (where it reads the address); RULES: the RBAC matchers.
listener() {
  local name=$1 port=$2 early=$3 detection=$4 geoip=$5 rules=$6 default=$7
  cat <<EOF
  - name: $name
    address: { socket_address: { address: 0.0.0.0, port_value: $port } }
    filter_chains:
    - filters:
      - name: envoy.filters.network.http_connection_manager
        typed_config:
          "@type": type.googleapis.com/envoy.extensions.filters.network.http_connection_manager.v3.HttpConnectionManager
          stat_prefix: $name
$detection
EOF
  if [ "$early" = yes ]; then
    cat <<'EOF'
          # system.rs visitor_headers: removeOnMatch, as Envoy Gateway
          # writes it (listener.go buildEarlyHeaderMutation)
          early_header_mutation_extensions:
          - name: envoy.http.early_header_mutation.header_mutation
            typed_config:
              "@type": type.googleapis.com/envoy.extensions.http.early_header_mutation.header_mutation.v3.HeaderMutation
              mutations:
              - remove_on_match: { key_matcher: { prefix: "x-eg-internal-" } }
EOF
  fi
  cat <<EOF
          route_config:
            virtual_hosts:
            - name: all
              domains: ["*"]
              routes:
              - match: { prefix: "/" }
                direct_response: { status: 200, body: { inline_string: "ok" } }
                response_headers_to_add:
                - header: { key: x-country, value: "%REQ(x-eg-internal-geoip-country)%" }
          http_filters:
$geoip
          - name: envoy.filters.http.rbac
            typed_config:
              "@type": type.googleapis.com/envoy.extensions.filters.http.rbac.v3.RBAC
              matcher:
                matcher_list:
                  matchers:
$rules
                on_no_match: { action: { name: default, typed_config: { "@type": type.googleapis.com/envoy.config.rbac.v3.Action, name: $default, action: $default } } }
          - name: envoy.filters.http.router
            typed_config:
              "@type": type.googleapis.com/envoy.extensions.filters.http.router.v3.Router
EOF
}

# The geoip filter as Envoy Gateway writes it (geoip.go), reading the
# address from the connection, or (FROM_HEADER) from a test header, so a
# public address can be looked up from this network.
geoip_filter() {
  cat <<'EOF'
          - name: envoy.filters.http.geoip
            typed_config:
              "@type": type.googleapis.com/envoy.extensions.filters.http.geoip.v3.Geoip
EOF
  [ "${1:-}" != FROM_HEADER ] || echo '              custom_header_config: { header_name: x-test-ip }'
  cat <<'EOF'
              provider:
                name: envoy.geoip_providers.maxmind
                typed_config:
                  "@type": type.googleapis.com/envoy.extensions.geoip_providers.maxmind.v3.MaxMindConfig
                  country_db_path: /geo/GeoLite2-Country.mmdb
                  common_provider_config:
                    geo_field_keys: { country: x-eg-internal-geoip-country }
EOF
}

# One RBAC matcher on a header: NAME HEADER VALUE IGNORE_CASE ACTION, as
# Envoy Gateway matches a principal (authorization.go buildHeaderPredicate).
header_rule() {
  cat <<EOF
                  - predicate:
                      single_predicate:
                        input: { name: h, typed_config: { "@type": type.googleapis.com/envoy.type.matcher.v3.HttpRequestHeaderMatchInput, header_name: "$2" } }
                        value_match: { exact: "$3", ignore_case: $4 }
                    on_match: { action: { name: $1, typed_config: { "@type": type.googleapis.com/envoy.config.rbac.v3.Action, name: $5, action: $5 } } }
EOF
}

# firewall.rs behind_gate_rule: source.port != 0, as Envoy Gateway parses it.
gate_rule() {
  cat <<'EOF'
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
EOF
}

DIRECT='          use_remote_address: true'
XFF=$(cat <<EOF
          use_remote_address: false
          original_ip_detection_extensions:
          - name: envoy.extensions.http.original_ip_detection.xff
            typed_config:
              "@type": type.googleapis.com/envoy.extensions.http.original_ip_detection.xff.v3.XffConfig
              xff_trusted_cidrs: { cidrs: [ { address_prefix: "$CDN_IP", prefix_len: 32 } ] }
              skip_xff_append: false
EOF
)
ONLY_DE=$(header_rule de x-eg-internal-geoip-country DE true ALLOW)
BLOCK_RU=$(header_rule ru x-eg-internal-geoip-country RU true DENY)
# Behind the gate: block RU by Cloudflare's header (exact, as a headers
# principal is matched).
CF_BLOCK_RU="$(gate_rule)
$(header_rule ru cf-ipcountry RU false DENY)"

{
  echo "static_resources:"
  echo "  listeners:"
  # A direct domain whose only rule is "is not in DE": block (rest DE allowed).
  listener before 10080 no "$DIRECT" "$(geoip_filter)" "$ONLY_DE" DENY
  listener now 10081 yes "$DIRECT" "$(geoip_filter)" "$ONLY_DE" DENY
  # Block RU, with the address from a test header (a public address).
  listener lookup-before 10082 no "$DIRECT" "$(geoip_filter FROM_HEADER)" "$BLOCK_RU" ALLOW
  listener lookup-now 10083 yes "$DIRECT" "$(geoip_filter FROM_HEADER)" "$BLOCK_RU" ALLOW
  # Behind Cloudflare or the tunnel: no geoip filter (Envoy Gateway takes
  # none there), the gate, then the country as Cloudflare's header.
  listener behind 10084 yes "$XFF" "" "$CF_BLOCK_RU" ALLOW
} >"$DIR/envoy.yaml"

docker network create --subnet "$SUBNET" "$NET" >/dev/null
docker run -d --name "$NET-envoy" --network "$NET" --ip "$ENVOY_IP" --memory 512m \
  -v "$DIR/envoy.yaml:/etc/envoy/envoy.yaml:ro" -v "$DIR/geo:/geo:ro" "$ENVOY_IMAGE" \
  -c /etc/envoy/envoy.yaml --log-level warn >/dev/null

# ask FROM PORT [CURL ARGS...]: "<status> <country Envoy Gateway's rules saw>"
ask() {
  local from=$1 port=$2
  shift 2
  docker run --rm --network "$NET" --ip "$from" --memory 128m "$CURL_IMAGE" -s -o /dev/null \
    -w '%{http_code} %header{x-country}' "$@" "http://$ENVOY_IP:$port/"
}

for _ in $(seq 1 30); do
  if [ "$(ask "$DIRECT_IP" 10083 | cut -d' ' -f1)" = 200 ]; then break; fi
  sleep 1
done
if [ "$(ask "$DIRECT_IP" 10083 | cut -d' ' -f1)" != 200 ]; then
  echo "Envoy didn't start:"
  docker logs "$NET-envoy" 2>&1 | tail -20
  exit 2
fi

fail=0
check() {
  local what="$1" want="$2" got="$3"
  # shellcheck disable=SC2053
  if [[ "$got" == $want ]]; then
    echo "ok    $what ($got)"
  else
    echo "FAIL  $what: want $want, got $got"
    fail=1
  fi
}
SPOOF_DE=(-H 'x-eg-internal-geoip-country: DE')

echo "Without the removal (Envoy Gateway as it is):"
check "an unplaced client is refused" "403*" "$(ask "$DIRECT_IP" 10080)"
check "an unplaced client claiming DE gets in (the hole)" "200 DE" "$(ask "$DIRECT_IP" 10080 "${SPOOF_DE[@]}")"
check "a placed address gets its own country, whatever it claims" "403*" \
  "$(ask "$DIRECT_IP" 10082 -H "x-test-ip: $RU_IP" "${SPOOF_DE[@]}")"

echo "With Pyo's client policies:"
check "an unplaced client is refused" "403*" "$(ask "$DIRECT_IP" 10081)"
check "an unplaced client claiming DE is refused" "403 " "$(ask "$DIRECT_IP" 10081 "${SPOOF_DE[@]}")"
check "an unplaced client claiming DE in another case is refused" "403 " \
  "$(ask "$DIRECT_IP" 10081 -H 'X-EG-Internal-GeoIP-Country: DE')"
check "an unplaced client sending CF-IPCountry: DE is refused" "403 " "$(ask "$DIRECT_IP" 10081 -H 'CF-IPCountry: DE')"
check "a Russian address is refused, whatever it claims" "403*" \
  "$(ask "$DIRECT_IP" 10083 -H "x-test-ip: $RU_IP" "${SPOOF_DE[@]}")"
check "a German address gets in as DE" "200 DE" "$(ask "$DIRECT_IP" 10083 -H "x-test-ip: $DE_IP")"
check "an unplaced client claiming RU isn't placed in RU" "200 " \
  "$(ask "$DIRECT_IP" 10083 -H 'x-eg-internal-geoip-country: RU')"

echo "Behind Cloudflare or the tunnel (CF-IPCountry after the gate):"
check "the CDN's visitor from RU is refused" "403*" \
  "$(ask "$CDN_IP" 10084 -H 'X-Forwarded-For: 203.0.113.9' -H 'CF-IPCountry: RU')"
check "the CDN's visitor from DE gets in" "200*" \
  "$(ask "$CDN_IP" 10084 -H 'X-Forwarded-For: 203.0.113.9' -H 'CF-IPCountry: DE')"
check "the CDN's visitor's own x-eg-internal header changes nothing" "403*" \
  "$(ask "$CDN_IP" 10084 -H 'X-Forwarded-For: 203.0.113.9' -H 'CF-IPCountry: RU' "${SPOOF_DE[@]}")"
check "a direct client sending CF-IPCountry: DE is refused" "403*" \
  "$(ask "$DIRECT_IP" 10084 -H 'CF-IPCountry: DE')"
check "a direct client sending CF-IPCountry: DE and a visitor is refused" "403*" \
  "$(ask "$DIRECT_IP" 10084 -H 'X-Forwarded-For: 203.0.113.9' -H 'CF-IPCountry: DE')"

exit "$fail"
