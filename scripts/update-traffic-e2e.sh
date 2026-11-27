#!/usr/bin/env bash
# Counts what visitors lose while Pyo's inbound path restarts or updates:
# Pyo Gateway (Envoy), the WAF and the tunnel connector.
#
#   scripts/update-traffic-e2e.sh fast   [--servers 2] [--out DIR] [--keep]
#   scripts/update-traffic-e2e.sh fast   --dev [--dev-tag TAG] [--out DIR] [--keep]
#   scripts/update-traffic-e2e.sh update [--dev-tag TAG] [--out DIR] [--keep]
#   scripts/update-traffic-e2e.sh paths  [--dev-tag TAG] [--out DIR] [--keep]
#   scripts/update-traffic-e2e.sh gateway [--dev-tag TAG] [--out DIR] [--keep]
#
# It makes its own throwaway multipass VMs (zd-e2e-1, zd-e2e-2), installs a
# published Pyo release on the first (--from, default 0.51.0), and deploys
# two small echo services on nip.io domains of the VM's private address:
#
#   web  no WAF
#   waf  the WAF on, in block mode
#
# Then it sends traffic to both from this machine, with no retries
# (update-traffic-e2e.py load), and logs every request:
#
#   fresh      10 requests a second, a new connection each
#   keepalive  10 requests a second on one connection, a POST every second
#   long       one request that takes 20 seconds, again and again
#   ws         one WebSocket, a message every second
#
# fast    restarts each part in turn, with a quiet gap between: Envoy, the
#         WAF, the tunnel connector, Envoy Gateway's controller, the agent
#         and pyo-tailnet (kubectl rollout restart). Then it changes a
#         firewall rule and a domain, and restarts k3s. About 15 minutes,
#         20 with 2 servers. With --dev it first updates to the dev build,
#         as update does, and sends traffic only once that's over: the
#         restarts are then the dev build's. One server only, and it needs
#         bun and docker too. About 10 minutes more.
# update  updates the install to the dev build with this branch's
#         install.sh, and waits until every rollout is over. The dev build
#         is the one scripts/pyo-up.sh pushed to the local registry
#         (PYO_IMAGES, default 127.0.0.1:5005/pyo): --dev-tag names its
#         tag, the newest one otherwise. It isn't signed, so only the
#         signature check is left out. One server only. Needs bun and
#         docker. About 10 minutes.
# paths   the cluster's traffic paths on the dev build (setClusterPaths,
#         docs/traffic-and-gateway-design.md §16.3), under traffic: Tailnet
#         off and on, PROXY protocol on and off, one domain moved to the
#         tunnel and back, Public ports off (the domain moves first, the
#         ports close after it) and on, and Cloudflare Tunnel off. Needs
#         PYO_E2E_CF_TOKEN (below). One server, like update. Each step
#         prints its numbers per target. The moving domain (mv,
#         zd-mv-<random>.<zone>) is probed on both of its paths 4 times a
#         second while it moves: "both down" counts the moments neither
#         answered, which a visitor following DNS would have seen. A
#         third service, tcp, has a public TCP port (18080) through the
#         gateway: it's probed the same way, and the run turns TCP and UDP
#         ports off and on while public ports are off.
# gateway the cluster's Gateway section on the dev build
#         (setClusterGateway, docs/traffic-and-gateway-design.md §15),
#         under traffic: compression on and off (each answer checked for
#         zstd, br and gzip as asked), the request timeout default changed
#         (a slower request checked for its 504) and back, the idle
#         timeout default changed and back, the TLS minimum 1.3 (TLS 1.2
#         refused, 1.3 answered) and back, and HTTP/3 on (one real
#         HTTP/3 request, from the VM with Ubuntu's python3-aioquic) and
#         off. One server, like paths. The tunnel leg runs only with
#         PYO_E2E_CF_TOKEN. Its VMs are zd-gw-1 and zd-gw-2.
#
# A watcher on the server logs every pod that starts, turns ready, stops
# or goes away, so each failure is matched to the rollouts running then.
# The summary lists, per domain: requests sent, failed (by status), long
# requests cut, WebSocket drops, and when each happened.
#
# Pass: no fresh or keepalive request failed. Long requests and WebSockets
# that were cut are reported, not failed.
#
# --servers 2  joins the second VM as a plain k3s agent over the VMs'
#              network, and sends the same traffic to it too. Pyo's own
#              "Add server" command needs Tailscale, which this test
#              doesn't use.
# --out DIR    keeps the logs and the summary there
# --keep       leaves the VMs running afterwards
# --gap N      seconds of quiet between actions (30)
# --rate N     requests a second, per domain and connection style (10)
#
# The tunnel leg runs when PYO_E2E_CF_TOKEN holds a Cloudflare API token
# for one throwaway zone (Zone:Read, DNS:Edit, Cloudflare Tunnel:Edit, SSL
# and Certificates:Edit). It refuses a token that sees more than one zone.
# Never give it a zone people use. Through Pyo's API it connects
# Cloudflare, turns on Cloudflare Tunnel for the cluster, goes back to
# public ports with the tunnel kept (so web and waf keep their direct
# path), and puts two new names of the zone on the tunnel:
#
#   tun     zd-tun-<random>.<zone>, a domain of web
#   tunwaf  zd-tunwaf-<random>.<zone>, a domain of waf (the WAF on)
#
# They get the same traffic, through Cloudflare's edge: the edge address
# comes from Cloudflare's resolver, and its certificate is checked. A
# failed answer is logged as Cloudflare's own or the origin's. fast also
# restarts the connector and rotates the tunnel's token (rotateTunnelToken).
# Without the token it prints "Tunnel domain: skipped: no test zone".
#
# At the end, however it ends, Pyo removes the domains, the tunnel and the
# token (removeDomain, removeClusterTunnel, removeCloudflare). Then
# Cloudflare's API is asked what's left of this run: records on the
# test's names, tunnels of the test's cluster, Pyo's checks of this
# install, Origin CA certificates for the test's names. Those are deleted.
# Nothing the zone had before the test is touched. It prints whether the
# zone is as before.
#
# The VMs are deleted when it ends, however it ends. It refuses to start
# when a VM of those names exists, and never touches any other VM. Needs
# multipass, python3, curl and jq, and 3 GB of free memory for the first
# VM plus 2 GB for the second.
set -euo pipefail
cd "$(dirname "$0")/.."
PY=scripts/update-traffic-e2e.py
RELEASES=https://github.com/upsetdev/pyo/releases/download
ECHO_IMAGE=${ECHO_IMAGE:-traefik/whoami:v1.11.0}
VM_IMAGE=${PYO_E2E_VM_IMAGE:-24.04}
# The registry every Pyo install has on its own server.
VM_IMAGES=127.0.0.1:5005/pyo
DEV_IMAGES=${PYO_IMAGES:-127.0.0.1:5005/pyo}
VM1=zd-e2e-1 VM2=zd-e2e-2
[ "${1:-}" != gateway ] || VM1=zd-gw-1 VM2=zd-gw-2
# paths: the tcp service's public TCP port.
TCP_PORT=18080

MODE=${1:-}
[ $# -eq 0 ] || shift
SERVERS=1 KEEP="" OUT="" GAP=30 RATE=10 FROM=0.51.0 DEV_TAG="" DEV=""
while [ $# -gt 0 ]; do
  case "$1" in
    --servers) SERVERS=${2:?--servers takes 1 or 2}; shift 2 ;;
    --keep) KEEP=1; shift ;;
    --out) OUT=${2:?--out takes a folder}; shift 2 ;;
    --gap) GAP=${2:?--gap takes seconds}; shift 2 ;;
    --rate) RATE=${2:?--rate takes a number}; shift 2 ;;
    --from) FROM=${2:?--from takes a version}; FROM=${FROM#v}; shift 2 ;;
    --dev-tag) DEV_TAG=${2:?--dev-tag takes a tag}; shift 2 ;;
    --dev) DEV=1; shift ;;
    *) echo "Unknown option: $1 (see the top of scripts/update-traffic-e2e.sh)" >&2; exit 2 ;;
  esac
done
case "$MODE" in
  fast | update | paths | gateway) ;;
  *) echo "Usage: update-traffic-e2e.sh fast|update|paths|gateway [--servers 2] [--out DIR] [--keep]" >&2; exit 2 ;;
esac
[ "$MODE" != gateway ] || DEV=1
if [ "$MODE" = paths ]; then
  DEV=1
  [ -n "${PYO_E2E_CF_TOKEN:-}" ] || { echo "paths needs PYO_E2E_CF_TOKEN: a Cloudflare token for one throwaway zone." >&2; exit 2; }
fi
case "$SERVERS" in 1 | 2) ;; *) echo "--servers takes 1 or 2." >&2; exit 2 ;; esac
[ "$MODE" != update ] || DEV=1
if [ -n "$DEV" ] && [ "$SERVERS" != 1 ]; then
  echo "The dev build runs on one server: a second one can't pull it without Tailscale." >&2
  exit 2
fi

say() { printf '%s  %s\n' "$(date +%H:%M:%S)" "$*"; }
note() { printf '          %s\n' "$*"; }
die() {
  printf 'Error: %s\n' "$*" >&2
  exit 1
}
indent() { sed -u -e 's/^\(  Password: \).*/\1(hidden)/' -e 's/^/          | /'; }
rand() { head -c 5 /dev/urandom | od -An -tx1 | tr -d ' \n'; }

need="multipass python3 curl jq"
[ -z "$DEV" ] || need="$need bun docker"
for tool in $need; do
  command -v "$tool" >/dev/null || die "$tool is missing. Install it and run this again."
done
want_gb=$((3 + (SERVERS - 1) * 2))
free_gb=$(LC_ALL=C awk '/^MemAvailable:/ { print int($2 / 1048576) }' /proc/meminfo)
[ "$free_gb" -ge $((want_gb + 1)) ] || die "The VMs need $want_gb GB of memory, and $free_gb GB is free."

DIR=$(mktemp -d)
chmod 700 "$DIR"
CREATED=() LOAD_PID="" PULLED="" KEY="" URL="" PROBE_PID=""
# The tunnel leg: on with PYO_E2E_CF_TOKEN. CF_USED once Pyo has the token.
TUNNEL=${PYO_E2E_CF_TOKEN:+1} CF_TOKEN="" CF_USED="" CLUSTER="" TEAM="" INSTALL_ID="" TUN_HOST="" TUNWAF_HOST="" MV_HOST=""
TUN_DOMAINS=()
: >"$DIR/actions.log"

cleanup() {
  local rc=$? n
  trap - EXIT INT TERM
  set +e
  if [ -n "$LOAD_PID" ]; then
    touch "$DIR/stop"
    kill "$LOAD_PID" 2>/dev/null || true
  fi
  if [ -n "$PROBE_PID" ]; then
    touch "$DIR/probe-stop"
    kill "$PROBE_PID" 2>/dev/null || true
  fi
  # Before the VMs go: Pyo's own remove actions need them.
  [ -z "$CF_USED" ] || cf_cleanup
  [ -z "$PULLED" ] || docker rmi "$PULLED" >/dev/null 2>&1 || true
  if [ "${#CREATED[@]}" -gt 0 ]; then
    if [ -n "$KEEP" ]; then
      say "Kept ${CREATED[*]}. Delete them: multipass delete --purge ${CREATED[*]}"
    else
      say "Deleting ${CREATED[*]}"
      for n in "${CREATED[@]}"; do
        multipass delete --purge "$n" >/dev/null 2>&1 || echo "Couldn't delete $n. Delete it: multipass delete --purge $n" >&2
      done
    fi
  fi
  rm -rf "$DIR"
  exit "$rc"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

# ── The VMs ─────────────────────────────────────────────────────────────────

vm() {
  local n=$1
  shift
  multipass exec "$n" -- sudo "$@"
}
kc() { vm "$VM1" k3s kubectl "$@"; }
# send FILE PATH: a file to the first VM. multipass exec cuts its input off
# after about 2 MB, and multipass can't read this machine's /tmp itself.
send() {
  multipass transfer - "$VM1:/home/ubuntu/e2e-sent" <"$1"
  vm "$VM1" install -m 644 /home/ubuntu/e2e-sent "$2"
  vm "$VM1" rm -f /home/ubuntu/e2e-sent
}
ip_of() { multipass info "$1" --format json | jq -r --arg n "$1" '.info[$n].ipv4[0]'; }

# wait_for SECONDS WHAT COMMAND...: 1 when it didn't happen in time.
wait_for() {
  local limit=$1 what=$2 start=$SECONDS told=0
  shift 2
  until "$@" >/dev/null 2>&1; do
    if [ $((SECONDS - start)) -ge "$limit" ]; then
      note "Gave up after $limit seconds: $what"
      return 1
    fi
    if [ $((SECONDS - start - told)) -ge 60 ]; then
      told=$((SECONDS - start))
      note "Waiting until $what ($((told / 60)) min)"
    fi
    sleep 3
  done
}

launch() { # NAME MEMORY
  if multipass info "$1" >/dev/null 2>&1; then
    die "A VM named $1 exists. This test makes and deletes its own, so delete that one first: multipass delete --purge $1"
  fi
  say "Creating $1 (2 CPUs, $2 of memory)"
  CREATED+=("$1")
  multipass launch "$VM_IMAGE" --name "$1" --cpus 2 --memory "$2" --disk 20G >/dev/null
}

install_pyo() {
  say "Installing Pyo $FROM on $VM1 (about 5 minutes)"
  local sum
  sum=$(jq -r '.scripts["install.sh"] // empty' "$DIR/from.json")
  [ -n "$sum" ] || die "The release.json of Pyo $FROM doesn't list its install.sh."
  # PYO_TELEMETRY=off: a test isn't an install.
  multipass exec "$VM1" -- bash -s -- "$RELEASES/v$FROM/install.sh" "$sum" "$FROM" <<'SH' 2>&1 | indent || die "Pyo $FROM didn't install on $VM1."
set -euo pipefail
curl -fsSL --proto '=https' --max-time 120 -o install.sh "$1"
[ "$(sha256sum install.sh | cut -d' ' -f1)" = "$2" ] || { echo "install.sh isn't the one release.json lists."; exit 1; }
sudo env PYO_TELEMETRY=off bash install.sh --version "$3" --yes </dev/null
SH
}

# The first sign-in, then an API key. The key is all that leaves the VM.
sign_in() {
  say "Signing in and making an API key"
  KEY=$(
    multipass exec "$VM1" -- sudo bash -s <<'SH'
set -euo pipefail
umask 077
t=$(mktemp -d)
trap 'rm -rf "$t"' EXIT
post() {
  curl -fsS -m 30 -o "$t/body" -b "$t/jar" -c "$t/jar" -H 'x-pyo-csrf: 1' -H 'Content-Type: application/json' \
    --data @- "http://127.0.0.1:8000$1"
}
first=$(k3s kubectl -n pyo-system get secret pyo-admin -o jsonpath='{.data.password}' | base64 -d)
new=$(head -c 48 /dev/urandom | base64 | tr -dc A-Za-z0-9 | cut -c1-28)
printf '{"email":"admin@pyo.local","password":"%s"}' "$first" | post /api/auth/login
printf '{"name":"E2E","email":"e2e@example.com","password":"%s"}' "$new" | post /api/auth/setup
printf '{"op":"createApiToken","id":"tok_e2e%s","name":"update-traffic-e2e"}' "$(head -c 5 /dev/urandom | od -An -tx1 | tr -d ' \n')" |
  post /api/v1/ops
sed -n 's/.*"token":"\([^"]*\)".*/\1/p' "$t/body"
SH
  ) || die "Couldn't sign in to Pyo on $VM1."
  [ -n "$KEY" ] || die "Pyo on $VM1 gave no API key."
  (
    umask 077
    printf 'Authorization: Bearer %s\n' "$KEY" >"$DIR/auth"
  )
}

api() { # METHOD PATH [JSON]
  local args=(-sS --fail-with-body -m 60 -H "@$DIR/auth" -X "$1")
  [ -z "${3:-}" ] || args+=(-H 'Content-Type: application/json' --data "$3")
  curl "${args[@]}" "$URL$2"
}
op() { api POST /api/v1/ops "$1"; }
state() { api GET /api/v1/state; }
cluster_connected() { state | jq -e '.clusters[0].status == "connected"'; }

# The tcp service's public TCP port answers (plain HTTP over it).
tcp_answers() { [ "$(curl -s -o /dev/null -w '%{http_code}' -m 3 "http://$IP1:$TCP_PORT/" || true)" = 200 ]; }

# answers HOST IP PATH [STATUS]
answers() {
  [ "$(curl -ks -o /dev/null -w '%{http_code}' -m 5 --resolve "$1:443:$2" "https://$1/$3" || true)" = "${4:-200}" ]
}

join_second() {
  say "Joining $VM2 as a second server"
  note "As a plain k3s agent over the VMs' network. Pyo's own command needs Tailscale."
  local sum
  sum=$(vm "$VM1" sha256sum /usr/local/bin/k3s | cut -d' ' -f1)
  [ "$sum" = "$(jq -r .k3s.amd64Sha256 "$DIR/from.json")" ] || die "k3s on $VM1 isn't the one Pyo $FROM pins."
  {
    # Read by the VM's shell, so the join token is in no command line.
    printf 'export K3S_URL=%q K3S_TOKEN=%q\n' "https://$IP1:6443" "$(vm "$VM1" cat /var/lib/rancher/k3s/server/node-token)"
    printf 'export PYO_K3S_VERSION=%q PYO_K3S_INSTALLER_SHA256=%q PYO_K3S_SHA256_AMD64=%q PYO_K3S_SHA256_ARM64=%q\n' \
      "$(jq -r .k3s.version "$DIR/from.json")" "$(jq -r .k3s.installerSha256 "$DIR/from.json")" \
      "$(jq -r .k3s.amd64Sha256 "$DIR/from.json")" "$(jq -r .k3s.arm64Sha256 "$DIR/from.json")"
    printf 'url=%q sum=%q\n' "$RELEASES/v$FROM/install.sh" "$(jq -r '.scripts["install.sh"]' "$DIR/from.json")"
    cat <<'SH'
set -euo pipefail
cd /root
curl -fsSL --proto '=https' --max-time 120 -o install.sh "$url"
[ "$(sha256sum install.sh | cut -d' ' -f1)" = "$sum" ] || { echo "install.sh isn't the one release.json lists."; exit 1; }
# install.sh's own k3s download, which checks the installer and the binary.
source <(sed '$d' install.sh)
k3s_fetch /root/k3s "$PYO_K3S_VERSION"
k3s_install /root/k3s agent --node-name "$(hostname)" </dev/null >/dev/null
echo "k3s agent installed"
SH
  } | multipass exec "$VM2" -- sudo bash -s 2>&1 | indent || die "k3s didn't install on $VM2."
  wait_for 300 "both servers are ready" two_ready || die "$VM2 didn't join the cluster."
}
two_ready() { [ "$(kc get nodes --no-headers | grep -cw Ready)" = 2 ]; }
envoy_on_both() { [ "$(kc -n pyo-gateway get daemonset -o jsonpath='{.items[0].status.numberReady}')" = 2 ]; }

deploy() {
  local dash=${IP1//./-} plan compose
  RUN="e2e-traffic-$(rand)"
  WEB_HOST="web.$dash.nip.io" WAF_HOST="waf.$dash.nip.io" EXTRA_HOST="extra.$dash.nip.io"
  say "Deploying $RUN: https://$WEB_HOST and https://$WAF_HOST"
  compose=$(printf 'services:\n  web:\n    image: %s\n    ports: ["80"]\n  waf:\n    image: %s\n    ports: ["80"]\n' "$ECHO_IMAGE" "$ECHO_IMAGE")
  # paths: a public TCP port too, through the gateway (TCP_PORT).
  [ "$MODE" != paths ] || compose+=$(printf '\n  tcp:\n    image: %s\n    ports: ["80"]\n' "$ECHO_IMAGE")
  plan=$(api POST /api/v1/compose/preview "$(jq -n --arg t "$TEAM" --arg c "$CLUSTER" --arg f "$compose" '{teamId: $t, clusterId: $c, file: $f}')") ||
    die "Pyo couldn't read the test's Compose file: $plan"
  plan=$(jq --arg n "$RUN" --arg web "$WEB_HOST" --arg waf "$WAF_HOST" '
    .project.name = $n | .project.namespace = $n
    | (.services[] | select(.name == "web") | .ports[0].domain) = $web
    | (.services[] | select(.name == "waf") | .ports[0].domain) = $waf
    | (.services[] | select(.name == "tcp") | .ports[0]) |= (.protocol = "TCP" | .public = true | .publicPort = 18080 | .domain = null)' <<<"$plan")
  out=$(op "$(jq -n --arg t "$TEAM" --arg c "$CLUSTER" --argjson p "$plan" '{op: "importCompose", teamId: $t, clusterId: $c, plan: $p}')") ||
    die "Pyo couldn't deploy the test project: $out"
  WEB_ID=$(service_id web)
  WAF_ID=$(service_id waf)
  [ -n "$WEB_ID" ] && [ -n "$WAF_ID" ] || die "No services in $RUN after the deploy."
  local host start=$SECONDS why
  for host in "$WEB_HOST" "$WAF_HOST"; do
    until answers "$host" "$IP1" ""; do
      why=$(failed_deploy)
      [ -z "$why" ] || { diagnose; die "The test project's deploy failed: $why"; }
      [ $((SECONDS - start)) -lt 600 ] || { diagnose; die "$host didn't answer over HTTPS in 10 minutes."; }
      sleep 5
    done
  done

  say "Turning the WAF on for waf, in block mode"
  out=$(op "$(jq -n --arg s "$WAF_ID" '{op: "setServiceWaf", serviceId: $s, mode: "block"}')") || die "Pyo couldn't turn the WAF on: $out"
  local ip
  for ip in "$IP1" ${IP2:+"$IP2"}; do
    wait_for 600 "the WAF blocks a test attack on $ip" answers "$WAF_HOST" "$ip" "?q=%3Cscript%3Ealert(1)%3C/script%3E" 403 ||
      die "The WAF didn't block a test attack on $ip."
    wait_for 120 "$WAF_HOST answers on $ip" answers "$WAF_HOST" "$ip" "" || die "$WAF_HOST stopped answering once its WAF was on."
    wait_for 120 "$WEB_HOST answers on $ip" answers "$WEB_HOST" "$ip" "" || die "$WEB_HOST doesn't answer on $ip."
  done
  note "The WAF blocks a test attack with 403, and lets a plain request through."
  if [ "$MODE" = paths ]; then
    wait_for 600 "the TCP port $TCP_PORT answers on $IP1" tcp_answers || { diagnose; die "The public TCP port didn't answer."; }
    note "The tcp service answers on TCP port $TCP_PORT."
  fi
  [ -n "$TUNNEL" ] || note "Tunnel domain: skipped: no test zone"
}
failed_deploy() {
  state | jq -r --arg n "$RUN" '(.projects[] | select(.name == $n) | .id) as $p
    | [.deployments[]? | select(.projectId == $p and .status == "failed") | (.failure.message // .failure.reason // "Pyo gave no reason")] | first // empty'
}
# What Pyo and the cluster say about the test project, when it doesn't answer.
diagnose() {
  note "https answers $(curl -ksS -o /dev/null -w '%{http_code}' -m 5 --resolve "$WEB_HOST:443:$IP1" "https://$WEB_HOST/" 2>&1 || true), http answers $(curl -sS -o /dev/null -w '%{http_code}' -m 5 --resolve "$WEB_HOST:80:$IP1" "http://$WEB_HOST/" 2>&1 || true)"
  state | jq -r --arg n "$RUN" '
    (.projects[] | select(.name == $n) | .id) as $p
    | (.services[] | select(.projectId == $p) | "service \(.name): \(.status // "?") \(.statusMessage // "") domains \([.domains[]? | "\(.hostname) \(.status) tls \(.tls) \(.tlsMode) \(.error // "")"] | join(", "))"),
      ([.services[] | select(.projectId == $p) | .id]) as $ids
    | (.deployments[]? | select(.serviceId as $s | $ids | index($s)) | "deployment of \(.serviceId): \(.status) \(.failure.message // "")"),
      (.clusters[0] | "cluster: \(.status) traffic \(.traffic) gateway error \(.capabilities.gatewayError // "none") ports taken \(.capabilities.gatewayPortsTaken)")' 2>&1 | indent || true
  kc get pods -A --no-headers 2>&1 | grep -v -e ' Running ' -e ' Completed ' | indent || true
  kc get gateway,httproute,certificate -A --no-headers 2>&1 | indent || true
  kc -n pyo-gateway get pods,svc --no-headers 2>&1 | indent || true
  kc -n kube-system get pods --no-headers 2>&1 | grep svclb | indent || true
  kc -n pyo-system logs deployment/pyo-agent --tail=25 2>&1 | cut -c1-300 | indent || true
}
service_id() {
  state | jq -r --arg n "$RUN" --arg s "$1" '(.projects[] | select(.name == $n) | .id) as $p | .services[] | select(.projectId == $p and .name == $s) | .id'
}

# ── The tunnel leg (with PYO_E2E_CF_TOKEN) ──────────────────────────────────

# Text from Cloudflare or Pyo, with the token taken out.
masked() { printf '%s' "${1//"$CF_TOKEN"/(hidden)}"; }
# cf METHOD PATH: Cloudflare's API. The token goes in a header read from a
# pipe, so it's in no command line.
cf() {
  curl -sS -m 30 -X "$1" -H @<(printf 'Authorization: Bearer %s\n' "$CF_TOKEN") "https://api.cloudflare.com/client/v4$2"
}

# The token's one zone, and what the zone and its account hold before the
# test: the cleanup compares with that, and never deletes anything in it.
cf_start() {
  CF_TOKEN=$PYO_E2E_CF_TOKEN
  unset PYO_E2E_CF_TOKEN
  [[ $CF_TOKEN =~ ^[A-Za-z0-9_-]+$ ]] || die "PYO_E2E_CF_TOKEN doesn't look like a Cloudflare API token."
  local zones n
  zones=$(cf GET '/zones?per_page=50') || die "Cloudflare's API didn't answer."
  jq -e .success <<<"$zones" >/dev/null || die "Cloudflare refused the token: $(masked "$(jq -c .errors <<<"$zones")")"
  n=$(jq '.result | length' <<<"$zones")
  [ "$n" = 1 ] || die "The token sees $n zones. Give the test a token for one throwaway zone."
  CF_ZONE=$(jq -r '.result[0].name' <<<"$zones")
  CF_ZONE_ID=$(jq -r '.result[0].id' <<<"$zones")
  CF_ACCOUNT=$(jq -r '.result[0].account.id' <<<"$zones")
  cf_list >"$DIR/cf-before.json" || die "Couldn't list what the zone has."
  TUN_HOST="zd-tun-$(rand).$CF_ZONE" TUNWAF_HOST="zd-tunwaf-$(rand).$CF_ZONE"
  [ "$MODE" != paths ] || MV_HOST="zd-mv-$(rand).$CF_ZONE"
  say "Tunnel leg on the token's zone: $(jq -r '"\(.records | length) records, \(.tunnels | length) tunnels, \(.certificates | length) certificates"' "$DIR/cf-before.json") before the test"
}
# The zone's DNS records, the account's tunnels and the zone's Origin CA
# certificates, as one JSON object.
cf_list() {
  local r t c
  r=$(cf GET "/zones/$CF_ZONE_ID/dns_records?per_page=500") && jq -e .success <<<"$r" >/dev/null || return 1
  t=$(cf GET "/accounts/$CF_ACCOUNT/cfd_tunnel?is_deleted=false&per_page=1000") && jq -e .success <<<"$t" >/dev/null || return 1
  c=$(cf GET "/certificates?zone_id=$CF_ZONE_ID") && jq -e .success <<<"$c" >/dev/null || return 1
  jq -n --argjson r "$r" --argjson t "$t" --argjson c "$c" '{
    records: [$r.result[] | {id, type, name, content, comment}],
    tunnels: [$t.result[] | {id, name}],
    certificates: [$c.result[] | {id, hostnames}]}'
}
# What the test made that's still there: not there before, and named for
# this run (its hostnames, its cluster's tunnel, Pyo's checks with this
# installation's id).
cf_leftovers() {
  cf_list | jq --slurpfile b "$DIR/cf-before.json" --arg zone "$CF_ZONE" --arg cluster "$CLUSTER" \
    --arg install "$INSTALL_ID" --arg tun "$TUN_HOST" --arg tunwaf "$TUNWAF_HOST" --arg mv "$MV_HOST" '
    ($b[0].records | map(.id)) as $r0 | ($b[0].tunnels | map(.id)) as $t0 | ($b[0].certificates | map(.id)) as $c0
    | {records: [.records[] | select((.id as $i | $r0 | index($i) | not)
          and (.name == $tun or .name == $tunwaf or ($mv != "" and .name == $mv)
            or ($install != "" and .comment == "Made by Pyo (\($install))" and .name == "_pyo-check.\($zone)")))],
       tunnels: [.tunnels[] | select((.id as $i | $t0 | index($i) | not)
          and ((.name | startswith("pyo-\($cluster)-")) or ($install != "" and (.name | startswith("pyo-check-\($install)-")))))],
       certificates: [.certificates[] | select((.id as $i | $c0 | index($i) | not)
          and (.hostnames | length > 0) and all(.hostnames[]; . == $tun or . == $tunwaf or . == $mv))]}'
}

tunnel_api() { api GET "/api/v1/clusters/$CLUSTER/tunnel"; }
tunnel_up() {
  tunnel_api | jq -e '.tunnel.state == "ready" and .tunnel.tunnelId != null' >/dev/null &&
    [ "$(kc -n pyo-system get deployment pyo-tunnel -o jsonpath='{.status.readyReplicas}')" = 2 ]
}
tunnel_gone() { tunnel_api | jq -e '.tunnel == null' >/dev/null; }

# Cloudflare's edge address for a name, from Cloudflare's resolver: this
# machine's own resolver may still have the name as missing.
edge_ip() {
  curl -fsS -m 10 -H 'accept: application/dns-json' "https://cloudflare-dns.com/dns-query?name=$1&type=A" |
    jq -r '[.Answer[]? | select(.type == 1) | .data][0] // empty'
}
edge_known() { [ -n "$(edge_ip "$1")" ]; }
# answers_edge HOST PATH [STATUS]: through Cloudflare, its certificate checked.
# The last answer is kept in $DIR/edge-last for tunnel_diagnose.
answers_edge() {
  local code
  code=$(curl -s -D "$DIR/edge-last" -o "$DIR/edge-body" -w '%{http_code}' -m 10 --resolve "$1:443:$EDGE" "https://$1/$2" 2>"$DIR/edge-err" || true)
  [ "$code" = "${3:-200}" ]
}
# What Cloudflare, Pyo and the cluster say, when a tunnel domain doesn't answer.
tunnel_diagnose() {
  {
    echo "Cloudflare answered: $(head -n1 "$DIR/edge-last" 2>/dev/null) $(grep -i -e '^cf-ray' -e '^cf-mitigated' -e '^server' "$DIR/edge-last" 2>/dev/null | tr -d '\r' | tr '\n' ' ')"
    echo "body: $(head -c 300 "$DIR/edge-body" 2>/dev/null | tr -s '\n\t ' ' ')"
    cat "$DIR/edge-err" 2>/dev/null
    echo "tunnel: $(tunnel_api | jq -c .)"
    echo "Cloudflare's routes: $(cf GET "/accounts/$CF_ACCOUNT/cfd_tunnel/$TUN_ID/configurations" | jq -c '.result.config.ingress // .errors')"
    echo "Cloudflare's connections: $(cf GET "/accounts/$CF_ACCOUNT/cfd_tunnel/$TUN_ID/connections" | jq -c '[.result[]? | {colo_name: .conns[0].colo_name, n: (.conns | length)}]')"
    echo "records: $(cf GET "/zones/$CF_ZONE_ID/dns_records?per_page=50" | jq -c '[.result[] | {type, name, content, proxied}]')"
    echo "web domain: $(domain_of "$WEB_ID" "$TUN_HOST" | jq -c .)"
    kc -n pyo-system get pods -l app.kubernetes.io/name=pyo-tunnel -o wide --no-headers
    kc -n pyo-system logs deployment/pyo-tunnel --all-containers --tail=30 | cut -c1-300
    kc -n pyo-gateway get svc,clienttrafficpolicy --no-headers
    kc get httproute -A --no-headers | grep -e tunnel -e switching
    kc -n pyo-system logs deployment/pyo-agent --tail=40 | grep -i -e tunnel -e error -e warn | cut -c1-300
  } 2>&1 | while IFS= read -r line; do masked "$line"; echo; done | indent
}
domain_of() { # SERVICE HOST: the domain's JSON
  state | jq --arg s "$1" --arg h "$2" '.services[] | select(.id == $s) | .domains[] | select(.hostname == $h)'
}
switched() { domain_of "$1" "$2" | jq -e '.tunnel == "on" and .status == "active"' >/dev/null; }

# tunnel_domain SERVICE HOST: a new domain of the service, through the
# tunnel. Pyo makes its record. Someone else's record is never replaced.
tunnel_domain() {
  local sid=$1 host=$2 svc out did
  svc=$(state | jq --arg id "$sid" --arg h "$host" --arg d "dom_e2e$(rand)" --argjson now "$(date +%s)000" '
    .services[] | select(.id == $id)
    | .domains += [{id: $d, hostname: $h, portName: .domains[0].portName, status: "pending_dns", tls: "pending", tlsMode: "auto", redirectHttps: false, createdAt: $now}]')
  out=$(op "$(jq -n --argjson s "$svc" '{op: "saveService", service: $s}')") || die "Pyo refused $host: $out"
  did=$(domain_of "$sid" "$host" | jq -r .id)
  [ -n "$did" ] || die "Pyo didn't keep $host."
  TUN_DOMAINS+=("$sid $did")
  out=$(op "$(jq -n --arg s "$sid" --arg d "$did" '{op: "setDomainTunnel", serviceId: $s, domainId: $d, on: true}')") ||
    die "Pyo couldn't put $host on the tunnel: $(masked "$out")"
  case "$(jq -r .status <<<"$out")" in
    switching | active) ;;
    *) die "Pyo didn't switch $host: $(masked "$out")" ;;
  esac
}

tunnel_setup() {
  say "Connecting Cloudflare with the test token"
  local out ready
  CF_USED=1
  # On stdin, so the token is in no command line.
  out=$(printf '{"op":"connectCloudflare","teamId":"%s","token":"%s"}' "$TEAM" "$CF_TOKEN" |
    curl -sS --fail-with-body -m 120 -H "@$DIR/auth" -H 'Content-Type: application/json' --data-binary @- "$URL/api/v1/ops") ||
    die "Pyo couldn't connect Cloudflare: $(masked "$out")"
  ready=$(jq -r --arg z "$CF_ZONE" '.zones[]? | select(.name == $z) | .tunnel.ready' <<<"$out")
  [ "$ready" = true ] || die "The zone can't use Cloudflare Tunnel: $(masked "$(jq -c '.problems // .zones' <<<"$out")")"

  say "Turning on Cloudflare Tunnel for the cluster"
  if [ "$MODE" = paths ]; then
    # The switch: the tunnel, no domain moved, public ports as they are.
    out=$(paths_op '{tunnel: true}') || die "Pyo refused Cloudflare Tunnel: $(masked "$out")"
  else
    out=$(op "$(jq -n --arg c "$CLUSTER" '{op: "setClusterTraffic", clusterId: $c, mode: "tunnel"}')") ||
      die "Pyo refused tunnel mode: $(masked "$out")"
  fi
  wait_for 600 "Cloudflare has the tunnel and both connectors run" tunnel_up || {
    note "$(masked "$(tunnel_api | jq -c .tunnel)")"
    die "The tunnel didn't come up."
  }
  out=$(tunnel_api)
  TUN_ID=$(jq -r .tunnel.tunnelId <<<"$out")
  TUN_NAME=$(jq -r .tunnel.tunnelName <<<"$out")
  INSTALL_ID=${TUN_NAME#"pyo-$CLUSTER-"}
  [ "$INSTALL_ID" != "$TUN_NAME" ] || INSTALL_ID=""
  note "Tunnel $TUN_ID, 2 connectors ready"
  # The tunnel stays with public ports, so web and waf keep their direct
  # path. Pyo serves a domain through it once it's switched.
  if [ "$MODE" != paths ]; then
    say "Back to public ports for the cluster, keeping the tunnel"
    out=$(op "$(jq -n --arg c "$CLUSTER" '{op: "setClusterTraffic", clusterId: $c, mode: "public"}')") ||
      die "Pyo refused public ports: $(masked "$out")"
  fi
  wait_for 300 "$WEB_HOST answers again" answers "$WEB_HOST" "$IP1" "" || die "$WEB_HOST didn't answer on public ports again."
  wait_for 300 "$WAF_HOST answers again" answers "$WAF_HOST" "$IP1" "" || die "$WAF_HOST didn't answer on public ports again."

  say "Adding https://$TUN_HOST (web) and https://$TUNWAF_HOST (waf) through the tunnel"
  tunnel_domain "$WEB_ID" "$TUN_HOST"
  tunnel_domain "$WAF_ID" "$TUNWAF_HOST"
  wait_for 600 "$TUN_HOST is switched" switched "$WEB_ID" "$TUN_HOST" || die "$TUN_HOST didn't switch: $(masked "$(domain_of "$WEB_ID" "$TUN_HOST" | jq -c .)")"
  wait_for 600 "$TUNWAF_HOST is switched" switched "$WAF_ID" "$TUNWAF_HOST" || die "$TUNWAF_HOST didn't switch: $(masked "$(domain_of "$WAF_ID" "$TUNWAF_HOST" | jq -c .)")"
  EDGE=""
  wait_for 300 "Cloudflare's resolver has $TUN_HOST" edge_known "$TUN_HOST" || die "Cloudflare's resolver doesn't have $TUN_HOST."
  EDGE=$(edge_ip "$TUN_HOST")
  [ "$(edge_ip "$TUNWAF_HOST")" = "$EDGE" ] || note "$TUNWAF_HOST is on another edge address. Both use $EDGE."
  wait_for 600 "$TUN_HOST answers through Cloudflare" answers_edge "$TUN_HOST" "" || { tunnel_diagnose; die "$TUN_HOST doesn't answer through Cloudflare."; }
  wait_for 600 "the WAF blocks a test attack on $TUNWAF_HOST" answers_edge "$TUNWAF_HOST" "?q=%3Cscript%3Ealert(1)%3C/script%3E" 403 ||
    die "The WAF didn't block a test attack through the tunnel."
  wait_for 120 "$TUNWAF_HOST answers through Cloudflare" answers_edge "$TUNWAF_HOST" "" || die "$TUNWAF_HOST doesn't answer through Cloudflare."
  note "Both answer through Cloudflare's edge ($EDGE) and the tunnel. The WAF blocks a test attack there too."
  if [ -n "$MV_HOST" ]; then
    say "Adding https://$MV_HOST (web), direct on public ports"
    plain_domain "$WEB_ID" "$MV_HOST"
    wait_for 300 "$MV_HOST answers on $IP1" answers "$MV_HOST" "$IP1" "" || die "$MV_HOST doesn't answer on $IP1."
  fi
}
# plain_domain SERVICE HOST: a new domain of the service, no DNS record.
plain_domain() {
  local svc out did
  svc=$(state | jq --arg id "$1" --arg h "$2" --arg d "dom_e2e$(rand)" --argjson now "$(date +%s)000" '
    .services[] | select(.id == $id)
    | .domains += [{id: $d, hostname: $h, portName: .domains[0].portName, status: "pending_dns", tls: "pending", tlsMode: "selfsigned", redirectHttps: false, createdAt: $now}]')
  out=$(op "$(jq -n --argjson s "$svc" '{op: "saveService", service: $s}')") || die "Pyo refused $2: $out"
  did=$(domain_of "$1" "$2" | jq -r .id)
  [ -n "$did" ] || die "Pyo didn't keep $2."
  TUN_DOMAINS+=("$1 $did")
}

# Rotate: a new token for the connector. Pyo cuts what's left of the old
# token's connectors once the new ones are connected, so the step ends
# when Pyo says the rotation is over (`rotating`), not at the rollout.
connector_token() { kc -n pyo-system get deployment pyo-tunnel -o jsonpath='{.spec.template.metadata.annotations.pyo\.dev/token-hash}'; }
token_moved() { [ "$(connector_token)" != "$1" ]; }
rotation_over() { tunnel_api | jq -e '.tunnel.rotating != true' >/dev/null; }
rotate_token() {
  local before out
  before=$(connector_token)
  out=$(op "$(jq -n --arg c "$CLUSTER" '{op: "rotateTunnelToken", clusterId: $c}')") || { note "Pyo refused: $(masked "$out")"; return 1; }
  wait_for 180 "the connector has the new token" token_moved "$before" || return 1
  kc -n pyo-system rollout status deployment/pyo-tunnel --timeout=300s >/dev/null || return 1
  wait_for 240 "the old connectors are gone" no_stopping pyo-system || return 1
  wait_for 420 "Pyo says the rotation is over" rotation_over
}

# Pyo's own remove actions first, then Cloudflare's API for what's left
# of this run. Called by the cleanup, whatever happened before.
cf_cleanup() {
  local pair out left
  say "Cleaning up at Cloudflare"
  if [ -n "$KEY" ] && curl -fsS -m 5 -o /dev/null "$URL/api/v1/meta" 2>/dev/null; then
    for pair in "${TUN_DOMAINS[@]}"; do
      set -- $pair
      # removeDomain came after 0.51.0. Before it, the service is saved without the domain.
      out=$(op "$(jq -n --arg s "$1" --arg d "$2" '{op: "removeDomain", serviceId: $s, domainId: $d}')" 2>&1) ||
        out=$(op "$(state | jq --arg s "$1" --arg d "$2" '{op: "saveService", service: (.services[] | select(.id == $s) | .domains |= map(select(.id != $d)))}')" 2>&1) ||
        note "Pyo couldn't remove a tunnel domain: $(masked "$out")"
    done
    op "$(jq -n --arg c "$CLUSTER" '{op: "setClusterTraffic", clusterId: $c, mode: "public"}')" >/dev/null 2>&1 || true
    if [ "$MODE" = paths ] && [ -n "$CLUSTER" ] && ! tunnel_gone 2>/dev/null; then
      out=$(paths_op '{tunnel: false, restoreRecords: false, confirm: true}' 2>&1) || note "Pyo refused to turn the tunnel off: $(masked "$out")"
      wait_for 900 "Pyo removed the tunnel" tunnel_gone || note "Pyo didn't remove the tunnel."
    fi
    if [ -n "$CLUSTER" ] && ! tunnel_gone 2>/dev/null; then
      out=$(op "$(jq -n --arg c "$CLUSTER" '{op: "removeClusterTunnel", clusterId: $c, restoreRecords: false}')" 2>&1) ||
        note "Pyo refused to remove the tunnel: $(masked "$out")"
      wait_for 660 "Pyo removed the tunnel" tunnel_gone || note "Pyo didn't remove the tunnel. Deleting what's left through Cloudflare's API."
    fi
    op "$(jq -n --arg t "$TEAM" '{op: "removeCloudflare", teamId: $t}')" >/dev/null 2>&1 || note "Pyo kept the Cloudflare token. The VM goes with it."
  else
    note "Pyo doesn't answer. Deleting what's left through Cloudflare's API."
  fi
  left=$(cf_leftovers) || { note "Couldn't read the zone. Check it by hand for $TUN_HOST, $TUNWAF_HOST and tunnels named pyo-$CLUSTER-…"; return; }
  local id
  for id in $(jq -r '.records[].id' <<<"$left"); do
    cf DELETE "/zones/$CF_ZONE_ID/dns_records/$id" >/dev/null && note "Deleted a record Pyo left: $(jq -r --arg i "$id" '.records[] | select(.id == $i) | "\(.type) \(.name)"' <<<"$left")"
  done
  for id in $(jq -r '.tunnels[].id' <<<"$left"); do
    cf DELETE "/accounts/$CF_ACCOUNT/cfd_tunnel/$id/connections" >/dev/null || true
    cf DELETE "/accounts/$CF_ACCOUNT/cfd_tunnel/$id" >/dev/null && note "Deleted a tunnel Pyo left: $(jq -r --arg i "$id" '.tunnels[] | select(.id == $i) | .name' <<<"$left")"
  done
  for id in $(jq -r '.certificates[].id' <<<"$left"); do
    cf DELETE "/certificates/$id" >/dev/null && note "Revoked an Origin CA certificate Pyo left"
  done
  # What the zone has now, against before the test.
  cf_list >"$DIR/cf-after.json" || { note "Couldn't read the zone afterwards."; return; }
  jq -r --slurpfile b "$DIR/cf-before.json" '
    def ids(k): [.[k][].id] | sort;
    if ids("records") == ($b[0] | ids("records")) and ids("tunnels") == ($b[0] | ids("tunnels")) and ids("certificates") == ($b[0] | ids("certificates"))
    then "          Cloudflare: \(.records | length) records, \(.tunnels | length) tunnels, \(.certificates | length) certificates, as before the test."
    else "          Cloudflare differs from before the test:",
      (.records[] | select(.id as $i | $b[0].records | map(.id) | index($i) | not) | "            record \(.type) \(.name)"),
      (.tunnels[] | select(.id as $i | $b[0].tunnels | map(.id) | index($i) | not) | "            tunnel \(.name)"),
      (.certificates[] | select(.id as $i | $b[0].certificates | map(.id) | index($i) | not) | "            certificate \(.hostnames | join(" "))")
    end' "$DIR/cf-after.json"
}

# ── Watching and traffic ────────────────────────────────────────────────────

start_watch() {
  vm "$VM1" mkdir -p /var/tmp/e2e
  send "$PY" /var/tmp/e2e/e2e.py
  vm "$VM1" systemd-run --quiet --unit e2e-watch --collect python3 /var/tmp/e2e/e2e.py watch /var/tmp/e2e/pods.log
  # The VM's clock against this machine's, for matching the two logs.
  local a b v
  a=$(date +%s.%N)
  v=$(multipass exec "$VM1" -- date +%s.%N)
  b=$(date +%s.%N)
  # LC_ALL=C: a dot in the number, whatever the locale.
  echo "offset $(LC_ALL=C awk -v a="$a" -v b="$b" -v v="$v" 'BEGIN { printf "%.3f", v - (a + b) / 2 }') clock" >>"$DIR/actions.log"
}

start_load() {
  TARGETS=("web=$WEB_HOST@$IP1" "waf=$WAF_HOST@$IP1")
  [ -z "${IP2:-}" ] || TARGETS+=("web-2=$WEB_HOST@$IP2" "waf-2=$WAF_HOST@$IP2")
  [ -z "$TUNNEL" ] || TARGETS+=("tun=$TUN_HOST@$EDGE" "tunwaf=$TUNWAF_HOST@$EDGE")
  printf '%s\n' "${TARGETS[@]}" >"$DIR/targets"
  say "Sending traffic: $((RATE * 2)) requests a second, a long request and a WebSocket to each of ${#TARGETS[@]} targets"
  (
    # Nothing this script inherited (a lock its caller holds) stays open
    # in the traffic generator.
    for fd in 3 4 5 6 7 8 9; do eval "exec $fd>&-"; done
    exec python3 "$PY" load "$DIR/load.log" "$DIR/stop" "$RATE" "${TARGETS[@]}"
  ) &
  LOAD_PID=$!
  local start
  start=$(date +%s.%N)
  sleep "$GAP"
  kill -0 "$LOAD_PID" 2>/dev/null || die "The traffic generator stopped."
  note "Before any action: $(tally "$start" "$(date +%s.%N)")"
}

tally() { # START END
  LC_ALL=C awk -v a="$1" -v b="$2" '
    # A long request counts when it ended.
    $3 == "long" { if ($4 != "200" && $1 + $5 / 1000 >= a && $1 + $5 / 1000 <= b) l++; next }
    $1 >= a && $1 <= b {
      if ($3 == "fresh" || $3 == "keepalive") { n++; if ($4 != "200") f++ }
      else if ($3 == "ws" && $4 == "drop") w++
    }
    END { printf "%d requests, %d failed, %d long requests cut, %d WebSocket drops", n, f, l, w }' "$DIR/load.log"
}

# action NAME COMMAND...: runs it under traffic, then a quiet gap.
action() {
  local name=$1 start
  shift
  start=$(date +%s.%N)
  echo "start $start $name" >>"$DIR/actions.log"
  say "$name"
  "$@" || note "This step didn't finish. The numbers below are for what ran."
  echo "end $(date +%s.%N) $name" >>"$DIR/actions.log"
  # Requests in flight get logged when they end.
  sleep 2
  note "$(tally "$start" "$(date +%s.%N)")"
  sleep "$GAP"
}
skipped() { note "$1: skipped: $2"; }

no_stopping() { [ -z "$(kc -n "$1" get pods -o jsonpath='{.items[*].metadata.deletionTimestamp}')" ]; }
# restart KIND NAMESPACE NAME...: a rollout restart, until the old pods are gone.
restart() {
  local kind=$1 ns=$2 name
  shift 2
  for name in "$@"; do
    kc -n "$ns" rollout restart "$kind/$name" >/dev/null || return 1
    kc -n "$ns" rollout status "$kind/$name" --timeout=300s >/dev/null || return 1
  done
  wait_for 240 "the old pods in $ns are gone" no_stopping "$ns"
}
restart_k3s() { # VM UNIT
  vm "$1" systemctl restart "$2" || return 1
  wait_for 240 "k3s answers again" kc get nodes || return 1
  sleep 10
}

firewall_change() {
  local id out
  id="fwr_e2e$(rand)"
  out=$(op "$(jq -n --arg id "$id" --arg s "$WEB_ID" '{op: "addFirewallRule", id: $id, serviceId: $s, action: "block", sources: ["203.0.113.7"], note: "update-traffic-e2e"}')") ||
    { note "Pyo refused the rule: $out"; return 1; }
  sleep 15
  out=$(op "$(jq -n --arg id "$id" '{op: "removeFirewallRule", id: $id}')") || { note "Pyo couldn't remove the rule: $out"; return 1; }
  sleep 15
}

web_service() { state | jq --arg id "$WEB_ID" '.services[] | select(.id == $id)'; }
domain_change() {
  local svc out
  svc=$(web_service | jq --arg h "$EXTRA_HOST" --arg id "dom_e2e$(rand)" --argjson now "$(date +%s)000" '
    .domains += [{id: $id, hostname: $h, portName: .domains[0].portName, status: "pending_dns", tls: "pending", tlsMode: "auto", redirectHttps: false, createdAt: $now}]')
  out=$(op "$(jq -n --argjson s "$svc" '{op: "saveService", service: $s}')") || { note "Pyo refused the domain: $out"; return 1; }
  wait_for 180 "https://$EXTRA_HOST answers" answers "$EXTRA_HOST" "$IP1" "" || return 1
  svc=$(web_service | jq --arg h "$EXTRA_HOST" '.domains |= map(select(.hostname != $h))')
  out=$(op "$(jq -n --argjson s "$svc" '{op: "saveService", service: $s}')") || { note "Pyo couldn't remove the domain: $out"; return 1; }
  sleep 15
}

fast() {
  local ns names
  mapfile -t names < <(kc -n pyo-gateway get daemonset -o jsonpath='{range .items[*]}{.metadata.name}{"\n"}{end}')
  action "restart Envoy" restart daemonset pyo-gateway "${names[@]}"
  action "restart the WAF" restart daemonset pyo-waf pyo-waf
  ns=$(kc get deployment -A --field-selector metadata.name=pyo-tunnel -o jsonpath='{.items[0].metadata.namespace}' 2>/dev/null || true)
  if [ -n "$ns" ]; then
    action "restart the tunnel connector" restart deployment "$ns" pyo-tunnel
    action "rotate the tunnel token" rotate_token
  else
    skipped "restart the tunnel connector" "no test zone"
    skipped "rotate the tunnel token" "no test zone"
  fi
  action "restart Envoy Gateway's controller" restart deployment pyo-gateway envoy-gateway
  action "restart the agent" restart deployment pyo-system pyo-agent
  if kc -n pyo-system get daemonset pyo-tailnet >/dev/null 2>&1; then
    action "restart pyo-tailnet" restart daemonset pyo-system pyo-tailnet
  else
    skipped "restart pyo-tailnet" "no Tailscale on this install"
  fi
  action "change a firewall rule of web" firewall_change
  action "add and remove a domain of web" domain_change
  action "restart k3s on $VM1" restart_k3s "$VM1" k3s
  [ -z "${IP2:-}" ] || action "restart k3s on $VM2" restart_k3s "$VM2" k3s-agent
}

# ── paths: the traffic paths, one switch at a time ──────────────────────────

# paths_op JQ: setClusterPaths for the cluster with the JQ object's fields.
paths_op() {
  op "$(jq -n --arg c "$CLUSTER" "{op: \"setClusterPaths\", clusterId: \$c} + ($1)")"
}
cluster_is() { state | jq -e --arg c "$CLUSTER" ".clusters[] | select(.id == \$c) | $1" >/dev/null; }
envoy_service_type() { kc -n pyo-gateway get svc -l gateway.envoyproxy.io/owning-gatewayclass=pyo -o jsonpath='{.items[0].spec.type}'; }
is_type() { [ "$(envoy_service_type)" = "$1" ]; }
proxy_protocol_is() { [ "$(kc -n pyo-gateway get clienttrafficpolicy pyo-https -o jsonpath='{.spec.proxyProtocol}' | grep -c optional)" = "$1" ]; }

# set_path JQ WHAT COMMAND...: the switch, confirmed, then until COMMAND says it's done.
set_path() {
  local change=$1 what=$2 out
  shift 2
  out=$(paths_op "$change + {confirm: true}") || { note "Pyo refused: $(masked "$out")"; return 1; }
  note "Pyo: $(jq -c '{status, domains: [.domains[]? | "\(.hostname) \(.outcome)"]}' <<<"$out")"
  wait_for 900 "$what" "$@"
}

# During each step, 4 times a second: the moving domain on both of its
# paths and the TCP port. "<time> <direct> <edge> <tcp>" (HTTP statuses).
probe_start() {
  rm -f "$DIR/probe-stop"
  : >"$DIR/probe.log"
  (
    for fd in 3 4 5 6 7 8 9; do eval "exec $fd>&-"; done
    while [ ! -e "$DIR/probe-stop" ]; do
      d=$(curl -ks -o /dev/null -w '%{http_code}' -m 2 --resolve "$MV_HOST:443:$IP1" "https://$MV_HOST/" || true)
      e=$(curl -s -o /dev/null -w '%{http_code}' -m 2 --resolve "$MV_HOST:443:$EDGE" "https://$MV_HOST/" || true)
      t=$(curl -s -o /dev/null -w '%{http_code}' -m 2 "http://$IP1:$TCP_PORT/" || true)
      echo "$(date +%s.%N) $d $e $t" >>"$DIR/probe.log"
      sleep 0.25
    done
  ) &
  PROBE_PID=$!
}
probe_stop() {
  touch "$DIR/probe-stop"
  wait "$PROBE_PID" 2>/dev/null || true
  PROBE_PID=""
  LC_ALL=C awk '{ n++; d = ($2 == "200"); e = ($3 == "200"); if (d && e) both++; else if (d) direct++; else if (e) edge++; else down++
                  if ($4 == "200") t++; else { tf++; ts[$4]++ } }
    END { printf "mv: %d probes, %d on both paths, %d direct only, %d through the edge only, %d both down. TCP port: %d answered, %d failed", n, both, direct, edge, down, t, tf
          for (k in ts) printf " (%s: %d)", k, ts[k]; printf "\n" }' "$DIR/probe.log"
  cat "$DIR/probe.log" >>"$DIR/probes.log"
}

mv_on_tunnel() { domain_of "$WEB_ID" "$MV_HOST" | jq -e '.tunnel == "on" and .status == "active"' >/dev/null && answers_edge "$MV_HOST" ""; }
mv_direct() { domain_of "$WEB_ID" "$MV_HOST" | jq -e '.tunnel == null' >/dev/null; }
move_mv() { # on|off
  local id out rc=0
  id=$(domain_of "$WEB_ID" "$MV_HOST" | jq -r .id)
  out=$(op "$(jq -n --arg s "$WEB_ID" --arg d "$id" --argjson on "$([ "$1" = on ] && echo true || echo false)" '{op: "setDomainTunnel", serviceId: $s, domainId: $d, on: $on, restore: false}')") ||
    { note "Pyo refused: $(masked "$out")"; return 1; }
  if [ "$1" = on ]; then
    wait_for 600 "$MV_HOST is on the tunnel" mv_on_tunnel || rc=1
  else
    wait_for 600 "$MV_HOST is direct again" mv_direct || rc=1
  fi
  # A minute more on both paths.
  sleep 60
  return $rc
}
public_off() {
  local rc=0
  set_path '{public: false, moveToTunnel: true, publicPorts: true}' "public ports are closed after the move" \
    cluster_is '.traffic == "tunnel" and (.closingPublic | not)' || rc=1
  wait_for 300 "Envoy's Service is ClusterIP" is_type ClusterIP || rc=1
  sleep 60
  return $rc
}
ports_service() { kc -n pyo-gateway get svc pyo-public-ports >/dev/null 2>&1; }
no_ports_service() { ! ports_service; }
public_on() {
  local rc=0
  set_path '{public: true}' "web answers on public ports again" answers "$WEB_HOST" "$IP1" "" || rc=1
  wait_for 300 "the TCP port is back on Envoy's own Service" no_ports_service || rc=1
  wait_for 120 "the TCP port answers" tcp_answers || rc=1
  return $rc
}

# Each target's requests and failures between START and END.
tally_targets() {
  LC_ALL=C awk -v a="$1" -v b="$2" '
    $3 == "long" { if ($4 != "200" && $1 + $5 / 1000 >= a && $1 + $5 / 1000 <= b) l[$2]++; seen[$2] = 1; next }
    $1 >= a && $1 <= b {
      seen[$2] = 1
      if ($3 == "fresh" || $3 == "keepalive") { n[$2]++; if ($4 != "200") f[$2]++ }
      else if ($3 == "ws" && $4 == "drop") w[$2]++
    }
    END { for (t in seen) printf "%s: %d requests, %d failed, %d long cut, %d WebSocket drops\n", t, n[t], f[t], l[t], w[t] }' "$DIR/load.log" | sort
}
# path_action NAME COMMAND...: action, then its numbers per target.
path_action() {
  local start end probes
  probe_start
  start=$(date +%s.%N)
  action "$@"
  end=$(date +%s.%N)
  probes=$(probe_stop)
  echo "$1" >>"$DIR/paths.txt"
  { tally_targets "$start" "$end"; echo "$probes"; } | sed 's/^/  /' | tee -a "$DIR/paths.txt" | while IFS= read -r l; do note "$l"; done
}

paths() {
  : >"$DIR/paths.txt"
  : >"$DIR/probes.log"
  path_action "turn Tailnet off" set_path '{tailnet: false}' "the cluster has Tailnet off" cluster_is '.paths.tailnet == false'
  path_action "turn Tailnet on" set_path '{tailnet: true}' "the cluster has Tailnet on" cluster_is '.paths.tailnet == true'
  path_action "accept PROXY protocol" set_path '{proxyProtocol: true}' "the gateway reads PROXY protocol" proxy_protocol_is 1
  path_action "stop reading PROXY protocol" set_path '{proxyProtocol: false}' "the gateway no longer reads it" proxy_protocol_is 0
  path_action "move $MV_HOST to the tunnel" move_mv on
  path_action "move $MV_HOST back to public ports" move_mv off
  path_action "turn Public ports off ($MV_HOST moves first)" public_off
  path_action "turn TCP and UDP ports off" set_path '{publicPorts: false}' "pyo-public-ports is gone" no_ports_service
  path_action "turn TCP and UDP ports on" set_path '{publicPorts: true}' "the TCP port answers again" tcp_answers
  path_action "turn Public ports on" public_on
  path_action "turn Cloudflare Tunnel off" set_path '{tunnel: false, restoreRecords: false}' "Pyo removed the tunnel" tunnel_gone
  # The tunnel and its records are gone with it.
  TUN_DOMAINS=()
}

# ── gateway: the Gateway section, one setting at a time ─────────────────────

# gw_op JQ: setClusterGateway for the cluster with the JQ object's fields.
gw_op() {
  op "$(jq -n --arg c "$CLUSTER" "{op: \"setClusterGateway\", clusterId: \$c} + ($1)")"
}
# set_gateway JQ WHAT COMMAND...: the setting, then until COMMAND says it's in place.
set_gateway() {
  local change=$1 what=$2 out
  shift 2
  out=$(gw_op "$change") || { note "Pyo refused: $(masked "$out")"; return 1; }
  note "Pyo: $(jq -c '{drops, settings}' <<<"$out")"
  wait_for 600 "$what" "$@"
}
# web's route policies (not its anchor's) and routes, as the agent wrote them.
web_policies() { kc -n pyo-gateway get backendtrafficpolicy -l "pyo/service-id=$WEB_ID" -o json | jq '[.items[] | select(.metadata.name | endswith("-anchor") | not)]'; }
compressing() { web_policies | jq -e 'length > 0 and all(.[]; .spec.compressor != null or (.metadata.name | contains("-http-")))' >/dev/null; }
not_compressing() { web_policies | jq -e 'length > 0 and all(.[]; .spec.compressor == null)' >/dev/null; }
idle_is() { web_policies | jq -e --arg t "$1" 'length > 0 and all(.[]; (.spec.timeout.http.streamIdleTimeout // "none") == $t)' >/dev/null; }
request_is() {
  kc -n pyo-gateway get httproute -l "pyo/service-id=$WEB_ID" -o json |
    jq -e --arg t "$1" '[.items[].spec.rules[] | select(.backendRefs) | .timeouts.request] | length > 0 and all(. == $t)' >/dev/null
}
# encoding ACCEPT: the Content-Encoding web answers with.
encoding() {
  curl -ks -o /dev/null -D - -m 5 -H "Accept-Encoding: $1" --resolve "$WEB_HOST:443:$IP1" "https://$WEB_HOST/" |
    tr -d '\r' | awk -F': ' 'tolower($1) == "content-encoding" { print $2 }'
}
encodes() { [ "$(encoding "$1")" = "$2" ]; }
compression_answers() {
  local rc=0 e
  for e in zstd br gzip; do
    if wait_for 60 "web answers with $e" encodes "$e" "$e"; then note "Accept-Encoding: $e gets Content-Encoding: $e"; else rc=1; fi
  done
  return $rc
}
compression_on() { set_gateway '{compression: true}' "web's routes compress" compressing && compression_answers; }
compression_off() {
  set_gateway '{compression: false}' "web's routes don't compress" not_compressing || return 1
  wait_for 60 "web answers uncompressed" encodes "zstd, br, gzip" "" && note "Accept-Encoding: zstd, br, gzip gets no Content-Encoding"
}
# A request slower than the default is cut with 504 at it, unless a service sets its own.
request_default() { # SECONDS
  set_gateway "{requestTimeout: $1}" "web's routes take ${1}s" request_is "${1}s" || return 1
  [ "$1" = 120 ] || return 0
  local got
  got=$(curl -ks -o /dev/null -w '%{http_code} %{time_total}' -m 200 --resolve "$WEB_HOST:443:$IP1" "https://$WEB_HOST/?wait=125s" || true)
  note "A request taking 125 s: $got (504 at about 120 s expected)"
  [ "${got%% *}" = 504 ]
}
idle_default() { # SECONDS
  local want="${1}s"
  [ "$1" != 3600 ] || want=none
  set_gateway "{idleTimeout: $1}" "web's routes have idle timeout $want" idle_is "$want"
}
tls_min_is() { [ "$(kc -n pyo-gateway get clienttrafficpolicy pyo-https -o jsonpath='{.spec.tls.minVersion}')" = "$1" ]; }
tls_answers() { # VERSION: 200 over that TLS version only
  [ "$(curl -ks -o /dev/null -w '%{http_code}' -m 5 "--tlsv$1" --tls-max "$1" --resolve "$WEB_HOST:443:$IP1" "https://$WEB_HOST/" || true)" = 200 ]
}
tls_refused() { ! tls_answers "$1"; }
tls_min() { # 1.2|1.3
  local rc=0
  if [ "$1" = 1.3 ]; then
    set_gateway '{tlsMin: "1.3"}' "pyo-https takes TLS 1.3 only" tls_min_is 1.3 || return 1
    wait_for 60 "TLS 1.2 is refused" tls_refused 1.2 && note "TLS 1.2 refused" || rc=1
    tls_answers 1.3 && note "TLS 1.3 answers" || { note "TLS 1.3 doesn't answer"; rc=1; }
  else
    set_gateway '{tlsMin: "1.2"}' "pyo-https takes TLS 1.2 again" tls_min_is "" || return 1
    wait_for 60 "TLS 1.2 answers" tls_answers 1.2 && note "TLS 1.2 answers again" || rc=1
  fi
  return $rc
}
envoy_service() { kc -n pyo-gateway get svc -l gateway.envoyproxy.io/owning-gatewayclass=pyo -o json; }
udp443() { envoy_service | jq -e '[.items[0].spec.ports[] | select(.protocol == "UDP" and .port == 443)] | length == 1' >/dev/null; }
no_udp443() { envoy_service | jq -e '[.items[0].spec.ports[] | select(.protocol == "UDP")] | length == 0' >/dev/null; }
svclb_ready() {
  kc -n kube-system get pods -l svccontroller.k3s.cattle.io/svcnamespace=pyo-gateway -o json |
    jq -e '.items | length > 0 and all(.[]; .metadata.deletionTimestamp == null and ([.status.conditions[]? | select(.type == "Ready" and .status == "True")] | length == 1))' >/dev/null
}
h3_answers() { vm "$VM1" python3 /var/tmp/e2e/e2e.py h3 "$WEB_HOST" "$IP1" | grep -q '^h3 200$'; }
alt_svc() {
  curl -ks -o /dev/null -D - -m 5 --resolve "$WEB_HOST:443:$IP1" "https://$WEB_HOST/" | tr -d '\r' | awk -F': ' 'tolower($1) == "alt-svc" { print $2 }'
}
http3() { # on|off
  if [ "$1" = on ]; then
    set_gateway '{http3: true}' "Envoy's Service publishes UDP 443" udp443 || return 1
    wait_for 300 "ServiceLB's pods are ready" svclb_ready || return 1
    note "alt-svc: $(alt_svc)"
    if [ -z "$H3_CLIENT" ]; then
      note "HTTP/3 request: skipped: $H3_SKIP"
    elif wait_for 120 "an HTTP/3 request answers" h3_answers; then
      note "HTTP/3 request: $(vm "$VM1" python3 /var/tmp/e2e/e2e.py h3 "$WEB_HOST" "$IP1")"
    else
      note "HTTP/3 request: $(vm "$VM1" python3 /var/tmp/e2e/e2e.py h3 "$WEB_HOST" "$IP1" || true)"
      return 1
    fi
  else
    set_gateway '{http3: false}' "Envoy's Service no longer publishes UDP" no_udp443 || return 1
    wait_for 300 "ServiceLB's pods are ready" svclb_ready
  fi
}
# gw_action NAME COMMAND...: action, then its numbers per target.
gw_action() {
  local start end
  start=$(date +%s.%N)
  action "$@"
  end=$(date +%s.%N)
  echo "$1" >>"$DIR/gateway.txt"
  tally_targets "$start" "$end" | sed 's/^/  /' | tee -a "$DIR/gateway.txt" | while IFS= read -r l; do note "$l"; done
}
H3_CLIENT="" H3_SKIP=""
gateway_client() {
  # An HTTP/3 client on the VM: Ubuntu's own aioquic, from its archive.
  if vm "$VM1" sh -c 'apt-get update -q && DEBIAN_FRONTEND=noninteractive apt-get install -y -q python3-aioquic' >/dev/null 2>&1; then
    H3_CLIENT=1
  else
    H3_SKIP="python3-aioquic didn't install on $VM1"
  fi
}
gateway() {
  : >"$DIR/gateway.txt"
  gateway_client
  gw_action "turn compression on" compression_on
  gw_action "turn compression off" compression_off
  gw_action "set the request timeout default to 120 s" request_default 120
  gw_action "set the request timeout default back to 300 s" request_default 300
  gw_action "set the idle timeout default to 10 minutes" idle_default 600
  gw_action "set the idle timeout default back to 1 hour" idle_default 3600
  gw_action "set the TLS minimum to 1.3" tls_min 1.3
  gw_action "set the TLS minimum back to 1.2" tls_min 1.2
  gw_action "turn HTTP/3 on" http3 on
  gw_action "turn HTTP/3 off" http3 off
}

# ── update: the dev build as a release ──────────────────────────────────────

# release.json for the dev build, as the release workflow writes it, with
# its images in the VM's own registry. Before the VMs: it needs none.
prepare_update() {
  local host=${DEV_IMAGES%%/*} path=${DEV_IMAGES#*/} cargo
  if [ -z "$DEV_TAG" ]; then
    DEV_TAG=$(curl -fsS -m 10 "http://$host/v2/$path/pyo/tags/list" | jq -r '[.tags[]? | select(test("^[0-9]+$"))] | max // empty') ||
      die "No registry answers at $host. Build first: scripts/pyo-up.sh"
    [ -n "$DEV_TAG" ] || die "$DEV_IMAGES has no pyo image. Build first: scripts/pyo-up.sh"
  fi
  say "Reading the dev build $DEV_TAG from $DEV_IMAGES"
  python3 "$PY" images-pull "$DEV_IMAGES" "$DIR/images" "$DEV_TAG" pyo agent builder waf geoip
  note "$(jq -r 'to_entries | map(select(.value.copied) | "\(.key):\(.value.tag)") | join(" ")' "$DIR/images/images.json")"
  cargo=$(sed -n 's/^version = "\(.*\)"$/\1/p' Cargo.toml | head -n1)
  # The next patch version, so install.sh takes it as an update.
  TO="${cargo%.*}.$((${cargo##*.} + 1))"
  local agent
  agent="$DEV_IMAGES/agent:$(jq -r .agent.tag "$DIR/images/images.json")"
  if ! docker image inspect "$agent" >/dev/null 2>&1; then
    PULLED=$agent
    docker pull -q "$agent" >/dev/null || die "docker couldn't pull $agent."
  fi
  mkdir -p "$DIR/release/releases/tags" "$DIR/verifier"
  local arch
  for arch in x86_64 aarch64; do
    curl -fsSL --proto '=https' -m 120 -o "$DIR/verifier/pyo-verify-linux-$arch" "$RELEASES/v$FROM/pyo-verify-linux-$arch" ||
      die "Couldn't download the verifier of Pyo $FROM."
    [ "$(sha256sum "$DIR/verifier/pyo-verify-linux-$arch" | cut -d' ' -f1)" = "$(jq -r --arg a "linux-$arch" '.verifier[$a]' "$DIR/from.json")" ] ||
      die "The verifier of Pyo $FROM isn't the one its release.json lists."
  done
  digest() { jq -r --arg n "$1" '.[$n].digest' "$DIR/images/images.json"; }
  bun run scripts/release-manifest.ts --version "$TO" --allow-version-mismatch yes --images "$VM_IMAGES" \
    --pyo "$(digest pyo)" --agent "$(digest agent)" --builder "$(digest builder)" --flows "$(digest flows)" \
    --waf "$(digest waf)" --geoip "$(digest geoip)" --db-controller "$(digest db-controller)" --kv "$(digest kv)" \
    --static "$(digest static)" --mysql-operator "$(digest mysql-operator)" --mysql-backup "$(digest mysql-backup)" \
    --postgres-16 "$(digest postgres-16)" --postgres-17 "$(digest postgres-17)" --postgres-18 "$(digest postgres-18)" \
    --agent-access "$(docker run --rm --memory 512m "$agent" access-hash)" \
    --database-pins "$(docker run --rm --memory 512m "$agent" database-pins)" \
    --sandbox-x86_64 "$(jq -r .sandbox.x86_64 "$DIR/from.json")" --sandbox-aarch64 "$(jq -r .sandbox.aarch64 "$DIR/from.json")" \
    --sandbox-gvisor-x86_64 "$(jq -r .sandboxGvisor.x86_64 "$DIR/from.json")" --sandbox-gvisor-aarch64 "$(jq -r .sandboxGvisor.aarch64 "$DIR/from.json")" \
    --verifier "$DIR/verifier" --scripts "$DIR/scripts" >"$DIR/release/release.json" 2>"$DIR/manifest.log" ||
    { cat "$DIR/manifest.log" >&2; die "Couldn't write the dev build's release.json."; }
  # The release's own agent reads it, as the release workflow has it do.
  docker run --rm --memory 512m -v "$DIR/release/release.json:/release.json:ro" "$agent" check-release /release.json >/dev/null ||
    die "The dev build's agent refuses its release.json."
  cp "$DIR/scripts/install.sh" "$DIR/release/install.sh"
  # What install.sh asks its release source for: where release.json is.
  jq -n --arg v "$TO" '{tag_name: ("v" + $v), assets: [{browser_download_url: "http://127.0.0.1:8765/release.json"}]}' \
    >"$DIR/release/releases/tags/v$TO"
  note "It updates as Pyo $TO"
}

push_build() {
  say "Copying the dev build to $VM1's registry ($(du -sh "$DIR/images" | cut -f1))"
  tar -C "$DIR" -cf "$DIR/build.tar" images release
  send "$DIR/build.tar" /var/tmp/e2e/build.tar
  rm -f "$DIR/build.tar"
  vm "$VM1" tar -C /var/tmp/e2e -xf /var/tmp/e2e/build.tar
  vm "$VM1" rm -f /var/tmp/e2e/build.tar
  vm "$VM1" python3 /var/tmp/e2e/e2e.py images-push "$VM_IMAGES" /var/tmp/e2e/images
  vm "$VM1" rm -rf /var/tmp/e2e/images
}

all_steady() {
  kc get pods -A -o jsonpath='{range .items[*]}{.status.phase}{" "}{.metadata.deletionTimestamp}{" "}{range .status.conditions[?(@.type=="Ready")]}{.status}{end}{"\n"}{end}' |
    awk '$1 != "Succeeded" && $1 != "Failed" && !(NF == 2 && $2 == "True") { busy = 1 } END { exit busy }'
}
# Until no pod has started, stopped or been unready for 2 minutes.
settle() {
  local quiet=0 start=$SECONDS
  while [ "$quiet" -lt 120 ]; do
    [ $((SECONDS - start)) -lt 1200 ] || { note "Pods were still changing after 20 minutes."; return 1; }
    if all_steady 2>/dev/null; then quiet=$((quiet + 10)); else quiet=0; fi
    sleep 10
  done
}

run_update() {
  local rc=0
  vm "$VM1" systemd-run --quiet --unit e2e-release --collect --working-directory /var/tmp/e2e/release \
    python3 -m http.server 8765 --bind 127.0.0.1
  # This branch's install.sh, whole. The dev build isn't signed, so the
  # signature check alone is left out.
  multipass exec "$VM1" -- sudo bash -s -- "$TO" <<'SH' 2>&1 | indent || rc=$?
set -euo pipefail
sleep 2
source <(sed '$d' /var/tmp/e2e/release/install.sh)
verify_release() { say "Not checking the signature: this is a test build"; }
export PYO_UPDATE_SOURCE=http://127.0.0.1:8765 PYO_TELEMETRY=off
main --version "$1" --yes </dev/null
SH
  vm "$VM1" systemctl stop e2e-release || true
  [ "$rc" = 0 ] || return 1
  note "install.sh is done. Waiting until every rollout is over."
  settle
}

update() {
  action "update Pyo $FROM to the dev build" run_update
  note "Pyo runs $(kc -n pyo-system get deployment pyo -o jsonpath='{.spec.template.spec.containers[0].image}' | sed 's/@.*//')"
  note "Envoy runs $(kc -n pyo-gateway get daemonset -o jsonpath='{.items[0].spec.template.spec.containers[0].image}' | sed 's/@.*//')"
}

# ── Run ─────────────────────────────────────────────────────────────────────

say "update-traffic-e2e: $MODE, $SERVERS server$([ "$SERVERS" = 1 ] || echo s), from Pyo $FROM"
# The release Pyo is installed from, read before anything is made.
curl -fsSL --proto '=https' -m 60 -o "$DIR/from.json" "$RELEASES/v$FROM/release.json" || die "There's no Pyo $FROM release to install."
[ -z "$TUNNEL" ] || cf_start
[ -z "$DEV" ] || prepare_update

launch "$VM1" 3G
[ "$SERVERS" = 1 ] || launch "$VM2" 2G
IP1=$(ip_of "$VM1")
IP2=""
[ "$SERVERS" = 1 ] || IP2=$(ip_of "$VM2")
[[ $IP1 =~ ^[0-9.]+$ ]] || die "$VM1 has no address."
URL="http://$IP1:8000"

install_pyo
wait_for 300 "Pyo answers on $VM1" curl -fsS -m 5 -o /dev/null "$URL/api/v1/meta" || die "Pyo didn't start on $VM1."
sign_in
TEAM=$(state | jq -r '.teams[0].id')
wait_for 600 "$VM1 is connected as a cluster" cluster_connected || die "$VM1 didn't connect as a cluster."
CLUSTER=$(state | jq -r '.clusters[0].id')
# A deploy before the cluster can make certificates fails.
wait_for 420 "the cluster can make certificates" kc get clusterissuer pyo-selfsigned pyo-letsencrypt || die "cert-manager didn't start on $VM1."
# The issuers come before cert-manager's webhook answers. A Certificate
# written before then is refused, and the deploy fails.
wait_for 300 "cert-manager's webhook answers" kc -n cert-manager wait --for=condition=Available deployment/cert-manager-webhook --timeout=5s ||
  die "cert-manager's webhook didn't start on $VM1."
[ "$SERVERS" = 1 ] || join_second
if [ -n "$IP2" ]; then
  wait_for 300 "Envoy runs on both servers" envoy_on_both || die "Envoy didn't start on $VM2."
fi
deploy
if { [ "$MODE" = fast ] || [ "$MODE" = paths ] || [ "$MODE" = gateway ]; } && [ -n "$DEV" ]; then
  # The dev build first, with no traffic: the restarts below are its own.
  vm "$VM1" mkdir -p /var/tmp/e2e
  send "$PY" /var/tmp/e2e/e2e.py
  push_build
  say "Updating Pyo $FROM to the dev build, before the traffic starts"
  run_update || die "The update to the dev build didn't finish."
  note "Pyo runs $(kc -n pyo-system get deployment pyo -o jsonpath='{.spec.template.spec.containers[0].image}' | sed 's/@.*//')"
fi
[ -z "$TUNNEL" ] || tunnel_setup
start_watch
[ "$MODE" != update ] || push_build
start_load

case "$MODE" in
  fast) fast ;;
  update) update ;;
  paths) paths ;;
  gateway) gateway ;;
esac

say "Stopping the traffic"
touch "$DIR/stop"
wait "$LOAD_PID" || true
LOAD_PID=""
vm "$VM1" systemctl stop e2e-watch || true
multipass transfer "$VM1:/var/tmp/e2e/pods.log" - >"$DIR/pods.log"

rc=0
python3 "$PY" summary "$DIR" | tee "$DIR/summary.txt" || rc=$?
[ -n "$TUNNEL" ] || echo "Tunnel domain: skipped: no test zone" | tee -a "$DIR/summary.txt"
if [ "$MODE" = paths ]; then
  echo "Per switch change:"
  cat "$DIR/paths.txt"
fi
if [ "$MODE" = gateway ]; then
  echo "Per gateway setting:"
  cat "$DIR/gateway.txt"
fi
if [ -n "$OUT" ]; then
  mkdir -p "$OUT"
  cp "$DIR/load.log" "$DIR/pods.log" "$DIR/actions.log" "$DIR/targets" "$DIR/summary.txt" "$OUT/"
  [ "$MODE" != paths ] || cp "$DIR/paths.txt" "$DIR/probes.log" "$OUT/"
  [ "$MODE" != gateway ] || cp "$DIR/gateway.txt" "$OUT/"
  echo "Logs: $OUT"
fi
exit "$rc"
