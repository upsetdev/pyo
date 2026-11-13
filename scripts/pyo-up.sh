#!/usr/bin/env bash
# Build Pyo's images with the in-cluster BuildKit and (re)deploy Pyo into
# k3s. Run as your normal user after scripts/setup-host.sh:
#
#   scripts/pyo-up.sh                  # build what changed and deploy
#   scripts/pyo-up.sh --only pyo,agent # build just these images
#   scripts/pyo-up.sh --all            # build every image
#   scripts/pyo-up.sh --dry-run [...]  # print what would be built, and stop
#   scripts/pyo-up.sh --verbose [...]  # stream the builds' output as well
#   scripts/pyo-up.sh --no-build TAG   # redeploy an existing tag
#   scripts/pyo-up.sh --release 0.1.0  # install or update to a published
#                                      # release (runs deploy/install.sh);
#                                      # needs cosign, or
#                                      # --insecure-skip-verify after it
#
# The dashboard ends up on http://localhost:8000. While it waits (for the
# builds, a rollout, ...), it shows what for and for how long; a pod that
# can't start (image pull, crash) is shown with its last log lines.
set -euo pipefail
cd "$(dirname "$0")/.."

export KUBECONFIG=${KUBECONFIG:-/etc/rancher/k3s/k3s.yaml}
BUILDKIT_VERSION=v0.34.1
BIN="$HOME/.local/bin"
NS=pyo-system
# Where Pyo's own images come from: built here, or a published release.
IMAGES=127.0.0.1:5005/pyo
RELEASE_IMAGES=ghcr.io/upsetdev/pyo
RELEASE_SIGNER='^https://github\.com/upsetdev/pyo/\.github/workflows/release\.yml@refs/tags/v'

TTY="" BOLD="" RED="" RESET=""
if [ -t 1 ] && [ "${TERM:-dumb}" != dumb ]; then
  TTY=1
  [ -n "${NO_COLOR:-}" ] || BOLD=$'\033[1m' RED=$'\033[31m' RESET=$'\033[0m'
fi
log() { printf '%s==> %s%s\n' "$BOLD" "$*" "$RESET"; }

# A published release goes in the way every install and update does: its
# release.json, images pinned by digest, applied by its own upgrader. Its
# signatures are checked first (cosign): release.json's, then every image's.
# Without cosign this stops, unless --insecure-skip-verify says not to check
# (install.sh still checks release.json, with a cosign it downloads).
if [ "${1:-}" = "--release" ]; then
  VERSION=${2:?usage: pyo-up.sh --release VERSION [--insecure-skip-verify]}
  VERSION=${VERSION#v}
  shift 2
  SKIP_VERIFY=""
  if [ "${1:-}" = "--insecure-skip-verify" ]; then
    SKIP_VERIFY=1
    shift
  fi
  if [ -n "$SKIP_VERIFY" ]; then
    echo "--insecure-skip-verify: not checking the images' signatures." >&2
  elif ! command -v cosign >/dev/null; then
    echo "cosign isn't installed, so the release's signatures can't be checked." >&2
    echo "Install it (https://docs.sigstore.dev/cosign/system_config/installation/), or" >&2
    echo "run this with --insecure-skip-verify to install without checking them." >&2
    exit 1
  else
    base="https://github.com/upsetdev/pyo/releases/download/v$VERSION"
    tmp=$(mktemp -d)
    trap 'rm -rf "$tmp"' EXIT
    for f in release.json release.json.sigstore.json; do
      curl -fsSL --proto '=https' --max-time 60 -o "$tmp/$f" "$base/$f" ||
        { echo "Couldn't download the $f of $VERSION." >&2; exit 1; }
    done
    log "Checking the signature of release.json"
    cosign verify-blob --bundle "$tmp/release.json.sigstore.json" \
      --certificate-identity-regexp "$RELEASE_SIGNER" \
      --certificate-oidc-issuer https://token.actions.githubusercontent.com "$tmp/release.json" >/dev/null
    # Every image the release pins, read from its own release.json
    # (scripts/release-manifest.ts), so the list can't drift from it.
    mapfile -t refs < <(sed -n '/^  "images": {/,/^  }/s/^    "[^"]*": "\([^"]*\)",*$/\1/p' "$tmp/release.json")
    [ "${#refs[@]}" -gt 0 ] || { echo "The release.json of $VERSION lists no images." >&2; exit 1; }
    for ref in "${refs[@]}"; do
      case "$ref" in
        "$RELEASE_IMAGES"/*@sha256:*) ;;
        *) echo "The release.json of $VERSION has an image from elsewhere: $ref" >&2; exit 1 ;;
      esac
      image=${ref#"$RELEASE_IMAGES"/}
      log "Checking the signature of ${image%@*}"
      cosign verify "$ref" \
        --certificate-identity-regexp "$RELEASE_SIGNER" \
        --certificate-oidc-issuer https://token.actions.githubusercontent.com >/dev/null
    done
  fi
  exec sudo bash deploy/install.sh --version "$VERSION" "$@"
fi

# What to build: what changed (no arguments), --only a,b, --all, or
# nothing (--no-build TAG). scripts/build-images.sh does the building;
# --dry-run only prints what it would build and keep, and stops.
BUILD_ARGS=() NO_BUILD_TAG="" DRY_RUN="" VERBOSE=()
while [ $# -gt 0 ]; do
  case "$1" in
    --no-build) NO_BUILD_TAG=${2:?usage: pyo-up.sh --no-build TAG}; shift 2 ;;
    --only) BUILD_ARGS+=(--only "${2:?usage: pyo-up.sh --only IMAGE[,IMAGE...]}"); shift 2 ;;
    --all) BUILD_ARGS+=(--all); shift ;;
    --dry-run) DRY_RUN=1; shift ;;
    --verbose) VERBOSE=(--verbose); shift ;;
    *) echo "pyo-up: unknown argument: $1 (see the top of scripts/pyo-up.sh)" >&2; exit 2 ;;
  esac
done
if [ -n "$NO_BUILD_TAG" ] && { [ "${#BUILD_ARGS[@]}" -gt 0 ] || [ -n "$DRY_RUN" ]; }; then
  echo "pyo-up: --no-build doesn't go with --only, --all or --dry-run" >&2
  exit 2
fi
if [ -n "$DRY_RUN" ]; then
  exec env PYO_IMAGES="$IMAGES" scripts/build-images.sh --dry-run "${BUILD_ARGS[@]}"
fi

# ── Progress ───────────────────────────────────────────────────────────────
# A wait shows what it's waiting for and for how long: a spinner on a
# terminal, a line every 15 s otherwise. Nothing is drawn while sudo might
# ask for a password: steps that need root run after `sudo -v`.
CLEANUP=() BG_PID="" SPINNING="" BUILDKIT_UP=""
on_exit() {
  # BuildKit only runs while this builds (platform.yaml has it at 0).
  if [ -n "$BUILDKIT_UP" ]; then
    kubectl -n pyo-system scale deployment/buildkitd --replicas=0 >/dev/null 2>&1 || true
  fi
  if [ -n "$BG_PID" ]; then
    pkill -P "$BG_PID" 2>/dev/null || true
    kill "$BG_PID" 2>/dev/null || true
  fi
  if [ -n "$SPINNING" ]; then printf '\r\033[2K\033[?25h'; fi
  [ "${#CLEANUP[@]}" -eq 0 ] || rm -rf "${CLEANUP[@]}"
}
trap on_exit EXIT
trap 'exit 130' INT TERM

fmt_time() { printf '%d:%02d' "$(($1 / 60))" "$(($1 % 60))"; }

SPIN_LABEL="" SPIN_T0=0 SPIN_N=0 SPIN_NEXT=0
spin_start() {
  SPIN_LABEL=$1 SPIN_T0=$SECONDS SPIN_N=0 SPIN_NEXT=$((SECONDS + 15)) SPINNING=1
  if [ -n "$TTY" ]; then printf '\033[?25l'; else printf '    %s ...\n' "$SPIN_LABEL"; fi
}
spin_tick() {
  local el=$((SECONDS - SPIN_T0)) line cols chars='|/-'"\\"
  if [ -n "$TTY" ]; then
    cols=$(tput cols 2>/dev/null) || cols=80
    [[ "$cols" =~ ^[0-9]+$ ]] && [ "$cols" -ge 20 ] || cols=80
    line="${chars:$((SPIN_N % 4)):1} $SPIN_LABEL $(fmt_time "$el")"
    printf '\r\033[2K%s' "${line:0:$((cols - 1))}"
  elif [ "$SECONDS" -ge "$SPIN_NEXT" ]; then
    printf '    %s %s\n' "$SPIN_LABEL" "$(fmt_time "$el")"
    SPIN_NEXT=$((SECONDS + 15))
  fi
  SPIN_N=$((SPIN_N + 1))
}
# Clears the spinner's line, to print something else.
spin_clear() { if [ -n "$TTY" ]; then printf '\r\033[2K'; fi; }
spin_stop() { # what to say, e.g. "pyo ready"
  spin_clear
  if [ -n "$TTY" ]; then printf '\033[?25h'; fi
  SPINNING=""
  printf '    %s (%s)\n' "$1" "$(fmt_time "$((SECONDS - SPIN_T0))")"
}
# Ticks until process $1 ends, calling function $2 (if any) every 3 s;
# returns the process's exit status.
spin_while() {
  local pid=$1 hook=${2:-} last=-10
  BG_PID=$pid
  while kill -0 "$pid" 2>/dev/null; do
    if [ -n "$hook" ] && [ $((SECONDS - last)) -ge 3 ]; then
      last=$SECONDS
      "$hook"
    fi
    spin_tick
    if [ -n "$TTY" ]; then sleep 0.2; else sleep 1; fi
  done
  BG_PID=""
  wait "$pid"
}

# Runs a command with a spinner, its output in $STEP_OUT; returns its status.
STEP_OUT=""
run_step() { # label command...
  local label=$1 rc=0
  shift
  STEP_OUT=$(mktemp)
  CLEANUP+=("$STEP_OUT")
  spin_start "$label"
  "$@" >"$STEP_OUT" 2>&1 &
  spin_while "$!" || rc=$?
  if [ "$rc" -eq 0 ]; then spin_stop "$label: done"; else spin_stop "$label: failed"; fi
  return "$rc"
}

# A workload's pods that can't run: each problem (image pull, crash, error)
# is shown once, with what the pod says about it.
declare -A SEEN_PROBLEM=()
WAIT_SELECTOR=""
pod_problems() {
  local line pod gone c name reason msg key
  [ -n "$WAIT_SELECTOR" ] || return 0
  while read -r line; do
    pod=${line%% *} gone=${pod#*|} pod=${pod%%|*}
    [ -z "$gone" ] || continue # being deleted: its old containers' errors don't count
    for c in ${line#* }; do
      name=${c%%=*} reason=${c#*=}
      case "$reason" in
        ImagePullBackOff|ErrImagePull|InvalidImageName|CrashLoopBackOff|Error|OOMKilled|CreateContainerConfigError|CreateContainerError|RunContainerError) ;;
        *) continue ;;
      esac
      key="$pod/$name/$reason"
      [ -z "${SEEN_PROBLEM[$key]:-}" ] || continue
      SEEN_PROBLEM[$key]=1
      spin_clear
      printf '    %s! pod %s (%s): %s%s\n' "$RED" "$pod" "$name" "$reason" "$RESET"
      case "$reason" in
        ImagePullBackOff|ErrImagePull|InvalidImageName|CreateContainerConfigError|CreateContainerError)
          msg=$(kubectl -n "$NS" get pod "$pod" -o jsonpath="{.status.initContainerStatuses[?(@.name==\"$name\")].state.waiting.message}{.status.containerStatuses[?(@.name==\"$name\")].state.waiting.message}" 2>/dev/null || true)
          [ -z "$msg" ] || printf '%s\n' "$msg" | fold -w 160 | sed 's/^/      | /'
          ;;
        *)
          { kubectl -n "$NS" logs "$pod" -c "$name" --previous --tail=15 2>/dev/null ||
              kubectl -n "$NS" logs "$pod" -c "$name" --tail=15 2>/dev/null || true; } | sed 's/^/      | /'
          ;;
      esac
    done
  done < <(kubectl -n "$NS" get pods -l "$WAIT_SELECTOR" -o jsonpath='{range .items[*]}{.metadata.name}{"|"}{.metadata.deletionTimestamp}{range .status.initContainerStatuses[*]}{" "}{.name}{"="}{.state.waiting.reason}{.state.terminated.reason}{end}{range .status.containerStatuses[*]}{" "}{.name}{"="}{.state.waiting.reason}{.state.terminated.reason}{end}{"\n"}{end}' 2>/dev/null || true)
}

# The ready replicas of $WAIT_RES, in the spinner's label, and its pods'
# problems.
WAIT_RES=""
rollout_hook() {
  local counts updated ready want sel
  counts=$(kubectl -n "$NS" get "$WAIT_RES" -o jsonpath='{.status.updatedReplicas}/{.status.readyReplicas}/{.spec.replicas}' 2>/dev/null || true)
  IFS=/ read -r updated ready want <<<"$counts" || true
  if [ -n "$want" ]; then
    if [[ "$WAIT_RES" == deployment/* ]] && [ "${updated:-0}" -lt "$want" ]; then
      SPIN_LABEL="Waiting for ${WAIT_RES#*/} (${updated:-0}/$want updated, ${ready:-0} ready)"
    else
      SPIN_LABEL="Waiting for ${WAIT_RES#*/} (${ready:-0}/$want ready)"
    fi
  fi
  if [ -z "$WAIT_SELECTOR" ]; then
    sel=$(kubectl -n "$NS" get "$WAIT_RES" -o jsonpath='{.spec.selector.matchLabels}' 2>/dev/null || true)
    WAIT_SELECTOR=$(sed -e 's/[{}"]//g' -e 's/:/=/g' <<<"$sel")
  fi
  pod_problems
}

# `kubectl rollout status`, showing the ready replicas and the pods'
# problems as it goes; on a timeout, what it said and the pods.
wait_rollout() { # kind/name timeout
  local rc=0 out
  WAIT_RES=$1 WAIT_SELECTOR=""
  out=$(mktemp)
  CLEANUP+=("$out")
  spin_start "Waiting for ${1#*/}"
  kubectl -n "$NS" rollout status "$1" --timeout="$2" >"$out" 2>&1 &
  spin_while "$!" rollout_hook || rc=$?
  if [ "$rc" -eq 0 ]; then
    spin_stop "${1#*/} ready"
    return 0
  fi
  spin_stop "${RED}${1#*/} isn't ready${RESET} after $2"
  sed 's/^/    | /' "$out" >&2
  [ -z "$WAIT_SELECTOR" ] || kubectl -n "$NS" get pods -l "$WAIT_SELECTOR" -o wide 2>&1 | sed 's/^/    | /' >&2 || true
  return "$rc"
}

# Steps that need root run with sudo's password asked first, so it isn't
# asked for behind a spinner.
SUDO=()
as_root() {
  if [ "$(id -u)" = 0 ]; then SUDO=(); elif sudo -v; then SUDO=(sudo -n); else SUDO=(sudo); fi
}

# k3s (and its kubectl) come from the one-time root setup.
if ! command -v kubectl >/dev/null || [ ! -r "$KUBECONFIG" ]; then
  echo "k3s isn't installed yet. Run the one-time host setup first:" >&2
  echo "  sudo bash scripts/setup-host.sh" >&2
  exit 1
fi
if ! kubectl get nodes >/dev/null 2>&1; then
  echo "k3s is installed but its API isn't answering. Check: sudo systemctl status k3s" >&2
  exit 1
fi

# ── buildctl, the BuildKit client (user-space download) ────────────────────
if ! command -v buildctl >/dev/null && [ ! -x "$BIN/buildctl" ]; then
  log "Installing buildctl $BUILDKIT_VERSION to $BIN"
  mkdir -p "$BIN"
  tmp=$(mktemp -d)
  curl -fsSL "https://github.com/moby/buildkit/releases/download/$BUILDKIT_VERSION/buildkit-$BUILDKIT_VERSION.linux-amd64.tar.gz" | tar -xz -C "$tmp" bin/buildctl
  install -m 0755 "$tmp/bin/buildctl" "$BIN/buildctl"
  rm -rf "$tmp"
fi
BUILDCTL=$(command -v buildctl || echo "$BIN/buildctl")

# ── AppArmor check ─────────────────────────────────────────────────────────
# On Ubuntu with kernel 6.17+, containerd's default AppArmor profile stops
# processes in a container signalling each other: Postgres never becomes
# ready and the rollouts below time out. The fix is a corrected profile
# (deploy/host/apparmor-workaround.sh).
AA=deploy/host/apparmor-workaround.sh
if "$AA" needed && ! "$AA" installed; then
  if sudo -n true 2>/dev/null; then
    log "Applying the AppArmor workaround for kernel $(uname -r)"
    sudo "$AA" install
  else
    echo "This kernel ($(uname -r)) stops processes in containers from signalling each other," >&2
    echo "so Postgres would never start. Apply Pyo's AppArmor workaround, then run this again:" >&2
    echo "  sudo $AA install" >&2
    echo "Details: https://github.com/containerd/containerd/issues/14485" >&2
    exit 1
  fi
fi

# ── HTTPS on the tailnet, and connecting this machine ──────────────────────
# Pyo is reached from other devices over HTTPS on this machine's tailnet
# name (https://<name>:8443; its registry on :5443), with Tailscale's
# certificate. That needs MagicDNS and HTTPS certificates on the tailnet;
# without them Pyo stays on this machine (http://localhost:8000).
TAILNET_NAME=""
tailnet_name() {
  TAILNET_NAME=""
  command -v tailscale >/dev/null || return 0
  local js name
  js=$(tailscale status --json 2>/dev/null) || return 0
  name=$(sed -n '/"Self"/,/}/s/^ *"DNSName": *"\([^"]*\)",*$/\1/p' <<<"$js" | head -n1)
  name=${name%.}
  [ -n "$name" ] && grep -q '"CertDomains": *\[' <<<"$js" || return 0
  TAILNET_NAME=$name
}
remote_access_note() {
  if [ -z "$TAILNET_NAME" ]; then
    echo "Pyo is only on this machine: http://localhost:8000. To use it from other devices,"
    echo "put this machine on Tailscale, turn on MagicDNS and HTTPS Certificates"
    echo "(Tailscale admin console › DNS), and run this again."
  else
    echo "Note: HTTPS certificates are public: \"$TAILNET_NAME\" (this machine's and your"
    echo "tailnet's name) is in Certificate Transparency logs. Prefer a randomized tailnet"
    echo "name (admin console › DNS › Rename tailnet) and a generic machine name."
  fi
}
# The host's own helpers, as systemd timers (root): Tailscale's certificate
# into the gateway's Secret, daily; and connecting this machine as a cluster
# once someone has signed up, every minute until it's done. Idempotent.
host_units() {
  local sudo=()
  [ "$(id -u)" = 0 ] || sudo=(sudo)
  "${sudo[@]}" install -d -m 755 /usr/local/lib/pyo
  "${sudo[@]}" install -d -m 700 /etc/pyo
  "${sudo[@]}" tee /usr/local/lib/pyo/tailnet-cert.sh >/dev/null <<'SH'
#!/usr/bin/env bash
# Tailscale's certificate for this machine's tailnet name, into Pyo
# Gateway's Secret pyo-gateway/pyo-tls-tailnet; and the Service that takes
# the tailnet listeners' connections on this machine's Tailscale address.
# Daily; idempotent.
set -euo pipefail
name=$(tailscale status --json | sed -n '/"Self"/,/}/s/^ *"DNSName": *"\([^"]*\)",*$/\1/p' | head -n1)
name=${name%.}
[ -n "$name" ] || exit 0
d=$(mktemp -d)
trap 'rm -rf "$d"' EXIT
tailscale cert --cert-file "$d/tls.crt" --key-file "$d/tls.key" "$name" >/dev/null
k3s kubectl create namespace pyo-gateway --dry-run=client -o yaml | k3s kubectl apply -f - >/dev/null
k3s kubectl -n pyo-gateway create secret tls pyo-tls-tailnet --cert "$d/tls.crt" --key "$d/tls.key" \
  --dry-run=client -o yaml | k3s kubectl apply -f - >/dev/null
# k3s's ServiceLB publishes only the server's own addresses (InternalIP or
# ExternalIP): a connection to the Tailscale address goes through its pod
# and arrives from that pod, which the listeners' tailnet-only policy
# refuses. Listed here, kube-proxy hands it to the proxy on this machine
# unchanged (externalTrafficPolicy Local), so the proxy sees the device's
# own 100.x address.
ip4=$(tailscale ip -4 2>/dev/null | head -n1)
[ -n "$ip4" ] || exit 0
# Its only warning (externalIPs are deprecated) stays out of the journal.
out=$(k3s kubectl apply -f - 2>&1 <<YAML
apiVersion: v1
kind: Service
metadata:
  name: pyo-tailnet
  namespace: pyo-gateway
  labels: { app.kubernetes.io/managed-by: pyo }
spec:
  type: ClusterIP
  externalIPs: ["$ip4"]
  externalTrafficPolicy: Local
  selector:
    app.kubernetes.io/component: proxy
    app.kubernetes.io/managed-by: envoy-gateway
    gateway.envoyproxy.io/owning-gatewayclass: pyo
  ports:
    - { name: dashboard, port: 8443, targetPort: 8443 }
    - { name: registry, port: 5443, targetPort: 5443 }
YAML
) || { echo "$out" >&2; exit 1; }
SH
  "${sudo[@]}" tee /usr/local/lib/pyo/connect-this-machine.sh >/dev/null <<'SH'
#!/usr/bin/env bash
# Connects the machine Pyo runs on as a cluster (its gateway serves Pyo over
# HTTPS), once a team exists: Pyo hands over the same command as "Add
# cluster › This machine", which fetches connect.sh from Pyo here (no
# internet needed). Stops itself once the agent has reported in.
set -euo pipefail
done_() { systemctl disable --now pyo-connect.timer >/dev/null 2>&1 || true; }
# One run at a time (the timer and the setup's own run): a second one waits,
# then finds this machine connected, or runs the same command again.
exec 9>/run/pyo-connect.lock
flock 9
key=$(cat /etc/pyo/bootstrap-key)
r=$(curl -fsS -m 20 "http://127.0.0.1:8000/install/this-machine?key=$key") || exit 0
case "$r" in
  *'"state":"connected"'*) done_ ;;
  *'"state":"connect"'*)
    cmd=$(sed -n 's/.*"command":"\([^"]*\)".*/\1/p' <<<"$r")
    [ -n "$cmd" ] || exit 0
    bash -c "$cmd"
    done_
    ;;
esac
SH
  "${sudo[@]}" chmod 755 /usr/local/lib/pyo/tailnet-cert.sh /usr/local/lib/pyo/connect-this-machine.sh
  "${sudo[@]}" tee /etc/systemd/system/pyo-tailnet-cert.service >/dev/null <<'UNIT'
[Unit]
Description=Pyo: Tailscale certificate for HTTPS on the tailnet
After=network-online.target tailscaled.service k3s.service
[Service]
Type=oneshot
ExecStart=/usr/local/lib/pyo/tailnet-cert.sh
UNIT
  "${sudo[@]}" tee /etc/systemd/system/pyo-tailnet-cert.timer >/dev/null <<'UNIT'
[Unit]
Description=Pyo: renew the tailnet certificate daily
[Timer]
OnCalendar=daily
RandomizedDelaySec=1h
Persistent=true
[Install]
WantedBy=timers.target
UNIT
  "${sudo[@]}" tee /etc/systemd/system/pyo-connect.service >/dev/null <<'UNIT'
[Unit]
Description=Pyo: connect this machine as a cluster
After=network-online.target k3s.service
[Service]
Type=oneshot
ExecStart=/usr/local/lib/pyo/connect-this-machine.sh
UNIT
  "${sudo[@]}" tee /etc/systemd/system/pyo-connect.timer >/dev/null <<'UNIT'
[Unit]
Description=Pyo: connect this machine once a team exists
[Timer]
OnBootSec=1min
OnUnitActiveSec=1min
[Install]
WantedBy=timers.target
UNIT
  "${sudo[@]}" systemctl daemon-reload
  # Only on the tailnet with HTTPS certificates; off otherwise.
  if [ -n "$TAILNET_NAME" ]; then
    "${sudo[@]}" systemctl enable --now pyo-tailnet-cert.timer >/dev/null
  else
    "${sudo[@]}" systemctl disable --now pyo-tailnet-cert.timer >/dev/null 2>&1 || true
  fi
  "${sudo[@]}" systemctl enable --now pyo-connect.timer >/dev/null
}
# The key the host proves itself with to connect this machine (Pyo reads it
# from the pyo-bootstrap Secret). Made once.
bootstrap_key() { # $1: kubectl command
  local sudo=() key
  [ "$(id -u)" = 0 ] || sudo=(sudo)
  if ! "${sudo[@]}" test -s /etc/pyo/bootstrap-key; then
    "${sudo[@]}" install -d -m 700 /etc/pyo
    head -c 32 /dev/urandom | od -An -tx1 | tr -d ' \n' | "${sudo[@]}" tee /etc/pyo/bootstrap-key >/dev/null
    "${sudo[@]}" chmod 600 /etc/pyo/bootstrap-key
  fi
  key=$("${sudo[@]}" cat /etc/pyo/bootstrap-key)
  $1 -n pyo-system create secret generic pyo-bootstrap --from-literal=key="$key" --dry-run=client -o yaml | $1 apply -f - >/dev/null
}

# ── Platform: namespace, registry, BuildKit, Postgres ──────────────────────
log "Applying platform services"
# Other nodes reach the registry on this machine's Tailscale IP (if any).
TAILNET_IP=$(tailscale ip -4 2>/dev/null | head -n1 || true)
tailnet_name
# Server-side, with the field manager the dashboard's updates use too
# (crates/pyo-agent/src/upgrade.rs), so both own the same fields.
APPLY=(kubectl apply --server-side --force-conflicts --field-manager=pyo-install -f -)
# Volume claims are only created, never re-applied: their spec can't change
# once bound, and claims made by an older client-side apply would otherwise
# have their bound volume cleared. The namespace goes first, so the claims
# can be made on a fresh cluster, and the claims before the workloads.
split_kind() { # $1 = keep (ns|pvc|rest); manifest on stdin
  awk -v keep="$1" '
    function flush() { if (doc != "" && kind == keep) printf "%s---\n", doc; doc = ""; kind = "rest" }
    BEGIN { kind = "rest" }
    /^---/ { flush(); next }
    /^kind: Namespace/ { kind = "ns" }
    /^kind: PersistentVolumeClaim/ { kind = "pvc" }
    { doc = doc $0 "\n" }
    END { flush() }'
}
PLATFORM=$(sed "s/__TAILNET_IP__/$TAILNET_IP/g" deploy/k8s/platform.yaml)
split_kind ns <<<"$PLATFORM" | "${APPLY[@]}" >/dev/null
# Claims that already exist are fine; any other error stops here.
if ! out=$(split_kind pvc <<<"$PLATFORM" | kubectl create -f - 2>&1); then
  errs=$(grep -v -e AlreadyExists -e ' created$' <<<"$out" || true)
  if [ -n "$errs" ]; then
    printf 'Creating the volume claims failed:\n%s\n' "$errs" >&2
    exit 1
  fi
fi
split_kind rest <<<"$PLATFORM" | "${APPLY[@]}" >/dev/null

# The key this machine's setup connects it as a cluster with.
bootstrap_key kubectl

# The Postgres password is generated once and kept in a secret.
if ! kubectl -n "$NS" get secret pyo-postgres >/dev/null 2>&1; then
  log "Creating the Postgres secret"
  PW=$(head -c 32 /dev/urandom | base64 | tr -dc 'A-Za-z0-9' | head -c 32)
  kubectl -n "$NS" create secret generic pyo-postgres \
    --from-literal=password="$PW" \
    --from-literal=url="postgres://pyo:$PW@pyo-postgres.$NS.svc:5432/pyo" >/dev/null
fi

if ! kubectl -n "$NS" get secret pyo-clickhouse >/dev/null 2>&1; then
  log "Creating the ClickHouse secret"
  kubectl -n "$NS" create secret generic pyo-clickhouse \
    --from-literal=password="$(head -c 32 /dev/urandom | base64 | tr -dc 'A-Za-z0-9' | head -c 32)" >/dev/null
fi

# A new installation: Pyo creates its admin with this password at first start.
ADMIN_PASSWORD=""
if ! kubectl -n "$NS" get deployment pyo >/dev/null 2>&1 && ! kubectl -n "$NS" get secret pyo-admin >/dev/null 2>&1; then
  log "Creating the first sign-in"
  ADMIN_PASSWORD=$(head -c 16 /dev/urandom | od -An -tx1 | tr -d ' \n')
  kubectl -n "$NS" create secret generic pyo-admin --from-literal=password="$ADMIN_PASSWORD" >/dev/null
fi

if ! kubectl -n "$NS" get secret pyo-secrets >/dev/null 2>&1; then
  log "Creating the key that seals saved integration keys"
  kubectl -n "$NS" create secret generic pyo-secrets \
    --from-literal=secret-key="$(head -c 32 /dev/urandom | od -An -tx1 | tr -d ' \n')" >/dev/null
fi

if ! kubectl -n "$NS" get secret pyo-backup-store >/dev/null 2>&1; then
  log "Creating the backup store's credentials"
  hex() { head -c "$1" /dev/urandom | od -An -tx1 | tr -d ' \n'; }
  # Garage's RPC secret and admin token, and Pyo's own S3 key (Garage's
  # format: GK + 24 hex), which Pyo imports at start (teams get their own).
  kubectl -n "$NS" create secret generic pyo-backup-store \
    --from-literal=rpc-secret="$(hex 32)" \
    --from-literal=admin-token="$(hex 32)" \
    --from-literal=access-key="GK$(hex 12)" \
    --from-literal=secret-key="$(hex 32)" >/dev/null
fi

# Visitor locations: MaxMind GeoLite2, downloaded with this license key.
# Set MAXMIND_LICENSE_KEY when running this script to save or change it;
# without one, locations show as unknown.
if [ -n "${MAXMIND_LICENSE_KEY:-}" ]; then
  log "Saving the MaxMind license key"
  kubectl -n "$NS" create secret generic pyo-geoip \
    --from-literal=maxmind-license-key="$MAXMIND_LICENSE_KEY" \
    --dry-run=client -o yaml | kubectl apply -f - >/dev/null
fi

log "Waiting for registry, BuildKit, Postgres and ClickHouse"
wait_rollout deployment/registry 300s
# The registry on this machine's 127.0.0.1:5005, where the builds below push.
wait_rollout deployment/pyo-host 300s
if [ -z "$NO_BUILD_TAG" ]; then
  # Stopped between builds; its cache volume keeps warm builds fast.
  BUILDKIT_UP=1
  kubectl -n "$NS" scale deployment/buildkitd --replicas=1 >/dev/null
  wait_rollout deployment/buildkitd 300s
fi
wait_rollout statefulset/pyo-postgres 300s
wait_rollout statefulset/pyo-clickhouse 300s
wait_rollout statefulset/pyo-backup-store 300s

# ── Build ──────────────────────────────────────────────────────────────────
# Each image gets a tag: this run's timestamp if it was built now, or its
# last build's if nothing it's made of changed (or --only left it out).
TAGS=$(mktemp)
CLEANUP+=("$TAGS")
if [ -n "$NO_BUILD_TAG" ]; then
  for image in pyo agent builder flows db-controller kv waf geoip postgres-16 postgres-17 postgres-18 mysql-operator mysql-backup; do
    echo "$image $NO_BUILD_TAG"
  done >"$TAGS"
else
  BUILDCTL="$BUILDCTL" PYO_IMAGES="$IMAGES" PYO_BUILDKIT_ADDR=tcp://127.0.0.1:1234 \
    scripts/build-images.sh --tag "$(date +%Y%m%d%H%M%S)" --out "$TAGS" "${BUILD_ARGS[@]}" "${VERBOSE[@]}"
  kubectl -n "$NS" scale deployment/buildkitd --replicas=0 >/dev/null
  BUILDKIT_UP=""
fi
declare -A TAG_OF=()
while read -r image tag; do TAG_OF[$image]=$tag; done <"$TAGS"
TAG=${TAG_OF[pyo]}

# ── Deploy ─────────────────────────────────────────────────────────────────
log "Deploying pyo:$TAG"
# Other machines' builders and servers reach the registry door on the
# tailnet (pyo-host); none without one.
REGISTRY_PUSH=${TAILNET_IP:+$TAILNET_IP:5005}
TAILNET_API=${TAILNET_IP:+http://$TAILNET_IP:8000}
# Every image at its own tag (postgres:<version>-<tag>).
IMAGE_SED=()
for image in "${!TAG_OF[@]}"; do
  case "$image" in
    postgres-*) IMAGE_SED+=(-e "s|__IMAGES__/postgres:${image#postgres-}-__TAG__|$IMAGES/postgres:${image#postgres-}-${TAG_OF[$image]}|g") ;;
    *) IMAGE_SED+=(-e "s|__IMAGES__/$image:__TAG__|$IMAGES/$image:${TAG_OF[$image]}|g") ;;
  esac
done
PYO_YAML=$(sed "${IMAGE_SED[@]}" -e "s|__IMAGES__|$IMAGES|g" -e "s|__REGISTRY_PUSH__|$REGISTRY_PUSH|g" -e "s|__TAILNET_API__|$TAILNET_API|g" -e "s|__TAILNET_NAME__|$TAILNET_NAME|g" deploy/k8s/pyo.yaml)
if grep -q "__TAG__" <<<"$PYO_YAML"; then
  echo "pyo-up: deploy/k8s/pyo.yaml has an image scripts/build-images.sh doesn't build:" >&2
  grep "__TAG__" <<<"$PYO_YAML" >&2
  exit 1
fi
"${APPLY[@]}" <<<"$PYO_YAML" >/dev/null || { echo "pyo-up: applying Pyo failed" >&2; exit 1; }
wait_rollout deployment/pyo 300s
wait_rollout deployment/pyo-worker 300s

# An agent installed on this cluster runs the image it was installed with;
# move it to this build too. (Agents on other clusters don't self-update yet.)
# Its install manifest is applied again, from this Pyo: that moves the agent
# and also brings its roles and admission policy up to date, which pins
# this build's WAF, GeoIP and database images (waf, geoip, db-controller,
# kv, mysql-operator, mysql-backup, at their tags in TAG_OF):
# the new agent updates this cluster's databases to them once Pyo asks
# (every heartbeat). The new agent
# applies the proxies' settings with the new geoip image at start, so the proxies
# roll. If the manifest can't be fetched, only the image moves.
if kubectl -n "$NS" get deployment pyo-agent >/dev/null 2>&1; then
  log "Updating this cluster's agent to agent:${TAG_OF[agent]}"
  AGENT_TOKEN=$(kubectl -n "$NS" get secret pyo-agent -o jsonpath='{.data.PYO_AGENT_TOKEN}' 2>/dev/null | base64 -d 2>/dev/null || true)
  AGENT_YAML=$(mktemp)
  CLEANUP+=("$AGENT_YAML")
  # Right after the rollout an old Pyo can still answer and hand out the
  # previous build's manifest, so wait for one that pins this build.
  agent_manifest() {
    local _
    for _ in $(seq 1 30); do
      if [ -n "$AGENT_TOKEN" ] && curl -fsS -m 20 "http://127.0.0.1:8000/install/agent.yaml?token=$AGENT_TOKEN" -o "$AGENT_YAML" 2>/dev/null \
        && grep -q "ValidatingAdmissionPolicy" "$AGENT_YAML" && grep -q "/waf:${TAG_OF[waf]}" "$AGENT_YAML" && grep -q "/geoip:${TAG_OF[geoip]}" "$AGENT_YAML" && grep -q "/db-controller:${TAG_OF[db-controller]}" "$AGENT_YAML"; then
        return 0
      fi
      sleep 2
    done
    return 1
  }
  fetched=
  if run_step "Fetching this build's agent manifest from Pyo" agent_manifest; then fetched=1; fi
  if [ -n "$fetched" ]; then
    # The gateway's CRDs first, as connect.sh applies them (its chart
    # installs none): Envoy Gateway's, and the Gateway API's standard
    # channel unless the cluster has its own (k3s 1.37+, which also takes
    # over a copy Pyo applied before).
    crds_q="" gw_api=customresourcedefinition.apiextensions.k8s.io/gateways.gateway.networking.k8s.io
    if kubectl -n kube-system get helmchart.helm.cattle.io gateway-api-crd >/dev/null 2>&1; then
      :
    elif ! kubectl get "$gw_api" >/dev/null 2>&1 || [ "$(kubectl get "$gw_api" -o jsonpath='{.metadata.labels.pyo\.dev/gateway-api}')" = true ]; then
      crds_q="?gatewayApi=1"
    fi
    if curl -fsS -m 60 "http://127.0.0.1:8000/install/gateway-crds.yaml$crds_q" -o "$AGENT_YAML.crds"; then
      kubectl apply --server-side --force-conflicts --field-manager=pyo -f "$AGENT_YAML.crds" >/dev/null
    else
      echo "pyo-up: couldn't fetch the gateway's CRDs. Envoy Gateway may not start." >&2
    fi
    rm -f "$AGENT_YAML.crds"
    kubectl apply -f "$AGENT_YAML" >/dev/null
  else
    echo "pyo-up: couldn't fetch the agent's manifest, so only its image moves (its admission policy may not allow waf:${TAG_OF[waf]} or geoip:${TAG_OF[geoip]}, so the gateway can't take the new GeoIP image)." >&2
    kubectl -n "$NS" set image deployment/pyo-agent "agent=$IMAGES/agent:${TAG_OF[agent]}" >/dev/null
  fi
  rm -f "$AGENT_YAML"
  wait_rollout deployment/pyo-agent 180s
fi

# In-cluster builders also run the image they were created with; move them on.
for b in $(kubectl -n "$NS" get deployments -l app.kubernetes.io/name=pyo-builder -o name 2>/dev/null); do
  log "Updating ${b#deployment.apps/} to builder:${TAG_OF[builder]}"
  kubectl -n "$NS" set image "$b" "builder=$IMAGES/builder:${TAG_OF[builder]}" >/dev/null
done

# HTTPS on the tailnet before the address is shown, then this machine as a
# cluster (its gateway serves Pyo over HTTPS; waits for the first sign-up).
host_units
as_root
if [ -n "$TAILNET_NAME" ]; then
  log "Getting the tailnet certificate for $TAILNET_NAME"
  if ! run_step "Getting the certificate" "${SUDO[@]}" /usr/local/lib/pyo/tailnet-cert.sh; then
    sed 's/^/    | /' "$STEP_OUT" >&2
    echo "pyo-up: couldn't get the tailnet certificate yet. pyo-tailnet-cert.timer tries again daily." >&2
  fi
fi
run_step "Connecting this machine to Pyo" "${SUDO[@]}" /usr/local/lib/pyo/connect-this-machine.sh || true
sed 's/^/    | /' "$STEP_OUT"

echo
echo "Pyo $TAG is up: http://localhost:8000"
[ -z "$TAILNET_NAME" ] || echo "  and https://$TAILNET_NAME:8443 from your tailnet (registry: $TAILNET_NAME:5443)"
remote_access_note
if [ -n "$ADMIN_PASSWORD" ]; then
  echo
  echo "Sign in with this login, then choose your own email and password:"
  echo "  Email:    admin@pyo.local"
  echo "  Password: $ADMIN_PASSWORD"
elif kubectl -n "$NS" get secret pyo-admin >/dev/null 2>&1; then
  # An earlier run made the login (maybe one that stopped before printing
  # it). It works until the first sign-in sets your own.
  echo
  echo "First sign-in: admin@pyo.local, with the password from:"
  echo "  kubectl -n $NS get secret pyo-admin -o jsonpath='{.data.password}' | base64 -d"
fi
