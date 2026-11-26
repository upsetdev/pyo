#!/usr/bin/env bash
# Counts what visitors lose while Pyo's inbound path restarts or updates:
# Pyo Gateway (Envoy), the WAF and the tunnel connector.
#
#   scripts/update-traffic-e2e.sh fast   [--servers 2] [--out DIR] [--keep]
#   scripts/update-traffic-e2e.sh fast   --dev [--dev-tag TAG] [--out DIR] [--keep]
#   scripts/update-traffic-e2e.sh update [--dev-tag TAG] [--out DIR] [--keep]
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
# The tunnel domain is skipped: it needs a throwaway Cloudflare zone, and
# that leg isn't written yet. Never give this script a zone people use.
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
  fast | update) ;;
  *) echo "Usage: update-traffic-e2e.sh fast|update [--servers 2] [--out DIR] [--keep]" >&2; exit 2 ;;
esac
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
CREATED=() LOAD_PID="" PULLED="" KEY="" URL=""
: >"$DIR/actions.log"

cleanup() {
  local rc=$? n
  trap - EXIT INT TERM
  if [ -n "$LOAD_PID" ]; then
    touch "$DIR/stop"
    kill "$LOAD_PID" 2>/dev/null || true
  fi
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
  plan=$(api POST /api/v1/compose/preview "$(jq -n --arg t "$TEAM" --arg c "$CLUSTER" --arg f "$compose" '{teamId: $t, clusterId: $c, file: $f}')") ||
    die "Pyo couldn't read the test's Compose file: $plan"
  plan=$(jq --arg n "$RUN" --arg web "$WEB_HOST" --arg waf "$WAF_HOST" '
    .project.name = $n | .project.namespace = $n
    | (.services[] | select(.name == "web") | .ports[0].domain) = $web
    | (.services[] | select(.name == "waf") | .ports[0].domain) = $waf' <<<"$plan")
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
  note "Tunnel domain: skipped: no test zone"
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
  else
    skipped "restart the tunnel connector" "no test zone"
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
[ "$SERVERS" = 1 ] || join_second
if [ -n "$IP2" ]; then
  wait_for 300 "Envoy runs on both servers" envoy_on_both || die "Envoy didn't start on $VM2."
fi
deploy
if [ "$MODE" = fast ] && [ -n "$DEV" ]; then
  # The dev build first, with no traffic: the restarts below are its own.
  vm "$VM1" mkdir -p /var/tmp/e2e
  send "$PY" /var/tmp/e2e/e2e.py
  push_build
  say "Updating Pyo $FROM to the dev build, before the traffic starts"
  run_update || die "The update to the dev build didn't finish."
  note "Pyo runs $(kc -n pyo-system get deployment pyo -o jsonpath='{.spec.template.spec.containers[0].image}' | sed 's/@.*//')"
fi
start_watch
[ "$MODE" != update ] || push_build
start_load

case "$MODE" in
  fast) fast ;;
  update) update ;;
esac

say "Stopping the traffic"
touch "$DIR/stop"
wait "$LOAD_PID" || true
LOAD_PID=""
vm "$VM1" systemctl stop e2e-watch || true
multipass transfer "$VM1:/var/tmp/e2e/pods.log" - >"$DIR/pods.log"

rc=0
python3 "$PY" summary "$DIR" | tee "$DIR/summary.txt" || rc=$?
echo "Tunnel domain: skipped: no test zone"
if [ -n "$OUT" ]; then
  mkdir -p "$OUT"
  cp "$DIR/load.log" "$DIR/pods.log" "$DIR/actions.log" "$DIR/targets" "$DIR/summary.txt" "$OUT/"
  echo "Logs: $OUT"
fi
exit "$rc"
